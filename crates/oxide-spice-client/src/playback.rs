//! Nonblocking delivery for raw SPICE Playback streams.

use std::sync::Arc;

use oxide_spice_codecs::SpiceOpusDecoder;
use oxide_spice_protocol::{
    AudioDataMode, AudioSampleFormat, PlaybackMode, PlaybackPacket as WirePlaybackPacket,
    PlaybackStart, decode_audio_mute, decode_audio_volume, decode_playback_latency,
    playback_server,
};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::{mpsc, watch};

use crate::ClientError;
use crate::channel::{
    Channel, ChannelIdentity, ControlDisposition, ControlState, IncomingEnvelope, ProgressRegistry,
    handle_channel_wait,
};

/// Bounded packet count; the protocol parser independently limits bytes per packet.
const PLAYBACK_PACKET_QUEUE_CAPACITY: usize = 16;

/// Negotiated raw PCM representation for one Playback stream generation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlaybackFormat {
    pub channels: u32,
    pub sample_rate_hz: u32,
    pub sample_format: AudioSampleFormat,
}

impl From<PlaybackStart> for PlaybackFormat {
    fn from(start: PlaybackStart) -> Self {
        Self {
            channels: start.channels,
            sample_rate_hz: start.sample_rate_hz,
            sample_format: start.format,
        }
    }
}

impl PlaybackFormat {
    /// Returns the byte width of one interleaved sample frame.
    pub fn frame_bytes(self) -> Result<usize, ClientError> {
        let bytes_per_sample = match self.sample_format {
            AudioSampleFormat::Signed16LittleEndian => 2,
        };
        usize::try_from(self.channels)
            .ok()
            .and_then(|channels| channels.checked_mul(bytes_per_sample))
            .ok_or_else(|| resource_limit_error("Playback frame bytes"))
    }
}

/// Latest state for one independently linked Playback channel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlaybackState {
    AwaitingMode {
        connection_generation: u64,
        channel_id: u8,
    },
    Stopped {
        connection_generation: u64,
        channel_id: u8,
        stream_generation: u64,
        mode_timestamp_ms: u32,
    },
    Started {
        connection_generation: u64,
        channel_id: u8,
        stream_generation: u64,
        mode_timestamp_ms: u32,
        start_timestamp_ms: u32,
        format: PlaybackFormat,
    },
    Closed {
        connection_generation: u64,
        channel_id: u8,
        stream_generation: u64,
    },
}

/// Latest host-facing Playback gain, mute, and latency controls.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlaybackAudioSettings {
    pub volume: Arc<[u16]>,
    pub muted: bool,
    pub latency_ms: Option<u32>,
}

impl Default for PlaybackAudioSettings {
    fn default() -> Self {
        Self {
            volume: Arc::from([]),
            muted: false,
            latency_ms: None,
        }
    }
}

/// Cloneable latest-state handle for one Playback channel id.
#[derive(Clone)]
pub struct PlaybackChannel {
    channel_id: u8,
    state: watch::Receiver<PlaybackState>,
    audio_settings: watch::Receiver<PlaybackAudioSettings>,
}

impl std::fmt::Debug for PlaybackChannel {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PlaybackChannel")
            .field("channel_id", &self.channel_id)
            .field("state", &*self.state.borrow())
            .finish_non_exhaustive()
    }
}

impl PlaybackChannel {
    pub const fn channel_id(&self) -> u8 {
        self.channel_id
    }

    pub fn state(&self) -> PlaybackState {
        *self.state.borrow()
    }

    pub fn audio_settings(&self) -> PlaybackAudioSettings {
        self.audio_settings.borrow().clone()
    }

    /// Waits until the channel publishes a state different from the current value.
    pub async fn changed(&mut self) -> Result<PlaybackState, ClientError> {
        self.state
            .changed()
            .await
            .map_err(|_| ClientError::TaskTerminated)?;
        Ok(*self.state.borrow_and_update())
    }
}

/// One owned raw PCM packet ready for a host audio adapter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlaybackPcmPacket {
    pub connection_generation: u64,
    pub channel_id: u8,
    pub stream_generation: u64,
    pub sequence: u64,
    pub timestamp_ms: u32,
    pub format: PlaybackFormat,
    pub interleaved_s16le: Arc<[u8]>,
    pub discontinuity: bool,
}

/// Single-consumer bounded stream shared by every Playback channel in a session.
pub struct PlaybackPackets {
    receiver: mpsc::Receiver<PlaybackPcmPacket>,
}

impl std::fmt::Debug for PlaybackPackets {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PlaybackPackets")
            .finish_non_exhaustive()
    }
}

impl PlaybackPackets {
    pub async fn next(&mut self) -> Result<PlaybackPcmPacket, ClientError> {
        self.receiver
            .recv()
            .await
            .ok_or(ClientError::TaskTerminated)
    }
}

/// Creates the shared bounded packet queue for linked Playback owners.
pub(crate) fn playback_packets() -> (mpsc::Sender<PlaybackPcmPacket>, PlaybackPackets) {
    let (sender, receiver) = mpsc::channel(PLAYBACK_PACKET_QUEUE_CAPACITY);
    (sender, PlaybackPackets { receiver })
}

/// Creates one channel-id-specific latest-state path.
pub(crate) fn playback_channel(
    connection_generation: u64,
    channel_id: u8,
) -> (
    PlaybackChannel,
    watch::Sender<PlaybackState>,
    watch::Sender<PlaybackAudioSettings>,
) {
    let initial = PlaybackState::AwaitingMode {
        connection_generation,
        channel_id,
    };
    let (state_sender, state) = watch::channel(initial);
    let (audio_sender, audio_settings) = watch::channel(PlaybackAudioSettings::default());
    (
        PlaybackChannel {
            channel_id,
            state,
            audio_settings,
        },
        state_sender,
        audio_sender,
    )
}

/// Owns one Playback transport and drops packets instead of blocking network progress.
pub(crate) async fn run_playback<S>(
    mut channel: Channel<S>,
    mut cancel: watch::Receiver<bool>,
    state_sender: watch::Sender<PlaybackState>,
    audio_sender: watch::Sender<PlaybackAudioSettings>,
    packet_sender: mpsc::Sender<PlaybackPcmPacket>,
    connection_generation: u64,
    channel_id: u8,
    progress: ProgressRegistry,
) -> Result<(), ClientError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let identity = ChannelIdentity {
        channel_type: oxide_spice_protocol::ChannelType::Playback,
        channel_id,
    };
    let mut control = ControlState::new();
    let mut message_body = Vec::new();
    let mut mode_timestamp_ms = None;
    let mut data_mode = None;
    let mut active_format: Option<PlaybackFormat> = None;
    let mut active_start_timestamp_ms = None;
    let mut opus_decoder = None;
    let mut decoded_pcm = Vec::new();
    let mut stream_generation = 0_u64;
    let mut sequence = 0_u64;
    let mut discontinuity = false;
    let mut observed_migration_activation = channel.migration_activation_count();

    loop {
        let header = tokio::select! {
            changed = cancel.changed() => {
                if changed.is_err() || *cancel.borrow() {
                    state_sender.send_replace(PlaybackState::Closed {
                        connection_generation,
                        channel_id,
                        stream_generation,
                    });
                    let _ = channel.shutdown().await;
                    return Ok(());
                }
                continue;
            }
            incoming = channel.read_message(&mut message_body) => incoming?,
        };
        let envelope = IncomingEnvelope::decode(header, &message_body)?;
        let counts_for_ack = envelope.counts_for_ack();
        let serial = channel.received_serial();
        if let Some(seamless) =
            channel.observe_migration_activation(&mut observed_migration_activation)
            && !seamless
        {
            mode_timestamp_ms = None;
            data_mode = None;
            active_format = None;
            active_start_timestamp_ms = None;
            opus_decoder = None;
            decoded_pcm.clear();
            sequence = 0;
            discontinuity = true;
            state_sender.send_replace(PlaybackState::AwaitingMode {
                connection_generation,
                channel_id,
            });
            audio_sender.send_replace(PlaybackAudioSettings::default());
        }
        for message in envelope.messages() {
            if control.handle_without_ack(&mut channel, &message).await?
                == ControlDisposition::Consumed
            {
                continue;
            }
            if handle_channel_wait(&progress, identity, serial, &mut cancel, &message).await? {
                continue;
            }

            match message.header.message_type {
                playback_server::MODE => {
                    let mode = PlaybackMode::decode(message.body)?;
                    if mode.mode != AudioDataMode::Raw {
                        if mode.mode != AudioDataMode::Opus {
                            return Err(unsupported_playback_mode());
                        }
                    }
                    mode_timestamp_ms = Some(mode.timestamp_ms);
                    data_mode = Some(mode.mode);
                    opus_decoder = match (mode.mode, active_format) {
                        (AudioDataMode::Opus, Some(format)) => Some(SpiceOpusDecoder::new(
                            format.channels,
                            format.sample_rate_hz,
                        )?),
                        _ => None,
                    };
                    let state = match active_format {
                        Some(format) => PlaybackState::Started {
                            connection_generation,
                            channel_id,
                            stream_generation,
                            mode_timestamp_ms: mode.timestamp_ms,
                            start_timestamp_ms: active_start_timestamp_ms
                                .expect("start timestamp exists with active format"),
                            format,
                        },
                        None => PlaybackState::Stopped {
                            connection_generation,
                            channel_id,
                            stream_generation,
                            mode_timestamp_ms: mode.timestamp_ms,
                        },
                    };
                    state_sender.send_replace(state);
                }
                playback_server::START => {
                    if active_format.is_some() || mode_timestamp_ms.is_none() {
                        return Err(protocol_value_error("Playback Start state"));
                    }
                    let start = PlaybackStart::decode(message.body)?;
                    stream_generation = stream_generation
                        .checked_add(1)
                        .ok_or_else(|| resource_limit_error("Playback stream generation"))?;
                    sequence = 0;
                    discontinuity = false;
                    let format = PlaybackFormat::from(start);
                    opus_decoder = match data_mode.expect("mode checked before Start") {
                        AudioDataMode::Raw => None,
                        AudioDataMode::Opus => Some(SpiceOpusDecoder::new(
                            format.channels,
                            format.sample_rate_hz,
                        )?),
                        AudioDataMode::Celt051 => return Err(unsupported_playback_mode()),
                    };
                    active_format = Some(format);
                    active_start_timestamp_ms = Some(start.timestamp_ms);
                    state_sender.send_replace(PlaybackState::Started {
                        connection_generation,
                        channel_id,
                        stream_generation,
                        mode_timestamp_ms: mode_timestamp_ms.expect("mode checked before Start"),
                        start_timestamp_ms: start.timestamp_ms,
                        format,
                    });
                }
                playback_server::DATA => {
                    let format = active_format
                        .ok_or_else(|| protocol_value_error("Playback Data before Start"))?;
                    let packet = WirePlaybackPacket::decode(message.body)?;
                    let frame_bytes = format.frame_bytes()?;
                    let pcm: Arc<[u8]> = match data_mode
                        .ok_or_else(|| protocol_value_error("Playback Data before Mode"))?
                    {
                        AudioDataMode::Raw => Arc::from(packet.data),
                        AudioDataMode::Opus => {
                            opus_decoder
                                .as_mut()
                                .ok_or_else(|| protocol_value_error("Playback Opus decoder state"))?
                                .decode_packet(packet.data, &mut decoded_pcm)?;
                            Arc::from(decoded_pcm.as_slice())
                        }
                        AudioDataMode::Celt051 => return Err(unsupported_playback_mode()),
                    };
                    if !pcm.len().is_multiple_of(frame_bytes) {
                        return Err(protocol_value_error("Playback packet frame alignment"));
                    }
                    let packet_sequence = sequence;
                    sequence = sequence
                        .checked_add(1)
                        .ok_or_else(|| resource_limit_error("Playback packet sequence"))?;
                    if !packet_sender.is_closed() {
                        let owned = PlaybackPcmPacket {
                            connection_generation,
                            channel_id,
                            stream_generation,
                            sequence: packet_sequence,
                            timestamp_ms: packet.timestamp_ms,
                            format,
                            interleaved_s16le: pcm,
                            discontinuity,
                        };
                        match packet_sender.try_send(owned) {
                            Ok(()) => discontinuity = false,
                            Err(mpsc::error::TrySendError::Full(_)) => discontinuity = true,
                            Err(mpsc::error::TrySendError::Closed(_)) => discontinuity = true,
                        }
                    }
                }
                playback_server::STOP => {
                    if !message.body.is_empty() || active_format.take().is_none() {
                        return Err(protocol_value_error("Playback Stop state"));
                    }
                    active_start_timestamp_ms = None;
                    opus_decoder = None;
                    state_sender.send_replace(PlaybackState::Stopped {
                        connection_generation,
                        channel_id,
                        stream_generation,
                        mode_timestamp_ms: mode_timestamp_ms.expect("mode exists while started"),
                    });
                }
                playback_server::VOLUME => {
                    let volume: Arc<[u16]> = decode_audio_volume(message.body)?.into();
                    audio_sender.send_modify(|settings| settings.volume = volume);
                }
                playback_server::MUTE => {
                    let muted = decode_audio_mute(message.body)?;
                    audio_sender.send_modify(|settings| settings.muted = muted);
                }
                playback_server::LATENCY => {
                    let latency_ms = decode_playback_latency(message.body)?;
                    audio_sender.send_modify(|settings| settings.latency_ms = Some(latency_ms));
                }
                message_type => {
                    return Err(ClientError::UnsupportedMessage {
                        channel: "playback",
                        message_type,
                    });
                }
            }
        }
        if counts_for_ack {
            control.acknowledge_envelope(&mut channel).await?;
        }
        progress.complete(identity, serial)?;
    }
}

fn unsupported_playback_mode() -> ClientError {
    oxide_spice_protocol::DecodeError::new(
        oxide_spice_protocol::DecodeErrorKind::Unsupported,
        4,
        "playback data mode",
    )
    .into()
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
