//! Checked wire types for SPICE Port and WebDAV byte-stream channels.

use crate::wire::Reader;
use crate::{DecodeError, DecodeErrorKind};

/// Maximum UTF-8 bytes accepted for a protocol port name, including its terminator.
pub const MAX_PORT_NAME_BYTES: usize = 1024;
/// Maximum raw bytes accepted in one Port Data message.
pub const MAX_PORT_DATA_BYTES: usize = 256 * 1024;

/// Server-to-client Port message identifiers.
pub mod port_server {
    pub const DATA: u16 = 101;
    pub const COMPRESSED_DATA: u16 = 102;
    pub const INIT: u16 = 201;
    pub const EVENT: u16 = 202;
}

/// Client-to-server Port message identifiers.
pub mod port_client {
    pub const DATA: u16 = 101;
    pub const COMPRESSED_DATA: u16 = 102;
    pub const EVENT: u16 = 201;
}

/// Port readiness events shared by both directions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum PortEvent {
    Opened = 0,
    Closed = 1,
    Break = 2,
}

impl TryFrom<u8> for PortEvent {
    type Error = DecodeError;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(Self::Opened),
            1 => Ok(Self::Closed),
            2 => Ok(Self::Break),
            _ => Err(DecodeError::new(
                DecodeErrorKind::InvalidValue,
                0,
                "port event",
            )),
        }
    }
}

/// Initial port name and peer-open state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PortInit<'a> {
    pub name: &'a str,
    pub opened: bool,
}

impl<'a> PortInit<'a> {
    pub fn decode(body: &'a [u8]) -> Result<Self, DecodeError> {
        let mut reader = Reader::new(body);
        let name_size = usize::try_from(reader.u32("port name size")?)
            .map_err(|_| DecodeError::new(DecodeErrorKind::Overflow, 0, "port name size"))?;
        let name_offset = usize::try_from(reader.u32("port name offset")?)
            .map_err(|_| DecodeError::new(DecodeErrorKind::Overflow, 4, "port name offset"))?;
        let opened = match reader.u8("port opened")? {
            0 => false,
            1 => true,
            _ => {
                return Err(DecodeError::new(
                    DecodeErrorKind::InvalidValue,
                    8,
                    "port opened",
                ));
            }
        };
        if name_size < 2 {
            return Err(DecodeError::new(
                DecodeErrorKind::InvalidValue,
                0,
                "port name",
            ));
        }
        if name_size > MAX_PORT_NAME_BYTES {
            return Err(DecodeError::new(
                DecodeErrorKind::ResourceLimit,
                0,
                "port name",
            ));
        }
        if name_offset < 9 {
            return Err(DecodeError::new(
                DecodeErrorKind::InvalidOffset,
                4,
                "port name",
            ));
        }
        let name_end = name_offset
            .checked_add(name_size)
            .ok_or_else(|| DecodeError::new(DecodeErrorKind::Overflow, 4, "port name"))?;
        if name_end != body.len() {
            return Err(DecodeError::new(
                DecodeErrorKind::InvalidOffset,
                4,
                "port name",
            ));
        }
        let name = &body[name_offset..name_end];
        if name.last() != Some(&0) || name[..name.len() - 1].contains(&0) {
            return Err(DecodeError::new(
                DecodeErrorKind::InvalidValue,
                name_offset,
                "port name terminator",
            ));
        }
        let name = std::str::from_utf8(&name[..name.len() - 1]).map_err(|_| {
            DecodeError::new(
                DecodeErrorKind::InvalidValue,
                name_offset,
                "port name UTF-8",
            )
        })?;
        Ok(Self { name, opened })
    }
}

/// Decodes one exact one-byte Port Event body.
pub fn decode_port_event(body: &[u8]) -> Result<PortEvent, DecodeError> {
    if body.len() != 1 {
        return Err(DecodeError::new(
            if body.is_empty() {
                DecodeErrorKind::Truncated
            } else {
                DecodeErrorKind::InvalidValue
            },
            body.len(),
            "port event",
        ));
    }
    PortEvent::try_from(body[0])
}

/// Encodes one client Port Event body.
pub const fn encode_port_event(event: PortEvent) -> [u8; 1] {
    [event as u8]
}

/// Validates one already-bounded raw Port Data body.
pub fn decode_port_data(body: &[u8]) -> Result<&[u8], DecodeError> {
    if body.len() > MAX_PORT_DATA_BYTES {
        return Err(DecodeError::new(
            DecodeErrorKind::ResourceLimit,
            0,
            "port data",
        ));
    }
    Ok(body)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn port_init_resolves_exact_bounded_name_pointer() {
        let name = b"org.spice.test\0";
        let mut body = u32::try_from(name.len())
            .expect("port name size")
            .to_le_bytes()
            .to_vec();
        body.extend_from_slice(&9_u32.to_le_bytes());
        body.push(1);
        body.extend_from_slice(name);
        let init = PortInit::decode(&body).expect("port init");
        assert_eq!(init.name, "org.spice.test");
        assert!(init.opened);

        body[4..8].copy_from_slice(&8_u32.to_le_bytes());
        let error = PortInit::decode(&body).expect_err("name overlaps fixed fields");
        assert_eq!(error.kind, DecodeErrorKind::InvalidOffset);
    }

    #[test]
    fn port_data_and_events_are_bounded() {
        assert_eq!(
            decode_port_event(&[2]).expect("Break event"),
            PortEvent::Break
        );
        let oversized = vec![0; MAX_PORT_DATA_BYTES + 1];
        let error = decode_port_data(&oversized).expect_err("Port Data byte bound");
        assert_eq!(error.kind, DecodeErrorKind::ResourceLimit);
    }
}
