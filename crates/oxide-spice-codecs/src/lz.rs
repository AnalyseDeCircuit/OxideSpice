//! Decoder for the SPICE LZ 1.1 image format.

use thiserror::Error;

const HEADER_BYTES: usize = 7 * size_of::<u32>();
const LZ_MAGIC: u32 = 0x2020_5a4c;
const LZ_VERSION: u32 = 0x0001_0001;
const LITERAL_CONTROL_LIMIT: u8 = 32;
const EXTENDED_LENGTH_MARKER: usize = 6;
const LENGTH_EXTENSION_MAX: u8 = u8::MAX;
const NEAR_DISTANCE_PREFIX: usize = 31 << 8;
const NEAR_DISTANCE_MAX: usize = 8191;
const MAX_WINDOW_UNITS: usize = 1 << 25;
const CANCELLATION_CHECK_INTERVAL: usize = 4096;

/// Caller-selected limits applied before allocating decoded pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DecodeLimits {
    pub maximum_width: u32,
    pub maximum_height: u32,
    pub maximum_output_bytes: usize,
}

impl DecodeLimits {
    /// A conservative bound suitable for a desktop Display channel.
    pub const DISPLAY: Self = Self {
        maximum_width: 16_384,
        maximum_height: 16_384,
        maximum_output_bytes: 256 * 1024 * 1024,
    };
}

/// Pixel representation declared in the SPICE LZ header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum LzImageType {
    Palette1Le = 1,
    Palette1Be = 2,
    Palette4Le = 3,
    Palette4Be = 4,
    Palette8 = 5,
    Rgb16 = 6,
    Rgb24 = 7,
    Rgb32 = 8,
    Rgba = 9,
    XxxAlpha = 10,
    Alpha8 = 11,
}

impl TryFrom<u32> for LzImageType {
    type Error = LzError;

    fn try_from(value: u32) -> Result<Self, Self::Error> {
        match value {
            1 => Ok(Self::Palette1Le),
            2 => Ok(Self::Palette1Be),
            3 => Ok(Self::Palette4Le),
            4 => Ok(Self::Palette4Be),
            5 => Ok(Self::Palette8),
            6 => Ok(Self::Rgb16),
            7 => Ok(Self::Rgb24),
            8 => Ok(Self::Rgb32),
            9 => Ok(Self::Rgba),
            10 => Ok(Self::XxxAlpha),
            11 => Ok(Self::Alpha8),
            _ => Err(LzError::new(LzErrorKind::UnsupportedType, "image type")),
        }
    }
}

impl LzImageType {
    /// Returns the exact encoded stride for the declared image width.
    fn expected_stride(self, width: usize) -> Option<usize> {
        match self {
            Self::Palette1Le | Self::Palette1Be => width.checked_add(7).map(|value| value / 8),
            Self::Palette4Le | Self::Palette4Be => width.checked_add(1).map(|value| value / 2),
            Self::Palette8 | Self::Alpha8 => Some(width),
            Self::Rgb16 => width.checked_mul(2),
            Self::Rgb24 => width.checked_mul(3),
            Self::Rgb32 | Self::Rgba | Self::XxxAlpha => width.checked_mul(4),
        }
    }

    /// Returns the minimum match length encoded by a back-reference.
    const fn match_length_bias(self) -> usize {
        match self {
            Self::Palette1Le
            | Self::Palette1Be
            | Self::Palette4Le
            | Self::Palette4Be
            | Self::Palette8
            | Self::XxxAlpha
            | Self::Alpha8 => 3,
            Self::Rgb16 => 2,
            Self::Rgb24 | Self::Rgb32 | Self::Rgba => 1,
        }
    }

    /// Returns the compressed literal width of one logical unit.
    const fn compressed_unit_bytes(self) -> usize {
        match self {
            Self::Rgb16 => 2,
            Self::Rgb24 | Self::Rgb32 | Self::Rgba => 3,
            _ => 1,
        }
    }

    /// Returns whether the output requires a palette supplied by the outer image.
    const fn is_palette(self) -> bool {
        matches!(
            self,
            Self::Palette1Le
                | Self::Palette1Be
                | Self::Palette4Le
                | Self::Palette4Be
                | Self::Palette8
        )
    }
}

/// Canonical decoded storage returned to the Display channel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DecodedPixels {
    /// Top-down red, green, blue, alpha pixels.
    Rgba(Vec<u8>),
    /// Top-down alpha samples for mask-oriented image types.
    Alpha8(Vec<u8>),
}

/// A validated decoded image with rows normalized to top-down order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodedImage {
    pub width: u32,
    pub height: u32,
    /// The row direction declared by the LZ header before output normalization.
    pub top_down: bool,
    pub image_type: LzImageType,
    pub pixels: DecodedPixels,
}

/// Stable categories for malformed, unsupported, or cancelled LZ data.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LzErrorKind {
    Truncated,
    InvalidHeader,
    InvalidBackReference,
    OutputOverflow,
    ResourceLimit,
    UnsupportedType,
    MissingPalette,
    UnexpectedPalette,
    InvalidPaletteIndex,
    TrailingData,
    Cancelled,
}

/// An LZ failure that never retains peer-controlled input bytes.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("SPICE LZ {context}: {kind:?}")]
pub struct LzError {
    pub kind: LzErrorKind,
    pub context: &'static str,
}

impl LzError {
    const fn new(kind: LzErrorKind, context: &'static str) -> Self {
        Self { kind, context }
    }
}

/// Decodes one complete SPICE LZ payload without cancellation.
pub fn decode_lz(
    input: &[u8],
    palette: Option<&[[u8; 4]]>,
    limits: DecodeLimits,
) -> Result<DecodedImage, LzError> {
    decode_lz_with_cancel(input, palette, limits, || false)
}

/// Decodes one complete SPICE LZ payload and polls cooperative cancellation.
pub fn decode_lz_with_cancel(
    input: &[u8],
    palette: Option<&[[u8; 4]]>,
    limits: DecodeLimits,
    mut should_cancel: impl FnMut() -> bool,
) -> Result<DecodedImage, LzError> {
    if should_cancel() {
        return Err(LzError::new(LzErrorKind::Cancelled, "decode"));
    }
    if input.len() < HEADER_BYTES {
        return Err(LzError::new(LzErrorKind::Truncated, "header"));
    }
    let mut reader = BigEndianReader::new(input);
    if reader.u32("magic")? != LZ_MAGIC || reader.u32("version")? != LZ_VERSION {
        return Err(LzError::new(LzErrorKind::InvalidHeader, "magic or version"));
    }
    let image_type = LzImageType::try_from(reader.u32("image type")?)?;
    let width = reader.u32("width")?;
    let height = reader.u32("height")?;
    let stride = reader.u32("stride")?;
    let top_down = match reader.u32("row orientation")? {
        0 => false,
        1 => true,
        _ => {
            return Err(LzError::new(LzErrorKind::InvalidHeader, "row orientation"));
        }
    };
    if width == 0 || height == 0 || width > limits.maximum_width || height > limits.maximum_height {
        return Err(LzError::new(LzErrorKind::ResourceLimit, "dimensions"));
    }
    let width_usize =
        usize::try_from(width).map_err(|_| LzError::new(LzErrorKind::ResourceLimit, "width"))?;
    let height_usize =
        usize::try_from(height).map_err(|_| LzError::new(LzErrorKind::ResourceLimit, "height"))?;
    let stride_usize =
        usize::try_from(stride).map_err(|_| LzError::new(LzErrorKind::ResourceLimit, "stride"))?;
    if image_type.expected_stride(width_usize) != Some(stride_usize) {
        return Err(LzError::new(LzErrorKind::InvalidHeader, "stride"));
    }
    let pixel_count = width_usize
        .checked_mul(height_usize)
        .ok_or_else(|| LzError::new(LzErrorKind::ResourceLimit, "pixel count"))?;
    let output_bytes_per_pixel =
        if matches!(image_type, LzImageType::XxxAlpha | LzImageType::Alpha8) {
            1
        } else {
            4
        };
    let output_bytes = pixel_count
        .checked_mul(output_bytes_per_pixel)
        .ok_or_else(|| LzError::new(LzErrorKind::ResourceLimit, "output size"))?;
    if output_bytes > limits.maximum_output_bytes {
        return Err(LzError::new(LzErrorKind::ResourceLimit, "output size"));
    }
    if image_type.is_palette() && palette.is_none() {
        return Err(LzError::new(LzErrorKind::MissingPalette, "palette"));
    }
    if !image_type.is_palette() && palette.is_some() {
        return Err(LzError::new(LzErrorKind::UnexpectedPalette, "palette"));
    }
    let geometry = ImageGeometry {
        width: width_usize,
        height: height_usize,
        top_down,
    };

    let pixels = match image_type {
        LzImageType::Palette1Le
        | LzImageType::Palette1Be
        | LzImageType::Palette4Le
        | LzImageType::Palette4Be
        | LzImageType::Palette8 => {
            let unit_count = stride_usize
                .checked_mul(height_usize)
                .ok_or_else(|| LzError::new(LzErrorKind::ResourceLimit, "palette units"))?;
            let indices = decompress_units(
                &mut reader,
                unit_count,
                image_type.compressed_unit_bytes(),
                image_type.match_length_bias(),
                &mut should_cancel,
            )?;
            DecodedPixels::Rgba(expand_palette(
                &indices,
                palette.expect("validated palette presence"),
                image_type,
                geometry,
                stride_usize,
                &mut should_cancel,
            )?)
        }
        LzImageType::Rgb16 | LzImageType::Rgb24 | LzImageType::Rgb32 => {
            let colors = decompress_units(
                &mut reader,
                pixel_count,
                image_type.compressed_unit_bytes(),
                image_type.match_length_bias(),
                &mut should_cancel,
            )?;
            DecodedPixels::Rgba(expand_direct_color(
                &colors,
                image_type,
                width_usize,
                height_usize,
                top_down,
                &mut should_cancel,
            )?)
        }
        LzImageType::Rgba => {
            let colors = decompress_units(
                &mut reader,
                pixel_count,
                image_type.compressed_unit_bytes(),
                image_type.match_length_bias(),
                &mut should_cancel,
            )?;
            let alpha = decompress_units(
                &mut reader,
                pixel_count,
                1,
                LzImageType::XxxAlpha.match_length_bias(),
                &mut should_cancel,
            )?;
            DecodedPixels::Rgba(expand_rgba(
                &colors,
                &alpha,
                width_usize,
                height_usize,
                top_down,
                &mut should_cancel,
            )?)
        }
        LzImageType::XxxAlpha | LzImageType::Alpha8 => {
            let alpha = decompress_units(
                &mut reader,
                pixel_count,
                1,
                image_type.match_length_bias(),
                &mut should_cancel,
            )?;
            DecodedPixels::Alpha8(orient_rows(
                &alpha,
                width_usize,
                height_usize,
                top_down,
                &mut should_cancel,
            )?)
        }
    };
    if reader.remaining() != 0 {
        return Err(LzError::new(LzErrorKind::TrailingData, "compressed data"));
    }
    if should_cancel() {
        return Err(LzError::new(LzErrorKind::Cancelled, "decode"));
    }
    Ok(DecodedImage {
        width,
        height,
        top_down,
        image_type,
        pixels,
    })
}

/// Expands one independently compressed stream into fixed-width logical units.
fn decompress_units(
    reader: &mut BigEndianReader<'_>,
    expected_units: usize,
    unit_bytes: usize,
    match_bias: usize,
    should_cancel: &mut impl FnMut() -> bool,
) -> Result<Vec<u8>, LzError> {
    let output_bytes = expected_units
        .checked_mul(unit_bytes)
        .ok_or_else(|| LzError::new(LzErrorKind::ResourceLimit, "stream output"))?;
    let mut output = Vec::with_capacity(output_bytes);
    let mut produced_units = 0_usize;
    while produced_units < expected_units {
        if produced_units.is_multiple_of(CANCELLATION_CHECK_INTERVAL) && should_cancel() {
            return Err(LzError::new(LzErrorKind::Cancelled, "decode"));
        }
        let control = reader.u8("control byte")?;
        if control < LITERAL_CONTROL_LIMIT {
            let literal_units = usize::from(control) + 1;
            ensure_output_room(produced_units, literal_units, expected_units)?;
            let literal_bytes = literal_units
                .checked_mul(unit_bytes)
                .ok_or_else(|| LzError::new(LzErrorKind::OutputOverflow, "literal bytes"))?;
            output.extend_from_slice(reader.bytes(literal_bytes, "literal bytes")?);
            produced_units += literal_units;
            continue;
        }

        let mut match_units = usize::from(control >> 5).saturating_sub(1);
        let distance_prefix = usize::from(control & 0x1f) << 8;
        if match_units == EXTENDED_LENGTH_MARKER {
            let mut extension_bytes = 0_usize;
            loop {
                let extension = reader.u8("match length extension")?;
                match_units = match_units
                    .checked_add(usize::from(extension))
                    .ok_or_else(|| LzError::new(LzErrorKind::OutputOverflow, "match length"))?;
                extension_bytes += 1;
                if extension_bytes.is_multiple_of(CANCELLATION_CHECK_INTERVAL) && should_cancel() {
                    return Err(LzError::new(LzErrorKind::Cancelled, "decode"));
                }
                if extension != LENGTH_EXTENSION_MAX {
                    break;
                }
            }
        }
        let distance_low = reader.u8("match distance")?;
        let encoded_distance = distance_prefix + usize::from(distance_low);
        let mut distance = if distance_low == u8::MAX && distance_prefix == NEAR_DISTANCE_PREFIX {
            let far_high = usize::from(reader.u8("far match distance")?);
            let far_low = usize::from(reader.u8("far match distance")?);
            (far_high << 8)
                .checked_add(far_low)
                .and_then(|value| value.checked_add(NEAR_DISTANCE_MAX))
                .ok_or_else(|| LzError::new(LzErrorKind::InvalidBackReference, "distance"))?
        } else {
            encoded_distance
        };
        distance = distance
            .checked_add(1)
            .ok_or_else(|| LzError::new(LzErrorKind::InvalidBackReference, "distance"))?;
        match_units = match_units
            .checked_add(match_bias)
            .ok_or_else(|| LzError::new(LzErrorKind::OutputOverflow, "match length"))?;
        if distance == 0 || distance > produced_units || distance > MAX_WINDOW_UNITS {
            return Err(LzError::new(LzErrorKind::InvalidBackReference, "distance"));
        }
        ensure_output_room(produced_units, match_units, expected_units)?;
        for copied_units in 0..match_units {
            if copied_units.is_multiple_of(CANCELLATION_CHECK_INTERVAL) && should_cancel() {
                return Err(LzError::new(LzErrorKind::Cancelled, "decode"));
            }
            let source_unit = produced_units - distance;
            let source_start = source_unit * unit_bytes;
            let source_end = source_start + unit_bytes;
            output.extend_from_within(source_start..source_end);
            produced_units += 1;
        }
    }
    debug_assert_eq!(output.len(), output_bytes);
    Ok(output)
}

fn ensure_output_room(produced: usize, additional: usize, expected: usize) -> Result<(), LzError> {
    if produced
        .checked_add(additional)
        .is_none_or(|total| total > expected)
    {
        return Err(LzError::new(LzErrorKind::OutputOverflow, "stream units"));
    }
    Ok(())
}

fn expand_direct_color(
    source: &[u8],
    image_type: LzImageType,
    width: usize,
    height: usize,
    top_down: bool,
    should_cancel: &mut impl FnMut() -> bool,
) -> Result<Vec<u8>, LzError> {
    let unit_bytes = image_type.compressed_unit_bytes();
    let mut output = vec![0; width * height * 4];
    for storage_y in 0..height {
        let output_y = oriented_y(storage_y, height, top_down);
        for x in 0..width {
            let source_pixel = storage_y * width + x;
            if source_pixel.is_multiple_of(CANCELLATION_CHECK_INTERVAL) && should_cancel() {
                return Err(LzError::new(LzErrorKind::Cancelled, "pixel conversion"));
            }
            let source_start = source_pixel * unit_bytes;
            let destination_start = (output_y * width + x) * 4;
            let rgba = match image_type {
                LzImageType::Rgb16 => {
                    let value =
                        u16::from_be_bytes([source[source_start], source[source_start + 1]]);
                    let red = expand_five_bits(((value >> 10) & 0x1f) as u8);
                    let green = expand_five_bits(((value >> 5) & 0x1f) as u8);
                    let blue = expand_five_bits((value & 0x1f) as u8);
                    [red, green, blue, u8::MAX]
                }
                LzImageType::Rgb24 | LzImageType::Rgb32 => [
                    source[source_start + 2],
                    source[source_start + 1],
                    source[source_start],
                    u8::MAX,
                ],
                _ => unreachable!("direct-color expansion receives only direct-color types"),
            };
            output[destination_start..destination_start + 4].copy_from_slice(&rgba);
        }
    }
    Ok(output)
}

fn expand_rgba(
    colors: &[u8],
    alpha: &[u8],
    width: usize,
    height: usize,
    top_down: bool,
    should_cancel: &mut impl FnMut() -> bool,
) -> Result<Vec<u8>, LzError> {
    let mut output = vec![0; width * height * 4];
    for storage_y in 0..height {
        let output_y = oriented_y(storage_y, height, top_down);
        for x in 0..width {
            let source_pixel = storage_y * width + x;
            if source_pixel.is_multiple_of(CANCELLATION_CHECK_INTERVAL) && should_cancel() {
                return Err(LzError::new(LzErrorKind::Cancelled, "pixel conversion"));
            }
            let color_start = source_pixel * 3;
            let destination_start = (output_y * width + x) * 4;
            output[destination_start..destination_start + 4].copy_from_slice(&[
                colors[color_start + 2],
                colors[color_start + 1],
                colors[color_start],
                alpha[source_pixel],
            ]);
        }
    }
    Ok(output)
}

fn expand_palette(
    indices: &[u8],
    palette: &[[u8; 4]],
    image_type: LzImageType,
    geometry: ImageGeometry,
    stride: usize,
    should_cancel: &mut impl FnMut() -> bool,
) -> Result<Vec<u8>, LzError> {
    let mut output = vec![0; geometry.width * geometry.height * 4];
    for storage_y in 0..geometry.height {
        let output_y = oriented_y(storage_y, geometry.height, geometry.top_down);
        let row = &indices[storage_y * stride..(storage_y + 1) * stride];
        for x in 0..geometry.width {
            let source_pixel = storage_y * geometry.width + x;
            if source_pixel.is_multiple_of(CANCELLATION_CHECK_INTERVAL) && should_cancel() {
                return Err(LzError::new(LzErrorKind::Cancelled, "palette expansion"));
            }
            let index = match image_type {
                LzImageType::Palette1Le => usize::from((row[x / 8] >> (x % 8)) & 1),
                LzImageType::Palette1Be => usize::from((row[x / 8] >> (7 - x % 8)) & 1),
                LzImageType::Palette4Le => usize::from(if x & 1 == 0 {
                    row[x / 2] & 0x0f
                } else {
                    row[x / 2] >> 4
                }),
                LzImageType::Palette4Be => usize::from(if x & 1 == 0 {
                    row[x / 2] >> 4
                } else {
                    row[x / 2] & 0x0f
                }),
                LzImageType::Palette8 => usize::from(row[x]),
                _ => unreachable!("palette expansion receives only palette types"),
            };
            let color = palette
                .get(index)
                .ok_or_else(|| LzError::new(LzErrorKind::InvalidPaletteIndex, "palette index"))?;
            let destination_start = (output_y * geometry.width + x) * 4;
            output[destination_start..destination_start + 4].copy_from_slice(color);
        }
    }
    Ok(output)
}

#[derive(Clone, Copy)]
struct ImageGeometry {
    width: usize,
    height: usize,
    top_down: bool,
}

fn orient_rows(
    source: &[u8],
    width: usize,
    height: usize,
    top_down: bool,
    should_cancel: &mut impl FnMut() -> bool,
) -> Result<Vec<u8>, LzError> {
    if top_down {
        if should_cancel() {
            return Err(LzError::new(LzErrorKind::Cancelled, "row orientation"));
        }
        return Ok(source.to_vec());
    }
    let mut output = vec![0; source.len()];
    for storage_y in 0..height {
        if should_cancel() {
            return Err(LzError::new(LzErrorKind::Cancelled, "row orientation"));
        }
        let output_y = oriented_y(storage_y, height, false);
        output[output_y * width..(output_y + 1) * width]
            .copy_from_slice(&source[storage_y * width..(storage_y + 1) * width]);
    }
    Ok(output)
}

const fn oriented_y(storage_y: usize, height: usize, top_down: bool) -> usize {
    if top_down {
        storage_y
    } else {
        height - storage_y - 1
    }
}

const fn expand_five_bits(value: u8) -> u8 {
    (value << 3) | (value >> 2)
}

struct BigEndianReader<'a> {
    input: &'a [u8],
    offset: usize,
}

impl<'a> BigEndianReader<'a> {
    const fn new(input: &'a [u8]) -> Self {
        Self { input, offset: 0 }
    }

    fn remaining(&self) -> usize {
        self.input.len().saturating_sub(self.offset)
    }

    fn u8(&mut self, context: &'static str) -> Result<u8, LzError> {
        let byte = *self
            .input
            .get(self.offset)
            .ok_or_else(|| LzError::new(LzErrorKind::Truncated, context))?;
        self.offset += 1;
        Ok(byte)
    }

    fn u32(&mut self, context: &'static str) -> Result<u32, LzError> {
        let bytes: [u8; 4] = self
            .bytes(size_of::<u32>(), context)?
            .try_into()
            .expect("validated fixed-width integer");
        Ok(u32::from_be_bytes(bytes))
    }

    fn bytes(&mut self, length: usize, context: &'static str) -> Result<&'a [u8], LzError> {
        let end = self
            .offset
            .checked_add(length)
            .ok_or_else(|| LzError::new(LzErrorKind::Truncated, context))?;
        let bytes = self
            .input
            .get(self.offset..end)
            .ok_or_else(|| LzError::new(LzErrorKind::Truncated, context))?;
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
        stride: u32,
        top_down: bool,
    ) -> Vec<u8> {
        let mut output = Vec::with_capacity(HEADER_BYTES);
        for value in [
            LZ_MAGIC,
            LZ_VERSION,
            image_type as u32,
            width,
            height,
            stride,
            u32::from(top_down),
        ] {
            output.extend_from_slice(&value.to_be_bytes());
        }
        output
    }

    #[test]
    fn rgb32_literals_and_overlap_reference_decode_to_rgba() {
        let mut encoded = header(LzImageType::Rgb32, 4, 1, 16, true);
        encoded.extend_from_slice(&[0, 1, 2, 3]);
        encoded.extend_from_slice(&[0x60, 0]);
        let decoded = decode_lz(&encoded, None, DecodeLimits::DISPLAY).expect("valid LZ image");
        assert_eq!(
            decoded.pixels,
            DecodedPixels::Rgba(vec![3, 2, 1, 255, 3, 2, 1, 255, 3, 2, 1, 255, 3, 2, 1, 255,])
        );
    }

    #[test]
    fn extended_match_lengths_remain_bounded() {
        let mut encoded = header(LzImageType::Rgb32, 260, 1, 1040, true);
        encoded.extend_from_slice(&[0, 1, 2, 3]);
        encoded.extend_from_slice(&[0xe0, 252, 0]);
        let decoded = decode_lz(&encoded, None, DecodeLimits::DISPLAY)
            .expect("extended match length fits declared image");
        let DecodedPixels::Rgba(pixels) = decoded.pixels else {
            panic!("RGB32 must decode to RGBA");
        };
        assert_eq!(pixels.len(), 260 * 4);
        assert!(pixels.chunks_exact(4).all(|pixel| pixel == [3, 2, 1, 255]));
    }

    #[test]
    fn rgba_uses_a_separate_alpha_stream() {
        let mut encoded = header(LzImageType::Rgba, 2, 1, 8, true);
        encoded.extend_from_slice(&[1, 1, 2, 3, 4, 5, 6]);
        encoded.extend_from_slice(&[1, 0x40, 0x80]);
        let decoded = decode_lz(&encoded, None, DecodeLimits::DISPLAY).expect("valid RGBA image");
        assert_eq!(
            decoded.pixels,
            DecodedPixels::Rgba(vec![3, 2, 1, 0x40, 6, 5, 4, 0x80])
        );
    }

    #[test]
    fn bottom_up_palette_bits_are_normalized_and_bounds_checked() {
        let palette = [[1, 2, 3, 255], [4, 5, 6, 255]];
        let mut encoded = header(LzImageType::Palette1Be, 2, 2, 1, false);
        encoded.extend_from_slice(&[1, 0b0100_0000, 0b1000_0000]);
        let decoded = decode_lz(&encoded, Some(&palette), DecodeLimits::DISPLAY)
            .expect("valid palette image");
        assert_eq!(
            decoded.pixels,
            DecodedPixels::Rgba(vec![4, 5, 6, 255, 1, 2, 3, 255, 1, 2, 3, 255, 4, 5, 6, 255,])
        );

        let mut invalid = header(LzImageType::Palette8, 1, 1, 1, true);
        invalid.extend_from_slice(&[0, 2]);
        let error = decode_lz(&invalid, Some(&palette), DecodeLimits::DISPLAY)
            .expect_err("invalid palette index");
        assert_eq!(error.kind, LzErrorKind::InvalidPaletteIndex);
    }

    #[test]
    fn malformed_streams_fail_before_overwriting_output() {
        let mut back_reference = header(LzImageType::Rgb32, 1, 1, 4, true);
        back_reference.extend_from_slice(&[0x20, 0]);
        assert_eq!(
            decode_lz(&back_reference, None, DecodeLimits::DISPLAY)
                .expect_err("reference before output")
                .kind,
            LzErrorKind::InvalidBackReference
        );

        let mut overflow = header(LzImageType::Rgb32, 1, 1, 4, true);
        overflow.extend_from_slice(&[1, 1, 2, 3, 4, 5, 6]);
        assert_eq!(
            decode_lz(&overflow, None, DecodeLimits::DISPLAY)
                .expect_err("literal exceeds image")
                .kind,
            LzErrorKind::OutputOverflow
        );

        let mut trailing = header(LzImageType::Rgb32, 1, 1, 4, true);
        trailing.extend_from_slice(&[0, 1, 2, 3, 4]);
        trailing.push(0);
        assert_eq!(
            decode_lz(&trailing, None, DecodeLimits::DISPLAY)
                .expect_err("trailing input")
                .kind,
            LzErrorKind::TrailingData
        );
    }

    #[test]
    fn limits_and_cancellation_precede_large_allocations() {
        let encoded = header(LzImageType::Rgb32, 16_384, 16_384, 65_536, true);
        let error = decode_lz(
            &encoded,
            None,
            DecodeLimits {
                maximum_width: 16_384,
                maximum_height: 16_384,
                maximum_output_bytes: 1024,
            },
        )
        .expect_err("output limit");
        assert_eq!(error.kind, LzErrorKind::ResourceLimit);

        let error = decode_lz_with_cancel(&encoded, None, DecodeLimits::DISPLAY, || true)
            .expect_err("cancelled before parsing");
        assert_eq!(error.kind, LzErrorKind::Cancelled);
    }

    #[test]
    fn cancellation_interrupts_a_long_overlap_copy() {
        const MATCH_UNITS: usize = 8192;
        let mut encoded = header(
            LzImageType::Rgb32,
            u32::try_from(MATCH_UNITS + 1).expect("test width"),
            1,
            u32::try_from((MATCH_UNITS + 1) * 4).expect("test stride"),
            true,
        );
        encoded.extend_from_slice(&[0, 1, 2, 3]);
        encoded.push(0xe0);
        encoded.extend(std::iter::repeat_n(u8::MAX, 32));
        encoded.extend_from_slice(&[25, 0]);
        let mut cancellation_polls = 0;
        let error = decode_lz_with_cancel(&encoded, None, DecodeLimits::DISPLAY, || {
            cancellation_polls += 1;
            cancellation_polls >= 4
        })
        .expect_err("long match observes cancellation");
        assert_eq!(error.kind, LzErrorKind::Cancelled);
    }
}
