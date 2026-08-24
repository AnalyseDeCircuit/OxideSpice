use crate::wire::{Reader, checked_array_bytes, resolve_range};
use crate::{CapabilitySet, ChannelType, DecodeError, DecodeErrorKind, MAX_CAPABILITY_WORDS};

/// Literal wire magic, kept as bytes to avoid host-endian ambiguity.
pub const SPICE_MAGIC: [u8; 4] = *b"REDQ";
/// Current protocol major version accepted by spice-server.
pub const SPICE_VERSION_MAJOR: u32 = 2;
/// Current protocol minor version sent by this client.
pub const SPICE_VERSION_MINOR: u32 = 2;
/// Fixed link header size in bytes.
pub const LINK_HEADER_SIZE: usize = 16;
/// Packed fixed link-message prefix size in bytes.
pub const LINK_MESSAGE_FIXED_SIZE: usize = 18;
/// Packed fixed server link-reply prefix size in bytes.
pub const LINK_REPLY_FIXED_SIZE: usize = 178;
/// DER public-key field size used by the 1024-bit SPICE Ticket protocol.
pub const SPICE_TICKET_PUBLIC_KEY_SIZE: usize = 162;
/// Current spice-server rejects larger link bodies before allocating them.
pub const MAX_LINK_BODY_SIZE: usize = 4096;
/// Ticket authentication mechanism value used after auth selection.
pub const AUTH_MECHANISM_SPICE: u32 = 1;
/// SASL authentication mechanism value used after auth selection.
pub const AUTH_MECHANISM_SASL: u32 = 2;

/// Common capability bit indices.
pub mod common_capability {
    pub const AUTH_SELECTION: u32 = 0;
    pub const AUTH_SPICE: u32 = 1;
    pub const AUTH_SASL: u32 = 2;
    pub const MINI_HEADER: u32 = 3;
}

/// Main capability bit indices used by the session owner.
pub mod main_capability {
    pub const SEMI_SEAMLESS_MIGRATION: u32 = 0;
    pub const NAME_AND_UUID: u32 = 1;
    pub const AGENT_CONNECTED_TOKENS: u32 = 2;
    pub const SEAMLESS_MIGRATION: u32 = 3;
}

/// Display capability bit indices used by feature advertisement.
pub mod display_capability {
    pub const SIZED_STREAM: u32 = 0;
    pub const MONITORS_CONFIG: u32 = 1;
    pub const COMPOSITE: u32 = 2;
    pub const A8_SURFACE: u32 = 3;
    pub const STREAM_REPORT: u32 = 4;
    pub const LZ4_COMPRESSION: u32 = 5;
    pub const PREFERRED_COMPRESSION: u32 = 6;
    pub const GL_SCANOUT: u32 = 7;
    pub const MULTI_CODEC: u32 = 8;
    pub const CODEC_MJPEG: u32 = 9;
    pub const CODEC_VP8: u32 = 10;
    pub const CODEC_H264: u32 = 11;
    pub const PREFERRED_VIDEO_CODEC: u32 = 12;
    pub const CODEC_VP9: u32 = 13;
    pub const CODEC_H265: u32 = 14;
    pub const GL_SCANOUT2: u32 = 15;
}

/// Playback capability bit indices used by negotiated audio delivery.
pub mod playback_capability {
    pub const CELT_0_5_1: u32 = 0;
    pub const VOLUME: u32 = 1;
    pub const LATENCY: u32 = 2;
    pub const OPUS: u32 = 3;
}

/// Record capability bit indices used by negotiated capture delivery.
pub mod record_capability {
    pub const CELT_0_5_1: u32 = 0;
    pub const VOLUME: u32 = 1;
    pub const OPUS: u32 = 2;
}

/// SpiceVMC capability bit indices shared by USB redirection and Port channels.
pub mod spicevmc_capability {
    pub const DATA_COMPRESS_LZ4: u32 = 0;
}

/// A validated SPICE Link Header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LinkHeader {
    pub major_version: u32,
    pub minor_version: u32,
    pub body_size: u32,
}

impl LinkHeader {
    /// Creates the version 2.2 header emitted by this client.
    pub const fn current(body_size: u32) -> Self {
        Self {
            major_version: SPICE_VERSION_MAJOR,
            minor_version: SPICE_VERSION_MINOR,
            body_size,
        }
    }

    /// Encodes the fixed little-endian link header.
    pub fn encode(self, output: &mut Vec<u8>) {
        output.extend_from_slice(&SPICE_MAGIC);
        output.extend_from_slice(&self.major_version.to_le_bytes());
        output.extend_from_slice(&self.minor_version.to_le_bytes());
        output.extend_from_slice(&self.body_size.to_le_bytes());
    }

    /// Decodes a fixed link header and enforces local allocation limits.
    pub fn decode(input: &[u8]) -> Result<Self, DecodeError> {
        if input.len() != LINK_HEADER_SIZE {
            return Err(DecodeError::new(
                DecodeErrorKind::Truncated,
                input.len(),
                "link header",
            ));
        }
        let mut reader = Reader::new(input);
        if reader.take(4, "link magic")? != SPICE_MAGIC {
            return Err(DecodeError::new(
                DecodeErrorKind::InvalidValue,
                0,
                "link magic",
            ));
        }
        let major_version = reader.u32("link major version")?;
        let minor_version = reader.u32("link minor version")?;
        let body_size = reader.u32("link body size")?;
        if major_version != SPICE_VERSION_MAJOR {
            return Err(DecodeError::new(
                DecodeErrorKind::Unsupported,
                4,
                "link major version",
            ));
        }
        let body_size_usize = usize::try_from(body_size)
            .map_err(|_| DecodeError::new(DecodeErrorKind::Overflow, 12, "link body size"))?;
        if body_size_usize > MAX_LINK_BODY_SIZE {
            return Err(DecodeError::new(
                DecodeErrorKind::ResourceLimit,
                12,
                "link body size",
            ));
        }
        Ok(Self {
            major_version,
            minor_version,
            body_size,
        })
    }
}

/// A client link request for one independently connected channel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkMessage {
    pub connection_id: u32,
    pub channel_type: ChannelType,
    pub channel_id: u8,
    pub common_capabilities: CapabilitySet,
    pub channel_capabilities: CapabilitySet,
}

impl LinkMessage {
    /// Encodes the link body and its capability words without padding.
    pub fn encode(&self) -> Result<Vec<u8>, DecodeError> {
        let common_count = u32::try_from(self.common_capabilities.words().len()).map_err(|_| {
            DecodeError::new(DecodeErrorKind::Overflow, 0, "common capability count")
        })?;
        let channel_count =
            u32::try_from(self.channel_capabilities.words().len()).map_err(|_| {
                DecodeError::new(DecodeErrorKind::Overflow, 0, "channel capability count")
            })?;
        if self.common_capabilities.words().len() > MAX_CAPABILITY_WORDS
            || self.channel_capabilities.words().len() > MAX_CAPABILITY_WORDS
        {
            return Err(DecodeError::new(
                DecodeErrorKind::ResourceLimit,
                0,
                "link capability count",
            ));
        }
        let word_count = self
            .common_capabilities
            .words()
            .len()
            .checked_add(self.channel_capabilities.words().len())
            .ok_or_else(|| {
                DecodeError::new(DecodeErrorKind::Overflow, 0, "link capability count")
            })?;
        let capacity = LINK_MESSAGE_FIXED_SIZE
            .checked_add(word_count.checked_mul(4).ok_or_else(|| {
                DecodeError::new(DecodeErrorKind::Overflow, 0, "link capability bytes")
            })?)
            .ok_or_else(|| DecodeError::new(DecodeErrorKind::Overflow, 0, "link message bytes"))?;
        let mut output = Vec::with_capacity(capacity);
        output.extend_from_slice(&self.connection_id.to_le_bytes());
        output.push(self.channel_type as u8);
        output.push(self.channel_id);
        output.extend_from_slice(&common_count.to_le_bytes());
        output.extend_from_slice(&channel_count.to_le_bytes());
        output.extend_from_slice(&(LINK_MESSAGE_FIXED_SIZE as u32).to_le_bytes());
        for word in self
            .common_capabilities
            .words()
            .iter()
            .chain(self.channel_capabilities.words())
        {
            output.extend_from_slice(&word.to_le_bytes());
        }
        Ok(output)
    }
}

/// Server link result codes from `SpiceLinkErr`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum LinkError {
    Ok = 0,
    Error = 1,
    InvalidMagic = 2,
    InvalidData = 3,
    VersionMismatch = 4,
    NeedSecured = 5,
    NeedUnsecured = 6,
    PermissionDenied = 7,
    BadConnectionId = 8,
    ChannelUnavailable = 9,
}

impl TryFrom<u32> for LinkError {
    type Error = DecodeError;

    /// Preserves unknown results as invalid wire data rather than a generic denial.
    fn try_from(value: u32) -> Result<Self, DecodeError> {
        match value {
            0 => Ok(Self::Ok),
            1 => Ok(Self::Error),
            2 => Ok(Self::InvalidMagic),
            3 => Ok(Self::InvalidData),
            4 => Ok(Self::VersionMismatch),
            5 => Ok(Self::NeedSecured),
            6 => Ok(Self::NeedUnsecured),
            7 => Ok(Self::PermissionDenied),
            8 => Ok(Self::BadConnectionId),
            9 => Ok(Self::ChannelUnavailable),
            _ => Err(DecodeError::new(
                DecodeErrorKind::InvalidValue,
                0,
                "link result",
            )),
        }
    }
}

/// A validated server link reply with capability arrays separated by ownership.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkReply {
    pub result: LinkError,
    pub public_key_der: [u8; SPICE_TICKET_PUBLIC_KEY_SIZE],
    pub common_capabilities: CapabilitySet,
    pub channel_capabilities: CapabilitySet,
}

impl LinkReply {
    /// Decodes one link reply body using relative capability offsets.
    pub fn decode(body: &[u8]) -> Result<Self, DecodeError> {
        if body.len() < LINK_REPLY_FIXED_SIZE {
            return Err(DecodeError::new(
                DecodeErrorKind::Truncated,
                body.len(),
                "link reply",
            ));
        }
        if body.len() > MAX_LINK_BODY_SIZE {
            return Err(DecodeError::new(
                DecodeErrorKind::ResourceLimit,
                body.len(),
                "link reply",
            ));
        }
        let mut reader = Reader::new(body);
        let result = LinkError::try_from(reader.u32("link reply result")?)?;
        let public_key_der = reader
            .take(SPICE_TICKET_PUBLIC_KEY_SIZE, "ticket public key")?
            .try_into()
            .expect("exact public-key field length");
        let common_count = reader.u32("server common capability count")?;
        let channel_count = reader.u32("server channel capability count")?;
        let caps_offset = reader.u32("server capability offset")?;

        let common_bytes = checked_array_bytes(
            common_count,
            4,
            MAX_CAPABILITY_WORDS,
            reader.offset(),
            "server common capabilities",
        )?;
        let channel_bytes = checked_array_bytes(
            channel_count,
            4,
            MAX_CAPABILITY_WORDS,
            reader.offset(),
            "server channel capabilities",
        )?;
        let total_bytes = common_bytes.checked_add(channel_bytes).ok_or_else(|| {
            DecodeError::new(
                DecodeErrorKind::Overflow,
                reader.offset(),
                "server capabilities",
            )
        })?;
        if usize::try_from(caps_offset).unwrap_or(usize::MAX) < LINK_REPLY_FIXED_SIZE {
            return Err(DecodeError::new(
                DecodeErrorKind::InvalidOffset,
                usize::try_from(caps_offset).unwrap_or(usize::MAX),
                "server capability offset",
            ));
        }
        let capabilities_range =
            resolve_range(body, caps_offset, total_bytes, "server capabilities")?;
        let mut capabilities = Reader::new(&body[capabilities_range]);
        let mut common_words = Vec::with_capacity(common_bytes / 4);
        for _ in 0..common_bytes / 4 {
            common_words.push(capabilities.u32("server common capability")?);
        }
        let mut channel_words = Vec::with_capacity(channel_bytes / 4);
        for _ in 0..channel_bytes / 4 {
            channel_words.push(capabilities.u32("server channel capability")?);
        }
        Ok(Self {
            result,
            public_key_der,
            common_capabilities: CapabilitySet::from_words(common_words),
            channel_capabilities: CapabilitySet::from_words(channel_words),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn link_reply_rejects_capability_offset_outside_body() {
        let mut body = vec![0; LINK_REPLY_FIXED_SIZE];
        body[166..170].copy_from_slice(&1_u32.to_le_bytes());
        body[174..178].copy_from_slice(&u32::MAX.to_le_bytes());

        let error = LinkReply::decode(&body).expect_err("invalid offset must fail");
        assert_eq!(error.kind, DecodeErrorKind::InvalidOffset);
    }

    #[test]
    fn link_header_rejects_oversized_body_before_allocation() {
        let mut header = Vec::new();
        LinkHeader::current((MAX_LINK_BODY_SIZE + 1) as u32).encode(&mut header);

        let error = LinkHeader::decode(&header).expect_err("oversized link must fail");
        assert_eq!(error.kind, DecodeErrorKind::ResourceLimit);
    }

    #[test]
    fn link_message_uses_packed_fixed_prefix_and_capability_words() {
        let message = LinkMessage {
            connection_id: 0,
            channel_type: ChannelType::Main,
            channel_id: 0,
            common_capabilities: CapabilitySet::from_bits([
                common_capability::AUTH_SELECTION,
                common_capability::AUTH_SPICE,
                common_capability::MINI_HEADER,
            ])
            .expect("known capability bits fit"),
            channel_capabilities: CapabilitySet::new(),
        };

        let encoded = message.encode().expect("valid link message");
        assert_eq!(encoded.len(), LINK_MESSAGE_FIXED_SIZE + 4);
        assert_eq!(
            &encoded[14..18],
            &(LINK_MESSAGE_FIXED_SIZE as u32).to_le_bytes()
        );
        assert_eq!(&encoded[18..22], &0b1011_u32.to_le_bytes());
    }
}
