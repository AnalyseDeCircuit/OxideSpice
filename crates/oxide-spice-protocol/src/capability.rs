use crate::{DecodeError, DecodeErrorKind};

/// Local capability-word bound with ample headroom over current one-word sets.
pub const MAX_CAPABILITY_WORDS: usize = 16;

/// A SPICE capability bitmap represented as little-endian 32-bit words.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CapabilitySet {
    words: Vec<u32>,
}

impl CapabilitySet {
    /// Creates an empty capability set.
    pub const fn new() -> Self {
        Self { words: Vec::new() }
    }

    /// Creates a set containing the supplied protocol bit indices.
    pub fn from_bits(bits: impl IntoIterator<Item = u32>) -> Result<Self, DecodeError> {
        let mut capabilities = Self::new();
        for bit in bits {
            capabilities.insert(bit)?;
        }
        Ok(capabilities)
    }

    /// Creates a set from validated capability words.
    pub(crate) fn from_words(words: Vec<u32>) -> Self {
        Self { words }
    }

    /// Adds one protocol bit, growing only to the required word.
    pub fn insert(&mut self, bit: u32) -> Result<(), DecodeError> {
        let maximum_bits = u32::try_from(MAX_CAPABILITY_WORDS)
            .expect("capability word bound fits u32")
            * u32::BITS;
        if bit >= maximum_bits {
            return Err(DecodeError::new(
                DecodeErrorKind::ResourceLimit,
                usize::try_from(bit).unwrap_or(usize::MAX),
                "capability bit index",
            ));
        }
        let word_index = usize::try_from(bit / u32::BITS)
            .map_err(|_| DecodeError::new(DecodeErrorKind::Overflow, 0, "capability bit index"))?;
        if self.words.len() <= word_index {
            self.words.resize(word_index + 1, 0);
        }
        self.words[word_index] |= 1 << (bit % u32::BITS);
        Ok(())
    }

    /// Reports whether a protocol bit is present.
    pub fn contains(&self, bit: u32) -> bool {
        let word_index = usize::try_from(bit / u32::BITS).expect("u32 bit index fits usize");
        self.words
            .get(word_index)
            .is_some_and(|word| word & (1 << (bit % u32::BITS)) != 0)
    }

    /// Exposes the words for wire encoding without allocating.
    pub fn words(&self) -> &[u32] {
        &self.words
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extreme_bit_is_rejected_before_bitmap_growth() {
        let error = CapabilitySet::from_bits([u32::MAX])
            .expect_err("extreme capability bit must not allocate");
        assert_eq!(error.kind, DecodeErrorKind::ResourceLimit);
    }
}
