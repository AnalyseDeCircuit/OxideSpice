//! Bounded SPICE audio adapters around the bundled native libopus implementation.

use opus::{Application, Channels, Decoder, Encoder};

/// SPICE uses one 480-sample block per channel for each Opus packet.
pub const OPUS_FRAME_SAMPLES_PER_CHANNEL: usize = 480;
/// The reference SPICE implementation bounds one compressed Opus frame to 480 bytes.
pub const OPUS_COMPRESSED_FRAME_BYTES: usize = 480;
/// SPICE Opus audio is encoded as interleaved stereo signed 16-bit PCM.
const OPUS_CHANNEL_COUNT: u32 = 2;
const PCM_BYTES_PER_SAMPLE: usize = size_of::<i16>();
const OPUS_INTERLEAVED_SAMPLES: usize =
    OPUS_FRAME_SAMPLES_PER_CHANNEL * OPUS_CHANNEL_COUNT as usize;

/// Failures at the native Opus codec boundary.
#[derive(Debug, thiserror::Error)]
pub enum OpusCodecError {
    #[error("SPICE Opus requires stereo at a supported Opus sample rate")]
    UnsupportedFormat,
    #[error("SPICE Opus PCM must contain exactly one 480-sample stereo frame")]
    InvalidPcmFrame,
    #[error("SPICE Opus packet exceeds the protocol codec bound")]
    PacketTooLarge,
    #[error("libopus rejected the audio stream: {0}")]
    Native(#[from] opus::Error),
}

/// Reports whether a SPICE audio format can use the negotiated Opus mode.
pub const fn supports_spice_opus_format(channels: u32, sample_rate_hz: u32) -> bool {
    channels == OPUS_CHANNEL_COUNT
        && matches!(sample_rate_hz, 8_000 | 12_000 | 16_000 | 24_000 | 48_000)
}

/// Stateful decoder for one SPICE Playback generation.
#[derive(Debug)]
pub struct SpiceOpusDecoder {
    decoder: Decoder,
    samples: Vec<i16>,
}

impl SpiceOpusDecoder {
    /// Creates one stereo decoder for a negotiated SPICE sample rate.
    pub fn new(channels: u32, sample_rate_hz: u32) -> Result<Self, OpusCodecError> {
        if !supports_spice_opus_format(channels, sample_rate_hz) {
            return Err(OpusCodecError::UnsupportedFormat);
        }
        Ok(Self {
            decoder: Decoder::new(sample_rate_hz, Channels::Stereo)?,
            samples: vec![0; OPUS_INTERLEAVED_SAMPLES],
        })
    }

    /// Decodes one bounded Opus packet into little-endian interleaved PCM.
    pub fn decode_packet(
        &mut self,
        packet: &[u8],
        output: &mut Vec<u8>,
    ) -> Result<(), OpusCodecError> {
        if packet.len() > OPUS_COMPRESSED_FRAME_BYTES {
            return Err(OpusCodecError::PacketTooLarge);
        }
        let samples_per_channel = self.decoder.decode(packet, &mut self.samples, false)?;
        let sample_count = samples_per_channel
            .checked_mul(OPUS_CHANNEL_COUNT as usize)
            .ok_or(OpusCodecError::InvalidPcmFrame)?;
        output.clear();
        output.reserve(sample_count * PCM_BYTES_PER_SAMPLE);
        for sample in &self.samples[..sample_count] {
            output.extend_from_slice(&sample.to_le_bytes());
        }
        Ok(())
    }
}

/// Stateful encoder for one SPICE Record generation.
#[derive(Debug)]
pub struct SpiceOpusEncoder {
    encoder: Encoder,
    samples: Vec<i16>,
}

impl SpiceOpusEncoder {
    /// Creates one stereo encoder using libopus's full-audio application profile.
    pub fn new(channels: u32, sample_rate_hz: u32) -> Result<Self, OpusCodecError> {
        if !supports_spice_opus_format(channels, sample_rate_hz) {
            return Err(OpusCodecError::UnsupportedFormat);
        }
        Ok(Self {
            encoder: Encoder::new(sample_rate_hz, Channels::Stereo, Application::Audio)?,
            samples: vec![0; OPUS_INTERLEAVED_SAMPLES],
        })
    }

    /// Returns the exact PCM byte count required by one SPICE Opus packet.
    pub const fn frame_bytes() -> usize {
        OPUS_INTERLEAVED_SAMPLES * PCM_BYTES_PER_SAMPLE
    }

    /// Encodes exactly one little-endian interleaved PCM frame.
    pub fn encode_frame(&mut self, pcm: &[u8], output: &mut Vec<u8>) -> Result<(), OpusCodecError> {
        if pcm.len() != Self::frame_bytes() {
            return Err(OpusCodecError::InvalidPcmFrame);
        }
        for (sample, bytes) in self
            .samples
            .iter_mut()
            .zip(pcm.chunks_exact(PCM_BYTES_PER_SAMPLE))
        {
            *sample = i16::from_le_bytes([bytes[0], bytes[1]]);
        }
        output.resize(OPUS_COMPRESSED_FRAME_BYTES, 0);
        let encoded_bytes = self.encoder.encode(&self.samples, output)?;
        output.truncate(encoded_bytes);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opus_adapter_round_trips_one_spice_frame() {
        let mut encoder = SpiceOpusEncoder::new(2, 48_000).expect("Opus encoder");
        let mut decoder = SpiceOpusDecoder::new(2, 48_000).expect("Opus decoder");
        let pcm = vec![0; SpiceOpusEncoder::frame_bytes()];
        let mut packet = Vec::new();
        encoder
            .encode_frame(&pcm, &mut packet)
            .expect("encode silent frame");
        assert!(!packet.is_empty());
        assert!(packet.len() <= OPUS_COMPRESSED_FRAME_BYTES);

        let mut decoded = Vec::new();
        decoder
            .decode_packet(&packet, &mut decoded)
            .expect("decode silent frame");
        assert_eq!(decoded.len(), pcm.len());
    }

    #[test]
    fn opus_adapter_rejects_non_spice_formats_and_frame_sizes() {
        assert!(matches!(
            SpiceOpusEncoder::new(1, 48_000),
            Err(OpusCodecError::UnsupportedFormat)
        ));
        let mut encoder = SpiceOpusEncoder::new(2, 48_000).expect("Opus encoder");
        let error = encoder
            .encode_frame(&[0; 4], &mut Vec::new())
            .expect_err("short PCM frame");
        assert!(matches!(error, OpusCodecError::InvalidPcmFrame));
    }
}
