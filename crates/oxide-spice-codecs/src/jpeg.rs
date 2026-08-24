//! Bounded baseline and progressive JPEG adapter for SPICE display images.

use std::io::Cursor;
use std::panic::{AssertUnwindSafe, catch_unwind};

use thiserror::Error;
use zune_jpeg::JpegDecoder;
use zune_jpeg::errors::DecodeErrors;
use zune_jpeg::zune_core::bytestream::ZCursor;
use zune_jpeg::zune_core::colorspace::ColorSpace;
use zune_jpeg::zune_core::options::DecoderOptions;

use crate::DecodeLimits;

const START_OF_IMAGE: u8 = 0xd8;
const START_OF_SCAN: u8 = 0xda;
const BASELINE_FRAME: u8 = 0xc0;
const PROGRESSIVE_FRAME: u8 = 0xc2;
const JPEG_CANCEL_INTERVAL_MCUS: usize = 256;
const MAX_PROGRESSIVE_SCANS: usize = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum JpegFrameKind {
    Baseline,
    Progressive,
}

/// One decoded top-down JPEG image in RGBA order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodedJpeg {
    pub width: u32,
    pub height: u32,
    pub pixels: Vec<u8>,
}

/// Stable categories for the JPEG codec boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JpegErrorKind {
    InvalidData,
    UnsupportedFrame,
    DimensionMismatch,
    ResourceLimit,
    Cancelled,
    DecoderPanic,
}

/// A JPEG failure that never retains peer-controlled bytes or decoder messages.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("SPICE JPEG {context}: {kind:?}")]
pub struct JpegError {
    pub kind: JpegErrorKind,
    pub context: &'static str,
}

impl JpegError {
    const fn new(kind: JpegErrorKind, context: &'static str) -> Self {
        Self { kind, context }
    }
}

/// Decodes one baseline or progressive JPEG into a caller-bounded RGBA image.
pub fn decode_jpeg_with_cancel(
    input: &[u8],
    expected_width: u32,
    expected_height: u32,
    limits: DecodeLimits,
    should_cancel: impl Fn() -> bool + Send + Sync + 'static,
) -> Result<DecodedJpeg, JpegError> {
    if should_cancel() {
        return Err(JpegError::new(JpegErrorKind::Cancelled, "decode"));
    }
    let frame_kind = validate_supported_frame(input)?;
    if frame_kind == JpegFrameKind::Progressive {
        return decode_progressive(
            input,
            expected_width,
            expected_height,
            limits,
            should_cancel,
        );
    }
    let options = DecoderOptions::default()
        .set_max_width(limits.maximum_width as usize)
        .set_max_height(limits.maximum_height as usize)
        .set_use_unsafe(false)
        .set_strict_mode(true)
        .jpeg_set_max_scans(MAX_PROGRESSIVE_SCANS)
        .jpeg_set_out_colorspace(ColorSpace::RGBA);
    let decode = || {
        let mut decoder = JpegDecoder::new_with_options(ZCursor::new(input), options);
        decoder.set_cancel(should_cancel);
        decoder.set_cancel_interval(JPEG_CANCEL_INTERVAL_MCUS);
        decoder.decode_headers().map_err(map_decode_error)?;
        let (width, height) = decoder
            .dimensions()
            .ok_or_else(|| JpegError::new(JpegErrorKind::InvalidData, "dimensions"))?;
        let width_u32 = u32::try_from(width)
            .map_err(|_| JpegError::new(JpegErrorKind::ResourceLimit, "width"))?;
        let height_u32 = u32::try_from(height)
            .map_err(|_| JpegError::new(JpegErrorKind::ResourceLimit, "height"))?;
        if width_u32 != expected_width || height_u32 != expected_height {
            return Err(JpegError::new(
                JpegErrorKind::DimensionMismatch,
                "image descriptor dimensions",
            ));
        }
        let expected_output_bytes = width
            .checked_mul(height)
            .and_then(|pixels| pixels.checked_mul(4))
            .ok_or_else(|| JpegError::new(JpegErrorKind::ResourceLimit, "output size"))?;
        if expected_output_bytes > limits.maximum_output_bytes
            || decoder.output_buffer_size() != Some(expected_output_bytes)
        {
            return Err(JpegError::new(JpegErrorKind::ResourceLimit, "output size"));
        }
        let mut pixels = vec![0; expected_output_bytes];
        decoder.decode_into(&mut pixels).map_err(map_decode_error)?;
        Ok(DecodedJpeg {
            width: width_u32,
            height: height_u32,
            pixels,
        })
    };
    catch_unwind(AssertUnwindSafe(decode))
        .map_err(|_| JpegError::new(JpegErrorKind::DecoderPanic, "decoder panic"))?
}

/// Accepts sequential and progressive Huffman DCT frames and rejects other JPEG processes.
fn validate_supported_frame(input: &[u8]) -> Result<JpegFrameKind, JpegError> {
    if input.get(..2) != Some(&[0xff, START_OF_IMAGE]) {
        return Err(JpegError::new(JpegErrorKind::InvalidData, "start marker"));
    }
    let mut offset = 2_usize;
    let mut found_frame = None;
    while offset < input.len() {
        if input[offset] != 0xff {
            return Err(JpegError::new(JpegErrorKind::InvalidData, "marker prefix"));
        }
        while input.get(offset) == Some(&0xff) {
            offset += 1;
        }
        let marker = *input
            .get(offset)
            .ok_or_else(|| JpegError::new(JpegErrorKind::InvalidData, "marker"))?;
        offset += 1;
        if marker == START_OF_SCAN {
            return if let Some(frame_kind) = found_frame {
                Ok(frame_kind)
            } else {
                Err(JpegError::new(JpegErrorKind::InvalidData, "frame marker"))
            };
        }
        if marker == 0x01
            || marker == START_OF_IMAGE
            || marker == 0xd9
            || (0xd0..=0xd7).contains(&marker)
        {
            continue;
        }
        let length_bytes: [u8; 2] = input
            .get(offset..offset + 2)
            .ok_or_else(|| JpegError::new(JpegErrorKind::InvalidData, "segment length"))?
            .try_into()
            .expect("validated JPEG segment length");
        let segment_length = usize::from(u16::from_be_bytes(length_bytes));
        if segment_length < 2 {
            return Err(JpegError::new(JpegErrorKind::InvalidData, "segment length"));
        }
        if is_start_of_frame(marker) {
            let frame_kind = match marker {
                BASELINE_FRAME => JpegFrameKind::Baseline,
                PROGRESSIVE_FRAME => JpegFrameKind::Progressive,
                _ => {
                    return Err(JpegError::new(
                        JpegErrorKind::UnsupportedFrame,
                        "unsupported JPEG process",
                    ));
                }
            };
            if found_frame.is_some() {
                return Err(JpegError::new(
                    JpegErrorKind::InvalidData,
                    "duplicate frame",
                ));
            }
            found_frame = Some(frame_kind);
        }
        offset = offset
            .checked_add(segment_length)
            .filter(|end| *end <= input.len())
            .ok_or_else(|| JpegError::new(JpegErrorKind::InvalidData, "segment bounds"))?;
    }
    Err(JpegError::new(JpegErrorKind::InvalidData, "scan marker"))
}

fn decode_progressive(
    input: &[u8],
    expected_width: u32,
    expected_height: u32,
    limits: DecodeLimits,
    should_cancel: impl Fn() -> bool,
) -> Result<DecodedJpeg, JpegError> {
    if should_cancel() {
        return Err(JpegError::new(JpegErrorKind::Cancelled, "decode"));
    }
    let mut decoder = jpeg_decoder::Decoder::new(Cursor::new(input));
    decoder.set_max_decoding_buffer_size(limits.maximum_output_bytes);
    decoder
        .read_info()
        .map_err(|_| JpegError::new(JpegErrorKind::InvalidData, "progressive headers"))?;
    let info = decoder
        .info()
        .ok_or_else(|| JpegError::new(JpegErrorKind::InvalidData, "dimensions"))?;
    let width = u32::from(info.width);
    let height = u32::from(info.height);
    if width != expected_width || height != expected_height {
        return Err(JpegError::new(
            JpegErrorKind::DimensionMismatch,
            "image descriptor dimensions",
        ));
    }
    let output_bytes = usize::try_from(width)
        .ok()
        .and_then(|width| {
            usize::try_from(height)
                .ok()
                .and_then(|height| width.checked_mul(height))
        })
        .and_then(|pixels| pixels.checked_mul(4))
        .filter(|bytes| *bytes <= limits.maximum_output_bytes)
        .ok_or_else(|| JpegError::new(JpegErrorKind::ResourceLimit, "output size"))?;
    let mut pixels = decoder
        .decode()
        .map_err(|_| JpegError::new(JpegErrorKind::InvalidData, "progressive decode"))?;
    if should_cancel() {
        return Err(JpegError::new(JpegErrorKind::Cancelled, "decode"));
    }
    match info.pixel_format {
        jpeg_decoder::PixelFormat::RGB24 => {
            let pixel_count = output_bytes / 4;
            if pixels.len() != pixel_count * 3 {
                return Err(JpegError::new(
                    JpegErrorKind::InvalidData,
                    "RGB output size",
                ));
            }
            pixels.resize(output_bytes, 0);
            for index in (0..pixel_count).rev() {
                let source = index * 3;
                let destination = index * 4;
                pixels.copy_within(source..source + 3, destination);
                pixels[destination + 3] = u8::MAX;
            }
        }
        jpeg_decoder::PixelFormat::L8 => {
            let pixel_count = output_bytes / 4;
            if pixels.len() != pixel_count {
                return Err(JpegError::new(
                    JpegErrorKind::InvalidData,
                    "luma output size",
                ));
            }
            pixels.resize(output_bytes, 0);
            for index in (0..pixel_count).rev() {
                let luminance = pixels[index];
                let destination = index * 4;
                pixels[destination..destination + 4].copy_from_slice(&[
                    luminance,
                    luminance,
                    luminance,
                    u8::MAX,
                ]);
            }
        }
        _ => {
            return Err(JpegError::new(
                JpegErrorKind::UnsupportedFrame,
                "progressive pixel format",
            ));
        }
    }
    Ok(DecodedJpeg {
        width,
        height,
        pixels,
    })
}

const fn is_start_of_frame(marker: u8) -> bool {
    matches!(
        marker,
        0xc0..=0xc3 | 0xc5..=0xc7 | 0xc9..=0xcb | 0xcd..=0xcf
    )
}

fn map_decode_error(error: DecodeErrors) -> JpegError {
    if matches!(error, DecodeErrors::Cancelled) {
        JpegError::new(JpegErrorKind::Cancelled, "decode")
    } else {
        JpegError::new(JpegErrorKind::InvalidData, "decode")
    }
}

#[cfg(test)]
mod tests {
    use jpeg_encoder::{ColorType, Encoder};

    use super::*;

    fn baseline_jpeg() -> Vec<u8> {
        let mut encoded = Vec::new();
        Encoder::new(&mut encoded, 90)
            .encode(&[255, 0, 0], 1, 1, ColorType::Rgb)
            .expect("encode baseline JPEG fixture");
        encoded
    }

    #[test]
    fn baseline_decode_is_bounded_and_matches_descriptor() {
        let decoded =
            decode_jpeg_with_cancel(&baseline_jpeg(), 1, 1, DecodeLimits::DISPLAY, || false)
                .expect("baseline JPEG");
        assert_eq!((decoded.width, decoded.height), (1, 1));
        assert_eq!(decoded.pixels.len(), 4);
        assert_eq!(decoded.pixels[3], u8::MAX);

        let error =
            decode_jpeg_with_cancel(&baseline_jpeg(), 2, 1, DecodeLimits::DISPLAY, || false)
                .expect_err("descriptor mismatch");
        assert_eq!(error.kind, JpegErrorKind::DimensionMismatch);
    }

    #[test]
    fn cancellation_and_progressive_process_detection_are_bounded() {
        let error = decode_jpeg_with_cancel(&baseline_jpeg(), 1, 1, DecodeLimits::DISPLAY, || true)
            .expect_err("cancelled decode");
        assert_eq!(error.kind, JpegErrorKind::Cancelled);

        let mut progressive = baseline_jpeg();
        let frame = progressive
            .windows(2)
            .position(|bytes| bytes == [0xff, BASELINE_FRAME])
            .expect("baseline frame marker");
        progressive[frame + 1] = 0xc2;
        assert_eq!(
            validate_supported_frame(&progressive).expect("progressive process marker"),
            JpegFrameKind::Progressive
        );
    }
}
