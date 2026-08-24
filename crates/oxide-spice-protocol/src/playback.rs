//! Checked wire types for the server-to-client SPICE Playback channel.

use crate::wire::Reader;
use crate::{DecodeError, DecodeErrorKind};

/// Conservative channel-count limit for interleaved raw PCM.
pub const MAX_PLAYBACK_CHANNELS: u32 = 32;
/// Conservative sample-rate limit accepted from an untrusted peer.
pub const MAX_PLAYBACK_SAMPLE_RATE_HZ: u32 = 384_000;
/// Maximum raw PCM bytes retained from one Playback Data message.
pub const MAX_PLAYBACK_PACKET_BYTES: usize = 256 * 1024;
/// Maximum channel count accepted in one volume update.
pub const MAX_AUDIO_VOLUME_CHANNELS: usize = MAX_PLAYBACK_CHANNELS as usize;

/// Server-to-client Playback message identifiers.
pub mod playback_server {
    pub const DATA: u16 = 101;
    pub const MODE: u16 = 102;
    pub const START: u16 = 103;
    pub const STOP: u16 = 104;
    pub const VOLUME: u16 = 105;
    pub const MUTE: u16 = 106;
    pub const LATENCY: u16 = 107;
}

/// Decodes one exact per-channel unsigned volume array.
pub fn decode_audio_volume(body: &[u8]) -> Result<Vec<u16>, DecodeError> {
    let mut reader = Reader::new(body);
    let channel_count = usize::from(reader.u8("audio volume channel count")?);
    if channel_count == 0 || channel_count > MAX_AUDIO_VOLUME_CHANNELS {
        return Err(DecodeError::new(
            DecodeErrorKind::ResourceLimit,
            0,
            "audio volume channel count",
        ));
    }
    let expected_bytes = channel_count
        .checked_mul(2)
        .ok_or_else(|| DecodeError::new(DecodeErrorKind::Overflow, 1, "audio volume values"))?;
    if reader.remaining() != expected_bytes {
        return Err(DecodeError::new(
            if reader.remaining() < expected_bytes {
                DecodeErrorKind::Truncated
            } else {
                DecodeErrorKind::InvalidValue
            },
            1,
            "audio volume values",
        ));
    }
    let mut volumes = Vec::with_capacity(channel_count);
    for _ in 0..channel_count {
        volumes.push(reader.u16("audio volume value")?);
    }
    Ok(volumes)
}

/// Decodes one exact boolean audio mute state.
pub fn decode_audio_mute(body: &[u8]) -> Result<bool, DecodeError> {
    if body.len() != 1 {
        return Err(DecodeError::new(
            if body.is_empty() {
                DecodeErrorKind::Truncated
            } else {
                DecodeErrorKind::InvalidValue
            },
            body.len(),
            "audio mute",
        ));
    }
    match body[0] {
        0 => Ok(false),
        1 => Ok(true),
        _ => Err(DecodeError::new(
            DecodeErrorKind::InvalidValue,
            0,
            "audio mute",
        )),
    }
}

/// Decodes one exact playback latency in milliseconds.
pub fn decode_playback_latency(body: &[u8]) -> Result<u32, DecodeError> {
    if body.len() != size_of::<u32>() {
        return Err(DecodeError::new(
            if body.len() < size_of::<u32>() {
                DecodeErrorKind::Truncated
            } else {
                DecodeErrorKind::InvalidValue
            },
            body.len(),
            "playback latency",
        ));
    }
    Ok(u32::from_le_bytes(
        body.try_into().expect("four-byte playback latency"),
    ))
}

/// Playback data encodings selected by a Mode message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum AudioDataMode {
    Raw = 1,
    Celt051 = 2,
    Opus = 3,
}

impl TryFrom<u32> for AudioDataMode {
    type Error = DecodeError;

    fn try_from(value: u32) -> Result<Self, Self::Error> {
        match value {
            1 => Ok(Self::Raw),
            2 => Ok(Self::Celt051),
            3 => Ok(Self::Opus),
            _ => Err(DecodeError::new(
                DecodeErrorKind::InvalidValue,
                4,
                "playback data mode",
            )),
        }
    }
}

/// Sample representation selected by Playback Start.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum AudioSampleFormat {
    Signed16LittleEndian = 1,
}

impl TryFrom<u32> for AudioSampleFormat {
    type Error = DecodeError;

    fn try_from(value: u32) -> Result<Self, Self::Error> {
        match value {
            1 => Ok(Self::Signed16LittleEndian),
            _ => Err(DecodeError::new(
                DecodeErrorKind::Unsupported,
                4,
                "playback sample format",
            )),
        }
    }
}

/// Playback encoding selection with mode-dependent trailing bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlaybackMode<'a> {
    pub timestamp_ms: u32,
    pub mode: AudioDataMode,
    pub codec_data: &'a [u8],
}

impl<'a> PlaybackMode<'a> {
    pub fn decode(body: &'a [u8]) -> Result<Self, DecodeError> {
        let mut reader = Reader::new(body);
        let timestamp_ms = reader.u32("playback mode timestamp")?;
        let mode = AudioDataMode::try_from(reader.u32("playback data mode")?)?;
        let codec_data = reader.take(reader.remaining(), "playback mode data")?;
        if mode == AudioDataMode::Raw && !codec_data.is_empty() {
            return Err(DecodeError::new(
                DecodeErrorKind::InvalidValue,
                8,
                "raw playback mode data",
            ));
        }
        Ok(Self {
            timestamp_ms,
            mode,
            codec_data,
        })
    }
}

/// Stream format declared by Playback Start.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlaybackStart {
    pub channels: u32,
    pub format: AudioSampleFormat,
    pub sample_rate_hz: u32,
    pub timestamp_ms: u32,
}

impl PlaybackStart {
    pub fn decode(body: &[u8]) -> Result<Self, DecodeError> {
        const PLAYBACK_START_BYTES: usize = 4 * size_of::<u32>();
        if body.len() != PLAYBACK_START_BYTES {
            return Err(DecodeError::new(
                if body.len() < PLAYBACK_START_BYTES {
                    DecodeErrorKind::Truncated
                } else {
                    DecodeErrorKind::InvalidValue
                },
                body.len(),
                "playback start",
            ));
        }
        let mut reader = Reader::new(body);
        let channels = reader.u32("playback channel count")?;
        if channels == 0 || channels > MAX_PLAYBACK_CHANNELS {
            return Err(DecodeError::new(
                DecodeErrorKind::ResourceLimit,
                0,
                "playback channel count",
            ));
        }
        let format = AudioSampleFormat::try_from(reader.u32("playback sample format")?)?;
        let sample_rate_hz = reader.u32("playback sample rate")?;
        if sample_rate_hz == 0 || sample_rate_hz > MAX_PLAYBACK_SAMPLE_RATE_HZ {
            return Err(DecodeError::new(
                DecodeErrorKind::ResourceLimit,
                8,
                "playback sample rate",
            ));
        }
        let timestamp_ms = reader.u32("playback start timestamp")?;
        Ok(Self {
            channels,
            format,
            sample_rate_hz,
            timestamp_ms,
        })
    }

    /// Returns the byte width of one complete interleaved sample frame.
    pub fn frame_bytes(self) -> Result<usize, DecodeError> {
        let bytes_per_sample = match self.format {
            AudioSampleFormat::Signed16LittleEndian => 2,
        };
        usize::try_from(self.channels)
            .ok()
            .and_then(|channels| channels.checked_mul(bytes_per_sample))
            .ok_or_else(|| {
                DecodeError::new(DecodeErrorKind::Overflow, 0, "playback sample frame bytes")
            })
    }
}

/// Borrowed raw body from one Playback Data message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlaybackPacket<'a> {
    pub timestamp_ms: u32,
    pub data: &'a [u8],
}

impl<'a> PlaybackPacket<'a> {
    pub fn decode(body: &'a [u8]) -> Result<Self, DecodeError> {
        let mut reader = Reader::new(body);
        let timestamp_ms = reader.u32("playback packet timestamp")?;
        let data = reader.take(reader.remaining(), "playback packet data")?;
        if data.is_empty() {
            return Err(DecodeError::new(
                DecodeErrorKind::InvalidValue,
                4,
                "playback packet data",
            ));
        }
        if data.len() > MAX_PLAYBACK_PACKET_BYTES {
            return Err(DecodeError::new(
                DecodeErrorKind::ResourceLimit,
                4,
                "playback packet data",
            ));
        }
        Ok(Self { timestamp_ms, data })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raw_playback_fields_and_frame_alignment_are_checked() {
        let mut mode = 17_u32.to_le_bytes().to_vec();
        mode.extend_from_slice(&(AudioDataMode::Raw as u32).to_le_bytes());
        let mode = PlaybackMode::decode(&mode).expect("raw playback mode");
        assert_eq!(mode.timestamp_ms, 17);
        assert!(mode.codec_data.is_empty());

        let mut start = 2_u32.to_le_bytes().to_vec();
        start.extend_from_slice(&(AudioSampleFormat::Signed16LittleEndian as u32).to_le_bytes());
        start.extend_from_slice(&48_000_u32.to_le_bytes());
        start.extend_from_slice(&19_u32.to_le_bytes());
        let start = PlaybackStart::decode(&start).expect("playback start");
        assert_eq!(start.frame_bytes().expect("frame bytes"), 4);
        assert_eq!(start.timestamp_ms, 19);

        let mut packet = 21_u32.to_le_bytes().to_vec();
        packet.extend_from_slice(&[1, 2, 3, 4]);
        let packet = PlaybackPacket::decode(&packet).expect("playback packet");
        assert_eq!(packet.timestamp_ms, 21);
        assert_eq!(packet.data, [1, 2, 3, 4]);
    }

    #[test]
    fn playback_resource_bounds_precede_host_allocation() {
        let mut start = (MAX_PLAYBACK_CHANNELS + 1).to_le_bytes().to_vec();
        start.extend_from_slice(&(AudioSampleFormat::Signed16LittleEndian as u32).to_le_bytes());
        start.extend_from_slice(&48_000_u32.to_le_bytes());
        start.extend_from_slice(&0_u32.to_le_bytes());
        let error = PlaybackStart::decode(&start).expect_err("channel count bound");
        assert_eq!(error.kind, DecodeErrorKind::ResourceLimit);

        let mut packet = 0_u32.to_le_bytes().to_vec();
        packet.resize(4 + MAX_PLAYBACK_PACKET_BYTES + 1, 0);
        let error = PlaybackPacket::decode(&packet).expect_err("packet byte bound");
        assert_eq!(error.kind, DecodeErrorKind::ResourceLimit);
    }
}
