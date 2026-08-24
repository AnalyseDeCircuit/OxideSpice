//! Session supervision and child-channel ownership.

use std::collections::{HashMap, HashSet};
#[cfg(unix)]
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::{fmt, time::Duration};

use oxide_spice_protocol::{
    CapabilitySet, ChannelId, ChannelType, ChannelsList, MainInit, MigrationBegin, MouseMode,
    MouseModeState, common_capability, decode_agent_u32, decode_main_name, decode_main_uuid,
    display_capability, encode_mouse_mode_request, inputs_capability, main_capability, main_client,
    main_server, playback_capability, record_capability, spicevmc_capability,
};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpStream;
#[cfg(unix)]
use tokio::net::UnixStream;
use tokio::sync::{Mutex as AsyncMutex, Semaphore, mpsc, watch};
use tokio::task::{JoinHandle, JoinSet};
use tokio::time::timeout;
use zeroize::Zeroizing;

use crate::agent::{AgentHandle, AgentTaskPaths, agent_paths};
#[cfg(unix)]
use crate::channel::link_channel_with_file_descriptors;
use crate::channel::{
    BoxedStream, Channel, ChannelIdentity, ControlDisposition, ControlState,
    DEFAULT_MAX_MESSAGE_BODY, IncomingMessage, LinkParameters, MigrationReplacement,
    ProgressRegistry, handle_channel_wait, link_channel,
};
use crate::cursor::{CursorEvents, CursorState, cursor_events, run_cursor};
use crate::display::{
    DisplayTaskContext, FrameEvent, FrameReceiver, glz_window, image_decode_slots,
    initialize_display_channel, next_frame, run_display, surface_budget,
};
use crate::display::{DisplayTopology, DisplayTopologyEvents, topology_events};
#[cfg(unix)]
use crate::display::{GlFrameEvents, gl_frame_events};
use crate::inputs::{InputTaskPaths, InputsHandle, input_paths, run_inputs};
use crate::playback::{
    PlaybackAudioSettings, PlaybackChannel, PlaybackPackets, PlaybackPcmPacket, PlaybackState,
    playback_channel, playback_packets, run_playback,
};
use crate::port::{PortChannel, PortTaskPaths, port_channel, run_port};
use crate::record::{
    RecordChannel, RecordClock, RecordTaskPaths, initialize_record_channel, record_channel,
    run_record,
};
use crate::sasl::{SaslOptions, SaslParameters};
use crate::smartcard::{SmartcardChannel, SmartcardTaskPaths, run_smartcard, smartcard_channel};
#[cfg(unix)]
use crate::unix_stream::{ReceivedFileDescriptors, UnixFdStream};
use crate::usbredir::{UsbRedirChannel, UsbRedirTaskPaths, run_usbredir, usbredir_channel};
use crate::{ClientError, ErrorCategory};

/// First generation assigned before reconnect or migration exists.
const INITIAL_CONNECTION_GENERATION: u64 = 1;
/// Cooperative channel cleanup deadline before the supervisor aborts and reaps a stuck task.
const CHANNEL_SHUTDOWN_GRACE: Duration = Duration::from_secs(2);
/// Conservative default bound for independently linked Display transports.
const DEFAULT_MAX_DISPLAY_CHANNELS: usize = 16;
/// Conservative default bound for independently linked Playback transports.
const DEFAULT_MAX_PLAYBACK_CHANNELS: usize = 4;
/// Conservative default bound for independently linked Record transports.
const DEFAULT_MAX_RECORD_CHANNELS: usize = 4;
/// Conservative default bound for linked USB redirection transports.
const DEFAULT_MAX_USBREDIR_CHANNELS: usize = 8;
/// Conservative default bound for independently linked Smartcard transports.
const DEFAULT_MAX_SMARTCARD_CHANNELS: usize = 4;
/// Conservative default bound for linked generic Port and WebDAV transports.
const DEFAULT_MAX_PORT_CHANNELS: usize = 8;

/// Linked transports and task-private paths transferred atomically to the supervisor.
struct LinkedChannels {
    main: Channel<BoxedStream>,
    displays: Vec<(Channel<BoxedStream>, u8)>,
    inputs: Option<(Channel<BoxedStream>, InputTaskPaths, u8)>,
    cursors: Vec<(Channel<BoxedStream>, watch::Sender<Option<CursorState>>, u8)>,
    playbacks: Vec<LinkedPlayback>,
    records: Vec<LinkedRecord>,
    usbredir: Vec<LinkedUsbRedir>,
    smartcards: Vec<LinkedSmartcard>,
    ports: Vec<LinkedPort>,
}

/// Task-owned transport and bounded host paths for one Playback channel id.
struct LinkedPlayback {
    channel: Channel<BoxedStream>,
    state_sender: watch::Sender<PlaybackState>,
    audio_sender: watch::Sender<PlaybackAudioSettings>,
    packet_sender: mpsc::Sender<PlaybackPcmPacket>,
    channel_id: u8,
}

/// Task-owned transport and paths for one Record channel id.
struct LinkedRecord {
    channel: Channel<BoxedStream>,
    paths: RecordTaskPaths,
    clock: Arc<RecordClock>,
    channel_id: u8,
}

/// Task-owned transport and paths for one usbredir channel id.
struct LinkedUsbRedir {
    channel: Channel<BoxedStream>,
    paths: UsbRedirTaskPaths,
    channel_id: u8,
}

/// Task-owned transport and paths for one Smartcard channel id.
struct LinkedSmartcard {
    channel: Channel<BoxedStream>,
    paths: SmartcardTaskPaths,
    channel_id: u8,
}

/// Task-owned transport and paths for one generic Port wire stream.
struct LinkedPort {
    channel: Channel<BoxedStream>,
    paths: PortTaskPaths,
    channel_type: ChannelType,
    channel_id: u8,
}

/// Session-wide signals shared with the channel task set.
struct SupervisorSignals {
    cancel_sender: watch::Sender<bool>,
    cancel_receiver: watch::Receiver<bool>,
    frame_sender: watch::Sender<Option<FrameEvent>>,
    topology_sender: watch::Sender<Option<DisplayTopology>>,
    mouse_mode_sender: watch::Sender<MouseMode>,
    mouse_mode_receiver: watch::Receiver<MouseMode>,
    server_identity_sender: watch::Sender<ServerIdentity>,
    state_sender: watch::Sender<SessionState>,
    image_decode_slots: Arc<Semaphore>,
    glz_window: Arc<crate::display::GlzWindow>,
    agent_paths: AgentTaskPaths,
    #[cfg(unix)]
    gl_frame_sender: mpsc::Sender<crate::display::GlFrame>,
    migration_manager: MigrationManager,
}

#[derive(Clone)]
struct MigrationChannelSpec {
    identity: ChannelIdentity,
    common_capabilities: CapabilitySet,
    channel_capabilities: CapabilitySet,
    initialization: MigrationChannelInitialization,
}

#[derive(Clone)]
enum MigrationChannelInitialization {
    None,
    Display,
    Record(Arc<RecordClock>),
}

#[derive(Clone)]
struct MigrationManager {
    options: ConnectOptions,
    session_id: Arc<AtomicU32>,
    specs: Arc<[MigrationChannelSpec]>,
    replacement_senders:
        Arc<HashMap<ChannelIdentity, mpsc::Sender<MigrationReplacement<BoxedStream>>>>,
    active_generation: Arc<AtomicU64>,
    prepare_gate: Arc<AsyncMutex<()>>,
}

struct SwitchHostBootstrap {
    mouse_mode: MouseModeState,
    agent: AgentBootstrapState,
    server_identity: ServerIdentity,
    control: ControlState,
}

impl MigrationManager {
    async fn prepare(
        &self,
        destination: &oxide_spice_protocol::MigrationDestination,
        seamless_requested: bool,
        source_version: u32,
    ) -> Result<(u64, bool), ClientError> {
        let _prepare = self.prepare_gate.lock().await;
        let generation = self
            .active_generation
            .fetch_add(1, Ordering::AcqRel)
            .checked_add(1)
            .ok_or_else(|| resource_limit_error("migration generation"))?;
        let target_options = self.target_options(destination)?;
        let result = self
            .connect_target_channels(
                &target_options,
                generation,
                seamless_requested,
                source_version,
            )
            .await;
        if result.is_err() {
            self.active_generation.fetch_add(1, Ordering::AcqRel);
        }
        result.map(|seamless| (generation, seamless))
    }

    fn cancel(&self) {
        self.active_generation.fetch_add(1, Ordering::AcqRel);
    }

    async fn switch_host(
        &self,
        destination: &oxide_spice_protocol::MigrationDestination,
    ) -> Result<SwitchHostBootstrap, ClientError> {
        let _prepare = self.prepare_gate.lock().await;
        let generation = self
            .active_generation
            .fetch_add(1, Ordering::AcqRel)
            .checked_add(1)
            .ok_or_else(|| resource_limit_error("migration generation"))?;
        let target_options = self.target_options(destination)?;
        let result = self.connect_switch_host(&target_options, generation).await;
        if result.is_err() {
            self.active_generation.fetch_add(1, Ordering::AcqRel);
        }
        result
    }

    async fn connect_switch_host(
        &self,
        options: &ConnectOptions,
        generation: u64,
    ) -> Result<SwitchHostBootstrap, ClientError> {
        let main_spec = self
            .specs
            .iter()
            .find(|spec| spec.identity.channel_type == ChannelType::Main)
            .ok_or(ClientError::Internal("missing Main migration spec"))?;
        let main = self.connect_target_channel(options, 0, main_spec).await?;
        let (main, main_init, channels, mouse_mode, agent, server_identity, control) =
            timeout(options.handshake_timeout, bootstrap_main(main))
                .await
                .map_err(|_| handshake_timeout_error("switch-host Main bootstrap timed out"))??;
        let expected_identities: HashSet<_> = self
            .specs
            .iter()
            .filter(|spec| spec.identity.channel_type != ChannelType::Main)
            .map(|spec| spec.identity)
            .collect();
        let target_identities: HashSet<_> = channels
            .channels
            .iter()
            .map(|channel| ChannelIdentity {
                channel_type: channel.channel_type,
                channel_id: channel.channel_id,
            })
            .collect();
        if target_identities != expected_identities {
            return Err(protocol_value_error("switch-host channel topology"));
        }

        let mut replacements = Vec::with_capacity(self.specs.len());
        replacements.push((main_spec.identity, main));
        for spec in self
            .specs
            .iter()
            .filter(|spec| spec.identity.channel_type != ChannelType::Main)
        {
            replacements.push((
                spec.identity,
                self.connect_target_channel(options, main_init.session_id, spec)
                    .await?,
            ));
        }
        self.session_id
            .store(main_init.session_id, Ordering::Release);
        for (identity, channel) in replacements {
            self.replacement_senders
                .get(&identity)
                .ok_or(ClientError::Internal(
                    "missing migration replacement sender",
                ))?
                .send(MigrationReplacement {
                    generation,
                    seamless: false,
                    activate_immediately: true,
                    channel,
                })
                .await
                .map_err(|_| ClientError::TaskTerminated)?;
        }
        Ok(SwitchHostBootstrap {
            mouse_mode,
            agent,
            server_identity,
            control,
        })
    }

    fn target_options(
        &self,
        destination: &oxide_spice_protocol::MigrationDestination,
    ) -> Result<ConnectOptions, ClientError> {
        let mut options = self.options.clone();
        let port = match &mut options.transport_security {
            TransportSecurity::Plain => destination.port,
            #[cfg(feature = "tls-ring")]
            TransportSecurity::Tls {
                server_name,
                client_config,
            } => {
                if let Some(policy) = options.migration_tls_policy.as_ref() {
                    let configuration = policy.configure(destination)?;
                    if configuration.server_name.is_empty() {
                        return Err(ClientError::Configuration(
                            "migration TLS server name must not be empty",
                        ));
                    }
                    *server_name = configuration.server_name;
                    *client_config = configuration.client_config;
                } else {
                    if destination.certificate_subject.is_some() {
                        return Err(ClientError::Configuration(
                            "migration certificate subject requires a migration TLS policy",
                        ));
                    }
                    *server_name = destination.host.clone();
                }
                destination.secure_port
            }
        };
        if port == 0 {
            return Err(ClientError::Configuration(
                "migration target does not provide the required transport port",
            ));
        }
        options.endpoint = ConnectionEndpoint::Tcp {
            host: destination.host.clone(),
            port,
        };
        if let Some(sasl) = options.sasl.as_mut() {
            sasl.hostname = destination.host.clone();
        }
        Ok(options)
    }

    async fn connect_target_channels(
        &self,
        options: &ConnectOptions,
        generation: u64,
        seamless_requested: bool,
        source_version: u32,
    ) -> Result<bool, ClientError> {
        let main_spec = self
            .specs
            .iter()
            .find(|spec| spec.identity.channel_type == ChannelType::Main)
            .ok_or(ClientError::Internal("missing Main migration spec"))?;
        let source_session_id = self.session_id.load(Ordering::Acquire);
        let mut main = self
            .connect_target_channel(options, source_session_id, main_spec)
            .await?;
        let seamless =
            if seamless_requested && main.peer_supports(main_capability::SEAMLESS_MIGRATION) {
                main.write_message(
                    main_client::MIGRATE_DST_DO_SEAMLESS,
                    &source_version.to_le_bytes(),
                )
                .await?;
                await_seamless_decision(&mut main).await?
            } else {
                false
            };
        let mut replacements = Vec::with_capacity(self.specs.len());
        replacements.push((main_spec.identity, main));
        for spec in self
            .specs
            .iter()
            .filter(|spec| spec.identity.channel_type != ChannelType::Main)
        {
            replacements.push((
                spec.identity,
                self.connect_target_channel(options, source_session_id, spec)
                    .await?,
            ));
        }
        for (identity, channel) in replacements {
            self.replacement_senders
                .get(&identity)
                .ok_or(ClientError::Internal(
                    "missing migration replacement sender",
                ))?
                .send(MigrationReplacement {
                    generation,
                    seamless,
                    activate_immediately: false,
                    channel,
                })
                .await
                .map_err(|_| ClientError::TaskTerminated)?;
        }
        Ok(seamless)
    }

    async fn connect_target_channel(
        &self,
        options: &ConnectOptions,
        connection_id: u32,
        spec: &MigrationChannelSpec,
    ) -> Result<Channel<BoxedStream>, ClientError> {
        let transport = connect_transport(options).await?;
        let mut channel = timeout(
            options.handshake_timeout,
            link_connected_transport(
                transport,
                LinkParameters {
                    connection_id,
                    channel_type: spec.identity.channel_type,
                    channel_id: spec.identity.channel_id,
                    common_capabilities: spec.common_capabilities.clone(),
                    channel_capabilities: spec.channel_capabilities.clone(),
                    password: options.ticket.expose(),
                    maximum_message_body: options.maximum_message_body,
                    sasl: sasl_parameters(options),
                },
            ),
        )
        .await
        .map_err(|_| handshake_timeout_error("migration target Link timed out"))??;
        match &spec.initialization {
            MigrationChannelInitialization::None => {}
            MigrationChannelInitialization::Display => {
                initialize_display_channel(&mut channel).await?;
            }
            MigrationChannelInitialization::Record(clock) => {
                initialize_record_channel(&mut channel, clock).await?;
            }
        }
        Ok(channel)
    }
}

async fn await_seamless_decision(channel: &mut Channel<BoxedStream>) -> Result<bool, ClientError> {
    let mut body = Vec::new();
    let mut control = ControlState::new();
    loop {
        let header = channel.read_message(&mut body).await?;
        let message = IncomingMessage {
            header,
            body: &body,
        };
        if control.handle(channel, &message).await? == ControlDisposition::Consumed {
            continue;
        }
        match message.header.message_type {
            main_server::MIGRATE_DST_SEAMLESS_ACK => {
                if !message.body.is_empty() {
                    return Err(protocol_value_error("seamless migration ACK body"));
                }
                return Ok(true);
            }
            main_server::MIGRATE_DST_SEAMLESS_NACK => {
                if !message.body.is_empty() {
                    return Err(protocol_value_error("seamless migration NACK body"));
                }
                return Ok(false);
            }
            message_type => {
                return Err(ClientError::UnsupportedMessage {
                    channel: "migration target Main",
                    message_type,
                });
            }
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct AgentBootstrapState {
    connected: bool,
    outbound_tokens: u64,
    disconnect_reason: Option<u32>,
}

struct MainTaskContext {
    connection_generation: u64,
    mouse_mode_sender: watch::Sender<MouseMode>,
    server_identity_sender: watch::Sender<ServerIdentity>,
    progress: ProgressRegistry,
    identity: ChannelIdentity,
    agent_bootstrap: AgentBootstrapState,
    agent_paths: AgentTaskPaths,
    migration_manager: MigrationManager,
    control: ControlState,
}

/// A Ticket secret that zeroizes storage and never exposes its value through Debug.
#[derive(Clone)]
pub struct TicketSecret(Zeroizing<String>);

impl TicketSecret {
    /// Takes ownership of a Ticket password for the session lifetime.
    pub fn new(password: impl Into<String>) -> Self {
        Self(Zeroizing::new(password.into()))
    }

    /// Borrows the cleartext only at the link-authentication call boundary.
    fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for TicketSecret {
    /// Prevents credentials from entering diagnostics.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("TicketSecret([REDACTED])")
    }
}

/// Network address used for every independently linked SPICE channel.
#[derive(Debug, Clone)]
pub enum ConnectionEndpoint {
    Tcp {
        host: String,
        port: u16,
    },
    #[cfg(unix)]
    Unix {
        path: PathBuf,
    },
}

/// Inputs for one SPICE session attempt.
#[derive(Debug, Clone)]
pub struct ConnectOptions {
    pub endpoint: ConnectionEndpoint,
    pub ticket: TicketSecret,
    pub transport_security: TransportSecurity,
    #[cfg(feature = "tls-ring")]
    pub migration_tls_policy: Option<Arc<dyn MigrationTlsPolicy>>,
    pub sasl: Option<SaslOptions>,
    pub connect_timeout: Duration,
    pub handshake_timeout: Duration,
    pub maximum_message_body: usize,
    pub maximum_display_channels: usize,
    pub maximum_playback_channels: usize,
    pub maximum_record_channels: usize,
    pub maximum_usbredir_channels: usize,
    pub maximum_smartcard_channels: usize,
    pub maximum_port_channels: usize,
    pub enable_gl_scanout: bool,
}

impl ConnectOptions {
    /// Creates conservative options for a plain TCP SPICE endpoint.
    pub fn new(host: impl Into<String>, port: u16, ticket: TicketSecret) -> Self {
        Self {
            endpoint: ConnectionEndpoint::Tcp {
                host: host.into(),
                port,
            },
            ticket,
            transport_security: TransportSecurity::Plain,
            #[cfg(feature = "tls-ring")]
            migration_tls_policy: None,
            sasl: None,
            connect_timeout: Duration::from_secs(10),
            handshake_timeout: Duration::from_secs(10),
            maximum_message_body: DEFAULT_MAX_MESSAGE_BODY,
            maximum_display_channels: DEFAULT_MAX_DISPLAY_CHANNELS,
            maximum_playback_channels: DEFAULT_MAX_PLAYBACK_CHANNELS,
            maximum_record_channels: DEFAULT_MAX_RECORD_CHANNELS,
            maximum_usbredir_channels: DEFAULT_MAX_USBREDIR_CHANNELS,
            maximum_smartcard_channels: DEFAULT_MAX_SMARTCARD_CHANNELS,
            maximum_port_channels: DEFAULT_MAX_PORT_CHANNELS,
            enable_gl_scanout: true,
        }
    }

    /// Creates conservative options for a plain Unix-domain SPICE endpoint.
    #[cfg(unix)]
    pub fn new_unix(path: impl Into<PathBuf>, ticket: TicketSecret) -> Self {
        let mut options = Self::new("localhost", 1, ticket);
        options.endpoint = ConnectionEndpoint::Unix { path: path.into() };
        options
    }
}

/// Caller-owned TLS configuration selected for one authenticated migration destination.
#[cfg(feature = "tls-ring")]
#[derive(Clone)]
pub struct MigrationTlsConfiguration {
    pub server_name: String,
    pub client_config: Arc<tokio_rustls::rustls::ClientConfig>,
}

/// Resolves source-provided migration identity without weakening the caller's trust policy.
#[cfg(feature = "tls-ring")]
pub trait MigrationTlsPolicy: fmt::Debug + Send + Sync {
    fn configure(
        &self,
        destination: &oxide_spice_protocol::MigrationDestination,
    ) -> Result<MigrationTlsConfiguration, ClientError>;
}

/// Transport security selected before the SPICE Link Header is sent.
#[derive(Clone)]
pub enum TransportSecurity {
    /// Direct TCP, suitable for a trusted local or externally protected transport.
    Plain,
    /// Rustls with the explicitly enabled ring provider and caller-owned certificate policy.
    #[cfg(feature = "tls-ring")]
    Tls {
        server_name: String,
        client_config: Arc<tokio_rustls::rustls::ClientConfig>,
    },
}

impl fmt::Debug for TransportSecurity {
    /// Reports the security mode without expanding certificate stores or verifier state.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Plain => formatter.write_str("Plain"),
            #[cfg(feature = "tls-ring")]
            Self::Tls { server_name, .. } => formatter
                .debug_struct("Tls")
                .field("server_name", server_name)
                .finish_non_exhaustive(),
        }
    }
}

/// Observable lifecycle state for one attempt generation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionState {
    Running,
    Closing,
    Closed,
    Failed(ErrorCategory),
}

/// Latest server identity delivered by the negotiated Main extension.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ServerIdentity {
    pub name: Option<Arc<str>>,
    pub uuid: Option<[u8; 16]>,
}

/// An active session whose supervisor owns every child-channel task.
pub struct Session {
    connection_generation: u64,
    session_id: Arc<AtomicU32>,
    frame_receiver: FrameReceiver,
    topology_events: DisplayTopologyEvents,
    cursor_events: Option<CursorEvents>,
    inputs: Option<InputsHandle>,
    agent: AgentHandle,
    agent_events: Option<crate::agent::AgentEvents>,
    playback_channels: Vec<PlaybackChannel>,
    playback_packets: Option<PlaybackPackets>,
    record_channels: Vec<RecordChannel>,
    usbredir_channels: Vec<UsbRedirChannel>,
    smartcard_channels: Vec<SmartcardChannel>,
    port_channels: Vec<PortChannel>,
    mouse_mode_receiver: watch::Receiver<MouseMode>,
    server_identity: watch::Receiver<ServerIdentity>,
    state_receiver: watch::Receiver<SessionState>,
    cancel_sender: watch::Sender<bool>,
    supervisor: Option<JoinHandle<Result<(), ClientError>>>,
    #[cfg(unix)]
    gl_frame_events: GlFrameEvents,
}

impl fmt::Debug for Session {
    /// Reports identities and state without endpoint credentials.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Session")
            .field("connection_generation", &self.connection_generation)
            .field("session_id", &self.session_id())
            .field("state", &*self.state_receiver.borrow())
            .finish_non_exhaustive()
    }
}

impl Session {
    /// Connects Main, discovers channels, links the first Display channel, and starts ownership tasks.
    pub async fn connect(options: ConnectOptions) -> Result<Self, ClientError> {
        validate_options(&options)?;
        let mut common_capability_bits = vec![
            common_capability::AUTH_SELECTION,
            common_capability::AUTH_SPICE,
            common_capability::MINI_HEADER,
        ];
        if options.sasl.is_some() {
            common_capability_bits.push(common_capability::AUTH_SASL);
        }
        let common_capabilities = CapabilitySet::from_bits(common_capability_bits)?;

        let main_transport = connect_transport(&options).await?;
        let main_channel = timeout(
            options.handshake_timeout,
            link_connected_transport(
                main_transport,
                LinkParameters {
                    connection_id: 0,
                    channel_type: ChannelType::Main,
                    channel_id: 0,
                    common_capabilities: common_capabilities.clone(),
                    channel_capabilities: CapabilitySet::from_bits([
                        main_capability::SEMI_SEAMLESS_MIGRATION,
                        main_capability::NAME_AND_UUID,
                        main_capability::AGENT_CONNECTED_TOKENS,
                        main_capability::SEAMLESS_MIGRATION,
                    ])?,
                    password: options.ticket.expose(),
                    maximum_message_body: options.maximum_message_body,
                    sasl: sasl_parameters(&options),
                },
            ),
        )
        .await
        .map_err(|_| handshake_timeout_error("Main Link timed out"))??;
        let (
            main_channel,
            main_init,
            channels,
            initial_mouse_mode,
            agent_bootstrap,
            initial_server_identity,
            main_control,
        ) = timeout(options.handshake_timeout, bootstrap_main(main_channel))
            .await
            .map_err(|_| handshake_timeout_error("Main bootstrap timed out"))??;
        let display_ids: Vec<_> = channels
            .channels
            .iter()
            .filter(|channel| channel.channel_type == ChannelType::Display)
            .copied()
            .collect();
        if display_ids.is_empty() {
            return Err(ClientError::Configuration(
                "server advertised no Display channel",
            ));
        }
        if display_ids.len() > options.maximum_display_channels {
            return Err(ClientError::ChannelLimit {
                channel: "Display",
                advertised: display_ids.len(),
                maximum: options.maximum_display_channels,
            });
        }
        let display_capability_bits = vec![
            display_capability::SIZED_STREAM,
            display_capability::MONITORS_CONFIG,
            display_capability::COMPOSITE,
            display_capability::A8_SURFACE,
            display_capability::STREAM_REPORT,
            display_capability::LZ4_COMPRESSION,
            display_capability::PREFERRED_COMPRESSION,
            display_capability::MULTI_CODEC,
            display_capability::CODEC_MJPEG,
            display_capability::CODEC_VP8,
            display_capability::CODEC_H264,
            display_capability::PREFERRED_VIDEO_CODEC,
            display_capability::CODEC_VP9,
            display_capability::CODEC_H265,
        ];
        #[cfg(target_os = "linux")]
        let display_capability_bits = if options.enable_gl_scanout
            && matches!(options.endpoint, ConnectionEndpoint::Unix { .. })
        {
            display_capability_bits
                .into_iter()
                .chain([
                    display_capability::GL_SCANOUT,
                    display_capability::GL_SCANOUT2,
                ])
                .collect::<Vec<_>>()
        } else {
            display_capability_bits
        };
        let display_capabilities = CapabilitySet::from_bits(display_capability_bits)?;
        let mut display_channels = Vec::with_capacity(display_ids.len());
        for display in &display_ids {
            let channel = timeout(
                options.handshake_timeout,
                link_child_channel(
                    &options,
                    main_init.session_id,
                    *display,
                    common_capabilities.clone(),
                    display_capabilities.clone(),
                ),
            )
            .await
            .map_err(|_| handshake_timeout_error("Display Link timed out"))??;
            display_channels.push((channel, display.channel_id));
        }

        let inputs_channel = match first_channel(&channels, ChannelType::Inputs) {
            Some(inputs) => Some((
                timeout(
                    options.handshake_timeout,
                    link_child_channel(
                        &options,
                        main_init.session_id,
                        inputs,
                        common_capabilities.clone(),
                        CapabilitySet::from_bits([inputs_capability::KEY_SCANCODE])?,
                    ),
                )
                .await
                .map_err(|_| handshake_timeout_error("Inputs Link timed out"))??,
                inputs.channel_id,
            )),
            None => None,
        };
        let cursor_ids: Vec<_> = channels
            .channels
            .iter()
            .filter(|channel| {
                channel.channel_type == ChannelType::Cursor
                    && display_ids
                        .iter()
                        .any(|display| display.channel_id == channel.channel_id)
            })
            .copied()
            .collect();
        let mut cursor_channels = Vec::with_capacity(cursor_ids.len());
        for cursor in cursor_ids {
            let channel = timeout(
                options.handshake_timeout,
                link_child_channel(
                    &options,
                    main_init.session_id,
                    cursor,
                    common_capabilities.clone(),
                    CapabilitySet::new(),
                ),
            )
            .await
            .map_err(|_| handshake_timeout_error("Cursor Link timed out"))??;
            cursor_channels.push((channel, cursor.channel_id));
        }

        let playback_ids: Vec<_> = channels
            .channels
            .iter()
            .filter(|channel| channel.channel_type == ChannelType::Playback)
            .copied()
            .collect();
        if playback_ids.len() > options.maximum_playback_channels {
            return Err(ClientError::ChannelLimit {
                channel: "Playback",
                advertised: playback_ids.len(),
                maximum: options.maximum_playback_channels,
            });
        }
        let mut playback_transports = Vec::with_capacity(playback_ids.len());
        for playback in playback_ids {
            let channel = timeout(
                options.handshake_timeout,
                link_child_channel(
                    &options,
                    main_init.session_id,
                    playback,
                    common_capabilities.clone(),
                    CapabilitySet::from_bits([
                        playback_capability::VOLUME,
                        playback_capability::LATENCY,
                        playback_capability::OPUS,
                    ])?,
                ),
            )
            .await
            .map_err(|_| handshake_timeout_error("Playback Link timed out"))??;
            playback_transports.push((channel, playback.channel_id));
        }

        let record_ids: Vec<_> = channels
            .channels
            .iter()
            .filter(|channel| channel.channel_type == ChannelType::Record)
            .copied()
            .collect();
        if record_ids.len() > options.maximum_record_channels {
            return Err(ClientError::ChannelLimit {
                channel: "Record",
                advertised: record_ids.len(),
                maximum: options.maximum_record_channels,
            });
        }
        let mut record_transports = Vec::with_capacity(record_ids.len());
        for record in record_ids {
            let channel = timeout(
                options.handshake_timeout,
                link_child_channel(
                    &options,
                    main_init.session_id,
                    record,
                    common_capabilities.clone(),
                    CapabilitySet::from_bits([record_capability::VOLUME, record_capability::OPUS])?,
                ),
            )
            .await
            .map_err(|_| handshake_timeout_error("Record Link timed out"))??;
            record_transports.push((channel, record.channel_id));
        }

        let usbredir_ids: Vec<_> = channels
            .channels
            .iter()
            .filter(|channel| channel.channel_type == ChannelType::UsbRedirection)
            .copied()
            .collect();
        if usbredir_ids.len() > options.maximum_usbredir_channels {
            return Err(ClientError::ChannelLimit {
                channel: "USB redirection",
                advertised: usbredir_ids.len(),
                maximum: options.maximum_usbredir_channels,
            });
        }
        let mut usbredir_transports = Vec::with_capacity(usbredir_ids.len());
        for usbredir in usbredir_ids {
            let channel = timeout(
                options.handshake_timeout,
                link_child_channel(
                    &options,
                    main_init.session_id,
                    usbredir,
                    common_capabilities.clone(),
                    CapabilitySet::from_bits([spicevmc_capability::DATA_COMPRESS_LZ4])?,
                ),
            )
            .await
            .map_err(|_| handshake_timeout_error("USB redirection Link timed out"))??;
            usbredir_transports.push((channel, usbredir.channel_id));
        }

        let smartcard_ids: Vec<_> = channels
            .channels
            .iter()
            .filter(|channel| channel.channel_type == ChannelType::Smartcard)
            .copied()
            .collect();
        if smartcard_ids.len() > options.maximum_smartcard_channels {
            return Err(ClientError::ChannelLimit {
                channel: "Smartcard",
                advertised: smartcard_ids.len(),
                maximum: options.maximum_smartcard_channels,
            });
        }
        let mut smartcard_transports = Vec::with_capacity(smartcard_ids.len());
        for smartcard in smartcard_ids {
            let channel = timeout(
                options.handshake_timeout,
                link_child_channel(
                    &options,
                    main_init.session_id,
                    smartcard,
                    common_capabilities.clone(),
                    CapabilitySet::new(),
                ),
            )
            .await
            .map_err(|_| handshake_timeout_error("Smartcard Link timed out"))??;
            smartcard_transports.push((channel, smartcard.channel_id));
        }

        let port_ids: Vec<_> = channels
            .channels
            .iter()
            .filter(|channel| {
                matches!(
                    channel.channel_type,
                    ChannelType::Port | ChannelType::WebDav
                )
            })
            .copied()
            .collect();
        if port_ids.len() > options.maximum_port_channels {
            return Err(ClientError::ChannelLimit {
                channel: "Port/WebDAV",
                advertised: port_ids.len(),
                maximum: options.maximum_port_channels,
            });
        }
        let mut port_transports = Vec::with_capacity(port_ids.len());
        for port in port_ids {
            let channel = timeout(
                options.handshake_timeout,
                link_child_channel(
                    &options,
                    main_init.session_id,
                    port,
                    common_capabilities.clone(),
                    CapabilitySet::from_bits([spicevmc_capability::DATA_COMPRESS_LZ4])?,
                ),
            )
            .await
            .map_err(|_| handshake_timeout_error("Port Link timed out"))??;
            port_transports.push((channel, port));
        }

        let connection_generation = INITIAL_CONNECTION_GENERATION;
        let (agent, agent_events, agent_task_paths) = agent_paths(connection_generation);
        let (cancel_sender, cancel_receiver) = watch::channel(false);
        let (frame_sender, frame_receiver) = watch::channel(None);
        #[cfg(unix)]
        let (gl_frame_sender, gl_frame_events) = gl_frame_events();
        let (topology_sender, topology_events) = topology_events();
        let (mouse_mode_sender, mouse_mode_receiver) =
            watch::channel(initial_mouse_mode.current_mode);
        let (server_identity_sender, server_identity) = watch::channel(initial_server_identity);
        let (inputs, linked_inputs) = match inputs_channel {
            Some((channel, channel_id)) => {
                let raw_scancodes_supported =
                    channel.peer_supports(inputs_capability::KEY_SCANCODE);
                let (handle, paths) =
                    input_paths(raw_scancodes_supported, mouse_mode_receiver.clone());
                (Some(handle), Some((channel, paths, channel_id)))
            }
            None => (None, None),
        };
        let (cursor_events, linked_cursors) = if cursor_channels.is_empty() {
            (None, Vec::new())
        } else {
            let (sender, events) = cursor_events();
            let linked = cursor_channels
                .into_iter()
                .map(|(channel, channel_id)| (channel, sender.clone(), channel_id))
                .collect();
            (Some(events), linked)
        };
        let (playback_channels, linked_playbacks, playback_packet_events) =
            if playback_transports.is_empty() {
                (Vec::new(), Vec::new(), None)
            } else {
                let (packet_sender, packets) = playback_packets();
                let mut handles = Vec::with_capacity(playback_transports.len());
                let mut linked = Vec::with_capacity(playback_transports.len());
                for (channel, channel_id) in playback_transports {
                    let (handle, state_sender, audio_sender) =
                        playback_channel(connection_generation, channel_id);
                    handles.push(handle);
                    linked.push(LinkedPlayback {
                        channel,
                        state_sender,
                        audio_sender,
                        packet_sender: packet_sender.clone(),
                        channel_id,
                    });
                }
                (handles, linked, Some(packets))
            };
        let record_clock = Arc::new(RecordClock::new());
        let mut record_channels = Vec::with_capacity(record_transports.len());
        let mut linked_records = Vec::with_capacity(record_transports.len());
        for (channel, channel_id) in record_transports {
            let (handle, paths) =
                record_channel(connection_generation, channel_id, record_clock.clone());
            record_channels.push(handle);
            linked_records.push(LinkedRecord {
                channel,
                paths,
                clock: record_clock.clone(),
                channel_id,
            });
        }
        let mut usbredir_channels = Vec::with_capacity(usbredir_transports.len());
        let mut linked_usbredir = Vec::with_capacity(usbredir_transports.len());
        for (channel, channel_id) in usbredir_transports {
            let (handle, paths) = usbredir_channel(connection_generation, channel_id);
            usbredir_channels.push(handle);
            linked_usbredir.push(LinkedUsbRedir {
                channel,
                paths,
                channel_id,
            });
        }
        let mut smartcard_channels = Vec::with_capacity(smartcard_transports.len());
        let mut linked_smartcards = Vec::with_capacity(smartcard_transports.len());
        for (channel, channel_id) in smartcard_transports {
            let (handle, paths) = smartcard_channel(connection_generation, channel_id);
            smartcard_channels.push(handle);
            linked_smartcards.push(LinkedSmartcard {
                channel,
                paths,
                channel_id,
            });
        }
        let mut port_channels = Vec::with_capacity(port_transports.len());
        let mut linked_ports = Vec::with_capacity(port_transports.len());
        for (channel, identity) in port_transports {
            let (handle, paths) = port_channel(
                connection_generation,
                identity.channel_type,
                identity.channel_id,
            );
            port_channels.push(handle);
            linked_ports.push(LinkedPort {
                channel,
                paths,
                channel_type: identity.channel_type,
                channel_id: identity.channel_id,
            });
        }
        let (state_sender, state_receiver) = watch::channel(SessionState::Running);
        let mut linked_channels = LinkedChannels {
            main: main_channel,
            displays: display_channels,
            inputs: linked_inputs,
            cursors: linked_cursors,
            playbacks: linked_playbacks,
            records: linked_records,
            usbredir: linked_usbredir,
            smartcards: linked_smartcards,
            ports: linked_ports,
        };
        let session_id = Arc::new(AtomicU32::new(main_init.session_id));
        let migration_manager = install_migration_manager(
            &mut linked_channels,
            options.clone(),
            session_id.clone(),
            cancel_sender.subscribe(),
        );
        let supervisor = tokio::spawn(supervise_channels(
            linked_channels,
            connection_generation,
            SupervisorSignals {
                cancel_sender: cancel_sender.clone(),
                cancel_receiver,
                frame_sender,
                topology_sender,
                mouse_mode_sender,
                mouse_mode_receiver: mouse_mode_receiver.clone(),
                server_identity_sender,
                state_sender,
                image_decode_slots: image_decode_slots(),
                glz_window: glz_window(),
                agent_paths: agent_task_paths,
                #[cfg(unix)]
                gl_frame_sender,
                migration_manager,
            },
            agent_bootstrap,
            main_control,
        ));

        Ok(Self {
            connection_generation,
            session_id,
            frame_receiver,
            topology_events,
            cursor_events,
            inputs,
            agent,
            agent_events: Some(agent_events),
            playback_channels,
            playback_packets: playback_packet_events,
            record_channels,
            usbredir_channels,
            smartcard_channels,
            port_channels,
            mouse_mode_receiver,
            server_identity,
            state_receiver,
            cancel_sender,
            supervisor: Some(supervisor),
            #[cfg(unix)]
            gl_frame_events,
        })
    }

    /// Returns the Main-provided identity used for child channel links.
    pub fn session_id(&self) -> u32 {
        self.session_id.load(Ordering::Acquire)
    }

    /// Returns the latest lifecycle state without waiting.
    pub fn state(&self) -> SessionState {
        *self.state_receiver.borrow()
    }

    /// Returns a cloneable bounded Inputs API when the server advertised that channel.
    pub fn inputs(&self) -> Option<InputsHandle> {
        self.inputs.clone()
    }

    /// Returns the dynamic guest Agent handle even when the Agent is currently disconnected.
    pub fn agent(&self) -> AgentHandle {
        self.agent.clone()
    }

    /// Transfers ownership of the reliable Agent event stream once.
    pub fn take_agent_events(&mut self) -> Option<crate::agent::AgentEvents> {
        self.agent_events.take()
    }

    /// Returns every linked raw Playback channel and its latest stream state.
    pub fn playback_channels(&self) -> &[PlaybackChannel] {
        &self.playback_channels
    }

    /// Transfers ownership of the bounded raw Playback packet stream once.
    pub fn take_playback_packets(&mut self) -> Option<PlaybackPackets> {
        self.playback_packets.take()
    }

    /// Transfers unique ownership of every raw Record host API once.
    pub fn take_record_channels(&mut self) -> Vec<RecordChannel> {
        std::mem::take(&mut self.record_channels)
    }

    /// Transfers unique ownership of every raw usbredir stream once.
    pub fn take_usbredir_channels(&mut self) -> Vec<UsbRedirChannel> {
        std::mem::take(&mut self.usbredir_channels)
    }

    /// Transfers unique ownership of every typed Smartcard channel once.
    pub fn take_smartcard_channels(&mut self) -> Vec<SmartcardChannel> {
        std::mem::take(&mut self.smartcard_channels)
    }

    /// Transfers unique ownership of every generic Port/WebDAV host API once.
    pub fn take_port_channels(&mut self) -> Vec<PortChannel> {
        std::mem::take(&mut self.port_channels)
    }

    /// Returns the server-confirmed pointer mode used to gate absolute motion.
    pub fn mouse_mode(&self) -> MouseMode {
        *self.mouse_mode_receiver.borrow()
    }

    /// Returns the latest server name and UUID received on Main.
    pub fn server_identity(&self) -> ServerIdentity {
        self.server_identity.borrow().clone()
    }

    /// Returns an independently consumable cursor event stream when advertised by the server.
    pub fn cursor_events(&self) -> Option<CursorEvents> {
        self.cursor_events.clone()
    }

    /// Returns an independently consumable stream of complete monitor topologies.
    pub fn display_topology_events(&self) -> DisplayTopologyEvents {
        self.topology_events.clone()
    }

    /// Waits for a coalesced notification that the shared surface changed.
    pub async fn next_frame(&mut self) -> Result<FrameEvent, ClientError> {
        loop {
            tokio::select! {
                frame = next_frame(&mut self.frame_receiver) => return frame,
                changed = self.state_receiver.changed() => {
                    if changed.is_err() {
                        return Err(ClientError::TaskTerminated);
                    }
                    match *self.state_receiver.borrow_and_update() {
                        SessionState::Running => {}
                        SessionState::Closing | SessionState::Closed => {
                            return Err(ClientError::Cancelled);
                        }
                        SessionState::Failed(_) => return Err(ClientError::TaskTerminated),
                    }
                }
            }
        }
    }

    /// Waits for the next DMA-BUF dirty region and transfers its completion token to the caller.
    #[cfg(unix)]
    pub async fn next_gl_frame(&mut self) -> Result<crate::display::GlFrame, ClientError> {
        self.gl_frame_events.next().await
    }

    /// Cancels all channel owners and waits until the supervisor has reaped them.
    pub async fn shutdown(mut self) -> Result<(), ClientError> {
        self.cancel_sender.send_replace(true);
        self.join_supervisor().await
    }

    /// Waits for remote termination without initiating cancellation.
    pub async fn wait(mut self) -> Result<(), ClientError> {
        self.join_supervisor().await
    }

    /// Joins the single supervisor that in turn joins every channel task.
    async fn join_supervisor(&mut self) -> Result<(), ClientError> {
        let supervisor = self.supervisor.take().ok_or(ClientError::TaskTerminated)?;
        supervisor.await.map_err(|_| ClientError::TaskTerminated)?
    }
}

impl Drop for Session {
    /// Starts cooperative cancellation even when the caller cannot await shutdown.
    fn drop(&mut self) {
        self.cancel_sender.send_replace(true);
    }
}

struct ConnectedTransport {
    stream: BoxedStream,
    #[cfg(unix)]
    received_file_descriptors: Option<ReceivedFileDescriptors>,
}

/// Connects one transport within the attempt's explicit deadline.
async fn connect_transport(options: &ConnectOptions) -> Result<ConnectedTransport, ClientError> {
    match &options.endpoint {
        ConnectionEndpoint::Tcp { host, port } => {
            let stream = timeout(
                options.connect_timeout,
                TcpStream::connect((host.as_str(), *port)),
            )
            .await
            .map_err(|_| {
                std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "SPICE TCP connection timed out",
                )
            })??;
            match &options.transport_security {
                TransportSecurity::Plain => Ok(ConnectedTransport {
                    stream: Box::pin(stream),
                    #[cfg(unix)]
                    received_file_descriptors: None,
                }),
                #[cfg(feature = "tls-ring")]
                TransportSecurity::Tls {
                    server_name,
                    client_config,
                } => {
                    let server_name =
                        tokio_rustls::rustls::pki_types::ServerName::try_from(server_name.clone())
                            .map_err(|_| ClientError::Configuration("invalid TLS server name"))?;
                    let connector = tokio_rustls::TlsConnector::from(client_config.clone());
                    let tls_stream = timeout(
                        options.connect_timeout,
                        connector.connect(server_name, stream),
                    )
                    .await
                    .map_err(|_| {
                        std::io::Error::new(
                            std::io::ErrorKind::TimedOut,
                            "SPICE TLS handshake timed out",
                        )
                    })?
                    .map_err(|error| ClientError::Tls(error.to_string()))?;
                    Ok(ConnectedTransport {
                        stream: Box::pin(tls_stream),
                        #[cfg(unix)]
                        received_file_descriptors: None,
                    })
                }
            }
        }
        #[cfg(unix)]
        ConnectionEndpoint::Unix { path } => {
            if !matches!(options.transport_security, TransportSecurity::Plain) {
                return Err(ClientError::Configuration(
                    "TLS is not supported over a Unix-domain SPICE endpoint",
                ));
            }
            let stream = timeout(options.connect_timeout, UnixStream::connect(path))
                .await
                .map_err(|_| {
                    std::io::Error::new(
                        std::io::ErrorKind::TimedOut,
                        "SPICE Unix connection timed out",
                    )
                })??;
            let (stream, received_file_descriptors) = UnixFdStream::new(stream);
            Ok(ConnectedTransport {
                stream: Box::pin(stream),
                received_file_descriptors: Some(received_file_descriptors),
            })
        }
    }
}

async fn link_connected_transport(
    transport: ConnectedTransport,
    parameters: LinkParameters<'_>,
) -> Result<Channel<BoxedStream>, ClientError> {
    #[cfg(unix)]
    if let Some(received_file_descriptors) = transport.received_file_descriptors {
        return link_channel_with_file_descriptors(
            transport.stream,
            parameters,
            received_file_descriptors,
        )
        .await;
    }
    link_channel(transport.stream, parameters).await
}

/// Links one advertised child channel over a fresh transport.
async fn link_child_channel(
    options: &ConnectOptions,
    session_id: u32,
    channel_id: ChannelId,
    common_capabilities: CapabilitySet,
    channel_capabilities: CapabilitySet,
) -> Result<Channel<BoxedStream>, ClientError> {
    let transport = connect_transport(options).await?;
    link_connected_transport(
        transport,
        LinkParameters {
            connection_id: session_id,
            channel_type: channel_id.channel_type,
            channel_id: channel_id.channel_id,
            common_capabilities,
            channel_capabilities,
            password: options.ticket.expose(),
            maximum_message_body: options.maximum_message_body,
            sasl: sasl_parameters(options),
        },
    )
    .await
}

fn sasl_parameters(options: &ConnectOptions) -> Option<SaslParameters<'_>> {
    options.sasl.as_ref().map(|sasl| SaslParameters {
        options: sasl,
        require_security_layer: matches!(
            (&options.endpoint, &options.transport_security),
            (ConnectionEndpoint::Tcp { .. }, TransportSecurity::Plain)
        ),
    })
}

fn install_migration_manager(
    channels: &mut LinkedChannels,
    options: ConnectOptions,
    session_id: Arc<AtomicU32>,
    cancel: watch::Receiver<bool>,
) -> MigrationManager {
    let active_generation = Arc::new(AtomicU64::new(0));
    let mut specs = Vec::new();
    let mut replacement_senders = HashMap::new();
    register_migration_channel(
        &mut channels.main,
        ChannelIdentity {
            channel_type: ChannelType::Main,
            channel_id: 0,
        },
        MigrationChannelInitialization::None,
        &cancel,
        &active_generation,
        &mut specs,
        &mut replacement_senders,
    );
    for (channel, channel_id) in &mut channels.displays {
        register_migration_channel(
            channel,
            ChannelIdentity {
                channel_type: ChannelType::Display,
                channel_id: *channel_id,
            },
            MigrationChannelInitialization::Display,
            &cancel,
            &active_generation,
            &mut specs,
            &mut replacement_senders,
        );
    }
    if let Some((channel, _, channel_id)) = &mut channels.inputs {
        register_migration_channel(
            channel,
            ChannelIdentity {
                channel_type: ChannelType::Inputs,
                channel_id: *channel_id,
            },
            MigrationChannelInitialization::None,
            &cancel,
            &active_generation,
            &mut specs,
            &mut replacement_senders,
        );
    }
    for (channel, _, channel_id) in &mut channels.cursors {
        register_migration_channel(
            channel,
            ChannelIdentity {
                channel_type: ChannelType::Cursor,
                channel_id: *channel_id,
            },
            MigrationChannelInitialization::None,
            &cancel,
            &active_generation,
            &mut specs,
            &mut replacement_senders,
        );
    }
    for playback in &mut channels.playbacks {
        register_migration_channel(
            &mut playback.channel,
            ChannelIdentity {
                channel_type: ChannelType::Playback,
                channel_id: playback.channel_id,
            },
            MigrationChannelInitialization::None,
            &cancel,
            &active_generation,
            &mut specs,
            &mut replacement_senders,
        );
    }
    for record in &mut channels.records {
        register_migration_channel(
            &mut record.channel,
            ChannelIdentity {
                channel_type: ChannelType::Record,
                channel_id: record.channel_id,
            },
            MigrationChannelInitialization::Record(record.clock.clone()),
            &cancel,
            &active_generation,
            &mut specs,
            &mut replacement_senders,
        );
    }
    for usbredir in &mut channels.usbredir {
        register_migration_channel(
            &mut usbredir.channel,
            ChannelIdentity {
                channel_type: ChannelType::UsbRedirection,
                channel_id: usbredir.channel_id,
            },
            MigrationChannelInitialization::None,
            &cancel,
            &active_generation,
            &mut specs,
            &mut replacement_senders,
        );
    }
    for smartcard in &mut channels.smartcards {
        register_migration_channel(
            &mut smartcard.channel,
            ChannelIdentity {
                channel_type: ChannelType::Smartcard,
                channel_id: smartcard.channel_id,
            },
            MigrationChannelInitialization::None,
            &cancel,
            &active_generation,
            &mut specs,
            &mut replacement_senders,
        );
    }
    for port in &mut channels.ports {
        register_migration_channel(
            &mut port.channel,
            ChannelIdentity {
                channel_type: port.channel_type,
                channel_id: port.channel_id,
            },
            MigrationChannelInitialization::None,
            &cancel,
            &active_generation,
            &mut specs,
            &mut replacement_senders,
        );
    }
    MigrationManager {
        options,
        session_id,
        specs: specs.into(),
        replacement_senders: Arc::new(replacement_senders),
        active_generation,
        prepare_gate: Arc::new(AsyncMutex::new(())),
    }
}

fn register_migration_channel(
    channel: &mut Channel<BoxedStream>,
    identity: ChannelIdentity,
    initialization: MigrationChannelInitialization,
    cancel: &watch::Receiver<bool>,
    active_generation: &Arc<AtomicU64>,
    specs: &mut Vec<MigrationChannelSpec>,
    senders: &mut HashMap<ChannelIdentity, mpsc::Sender<MigrationReplacement<BoxedStream>>>,
) {
    let (common_capabilities, channel_capabilities) = channel.local_capabilities();
    specs.push(MigrationChannelSpec {
        identity,
        common_capabilities,
        channel_capabilities,
        initialization,
    });
    let (sender, receiver) = mpsc::channel(4);
    channel.install_migration_path(receiver, cancel.clone(), active_generation.clone());
    let replaced = senders.insert(identity, sender);
    debug_assert!(
        replaced.is_none(),
        "channel identities were validated earlier"
    );
}

/// Finds the first advertised instance of one channel type.
fn first_channel(channels: &ChannelsList, channel_type: ChannelType) -> Option<ChannelId> {
    channels
        .channels
        .iter()
        .find(|channel| channel.channel_type == channel_type)
        .copied()
}

/// Validates values whose failure is independent of network state.
fn validate_options(options: &ConnectOptions) -> Result<(), ClientError> {
    match &options.endpoint {
        ConnectionEndpoint::Tcp { host, port } => {
            if host.is_empty() {
                return Err(ClientError::Configuration("host must not be empty"));
            }
            if *port == 0 {
                return Err(ClientError::Configuration("port must not be zero"));
            }
        }
        #[cfg(unix)]
        ConnectionEndpoint::Unix { path } => {
            if path.as_os_str().is_empty() {
                return Err(ClientError::Configuration(
                    "Unix socket path must not be empty",
                ));
            }
        }
    }
    if let Some(sasl) = options.sasl.as_ref() {
        sasl.validate()?;
    }
    if options.maximum_message_body == 0 {
        return Err(ClientError::Configuration(
            "maximum message body must not be zero",
        ));
    }
    if options.maximum_display_channels == 0 {
        return Err(ClientError::Configuration(
            "maximum Display channels must not be zero",
        ));
    }
    if options.maximum_playback_channels == 0 {
        return Err(ClientError::Configuration(
            "maximum Playback channels must not be zero",
        ));
    }
    if options.maximum_record_channels == 0 {
        return Err(ClientError::Configuration(
            "maximum Record channels must not be zero",
        ));
    }
    if options.maximum_usbredir_channels == 0 {
        return Err(ClientError::Configuration(
            "maximum USB redirection channels must not be zero",
        ));
    }
    if options.maximum_smartcard_channels == 0 {
        return Err(ClientError::Configuration(
            "maximum Smartcard channels must not be zero",
        ));
    }
    if options.maximum_port_channels == 0 {
        return Err(ClientError::Configuration(
            "maximum Port channels must not be zero",
        ));
    }
    Ok(())
}

/// Creates a timeout I/O error without embedding endpoint or credential data.
fn handshake_timeout_error(message: &'static str) -> ClientError {
    std::io::Error::new(std::io::ErrorKind::TimedOut, message).into()
}

fn protocol_value_error(context: &'static str) -> ClientError {
    oxide_spice_protocol::DecodeError::new(
        oxide_spice_protocol::DecodeErrorKind::InvalidValue,
        0,
        context,
    )
    .into()
}

fn resource_limit_error(context: &'static str) -> ClientError {
    oxide_spice_protocol::DecodeError::new(
        oxide_spice_protocol::DecodeErrorKind::ResourceLimit,
        0,
        context,
    )
    .into()
}

/// Reads Main Init, requests attachment, and returns the first complete channel list.
async fn bootstrap_main<S>(
    mut channel: Channel<S>,
) -> Result<
    (
        Channel<S>,
        MainInit,
        ChannelsList,
        MouseModeState,
        AgentBootstrapState,
        ServerIdentity,
        ControlState,
    ),
    ClientError,
>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let mut control = ControlState::new();
    let mut message_body = Vec::new();
    let main_init = loop {
        let header = channel.read_message(&mut message_body).await?;
        let message = IncomingMessage {
            header,
            body: &message_body,
        };
        if control.handle(&mut channel, &message).await? == ControlDisposition::Consumed {
            continue;
        }
        if message.header.message_type == main_server::INIT {
            break MainInit::decode(message.body)?;
        }
        return Err(ClientError::UnsupportedMessage {
            channel: "main bootstrap",
            message_type: message.header.message_type,
        });
    };
    channel
        .write_message(main_client::ATTACH_CHANNELS, &[])
        .await?;
    let mut mouse_mode = main_init.mouse_mode_state()?;
    let mut agent_state = AgentBootstrapState {
        connected: main_init.agent_connected,
        outbound_tokens: u64::from(main_init.agent_tokens),
        disconnect_reason: None,
    };
    let mut server_identity = ServerIdentity::default();
    request_client_mouse_mode(&mut channel, mouse_mode).await?;
    let channels = loop {
        let header = channel.read_message(&mut message_body).await?;
        let message = IncomingMessage {
            header,
            body: &message_body,
        };
        if control.handle(&mut channel, &message).await? == ControlDisposition::Consumed {
            continue;
        }
        match message.header.message_type {
            main_server::CHANNELS_LIST => break ChannelsList::decode(message.body)?,
            main_server::MOUSE_MODE => {
                mouse_mode = MouseModeState::decode(message.body)?;
            }
            main_server::AGENT_CONNECTED => {
                if !message.body.is_empty() {
                    return Err(protocol_value_error("Main Agent Connected body"));
                }
                agent_state.connected = true;
                agent_state.disconnect_reason = None;
            }
            main_server::AGENT_CONNECTED_TOKENS => {
                agent_state.connected = true;
                agent_state.outbound_tokens = u64::from(decode_agent_u32(
                    message.body,
                    "Main Agent Connected tokens",
                )?);
                agent_state.disconnect_reason = None;
            }
            main_server::AGENT_DISCONNECTED => {
                agent_state.connected = false;
                agent_state.disconnect_reason = Some(decode_agent_u32(
                    message.body,
                    "Main Agent disconnect reason",
                )?);
            }
            main_server::AGENT_TOKEN => {
                let tokens = u64::from(decode_agent_u32(message.body, "Main Agent tokens")?);
                agent_state.outbound_tokens = agent_state
                    .outbound_tokens
                    .checked_add(tokens)
                    .ok_or_else(|| resource_limit_error("Main Agent token count"))?;
            }
            main_server::AGENT_DATA => {
                return Err(protocol_value_error("Agent data before Agent Start"));
            }
            main_server::MULTI_MEDIA_TIME => {}
            main_server::NAME => {
                server_identity.name = Some(Arc::from(decode_main_name(message.body)?));
            }
            main_server::UUID => {
                server_identity.uuid = Some(decode_main_uuid(message.body)?);
            }
            message_type => {
                return Err(ClientError::UnsupportedMessage {
                    channel: "main discovery",
                    message_type,
                });
            }
        }
    };
    Ok((
        channel,
        main_init,
        channels,
        mouse_mode,
        agent_state,
        server_identity,
        control,
    ))
}

/// Requests absolute client mouse mode only when the server currently permits it.
async fn request_client_mouse_mode<S>(
    channel: &mut Channel<S>,
    state: MouseModeState,
) -> Result<(), ClientError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    if state.supports(MouseMode::Client) && state.current_mode != MouseMode::Client {
        channel
            .write_message(
                main_client::MOUSE_MODE_REQUEST,
                &encode_mouse_mode_request(MouseMode::Client),
            )
            .await?;
    }
    Ok(())
}

/// Owns all channel tasks and guarantees cancellation plus join on first terminal result.
async fn supervise_channels(
    channels: LinkedChannels,
    connection_generation: u64,
    signals: SupervisorSignals,
    agent_bootstrap: AgentBootstrapState,
    main_control: ControlState,
) -> Result<(), ClientError> {
    let LinkedChannels {
        main,
        displays,
        inputs,
        cursors,
        playbacks,
        records,
        usbredir,
        smartcards,
        ports,
    } = channels;
    let SupervisorSignals {
        cancel_sender,
        cancel_receiver,
        frame_sender,
        topology_sender,
        mouse_mode_sender,
        mouse_mode_receiver,
        server_identity_sender,
        state_sender,
        image_decode_slots,
        glz_window,
        agent_paths,
        migration_manager,
        #[cfg(unix)]
        gl_frame_sender,
    } = signals;
    let main_identity = ChannelIdentity {
        channel_type: ChannelType::Main,
        channel_id: 0,
    };
    let mut progress_channels = vec![(main_identity, main.received_serial())];
    progress_channels.extend(displays.iter().map(|(channel, channel_id)| {
        (
            ChannelIdentity {
                channel_type: ChannelType::Display,
                channel_id: *channel_id,
            },
            channel.received_serial(),
        )
    }));
    if let Some((channel, _, channel_id)) = &inputs {
        progress_channels.push((
            ChannelIdentity {
                channel_type: ChannelType::Inputs,
                channel_id: *channel_id,
            },
            channel.received_serial(),
        ));
    }
    progress_channels.extend(cursors.iter().map(|(channel, _, channel_id)| {
        (
            ChannelIdentity {
                channel_type: ChannelType::Cursor,
                channel_id: *channel_id,
            },
            channel.received_serial(),
        )
    }));
    progress_channels.extend(playbacks.iter().map(|playback| {
        (
            ChannelIdentity {
                channel_type: ChannelType::Playback,
                channel_id: playback.channel_id,
            },
            playback.channel.received_serial(),
        )
    }));
    progress_channels.extend(records.iter().map(|record| {
        (
            ChannelIdentity {
                channel_type: ChannelType::Record,
                channel_id: record.channel_id,
            },
            record.channel.received_serial(),
        )
    }));
    progress_channels.extend(usbredir.iter().map(|usbredir| {
        (
            ChannelIdentity {
                channel_type: ChannelType::UsbRedirection,
                channel_id: usbredir.channel_id,
            },
            usbredir.channel.received_serial(),
        )
    }));
    progress_channels.extend(smartcards.iter().map(|smartcard| {
        (
            ChannelIdentity {
                channel_type: ChannelType::Smartcard,
                channel_id: smartcard.channel_id,
            },
            smartcard.channel.received_serial(),
        )
    }));
    progress_channels.extend(ports.iter().map(|port| {
        (
            ChannelIdentity {
                channel_type: port.channel_type,
                channel_id: port.channel_id,
            },
            port.channel.received_serial(),
        )
    }));
    let progress = ProgressRegistry::new(progress_channels)?;
    let mut supervisor_cancel = cancel_sender.subscribe();
    let mut tasks = JoinSet::new();
    tasks.spawn(run_main(
        main,
        cancel_receiver.clone(),
        MainTaskContext {
            connection_generation,
            mouse_mode_sender,
            server_identity_sender,
            progress: progress.clone(),
            identity: main_identity,
            agent_bootstrap,
            agent_paths,
            migration_manager,
            control: main_control,
        },
    ));
    let surface_budget = surface_budget();
    for (display, display_channel_id) in displays {
        tasks.spawn(run_display(
            display,
            cancel_receiver.clone(),
            DisplayTaskContext {
                connection_generation,
                display_channel_id,
                frame_sender: frame_sender.clone(),
                topology_sender: topology_sender.clone(),
                surface_budget: surface_budget.clone(),
                image_decode_slots: image_decode_slots.clone(),
                glz_window: glz_window.clone(),
                progress: progress.clone(),
                #[cfg(unix)]
                gl_frame_sender: gl_frame_sender.clone(),
            },
        ));
    }
    if let Some((channel, paths, channel_id)) = inputs {
        tasks.spawn(run_inputs(
            channel,
            cancel_receiver.clone(),
            paths,
            mouse_mode_receiver,
            progress.clone(),
            channel_id,
        ));
    }
    for (channel, cursor_sender, channel_id) in cursors {
        tasks.spawn(run_cursor(
            channel,
            cancel_receiver.clone(),
            cursor_sender,
            connection_generation,
            channel_id,
            progress.clone(),
        ));
    }
    for playback in playbacks {
        tasks.spawn(run_playback(
            playback.channel,
            cancel_receiver.clone(),
            playback.state_sender,
            playback.audio_sender,
            playback.packet_sender,
            connection_generation,
            playback.channel_id,
            progress.clone(),
        ));
    }
    for record in records {
        tasks.spawn(run_record(
            record.channel,
            cancel_receiver.clone(),
            record.paths,
            record.clock,
            connection_generation,
            record.channel_id,
            progress.clone(),
        ));
    }
    for usbredir in usbredir {
        tasks.spawn(run_usbredir(
            usbredir.channel,
            cancel_receiver.clone(),
            usbredir.paths,
            connection_generation,
            usbredir.channel_id,
            progress.clone(),
        ));
    }
    for smartcard in smartcards {
        tasks.spawn(run_smartcard(
            smartcard.channel,
            cancel_receiver.clone(),
            smartcard.paths,
            connection_generation,
            smartcard.channel_id,
            progress.clone(),
        ));
    }
    for port in ports {
        tasks.spawn(run_port(
            port.channel,
            cancel_receiver.clone(),
            port.paths,
            connection_generation,
            port.channel_type,
            port.channel_id,
            progress.clone(),
        ));
    }

    let mut terminal = if *supervisor_cancel.borrow() {
        Ok(())
    } else {
        tokio::select! {
            result = tasks.join_next() => flatten_optional_join_result(result),
            changed = supervisor_cancel.changed() => {
                let _ = changed;
                Ok(())
            },
        }
    };
    state_sender.send_replace(SessionState::Closing);
    cancel_sender.send_replace(true);

    let cleanup = timeout(CHANNEL_SHUTDOWN_GRACE, async {
        let mut result = Ok(());
        while let Some(joined) = tasks.join_next().await {
            if result.is_ok() {
                result = flatten_join_result(joined);
            }
        }
        result
    })
    .await;
    match cleanup {
        Ok(cleanup_result) if terminal.is_ok() => terminal = cleanup_result,
        Ok(_) => {}
        Err(_) => {
            tasks.abort_all();
            while tasks.join_next().await.is_some() {}
        }
    }
    match &terminal {
        Ok(()) => {
            state_sender.send_replace(SessionState::Closed);
        }
        Err(error) if error.category() == ErrorCategory::Cancelled => {
            state_sender.send_replace(SessionState::Closed);
        }
        Err(error) => {
            state_sender.send_replace(SessionState::Failed(error.category()));
        }
    }
    terminal
}

/// Converts exhaustion of a non-empty task set into an internal ownership failure.
fn flatten_optional_join_result(
    result: Option<Result<Result<(), ClientError>, tokio::task::JoinError>>,
) -> Result<(), ClientError> {
    flatten_join_result(result.ok_or(ClientError::TaskTerminated)?)
}

/// Converts a task panic or cancellation only after its sibling has entered cleanup.
fn flatten_join_result(
    result: Result<Result<(), ClientError>, tokio::task::JoinError>,
) -> Result<(), ClientError> {
    result.map_err(|_| ClientError::TaskTerminated)?
}

/// Keeps Main responsive to control traffic while child channels run.
async fn run_main<S>(
    mut channel: Channel<S>,
    mut cancel: watch::Receiver<bool>,
    context: MainTaskContext,
) -> Result<(), ClientError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let MainTaskContext {
        connection_generation,
        mouse_mode_sender,
        server_identity_sender,
        progress,
        identity,
        agent_bootstrap,
        agent_paths,
        migration_manager,
        mut control,
    } = context;
    let mut message_body = Vec::new();
    let (mut agent, mut agent_commands, mut monitor_layout) = crate::agent::AgentRuntime::new(
        connection_generation,
        agent_bootstrap.connected,
        agent_bootstrap.outbound_tokens,
        agent_paths,
    );
    let credit_returns = agent.credit_returns().clone();
    let mut migration_activation_baseline = None;
    let mut observed_migration_activation = channel.migration_activation_count();
    let mut awaiting_nonseamless_main_init = false;
    agent.initialize(&mut channel).await?;
    loop {
        agent.prepare_internal(&mut monitor_layout)?;
        tokio::select! {
            changed = cancel.changed() => {
                if changed.is_err() || *cancel.borrow() {
                    // A peer that closed first has already achieved the same transport cleanup.
                    let _ = channel.shutdown().await;
                    return Ok(());
                }
            }
            incoming = channel.read_message(&mut message_body) => {
                let header = incoming?;
                let message = IncomingMessage { header, body: &message_body };
                let mut serial = channel.received_serial();
                if let Some(seamless) =
                    channel.observe_migration_activation(&mut observed_migration_activation)
                    && !seamless
                {
                    awaiting_nonseamless_main_init = true;
                    server_identity_sender.send_replace(ServerIdentity::default());
                }
                if control.handle(&mut channel, &message).await? == ControlDisposition::Consumed {
                    progress.complete(identity, serial)?;
                    continue;
                }
                if handle_channel_wait(&progress, identity, serial, &mut cancel, &message).await? {
                    progress.complete(identity, serial)?;
                    continue;
                }
                if agent
                    .handle_server_message(
                        message.header.message_type,
                        message.body,
                        &mut channel,
                    )
                    .await?
                {
                    progress.complete(identity, serial)?;
                    continue;
                }
                match message.header.message_type {
                    main_server::CHANNELS_LIST => {
                        let _ = ChannelsList::decode(message.body)?;
                    }
                    main_server::MOUSE_MODE => {
                        let state = MouseModeState::decode(message.body)?;
                        mouse_mode_sender.send_replace(state.current_mode);
                        request_client_mouse_mode(&mut channel, state).await?;
                    }
                    main_server::MULTI_MEDIA_TIME => {}
                    main_server::NAME => {
                        let name: Arc<str> = decode_main_name(message.body)?.into();
                        server_identity_sender.send_modify(|identity| identity.name = Some(name));
                    }
                    main_server::UUID => {
                        let uuid = decode_main_uuid(message.body)?;
                        server_identity_sender.send_modify(|identity| identity.uuid = Some(uuid));
                    }
                    main_server::MIGRATE_BEGIN | main_server::MIGRATE_BEGIN_SEAMLESS => {
                        let seamless_requested =
                            message.header.message_type == main_server::MIGRATE_BEGIN_SEAMLESS;
                        let migration = MigrationBegin::decode(message.body, seamless_requested)?;
                        let source_version = migration.source_version.unwrap_or(0);
                        let activation_baseline = channel.migration_activation_count();
                        match migration_manager
                            .prepare(
                                &migration.destination,
                                seamless_requested,
                                source_version,
                            )
                            .await
                        {
                            Ok((_, seamless)) => {
                                migration_activation_baseline = Some(activation_baseline);
                                channel
                                    .write_message(
                                        if seamless {
                                            main_client::MIGRATE_CONNECTED_SEAMLESS
                                        } else {
                                            main_client::MIGRATE_CONNECTED
                                        },
                                        &[],
                                    )
                                    .await?;
                            }
                            Err(_) => {
                                migration_manager.cancel();
                                migration_activation_baseline = None;
                                channel
                                    .write_message(main_client::MIGRATE_CONNECT_ERROR, &[])
                                    .await?;
                            }
                        }
                    }
                    main_server::MIGRATE_CANCEL => {
                        if !message.body.is_empty() {
                            return Err(protocol_value_error("Main migration cancel body"));
                        }
                        migration_manager.cancel();
                        migration_activation_baseline = None;
                    }
                    main_server::MIGRATE_END => {
                        if !message.body.is_empty() {
                            return Err(protocol_value_error("Main migration end body"));
                        }
                        let baseline = migration_activation_baseline
                            .take()
                            .ok_or_else(|| protocol_value_error("migration end without begin"))?;
                        if channel.migration_activation_count() == baseline {
                            channel.activate_migration_target().await?;
                        }
                        channel
                            .write_message(main_client::MIGRATE_END, &[])
                            .await?;
                    }
                    main_server::INIT => {
                        if !awaiting_nonseamless_main_init {
                            return Err(protocol_value_error("unexpected migrated Main Init"));
                        }
                        let main_init = MainInit::decode(message.body)?;
                        if main_init.session_id
                            != migration_manager.session_id.load(Ordering::Acquire)
                        {
                            return Err(protocol_value_error("migrated Main session id"));
                        }
                        let state = main_init.mouse_mode_state()?;
                        mouse_mode_sender.send_replace(state.current_mode);
                        request_client_mouse_mode(&mut channel, state).await?;
                        agent
                            .reinitialize_after_migration(
                                main_init.agent_connected,
                                u64::from(main_init.agent_tokens),
                                None,
                                &mut channel,
                            )
                            .await?;
                        awaiting_nonseamless_main_init = false;
                    }
                    main_server::MIGRATE_SWITCH_HOST => {
                        let destination =
                            oxide_spice_protocol::MigrationDestination::decode_switch_host(
                                message.body,
                            )?;
                        let bootstrap = migration_manager.switch_host(&destination).await?;
                        channel.activate_migration_target().await?;
                        observed_migration_activation = channel.migration_activation_count();
                        migration_activation_baseline = None;
                        awaiting_nonseamless_main_init = false;
                        control = bootstrap.control;
                        control.synchronize_migration_activation(&channel);
                        mouse_mode_sender.send_replace(bootstrap.mouse_mode.current_mode);
                        server_identity_sender.send_replace(bootstrap.server_identity);
                        agent
                            .reinitialize_after_migration(
                                bootstrap.agent.connected,
                                bootstrap.agent.outbound_tokens,
                                bootstrap.agent.disconnect_reason,
                                &mut channel,
                            )
                            .await?;
                        serial = channel.received_serial();
                    }
                    message_type => {
                        return Err(ClientError::UnsupportedMessage {
                            channel: "main",
                            message_type,
                        });
                    }
                }
                progress.complete(identity, serial)?;
            }
            _ = std::future::ready(()), if agent.can_send() => {
                agent.send_one(&mut channel).await?;
            }
            command = agent_commands.recv(), if agent.can_accept_command() => {
                let Some(command) = command else {
                    let _ = channel.shutdown().await;
                    return Ok(());
                };
                agent.accept_command(command)?;
            }
            changed = monitor_layout.changed() => {
                if changed.is_err() {
                    let _ = channel.shutdown().await;
                    return Ok(());
                }
                agent.note_layout_changed();
            }
            _ = credit_returns.notify.notified(), if agent.connected() => {
                agent.send_returned_credits(&mut channel).await?;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use jpeg_encoder::{ColorType, Encoder};
    use oxide_spice_protocol::{
        LINK_HEADER_SIZE, LINK_REPLY_FIXED_SIZE, LinkError, LinkHeader,
        SPICE_TICKET_PUBLIC_KEY_SIZE, SurfaceFormat, common_server,
    };
    use rsa::pkcs8::EncodePublicKey;
    use rsa::rand_core::OsRng;
    use rsa::{Oaep, RsaPrivateKey};
    use sha1::Sha1;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    const TEST_SESSION_ID: u32 = 0x0A11_CE01;
    const TEST_MESSAGE_TIMEOUT: Duration = Duration::from_secs(5);

    #[tokio::test]
    async fn session_drives_display_cursor_inputs_and_reaps_every_channel() {
        let listener = TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("bind fake SPICE server");
        let port = listener.local_addr().expect("listener address").port();
        let private_key = RsaPrivateKey::new(&mut OsRng, 1024).expect("test RSA key");
        let server = tokio::spawn(async move {
            let (mut main_stream, _) = listener.accept().await.expect("Main connection");
            accept_link(&mut main_stream, &private_key, 0, ChannelType::Main, 0).await;

            let mut main_init = Vec::with_capacity(32);
            for field in [TEST_SESSION_ID, 1, 3, 2, 1, 1, 0, 0] {
                main_init.extend_from_slice(&field.to_le_bytes());
            }
            write_mini_message(&mut main_stream, main_server::INIT, &main_init).await;
            let (message_type, body) = read_mini_message(&mut main_stream).await;
            assert_eq!(message_type, main_client::ATTACH_CHANNELS);
            assert!(body.is_empty());

            let mut padded_ping = vec![0x5A; 128];
            padded_ping[..4].copy_from_slice(&7_u32.to_le_bytes());
            padded_ping[4..12].copy_from_slice(&11_u64.to_le_bytes());
            write_mini_message(&mut main_stream, common_server::PING, &padded_ping).await;
            let (message_type, pong) = read_mini_message(&mut main_stream).await;
            assert_eq!(message_type, oxide_spice_protocol::common_client::PONG);
            assert_eq!(pong, padded_ping[..12]);

            let mut channels = 8_u32.to_le_bytes().to_vec();
            channels.extend_from_slice(&[ChannelType::Display as u8, 0]);
            channels.extend_from_slice(&[ChannelType::Display as u8, 1]);
            channels.extend_from_slice(&[ChannelType::Inputs as u8, 0]);
            channels.extend_from_slice(&[ChannelType::Cursor as u8, 0]);
            channels.extend_from_slice(&[ChannelType::Playback as u8, 0]);
            channels.extend_from_slice(&[ChannelType::Record as u8, 0]);
            channels.extend_from_slice(&[ChannelType::UsbRedirection as u8, 0]);
            channels.extend_from_slice(&[ChannelType::Port as u8, 0]);
            write_mini_message(&mut main_stream, main_server::CHANNELS_LIST, &channels).await;

            let (mut display_stream, _) = listener.accept().await.expect("Display connection");
            accept_link(
                &mut display_stream,
                &private_key,
                TEST_SESSION_ID,
                ChannelType::Display,
                0,
            )
            .await;
            let (mut second_display_stream, _) =
                listener.accept().await.expect("second Display connection");
            accept_link(
                &mut second_display_stream,
                &private_key,
                TEST_SESSION_ID,
                ChannelType::Display,
                1,
            )
            .await;
            let (mut inputs_stream, _) = listener.accept().await.expect("Inputs connection");
            accept_link(
                &mut inputs_stream,
                &private_key,
                TEST_SESSION_ID,
                ChannelType::Inputs,
                0,
            )
            .await;
            let (mut cursor_stream, _) = listener.accept().await.expect("Cursor connection");
            accept_link(
                &mut cursor_stream,
                &private_key,
                TEST_SESSION_ID,
                ChannelType::Cursor,
                0,
            )
            .await;
            let (mut playback_stream, _) = listener.accept().await.expect("Playback connection");
            accept_link(
                &mut playback_stream,
                &private_key,
                TEST_SESSION_ID,
                ChannelType::Playback,
                0,
            )
            .await;
            let (mut record_stream, _) = listener.accept().await.expect("Record connection");
            accept_link(
                &mut record_stream,
                &private_key,
                TEST_SESSION_ID,
                ChannelType::Record,
                0,
            )
            .await;
            let (mut usbredir_stream, _) = listener.accept().await.expect("usbredir connection");
            accept_link(
                &mut usbredir_stream,
                &private_key,
                TEST_SESSION_ID,
                ChannelType::UsbRedirection,
                0,
            )
            .await;
            let (mut port_stream, _) = listener.accept().await.expect("Port connection");
            accept_link(
                &mut port_stream,
                &private_key,
                TEST_SESSION_ID,
                ChannelType::Port,
                0,
            )
            .await;

            let (message_type, record_mode) = read_mini_message(&mut record_stream).await;
            assert_eq!(message_type, oxide_spice_protocol::record_client::MODE);
            assert_eq!(record_mode.len(), 8);
            assert_eq!(
                &record_mode[4..],
                &(oxide_spice_protocol::AudioDataMode::Raw as u32).to_le_bytes()
            );
            let (message_type, display_init) = read_mini_message(&mut display_stream).await;
            assert_eq!(message_type, oxide_spice_protocol::display_client::INIT);
            assert_eq!(display_init.len(), 14);
            let (message_type, preferred_compression) =
                read_mini_message(&mut display_stream).await;
            assert_eq!(
                message_type,
                oxide_spice_protocol::display_client::PREFERRED_COMPRESSION
            );
            assert_eq!(
                preferred_compression,
                oxide_spice_protocol::ImageCompression::Lz.encode()
            );
            let (message_type, display_init) = read_mini_message(&mut second_display_stream).await;
            assert_eq!(message_type, oxide_spice_protocol::display_client::INIT);
            assert_eq!(display_init.len(), 14);
            let (message_type, preferred_compression) =
                read_mini_message(&mut second_display_stream).await;
            assert_eq!(
                message_type,
                oxide_spice_protocol::display_client::PREFERRED_COMPRESSION
            );
            assert_eq!(
                preferred_compression,
                oxide_spice_protocol::ImageCompression::Lz.encode()
            );

            let (message_type, receive_tokens) =
                read_client_main_control(&mut main_stream, main_client::AGENT_START).await;
            assert_eq!(message_type, main_client::AGENT_START);
            assert_eq!(
                u32::from_le_bytes(receive_tokens.try_into().expect("Agent receive tokens")),
                crate::agent::AGENT_RECEIVE_WINDOW
            );
            let announcement = read_client_agent_message(&mut main_stream).await;
            assert_eq!(
                announcement.0,
                oxide_spice_protocol::agent_message::ANNOUNCE_CAPABILITIES
            );
            assert_eq!(
                u32::from_le_bytes(announcement.1[..4].try_into().expect("capability request")),
                1
            );
            let peer_capabilities = CapabilitySet::from_bits([
                oxide_spice_protocol::agent_capability::MONITORS_CONFIG,
                oxide_spice_protocol::agent_capability::CLIPBOARD_BY_DEMAND,
                oxide_spice_protocol::agent_capability::CLIPBOARD_SELECTION,
                oxide_spice_protocol::agent_capability::MAX_CLIPBOARD,
                oxide_spice_protocol::agent_capability::MONITORS_CONFIG_POSITION,
                oxide_spice_protocol::agent_capability::CLIPBOARD_GRAB_SERIAL,
                oxide_spice_protocol::agent_capability::MONITORS_PHYSICAL_SIZE,
            ])
            .expect("Agent peer capabilities");
            let capabilities = oxide_spice_protocol::AgentCapabilities {
                request_reply: false,
                capabilities: peer_capabilities,
            }
            .encode();
            write_mini_message(
                &mut main_stream,
                main_server::AGENT_DATA,
                &agent_wire_message(
                    oxide_spice_protocol::agent_message::ANNOUNCE_CAPABILITIES,
                    &capabilities,
                ),
            )
            .await;
            write_mini_message(
                &mut main_stream,
                main_server::AGENT_TOKEN,
                &10_u32.to_le_bytes(),
            )
            .await;

            let mut saw_monitor_layout = false;
            let mut saw_clipboard_grab = false;
            while !saw_monitor_layout || !saw_clipboard_grab {
                let (message_type, payload) = read_client_agent_message(&mut main_stream).await;
                match message_type {
                    oxide_spice_protocol::agent_message::MAX_CLIPBOARD => {
                        assert_eq!(payload.len(), 4);
                    }
                    oxide_spice_protocol::agent_message::MONITORS_CONFIG => {
                        assert_eq!(&payload[..4], &1_u32.to_le_bytes());
                        assert_eq!(&payload[8..12], &600_u32.to_le_bytes());
                        assert_eq!(&payload[12..16], &800_u32.to_le_bytes());
                        saw_monitor_layout = true;
                    }
                    oxide_spice_protocol::agent_message::CLIPBOARD_GRAB => {
                        let grab =
                            oxide_spice_protocol::decode_clipboard_grab(&payload, true, true)
                                .expect("client clipboard grab");
                        assert_eq!(grab.serial, Some(0));
                        assert_eq!(
                            grab.types,
                            [oxide_spice_protocol::AgentClipboardType::Utf8Text as u32]
                        );
                        saw_clipboard_grab = true;
                    }
                    other => panic!("unexpected initial Agent message {other}"),
                }
            }

            let mut remote_grab = vec![
                oxide_spice_protocol::AgentClipboardSelection::Clipboard as u8,
                0,
                0,
                0,
            ];
            remote_grab.extend_from_slice(&0_u32.to_le_bytes());
            remote_grab.extend_from_slice(
                &(oxide_spice_protocol::AgentClipboardType::Utf8Text as u32).to_le_bytes(),
            );
            write_mini_message(
                &mut main_stream,
                main_server::AGENT_DATA,
                &agent_wire_message(
                    oxide_spice_protocol::agent_message::CLIPBOARD_GRAB,
                    &remote_grab,
                ),
            )
            .await;

            let request = read_client_agent_message(&mut main_stream).await;
            assert_eq!(
                request.0,
                oxide_spice_protocol::agent_message::CLIPBOARD_REQUEST
            );
            let request = oxide_spice_protocol::decode_clipboard_request(&request.1, true)
                .expect("client clipboard request");
            assert_eq!(
                request.clipboard_type,
                oxide_spice_protocol::AgentClipboardType::Utf8Text as u32
            );
            let mut remote_text = vec![
                oxide_spice_protocol::AgentClipboardSelection::Clipboard as u8,
                0,
                0,
                0,
            ];
            remote_text.extend_from_slice(
                &(oxide_spice_protocol::AgentClipboardType::Utf8Text as u32).to_le_bytes(),
            );
            remote_text.extend_from_slice(b"guest text");
            write_mini_message(
                &mut main_stream,
                main_server::AGENT_DATA,
                &agent_wire_message(oxide_spice_protocol::agent_message::CLIPBOARD, &remote_text),
            )
            .await;

            let mut host_text_request = vec![
                oxide_spice_protocol::AgentClipboardSelection::Clipboard as u8,
                0,
                0,
                0,
            ];
            host_text_request.extend_from_slice(
                &(oxide_spice_protocol::AgentClipboardType::Utf8Text as u32).to_le_bytes(),
            );
            write_mini_message(
                &mut main_stream,
                main_server::AGENT_DATA,
                &agent_wire_message(
                    oxide_spice_protocol::agent_message::CLIPBOARD_REQUEST,
                    &host_text_request,
                ),
            )
            .await;
            let response = read_client_agent_message(&mut main_stream).await;
            assert_eq!(response.0, oxide_spice_protocol::agent_message::CLIPBOARD);
            let response = oxide_spice_protocol::decode_clipboard_data(&response.1, true)
                .expect("client clipboard response");
            assert_eq!(response.data, b"host text");

            let file_start = read_client_agent_message(&mut main_stream).await;
            assert_eq!(
                file_start.0,
                oxide_spice_protocol::agent_message::FILE_TRANSFER_START
            );
            let transfer_id = u32::from_le_bytes(
                file_start.1[..4]
                    .try_into()
                    .expect("file transfer identity"),
            );
            let metadata = std::str::from_utf8(&file_start.1[4..file_start.1.len() - 1])
                .expect("file transfer metadata UTF-8");
            assert!(metadata.contains("name=note.txt\n"));
            assert!(metadata.contains("size=5\n"));
            write_mini_message(
                &mut main_stream,
                main_server::AGENT_DATA,
                &agent_wire_message(
                    oxide_spice_protocol::agent_message::FILE_TRANSFER_STATUS,
                    &oxide_spice_protocol::encode_file_transfer_status(
                        transfer_id,
                        oxide_spice_protocol::AgentFileTransferStatus::CanSendData,
                    )
                    .expect("file transfer acceptance"),
                ),
            )
            .await;
            let file_data = read_client_agent_message(&mut main_stream).await;
            assert_eq!(
                file_data.0,
                oxide_spice_protocol::agent_message::FILE_TRANSFER_DATA
            );
            assert_eq!(&file_data.1[..4], &transfer_id.to_le_bytes());
            assert_eq!(&file_data.1[4..12], &5_u64.to_le_bytes());
            assert_eq!(&file_data.1[12..], b"hello");
            write_mini_message(
                &mut main_stream,
                main_server::AGENT_DATA,
                &agent_wire_message(
                    oxide_spice_protocol::agent_message::FILE_TRANSFER_STATUS,
                    &oxide_spice_protocol::encode_file_transfer_status(
                        transfer_id,
                        oxide_spice_protocol::AgentFileTransferStatus::Success,
                    )
                    .expect("file transfer success"),
                ),
            )
            .await;

            let cancelled_start = read_client_agent_message(&mut main_stream).await;
            assert_eq!(
                cancelled_start.0,
                oxide_spice_protocol::agent_message::FILE_TRANSFER_START
            );
            let cancelled_transfer_id = u32::from_le_bytes(
                cancelled_start.1[..4]
                    .try_into()
                    .expect("cancelled file transfer identity"),
            );
            let cancelled_metadata =
                std::str::from_utf8(&cancelled_start.1[4..cancelled_start.1.len() - 1])
                    .expect("cancelled file transfer metadata UTF-8");
            assert!(cancelled_metadata.contains("name=cancelled.bin\n"));
            assert!(cancelled_metadata.contains("size=10\n"));
            write_mini_message(
                &mut main_stream,
                main_server::AGENT_DATA,
                &agent_wire_message(
                    oxide_spice_protocol::agent_message::FILE_TRANSFER_STATUS,
                    &oxide_spice_protocol::encode_file_transfer_status(
                        cancelled_transfer_id,
                        oxide_spice_protocol::AgentFileTransferStatus::CanSendData,
                    )
                    .expect("cancelled file transfer acceptance"),
                ),
            )
            .await;
            let cancelled_status = read_client_agent_message(&mut main_stream).await;
            assert_eq!(
                cancelled_status.0,
                oxide_spice_protocol::agent_message::FILE_TRANSFER_STATUS
            );
            let cancelled_status =
                oxide_spice_protocol::decode_file_transfer_status(&cancelled_status.1)
                    .expect("cancelled file transfer status");
            assert_eq!(cancelled_status.transfer_id, cancelled_transfer_id);
            assert_eq!(
                cancelled_status.status,
                oxide_spice_protocol::AgentFileTransferStatus::Cancelled
            );

            write_mini_message(
                &mut main_stream,
                main_server::AGENT_DISCONNECTED,
                &0_u32.to_le_bytes(),
            )
            .await;
            write_mini_message(
                &mut main_stream,
                main_server::AGENT_CONNECTED_TOKENS,
                &1_u32.to_le_bytes(),
            )
            .await;
            let (message_type, receive_tokens) =
                read_client_main_control(&mut main_stream, main_client::AGENT_START).await;
            assert_eq!(message_type, main_client::AGENT_START);
            assert_eq!(
                receive_tokens,
                crate::agent::AGENT_RECEIVE_WINDOW.to_le_bytes()
            );
            let announcement = read_client_agent_message(&mut main_stream).await;
            assert_eq!(
                announcement.0,
                oxide_spice_protocol::agent_message::ANNOUNCE_CAPABILITIES
            );
            write_mini_message(
                &mut main_stream,
                main_server::AGENT_DATA,
                &agent_wire_message(
                    oxide_spice_protocol::agent_message::ANNOUNCE_CAPABILITIES,
                    &capabilities,
                ),
            )
            .await;
            write_mini_message(
                &mut main_stream,
                main_server::AGENT_TOKEN,
                &3_u32.to_le_bytes(),
            )
            .await;
            let mut replayed_monitor = false;
            let mut replayed_offer = false;
            while !replayed_monitor || !replayed_offer {
                let (message_type, payload) = read_client_agent_message(&mut main_stream).await;
                match message_type {
                    oxide_spice_protocol::agent_message::MAX_CLIPBOARD => {}
                    oxide_spice_protocol::agent_message::MONITORS_CONFIG => {
                        replayed_monitor = true;
                    }
                    oxide_spice_protocol::agent_message::CLIPBOARD_GRAB => {
                        let grab =
                            oxide_spice_protocol::decode_clipboard_grab(&payload, true, true)
                                .expect("replayed clipboard grab");
                        assert_eq!(grab.serial, Some(0));
                        replayed_offer = true;
                    }
                    other => panic!("unexpected replayed Agent message {other}"),
                }
            }

            let mut playback_mode = 100_u32.to_le_bytes().to_vec();
            playback_mode.extend_from_slice(
                &(oxide_spice_protocol::AudioDataMode::Raw as u32).to_le_bytes(),
            );
            write_mini_message(
                &mut playback_stream,
                oxide_spice_protocol::playback_server::MODE,
                &playback_mode,
            )
            .await;
            let mut playback_start = 2_u32.to_le_bytes().to_vec();
            playback_start.extend_from_slice(
                &(oxide_spice_protocol::AudioSampleFormat::Signed16LittleEndian as u32)
                    .to_le_bytes(),
            );
            playback_start.extend_from_slice(&48_000_u32.to_le_bytes());
            playback_start.extend_from_slice(&100_u32.to_le_bytes());
            write_mini_message(
                &mut playback_stream,
                oxide_spice_protocol::playback_server::START,
                &playback_start,
            )
            .await;
            let mut playback_data = 104_u32.to_le_bytes().to_vec();
            playback_data.extend_from_slice(&[1, 0, 0xff, 0xff]);
            write_mini_message(
                &mut playback_stream,
                oxide_spice_protocol::playback_server::DATA,
                &playback_data,
            )
            .await;
            write_mini_message(
                &mut playback_stream,
                oxide_spice_protocol::playback_server::STOP,
                &[],
            )
            .await;

            let mut record_start = 1_u32.to_le_bytes().to_vec();
            record_start.extend_from_slice(
                &(oxide_spice_protocol::AudioSampleFormat::Signed16LittleEndian as u32)
                    .to_le_bytes(),
            );
            record_start.extend_from_slice(&48_000_u32.to_le_bytes());
            write_mini_message(
                &mut record_stream,
                oxide_spice_protocol::record_server::START,
                &record_start,
            )
            .await;
            let (message_type, start_mark) = read_mini_message(&mut record_stream).await;
            assert_eq!(
                message_type,
                oxide_spice_protocol::record_client::START_MARK
            );
            assert_eq!(start_mark.len(), 4);
            let (message_type, record_data) = read_mini_message(&mut record_stream).await;
            assert_eq!(message_type, oxide_spice_protocol::record_client::DATA);
            assert_eq!(record_data.len(), 8);
            assert_eq!(&record_data[4..], &[1, 0, 2, 0]);
            write_mini_message(
                &mut record_stream,
                oxide_spice_protocol::record_server::STOP,
                &[],
            )
            .await;

            write_mini_message(
                &mut usbredir_stream,
                101,
                &oxide_spice_protocol::encode_usbredir_hello(
                    "fake-usb",
                    oxide_spice_protocol::UsbRedirCapabilities::default(),
                )
                .expect("peer usbredir Hello"),
            )
            .await;
            write_mini_message(
                &mut usbredir_stream,
                101,
                &oxide_spice_protocol::encode_usbredir_packet(2, 0, &[], false)
                    .expect("peer usbredir packet"),
            )
            .await;
            let (message_type, usbredir_packet) = read_mini_message(&mut usbredir_stream).await;
            assert_eq!(message_type, 101);
            assert_eq!(&usbredir_packet[..4], &3_u32.to_le_bytes());
            assert_eq!(&usbredir_packet[8..12], &7_u32.to_le_bytes());

            let port_name = b"org.oxide.test\0";
            let mut port_init = u32::try_from(port_name.len())
                .expect("Port name size")
                .to_le_bytes()
                .to_vec();
            port_init.extend_from_slice(&9_u32.to_le_bytes());
            port_init.push(1);
            port_init.extend_from_slice(port_name);
            write_mini_message(
                &mut port_stream,
                oxide_spice_protocol::port_server::INIT,
                &port_init,
            )
            .await;
            write_mini_message(
                &mut port_stream,
                oxide_spice_protocol::port_server::DATA,
                b"guest-port",
            )
            .await;
            let (message_type, port_data) = read_mini_message(&mut port_stream).await;
            assert_eq!(message_type, oxide_spice_protocol::port_client::DATA);
            assert_eq!(port_data, b"host-port");
            let (message_type, port_event) = read_mini_message(&mut port_stream).await;
            assert_eq!(message_type, oxide_spice_protocol::port_client::EVENT);
            assert_eq!(port_event, [oxide_spice_protocol::PortEvent::Break as u8]);
            write_mini_message(
                &mut port_stream,
                oxide_spice_protocol::port_server::EVENT,
                &[oxide_spice_protocol::PortEvent::Break as u8],
            )
            .await;
            write_mini_message(
                &mut port_stream,
                oxide_spice_protocol::port_server::EVENT,
                &[oxide_spice_protocol::PortEvent::Closed as u8],
            )
            .await;

            let mut ack_window = 1_u32.to_le_bytes().to_vec();
            ack_window.extend_from_slice(&100_u32.to_le_bytes());
            write_mini_message(&mut display_stream, common_server::SET_ACK, &ack_window).await;
            let (message_type, ack_generation) = read_mini_message(&mut display_stream).await;
            assert_eq!(message_type, oxide_spice_protocol::common_client::ACK_SYNC);
            assert_eq!(ack_generation, 1_u32.to_le_bytes());

            let mut wait_for_second_display = vec![1, ChannelType::Display as u8, 1];
            wait_for_second_display.extend_from_slice(&1_u64.to_le_bytes());
            write_mini_message(
                &mut display_stream,
                common_server::WAIT_FOR_CHANNELS,
                &wait_for_second_display,
            )
            .await;

            let mut second_display_ack_window = 4_u32.to_le_bytes().to_vec();
            second_display_ack_window.extend_from_slice(&100_u32.to_le_bytes());
            write_mini_message(
                &mut second_display_stream,
                common_server::SET_ACK,
                &second_display_ack_window,
            )
            .await;
            let (message_type, ack_generation) =
                read_mini_message(&mut second_display_stream).await;
            assert_eq!(message_type, oxide_spice_protocol::common_client::ACK_SYNC);
            assert_eq!(ack_generation, 4_u32.to_le_bytes());

            let mut inputs_ack_window = 2_u32.to_le_bytes().to_vec();
            inputs_ack_window.extend_from_slice(&100_u32.to_le_bytes());
            write_mini_message(
                &mut inputs_stream,
                common_server::SET_ACK,
                &inputs_ack_window,
            )
            .await;
            let (message_type, ack_generation) = read_mini_message(&mut inputs_stream).await;
            assert_eq!(message_type, oxide_spice_protocol::common_client::ACK_SYNC);
            assert_eq!(ack_generation, 2_u32.to_le_bytes());
            write_mini_message(
                &mut inputs_stream,
                oxide_spice_protocol::inputs_server::INIT,
                &oxide_spice_protocol::KeyboardModifiers::NUM_LOCK
                    .bits()
                    .to_le_bytes(),
            )
            .await;

            let mut cursor_ack_window = 3_u32.to_le_bytes().to_vec();
            cursor_ack_window.extend_from_slice(&100_u32.to_le_bytes());
            write_mini_message(
                &mut cursor_stream,
                common_server::SET_ACK,
                &cursor_ack_window,
            )
            .await;
            let (message_type, ack_generation) = read_mini_message(&mut cursor_stream).await;
            assert_eq!(message_type, oxide_spice_protocol::common_client::ACK_SYNC);
            assert_eq!(ack_generation, 3_u32.to_le_bytes());
            let mut cursor_init = Vec::new();
            cursor_init.extend_from_slice(&10_i16.to_le_bytes());
            cursor_init.extend_from_slice(&20_i16.to_le_bytes());
            cursor_init.extend_from_slice(&0_u16.to_le_bytes());
            cursor_init.extend_from_slice(&0_u16.to_le_bytes());
            cursor_init.push(1);
            cursor_init.extend_from_slice(&(1_u16 << 1).to_le_bytes());
            cursor_init.extend_from_slice(&42_u64.to_le_bytes());
            cursor_init.push(oxide_spice_protocol::CursorType::Alpha as u8);
            for field in [1_u16, 1, 0, 0] {
                cursor_init.extend_from_slice(&field.to_le_bytes());
            }
            cursor_init.extend_from_slice(&[0x10, 0x20, 0x30, 0x40]);
            cursor_init.extend_from_slice(&[0xA5; 16]);
            write_mini_message(
                &mut cursor_stream,
                oxide_spice_protocol::cursor_server::INIT,
                &cursor_init,
            )
            .await;

            write_mini_message(
                &mut display_stream,
                oxide_spice_protocol::display_server::INVALIDATE_ALL_PIXMAPS,
                &[0],
            )
            .await;
            write_mini_message(
                &mut display_stream,
                oxide_spice_protocol::display_server::INVALIDATE_ALL_PALETTES,
                &[],
            )
            .await;

            let mut surface_create = Vec::with_capacity(20);
            for field in [0, 1, 1, SurfaceFormat::Xrgb32 as u32, 1] {
                surface_create.extend_from_slice(&field.to_le_bytes());
            }
            write_mini_message(
                &mut display_stream,
                oxide_spice_protocol::display_server::SURFACE_CREATE,
                &surface_create,
            )
            .await;
            let mut monitors = 1_u16.to_le_bytes().to_vec();
            monitors.extend_from_slice(&4_u16.to_le_bytes());
            for field in [0_u32, 0, 1, 1, 0, 0, 0] {
                monitors.extend_from_slice(&field.to_le_bytes());
            }
            write_mini_message(
                &mut display_stream,
                oxide_spice_protocol::display_server::MONITORS_CONFIG,
                &monitors,
            )
            .await;
            let draw_copy = one_pixel_draw_copy([0x10, 0x20, 0x30, 0]);
            write_mini_message(
                &mut display_stream,
                oxide_spice_protocol::display_server::DRAW_COPY,
                &draw_copy,
            )
            .await;

            let (message_type, pointer) = read_mini_message(&mut inputs_stream).await;
            assert_eq!(
                message_type,
                oxide_spice_protocol::inputs_client::MOUSE_POSITION
            );
            assert_eq!(&pointer[..4], &40_u32.to_le_bytes());
            assert_eq!(&pointer[4..8], &50_u32.to_le_bytes());
            let (message_type, button) = read_mini_message(&mut inputs_stream).await;
            assert_eq!(
                message_type,
                oxide_spice_protocol::inputs_client::MOUSE_PRESS
            );
            assert_eq!(
                button,
                &[oxide_spice_protocol::MouseButton::Left as u8, 1, 0]
            );
            let (message_type, key) = read_mini_message(&mut inputs_stream).await;
            assert_eq!(message_type, oxide_spice_protocol::inputs_client::KEY_DOWN);
            assert_eq!(key, 0x1E_u32.to_le_bytes());

            write_mini_message(
                &mut display_stream,
                oxide_spice_protocol::display_server::RESET,
                &[],
            )
            .await;
            write_mini_message(
                &mut display_stream,
                oxide_spice_protocol::display_server::SURFACE_CREATE,
                &surface_create,
            )
            .await;
            write_mini_message(
                &mut display_stream,
                oxide_spice_protocol::display_server::MONITORS_CONFIG,
                &monitors,
            )
            .await;
            let draw_copy = one_pixel_glz_reference_draw_copy(1, 1);
            write_mini_message(
                &mut display_stream,
                oxide_spice_protocol::display_server::DRAW_COPY,
                &draw_copy,
            )
            .await;
            let mut offscreen_surface = Vec::with_capacity(20);
            for field in [9, 1, 1, SurfaceFormat::Xrgb32 as u32, 0] {
                offscreen_surface.extend_from_slice(&field.to_le_bytes());
            }
            write_mini_message(
                &mut second_display_stream,
                oxide_spice_protocol::display_server::SURFACE_CREATE,
                &offscreen_surface,
            )
            .await;
            let mut lz_offscreen = one_pixel_lz_rgb_draw_copy([0x10, 0x20, 0x30]);
            lz_offscreen[..4].copy_from_slice(&9_u32.to_le_bytes());
            write_mini_message(
                &mut second_display_stream,
                oxide_spice_protocol::display_server::DRAW_COPY,
                &lz_offscreen,
            )
            .await;
            let jpeg_offscreen = one_pixel_jpeg_draw_copy(9, [0x60, 0x40, 0x20]);
            write_mini_message(
                &mut second_display_stream,
                oxide_spice_protocol::display_server::DRAW_COPY,
                &jpeg_offscreen,
            )
            .await;
            let jpeg_alpha_offscreen =
                one_pixel_jpeg_alpha_draw_copy(9, [0x20, 0x40, 0x60], 0x80, true);
            write_mini_message(
                &mut second_display_stream,
                oxide_spice_protocol::display_server::DRAW_COPY,
                &jpeg_alpha_offscreen,
            )
            .await;
            let quic_offscreen = one_pixel_quic_draw_copy(9);
            write_mini_message(
                &mut second_display_stream,
                oxide_spice_protocol::display_server::DRAW_COPY,
                &quic_offscreen,
            )
            .await;
            let glz_base = one_pixel_zlib_glz_literal_draw_copy(9, 0, [0x40, 0x50, 0x60]);
            write_mini_message(
                &mut second_display_stream,
                oxide_spice_protocol::display_server::DRAW_COPY,
                &glz_base,
            )
            .await;
            let (message_type, key) = read_mini_message(&mut inputs_stream).await;
            assert_eq!(message_type, oxide_spice_protocol::inputs_client::KEY_UP);
            assert_eq!(key, 0x1E_u32.to_le_bytes());

            write_mini_message(
                &mut second_display_stream,
                oxide_spice_protocol::display_server::INVALIDATE_ALL_PALETTES,
                &[],
            )
            .await;
            write_mini_message(
                &mut second_display_stream,
                oxide_spice_protocol::display_server::SURFACE_CREATE,
                &surface_create,
            )
            .await;
            write_mini_message(
                &mut second_display_stream,
                oxide_spice_protocol::display_server::MONITORS_CONFIG,
                &monitors,
            )
            .await;
            let draw_copy = one_pixel_lz_palette_draw_copy(99, [0x70, 0x80, 0x90, 0]);
            write_mini_message(
                &mut second_display_stream,
                oxide_spice_protocol::display_server::DRAW_COPY,
                &draw_copy,
            )
            .await;
            write_mini_message(
                &mut second_display_stream,
                oxide_spice_protocol::display_server::INVALIDATE_PALETTE,
                &99_u64.to_le_bytes(),
            )
            .await;

            let mut display_eof = [0; 1];
            let mut second_display_eof = [0; 1];
            let mut inputs_eof = [0; 1];
            let mut cursor_eof = [0; 1];
            let mut playback_eof = [0; 1];
            let mut record_eof = [0; 1];
            let mut usbredir_eof = [0; 1];
            let mut port_eof = [0; 1];
            let (
                main_read,
                display_read,
                second_display_read,
                inputs_read,
                cursor_read,
                playback_read,
                record_read,
                usbredir_read,
                port_read,
            ) = tokio::join!(
                read_main_shutdown(&mut main_stream),
                display_stream.read(&mut display_eof),
                second_display_stream.read(&mut second_display_eof),
                inputs_stream.read(&mut inputs_eof),
                cursor_stream.read(&mut cursor_eof),
                playback_stream.read(&mut playback_eof),
                record_stream.read(&mut record_eof),
                usbredir_stream.read(&mut usbredir_eof),
                port_stream.read(&mut port_eof),
            );
            main_read.expect("Main shutdown");
            assert_eq!(display_read.expect("Display shutdown"), 0);
            assert_eq!(second_display_read.expect("second Display shutdown"), 0);
            assert_eq!(inputs_read.expect("Inputs shutdown"), 0);
            assert_eq!(cursor_read.expect("Cursor shutdown"), 0);
            assert_eq!(playback_read.expect("Playback shutdown"), 0);
            assert_eq!(record_read.expect("Record shutdown"), 0);
            assert_eq!(usbredir_read.expect("usbredir shutdown"), 0);
            assert_eq!(port_read.expect("Port shutdown"), 0);
        });

        let mut session = Session::connect(ConnectOptions::new(
            "127.0.0.1",
            port,
            TicketSecret::new("test-ticket"),
        ))
        .await
        .expect("session connects");
        assert_eq!(session.session_id(), TEST_SESSION_ID);
        assert_eq!(session.state(), SessionState::Running);
        assert_eq!(session.mouse_mode(), MouseMode::Client);
        let mut playback_packets = session
            .take_playback_packets()
            .expect("Playback packet stream");
        let mut playback_channel = session
            .playback_channels()
            .first()
            .cloned()
            .expect("Playback channel state");
        let mut record_channels = session.take_record_channels();
        let mut record_channel = record_channels.pop().expect("Record channel");
        assert!(record_channels.is_empty());
        let mut usbredir_channels = session.take_usbredir_channels();
        let mut usbredir_channel = usbredir_channels.pop().expect("usbredir channel");
        assert!(usbredir_channels.is_empty());
        let mut port_channels = session.take_port_channels();
        let mut port_channel = port_channels.pop().expect("Port channel");
        assert!(port_channels.is_empty());

        let agent = session.agent();
        let mut agent_events = session.take_agent_events().expect("Agent event stream");
        let ready = agent.wait_ready().await.expect("Agent capabilities");
        assert!(matches!(ready, crate::AgentState::Ready { .. }));
        let first_agent_generation = ready.agent_generation();
        agent
            .set_monitor_layout(crate::GuestMonitorLayout {
                monitors: vec![crate::GuestMonitor {
                    width: 800,
                    height: 600,
                    depth: 32,
                    x: 0,
                    y: 0,
                    width_mm: Some(300),
                    height_mm: Some(220),
                }]
                .into(),
            })
            .expect("desired guest monitor layout");
        agent
            .offer_clipboard_text(oxide_spice_protocol::AgentClipboardSelection::Clipboard)
            .await
            .expect("offer host clipboard text");
        let offer = agent
            .wait_clipboard_offer(oxide_spice_protocol::AgentClipboardSelection::Clipboard)
            .await
            .expect("remote clipboard offer");
        assert!(offer.supports(oxide_spice_protocol::AgentClipboardType::Utf8Text));
        let remote_text = agent
            .request_clipboard_text(oxide_spice_protocol::AgentClipboardSelection::Clipboard)
            .await
            .expect("request remote clipboard text");
        assert_eq!(&*remote_text, "guest text");
        let crate::AgentEvent::ClipboardRequested(request) =
            agent_events.next().await.expect("host clipboard request");
        let request_id = request.request_id;
        agent
            .provide_clipboard_text(request_id, "host text")
            .await
            .expect("provide host clipboard text");
        let mut transfer = agent
            .start_file_transfer(crate::FileTransferMetadata {
                file_name: "note.txt".to_owned(),
                size: 5,
            })
            .await
            .expect("start file transfer");
        transfer
            .wait_until_sending()
            .await
            .expect("guest accepted file transfer");
        transfer
            .send_chunk(b"hello")
            .await
            .expect("send bounded file chunk");
        timeout(TEST_MESSAGE_TIMEOUT, transfer.finish())
            .await
            .expect("file completion timeout")
            .expect("finish file transfer");
        let mut cancelled_transfer = timeout(
            TEST_MESSAGE_TIMEOUT,
            agent.start_file_transfer(crate::FileTransferMetadata {
                file_name: "cancelled.bin".to_owned(),
                size: 10,
            }),
        )
        .await
        .expect("cancelled file start timeout")
        .expect("start cancelled file transfer");
        timeout(
            TEST_MESSAGE_TIMEOUT,
            cancelled_transfer.wait_until_sending(),
        )
        .await
        .expect("cancelled file acceptance timeout")
        .expect("guest accepted cancelled file transfer");
        timeout(TEST_MESSAGE_TIMEOUT, cancelled_transfer.cancel())
            .await
            .expect("file cancellation timeout")
            .expect("cancel file transfer");
        let reconnected = timeout(
            TEST_MESSAGE_TIMEOUT,
            agent.wait_ready_after(first_agent_generation),
        )
        .await
        .expect("Agent reconnect timeout")
        .expect("Agent reconnect capabilities");
        assert!(reconnected.agent_generation() > first_agent_generation);
        drop(request);

        let playback_packet = timeout(TEST_MESSAGE_TIMEOUT, playback_packets.next())
            .await
            .expect("Playback packet timeout")
            .expect("raw Playback packet");
        assert_eq!(playback_packet.channel_id, 0);
        assert_eq!(playback_packet.stream_generation, 1);
        assert_eq!(playback_packet.sequence, 0);
        assert_eq!(playback_packet.timestamp_ms, 104);
        assert_eq!(playback_packet.format.channels, 2);
        assert_eq!(playback_packet.format.sample_rate_hz, 48_000);
        assert_eq!(&*playback_packet.interleaved_s16le, &[1, 0, 0xff, 0xff]);
        assert!(!playback_packet.discontinuity);
        while !matches!(
            playback_channel.state(),
            crate::PlaybackState::Stopped { .. }
        ) {
            timeout(TEST_MESSAGE_TIMEOUT, playback_channel.changed())
                .await
                .expect("Playback Stop timeout")
                .expect("Playback state update");
        }

        while !matches!(
            record_channel.state(),
            crate::RecordState::StartRequested { .. }
        ) {
            timeout(TEST_MESSAGE_TIMEOUT, record_channel.changed())
                .await
                .expect("Record Start timeout")
                .expect("Record state update");
        }
        let start_timestamp = record_channel.begin().await.expect("Record Start Mark");
        record_channel
            .send_pcm_at(start_timestamp.wrapping_add(1), &[1, 0, 2, 0])
            .await
            .expect("raw Record packet");
        while !matches!(record_channel.state(), crate::RecordState::Stopped { .. }) {
            timeout(TEST_MESSAGE_TIMEOUT, record_channel.changed())
                .await
                .expect("Record Stop timeout")
                .expect("Record state update");
        }

        let usbredir_hello = timeout(TEST_MESSAGE_TIMEOUT, usbredir_channel.next())
            .await
            .expect("usbredir Hello timeout")
            .expect("usbredir Hello");
        assert_eq!(usbredir_hello.transport_generation, 0);
        assert_eq!(&usbredir_hello.bytes[..4], &0_u32.to_le_bytes());
        let usbredir_packet = timeout(TEST_MESSAGE_TIMEOUT, usbredir_channel.next())
            .await
            .expect("usbredir packet timeout")
            .expect("usbredir packet");
        assert_eq!(usbredir_packet.transport_generation, 0);
        assert_eq!(&usbredir_packet.bytes[..4], &2_u32.to_le_bytes());
        assert_eq!(&usbredir_packet.bytes[8..12], &0_u32.to_le_bytes());
        usbredir_channel
            .write(
                &oxide_spice_protocol::encode_usbredir_packet(3, 7, &[], false)
                    .expect("usbredir reset packet"),
            )
            .await
            .expect("usbredir reset packet");

        while !matches!(port_channel.state(), crate::PortState::Ready { .. }) {
            timeout(TEST_MESSAGE_TIMEOUT, port_channel.changed())
                .await
                .expect("Port Init timeout")
                .expect("Port state update");
        }
        let crate::PortState::Ready { name, opened, .. } = port_channel.state() else {
            unreachable!("Port reached Ready state");
        };
        assert_eq!(&*name, "org.oxide.test");
        assert!(opened);
        let crate::PortInbound::Data {
            bytes,
            discontinuity,
        } = timeout(TEST_MESSAGE_TIMEOUT, port_channel.next())
            .await
            .expect("Port Data timeout")
            .expect("Port Data")
        else {
            panic!("expected Port Data");
        };
        assert_eq!(&*bytes, b"guest-port");
        assert!(!discontinuity);
        port_channel.write(b"host-port").await.expect("Port write");
        port_channel.send_break().await.expect("Port Break");
        assert_eq!(
            timeout(TEST_MESSAGE_TIMEOUT, port_channel.next())
                .await
                .expect("Port Break timeout")
                .expect("Port Break"),
            crate::PortInbound::Break
        );

        let inputs = session.inputs().expect("Inputs channel handle");
        let mut cursor_events = session.cursor_events().expect("Cursor channel events");
        let mut topology_events = session.display_topology_events();
        inputs
            .set_pointer_position(crate::PointerPosition {
                x: 40,
                y: 50,
                buttons: oxide_spice_protocol::MouseButtons::default(),
                display_id: 0,
            })
            .expect("queue latest pointer");
        inputs
            .try_button_press(
                oxide_spice_protocol::MouseButton::Left,
                oxide_spice_protocol::MouseButtons::LEFT,
            )
            .expect("queue button edge");

        let cursor = cursor_events.next().await.expect("first cursor state");
        assert_eq!(cursor.connection_generation, INITIAL_CONNECTION_GENERATION);
        assert_eq!(cursor.channel_id, 0);
        assert_eq!((cursor.position.x, cursor.position.y), (10, 20));
        assert!(cursor.visible);
        let shape = cursor.shape.expect("cursor shape");
        assert_eq!(shape.unique_id, 42);
        assert_eq!(&*shape.rgba, &[191, 128, 64, 0x40]);

        let topology = topology_events
            .next()
            .await
            .expect("first monitor topology");
        assert_eq!(
            topology.connection_generation,
            INITIAL_CONNECTION_GENERATION
        );
        assert_eq!(topology.graphics_epoch, 1);
        assert_eq!(topology.display_channel_id, 0);
        assert_eq!(topology.maximum_allowed, 4);
        assert_eq!(topology.monitors.len(), 1);
        assert_eq!(topology.monitors[0].surface_id, 0);

        let frame = session.next_frame().await.expect("first bitmap event");
        assert_eq!(frame.display_channel_id, 0);
        assert_eq!(frame.graphics_epoch, 1);
        assert_eq!(frame.surface_id, 0);
        let snapshot = frame.surface.snapshot().await.expect("surface snapshot");
        assert_eq!((snapshot.width, snapshot.height), (1, 1));
        assert_eq!(snapshot.pixels, &[0x30, 0x20, 0x10, u8::MAX]);

        inputs.key_down(0x1E).await.expect("send reset trigger key");
        let topology = topology_events
            .next()
            .await
            .expect("topology after Display Reset");
        assert_eq!(topology.graphics_epoch, 2);
        let frame = session
            .next_frame()
            .await
            .expect("frame after Display Reset");
        assert_eq!(frame.graphics_epoch, 2);
        let snapshot = frame
            .surface
            .snapshot()
            .await
            .expect("second surface snapshot");
        assert_eq!(snapshot.pixels, &[0x60, 0x50, 0x40, u8::MAX]);

        inputs
            .key_up(0x1E)
            .await
            .expect("send second display trigger");
        let topology = topology_events
            .next()
            .await
            .expect("second Display topology");
        assert_eq!(topology.display_channel_id, 1);
        assert_eq!(topology.graphics_epoch, 1);
        let frame = session.next_frame().await.expect("second Display frame");
        assert_eq!(frame.display_channel_id, 1);
        assert_eq!(frame.graphics_epoch, 1);
        let snapshot = frame
            .surface
            .snapshot()
            .await
            .expect("second Display surface snapshot");
        assert_eq!(snapshot.pixels, &[0x90, 0x80, 0x70, u8::MAX]);

        session.shutdown().await.expect("clean session shutdown");
        server.await.expect("fake server task");
    }

    #[tokio::test]
    async fn accepted_connection_cannot_stall_link_forever() {
        let listener = TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("bind stalled server");
        let port = listener.local_addr().expect("listener address").port();
        let server = tokio::spawn(async move {
            let (_stream, _) = listener.accept().await.expect("accepted connection");
            tokio::time::sleep(Duration::from_secs(1)).await;
        });
        let mut options = ConnectOptions::new("127.0.0.1", port, TicketSecret::new("test-ticket"));
        options.handshake_timeout = Duration::from_millis(50);

        let error = Session::connect(options)
            .await
            .expect_err("stalled Link must time out");
        assert_eq!(error.category(), ErrorCategory::Network);
        server.abort();
        let _ = server.await;
    }

    #[tokio::test]
    async fn switch_host_bootstraps_new_session_and_replaces_display() {
        const TARGET_SESSION_ID: u32 = TEST_SESSION_ID + 1;
        let source_listener = TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("bind source server");
        let source_port = source_listener.local_addr().expect("source address").port();
        let target_listener = TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("bind target server");
        let target_port = target_listener.local_addr().expect("target address").port();
        let private_key = RsaPrivateKey::new(&mut OsRng, 1024).expect("test RSA key");
        let target_private_key = private_key.clone();

        let target_server = tokio::spawn(async move {
            let (mut main, _) = target_listener.accept().await.expect("target Main");
            accept_link(&mut main, &target_private_key, 0, ChannelType::Main, 0).await;
            write_test_main_init(&mut main, TARGET_SESSION_ID).await;
            let (message_type, body) = read_mini_message(&mut main).await;
            assert_eq!(message_type, main_client::ATTACH_CHANNELS);
            assert!(body.is_empty());
            write_single_display_list(&mut main).await;

            let (mut display, _) = target_listener.accept().await.expect("target Display");
            accept_link(
                &mut display,
                &target_private_key,
                TARGET_SESSION_ID,
                ChannelType::Display,
                0,
            )
            .await;
            let (message_type, _) = read_mini_message(&mut display).await;
            assert_eq!(message_type, oxide_spice_protocol::display_client::INIT);
            let (message_type, _) = read_mini_message(&mut display).await;
            assert_eq!(
                message_type,
                oxide_spice_protocol::display_client::PREFERRED_COMPRESSION
            );
            let mut surface_create = Vec::with_capacity(20);
            for field in [0, 1, 1, SurfaceFormat::Xrgb32 as u32, 1] {
                surface_create.extend_from_slice(&field.to_le_bytes());
            }
            write_mini_message(
                &mut display,
                oxide_spice_protocol::display_server::SURFACE_CREATE,
                &surface_create,
            )
            .await;
            write_mini_message(
                &mut display,
                oxide_spice_protocol::display_server::DRAW_COPY,
                &one_pixel_draw_copy([0x21, 0x43, 0x65, 0]),
            )
            .await;
            let mut closed = [0; 1];
            let _ = main.read(&mut closed).await;
        });

        let source_server = tokio::spawn(async move {
            let (mut main, _) = source_listener.accept().await.expect("source Main");
            accept_link(&mut main, &private_key, 0, ChannelType::Main, 0).await;
            write_test_main_init(&mut main, TEST_SESSION_ID).await;
            let (message_type, body) = read_mini_message(&mut main).await;
            assert_eq!(message_type, main_client::ATTACH_CHANNELS);
            assert!(body.is_empty());
            write_single_display_list(&mut main).await;

            let (mut display, _) = source_listener.accept().await.expect("source Display");
            accept_link(
                &mut display,
                &private_key,
                TEST_SESSION_ID,
                ChannelType::Display,
                0,
            )
            .await;
            let (message_type, _) = read_mini_message(&mut display).await;
            assert_eq!(message_type, oxide_spice_protocol::display_client::INIT);
            let (message_type, _) = read_mini_message(&mut display).await;
            assert_eq!(
                message_type,
                oxide_spice_protocol::display_client::PREFERRED_COMPRESSION
            );

            let host = b"127.0.0.1\0";
            let mut switch_host = Vec::with_capacity(20 + host.len());
            switch_host.extend_from_slice(&target_port.to_le_bytes());
            switch_host.extend_from_slice(&0_u16.to_le_bytes());
            switch_host.extend_from_slice(&(host.len() as u32).to_le_bytes());
            switch_host.extend_from_slice(&20_u32.to_le_bytes());
            switch_host.extend_from_slice(&0_u32.to_le_bytes());
            switch_host.extend_from_slice(&0_u32.to_le_bytes());
            switch_host.extend_from_slice(host);
            write_mini_message(&mut main, main_server::MIGRATE_SWITCH_HOST, &switch_host).await;
            let mut closed = [0; 1];
            let _ = main.read(&mut closed).await;
            drop(display);
        });

        let mut session = Session::connect(ConnectOptions::new(
            "127.0.0.1",
            source_port,
            TicketSecret::new("test-ticket"),
        ))
        .await
        .expect("connect source session");
        let frame = timeout(TEST_MESSAGE_TIMEOUT, session.next_frame())
            .await
            .expect("target frame timeout")
            .expect("target frame");
        assert_eq!(session.session_id(), TARGET_SESSION_ID);
        assert_eq!(frame.graphics_epoch, 2);
        let snapshot = frame.surface.snapshot().await.expect("target snapshot");
        assert_eq!(snapshot.pixels, &[0x65, 0x43, 0x21, u8::MAX]);
        session.shutdown().await.expect("switch-host shutdown");
        source_server.await.expect("source server task");
        target_server.await.expect("target server task");
    }

    async fn write_test_main_init(stream: &mut TcpStream, session_id: u32) {
        let mut main_init = Vec::with_capacity(32);
        for field in [session_id, 1, 3, 2, 0, 0, 0, 0] {
            main_init.extend_from_slice(&field.to_le_bytes());
        }
        write_mini_message(stream, main_server::INIT, &main_init).await;
    }

    async fn write_single_display_list(stream: &mut TcpStream) {
        let mut channels = 1_u32.to_le_bytes().to_vec();
        channels.extend_from_slice(&[ChannelType::Display as u8, 0]);
        write_mini_message(stream, main_server::CHANNELS_LIST, &channels).await;
    }

    /// Completes the test peer's Link exchange and decrypts the Ticket.
    async fn accept_link(
        stream: &mut TcpStream,
        private_key: &RsaPrivateKey,
        expected_connection_id: u32,
        expected_channel_type: ChannelType,
        expected_channel_id: u8,
    ) {
        let mut header = [0; LINK_HEADER_SIZE];
        stream.read_exact(&mut header).await.expect("link header");
        let header = LinkHeader::decode(&header).expect("valid link header");
        let mut link_body = vec![0; header.body_size as usize];
        stream.read_exact(&mut link_body).await.expect("link body");
        assert_eq!(
            u32::from_le_bytes(link_body[..4].try_into().expect("connection id")),
            expected_connection_id
        );
        assert_eq!(link_body[4], expected_channel_type as u8);
        assert_eq!(link_body[5], expected_channel_id);
        let common_count = usize::try_from(u32::from_le_bytes(
            link_body[6..10]
                .try_into()
                .expect("common capability count"),
        ))
        .expect("common capability count fits usize");
        let channel_count = usize::try_from(u32::from_le_bytes(
            link_body[10..14]
                .try_into()
                .expect("channel capability count"),
        ))
        .expect("channel capability count fits usize");
        let capabilities_offset = usize::try_from(u32::from_le_bytes(
            link_body[14..18].try_into().expect("capability offset"),
        ))
        .expect("capability offset fits usize");
        assert_eq!(common_count, 1);
        let common_word = u32::from_le_bytes(
            link_body[capabilities_offset..capabilities_offset + 4]
                .try_into()
                .expect("common capability word"),
        );
        for capability in [
            common_capability::AUTH_SELECTION,
            common_capability::AUTH_SPICE,
            common_capability::MINI_HEADER,
        ] {
            assert_ne!(common_word & (1 << capability), 0);
        }
        if matches!(
            expected_channel_type,
            ChannelType::Main | ChannelType::Display
        ) {
            assert_eq!(channel_count, 1);
            let channel_word_offset = capabilities_offset + common_count * 4;
            let channel_word = u32::from_le_bytes(
                link_body[channel_word_offset..channel_word_offset + 4]
                    .try_into()
                    .expect("channel capability word"),
            );
            let required_capabilities: &[u32] = match expected_channel_type {
                ChannelType::Main => &[
                    main_capability::SEMI_SEAMLESS_MIGRATION,
                    main_capability::SEAMLESS_MIGRATION,
                ],
                ChannelType::Display => &[
                    display_capability::COMPOSITE,
                    display_capability::A8_SURFACE,
                    display_capability::LZ4_COMPRESSION,
                ],
                _ => unreachable!("channel type guarded above"),
            };
            for capability in required_capabilities {
                assert_ne!(channel_word & (1 << capability), 0);
            }
        }

        let public_der = private_key
            .to_public_key()
            .to_public_key_der()
            .expect("public DER");
        assert_eq!(public_der.as_bytes().len(), SPICE_TICKET_PUBLIC_KEY_SIZE);
        let common_word: u32 = (1 << common_capability::AUTH_SELECTION)
            | (1 << common_capability::AUTH_SPICE)
            | (1 << common_capability::MINI_HEADER);
        let mut reply = vec![0; LINK_REPLY_FIXED_SIZE];
        reply[4..4 + SPICE_TICKET_PUBLIC_KEY_SIZE].copy_from_slice(public_der.as_bytes());
        reply[166..170].copy_from_slice(&1_u32.to_le_bytes());
        let display_capabilities = if expected_channel_type == ChannelType::Display {
            1_u32 << display_capability::PREFERRED_COMPRESSION
        } else {
            0
        };
        reply[170..174].copy_from_slice(&u32::from(display_capabilities != 0).to_le_bytes());
        reply[174..178].copy_from_slice(&(LINK_REPLY_FIXED_SIZE as u32).to_le_bytes());
        reply.extend_from_slice(&common_word.to_le_bytes());
        if display_capabilities != 0 {
            reply.extend_from_slice(&display_capabilities.to_le_bytes());
        }
        let mut response = Vec::new();
        LinkHeader::current(reply.len() as u32).encode(&mut response);
        response.extend_from_slice(&reply);
        stream.write_all(&response).await.expect("link reply");

        let mut mechanism = [0; 4];
        stream
            .read_exact(&mut mechanism)
            .await
            .expect("auth mechanism");
        assert_eq!(u32::from_le_bytes(mechanism), 1);
        let mut encrypted_ticket = [0; 128];
        stream
            .read_exact(&mut encrypted_ticket)
            .await
            .expect("encrypted Ticket");
        let clear_ticket = private_key
            .decrypt(Oaep::new::<Sha1>(), &encrypted_ticket)
            .expect("Ticket decrypts");
        assert_eq!(&clear_ticket, b"test-ticket\0");
        stream
            .write_all(&(LinkError::Ok as u32).to_le_bytes())
            .await
            .expect("link result");
    }

    /// Writes one mini-header message for the negotiated test channel.
    async fn write_mini_message(stream: &mut TcpStream, message_type: u16, body: &[u8]) {
        stream
            .write_all(&message_type.to_le_bytes())
            .await
            .expect("message type");
        stream
            .write_all(&(body.len() as u32).to_le_bytes())
            .await
            .expect("message size");
        stream.write_all(body).await.expect("message body");
    }

    /// Reads one client mini-header message and its bounded test body.
    async fn read_mini_message(stream: &mut TcpStream) -> (u16, Vec<u8>) {
        let mut header = [0; 6];
        timeout(TEST_MESSAGE_TIMEOUT, stream.read_exact(&mut header))
            .await
            .expect("mini header timeout")
            .expect("mini header");
        let message_type = u16::from_le_bytes(header[..2].try_into().expect("message type"));
        let body_size = u32::from_le_bytes(header[2..].try_into().expect("message size"));
        let mut body = vec![0; body_size as usize];
        timeout(TEST_MESSAGE_TIMEOUT, stream.read_exact(&mut body))
            .await
            .expect("message body timeout")
            .expect("message body");
        (message_type, body)
    }

    /// Drains late Agent credit returns until the Main transport closes.
    async fn read_main_shutdown(stream: &mut TcpStream) -> std::io::Result<()> {
        loop {
            let mut header = [0; 6];
            match stream.read_exact(&mut header).await {
                Ok(_) => {}
                Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(()),
                Err(error) => return Err(error),
            }
            let message_type = u16::from_le_bytes(header[..2].try_into().expect("message type"));
            let body_size = usize::try_from(u32::from_le_bytes(
                header[2..].try_into().expect("message size"),
            ))
            .expect("message size fits usize");
            let mut body = vec![0; body_size];
            stream.read_exact(&mut body).await?;
            assert_eq!(message_type, main_client::AGENT_TOKEN);
            assert_eq!(body.len(), 4);
        }
    }

    /// Reads one complete small client Agent message while ignoring returned-credit controls.
    async fn read_client_agent_message(stream: &mut TcpStream) -> (u32, Vec<u8>) {
        loop {
            let (message_type, body) = read_mini_message(stream).await;
            if message_type == main_client::AGENT_TOKEN {
                assert_eq!(body.len(), 4);
                continue;
            }
            assert_eq!(message_type, main_client::AGENT_DATA);
            assert!(body.len() >= oxide_spice_protocol::AGENT_MESSAGE_HEADER_BYTES);
            assert_eq!(
                u32::from_le_bytes(body[..4].try_into().expect("Agent protocol")),
                oxide_spice_protocol::AGENT_PROTOCOL
            );
            let agent_type = u32::from_le_bytes(body[4..8].try_into().expect("Agent type"));
            let payload_size = usize::try_from(u32::from_le_bytes(
                body[16..20].try_into().expect("Agent payload size"),
            ))
            .expect("Agent payload size fits usize");
            assert_eq!(
                body.len(),
                oxide_spice_protocol::AGENT_MESSAGE_HEADER_BYTES + payload_size
            );
            return (
                agent_type,
                body[oxide_spice_protocol::AGENT_MESSAGE_HEADER_BYTES..].to_vec(),
            );
        }
    }

    /// Reads one requested Main control while draining unrelated Agent credit returns.
    async fn read_client_main_control(
        stream: &mut TcpStream,
        requested_type: u16,
    ) -> (u16, Vec<u8>) {
        loop {
            let message = read_mini_message(stream).await;
            if message.0 == main_client::AGENT_TOKEN {
                assert_eq!(message.1.len(), 4);
                continue;
            }
            assert_eq!(message.0, requested_type);
            return message;
        }
    }

    /// Encodes one complete server Agent message that fits a single Main fragment.
    fn agent_wire_message(agent_type: u32, payload: &[u8]) -> Vec<u8> {
        let mut body =
            Vec::with_capacity(oxide_spice_protocol::AGENT_MESSAGE_HEADER_BYTES + payload.len());
        body.extend_from_slice(&oxide_spice_protocol::AGENT_PROTOCOL.to_le_bytes());
        body.extend_from_slice(&agent_type.to_le_bytes());
        body.extend_from_slice(&0_u64.to_le_bytes());
        body.extend_from_slice(
            &u32::try_from(payload.len())
                .expect("bounded Agent test payload")
                .to_le_bytes(),
        );
        body.extend_from_slice(payload);
        body
    }

    /// Builds the exact baseline Draw Copy shape used by the controlled QEMU path.
    fn one_pixel_draw_copy(bgrx: [u8; 4]) -> Vec<u8> {
        const IMAGE_OFFSET: u32 = 57;
        let mut body = Vec::new();
        body.extend_from_slice(&0_u32.to_le_bytes());
        push_rect(&mut body, [0, 0, 1, 1]);
        body.push(0);
        body.extend_from_slice(&IMAGE_OFFSET.to_le_bytes());
        push_rect(&mut body, [0, 0, 1, 1]);
        body.extend_from_slice(&(1_u16 << 3).to_le_bytes());
        body.push(0);
        body.push(0);
        body.extend_from_slice(&0_i32.to_le_bytes());
        body.extend_from_slice(&0_i32.to_le_bytes());
        body.extend_from_slice(&0_u32.to_le_bytes());
        body.extend_from_slice(&1_u64.to_le_bytes());
        body.push(0);
        body.push(0);
        body.extend_from_slice(&1_u32.to_le_bytes());
        body.extend_from_slice(&1_u32.to_le_bytes());
        body.push(8);
        body.push(1 << 2);
        body.extend_from_slice(&1_u32.to_le_bytes());
        body.extend_from_slice(&1_u32.to_le_bytes());
        body.extend_from_slice(&4_u32.to_le_bytes());
        body.extend_from_slice(&0_u32.to_le_bytes());
        body.extend_from_slice(&bgrx);
        body
    }

    /// Builds one literal-only LZ_RGB32 Draw Copy payload.
    fn one_pixel_lz_rgb_draw_copy(bgr: [u8; 3]) -> Vec<u8> {
        let mut compressed = lz_header(8, 1, 1, 4, true);
        compressed.push(0);
        compressed.extend_from_slice(&bgr);
        let mut body = one_pixel_draw_copy_envelope(101, 3);
        body.extend_from_slice(
            &u32::try_from(compressed.len())
                .expect("bounded test LZ payload")
                .to_le_bytes(),
        );
        body.extend_from_slice(&compressed);
        body
    }

    /// Builds one baseline JPEG Draw Copy payload using a test-only pure-Rust encoder.
    fn one_pixel_jpeg_draw_copy(surface_id: u32, rgb: [u8; 3]) -> Vec<u8> {
        let jpeg = encode_one_pixel_jpeg(rgb);
        let mut body = one_pixel_draw_copy_envelope(105, 30);
        body[..4].copy_from_slice(&surface_id.to_le_bytes());
        body.extend_from_slice(
            &u32::try_from(jpeg.len())
                .expect("bounded JPEG test payload")
                .to_le_bytes(),
        );
        body.extend_from_slice(&jpeg);
        body
    }

    /// Builds one baseline JPEG plus a literal-only LZ `XXXA` plane.
    fn one_pixel_jpeg_alpha_draw_copy(
        surface_id: u32,
        rgb: [u8; 3],
        alpha: u8,
        top_down: bool,
    ) -> Vec<u8> {
        let jpeg = encode_one_pixel_jpeg(rgb);
        let mut alpha_lz = lz_header(10, 1, 1, 4, top_down);
        alpha_lz.extend_from_slice(&[0, alpha]);
        let data_size = jpeg
            .len()
            .checked_add(alpha_lz.len())
            .expect("bounded JPEG alpha test payload");
        let mut body = one_pixel_draw_copy_envelope(108, 31);
        body[..4].copy_from_slice(&surface_id.to_le_bytes());
        body.push(u8::from(top_down));
        body.extend_from_slice(
            &u32::try_from(jpeg.len())
                .expect("bounded JPEG test payload")
                .to_le_bytes(),
        );
        body.extend_from_slice(
            &u32::try_from(data_size)
                .expect("bounded JPEG alpha test payload")
                .to_le_bytes(),
        );
        body.extend_from_slice(&jpeg);
        body.extend_from_slice(&alpha_lz);
        body
    }

    fn encode_one_pixel_jpeg(rgb: [u8; 3]) -> Vec<u8> {
        let mut jpeg = Vec::new();
        Encoder::new(&mut jpeg, 100)
            .encode(&rgb, 1, 1, ColorType::Rgb)
            .expect("encode baseline JPEG fixture");
        jpeg
    }

    /// Wraps the official one-pixel RGB32 QUIC interoperability vector.
    fn one_pixel_quic_draw_copy(surface_id: u32) -> Vec<u8> {
        const OFFICIAL_QUIC_RGB32: [u8; 28] = [
            0x51, 0x55, 0x49, 0x43, 0, 0, 0, 0, 4, 0, 0, 0, 1, 0, 0, 0, 1, 0, 0, 0, 0, 0xe6, 0xc4,
            0xa2, 0, 0, 0, 0,
        ];
        compressed_draw_copy(
            surface_id,
            oxide_spice_protocol::DrawCopyImageType::Quic.wire_value(),
            32,
            &OFFICIAL_QUIC_RGB32,
        )
    }

    /// Wraps one literal GLZ image in the protocol's zlib transport form.
    fn one_pixel_zlib_glz_literal_draw_copy(
        surface_id: u32,
        image_id: u64,
        bgr: [u8; 3],
    ) -> Vec<u8> {
        let mut glz = glz_header(image_id, 0);
        glz.push(0);
        glz.extend_from_slice(&bgr);
        let compressed = miniz_oxide::deflate::compress_to_vec_zlib(&glz, 6);
        let mut body = one_pixel_draw_copy_envelope(107, image_id + 20);
        body[..4].copy_from_slice(&surface_id.to_le_bytes());
        body.extend_from_slice(
            &u32::try_from(glz.len())
                .expect("bounded GLZ test payload")
                .to_le_bytes(),
        );
        body.extend_from_slice(
            &u32::try_from(compressed.len())
                .expect("bounded zlib test payload")
                .to_le_bytes(),
        );
        body.extend_from_slice(&compressed);
        body
    }

    /// Builds one GLZ image whose only pixel references the preceding dictionary image.
    fn one_pixel_glz_reference_draw_copy(image_id: u64, window_head_distance: u32) -> Vec<u8> {
        let mut compressed = glz_header(image_id, window_head_distance);
        compressed.extend_from_slice(&[0x20, 0, 1]);
        compressed_draw_copy(0, 102, image_id + 10, &compressed)
    }

    fn compressed_draw_copy(
        surface_id: u32,
        image_type: u8,
        descriptor_id: u64,
        compressed: &[u8],
    ) -> Vec<u8> {
        let mut body = one_pixel_draw_copy_envelope(image_type, descriptor_id);
        body[..4].copy_from_slice(&surface_id.to_le_bytes());
        body.extend_from_slice(
            &u32::try_from(compressed.len())
                .expect("bounded compressed test payload")
                .to_le_bytes(),
        );
        body.extend_from_slice(compressed);
        body
    }

    /// Encodes the fixed stateful GLZ header for one RGB32 pixel.
    fn glz_header(image_id: u64, window_head_distance: u32) -> Vec<u8> {
        let mut output = Vec::new();
        output.extend_from_slice(&0x2020_5a4c_u32.to_be_bytes());
        output.extend_from_slice(&0x0001_0001_u32.to_be_bytes());
        output.push(8 | 0x10);
        output.extend_from_slice(&1_u32.to_be_bytes());
        output.extend_from_slice(&1_u32.to_be_bytes());
        output.extend_from_slice(&4_u32.to_be_bytes());
        output.extend_from_slice(&image_id.to_be_bytes());
        output.extend_from_slice(&window_head_distance.to_be_bytes());
        output
    }

    /// Builds one inline cached LZ_PLT8 Draw Copy payload.
    fn one_pixel_lz_palette_draw_copy(palette_id: u64, bgrx: [u8; 4]) -> Vec<u8> {
        const LZ_PALETTE_FIXED_BYTES: usize = 1 + 4 + 4;
        let mut compressed = lz_header(5, 1, 1, 1, true);
        compressed.extend_from_slice(&[0, 0]);
        let mut body = one_pixel_draw_copy_envelope(100, 4);
        let palette_offset = body
            .len()
            .checked_add(LZ_PALETTE_FIXED_BYTES)
            .and_then(|offset| offset.checked_add(compressed.len()))
            .and_then(|offset| u32::try_from(offset).ok())
            .expect("bounded test palette offset");
        body.push(1);
        body.extend_from_slice(
            &u32::try_from(compressed.len())
                .expect("bounded test LZ payload")
                .to_le_bytes(),
        );
        body.extend_from_slice(&palette_offset.to_le_bytes());
        body.extend_from_slice(&compressed);
        body.extend_from_slice(&palette_id.to_le_bytes());
        body.extend_from_slice(&1_u16.to_le_bytes());
        body.extend_from_slice(&bgrx);
        body
    }

    /// Builds the common one-pixel Draw Copy and image descriptor fields.
    fn one_pixel_draw_copy_envelope(image_type: u8, image_id: u64) -> Vec<u8> {
        const IMAGE_OFFSET: u32 = 57;
        let mut body = Vec::new();
        body.extend_from_slice(&0_u32.to_le_bytes());
        push_rect(&mut body, [0, 0, 1, 1]);
        body.push(0);
        body.extend_from_slice(&IMAGE_OFFSET.to_le_bytes());
        push_rect(&mut body, [0, 0, 1, 1]);
        body.extend_from_slice(&(1_u16 << 3).to_le_bytes());
        body.push(0);
        body.push(0);
        body.extend_from_slice(&0_i32.to_le_bytes());
        body.extend_from_slice(&0_i32.to_le_bytes());
        body.extend_from_slice(&0_u32.to_le_bytes());
        body.extend_from_slice(&image_id.to_le_bytes());
        body.push(image_type);
        body.push(0);
        body.extend_from_slice(&1_u32.to_le_bytes());
        body.extend_from_slice(&1_u32.to_le_bytes());
        body
    }

    /// Encodes the fixed SPICE LZ 1.1 header used by literal test images.
    fn lz_header(image_type: u32, width: u32, height: u32, stride: u32, top_down: bool) -> Vec<u8> {
        let mut output = Vec::new();
        for value in [
            0x2020_5a4c,
            0x0001_0001,
            image_type,
            width,
            height,
            stride,
            u32::from(top_down),
        ] {
            output.extend_from_slice(&value.to_be_bytes());
        }
        output
    }

    /// Writes a SPICE rectangle in its declared top-left-bottom-right order.
    fn push_rect(output: &mut Vec<u8>, coordinates: [i32; 4]) {
        for coordinate in coordinates {
            output.extend_from_slice(&coordinate.to_le_bytes());
        }
    }
}
