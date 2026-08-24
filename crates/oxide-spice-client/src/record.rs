//! Bounded host capture API for raw SPICE Record streams.

use std::sync::Arc;
use std::time::Instant;

use oxide_spice_codecs::{SpiceOpusEncoder, supports_spice_opus_format};
use oxide_spice_protocol::{
    AudioDataMode, MAX_RECORD_PACKET_BYTES, RecordStart, decode_audio_mute, decode_audio_volume,
    encode_record_mode, encode_record_packet, encode_record_start_mark, record_capability,
    record_client, record_server,
};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::{mpsc, oneshot, watch};

use crate::ClientError;
use crate::channel::{
    Channel, ChannelIdentity, ControlDisposition, ControlState, IncomingMessage, ProgressRegistry,
    handle_channel_wait,
};
use crate::playback::PlaybackFormat;

/// Bounded number of capture submissions waiting for one Record transport.
const RECORD_COMMAND_QUEUE_CAPACITY: usize = 16;

/// Session-relative monotonic multimedia clock shared with capture adapters.
#[derive(Debug)]
pub(crate) struct RecordClock {
    origin: Instant,
}

impl RecordClock {
    pub fn new() -> Self {
        Self {
            origin: Instant::now(),
        }
    }

    /// Returns elapsed milliseconds modulo the protocol's 32-bit timestamp space.
    pub fn now_ms(&self) -> u32 {
        let modulus = u128::from(u32::MAX) + 1;
        (self.origin.elapsed().as_millis() % modulus) as u32
    }
}

/// Latest server-requested state for one Record channel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecordState {
    Stopped {
        connection_generation: u64,
        channel_id: u8,
        stream_generation: u64,
    },
    StartRequested {
        connection_generation: u64,
        channel_id: u8,
        stream_generation: u64,
        mode: AudioDataMode,
        format: PlaybackFormat,
    },
    Recording {
        connection_generation: u64,
        channel_id: u8,
        stream_generation: u64,
        start_timestamp_ms: u32,
        mode: AudioDataMode,
        format: PlaybackFormat,
    },
    Closed {
        connection_generation: u64,
        channel_id: u8,
        stream_generation: u64,
    },
}

/// Latest host-facing Record gain and mute controls.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordAudioSettings {
    pub volume: Arc<[u16]>,
    pub muted: bool,
}

impl Default for RecordAudioSettings {
    fn default() -> Self {
        Self {
            volume: Arc::from([]),
            muted: false,
        }
    }
}

/// Host-facing failure for raw capture submissions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum RecordSendError {
    #[error("Record stream is not in the required state")]
    Unavailable,
    #[error("Record payload exceeds the configured bound")]
    ResourceLimit,
    #[error("Record PCM data is not aligned to a complete sample frame")]
    InvalidData,
    #[error("Record channel is closed")]
    Closed,
}

enum RecordCommand {
    Begin {
        stream_generation: u64,
        timestamp_ms: u32,
        completion: oneshot::Sender<Result<(), RecordSendError>>,
    },
    Data {
        stream_generation: u64,
        timestamp_ms: u32,
        pcm: Box<[u8]>,
        completion: oneshot::Sender<Result<(), RecordSendError>>,
    },
}

/// Unique host owner for one raw Record channel.
pub struct RecordChannel {
    channel_id: u8,
    state: watch::Receiver<RecordState>,
    audio_settings: watch::Receiver<RecordAudioSettings>,
    commands: mpsc::Sender<RecordCommand>,
    clock: Arc<RecordClock>,
}

impl std::fmt::Debug for RecordChannel {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RecordChannel")
            .field("channel_id", &self.channel_id)
            .field("state", &*self.state.borrow())
            .finish_non_exhaustive()
    }
}

impl RecordChannel {
    pub const fn channel_id(&self) -> u8 {
        self.channel_id
    }

    pub fn state(&self) -> RecordState {
        *self.state.borrow()
    }

    pub fn audio_settings(&self) -> RecordAudioSettings {
        self.audio_settings.borrow().clone()
    }

    pub fn timestamp_now_ms(&self) -> u32 {
        self.clock.now_ms()
    }

    pub async fn changed(&mut self) -> Result<RecordState, RecordSendError> {
        self.state
            .changed()
            .await
            .map_err(|_| RecordSendError::Closed)?;
        Ok(*self.state.borrow_and_update())
    }

    /// Sends Start Mark using the shared monotonic multimedia clock.
    pub async fn begin(&self) -> Result<u32, RecordSendError> {
        let timestamp_ms = self.timestamp_now_ms();
        self.begin_at(timestamp_ms).await?;
        Ok(timestamp_ms)
    }

    /// Sends Start Mark using a capture backend's explicit timestamp.
    pub async fn begin_at(&self, timestamp_ms: u32) -> Result<(), RecordSendError> {
        let RecordState::StartRequested {
            stream_generation, ..
        } = self.state()
        else {
            return Err(RecordSendError::Unavailable);
        };
        let (completion, completed) = oneshot::channel();
        self.commands
            .send(RecordCommand::Begin {
                stream_generation,
                timestamp_ms,
                completion,
            })
            .await
            .map_err(|_| RecordSendError::Closed)?;
        completed.await.map_err(|_| RecordSendError::Closed)?
    }

    /// Sends one raw PCM packet timestamped by the shared monotonic clock.
    pub async fn send_pcm(&self, pcm: &[u8]) -> Result<u32, RecordSendError> {
        let timestamp_ms = self.timestamp_now_ms();
        self.send_pcm_at(timestamp_ms, pcm).await?;
        Ok(timestamp_ms)
    }

    /// Sends one raw PCM packet using a capture backend's explicit timestamp.
    pub async fn send_pcm_at(&self, timestamp_ms: u32, pcm: &[u8]) -> Result<(), RecordSendError> {
        let RecordState::Recording {
            stream_generation,
            format,
            ..
        } = self.state()
        else {
            return Err(RecordSendError::Unavailable);
        };
        if pcm.len() > MAX_RECORD_PACKET_BYTES {
            return Err(RecordSendError::ResourceLimit);
        }
        let frame_bytes = format
            .frame_bytes()
            .map_err(|_| RecordSendError::InvalidData)?;
        if pcm.is_empty() || !pcm.len().is_multiple_of(frame_bytes) {
            return Err(RecordSendError::InvalidData);
        }
        let (completion, completed) = oneshot::channel();
        self.commands
            .send(RecordCommand::Data {
                stream_generation,
                timestamp_ms,
                pcm: Box::from(pcm),
                completion,
            })
            .await
            .map_err(|_| RecordSendError::Closed)?;
        completed.await.map_err(|_| RecordSendError::Closed)?
    }
}

/// Task-owned bounded paths for one linked Record transport.
pub(crate) struct RecordTaskPaths {
    commands: mpsc::Receiver<RecordCommand>,
    state: watch::Sender<RecordState>,
    audio_settings: watch::Sender<RecordAudioSettings>,
}

/// Creates one unique Record API and its task-private paths.
pub(crate) fn record_channel(
    connection_generation: u64,
    channel_id: u8,
    clock: Arc<RecordClock>,
) -> (RecordChannel, RecordTaskPaths) {
    let initial = RecordState::Stopped {
        connection_generation,
        channel_id,
        stream_generation: 0,
    };
    let (state_sender, state) = watch::channel(initial);
    let (audio_sender, audio_settings) = watch::channel(RecordAudioSettings::default());
    let (command_sender, commands) = mpsc::channel(RECORD_COMMAND_QUEUE_CAPACITY);
    (
        RecordChannel {
            channel_id,
            state,
            audio_settings,
            commands: command_sender,
            clock,
        },
        RecordTaskPaths {
            commands,
            state: state_sender,
            audio_settings: audio_sender,
        },
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RecordPhase {
    Stopped,
    StartRequested,
    Recording,
}

/// Announces the initial raw mode required by every newly linked Record transport.
pub(crate) async fn initialize_record_channel<S>(
    channel: &mut Channel<S>,
    clock: &RecordClock,
) -> Result<(), ClientError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    channel
        .write_message(
            record_client::MODE,
            &encode_record_mode(clock.now_ms(), AudioDataMode::Raw),
        )
        .await
}

/// Owns Record ordering and emits raw PCM only after Start Mark completion.
pub(crate) async fn run_record<S>(
    mut channel: Channel<S>,
    mut cancel: watch::Receiver<bool>,
    mut paths: RecordTaskPaths,
    clock: Arc<RecordClock>,
    connection_generation: u64,
    channel_id: u8,
    progress: ProgressRegistry,
) -> Result<(), ClientError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let mut peer_supports_opus = channel.peer_supports(record_capability::OPUS);
    initialize_record_channel(&mut channel, &clock).await?;
    let identity = ChannelIdentity {
        channel_type: oxide_spice_protocol::ChannelType::Record,
        channel_id,
    };
    let mut control = ControlState::new();
    let mut message_body = Vec::new();
    let mut stream_generation = 0_u64;
    let mut phase = RecordPhase::Stopped;
    let mut format = None;
    let mut data_mode = AudioDataMode::Raw;
    let mut opus_encoder = None;
    let mut pending_pcm = Vec::new();
    let mut encoded_opus = Vec::new();
    let mut commands_open = true;
    let mut observed_migration_activation = channel.migration_activation_count();

    loop {
        tokio::select! {
            changed = cancel.changed() => {
                if changed.is_err() || *cancel.borrow() {
                    paths.state.send_replace(RecordState::Closed {
                        connection_generation,
                        channel_id,
                        stream_generation,
                    });
                    let _ = channel.shutdown().await;
                    return Ok(());
                }
            }
            incoming = channel.read_message(&mut message_body) => {
                let header = incoming?;
                let message = IncomingMessage { header, body: &message_body };
                let serial = channel.received_serial();
                if let Some(seamless) =
                    channel.observe_migration_activation(&mut observed_migration_activation)
                    && !seamless
                {
                    peer_supports_opus = channel.peer_supports(record_capability::OPUS);
                    phase = RecordPhase::Stopped;
                    format = None;
                    data_mode = AudioDataMode::Raw;
                    opus_encoder = None;
                    pending_pcm.clear();
                    encoded_opus.clear();
                    paths.state.send_replace(RecordState::Stopped {
                        connection_generation,
                        channel_id,
                        stream_generation,
                    });
                    paths
                        .audio_settings
                        .send_replace(RecordAudioSettings::default());
                }
                if control.handle(&mut channel, &message).await? == ControlDisposition::Consumed {
                    progress.complete(identity, serial)?;
                    continue;
                }
                if handle_channel_wait(&progress, identity, serial, &mut cancel, &message).await? {
                    progress.complete(identity, serial)?;
                    continue;
                }
                match message.header.message_type {
                    record_server::START => {
                        if phase != RecordPhase::Stopped {
                            return Err(protocol_value_error("repeated Record Start"));
                        }
                        let requested = RecordStart::decode(message.body)?;
                        stream_generation = stream_generation
                            .checked_add(1)
                            .ok_or_else(|| resource_limit_error("Record stream generation"))?;
                        let requested_format = PlaybackFormat {
                            channels: requested.channels,
                            sample_rate_hz: requested.sample_rate_hz,
                            sample_format: requested.format,
                        };
                        let requested_mode = if peer_supports_opus
                            && supports_spice_opus_format(
                                requested.channels,
                                requested.sample_rate_hz,
                            )
                        {
                            AudioDataMode::Opus
                        } else {
                            AudioDataMode::Raw
                        };
                        opus_encoder = match requested_mode {
                            AudioDataMode::Opus => Some(SpiceOpusEncoder::new(
                                requested.channels,
                                requested.sample_rate_hz,
                            )?),
                            AudioDataMode::Raw => None,
                            AudioDataMode::Celt051 => unreachable!("CELT is never selected"),
                        };
                        pending_pcm.clear();
                        if requested_mode != data_mode {
                            channel
                                .write_message(
                                    record_client::MODE,
                                    &encode_record_mode(clock.now_ms(), requested_mode),
                                )
                                .await?;
                            data_mode = requested_mode;
                        }
                        format = Some(requested_format);
                        phase = RecordPhase::StartRequested;
                        paths.state.send_replace(RecordState::StartRequested {
                            connection_generation,
                            channel_id,
                            stream_generation,
                            mode: data_mode,
                            format: requested_format,
                        });
                    }
                    record_server::STOP => {
                        if !message.body.is_empty() || phase == RecordPhase::Stopped {
                            return Err(protocol_value_error("Record Stop state"));
                        }
                        phase = RecordPhase::Stopped;
                        format = None;
                        opus_encoder = None;
                        pending_pcm.clear();
                        paths.state.send_replace(RecordState::Stopped {
                            connection_generation,
                            channel_id,
                            stream_generation,
                        });
                    }
                    record_server::VOLUME => {
                        let volume: Arc<[u16]> = decode_audio_volume(message.body)?.into();
                        paths.audio_settings.send_modify(|settings| settings.volume = volume);
                    }
                    record_server::MUTE => {
                        let muted = decode_audio_mute(message.body)?;
                        paths.audio_settings.send_modify(|settings| settings.muted = muted);
                    }
                    message_type => return Err(ClientError::UnsupportedMessage {
                        channel: "record",
                        message_type,
                    }),
                }
                progress.complete(identity, serial)?;
            }
            command = paths.commands.recv(), if commands_open => {
                let Some(command) = command else {
                    commands_open = false;
                    continue;
                };
                match command {
                    RecordCommand::Begin { stream_generation: command_generation, timestamp_ms, completion } => {
                        if phase != RecordPhase::StartRequested || command_generation != stream_generation {
                            let _ = completion.send(Err(RecordSendError::Unavailable));
                            continue;
                        }
                        if let Err(error) = channel
                            .write_message(record_client::START_MARK, &encode_record_start_mark(timestamp_ms))
                            .await
                        {
                            let _ = completion.send(Err(RecordSendError::Closed));
                            return Err(error);
                        }
                        phase = RecordPhase::Recording;
                        paths.state.send_replace(RecordState::Recording {
                            connection_generation,
                            channel_id,
                            stream_generation,
                            start_timestamp_ms: timestamp_ms,
                            mode: data_mode,
                            format: format.expect("Record format exists after Start"),
                        });
                        let _ = completion.send(Ok(()));
                    }
                    RecordCommand::Data {
                        stream_generation: command_generation,
                        timestamp_ms,
                        pcm,
                        completion,
                    } => {
                        if phase != RecordPhase::Recording || command_generation != stream_generation {
                            let _ = completion.send(Err(RecordSendError::Unavailable));
                            continue;
                        }
                        match data_mode {
                            AudioDataMode::Raw => {
                                let body = encode_record_packet(timestamp_ms, &pcm)?;
                                if let Err(error) = channel.write_message(record_client::DATA, &body).await {
                                    let _ = completion.send(Err(RecordSendError::Closed));
                                    return Err(error);
                                }
                            }
                            AudioDataMode::Opus => {
                                pending_pcm.extend_from_slice(&pcm);
                                let frame_bytes = SpiceOpusEncoder::frame_bytes();
                                let mut consumed_bytes = 0;
                                while pending_pcm.len() - consumed_bytes >= frame_bytes {
                                    let frame_end = consumed_bytes + frame_bytes;
                                    opus_encoder
                                        .as_mut()
                                        .ok_or_else(|| protocol_value_error("Record Opus encoder state"))?
                                        .encode_frame(
                                            &pending_pcm[consumed_bytes..frame_end],
                                            &mut encoded_opus,
                                        )?;
                                    let body = encode_record_packet(timestamp_ms, &encoded_opus)?;
                                    if let Err(error) = channel.write_message(record_client::DATA, &body).await {
                                        let _ = completion.send(Err(RecordSendError::Closed));
                                        return Err(error);
                                    }
                                    consumed_bytes = frame_end;
                                }
                                pending_pcm.drain(..consumed_bytes);
                            }
                            AudioDataMode::Celt051 => unreachable!("CELT is never selected"),
                        }
                        let _ = completion.send(Ok(()));
                    }
                }
            }
        }
    }
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
