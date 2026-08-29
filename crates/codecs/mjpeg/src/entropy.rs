use mmrecode_core::{Error, Result};

use crate::HuffmanTable;

#[derive(Clone, Copy, Debug)]
struct HuffmanCode {
    bits: u16,
    length: u8,
    symbol: u8,
}

#[derive(Clone, Debug)]
pub(crate) struct HuffmanDecoder {
    codes: Vec<HuffmanCode>,
}

impl HuffmanDecoder {
    pub(crate) fn new(table: &HuffmanTable) -> Result<Self> {
        let symbol_count: usize = table
            .code_counts
            .iter()
            .map(|&count| usize::from(count))
            .sum();
        if symbol_count == 0 || symbol_count != table.symbols.len() {
            return Err(Error::InvalidData(format!(
                "JPEG Huffman table {} has inconsistent symbol counts",
                table.id
            )));
        }

        let mut codes = Vec::with_capacity(symbol_count);
        let mut code = 0_u32;
        let mut symbol_index = 0;
        for (length_index, &count) in table.code_counts.iter().enumerate() {
            let length = u8::try_from(length_index + 1).expect("JPEG code lengths fit in u8");
            let limit = 1_u32 << length;
            if code + u32::from(count) > limit {
                return Err(Error::InvalidData(format!(
                    "JPEG Huffman table {} is oversubscribed at length {length}",
                    table.id
                )));
            }
            for _ in 0..count {
                codes.push(HuffmanCode {
                    bits: u16::try_from(code).expect("JPEG Huffman code fits in u16"),
                    length,
                    symbol: table.symbols[symbol_index],
                });
                symbol_index += 1;
                code += 1;
            }
            code <<= 1;
        }
        Ok(Self { codes })
    }

    pub(crate) fn decode(&self, reader: &mut EntropyReader<'_>) -> Result<u8> {
        let mut bits = 0_u16;
        for length in 1..=16 {
            bits = (bits << 1) | u16::from(reader.read_bit()?);
            if let Some(code) = self
                .codes
                .iter()
                .find(|code| code.length == length && code.bits == bits)
            {
                return Ok(code.symbol);
            }
        }
        Err(reader.error("no Huffman code matches the next 16 bits"))
    }
}

#[derive(Debug)]
pub(crate) struct EntropyReader<'a> {
    data: &'a [u8],
    base_offset: usize,
    position: usize,
    current_byte: u8,
    bits_remaining: u8,
}

impl<'a> EntropyReader<'a> {
    pub(crate) const fn new(data: &'a [u8], base_offset: usize) -> Self {
        Self {
            data,
            base_offset,
            position: 0,
            current_byte: 0,
            bits_remaining: 0,
        }
    }

    pub(crate) fn read_bit(&mut self) -> Result<u8> {
        if self.bits_remaining == 0 {
            self.current_byte = self.read_data_byte()?;
            self.bits_remaining = 8;
        }
        self.bits_remaining -= 1;
        Ok((self.current_byte >> self.bits_remaining) & 1)
    }

    pub(crate) fn read_bits(&mut self, count: u8) -> Result<u16> {
        let mut value = 0_u16;
        for _ in 0..count {
            value = (value << 1) | u16::from(self.read_bit()?);
        }
        Ok(value)
    }

    pub(crate) fn consume_restart(&mut self, expected: u8) -> Result<()> {
        self.bits_remaining = 0;
        let marker_offset = self.base_offset + self.position;
        if self.data.get(self.position) != Some(&0xff) {
            return Err(self.error("expected restart marker at MCU boundary"));
        }
        self.position += 1;
        while self.data.get(self.position) == Some(&0xff) {
            self.position += 1;
        }
        let code = *self
            .data
            .get(self.position)
            .ok_or_else(|| self.error("truncated restart marker"))?;
        self.position += 1;
        if code != 0xd0 + expected {
            return Err(Error::InvalidData(format!(
                "JPEG at byte 0x{marker_offset:08x}: expected RST{expected}, found marker 0x{code:02x}"
            )));
        }
        Ok(())
    }

    pub(crate) fn error(&self, message: &str) -> Error {
        Error::InvalidData(format!(
            "JPEG entropy at byte 0x{:08x}: {message}",
            self.base_offset + self.position
        ))
    }

    fn read_data_byte(&mut self) -> Result<u8> {
        let byte = *self
            .data
            .get(self.position)
            .ok_or_else(|| self.error("unexpected end of entropy-coded data"))?;
        self.position += 1;
        if byte != 0xff {
            return Ok(byte);
        }

        while self.data.get(self.position) == Some(&0xff) {
            self.position += 1;
        }
        let code = *self
            .data
            .get(self.position)
            .ok_or_else(|| self.error("truncated entropy marker"))?;
        self.position += 1;
        if code == 0x00 {
            Ok(0xff)
        } else {
            Err(self.error(&format!(
                "unexpected marker 0xff{code:02x} inside coded block"
            )))
        }
    }
}

pub(crate) fn receive_extend(reader: &mut EntropyReader<'_>, size: u8) -> Result<i32> {
    if size == 0 {
        return Ok(0);
    }
    if size > 15 {
        return Err(reader.error("coefficient magnitude contains more than 15 bits"));
    }
    let bits = i32::from(reader.read_bits(size)?);
    let threshold = 1_i32 << (size - 1);
    if bits < threshold {
        Ok(bits + 1 - (1_i32 << size))
    } else {
        Ok(bits)
    }
}

#[cfg(test)]
mod tests {
    use crate::{HuffmanTable, HuffmanTableClass};

    use super::{EntropyReader, HuffmanDecoder, receive_extend};

    #[test]
    fn decodes_canonical_codes_and_stuffed_bytes() {
        let table = HuffmanTable {
            class: HuffmanTableClass::Dc,
            id: 0,
            code_counts: [1, 2, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
            symbols: vec![10, 20, 30],
        };
        let decoder = HuffmanDecoder::new(&table).expect("valid table");
        let mut reader = EntropyReader::new(&[0b0101_1000], 0);
        assert_eq!(decoder.decode(&mut reader).unwrap(), 10);
        assert_eq!(decoder.decode(&mut reader).unwrap(), 20);
        assert_eq!(decoder.decode(&mut reader).unwrap(), 30);

        let mut stuffed = EntropyReader::new(&[0xff, 0x00], 0);
        assert_eq!(stuffed.read_bits(8).unwrap(), 0xff);
    }

    #[test]
    fn extends_negative_coefficient_values() {
        let mut reader = EntropyReader::new(&[0b0011_0000], 0);
        assert_eq!(receive_extend(&mut reader, 3).unwrap(), -6);
        assert_eq!(receive_extend(&mut reader, 3).unwrap(), 4);
    }
}
