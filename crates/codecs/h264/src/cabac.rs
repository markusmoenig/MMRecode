//! Context-adaptive binary arithmetic decoding primitives.

use mmrecode_bitstream::BitReader;
use mmrecode_core::{Error, Result};

// ITU-T H.264 Tables 9-44 and 9-45.
const RANGE_LPS: [[u16; 4]; 64] = [
    [128, 176, 208, 240],
    [128, 167, 197, 227],
    [128, 158, 187, 216],
    [123, 150, 178, 205],
    [116, 142, 169, 195],
    [111, 135, 160, 185],
    [105, 128, 152, 175],
    [100, 122, 144, 166],
    [95, 116, 137, 158],
    [90, 110, 130, 150],
    [85, 104, 123, 142],
    [81, 99, 117, 135],
    [77, 94, 111, 128],
    [73, 89, 105, 122],
    [69, 85, 100, 116],
    [66, 80, 95, 110],
    [62, 76, 90, 104],
    [59, 72, 86, 99],
    [56, 69, 81, 94],
    [53, 65, 77, 89],
    [51, 62, 73, 85],
    [48, 59, 69, 80],
    [46, 56, 66, 76],
    [43, 53, 63, 72],
    [41, 50, 59, 69],
    [39, 48, 56, 65],
    [37, 45, 54, 62],
    [35, 43, 51, 59],
    [33, 41, 48, 56],
    [32, 39, 46, 53],
    [30, 37, 43, 50],
    [29, 35, 41, 48],
    [27, 33, 39, 45],
    [26, 31, 37, 43],
    [24, 30, 35, 41],
    [23, 28, 33, 39],
    [22, 27, 32, 37],
    [21, 26, 30, 35],
    [20, 24, 29, 33],
    [19, 23, 27, 31],
    [18, 22, 26, 30],
    [17, 21, 25, 28],
    [16, 20, 23, 27],
    [15, 19, 22, 25],
    [14, 18, 21, 24],
    [14, 17, 20, 23],
    [13, 16, 19, 22],
    [12, 15, 18, 21],
    [12, 14, 17, 20],
    [11, 14, 16, 19],
    [11, 13, 15, 18],
    [10, 12, 15, 17],
    [10, 12, 14, 16],
    [9, 11, 13, 15],
    [9, 11, 12, 14],
    [8, 10, 12, 14],
    [8, 9, 11, 13],
    [7, 9, 11, 12],
    [7, 9, 10, 12],
    [7, 8, 10, 11],
    [6, 8, 9, 11],
    [6, 7, 9, 10],
    [6, 7, 8, 9],
    [2, 2, 2, 2],
];

const TRANSITION_MPS: [u8; 64] = [
    1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26,
    27, 28, 29, 30, 31, 32, 33, 34, 35, 36, 37, 38, 39, 40, 41, 42, 43, 44, 45, 46, 47, 48, 49, 50,
    51, 52, 53, 54, 55, 56, 57, 58, 59, 60, 61, 62, 62, 63,
];

const TRANSITION_LPS: [u8; 64] = [
    0, 0, 1, 2, 2, 4, 4, 5, 6, 7, 8, 9, 9, 11, 11, 12, 13, 13, 15, 15, 16, 16, 18, 18, 19, 19, 21,
    21, 22, 22, 23, 24, 24, 25, 26, 26, 27, 27, 28, 29, 29, 30, 30, 30, 31, 32, 32, 33, 33, 33, 34,
    34, 35, 35, 35, 36, 36, 36, 37, 37, 37, 38, 38, 63,
];

// ITU-T H.264 Table 9-12, context indices 0 through 10 for I/SI slices.
const I_MACROBLOCK_CONTEXT_INIT: [(i8, i8); 11] = [
    (20, -15),
    (2, 54),
    (3, 74),
    (20, -15),
    (2, 54),
    (3, 74),
    (-28, 127),
    (-23, 104),
    (-6, 53),
    (-1, 54),
    (7, 51),
];

/// One adaptive probability state used by a CABAC context model.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ContextState {
    probability_state: u8,
    most_probable_symbol: bool,
}

impl ContextState {
    /// Initializes a context from its table `(m, n)` pair and the slice QP.
    pub(crate) fn from_mn(m: i8, n: i8, slice_qp_y: i32) -> Result<Self> {
        if !(0..=51).contains(&slice_qp_y) {
            return Err(Error::InvalidData(
                "H.264 CABAC context initialization QP is out of range".into(),
            ));
        }
        let pre_context_state = (((i32::from(m) * slice_qp_y) >> 4) + i32::from(n)).clamp(1, 126);
        if pre_context_state <= 63 {
            Ok(Self {
                probability_state: u8::try_from(63 - pre_context_state)
                    .expect("clamped CABAC state fits u8"),
                most_probable_symbol: false,
            })
        } else {
            Ok(Self {
                probability_state: u8::try_from(pre_context_state - 64)
                    .expect("clamped CABAC state fits u8"),
                most_probable_symbol: true,
            })
        }
    }
}

/// Initializes the I-slice contexts used to decode `mb_type`.
pub(crate) fn initial_i_macroblock_contexts(slice_qp_y: i32) -> Result<[ContextState; 11]> {
    initial_contexts(&I_MACROBLOCK_CONTEXT_INIT, slice_qp_y)
}

/// Initializes a contiguous group of CABAC context models from `(m, n)` table entries.
pub(crate) fn initial_contexts<const N: usize>(
    parameters: &[(i8, i8); N],
    slice_qp_y: i32,
) -> Result<[ContextState; N]> {
    let initial = ContextState::from_mn(parameters[0].0, parameters[0].1, slice_qp_y)?;
    let mut contexts = [initial; N];
    for (context, &(m, n)) in contexts.iter_mut().zip(parameters) {
        *context = ContextState::from_mn(m, n, slice_qp_y)?;
    }
    Ok(contexts)
}

/// Binary arithmetic decoder state for one CABAC slice.
pub(crate) struct CabacDecoder<'reader, 'data> {
    bits: &'reader mut BitReader<'data>,
    range: u16,
    offset: u16,
    zero_padding_bits: u8,
}

impl<'reader, 'data> CabacDecoder<'reader, 'data> {
    /// Consumes `cabac_alignment_one_bit` syntax and initializes the arithmetic registers.
    pub(crate) fn new(bits: &'reader mut BitReader<'data>) -> Result<Self> {
        while !bits.bit_position().is_multiple_of(8) {
            if !bits.read_bit()? {
                return Err(Error::InvalidData(
                    "H.264 CABAC alignment contains a zero bit".into(),
                ));
            }
        }
        let mut decoder = Self {
            bits,
            range: 510,
            offset: 0,
            zero_padding_bits: 0,
        };
        decoder.initialize_registers()?;
        Ok(decoder)
    }

    /// Decodes one regular decision bin and updates its context model.
    pub(crate) fn decision(&mut self, context: &mut ContextState) -> Result<bool> {
        let range_index = usize::from((self.range >> 6) & 3);
        let state_index = usize::from(context.probability_state);
        let range_lps = RANGE_LPS[state_index][range_index];
        self.range -= range_lps;
        let bin = if self.offset >= self.range {
            self.offset -= self.range;
            self.range = range_lps;
            let bin = !context.most_probable_symbol;
            if context.probability_state == 0 {
                context.most_probable_symbol = !context.most_probable_symbol;
            }
            context.probability_state = TRANSITION_LPS[state_index];
            bin
        } else {
            context.probability_state = TRANSITION_MPS[state_index];
            context.most_probable_symbol
        };
        self.renormalize()?;
        Ok(bin)
    }

    /// Decodes one equiprobable bypass bin without a context model.
    pub(crate) fn bypass(&mut self) -> Result<bool> {
        self.offset = self.offset * 2 + u16::from(self.coded_bit()?);
        if self.offset >= self.range {
            self.offset -= self.range;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    /// Decodes `end_of_slice_flag` using the termination process.
    pub(crate) fn terminate(&mut self) -> Result<bool> {
        self.range -= 2;
        if self.offset >= self.range {
            Ok(true)
        } else {
            self.renormalize()?;
            Ok(false)
        }
    }

    /// Reads byte-aligned `I_PCM` samples and restarts arithmetic decoding afterward.
    pub(crate) fn pcm_samples(&mut self, count: usize) -> Result<Vec<u8>> {
        self.bits.align_to_byte();
        let mut samples = Vec::with_capacity(count);
        for _ in 0..count {
            samples.push(
                u8::try_from(self.bits.read_bits(8)?).expect("eight-bit H.264 PCM sample fits u8"),
            );
        }
        self.initialize_registers()?;
        Ok(samples)
    }

    fn initialize_registers(&mut self) -> Result<()> {
        self.range = 510;
        self.zero_padding_bits = 0;
        self.offset =
            u16::try_from(self.bits.read_bits(9)?).expect("nine-bit CABAC offset always fits u16");
        if self.offset >= 510 {
            return Err(Error::InvalidData(
                "H.264 CABAC initial offset is not less than 510".into(),
            ));
        }
        Ok(())
    }

    fn renormalize(&mut self) -> Result<()> {
        while self.range < 256 {
            self.range <<= 1;
            self.offset = (self.offset << 1) | u16::from(self.coded_bit()?);
        }
        Ok(())
    }

    fn coded_bit(&mut self) -> Result<bool> {
        if self.bits.bits_remaining() > 0 {
            return self.bits.read_bit();
        }
        // H.264 decoders conventionally provide zero-padded input for CABAC's
        // arithmetic lookahead. Keep that behavior bounded so truncated slices
        // still fail instead of turning into an unbounded stream of zero bins.
        self.zero_padding_bits = self
            .zero_padding_bits
            .checked_add(1)
            .filter(|&count| count <= 16)
            .ok_or_else(|| Error::InvalidData("H.264 CABAC zero padding exceeds 16 bits".into()))?;
        Ok(false)
    }
}

#[cfg(test)]
mod tests {
    use mmrecode_bitstream::BitReader;

    use super::{CabacDecoder, ContextState};

    #[test]
    fn initializes_contexts_on_both_sides_of_the_mps_boundary() {
        assert_eq!(
            ContextState::from_mn(20, -15, 26).unwrap(),
            ContextState {
                probability_state: 46,
                most_probable_symbol: false,
            }
        );
        assert_eq!(
            ContextState::from_mn(-3, 70, 26).unwrap(),
            ContextState {
                probability_state: 1,
                most_probable_symbol: true,
            }
        );
    }

    #[test]
    fn decodes_regular_bypass_and_termination_bins() {
        let mut bits = BitReader::new(&[0x12, 0x34, 0x56, 0x78, 0x9a, 0xbc]);
        let mut decoder = CabacDecoder::new(&mut bits).unwrap();
        let mut context = ContextState {
            probability_state: 10,
            most_probable_symbol: false,
        };
        let regular = (0..12)
            .map(|_| decoder.decision(&mut context).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(
            regular,
            [
                false, false, false, false, false, false, false, false, false, true, false, false,
            ]
        );
        assert_eq!(
            (0..4)
                .map(|_| decoder.bypass().unwrap())
                .collect::<Vec<_>>(),
            [true, true, false, true]
        );
        assert!(!decoder.terminate().unwrap());
        assert_eq!(
            context,
            ContextState {
                probability_state: 17,
                most_probable_symbol: false,
            }
        );
    }

    #[test]
    fn rejects_zero_alignment_and_forbidden_initial_offsets() {
        let mut bad_alignment = BitReader::new(&[0x80, 0x00]);
        bad_alignment.skip_bits(1).unwrap();
        assert!(CabacDecoder::new(&mut bad_alignment).is_err());

        let mut forbidden_offset = BitReader::new(&[0xff, 0x80]);
        assert!(CabacDecoder::new(&mut forbidden_offset).is_err());
    }

    #[test]
    fn bounds_implicit_zero_padding_for_truncated_cabac_data() {
        let mut bits = BitReader::new(&[0x00, 0x00]);
        let mut decoder = CabacDecoder::new(&mut bits).unwrap();
        let error = (0..64).find_map(|_| decoder.bypass().err());
        assert!(error.is_some());
    }
}
