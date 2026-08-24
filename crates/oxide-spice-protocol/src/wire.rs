use crate::{DecodeError, DecodeErrorKind};

/// A cursor over one already-bounded protocol object.
pub(crate) struct Reader<'a> {
    input: &'a [u8],
    offset: usize,
}

impl<'a> Reader<'a> {
    /// Starts a checked reader at the beginning of a bounded input.
    pub(crate) const fn new(input: &'a [u8]) -> Self {
        Self { input, offset: 0 }
    }

    /// Returns the current byte offset for contextual validation.
    pub(crate) const fn offset(&self) -> usize {
        self.offset
    }

    /// Returns the unread byte count without underflow.
    pub(crate) const fn remaining(&self) -> usize {
        self.input.len() - self.offset
    }

    /// Reads an exact borrowed slice after checked end-offset arithmetic.
    pub(crate) fn take(
        &mut self,
        length: usize,
        context: &'static str,
    ) -> Result<&'a [u8], DecodeError> {
        let end = self
            .offset
            .checked_add(length)
            .ok_or_else(|| DecodeError::new(DecodeErrorKind::Overflow, self.offset, context))?;
        let value = self
            .input
            .get(self.offset..end)
            .ok_or_else(|| DecodeError::new(DecodeErrorKind::Truncated, self.offset, context))?;
        self.offset = end;
        Ok(value)
    }

    /// Reads one unsigned byte.
    pub(crate) fn u8(&mut self, context: &'static str) -> Result<u8, DecodeError> {
        Ok(self.take(1, context)?[0])
    }

    /// Reads a little-endian unsigned 16-bit value.
    pub(crate) fn u16(&mut self, context: &'static str) -> Result<u16, DecodeError> {
        let bytes: [u8; 2] = self
            .take(2, context)?
            .try_into()
            .expect("exact slice length");
        Ok(u16::from_le_bytes(bytes))
    }

    /// Reads a little-endian signed 16-bit value.
    pub(crate) fn i16(&mut self, context: &'static str) -> Result<i16, DecodeError> {
        let bytes: [u8; 2] = self
            .take(2, context)?
            .try_into()
            .expect("exact slice length");
        Ok(i16::from_le_bytes(bytes))
    }

    /// Reads a little-endian unsigned 32-bit value.
    pub(crate) fn u32(&mut self, context: &'static str) -> Result<u32, DecodeError> {
        let bytes: [u8; 4] = self
            .take(4, context)?
            .try_into()
            .expect("exact slice length");
        Ok(u32::from_le_bytes(bytes))
    }

    /// Reads a little-endian signed 32-bit value.
    pub(crate) fn i32(&mut self, context: &'static str) -> Result<i32, DecodeError> {
        let bytes: [u8; 4] = self
            .take(4, context)?
            .try_into()
            .expect("exact slice length");
        Ok(i32::from_le_bytes(bytes))
    }

    /// Reads a little-endian unsigned 64-bit value.
    pub(crate) fn u64(&mut self, context: &'static str) -> Result<u64, DecodeError> {
        let bytes: [u8; 8] = self
            .take(8, context)?
            .try_into()
            .expect("exact slice length");
        Ok(u64::from_le_bytes(bytes))
    }
}

/// Resolves an offset and fixed length within the current message body.
pub(crate) fn resolve_range(
    body: &[u8],
    offset: u32,
    length: usize,
    context: &'static str,
) -> Result<std::ops::Range<usize>, DecodeError> {
    let start = usize::try_from(offset)
        .map_err(|_| DecodeError::new(DecodeErrorKind::Overflow, 0, context))?;
    let end = start
        .checked_add(length)
        .ok_or_else(|| DecodeError::new(DecodeErrorKind::Overflow, start, context))?;
    if end > body.len() {
        return Err(DecodeError::new(
            DecodeErrorKind::InvalidOffset,
            start,
            context,
        ));
    }
    Ok(start..end)
}

/// Converts a wire count and element size into a bounded byte length.
pub(crate) fn checked_array_bytes(
    count: u32,
    element_size: usize,
    maximum_count: usize,
    offset: usize,
    context: &'static str,
) -> Result<usize, DecodeError> {
    let count = usize::try_from(count)
        .map_err(|_| DecodeError::new(DecodeErrorKind::Overflow, offset, context))?;
    if count > maximum_count {
        return Err(DecodeError::new(
            DecodeErrorKind::ResourceLimit,
            offset,
            context,
        ));
    }
    count
        .checked_mul(element_size)
        .ok_or_else(|| DecodeError::new(DecodeErrorKind::Overflow, offset, context))
}
