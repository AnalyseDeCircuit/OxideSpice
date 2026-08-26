//! Helper-owned session lifecycle and public client API adaptation.

use std::collections::HashMap;
#[cfg(feature = "tls-ring")]
use std::sync::Arc;
use std::time::Duration;

#[cfg(feature = "tls-ring")]
use oxide_spice_client::tls::pki_types::CertificateDer;
#[cfg(feature = "tls-ring")]
use oxide_spice_client::tls::{ClientConfig, RootCertStore};
use oxide_spice_client::{
    AgentClipboardSelection, AgentEvent, AgentEvents, AgentFeatures, AgentHandle, AgentSendError,
    AgentState, ClientError, ConnectOptions, CursorEvents, DisplayTopologyEvents, ErrorCategory,
    FileTransferMetadata, FileTransferState, GuestMonitor, GuestMonitorLayout, InputSendError,
    InputsHandle, KeyboardModifiers, MouseButton, MouseButtons, PlaybackAudioSettings,
    PlaybackPackets, PlaybackState, PointerPosition, PortChannel, PortInbound, PortState,
    RecordAudioSettings, RecordChannel, RecordState, SaslCredentials, SaslOptions, Session,
    SmartcardChannel, TicketSecret, TransportSecurity, UsbRedirChannel,
};
#[cfg(feature = "tls-ring")]
use oxide_spice_client::{MigrationTlsConfiguration, MigrationTlsPolicy};
use oxide_spice_protocol::{AgentClipboardType, ChannelType, Rect};
use tokio::sync::mpsc;
use tokio::task::JoinSet;

use crate::event_writer::EventSender;
use crate::ipc::{
    HelperAgentFeatures, HelperAgentStateKind, HelperAudioDataMode, HelperButtonState,
    HelperChannelCapabilities, HelperClipboardFormat, HelperClipboardSelection,
    HelperConnectOptions, HelperEndpoint, HelperErrorCategory, HelperEvent,
    HelperFileTransferFailure, HelperFileTransferState, HelperGraphicsDevice, HelperIpcError,
    HelperKeyState, HelperMonitor, HelperMouseButton, HelperMouseMode, HelperPixelFormat,
    HelperPlaybackStateKind, HelperPortStateKind, HelperRecordStateKind, HelperRect, HelperRequest,
    HelperSasl, HelperStatus, HelperTopologyMonitor, HelperTransportSecurity,
    HelperUsbDeviceIdentity,
};
#[cfg(feature = "smartcard")]
use crate::smartcard::{list_pcsc_readers, run_smartcard_redirection};
#[cfg(feature = "usbredir")]
use crate::usbredir::{UsbDeviceIdentity, list_usb_devices, run_usb_redirection};
#[cfg(feature = "webdav")]
use crate::webdav::{WebDavConfig, run_webdav};

const RECORD_STATE_POLL_INTERVAL: Duration = Duration::from_millis(20);
#[cfg(feature = "tls-ring")]
const MAX_TLS_ROOT_CERTIFICATES: usize = 64;
#[cfg(feature = "tls-ring")]
const MAX_TLS_ROOT_CERTIFICATE_BYTES: usize = 1024 * 1024;
const MAX_NATIVE_INTEGRATION_TASKS: usize = 64;
const MAX_HELPER_FILE_TRANSFERS: usize = 8;
const FILE_TRANSFER_COMMAND_QUEUE_CAPACITY: usize = 4;
const PORT_COMMAND_QUEUE_CAPACITY: usize = 16;
const PORT_STATE_POLL_INTERVAL: Duration = Duration::from_millis(20);

#[derive(Debug, thiserror::Error)]
pub enum HelperRuntimeError {
    #[error("SPICE client failed: {0}")]
    Client(#[from] ClientError),
    #[error("helper IPC failed: {0}")]
    Ipc(#[from] HelperIpcError),
    #[error("helper configuration is invalid: {0}")]
    Configuration(String),
    #[error("SPICE host API failed: {0}")]
    HostApi(String),
}

struct SessionResources {
    session: Session,
    inputs: Option<InputsHandle>,
    input_mouse_events: Option<InputsHandle>,
    input_modifier_events: Option<InputsHandle>,
    agent: AgentHandle,
    agent_state_events: AgentHandle,
    agent_offer_events: AgentHandle,
    agent_audio_events: AgentHandle,
    agent_graphics_events: AgentHandle,
    agent_events: Option<AgentEvents>,
    cursor_events: Option<CursorEvents>,
    topology_events: DisplayTopologyEvents,
    playback_packets: Option<PlaybackPackets>,
    playback_states: HashMap<u8, PlaybackState>,
    playback_settings: HashMap<u8, PlaybackAudioSettings>,
    record_channels: HashMap<u8, RecordChannel>,
    record_states: HashMap<u8, RecordState>,
    record_settings: HashMap<u8, RecordAudioSettings>,
    usbredir_channel_ids: Vec<u8>,
    smartcard_channel_ids: Vec<u8>,
    webdav_channel_ids: Vec<u8>,
    port_channel_ids: Vec<u8>,
    _usbredir_channels: Vec<UsbRedirChannel>,
    _smartcard_channels: Vec<SmartcardChannel>,
    _port_channels: Vec<oxide_spice_client::PortChannel>,
    integration_tasks: JoinSet<IntegrationCompletion>,
    file_transfer_senders: HashMap<u64, mpsc::Sender<FileTransferCommand>>,
    file_transfer_tasks: JoinSet<u64>,
    port_senders: HashMap<u8, mpsc::Sender<HelperPortCommand>>,
    port_tasks: JoinSet<u8>,
    background_tasks: JoinSet<()>,
    pending_clipboard_requests: HashMap<u64, oxide_spice_client::ClipboardRequest>,
    last_cursor_shape: Option<(u64, u64)>,
}

struct IntegrationCompletion {
    context: String,
    result: Result<(), String>,
}

enum FileTransferCommand {
    Data(Vec<u8>),
    Finish,
    Cancel,
}

enum HelperPortCommand {
    Write(Vec<u8>),
    Break,
}

#[derive(Debug)]
#[cfg(feature = "tls-ring")]
struct HostnameMigrationTlsPolicy {
    client_config: Arc<ClientConfig>,
}

#[cfg(feature = "tls-ring")]
impl MigrationTlsPolicy for HostnameMigrationTlsPolicy {
    fn configure(
        &self,
        destination: &oxide_spice_protocol::MigrationDestination,
    ) -> Result<MigrationTlsConfiguration, ClientError> {
        Ok(MigrationTlsConfiguration {
            server_name: destination.host.clone(),
            client_config: self.client_config.clone(),
        })
    }
}

pub(crate) async fn run_helper(
    mut requests: mpsc::Receiver<HelperRequest>,
    events: EventSender,
) -> Result<(), HelperRuntimeError> {
    let Some(first_request) = requests.recv().await else {
        return Ok(());
    };
    let HelperRequest::Connect { options } = first_request else {
        events.send_control(HelperEvent::Error {
            category: HelperErrorCategory::Configuration,
            message: "the first helper request must be Connect".to_owned(),
        })?;
        return Ok(());
    };

    events.send_control(HelperEvent::Status {
        status: HelperStatus::Connecting,
        message: None,
    })?;
    let connect_options = match build_connect_options(options) {
        Ok(options) => options,
        Err(error) => {
            send_runtime_error(&events, &error)?;
            events.send_control(HelperEvent::Status {
                status: HelperStatus::Failed,
                message: Some(error.to_string()),
            })?;
            return Ok(());
        }
    };
    let session = match Session::connect(connect_options).await {
        Ok(session) => session,
        Err(error) => {
            send_client_error(&events, &error)?;
            events.send_control(HelperEvent::Status {
                status: HelperStatus::Failed,
                message: Some(error.to_string()),
            })?;
            return Ok(());
        }
    };
    let mut resources = take_session_resources(session);
    let capabilities = session_capabilities(&resources);
    let server_identity = resources.session.server_identity();
    events.send_control(HelperEvent::Connected {
        session_id: resources.session.session_id(),
        capabilities,
    })?;
    events.send_control(HelperEvent::ServerIdentity {
        name: server_identity.name.map(|name| name.to_string()),
        uuid: server_identity.uuid,
    })?;
    events.send_control(HelperEvent::Status {
        status: HelperStatus::Connected,
        message: None,
    })?;
    if let Some(inputs) = resources.inputs.as_ref() {
        publish_mouse_mode(inputs.mouse_mode(), &events)?;
        publish_keyboard_modifiers(inputs.modifiers_state(), &events)?;
    }
    publish_agent_state(resources.agent.state(), &events)?;
    publish_agent_audio_volume(resources.agent.audio_volume(), &events)?;
    publish_agent_graphics_devices(resources.agent.graphics_devices(), &events)?;
    start_generic_port_bridges(&mut resources, events.clone());

    let mut record_poll = tokio::time::interval(RECORD_STATE_POLL_INTERVAL);
    loop {
        tokio::select! {
            request = requests.recv() => {
                let Some(request) = request else {
                    break;
                };
                if handle_request(request, &mut resources, &events).await? {
                    break;
                }
            }
            mouse_mode = next_mouse_mode(&mut resources.input_mouse_events) => {
                if let Some(mouse_mode) = mouse_mode {
                    publish_mouse_mode(
                        mouse_mode.map_err(|error| HelperRuntimeError::HostApi(error.to_string()))?,
                        &events,
                    )?;
                }
            }
            modifiers = next_keyboard_modifiers(&mut resources.input_modifier_events) => {
                if let Some(modifiers) = modifiers {
                    publish_keyboard_modifiers(
                        modifiers.map_err(|error| HelperRuntimeError::HostApi(error.to_string()))?,
                        &events,
                    )?;
                }
            }
            frame = resources.session.next_frame() => {
                match frame {
                    Ok(frame) => publish_frame(frame, &events).await?,
                    Err(error) => {
                        if error.category() != ErrorCategory::Cancelled {
                            send_client_error(&events, &error)?;
                        }
                        break;
                    }
                }
            }
            cursor = next_cursor(&mut resources.cursor_events) => {
                if let Some(cursor) = cursor {
                    publish_cursor(cursor?, &mut resources.last_cursor_shape, &events)?;
                }
            }
            topology = resources.topology_events.next() => {
                publish_topology(topology?, &events)?;
            }
            agent_event = next_agent_event(&mut resources.agent_events) => {
                if let Some(agent_event) = agent_event {
                    publish_agent_event(
                        agent_event.map_err(|error| HelperRuntimeError::HostApi(error.to_string()))?,
                        &mut resources.pending_clipboard_requests,
                        &events,
                    )?;
                }
            }
            offers = resources.agent_offer_events.clipboard_offers_changed() => {
                publish_clipboard_offers(
                    offers.map_err(|error| HelperRuntimeError::HostApi(error.to_string()))?,
                    &events,
                )?;
            }
            agent_state = resources.agent_state_events.changed() => {
                let agent_state = agent_state
                    .map_err(|error| HelperRuntimeError::HostApi(error.to_string()))?;
                publish_agent_state(agent_state, &events)?;
            }
            volume = resources.agent_audio_events.audio_volume_changed() => {
                publish_agent_audio_volume(
                    volume.map_err(|error| HelperRuntimeError::HostApi(error.to_string()))?,
                    &events,
                )?;
            }
            devices = resources.agent_graphics_events.graphics_devices_changed() => {
                publish_agent_graphics_devices(
                    devices.map_err(|error| HelperRuntimeError::HostApi(error.to_string()))?,
                    &events,
                )?;
            }
            playback = next_playback(&mut resources.playback_packets) => {
                if let Some(playback) = playback {
                    let packet = playback?;
                    events.send_control(HelperEvent::PlaybackData {
                        channel_id: packet.channel_id,
                        stream_generation: packet.stream_generation,
                        sequence: packet.sequence,
                        timestamp_ms: packet.timestamp_ms,
                        channels: packet.format.channels,
                        sample_rate_hz: packet.format.sample_rate_hz,
                        discontinuity: packet.discontinuity,
                        pcm_s16le: packet.interleaved_s16le.to_vec(),
                    })?;
                }
            }
            _ = record_poll.tick(), if !resources.record_channels.is_empty() || !resources.session.playback_channels().is_empty() => {
                publish_playback_state_changes(&mut resources, &events)?;
                publish_record_state_changes(&mut resources, &events)?;
            }
            integration = resources.integration_tasks.join_next(), if !resources.integration_tasks.is_empty() => {
                if let Some(integration) = integration {
                    publish_integration_completion(integration, &events)?;
                }
            }
            transfer = resources.file_transfer_tasks.join_next(), if !resources.file_transfer_tasks.is_empty() => {
                if let Some(Ok(transfer_id)) = transfer {
                    resources.file_transfer_senders.remove(&transfer_id);
                }
            }
            port = resources.port_tasks.join_next(), if !resources.port_tasks.is_empty() => {
                if let Some(Ok(channel_id)) = port {
                    resources.port_senders.remove(&channel_id);
                }
            }
            _ = resources.background_tasks.join_next(), if !resources.background_tasks.is_empty() => {}
        }
    }

    events.send_control(HelperEvent::Status {
        status: HelperStatus::Closing,
        message: None,
    })?;
    resources.integration_tasks.abort_all();
    while resources.integration_tasks.join_next().await.is_some() {}
    resources.file_transfer_senders.clear();
    resources.file_transfer_tasks.abort_all();
    while resources.file_transfer_tasks.join_next().await.is_some() {}
    resources.port_senders.clear();
    resources.port_tasks.abort_all();
    while resources.port_tasks.join_next().await.is_some() {}
    resources.background_tasks.abort_all();
    while resources.background_tasks.join_next().await.is_some() {}
    let shutdown = resources.session.shutdown().await;
    if let Err(error) = shutdown
        && error.category() != ErrorCategory::Cancelled
    {
        send_client_error(&events, &error)?;
    }
    events.send_control(HelperEvent::Status {
        status: HelperStatus::Disconnected,
        message: None,
    })?;
    Ok(())
}

fn build_connect_options(
    options: HelperConnectOptions,
) -> Result<ConnectOptions, HelperRuntimeError> {
    let HelperConnectOptions {
        endpoint,
        ticket,
        transport_security,
        sasl,
    } = options;
    let mut connect_options = match endpoint {
        HelperEndpoint::Tcp { host, port } => {
            ConnectOptions::new(host, port, TicketSecret::new(ticket.into_inner()))
        }
        #[cfg(unix)]
        HelperEndpoint::Unix { path } => {
            ConnectOptions::new_unix(path, TicketSecret::new(ticket.into_inner()))
        }
    };
    connect_options.enable_gl_scanout = false;
    connect_options.transport_security = match transport_security {
        HelperTransportSecurity::Plain => TransportSecurity::Plain,
        HelperTransportSecurity::Tls {
            server_name,
            root_certificates_der,
        } => {
            #[cfg(not(feature = "tls-ring"))]
            {
                let _ = (server_name, root_certificates_der);
                return Err(HelperRuntimeError::Configuration(
                    "TLS is disabled in this helper build".to_owned(),
                ));
            }
            #[cfg(feature = "tls-ring")]
            {
                if root_certificates_der.is_empty()
                    || root_certificates_der.len() > MAX_TLS_ROOT_CERTIFICATES
                {
                    return Err(HelperRuntimeError::Configuration(
                        "TLS requires a bounded non-empty root certificate list".to_owned(),
                    ));
                }
                let mut roots = RootCertStore::empty();
                for certificate in root_certificates_der {
                    if certificate.is_empty() || certificate.len() > MAX_TLS_ROOT_CERTIFICATE_BYTES
                    {
                        return Err(HelperRuntimeError::Configuration(
                            "TLS root certificate exceeds its size bound".to_owned(),
                        ));
                    }
                    roots.add(CertificateDer::from(certificate)).map_err(|_| {
                        HelperRuntimeError::Configuration("invalid TLS root certificate".to_owned())
                    })?;
                }
                let client_config = Arc::new(
                    ClientConfig::builder()
                        .with_root_certificates(roots)
                        .with_no_client_auth(),
                );
                connect_options.migration_tls_policy = Some(Arc::new(HostnameMigrationTlsPolicy {
                    client_config: client_config.clone(),
                }));
                TransportSecurity::Tls {
                    server_name,
                    client_config,
                }
            }
        }
    };
    connect_options.sasl = sasl.map(build_sasl_options);
    Ok(connect_options)
}

fn build_sasl_options(sasl: HelperSasl) -> SaslOptions {
    match sasl {
        HelperSasl::Gssapi { hostname, service } => {
            let mut options = SaslOptions::gssapi(hostname);
            options.service = service;
            options
        }
        HelperSasl::Password {
            hostname,
            service,
            authentication_id,
            authorization_id,
            password,
            allow_gssapi,
        } => {
            let mut credentials = SaslCredentials::new(authentication_id, password.into_inner());
            if let Some(authorization_id) = authorization_id {
                credentials = credentials.with_authorization_id(authorization_id);
            }
            let mut options = SaslOptions::with_credentials(hostname, credentials);
            options.service = service;
            options.allow_gssapi = allow_gssapi;
            options
        }
    }
}

fn take_session_resources(mut session: Session) -> SessionResources {
    let inputs = session.inputs();
    let input_mouse_events = inputs.clone();
    let input_modifier_events = inputs.clone();
    let agent = session.agent();
    let agent_state_events = agent.clone();
    let agent_offer_events = agent.clone();
    let agent_audio_events = agent.clone();
    let agent_graphics_events = agent.clone();
    let agent_events = session.take_agent_events();
    let cursor_events = session.cursor_events();
    let topology_events = session.display_topology_events();
    let playback_packets = session.take_playback_packets();
    let record_channels: HashMap<_, _> = session
        .take_record_channels()
        .into_iter()
        .map(|channel| (channel.channel_id(), channel))
        .collect();
    let usbredir_channels = session.take_usbredir_channels();
    let usbredir_channel_ids = usbredir_channels
        .iter()
        .map(|channel| channel.channel_id())
        .collect();
    let smartcard_channels = session.take_smartcard_channels();
    let smartcard_channel_ids = smartcard_channels
        .iter()
        .map(|channel| channel.channel_id())
        .collect();
    let port_channels = session.take_port_channels();
    let webdav_channel_ids = port_channels
        .iter()
        .filter(|channel| channel.channel_type() == ChannelType::WebDav)
        .map(|channel| channel.channel_id())
        .collect();
    let port_channel_ids = port_channels
        .iter()
        .filter(|channel| channel.channel_type() == ChannelType::Port)
        .map(|channel| channel.channel_id())
        .collect();
    SessionResources {
        usbredir_channel_ids,
        smartcard_channel_ids,
        webdav_channel_ids,
        port_channel_ids,
        _usbredir_channels: usbredir_channels,
        _smartcard_channels: smartcard_channels,
        _port_channels: port_channels,
        integration_tasks: JoinSet::new(),
        file_transfer_senders: HashMap::new(),
        file_transfer_tasks: JoinSet::new(),
        port_senders: HashMap::new(),
        port_tasks: JoinSet::new(),
        background_tasks: JoinSet::new(),
        session,
        inputs,
        input_mouse_events,
        input_modifier_events,
        agent,
        agent_state_events,
        agent_offer_events,
        agent_audio_events,
        agent_graphics_events,
        agent_events,
        cursor_events,
        topology_events,
        playback_packets,
        playback_states: HashMap::new(),
        playback_settings: HashMap::new(),
        record_channels,
        record_states: HashMap::new(),
        record_settings: HashMap::new(),
        pending_clipboard_requests: HashMap::new(),
        last_cursor_shape: None,
    }
}

#[cfg(feature = "webdav")]
fn start_webdav_integration(
    resources: &mut SessionResources,
    channel_id: u8,
    root: std::path::PathBuf,
    read_only: bool,
) -> Result<(), String> {
    ensure_integration_task_capacity(resources)?;
    let position = resources._port_channels.iter().position(|channel| {
        channel.channel_type() == ChannelType::WebDav && channel.channel_id() == channel_id
    });
    let Some(position) = position else {
        return Err("unknown or already active WebDAV channel id".to_owned());
    };
    let channel = resources._port_channels.swap_remove(position);
    resources.integration_tasks.spawn(async move {
        let result = run_webdav(channel, WebDavConfig { root, read_only })
            .await
            .map_err(|error| error.to_string());
        IntegrationCompletion {
            context: format!("WebDAV channel {channel_id}"),
            result,
        }
    });
    Ok(())
}

#[cfg(not(feature = "webdav"))]
fn start_webdav_integration(
    _resources: &mut SessionResources,
    _channel_id: u8,
    _root: std::path::PathBuf,
    _read_only: bool,
) -> Result<(), String> {
    Err("WebDAV support is disabled in this helper build".to_owned())
}

fn start_generic_port_bridges(resources: &mut SessionResources, events: EventSender) {
    while let Some(position) = resources
        ._port_channels
        .iter()
        .position(|channel| channel.channel_type() == ChannelType::Port)
    {
        let channel = resources._port_channels.swap_remove(position);
        let channel_id = channel.channel_id();
        let (commands, command_receiver) = mpsc::channel(PORT_COMMAND_QUEUE_CAPACITY);
        resources.port_senders.insert(channel_id, commands);
        let task_events = events.clone();
        resources.port_tasks.spawn(async move {
            run_port_bridge(channel, command_receiver, task_events).await;
            channel_id
        });
    }
}

async fn run_port_bridge(
    mut channel: PortChannel,
    mut commands: mpsc::Receiver<HelperPortCommand>,
    events: EventSender,
) {
    let channel_id = channel.channel_id();
    let mut last_state = None;
    let mut state_poll = tokio::time::interval(PORT_STATE_POLL_INTERVAL);
    loop {
        tokio::select! {
            _ = state_poll.tick() => {
                let state = channel.state();
                if last_state.as_ref() != Some(&state) {
                    last_state = Some(state.clone());
                    if publish_port_state(state, &events).is_err() {
                        return;
                    }
                }
            }
            inbound = channel.next() => match inbound {
                Ok(PortInbound::Data { bytes, discontinuity }) => {
                    if events.send_control(HelperEvent::PortData {
                        channel_id,
                        discontinuity,
                        data: bytes.to_vec(),
                    }).is_err() {
                        return;
                    }
                }
                Ok(PortInbound::Break) => {
                    if events.send_control(HelperEvent::PortBreak { channel_id }).is_err() {
                        return;
                    }
                }
                Err(error) => {
                    let _ = events.send_control(HelperEvent::Error {
                        category: HelperErrorCategory::Protocol,
                        message: format!("Port channel {channel_id} failed: {error}"),
                    });
                    return;
                }
            },
            command = commands.recv() => match command {
                Some(HelperPortCommand::Write(data)) => {
                    if let Err(error) = channel.write(&data).await {
                        let _ = events.send_control(HelperEvent::Error {
                            category: HelperErrorCategory::Protocol,
                            message: format!("Port channel {channel_id} rejected data: {error}"),
                        });
                    }
                }
                Some(HelperPortCommand::Break) => {
                    if let Err(error) = channel.send_break().await {
                        let _ = events.send_control(HelperEvent::Error {
                            category: HelperErrorCategory::Protocol,
                            message: format!("Port channel {channel_id} rejected break: {error}"),
                        });
                    }
                }
                None => return,
            }
        }
    }
}

fn publish_port_state(state: PortState, events: &EventSender) -> Result<(), HelperIpcError> {
    let (connection_generation, channel_id, state, name, opened) = match state {
        PortState::AwaitingInit {
            connection_generation,
            channel_id,
            ..
        } => (
            connection_generation,
            channel_id,
            HelperPortStateKind::AwaitingInit,
            None,
            false,
        ),
        PortState::Ready {
            connection_generation,
            channel_id,
            name,
            opened,
            ..
        } => (
            connection_generation,
            channel_id,
            HelperPortStateKind::Ready,
            Some(name.to_string()),
            opened,
        ),
        PortState::Closed {
            connection_generation,
            channel_id,
            name,
            ..
        } => (
            connection_generation,
            channel_id,
            HelperPortStateKind::Closed,
            name.map(|name| name.to_string()),
            false,
        ),
    };
    events.send_control(HelperEvent::PortState {
        connection_generation,
        channel_id,
        state,
        name,
        opened,
    })
}

fn send_port_command(
    resources: &SessionResources,
    channel_id: u8,
    command: HelperPortCommand,
) -> Result<(), String> {
    let sender = resources
        .port_senders
        .get(&channel_id)
        .ok_or_else(|| "unknown or closed Port channel id".to_owned())?;
    sender.try_send(command).map_err(|error| match error {
        mpsc::error::TrySendError::Full(_) => "Port command queue is full".to_owned(),
        mpsc::error::TrySendError::Closed(_) => "Port task is closed".to_owned(),
    })
}

#[cfg(feature = "usbredir")]
fn start_usb_integration(
    resources: &mut SessionResources,
    channel_id: u8,
    device: HelperUsbDeviceIdentity,
) -> Result<(), String> {
    ensure_integration_task_capacity(resources)?;
    let position = resources
        ._usbredir_channels
        .iter()
        .position(|channel| channel.channel_id() == channel_id);
    let Some(position) = position else {
        return Err("unknown or already active USB redirection channel id".to_owned());
    };
    let channel = resources._usbredir_channels.swap_remove(position);
    resources.integration_tasks.spawn(async move {
        let result = run_usb_redirection(
            channel,
            UsbDeviceIdentity {
                bus_number: device.bus_number,
                device_address: device.device_address,
                vendor_id: device.vendor_id,
                product_id: device.product_id,
            },
        )
        .await
        .map_err(|error| error.to_string());
        IntegrationCompletion {
            context: format!("USB redirection channel {channel_id}"),
            result,
        }
    });
    Ok(())
}

#[cfg(not(feature = "usbredir"))]
fn start_usb_integration(
    _resources: &mut SessionResources,
    _channel_id: u8,
    _device: HelperUsbDeviceIdentity,
) -> Result<(), String> {
    Err("USB redirection support is disabled in this helper build".to_owned())
}

#[cfg(feature = "smartcard")]
fn start_smartcard_integration(
    resources: &mut SessionResources,
    channel_id: u8,
    display_name: String,
) -> Result<(), String> {
    ensure_integration_task_capacity(resources)?;
    let position = resources
        ._smartcard_channels
        .iter()
        .position(|channel| channel.channel_id() == channel_id);
    let Some(position) = position else {
        return Err("unknown or already active Smartcard channel id".to_owned());
    };
    let channel = resources._smartcard_channels.swap_remove(position);
    resources.integration_tasks.spawn(async move {
        let result = async {
            let readers = tokio::task::spawn_blocking(list_pcsc_readers)
                .await
                .map_err(|_| "PC/SC reader discovery panicked".to_owned())?
                .map_err(|error| error.to_string())?;
            let reader = readers
                .into_iter()
                .find(|reader| reader.display_name() == display_name)
                .ok_or_else(|| format!("PC/SC reader {display_name:?} is not available"))?;
            run_smartcard_redirection(channel, reader)
                .await
                .map_err(|error| error.to_string())
        }
        .await;
        IntegrationCompletion {
            context: format!("Smartcard channel {channel_id}"),
            result,
        }
    });
    Ok(())
}

#[cfg(not(feature = "smartcard"))]
fn start_smartcard_integration(
    _resources: &mut SessionResources,
    _channel_id: u8,
    _display_name: String,
) -> Result<(), String> {
    Err("Smartcard redirection support is disabled in this helper build".to_owned())
}

fn ensure_integration_task_capacity(resources: &SessionResources) -> Result<(), String> {
    if resources.integration_tasks.len() >= MAX_NATIVE_INTEGRATION_TASKS {
        Err("native integration task limit reached".to_owned())
    } else {
        Ok(())
    }
}

fn session_capabilities(resources: &SessionResources) -> HelperChannelCapabilities {
    HelperChannelCapabilities {
        inputs: resources.inputs.is_some(),
        raw_scancodes: resources
            .inputs
            .as_ref()
            .is_some_and(InputsHandle::raw_scancodes_supported),
        cursor: resources.cursor_events.is_some(),
        agent: true,
        playback_channel_ids: resources
            .session
            .playback_channels()
            .iter()
            .map(|channel| channel.channel_id())
            .collect(),
        record_channel_ids: resources.record_channels.keys().copied().collect(),
        port_channel_ids: resources.port_channel_ids.clone(),
        usbredir_channel_ids: resources.usbredir_channel_ids.clone(),
        smartcard_channel_ids: resources.smartcard_channel_ids.clone(),
        webdav_channel_ids: resources.webdav_channel_ids.clone(),
    }
}

fn publish_integration_completion(
    completion: Result<IntegrationCompletion, tokio::task::JoinError>,
    events: &EventSender,
) -> Result<(), HelperIpcError> {
    match completion {
        Ok(IntegrationCompletion { result: Ok(()), .. }) => Ok(()),
        Ok(IntegrationCompletion {
            context,
            result: Err(message),
        }) => events.send_control(HelperEvent::Error {
            category: HelperErrorCategory::Internal,
            message: format!("{context} failed: {message}"),
        }),
        Err(error) => events.send_control(HelperEvent::Error {
            category: HelperErrorCategory::Internal,
            message: format!("native integration task failed: {error}"),
        }),
    }
}

async fn handle_request(
    request: HelperRequest,
    resources: &mut SessionResources,
    events: &EventSender,
) -> Result<bool, HelperRuntimeError> {
    let result = match request {
        HelperRequest::Close => return Ok(true),
        HelperRequest::Connect { .. } => Err("session is already connected".to_owned()),
        HelperRequest::PointerPosition {
            x,
            y,
            buttons,
            display_id,
        } => with_inputs(resources, |inputs| {
            inputs
                .set_pointer_position(PointerPosition {
                    x,
                    y,
                    buttons: mouse_buttons(buttons)?,
                    display_id,
                })
                .map_err(input_error)
        }),
        HelperRequest::PointerMotion { dx, dy, buttons } => {
            let Some(inputs) = resources.inputs.as_ref() else {
                return send_action_error(events, "server has no Inputs channel");
            };
            match mouse_buttons(buttons) {
                Ok(buttons) => inputs
                    .move_pointer(dx, dy, buttons)
                    .await
                    .map_err(input_error),
                Err(message) => Err(message.to_owned()),
            }
        }
        HelperRequest::MouseButton {
            button,
            state,
            buttons,
        } => {
            let Some(inputs) = resources.inputs.as_ref() else {
                return send_action_error(events, "server has no Inputs channel");
            };
            match mouse_buttons(buttons) {
                Ok(buttons) => match state {
                    HelperButtonState::Pressed => {
                        inputs.button_press(mouse_button(button), buttons).await
                    }
                    HelperButtonState::Released => {
                        inputs.button_release(mouse_button(button), buttons).await
                    }
                }
                .map_err(input_error),
                Err(message) => Err(message.to_owned()),
            }
        }
        HelperRequest::KeyCode { code, state } => {
            let Some(inputs) = resources.inputs.as_ref() else {
                return send_action_error(events, "server has no Inputs channel");
            };
            match state {
                HelperKeyState::Pressed => inputs.key_down(code).await,
                HelperKeyState::Released => inputs.key_up(code).await,
            }
            .map_err(input_error)
        }
        HelperRequest::Scancodes { bytes } => {
            let Some(inputs) = resources.inputs.as_ref() else {
                return send_action_error(events, "server has no Inputs channel");
            };
            inputs.scancodes(&bytes).await.map_err(input_error)
        }
        HelperRequest::Modifiers { bits } => {
            let Some(inputs) = resources.inputs.as_ref() else {
                return send_action_error(events, "server has no Inputs channel");
            };
            match KeyboardModifiers::from_bits(bits) {
                Ok(modifiers) => inputs.modifiers(modifiers).await.map_err(input_error),
                Err(_) => Err("invalid keyboard modifier bits".to_owned()),
            }
        }
        HelperRequest::ClipboardOffer { selection, formats } => resources
            .agent
            .offer_clipboard(
                clipboard_selection(selection),
                formats
                    .into_iter()
                    .map(clipboard_type)
                    .collect::<Vec<_>>()
                    .into(),
            )
            .await
            .map_err(agent_error),
        HelperRequest::ClipboardRelease { selection } => resources
            .agent
            .release_clipboard(clipboard_selection(selection))
            .await
            .map_err(agent_error),
        HelperRequest::ClipboardRequest { selection, format } => {
            let agent = resources.agent.clone();
            let events = events.clone();
            resources.background_tasks.spawn(async move {
                match agent
                    .request_clipboard(clipboard_selection(selection), clipboard_type(format))
                    .await
                {
                    Ok(data) => {
                        let _ = events.send_control(HelperEvent::ClipboardData {
                            selection,
                            format,
                            data: data.to_vec(),
                        });
                    }
                    Err(error) => {
                        let _ = events.send_control(HelperEvent::Error {
                            category: HelperErrorCategory::Protocol,
                            message: error.to_string(),
                        });
                    }
                }
            });
            Ok(())
        }
        HelperRequest::ClipboardProvide { request_id, data } => {
            if !resources
                .pending_clipboard_requests
                .contains_key(&request_id)
            {
                Err("unknown clipboard request id".to_owned())
            } else {
                let result = resources
                    .agent
                    .provide_clipboard(request_id, data.into())
                    .await
                    .map_err(agent_error);
                resources.pending_clipboard_requests.remove(&request_id);
                result
            }
        }
        HelperRequest::FileTransferStart {
            transfer_id,
            file_name,
            size,
        } => start_file_transfer(resources, events.clone(), transfer_id, file_name, size),
        HelperRequest::FileTransferData { transfer_id, data } => {
            send_file_transfer_command(resources, transfer_id, FileTransferCommand::Data(data))
        }
        HelperRequest::FileTransferFinish { transfer_id } => {
            send_file_transfer_command(resources, transfer_id, FileTransferCommand::Finish)
        }
        HelperRequest::FileTransferCancel { transfer_id } => {
            send_file_transfer_command(resources, transfer_id, FileTransferCommand::Cancel)
        }
        HelperRequest::PortWrite { channel_id, data } => {
            send_port_command(resources, channel_id, HelperPortCommand::Write(data))
        }
        HelperRequest::PortBreak { channel_id } => {
            send_port_command(resources, channel_id, HelperPortCommand::Break)
        }
        HelperRequest::MonitorLayout { monitors } => resources
            .agent
            .set_monitor_layout(GuestMonitorLayout {
                monitors: monitors
                    .into_iter()
                    .map(guest_monitor)
                    .collect::<Vec<_>>()
                    .into(),
            })
            .map_err(agent_error),
        HelperRequest::SyncAgentAudioVolume {
            is_playback,
            muted,
            volumes,
        } => resources
            .agent
            .sync_audio_volume(is_playback, muted, volumes)
            .await
            .map_err(agent_error),
        HelperRequest::RecordBegin { channel_id } => {
            let Some(channel) = resources.record_channels.get(&channel_id) else {
                return send_action_error(events, "unknown Record channel id");
            };
            channel
                .begin()
                .await
                .map(|_| ())
                .map_err(|error| error.to_string())
        }
        HelperRequest::RecordData {
            channel_id,
            timestamp_ms,
            pcm_s16le,
        } => {
            let Some(channel) = resources.record_channels.get(&channel_id) else {
                return send_action_error(events, "unknown Record channel id");
            };
            channel
                .send_pcm_at(timestamp_ms, &pcm_s16le)
                .await
                .map_err(|error| error.to_string())
        }
        HelperRequest::ListNativeDevices => {
            start_native_device_discovery(resources, events.clone())
        }
        HelperRequest::StartWebDav {
            channel_id,
            root,
            read_only,
        } => start_webdav_integration(resources, channel_id, root, read_only),
        HelperRequest::StartUsbRedirection { channel_id, device } => {
            start_usb_integration(resources, channel_id, device)
        }
        HelperRequest::StartSmartcardRedirection {
            channel_id,
            display_name,
        } => start_smartcard_integration(resources, channel_id, display_name),
    };
    if let Err(message) = result {
        events.send_control(HelperEvent::Error {
            category: HelperErrorCategory::Protocol,
            message,
        })?;
    }
    Ok(false)
}

fn start_file_transfer(
    resources: &mut SessionResources,
    events: EventSender,
    transfer_id: u64,
    file_name: String,
    size: u64,
) -> Result<(), String> {
    if resources.file_transfer_senders.len() >= MAX_HELPER_FILE_TRANSFERS {
        return Err("helper file-transfer limit reached".to_owned());
    }
    if resources.file_transfer_senders.contains_key(&transfer_id) {
        return Err("helper file-transfer id is already active".to_owned());
    }
    let (commands, command_receiver) = mpsc::channel(FILE_TRANSFER_COMMAND_QUEUE_CAPACITY);
    resources
        .file_transfer_senders
        .insert(transfer_id, commands);
    let agent = resources.agent.clone();
    resources.file_transfer_tasks.spawn(async move {
        run_file_transfer(
            agent,
            events,
            transfer_id,
            FileTransferMetadata { file_name, size },
            command_receiver,
        )
        .await;
        transfer_id
    });
    Ok(())
}

fn send_file_transfer_command(
    resources: &SessionResources,
    transfer_id: u64,
    command: FileTransferCommand,
) -> Result<(), String> {
    let sender = resources
        .file_transfer_senders
        .get(&transfer_id)
        .ok_or_else(|| "unknown helper file-transfer id".to_owned())?;
    sender.try_send(command).map_err(|error| match error {
        mpsc::error::TrySendError::Full(_) => "file-transfer command queue is full".to_owned(),
        mpsc::error::TrySendError::Closed(_) => "file-transfer task is closed".to_owned(),
    })
}

async fn run_file_transfer(
    agent: AgentHandle,
    events: EventSender,
    transfer_id: u64,
    metadata: FileTransferMetadata,
    mut commands: mpsc::Receiver<FileTransferCommand>,
) {
    let mut transfer = match agent.start_file_transfer(metadata).await {
        Ok(transfer) => transfer,
        Err(error) => {
            let _ = events.send_control(HelperEvent::Error {
                category: HelperErrorCategory::Protocol,
                message: format!("file transfer {transfer_id} failed to start: {error}"),
            });
            let _ = events.send_control(HelperEvent::FileTransferState {
                transfer_id,
                state: HelperFileTransferState::Failed,
                accepted_bytes: 0,
                failure: None,
            });
            return;
        }
    };
    let mut accepted_bytes = 0;
    let _ = publish_file_transfer_state(transfer_id, transfer.state(), accepted_bytes, &events);
    loop {
        tokio::select! {
            state = transfer.changed() => {
                let state = match state {
                    Ok(state) => state,
                    Err(error) => {
                        let _ = events.send_control(HelperEvent::Error {
                            category: HelperErrorCategory::Protocol,
                            message: format!("file transfer {transfer_id} state failed: {error}"),
                        });
                        return;
                    }
                };
                accepted_bytes = file_transfer_accepted_bytes(state, accepted_bytes);
                let terminal = is_terminal_file_transfer_state(state);
                let _ = publish_file_transfer_state(transfer_id, state, accepted_bytes, &events);
                if terminal {
                    return;
                }
            }
            command = commands.recv() => match command {
                Some(FileTransferCommand::Data(data)) => {
                    if let Err(error) = transfer.send_chunk(&data).await {
                        let _ = events.send_control(HelperEvent::Error {
                            category: HelperErrorCategory::Protocol,
                            message: format!("file transfer {transfer_id} rejected data: {error}"),
                        });
                    }
                }
                Some(FileTransferCommand::Finish) => {
                    match transfer.finish_with_state().await {
                        Ok(state) => {
                            accepted_bytes = file_transfer_accepted_bytes(state, accepted_bytes);
                            let _ = publish_file_transfer_state(
                                transfer_id,
                                state,
                                accepted_bytes,
                                &events,
                            );
                        }
                        Err(error) => {
                            let _ = events.send_control(HelperEvent::Error {
                                category: HelperErrorCategory::Protocol,
                                message: format!("file transfer {transfer_id} failed to finish: {error}"),
                            });
                        }
                    }
                    return;
                }
                Some(FileTransferCommand::Cancel) | None => {
                    if let Err(error) = transfer.cancel().await {
                        let _ = events.send_control(HelperEvent::Error {
                            category: HelperErrorCategory::Protocol,
                            message: format!("file transfer {transfer_id} failed to cancel: {error}"),
                        });
                    } else {
                        let _ = events.send_control(HelperEvent::FileTransferState {
                            transfer_id,
                            state: HelperFileTransferState::Cancelled,
                            accepted_bytes,
                            failure: None,
                        });
                    }
                    return;
                }
            }
        }
    }
}

fn file_transfer_accepted_bytes(state: FileTransferState, previous: u64) -> u64 {
    match state {
        FileTransferState::Sending { accepted_bytes }
        | FileTransferState::AwaitingCompletion { accepted_bytes } => accepted_bytes,
        _ => previous,
    }
}

fn is_terminal_file_transfer_state(state: FileTransferState) -> bool {
    matches!(
        state,
        FileTransferState::Completed
            | FileTransferState::Cancelled
            | FileTransferState::Failed { .. }
            | FileTransferState::AgentDisconnected
    )
}

fn publish_file_transfer_state(
    transfer_id: u64,
    state: FileTransferState,
    accepted_bytes: u64,
    events: &EventSender,
) -> Result<(), HelperIpcError> {
    let (state, failure) = match state {
        FileTransferState::WaitingForGuest => (HelperFileTransferState::WaitingForGuest, None),
        FileTransferState::Sending { .. } => (HelperFileTransferState::Sending, None),
        FileTransferState::AwaitingCompletion { .. } => {
            (HelperFileTransferState::AwaitingCompletion, None)
        }
        FileTransferState::Completed => (HelperFileTransferState::Completed, None),
        FileTransferState::Cancelled => (HelperFileTransferState::Cancelled, None),
        FileTransferState::Failed { failure } => (
            HelperFileTransferState::Failed,
            Some(helper_file_transfer_failure(failure)),
        ),
        FileTransferState::AgentDisconnected => (HelperFileTransferState::AgentDisconnected, None),
    };
    events.send_control(HelperEvent::FileTransferState {
        transfer_id,
        state,
        accepted_bytes,
        failure,
    })
}

fn helper_file_transfer_failure(
    failure: oxide_spice_protocol::AgentFileTransferFailure,
) -> HelperFileTransferFailure {
    match failure {
        oxide_spice_protocol::AgentFileTransferFailure::RemoteError {
            error_domain,
            error_code,
        } => HelperFileTransferFailure::RemoteError {
            error_domain,
            error_code,
        },
        oxide_spice_protocol::AgentFileTransferFailure::NotEnoughSpace { available_bytes } => {
            HelperFileTransferFailure::NotEnoughSpace { available_bytes }
        }
        oxide_spice_protocol::AgentFileTransferFailure::SessionLocked => {
            HelperFileTransferFailure::SessionLocked
        }
        oxide_spice_protocol::AgentFileTransferFailure::AgentNotConnected => {
            HelperFileTransferFailure::AgentNotConnected
        }
        oxide_spice_protocol::AgentFileTransferFailure::Disabled => {
            HelperFileTransferFailure::Disabled
        }
    }
}

fn start_native_device_discovery(
    resources: &mut SessionResources,
    events: EventSender,
) -> Result<(), String> {
    ensure_integration_task_capacity(resources)?;
    resources.integration_tasks.spawn(async move {
        let result = async {
            let (usb_devices, smartcard_readers) =
                tokio::join!(discover_usb_devices(), discover_smartcard_readers());
            events
                .send_control(HelperEvent::NativeDevices {
                    usb_devices: usb_devices?,
                    smartcard_readers: smartcard_readers?,
                })
                .map_err(|error| error.to_string())
        }
        .await;
        IntegrationCompletion {
            context: "native device discovery".to_owned(),
            result,
        }
    });
    Ok(())
}

#[cfg(feature = "usbredir")]
async fn discover_usb_devices() -> Result<Vec<HelperUsbDeviceIdentity>, String> {
    tokio::task::spawn_blocking(list_usb_devices)
        .await
        .map_err(|_| "USB device discovery panicked".to_owned())?
        .map_err(|error| error.to_string())
        .map(|devices| {
            devices
                .into_iter()
                .map(|device| HelperUsbDeviceIdentity {
                    bus_number: device.bus_number,
                    device_address: device.device_address,
                    vendor_id: device.vendor_id,
                    product_id: device.product_id,
                })
                .collect()
        })
}

#[cfg(not(feature = "usbredir"))]
async fn discover_usb_devices() -> Result<Vec<HelperUsbDeviceIdentity>, String> {
    Ok(Vec::new())
}

#[cfg(feature = "smartcard")]
async fn discover_smartcard_readers() -> Result<Vec<String>, String> {
    tokio::task::spawn_blocking(list_pcsc_readers)
        .await
        .map_err(|_| "PC/SC reader discovery panicked".to_owned())?
        .map_err(|error| error.to_string())
        .map(|readers| {
            readers
                .into_iter()
                .map(|reader| reader.display_name())
                .collect()
        })
}

#[cfg(not(feature = "smartcard"))]
async fn discover_smartcard_readers() -> Result<Vec<String>, String> {
    Ok(Vec::new())
}

fn with_inputs(
    resources: &SessionResources,
    operation: impl FnOnce(&InputsHandle) -> Result<(), String>,
) -> Result<(), String> {
    resources
        .inputs
        .as_ref()
        .ok_or_else(|| "server has no Inputs channel".to_owned())
        .and_then(operation)
}

async fn publish_frame(
    frame: oxide_spice_client::FrameEvent,
    events: &EventSender,
) -> Result<(), HelperRuntimeError> {
    if !frame.surface.is_primary() {
        return Ok(());
    }
    let full_refresh = frame.full_refresh_required || events.has_pending_frame()?;
    let (rect, snapshot) = if full_refresh {
        (
            HelperRect {
                x: 0,
                y: 0,
                width: frame.surface.width(),
                height: frame.surface.height(),
            },
            frame.surface.snapshot().await?,
        )
    } else {
        (
            helper_rect(frame.dirty)?,
            frame.surface.snapshot_region(frame.dirty).await?,
        )
    };
    events.send_frame(HelperEvent::Frame {
        connection_generation: frame.connection_generation,
        graphics_epoch: frame.graphics_epoch,
        display_channel_id: frame.display_channel_id,
        surface_id: frame.surface_id,
        surface_width: frame.surface.width(),
        surface_height: frame.surface.height(),
        rect,
        full_refresh,
        format: HelperPixelFormat::Rgba8,
        pixels: snapshot.pixels,
    })?;
    Ok(())
}

fn publish_cursor(
    cursor: oxide_spice_client::CursorState,
    last_shape: &mut Option<(u64, u64)>,
    events: &EventSender,
) -> Result<(), HelperIpcError> {
    let shape_key = cursor
        .shape
        .as_ref()
        .map(|shape| (cursor.cursor_epoch, shape.unique_id));
    let include_shape = shape_key.is_some() && shape_key != *last_shape;
    let (width, height, hot_spot_x, hot_spot_y, shape_id, rgba) = match cursor.shape {
        Some(shape) => (
            shape.width,
            shape.height,
            shape.hot_spot_x,
            shape.hot_spot_y,
            Some(shape.unique_id),
            include_shape
                .then(|| shape.rgba.to_vec())
                .unwrap_or_default(),
        ),
        None => (0, 0, 0, 0, None, Vec::new()),
    };
    *last_shape = shape_key;
    events.send_control(HelperEvent::Cursor {
        connection_generation: cursor.connection_generation,
        cursor_epoch: cursor.cursor_epoch,
        channel_id: cursor.channel_id,
        x: i32::from(cursor.position.x),
        y: i32::from(cursor.position.y),
        visible: cursor.visible,
        width,
        height,
        hot_spot_x,
        hot_spot_y,
        shape_id,
        rgba,
    })
}

fn publish_topology(
    topology: oxide_spice_client::DisplayTopology,
    events: &EventSender,
) -> Result<(), HelperIpcError> {
    events.send_control(HelperEvent::Topology {
        connection_generation: topology.connection_generation,
        graphics_epoch: topology.graphics_epoch,
        display_channel_id: topology.display_channel_id,
        maximum_allowed: topology.maximum_allowed,
        monitors: topology
            .monitors
            .iter()
            .map(|monitor| HelperTopologyMonitor {
                id: monitor.monitor_id,
                surface_id: monitor.surface_id,
                width: monitor.width,
                height: monitor.height,
                x: monitor.x,
                y: monitor.y,
                flags: monitor.flags,
            })
            .collect(),
    })
}

fn publish_agent_event(
    event: AgentEvent,
    pending: &mut HashMap<u64, oxide_spice_client::ClipboardRequest>,
    events: &EventSender,
) -> Result<(), HelperIpcError> {
    match event {
        AgentEvent::ClipboardRequested(request) => {
            let event = HelperEvent::ClipboardRequest {
                request_id: request.request_id,
                selection: helper_clipboard_selection(request.selection),
                format: helper_clipboard_format(request.clipboard_type),
            };
            if pending.insert(request.request_id, request).is_some() {
                return Err(HelperIpcError::Io(std::io::Error::other(
                    "duplicate clipboard request id",
                )));
            }
            events.send_control(event)
        }
    }
}

fn publish_agent_state(state: AgentState, events: &EventSender) -> Result<(), HelperIpcError> {
    let (connection_generation, agent_generation, state, reason, features) = match state {
        AgentState::Disconnected {
            connection_generation,
            agent_generation,
            reason,
        } => (
            connection_generation,
            agent_generation,
            HelperAgentStateKind::Disconnected,
            reason,
            None,
        ),
        AgentState::Negotiating {
            connection_generation,
            agent_generation,
        } => (
            connection_generation,
            agent_generation,
            HelperAgentStateKind::Negotiating,
            None,
            None,
        ),
        AgentState::Ready {
            connection_generation,
            agent_generation,
            features,
        } => (
            connection_generation,
            agent_generation,
            HelperAgentStateKind::Ready,
            None,
            Some(helper_agent_features(features)),
        ),
    };
    events.send_control(HelperEvent::AgentState {
        connection_generation,
        agent_generation,
        state,
        reason,
        features,
    })
}

fn helper_agent_features(features: AgentFeatures) -> HelperAgentFeatures {
    HelperAgentFeatures {
        clipboard_by_demand: features.clipboard_by_demand,
        clipboard_selection: features.clipboard_selection,
        clipboard_grab_serial: features.clipboard_grab_serial,
        monitor_config: features.monitor_config,
        sparse_monitors: features.sparse_monitors,
        monitor_positions: features.monitor_positions,
        monitor_physical_size: features.monitor_physical_size,
        file_transfer_disabled: features.file_transfer_disabled,
        file_transfer_detailed_errors: features.file_transfer_detailed_errors,
        audio_volume_sync: features.audio_volume_sync,
        graphics_device_info: features.graphics_device_info,
    }
}

fn publish_agent_audio_volume(
    volume: Option<oxide_spice_client::AgentAudioVolumeState>,
    events: &EventSender,
) -> Result<(), HelperIpcError> {
    match volume {
        Some(volume) => events.send_control(HelperEvent::AgentAudioVolume {
            connection_generation: volume.connection_generation,
            agent_generation: volume.agent_generation,
            is_playback: volume.is_playback,
            muted: volume.muted,
            volumes: volume.volumes.to_vec(),
        }),
        None => events.send_control(HelperEvent::AgentAudioVolumeReset),
    }
}

fn publish_agent_graphics_devices(
    devices: Option<oxide_spice_client::AgentGraphicsDeviceState>,
    events: &EventSender,
) -> Result<(), HelperIpcError> {
    match devices {
        Some(devices) => events.send_control(HelperEvent::AgentGraphicsDevices {
            connection_generation: devices.connection_generation,
            agent_generation: devices.agent_generation,
            displays: devices
                .displays
                .iter()
                .map(|display| HelperGraphicsDevice {
                    channel_id: display.channel_id,
                    monitor_id: display.monitor_id,
                    device_display_id: display.device_display_id,
                    device_address: display.device_address.clone(),
                })
                .collect(),
        }),
        None => events.send_control(HelperEvent::AgentGraphicsDevicesReset),
    }
}

fn publish_clipboard_offers(
    offers: [Option<oxide_spice_client::ClipboardOffer>; 3],
    events: &EventSender,
) -> Result<(), HelperIpcError> {
    for (index, offer) in offers.into_iter().enumerate() {
        let selection = match index {
            0 => HelperClipboardSelection::Clipboard,
            1 => HelperClipboardSelection::Primary,
            2 => HelperClipboardSelection::Secondary,
            _ => unreachable!("clipboard selection array has three entries"),
        };
        let (revision, formats) = offer.map_or((0, Vec::new()), |offer| {
            (
                offer.revision,
                offer
                    .types
                    .iter()
                    .filter_map(|value| AgentClipboardType::try_from(*value).ok())
                    .filter(|value| *value != AgentClipboardType::None)
                    .map(helper_clipboard_format)
                    .collect(),
            )
        });
        events.send_control(HelperEvent::ClipboardOffer {
            selection,
            revision,
            formats,
        })?;
    }
    Ok(())
}

fn publish_playback_state_changes(
    resources: &mut SessionResources,
    events: &EventSender,
) -> Result<(), HelperIpcError> {
    let snapshots: Vec<_> = resources
        .session
        .playback_channels()
        .iter()
        .map(|channel| {
            (
                channel.channel_id(),
                channel.state(),
                channel.audio_settings(),
            )
        })
        .collect();
    for (channel_id, current, settings) in snapshots {
        if resources.playback_states.get(&channel_id) != Some(&current) {
            resources.playback_states.insert(channel_id, current);
            let (
                connection_generation,
                stream_generation,
                state,
                mode_timestamp_ms,
                start_timestamp_ms,
                format,
            ) = match current {
                PlaybackState::AwaitingMode {
                    connection_generation,
                    ..
                } => (
                    connection_generation,
                    None,
                    HelperPlaybackStateKind::AwaitingMode,
                    None,
                    None,
                    None,
                ),
                PlaybackState::Stopped {
                    connection_generation,
                    stream_generation,
                    mode_timestamp_ms,
                    ..
                } => (
                    connection_generation,
                    Some(stream_generation),
                    HelperPlaybackStateKind::Stopped,
                    Some(mode_timestamp_ms),
                    None,
                    None,
                ),
                PlaybackState::Started {
                    connection_generation,
                    stream_generation,
                    mode_timestamp_ms,
                    start_timestamp_ms,
                    format,
                    ..
                } => (
                    connection_generation,
                    Some(stream_generation),
                    HelperPlaybackStateKind::Started,
                    Some(mode_timestamp_ms),
                    Some(start_timestamp_ms),
                    Some(format),
                ),
                PlaybackState::Closed {
                    connection_generation,
                    stream_generation,
                    ..
                } => (
                    connection_generation,
                    Some(stream_generation),
                    HelperPlaybackStateKind::Closed,
                    None,
                    None,
                    None,
                ),
            };
            events.send_control(HelperEvent::PlaybackState {
                connection_generation,
                channel_id,
                stream_generation,
                state,
                mode_timestamp_ms,
                start_timestamp_ms,
                channels: format.map(|format| format.channels),
                sample_rate_hz: format.map(|format| format.sample_rate_hz),
            })?;
        }
        if resources.playback_settings.get(&channel_id) != Some(&settings) {
            resources
                .playback_settings
                .insert(channel_id, settings.clone());
            events.send_control(HelperEvent::PlaybackSettings {
                channel_id,
                volumes: settings.volume.to_vec(),
                muted: settings.muted,
                latency_ms: settings.latency_ms,
            })?;
        }
    }
    Ok(())
}

fn publish_record_state_changes(
    resources: &mut SessionResources,
    events: &EventSender,
) -> Result<(), HelperIpcError> {
    for (channel_id, channel) in &resources.record_channels {
        let current = channel.state();
        if resources.record_states.get(channel_id) != Some(&current) {
            resources.record_states.insert(*channel_id, current);
            let (connection_generation, stream_generation, state, start_timestamp_ms, mode, format) =
                match current {
                    RecordState::Stopped {
                        connection_generation,
                        stream_generation,
                        ..
                    } => (
                        connection_generation,
                        stream_generation,
                        HelperRecordStateKind::Stopped,
                        None,
                        None,
                        None,
                    ),
                    RecordState::StartRequested {
                        connection_generation,
                        stream_generation,
                        mode,
                        format,
                        ..
                    } => (
                        connection_generation,
                        stream_generation,
                        HelperRecordStateKind::StartRequested,
                        None,
                        Some(helper_audio_data_mode(mode)),
                        Some(format),
                    ),
                    RecordState::Recording {
                        connection_generation,
                        stream_generation,
                        start_timestamp_ms,
                        mode,
                        format,
                        ..
                    } => (
                        connection_generation,
                        stream_generation,
                        HelperRecordStateKind::Recording,
                        Some(start_timestamp_ms),
                        Some(helper_audio_data_mode(mode)),
                        Some(format),
                    ),
                    RecordState::Closed {
                        connection_generation,
                        stream_generation,
                        ..
                    } => (
                        connection_generation,
                        stream_generation,
                        HelperRecordStateKind::Closed,
                        None,
                        None,
                        None,
                    ),
                };
            events.send_control(HelperEvent::RecordState {
                connection_generation,
                channel_id: *channel_id,
                stream_generation,
                state,
                start_timestamp_ms,
                mode,
                channels: format.map(|format| format.channels),
                sample_rate_hz: format.map(|format| format.sample_rate_hz),
            })?;
        }
        let settings = channel.audio_settings();
        if resources.record_settings.get(channel_id) != Some(&settings) {
            resources
                .record_settings
                .insert(*channel_id, settings.clone());
            events.send_control(HelperEvent::RecordSettings {
                channel_id: *channel_id,
                volumes: settings.volume.to_vec(),
                muted: settings.muted,
            })?;
        }
    }
    Ok(())
}

fn helper_audio_data_mode(mode: oxide_spice_protocol::AudioDataMode) -> HelperAudioDataMode {
    match mode {
        oxide_spice_protocol::AudioDataMode::Raw => HelperAudioDataMode::Raw,
        oxide_spice_protocol::AudioDataMode::Celt051 => HelperAudioDataMode::Celt051,
        oxide_spice_protocol::AudioDataMode::Opus => HelperAudioDataMode::Opus,
    }
}

async fn next_cursor(
    cursor: &mut Option<CursorEvents>,
) -> Option<Result<oxide_spice_client::CursorState, ClientError>> {
    match cursor {
        Some(cursor) => Some(cursor.next().await),
        None => std::future::pending().await,
    }
}

async fn next_mouse_mode(
    inputs: &mut Option<InputsHandle>,
) -> Option<Result<oxide_spice_client::MouseMode, InputSendError>> {
    match inputs {
        Some(inputs) => Some(inputs.mouse_mode_changed().await),
        None => std::future::pending().await,
    }
}

async fn next_keyboard_modifiers(
    inputs: &mut Option<InputsHandle>,
) -> Option<Result<KeyboardModifiers, InputSendError>> {
    match inputs {
        Some(inputs) => Some(inputs.modifiers_state_changed().await),
        None => std::future::pending().await,
    }
}

fn publish_mouse_mode(
    mode: oxide_spice_client::MouseMode,
    events: &EventSender,
) -> Result<(), HelperIpcError> {
    let mode = match mode {
        oxide_spice_client::MouseMode::Server => HelperMouseMode::Server,
        oxide_spice_client::MouseMode::Client => HelperMouseMode::Client,
    };
    events.send_control(HelperEvent::MouseMode { mode })
}

fn publish_keyboard_modifiers(
    modifiers: KeyboardModifiers,
    events: &EventSender,
) -> Result<(), HelperIpcError> {
    events.send_control(HelperEvent::KeyboardModifiers {
        bits: modifiers.bits(),
    })
}

async fn next_agent_event(
    events: &mut Option<AgentEvents>,
) -> Option<Result<AgentEvent, AgentSendError>> {
    match events {
        Some(events) => Some(events.next().await),
        None => std::future::pending().await,
    }
}

async fn next_playback(
    playback: &mut Option<PlaybackPackets>,
) -> Option<Result<oxide_spice_client::PlaybackPcmPacket, ClientError>> {
    match playback {
        Some(playback) => Some(playback.next().await),
        None => std::future::pending().await,
    }
}

fn helper_rect(rect: Rect) -> Result<HelperRect, HelperRuntimeError> {
    let x = u32::try_from(rect.left)
        .map_err(|_| HelperRuntimeError::Configuration("negative frame rectangle".to_owned()))?;
    let y = u32::try_from(rect.top)
        .map_err(|_| HelperRuntimeError::Configuration("negative frame rectangle".to_owned()))?;
    let width = u32::try_from(rect.right - rect.left)
        .map_err(|_| HelperRuntimeError::Configuration("invalid frame rectangle".to_owned()))?;
    let height = u32::try_from(rect.bottom - rect.top)
        .map_err(|_| HelperRuntimeError::Configuration("invalid frame rectangle".to_owned()))?;
    Ok(HelperRect {
        x,
        y,
        width,
        height,
    })
}

fn mouse_buttons(bits: u16) -> Result<MouseButtons, &'static str> {
    MouseButtons::from_bits(bits).ok_or("invalid mouse button bits")
}

fn mouse_button(button: HelperMouseButton) -> MouseButton {
    match button {
        HelperMouseButton::Left => MouseButton::Left,
        HelperMouseButton::Middle => MouseButton::Middle,
        HelperMouseButton::Right => MouseButton::Right,
        HelperMouseButton::WheelUp => MouseButton::WheelUp,
        HelperMouseButton::WheelDown => MouseButton::WheelDown,
        HelperMouseButton::Side => MouseButton::Side,
        HelperMouseButton::Extra => MouseButton::Extra,
    }
}

fn clipboard_selection(selection: HelperClipboardSelection) -> AgentClipboardSelection {
    match selection {
        HelperClipboardSelection::Clipboard => AgentClipboardSelection::Clipboard,
        HelperClipboardSelection::Primary => AgentClipboardSelection::Primary,
        HelperClipboardSelection::Secondary => AgentClipboardSelection::Secondary,
    }
}

fn helper_clipboard_selection(selection: AgentClipboardSelection) -> HelperClipboardSelection {
    match selection {
        AgentClipboardSelection::Clipboard => HelperClipboardSelection::Clipboard,
        AgentClipboardSelection::Primary => HelperClipboardSelection::Primary,
        AgentClipboardSelection::Secondary => HelperClipboardSelection::Secondary,
    }
}

fn clipboard_type(format: HelperClipboardFormat) -> AgentClipboardType {
    match format {
        HelperClipboardFormat::Utf8Text => AgentClipboardType::Utf8Text,
        HelperClipboardFormat::ImagePng => AgentClipboardType::ImagePng,
        HelperClipboardFormat::ImageBmp => AgentClipboardType::ImageBmp,
        HelperClipboardFormat::ImageTiff => AgentClipboardType::ImageTiff,
        HelperClipboardFormat::ImageJpeg => AgentClipboardType::ImageJpeg,
        HelperClipboardFormat::FileList => AgentClipboardType::FileList,
    }
}

fn helper_clipboard_format(format: AgentClipboardType) -> HelperClipboardFormat {
    match format {
        AgentClipboardType::Utf8Text => HelperClipboardFormat::Utf8Text,
        AgentClipboardType::ImagePng => HelperClipboardFormat::ImagePng,
        AgentClipboardType::ImageBmp => HelperClipboardFormat::ImageBmp,
        AgentClipboardType::ImageTiff => HelperClipboardFormat::ImageTiff,
        AgentClipboardType::ImageJpeg => HelperClipboardFormat::ImageJpeg,
        AgentClipboardType::FileList => HelperClipboardFormat::FileList,
        AgentClipboardType::None => unreachable!("None is not a transferable clipboard format"),
    }
}

fn guest_monitor(monitor: HelperMonitor) -> GuestMonitor {
    GuestMonitor {
        width: monitor.width,
        height: monitor.height,
        depth: monitor.depth,
        x: monitor.x,
        y: monitor.y,
        width_mm: monitor.width_mm,
        height_mm: monitor.height_mm,
    }
}

fn send_client_error(events: &EventSender, error: &ClientError) -> Result<(), HelperIpcError> {
    events.send_control(HelperEvent::Error {
        category: helper_error_category(error.category()),
        message: error.to_string(),
    })
}

fn send_runtime_error(
    events: &EventSender,
    error: &HelperRuntimeError,
) -> Result<(), HelperIpcError> {
    let category = match error {
        HelperRuntimeError::Client(error) => helper_error_category(error.category()),
        HelperRuntimeError::Configuration(_) => HelperErrorCategory::Configuration,
        HelperRuntimeError::Ipc(_) | HelperRuntimeError::HostApi(_) => {
            HelperErrorCategory::Internal
        }
    };
    events.send_control(HelperEvent::Error {
        category,
        message: error.to_string(),
    })
}

fn send_action_error(events: &EventSender, message: &str) -> Result<bool, HelperRuntimeError> {
    events.send_control(HelperEvent::Error {
        category: HelperErrorCategory::Protocol,
        message: message.to_owned(),
    })?;
    Ok(false)
}

fn helper_error_category(category: ErrorCategory) -> HelperErrorCategory {
    match category {
        ErrorCategory::Configuration => HelperErrorCategory::Configuration,
        ErrorCategory::Network => HelperErrorCategory::Network,
        ErrorCategory::Tls => HelperErrorCategory::Tls,
        ErrorCategory::Authentication => HelperErrorCategory::Authentication,
        ErrorCategory::Negotiation => HelperErrorCategory::Negotiation,
        ErrorCategory::Protocol => HelperErrorCategory::Protocol,
        ErrorCategory::Unsupported => HelperErrorCategory::Unsupported,
        ErrorCategory::ResourceLimit => HelperErrorCategory::ResourceLimit,
        ErrorCategory::RemoteDisconnect => HelperErrorCategory::RemoteDisconnect,
        ErrorCategory::Cancelled => HelperErrorCategory::Cancelled,
        ErrorCategory::Internal => HelperErrorCategory::Internal,
    }
}

fn input_error(error: InputSendError) -> String {
    error.to_string()
}

fn agent_error(error: AgentSendError) -> String {
    error.to_string()
}
