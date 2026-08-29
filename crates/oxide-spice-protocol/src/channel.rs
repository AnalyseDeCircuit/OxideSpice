use crate::wire::{Reader, resolve_range};
use crate::{DecodeError, DecodeErrorKind};

/// Maximum channel barriers carried by one Wait For Channels message.
pub const MAX_CHANNEL_WAITS: usize = 64;
/// Maximum logical messages accepted from one sub-message envelope.
pub const MAX_SUBMESSAGES: usize = 64;
/// Maximum untrusted text accepted from one server notification.
pub const MAX_SERVER_NOTIFICATION_BYTES: usize = 64 * 1024;

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

/// One borrowed logical message resolved from a SPICE sub-message list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SubMessage<'a> {
    pub message_type: u16,
    pub body: &'a [u8],
}

/// A fully validated, allocation-free view over one SPICE sub-message list.
#[derive(Debug, Clone, Copy)]
pub struct SubMessageList<'a> {
    body: &'a [u8],
    offsets_start: usize,
    count: usize,
}

impl<'a> SubMessageList<'a> {
    /// Validates every offset and declared body before any logical message is dispatched.
    pub fn decode(body: &'a [u8], list_offset: u32) -> Result<Self, DecodeError> {
        let list_start = resolve_range(body, list_offset, 2, "sub-message list")?.start;
        let mut list_reader = Reader::new(&body[list_start..]);
        let count = usize::from(list_reader.u16("sub-message count")?);
        if count > MAX_SUBMESSAGES {
            return Err(DecodeError::new(
                DecodeErrorKind::ResourceLimit,
                list_start,
                "sub-message count",
            ));
        }
        let offsets_start = list_start.checked_add(2).ok_or_else(|| {
            DecodeError::new(DecodeErrorKind::Overflow, list_start, "sub-message offsets")
        })?;
        let offsets_bytes = count.checked_mul(4).ok_or_else(|| {
            DecodeError::new(
                DecodeErrorKind::Overflow,
                offsets_start,
                "sub-message offsets",
            )
        })?;
        let offsets_range = resolve_range(
            body,
            u32::try_from(offsets_start).map_err(|_| {
                DecodeError::new(
                    DecodeErrorKind::Overflow,
                    offsets_start,
                    "sub-message offsets",
                )
            })?,
            offsets_bytes,
            "sub-message offsets",
        )?;
        let table_end = offsets_range.end;
        for index in 0..count {
            let offset = read_submessage_offset(body, offsets_start, index)?;
            for earlier in 0..index {
                if read_submessage_offset(body, offsets_start, earlier)? == offset {
                    return Err(DecodeError::new(
                        DecodeErrorKind::InvalidOffset,
                        offsets_start + index * 4,
                        "duplicate sub-message offset",
                    ));
                }
            }
            let header = resolve_range(body, offset, 6, "sub-message header")?;
            let mut message_reader = Reader::new(&body[header.clone()]);
            let message_type = message_reader.u16("sub-message type")?;
            if message_type == common_server::LIST {
                return Err(DecodeError::new(
                    DecodeErrorKind::Unsupported,
                    header.start,
                    "nested sub-message list",
                ));
            }
            let message_size =
                usize::try_from(message_reader.u32("sub-message size")?).map_err(|_| {
                    DecodeError::new(
                        DecodeErrorKind::Overflow,
                        header.start + 2,
                        "sub-message size",
                    )
                })?;
            let message_end = header.end.checked_add(message_size).ok_or_else(|| {
                DecodeError::new(DecodeErrorKind::Overflow, header.end, "sub-message body")
            })?;
            if message_end > body.len() {
                return Err(DecodeError::new(
                    DecodeErrorKind::InvalidOffset,
                    header.end,
                    "sub-message body",
                ));
            }
            if ranges_overlap(header.start, message_end, list_start, table_end) {
                return Err(DecodeError::new(
                    DecodeErrorKind::InvalidOffset,
                    header.start,
                    "sub-message overlaps list",
                ));
            }
        }
        Ok(Self {
            body,
            offsets_start,
            count,
        })
    }

    pub const fn len(&self) -> usize {
        self.count
    }

    pub const fn is_empty(&self) -> bool {
        self.count == 0
    }

    pub fn iter(&self) -> SubMessages<'a> {
        SubMessages {
            list: *self,
            index: 0,
        }
    }
}

/// Iterator over a list that was completely validated before construction.
pub struct SubMessages<'a> {
    list: SubMessageList<'a>,
    index: usize,
}

impl<'a> Iterator for SubMessages<'a> {
    type Item = SubMessage<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.index == self.list.count {
            return None;
        }
        let offset = read_submessage_offset(self.list.body, self.list.offsets_start, self.index)
            .expect("validated sub-message offset");
        self.index += 1;
        let start = usize::try_from(offset).expect("validated offset fits usize");
        let mut reader = Reader::new(&self.list.body[start..]);
        let message_type = reader.u16("sub-message type").expect("validated type");
        let size = usize::try_from(reader.u32("sub-message size").expect("validated size"))
            .expect("validated size fits usize");
        Some(SubMessage {
            message_type,
            body: reader
                .take(size, "sub-message body")
                .expect("validated body"),
        })
    }
}

fn read_submessage_offset(
    body: &[u8],
    offsets_start: usize,
    index: usize,
) -> Result<u32, DecodeError> {
    let start = offsets_start
        .checked_add(index.checked_mul(4).ok_or_else(|| {
            DecodeError::new(
                DecodeErrorKind::Overflow,
                offsets_start,
                "sub-message offset",
            )
        })?)
        .ok_or_else(|| {
            DecodeError::new(
                DecodeErrorKind::Overflow,
                offsets_start,
                "sub-message offset",
            )
        })?;
    let range = resolve_range(
        body,
        u32::try_from(start).map_err(|_| {
            DecodeError::new(DecodeErrorKind::Overflow, start, "sub-message offset")
        })?,
        4,
        "sub-message offset",
    )?;
    Reader::new(&body[range]).u32("sub-message offset")
}

const fn ranges_overlap(
    first_start: usize,
    first_end: usize,
    second_start: usize,
    second_end: usize,
) -> bool {
    first_start < second_end && second_start < first_end
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
    pub const LIST: u16 = 8;
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

/// One informational server notification shared by every SPICE channel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ServerNotification<'a> {
    pub timestamp: u64,
    pub severity: u32,
    pub visibility: u32,
    pub code: u32,
    pub message: &'a [u8],
}

impl<'a> ServerNotification<'a> {
    /// Decodes the exact fixed fields and bounded message defined by `SpiceMsgNotify`.
    pub fn decode(body: &'a [u8]) -> Result<Self, DecodeError> {
        let mut reader = Reader::new(body);
        let timestamp = reader.u64("notification timestamp")?;
        let severity = reader.u32("notification severity")?;
        if severity > 2 {
            return Err(DecodeError::new(
                DecodeErrorKind::InvalidValue,
                8,
                "notification severity",
            ));
        }
        let visibility = reader.u32("notification visibility")?;
        if visibility > 2 {
            return Err(DecodeError::new(
                DecodeErrorKind::InvalidValue,
                12,
                "notification visibility",
            ));
        }
        let code = reader.u32("notification code")?;
        let message_length =
            usize::try_from(reader.u32("notification message length")?).map_err(|_| {
                DecodeError::new(DecodeErrorKind::Overflow, 20, "notification message length")
            })?;
        if message_length > MAX_SERVER_NOTIFICATION_BYTES {
            return Err(DecodeError::new(
                DecodeErrorKind::ResourceLimit,
                20,
                "notification message length",
            ));
        }
        let trailing_terminator =
            reader.remaining() == message_length.saturating_add(1) && body.last() == Some(&0);
        if reader.remaining() != message_length && !trailing_terminator {
            return Err(DecodeError::new(
                if reader.remaining() < message_length {
                    DecodeErrorKind::Truncated
                } else {
                    DecodeErrorKind::InvalidValue
                },
                reader.offset(),
                "notification message",
            ));
        }
        let message = reader.take(message_length, "notification message")?;
        if trailing_terminator {
            // spice-server declares strlen(message) but transmits its terminating NUL as well.
            let _ = reader.take(1, "notification message terminator")?;
        }
        Ok(Self {
            timestamp,
            severity,
            visibility,
            code,
            message,
        })
    }
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

    #[test]
    fn server_notification_requires_an_exact_bounded_message() {
        let message = b"keyboard channel is insecure";
        let mut body = 7_u64.to_le_bytes().to_vec();
        body.extend_from_slice(&1_u32.to_le_bytes());
        body.extend_from_slice(&2_u32.to_le_bytes());
        body.extend_from_slice(&9_u32.to_le_bytes());
        body.extend_from_slice(&(message.len() as u32).to_le_bytes());
        body.extend_from_slice(message);

        let notification = ServerNotification::decode(&body).expect("valid notification");
        assert_eq!(notification.message, message);

        body.push(0);
        let notification =
            ServerNotification::decode(&body).expect("one server terminator is valid");
        assert_eq!(notification.message, message);

        *body.last_mut().expect("terminator") = 1;
        let error = ServerNotification::decode(&body).expect_err("nonzero trailing byte must fail");
        assert_eq!(error.kind, DecodeErrorKind::InvalidValue);
    }

    #[test]
    fn submessages_follow_list_order_instead_of_physical_order() {
        let mut body = 2_u16.to_le_bytes().to_vec();
        body.extend_from_slice(&20_u32.to_le_bytes());
        body.extend_from_slice(&12_u32.to_le_bytes());
        body.extend_from_slice(&[0, 0]);
        body.extend_from_slice(&101_u16.to_le_bytes());
        body.extend_from_slice(&1_u32.to_le_bytes());
        body.push(0xaa);
        body.push(0);
        body.extend_from_slice(&202_u16.to_le_bytes());
        body.extend_from_slice(&1_u32.to_le_bytes());
        body.push(0xbb);

        let list = SubMessageList::decode(&body, 0).expect("valid sub-message list");
        let messages: Vec<_> = list.iter().collect();
        assert_eq!(messages[0].message_type, 202);
        assert_eq!(messages[0].body, [0xbb]);
        assert_eq!(messages[1].message_type, 101);
        assert_eq!(messages[1].body, [0xaa]);
    }

    #[test]
    fn submessage_list_is_fully_validated_before_iteration() {
        let mut duplicate = 2_u16.to_le_bytes().to_vec();
        duplicate.extend_from_slice(&10_u32.to_le_bytes());
        duplicate.extend_from_slice(&10_u32.to_le_bytes());
        duplicate.extend_from_slice(&101_u16.to_le_bytes());
        duplicate.extend_from_slice(&0_u32.to_le_bytes());
        let error = SubMessageList::decode(&duplicate, 0)
            .expect_err("duplicate side effects must be rejected");
        assert_eq!(error.kind, DecodeErrorKind::InvalidOffset);

        let mut overlap = 1_u16.to_le_bytes().to_vec();
        overlap.extend_from_slice(&2_u32.to_le_bytes());
        overlap.extend_from_slice(&0_u32.to_le_bytes());
        let error = SubMessageList::decode(&overlap, 0)
            .expect_err("a sub-message cannot overlap its offset table");
        assert_eq!(error.kind, DecodeErrorKind::InvalidOffset);
    }
}
