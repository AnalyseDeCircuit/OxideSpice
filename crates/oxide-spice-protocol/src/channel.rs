use crate::wire::Reader;
use crate::{DecodeError, DecodeErrorKind};

/// Maximum channel barriers carried by one Wait For Channels message.
pub const MAX_CHANNEL_WAITS: usize = 64;

/// SPICE channel type values from `spice-protocol`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum ChannelType {
    Main = 1,
    Display = 2,
    Inputs = 3,
    Cursor = 4,
    Playback = 5,
    Record = 6,
    Tunnel = 7,
    Smartcard = 8,
    UsbRedirection = 9,
    Port = 10,
    WebDav = 11,
}

impl TryFrom<u8> for ChannelType {
    type Error = DecodeError;

    /// Rejects unknown channel types instead of folding them into a known owner.
    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            1 => Ok(Self::Main),
            2 => Ok(Self::Display),
            3 => Ok(Self::Inputs),
            4 => Ok(Self::Cursor),
            5 => Ok(Self::Playback),
            6 => Ok(Self::Record),
            7 => Ok(Self::Tunnel),
            8 => Ok(Self::Smartcard),
            9 => Ok(Self::UsbRedirection),
            10 => Ok(Self::Port),
            11 => Ok(Self::WebDav),
            _ => Err(DecodeError::new(
                DecodeErrorKind::InvalidValue,
                0,
                "channel type",
            )),
        }
    }
}

/// Negotiated normal-message header mode for one channel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Framing {
    /// Six-byte message header containing type and body size.
    Mini,
    /// Eighteen-byte message header containing serial, type, size, and sub-list offset.
    Full,
}

impl Framing {
    /// Returns the exact wire header length for bounded reads.
    pub const fn header_len(self) -> usize {
        match self {
            Self::Mini => 6,
            Self::Full => 18,
        }
    }
}

/// Metadata decoded from one normal channel message header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DataHeader {
    pub serial: Option<u64>,
    pub message_type: u16,
    pub body_size: u32,
    pub sub_list_offset: Option<u32>,
}

impl DataHeader {
    /// Decodes exactly the already-negotiated header representation.
    pub fn decode(framing: Framing, input: &[u8]) -> Result<Self, DecodeError> {
        if input.len() != framing.header_len() {
            return Err(DecodeError::new(
                DecodeErrorKind::Truncated,
                input.len(),
                "data header",
            ));
        }
        let mut reader = Reader::new(input);
        match framing {
            Framing::Mini => Ok(Self {
                serial: None,
                message_type: reader.u16("mini header message type")?,
                body_size: reader.u32("mini header body size")?,
                sub_list_offset: None,
            }),
            Framing::Full => Ok(Self {
                serial: Some(reader.u64("data header serial")?),
                message_type: reader.u16("data header message type")?,
                body_size: reader.u32("data header body size")?,
                sub_list_offset: Some(reader.u32("data header sub-list offset")?),
            }),
        }
    }

    /// Encodes a client message header using the channel's negotiated framing.
    pub fn encode(
        framing: Framing,
        serial: u64,
        message_type: u16,
        body_size: u32,
        output: &mut Vec<u8>,
    ) {
        if framing == Framing::Full {
            output.extend_from_slice(&serial.to_le_bytes());
        }
        output.extend_from_slice(&message_type.to_le_bytes());
        output.extend_from_slice(&body_size.to_le_bytes());
        if framing == Framing::Full {
            // Client messages emitted by this encoder do not carry sub-messages.
            output.extend_from_slice(&0_u32.to_le_bytes());
        }
    }
}

/// Common server-to-client message identifiers inherited by every channel.
pub mod common_server {
    pub const MIGRATE: u16 = 1;
    pub const MIGRATE_DATA: u16 = 2;
    pub const SET_ACK: u16 = 3;
    pub const PING: u16 = 4;
    pub const WAIT_FOR_CHANNELS: u16 = 5;
    pub const DISCONNECTING: u16 = 6;
    pub const NOTIFY: u16 = 7;
}

/// Common client-to-server message identifiers inherited by every channel.
pub mod common_client {
    pub const ACK_SYNC: u16 = 1;
    pub const ACK: u16 = 2;
    pub const PONG: u16 = 3;
    pub const MIGRATE_FLUSH_MARK: u16 = 4;
    pub const MIGRATE_DATA: u16 = 5;
    pub const DISCONNECTING: u16 = 6;
}

/// One cross-channel serial barrier.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChannelWait {
    pub channel_type: ChannelType,
    pub channel_id: u8,
    pub message_serial: u64,
}

/// Bounded list of serial barriers carried by common and Display reset messages.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WaitForChannels {
    pub waits: Vec<ChannelWait>,
}

impl WaitForChannels {
    /// Decodes an exact count followed by packed ten-byte channel barriers.
    pub fn decode(body: &[u8]) -> Result<Self, DecodeError> {
        let mut reader = Reader::new(body);
        let count = usize::from(reader.u8("channel wait count")?);
        if count > MAX_CHANNEL_WAITS {
            return Err(DecodeError::new(
                DecodeErrorKind::ResourceLimit,
                0,
                "channel wait count",
            ));
        }
        let expected_bytes = count.checked_mul(10).ok_or_else(|| {
            DecodeError::new(DecodeErrorKind::Overflow, reader.offset(), "channel waits")
        })?;
        if reader.remaining() != expected_bytes {
            return Err(DecodeError::new(
                if reader.remaining() < expected_bytes {
                    DecodeErrorKind::Truncated
                } else {
                    DecodeErrorKind::InvalidValue
                },
                reader.offset(),
                "channel waits",
            ));
        }
        let mut waits = Vec::with_capacity(count);
        for _ in 0..count {
            waits.push(ChannelWait {
                channel_type: ChannelType::try_from(reader.u8("wait channel type")?)?,
                channel_id: reader.u8("wait channel id")?,
                message_serial: reader.u64("wait message serial")?,
            });
        }
        Ok(Self { waits })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn channel_waits_require_an_exact_bounded_list() {
        let mut body = vec![1, ChannelType::Display as u8, 3];
        body.extend_from_slice(&42_u64.to_le_bytes());
        let waits = WaitForChannels::decode(&body).expect("valid channel wait");
        assert_eq!(waits.waits[0].channel_id, 3);
        assert_eq!(waits.waits[0].message_serial, 42);

        body.push(0);
        let error = WaitForChannels::decode(&body).expect_err("trailing bytes must fail");
        assert_eq!(error.kind, DecodeErrorKind::InvalidValue);
    }
}
