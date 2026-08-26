//! Bounded, runtime-independent codecs for SPICE media payloads.

mod glz;
mod jpeg;
mod lz;
mod lz4;
#[cfg(feature = "audio-opus")]
mod opus;
#[cfg(not(feature = "audio-opus"))]
mod opus_disabled;
mod quic;
mod video;
mod zlib;

pub use glz::{DecodedGlzImage, GlzError, GlzErrorKind, decode_glz_with_cancel};
pub use jpeg::{DecodedJpeg, JpegError, JpegErrorKind, decode_jpeg_with_cancel};

pub use lz::{
    DecodeLimits, DecodedImage, DecodedPixels, LzError, LzErrorKind, LzImageType, decode_lz,
    decode_lz_with_cancel,
};
pub use lz4::{
    DecodedLz4Image, Lz4Error, Lz4ErrorKind, compress_lz4_block_if_smaller, decode_lz4_block_exact,
    decode_lz4_with_cancel,
};
#[cfg(feature = "audio-opus")]
pub use opus::{
    OPUS_COMPRESSED_FRAME_BYTES, OPUS_FRAME_SAMPLES_PER_CHANNEL, OpusCodecError, SpiceOpusDecoder,
    SpiceOpusEncoder, supports_spice_opus_format,
};
#[cfg(not(feature = "audio-opus"))]
pub use opus_disabled::{
    OPUS_COMPRESSED_FRAME_BYTES, OPUS_FRAME_SAMPLES_PER_CHANNEL, OpusCodecError, SpiceOpusDecoder,
    SpiceOpusEncoder, supports_spice_opus_format,
};
pub use quic::{
    DecodedQuicImage, QuicError, QuicErrorKind, QuicImageType, decode_quic_with_cancel,
};
pub use video::{DecodedVideoFrame, SpiceVideoCodec, SpiceVideoDecoder, VideoDecodeError};
pub use zlib::{ZlibError, ZlibErrorKind, inflate_zlib_exact_with_cancel};
