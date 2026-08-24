//! Checked VSC messages carried by the SPICE Smartcard channel.

use crate::wire::Reader;
use crate::{DecodeError, DecodeErrorKind};

/// Maximum VSC payload retained from one Smartcard message.
pub const MAX_SMARTCARD_DATA_BYTES: usize = 64 * 1024;
/// Reader identifier used before the server assigns a stable id.
pub const SMARTCARD_UNDEFINED_READER_ID: u32 = u32::MAX;
const SMARTCARD_HEADER_BYTES: usize = 3 * size_of::<u32>();

pub mod smartcard_server {
    pub const DATA: u16 = 101;
}

pub mod smartcard_client {
    pub const DATA: u16 = 101;
}

/// VSC operation identifiers shared in both directions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum SmartcardMessageType {
    Init = 1,
    Error = 2,
    ReaderAdd = 3,
    ReaderRemove = 4,
    Atr = 5,
    CardRemove = 6,
    Apdu = 7,
    Flush = 8,
    FlushComplete = 9,
}

impl TryFrom<u32> for SmartcardMessageType {
    type Error = DecodeError;

    fn try_from(value: u32) -> Result<Self, DecodeError> {
        match value {
            1 => Ok(Self::Init),
            2 => Ok(Self::Error),
            3 => Ok(Self::ReaderAdd),
            4 => Ok(Self::ReaderRemove),
            5 => Ok(Self::Atr),
            6 => Ok(Self::CardRemove),
            7 => Ok(Self::Apdu),
            8 => Ok(Self::Flush),
            9 => Ok(Self::FlushComplete),
            _ => Err(DecodeError::new(
                DecodeErrorKind::InvalidValue,
                0,
                "smartcard message type",
            )),
        }
    }
}

/// Borrowed validated VSC message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SmartcardMessage<'a> {
    pub message_type: SmartcardMessageType,
    pub reader_id: u32,
    pub data: &'a [u8],
}

impl<'a> SmartcardMessage<'a> {
    pub fn decode(body: &'a [u8]) -> Result<Self, DecodeError> {
        let mut reader = Reader::new(body);
        let message_type = SmartcardMessageType::try_from(reader.u32("smartcard message type")?)?;
        let reader_id = reader.u32("smartcard reader id")?;
        let data_length = usize::try_from(reader.u32("smartcard data length")?)
            .map_err(|_| DecodeError::new(DecodeErrorKind::Overflow, 8, "smartcard data length"))?;
        if data_length > MAX_SMARTCARD_DATA_BYTES {
            return Err(DecodeError::new(
                DecodeErrorKind::ResourceLimit,
                8,
                "smartcard data length",
            ));
        }
        if reader.remaining() != data_length {
            return Err(DecodeError::new(
                if reader.remaining() < data_length {
                    DecodeErrorKind::Truncated
                } else {
                    DecodeErrorKind::InvalidValue
                },
                SMARTCARD_HEADER_BYTES,
                "smartcard data length",
            ));
        }
        let data = reader.take(data_length, "smartcard data")?;
        Ok(Self {
            message_type,
            reader_id,
            data,
        })
    }
}

/// Encodes one bounded VSC message.
pub fn encode_smartcard_message(
    message_type: SmartcardMessageType,
    reader_id: u32,
    data: &[u8],
) -> Result<Vec<u8>, DecodeError> {
    if data.len() > MAX_SMARTCARD_DATA_BYTES {
        return Err(DecodeError::new(
            DecodeErrorKind::ResourceLimit,
            SMARTCARD_HEADER_BYTES,
            "smartcard data length",
        ));
    }
    let data_length = u32::try_from(data.len())
        .map_err(|_| DecodeError::new(DecodeErrorKind::Overflow, 8, "smartcard data length"))?;
    let mut body = Vec::with_capacity(SMARTCARD_HEADER_BYTES + data.len());
    body.extend_from_slice(&(message_type as u32).to_le_bytes());
    body.extend_from_slice(&reader_id.to_le_bytes());
    body.extend_from_slice(&data_length.to_le_bytes());
    body.extend_from_slice(data);
    Ok(body)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn smartcard_message_round_trips_exact_length() {
        let body = encode_smartcard_message(SmartcardMessageType::Apdu, 7, &[0, 164, 4, 0])
            .expect("APDU message");
        let decoded = SmartcardMessage::decode(&body).expect("APDU message");
        assert_eq!(decoded.message_type, SmartcardMessageType::Apdu);
        assert_eq!(decoded.reader_id, 7);
        assert_eq!(decoded.data, [0, 164, 4, 0]);
    }
}
