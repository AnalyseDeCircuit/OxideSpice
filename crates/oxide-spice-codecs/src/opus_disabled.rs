//! Build-time fallback for clients that do not enable the native Opus boundary.

pub const OPUS_FRAME_SAMPLES_PER_CHANNEL: usize = 480;
pub const OPUS_COMPRESSED_FRAME_BYTES: usize = 480;

#[derive(Debug, thiserror::Error)]
pub enum OpusCodecError {
    #[error("SPICE Opus requires stereo at a supported Opus sample rate")]
    UnsupportedFormat,
    #[error("SPICE Opus PCM must contain exactly one 480-sample stereo frame")]
    InvalidPcmFrame,
    #[error("SPICE Opus packet exceeds the protocol codec bound")]
    PacketTooLarge,
    #[error("native Opus support is disabled in this build")]
    Native(String),
}

pub const fn supports_spice_opus_format(_channels: u32, _sample_rate_hz: u32) -> bool {
    false
}

pub struct SpiceOpusDecoder;

impl SpiceOpusDecoder {
    pub fn new(_channels: u32, _sample_rate_hz: u32) -> Result<Self, OpusCodecError> {
        Err(OpusCodecError::Native("disabled at build time".to_owned()))
    }

    pub fn decode_packet(
        &mut self,
        _packet: &[u8],
        _output: &mut Vec<u8>,
    ) -> Result<(), OpusCodecError> {
        Err(OpusCodecError::Native("disabled at build time".to_owned()))
    }
}

pub struct SpiceOpusEncoder;

impl SpiceOpusEncoder {
    pub fn new(_channels: u32, _sample_rate_hz: u32) -> Result<Self, OpusCodecError> {
        Err(OpusCodecError::Native("disabled at build time".to_owned()))
    }

    pub const fn frame_bytes() -> usize {
        OPUS_FRAME_SAMPLES_PER_CHANNEL * 2 * 2
    }

    pub fn encode_frame(
        &mut self,
        _pcm: &[u8],
        _output: &mut Vec<u8>,
    ) -> Result<(), OpusCodecError> {
        Err(OpusCodecError::Native("disabled at build time".to_owned()))
    }
}
