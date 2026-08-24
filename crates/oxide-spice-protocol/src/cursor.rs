use crate::wire::Reader;
use crate::{DecodeError, DecodeErrorKind};

/// Cursor server-to-client message identifiers.
pub mod cursor_server {
    pub const INIT: u16 = 101;
    pub const RESET: u16 = 102;
    pub const SET: u16 = 103;
    pub const MOVE: u16 = 104;
    pub const HIDE: u16 = 105;
    pub const TRAIL: u16 = 106;
    pub const INVALIDATE_ONE: u16 = 107;
    pub const INVALIDATE_ALL: u16 = 108;
}

const CURSOR_FLAG_NONE: u16 = 1 << 0;
const CURSOR_FLAG_CACHE_ME: u16 = 1 << 1;
const CURSOR_FLAG_FROM_CACHE: u16 = 1 << 2;
const KNOWN_CURSOR_FLAGS: u16 = CURSOR_FLAG_NONE | CURSOR_FLAG_CACHE_ME | CURSOR_FLAG_FROM_CACHE;

/// Signed cursor location relative to the virtual desktop.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CursorPosition {
    pub x: i16,
    pub y: i16,
}

/// Cursor wire encoding selected by the server.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum CursorType {
    Alpha = 0,
    Mono = 1,
    Color4 = 2,
    Color8 = 3,
    Color16 = 4,
    Color24 = 5,
    Color32 = 6,
}

impl TryFrom<u8> for CursorType {
    type Error = DecodeError;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(Self::Alpha),
            1 => Ok(Self::Mono),
            2 => Ok(Self::Color4),
            3 => Ok(Self::Color8),
            4 => Ok(Self::Color16),
            5 => Ok(Self::Color24),
            6 => Ok(Self::Color32),
            _ => Err(DecodeError::new(
                DecodeErrorKind::InvalidValue,
                0,
                "cursor type",
            )),
        }
    }
}

/// Header that identifies and sizes a cursor image.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CursorHeader {
    pub unique_id: u64,
    pub cursor_type: CursorType,
    pub width: u16,
    pub height: u16,
    pub hot_spot_x: u16,
    pub hot_spot_y: u16,
}

/// Borrowed cursor image reference decoded from one bounded message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CursorImage<'a> {
    None,
    Cached(u64),
    Data {
        header: CursorHeader,
        cache_me: bool,
        data: &'a [u8],
    },
}

/// Cursor Init payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CursorInit<'a> {
    pub position: CursorPosition,
    pub trail_length: u16,
    pub trail_frequency: u16,
    pub visible: bool,
    pub image: CursorImage<'a>,
}

impl<'a> CursorInit<'a> {
    /// Decodes Init and leaves image data borrowed from the channel read buffer.
    pub fn decode(body: &'a [u8]) -> Result<Self, DecodeError> {
        let mut reader = Reader::new(body);
        let position = read_position(&mut reader)?;
        let trail_length = reader.u16("cursor trail length")?;
        let trail_frequency = reader.u16("cursor trail frequency")?;
        let visible = read_bool(&mut reader, "cursor visible")?;
        let image = read_image(&mut reader, body)?;
        Ok(Self {
            position,
            trail_length,
            trail_frequency,
            visible,
            image,
        })
    }
}

/// Cursor Set payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CursorSet<'a> {
    pub position: CursorPosition,
    pub visible: bool,
    pub image: CursorImage<'a>,
}

impl<'a> CursorSet<'a> {
    /// Decodes Set and leaves image data borrowed from the channel read buffer.
    pub fn decode(body: &'a [u8]) -> Result<Self, DecodeError> {
        let mut reader = Reader::new(body);
        let position = read_position(&mut reader)?;
        let visible = read_bool(&mut reader, "cursor visible")?;
        let image = read_image(&mut reader, body)?;
        Ok(Self {
            position,
            visible,
            image,
        })
    }
}

/// Decodes the exact four-byte Cursor Move body.
pub fn decode_cursor_position(body: &[u8]) -> Result<CursorPosition, DecodeError> {
    let mut reader = exact_reader(body, 4, "cursor position")?;
    read_position(&mut reader)
}

/// Decodes the exact four-byte Cursor Trail body.
pub fn decode_cursor_trail(body: &[u8]) -> Result<(u16, u16), DecodeError> {
    let mut reader = exact_reader(body, 4, "cursor trail")?;
    Ok((
        reader.u16("cursor trail length")?,
        reader.u16("cursor trail frequency")?,
    ))
}

/// Decodes the exact eight-byte cursor cache invalidation body.
pub fn decode_cursor_cache_id(body: &[u8]) -> Result<u64, DecodeError> {
    let mut reader = exact_reader(body, 8, "cursor cache id")?;
    reader.u64("cursor cache id")
}

fn read_position(reader: &mut Reader<'_>) -> Result<CursorPosition, DecodeError> {
    Ok(CursorPosition {
        x: reader.u16("cursor x")? as i16,
        y: reader.u16("cursor y")? as i16,
    })
}

fn read_bool(reader: &mut Reader<'_>, context: &'static str) -> Result<bool, DecodeError> {
    match reader.u8(context)? {
        0 => Ok(false),
        1 => Ok(true),
        _ => Err(DecodeError::new(
            DecodeErrorKind::InvalidValue,
            reader.offset().saturating_sub(1),
            context,
        )),
    }
}

fn read_image<'a>(reader: &mut Reader<'a>, body: &'a [u8]) -> Result<CursorImage<'a>, DecodeError> {
    let flags = reader.u16("cursor flags")?;
    if flags & !KNOWN_CURSOR_FLAGS != 0
        || flags & CURSOR_FLAG_NONE != 0 && flags != CURSOR_FLAG_NONE
        || flags & CURSOR_FLAG_FROM_CACHE != 0 && flags & CURSOR_FLAG_CACHE_ME != 0
    {
        return Err(DecodeError::new(
            DecodeErrorKind::InvalidValue,
            reader.offset().saturating_sub(2),
            "cursor flags",
        ));
    }
    if flags == CURSOR_FLAG_NONE {
        if reader.remaining() != 0 {
            return Err(DecodeError::new(
                DecodeErrorKind::InvalidValue,
                reader.offset(),
                "none cursor data",
            ));
        }
        return Ok(CursorImage::None);
    }

    let unique_id = reader.u64("cursor unique id")?;
    if flags & CURSOR_FLAG_FROM_CACHE != 0 {
        reader.take(9, "cached cursor unused header fields")?;
        if reader.remaining() != 0 {
            return Err(DecodeError::new(
                DecodeErrorKind::InvalidValue,
                reader.offset(),
                "cached cursor data",
            ));
        }
        return Ok(CursorImage::Cached(unique_id));
    }
    let header = CursorHeader {
        unique_id,
        cursor_type: CursorType::try_from(reader.u8("cursor type")?)?,
        width: reader.u16("cursor width")?,
        height: reader.u16("cursor height")?,
        hot_spot_x: reader.u16("cursor hotspot x")?,
        hot_spot_y: reader.u16("cursor hotspot y")?,
    };
    let data_offset = reader.offset();
    let data = &body[data_offset..];
    if data.is_empty() {
        return Err(DecodeError::new(
            DecodeErrorKind::Truncated,
            data_offset,
            "cursor image data",
        ));
    }
    Ok(CursorImage::Data {
        header,
        cache_me: flags & CURSOR_FLAG_CACHE_ME != 0,
        data,
    })
}

fn exact_reader<'a>(
    body: &'a [u8],
    expected: usize,
    context: &'static str,
) -> Result<Reader<'a>, DecodeError> {
    if body.len() != expected {
        return Err(DecodeError::new(
            if body.len() < expected {
                DecodeErrorKind::Truncated
            } else {
                DecodeErrorKind::InvalidValue
            },
            body.len(),
            context,
        ));
    }
    Ok(Reader::new(body))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cursor_set_rejects_cached_image_with_trailing_data() {
        let mut body = vec![0; 5];
        body.extend_from_slice(&CURSOR_FLAG_FROM_CACHE.to_le_bytes());
        body.extend_from_slice(&7_u64.to_le_bytes());
        body.push(CursorType::Alpha as u8);
        body.extend_from_slice(&1_u16.to_le_bytes());
        body.extend_from_slice(&1_u16.to_le_bytes());
        body.extend_from_slice(&0_u16.to_le_bytes());
        body.extend_from_slice(&0_u16.to_le_bytes());
        body.push(0);

        let error = CursorSet::decode(&body).expect_err("cached cursor has no data");
        assert_eq!(error.kind, DecodeErrorKind::InvalidValue);
    }

    #[test]
    fn cursor_set_borrows_alpha_payload() {
        let mut body = vec![0; 5];
        body.extend_from_slice(&CURSOR_FLAG_CACHE_ME.to_le_bytes());
        body.extend_from_slice(&7_u64.to_le_bytes());
        body.push(CursorType::Alpha as u8);
        for field in [1_u16, 1, 0, 0] {
            body.extend_from_slice(&field.to_le_bytes());
        }
        body.extend_from_slice(&[1, 2, 3, 4]);

        let update = CursorSet::decode(&body).expect("valid cursor set");
        let CursorImage::Data { data, cache_me, .. } = update.image else {
            panic!("expected inline cursor image");
        };
        assert!(cache_me);
        assert_eq!(data, &[1, 2, 3, 4]);
    }
}
