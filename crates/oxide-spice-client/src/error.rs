//! Stable client error categories.

use std::io;

use oxide_spice_codecs::{
    GlzError, GlzErrorKind, JpegError, JpegErrorKind, Lz4Error, Lz4ErrorKind, LzError, LzErrorKind,
    OpusCodecError, QuicError, QuicErrorKind, VideoDecodeError, ZlibError, ZlibErrorKind,
};
use oxide_spice_protocol::{DecodeError, LinkError};
use thiserror::Error;

/// Stable categories suitable for host integration and retry policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorCategory {
    Configuration,
    Network,
    Tls,
    Authentication,
    Negotiation,
    Protocol,
    Unsupported,
    ResourceLimit,
    RemoteDisconnect,
    Cancelled,
    Internal,
}

/// A typed client failure that never includes credentials or frame payloads.
#[derive(Debug, Error)]
pub enum ClientError {
    #[error("invalid client configuration: {0}")]
    Configuration(&'static str),
    #[error("network I/O failed: {0}")]
    Io(#[from] io::Error),
    #[error("TLS handshake or identity verification failed: {0}")]
    Tls(String),
    #[error("wire decode failed: {0}")]
    Decode(#[from] DecodeError),
    #[error("image decode failed: {0}")]
    ImageDecode(#[from] LzError),
    #[error("LZ4 image decode failed: {0}")]
    Lz4Decode(#[from] Lz4Error),
    #[error("stateful image decode failed: {0}")]
    GlzDecode(#[from] GlzError),
    #[error("compressed image wrapper failed: {0}")]
    ZlibDecode(#[from] ZlibError),
    #[error("JPEG image decode failed: {0}")]
    JpegDecode(#[from] JpegError),
    #[error("QUIC image decode failed: {0}")]
    QuicDecode(#[from] QuicError),
    #[error("video decode failed: {0}")]
    VideoDecode(#[from] VideoDecodeError),
    #[error("Opus audio processing failed: {0}")]
    Opus(#[from] OpusCodecError),
    #[error("server rejected link with {0:?}")]
    Link(LinkError),
    #[error("SPICE Ticket authentication failed")]
    Authentication,
    #[error("SASL authentication or security layer failed: {0}")]
    Sasl(String),
    #[error("server does not offer the required authentication mechanism")]
    AuthenticationMechanism,
    #[error("server Ticket public key is invalid")]
    InvalidTicketKey,
    #[error("Ticket encryption failed")]
    TicketEncryption,
    #[error("message body of {declared} bytes exceeds the {maximum}-byte channel limit")]
    MessageTooLarge { declared: u32, maximum: usize },
    #[error(
        "server advertised {advertised} {channel} channels, exceeding the local limit {maximum}"
    )]
    ChannelLimit {
        channel: &'static str,
        advertised: usize,
        maximum: usize,
    },
    #[error("unsupported stateful SPICE message {message_type} on {channel}")]
    UnsupportedMessage {
        channel: &'static str,
        message_type: u16,
    },
    #[error("remote peer disconnected with reason {reason}")]
    RemoteDisconnect { reason: u32 },
    #[error("session was cancelled")]
    Cancelled,
    #[error("session task terminated unexpectedly")]
    TaskTerminated,
    #[error("internal client invariant failed: {0}")]
    Internal(&'static str),
}

impl ClientError {
    /// Maps detailed errors to stable host-facing categories.
    pub const fn category(&self) -> ErrorCategory {
        match self {
            Self::Configuration(_) => ErrorCategory::Configuration,
            Self::Io(_) => ErrorCategory::Network,
            Self::Tls(_) => ErrorCategory::Tls,
            Self::Decode(error) => match error.kind {
                oxide_spice_protocol::DecodeErrorKind::ResourceLimit => {
                    ErrorCategory::ResourceLimit
                }
                oxide_spice_protocol::DecodeErrorKind::Unsupported => ErrorCategory::Unsupported,
                _ => ErrorCategory::Protocol,
            },
            Self::ImageDecode(error) => match error.kind {
                LzErrorKind::ResourceLimit | LzErrorKind::OutputOverflow => {
                    ErrorCategory::ResourceLimit
                }
                LzErrorKind::UnsupportedType => ErrorCategory::Unsupported,
                LzErrorKind::Cancelled => ErrorCategory::Cancelled,
                _ => ErrorCategory::Protocol,
            },
            Self::Lz4Decode(error) => match error.kind {
                Lz4ErrorKind::ResourceLimit => ErrorCategory::ResourceLimit,
                Lz4ErrorKind::UnsupportedType => ErrorCategory::Unsupported,
                Lz4ErrorKind::Cancelled => ErrorCategory::Cancelled,
                Lz4ErrorKind::Truncated
                | Lz4ErrorKind::InvalidHeader
                | Lz4ErrorKind::InvalidBlock => ErrorCategory::Protocol,
            },
            Self::GlzDecode(error) => match error.kind {
                GlzErrorKind::ResourceLimit | GlzErrorKind::OutputOverflow => {
                    ErrorCategory::ResourceLimit
                }
                GlzErrorKind::UnsupportedType => ErrorCategory::Unsupported,
                GlzErrorKind::Cancelled => ErrorCategory::Cancelled,
                _ => ErrorCategory::Protocol,
            },
            Self::ZlibDecode(error) => match error.kind {
                ZlibErrorKind::ResourceLimit => ErrorCategory::ResourceLimit,
                ZlibErrorKind::Cancelled => ErrorCategory::Cancelled,
                _ => ErrorCategory::Protocol,
            },
            Self::JpegDecode(error) => match error.kind {
                JpegErrorKind::ResourceLimit => ErrorCategory::ResourceLimit,
                JpegErrorKind::UnsupportedFrame => ErrorCategory::Unsupported,
                JpegErrorKind::Cancelled => ErrorCategory::Cancelled,
                JpegErrorKind::DecoderPanic => ErrorCategory::Internal,
                JpegErrorKind::InvalidData | JpegErrorKind::DimensionMismatch => {
                    ErrorCategory::Protocol
                }
            },
            Self::QuicDecode(error) => match error.kind {
                QuicErrorKind::ResourceLimit => ErrorCategory::ResourceLimit,
                QuicErrorKind::UnsupportedType => ErrorCategory::Unsupported,
                QuicErrorKind::Cancelled => ErrorCategory::Cancelled,
                QuicErrorKind::Truncated
                | QuicErrorKind::InvalidHeader
                | QuicErrorKind::DimensionMismatch
                | QuicErrorKind::InvalidCode
                | QuicErrorKind::InvalidRun => ErrorCategory::Protocol,
            },
            Self::VideoDecode(VideoDecodeError::Initialization(_)) => ErrorCategory::Internal,
            Self::VideoDecode(VideoDecodeError::UnsupportedPixelLayout) => {
                ErrorCategory::Unsupported
            }
            Self::VideoDecode(VideoDecodeError::ResourceLimit) => ErrorCategory::ResourceLimit,
            Self::VideoDecode(VideoDecodeError::Cancelled) => ErrorCategory::Cancelled,
            Self::VideoDecode(VideoDecodeError::InvalidBitstream(_)) => ErrorCategory::Protocol,
            Self::Opus(OpusCodecError::UnsupportedFormat) => ErrorCategory::Unsupported,
            Self::Opus(OpusCodecError::PacketTooLarge) => ErrorCategory::ResourceLimit,
            Self::Opus(OpusCodecError::InvalidPcmFrame | OpusCodecError::Native(_)) => {
                ErrorCategory::Protocol
            }
            Self::Link(LinkError::PermissionDenied)
            | Self::Authentication
            | Self::Sasl(_)
            | Self::AuthenticationMechanism
            | Self::InvalidTicketKey
            | Self::TicketEncryption => ErrorCategory::Authentication,
            Self::Link(_) => ErrorCategory::Negotiation,
            Self::MessageTooLarge { .. } | Self::ChannelLimit { .. } => {
                ErrorCategory::ResourceLimit
            }
            Self::UnsupportedMessage { .. } => ErrorCategory::Unsupported,
            Self::RemoteDisconnect { .. } => ErrorCategory::RemoteDisconnect,
            Self::Cancelled => ErrorCategory::Cancelled,
            Self::TaskTerminated | Self::Internal(_) => ErrorCategory::Internal,
        }
    }
}
