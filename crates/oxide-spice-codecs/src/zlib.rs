//! Bounded streaming zlib wrapper used by SPICE ZLIB_GLZ_RGB images.

use miniz_oxide::inflate::stream::{InflateState, inflate};
use miniz_oxide::{DataFormat, MZFlush, MZStatus};
use thiserror::Error;

const OUTPUT_CHUNK_BYTES: usize = 64 * 1024;

/// Stable categories for the zlib wrapper boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ZlibErrorKind {
    InvalidData,
    SizeMismatch,
    ResourceLimit,
    TrailingData,
    Cancelled,
}

/// A zlib failure that does not retain input or partial output.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("SPICE zlib {context}: {kind:?}")]
pub struct ZlibError {
    pub kind: ZlibErrorKind,
    pub context: &'static str,
}

impl ZlibError {
    const fn new(kind: ZlibErrorKind, context: &'static str) -> Self {
        Self { kind, context }
    }
}

/// Inflates one zlib stream to its exact declared GLZ byte length.
pub fn inflate_zlib_exact_with_cancel(
    input: &[u8],
    expected_bytes: usize,
    maximum_bytes: usize,
    mut should_cancel: impl FnMut() -> bool,
) -> Result<Vec<u8>, ZlibError> {
    if expected_bytes == 0 || expected_bytes > maximum_bytes {
        return Err(ZlibError::new(
            ZlibErrorKind::ResourceLimit,
            "declared output size",
        ));
    }
    let mut state = InflateState::new_boxed(DataFormat::Zlib);
    let mut output = Vec::with_capacity(expected_bytes);
    let mut input_offset = 0_usize;
    let mut chunk = [0_u8; OUTPUT_CHUNK_BYTES];
    loop {
        if should_cancel() {
            return Err(ZlibError::new(ZlibErrorKind::Cancelled, "inflate"));
        }
        let result = inflate(
            &mut state,
            &input[input_offset..],
            &mut chunk,
            MZFlush::None,
        );
        input_offset = input_offset
            .checked_add(result.bytes_consumed)
            .ok_or_else(|| ZlibError::new(ZlibErrorKind::InvalidData, "input progress"))?;
        let updated_size = output
            .len()
            .checked_add(result.bytes_written)
            .ok_or_else(|| ZlibError::new(ZlibErrorKind::ResourceLimit, "output size"))?;
        if updated_size > expected_bytes {
            return Err(ZlibError::new(
                ZlibErrorKind::SizeMismatch,
                "expanded output",
            ));
        }
        output.extend_from_slice(&chunk[..result.bytes_written]);
        match result.status {
            Ok(MZStatus::StreamEnd) => {
                if input_offset != input.len() {
                    return Err(ZlibError::new(
                        ZlibErrorKind::TrailingData,
                        "compressed input",
                    ));
                }
                if output.len() != expected_bytes {
                    return Err(ZlibError::new(
                        ZlibErrorKind::SizeMismatch,
                        "expanded output",
                    ));
                }
                return Ok(output);
            }
            Ok(MZStatus::Ok) => {
                if result.bytes_consumed == 0 && result.bytes_written == 0 {
                    return Err(ZlibError::new(
                        ZlibErrorKind::InvalidData,
                        "stream progress",
                    ));
                }
            }
            Ok(MZStatus::NeedDict) | Err(_) => {
                return Err(ZlibError::new(
                    ZlibErrorKind::InvalidData,
                    "compressed input",
                ));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use miniz_oxide::deflate::compress_to_vec_zlib;

    use super::*;

    #[test]
    fn exact_output_and_trailing_input_are_enforced() {
        let encoded = compress_to_vec_zlib(b"stateful glz", 6);
        assert_eq!(
            inflate_zlib_exact_with_cancel(&encoded, 12, 12, || false).expect("exact zlib output"),
            b"stateful glz"
        );
        let error = inflate_zlib_exact_with_cancel(&encoded, 11, 12, || false)
            .expect_err("declared size is authoritative");
        assert_eq!(error.kind, ZlibErrorKind::SizeMismatch);

        let mut trailing = encoded;
        trailing.push(0);
        let error = inflate_zlib_exact_with_cancel(&trailing, 12, 12, || false)
            .expect_err("trailing compressed bytes");
        assert_eq!(error.kind, ZlibErrorKind::TrailingData);
    }

    #[test]
    fn limits_and_cancellation_apply_before_or_between_chunks() {
        let encoded = compress_to_vec_zlib(&vec![7; OUTPUT_CHUNK_BYTES * 2], 6);
        let error = inflate_zlib_exact_with_cancel(
            &encoded,
            OUTPUT_CHUNK_BYTES * 2,
            OUTPUT_CHUNK_BYTES,
            || false,
        )
        .expect_err("output limit");
        assert_eq!(error.kind, ZlibErrorKind::ResourceLimit);

        let mut polls = 0;
        let error = inflate_zlib_exact_with_cancel(
            &encoded,
            OUTPUT_CHUNK_BYTES * 2,
            OUTPUT_CHUNK_BYTES * 2,
            || {
                polls += 1;
                polls > 1
            },
        )
        .expect_err("second output chunk observes cancellation");
        assert_eq!(error.kind, ZlibErrorKind::Cancelled);
    }
}
