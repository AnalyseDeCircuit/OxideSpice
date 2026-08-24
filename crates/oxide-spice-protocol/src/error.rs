use thiserror::Error;

/// The stable reason a bounded wire decode failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecodeErrorKind {
    /// The input ended before the declared value was complete.
    Truncated,
    /// A discriminant or field value is not valid for this protocol state.
    InvalidValue,
    /// Checked arithmetic detected an attacker-controlled overflow.
    Overflow,
    /// A declared count or allocation exceeds the configured resource limit.
    ResourceLimit,
    /// A relative protocol offset points outside the current message body.
    InvalidOffset,
    /// The wire value is valid SPICE but unsupported by the current implementation.
    Unsupported,
}

/// A checked decode failure with the byte position that was being consumed.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("{kind:?} at byte {offset}: {context}")]
pub struct DecodeError {
    /// The stable error category.
    pub kind: DecodeErrorKind,
    /// The byte offset within the current bounded input.
    pub offset: usize,
    /// Static context that never contains peer payload data.
    pub context: &'static str,
}

impl DecodeError {
    /// Creates a decode error without retaining untrusted payload contents.
    pub const fn new(kind: DecodeErrorKind, offset: usize, context: &'static str) -> Self {
        Self {
            kind,
            offset,
            context,
        }
    }
}
