use mmrecode_bitstream::BitReader;
use mmrecode_core::{Error, Result};

const COEFF_TOKEN_LEN: [[u8; 68]; 4] = [
    [
        1, 0, 0, 0, 6, 2, 0, 0, 8, 6, 3, 0, 9, 8, 7, 5, 10, 9, 8, 6, 11, 10, 9, 7, 13, 11, 10, 8,
        13, 13, 11, 9, 13, 13, 13, 10, 14, 14, 13, 11, 14, 14, 14, 13, 15, 15, 14, 14, 15, 15, 15,
        14, 16, 15, 15, 15, 16, 16, 16, 15, 16, 16, 16, 16, 16, 16, 16, 16,
    ],
    [
        2, 0, 0, 0, 6, 2, 0, 0, 6, 5, 3, 0, 7, 6, 6, 4, 8, 6, 6, 4, 8, 7, 7, 5, 9, 8, 8, 6, 11, 9,
        9, 6, 11, 11, 11, 7, 12, 11, 11, 9, 12, 12, 12, 11, 12, 12, 12, 11, 13, 13, 13, 12, 13, 13,
        13, 13, 13, 14, 13, 13, 14, 14, 14, 13, 14, 14, 14, 14,
    ],
    [
        4, 0, 0, 0, 6, 4, 0, 0, 6, 5, 4, 0, 6, 5, 5, 4, 7, 5, 5, 4, 7, 5, 5, 4, 7, 6, 6, 4, 7, 6,
        6, 4, 8, 7, 7, 5, 8, 8, 7, 6, 9, 8, 8, 7, 9, 9, 8, 8, 9, 9, 9, 8, 10, 9, 9, 9, 10, 10, 10,
        10, 10, 10, 10, 10, 10, 10, 10, 10,
    ],
    [
        6, 0, 0, 0, 6, 6, 0, 0, 6, 6, 6, 0, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6,
        6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6,
        6, 6, 6, 6, 6, 6, 6, 6,
    ],
];

const COEFF_TOKEN_BITS: [[u16; 68]; 4] = [
    [
        1, 0, 0, 0, 5, 1, 0, 0, 7, 4, 1, 0, 7, 6, 5, 3, 7, 6, 5, 3, 7, 6, 5, 4, 15, 6, 5, 4, 11,
        14, 5, 4, 8, 10, 13, 4, 15, 14, 9, 4, 11, 10, 13, 12, 15, 14, 9, 12, 11, 10, 13, 8, 15, 1,
        9, 12, 11, 14, 13, 8, 7, 10, 9, 12, 4, 6, 5, 8,
    ],
    [
        3, 0, 0, 0, 11, 2, 0, 0, 7, 7, 3, 0, 7, 10, 9, 5, 7, 6, 5, 4, 4, 6, 5, 6, 7, 6, 5, 8, 15,
        6, 5, 4, 11, 14, 13, 4, 15, 10, 9, 4, 11, 14, 13, 12, 8, 10, 9, 8, 15, 14, 13, 12, 11, 10,
        9, 12, 7, 11, 6, 8, 9, 8, 10, 1, 7, 6, 5, 4,
    ],
    [
        15, 0, 0, 0, 15, 14, 0, 0, 11, 15, 13, 0, 8, 12, 14, 12, 15, 10, 11, 11, 11, 8, 9, 10, 9,
        14, 13, 9, 8, 10, 9, 8, 15, 14, 13, 13, 11, 14, 10, 12, 15, 10, 13, 12, 11, 14, 9, 12, 8,
        10, 13, 8, 13, 7, 9, 12, 9, 12, 11, 10, 5, 8, 7, 6, 1, 4, 3, 2,
    ],
    [
        3, 0, 0, 0, 0, 1, 0, 0, 4, 5, 6, 0, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21,
        22, 23, 24, 25, 26, 27, 28, 29, 30, 31, 32, 33, 34, 35, 36, 37, 38, 39, 40, 41, 42, 43, 44,
        45, 46, 47, 48, 49, 50, 51, 52, 53, 54, 55, 56, 57, 58, 59, 60, 61, 62, 63,
    ],
];

const CHROMA_DC_COEFF_TOKEN_LEN: [u8; 20] =
    [2, 0, 0, 0, 6, 1, 0, 0, 6, 6, 3, 0, 6, 7, 7, 6, 6, 8, 8, 7];
const CHROMA_DC_COEFF_TOKEN_BITS: [u16; 20] =
    [1, 0, 0, 0, 7, 1, 0, 0, 4, 6, 1, 0, 3, 3, 2, 5, 2, 3, 2, 0];

const TOTAL_ZEROS_LEN: [[u8; 16]; 15] = [
    [1, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7, 8, 8, 9, 9, 9],
    [3, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 6, 6, 6, 6, 0],
    [4, 3, 3, 3, 4, 4, 3, 3, 4, 5, 5, 6, 5, 6, 0, 0],
    [5, 3, 4, 4, 3, 3, 3, 4, 3, 4, 5, 5, 5, 0, 0, 0],
    [4, 4, 4, 3, 3, 3, 3, 3, 4, 5, 4, 5, 0, 0, 0, 0],
    [6, 5, 3, 3, 3, 3, 3, 3, 4, 3, 6, 0, 0, 0, 0, 0],
    [6, 5, 3, 3, 3, 2, 3, 4, 3, 6, 0, 0, 0, 0, 0, 0],
    [6, 4, 5, 3, 2, 2, 3, 3, 6, 0, 0, 0, 0, 0, 0, 0],
    [6, 6, 4, 2, 2, 3, 2, 5, 0, 0, 0, 0, 0, 0, 0, 0],
    [5, 5, 3, 2, 2, 2, 4, 0, 0, 0, 0, 0, 0, 0, 0, 0],
    [4, 4, 3, 3, 1, 3, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
    [4, 4, 2, 1, 3, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
    [3, 3, 1, 2, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
    [2, 2, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
    [1, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
];

const TOTAL_ZEROS_BITS: [[u16; 16]; 15] = [
    [1, 3, 2, 3, 2, 3, 2, 3, 2, 3, 2, 3, 2, 3, 2, 1],
    [7, 6, 5, 4, 3, 5, 4, 3, 2, 3, 2, 3, 2, 1, 0, 0],
    [5, 7, 6, 5, 4, 3, 4, 3, 2, 3, 2, 1, 1, 0, 0, 0],
    [3, 7, 5, 4, 6, 5, 4, 3, 3, 2, 2, 1, 0, 0, 0, 0],
    [5, 4, 3, 7, 6, 5, 4, 3, 2, 1, 1, 0, 0, 0, 0, 0],
    [1, 1, 7, 6, 5, 4, 3, 2, 1, 1, 0, 0, 0, 0, 0, 0],
    [1, 1, 5, 4, 3, 3, 2, 1, 1, 0, 0, 0, 0, 0, 0, 0],
    [1, 1, 1, 3, 3, 2, 2, 1, 0, 0, 0, 0, 0, 0, 0, 0],
    [1, 0, 1, 3, 2, 1, 1, 1, 0, 0, 0, 0, 0, 0, 0, 0],
    [1, 0, 1, 3, 2, 1, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0],
    [0, 1, 1, 2, 1, 3, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
    [0, 1, 1, 1, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
    [0, 1, 1, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
    [0, 1, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
    [0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
];

const CHROMA_DC_TOTAL_ZEROS_LEN: [[u8; 4]; 3] = [[1, 2, 3, 3], [1, 2, 2, 0], [1, 1, 0, 0]];
const CHROMA_DC_TOTAL_ZEROS_BITS: [[u16; 4]; 3] = [[1, 1, 1, 0], [1, 1, 0, 0], [1, 0, 0, 0]];

const RUN_LEN: [[u8; 16]; 7] = [
    [1, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
    [1, 2, 2, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
    [2, 2, 2, 2, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
    [2, 2, 2, 3, 3, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
    [2, 2, 3, 3, 3, 3, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
    [2, 3, 3, 3, 3, 3, 3, 0, 0, 0, 0, 0, 0, 0, 0, 0],
    [3, 3, 3, 3, 3, 3, 3, 4, 5, 6, 7, 8, 9, 10, 11, 0],
];
const RUN_BITS: [[u16; 16]; 7] = [
    [1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
    [1, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
    [3, 2, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
    [3, 2, 1, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
    [3, 2, 3, 2, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
    [3, 0, 1, 3, 2, 5, 4, 0, 0, 0, 0, 0, 0, 0, 0, 0],
    [7, 6, 5, 4, 3, 2, 1, 1, 1, 1, 1, 1, 1, 1, 1, 0],
];

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ResidualBlock {
    pub(crate) coefficients: Vec<i32>,
    pub(crate) total_coeff: u8,
}

pub(crate) fn decode_residual_block(
    reader: &mut BitReader<'_>,
    n_c: i8,
    max_num_coeff: u8,
) -> Result<ResidualBlock> {
    if !matches!(max_num_coeff, 4 | 15 | 16) {
        return Err(Error::Unsupported(format!(
            "native H.264 CAVLC maxNumCoeff {max_num_coeff} is not supported"
        )));
    }
    let (total_coeff, trailing_ones) = decode_coeff_token(reader, n_c)?;
    if total_coeff > max_num_coeff || trailing_ones > total_coeff.min(3) {
        return Err(Error::InvalidData("invalid H.264 CAVLC coeff_token".into()));
    }
    let mut coefficients = vec![0_i32; usize::from(max_num_coeff)];
    if total_coeff == 0 {
        return Ok(ResidualBlock {
            coefficients,
            total_coeff,
        });
    }

    let mut levels = Vec::with_capacity(usize::from(total_coeff));
    for _ in 0..trailing_ones {
        levels.push(if reader.read_bit()? { -1 } else { 1 });
    }
    let mut suffix_length = u8::from(total_coeff > 10 && trailing_ones < 3);
    for index in trailing_ones..total_coeff {
        let prefix = read_level_prefix(reader)?;
        let suffix_size = if prefix == 14 && suffix_length == 0 {
            4
        } else if prefix >= 15 {
            prefix - 3
        } else {
            suffix_length
        };
        let suffix = if suffix_size == 0 {
            0_i64
        } else {
            i64::try_from(reader.read_bits(suffix_size)?)
                .map_err(|_| Error::InvalidData("H.264 CAVLC level suffix overflows".into()))?
        };
        let mut level_code = (i64::from(prefix.min(15)) << suffix_length) + suffix;
        if prefix >= 15 && suffix_length == 0 {
            level_code += 15;
        }
        if prefix >= 16 {
            level_code += (1_i64 << (prefix - 3)) - 4096;
        }
        if index == trailing_ones && trailing_ones < 3 {
            level_code += 2;
        }
        let level = if level_code % 2 == 0 {
            (level_code + 2) >> 1
        } else {
            (-level_code - 1) >> 1
        };
        let level = i32::try_from(level)
            .map_err(|_| Error::InvalidData("H.264 CAVLC level overflows i32".into()))?;
        levels.push(level);
        if suffix_length == 0 {
            suffix_length = 1;
        }
        if i64::from(level.unsigned_abs()) > (3_i64 << (suffix_length - 1)) && suffix_length < 6 {
            suffix_length += 1;
        }
    }

    let mut zeros_left = if total_coeff == max_num_coeff {
        0
    } else {
        decode_total_zeros(reader, total_coeff, max_num_coeff)?
    };
    let mut runs = Vec::with_capacity(usize::from(total_coeff));
    for _ in 0..total_coeff - 1 {
        let run = if zeros_left == 0 {
            0
        } else {
            decode_run_before(reader, zeros_left)?
        };
        zeros_left = zeros_left
            .checked_sub(run)
            .ok_or_else(|| Error::InvalidData("H.264 CAVLC run exceeds zeros remaining".into()))?;
        runs.push(run);
    }
    runs.push(zeros_left);

    let mut coefficient_index = -1_i32;
    for index in (0..usize::from(total_coeff)).rev() {
        coefficient_index += i32::from(runs[index]) + 1;
        let destination = usize::try_from(coefficient_index)
            .ok()
            .filter(|&value| value < coefficients.len())
            .ok_or_else(|| {
                Error::InvalidData("H.264 CAVLC coefficient run overflows block".into())
            })?;
        coefficients[destination] = levels[index];
    }
    Ok(ResidualBlock {
        coefficients,
        total_coeff,
    })
}

fn decode_coeff_token(reader: &mut BitReader<'_>, n_c: i8) -> Result<(u8, u8)> {
    let (lengths, bits): (&[u8], &[u16]) = if n_c == -1 {
        (&CHROMA_DC_COEFF_TOKEN_LEN, &CHROMA_DC_COEFF_TOKEN_BITS)
    } else {
        let table = match n_c {
            ..=1 => 0,
            2..=3 => 1,
            4..=7 => 2,
            _ => 3,
        };
        (&COEFF_TOKEN_LEN[table], &COEFF_TOKEN_BITS[table])
    };
    let index = decode_vlc(reader, lengths, bits, 16, "coeff_token")?;
    Ok((
        u8::try_from(index / 4).expect("coefficient token table is bounded"),
        u8::try_from(index % 4).expect("coefficient token table is bounded"),
    ))
}

fn decode_total_zeros(
    reader: &mut BitReader<'_>,
    total_coeff: u8,
    max_num_coeff: u8,
) -> Result<u8> {
    let row = usize::from(total_coeff - 1);
    let index = if max_num_coeff == 4 {
        decode_vlc(
            reader,
            &CHROMA_DC_TOTAL_ZEROS_LEN[row],
            &CHROMA_DC_TOTAL_ZEROS_BITS[row],
            3,
            "chroma total_zeros",
        )?
    } else {
        decode_vlc(
            reader,
            &TOTAL_ZEROS_LEN[row],
            &TOTAL_ZEROS_BITS[row],
            9,
            "total_zeros",
        )?
    };
    u8::try_from(index).map_err(|_| Error::InvalidData("H.264 total_zeros overflows".into()))
}

fn decode_run_before(reader: &mut BitReader<'_>, zeros_left: u8) -> Result<u8> {
    let row = usize::from(zeros_left.min(7) - 1);
    let index = decode_vlc(reader, &RUN_LEN[row], &RUN_BITS[row], 11, "run_before")?;
    u8::try_from(index).map_err(|_| Error::InvalidData("H.264 run_before overflows".into()))
}

fn decode_vlc(
    reader: &mut BitReader<'_>,
    lengths: &[u8],
    bits: &[u16],
    max_length: u8,
    name: &str,
) -> Result<usize> {
    let mut code = 0_u16;
    for length in 1..=max_length {
        code = (code << 1) | u16::from(reader.read_bit()?);
        if let Some(index) = lengths
            .iter()
            .zip(bits)
            .position(|(&candidate_length, &candidate)| {
                candidate_length == length && candidate == code
            })
        {
            return Ok(index);
        }
    }
    Err(Error::InvalidData(format!(
        "invalid H.264 CAVLC {name} codeword"
    )))
}

fn read_level_prefix(reader: &mut BitReader<'_>) -> Result<u8> {
    let mut prefix = 0_u8;
    while !reader.read_bit()? {
        prefix = prefix
            .checked_add(1)
            .ok_or_else(|| Error::InvalidData("H.264 CAVLC level prefix overflows".into()))?;
        if prefix > 31 {
            return Err(Error::InvalidData(
                "H.264 CAVLC level prefix exceeds native bounds".into(),
            ));
        }
    }
    Ok(prefix)
}

#[cfg(test)]
mod tests {
    use mmrecode_bitstream::BitReader;

    use super::decode_residual_block;

    #[test]
    fn decodes_empty_and_trailing_one_blocks() {
        let mut empty = BitReader::new(&[0b1000_0000]);
        assert_eq!(
            decode_residual_block(&mut empty, 0, 16)
                .unwrap()
                .coefficients,
            [0; 16]
        );

        // coeff_token=(TotalCoeff 1, TrailingOnes 1), positive sign, total_zeros=0.
        let mut one = BitReader::new(&[0b0101_0000]);
        let decoded = decode_residual_block(&mut one, 0, 16).unwrap();
        assert_eq!(decoded.total_coeff, 1);
        assert_eq!(decoded.coefficients[0], 1);
        assert!(decoded.coefficients[1..].iter().all(|&value| value == 0));
    }

    #[test]
    fn decodes_non_trailing_level_and_zero_run() {
        // TotalCoeff=1, TrailingOnes=0; level_prefix=0 becomes +2 for the first level.
        let mut level = BitReader::new(&[0b0001_0111]);
        let decoded = decode_residual_block(&mut level, 0, 16).unwrap();
        assert_eq!(decoded.coefficients[0], 2);

        // TotalCoeff=1, TrailingOnes=1, positive sign, total_zeros=3.
        let mut run = BitReader::new(&[0b0100_0110]);
        let decoded = decode_residual_block(&mut run, 0, 16).unwrap();
        assert_eq!(decoded.coefficients[3], 1);
        assert_eq!(decoded.coefficients.iter().sum::<i32>(), 1);
    }
}
