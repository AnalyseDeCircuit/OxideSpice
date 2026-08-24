//! Decoder for the stateful SPICE GLZ image format.

use std::sync::Arc;

use thiserror::Error;

use crate::{DecodeLimits, LzImageType};

const HEADER_BYTES: usize = 33;
const LZ_MAGIC: u32 = 0x2020_5a4c;
const LZ_VERSION: u32 = 0x0001_0001;
const LITERAL_CONTROL_LIMIT: u8 = 32;
const EXTENDED_LENGTH_MARKER: usize = 7;
const CANCELLATION_CHECK_INTERVAL: usize = 4096;

/// One decoded image retained in stream order for future dictionary references.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodedGlzImage {
    pub image_id: u64,
    pub oldest_retained_image_id: u64,
    pub width: u32,
    pub height: u32,
    pub top_down: bool,
    pub pixels: Arc<[u8]>,
}

/// Stable categories for malformed or incomplete GLZ data.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GlzErrorKind {
    Truncated,
    InvalidHeader,
    InvalidBackReference,
    MissingReference,
    OutputOverflow,
    ResourceLimit,
    UnsupportedType,
    TrailingData,
    Cancelled,
}

/// A GLZ failure that never retains peer-controlled input bytes.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("SPICE GLZ {context}: {kind:?}")]
pub struct GlzError {
    pub kind: GlzErrorKind,
    pub context: &'static str,
    pub missing_image_id: Option<u64>,
}

impl GlzError {
    const fn new(kind: GlzErrorKind, context: &'static str) -> Self {
        Self {
            kind,
            context,
            missing_image_id: None,
        }
    }

    const fn missing(image_id: u64) -> Self {
        Self {
            kind: GlzErrorKind::MissingReference,
            context: "dictionary image",
            missing_image_id: Some(image_id),
        }
    }
}

/// Decodes one complete GLZ image using previously retained stream-order RGBA images.
pub fn decode_glz_with_cancel(
    input: &[u8],
    limits: DecodeLimits,
    mut resolve_image: impl FnMut(u64) -> Option<Arc<[u8]>>,
    mut should_cancel: impl FnMut() -> bool,
) -> Result<DecodedGlzImage, GlzError> {
    if should_cancel() {
        return Err(GlzError::new(GlzErrorKind::Cancelled, "decode"));
    }
    if input.len() < HEADER_BYTES {
        return Err(GlzError::new(GlzErrorKind::Truncated, "header"));
    }
    let mut reader = Reader::new(input);
    if reader.u32("magic")? != LZ_MAGIC || reader.u32("version")? != LZ_VERSION {
        return Err(GlzError::new(
            GlzErrorKind::InvalidHeader,
            "magic or version",
        ));
    }
    let type_and_orientation = reader.u8("image type")?;
    if type_and_orientation & !0x1f != 0 {
        return Err(GlzError::new(
            GlzErrorKind::InvalidHeader,
            "image type flags",
        ));
    }
    let image_type = LzImageType::try_from(u32::from(type_and_orientation & 0x0f))
        .map_err(|_| GlzError::new(GlzErrorKind::UnsupportedType, "image type"))?;
    if !matches!(
        image_type,
        LzImageType::Rgb16 | LzImageType::Rgb24 | LzImageType::Rgb32 | LzImageType::Rgba
    ) {
        return Err(GlzError::new(
            GlzErrorKind::UnsupportedType,
            "GLZ_RGB image type",
        ));
    }
    let top_down = type_and_orientation & 0x10 != 0;
    let width = reader.u32("width")?;
    let height = reader.u32("height")?;
    let stride = reader.u32("stride")?;
    let image_id = reader.u64("image id")?;
    let window_head_distance = u64::from(reader.u32("window head distance")?);
    let oldest_retained_image_id = image_id
        .checked_sub(window_head_distance)
        .ok_or_else(|| GlzError::new(GlzErrorKind::InvalidHeader, "window head distance"))?;
    if width == 0 || height == 0 || width > limits.maximum_width || height > limits.maximum_height {
        return Err(GlzError::new(GlzErrorKind::ResourceLimit, "dimensions"));
    }
    let width_usize =
        usize::try_from(width).map_err(|_| GlzError::new(GlzErrorKind::ResourceLimit, "width"))?;
    let height_usize = usize::try_from(height)
        .map_err(|_| GlzError::new(GlzErrorKind::ResourceLimit, "height"))?;
    let expected_stride = match image_type {
        LzImageType::Rgb16 => width_usize.checked_mul(2),
        LzImageType::Rgb24 => width_usize.checked_mul(3),
        LzImageType::Rgb32 | LzImageType::Rgba => width_usize.checked_mul(4),
        _ => unreachable!("unsupported GLZ image type rejected"),
    }
    .ok_or_else(|| GlzError::new(GlzErrorKind::ResourceLimit, "stride"))?;
    if usize::try_from(stride).ok() != Some(expected_stride) {
        return Err(GlzError::new(GlzErrorKind::InvalidHeader, "stride"));
    }
    let pixel_count = width_usize
        .checked_mul(height_usize)
        .ok_or_else(|| GlzError::new(GlzErrorKind::ResourceLimit, "pixel count"))?;
    let output_bytes = pixel_count
        .checked_mul(4)
        .ok_or_else(|| GlzError::new(GlzErrorKind::ResourceLimit, "output size"))?;
    if output_bytes > limits.maximum_output_bytes {
        return Err(GlzError::new(GlzErrorKind::ResourceLimit, "output size"));
    }

    let mut pixels = Vec::with_capacity(output_bytes);
    decode_color_stream(
        &mut reader,
        &mut pixels,
        pixel_count,
        image_type,
        image_id,
        &mut resolve_image,
        &mut should_cancel,
    )?;
    if image_type == LzImageType::Rgba {
        decode_alpha_stream(
            &mut reader,
            &mut pixels,
            pixel_count,
            image_id,
            &mut resolve_image,
            &mut should_cancel,
        )?;
    }
    if reader.remaining() != 0 {
        return Err(GlzError::new(GlzErrorKind::TrailingData, "compressed data"));
    }
    Ok(DecodedGlzImage {
        image_id,
        oldest_retained_image_id,
        width,
        height,
        top_down,
        pixels: pixels.into(),
    })
}

/// Expands the primary color stream into stream-order RGBA pixels.
fn decode_color_stream(
    reader: &mut Reader<'_>,
    output: &mut Vec<u8>,
    expected_pixels: usize,
    image_type: LzImageType,
    image_id: u64,
    resolve_image: &mut impl FnMut(u64) -> Option<Arc<[u8]>>,
    should_cancel: &mut impl FnMut() -> bool,
) -> Result<(), GlzError> {
    let literal_bytes = match image_type {
        LzImageType::Rgb16 => 2,
        LzImageType::Rgb24 | LzImageType::Rgb32 | LzImageType::Rgba => 3,
        _ => unreachable!("unsupported GLZ image type rejected"),
    };
    let match_bias = usize::from(image_type == LzImageType::Rgb16);
    let mut produced = 0_usize;
    while produced < expected_pixels {
        check_cancel(produced, should_cancel)?;
        let control = reader.u8("control byte")?;
        if control < LITERAL_CONTROL_LIMIT {
            let count = usize::from(control) + 1;
            ensure_output_room(produced, count, expected_pixels)?;
            for _ in 0..count {
                let encoded = reader.bytes(literal_bytes, "literal pixel")?;
                let rgba = if image_type == LzImageType::Rgb16 {
                    let value = u16::from_be_bytes([encoded[0], encoded[1]]);
                    [
                        expand_five_bits(((value >> 10) & 0x1f) as u8),
                        expand_five_bits(((value >> 5) & 0x1f) as u8),
                        expand_five_bits((value & 0x1f) as u8),
                        u8::MAX,
                    ]
                } else {
                    [encoded[2], encoded[1], encoded[0], u8::MAX]
                };
                output.extend_from_slice(&rgba);
                produced += 1;
            }
            continue;
        }
        let reference = decode_reference(reader, control, match_bias)?;
        ensure_output_room(produced, reference.length, expected_pixels)?;
        output.resize((produced + reference.length) * 4, 0);
        copy_reference(
            output,
            produced,
            reference,
            image_id,
            resolve_image,
            false,
            should_cancel,
        )?;
        produced += reference.length;
    }
    Ok(())
}

/// Applies the second RGBA stream to the alpha byte of existing pixels.
fn decode_alpha_stream(
    reader: &mut Reader<'_>,
    output: &mut [u8],
    expected_pixels: usize,
    image_id: u64,
    resolve_image: &mut impl FnMut(u64) -> Option<Arc<[u8]>>,
    should_cancel: &mut impl FnMut() -> bool,
) -> Result<(), GlzError> {
    let mut produced = 0_usize;
    while produced < expected_pixels {
        check_cancel(produced, should_cancel)?;
        let control = reader.u8("alpha control byte")?;
        if control < LITERAL_CONTROL_LIMIT {
            let count = usize::from(control) + 1;
            ensure_output_room(produced, count, expected_pixels)?;
            for pixel in produced..produced + count {
                output[pixel * 4 + 3] = reader.u8("literal alpha")?;
            }
            produced += count;
            continue;
        }
        let reference = decode_reference(reader, control, 2)?;
        ensure_output_room(produced, reference.length, expected_pixels)?;
        copy_reference(
            output,
            produced,
            reference,
            image_id,
            resolve_image,
            true,
            should_cancel,
        )?;
        produced += reference.length;
    }
    Ok(())
}

#[derive(Clone, Copy)]
struct Reference {
    length: usize,
    pixel_offset: usize,
    image_distance: u64,
}

/// Decodes the variable-width GLZ reference descriptor.
fn decode_reference(
    reader: &mut Reader<'_>,
    control: u8,
    match_bias: usize,
) -> Result<Reference, GlzError> {
    let mut length = usize::from(control >> 5);
    let long_pixel_offset = control & 0x10 != 0;
    let mut pixel_offset = usize::from(control & 0x0f);
    if length == EXTENDED_LENGTH_MARKER {
        loop {
            let extension = reader.u8("match length extension")?;
            length = length
                .checked_add(usize::from(extension))
                .ok_or_else(|| GlzError::new(GlzErrorKind::OutputOverflow, "match length"))?;
            if extension != u8::MAX {
                break;
            }
        }
    }
    pixel_offset = pixel_offset
        .checked_add(usize::from(reader.u8("pixel offset")?) << 4)
        .ok_or_else(|| GlzError::new(GlzErrorKind::InvalidBackReference, "pixel offset"))?;
    let descriptor = reader.u8("reference descriptor")?;
    let distance_bytes = usize::from(descriptor >> 6);
    let image_distance = if long_pixel_offset {
        pixel_offset = pixel_offset
            .checked_add(usize::from(descriptor & 0x1f) << 12)
            .ok_or_else(|| GlzError::new(GlzErrorKind::InvalidBackReference, "pixel offset"))?;
        let mut distance = 0_u64;
        for byte_index in 0..distance_bytes {
            distance |= u64::from(reader.u8("image distance")?) << (byte_index * 8);
        }
        if descriptor & 0x20 != 0 {
            pixel_offset = pixel_offset
                .checked_add(usize::from(reader.u8("long pixel offset")?) << 17)
                .ok_or_else(|| GlzError::new(GlzErrorKind::InvalidBackReference, "pixel offset"))?;
        }
        distance
    } else {
        let mut distance = u64::from(descriptor & 0x3f);
        for byte_index in 0..distance_bytes {
            distance |= u64::from(reader.u8("image distance")?) << (6 + byte_index * 8);
        }
        distance
    };
    length = length
        .checked_add(match_bias)
        .ok_or_else(|| GlzError::new(GlzErrorKind::OutputOverflow, "match length"))?;
    if image_distance == 0 {
        pixel_offset = pixel_offset
            .checked_add(1)
            .ok_or_else(|| GlzError::new(GlzErrorKind::InvalidBackReference, "pixel offset"))?;
    }
    Ok(Reference {
        length,
        pixel_offset,
        image_distance,
    })
}

/// Copies a local overlap reference or a prior dictionary image.
fn copy_reference(
    output: &mut [u8],
    produced: usize,
    reference: Reference,
    image_id: u64,
    resolve_image: &mut impl FnMut(u64) -> Option<Arc<[u8]>>,
    alpha_only: bool,
    should_cancel: &mut impl FnMut() -> bool,
) -> Result<(), GlzError> {
    if reference.length == 0 {
        return Err(GlzError::new(
            GlzErrorKind::InvalidBackReference,
            "zero match length",
        ));
    }
    if reference.image_distance == 0 {
        if reference.pixel_offset > produced {
            return Err(GlzError::new(
                GlzErrorKind::InvalidBackReference,
                "local pixel offset",
            ));
        }
        for copied in 0..reference.length {
            check_cancel(copied, should_cancel)?;
            let source_pixel = produced + copied - reference.pixel_offset;
            copy_pixel(output, source_pixel, produced + copied, alpha_only);
        }
        return Ok(());
    }
    let reference_id = image_id
        .checked_sub(reference.image_distance)
        .ok_or_else(|| GlzError::new(GlzErrorKind::InvalidBackReference, "image distance"))?;
    let source = resolve_image(reference_id).ok_or_else(|| GlzError::missing(reference_id))?;
    let source_pixels = source.len() / 4;
    if source.len() % 4 != 0
        || reference
            .pixel_offset
            .checked_add(reference.length)
            .is_none_or(|end| end > source_pixels)
    {
        return Err(GlzError::new(
            GlzErrorKind::InvalidBackReference,
            "dictionary pixel range",
        ));
    }
    for copied in 0..reference.length {
        check_cancel(copied, should_cancel)?;
        let source_start = (reference.pixel_offset + copied) * 4;
        let destination_start = (produced + copied) * 4;
        if alpha_only {
            output[destination_start + 3] = source[source_start + 3];
        } else {
            output[destination_start..destination_start + 4]
                .copy_from_slice(&source[source_start..source_start + 4]);
        }
    }
    Ok(())
}

fn copy_pixel(output: &mut [u8], source: usize, destination: usize, alpha_only: bool) {
    let source_start = source * 4;
    let destination_start = destination * 4;
    if alpha_only {
        output[destination_start + 3] = output[source_start + 3];
    } else {
        output.copy_within(source_start..source_start + 4, destination_start);
    }
}

fn ensure_output_room(produced: usize, additional: usize, expected: usize) -> Result<(), GlzError> {
    if produced
        .checked_add(additional)
        .is_none_or(|total| total > expected)
    {
        return Err(GlzError::new(GlzErrorKind::OutputOverflow, "stream pixels"));
    }
    Ok(())
}

fn check_cancel(progress: usize, should_cancel: &mut impl FnMut() -> bool) -> Result<(), GlzError> {
    if progress.is_multiple_of(CANCELLATION_CHECK_INTERVAL) && should_cancel() {
        return Err(GlzError::new(GlzErrorKind::Cancelled, "decode"));
    }
    Ok(())
}

const fn expand_five_bits(value: u8) -> u8 {
    (value << 3) | (value >> 2)
}

struct Reader<'a> {
    input: &'a [u8],
    offset: usize,
}

impl<'a> Reader<'a> {
    const fn new(input: &'a [u8]) -> Self {
        Self { input, offset: 0 }
    }

    fn remaining(&self) -> usize {
        self.input.len().saturating_sub(self.offset)
    }

    fn u8(&mut self, context: &'static str) -> Result<u8, GlzError> {
        let value = *self
            .input
            .get(self.offset)
            .ok_or_else(|| GlzError::new(GlzErrorKind::Truncated, context))?;
        self.offset += 1;
        Ok(value)
    }

    fn u32(&mut self, context: &'static str) -> Result<u32, GlzError> {
        Ok(u32::from_be_bytes(
            self.bytes(4, context)?
                .try_into()
                .expect("validated fixed-width integer"),
        ))
    }

    fn u64(&mut self, context: &'static str) -> Result<u64, GlzError> {
        Ok(u64::from_be_bytes(
            self.bytes(8, context)?
                .try_into()
                .expect("validated fixed-width integer"),
        ))
    }

    fn bytes(&mut self, length: usize, context: &'static str) -> Result<&'a [u8], GlzError> {
        let end = self
            .offset
            .checked_add(length)
            .ok_or_else(|| GlzError::new(GlzErrorKind::Truncated, context))?;
        let bytes = self
            .input
            .get(self.offset..end)
            .ok_or_else(|| GlzError::new(GlzErrorKind::Truncated, context))?;
        self.offset = end;
        Ok(bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header(
        image_type: LzImageType,
        width: u32,
        height: u32,
        image_id: u64,
        window_head_distance: u32,
    ) -> Vec<u8> {
        let mut output = Vec::new();
        output.extend_from_slice(&LZ_MAGIC.to_be_bytes());
        output.extend_from_slice(&LZ_VERSION.to_be_bytes());
        output.push((image_type as u8) | 0x10);
        output.extend_from_slice(&width.to_be_bytes());
        output.extend_from_slice(&height.to_be_bytes());
        output.extend_from_slice(&(width * 4).to_be_bytes());
        output.extend_from_slice(&image_id.to_be_bytes());
        output.extend_from_slice(&window_head_distance.to_be_bytes());
        output
    }

    #[test]
    fn literal_and_cross_image_reference_decode_in_stream_order() {
        let mut first = header(LzImageType::Rgb32, 1, 1, 0, 0);
        first.extend_from_slice(&[0, 1, 2, 3]);
        let first = decode_glz_with_cancel(&first, DecodeLimits::DISPLAY, |_| None, || false)
            .expect("first dictionary image");
        assert_eq!(first.pixels.as_ref(), &[3, 2, 1, 255]);

        let mut second = header(LzImageType::Rgb32, 1, 1, 1, 1);
        second.extend_from_slice(&[0x20, 0, 1]);
        let first_pixels = first.pixels.clone();
        let second = decode_glz_with_cancel(
            &second,
            DecodeLimits::DISPLAY,
            move |image_id| (image_id == 0).then(|| first_pixels.clone()),
            || false,
        )
        .expect("cross-image reference");
        assert_eq!(second.pixels.as_ref(), &[3, 2, 1, 255]);
        assert_eq!(second.oldest_retained_image_id, 0);
    }

    #[test]
    fn missing_and_invalid_references_are_distinct() {
        let mut missing = header(LzImageType::Rgb32, 1, 1, 1, 1);
        missing.extend_from_slice(&[0x20, 0, 1]);
        let error = decode_glz_with_cancel(&missing, DecodeLimits::DISPLAY, |_| None, || false)
            .expect_err("missing prior image");
        assert_eq!(error.kind, GlzErrorKind::MissingReference);
        assert_eq!(error.missing_image_id, Some(0));

        let mut invalid = header(LzImageType::Rgb32, 1, 1, 0, 0);
        invalid.extend_from_slice(&[0x20, 0, 1]);
        let error = decode_glz_with_cancel(&invalid, DecodeLimits::DISPLAY, |_| None, || false)
            .expect_err("image distance underflow");
        assert_eq!(error.kind, GlzErrorKind::InvalidBackReference);
    }

    #[test]
    fn rgba_alpha_stream_overlays_color_stream() {
        let mut encoded = header(LzImageType::Rgba, 1, 1, 4, 0);
        encoded.extend_from_slice(&[0, 1, 2, 3, 0, 0x80]);
        let decoded = decode_glz_with_cancel(&encoded, DecodeLimits::DISPLAY, |_| None, || false)
            .expect("RGBA streams");
        assert_eq!(decoded.pixels.as_ref(), &[3, 2, 1, 0x80]);
    }
}
