//! Native AAC scalefactor and spectral Huffman decoding.

use std::sync::OnceLock;

use mmrecode_bitstream::BitReader;
use mmrecode_core::{Error, Result};

use crate::tables;

#[derive(Debug, Default)]
struct Node {
    children: [Option<usize>; 2],
    symbol: Option<u16>,
}

#[derive(Debug)]
struct Codebook(Vec<Node>);

impl Codebook {
    fn new(entries: &[(u32, u8)]) -> Self {
        let mut nodes = vec![Node::default()];
        for (symbol, &(code, length)) in entries.iter().enumerate() {
            let mut node = 0;
            for shift in (0..length).rev() {
                assert!(nodes[node].symbol.is_none(), "fixed table prefix collision");
                let branch = usize::from((code >> shift) & 1 != 0);
                node = if let Some(child) = nodes[node].children[branch] {
                    child
                } else {
                    let child = nodes.len();
                    nodes.push(Node::default());
                    nodes[node].children[branch] = Some(child);
                    child
                };
            }
            assert!(nodes[node].children == [None, None] && nodes[node].symbol.is_none());
            nodes[node].symbol = Some(u16::try_from(symbol).expect("AAC codebook size"));
        }
        Self(nodes)
    }

    fn decode(&self, reader: &mut BitReader<'_>) -> Result<u16> {
        let mut node = 0;
        loop {
            if let Some(symbol) = self.0[node].symbol {
                return Ok(symbol);
            }
            node = self.0[node].children[usize::from(reader.read_bit()?)]
                .ok_or_else(|| Error::InvalidData("invalid AAC Huffman codeword".into()))?;
        }
    }
}

fn codebooks() -> &'static [Codebook; 12] {
    static BOOKS: OnceLock<[Codebook; 12]> = OnceLock::new();
    BOOKS.get_or_init(|| {
        std::array::from_fn(|index| {
            Codebook::new(if index == 0 {
                &tables::SCALEFACTORS
            } else {
                tables::SPECTRAL[index - 1]
            })
        })
    })
}

pub(crate) fn scalefactor(reader: &mut BitReader<'_>) -> Result<i16> {
    Ok(i16::try_from(codebooks()[0].decode(reader)?).expect("121 symbols") - 60)
}

pub(crate) const fn dimension(book: u8) -> usize {
    if book <= 4 { 4 } else { 2 }
}

/// Returns signed quantized coefficients. Unused lanes in pair codebooks are zero.
pub(crate) fn spectral(reader: &mut BitReader<'_>, book: u8) -> Result<[i16; 4]> {
    if !(1..=11).contains(&book) {
        return Err(Error::InvalidData(
            "invalid AAC spectral Huffman codebook".into(),
        ));
    }
    let mut symbol =
        i16::try_from(codebooks()[usize::from(book)].decode(reader)?).expect("289 symbols");
    let (radix, bias, sign_bits) = match book {
        1 | 2 => (3, 1, false),
        3 | 4 => (3, 0, true),
        5 | 6 => (9, 4, false),
        7 | 8 => (8, 0, true),
        9 | 10 => (13, 0, true),
        _ => (17, 0, true),
    };
    let mut values = [0; 4];
    for value in values[..dimension(book)].iter_mut().rev() {
        *value = symbol % radix - bias;
        symbol /= radix;
    }
    if sign_bits {
        for value in &mut values[..dimension(book)] {
            if *value != 0 && reader.read_bit()? {
                *value = -*value;
            }
        }
    }
    // All sign bits precede both escapes, rather than one sign/escape per coefficient.
    if book == 11 {
        for value in &mut values[..2] {
            if value.abs() == 16 {
                let mut width = 4;
                while reader.read_bit()? {
                    width += 1;
                    if width > 12 {
                        return Err(Error::InvalidData(
                            "AAC escape coefficient exceeds 8191".into(),
                        ));
                    }
                }
                let magnitude = (1_i16 << width)
                    + i16::try_from(reader.read_bits(width)?).expect("12-bit escape");
                *value = magnitude * value.signum();
            }
        }
    }
    Ok(values)
}

pub(crate) fn inverse_quantize(value: i16, scalefactor: i16) -> f64 {
    let value = f64::from(value);
    value.signum() * value.abs().powf(4.0 / 3.0) * (f64::from(scalefactor - 100) / 4.0).exp2()
}

#[cfg(test)]
mod tests {
    use mmrecode_bitstream::{BitWriter, VlcEntry, VlcTable};

    use super::*;

    #[test]
    fn every_fixed_codeword_matches_independent_prefix_validation() {
        for (book, entries) in std::iter::once(tables::SCALEFACTORS.as_slice())
            .chain(tables::SPECTRAL)
            .enumerate()
        {
            let reference = VlcTable::new(
                entries
                    .iter()
                    .enumerate()
                    .map(|(symbol, &(code, bit_length))| VlcEntry {
                        code,
                        bit_length,
                        symbol: u16::try_from(symbol).unwrap(),
                    })
                    .collect(),
            )
            .unwrap();
            for (symbol, &(code, length)) in entries.iter().enumerate() {
                let mut writer = BitWriter::new();
                writer.write_bits(u64::from(code), length).unwrap();
                writer.write_bits(0x55, 8).unwrap();
                let data = writer.into_bytes();
                let mut reader = BitReader::new(&data);
                assert_eq!(
                    usize::from(codebooks()[book].decode(&mut reader).unwrap()),
                    symbol
                );
                assert_eq!(reader.bit_position(), usize::from(length));
                assert_eq!(reader.read_bits(8).unwrap(), 0x55);
                assert_eq!(
                    usize::from(reference.decode(&mut BitReader::new(&data)).unwrap()),
                    symbol
                );
            }
        }
    }

    #[test]
    fn decodes_signed_unsigned_and_escape_vectors_with_exact_bit_consumption() {
        for book in 1..=11 {
            for (symbol, &(code, length)) in
                tables::SPECTRAL[usize::from(book - 1)].iter().enumerate()
            {
                let radix = match book {
                    1..=4 => 3,
                    5..=6 => 9,
                    7..=8 => 8,
                    9..=10 => 13,
                    _ => 17,
                };
                let bias = match book {
                    1..=2 => 1,
                    5..=6 => 4,
                    _ => 0,
                };
                let signed = matches!(book, 1 | 2 | 5 | 6);
                let mut index = i16::try_from(symbol).unwrap();
                let mut expected = [0; 4];
                for value in expected[..dimension(book)].iter_mut().rev() {
                    *value = index % radix - bias;
                    index /= radix;
                }
                let mut writer = BitWriter::new();
                writer.write_bits(u64::from(code), length).unwrap();
                if !signed {
                    for (lane, value) in expected[..dimension(book)].iter_mut().enumerate() {
                        if *value != 0 {
                            writer.write_bit(lane % 2 == 0).unwrap();
                            if lane % 2 == 0 {
                                *value = -*value;
                            }
                        }
                    }
                }
                if book == 11 {
                    for value in &mut expected[..2] {
                        if value.abs() == 16 {
                            writer.write_bit(false).unwrap();
                            writer.write_bits(9, 4).unwrap();
                            *value = value.signum() * 25;
                        }
                    }
                }
                writer.write_bits(0x55, 8).unwrap();
                let data = writer.into_bytes();
                let mut reader = BitReader::new(&data);
                assert_eq!(
                    spectral(&mut reader, book).unwrap(),
                    expected,
                    "book {book}, symbol {symbol}"
                );
                assert_eq!(reader.read_bits(8).unwrap(), 0x55);
            }
        }
    }

    #[test]
    fn dequantization_known_values_and_scalefactor_delta() {
        assert!((inverse_quantize(8, 100) - 16.0).abs() < 1e-12);
        assert!((inverse_quantize(-8, 104) + 32.0).abs() < 1e-12);
        assert!((inverse_quantize(0, 255)).abs() < 1e-12);
        let mut reader = BitReader::new(&[0]);
        assert_eq!(scalefactor(&mut reader).unwrap(), 0);
        assert_eq!(reader.bit_position(), 1);
    }

    #[test]
    fn rejects_every_truncated_codeword_prefix() {
        for (book, entries) in std::iter::once(tables::SCALEFACTORS.as_slice())
            .chain(tables::SPECTRAL)
            .enumerate()
        {
            for &(code, length) in entries {
                for prefix_length in 0..length {
                    // Place the prefix at the end of the byte buffer, so zero byte padding
                    // cannot accidentally complete the missing codeword.
                    let leading = (8 - prefix_length % 8) % 8;
                    let mut writer = BitWriter::new();
                    writer.write_bits(0, leading).unwrap();
                    writer
                        .write_bits(u64::from(code >> (length - prefix_length)), prefix_length)
                        .unwrap();
                    let data = writer.into_bytes();
                    let mut reader = BitReader::new(&data);
                    reader.skip_bits(usize::from(leading)).unwrap();
                    assert!(codebooks()[book].decode(&mut reader).is_err());
                }
            }
        }
    }

    #[test]
    fn escape_bounds_and_truncated_sign_bits_are_checked() {
        let (code, length) = tables::BOOK_11[16 * 17 + 16];
        let mut writer = BitWriter::new();
        writer.write_bits(u64::from(code), length).unwrap();
        writer.write_bits(0b10, 2).unwrap(); // negative first, positive second
        for _ in 0..2 {
            writer.write_bits(0xff, 8).unwrap(); // width = 12
            writer.write_bit(false).unwrap();
            writer.write_bits(0xfff, 12).unwrap();
        }
        let data = writer.into_bytes();
        assert_eq!(
            spectral(&mut BitReader::new(&data), 11).unwrap(),
            [-8191, 8191, 0, 0]
        );
        let mut writer = BitWriter::new();
        writer.write_bits(u64::from(code), length).unwrap();
        writer.write_bits(0, 2).unwrap();
        writer.write_bits(0x1ff, 9).unwrap(); // ninth unary one is not permitted
        assert!(spectral(&mut BitReader::new(&writer.into_bytes()), 11).is_err());
        // Book 7's (1,1) codeword must have two sign bits; neither may come from padding.
        let (code, length) = tables::BOOK_7[9];
        let leading = (8 - length % 8) % 8;
        let mut writer = BitWriter::new();
        writer.write_bits(0, leading).unwrap();
        writer.write_bits(u64::from(code), length).unwrap();
        let data = writer.into_bytes();
        let mut reader = BitReader::new(&data);
        reader.skip_bits(usize::from(leading)).unwrap();
        assert!(spectral(&mut reader, 7).is_err());
    }
}
