//! Most-significant-bit-first bit reader.

use mmrecode_core::{Error, Result};

/// Reads individual bits and fixed-width integers from a byte slice.
#[derive(Clone, Debug)]
pub struct BitReader<'a> {
    data: &'a [u8],
    bit_position: usize,
}

impl<'a> BitReader<'a> {
    /// Creates a reader positioned at the first bit.
    #[must_use]
    pub const fn new(data: &'a [u8]) -> Self {
        Self {
            data,
            bit_position: 0,
        }
    }

    /// Returns the number of unread bits.
    #[must_use]
    pub const fn bits_remaining(&self) -> usize {
        self.data.len() * 8 - self.bit_position
    }

    /// Returns the absolute bit position from the beginning of the input.
    #[must_use]
    pub const fn bit_position(&self) -> usize {
        self.bit_position
    }

    /// Returns the byte containing the next unread bit.
    #[must_use]
    pub const fn byte_position(&self) -> usize {
        self.bit_position / 8
    }

    /// Reads bits without advancing the reader.
    ///
    /// # Errors
    ///
    /// Returns an error under the same conditions as [`Self::read_bits`].
    pub fn peek_bits(&self, count: u8) -> Result<u64> {
        let mut copy = self.clone();
        copy.read_bits(count)
    }

    /// Skips a fixed number of bits.
    ///
    /// # Errors
    ///
    /// Returns an error when the input does not contain enough bits.
    pub fn skip_bits(&mut self, count: usize) -> Result<()> {
        if count > self.bits_remaining() {
            return Err(Error::InvalidData(format!(
                "cannot skip {count} bits with {} remaining",
                self.bits_remaining()
            )));
        }
        self.bit_position += count;
        Ok(())
    }

    /// Reads one bit.
    ///
    /// # Errors
    ///
    /// Returns an error when no bits remain.
    pub fn read_bit(&mut self) -> Result<bool> {
        Ok(self.read_bits(1)? != 0)
    }

    /// Reads up to 64 bits in most-significant-bit-first order.
    ///
    /// # Errors
    ///
    /// Returns an error when `count` exceeds 64 or the input does not contain enough bits.
    pub fn read_bits(&mut self, count: u8) -> Result<u64> {
        if count > 64 || usize::from(count) > self.bits_remaining() {
            return Err(Error::InvalidData(format!(
                "cannot read {count} bits with {} remaining",
                self.bits_remaining()
            )));
        }

        let mut value = 0_u64;
        for _ in 0..count {
            let byte = self.data[self.bit_position / 8];
            let shift = 7 - self.bit_position % 8;
            value = (value << 1) | u64::from((byte >> shift) & 1);
            self.bit_position += 1;
        }
        Ok(value)
    }

    /// Advances to the next byte boundary.
    pub fn align_to_byte(&mut self) {
        self.bit_position = self.bit_position.div_ceil(8) * 8;
    }
}

#[cfg(test)]
mod tests {
    use super::BitReader;

    #[test]
    fn reads_across_byte_boundaries() {
        let mut reader = BitReader::new(&[0b1010_0101, 0b1100_0011]);
        assert_eq!(reader.read_bits(4).unwrap(), 0b1010);
        assert_eq!(reader.read_bits(8).unwrap(), 0b0101_1100);
        assert_eq!(reader.read_bits(4).unwrap(), 0b0011);
        assert_eq!(reader.bits_remaining(), 0);
    }

    #[test]
    fn peeks_and_skips_without_losing_position() {
        let mut reader = BitReader::new(&[0b1010_0101, 0b1100_0011]);
        assert_eq!(reader.peek_bits(4).unwrap(), 0b1010);
        assert_eq!(reader.bit_position(), 0);
        reader.skip_bits(3).unwrap();
        assert_eq!(reader.bit_position(), 3);
        assert_eq!(reader.byte_position(), 0);
        assert_eq!(reader.read_bits(5).unwrap(), 0b0_0101);
        assert_eq!(reader.byte_position(), 1);
    }
}
