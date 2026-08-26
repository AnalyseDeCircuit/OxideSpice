use crate::wire::{Reader, resolve_range};
use crate::{DecodeError, DecodeErrorKind};

/// Maximum monitor heads accepted from one Display message.
pub const MAX_MONITOR_HEADS: usize = 64;
pub const MAX_STREAM_CLIP_RECTS: usize = 256;
pub const MAX_STREAM_FRAME_BYTES: usize = 16 * 1024 * 1024;
pub const MAX_COMPOSITE_CLIP_RECTS: usize = 256;
pub const MAX_GL_SCANOUT_PLANES: usize = 4;
pub const MAX_INVALIDATE_RESOURCES: usize = 4_096;

/// Display server-to-client message identifiers.
pub mod display_server {
    pub const MODE: u16 = 101;
    pub const MARK: u16 = 102;
    pub const RESET: u16 = 103;
    pub const COPY_BITS: u16 = 104;
    pub const INVALIDATE_LIST: u16 = 105;
    pub const INVALIDATE_ALL_PIXMAPS: u16 = 106;
    pub const INVALIDATE_PALETTE: u16 = 107;
    pub const INVALIDATE_ALL_PALETTES: u16 = 108;
    pub const STREAM_CREATE: u16 = 122;
    pub const STREAM_DATA: u16 = 123;
    pub const STREAM_CLIP: u16 = 124;
    pub const STREAM_DESTROY: u16 = 125;
    pub const STREAM_DESTROY_ALL: u16 = 126;
    pub const DRAW_FILL: u16 = 302;
    pub const DRAW_OPAQUE: u16 = 303;
    pub const DRAW_COPY: u16 = 304;
    pub const DRAW_BLEND: u16 = 305;
    pub const DRAW_BLACKNESS: u16 = 306;
    pub const DRAW_WHITENESS: u16 = 307;
    pub const DRAW_INVERS: u16 = 308;
    pub const DRAW_ROP3: u16 = 309;
    pub const DRAW_STROKE: u16 = 310;
    pub const DRAW_TEXT: u16 = 311;
    pub const DRAW_TRANSPARENT: u16 = 312;
    pub const DRAW_ALPHA_BLEND: u16 = 313;
    pub const SURFACE_CREATE: u16 = 314;
    pub const SURFACE_DESTROY: u16 = 315;
    pub const STREAM_DATA_SIZED: u16 = 316;
    pub const MONITORS_CONFIG: u16 = 317;
    pub const DRAW_COMPOSITE: u16 = 318;
    pub const STREAM_ACTIVATE_REPORT: u16 = 319;
    pub const GL_SCANOUT_UNIX: u16 = 320;
    pub const GL_DRAW: u16 = 321;
    pub const GL_SCANOUT2_UNIX: u16 = 323;
}

/// Video codec identifiers carried by Display stream creation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum VideoCodec {
    Mjpeg = 1,
    Vp8 = 2,
    H264 = 3,
    Vp9 = 4,
    H265 = 5,
}

impl TryFrom<u8> for VideoCodec {
    type Error = DecodeError;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            1 => Ok(Self::Mjpeg),
            2 => Ok(Self::Vp8),
            3 => Ok(Self::H264),
            4 => Ok(Self::Vp9),
            5 => Ok(Self::H265),
            _ => Err(DecodeError::new(
                DecodeErrorKind::Unsupported,
                9,
                "Display stream codec",
            )),
        }
    }
}

/// Encodes an ordered, duplicate-free list of preferred negotiated codecs.
pub fn encode_preferred_video_codecs(codecs: &[VideoCodec]) -> Result<Vec<u8>, DecodeError> {
    if codecs.is_empty() || codecs.len() > 5 {
        return Err(DecodeError::new(
            if codecs.is_empty() {
                DecodeErrorKind::InvalidValue
            } else {
                DecodeErrorKind::ResourceLimit
            },
            0,
            "preferred Display video codecs",
        ));
    }
    if codecs
        .iter()
        .enumerate()
        .any(|(index, codec)| codecs[..index].contains(codec))
    {
        return Err(DecodeError::new(
            DecodeErrorKind::InvalidValue,
            0,
            "duplicate preferred Display video codec",
        ));
    }
    let mut output = Vec::with_capacity(codecs.len() + 1);
    output.push(u8::try_from(codecs.len()).expect("five video codecs fit one byte"));
    output.extend(codecs.iter().map(|codec| *codec as u8));
    Ok(output)
}

/// Display client-to-server message identifiers.
pub mod display_client {
    pub const INIT: u16 = 101;
    pub const STREAM_REPORT: u16 = 102;
    pub const PREFERRED_COMPRESSION: u16 = 103;
    pub const GL_DRAW_DONE: u16 = 104;
    pub const PREFERRED_VIDEO_CODEC: u16 = 105;
}

/// Server-side image compression policy requested by a Display client.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum ImageCompression {
    Off = 1,
    AutoGlz = 2,
    AutoLz = 3,
    Quic = 4,
    Glz = 5,
    Lz = 6,
    Lz4 = 7,
}

impl ImageCompression {
    /// Encodes the one-byte Preferred Compression message body.
    pub const fn encode(self) -> [u8; 1] {
        [self as u8]
    }
}

/// One monitor head mapped to a rectangular region of a Display surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MonitorHead {
    pub monitor_id: u32,
    pub surface_id: u32,
    pub width: u32,
    pub height: u32,
    pub x: u32,
    pub y: u32,
    pub flags: u32,
}

/// Complete monitor topology for one Display channel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MonitorsConfig {
    pub maximum_allowed: u16,
    pub heads: Vec<MonitorHead>,
}

/// One pixmap identity carried by Display Invalidate List.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InvalidateResource {
    pub id: u64,
}

/// A bounded exact resource invalidation list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvalidateList {
    pub resources: Vec<InvalidateResource>,
}

impl InvalidateList {
    pub fn decode(body: &[u8]) -> Result<Self, DecodeError> {
        let mut reader = Reader::new(body);
        let count = usize::from(reader.u16("Display invalidation resource count")?);
        if count > MAX_INVALIDATE_RESOURCES {
            return Err(DecodeError::new(
                DecodeErrorKind::ResourceLimit,
                0,
                "Display invalidation resource count",
            ));
        }
        let expected = count.checked_mul(9).ok_or_else(|| {
            DecodeError::new(
                DecodeErrorKind::Overflow,
                reader.offset(),
                "Display invalidation resources",
            )
        })?;
        if reader.remaining() != expected {
            return Err(DecodeError::new(
                if reader.remaining() < expected {
                    DecodeErrorKind::Truncated
                } else {
                    DecodeErrorKind::InvalidValue
                },
                reader.offset(),
                "Display invalidation resources",
            ));
        }
        let mut resources = Vec::with_capacity(count);
        for _ in 0..count {
            if reader.u8("Display invalidation resource type")? != 1 {
                return Err(DecodeError::new(
                    DecodeErrorKind::Unsupported,
                    reader.offset() - 1,
                    "Display invalidation resource type",
                ));
            }
            resources.push(InvalidateResource {
                id: reader.u64("Display invalidation resource id")?,
            });
        }
        Ok(Self { resources })
    }
}

impl MonitorsConfig {
    /// Decodes a count, server limit, and exact packed array of 28-byte heads.
    pub fn decode(body: &[u8]) -> Result<Self, DecodeError> {
        let mut reader = Reader::new(body);
        let count = usize::from(reader.u16("monitor head count")?);
        let maximum_allowed = reader.u16("maximum monitor heads")?;
        if count > MAX_MONITOR_HEADS || count > usize::from(maximum_allowed) {
            return Err(DecodeError::new(
                DecodeErrorKind::ResourceLimit,
                0,
                "monitor head count",
            ));
        }
        let head_bytes = count.checked_mul(28).ok_or_else(|| {
            DecodeError::new(DecodeErrorKind::Overflow, reader.offset(), "monitor heads")
        })?;
        if reader.remaining() != head_bytes {
            return Err(DecodeError::new(
                if reader.remaining() < head_bytes {
                    DecodeErrorKind::Truncated
                } else {
                    DecodeErrorKind::InvalidValue
                },
                reader.offset(),
                "monitor heads",
            ));
        }
        let mut heads = Vec::with_capacity(count);
        for _ in 0..count {
            let head = MonitorHead {
                monitor_id: reader.u32("monitor id")?,
                surface_id: reader.u32("monitor surface id")?,
                width: reader.u32("monitor width")?,
                height: reader.u32("monitor height")?,
                x: reader.u32("monitor x")?,
                y: reader.u32("monitor y")?,
                flags: reader.u32("monitor flags")?,
            };
            if head.width == 0
                || head.height == 0
                || head.x.checked_add(head.width).is_none()
                || head.y.checked_add(head.height).is_none()
                || heads
                    .iter()
                    .any(|existing: &MonitorHead| existing.monitor_id == head.monitor_id)
            {
                return Err(DecodeError::new(
                    DecodeErrorKind::InvalidValue,
                    reader.offset().saturating_sub(28),
                    "monitor head geometry or identity",
                ));
            }
            heads.push(head);
        }
        Ok(Self {
            maximum_allowed,
            heads,
        })
    }
}

/// A signed SPICE rectangle in top, left, bottom, right order on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    pub top: i32,
    pub left: i32,
    pub bottom: i32,
    pub right: i32,
}

/// A signed 16-bit point used for Composite sampling origins.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Point16 {
    pub x: i16,
    pub y: i16,
}

/// A six-element affine 16.16 transform with an implicit `[0, 0, 1]` row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CompositeTransform {
    pub xx: i32,
    pub xy: i32,
    pub x0: i32,
    pub yx: i32,
    pub yy: i32,
    pub y0: i32,
}

/// A Display clip attached to one Composite operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CompositeClip {
    None,
    Rectangles(Vec<Rect>),
}

/// A source surface referenced by a Composite image descriptor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CompositeSurface {
    pub image_id: u64,
    pub image_flags: u8,
    pub width: u32,
    pub height: u32,
    pub surface_id: u32,
}

/// An embedded image descriptor whose payload begins inside the current message body.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CompositeEmbeddedImage {
    pub image_offset: u32,
    pub image_id: u64,
    pub image_flags: u8,
    pub width: u32,
    pub height: u32,
    pub image_type: DrawCopyImageType,
}

/// Either a live Display surface or an embedded raster used by Composite.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompositeImage {
    Surface(CompositeSurface),
    Embedded(CompositeEmbeddedImage),
}

impl CompositeImage {
    pub const fn width(self) -> u32 {
        match self {
            Self::Surface(image) => image.width,
            Self::Embedded(image) => image.width,
        }
    }

    pub const fn height(self) -> u32 {
        match self {
            Self::Surface(image) => image.height,
            Self::Embedded(image) => image.height,
        }
    }

    /// Resolves one generic Display image pointer against its containing message body.
    pub fn decode_at(body: &[u8], image_offset: u32) -> Result<Self, DecodeError> {
        decode_composite_image(body, image_offset, "Display image")
    }
}

/// A validated Draw Composite command using server-owned Display surfaces.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DrawComposite {
    pub destination_surface_id: u32,
    pub destination: Rect,
    pub clip: CompositeClip,
    pub operation: u8,
    pub source_filter: u8,
    pub mask_filter: u8,
    pub source_repeat: u8,
    pub mask_repeat: u8,
    pub component_alpha: bool,
    pub source_opaque: bool,
    pub mask_opaque: bool,
    pub destination_opaque: bool,
    pub source: CompositeImage,
    pub mask: Option<CompositeImage>,
    pub source_transform: Option<CompositeTransform>,
    pub mask_transform: Option<CompositeTransform>,
    pub source_origin: Point16,
    pub mask_origin: Point16,
}

impl DrawComposite {
    /// Decodes Composite flags and resolves every relative pointer before rendering.
    pub fn decode(body: &[u8]) -> Result<Self, DecodeError> {
        const HAS_MASK: u32 = 1 << 19;
        const HAS_SOURCE_TRANSFORM: u32 = 1 << 20;
        const HAS_MASK_TRANSFORM: u32 = 1 << 21;
        const SOURCE_OPAQUE: u32 = 1 << 22;
        const MASK_OPAQUE: u32 = 1 << 23;
        const DESTINATION_OPAQUE: u32 = 1 << 24;
        const KNOWN_FLAGS: u32 = (1 << 25) - 1;

        let mut reader = Reader::new(body);
        let destination_surface_id = reader.u32("Composite destination surface")?;
        let destination = read_rect(&mut reader, "Composite destination")?;
        let clip = decode_display_clip(&mut reader, "Composite clip")?;
        let flags = reader.u32("Composite flags")?;
        if flags & !KNOWN_FLAGS != 0 {
            return Err(DecodeError::new(
                DecodeErrorKind::Unsupported,
                reader.offset() - 4,
                "Composite flags",
            ));
        }
        let operation = (flags & 0xff) as u8;
        let source_filter = ((flags >> 8) & 0x7) as u8;
        let mask_filter = ((flags >> 11) & 0x7) as u8;
        let source_repeat = ((flags >> 14) & 0x3) as u8;
        let mask_repeat = ((flags >> 16) & 0x3) as u8;
        let component_alpha = flags & (1 << 18) != 0;
        let source_offset = reader.u32("Composite source image offset")?;
        let mask_offset = if flags & HAS_MASK != 0 {
            Some(reader.u32("Composite mask image offset")?)
        } else {
            None
        };
        let source_transform = if flags & HAS_SOURCE_TRANSFORM != 0 {
            Some(read_composite_transform(&mut reader)?)
        } else {
            None
        };
        let mask_transform = if flags & HAS_MASK_TRANSFORM != 0 {
            Some(read_composite_transform(&mut reader)?)
        } else {
            None
        };
        let source_origin = Point16 {
            x: reader.i16("Composite source origin x")?,
            y: reader.i16("Composite source origin y")?,
        };
        let mask_origin = Point16 {
            x: reader.i16("Composite mask origin x")?,
            y: reader.i16("Composite mask origin y")?,
        };
        let _ = destination.width()?;
        let _ = destination.height()?;
        let source = decode_composite_image(body, source_offset, "Composite source image")?;
        let mask = mask_offset
            .map(|offset| decode_composite_image(body, offset, "Composite mask image"))
            .transpose()?;
        Ok(Self {
            destination_surface_id,
            destination,
            clip,
            operation,
            source_filter,
            mask_filter,
            source_repeat,
            mask_repeat,
            component_alpha,
            source_opaque: flags & SOURCE_OPAQUE != 0,
            mask_opaque: flags & MASK_OPAQUE != 0,
            destination_opaque: flags & DESTINATION_OPAQUE != 0,
            source,
            mask,
            source_transform,
            mask_transform,
            source_origin,
            mask_origin,
        })
    }
}

pub(crate) fn decode_display_clip(
    reader: &mut Reader<'_>,
    context: &'static str,
) -> Result<CompositeClip, DecodeError> {
    match reader.u8(context)? {
        0 => Ok(CompositeClip::None),
        1 => {
            let count_offset = reader.offset();
            let count = usize::try_from(reader.u32(context)?)
                .map_err(|_| DecodeError::new(DecodeErrorKind::Overflow, count_offset, context))?;
            if count > MAX_COMPOSITE_CLIP_RECTS {
                return Err(DecodeError::new(
                    DecodeErrorKind::ResourceLimit,
                    count_offset,
                    context,
                ));
            }
            let rectangle_bytes = count.checked_mul(16).ok_or_else(|| {
                DecodeError::new(DecodeErrorKind::Overflow, reader.offset(), context)
            })?;
            let mut rectangles_reader = Reader::new(reader.take(rectangle_bytes, context)?);
            let mut rectangles = Vec::with_capacity(count);
            for _ in 0..count {
                let rectangle = read_rect(&mut rectangles_reader, context)?;
                let _ = rectangle.width()?;
                let _ = rectangle.height()?;
                rectangles.push(rectangle);
            }
            Ok(CompositeClip::Rectangles(rectangles))
        }
        _ => Err(DecodeError::new(
            DecodeErrorKind::Unsupported,
            reader.offset() - 1,
            context,
        )),
    }
}

fn read_composite_transform(reader: &mut Reader<'_>) -> Result<CompositeTransform, DecodeError> {
    Ok(CompositeTransform {
        xx: reader.i32("Composite transform xx")?,
        xy: reader.i32("Composite transform xy")?,
        x0: reader.i32("Composite transform x0")?,
        yx: reader.i32("Composite transform yx")?,
        yy: reader.i32("Composite transform yy")?,
        y0: reader.i32("Composite transform y0")?,
    })
}

fn decode_composite_image(
    body: &[u8],
    image_offset: u32,
    context: &'static str,
) -> Result<CompositeImage, DecodeError> {
    const IMAGE_DESCRIPTOR_BYTES: usize = 18;
    const IMAGE_TYPE_SURFACE: u8 = 104;
    let descriptor_range = resolve_range(body, image_offset, IMAGE_DESCRIPTOR_BYTES, context)?;
    let mut descriptor = Reader::new(&body[descriptor_range.clone()]);
    let image_id = descriptor.u64("Composite image id")?;
    let image_type = descriptor.u8("Composite image type")?;
    let image_flags = descriptor.u8("Composite image flags")?;
    let width = descriptor.u32("Composite image width")?;
    let height = descriptor.u32("Composite image height")?;
    if width == 0 || height == 0 {
        return Err(DecodeError::new(
            DecodeErrorKind::InvalidValue,
            descriptor_range.start + 10,
            "Composite image dimensions",
        ));
    }
    if image_flags & !0b111 != 0 {
        return Err(DecodeError::new(
            DecodeErrorKind::InvalidValue,
            descriptor_range.start + 9,
            "Composite image flags",
        ));
    }
    if image_flags & 0b101 != 0 {
        return Err(DecodeError::new(
            DecodeErrorKind::Unsupported,
            descriptor_range.start + 9,
            "disabled Composite image cache",
        ));
    }
    if image_type != IMAGE_TYPE_SURFACE {
        let image_type = decode_embedded_image_type(image_type, descriptor_range.start + 8)?;
        return Ok(CompositeImage::Embedded(CompositeEmbeddedImage {
            image_offset,
            image_id,
            image_flags,
            width,
            height,
            image_type,
        }));
    }
    let payload_offset = image_offset
        .checked_add(IMAGE_DESCRIPTOR_BYTES as u32)
        .ok_or_else(|| {
            DecodeError::new(
                DecodeErrorKind::Overflow,
                descriptor_range.end,
                "Composite surface payload",
            )
        })?;
    let payload_range = resolve_range(body, payload_offset, 4, "Composite surface payload")?;
    let mut payload = Reader::new(&body[payload_range]);
    Ok(CompositeImage::Surface(CompositeSurface {
        image_id,
        image_flags,
        width,
        height,
        surface_id: payload.u32("Composite source surface id")?,
    }))
}

fn decode_embedded_image_type(value: u8, offset: usize) -> Result<DrawCopyImageType, DecodeError> {
    match value {
        0 => Ok(DrawCopyImageType::Bitmap),
        1 => Ok(DrawCopyImageType::Quic),
        100 => Ok(DrawCopyImageType::LzPalette),
        101 => Ok(DrawCopyImageType::LzRgb),
        102 => Ok(DrawCopyImageType::GlzRgb),
        105 => Ok(DrawCopyImageType::Jpeg),
        107 => Ok(DrawCopyImageType::ZlibGlzRgb),
        108 => Ok(DrawCopyImageType::JpegAlpha),
        109 => Ok(DrawCopyImageType::Lz4),
        103 | 106 => Err(DecodeError::new(
            DecodeErrorKind::Unsupported,
            offset,
            "disabled image cache reference",
        )),
        _ => Err(DecodeError::new(
            DecodeErrorKind::Unsupported,
            offset,
            "Composite image type",
        )),
    }
}

impl Rect {
    /// Validates ordering and returns the non-negative width.
    pub fn width(self) -> Result<u32, DecodeError> {
        let width = self
            .right
            .checked_sub(self.left)
            .ok_or_else(|| DecodeError::new(DecodeErrorKind::Overflow, 0, "rectangle width"))?;
        u32::try_from(width)
            .map_err(|_| DecodeError::new(DecodeErrorKind::InvalidValue, 0, "rectangle width"))
    }

    /// Validates ordering and returns the non-negative height.
    pub fn height(self) -> Result<u32, DecodeError> {
        let height = self
            .bottom
            .checked_sub(self.top)
            .ok_or_else(|| DecodeError::new(DecodeErrorKind::Overflow, 0, "rectangle height"))?;
        u32::try_from(height)
            .map_err(|_| DecodeError::new(DecodeErrorKind::InvalidValue, 0, "rectangle height"))
    }
}

/// Clip region attached to one Display stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StreamClip {
    None,
    Rectangles(Vec<Rect>),
}

/// Replacement clip region for an active Display stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamClipUpdate {
    pub stream_id: u32,
    pub clip: StreamClip,
}

impl StreamClipUpdate {
    pub fn decode(body: &[u8]) -> Result<Self, DecodeError> {
        let mut reader = Reader::new(body);
        let stream_id = reader.u32("Display stream clip id")?;
        let clip = decode_stream_clip(&mut reader)?;
        if reader.remaining() != 0 {
            return Err(DecodeError::new(
                DecodeErrorKind::InvalidValue,
                reader.offset(),
                "Display stream clip trailing data",
            ));
        }
        Ok(Self { stream_id, clip })
    }
}

/// Immutable stream geometry and codec selected by the server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamCreate {
    pub surface_id: u32,
    pub stream_id: u32,
    pub top_down: bool,
    pub codec: VideoCodec,
    pub stamp: u64,
    pub stream_width: u32,
    pub stream_height: u32,
    pub source_width: u32,
    pub source_height: u32,
    pub destination: Rect,
    pub clip: StreamClip,
}

impl StreamCreate {
    pub fn decode(body: &[u8]) -> Result<Self, DecodeError> {
        let mut reader = Reader::new(body);
        let surface_id = reader.u32("Display stream surface id")?;
        let stream_id = reader.u32("Display stream id")?;
        let flags = reader.u8("Display stream flags")?;
        if flags & !1 != 0 {
            return Err(DecodeError::new(
                DecodeErrorKind::Unsupported,
                8,
                "Display stream flags",
            ));
        }
        let codec = VideoCodec::try_from(reader.u8("Display stream codec")?)?;
        let stamp = reader.u64("Display stream stamp")?;
        let stream_width = reader.u32("Display stream width")?;
        let stream_height = reader.u32("Display stream height")?;
        let source_width = reader.u32("Display stream source width")?;
        let source_height = reader.u32("Display stream source height")?;
        let destination = read_rect(&mut reader, "Display stream destination")?;
        if stream_width == 0
            || stream_height == 0
            || source_width == 0
            || source_height == 0
            || source_width > stream_width
            || source_height > stream_height
        {
            return Err(DecodeError::new(
                DecodeErrorKind::InvalidValue,
                18,
                "Display stream dimensions",
            ));
        }
        let _ = destination.width()?;
        let _ = destination.height()?;
        let clip = decode_stream_clip(&mut reader)?;
        if reader.remaining() != 0 {
            return Err(DecodeError::new(
                DecodeErrorKind::InvalidValue,
                reader.offset(),
                "Display stream create trailing data",
            ));
        }
        Ok(Self {
            surface_id,
            stream_id,
            top_down: flags != 0,
            codec,
            stamp,
            stream_width,
            stream_height,
            source_width,
            source_height,
            destination,
            clip,
        })
    }
}

/// Borrowed compressed frame with optional sized-stream geometry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StreamData<'a> {
    pub stream_id: u32,
    pub multimedia_time: u32,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub destination: Option<Rect>,
    pub data: &'a [u8],
}

impl<'a> StreamData<'a> {
    pub fn decode(body: &'a [u8], sized: bool) -> Result<Self, DecodeError> {
        let mut reader = Reader::new(body);
        let stream_id = reader.u32("Display stream data id")?;
        let multimedia_time = reader.u32("Display stream multimedia time")?;
        let (width, height, destination) = if sized {
            let width = reader.u32("Display sized stream width")?;
            let height = reader.u32("Display sized stream height")?;
            let destination = read_rect(&mut reader, "Display sized stream destination")?;
            if width == 0 || height == 0 {
                return Err(DecodeError::new(
                    DecodeErrorKind::InvalidValue,
                    8,
                    "Display sized stream dimensions",
                ));
            }
            let _ = destination.width()?;
            let _ = destination.height()?;
            (Some(width), Some(height), Some(destination))
        } else {
            (None, None, None)
        };
        let data_bytes =
            usize::try_from(reader.u32("Display stream data size")?).map_err(|_| {
                DecodeError::new(
                    DecodeErrorKind::Overflow,
                    reader.offset() - 4,
                    "Display stream data size",
                )
            })?;
        if data_bytes > MAX_STREAM_FRAME_BYTES {
            return Err(DecodeError::new(
                DecodeErrorKind::ResourceLimit,
                reader.offset() - 4,
                "Display stream frame",
            ));
        }
        if reader.remaining() != data_bytes {
            return Err(DecodeError::new(
                if reader.remaining() < data_bytes {
                    DecodeErrorKind::Truncated
                } else {
                    DecodeErrorKind::InvalidValue
                },
                reader.offset(),
                "Display stream frame",
            ));
        }
        let data = reader.take(data_bytes, "Display stream frame")?;
        Ok(Self {
            stream_id,
            multimedia_time,
            width,
            height,
            destination,
            data,
        })
    }
}

/// Stream-report window requested by the server.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StreamReportActivation {
    pub stream_id: u32,
    pub unique_id: u32,
    pub maximum_window_frames: u32,
    pub timeout_ms: u32,
}

impl StreamReportActivation {
    pub fn decode(body: &[u8]) -> Result<Self, DecodeError> {
        if body.len() != 16 {
            return Err(DecodeError::new(
                if body.len() < 16 {
                    DecodeErrorKind::Truncated
                } else {
                    DecodeErrorKind::InvalidValue
                },
                body.len(),
                "Display stream report activation",
            ));
        }
        let mut reader = Reader::new(body);
        let activation = Self {
            stream_id: reader.u32("Display stream report id")?,
            unique_id: reader.u32("Display stream report unique id")?,
            maximum_window_frames: reader.u32("Display stream report window")?,
            timeout_ms: reader.u32("Display stream report timeout")?,
        };
        if activation.maximum_window_frames == 0 || activation.timeout_ms == 0 {
            return Err(DecodeError::new(
                DecodeErrorKind::InvalidValue,
                8,
                "Display stream report limits",
            ));
        }
        Ok(activation)
    }
}

/// Client feedback for one activated stream-report window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StreamReport {
    pub stream_id: u32,
    pub unique_id: u32,
    pub start_frame_multimedia_time: u32,
    pub end_frame_multimedia_time: u32,
    pub frame_count: u32,
    pub dropped_frame_count: u32,
    pub last_frame_delay_ms: i32,
    pub audio_delay_ms: u32,
}

impl StreamReport {
    pub fn encode(self) -> [u8; 32] {
        let mut output = [0; 32];
        for (index, bytes) in [
            self.stream_id.to_le_bytes(),
            self.unique_id.to_le_bytes(),
            self.start_frame_multimedia_time.to_le_bytes(),
            self.end_frame_multimedia_time.to_le_bytes(),
            self.frame_count.to_le_bytes(),
            self.dropped_frame_count.to_le_bytes(),
            self.last_frame_delay_ms.to_le_bytes(),
            self.audio_delay_ms.to_le_bytes(),
        ]
        .into_iter()
        .enumerate()
        {
            let offset = index * 4;
            output[offset..offset + 4].copy_from_slice(&bytes);
        }
        output
    }
}

fn decode_stream_clip(reader: &mut Reader<'_>) -> Result<StreamClip, DecodeError> {
    match reader.u8("Display stream clip type")? {
        0 => Ok(StreamClip::None),
        1 => {
            let count =
                usize::try_from(reader.u32("Display stream clip count")?).map_err(|_| {
                    DecodeError::new(
                        DecodeErrorKind::Overflow,
                        reader.offset() - 4,
                        "Display stream clip count",
                    )
                })?;
            if count > MAX_STREAM_CLIP_RECTS {
                return Err(DecodeError::new(
                    DecodeErrorKind::ResourceLimit,
                    reader.offset() - 4,
                    "Display stream clip rectangles",
                ));
            }
            let mut rectangles = Vec::with_capacity(count);
            for _ in 0..count {
                let rectangle = read_rect(reader, "Display stream clip rectangle")?;
                let _ = rectangle.width()?;
                let _ = rectangle.height()?;
                rectangles.push(rectangle);
            }
            Ok(StreamClip::Rectangles(rectangles))
        }
        _ => Err(DecodeError::new(
            DecodeErrorKind::Unsupported,
            reader.offset() - 1,
            "Display stream clip type",
        )),
    }
}

/// Display surface pixel formats used by QEMU.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum SurfaceFormat {
    A8 = 8,
    Xrgb32 = 32,
    Argb32 = 96,
}

impl TryFrom<u32> for SurfaceFormat {
    type Error = DecodeError;

    /// Accepts the surface formats implemented by the checked renderer.
    fn try_from(value: u32) -> Result<Self, Self::Error> {
        match value {
            8 => Ok(Self::A8),
            32 => Ok(Self::Xrgb32),
            96 => Ok(Self::Argb32),
            _ => Err(DecodeError::new(
                DecodeErrorKind::Unsupported,
                12,
                "surface format",
            )),
        }
    }
}

/// A server-created display surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SurfaceCreate {
    pub surface_id: u32,
    pub width: u32,
    pub height: u32,
    pub format: SurfaceFormat,
    pub flags: u32,
}

/// Legacy single-plane Unix DMA-BUF scanout metadata; the descriptor travels out of band.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GlScanoutUnix {
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    pub fourcc: u32,
    pub top_down: bool,
}

impl GlScanoutUnix {
    pub fn decode(body: &[u8]) -> Result<Self, DecodeError> {
        if body.len() != 20 {
            return Err(DecodeError::new(
                if body.len() < 20 {
                    DecodeErrorKind::Truncated
                } else {
                    DecodeErrorKind::InvalidValue
                },
                body.len(),
                "GL scanout body",
            ));
        }
        let mut reader = Reader::new(body);
        let width = reader.u32("GL scanout width")?;
        let height = reader.u32("GL scanout height")?;
        let stride = reader.u32("GL scanout stride")?;
        let fourcc = reader.u32("GL scanout fourcc")?;
        let flags = reader.u32("GL scanout flags")?;
        if flags & !1 != 0 {
            return Err(DecodeError::new(
                DecodeErrorKind::Unsupported,
                16,
                "GL scanout flags",
            ));
        }
        let disabled = width == 0 && height == 0 && stride == 0 && fourcc == 0;
        if !disabled && (width == 0 || height == 0 || stride == 0) {
            return Err(DecodeError::new(
                DecodeErrorKind::InvalidValue,
                0,
                "GL scanout dimensions",
            ));
        }
        Ok(Self {
            width,
            height,
            stride,
            fourcc,
            top_down: flags & 1 != 0,
        })
    }

    pub const fn disabled(self) -> bool {
        self.width == 0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GlScanoutPlane {
    pub offset: u32,
    pub stride: u32,
}

/// Multi-plane Unix DMA-BUF scanout metadata with one out-of-band descriptor per plane.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GlScanout2Unix {
    pub width: u32,
    pub height: u32,
    pub fourcc: u32,
    pub top_down: bool,
    pub modifier: u64,
    pub planes: Vec<GlScanoutPlane>,
}

impl GlScanout2Unix {
    pub fn decode(body: &[u8]) -> Result<Self, DecodeError> {
        const FIXED_BYTES: usize = 25;
        if body.len() < FIXED_BYTES {
            return Err(DecodeError::new(
                DecodeErrorKind::Truncated,
                body.len(),
                "GL scanout2 body",
            ));
        }
        let mut reader = Reader::new(body);
        let width = reader.u32("GL scanout2 width")?;
        let height = reader.u32("GL scanout2 height")?;
        let fourcc = reader.u32("GL scanout2 fourcc")?;
        let flags = reader.u32("GL scanout2 flags")?;
        if flags & !1 != 0 {
            return Err(DecodeError::new(
                DecodeErrorKind::Unsupported,
                12,
                "GL scanout2 flags",
            ));
        }
        let plane_count = usize::from(reader.u8("GL scanout2 plane count")?);
        let modifier = reader.u64("GL scanout2 modifier")?;
        if plane_count > MAX_GL_SCANOUT_PLANES {
            return Err(DecodeError::new(
                DecodeErrorKind::ResourceLimit,
                16,
                "GL scanout2 plane count",
            ));
        }
        let expected_bytes = FIXED_BYTES
            .checked_add(plane_count.checked_mul(8).ok_or_else(|| {
                DecodeError::new(DecodeErrorKind::Overflow, FIXED_BYTES, "GL scanout2 planes")
            })?)
            .ok_or_else(|| {
                DecodeError::new(DecodeErrorKind::Overflow, FIXED_BYTES, "GL scanout2 body")
            })?;
        if body.len() != expected_bytes {
            return Err(DecodeError::new(
                if body.len() < expected_bytes {
                    DecodeErrorKind::Truncated
                } else {
                    DecodeErrorKind::InvalidValue
                },
                body.len(),
                "GL scanout2 planes",
            ));
        }
        let disabled = width == 0 && height == 0 && fourcc == 0 && plane_count == 0;
        if !disabled && (width == 0 || height == 0 || plane_count == 0) {
            return Err(DecodeError::new(
                DecodeErrorKind::InvalidValue,
                0,
                "GL scanout2 dimensions",
            ));
        }
        let mut planes = Vec::with_capacity(plane_count);
        for _ in 0..plane_count {
            let plane = GlScanoutPlane {
                offset: reader.u32("GL scanout2 plane offset")?,
                stride: reader.u32("GL scanout2 plane stride")?,
            };
            if plane.stride == 0 {
                return Err(DecodeError::new(
                    DecodeErrorKind::InvalidValue,
                    reader.offset() - 4,
                    "GL scanout2 plane stride",
                ));
            }
            planes.push(plane);
        }
        Ok(Self {
            width,
            height,
            fourcc,
            top_down: flags & 1 != 0,
            modifier,
            planes,
        })
    }

    pub const fn disabled(&self) -> bool {
        self.width == 0
    }
}

/// Dirty region for the current GL scanout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GlDraw {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

impl GlDraw {
    pub fn decode(body: &[u8]) -> Result<Self, DecodeError> {
        if body.len() != 16 {
            return Err(DecodeError::new(
                if body.len() < 16 {
                    DecodeErrorKind::Truncated
                } else {
                    DecodeErrorKind::InvalidValue
                },
                body.len(),
                "GL draw body",
            ));
        }
        let mut reader = Reader::new(body);
        let draw = Self {
            x: reader.u32("GL draw x")?,
            y: reader.u32("GL draw y")?,
            width: reader.u32("GL draw width")?,
            height: reader.u32("GL draw height")?,
        };
        if draw.width == 0
            || draw.height == 0
            || draw.x.checked_add(draw.width).is_none()
            || draw.y.checked_add(draw.height).is_none()
        {
            return Err(DecodeError::new(
                DecodeErrorKind::InvalidValue,
                0,
                "GL draw geometry",
            ));
        }
        Ok(draw)
    }
}

impl SurfaceCreate {
    /// Decodes the fixed 20-byte Surface Create body.
    pub fn decode(body: &[u8]) -> Result<Self, DecodeError> {
        if body.len() != 20 {
            return Err(DecodeError::new(
                if body.len() < 20 {
                    DecodeErrorKind::Truncated
                } else {
                    DecodeErrorKind::InvalidValue
                },
                body.len(),
                "surface create size",
            ));
        }
        let mut reader = Reader::new(body);
        Ok(Self {
            surface_id: reader.u32("surface id")?,
            width: reader.u32("surface width")?,
            height: reader.u32("surface height")?,
            format: SurfaceFormat::try_from(reader.u32("surface format")?)?,
            flags: reader.u32("surface flags")?,
        })
    }
}

/// A validated same-surface Copy Bits operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CopyBits {
    pub surface_id: u32,
    pub destination: Rect,
    pub clip: CompositeClip,
    pub source_x: i32,
    pub source_y: i32,
}

impl CopyBits {
    /// Decodes a same-surface copy with an optional inline rectangle clip.
    pub fn decode(body: &[u8]) -> Result<Self, DecodeError> {
        let mut reader = Reader::new(body);
        let surface_id = reader.u32("copy bits surface id")?;
        let destination = read_rect(&mut reader, "copy bits destination")?;
        let clip = decode_display_clip(&mut reader, "copy bits clip")?;
        let source_x = reader.i32("copy bits source x")?;
        let source_y = reader.i32("copy bits source y")?;
        if reader.remaining() != 0 {
            return Err(DecodeError::new(
                DecodeErrorKind::InvalidValue,
                reader.offset(),
                "copy bits trailing bytes",
            ));
        }
        let _ = destination.width()?;
        let _ = destination.height()?;
        Ok(Self {
            surface_id,
            destination,
            clip,
            source_x,
            source_y,
        })
    }
}

/// The first client message on a linked Display channel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DisplayInit {
    pub pixmap_cache_id: u8,
    pub pixmap_cache_size: i64,
    pub glz_dictionary_id: u8,
    pub glz_dictionary_window_size: i32,
}

impl DisplayInit {
    /// Encodes the packed 14-byte Display Init body.
    pub fn encode(self) -> [u8; 14] {
        let mut output = [0; 14];
        output[0] = self.pixmap_cache_id;
        output[1..9].copy_from_slice(&self.pixmap_cache_size.to_le_bytes());
        output[9] = self.glz_dictionary_id;
        output[10..14].copy_from_slice(&self.glz_dictionary_window_size.to_le_bytes());
        output
    }
}

/// SPICE bitmap wire formats accepted by the bounded image decoders.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum BitmapFormat {
    Indexed1Be = 2,
    Indexed4Be = 4,
    Indexed8 = 5,
    Rgb16 = 6,
    Bgr24 = 7,
    Xrgb32 = 8,
    Rgba32 = 9,
    Alpha8 = 10,
}

impl TryFrom<u8> for BitmapFormat {
    type Error = DecodeError;

    /// Accepts direct-color formats whose pixels do not depend on a palette cache.
    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            2 => Ok(Self::Indexed1Be),
            4 => Ok(Self::Indexed4Be),
            5 => Ok(Self::Indexed8),
            6 => Ok(Self::Rgb16),
            7 => Ok(Self::Bgr24),
            8 => Ok(Self::Xrgb32),
            9 => Ok(Self::Rgba32),
            10 => Ok(Self::Alpha8),
            _ => Err(DecodeError::new(
                DecodeErrorKind::Unsupported,
                0,
                "bitmap format",
            )),
        }
    }
}

impl BitmapFormat {
    /// Returns the protocol storage width for direct-color pixels.
    pub const fn bytes_per_pixel(self) -> Option<usize> {
        match self {
            Self::Indexed1Be | Self::Indexed4Be | Self::Indexed8 => None,
            Self::Rgb16 => Some(2),
            Self::Bgr24 => Some(3),
            Self::Xrgb32 | Self::Rgba32 => Some(4),
            Self::Alpha8 => Some(1),
        }
    }

    /// Returns the minimum packed row size for the given pixel width.
    pub fn minimum_stride(self, width: u32) -> Option<usize> {
        let width = usize::try_from(width).ok()?;
        match self {
            Self::Indexed1Be => width.checked_add(7).map(|pixels| pixels / 8),
            Self::Indexed4Be => width.checked_add(1).map(|pixels| pixels / 2),
            Self::Indexed8 => Some(width),
            Self::Alpha8 => Some(width),
            direct => width.checked_mul(direct.bytes_per_pixel()?),
        }
    }

    /// Returns the maximum palette entries for an indexed format.
    pub const fn maximum_palette_entries(self) -> Option<usize> {
        match self {
            Self::Indexed1Be => Some(2),
            Self::Indexed4Be => Some(16),
            Self::Indexed8 => Some(256),
            _ => None,
        }
    }
}

/// Palette ownership selected by bitmap flags.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BitmapPalette<'a> {
    Inline {
        unique_id: u64,
        cache_me: bool,
        entries_bgrx: &'a [u8],
    },
    Cached {
        unique_id: u64,
    },
}

/// Image encodings accepted by the current Draw Copy parser.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DrawCopyImageType {
    Bitmap,
    LzPalette,
    LzRgb,
    GlzRgb,
    Quic,
    Jpeg,
    ZlibGlzRgb,
    JpegAlpha,
    Lz4,
}

impl DrawCopyImageType {
    /// Returns the image descriptor value defined by the SPICE display protocol.
    pub const fn wire_value(self) -> u8 {
        match self {
            Self::Bitmap => 0,
            Self::Quic => 1,
            Self::LzPalette => 100,
            Self::LzRgb => 101,
            Self::GlzRgb => 102,
            Self::Jpeg => 105,
            Self::ZlibGlzRgb => 107,
            Self::JpegAlpha => 108,
            Self::Lz4 => 109,
        }
    }

    /// Reads and validates the common Draw Copy envelope before dispatching its image payload.
    pub fn decode(body: &[u8]) -> Result<Self, DecodeError> {
        Ok(decode_draw_copy_common(body)?.image_type)
    }
}

/// A borrowed, bounded compressed image referenced by a Draw Copy message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompressedImageUpdate<'a> {
    pub surface_id: u32,
    pub destination: Rect,
    pub source: Rect,
    pub image_width: u32,
    pub image_height: u32,
    pub image_type: DrawCopyImageType,
    pub rop_descriptor: u16,
    pub scale_mode: u8,
    pub palette: Option<BitmapPalette<'a>>,
    pub compressed_bytes: &'a [u8],
    pub uncompressed_bytes: Option<usize>,
}

/// A borrowed, bounded JPEG or JPEG-with-alpha image referenced by Draw Copy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JpegUpdate<'a> {
    pub surface_id: u32,
    pub destination: Rect,
    pub source: Rect,
    pub image_width: u32,
    pub image_height: u32,
    pub rop_descriptor: u16,
    pub scale_mode: u8,
    /// The alpha row direction declared by JPEG_ALPHA, absent for plain JPEG.
    pub alpha_top_down: Option<bool>,
    pub jpeg_bytes: &'a [u8],
    /// A complete SPICE LZ `XXXA` stream, absent for plain JPEG.
    pub alpha_lz_bytes: Option<&'a [u8]>,
}

/// A borrowed, validated raw bitmap referenced by a Draw Copy message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BitmapUpdate<'a> {
    pub surface_id: u32,
    pub destination: Rect,
    pub source: Rect,
    pub image_width: u32,
    pub image_height: u32,
    pub format: BitmapFormat,
    pub rop_descriptor: u16,
    pub scale_mode: u8,
    pub stride: u32,
    pub top_down: bool,
    pub palette: Option<BitmapPalette<'a>>,
    pub pixel_bytes: &'a [u8],
}

/// A bounded embedded image payload referenced by Draw Composite.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EmbeddedImage<'a> {
    Bitmap(EmbeddedBitmap<'a>),
    Compressed(EmbeddedCompressedImage<'a>),
    Jpeg(EmbeddedJpeg<'a>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmbeddedBitmap<'a> {
    pub width: u32,
    pub height: u32,
    pub format: BitmapFormat,
    pub stride: u32,
    pub top_down: bool,
    pub palette: Option<BitmapPalette<'a>>,
    pub pixel_bytes: &'a [u8],
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmbeddedCompressedImage<'a> {
    pub width: u32,
    pub height: u32,
    pub image_type: DrawCopyImageType,
    pub palette: Option<BitmapPalette<'a>>,
    pub compressed_bytes: &'a [u8],
    pub uncompressed_bytes: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmbeddedJpeg<'a> {
    pub width: u32,
    pub height: u32,
    pub alpha_top_down: Option<bool>,
    pub jpeg_bytes: &'a [u8],
    pub alpha_lz_bytes: Option<&'a [u8]>,
}

impl<'a> EmbeddedImage<'a> {
    /// Decodes an already-validated Composite image descriptor without copying payload bytes.
    pub fn decode(
        body: &'a [u8],
        descriptor: CompositeEmbeddedImage,
        maximum_bytes: usize,
    ) -> Result<Self, DecodeError> {
        decode_embedded_image(body, descriptor, maximum_bytes)
    }
}

impl<'a> BitmapUpdate<'a> {
    /// Default raw-image byte bound for one decoded image.
    pub const DEFAULT_MAX_BITMAP_BYTES: usize = 256 * 1024 * 1024;

    /// Decodes a specialized raw-bitmap Draw Copy payload and resolves its relative image address.
    pub fn decode_draw_copy(
        body: &'a [u8],
        maximum_bitmap_bytes: usize,
    ) -> Result<Self, DecodeError> {
        let common = decode_draw_copy_common(body)?;
        if common.image_type != DrawCopyImageType::Bitmap {
            return Err(DecodeError::new(
                DecodeErrorKind::Unsupported,
                common.image_payload_offset.saturating_sub(10),
                "compressed image type",
            ));
        }
        let DrawCopyCommon {
            surface_id,
            destination,
            source,
            image_width,
            image_height,
            rop_descriptor,
            scale_mode,
            image_payload_offset: bitmap_offset,
            ..
        } = common;

        let bitmap_fixed_end = bitmap_offset.checked_add(14).ok_or_else(|| {
            DecodeError::new(DecodeErrorKind::Overflow, bitmap_offset, "bitmap header")
        })?;
        let bitmap_header = body.get(bitmap_offset..bitmap_fixed_end).ok_or_else(|| {
            DecodeError::new(DecodeErrorKind::Truncated, bitmap_offset, "bitmap header")
        })?;
        let mut bitmap = Reader::new(bitmap_header);
        let format = BitmapFormat::try_from(bitmap.u8("bitmap format")?)?;
        let flags = bitmap.u8("bitmap flags")?;
        const PALETTE_CACHE_ME: u8 = 1 << 0;
        const PALETTE_FROM_CACHE: u8 = 1 << 1;
        const TOP_DOWN: u8 = 1 << 2;
        if flags & !(PALETTE_CACHE_ME | PALETTE_FROM_CACHE | TOP_DOWN) != 0
            || flags & PALETTE_CACHE_ME != 0 && flags & PALETTE_FROM_CACHE != 0
        {
            return Err(DecodeError::new(
                DecodeErrorKind::InvalidValue,
                bitmap_offset + 1,
                "bitmap flags",
            ));
        }
        let bitmap_width = bitmap.u32("bitmap width")?;
        let bitmap_height = bitmap.u32("bitmap height")?;
        let stride = bitmap.u32("bitmap stride")?;
        if bitmap_width != image_width || bitmap_height != image_height {
            return Err(DecodeError::new(
                DecodeErrorKind::InvalidValue,
                bitmap_offset + 2,
                "bitmap and image dimensions",
            ));
        }

        let indexed_entries = format.maximum_palette_entries();
        let bitmap_union_offset = u32::try_from(bitmap_fixed_end).map_err(|_| {
            DecodeError::new(
                DecodeErrorKind::Overflow,
                bitmap_fixed_end,
                "bitmap palette union",
            )
        })?;
        let (bitmap_header_end, palette) = if flags & PALETTE_FROM_CACHE != 0 {
            let palette_id_range = resolve_range(body, bitmap_union_offset, 8, "palette id")?;
            let mut palette_id = Reader::new(&body[palette_id_range.clone()]);
            let unique_id = palette_id.u64("palette id")?;
            if indexed_entries.is_none() {
                return Err(DecodeError::new(
                    DecodeErrorKind::InvalidValue,
                    bitmap_offset + 1,
                    "direct bitmap palette flags",
                ));
            }
            (
                palette_id_range.end,
                Some(BitmapPalette::Cached { unique_id }),
            )
        } else {
            let palette_pointer_range =
                resolve_range(body, bitmap_union_offset, 4, "palette pointer")?;
            let mut palette_pointer = Reader::new(&body[palette_pointer_range.clone()]);
            let palette_offset = palette_pointer.u32("bitmap palette offset")?;
            let palette = match indexed_entries {
                Some(maximum_entries) => {
                    if palette_offset == 0 {
                        return Err(DecodeError::new(
                            DecodeErrorKind::InvalidOffset,
                            bitmap_fixed_end,
                            "indexed bitmap palette",
                        ));
                    }
                    let palette_header = resolve_range(body, palette_offset, 10, "palette header")?;
                    let mut palette_reader = Reader::new(&body[palette_header]);
                    let unique_id = palette_reader.u64("palette unique id")?;
                    let entry_count = usize::from(palette_reader.u16("palette entry count")?);
                    if entry_count == 0 || entry_count > maximum_entries {
                        return Err(DecodeError::new(
                            DecodeErrorKind::ResourceLimit,
                            usize::try_from(palette_offset).unwrap_or(usize::MAX),
                            "palette entry count",
                        ));
                    }
                    let entry_bytes = entry_count.checked_mul(4).ok_or_else(|| {
                        DecodeError::new(
                            DecodeErrorKind::Overflow,
                            usize::try_from(palette_offset).unwrap_or(usize::MAX),
                            "palette entries",
                        )
                    })?;
                    let entries_offset = palette_offset.checked_add(10).ok_or_else(|| {
                        DecodeError::new(
                            DecodeErrorKind::Overflow,
                            usize::try_from(palette_offset).unwrap_or(usize::MAX),
                            "palette entries",
                        )
                    })?;
                    let entries =
                        resolve_range(body, entries_offset, entry_bytes, "palette entries")?;
                    Some(BitmapPalette::Inline {
                        unique_id,
                        cache_me: flags & PALETTE_CACHE_ME != 0,
                        entries_bgrx: &body[entries],
                    })
                }
                None => {
                    if palette_offset != 0 || flags & PALETTE_CACHE_ME != 0 {
                        return Err(DecodeError::new(
                            DecodeErrorKind::InvalidValue,
                            bitmap_fixed_end,
                            "direct bitmap palette",
                        ));
                    }
                    None
                }
            };
            (palette_pointer_range.end, palette)
        };

        let minimum_stride = format.minimum_stride(bitmap_width).ok_or_else(|| {
            DecodeError::new(
                DecodeErrorKind::Overflow,
                bitmap_offset + 2,
                "bitmap stride",
            )
        })?;
        let stride_usize = usize::try_from(stride).map_err(|_| {
            DecodeError::new(
                DecodeErrorKind::Overflow,
                bitmap_offset + 10,
                "bitmap stride",
            )
        })?;
        if stride_usize < minimum_stride {
            return Err(DecodeError::new(
                DecodeErrorKind::InvalidValue,
                bitmap_offset + 10,
                "bitmap stride",
            ));
        }
        let pixel_length = stride_usize
            .checked_mul(usize::try_from(bitmap_height).map_err(|_| {
                DecodeError::new(
                    DecodeErrorKind::Overflow,
                    bitmap_offset + 6,
                    "bitmap height",
                )
            })?)
            .ok_or_else(|| {
                DecodeError::new(
                    DecodeErrorKind::Overflow,
                    bitmap_offset + 10,
                    "bitmap bytes",
                )
            })?;
        if pixel_length > maximum_bitmap_bytes {
            return Err(DecodeError::new(
                DecodeErrorKind::ResourceLimit,
                bitmap_header_end,
                "bitmap bytes",
            ));
        }
        let pixel_end = bitmap_header_end.checked_add(pixel_length).ok_or_else(|| {
            DecodeError::new(DecodeErrorKind::Overflow, bitmap_header_end, "bitmap bytes")
        })?;
        let pixel_bytes = body.get(bitmap_header_end..pixel_end).ok_or_else(|| {
            DecodeError::new(
                DecodeErrorKind::Truncated,
                bitmap_header_end,
                "bitmap bytes",
            )
        })?;

        // Validate rectangle ordering now so the client can blit without repeating wire checks.
        let _ = destination.width()?;
        let _ = destination.height()?;
        let _ = source.width()?;
        let _ = source.height()?;
        Ok(Self {
            surface_id,
            destination,
            source,
            image_width,
            image_height,
            format,
            rop_descriptor,
            scale_mode,
            stride,
            top_down: flags & TOP_DOWN != 0,
            palette,
            pixel_bytes,
        })
    }
}

impl<'a> CompressedImageUpdate<'a> {
    /// Default compressed or wrapped-image byte bound for one Display message.
    pub const DEFAULT_MAX_COMPRESSED_BYTES: usize = 256 * 1024 * 1024;

    /// Decodes supported compressed-image wrappers without interpreting their codec bytes.
    pub fn decode_draw_copy(
        body: &'a [u8],
        maximum_compressed_bytes: usize,
    ) -> Result<Self, DecodeError> {
        let common = decode_draw_copy_common(body)?;
        if matches!(
            common.image_type,
            DrawCopyImageType::Bitmap | DrawCopyImageType::Jpeg | DrawCopyImageType::JpegAlpha
        ) {
            return Err(DecodeError::new(
                DecodeErrorKind::Unsupported,
                common.image_payload_offset.saturating_sub(10),
                "uncompressed image type",
            ));
        }
        let mut payload =
            Reader::new(body.get(common.image_payload_offset..).ok_or_else(|| {
                DecodeError::new(
                    DecodeErrorKind::Truncated,
                    common.image_payload_offset,
                    "LZ image payload",
                )
            })?);
        let (palette, data_size, data_offset, uncompressed_bytes) = match common.image_type {
            DrawCopyImageType::LzRgb | DrawCopyImageType::GlzRgb | DrawCopyImageType::Lz4 => {
                let data_size = usize::try_from(payload.u32("LZ data size")?).map_err(|_| {
                    DecodeError::new(
                        DecodeErrorKind::Overflow,
                        common.image_payload_offset,
                        "LZ data size",
                    )
                })?;
                (
                    None,
                    data_size,
                    common.image_payload_offset + payload.offset(),
                    None,
                )
            }
            DrawCopyImageType::Quic => {
                let data_size = usize::try_from(payload.u32("QUIC data size")?).map_err(|_| {
                    DecodeError::new(
                        DecodeErrorKind::Overflow,
                        common.image_payload_offset,
                        "QUIC data size",
                    )
                })?;
                (
                    None,
                    data_size,
                    common.image_payload_offset + payload.offset(),
                    None,
                )
            }
            DrawCopyImageType::ZlibGlzRgb => {
                let uncompressed_bytes = usize::try_from(payload.u32("zlib GLZ output size")?)
                    .map_err(|_| {
                        DecodeError::new(
                            DecodeErrorKind::Overflow,
                            common.image_payload_offset,
                            "zlib GLZ output size",
                        )
                    })?;
                let data_size =
                    usize::try_from(payload.u32("zlib GLZ data size")?).map_err(|_| {
                        DecodeError::new(
                            DecodeErrorKind::Overflow,
                            common.image_payload_offset + 4,
                            "zlib GLZ data size",
                        )
                    })?;
                (
                    None,
                    data_size,
                    common.image_payload_offset + payload.offset(),
                    Some(uncompressed_bytes),
                )
            }
            DrawCopyImageType::LzPalette => {
                const PALETTE_CACHE_ME: u8 = 1 << 0;
                const PALETTE_FROM_CACHE: u8 = 1 << 1;
                const TOP_DOWN: u8 = 1 << 2;
                let flags = payload.u8("LZ palette flags")?;
                if flags & !(PALETTE_CACHE_ME | PALETTE_FROM_CACHE | TOP_DOWN) != 0
                    || flags & PALETTE_CACHE_ME != 0 && flags & PALETTE_FROM_CACHE != 0
                {
                    return Err(DecodeError::new(
                        DecodeErrorKind::InvalidValue,
                        common.image_payload_offset,
                        "LZ palette flags",
                    ));
                }
                let data_size =
                    usize::try_from(payload.u32("LZ palette data size")?).map_err(|_| {
                        DecodeError::new(
                            DecodeErrorKind::Overflow,
                            common.image_payload_offset + 1,
                            "LZ palette data size",
                        )
                    })?;
                let palette = if flags & PALETTE_FROM_CACHE != 0 {
                    Some(BitmapPalette::Cached {
                        unique_id: payload.u64("LZ palette id")?,
                    })
                } else {
                    let palette_offset = payload.u32("LZ palette offset")?;
                    let (unique_id, entries_bgrx) = decode_palette(body, palette_offset, 256)?;
                    Some(BitmapPalette::Inline {
                        unique_id,
                        cache_me: flags & PALETTE_CACHE_ME != 0,
                        entries_bgrx,
                    })
                };
                (
                    palette,
                    data_size,
                    common.image_payload_offset + payload.offset(),
                    None,
                )
            }
            DrawCopyImageType::Bitmap => {
                unreachable!("bitmap rejected before compressed payload parse")
            }
            DrawCopyImageType::Jpeg | DrawCopyImageType::JpegAlpha => {
                unreachable!("JPEG rejected before LZ payload parse")
            }
        };
        if data_size > maximum_compressed_bytes {
            return Err(DecodeError::new(
                DecodeErrorKind::ResourceLimit,
                data_offset,
                "LZ compressed bytes",
            ));
        }
        if uncompressed_bytes
            .is_some_and(|bytes| bytes == 0 || bytes > Self::DEFAULT_MAX_COMPRESSED_BYTES)
        {
            return Err(DecodeError::new(
                DecodeErrorKind::ResourceLimit,
                common.image_payload_offset,
                "zlib GLZ output bytes",
            ));
        }
        let data_end = data_offset.checked_add(data_size).ok_or_else(|| {
            DecodeError::new(
                DecodeErrorKind::Overflow,
                data_offset,
                "LZ compressed bytes",
            )
        })?;
        let compressed_bytes = body.get(data_offset..data_end).ok_or_else(|| {
            DecodeError::new(
                DecodeErrorKind::Truncated,
                data_offset,
                "LZ compressed bytes",
            )
        })?;
        Ok(Self {
            surface_id: common.surface_id,
            destination: common.destination,
            source: common.source,
            image_width: common.image_width,
            image_height: common.image_height,
            image_type: common.image_type,
            rop_descriptor: common.rop_descriptor,
            scale_mode: common.scale_mode,
            palette,
            compressed_bytes,
            uncompressed_bytes,
        })
    }
}

impl<'a> JpegUpdate<'a> {
    /// Default byte bound for one JPEG image and its optional alpha stream.
    pub const DEFAULT_MAX_COMPRESSED_BYTES: usize = 256 * 1024 * 1024;

    /// Decodes the JPEG wire wrapper without interpreting codec bytes.
    pub fn decode_draw_copy(
        body: &'a [u8],
        maximum_compressed_bytes: usize,
    ) -> Result<Self, DecodeError> {
        let common = decode_draw_copy_common(body)?;
        let payload_bytes = body.get(common.image_payload_offset..).ok_or_else(|| {
            DecodeError::new(
                DecodeErrorKind::Truncated,
                common.image_payload_offset,
                "JPEG image payload",
            )
        })?;
        let mut payload = Reader::new(payload_bytes);
        let (alpha_top_down, jpeg_size, data_size) = match common.image_type {
            DrawCopyImageType::Jpeg => {
                let data_size = usize::try_from(payload.u32("JPEG data size")?).map_err(|_| {
                    DecodeError::new(
                        DecodeErrorKind::Overflow,
                        common.image_payload_offset,
                        "JPEG data size",
                    )
                })?;
                (None, data_size, data_size)
            }
            DrawCopyImageType::JpegAlpha => {
                const TOP_DOWN: u8 = 1 << 0;
                let flags = payload.u8("JPEG alpha flags")?;
                if flags & !TOP_DOWN != 0 {
                    return Err(DecodeError::new(
                        DecodeErrorKind::InvalidValue,
                        common.image_payload_offset,
                        "JPEG alpha flags",
                    ));
                }
                let jpeg_size =
                    usize::try_from(payload.u32("JPEG alpha JPEG size")?).map_err(|_| {
                        DecodeError::new(
                            DecodeErrorKind::Overflow,
                            common.image_payload_offset + 1,
                            "JPEG alpha JPEG size",
                        )
                    })?;
                let data_size =
                    usize::try_from(payload.u32("JPEG alpha data size")?).map_err(|_| {
                        DecodeError::new(
                            DecodeErrorKind::Overflow,
                            common.image_payload_offset + 5,
                            "JPEG alpha data size",
                        )
                    })?;
                (Some(flags & TOP_DOWN != 0), jpeg_size, data_size)
            }
            _ => {
                return Err(DecodeError::new(
                    DecodeErrorKind::Unsupported,
                    common.image_payload_offset.saturating_sub(10),
                    "non-JPEG image type",
                ));
            }
        };
        let data_offset = common.image_payload_offset + payload.offset();
        if data_size == 0 || data_size > maximum_compressed_bytes || jpeg_size == 0 {
            return Err(DecodeError::new(
                DecodeErrorKind::ResourceLimit,
                data_offset,
                "JPEG compressed bytes",
            ));
        }
        if jpeg_size > data_size || alpha_top_down.is_some() && jpeg_size == data_size {
            return Err(DecodeError::new(
                DecodeErrorKind::InvalidValue,
                data_offset,
                "JPEG alpha stream sizes",
            ));
        }
        let data_end = data_offset.checked_add(data_size).ok_or_else(|| {
            DecodeError::new(
                DecodeErrorKind::Overflow,
                data_offset,
                "JPEG compressed bytes",
            )
        })?;
        let data = body.get(data_offset..data_end).ok_or_else(|| {
            DecodeError::new(
                DecodeErrorKind::Truncated,
                data_offset,
                "JPEG compressed bytes",
            )
        })?;
        Ok(Self {
            surface_id: common.surface_id,
            destination: common.destination,
            source: common.source,
            image_width: common.image_width,
            image_height: common.image_height,
            rop_descriptor: common.rop_descriptor,
            scale_mode: common.scale_mode,
            alpha_top_down,
            jpeg_bytes: &data[..jpeg_size],
            alpha_lz_bytes: alpha_top_down.map(|_| &data[jpeg_size..]),
        })
    }
}

struct DrawCopyCommon {
    surface_id: u32,
    destination: Rect,
    source: Rect,
    image_width: u32,
    image_height: u32,
    image_type: DrawCopyImageType,
    rop_descriptor: u16,
    scale_mode: u8,
    image_payload_offset: usize,
}

/// Validates fields shared by every supported Draw Copy image encoding.
fn decode_draw_copy_common(body: &[u8]) -> Result<DrawCopyCommon, DecodeError> {
    let mut reader = Reader::new(body);
    let surface_id = reader.u32("draw surface id")?;
    let destination = read_rect(&mut reader, "draw destination")?;
    if reader.u8("draw clip type")? != 0 {
        return Err(DecodeError::new(
            DecodeErrorKind::Unsupported,
            reader.offset() - 1,
            "draw clip rectangles",
        ));
    }
    let image_offset = reader.u32("draw source image offset")?;
    let source = read_rect(&mut reader, "draw source rectangle")?;
    let rop_descriptor = reader.u16("draw raster operation")?;
    let scale_mode = reader.u8("draw scale mode")?;
    if scale_mode > 1 {
        return Err(DecodeError::new(
            DecodeErrorKind::InvalidValue,
            reader.offset() - 1,
            "draw scale mode",
        ));
    }
    let _mask_flags = reader.u8("draw mask flags")?;
    let _mask_x = reader.i32("draw mask x")?;
    let _mask_y = reader.i32("draw mask y")?;
    if reader.u32("draw mask image offset")? != 0 {
        return Err(DecodeError::new(
            DecodeErrorKind::Unsupported,
            reader.offset() - 4,
            "draw mask image",
        ));
    }

    let image_header = resolve_range(body, image_offset, 18, "image descriptor")?;
    let mut image = Reader::new(&body[image_header.clone()]);
    let _image_id = image.u64("image id")?;
    let image_type = match image.u8("image type")? {
        0 => DrawCopyImageType::Bitmap,
        100 => DrawCopyImageType::LzPalette,
        101 => DrawCopyImageType::LzRgb,
        102 => DrawCopyImageType::GlzRgb,
        1 => DrawCopyImageType::Quic,
        103 | 106 => {
            return Err(DecodeError::new(
                DecodeErrorKind::Unsupported,
                image_header.start + 8,
                "disabled image cache reference",
            ));
        }
        105 => DrawCopyImageType::Jpeg,
        107 => DrawCopyImageType::ZlibGlzRgb,
        108 => DrawCopyImageType::JpegAlpha,
        109 => DrawCopyImageType::Lz4,
        _ => {
            return Err(DecodeError::new(
                DecodeErrorKind::Unsupported,
                image_header.start + 8,
                "image type",
            ));
        }
    };
    let image_flags = image.u8("image flags")?;
    if image_flags & !0b111 != 0 {
        return Err(DecodeError::new(
            DecodeErrorKind::InvalidValue,
            image_header.start + 9,
            "image flags",
        ));
    }
    if image_flags & 0b101 != 0 {
        return Err(DecodeError::new(
            DecodeErrorKind::Unsupported,
            image_header.start + 9,
            "image cache flags",
        ));
    }
    let image_width = image.u32("image width")?;
    let image_height = image.u32("image height")?;
    let _ = destination.width()?;
    let _ = destination.height()?;
    let _ = source.width()?;
    let _ = source.height()?;
    Ok(DrawCopyCommon {
        surface_id,
        destination,
        source,
        image_width,
        image_height,
        image_type,
        rop_descriptor,
        scale_mode,
        image_payload_offset: image_header.end,
    })
}

/// Resolves one pointed palette and applies an encoding-specific entry bound.
fn decode_palette(
    body: &[u8],
    palette_offset: u32,
    maximum_entries: usize,
) -> Result<(u64, &[u8]), DecodeError> {
    if palette_offset == 0 {
        return Err(DecodeError::new(
            DecodeErrorKind::InvalidOffset,
            0,
            "palette offset",
        ));
    }
    let palette_header = resolve_range(body, palette_offset, 10, "palette header")?;
    let mut palette = Reader::new(&body[palette_header]);
    let unique_id = palette.u64("palette unique id")?;
    let entry_count = usize::from(palette.u16("palette entry count")?);
    if entry_count == 0 || entry_count > maximum_entries {
        return Err(DecodeError::new(
            DecodeErrorKind::ResourceLimit,
            usize::try_from(palette_offset).unwrap_or(usize::MAX),
            "palette entry count",
        ));
    }
    let entry_bytes = entry_count.checked_mul(4).ok_or_else(|| {
        DecodeError::new(
            DecodeErrorKind::Overflow,
            usize::try_from(palette_offset).unwrap_or(usize::MAX),
            "palette entries",
        )
    })?;
    let entries_offset = palette_offset.checked_add(10).ok_or_else(|| {
        DecodeError::new(
            DecodeErrorKind::Overflow,
            usize::try_from(palette_offset).unwrap_or(usize::MAX),
            "palette entries",
        )
    })?;
    let entries = resolve_range(body, entries_offset, entry_bytes, "palette entries")?;
    Ok((unique_id, &body[entries]))
}

fn decode_embedded_image<'a>(
    body: &'a [u8],
    descriptor: CompositeEmbeddedImage,
    maximum_bytes: usize,
) -> Result<EmbeddedImage<'a>, DecodeError> {
    const IMAGE_DESCRIPTOR_BYTES: u32 = 18;
    let payload_offset = descriptor
        .image_offset
        .checked_add(IMAGE_DESCRIPTOR_BYTES)
        .ok_or_else(|| {
            DecodeError::new(
                DecodeErrorKind::Overflow,
                usize::try_from(descriptor.image_offset).unwrap_or(usize::MAX),
                "Composite embedded image payload",
            )
        })?;
    match descriptor.image_type {
        DrawCopyImageType::Bitmap => {
            decode_embedded_bitmap(body, descriptor, payload_offset, maximum_bytes)
                .map(EmbeddedImage::Bitmap)
        }
        DrawCopyImageType::Jpeg | DrawCopyImageType::JpegAlpha => {
            decode_embedded_jpeg(body, descriptor, payload_offset, maximum_bytes)
                .map(EmbeddedImage::Jpeg)
        }
        _ => decode_embedded_compressed(body, descriptor, payload_offset, maximum_bytes)
            .map(EmbeddedImage::Compressed),
    }
}

fn decode_embedded_bitmap<'a>(
    body: &'a [u8],
    descriptor: CompositeEmbeddedImage,
    payload_offset: u32,
    maximum_bytes: usize,
) -> Result<EmbeddedBitmap<'a>, DecodeError> {
    const PALETTE_CACHE_ME: u8 = 1 << 0;
    const PALETTE_FROM_CACHE: u8 = 1 << 1;
    const TOP_DOWN: u8 = 1 << 2;
    let header_range = resolve_range(body, payload_offset, 14, "Composite bitmap header")?;
    let mut header = Reader::new(&body[header_range.clone()]);
    let format = BitmapFormat::try_from(header.u8("Composite bitmap format")?)?;
    let flags = header.u8("Composite bitmap flags")?;
    if flags & !(PALETTE_CACHE_ME | PALETTE_FROM_CACHE | TOP_DOWN) != 0
        || flags & PALETTE_CACHE_ME != 0 && flags & PALETTE_FROM_CACHE != 0
    {
        return Err(DecodeError::new(
            DecodeErrorKind::InvalidValue,
            header_range.start + 1,
            "Composite bitmap flags",
        ));
    }
    let width = header.u32("Composite bitmap width")?;
    let height = header.u32("Composite bitmap height")?;
    let stride = header.u32("Composite bitmap stride")?;
    if width != descriptor.width || height != descriptor.height {
        return Err(DecodeError::new(
            DecodeErrorKind::InvalidValue,
            header_range.start + 2,
            "Composite bitmap descriptor dimensions",
        ));
    }
    let palette_entries = format.maximum_palette_entries();
    let union_offset = payload_offset.checked_add(14).ok_or_else(|| {
        DecodeError::new(
            DecodeErrorKind::Overflow,
            header_range.end,
            "Composite bitmap palette union",
        )
    })?;
    let (pixel_offset, palette) = if flags & PALETTE_FROM_CACHE != 0 {
        let range = resolve_range(body, union_offset, 8, "Composite bitmap palette id")?;
        if palette_entries.is_none() {
            return Err(DecodeError::new(
                DecodeErrorKind::InvalidValue,
                header_range.start + 1,
                "Composite direct bitmap palette",
            ));
        }
        let mut palette_id = Reader::new(&body[range.clone()]);
        (
            range.end,
            Some(BitmapPalette::Cached {
                unique_id: palette_id.u64("Composite bitmap palette id")?,
            }),
        )
    } else {
        let range = resolve_range(body, union_offset, 4, "Composite bitmap palette offset")?;
        let mut palette_pointer = Reader::new(&body[range.clone()]);
        let palette_offset = palette_pointer.u32("Composite bitmap palette offset")?;
        let palette = if let Some(maximum_entries) = palette_entries {
            let (unique_id, entries_bgrx) = decode_palette(body, palette_offset, maximum_entries)?;
            Some(BitmapPalette::Inline {
                unique_id,
                cache_me: flags & PALETTE_CACHE_ME != 0,
                entries_bgrx,
            })
        } else {
            if palette_offset != 0 || flags & PALETTE_CACHE_ME != 0 {
                return Err(DecodeError::new(
                    DecodeErrorKind::InvalidValue,
                    range.start,
                    "Composite direct bitmap palette",
                ));
            }
            None
        };
        (range.end, palette)
    };
    let minimum_stride = format.minimum_stride(width).ok_or_else(|| {
        DecodeError::new(
            DecodeErrorKind::Overflow,
            header_range.start + 10,
            "Composite bitmap stride",
        )
    })?;
    let stride_usize = usize::try_from(stride).map_err(|_| {
        DecodeError::new(
            DecodeErrorKind::Overflow,
            header_range.start + 10,
            "Composite bitmap stride",
        )
    })?;
    if stride_usize < minimum_stride {
        return Err(DecodeError::new(
            DecodeErrorKind::InvalidValue,
            header_range.start + 10,
            "Composite bitmap stride",
        ));
    }
    let pixel_length = stride_usize
        .checked_mul(usize::try_from(height).map_err(|_| {
            DecodeError::new(
                DecodeErrorKind::Overflow,
                header_range.start + 6,
                "Composite bitmap height",
            )
        })?)
        .ok_or_else(|| {
            DecodeError::new(
                DecodeErrorKind::Overflow,
                pixel_offset,
                "Composite bitmap bytes",
            )
        })?;
    if pixel_length > maximum_bytes {
        return Err(DecodeError::new(
            DecodeErrorKind::ResourceLimit,
            pixel_offset,
            "Composite bitmap bytes",
        ));
    }
    let pixel_end = pixel_offset.checked_add(pixel_length).ok_or_else(|| {
        DecodeError::new(
            DecodeErrorKind::Overflow,
            pixel_offset,
            "Composite bitmap bytes",
        )
    })?;
    let pixel_bytes = body.get(pixel_offset..pixel_end).ok_or_else(|| {
        DecodeError::new(
            DecodeErrorKind::Truncated,
            pixel_offset,
            "Composite bitmap bytes",
        )
    })?;
    Ok(EmbeddedBitmap {
        width,
        height,
        format,
        stride,
        top_down: flags & TOP_DOWN != 0,
        palette,
        pixel_bytes,
    })
}

fn decode_embedded_compressed<'a>(
    body: &'a [u8],
    descriptor: CompositeEmbeddedImage,
    payload_offset: u32,
    maximum_bytes: usize,
) -> Result<EmbeddedCompressedImage<'a>, DecodeError> {
    const PALETTE_CACHE_ME: u8 = 1 << 0;
    const PALETTE_FROM_CACHE: u8 = 1 << 1;
    const TOP_DOWN: u8 = 1 << 2;
    let payload_start = usize::try_from(payload_offset).map_err(|_| {
        DecodeError::new(
            DecodeErrorKind::Overflow,
            0,
            "Composite compressed image payload",
        )
    })?;
    let mut payload = Reader::new(body.get(payload_start..).ok_or_else(|| {
        DecodeError::new(
            DecodeErrorKind::InvalidOffset,
            payload_start,
            "Composite compressed image payload",
        )
    })?);
    let (palette, data_size, uncompressed_bytes) = match descriptor.image_type {
        DrawCopyImageType::LzPalette => {
            let flags = payload.u8("Composite LZ palette flags")?;
            if flags & !(PALETTE_CACHE_ME | PALETTE_FROM_CACHE | TOP_DOWN) != 0
                || flags & PALETTE_CACHE_ME != 0 && flags & PALETTE_FROM_CACHE != 0
            {
                return Err(DecodeError::new(
                    DecodeErrorKind::InvalidValue,
                    payload_start,
                    "Composite LZ palette flags",
                ));
            }
            let data_size = usize::try_from(payload.u32("Composite LZ palette data size")?)
                .map_err(|_| {
                    DecodeError::new(
                        DecodeErrorKind::Overflow,
                        payload_start + 1,
                        "Composite LZ palette data size",
                    )
                })?;
            let palette = if flags & PALETTE_FROM_CACHE != 0 {
                Some(BitmapPalette::Cached {
                    unique_id: payload.u64("Composite LZ palette id")?,
                })
            } else {
                let palette_offset = payload.u32("Composite LZ palette offset")?;
                let (unique_id, entries_bgrx) = decode_palette(body, palette_offset, 256)?;
                Some(BitmapPalette::Inline {
                    unique_id,
                    cache_me: flags & PALETTE_CACHE_ME != 0,
                    entries_bgrx,
                })
            };
            (palette, data_size, None)
        }
        DrawCopyImageType::ZlibGlzRgb => {
            let output_bytes = usize::try_from(payload.u32("Composite zlib GLZ output size")?)
                .map_err(|_| {
                    DecodeError::new(
                        DecodeErrorKind::Overflow,
                        payload_start,
                        "Composite zlib GLZ output size",
                    )
                })?;
            let data_size =
                usize::try_from(payload.u32("Composite zlib GLZ data size")?).map_err(|_| {
                    DecodeError::new(
                        DecodeErrorKind::Overflow,
                        payload_start + 4,
                        "Composite zlib GLZ data size",
                    )
                })?;
            (None, data_size, Some(output_bytes))
        }
        DrawCopyImageType::LzRgb
        | DrawCopyImageType::GlzRgb
        | DrawCopyImageType::Quic
        | DrawCopyImageType::Lz4 => {
            let data_size = usize::try_from(payload.u32("Composite compressed data size")?)
                .map_err(|_| {
                    DecodeError::new(
                        DecodeErrorKind::Overflow,
                        payload_start,
                        "Composite compressed data size",
                    )
                })?;
            (None, data_size, None)
        }
        _ => {
            return Err(DecodeError::new(
                DecodeErrorKind::Unsupported,
                payload_start,
                "Composite compressed image type",
            ));
        }
    };
    let data_offset = payload_start + payload.offset();
    if data_size == 0 || data_size > maximum_bytes {
        return Err(DecodeError::new(
            DecodeErrorKind::ResourceLimit,
            data_offset,
            "Composite compressed image bytes",
        ));
    }
    if uncompressed_bytes.is_some_and(|bytes| bytes == 0 || bytes > maximum_bytes) {
        return Err(DecodeError::new(
            DecodeErrorKind::ResourceLimit,
            payload_start,
            "Composite zlib GLZ output bytes",
        ));
    }
    let data_end = data_offset.checked_add(data_size).ok_or_else(|| {
        DecodeError::new(
            DecodeErrorKind::Overflow,
            data_offset,
            "Composite compressed image bytes",
        )
    })?;
    let compressed_bytes = body.get(data_offset..data_end).ok_or_else(|| {
        DecodeError::new(
            DecodeErrorKind::Truncated,
            data_offset,
            "Composite compressed image bytes",
        )
    })?;
    Ok(EmbeddedCompressedImage {
        width: descriptor.width,
        height: descriptor.height,
        image_type: descriptor.image_type,
        palette,
        compressed_bytes,
        uncompressed_bytes,
    })
}

fn decode_embedded_jpeg<'a>(
    body: &'a [u8],
    descriptor: CompositeEmbeddedImage,
    payload_offset: u32,
    maximum_bytes: usize,
) -> Result<EmbeddedJpeg<'a>, DecodeError> {
    let payload_start = usize::try_from(payload_offset)
        .map_err(|_| DecodeError::new(DecodeErrorKind::Overflow, 0, "Composite JPEG payload"))?;
    let mut payload = Reader::new(body.get(payload_start..).ok_or_else(|| {
        DecodeError::new(
            DecodeErrorKind::InvalidOffset,
            payload_start,
            "Composite JPEG payload",
        )
    })?);
    let (alpha_top_down, jpeg_size, data_size) = match descriptor.image_type {
        DrawCopyImageType::Jpeg => {
            let size = usize::try_from(payload.u32("Composite JPEG data size")?).map_err(|_| {
                DecodeError::new(
                    DecodeErrorKind::Overflow,
                    payload_start,
                    "Composite JPEG data size",
                )
            })?;
            (None, size, size)
        }
        DrawCopyImageType::JpegAlpha => {
            const TOP_DOWN: u8 = 1 << 0;
            let flags = payload.u8("Composite JPEG alpha flags")?;
            if flags & !TOP_DOWN != 0 {
                return Err(DecodeError::new(
                    DecodeErrorKind::InvalidValue,
                    payload_start,
                    "Composite JPEG alpha flags",
                ));
            }
            let jpeg_size = usize::try_from(payload.u32("Composite JPEG alpha JPEG size")?)
                .map_err(|_| {
                    DecodeError::new(
                        DecodeErrorKind::Overflow,
                        payload_start + 1,
                        "Composite JPEG alpha JPEG size",
                    )
                })?;
            let data_size = usize::try_from(payload.u32("Composite JPEG alpha data size")?)
                .map_err(|_| {
                    DecodeError::new(
                        DecodeErrorKind::Overflow,
                        payload_start + 5,
                        "Composite JPEG alpha data size",
                    )
                })?;
            (Some(flags & TOP_DOWN != 0), jpeg_size, data_size)
        }
        _ => unreachable!("caller dispatches only JPEG images"),
    };
    let data_offset = payload_start + payload.offset();
    if data_size == 0 || data_size > maximum_bytes || jpeg_size == 0 || jpeg_size > data_size {
        return Err(DecodeError::new(
            DecodeErrorKind::ResourceLimit,
            data_offset,
            "Composite JPEG bytes",
        ));
    }
    if alpha_top_down.is_some() && jpeg_size == data_size {
        return Err(DecodeError::new(
            DecodeErrorKind::InvalidValue,
            data_offset,
            "Composite JPEG alpha stream sizes",
        ));
    }
    let data_end = data_offset.checked_add(data_size).ok_or_else(|| {
        DecodeError::new(
            DecodeErrorKind::Overflow,
            data_offset,
            "Composite JPEG bytes",
        )
    })?;
    let data = body.get(data_offset..data_end).ok_or_else(|| {
        DecodeError::new(
            DecodeErrorKind::Truncated,
            data_offset,
            "Composite JPEG bytes",
        )
    })?;
    Ok(EmbeddedJpeg {
        width: descriptor.width,
        height: descriptor.height,
        alpha_top_down,
        jpeg_bytes: &data[..jpeg_size],
        alpha_lz_bytes: alpha_top_down.map(|_| &data[jpeg_size..]),
    })
}

/// Reads one rectangle while preserving the protocol's signed coordinates.
pub(crate) fn read_rect(
    reader: &mut Reader<'_>,
    context: &'static str,
) -> Result<Rect, DecodeError> {
    Ok(Rect {
        top: reader.i32(context)?,
        left: reader.i32(context)?,
        bottom: reader.i32(context)?,
        right: reader.i32(context)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn append_surface_image(body: &mut Vec<u8>, image_id: u64, surface_id: u32) {
        body.extend_from_slice(&image_id.to_le_bytes());
        body.push(104);
        body.push(0);
        body.extend_from_slice(&1_u32.to_le_bytes());
        body.extend_from_slice(&1_u32.to_le_bytes());
        body.extend_from_slice(&surface_id.to_le_bytes());
    }

    #[test]
    fn composite_resolves_surface_image_and_affine_transform() {
        const HAS_SOURCE_TRANSFORM: u32 = 1 << 20;
        let source_image_offset = 61_u32;
        let mut body = Vec::new();
        body.extend_from_slice(&7_u32.to_le_bytes());
        for coordinate in [0_i32, 0, 1, 1] {
            body.extend_from_slice(&coordinate.to_le_bytes());
        }
        body.push(0);
        let flags = 1_u32 | (3 << 8) | HAS_SOURCE_TRANSFORM;
        body.extend_from_slice(&flags.to_le_bytes());
        body.extend_from_slice(&source_image_offset.to_le_bytes());
        for value in [1_i32 << 16, 0, 2 << 16, 0, 1 << 16, 3 << 16] {
            body.extend_from_slice(&value.to_le_bytes());
        }
        body.extend_from_slice(&4_i16.to_le_bytes());
        body.extend_from_slice(&5_i16.to_le_bytes());
        body.extend_from_slice(&0_i16.to_le_bytes());
        body.extend_from_slice(&0_i16.to_le_bytes());
        assert_eq!(body.len(), source_image_offset as usize);
        append_surface_image(&mut body, 11, 9);

        let composite = DrawComposite::decode(&body).expect("valid Composite command");
        assert_eq!(composite.destination_surface_id, 7);
        assert!(matches!(
            composite.source,
            CompositeImage::Surface(CompositeSurface { surface_id: 9, .. })
        ));
        assert_eq!(composite.source_filter, 3);
        assert_eq!(composite.source_origin, Point16 { x: 4, y: 5 });
        assert_eq!(
            composite.source_transform,
            Some(CompositeTransform {
                xx: 1 << 16,
                xy: 0,
                x0: 2 << 16,
                yx: 0,
                yy: 1 << 16,
                y0: 3 << 16,
            })
        );
    }

    #[test]
    fn composite_rejects_out_of_body_image_offset() {
        let mut body = Vec::new();
        body.extend_from_slice(&0_u32.to_le_bytes());
        for coordinate in [0_i32, 0, 1, 1] {
            body.extend_from_slice(&coordinate.to_le_bytes());
        }
        body.push(0);
        body.extend_from_slice(&1_u32.to_le_bytes());
        body.extend_from_slice(&u32::MAX.to_le_bytes());
        body.extend_from_slice(&[0; 8]);

        let error = DrawComposite::decode(&body).expect_err("invalid image offset must fail");
        assert_eq!(error.kind, DecodeErrorKind::InvalidOffset);
    }

    #[test]
    fn composite_embedded_bitmap_is_borrowed_and_bounded() {
        let source_image_offset = 37_u32;
        let mut body = Vec::new();
        body.extend_from_slice(&0_u32.to_le_bytes());
        for coordinate in [0_i32, 0, 1, 1] {
            body.extend_from_slice(&coordinate.to_le_bytes());
        }
        body.push(0);
        body.extend_from_slice(&1_u32.to_le_bytes());
        body.extend_from_slice(&source_image_offset.to_le_bytes());
        body.extend_from_slice(&[0; 8]);
        body.extend_from_slice(&7_u64.to_le_bytes());
        body.push(0);
        body.push(0);
        body.extend_from_slice(&1_u32.to_le_bytes());
        body.extend_from_slice(&1_u32.to_le_bytes());
        body.push(BitmapFormat::Xrgb32 as u8);
        body.push(1 << 2);
        body.extend_from_slice(&1_u32.to_le_bytes());
        body.extend_from_slice(&1_u32.to_le_bytes());
        body.extend_from_slice(&4_u32.to_le_bytes());
        body.extend_from_slice(&0_u32.to_le_bytes());
        body.extend_from_slice(&[3, 2, 1, 0]);

        let composite = DrawComposite::decode(&body).expect("valid embedded bitmap");
        let CompositeImage::Embedded(descriptor) = composite.source else {
            panic!("embedded descriptor expected")
        };
        let EmbeddedImage::Bitmap(bitmap) =
            EmbeddedImage::decode(&body, descriptor, 4).expect("bounded bitmap")
        else {
            panic!("bitmap payload expected")
        };
        assert_eq!(bitmap.pixel_bytes, [3, 2, 1, 0]);
        assert!(bitmap.top_down);
    }

    #[test]
    fn draw_copy_does_not_confuse_cache_reference_with_quic() {
        let cached = one_pixel_draw_copy_envelope(103);
        let error = DrawCopyImageType::decode(&cached).expect_err("cache is disabled");
        assert_eq!(error.kind, DecodeErrorKind::Unsupported);

        let quic = one_pixel_draw_copy_envelope(1);
        assert_eq!(
            DrawCopyImageType::decode(&quic).expect("QUIC type"),
            DrawCopyImageType::Quic
        );
    }

    #[test]
    fn gl_scanout2_bounds_plane_array_and_preserves_modifier() {
        let mut body = Vec::new();
        body.extend_from_slice(&1920_u32.to_le_bytes());
        body.extend_from_slice(&1080_u32.to_le_bytes());
        body.extend_from_slice(&0x3231_564e_u32.to_le_bytes());
        body.extend_from_slice(&1_u32.to_le_bytes());
        body.push(2);
        body.extend_from_slice(&0x0102_0304_0506_0708_u64.to_le_bytes());
        body.extend_from_slice(&0_u32.to_le_bytes());
        body.extend_from_slice(&1920_u32.to_le_bytes());
        body.extend_from_slice(&(1920_u32 * 1080).to_le_bytes());
        body.extend_from_slice(&1920_u32.to_le_bytes());

        let scanout = GlScanout2Unix::decode(&body).expect("valid two-plane scanout");
        assert_eq!(scanout.planes.len(), 2);
        assert_eq!(scanout.modifier, 0x0102_0304_0506_0708);
        assert!(scanout.top_down);

        body[16] = 5;
        let error = GlScanout2Unix::decode(&body).expect_err("plane limit must apply first");
        assert_eq!(error.kind, DecodeErrorKind::ResourceLimit);
    }

    #[test]
    fn gl_draw_rejects_wrapping_geometry() {
        let mut body = Vec::new();
        body.extend_from_slice(&u32::MAX.to_le_bytes());
        body.extend_from_slice(&0_u32.to_le_bytes());
        body.extend_from_slice(&2_u32.to_le_bytes());
        body.extend_from_slice(&1_u32.to_le_bytes());
        let error = GlDraw::decode(&body).expect_err("wrapping right edge must fail");
        assert_eq!(error.kind, DecodeErrorKind::InvalidValue);
    }

    /// Builds the common one-pixel Draw Copy envelope for payload parser tests.
    fn one_pixel_draw_copy_envelope(image_type: u8) -> Vec<u8> {
        let mut body = Vec::new();
        body.extend_from_slice(&0_u32.to_le_bytes());
        for coordinate in [0_i32, 0, 1, 1] {
            body.extend_from_slice(&coordinate.to_le_bytes());
        }
        body.push(0);
        body.extend_from_slice(&57_u32.to_le_bytes());
        for coordinate in [0_i32, 0, 1, 1] {
            body.extend_from_slice(&coordinate.to_le_bytes());
        }
        body.extend_from_slice(&(1_u16 << 3).to_le_bytes());
        body.push(0);
        body.push(0);
        body.extend_from_slice(&0_i32.to_le_bytes());
        body.extend_from_slice(&0_i32.to_le_bytes());
        body.extend_from_slice(&0_u32.to_le_bytes());
        body.extend_from_slice(&1_u64.to_le_bytes());
        body.push(image_type);
        body.push(0);
        body.extend_from_slice(&1_u32.to_le_bytes());
        body.extend_from_slice(&1_u32.to_le_bytes());
        body
    }

    #[test]
    fn lz_draw_copy_bounds_binary_data_and_palette_offsets() {
        let mut lz_rgb = one_pixel_draw_copy_envelope(101);
        lz_rgb.extend_from_slice(&3_u32.to_le_bytes());
        lz_rgb.extend_from_slice(&[1, 2, 3]);
        let update =
            CompressedImageUpdate::decode_draw_copy(&lz_rgb, 3).expect("bounded LZ_RGB payload");
        assert_eq!(update.image_type, DrawCopyImageType::LzRgb);
        assert_eq!(update.compressed_bytes, &[1, 2, 3]);

        lz_rgb.pop();
        let error = CompressedImageUpdate::decode_draw_copy(&lz_rgb, 3)
            .expect_err("declared LZ bytes must be present");
        assert_eq!(error.kind, DecodeErrorKind::Truncated);

        let mut lz_palette = one_pixel_draw_copy_envelope(100);
        lz_palette.push(0);
        lz_palette.extend_from_slice(&1_u32.to_le_bytes());
        lz_palette.extend_from_slice(&u32::MAX.to_le_bytes());
        lz_palette.push(0);
        let error = CompressedImageUpdate::decode_draw_copy(&lz_palette, 1)
            .expect_err("palette pointer must resolve inside the message");
        assert_eq!(error.kind, DecodeErrorKind::InvalidOffset);

        let mut zlib_glz = one_pixel_draw_copy_envelope(107);
        zlib_glz.extend_from_slice(&33_u32.to_le_bytes());
        zlib_glz.extend_from_slice(&3_u32.to_le_bytes());
        zlib_glz.extend_from_slice(&[1, 2, 3]);
        let update = CompressedImageUpdate::decode_draw_copy(&zlib_glz, 64)
            .expect("bounded zlib GLZ wrapper");
        assert_eq!(update.image_type, DrawCopyImageType::ZlibGlzRgb);
        assert_eq!(update.uncompressed_bytes, Some(33));
        assert_eq!(update.compressed_bytes, &[1, 2, 3]);

        let mut quic = one_pixel_draw_copy_envelope(1);
        quic.extend_from_slice(&4_u32.to_le_bytes());
        quic.extend_from_slice(&[4, 3, 2, 1]);
        let update =
            CompressedImageUpdate::decode_draw_copy(&quic, 4).expect("bounded QUIC binary payload");
        assert_eq!(update.image_type, DrawCopyImageType::Quic);
        assert_eq!(update.compressed_bytes, &[4, 3, 2, 1]);
    }

    #[test]
    fn jpeg_wrappers_split_codec_streams_with_checked_sizes() {
        let mut jpeg = one_pixel_draw_copy_envelope(105);
        jpeg.extend_from_slice(&3_u32.to_le_bytes());
        jpeg.extend_from_slice(&[1, 2, 3]);
        let update = JpegUpdate::decode_draw_copy(&jpeg, 3).expect("bounded JPEG payload");
        assert_eq!(update.jpeg_bytes, &[1, 2, 3]);
        assert_eq!(update.alpha_lz_bytes, None);

        let mut jpeg_alpha = one_pixel_draw_copy_envelope(108);
        jpeg_alpha.push(1);
        jpeg_alpha.extend_from_slice(&2_u32.to_le_bytes());
        jpeg_alpha.extend_from_slice(&5_u32.to_le_bytes());
        jpeg_alpha.extend_from_slice(&[1, 2, 3, 4, 5]);
        let update =
            JpegUpdate::decode_draw_copy(&jpeg_alpha, 5).expect("bounded JPEG alpha payload");
        assert_eq!(update.alpha_top_down, Some(true));
        assert_eq!(update.jpeg_bytes, &[1, 2]);
        assert_eq!(update.alpha_lz_bytes, Some(&[3, 4, 5][..]));

        jpeg_alpha[76..80].copy_from_slice(&6_u32.to_le_bytes());
        let error = JpegUpdate::decode_draw_copy(&jpeg_alpha, 6)
            .expect_err("JPEG bytes cannot exceed combined data");
        assert_eq!(error.kind, DecodeErrorKind::InvalidValue);
    }

    #[test]
    fn draw_copy_rejects_bitmap_stride_smaller_than_row() {
        let mut body = Vec::new();
        body.extend_from_slice(&0_u32.to_le_bytes());
        for coordinate in [0_i32, 0, 1, 2] {
            body.extend_from_slice(&coordinate.to_le_bytes());
        }
        body.push(0);
        let image_offset = 57_u32;
        body.extend_from_slice(&image_offset.to_le_bytes());
        for coordinate in [0_i32, 0, 1, 2] {
            body.extend_from_slice(&coordinate.to_le_bytes());
        }
        body.extend_from_slice(&0_u16.to_le_bytes());
        body.push(0);
        body.push(0);
        body.extend_from_slice(&0_i32.to_le_bytes());
        body.extend_from_slice(&0_i32.to_le_bytes());
        body.extend_from_slice(&0_u32.to_le_bytes());
        body.extend_from_slice(&0_u64.to_le_bytes());
        body.push(0);
        body.push(0);
        body.extend_from_slice(&2_u32.to_le_bytes());
        body.extend_from_slice(&1_u32.to_le_bytes());
        body.push(BitmapFormat::Xrgb32 as u8);
        body.push(1 << 2);
        body.extend_from_slice(&2_u32.to_le_bytes());
        body.extend_from_slice(&1_u32.to_le_bytes());
        body.extend_from_slice(&4_u32.to_le_bytes());
        body.extend_from_slice(&0_u32.to_le_bytes());
        body.extend_from_slice(&[0; 4]);

        let error = BitmapUpdate::decode_draw_copy(&body, 1024)
            .expect_err("short stride must fail before row access");
        assert_eq!(error.kind, DecodeErrorKind::InvalidValue);

        let mut bgr24 = body.clone();
        bgr24[75] = BitmapFormat::Bgr24 as u8;
        bgr24[85..89].copy_from_slice(&6_u32.to_le_bytes());
        bgr24.resize(99, 0);
        let update = BitmapUpdate::decode_draw_copy(&bgr24, 1024)
            .expect("24-bit stride uses three bytes per pixel");
        assert_eq!(update.pixel_bytes.len(), 6);

        let mut indexed = body.clone();
        indexed[75] = BitmapFormat::Indexed8 as u8;
        indexed[76] = 1;
        indexed[85..89].copy_from_slice(&2_u32.to_le_bytes());
        indexed[89..93].copy_from_slice(&95_u32.to_le_bytes());
        indexed[93] = 0;
        indexed[94] = 1;
        indexed.truncate(95);
        indexed.extend_from_slice(&55_u64.to_le_bytes());
        indexed.extend_from_slice(&2_u16.to_le_bytes());
        indexed.extend_from_slice(&[0, 0, 0, 0, 0, 0, 255, 0]);
        let update = BitmapUpdate::decode_draw_copy(&indexed, 1024)
            .expect("inline 8-bit palette is bounded and addressable");
        assert_eq!(update.pixel_bytes, &[0, 1]);
        assert!(matches!(
            update.palette,
            Some(BitmapPalette::Inline {
                unique_id: 55,
                cache_me: true,
                ..
            })
        ));

        let mut cached = body.clone();
        cached[75] = BitmapFormat::Indexed8 as u8;
        cached[76] = 1 << 1;
        cached[85..89].copy_from_slice(&2_u32.to_le_bytes());
        cached.truncate(89);
        cached.extend_from_slice(&55_u64.to_le_bytes());
        cached.extend_from_slice(&[0, 1]);
        let update = BitmapUpdate::decode_draw_copy(&cached, 1024)
            .expect("cached palette uses the eight-byte union arm");
        assert_eq!(update.pixel_bytes, &[0, 1]);
        assert_eq!(
            update.palette,
            Some(BitmapPalette::Cached { unique_id: 55 })
        );

        // Palette flags on a direct-color bitmap are invalid before union bytes are interpreted.
        body[76] = 1 << 1;
        body[85..89].copy_from_slice(&8_u32.to_le_bytes());
        let error = BitmapUpdate::decode_draw_copy(&body, 1024)
            .expect_err("direct-color bitmap cannot reference a palette cache");
        assert_eq!(error.kind, DecodeErrorKind::InvalidValue);
    }

    #[test]
    fn monitor_config_requires_an_exact_bounded_head_array() {
        let mut body = 1_u16.to_le_bytes().to_vec();
        body.extend_from_slice(&4_u16.to_le_bytes());
        for field in [7_u32, 9, 1920, 1080, 100, 200, 1] {
            body.extend_from_slice(&field.to_le_bytes());
        }
        let config = MonitorsConfig::decode(&body).expect("valid monitor config");
        assert_eq!(config.maximum_allowed, 4);
        assert_eq!(config.heads[0].monitor_id, 7);
        assert_eq!((config.heads[0].x, config.heads[0].y), (100, 200));

        body.push(0);
        let error = MonitorsConfig::decode(&body).expect_err("trailing bytes must fail");
        assert_eq!(error.kind, DecodeErrorKind::InvalidValue);
    }

    #[test]
    fn stream_messages_enforce_geometry_and_frame_size_boundaries() {
        let mut create = Vec::new();
        create.extend_from_slice(&0_u32.to_le_bytes());
        create.extend_from_slice(&7_u32.to_le_bytes());
        create.push(1);
        create.push(VideoCodec::Mjpeg as u8);
        create.extend_from_slice(&9_u64.to_le_bytes());
        for dimension in [640_u32, 480, 640, 480] {
            create.extend_from_slice(&dimension.to_le_bytes());
        }
        for coordinate in [0_i32, 0, 480, 640] {
            create.extend_from_slice(&coordinate.to_le_bytes());
        }
        create.push(0);
        let stream = StreamCreate::decode(&create).expect("Display stream create");
        assert_eq!(stream.stream_id, 7);
        assert_eq!(stream.destination.width().expect("destination width"), 640);

        let mut frame = Vec::new();
        frame.extend_from_slice(&7_u32.to_le_bytes());
        frame.extend_from_slice(&123_u32.to_le_bytes());
        frame.extend_from_slice(&3_u32.to_le_bytes());
        frame.extend_from_slice(&[1, 2, 3]);
        assert_eq!(
            StreamData::decode(&frame, false)
                .expect("Display stream frame")
                .data,
            &[1, 2, 3]
        );
        frame[8..12].copy_from_slice(&4_u32.to_le_bytes());
        let error = StreamData::decode(&frame, false).expect_err("truncated stream frame");
        assert_eq!(error.kind, DecodeErrorKind::Truncated);
    }
}
