//! Most-significant-bit-first bit writer.

use mmrecode_core::{Error, Result};

/// Builds a byte vector from individual bits and fixed-width integers.
#[derive(Clone, Debug, Default)]
pub struct BitWriter {
    data: Vec<u8>,
    bit_position: usize,
}

impl BitWriter {
    /// Creates an empty writer.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            data: Vec::new(),
            bit_position: 0,
        }
    }

    /// Writes one bit.
    ///
    /// # Errors
    ///
    /// This operation currently cannot fail; it returns `Result` for symmetry with `write_bits`.
    pub fn write_bit(&mut self, value: bool) -> Result<()> {
        self.write_bits(u64::from(value), 1)
    }

    /// Writes the low `count` bits of `value` in most-significant-bit-first order.
    ///
    /// # Errors
    ///
    /// Returns an error when `count` exceeds 64 or `value` does not fit in the requested width.
    pub fn write_bits(&mut self, value: u64, count: u8) -> Result<()> {
        if count > 64 || (count < 64 && value >= (1_u64 << count)) {
            return Err(Error::InvalidData(format!(
                "value {value} does not fit in {count} bits"
            )));
        }

        for bit_index in (0..count).rev() {
            if self.bit_position.is_multiple_of(8) {
                self.data.push(0);
            }
            let bit = ((value >> bit_index) & 1) as u8;
            let byte_index = self.bit_position / 8;
            let shift = 7 - self.bit_position % 8;
            self.data[byte_index] |= bit << shift;
            self.bit_position += 1;
        }
        Ok(())
    }

    /// Pads with zero bits to the next byte boundary.
    pub fn align_to_byte(&mut self) {
        if !self.bit_position.is_multiple_of(8) {
            self.bit_position = self.bit_position.div_ceil(8) * 8;
        }
    }

    /// Finishes writing and returns the underlying bytes.
    #[must_use]
    pub fn into_bytes(self) -> Vec<u8> {
        self.data
    }
}

#[cfg(test)]
mod tests {
    use super::BitWriter;

    #[test]
    fn writes_across_byte_boundaries() {
        let mut writer = BitWriter::new();
        writer.write_bits(0b101, 3).unwrap();
        writer.write_bits(0b0010_1110, 8).unwrap();
        writer.write_bits(0b011, 3).unwrap();
        assert_eq!(writer.into_bytes(), vec![0b1010_0101, 0b1100_1100]);
    }
}
