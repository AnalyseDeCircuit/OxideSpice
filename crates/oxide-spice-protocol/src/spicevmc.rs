//! Checked compressed-data envelope shared by SpiceVMC channel types.

use crate::wire::Reader;
use crate::{DecodeError, DecodeErrorKind};

/// Wire discriminator for raw-block LZ4 compression.
pub const SPICEVMC_COMPRESSION_LZ4: u8 = 1;
const COMPRESSED_DATA_HEADER_BYTES: usize = 1 + size_of::<u32>();

/// Borrowed validated compressed SpiceVMC payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SpiceVmcCompressedData<'a> {
    pub uncompressed_size: usize,
    pub compressed_bytes: &'a [u8],
}

impl<'a> SpiceVmcCompressedData<'a> {
    /// Decodes the packed type, output length, and remaining compressed bytes.
    pub fn decode(body: &'a [u8], maximum_uncompressed_bytes: usize) -> Result<Self, DecodeError> {
        let mut reader = Reader::new(body);
        if reader.u8("SpiceVMC compression type")? != SPICEVMC_COMPRESSION_LZ4 {
            return Err(DecodeError::new(
                DecodeErrorKind::Unsupported,
                0,
                "SpiceVMC compression type",
            ));
        }
        let uncompressed_size = usize::try_from(reader.u32("SpiceVMC uncompressed size")?)
            .map_err(|_| {
                DecodeError::new(DecodeErrorKind::Overflow, 1, "SpiceVMC uncompressed size")
            })?;
        if uncompressed_size == 0 || uncompressed_size > maximum_uncompressed_bytes {
            return Err(DecodeError::new(
                DecodeErrorKind::ResourceLimit,
                1,
                "SpiceVMC uncompressed size",
            ));
        }
        let compressed_bytes = reader.take(reader.remaining(), "SpiceVMC compressed bytes")?;
        if compressed_bytes.is_empty() {
            return Err(DecodeError::new(
                DecodeErrorKind::InvalidValue,
                COMPRESSED_DATA_HEADER_BYTES,
                "SpiceVMC compressed bytes",
            ));
        }
        Ok(Self {
            uncompressed_size,
            compressed_bytes,
        })
    }
}

/// Encodes a packed LZ4 SpiceVMC envelope around caller-bounded bytes.
pub fn encode_spicevmc_compressed_data(
    uncompressed_size: usize,
    compressed_bytes: &[u8],
) -> Result<Vec<u8>, DecodeError> {
    if uncompressed_size == 0 || compressed_bytes.is_empty() {
        return Err(DecodeError::new(
            DecodeErrorKind::InvalidValue,
            0,
            "SpiceVMC compressed data",
        ));
    }
    let uncompressed_size = u32::try_from(uncompressed_size).map_err(|_| {
        DecodeError::new(DecodeErrorKind::Overflow, 1, "SpiceVMC uncompressed size")
    })?;
    let mut body = Vec::with_capacity(COMPRESSED_DATA_HEADER_BYTES + compressed_bytes.len());
    body.push(SPICEVMC_COMPRESSION_LZ4);
    body.extend_from_slice(&uncompressed_size.to_le_bytes());
    body.extend_from_slice(compressed_bytes);
    Ok(body)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compressed_data_round_trips_exact_packed_header() {
        let body = encode_spicevmc_compressed_data(4096, &[1, 2, 3]).expect("compressed body");
        assert_eq!(body, [1, 0, 16, 0, 0, 1, 2, 3]);
        let decoded = SpiceVmcCompressedData::decode(&body, 4096).expect("compressed body");
        assert_eq!(decoded.uncompressed_size, 4096);
        assert_eq!(decoded.compressed_bytes, [1, 2, 3]);
    }
}
