use crate::display::{CompositeClip, CompositeImage, Rect, decode_display_clip, read_rect};
use crate::wire::{Reader, resolve_range};
use crate::{DecodeError, DecodeErrorKind};

/// Maximum path segments accepted from one Draw Stroke command.
pub const MAX_PATH_SEGMENTS: usize = 4_096;
/// Maximum fixed-point vertices accepted from one Draw Stroke command.
pub const MAX_PATH_POINTS: usize = 262_144;
/// Maximum raster glyphs accepted from one Draw Text command.
pub const MAX_RASTER_GLYPHS: usize = 4_096;
/// Maximum aggregate raster-glyph bytes accepted from one Draw Text command.
pub const MAX_RASTER_GLYPH_BYTES: usize = 16 * 1024 * 1024;

/// A signed 32-bit point used by the classic Display drawing commands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Point {
    pub x: i32,
    pub y: i32,
}

/// The destination envelope shared by classic Display drawing commands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DisplayBase {
    pub surface_id: u32,
    pub destination: Rect,
    pub clip: CompositeClip,
}

impl DisplayBase {
    fn decode(reader: &mut Reader<'_>) -> Result<Self, DecodeError> {
        let surface_id = reader.u32("Display destination surface")?;
        let destination = read_rect(reader, "Display destination rectangle")?;
        if destination.width()? == 0 || destination.height()? == 0 {
            return Err(DecodeError::new(
                DecodeErrorKind::InvalidValue,
                reader.offset().saturating_sub(16),
                "Display destination rectangle",
            ));
        }
        let clip = decode_display_clip(reader, "Display clip")?;
        Ok(Self {
            surface_id,
            destination,
            clip,
        })
    }
}

/// A classic Display brush resolved against the containing message body.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DisplayBrush {
    None,
    Solid(u32),
    Pattern {
        image: CompositeImage,
        position: Point,
    },
}

/// A bitmap mask positioned in destination surface coordinates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DisplayMask {
    pub inverted: bool,
    pub position: Point,
    pub image: Option<CompositeImage>,
}

impl DisplayMask {
    pub const NONE: Self = Self {
        inverted: false,
        position: Point { x: 0, y: 0 },
        image: None,
    };
}

/// An image and source rectangle with an explicitly validated scale mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DisplayImageSource {
    pub image: CompositeImage,
    pub area: Rect,
    pub scale_mode: u8,
}

/// A complete Draw Fill command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DrawFill {
    pub base: DisplayBase,
    pub brush: DisplayBrush,
    pub rop_descriptor: u16,
    pub mask: DisplayMask,
}

impl DrawFill {
    pub fn decode(body: &[u8]) -> Result<Self, DecodeError> {
        let mut reader = Reader::new(body);
        let base = DisplayBase::decode(&mut reader)?;
        let brush = decode_brush(body, &mut reader, "Fill brush")?;
        let rop_descriptor = reader.u16("Fill raster operation")?;
        let mask = decode_mask(body, &mut reader, "Fill mask")?;
        Ok(Self {
            base,
            brush,
            rop_descriptor,
            mask,
        })
    }
}

/// A complete Draw Copy command; Draw Blend has the same wire representation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DrawCopy {
    pub base: DisplayBase,
    pub source: DisplayImageSource,
    pub rop_descriptor: u16,
    pub mask: DisplayMask,
}

impl DrawCopy {
    pub fn decode(body: &[u8]) -> Result<Self, DecodeError> {
        let mut reader = Reader::new(body);
        let base = DisplayBase::decode(&mut reader)?;
        let image_offset = required_offset(&mut reader, "Copy source image")?;
        let area = read_valid_rect(&mut reader, "Copy source rectangle")?;
        let rop_descriptor = reader.u16("Copy raster operation")?;
        let scale_mode = decode_scale_mode(&mut reader, "Copy scale mode")?;
        let mask = decode_mask(body, &mut reader, "Copy mask")?;
        let image = CompositeImage::decode_at(body, image_offset)?;
        Ok(Self {
            base,
            source: DisplayImageSource {
                image,
                area,
                scale_mode,
            },
            rop_descriptor,
            mask,
        })
    }
}

/// Draw Blend intentionally reuses the protocol-identical Draw Copy body.
pub type DrawBlend = DrawCopy;

/// A complete Draw Opaque command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DrawOpaque {
    pub base: DisplayBase,
    pub source: DisplayImageSource,
    pub brush: DisplayBrush,
    pub rop_descriptor: u16,
    pub mask: DisplayMask,
}

impl DrawOpaque {
    pub fn decode(body: &[u8]) -> Result<Self, DecodeError> {
        let mut reader = Reader::new(body);
        let base = DisplayBase::decode(&mut reader)?;
        let image_offset = required_offset(&mut reader, "Opaque source image")?;
        let area = read_valid_rect(&mut reader, "Opaque source rectangle")?;
        let brush = decode_brush(body, &mut reader, "Opaque brush")?;
        let rop_descriptor = reader.u16("Opaque raster operation")?;
        let scale_mode = decode_scale_mode(&mut reader, "Opaque scale mode")?;
        let mask = decode_mask(body, &mut reader, "Opaque mask")?;
        let image = CompositeImage::decode_at(body, image_offset)?;
        Ok(Self {
            base,
            source: DisplayImageSource {
                image,
                area,
                scale_mode,
            },
            brush,
            rop_descriptor,
            mask,
        })
    }
}

/// The shared body used by Blackness, Whiteness, and Invers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DrawMaskedDestination {
    pub base: DisplayBase,
    pub mask: DisplayMask,
}

impl DrawMaskedDestination {
    pub fn decode(body: &[u8]) -> Result<Self, DecodeError> {
        let mut reader = Reader::new(body);
        let base = DisplayBase::decode(&mut reader)?;
        let mask = decode_mask(body, &mut reader, "Destination mask")?;
        Ok(Self { base, mask })
    }
}

/// A complete ternary raster-operation command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DrawRop3 {
    pub base: DisplayBase,
    pub source: DisplayImageSource,
    pub brush: DisplayBrush,
    pub rop3: u8,
    pub mask: DisplayMask,
}

impl DrawRop3 {
    pub fn decode(body: &[u8]) -> Result<Self, DecodeError> {
        let mut reader = Reader::new(body);
        let base = DisplayBase::decode(&mut reader)?;
        let image_offset = required_offset(&mut reader, "ROP3 source image")?;
        let area = read_valid_rect(&mut reader, "ROP3 source rectangle")?;
        let brush = decode_brush(body, &mut reader, "ROP3 brush")?;
        let rop3 = reader.u8("ROP3 truth table")?;
        let scale_mode = decode_scale_mode(&mut reader, "ROP3 scale mode")?;
        let mask = decode_mask(body, &mut reader, "ROP3 mask")?;
        let image = CompositeImage::decode_at(body, image_offset)?;
        Ok(Self {
            base,
            source: DisplayImageSource {
                image,
                area,
                scale_mode,
            },
            brush,
            rop3,
            mask,
        })
    }
}

/// One fixed28.4 point used by a stroke path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FixedPoint {
    pub x: i32,
    pub y: i32,
}

/// One path segment with validated flags and bounded point storage.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathSegment {
    pub flags: u8,
    pub points: Vec<FixedPoint>,
}

/// A complete bounded path referenced by Draw Stroke.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DisplayPath {
    pub segments: Vec<PathSegment>,
}

/// A solid or styled cosmetic line description.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LineAttributes {
    pub flags: u8,
    pub style: Vec<i32>,
}

/// A complete Draw Stroke command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DrawStroke {
    pub base: DisplayBase,
    pub path: DisplayPath,
    pub line: LineAttributes,
    pub brush: DisplayBrush,
    pub foreground_rop: u16,
    pub background_rop: u16,
}

impl DrawStroke {
    pub fn decode(body: &[u8]) -> Result<Self, DecodeError> {
        let mut reader = Reader::new(body);
        let base = DisplayBase::decode(&mut reader)?;
        let path_offset = required_offset(&mut reader, "Stroke path")?;
        let line = decode_line_attributes(body, &mut reader)?;
        let brush = decode_brush(body, &mut reader, "Stroke brush")?;
        let foreground_rop = reader.u16("Stroke foreground raster operation")?;
        let background_rop = reader.u16("Stroke background raster operation")?;
        let path = decode_path(body, path_offset)?;
        Ok(Self {
            base,
            path,
            line,
            brush,
            foreground_rop,
            background_rop,
        })
    }
}

/// Raster coverage format used by one Draw Text string.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GlyphFormat {
    Alpha1,
    Alpha4,
    Alpha8,
}

/// One bounded raster glyph borrowed from the current message body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RasterGlyph<'a> {
    pub render_position: Point,
    pub origin: Point,
    pub width: u16,
    pub height: u16,
    pub pixels: &'a [u8],
}

/// A complete bounded raster string.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RasterString<'a> {
    pub format: GlyphFormat,
    pub top_down: bool,
    pub glyphs: Vec<RasterGlyph<'a>>,
}

/// A complete Draw Text command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DrawText<'a> {
    pub base: DisplayBase,
    pub text: RasterString<'a>,
    pub background_area: Rect,
    pub foreground_brush: DisplayBrush,
    pub background_brush: DisplayBrush,
    pub foreground_rop: u16,
    pub background_rop: u16,
}

impl<'a> DrawText<'a> {
    pub fn decode(body: &'a [u8]) -> Result<Self, DecodeError> {
        let mut reader = Reader::new(body);
        let base = DisplayBase::decode(&mut reader)?;
        let string_offset = required_offset(&mut reader, "Text raster string")?;
        let background_area = read_valid_rect(&mut reader, "Text background rectangle")?;
        let foreground_brush = decode_brush(body, &mut reader, "Text foreground brush")?;
        let background_brush = decode_brush(body, &mut reader, "Text background brush")?;
        let foreground_rop = reader.u16("Text foreground raster operation")?;
        let background_rop = reader.u16("Text background raster operation")?;
        let text = decode_raster_string(body, string_offset)?;
        Ok(Self {
            base,
            text,
            background_area,
            foreground_brush,
            background_brush,
            foreground_rop,
            background_rop,
        })
    }
}

/// A color-keyed Draw Transparent command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DrawTransparent {
    pub base: DisplayBase,
    pub source_image: CompositeImage,
    pub source_area: Rect,
    pub source_color: u32,
    pub transparent_color: u32,
}

impl DrawTransparent {
    pub fn decode(body: &[u8]) -> Result<Self, DecodeError> {
        let mut reader = Reader::new(body);
        let base = DisplayBase::decode(&mut reader)?;
        let image_offset = required_offset(&mut reader, "Transparent source image")?;
        let source_area = read_valid_rect(&mut reader, "Transparent source rectangle")?;
        let source_color = reader.u32("Transparent source color")?;
        let transparent_color = reader.u32("Transparent true color")?;
        let source_image = CompositeImage::decode_at(body, image_offset)?;
        Ok(Self {
            base,
            source_image,
            source_area,
            source_color,
            transparent_color,
        })
    }
}

/// A global-alpha Draw Alpha Blend command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DrawAlphaBlend {
    pub base: DisplayBase,
    pub destination_has_alpha: bool,
    pub source_surface_has_alpha: bool,
    pub alpha: u8,
    pub source_image: CompositeImage,
    pub source_area: Rect,
}

impl DrawAlphaBlend {
    pub fn decode(body: &[u8]) -> Result<Self, DecodeError> {
        let mut reader = Reader::new(body);
        let base = DisplayBase::decode(&mut reader)?;
        let flags_offset = reader.offset();
        let flags = reader.u8("Alpha Blend flags")?;
        if flags & !0b11 != 0 {
            return Err(DecodeError::new(
                DecodeErrorKind::InvalidValue,
                flags_offset,
                "Alpha Blend flags",
            ));
        }
        let alpha = reader.u8("Alpha Blend global alpha")?;
        let image_offset = required_offset(&mut reader, "Alpha Blend source image")?;
        let source_area = read_valid_rect(&mut reader, "Alpha Blend source rectangle")?;
        let source_image = CompositeImage::decode_at(body, image_offset)?;
        Ok(Self {
            base,
            destination_has_alpha: flags & 1 != 0,
            source_surface_has_alpha: flags & 2 != 0,
            alpha,
            source_image,
            source_area,
        })
    }
}

fn decode_brush(
    body: &[u8],
    reader: &mut Reader<'_>,
    context: &'static str,
) -> Result<DisplayBrush, DecodeError> {
    match reader.u8(context)? {
        0 => Ok(DisplayBrush::None),
        1 => Ok(DisplayBrush::Solid(reader.u32(context)?)),
        2 => {
            let image_offset = required_offset(reader, context)?;
            let position = read_point(reader, context)?;
            Ok(DisplayBrush::Pattern {
                image: CompositeImage::decode_at(body, image_offset)?,
                position,
            })
        }
        _ => Err(DecodeError::new(
            DecodeErrorKind::Unsupported,
            reader.offset() - 1,
            context,
        )),
    }
}

fn decode_mask(
    body: &[u8],
    reader: &mut Reader<'_>,
    context: &'static str,
) -> Result<DisplayMask, DecodeError> {
    let flags_offset = reader.offset();
    let flags = reader.u8(context)?;
    if flags & !1 != 0 {
        return Err(DecodeError::new(
            DecodeErrorKind::InvalidValue,
            flags_offset,
            context,
        ));
    }
    let position = read_point(reader, context)?;
    let image_offset = reader.u32(context)?;
    let image = (image_offset != 0)
        .then(|| CompositeImage::decode_at(body, image_offset))
        .transpose()?;
    Ok(DisplayMask {
        inverted: flags & 1 != 0,
        position,
        image,
    })
}

fn decode_scale_mode(reader: &mut Reader<'_>, context: &'static str) -> Result<u8, DecodeError> {
    let offset = reader.offset();
    let mode = reader.u8(context)?;
    if mode > 1 {
        return Err(DecodeError::new(
            DecodeErrorKind::InvalidValue,
            offset,
            context,
        ));
    }
    Ok(mode)
}

fn decode_line_attributes(
    body: &[u8],
    reader: &mut Reader<'_>,
) -> Result<LineAttributes, DecodeError> {
    const START_WITH_GAP: u8 = 1 << 2;
    const STYLED: u8 = 1 << 3;
    let flags_offset = reader.offset();
    let flags = reader.u8("Stroke line flags")?;
    if flags & !(START_WITH_GAP | STYLED) != 0 {
        return Err(DecodeError::new(
            DecodeErrorKind::InvalidValue,
            flags_offset,
            "Stroke line flags",
        ));
    }
    if flags & STYLED == 0 {
        return Ok(LineAttributes {
            flags,
            style: Vec::new(),
        });
    }
    let count = usize::from(reader.u8("Stroke style segment count")?);
    if count == 0 {
        return Err(DecodeError::new(
            DecodeErrorKind::InvalidValue,
            reader.offset() - 1,
            "Stroke style segment count",
        ));
    }
    let style_offset = required_offset(reader, "Stroke style array")?;
    let style_bytes = count.checked_mul(4).ok_or_else(|| {
        DecodeError::new(
            DecodeErrorKind::Overflow,
            reader.offset(),
            "Stroke style array",
        )
    })?;
    let range = resolve_range(body, style_offset, style_bytes, "Stroke style array")?;
    let mut style_reader = Reader::new(&body[range]);
    let mut style = Vec::with_capacity(count);
    for _ in 0..count {
        style.push(style_reader.i32("Stroke style segment")?);
    }
    Ok(LineAttributes { flags, style })
}

fn decode_path(body: &[u8], path_offset: u32) -> Result<DisplayPath, DecodeError> {
    let start = resolve_range(body, path_offset, 4, "Stroke path")?.start;
    let mut reader = Reader::new(&body[start..]);
    let segment_count_offset = start;
    let segment_count =
        usize::try_from(reader.u32("Stroke path segment count")?).map_err(|_| {
            DecodeError::new(
                DecodeErrorKind::Overflow,
                segment_count_offset,
                "Stroke path segment count",
            )
        })?;
    if segment_count > MAX_PATH_SEGMENTS {
        return Err(DecodeError::new(
            DecodeErrorKind::ResourceLimit,
            segment_count_offset,
            "Stroke path segment count",
        ));
    }
    let mut total_points = 0_usize;
    let mut segments = Vec::with_capacity(segment_count);
    for _ in 0..segment_count {
        let flags_offset = start + reader.offset();
        let flags = reader.u8("Stroke path flags")?;
        if flags & !0x1b != 0 {
            return Err(DecodeError::new(
                DecodeErrorKind::InvalidValue,
                flags_offset,
                "Stroke path flags",
            ));
        }
        let count_offset = start + reader.offset();
        let point_count =
            usize::try_from(reader.u32("Stroke path point count")?).map_err(|_| {
                DecodeError::new(
                    DecodeErrorKind::Overflow,
                    count_offset,
                    "Stroke path point count",
                )
            })?;
        total_points = total_points.checked_add(point_count).ok_or_else(|| {
            DecodeError::new(
                DecodeErrorKind::Overflow,
                count_offset,
                "Stroke path point count",
            )
        })?;
        if total_points > MAX_PATH_POINTS {
            return Err(DecodeError::new(
                DecodeErrorKind::ResourceLimit,
                count_offset,
                "Stroke path point count",
            ));
        }
        let mut points = Vec::with_capacity(point_count);
        for _ in 0..point_count {
            points.push(FixedPoint {
                x: reader.i32("Stroke path point x")?,
                y: reader.i32("Stroke path point y")?,
            });
        }
        segments.push(PathSegment { flags, points });
    }
    Ok(DisplayPath { segments })
}

fn decode_raster_string<'a>(
    body: &'a [u8],
    string_offset: u32,
) -> Result<RasterString<'a>, DecodeError> {
    let start = resolve_range(body, string_offset, 3, "Text raster string")?.start;
    let mut reader = Reader::new(&body[start..]);
    let glyph_count = usize::from(reader.u16("Text glyph count")?);
    if glyph_count > MAX_RASTER_GLYPHS {
        return Err(DecodeError::new(
            DecodeErrorKind::ResourceLimit,
            start,
            "Text glyph count",
        ));
    }
    let flags_offset = start + reader.offset();
    let flags = reader.u8("Text string flags")?;
    if flags & !0x0f != 0 || (flags & 0x07).count_ones() != 1 {
        return Err(DecodeError::new(
            DecodeErrorKind::InvalidValue,
            flags_offset,
            "Text string flags",
        ));
    }
    let (format, bits_per_pixel) = match flags & 0x07 {
        1 => (GlyphFormat::Alpha1, 1_usize),
        2 => (GlyphFormat::Alpha4, 4_usize),
        4 => (GlyphFormat::Alpha8, 8_usize),
        _ => unreachable!("exactly one raster format bit was validated"),
    };
    let mut aggregate_bytes = 0_usize;
    let mut glyphs = Vec::with_capacity(glyph_count);
    for _ in 0..glyph_count {
        let render_position = read_point(&mut reader, "Text glyph render position")?;
        let origin = read_point(&mut reader, "Text glyph origin")?;
        let width = reader.u16("Text glyph width")?;
        let height = reader.u16("Text glyph height")?;
        let row_bits = usize::from(width)
            .checked_mul(bits_per_pixel)
            .ok_or_else(|| {
                DecodeError::new(
                    DecodeErrorKind::Overflow,
                    start + reader.offset(),
                    "Text glyph row",
                )
            })?;
        let row_bytes = row_bits
            .checked_add(7)
            .map(|bits| bits / 8)
            .ok_or_else(|| {
                DecodeError::new(
                    DecodeErrorKind::Overflow,
                    start + reader.offset(),
                    "Text glyph row",
                )
            })?;
        let pixel_bytes = row_bytes.checked_mul(usize::from(height)).ok_or_else(|| {
            DecodeError::new(
                DecodeErrorKind::Overflow,
                start + reader.offset(),
                "Text glyph pixels",
            )
        })?;
        aggregate_bytes = aggregate_bytes.checked_add(pixel_bytes).ok_or_else(|| {
            DecodeError::new(
                DecodeErrorKind::Overflow,
                start + reader.offset(),
                "Text glyph pixels",
            )
        })?;
        if aggregate_bytes > MAX_RASTER_GLYPH_BYTES {
            return Err(DecodeError::new(
                DecodeErrorKind::ResourceLimit,
                start + reader.offset(),
                "Text glyph pixels",
            ));
        }
        let pixels = reader.take(pixel_bytes, "Text glyph pixels")?;
        glyphs.push(RasterGlyph {
            render_position,
            origin,
            width,
            height,
            pixels,
        });
    }
    Ok(RasterString {
        format,
        top_down: flags & 0x08 != 0,
        glyphs,
    })
}

fn read_point(reader: &mut Reader<'_>, context: &'static str) -> Result<Point, DecodeError> {
    Ok(Point {
        x: reader.i32(context)?,
        y: reader.i32(context)?,
    })
}

fn read_valid_rect(reader: &mut Reader<'_>, context: &'static str) -> Result<Rect, DecodeError> {
    let rectangle = read_rect(reader, context)?;
    if rectangle.width()? == 0 || rectangle.height()? == 0 {
        return Err(DecodeError::new(
            DecodeErrorKind::InvalidValue,
            reader.offset().saturating_sub(16),
            context,
        ));
    }
    Ok(rectangle)
}

fn required_offset(reader: &mut Reader<'_>, context: &'static str) -> Result<u32, DecodeError> {
    let offset_position = reader.offset();
    let offset = reader.u32(context)?;
    if offset == 0 {
        return Err(DecodeError::new(
            DecodeErrorKind::InvalidOffset,
            offset_position,
            context,
        ));
    }
    Ok(offset)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn append_rect(body: &mut Vec<u8>, rectangle: Rect) {
        body.extend_from_slice(&rectangle.top.to_le_bytes());
        body.extend_from_slice(&rectangle.left.to_le_bytes());
        body.extend_from_slice(&rectangle.bottom.to_le_bytes());
        body.extend_from_slice(&rectangle.right.to_le_bytes());
    }

    #[test]
    fn display_base_decodes_inline_clip_rectangles() {
        let mut body = 7_u32.to_le_bytes().to_vec();
        append_rect(
            &mut body,
            Rect {
                top: 1,
                left: 2,
                bottom: 9,
                right: 10,
            },
        );
        body.push(1);
        body.extend_from_slice(&1_u32.to_le_bytes());
        append_rect(
            &mut body,
            Rect {
                top: 3,
                left: 4,
                bottom: 7,
                right: 8,
            },
        );
        let base = DisplayBase::decode(&mut Reader::new(&body)).expect("valid Display base");
        assert_eq!(base.surface_id, 7);
        assert_eq!(
            base.clip,
            CompositeClip::Rectangles(vec![Rect {
                top: 3,
                left: 4,
                bottom: 7,
                right: 8,
            }])
        );
    }

    #[test]
    fn raster_string_rejects_ambiguous_format_flags() {
        let body = [0_u8, 0, 0b11];
        let error = decode_raster_string(&body, 0).expect_err("two raster formats must fail");
        assert_eq!(error.kind, DecodeErrorKind::InvalidValue);
    }

    #[test]
    fn path_point_limit_is_checked_before_allocation() {
        let mut body = 1_u32.to_le_bytes().to_vec();
        body.push(1);
        body.extend_from_slice(
            &u32::try_from(MAX_PATH_POINTS + 1)
                .expect("test limit fits u32")
                .to_le_bytes(),
        );
        let error = decode_path(&body, 0).expect_err("oversized path must fail");
        assert_eq!(error.kind, DecodeErrorKind::ResourceLimit);
    }

    #[test]
    fn styled_line_resolves_bounded_fixed_point_array() {
        let mut envelope = vec![1 << 3, 2];
        envelope.extend_from_slice(&6_u32.to_le_bytes());
        envelope.extend_from_slice(&16_i32.to_le_bytes());
        envelope.extend_from_slice(&32_i32.to_le_bytes());
        let mut reader = Reader::new(&envelope);
        let line = decode_line_attributes(&envelope, &mut reader).expect("valid line style");
        assert_eq!(line.style, [16, 32]);
    }
}
