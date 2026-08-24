use crate::wire::{Reader, checked_array_bytes, resolve_range};
use crate::{ChannelType, DecodeError, DecodeErrorKind};

/// Maximum server display name bytes accepted from Main, including its terminator.
pub const MAX_MAIN_NAME_BYTES: usize = 1024;
pub const MAX_MIGRATION_HOST_BYTES: usize = 1024;
pub const MAX_MIGRATION_CERT_SUBJECT_BYTES: usize = 4096;

/// Main server-to-client message identifiers.
pub mod main_server {
    pub const MIGRATE_BEGIN: u16 = 101;
    pub const MIGRATE_CANCEL: u16 = 102;
    pub const INIT: u16 = 103;
    pub const CHANNELS_LIST: u16 = 104;
    pub const MOUSE_MODE: u16 = 105;
    pub const MULTI_MEDIA_TIME: u16 = 106;
    pub const AGENT_CONNECTED: u16 = 107;
    pub const AGENT_DISCONNECTED: u16 = 108;
    pub const AGENT_DATA: u16 = 109;
    pub const AGENT_TOKEN: u16 = 110;
    pub const MIGRATE_SWITCH_HOST: u16 = 111;
    pub const MIGRATE_END: u16 = 112;
    pub const NAME: u16 = 113;
    pub const UUID: u16 = 114;
    pub const AGENT_CONNECTED_TOKENS: u16 = 115;
    pub const MIGRATE_BEGIN_SEAMLESS: u16 = 116;
    pub const MIGRATE_DST_SEAMLESS_ACK: u16 = 117;
    pub const MIGRATE_DST_SEAMLESS_NACK: u16 = 118;
}

/// Main client-to-server message identifiers.
pub mod main_client {
    pub const CLIENT_INFO: u16 = 101;
    pub const MIGRATE_CONNECTED: u16 = 102;
    pub const MIGRATE_CONNECT_ERROR: u16 = 103;
    pub const ATTACH_CHANNELS: u16 = 104;
    pub const MOUSE_MODE_REQUEST: u16 = 105;
    pub const AGENT_START: u16 = 106;
    pub const AGENT_DATA: u16 = 107;
    pub const AGENT_TOKEN: u16 = 108;
    pub const MIGRATE_END: u16 = 109;
    pub const MIGRATE_DST_DO_SEAMLESS: u16 = 110;
    pub const MIGRATE_CONNECTED_SEAMLESS: u16 = 111;
}

/// Target endpoint carried by Main migration control messages.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MigrationDestination {
    pub port: u16,
    pub secure_port: u16,
    pub host: String,
    pub certificate_subject: Option<String>,
}

/// Migration preconnect request, optionally carrying the seamless protocol version.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MigrationBegin {
    pub destination: MigrationDestination,
    pub source_version: Option<u32>,
}

impl MigrationBegin {
    pub fn decode(body: &[u8], seamless: bool) -> Result<Self, DecodeError> {
        let fixed_bytes = if seamless { 24 } else { 20 };
        if body.len() < fixed_bytes {
            return Err(DecodeError::new(
                DecodeErrorKind::Truncated,
                body.len(),
                "Main migration begin",
            ));
        }
        let destination = decode_migration_destination(body)?;
        let source_version = if seamless {
            let mut version = Reader::new(&body[20..24]);
            Some(version.u32("migration source version")?)
        } else {
            None
        };
        Ok(Self {
            destination,
            source_version,
        })
    }
}

impl MigrationDestination {
    pub fn decode_switch_host(body: &[u8]) -> Result<Self, DecodeError> {
        decode_migration_destination(body)
    }
}

fn decode_migration_destination(body: &[u8]) -> Result<MigrationDestination, DecodeError> {
    if body.len() < 20 {
        return Err(DecodeError::new(
            DecodeErrorKind::Truncated,
            body.len(),
            "migration destination",
        ));
    }
    let mut reader = Reader::new(&body[..20]);
    let port = reader.u16("migration port")?;
    let secure_port = reader.u16("migration secure port")?;
    if port == 0 && secure_port == 0 {
        return Err(DecodeError::new(
            DecodeErrorKind::InvalidValue,
            0,
            "migration destination ports",
        ));
    }
    let host_size = usize::try_from(reader.u32("migration host size")?)
        .map_err(|_| DecodeError::new(DecodeErrorKind::Overflow, 4, "migration host size"))?;
    let host_offset = reader.u32("migration host offset")?;
    let certificate_size = usize::try_from(reader.u32("migration certificate subject size")?)
        .map_err(|_| {
            DecodeError::new(
                DecodeErrorKind::Overflow,
                12,
                "migration certificate subject size",
            )
        })?;
    let certificate_offset = reader.u32("migration certificate subject offset")?;
    let host = decode_migration_string(
        body,
        host_offset,
        host_size,
        MAX_MIGRATION_HOST_BYTES,
        "migration host",
        false,
    )?
    .expect("migration host is required");
    let certificate_subject = decode_migration_string(
        body,
        certificate_offset,
        certificate_size,
        MAX_MIGRATION_CERT_SUBJECT_BYTES,
        "migration certificate subject",
        true,
    )?;
    Ok(MigrationDestination {
        port,
        secure_port,
        host,
        certificate_subject,
    })
}

fn decode_migration_string(
    body: &[u8],
    offset: u32,
    size: usize,
    maximum: usize,
    context: &'static str,
    optional: bool,
) -> Result<Option<String>, DecodeError> {
    if size == 0 {
        if optional && offset == 0 {
            return Ok(None);
        }
        return Err(DecodeError::new(
            DecodeErrorKind::InvalidValue,
            usize::try_from(offset).unwrap_or(usize::MAX),
            context,
        ));
    }
    if size > maximum {
        return Err(DecodeError::new(
            DecodeErrorKind::ResourceLimit,
            usize::try_from(offset).unwrap_or(usize::MAX),
            context,
        ));
    }
    let range = resolve_range(body, offset, size, context)?;
    let bytes = &body[range.clone()];
    if bytes.last() != Some(&0) || bytes[..bytes.len() - 1].contains(&0) {
        return Err(DecodeError::new(
            DecodeErrorKind::InvalidValue,
            range.start,
            context,
        ));
    }
    let text = std::str::from_utf8(&bytes[..bytes.len() - 1])
        .map_err(|_| DecodeError::new(DecodeErrorKind::InvalidValue, range.start, context))?;
    if text.is_empty() && !optional {
        return Err(DecodeError::new(
            DecodeErrorKind::InvalidValue,
            range.start,
            context,
        ));
    }
    Ok((!text.is_empty()).then(|| text.to_owned()))
}

/// Server-controlled pointer mode identifiers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u16)]
pub enum MouseMode {
    Server = 1,
    Client = 2,
}

impl TryFrom<u16> for MouseMode {
    type Error = DecodeError;

    fn try_from(value: u16) -> Result<Self, Self::Error> {
        match value {
            1 => Ok(Self::Server),
            2 => Ok(Self::Client),
            _ => Err(DecodeError::new(
                DecodeErrorKind::InvalidValue,
                0,
                "current mouse mode",
            )),
        }
    }
}

/// Confirmed mouse mode and the set of modes currently offered by the server.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MouseModeState {
    pub supported_modes: u16,
    pub current_mode: MouseMode,
}

impl MouseModeState {
    /// Decodes the packed flags16 plus unique flags16 Main message.
    pub fn decode(body: &[u8]) -> Result<Self, DecodeError> {
        if body.len() != 4 {
            return Err(DecodeError::new(
                if body.len() < 4 {
                    DecodeErrorKind::Truncated
                } else {
                    DecodeErrorKind::InvalidValue
                },
                body.len(),
                "main mouse mode",
            ));
        }
        let mut reader = Reader::new(body);
        let supported_modes = reader.u16("supported mouse modes")?;
        let current_mode = MouseMode::try_from(reader.u16("current mouse mode")?)?;
        let known_modes = MouseMode::Server as u16 | MouseMode::Client as u16;
        if supported_modes & !known_modes != 0 || supported_modes & current_mode as u16 == 0 {
            return Err(DecodeError::new(
                DecodeErrorKind::InvalidValue,
                0,
                "supported mouse modes",
            ));
        }
        Ok(Self {
            supported_modes,
            current_mode,
        })
    }

    /// Returns whether the server permits the requested mode.
    pub const fn supports(self, mode: MouseMode) -> bool {
        self.supported_modes & mode as u16 != 0
    }
}

/// Encodes the exact flags16 Main mouse mode request body.
pub const fn encode_mouse_mode_request(mode: MouseMode) -> [u8; 2] {
    (mode as u16).to_le_bytes()
}

/// Decodes an exact Main Agent token or disconnect-reason body.
pub fn decode_agent_u32(body: &[u8], context: &'static str) -> Result<u32, DecodeError> {
    if body.len() != 4 {
        return Err(DecodeError::new(
            if body.len() < 4 {
                DecodeErrorKind::Truncated
            } else {
                DecodeErrorKind::InvalidValue
            },
            body.len(),
            context,
        ));
    }
    let mut reader = Reader::new(body);
    reader.u32(context)
}

/// Encodes one Main Agent token count without protocol-specific sentinel values.
pub const fn encode_agent_tokens(tokens: u32) -> [u8; 4] {
    tokens.to_le_bytes()
}

/// Decodes one length-prefixed, NUL-terminated UTF-8 server name.
pub fn decode_main_name(body: &[u8]) -> Result<&str, DecodeError> {
    let mut reader = Reader::new(body);
    let name_bytes = usize::try_from(reader.u32("Main server name length")?)
        .map_err(|_| DecodeError::new(DecodeErrorKind::Overflow, 0, "Main server name length"))?;
    if name_bytes == 0 || name_bytes > MAX_MAIN_NAME_BYTES || reader.remaining() != name_bytes {
        return Err(DecodeError::new(
            if reader.remaining() < name_bytes {
                DecodeErrorKind::Truncated
            } else {
                DecodeErrorKind::InvalidValue
            },
            4,
            "Main server name",
        ));
    }
    let name = reader.take(name_bytes, "Main server name")?;
    if name.last() != Some(&0) || name[..name.len() - 1].contains(&0) {
        return Err(DecodeError::new(
            DecodeErrorKind::InvalidValue,
            4,
            "Main server name terminator",
        ));
    }
    std::str::from_utf8(&name[..name.len() - 1])
        .map_err(|_| DecodeError::new(DecodeErrorKind::InvalidValue, 4, "Main server name UTF-8"))
}

/// Decodes one exact RFC 4122 byte sequence without applying host endianness.
pub fn decode_main_uuid(body: &[u8]) -> Result<[u8; 16], DecodeError> {
    body.try_into().map_err(|_| {
        DecodeError::new(
            if body.len() < 16 {
                DecodeErrorKind::Truncated
            } else {
                DecodeErrorKind::InvalidValue
            },
            body.len(),
            "Main server UUID",
        )
    })
}

/// Initial session information sent as the first Main-specific message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MainInit {
    pub session_id: u32,
    pub display_channels_hint: u32,
    pub supported_mouse_modes: u32,
    pub current_mouse_mode: u32,
    pub agent_connected: bool,
    pub agent_tokens: u32,
    pub multi_media_time: u32,
    pub ram_hint: u32,
}

impl MainInit {
    /// Decodes the exact packed 32-byte Main Init body.
    pub fn decode(body: &[u8]) -> Result<Self, DecodeError> {
        if body.len() != 32 {
            return Err(DecodeError::new(
                if body.len() < 32 {
                    DecodeErrorKind::Truncated
                } else {
                    DecodeErrorKind::InvalidValue
                },
                body.len(),
                "main init size",
            ));
        }
        let mut reader = Reader::new(body);
        Ok(Self {
            session_id: reader.u32("main session id")?,
            display_channels_hint: reader.u32("display channel hint")?,
            supported_mouse_modes: reader.u32("supported mouse modes")?,
            current_mouse_mode: reader.u32("current mouse mode")?,
            agent_connected: reader.u32("agent connected")? != 0,
            agent_tokens: reader.u32("agent tokens")?,
            multi_media_time: reader.u32("multimedia time")?,
            ram_hint: reader.u32("RAM hint")?,
        })
    }

    /// Validates the wider legacy Init fields as the same mode semantics used by later messages.
    pub fn mouse_mode_state(self) -> Result<MouseModeState, DecodeError> {
        let supported_modes = u16::try_from(self.supported_mouse_modes).map_err(|_| {
            DecodeError::new(
                DecodeErrorKind::InvalidValue,
                0,
                "initial supported mouse modes",
            )
        })?;
        let current_mode = u16::try_from(self.current_mouse_mode).map_err(|_| {
            DecodeError::new(
                DecodeErrorKind::InvalidValue,
                0,
                "initial current mouse mode",
            )
        })?;
        let mut body = [0; 4];
        body[..2].copy_from_slice(&supported_modes.to_le_bytes());
        body[2..].copy_from_slice(&current_mode.to_le_bytes());
        MouseModeState::decode(&body)
    }
}

/// One independently linkable child channel advertised by Main.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChannelId {
    pub channel_type: ChannelType,
    pub channel_id: u8,
}

/// A bounded child-channel discovery list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChannelsList {
    pub channels: Vec<ChannelId>,
}

impl ChannelsList {
    /// The bound prevents a peer from converting a tiny Main message into a large allocation.
    pub const MAX_CHANNELS: usize = 256;

    /// Decodes a count followed by packed two-byte channel identities.
    pub fn decode(body: &[u8]) -> Result<Self, DecodeError> {
        let mut reader = Reader::new(body);
        let count = reader.u32("channel list count")?;
        let channel_bytes = checked_array_bytes(
            count,
            2,
            Self::MAX_CHANNELS,
            reader.offset(),
            "channel list",
        )?;
        if reader.remaining() != channel_bytes {
            return Err(DecodeError::new(
                if reader.remaining() < channel_bytes {
                    DecodeErrorKind::Truncated
                } else {
                    DecodeErrorKind::InvalidValue
                },
                reader.offset(),
                "channel list size",
            ));
        }
        let mut channels = Vec::with_capacity(channel_bytes / 2);
        for _ in 0..channel_bytes / 2 {
            let channel = ChannelId {
                channel_type: ChannelType::try_from(reader.u8("channel list type")?)?,
                channel_id: reader.u8("channel list id")?,
            };
            if channels.contains(&channel) {
                return Err(DecodeError::new(
                    DecodeErrorKind::InvalidValue,
                    reader.offset().saturating_sub(2),
                    "duplicate channel identity",
                ));
            }
            channels.push(channel);
        }
        Ok(Self { channels })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn channels_list_rejects_count_larger_than_body() {
        let mut body = 2_u32.to_le_bytes().to_vec();
        body.extend_from_slice(&[ChannelType::Display as u8, 0]);

        let error = ChannelsList::decode(&body).expect_err("truncated list must fail");
        assert_eq!(error.kind, DecodeErrorKind::Truncated);

        let mut duplicate = 2_u32.to_le_bytes().to_vec();
        duplicate.extend_from_slice(&[ChannelType::Display as u8, 0]);
        duplicate.extend_from_slice(&[ChannelType::Display as u8, 0]);
        let error = ChannelsList::decode(&duplicate).expect_err("duplicate identity must fail");
        assert_eq!(error.kind, DecodeErrorKind::InvalidValue);
    }

    #[test]
    fn migration_begin_resolves_terminated_strings_and_version() {
        let host = b"target.example\0";
        let subject = b"CN=target\0";
        let host_offset = 24_u32;
        let subject_offset = host_offset + host.len() as u32;
        let mut body = Vec::new();
        body.extend_from_slice(&5900_u16.to_le_bytes());
        body.extend_from_slice(&5901_u16.to_le_bytes());
        body.extend_from_slice(&(host.len() as u32).to_le_bytes());
        body.extend_from_slice(&host_offset.to_le_bytes());
        body.extend_from_slice(&(subject.len() as u32).to_le_bytes());
        body.extend_from_slice(&subject_offset.to_le_bytes());
        body.extend_from_slice(&1_u32.to_le_bytes());
        body.extend_from_slice(host);
        body.extend_from_slice(subject);

        let migration = MigrationBegin::decode(&body, true).expect("valid migration begin");
        assert_eq!(migration.destination.host, "target.example");
        assert_eq!(
            migration.destination.certificate_subject.as_deref(),
            Some("CN=target")
        );
        assert_eq!(migration.source_version, Some(1));

        body[host_offset as usize + host.len() - 1] = b'x';
        let error = MigrationBegin::decode(&body, true).expect_err("host must terminate");
        assert_eq!(error.kind, DecodeErrorKind::InvalidValue);
    }
}
