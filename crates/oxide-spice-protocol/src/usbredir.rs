//! Bounded pure-Rust framing for the usbredir byte stream carried by SpiceVMC.

use crate::{DecodeError, DecodeErrorKind};

/// Initial usbredir headers always carry a 32-bit packet identifier.
pub const USBREDIR_HEADER32_BYTES: usize = 12;
/// Negotiated usbredir headers can widen the packet identifier to 64 bits.
pub const USBREDIR_HEADER64_BYTES: usize = 16;
/// Fixed byte width of the free-form Hello version field.
pub const USBREDIR_VERSION_BYTES: usize = 64;
/// Local safety bound for one complete usbredir packet payload.
pub const MAX_USBREDIR_PACKET_BYTES: usize = 1024 * 1024;
/// Maximum capability words accepted from a future peer.
pub const MAX_USBREDIR_CAPABILITY_WORDS: usize = 8;

/// Mandatory first packet type in each direction.
pub const USBREDIR_PACKET_HELLO: u32 = 0;

/// Published usbredir capability bit indices.
pub mod usbredir_capability {
    pub const BULK_STREAMS: u32 = 0;
    pub const CONNECT_DEVICE_VERSION: u32 = 1;
    pub const FILTER: u32 = 2;
    pub const DEVICE_DISCONNECT_ACK: u32 = 3;
    pub const EP_INFO_MAX_PACKET_SIZE: u32 = 4;
    pub const IDS_64BIT: u32 = 5;
    pub const BULK_LENGTH_32BIT: u32 = 6;
    pub const BULK_RECEIVING: u32 = 7;
}

/// One-word capability set covering every currently published usbredir bit.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct UsbRedirCapabilities(u32);

impl UsbRedirCapabilities {
    pub const fn from_word(word: u32) -> Self {
        Self(word)
    }

    pub const fn word(self) -> u32 {
        self.0
    }

    pub const fn contains(self, bit: u32) -> bool {
        bit < u32::BITS && self.0 & (1_u32 << bit) != 0
    }
}

/// Parsed peer Hello fields needed to establish later header width.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsbRedirHello {
    pub version: String,
    pub capabilities: Vec<u32>,
}

/// One complete usbredir packet with its type-specific payload left opaque.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsbRedirPacket {
    pub packet_type: u32,
    pub id: u64,
    pub payload: Vec<u8>,
}

#[derive(Debug)]
struct PartialUsbRedirPacket {
    packet_type: u32,
    id: u64,
    declared_size: usize,
    payload: Vec<u8>,
}

/// Incremental parser for usbredir packets split across arbitrary SpiceVMC messages.
#[derive(Debug)]
pub struct UsbRedirStreamDecoder {
    local_capabilities: UsbRedirCapabilities,
    peer_hello: Option<UsbRedirHello>,
    wide_ids: bool,
    header: [u8; USBREDIR_HEADER64_BYTES],
    header_bytes: usize,
    current: Option<PartialUsbRedirPacket>,
}

impl UsbRedirStreamDecoder {
    pub fn new(local_capabilities: UsbRedirCapabilities) -> Self {
        Self {
            local_capabilities,
            peer_hello: None,
            wide_ids: false,
            header: [0; USBREDIR_HEADER64_BYTES],
            header_bytes: 0,
            current: None,
        }
    }

    pub fn peer_hello(&self) -> Option<&UsbRedirHello> {
        self.peer_hello.as_ref()
    }

    pub const fn uses_64_bit_ids(&self) -> bool {
        self.wide_ids
    }

    /// Appends complete packets while retaining partial header and payload bytes.
    pub fn push_bytes(
        &mut self,
        mut input: &[u8],
        output: &mut Vec<UsbRedirPacket>,
    ) -> Result<(), DecodeError> {
        while !input.is_empty() {
            if self.current.is_none() {
                let header_size = if self.peer_hello.is_none() || !self.wide_ids {
                    USBREDIR_HEADER32_BYTES
                } else {
                    USBREDIR_HEADER64_BYTES
                };
                let needed = header_size - self.header_bytes;
                let copied = needed.min(input.len());
                self.header[self.header_bytes..self.header_bytes + copied]
                    .copy_from_slice(&input[..copied]);
                self.header_bytes += copied;
                input = &input[copied..];
                if self.header_bytes != header_size {
                    continue;
                }
                let packet_type =
                    u32::from_le_bytes(self.header[..4].try_into().expect("usbredir packet type"));
                let declared_size = usize::try_from(u32::from_le_bytes(
                    self.header[4..8]
                        .try_into()
                        .expect("usbredir packet length"),
                ))
                .map_err(|_| {
                    DecodeError::new(DecodeErrorKind::Overflow, 4, "usbredir packet length")
                })?;
                if declared_size > MAX_USBREDIR_PACKET_BYTES {
                    return Err(DecodeError::new(
                        DecodeErrorKind::ResourceLimit,
                        4,
                        "usbredir packet length",
                    ));
                }
                let id = if header_size == USBREDIR_HEADER64_BYTES {
                    u64::from_le_bytes(
                        self.header[8..16]
                            .try_into()
                            .expect("usbredir 64-bit packet id"),
                    )
                } else {
                    u64::from(u32::from_le_bytes(
                        self.header[8..12]
                            .try_into()
                            .expect("usbredir 32-bit packet id"),
                    ))
                };
                if self.peer_hello.is_none() && (packet_type != USBREDIR_PACKET_HELLO || id != 0) {
                    return Err(DecodeError::new(
                        DecodeErrorKind::InvalidValue,
                        0,
                        "usbredir first packet",
                    ));
                }
                self.current = Some(PartialUsbRedirPacket {
                    packet_type,
                    id,
                    declared_size,
                    payload: Vec::with_capacity(declared_size),
                });
                self.header_bytes = 0;
            }

            let current = self.current.as_mut().expect("usbredir header initialized");
            let remaining = current.declared_size - current.payload.len();
            let copied = remaining.min(input.len());
            current.payload.extend_from_slice(&input[..copied]);
            input = &input[copied..];
            if current.payload.len() == current.declared_size {
                let complete = self.current.take().expect("complete usbredir packet");
                if self.peer_hello.is_none() {
                    let hello = decode_usbredir_hello(&complete.payload)?;
                    let peer_64bit = hello
                        .capabilities
                        .first()
                        .is_some_and(|word| word & (1 << usbredir_capability::IDS_64BIT) != 0);
                    self.wide_ids = self
                        .local_capabilities
                        .contains(usbredir_capability::IDS_64BIT)
                        && peer_64bit;
                    self.peer_hello = Some(hello);
                } else if complete.packet_type == USBREDIR_PACKET_HELLO {
                    return Err(DecodeError::new(
                        DecodeErrorKind::InvalidValue,
                        0,
                        "repeated usbredir Hello",
                    ));
                }
                output.push(UsbRedirPacket {
                    packet_type: complete.packet_type,
                    id: complete.id,
                    payload: complete.payload,
                });
            }
        }
        Ok(())
    }
}

/// Encodes the mandatory first packet using the always-32-bit initial header.
pub fn encode_usbredir_hello(
    version: &str,
    capabilities: UsbRedirCapabilities,
) -> Result<Vec<u8>, DecodeError> {
    if version.is_empty() || version.len() >= USBREDIR_VERSION_BYTES || version.contains('\0') {
        return Err(DecodeError::new(
            DecodeErrorKind::InvalidValue,
            0,
            "usbredir version",
        ));
    }
    let payload_size = USBREDIR_VERSION_BYTES + 4;
    let mut output = Vec::with_capacity(USBREDIR_HEADER32_BYTES + payload_size);
    output.extend_from_slice(&USBREDIR_PACKET_HELLO.to_le_bytes());
    output.extend_from_slice(&(payload_size as u32).to_le_bytes());
    output.extend_from_slice(&0_u32.to_le_bytes());
    output.extend_from_slice(version.as_bytes());
    output.resize(USBREDIR_HEADER32_BYTES + USBREDIR_VERSION_BYTES, 0);
    output.extend_from_slice(&capabilities.word().to_le_bytes());
    Ok(output)
}

/// Encodes one post-Hello packet using the negotiated identifier width.
pub fn encode_usbredir_packet(
    packet_type: u32,
    id: u64,
    payload: &[u8],
    wide_ids: bool,
) -> Result<Vec<u8>, DecodeError> {
    if packet_type == USBREDIR_PACKET_HELLO {
        return Err(DecodeError::new(
            DecodeErrorKind::InvalidValue,
            0,
            "post-Hello usbredir packet type",
        ));
    }
    if payload.len() > MAX_USBREDIR_PACKET_BYTES {
        return Err(DecodeError::new(
            DecodeErrorKind::ResourceLimit,
            4,
            "usbredir packet payload",
        ));
    }
    let payload_size = u32::try_from(payload.len())
        .map_err(|_| DecodeError::new(DecodeErrorKind::Overflow, 4, "usbredir packet payload"))?;
    let header_size = if wide_ids {
        USBREDIR_HEADER64_BYTES
    } else {
        USBREDIR_HEADER32_BYTES
    };
    let mut output = Vec::with_capacity(header_size + payload.len());
    output.extend_from_slice(&packet_type.to_le_bytes());
    output.extend_from_slice(&payload_size.to_le_bytes());
    if wide_ids {
        output.extend_from_slice(&id.to_le_bytes());
    } else {
        let id = u32::try_from(id).map_err(|_| {
            DecodeError::new(
                DecodeErrorKind::InvalidValue,
                8,
                "usbredir 32-bit packet id",
            )
        })?;
        output.extend_from_slice(&id.to_le_bytes());
    }
    output.extend_from_slice(payload);
    Ok(output)
}

fn decode_usbredir_hello(payload: &[u8]) -> Result<UsbRedirHello, DecodeError> {
    if payload.len() < USBREDIR_VERSION_BYTES
        || !(payload.len() - USBREDIR_VERSION_BYTES).is_multiple_of(4)
    {
        return Err(DecodeError::new(
            DecodeErrorKind::InvalidValue,
            0,
            "usbredir Hello length",
        ));
    }
    let capability_words = (payload.len() - USBREDIR_VERSION_BYTES) / 4;
    if capability_words > MAX_USBREDIR_CAPABILITY_WORDS {
        return Err(DecodeError::new(
            DecodeErrorKind::ResourceLimit,
            USBREDIR_VERSION_BYTES,
            "usbredir capability words",
        ));
    }
    let version_field = &payload[..USBREDIR_VERSION_BYTES];
    let terminator = version_field
        .iter()
        .position(|byte| *byte == 0)
        .ok_or_else(|| {
            DecodeError::new(
                DecodeErrorKind::InvalidValue,
                0,
                "usbredir version terminator",
            )
        })?;
    let version = std::str::from_utf8(&version_field[..terminator])
        .map_err(|_| DecodeError::new(DecodeErrorKind::InvalidValue, 0, "usbredir version UTF-8"))?
        .to_owned();
    let mut capabilities = Vec::with_capacity(capability_words);
    for word in payload[USBREDIR_VERSION_BYTES..].chunks_exact(4) {
        capabilities.push(u32::from_le_bytes(
            word.try_into().expect("usbredir capability word"),
        ));
    }
    Ok(UsbRedirHello {
        version,
        capabilities,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hello_and_following_packet_survive_arbitrary_stream_splits() {
        let local = UsbRedirCapabilities::from_word(1 << usbredir_capability::IDS_64BIT);
        let mut stream = encode_usbredir_hello("OxideSpice", local).expect("usbredir Hello");
        stream.extend_from_slice(&9_u32.to_le_bytes());
        stream.extend_from_slice(&3_u32.to_le_bytes());
        stream.extend_from_slice(&42_u64.to_le_bytes());
        stream.extend_from_slice(&[1, 2, 3]);

        let mut decoder = UsbRedirStreamDecoder::new(local);
        let mut packets = Vec::new();
        for fragment in stream.chunks(7) {
            decoder
                .push_bytes(fragment, &mut packets)
                .expect("fragmented usbredir stream");
        }
        assert!(decoder.uses_64_bit_ids());
        assert_eq!(
            decoder.peer_hello().expect("peer Hello").version,
            "OxideSpice"
        );
        assert_eq!(packets.len(), 2);
        assert_eq!(packets[1].id, 42);
        assert_eq!(packets[1].payload, [1, 2, 3]);
    }

    #[test]
    fn first_packet_and_declared_size_are_bounded_before_allocation() {
        let mut decoder = UsbRedirStreamDecoder::new(UsbRedirCapabilities::default());
        let mut packets = Vec::new();
        let mut invalid = 1_u32.to_le_bytes().to_vec();
        invalid.extend_from_slice(&0_u32.to_le_bytes());
        invalid.extend_from_slice(&0_u32.to_le_bytes());
        let error = decoder
            .push_bytes(&invalid, &mut packets)
            .expect_err("Hello must be first");
        assert_eq!(error.kind, DecodeErrorKind::InvalidValue);

        let mut oversized = USBREDIR_PACKET_HELLO.to_le_bytes().to_vec();
        oversized.extend_from_slice(
            &u32::try_from(MAX_USBREDIR_PACKET_BYTES + 1)
                .expect("test packet bound")
                .to_le_bytes(),
        );
        oversized.extend_from_slice(&0_u32.to_le_bytes());
        let mut decoder = UsbRedirStreamDecoder::new(UsbRedirCapabilities::default());
        let error = decoder
            .push_bytes(&oversized, &mut packets)
            .expect_err("usbredir packet bound");
        assert_eq!(error.kind, DecodeErrorKind::ResourceLimit);
    }
}
