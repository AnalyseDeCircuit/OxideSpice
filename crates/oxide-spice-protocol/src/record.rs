//! Checked wire types for the client-to-server SPICE Record channel.

use crate::wire::Reader;
use crate::{
    AudioDataMode, AudioSampleFormat, DecodeError, DecodeErrorKind, MAX_PLAYBACK_CHANNELS,
    MAX_PLAYBACK_PACKET_BYTES, MAX_PLAYBACK_SAMPLE_RATE_HZ,
};

/// Maximum raw PCM bytes emitted in one Record Data message.
pub const MAX_RECORD_PACKET_BYTES: usize = MAX_PLAYBACK_PACKET_BYTES;

/// Server-to-client Record message identifiers.
pub mod record_server {
    pub const START: u16 = 101;
    pub const STOP: u16 = 102;
    pub const VOLUME: u16 = 103;
    pub const MUTE: u16 = 104;
}

/// Client-to-server Record message identifiers.
pub mod record_client {
    pub const DATA: u16 = 101;
    pub const MODE: u16 = 102;
    pub const START_MARK: u16 = 103;
}

/// Capture format requested by Record Start.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RecordStart {
    pub channels: u32,
    pub format: AudioSampleFormat,
    pub sample_rate_hz: u32,
}

impl RecordStart {
    pub fn decode(body: &[u8]) -> Result<Self, DecodeError> {
        const RECORD_START_BYTES: usize = 3 * size_of::<u32>();
        if body.len() != RECORD_START_BYTES {
            return Err(DecodeError::new(
                if body.len() < RECORD_START_BYTES {
                    DecodeErrorKind::Truncated
                } else {
                    DecodeErrorKind::InvalidValue
                },
                body.len(),
                "record start",
            ));
        }
        let mut reader = Reader::new(body);
        let channels = reader.u32("record channel count")?;
        if channels == 0 || channels > MAX_PLAYBACK_CHANNELS {
            return Err(DecodeError::new(
                DecodeErrorKind::ResourceLimit,
                0,
                "record channel count",
            ));
        }
        let format = AudioSampleFormat::try_from(reader.u32("record sample format")?)?;
        let sample_rate_hz = reader.u32("record sample rate")?;
        if sample_rate_hz == 0 || sample_rate_hz > MAX_PLAYBACK_SAMPLE_RATE_HZ {
            return Err(DecodeError::new(
                DecodeErrorKind::ResourceLimit,
                8,
                "record sample rate",
            ));
        }
        Ok(Self {
            channels,
            format,
            sample_rate_hz,
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
                DecodeError::new(DecodeErrorKind::Overflow, 0, "record sample frame bytes")
            })
    }
}

/// Encodes a client Record mode selection.
pub fn encode_record_mode(timestamp_ms: u32, mode: AudioDataMode) -> [u8; 8] {
    let mut body = [0; 8];
    body[..4].copy_from_slice(&timestamp_ms.to_le_bytes());
    body[4..].copy_from_slice(&(mode as u32).to_le_bytes());
    body
}

/// Encodes the client timestamp that starts one requested capture generation.
pub const fn encode_record_start_mark(timestamp_ms: u32) -> [u8; 4] {
    timestamp_ms.to_le_bytes()
}

/// Encodes one bounded raw PCM packet after caller-side frame validation.
pub fn encode_record_packet(timestamp_ms: u32, pcm: &[u8]) -> Result<Vec<u8>, DecodeError> {
    if pcm.is_empty() {
        return Err(DecodeError::new(
            crate::DecodeErrorKind::InvalidValue,
            4,
            "record packet data",
        ));
    }
    if pcm.len() > MAX_RECORD_PACKET_BYTES {
        return Err(DecodeError::new(
            crate::DecodeErrorKind::ResourceLimit,
            4,
            "record packet data",
        ));
    }
    let mut body = Vec::with_capacity(4 + pcm.len());
    body.extend_from_slice(&timestamp_ms.to_le_bytes());
    body.extend_from_slice(pcm);
    Ok(body)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_control_and_pcm_bodies_are_exact_and_bounded() {
        assert_eq!(
            encode_record_mode(9, AudioDataMode::Raw),
            [9, 0, 0, 0, AudioDataMode::Raw as u8, 0, 0, 0]
        );
        assert_eq!(encode_record_start_mark(11), 11_u32.to_le_bytes());

        let packet = encode_record_packet(13, &[1, 2, 3, 4]).expect("record packet");
        assert_eq!(&packet[..4], &13_u32.to_le_bytes());
        assert_eq!(&packet[4..], &[1, 2, 3, 4]);

        let oversized = vec![0; MAX_RECORD_PACKET_BYTES + 1];
        let error = encode_record_packet(0, &oversized).expect_err("record packet bound");
        assert_eq!(error.kind, crate::DecodeErrorKind::ResourceLimit);
    }
}
