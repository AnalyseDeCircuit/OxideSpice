//! Checked decoder for the SPICE row-block LZ4 image container.

use lz4_flex::block::{compress, decompress_into, decompress_into_with_dict};
use thiserror::Error;

use crate::DecodeLimits;

const LZ4_IMAGE_HEADER_BYTES: usize = 2;
const LZ4_BLOCK_LENGTH_BYTES: usize = size_of::<u32>();

/// One decoded LZ4 image with canonical RGBA pixels.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodedLz4Image {
    pub width: u32,
    pub height: u32,
    pub top_down: bool,
    pub pixels: Vec<u8>,
}

/// Stable categories for malformed or unsupported SPICE LZ4 data.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lz4ErrorKind {
    Truncated,
    InvalidHeader,
    UnsupportedType,
    InvalidBlock,
    ResourceLimit,
    Cancelled,
}

/// An LZ4 failure that does not retain peer-controlled bytes.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("SPICE LZ4 {context}: {kind:?}")]
pub struct Lz4Error {
    pub kind: Lz4ErrorKind,
    pub context: &'static str,
}

impl Lz4Error {
    const fn new(kind: Lz4ErrorKind, context: &'static str) -> Self {
        Self { kind, context }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Lz4PixelFormat {
    Rgb16,
    Bgr24,
    Xrgb32,
    Rgba32,
}

impl Lz4PixelFormat {
    fn decode(value: u8) -> Result<Self, Lz4Error> {
        match value {
            6 => Ok(Self::Rgb16),
            7 => Ok(Self::Bgr24),
            8 => Ok(Self::Xrgb32),
            9 => Ok(Self::Rgba32),
            _ => Err(Lz4Error::new(Lz4ErrorKind::UnsupportedType, "pixel format")),
        }
    }

    const fn bytes_per_pixel(self) -> usize {
        match self {
            Self::Rgb16 => 2,
            Self::Bgr24 => 3,
            Self::Xrgb32 | Self::Rgba32 => 4,
        }
    }
}

/// Decodes one raw LZ4 block and requires its output to match the wire declaration exactly.
pub fn decode_lz4_block_exact(
    input: &[u8],
    expected_output_bytes: usize,
    maximum_output_bytes: usize,
) -> Result<Vec<u8>, Lz4Error> {
    if expected_output_bytes == 0 || expected_output_bytes > maximum_output_bytes {
        return Err(Lz4Error::new(
            Lz4ErrorKind::ResourceLimit,
            "raw block output bytes",
        ));
    }
    let mut output = vec![0; expected_output_bytes];
    let decoded_bytes = decompress_into(input, &mut output)
        .map_err(|_| Lz4Error::new(Lz4ErrorKind::InvalidBlock, "raw compressed block"))?;
    if decoded_bytes != expected_output_bytes {
        return Err(Lz4Error::new(
            Lz4ErrorKind::InvalidBlock,
            "raw block output size",
        ));
    }
    Ok(output)
}

/// Compresses one bounded raw block and returns it only when the wire body becomes smaller.
pub fn compress_lz4_block_if_smaller(
    input: &[u8],
    maximum_input_bytes: usize,
) -> Result<Option<Vec<u8>>, Lz4Error> {
    if input.is_empty() || input.len() > maximum_input_bytes {
        return Err(Lz4Error::new(
            Lz4ErrorKind::ResourceLimit,
            "raw block input bytes",
        ));
    }
    let compressed = compress(input);
    Ok((compressed.len() < input.len()).then_some(compressed))
}

/// Decodes every dictionary-linked LZ4 block and converts its pixels to RGBA.
pub fn decode_lz4_with_cancel<F>(
    input: &[u8],
    width: u32,
    height: u32,
    limits: DecodeLimits,
    mut cancelled: F,
) -> Result<DecodedLz4Image, Lz4Error>
where
    F: FnMut() -> bool,
{
    if input.len() < LZ4_IMAGE_HEADER_BYTES {
        return Err(Lz4Error::new(Lz4ErrorKind::Truncated, "image header"));
    }
    if width == 0 || height == 0 || width > limits.maximum_width || height > limits.maximum_height {
        return Err(Lz4Error::new(
            Lz4ErrorKind::ResourceLimit,
            "image dimensions",
        ));
    }
    let top_down = match input[0] {
        0 => false,
        1 => true,
        _ => {
            return Err(Lz4Error::new(
                Lz4ErrorKind::InvalidHeader,
                "row orientation",
            ));
        }
    };
    let format = Lz4PixelFormat::decode(input[1])?;
    let width = usize::try_from(width)
        .map_err(|_| Lz4Error::new(Lz4ErrorKind::ResourceLimit, "image width"))?;
    let height = usize::try_from(height)
        .map_err(|_| Lz4Error::new(Lz4ErrorKind::ResourceLimit, "image height"))?;
    let packed_stride = width
        .checked_mul(format.bytes_per_pixel())
        .ok_or_else(|| Lz4Error::new(Lz4ErrorKind::ResourceLimit, "packed row bytes"))?;
    let packed_bytes = packed_stride
        .checked_mul(height)
        .ok_or_else(|| Lz4Error::new(Lz4ErrorKind::ResourceLimit, "packed image bytes"))?;
    let rgba_bytes = width
        .checked_mul(height)
        .and_then(|pixels| pixels.checked_mul(4))
        .ok_or_else(|| Lz4Error::new(Lz4ErrorKind::ResourceLimit, "RGBA image bytes"))?;
    if packed_bytes
        .checked_add(rgba_bytes)
        .is_none_or(|peak_bytes| peak_bytes > limits.maximum_output_bytes)
    {
        return Err(Lz4Error::new(
            Lz4ErrorKind::ResourceLimit,
            "decoder working bytes",
        ));
    }

    let mut packed = vec![0; packed_bytes];
    let mut input_offset = LZ4_IMAGE_HEADER_BYTES;
    let mut output_offset = 0;
    while input_offset < input.len() {
        if cancelled() {
            return Err(Lz4Error::new(Lz4ErrorKind::Cancelled, "block decode"));
        }
        let length_end = input_offset
            .checked_add(LZ4_BLOCK_LENGTH_BYTES)
            .ok_or_else(|| Lz4Error::new(Lz4ErrorKind::ResourceLimit, "block length"))?;
        let length_bytes = input
            .get(input_offset..length_end)
            .ok_or_else(|| Lz4Error::new(Lz4ErrorKind::Truncated, "block length"))?;
        let compressed_bytes = usize::try_from(u32::from_be_bytes(
            length_bytes.try_into().expect("four-byte LZ4 block length"),
        ))
        .map_err(|_| Lz4Error::new(Lz4ErrorKind::ResourceLimit, "block length"))?;
        if compressed_bytes == 0 {
            return Err(Lz4Error::new(
                Lz4ErrorKind::InvalidBlock,
                "empty compressed block",
            ));
        }
        input_offset = length_end;
        let block_end = input_offset
            .checked_add(compressed_bytes)
            .ok_or_else(|| Lz4Error::new(Lz4ErrorKind::ResourceLimit, "compressed block"))?;
        let block = input
            .get(input_offset..block_end)
            .ok_or_else(|| Lz4Error::new(Lz4ErrorKind::Truncated, "compressed block"))?;
        let (dictionary, output) = packed.split_at_mut(output_offset);
        let decoded_bytes = decompress_into_with_dict(block, output, dictionary)
            .map_err(|_| Lz4Error::new(Lz4ErrorKind::InvalidBlock, "compressed block"))?;
        if decoded_bytes == 0 {
            return Err(Lz4Error::new(
                Lz4ErrorKind::InvalidBlock,
                "empty decoded block",
            ));
        }
        output_offset = output_offset
            .checked_add(decoded_bytes)
            .ok_or_else(|| Lz4Error::new(Lz4ErrorKind::ResourceLimit, "decoded bytes"))?;
        input_offset = block_end;
    }
    if output_offset != packed_bytes {
        return Err(Lz4Error::new(
            Lz4ErrorKind::InvalidBlock,
            "decoded image size",
        ));
    }

    let mut pixels = Vec::with_capacity(rgba_bytes);
    for row in packed.chunks_exact(packed_stride) {
        for pixel in row.chunks_exact(format.bytes_per_pixel()) {
            pixels.extend_from_slice(&decode_pixel(format, pixel));
        }
    }
    Ok(DecodedLz4Image {
        width: u32::try_from(width).expect("validated LZ4 width"),
        height: u32::try_from(height).expect("validated LZ4 height"),
        top_down,
        pixels,
    })
}

fn decode_pixel(format: Lz4PixelFormat, pixel: &[u8]) -> [u8; 4] {
    match format {
        Lz4PixelFormat::Rgb16 => {
            let value = u16::from_le_bytes([pixel[0], pixel[1]]);
            [
                expand_five_bits(((value >> 10) & 0x1f) as u8),
                expand_five_bits(((value >> 5) & 0x1f) as u8),
                expand_five_bits((value & 0x1f) as u8),
                u8::MAX,
            ]
        }
        Lz4PixelFormat::Bgr24 | Lz4PixelFormat::Xrgb32 => [pixel[2], pixel[1], pixel[0], u8::MAX],
        Lz4PixelFormat::Rgba32 => [pixel[2], pixel[1], pixel[0], pixel[3]],
    }
}

const fn expand_five_bits(value: u8) -> u8 {
    (value << 3) | (value >> 2)
}

#[cfg(test)]
mod tests {
    use lz4_flex::block::compress;

    use super::*;

    #[test]
    fn decodes_bounded_spice_lz4_row_block() {
        let raw = [3, 2, 1, 6, 5, 4, 9, 8, 7, 12, 11, 10];
        let block = compress(&raw);
        let mut image = vec![1, 7];
        image.extend_from_slice(
            &u32::try_from(block.len())
                .expect("compressed block length")
                .to_be_bytes(),
        );
        image.extend_from_slice(&block);
        let decoded = decode_lz4_with_cancel(&image, 2, 2, DecodeLimits::DISPLAY, || false)
            .expect("SPICE LZ4 image");
        assert!(decoded.top_down);
        assert_eq!(
            decoded.pixels,
            [1, 2, 3, 255, 4, 5, 6, 255, 7, 8, 9, 255, 10, 11, 12, 255,]
        );
    }

    #[test]
    fn rejects_truncated_lz4_block_before_decode() {
        let error =
            decode_lz4_with_cancel(&[1, 8, 0, 0, 0, 3, 1], 1, 1, DecodeLimits::DISPLAY, || {
                false
            })
            .expect_err("truncated compressed block");
        assert_eq!(error.kind, Lz4ErrorKind::Truncated);
    }
}
