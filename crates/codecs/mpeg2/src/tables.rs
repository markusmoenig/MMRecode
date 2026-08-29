//! Normative MPEG-1/2 scan, quantizer, and variable-length code tables.
//!
//! Numeric values in this module are standardized syntax data from ITU-T H.262 tables B-1
//! through B-15. They are explicit so decoding is deterministic and has no runtime dependency.

use std::sync::OnceLock;

use mmrecode_bitstream::{VlcEntry, VlcTable};

pub(crate) const ZIGZAG: [usize; 64] = [
    0, 1, 8, 16, 9, 2, 3, 10, 17, 24, 32, 25, 18, 11, 4, 5, 12, 19, 26, 33, 40, 48, 41, 34, 27, 20,
    13, 6, 7, 14, 21, 28, 35, 42, 49, 56, 57, 50, 43, 36, 29, 22, 15, 23, 30, 37, 44, 51, 58, 59,
    52, 45, 38, 31, 39, 46, 53, 60, 61, 54, 47, 55, 62, 63,
];

pub(crate) const ALTERNATE_SCAN: [usize; 64] = [
    0, 8, 16, 24, 1, 9, 2, 10, 17, 25, 32, 40, 48, 56, 57, 49, 41, 33, 26, 18, 3, 11, 4, 12, 19,
    27, 34, 42, 50, 58, 35, 43, 51, 59, 20, 28, 5, 13, 6, 14, 21, 29, 36, 44, 52, 60, 37, 45, 53,
    61, 22, 30, 7, 15, 23, 31, 38, 46, 54, 62, 39, 47, 55, 63,
];

pub(crate) const DEFAULT_INTRA_MATRIX: [i32; 64] = [
    8, 16, 19, 22, 26, 27, 29, 34, 16, 16, 22, 24, 27, 29, 34, 37, 19, 22, 26, 27, 29, 34, 34, 38,
    22, 22, 26, 27, 29, 34, 37, 40, 22, 26, 27, 29, 32, 35, 40, 48, 26, 27, 29, 32, 35, 40, 48, 58,
    26, 27, 29, 34, 38, 46, 56, 69, 27, 29, 35, 38, 46, 56, 69, 83,
];

pub(crate) const DEFAULT_NON_INTRA_MATRIX: [i32; 64] = [16; 64];

pub(crate) const NON_LINEAR_QUANTISER_SCALE: [i32; 32] = [
    0, 1, 2, 3, 4, 5, 6, 7, 8, 10, 12, 14, 16, 18, 20, 22, 24, 28, 32, 36, 40, 44, 48, 52, 56, 64,
    72, 80, 88, 96, 104, 112,
];

pub(crate) const DC_LUMA: [(u16, u8); 12] = [
    (0x4, 3),
    (0x0, 2),
    (0x1, 2),
    (0x5, 3),
    (0x6, 3),
    (0xe, 4),
    (0x1e, 5),
    (0x3e, 6),
    (0x7e, 7),
    (0xfe, 8),
    (0x1fe, 9),
    (0x1ff, 9),
];

pub(crate) const DC_CHROMA: [(u16, u8); 12] = [
    (0x0, 2),
    (0x1, 2),
    (0x2, 2),
    (0x6, 3),
    (0xe, 4),
    (0x1e, 5),
    (0x3e, 6),
    (0x7e, 7),
    (0xfe, 8),
    (0x1fe, 9),
    (0x3fe, 10),
    (0x3ff, 10),
];

pub(crate) const MACROBLOCK_ADDRESS_INCREMENT: [(u16, u8); 36] = [
    (0x1, 1),
    (0x3, 3),
    (0x2, 3),
    (0x3, 4),
    (0x2, 4),
    (0x3, 5),
    (0x2, 5),
    (0x7, 7),
    (0x6, 7),
    (0xb, 8),
    (0xa, 8),
    (0x9, 8),
    (0x8, 8),
    (0x7, 8),
    (0x6, 8),
    (0x17, 10),
    (0x16, 10),
    (0x15, 10),
    (0x14, 10),
    (0x13, 10),
    (0x12, 10),
    (0x23, 11),
    (0x22, 11),
    (0x21, 11),
    (0x20, 11),
    (0x1f, 11),
    (0x1e, 11),
    (0x1d, 11),
    (0x1c, 11),
    (0x1b, 11),
    (0x1a, 11),
    (0x19, 11),
    (0x18, 11),
    (0x8, 11),
    (0xf, 11),
    (0x0, 8),
];

pub(crate) const CODED_BLOCK_PATTERN: [(u16, u8); 64] = [
    (0x1, 9),
    (0xb, 5),
    (0x9, 5),
    (0xd, 6),
    (0xd, 4),
    (0x17, 7),
    (0x13, 7),
    (0x1f, 8),
    (0xc, 4),
    (0x16, 7),
    (0x12, 7),
    (0x1e, 8),
    (0x13, 5),
    (0x1b, 8),
    (0x17, 8),
    (0x13, 8),
    (0xb, 4),
    (0x15, 7),
    (0x11, 7),
    (0x1d, 8),
    (0x11, 5),
    (0x19, 8),
    (0x15, 8),
    (0x11, 8),
    (0xf, 6),
    (0xf, 8),
    (0xd, 8),
    (0x3, 9),
    (0xf, 5),
    (0xb, 8),
    (0x7, 8),
    (0x7, 9),
    (0xa, 4),
    (0x14, 7),
    (0x10, 7),
    (0x1c, 8),
    (0xe, 6),
    (0xe, 8),
    (0xc, 8),
    (0x2, 9),
    (0x10, 5),
    (0x18, 8),
    (0x14, 8),
    (0x10, 8),
    (0xe, 5),
    (0xa, 8),
    (0x6, 8),
    (0x6, 9),
    (0x12, 5),
    (0x1a, 8),
    (0x16, 8),
    (0x12, 8),
    (0xd, 5),
    (0x9, 8),
    (0x5, 8),
    (0x5, 9),
    (0xc, 5),
    (0x8, 8),
    (0x4, 8),
    (0x4, 9),
    (0x7, 3),
    (0xa, 5),
    (0x8, 5),
    (0xc, 6),
];

pub(crate) const MOTION_CODE: [(u16, u8); 17] = [
    (0x1, 1),
    (0x1, 2),
    (0x1, 3),
    (0x1, 4),
    (0x3, 6),
    (0x5, 7),
    (0x4, 7),
    (0x3, 7),
    (0xb, 9),
    (0xa, 9),
    (0x9, 9),
    (0x11, 10),
    (0x10, 10),
    (0xf, 10),
    (0xe, 10),
    (0xd, 10),
    (0xc, 10),
];

pub(crate) const MACROBLOCK_P: [(u16, u8, u16); 7] = [
    (3, 5, 0x01),
    (1, 2, 0x02),
    (1, 3, 0x04),
    (1, 1, 0x06),
    (1, 6, 0x11),
    (1, 5, 0x12),
    (2, 5, 0x16),
];

pub(crate) const MACROBLOCK_B: [(u16, u8, u16); 11] = [
    (3, 5, 0x01),
    (2, 3, 0x08),
    (3, 3, 0x0a),
    (2, 4, 0x04),
    (3, 4, 0x06),
    (2, 2, 0x0c),
    (3, 2, 0x0e),
    (1, 6, 0x11),
    (2, 6, 0x1a),
    (3, 6, 0x16),
    (2, 5, 0x1e),
];

pub(crate) const RUN: [u8; 111] = [
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 2, 2, 2, 2, 2, 3,
    3, 3, 3, 4, 4, 4, 5, 5, 5, 6, 6, 6, 7, 7, 8, 8, 9, 9, 10, 10, 11, 11, 12, 12, 13, 13, 14, 14,
    15, 15, 16, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26, 27, 28, 29, 30, 31,
];

pub(crate) const LEVEL: [i16; 111] = [
    1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26,
    27, 28, 29, 30, 31, 32, 33, 34, 35, 36, 37, 38, 39, 40, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12,
    13, 14, 15, 16, 17, 18, 1, 2, 3, 4, 5, 1, 2, 3, 4, 1, 2, 3, 1, 2, 3, 1, 2, 3, 1, 2, 1, 2, 1, 2,
    1, 2, 1, 2, 1, 2, 1, 2, 1, 2, 1, 2, 1, 2, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1,
];

pub(crate) const DCT_COEFFICIENT_ZERO: [(u16, u8); 113] = [
    (0x3, 2),
    (0x4, 4),
    (0x5, 5),
    (0x6, 7),
    (0x26, 8),
    (0x21, 8),
    (0xa, 10),
    (0x1d, 12),
    (0x18, 12),
    (0x13, 12),
    (0x10, 12),
    (0x1a, 13),
    (0x19, 13),
    (0x18, 13),
    (0x17, 13),
    (0x1f, 14),
    (0x1e, 14),
    (0x1d, 14),
    (0x1c, 14),
    (0x1b, 14),
    (0x1a, 14),
    (0x19, 14),
    (0x18, 14),
    (0x17, 14),
    (0x16, 14),
    (0x15, 14),
    (0x14, 14),
    (0x13, 14),
    (0x12, 14),
    (0x11, 14),
    (0x10, 14),
    (0x18, 15),
    (0x17, 15),
    (0x16, 15),
    (0x15, 15),
    (0x14, 15),
    (0x13, 15),
    (0x12, 15),
    (0x11, 15),
    (0x10, 15),
    (0x3, 3),
    (0x6, 6),
    (0x25, 8),
    (0xc, 10),
    (0x1b, 12),
    (0x16, 13),
    (0x15, 13),
    (0x1f, 15),
    (0x1e, 15),
    (0x1d, 15),
    (0x1c, 15),
    (0x1b, 15),
    (0x1a, 15),
    (0x19, 15),
    (0x13, 16),
    (0x12, 16),
    (0x11, 16),
    (0x10, 16),
    (0x5, 4),
    (0x4, 7),
    (0xb, 10),
    (0x14, 12),
    (0x14, 13),
    (0x7, 5),
    (0x24, 8),
    (0x1c, 12),
    (0x13, 13),
    (0x6, 5),
    (0xf, 10),
    (0x12, 12),
    (0x7, 6),
    (0x9, 10),
    (0x12, 13),
    (0x5, 6),
    (0x1e, 12),
    (0x14, 16),
    (0x4, 6),
    (0x15, 12),
    (0x7, 7),
    (0x11, 12),
    (0x5, 7),
    (0x11, 13),
    (0x27, 8),
    (0x10, 13),
    (0x23, 8),
    (0x1a, 16),
    (0x22, 8),
    (0x19, 16),
    (0x20, 8),
    (0x18, 16),
    (0xe, 10),
    (0x17, 16),
    (0xd, 10),
    (0x16, 16),
    (0x8, 10),
    (0x15, 16),
    (0x1f, 12),
    (0x1a, 12),
    (0x19, 12),
    (0x17, 12),
    (0x16, 12),
    (0x1f, 13),
    (0x1e, 13),
    (0x1d, 13),
    (0x1c, 13),
    (0x1b, 13),
    (0x1f, 16),
    (0x1e, 16),
    (0x1d, 16),
    (0x1c, 16),
    (0x1b, 16),
    (0x1, 6),
    (0x2, 2),
];

pub(crate) const DCT_COEFFICIENT_ONE: [(u16, u8); 113] = [
    (0x02, 2),
    (0x06, 3),
    (0x07, 4),
    (0x1c, 5),
    (0x1d, 5),
    (0x05, 6),
    (0x04, 6),
    (0x7b, 7),
    (0x7c, 7),
    (0x23, 8),
    (0x22, 8),
    (0xfa, 8),
    (0xfb, 8),
    (0xfe, 8),
    (0xff, 8),
    (0x1f, 14),
    (0x1e, 14),
    (0x1d, 14),
    (0x1c, 14),
    (0x1b, 14),
    (0x1a, 14),
    (0x19, 14),
    (0x18, 14),
    (0x17, 14),
    (0x16, 14),
    (0x15, 14),
    (0x14, 14),
    (0x13, 14),
    (0x12, 14),
    (0x11, 14),
    (0x10, 14),
    (0x18, 15),
    (0x17, 15),
    (0x16, 15),
    (0x15, 15),
    (0x14, 15),
    (0x13, 15),
    (0x12, 15),
    (0x11, 15),
    (0x10, 15),
    (0x02, 3),
    (0x06, 5),
    (0x79, 7),
    (0x27, 8),
    (0x20, 8),
    (0x16, 13),
    (0x15, 13),
    (0x1f, 15),
    (0x1e, 15),
    (0x1d, 15),
    (0x1c, 15),
    (0x1b, 15),
    (0x1a, 15),
    (0x19, 15),
    (0x13, 16),
    (0x12, 16),
    (0x11, 16),
    (0x10, 16),
    (0x05, 5),
    (0x07, 7),
    (0xfc, 8),
    (0x0c, 10),
    (0x14, 13),
    (0x07, 5),
    (0x26, 8),
    (0x1c, 12),
    (0x13, 13),
    (0x06, 6),
    (0xfd, 8),
    (0x12, 12),
    (0x07, 6),
    (0x04, 9),
    (0x12, 13),
    (0x06, 7),
    (0x1e, 12),
    (0x14, 16),
    (0x04, 7),
    (0x15, 12),
    (0x05, 7),
    (0x11, 12),
    (0x78, 7),
    (0x11, 13),
    (0x7a, 7),
    (0x10, 13),
    (0x21, 8),
    (0x1a, 16),
    (0x25, 8),
    (0x19, 16),
    (0x24, 8),
    (0x18, 16),
    (0x05, 9),
    (0x17, 16),
    (0x07, 9),
    (0x16, 16),
    (0x0d, 10),
    (0x15, 16),
    (0x1f, 12),
    (0x1a, 12),
    (0x19, 12),
    (0x17, 12),
    (0x16, 12),
    (0x1f, 13),
    (0x1e, 13),
    (0x1d, 13),
    (0x1c, 13),
    (0x1b, 13),
    (0x1f, 16),
    (0x1e, 16),
    (0x1d, 16),
    (0x1c, 16),
    (0x1b, 16),
    (0x01, 6),
    (0x06, 4),
];

static DC_LUMA_TABLE: OnceLock<VlcTable> = OnceLock::new();
static DC_CHROMA_TABLE: OnceLock<VlcTable> = OnceLock::new();
static MB_ADDRESS_TABLE: OnceLock<VlcTable> = OnceLock::new();
static CBP_TABLE: OnceLock<VlcTable> = OnceLock::new();
static MOTION_TABLE: OnceLock<VlcTable> = OnceLock::new();
static MB_P_TABLE: OnceLock<VlcTable> = OnceLock::new();
static MB_B_TABLE: OnceLock<VlcTable> = OnceLock::new();
static DCT_ZERO_TABLE: OnceLock<VlcTable> = OnceLock::new();
static DCT_ONE_TABLE: OnceLock<VlcTable> = OnceLock::new();

pub(crate) fn dc_luma_table() -> &'static VlcTable {
    table(&DC_LUMA_TABLE, &DC_LUMA)
}
pub(crate) fn dc_chroma_table() -> &'static VlcTable {
    table(&DC_CHROMA_TABLE, &DC_CHROMA)
}
pub(crate) fn macroblock_address_table() -> &'static VlcTable {
    table(&MB_ADDRESS_TABLE, &MACROBLOCK_ADDRESS_INCREMENT)
}
pub(crate) fn coded_block_pattern_table() -> &'static VlcTable {
    table(&CBP_TABLE, &CODED_BLOCK_PATTERN)
}
pub(crate) fn motion_code_table() -> &'static VlcTable {
    table(&MOTION_TABLE, &MOTION_CODE)
}
pub(crate) fn dct_zero_table() -> &'static VlcTable {
    table(&DCT_ZERO_TABLE, &DCT_COEFFICIENT_ZERO)
}
pub(crate) fn dct_one_table() -> &'static VlcTable {
    table(&DCT_ONE_TABLE, &DCT_COEFFICIENT_ONE)
}

pub(crate) fn macroblock_p_table() -> &'static VlcTable {
    MB_P_TABLE.get_or_init(|| sparse_table(&MACROBLOCK_P))
}

pub(crate) fn macroblock_b_table() -> &'static VlcTable {
    MB_B_TABLE.get_or_init(|| sparse_table(&MACROBLOCK_B))
}

fn table(lock: &'static OnceLock<VlcTable>, values: &[(u16, u8)]) -> &'static VlcTable {
    lock.get_or_init(|| {
        VlcTable::new(
            values
                .iter()
                .enumerate()
                .map(|(symbol, &(code, bit_length))| VlcEntry {
                    code: u32::from(code),
                    bit_length,
                    symbol: u16::try_from(symbol).expect("MPEG VLC table is small"),
                })
                .collect(),
        )
        .expect("standard MPEG VLC table is prefix-free")
    })
}

fn sparse_table(values: &[(u16, u8, u16)]) -> VlcTable {
    VlcTable::new(
        values
            .iter()
            .map(|&(code, bit_length, symbol)| VlcEntry {
                code: u32::from(code),
                bit_length,
                symbol,
            })
            .collect(),
    )
    .expect("standard MPEG macroblock table is prefix-free")
}
