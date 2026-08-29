use mmrecode_core::{Error, Result};

use crate::BitReader;

/// One variable-length code and its decoded symbol.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct VlcEntry {
    /// Code bits stored in the least-significant `bit_length` bits.
    pub code: u32,
    /// Number of code bits, from 1 through 32.
    pub bit_length: u8,
    /// Caller-defined decoded symbol.
    pub symbol: u16,
}

/// Validated prefix-code table for most-significant-bit-first streams.
#[derive(Clone, Debug)]
pub struct VlcTable {
    entries: Vec<VlcEntry>,
    maximum_length: u8,
}

impl VlcTable {
    /// Validates and constructs a variable-length prefix-code table.
    ///
    /// # Errors
    ///
    /// Returns an error for empty tables, invalid bit lengths, codes that do
    /// not fit their declared length, duplicate codes, or prefix collisions.
    pub fn new(mut entries: Vec<VlcEntry>) -> Result<Self> {
        if entries.is_empty() {
            return Err(Error::InvalidData("VLC table cannot be empty".into()));
        }
        for entry in &entries {
            if entry.bit_length == 0
                || entry.bit_length > 32
                || (entry.bit_length < 32 && entry.code >= (1_u32 << entry.bit_length))
            {
                return Err(Error::InvalidData(format!(
                    "VLC code {} does not fit in {} bits",
                    entry.code, entry.bit_length
                )));
            }
        }
        entries.sort_unstable_by_key(|entry| entry.bit_length);
        for (index, entry) in entries.iter().enumerate() {
            for longer in &entries[index + 1..] {
                let shift = longer.bit_length - entry.bit_length;
                if entry.bit_length == longer.bit_length && entry.code == longer.code
                    || entry.bit_length < longer.bit_length && longer.code >> shift == entry.code
                {
                    return Err(Error::InvalidData(
                        "VLC table contains duplicate or prefix-overlapping codes".into(),
                    ));
                }
            }
        }
        let maximum_length = entries
            .last()
            .map(|entry| entry.bit_length)
            .ok_or_else(|| Error::InvalidData("VLC table cannot be empty".into()))?;
        Ok(Self {
            entries,
            maximum_length,
        })
    }

    /// Decodes one symbol from a bit reader.
    ///
    /// # Errors
    ///
    /// Returns an error when the input is truncated or no table entry matches.
    pub fn decode(&self, reader: &mut BitReader<'_>) -> Result<u16> {
        let mut code = 0_u32;
        for length in 1..=self.maximum_length {
            code = (code << 1) | u32::from(reader.read_bit()?);
            if let Some(entry) = self
                .entries
                .iter()
                .find(|entry| entry.bit_length == length && entry.code == code)
            {
                return Ok(entry.symbol);
            }
        }
        Err(Error::InvalidData(
            "no VLC table entry matches the input bits".into(),
        ))
    }

    /// Returns the validated entries ordered by increasing bit length.
    #[must_use]
    pub fn entries(&self) -> &[VlcEntry] {
        &self.entries
    }
}

#[cfg(test)]
mod tests {
    use crate::{BitReader, VlcEntry, VlcTable};

    #[test]
    fn decodes_prefix_codes() {
        let table = VlcTable::new(vec![
            VlcEntry {
                code: 0,
                bit_length: 1,
                symbol: 10,
            },
            VlcEntry {
                code: 0b10,
                bit_length: 2,
                symbol: 20,
            },
            VlcEntry {
                code: 0b11,
                bit_length: 2,
                symbol: 30,
            },
        ])
        .unwrap();
        let mut reader = BitReader::new(&[0b0101_1000]);
        assert_eq!(table.decode(&mut reader).unwrap(), 10);
        assert_eq!(table.decode(&mut reader).unwrap(), 20);
        assert_eq!(table.decode(&mut reader).unwrap(), 30);
    }

    #[test]
    fn rejects_prefix_collisions() {
        assert!(
            VlcTable::new(vec![
                VlcEntry {
                    code: 0,
                    bit_length: 1,
                    symbol: 1,
                },
                VlcEntry {
                    code: 0,
                    bit_length: 2,
                    symbol: 2,
                },
            ])
            .is_err()
        );
    }
}
