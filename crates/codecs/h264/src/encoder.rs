use std::collections::VecDeque;

use mmrecode_bitstream::BitWriter;
use mmrecode_core::{
    CodecDescriptor, CodecId, Encoder, Error, FieldOrder, FourCc, MediaType, Packet, PacketFlags,
    PixelFormat, Plane, Result, StreamId, VideoEncoderSettings, VideoFrame,
};

use crate::{
    CODEC_NAME,
    cabac::{CabacEncoder, initial_i_macroblock_contexts},
    cavlc::encode_residual_block,
    decoder::{CabacBContexts, CabacIContexts, CabacPContexts},
};

const PROFILE_BASELINE: u8 = 66;
const PROFILE_MAIN: u8 = 77;
const PROFILE_HIGH: u8 = 100;
const BASELINE_COMPATIBILITY: u8 = 0xc0;
const NAL_LENGTH_SIZE: u8 = 4;
const MAX_MACROBLOCKS_PER_FRAME: usize = 36_864;
const RATE_CONTROL_BUFFER_FRAMES: u64 = 8;

// ITU-T H.264 Annex A, Table A-1. Bitrate and CPB entries use the Baseline/Main
// profile factor of 1_000 bits per table unit.
const AVC_LEVELS: [AvcLevel; 20] = [
    AvcLevel::new("1", 10, false, 1_485, 99, 396, 64, 175),
    AvcLevel::new("1b", 11, true, 1_485, 99, 396, 128, 350),
    AvcLevel::new("1.1", 11, false, 3_000, 396, 900, 192, 500),
    AvcLevel::new("1.2", 12, false, 6_000, 396, 2_376, 384, 1_000),
    AvcLevel::new("1.3", 13, false, 11_880, 396, 2_376, 768, 2_000),
    AvcLevel::new("2", 20, false, 11_880, 396, 2_376, 2_000, 2_000),
    AvcLevel::new("2.1", 21, false, 19_800, 792, 4_752, 4_000, 4_000),
    AvcLevel::new("2.2", 22, false, 20_250, 1_620, 8_100, 4_000, 4_000),
    AvcLevel::new("3", 30, false, 40_500, 1_620, 8_100, 10_000, 10_000),
    AvcLevel::new("3.1", 31, false, 108_000, 3_600, 18_000, 14_000, 14_000),
    AvcLevel::new("3.2", 32, false, 216_000, 5_120, 20_480, 20_000, 20_000),
    AvcLevel::new("4", 40, false, 245_760, 8_192, 32_768, 20_000, 25_000),
    AvcLevel::new("4.1", 41, false, 245_760, 8_192, 32_768, 50_000, 62_500),
    AvcLevel::new("4.2", 42, false, 522_240, 8_704, 34_816, 50_000, 62_500),
    AvcLevel::new("5", 50, false, 589_824, 22_080, 110_400, 135_000, 135_000),
    AvcLevel::new("5.1", 51, false, 983_040, 36_864, 184_320, 240_000, 240_000),
    AvcLevel::new(
        "5.2", 52, false, 2_073_600, 36_864, 184_320, 240_000, 240_000,
    ),
    AvcLevel::new(
        "6", 60, false, 4_177_920, 139_264, 696_320, 240_000, 240_000,
    ),
    AvcLevel::new(
        "6.1", 61, false, 8_355_840, 139_264, 696_320, 480_000, 480_000,
    ),
    AvcLevel::new(
        "6.2", 62, false, 16_711_680, 139_264, 696_320, 800_000, 800_000,
    ),
];

#[derive(Clone, Debug)]
struct Configuration {
    width: usize,
    height: usize,
    coded_width: usize,
    coded_height: usize,
    mode: EncoderMode,
    qp: i32,
    gop_size: usize,
    search_range: i32,
    analysis: AnalysisPreset,
    scene_cut_threshold: u8,
    max_references: usize,
    b_frames: usize,
    b_direct_mode: BDirectMode,
    aq_strength: u8,
    pic_order_cnt_type0: bool,
    transform_8x8_mode: bool,
    transform_bypass: bool,
    entropy_coding: EntropyCoding,
    scaling_matrix: ScalingMatrixPreset,
    level: LevelConfiguration,
}

#[derive(Clone, Copy, Debug)]
struct SequenceConfiguration {
    width: usize,
    height: usize,
    coded_width: usize,
    coded_height: usize,
    max_references: usize,
    pic_order_cnt_type0: bool,
    hrd: Option<EncoderHrd>,
    profile: EncoderProfile,
    transform_bypass: bool,
    scaling_matrix: ScalingMatrixPreset,
    bt709: bool,
    level: AvcLevel,
}

#[derive(Clone, Copy, Debug)]
struct LevelConfiguration {
    level: AvcLevel,
    macroblocks_per_frame: usize,
    default_frame_duration_ticks: u64,
    time_base: mmrecode_core::Rational,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct AvcLevel {
    name: &'static str,
    level_idc: u8,
    constraint_set3: bool,
    max_macroblocks_per_second: u64,
    max_frame_size: usize,
    max_dpb_macroblocks: usize,
    max_bitrate_kbps: u64,
    max_cpb_kbits: u64,
}

impl AvcLevel {
    #[allow(clippy::too_many_arguments)]
    const fn new(
        name: &'static str,
        level_idc: u8,
        constraint_set3: bool,
        max_macroblocks_per_second: u64,
        max_frame_size: usize,
        max_dpb_macroblocks: usize,
        max_bitrate_kbps: u64,
        max_cpb_kbits: u64,
    ) -> Self {
        Self {
            name,
            level_idc,
            constraint_set3,
            max_macroblocks_per_second,
            max_frame_size,
            max_dpb_macroblocks,
            max_bitrate_kbps,
            max_cpb_kbits,
        }
    }
}

struct EncodedFrame {
    nal: Vec<u8>,
    reconstructed: VideoFrame,
    coded_reconstruction: [Vec<u8>; 3],
    motion_l0: Vec<[Option<MotionState>; 16]>,
    reference_l0_poc: Vec<[Option<i32>; 16]>,
    macroblock_intra: Vec<bool>,
}

#[derive(Clone, Debug)]
struct EncoderReference {
    planes: [Vec<u8>; 3],
    pic_order_count: i32,
    motion_l0: Vec<[Option<MotionState>; 16]>,
    reference_l0_poc: Vec<[Option<i32>; 16]>,
    macroblock_intra: Vec<bool>,
}

impl EncoderReference {
    fn colocated_zero(&self, address: usize, block_x: usize, block_y: usize) -> bool {
        if self.macroblock_intra.get(address).copied().unwrap_or(true) {
            return false;
        }
        self.motion_l0
            .get(address)
            .and_then(|blocks| blocks[luma_block_index(block_x, block_y)])
            .is_some_and(|motion| {
                motion.reference_index == Some(0)
                    && motion.vector.x.unsigned_abs() <= 1
                    && motion.vector.y.unsigned_abs() <= 1
            })
    }

    fn colocated_motion(
        &self,
        address: usize,
        block_x: usize,
        block_y: usize,
    ) -> Option<(MotionVector, i32)> {
        if self.macroblock_intra.get(address).copied().unwrap_or(true) {
            return None;
        }
        let block = luma_block_index(block_x, block_y);
        self.motion_l0
            .get(address)
            .and_then(|blocks| blocks[block])
            .filter(|motion| motion.reference_index.is_some())
            .zip(
                self.reference_l0_poc
                    .get(address)
                    .and_then(|blocks| blocks[block]),
            )
            .map(|(motion, reference_poc)| (motion.vector, reference_poc))
    }
}

#[derive(Debug)]
struct PendingBFrame {
    frame: VideoFrame,
    pic_order_cnt_lsb: u8,
    picture_order_count: i32,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct MotionVector {
    x: i32,
    y: i32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct MotionState {
    vector: MotionVector,
    reference_index: Option<u8>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum MotionPredictionKind {
    Normal,
    Top16x8,
    Bottom16x8,
    Left8x16,
    Right8x16,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct InterPartition {
    block_x: usize,
    block_y: usize,
    block_width: usize,
    block_height: usize,
    prediction_kind: MotionPredictionKind,
}

#[derive(Clone, Copy, Debug)]
struct SelectedPartition {
    partition: InterPartition,
    predicted: MotionVector,
    motion: MotionVector,
    reference_index: u8,
}

#[derive(Debug)]
struct InterDecision {
    macroblock_type: u8,
    sub_macroblock_types: [u8; 4],
    partitions: Vec<SelectedPartition>,
    luma_prediction: Vec<u8>,
    chroma_predictions: [Vec<u8>; 2],
}

#[derive(Debug)]
struct BInterDecision {
    macroblock_type: u8,
    direct: bool,
    sub_macroblock_types: [u8; 4],
    partitions: Vec<BSelectedPartition>,
    luma_prediction: Vec<u8>,
    chroma_predictions: [Vec<u8>; 2],
}

#[derive(Clone, Copy, Debug)]
struct BSelectedPartition {
    list0: Option<SelectedPartition>,
    list1: Option<SelectedPartition>,
    direct: bool,
}

impl BSelectedPartition {
    fn partition(self) -> InterPartition {
        self.list0
            .or(self.list1)
            .expect("B partition uses at least one reference list")
            .partition
    }
}

#[derive(Clone, Copy, Debug)]
enum BPrediction {
    L0,
    L1,
    Bi,
}

impl BPrediction {
    const fn uses_l0(self) -> bool {
        matches!(self, Self::L0 | Self::Bi)
    }

    const fn uses_l1(self) -> bool {
        matches!(self, Self::L1 | Self::Bi)
    }
}

#[derive(Debug)]
struct P8x8Candidate {
    cost: u64,
    sub_type: u8,
    motions: [Option<MotionState>; 16],
    luma_prediction: Vec<u8>,
    chroma_predictions: [Vec<u8>; 2],
    partitions: Vec<SelectedPartition>,
}

#[derive(Debug)]
struct B8x8Candidate {
    cost: u64,
    sub_type: u8,
    history_l0: Vec<[Option<MotionState>; 16]>,
    history_l1: Vec<[Option<MotionState>; 16]>,
    prediction: [Vec<u8>; 3],
    partitions: Vec<BSelectedPartition>,
}

#[derive(Clone, Debug)]
struct ChromaResidual {
    dc: [i32; 4],
    ac: [[i32; 15]; 4],
}

#[derive(Clone, Debug)]
enum InterLumaResidual {
    FourByFour(Box<[[i32; 16]; 16]>),
    BypassFourByFour(Box<[[i32; 16]; 16]>),
    EightByEight(Box<[[i32; 64]; 4]>),
}

impl InterLumaResidual {
    const fn uses_8x8(&self) -> bool {
        matches!(self, Self::EightByEight(_))
    }

    fn coded_block_pattern(&self) -> u8 {
        match self {
            Self::FourByFour(levels) | Self::BypassFourByFour(levels) => {
                luma_coded_block_pattern(levels)
            }
            Self::EightByEight(levels) => (0..4).fold(0_u8, |pattern, group| {
                pattern | (u8::from(levels[group].iter().any(|&level| level != 0)) << group)
            }),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum EncoderMode {
    #[default]
    Ipcm,
    Intra16,
    Intra4,
    Intra8,
    Inter,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum AnalysisPreset {
    Fast,
    #[default]
    Full,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum EntropyCoding {
    #[default]
    Cavlc,
    Cabac,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum EncoderProfile {
    Baseline,
    Main,
    High,
}

impl EncoderProfile {
    const fn profile_idc(self) -> u8 {
        match self {
            Self::Baseline => PROFILE_BASELINE,
            Self::Main => PROFILE_MAIN,
            Self::High => PROFILE_HIGH,
        }
    }

    const fn compatibility(self) -> u8 {
        match self {
            Self::Baseline => BASELINE_COMPATIBILITY,
            Self::Main | Self::High => 0,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum ProfilePreference {
    #[default]
    Auto,
    Baseline,
    Main,
    High,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum ScalingMatrixPreset {
    #[default]
    Flat,
    Jvt,
}

const FLAT_4X4: [u8; 16] = [16; 16];
const FLAT_8X8: [u8; 64] = [16; 64];
const JVT_4X4_INTRA: [u8; 16] = [
    6, 13, 20, 28, 13, 20, 28, 32, 20, 28, 32, 37, 28, 32, 37, 42,
];
const JVT_4X4_INTER: [u8; 16] = [
    10, 14, 20, 24, 14, 20, 24, 27, 20, 24, 27, 30, 24, 27, 30, 34,
];
const JVT_8X8_INTRA: [u8; 64] = [
    6, 10, 13, 16, 18, 23, 25, 27, 10, 11, 16, 18, 23, 25, 27, 29, 13, 16, 18, 23, 25, 27, 29, 31,
    16, 18, 23, 25, 27, 29, 31, 33, 18, 23, 25, 27, 29, 31, 33, 36, 23, 25, 27, 29, 31, 33, 36, 38,
    25, 27, 29, 31, 33, 36, 38, 40, 27, 29, 31, 33, 36, 38, 40, 42,
];
const JVT_8X8_INTER: [u8; 64] = [
    9, 13, 15, 17, 19, 21, 22, 24, 13, 13, 17, 19, 21, 22, 24, 25, 15, 17, 19, 21, 22, 24, 25, 27,
    17, 19, 21, 22, 24, 25, 27, 28, 19, 21, 22, 24, 25, 27, 28, 30, 21, 22, 24, 25, 27, 28, 30, 32,
    22, 24, 25, 27, 28, 30, 32, 33, 24, 25, 27, 28, 30, 32, 33, 35,
];

impl ScalingMatrixPreset {
    const fn four_by_four(self, intra: bool) -> &'static [u8; 16] {
        match (self, intra) {
            (Self::Flat, _) => &FLAT_4X4,
            (Self::Jvt, true) => &JVT_4X4_INTRA,
            (Self::Jvt, false) => &JVT_4X4_INTER,
        }
    }

    const fn eight_by_eight(self, intra: bool) -> &'static [u8; 64] {
        match (self, intra) {
            (Self::Flat, _) => &FLAT_8X8,
            (Self::Jvt, true) => &JVT_8X8_INTRA,
            (Self::Jvt, false) => &JVT_8X8_INTER,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum LevelPreference {
    #[default]
    Auto,
    Exact(AvcLevel),
}

#[derive(Clone, Copy, Debug)]
struct LevelRequirements {
    width_in_macroblocks: usize,
    height_in_macroblocks: usize,
    macroblocks_per_frame: usize,
    decoded_picture_buffer_macroblocks: usize,
    frame_duration_ticks: u64,
    time_base: mmrecode_core::Rational,
    bitrate: Option<u64>,
    cpb_size: Option<u64>,
}

impl LevelRequirements {
    fn failures(self, level: AvcLevel) -> Vec<String> {
        let mut failures = Vec::new();
        if self.macroblocks_per_frame > level.max_frame_size {
            failures.push(format!(
                "{} macroblocks/frame exceeds {}",
                self.macroblocks_per_frame, level.max_frame_size
            ));
        }
        let squared_dimension_limit = level.max_frame_size.saturating_mul(8);
        if self
            .width_in_macroblocks
            .saturating_mul(self.width_in_macroblocks)
            > squared_dimension_limit
            || self
                .height_in_macroblocks
                .saturating_mul(self.height_in_macroblocks)
                > squared_dimension_limit
        {
            failures.push(format!(
                "{}x{} macroblock dimensions exceed the level's dimension limit",
                self.width_in_macroblocks, self.height_in_macroblocks
            ));
        }
        if !level_allows_frame_interval(
            level,
            self.macroblocks_per_frame,
            self.frame_duration_ticks,
            self.time_base,
        ) {
            failures.push(format!(
                "the declared frame cadence exceeds {} macroblocks/second",
                level.max_macroblocks_per_second
            ));
        }
        if self.decoded_picture_buffer_macroblocks > level.max_dpb_macroblocks {
            failures.push(format!(
                "{} decoded-picture-buffer macroblocks exceeds {}",
                self.decoded_picture_buffer_macroblocks, level.max_dpb_macroblocks
            ));
        }
        if self
            .bitrate
            .is_some_and(|bitrate| bitrate > level.max_bitrate_kbps.saturating_mul(1_000))
        {
            failures.push(format!(
                "bitrate exceeds {} bit/s",
                level.max_bitrate_kbps.saturating_mul(1_000)
            ));
        }
        if self
            .cpb_size
            .is_some_and(|size| size > level.max_cpb_kbits.saturating_mul(1_000))
        {
            failures.push(format!(
                "CPB size exceeds {} bits",
                level.max_cpb_kbits.saturating_mul(1_000)
            ));
        }
        failures
    }
}

fn parse_level_preference(value: &str) -> Option<LevelPreference> {
    if value == "auto" {
        return Some(LevelPreference::Auto);
    }
    AVC_LEVELS
        .iter()
        .copied()
        .find(|level| level.name == value)
        .map(LevelPreference::Exact)
}

fn negotiate_level(
    preference: LevelPreference,
    requirements: LevelRequirements,
) -> Result<AvcLevel> {
    if let LevelPreference::Exact(level) = preference {
        let failures = requirements.failures(level);
        if failures.is_empty() {
            return Ok(level);
        }
        return Err(Error::Unsupported(format!(
            "H.264 level {} cannot represent this sequence: {}",
            level.name,
            failures.join(", ")
        )));
    }
    AVC_LEVELS
        .iter()
        .copied()
        .find(|level| requirements.failures(*level).is_empty())
        .ok_or_else(|| {
            Error::Unsupported(
                "H.264 sequence exceeds the supported Level 6.2 limits; reduce dimensions, frame rate, references, bitrate, or VBV size"
                    .into(),
            )
        })
}

fn level_allows_frame_interval(
    level: AvcLevel,
    macroblocks_per_frame: usize,
    duration_ticks: u64,
    time_base: mmrecode_core::Rational,
) -> bool {
    if duration_ticks == 0 || time_base.numerator() <= 0 || time_base.denominator() <= 0 {
        return false;
    }
    let required = (macroblocks_per_frame as u128)
        .saturating_mul(u128::try_from(time_base.denominator()).expect("positive denominator"));
    let capacity = u128::from(level.max_macroblocks_per_second)
        .saturating_mul(u128::from(duration_ticks))
        .saturating_mul(
            u128::try_from(time_base.numerator()).expect("positive time-base numerator"),
        );
    required <= capacity
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum BDirectMode {
    #[default]
    Spatial,
    Temporal,
}

#[derive(Clone, Copy, Debug)]
struct BDirectContext {
    mode: BDirectMode,
    picture_order_count: i32,
}

impl BDirectContext {
    #[cfg(test)]
    const SPATIAL: Self = Self {
        mode: BDirectMode::Spatial,
        picture_order_count: 0,
    };
}

#[derive(Clone, Debug)]
struct RateControl {
    bitrate: u64,
    target_bits_per_frame: u64,
    buffer_capacity_bits: i128,
    buffer_fullness_bits: i128,
    current_qp: i32,
    fixed_buffer_capacity_bits: Option<i128>,
}

impl RateControl {
    fn new(
        bitrate: u64,
        time_base: mmrecode_core::Rational,
        initial_qp: i32,
        buffer_capacity_bits: Option<u64>,
    ) -> Result<Self> {
        if bitrate == 0 || time_base.numerator() <= 0 || time_base.denominator() <= 0 {
            return Err(Error::Unsupported(
                "H.264 bitrate control requires a non-zero bitrate and positive time_base".into(),
            ));
        }
        let target_bits_per_frame = Self::bits_for_interval(bitrate, 1, time_base)
            .ok_or_else(|| Error::Unsupported("H.264 target frame size overflows".into()))?;
        let fixed_buffer_capacity_bits = buffer_capacity_bits.map(i128::from);
        let buffer_capacity_bits = fixed_buffer_capacity_bits.unwrap_or_else(|| {
            i128::from(target_bits_per_frame) * i128::from(RATE_CONTROL_BUFFER_FRAMES)
        });
        Ok(Self {
            bitrate,
            target_bits_per_frame,
            buffer_capacity_bits,
            buffer_fullness_bits: 0,
            current_qp: initial_qp,
            fixed_buffer_capacity_bits,
        })
    }

    fn bits_for_interval(
        bitrate: u64,
        ticks: u64,
        time_base: mmrecode_core::Rational,
    ) -> Option<u64> {
        if ticks == 0 || time_base.numerator() <= 0 || time_base.denominator() <= 0 {
            return None;
        }
        u128::from(bitrate)
            .checked_mul(u128::from(ticks))?
            .checked_mul(u128::try_from(time_base.numerator()).ok()?)?
            .div_ceil(u128::try_from(time_base.denominator()).ok()?)
            .max(1)
            .try_into()
            .ok()
    }

    fn observe(&mut self, encoded_bits: u64, duration: Option<mmrecode_core::Timestamp>) {
        let target = duration
            .filter(|duration| duration.value > 0)
            .and_then(|duration| {
                Self::bits_for_interval(
                    self.bitrate,
                    u64::try_from(duration.value).ok()?,
                    duration.time_base,
                )
            })
            .unwrap_or(self.target_bits_per_frame);
        self.buffer_capacity_bits = self.fixed_buffer_capacity_bits.unwrap_or_else(|| {
            i128::from(target).saturating_mul(i128::from(RATE_CONTROL_BUFFER_FRAMES))
        });
        let half_capacity = self.buffer_capacity_bits / 2;
        self.buffer_fullness_bits = (self.buffer_fullness_bits + i128::from(encoded_bits)
            - i128::from(target))
        .clamp(-half_capacity, half_capacity);

        let size_adjustment = if u128::from(encoded_bits) >= u128::from(target) * 2 {
            2
        } else if u128::from(encoded_bits) * 4 >= u128::from(target) * 5 {
            1
        } else if u128::from(encoded_bits) * 2 <= u128::from(target) {
            -2
        } else if u128::from(encoded_bits) * 5 <= u128::from(target) * 4 {
            -1
        } else {
            0
        };
        let pressure_adjustment = if self.buffer_fullness_bits > i128::from(target) * 2 {
            1
        } else if self.buffer_fullness_bits < -i128::from(target) * 2 {
            -1
        } else {
            0
        };
        self.current_qp = (self.current_qp + size_adjustment + pressure_adjustment).clamp(0, 51);
    }
}

#[derive(Clone, Copy, Debug)]
struct EncoderHrd {
    bit_rate_scale: u8,
    bit_rate_value_minus1: u32,
    cpb_size_scale: u8,
    cpb_size_value_minus1: u32,
    signalled_cpb_size: u64,
    initial_cpb_removal_delay: u32,
    num_units_in_tick: u32,
    time_scale: u32,
    b_frames: usize,
}

#[derive(Clone, Debug)]
struct HrdState {
    parameters: EncoderHrd,
    cpb_removal_delay: u32,
    decode_index: i32,
    cpb_fullness_bits: u64,
}

impl EncoderHrd {
    fn new(
        bitrate: u64,
        buffer_milliseconds: u64,
        time_base: mmrecode_core::Rational,
        b_frames: usize,
    ) -> Result<Self> {
        if bitrate == 0 || time_base.numerator() <= 0 || time_base.denominator() <= 0 {
            return Err(Error::Unsupported(
                "H.264 HRD requires a non-zero bitrate and positive time_base".into(),
            ));
        }
        let numerator = u64::try_from(time_base.numerator()).expect("positive time-base numerator");
        let denominator =
            u64::try_from(time_base.denominator()).expect("positive time-base denominator");
        let divisor = greatest_common_divisor(numerator, denominator);
        let num_units_in_tick = u32::try_from(numerator / divisor)
            .map_err(|_| Error::Unsupported("H.264 HRD time-base numerator exceeds VUI".into()))?;
        let time_scale = u32::try_from(
            (denominator / divisor)
                .checked_mul(2)
                .ok_or_else(|| Error::Unsupported("H.264 HRD time scale overflows".into()))?,
        )
        .map_err(|_| Error::Unsupported("H.264 HRD time scale exceeds VUI".into()))?;
        let requested_cpb_size = u128::from(bitrate)
            .checked_mul(u128::from(buffer_milliseconds))
            .ok_or_else(|| Error::Unsupported("H.264 VBV buffer size overflows".into()))?
            .div_ceil(1_000)
            .max(1)
            .try_into()
            .map_err(|_| Error::Unsupported("H.264 VBV buffer size overflows".into()))?;
        let (bit_rate_scale, bit_rate_value_minus1, signalled_bit_rate) =
            scaled_hrd_value(bitrate, 6, "bitrate")?;
        let (cpb_size_scale, cpb_size_value_minus1, signalled_cpb_size) =
            scaled_hrd_value(requested_cpb_size, 4, "CPB size")?;
        let initial_cpb_removal_delay = u32::try_from(
            u128::from(signalled_cpb_size)
                .checked_mul(90_000)
                .expect("u64 CPB size times 90000 fits u128")
                .div_ceil(u128::from(signalled_bit_rate)),
        )
        .map_err(|_| {
            Error::Unsupported("H.264 initial CPB removal delay exceeds 24 bits".into())
        })?;
        if initial_cpb_removal_delay >= 1 << 24 {
            return Err(Error::Unsupported(
                "H.264 initial CPB removal delay exceeds 24 bits".into(),
            ));
        }
        Ok(Self {
            bit_rate_scale,
            bit_rate_value_minus1,
            cpb_size_scale,
            cpb_size_value_minus1,
            signalled_cpb_size,
            initial_cpb_removal_delay,
            num_units_in_tick,
            time_scale,
            b_frames,
        })
    }
}

impl HrdState {
    const DELAY_BITS: u8 = 24;
    const DELAY_MODULUS: u32 = 1 << Self::DELAY_BITS;

    fn new(parameters: EncoderHrd) -> Self {
        Self {
            parameters,
            cpb_removal_delay: 0,
            decode_index: 0,
            cpb_fullness_bits: parameters.signalled_cpb_size,
        }
    }

    fn access_unit_sei(
        &mut self,
        key: bool,
        picture_order_count: i32,
        timing: mmrecode_core::FrameTiming,
    ) -> Result<Vec<u8>> {
        if key {
            self.cpb_removal_delay = 0;
            self.decode_index = 0;
        }
        let duration_ticks = self.duration_clock_ticks(timing.duration)?;
        let display_index = picture_order_count / 2;
        let output_delay_frames = display_index - self.decode_index
            + i32::try_from(self.parameters.b_frames).expect("bounded B-frame count fits i32");
        let dpb_output_delay = u32::try_from(output_delay_frames.max(0))
            .ok()
            .and_then(|frames| frames.checked_mul(duration_ticks))
            .ok_or_else(|| Error::Unsupported("H.264 DPB output delay overflows".into()))?;
        if dpb_output_delay >= Self::DELAY_MODULUS {
            return Err(Error::Unsupported(
                "H.264 DPB output delay exceeds 24 bits".into(),
            ));
        }
        let sei = encode_hrd_sei(
            &self.parameters,
            key,
            self.cpb_removal_delay,
            dpb_output_delay,
        )?;
        self.cpb_removal_delay =
            self.cpb_removal_delay.wrapping_add(duration_ticks) % Self::DELAY_MODULUS;
        self.decode_index += 1;
        Ok(sei)
    }

    fn remove_access_unit(
        &mut self,
        key: bool,
        duration: Option<mmrecode_core::Timestamp>,
        access_unit_bits: u64,
    ) -> Result<()> {
        if key {
            self.cpb_fullness_bits = self.parameters.signalled_cpb_size;
        } else {
            let duration_ticks = self.duration_clock_ticks(duration)?;
            let arrived = u128::from(self.parameters.signalled_bit_rate())
                .checked_mul(u128::from(duration_ticks))
                .and_then(|value| value.checked_mul(u128::from(self.parameters.num_units_in_tick)))
                .ok_or_else(|| Error::Unsupported("H.264 CPB arrival size overflows".into()))?
                .div_ceil(u128::from(self.parameters.time_scale));
            let arrived = u64::try_from(arrived)
                .map_err(|_| Error::Unsupported("H.264 CPB arrival size overflows".into()))?;
            self.cpb_fullness_bits = self
                .cpb_fullness_bits
                .saturating_add(arrived)
                .min(self.parameters.signalled_cpb_size);
        }
        if access_unit_bits > self.cpb_fullness_bits {
            return Err(Error::InvalidData(format!(
                "H.264 access unit requires {access_unit_bits} bits but only {} bits are available in the configured CPB",
                self.cpb_fullness_bits
            )));
        }
        self.cpb_fullness_bits -= access_unit_bits;
        Ok(())
    }

    fn duration_clock_ticks(&self, duration: Option<mmrecode_core::Timestamp>) -> Result<u32> {
        let Some(duration) = duration.filter(|duration| duration.value > 0) else {
            return Ok(2);
        };
        if duration.time_base.numerator() <= 0 || duration.time_base.denominator() <= 0 {
            return Err(Error::InvalidData(
                "H.264 HRD frame duration must use a positive time base".into(),
            ));
        }
        let ticks = u128::try_from(duration.value)
            .expect("positive duration")
            .checked_mul(
                u128::try_from(duration.time_base.numerator()).expect("positive numerator"),
            )
            .and_then(|value| value.checked_mul(u128::from(self.parameters.time_scale)))
            .ok_or_else(|| Error::Unsupported("H.264 HRD duration overflows".into()))?
            .div_ceil(
                u128::try_from(duration.time_base.denominator()).expect("positive denominator")
                    * u128::from(self.parameters.num_units_in_tick),
            )
            .max(1);
        u32::try_from(ticks)
            .map_err(|_| Error::Unsupported("H.264 HRD duration exceeds delay syntax".into()))
    }
}

impl EncoderHrd {
    fn signalled_bit_rate(self) -> u64 {
        (u64::from(self.bit_rate_value_minus1) + 1) << (6 + self.bit_rate_scale)
    }
}

fn greatest_common_divisor(mut left: u64, mut right: u64) -> u64 {
    while right != 0 {
        (left, right) = (right, left % right);
    }
    left
}

fn scaled_hrd_value(value: u64, base_shift: u8, name: &str) -> Result<(u8, u32, u64)> {
    for scale in 0..=15 {
        let unit = 1_u64 << (base_shift + scale);
        let units = value.div_ceil(unit);
        if let Ok(units) = u32::try_from(units)
            && let Some(signalled) = u64::from(units).checked_mul(unit)
        {
            return Ok((scale, units - 1, signalled));
        }
    }
    Err(Error::Unsupported(format!(
        "H.264 HRD {name} cannot be represented"
    )))
}

/// Stateful deterministic H.264/AVC encoder foundation.
///
/// This encoder emits Baseline-, Main-, or High-profile access units. Its default `I_PCM` mode is
/// lossless but intentionally large. Setting the codec-specific `mode`
/// option to `intra16` enables reconstructed-neighbor macroblock prediction, while `intra4`
/// selects among all nine 4x4 luma prediction modes. High Profile `intra8` uses filtered 8x8
/// prediction and transforms. The compressed modes write quantized luma and chroma residuals with
/// CAVLC. `inter` adds periodic Intra4 IDRs, the complete P partition tree
/// down to 4x4, quarter-pixel luma refinement, multiple short-term references, optional B pictures,
/// and optional scene-cut IDRs. The `qp` option selects a luma QP from 0 through 51. When the
/// generic `bitrate` setting is present, it becomes the initial QP for a deterministic frame-level
/// virtual-buffer controller. `aq_strength=1..12` redistributes that picture QP between quiet and
/// textured macroblocks while preserving normative QP-delta state. `vbv_buffer_ms` activates
/// single-CPB NAL HRD signalling and checked removal scheduling. `profile=auto` selects Baseline
/// without B pictures, Main when B pictures are enabled, and High for `intra8`. Explicit High
/// Profile inter coding adaptively selects 4x4 or 8x8 luma transforms for eligible macroblocks.
/// `transform_bypass=true` enables exact QP-zero High Profile Intra4 or inter coding.
/// `scaling_matrix=jvt` selects the standard High Profile intra/inter quantization matrices.
/// `entropy=cabac` enables CABAC for all-intra, P, and B pictures.
/// `analysis=fast` starts with bounded hierarchical/subsampled P16x16 and direct-B analysis, then
/// evaluates split P or explicit B motion for high-error macroblocks. `analysis=full` is the default
/// exhaustive partition/reference search.
#[derive(Debug, Default)]
pub struct H264Encoder {
    configuration: Option<Configuration>,
    packets: VecDeque<Packet>,
    reconstructions: VecDeque<VideoFrame>,
    references: VecDeque<EncoderReference>,
    pending_b_frames: VecDeque<PendingBFrame>,
    decode_timestamps: VecDeque<Option<mmrecode_core::Timestamp>>,
    rate_control: Option<RateControl>,
    hrd_state: Option<HrdState>,
    next_frame_num: u8,
    frames_since_idr: usize,
    flushed: bool,
}

impl H264Encoder {
    /// Receives the normative visible reconstruction associated with an encoded packet.
    ///
    /// In `I_PCM` mode this is pixel-identical to the submitted frame. In compressed modes it is
    /// the locally derived normative lossy reconstruction used for subsequent prediction decisions.
    ///
    /// # Errors
    ///
    /// Returns an error when the encoder has not been configured.
    pub fn receive_reconstructed_frame(&mut self) -> Result<Option<VideoFrame>> {
        if self.configuration.is_none() {
            return Err(Error::InvalidState(
                "H.264 encoder must be configured before receiving reconstruction".into(),
            ));
        }
        Ok(self.reconstructions.pop_front())
    }

    fn picture_configuration(&self, configuration: &Configuration) -> Configuration {
        let mut picture_configuration = configuration.clone();
        if let Some(rate_control) = &self.rate_control {
            picture_configuration.qp = rate_control.current_qp;
        }
        picture_configuration
    }

    fn queue_encoded(
        &mut self,
        encoded: EncodedFrame,
        timing: mmrecode_core::FrameTiming,
        key: bool,
        retain_as_reference: bool,
        picture_order_count: i32,
        max_references: usize,
    ) -> Result<()> {
        let EncodedFrame {
            nal,
            reconstructed,
            coded_reconstruction,
            motion_l0,
            reference_l0_poc,
            macroblock_intra,
        } = encoded;
        let mut next_hrd_state = self.hrd_state.clone();
        let sei = next_hrd_state
            .as_mut()
            .map(|state| state.access_unit_sei(key, picture_order_count, timing))
            .transpose()?;
        let mut data =
            Vec::with_capacity(4 + nal.len() + sei.as_ref().map_or(0, |sei| 4 + sei.len()));
        if let Some(sei) = &sei {
            append_length_prefixed_nal(&mut data, sei)?;
        }
        append_length_prefixed_nal(&mut data, &nal)?;
        let encoded_bits = u64::try_from(data.len())
            .unwrap_or(u64::MAX)
            .saturating_mul(8);
        if let Some(state) = &mut next_hrd_state {
            state.remove_access_unit(key, timing.duration, encoded_bits)?;
        }
        let dts = self.decode_timestamps.pop_front().ok_or_else(|| {
            Error::InvalidState("H.264 encoder decode-timestamp queue is empty".into())
        })?;
        self.packets.push_back(Packet {
            stream_id: StreamId(0),
            data,
            pts: timing.pts,
            dts,
            duration: timing.duration,
            flags: if key {
                PacketFlags::KEY
            } else {
                PacketFlags::empty()
            },
            side_data: Vec::new(),
        });
        if retain_as_reference {
            if key {
                self.references.clear();
            }
            self.references.push_front(EncoderReference {
                planes: coded_reconstruction,
                pic_order_count: picture_order_count,
                motion_l0,
                reference_l0_poc,
                macroblock_intra,
            });
            self.references.truncate(max_references);
        }
        self.reconstructions.push_back(reconstructed);
        if let Some(rate_control) = &mut self.rate_control {
            rate_control.observe(encoded_bits, timing.duration);
        }
        self.hrd_state = next_hrd_state;
        Ok(())
    }

    fn queue_idr_frame(&mut self, frame: &VideoFrame, configuration: &Configuration) -> Result<()> {
        let picture_configuration = self.picture_configuration(configuration);
        let encoded = encode_idr(frame, &picture_configuration, 0)?;
        self.queue_encoded(
            encoded,
            frame.timing,
            true,
            true,
            0,
            configuration.max_references,
        )?;
        self.next_frame_num = 1;
        self.frames_since_idr = 1;
        Ok(())
    }

    fn queue_p_frame(
        &mut self,
        frame: &VideoFrame,
        configuration: &Configuration,
        pic_order_cnt_lsb: u8,
        picture_order_count: i32,
    ) -> Result<()> {
        let picture_configuration = self.picture_configuration(configuration);
        let encoded = encode_p_picture(
            frame,
            &picture_configuration,
            &self.references,
            self.next_frame_num,
            pic_order_cnt_lsb,
        )?;
        self.queue_encoded(
            encoded,
            frame.timing,
            false,
            true,
            picture_order_count,
            configuration.max_references,
        )?;
        self.next_frame_num = (self.next_frame_num + 1) & 15;
        Ok(())
    }

    fn send_immediate_inter_frame(
        &mut self,
        frame: &VideoFrame,
        configuration: &Configuration,
        key: bool,
    ) -> Result<()> {
        if key {
            self.queue_idr_frame(frame, configuration)
        } else {
            self.queue_p_frame(
                frame,
                configuration,
                0,
                picture_order_count(self.frames_since_idr),
            )?;
            self.frames_since_idr += 1;
            Ok(())
        }
    }

    fn send_reordered_inter_frame(
        &mut self,
        frame: VideoFrame,
        configuration: &Configuration,
        key: bool,
    ) -> Result<()> {
        if key {
            while let Some(pending) = self.pending_b_frames.pop_front() {
                self.queue_p_frame(
                    &pending.frame,
                    configuration,
                    pending.pic_order_cnt_lsb,
                    pending.picture_order_count,
                )?;
            }
            return self.queue_idr_frame(&frame, configuration);
        }
        if self.pending_b_frames.len() < configuration.b_frames {
            let pic_order_cnt_lsb = picture_order_count_lsb(self.frames_since_idr);
            let picture_order_count = picture_order_count(self.frames_since_idr);
            self.pending_b_frames.push_back(PendingBFrame {
                frame,
                pic_order_cnt_lsb,
                picture_order_count,
            });
            self.frames_since_idr += 1;
            return Ok(());
        }

        let previous = self
            .references
            .front()
            .cloned()
            .expect("B picture has a previous reference");
        let anchor_poc_lsb = picture_order_count_lsb(self.frames_since_idr);
        let anchor_poc = picture_order_count(self.frames_since_idr);
        self.queue_p_frame(&frame, configuration, anchor_poc_lsb, anchor_poc)?;
        let future = self
            .references
            .front()
            .cloned()
            .expect("encoded anchor becomes a future B reference");
        while let Some(pending) = self.pending_b_frames.pop_front() {
            let picture_configuration = self.picture_configuration(configuration);
            let b_picture = encode_b_picture(
                &pending.frame,
                &picture_configuration,
                &previous,
                &future,
                self.next_frame_num,
                pending.pic_order_cnt_lsb,
                pending.picture_order_count,
            )?;
            self.queue_encoded(
                b_picture,
                pending.frame.timing,
                false,
                false,
                pending.picture_order_count,
                configuration.max_references,
            )?;
        }
        self.frames_since_idr += 1;
        Ok(())
    }
}

fn picture_order_count_lsb(display_index_since_idr: usize) -> u8 {
    u8::try_from((display_index_since_idr * 2) & 15)
        .expect("four-bit H.264 picture order count fits u8")
}

fn picture_order_count(display_index_since_idr: usize) -> i32 {
    i32::try_from(display_index_since_idr * 2)
        .expect("bounded H.264 GOP picture order count fits i32")
}

impl Encoder for H264Encoder {
    #[allow(clippy::too_many_lines)]
    fn configure(&mut self, settings: &VideoEncoderSettings) -> Result<CodecDescriptor> {
        validate_dimensions(settings.width, settings.height)?;
        if settings.pixel_format != PixelFormat::Yuv420p8 {
            return Err(Error::Unsupported(
                "H.264 encoder foundation requires Yuv420p8 input".into(),
            ));
        }
        let mut mode = EncoderMode::Ipcm;
        let mut qp = 26;
        let mut gop_size = 30;
        let mut search_range = 8;
        let mut analysis = AnalysisPreset::Full;
        let mut scene_cut_threshold = 0;
        let mut max_references = 1;
        let mut b_frames = 0;
        let mut b_direct_mode = BDirectMode::Spatial;
        let mut aq_strength = 0;
        let mut vbv_buffer_milliseconds = None;
        let mut profile_preference = ProfilePreference::Auto;
        let mut transform_bypass = false;
        let mut entropy_coding = EntropyCoding::Cavlc;
        let mut scaling_matrix = ScalingMatrixPreset::Flat;
        let mut level_preference = LevelPreference::Auto;
        let mut frame_duration_ticks = 1_u64;
        let mut bt709 = false;
        for (name, value) in &settings.options {
            match name.as_str() {
                "mode" => {
                    mode = match value.as_str() {
                        "ipcm" => EncoderMode::Ipcm,
                        "intra16" => EncoderMode::Intra16,
                        "intra4" => EncoderMode::Intra4,
                        "intra8" => EncoderMode::Intra8,
                        "inter" => EncoderMode::Inter,
                        _ => {
                            return Err(Error::Unsupported(format!(
                                "unsupported H.264 encoder mode {value}; expected ipcm, intra16, intra4, intra8, or inter"
                            )));
                        }
                    };
                }
                "qp" => {
                    qp = value
                        .parse::<i32>()
                        .ok()
                        .filter(|qp| (0..=51).contains(qp))
                        .ok_or_else(|| {
                            Error::Unsupported(format!("invalid H.264 qp {value}; expected 0..=51"))
                        })?;
                }
                "gop_size" => {
                    gop_size = value
                        .parse::<usize>()
                        .ok()
                        .filter(|gop| (1..=1_000).contains(gop))
                        .ok_or_else(|| {
                            Error::Unsupported(format!(
                                "invalid H.264 gop_size {value}; expected 1..=1000"
                            ))
                        })?;
                }
                "search_range" => {
                    search_range = value
                        .parse::<i32>()
                        .ok()
                        .filter(|range| (0..=64).contains(range))
                        .ok_or_else(|| {
                            Error::Unsupported(format!(
                                "invalid H.264 search_range {value}; expected 0..=64"
                            ))
                        })?;
                }
                "analysis" => {
                    analysis = match value.as_str() {
                        "fast" => AnalysisPreset::Fast,
                        "full" => AnalysisPreset::Full,
                        _ => {
                            return Err(Error::Unsupported(format!(
                                "invalid H.264 analysis {value}; expected fast or full"
                            )));
                        }
                    };
                }
                "scene_cut_threshold" => {
                    scene_cut_threshold = value.parse::<u8>().map_err(|_| {
                        Error::Unsupported(format!(
                            "invalid H.264 scene_cut_threshold {value}; expected 0..=255"
                        ))
                    })?;
                }
                "max_refs" => {
                    max_references = value
                        .parse::<usize>()
                        .ok()
                        .filter(|references| (1..=4).contains(references))
                        .ok_or_else(|| {
                            Error::Unsupported(format!(
                                "invalid H.264 max_refs {value}; expected 1..=4"
                            ))
                        })?;
                }
                "b_frames" => {
                    b_frames = value
                        .parse::<usize>()
                        .ok()
                        .filter(|&frames| frames <= 3)
                        .ok_or_else(|| {
                            Error::Unsupported(format!(
                                "invalid H.264 b_frames {value}; expected 0..=3"
                            ))
                        })?;
                }
                "b_direct" => {
                    b_direct_mode = match value.as_str() {
                        "spatial" => BDirectMode::Spatial,
                        "temporal" => BDirectMode::Temporal,
                        _ => {
                            return Err(Error::Unsupported(format!(
                                "invalid H.264 b_direct {value}; expected spatial or temporal"
                            )));
                        }
                    };
                }
                "aq_strength" => {
                    aq_strength = value
                        .parse::<u8>()
                        .ok()
                        .filter(|strength| *strength <= 12)
                        .ok_or_else(|| {
                            Error::Unsupported(format!(
                                "invalid H.264 aq_strength {value}; expected 0..=12"
                            ))
                        })?;
                }
                "vbv_buffer_ms" => {
                    vbv_buffer_milliseconds = Some(
                        value
                            .parse::<u64>()
                            .ok()
                            .filter(|milliseconds| (1..=60_000).contains(milliseconds))
                            .ok_or_else(|| {
                                Error::Unsupported(format!(
                                    "invalid H.264 vbv_buffer_ms {value}; expected 1..=60000"
                                ))
                            })?,
                    );
                }
                "profile" => {
                    profile_preference = match value.as_str() {
                        "auto" => ProfilePreference::Auto,
                        "baseline" => ProfilePreference::Baseline,
                        "main" => ProfilePreference::Main,
                        "high" => ProfilePreference::High,
                        _ => {
                            return Err(Error::Unsupported(format!(
                                "invalid H.264 profile {value}; expected auto, baseline, main, or high"
                            )));
                        }
                    };
                }
                "transform_bypass" => {
                    transform_bypass = match value.as_str() {
                        "false" => false,
                        "true" => true,
                        _ => {
                            return Err(Error::Unsupported(format!(
                                "invalid H.264 transform_bypass {value}; expected true or false"
                            )));
                        }
                    };
                }
                "entropy" => {
                    entropy_coding = match value.as_str() {
                        "cavlc" => EntropyCoding::Cavlc,
                        "cabac" => EntropyCoding::Cabac,
                        _ => {
                            return Err(Error::Unsupported(format!(
                                "invalid H.264 entropy {value}; expected cavlc or cabac"
                            )));
                        }
                    };
                }
                "scaling_matrix" => {
                    scaling_matrix = match value.as_str() {
                        "flat" => ScalingMatrixPreset::Flat,
                        "jvt" => ScalingMatrixPreset::Jvt,
                        _ => {
                            return Err(Error::Unsupported(format!(
                                "invalid H.264 scaling_matrix {value}; expected flat or jvt"
                            )));
                        }
                    };
                }
                "level" => {
                    level_preference = parse_level_preference(value).ok_or_else(|| {
                        Error::Unsupported(format!(
                            "invalid H.264 level {value}; expected auto, 1, 1b, 1.1..1.3, 2..2.2, 3..3.2, 4..4.2, 5..5.2, or 6..6.2"
                        ))
                    })?;
                }
                "frame_duration_ticks" => {
                    frame_duration_ticks = value
                        .parse::<u64>()
                        .ok()
                        .filter(|duration| *duration != 0)
                        .ok_or_else(|| {
                            Error::Unsupported(format!(
                                "invalid H.264 frame_duration_ticks {value}; expected a positive integer"
                            ))
                        })?;
                }
                "color" => {
                    bt709 = match value.as_str() {
                        "unspecified" => false,
                        "bt709" => true,
                        _ => {
                            return Err(Error::Unsupported(format!(
                                "invalid H.264 color {value}; expected unspecified or bt709"
                            )));
                        }
                    };
                }
                _ => {
                    return Err(Error::Unsupported(format!(
                        "unsupported H.264 encoder option {name}={value}"
                    )));
                }
            }
        }

        let macroblocks_wide = settings.width.div_ceil(16);
        let macroblocks_high = settings.height.div_ceil(16);
        let macroblock_count = macroblocks_wide
            .checked_mul(macroblocks_high)
            .ok_or_else(|| Error::InvalidData("H.264 coded dimensions overflow".into()))?;
        if macroblock_count > MAX_MACROBLOCKS_PER_FRAME {
            return Err(Error::Unsupported(format!(
                "H.264 encoder foundation supports at most {MAX_MACROBLOCKS_PER_FRAME} macroblocks per frame"
            )));
        }
        let coded_width = macroblocks_wide * 16;
        let coded_height = macroblocks_high * 16;

        if settings.time_base.numerator() <= 0 || settings.time_base.denominator() <= 0 {
            return Err(Error::Unsupported(
                "H.264 level negotiation requires a positive time_base".into(),
            ));
        }

        if mode != EncoderMode::Inter && (max_references != 1 || b_frames != 0) {
            return Err(Error::Unsupported(
                "H.264 max_refs and b_frames require mode=inter".into(),
            ));
        }
        if b_direct_mode == BDirectMode::Temporal && (mode != EncoderMode::Inter || b_frames == 0) {
            return Err(Error::Unsupported(
                "H.264 b_direct=temporal requires mode=inter and b_frames=1..3".into(),
            ));
        }
        if mode == EncoderMode::Ipcm && aq_strength != 0 {
            return Err(Error::Unsupported(
                "H.264 aq_strength requires mode=intra16, intra4, intra8, or inter".into(),
            ));
        }
        if vbv_buffer_milliseconds.is_some() && settings.bitrate.is_none() {
            return Err(Error::Unsupported(
                "H.264 vbv_buffer_ms requires a target bitrate".into(),
            ));
        }
        let profile = match profile_preference {
            ProfilePreference::Auto
                if mode == EncoderMode::Intra8
                    || transform_bypass
                    || scaling_matrix == ScalingMatrixPreset::Jvt =>
            {
                EncoderProfile::High
            }
            ProfilePreference::Auto if b_frames == 0 && entropy_coding == EntropyCoding::Cavlc => {
                EncoderProfile::Baseline
            }
            ProfilePreference::Baseline => EncoderProfile::Baseline,
            ProfilePreference::Auto | ProfilePreference::Main => EncoderProfile::Main,
            ProfilePreference::High => EncoderProfile::High,
        };
        if profile == EncoderProfile::Baseline && b_frames != 0 {
            return Err(Error::Unsupported(
                "H.264 Baseline Profile does not support B pictures; use profile=main or auto"
                    .into(),
            ));
        }
        if entropy_coding == EntropyCoding::Cabac && profile == EncoderProfile::Baseline {
            return Err(Error::Unsupported(
                "H.264 CABAC requires profile=main, profile=high, or profile=auto".into(),
            ));
        }
        if mode == EncoderMode::Intra8 && profile != EncoderProfile::High {
            return Err(Error::Unsupported(
                "H.264 Intra8x8 coding requires profile=high or auto".into(),
            ));
        }
        if transform_bypass {
            if profile != EncoderProfile::High {
                return Err(Error::Unsupported(
                    "H.264 transform bypass requires profile=high or auto".into(),
                ));
            }
            if !matches!(mode, EncoderMode::Intra4 | EncoderMode::Inter) {
                return Err(Error::Unsupported(
                    "H.264 transform bypass currently requires mode=intra4 or inter".into(),
                ));
            }
            if qp != 0 || aq_strength != 0 || settings.bitrate.is_some() {
                return Err(Error::Unsupported(
                    "H.264 transform bypass requires qp=0, aq_strength=0, and fixed-QP encoding"
                        .into(),
                ));
            }
        }
        if scaling_matrix == ScalingMatrixPreset::Jvt && profile != EncoderProfile::High {
            return Err(Error::Unsupported(
                "H.264 JVT scaling matrices require profile=high or auto".into(),
            ));
        }
        if scaling_matrix == ScalingMatrixPreset::Jvt && transform_bypass {
            return Err(Error::Unsupported(
                "H.264 scaling matrices do not apply when transform_bypass=true".into(),
            ));
        }
        let hrd = vbv_buffer_milliseconds
            .zip(settings.bitrate)
            .map(|(milliseconds, bitrate)| {
                EncoderHrd::new(bitrate, milliseconds, settings.time_base, b_frames)
            })
            .transpose()?;
        let rate_control = settings
            .bitrate
            .map(|bitrate| {
                if mode == EncoderMode::Ipcm {
                    return Err(Error::Unsupported(
                        "H.264 bitrate control requires mode=intra16, intra4, intra8, or inter"
                            .into(),
                    ));
                }
                RateControl::new(
                    bitrate,
                    settings.time_base,
                    qp,
                    hrd.map(|parameters| parameters.signalled_cpb_size),
                )
            })
            .transpose()?;
        let decoded_picture_buffer_size = max_references.max(if b_frames == 0 { 1 } else { 2 });
        let buffered_pictures = decoded_picture_buffer_size + usize::from(b_frames != 0);
        let level = negotiate_level(
            level_preference,
            LevelRequirements {
                width_in_macroblocks: macroblocks_wide,
                height_in_macroblocks: macroblocks_high,
                macroblocks_per_frame: macroblock_count,
                decoded_picture_buffer_macroblocks: macroblock_count
                    .checked_mul(buffered_pictures)
                    .ok_or_else(|| {
                        Error::Unsupported("H.264 decoded-picture-buffer size overflows".into())
                    })?,
                frame_duration_ticks,
                time_base: settings.time_base,
                bitrate: hrd.map(EncoderHrd::signalled_bit_rate).or(settings.bitrate),
                cpb_size: hrd.map(|parameters| parameters.signalled_cpb_size),
            },
        )?;
        let sps = encode_sps(&SequenceConfiguration {
            width: settings.width,
            height: settings.height,
            coded_width,
            coded_height,
            max_references: decoded_picture_buffer_size,
            pic_order_cnt_type0: b_frames != 0,
            hrd,
            profile,
            transform_bypass,
            scaling_matrix,
            bt709,
            level,
        })?;
        let transform_8x8_mode = mode == EncoderMode::Intra8
            || mode == EncoderMode::Inter && profile == EncoderProfile::High && !transform_bypass;
        let pps = encode_pps(transform_8x8_mode, entropy_coding)?;
        let descriptor = CodecDescriptor {
            codec_id: CodecId::new(CODEC_NAME),
            codec_tag: Some(FourCc(*b"avc1")),
            media_type: MediaType::Video,
            configuration: encode_avcc(&sps, &pps)?,
        };
        self.configuration = Some(Configuration {
            width: settings.width,
            height: settings.height,
            coded_width,
            coded_height,
            mode,
            qp,
            gop_size,
            search_range,
            analysis,
            scene_cut_threshold,
            max_references: decoded_picture_buffer_size,
            b_frames,
            b_direct_mode,
            aq_strength,
            pic_order_cnt_type0: b_frames != 0,
            transform_8x8_mode,
            transform_bypass,
            entropy_coding,
            scaling_matrix,
            level: LevelConfiguration {
                level,
                macroblocks_per_frame: macroblock_count,
                default_frame_duration_ticks: frame_duration_ticks,
                time_base: settings.time_base,
            },
        });
        self.rate_control = rate_control;
        self.hrd_state = hrd.map(HrdState::new);
        self.packets.clear();
        self.reconstructions.clear();
        self.references.clear();
        self.pending_b_frames.clear();
        self.decode_timestamps.clear();
        self.next_frame_num = 0;
        self.frames_since_idr = 0;
        self.flushed = false;
        Ok(descriptor)
    }

    fn send_frame(&mut self, frame: VideoFrame) -> Result<()> {
        let configuration = self.configuration.clone().ok_or_else(|| {
            Error::InvalidState("H.264 encoder must be configured before receiving frames".into())
        })?;
        if self.flushed {
            return Err(Error::InvalidState(
                "H.264 encoder cannot receive frames after flush".into(),
            ));
        }
        validate_frame(&frame, configuration.width, configuration.height)?;
        validate_frame_level(&frame, configuration.level)?;
        self.decode_timestamps.push_back(frame.timing.pts);
        let inter_mode = configuration.mode == EncoderMode::Inter;
        let key = !inter_mode
            || self.references.is_empty()
            || self.frames_since_idr >= configuration.gop_size
            || self.references.front().is_some_and(|reference| {
                scene_cut_detected(&frame, &configuration, &reference.planes)
            });

        if !inter_mode {
            let picture_configuration = self.picture_configuration(&configuration);
            let encoded = encode_idr(&frame, &picture_configuration, 0)?;
            self.queue_encoded(encoded, frame.timing, true, false, 0, 0)?;
            return Ok(());
        }
        if configuration.b_frames == 0 {
            return self.send_immediate_inter_frame(&frame, &configuration, key);
        }
        self.send_reordered_inter_frame(frame, &configuration, key)
    }

    fn receive_packet(&mut self) -> Result<Option<Packet>> {
        if self.configuration.is_none() {
            return Err(Error::InvalidState(
                "H.264 encoder must be configured before receiving packets".into(),
            ));
        }
        Ok(self.packets.pop_front())
    }

    fn flush(&mut self) -> Result<()> {
        let configuration = self.configuration.clone().ok_or_else(|| {
            Error::InvalidState("H.264 encoder must be configured before flushing".into())
        })?;
        while let Some(pending) = self.pending_b_frames.pop_front() {
            self.queue_p_frame(
                &pending.frame,
                &configuration,
                pending.pic_order_cnt_lsb,
                pending.picture_order_count,
            )?;
        }
        self.flushed = true;
        Ok(())
    }
}

fn validate_dimensions(width: usize, height: usize) -> Result<()> {
    if width == 0 || height == 0 {
        return Err(Error::InvalidData(
            "H.264 encoded dimensions must be non-zero".into(),
        ));
    }
    if !width.is_multiple_of(2) || !height.is_multiple_of(2) {
        return Err(Error::Unsupported(
            "H.264 Yuv420p8 dimensions must be even".into(),
        ));
    }
    Ok(())
}

fn validate_frame(frame: &VideoFrame, width: usize, height: usize) -> Result<()> {
    if frame.format != PixelFormat::Yuv420p8 || (frame.width, frame.height) != (width, height) {
        return Err(Error::InvalidData(
            "H.264 input frame does not match the configured sequence".into(),
        ));
    }
    if !matches!(
        frame.field_order,
        FieldOrder::Progressive | FieldOrder::Unspecified
    ) {
        return Err(Error::Unsupported(
            "H.264 encoder foundation supports progressive frames only".into(),
        ));
    }
    let expected = [
        (width, height),
        (width / 2, height / 2),
        (width / 2, height / 2),
    ];
    if frame.planes.len() != expected.len() {
        return Err(Error::InvalidData(
            "H.264 Yuv420p8 input must contain Y, Cb, and Cr planes".into(),
        ));
    }
    for (index, (plane, dimensions)) in frame.planes.iter().zip(expected).enumerate() {
        validate_plane(plane, dimensions.0, dimensions.1, index)?;
    }
    Ok(())
}

fn validate_frame_level(frame: &VideoFrame, configuration: LevelConfiguration) -> Result<()> {
    let (duration_ticks, time_base) = frame
        .timing
        .duration
        .filter(|duration| duration.value > 0)
        .and_then(|duration| {
            u64::try_from(duration.value)
                .ok()
                .map(|ticks| (ticks, duration.time_base))
        })
        .unwrap_or((
            configuration.default_frame_duration_ticks,
            configuration.time_base,
        ));
    if level_allows_frame_interval(
        configuration.level,
        configuration.macroblocks_per_frame,
        duration_ticks,
        time_base,
    ) {
        return Ok(());
    }
    Err(Error::Unsupported(format!(
        "H.264 frame duration exceeds Level {}'s {} macroblocks/second limit",
        configuration.level.name, configuration.level.max_macroblocks_per_second
    )))
}

fn validate_plane(plane: &Plane, width: usize, height: usize, index: usize) -> Result<()> {
    if (plane.width, plane.height) != (width, height) || plane.stride < width {
        return Err(Error::InvalidData(format!(
            "H.264 input plane {index} has invalid dimensions or stride"
        )));
    }
    let required = plane
        .stride
        .checked_mul(height.saturating_sub(1))
        .and_then(|offset| offset.checked_add(width))
        .ok_or_else(|| Error::InvalidData(format!("H.264 input plane {index} size overflows")))?;
    if plane.data.len() < required {
        return Err(Error::InvalidData(format!(
            "H.264 input plane {index} is truncated"
        )));
    }
    Ok(())
}

fn encode_sps(configuration: &SequenceConfiguration) -> Result<Vec<u8>> {
    let mut writer = BitWriter::new();
    writer.write_bits(u64::from(configuration.profile.profile_idc()), 8)?;
    let level_compatibility = if configuration.level.constraint_set3 {
        0x10
    } else {
        0
    };
    writer.write_bits(
        u64::from(configuration.profile.compatibility() | level_compatibility),
        8,
    )?;
    writer.write_bits(u64::from(configuration.level.level_idc), 8)?;
    write_ue(&mut writer, 0)?; // seq_parameter_set_id
    if configuration.profile == EncoderProfile::High {
        write_ue(&mut writer, 1)?; // chroma_format_idc: 4:2:0
        write_ue(&mut writer, 0)?; // bit_depth_luma_minus8
        write_ue(&mut writer, 0)?; // bit_depth_chroma_minus8
        writer.write_bit(configuration.transform_bypass)?; // qpprime_y_zero_transform_bypass_flag
        let scaling_matrix_present = configuration.scaling_matrix == ScalingMatrixPreset::Jvt;
        writer.write_bit(scaling_matrix_present)?; // seq_scaling_matrix_present_flag
        if scaling_matrix_present {
            for _ in 0..8 {
                writer.write_bit(false)?; // seq_scaling_list_present_flag
            }
        }
    }
    write_ue(&mut writer, 0)?; // log2_max_frame_num_minus4
    if configuration.pic_order_cnt_type0 {
        write_ue(&mut writer, 0)?; // pic_order_cnt_type
        write_ue(&mut writer, 0)?; // log2_max_pic_order_cnt_lsb_minus4
    } else {
        write_ue(&mut writer, 2)?; // pic_order_cnt_type
    }
    write_ue(&mut writer, configuration.max_references as u64)?; // max_num_ref_frames
    writer.write_bit(false)?; // gaps_in_frame_num_value_allowed_flag
    write_ue(&mut writer, (configuration.coded_width / 16 - 1) as u64)?;
    write_ue(&mut writer, (configuration.coded_height / 16 - 1) as u64)?;
    writer.write_bit(true)?; // frame_mbs_only_flag
    writer.write_bit(true)?; // direct_8x8_inference_flag
    let cropped = configuration.width != configuration.coded_width
        || configuration.height != configuration.coded_height;
    writer.write_bit(cropped)?;
    if cropped {
        write_ue(&mut writer, 0)?; // frame_crop_left_offset
        write_ue(
            &mut writer,
            ((configuration.coded_width - configuration.width) / 2) as u64,
        )?;
        write_ue(&mut writer, 0)?; // frame_crop_top_offset
        write_ue(
            &mut writer,
            ((configuration.coded_height - configuration.height) / 2) as u64,
        )?;
    }
    let vui_present = configuration.hrd.is_some() || configuration.bt709;
    writer.write_bit(vui_present)?; // vui_parameters_present_flag
    if vui_present {
        writer.write_bit(false)?; // aspect_ratio_info_present_flag
        writer.write_bit(false)?; // overscan_info_present_flag
        writer.write_bit(configuration.bt709)?; // video_signal_type_present_flag
        if configuration.bt709 {
            writer.write_bits(5, 3)?; // video_format: unspecified
            writer.write_bit(false)?; // video_full_range_flag: limited range
            writer.write_bit(true)?; // colour_description_present_flag
            writer.write_bits(1, 8)?; // colour_primaries: BT.709
            writer.write_bits(1, 8)?; // transfer_characteristics: BT.709
            writer.write_bits(1, 8)?; // matrix_coefficients: BT.709
        }
        writer.write_bit(false)?; // chroma_loc_info_present_flag
        writer.write_bit(configuration.hrd.is_some())?; // timing_info_present_flag
        if let Some(hrd) = configuration.hrd {
            writer.write_bits(u64::from(hrd.num_units_in_tick), 32)?;
            writer.write_bits(u64::from(hrd.time_scale), 32)?;
            writer.write_bit(false)?; // fixed_frame_rate_flag
        }
        writer.write_bit(configuration.hrd.is_some())?; // nal_hrd_parameters_present_flag
        if let Some(hrd) = configuration.hrd {
            write_ue(&mut writer, 0)?; // cpb_cnt_minus1
            writer.write_bits(u64::from(hrd.bit_rate_scale), 4)?;
            writer.write_bits(u64::from(hrd.cpb_size_scale), 4)?;
            write_ue(&mut writer, u64::from(hrd.bit_rate_value_minus1))?;
            write_ue(&mut writer, u64::from(hrd.cpb_size_value_minus1))?;
            writer.write_bit(false)?; // cbr_flag
            writer.write_bits(u64::from(HrdState::DELAY_BITS - 1), 5)?;
            writer.write_bits(u64::from(HrdState::DELAY_BITS - 1), 5)?;
            writer.write_bits(u64::from(HrdState::DELAY_BITS - 1), 5)?;
            writer.write_bits(0, 5)?; // time_offset_length
        }
        writer.write_bit(false)?; // vcl_hrd_parameters_present_flag
        if configuration.hrd.is_some() {
            writer.write_bit(false)?; // low_delay_hrd_flag
        }
        writer.write_bit(false)?; // pic_struct_present_flag
        writer.write_bit(false)?; // bitstream_restriction_flag
    }
    finish_rbsp(&mut writer)?;
    Ok(make_nal(0x67, writer.into_bytes()))
}

fn encode_pps(transform_8x8_mode: bool, entropy_coding: EntropyCoding) -> Result<Vec<u8>> {
    let mut writer = BitWriter::new();
    write_ue(&mut writer, 0)?; // pic_parameter_set_id
    write_ue(&mut writer, 0)?; // seq_parameter_set_id
    writer.write_bit(entropy_coding == EntropyCoding::Cabac)?; // entropy_coding_mode_flag
    writer.write_bit(false)?; // bottom_field_pic_order_in_frame_present_flag
    write_ue(&mut writer, 0)?; // num_slice_groups_minus1
    write_ue(&mut writer, 0)?; // num_ref_idx_l0_default_active_minus1
    write_ue(&mut writer, 0)?; // num_ref_idx_l1_default_active_minus1
    writer.write_bit(false)?; // weighted_pred_flag
    writer.write_bits(0, 2)?; // weighted_bipred_idc
    write_se(&mut writer, 0)?; // pic_init_qp_minus26
    write_se(&mut writer, 0)?; // pic_init_qs_minus26
    write_se(&mut writer, 0)?; // chroma_qp_index_offset
    writer.write_bit(true)?; // deblocking_filter_control_present_flag
    writer.write_bit(false)?; // constrained_intra_pred_flag
    writer.write_bit(false)?; // redundant_pic_cnt_present_flag
    if transform_8x8_mode {
        writer.write_bit(true)?; // transform_8x8_mode_flag
        writer.write_bit(false)?; // pic_scaling_matrix_present_flag
        write_se(&mut writer, 0)?; // second_chroma_qp_index_offset
    }
    finish_rbsp(&mut writer)?;
    Ok(make_nal(0x68, writer.into_bytes()))
}

#[allow(clippy::too_many_lines)]
fn encode_idr(
    frame: &VideoFrame,
    configuration: &Configuration,
    pic_order_cnt_lsb: u8,
) -> Result<EncodedFrame> {
    let mut writer = BitWriter::new();
    write_ue(&mut writer, 0)?; // first_mb_in_slice
    write_ue(&mut writer, 2)?; // slice_type: I
    write_ue(&mut writer, 0)?; // pic_parameter_set_id
    writer.write_bits(0, 4)?; // frame_num
    write_ue(&mut writer, 0)?; // idr_pic_id
    if configuration.pic_order_cnt_type0 {
        writer.write_bits(u64::from(pic_order_cnt_lsb), 4)?;
    }
    writer.write_bit(false)?; // no_output_of_prior_pics_flag
    writer.write_bit(false)?; // long_term_reference_flag
    write_se(&mut writer, i64::from(configuration.qp - 26))?; // slice_qp_delta
    write_ue(&mut writer, 1)?; // disable_deblocking_filter_idc

    let macroblocks_wide = configuration.coded_width / 16;
    let macroblocks_high = configuration.coded_height / 16;
    let padded = padded_planes(frame, configuration);
    if configuration.entropy_coding == EntropyCoding::Cabac {
        writer.align_to_byte_with(true); // cabac_alignment_one_bit
        let rbsp = writer.into_bytes();
        return match configuration.mode {
            EncoderMode::Ipcm => encode_cabac_ipcm_idr(
                frame,
                configuration,
                rbsp,
                padded,
                macroblocks_wide,
                macroblocks_high,
            ),
            EncoderMode::Intra16 => encode_cabac_intra16_idr(
                frame,
                configuration,
                rbsp,
                &padded,
                macroblocks_wide,
                macroblocks_high,
            ),
            EncoderMode::Intra4 | EncoderMode::Inter => encode_cabac_intra4_idr(
                frame,
                configuration,
                rbsp,
                &padded,
                macroblocks_wide,
                macroblocks_high,
            ),
            EncoderMode::Intra8 => encode_cabac_intra8_idr(
                frame,
                configuration,
                rbsp,
                &padded,
                macroblocks_wide,
                macroblocks_high,
            ),
        };
    }
    let macroblock_qps = adaptive_macroblock_qps(
        &padded[0],
        configuration.coded_width,
        configuration.coded_height,
        configuration.qp,
        configuration.aq_strength,
    );
    let mut previous_qp = configuration.qp;
    let mut reconstructed: [Vec<u8>; 3] = std::array::from_fn(|component| {
        let divisor = if component == 0 { 1 } else { 2 };
        vec![0; configuration.coded_width / divisor * configuration.coded_height / divisor]
    });
    let mut luma_nonzero = vec![[0_u8; 16]; macroblocks_wide * macroblocks_high];
    let mut chroma_nonzero = vec![[[0_u8; 4]; 2]; macroblocks_wide * macroblocks_high];
    let mut luma_modes = vec![[2_u8; 16]; macroblocks_wide * macroblocks_high];
    for macroblock_y in 0..macroblocks_high {
        for macroblock_x in 0..macroblocks_wide {
            let address = macroblock_y * macroblocks_wide + macroblock_x;
            let macroblock_qp = macroblock_qps[address];
            let qp_delta = macroblock_qp_delta(previous_qp, macroblock_qp);
            if configuration.mode == EncoderMode::Intra8 {
                let qp_changed = encode_intra8_macroblock(
                    &mut writer,
                    &padded,
                    &mut reconstructed,
                    &mut luma_modes,
                    &mut luma_nonzero,
                    &mut chroma_nonzero,
                    address,
                    macroblocks_wide,
                    macroblock_x,
                    macroblock_y,
                    configuration.coded_width,
                    macroblock_qp,
                    qp_delta,
                    configuration.scaling_matrix,
                )?;
                if qp_changed {
                    previous_qp = macroblock_qp;
                }
                continue;
            }
            if matches!(configuration.mode, EncoderMode::Intra4 | EncoderMode::Inter) {
                let qp_changed = encode_intra4_macroblock(
                    &mut writer,
                    &padded,
                    &mut reconstructed,
                    &mut luma_modes,
                    &mut luma_nonzero,
                    &mut chroma_nonzero,
                    address,
                    macroblocks_wide,
                    macroblock_x,
                    macroblock_y,
                    configuration.coded_width,
                    macroblock_qp,
                    qp_delta,
                    configuration.transform_8x8_mode,
                    configuration.transform_bypass,
                    configuration.scaling_matrix,
                )?;
                if qp_changed {
                    previous_qp = macroblock_qp;
                }
                continue;
            }
            if configuration.mode == EncoderMode::Intra16 {
                let (luma_mode, luma_prediction) = select_luma_prediction(
                    &padded[0],
                    &reconstructed[0],
                    configuration.coded_width,
                    macroblock_x,
                    macroblock_y,
                );
                let (chroma_mode, chroma_predictions) = select_chroma_prediction(
                    [&padded[1], &padded[2]],
                    [&reconstructed[1], &reconstructed[2]],
                    configuration.coded_width / 2,
                    macroblock_x,
                    macroblock_y,
                );
                let dc_levels = quantize_intra16_luma_dc(
                    &padded[0],
                    configuration.coded_width,
                    macroblock_x,
                    macroblock_y,
                    &luma_prediction,
                    macroblock_qp,
                    configuration.scaling_matrix.four_by_four(true),
                );
                let ac_levels = quantize_intra16_luma_ac(
                    &padded[0],
                    configuration.coded_width,
                    macroblock_x,
                    macroblock_y,
                    &luma_prediction,
                    macroblock_qp,
                    configuration.scaling_matrix.four_by_four(true),
                );
                let chroma_qp = chroma_qp(macroblock_qp);
                let chroma_residuals: [ChromaResidual; 2] = std::array::from_fn(|component| {
                    quantize_chroma_residual(
                        &padded[component + 1],
                        configuration.coded_width / 2,
                        macroblock_x,
                        macroblock_y,
                        &chroma_predictions[component],
                        chroma_qp,
                        false,
                        Some(chroma_mode),
                        configuration.scaling_matrix.four_by_four(true),
                    )
                });
                let has_luma_ac = ac_levels.iter().flatten().any(|&level| level != 0);
                let has_chroma_ac = chroma_residuals
                    .iter()
                    .flat_map(|residual| residual.ac.iter().flatten())
                    .any(|&level| level != 0);
                let chroma_cbp = if has_chroma_ac {
                    2
                } else {
                    u8::from(
                        chroma_residuals
                            .iter()
                            .flat_map(|residual| residual.dc)
                            .any(|level| level != 0),
                    )
                };
                let macroblock_type = 1 + luma_mode + chroma_cbp * 4 + u8::from(has_luma_ac) * 12;
                write_ue(&mut writer, u64::from(macroblock_type))?;
                write_ue(&mut writer, u64::from(chroma_mode))?;
                write_se(&mut writer, i64::from(qp_delta))?;
                previous_qp = macroblock_qp;
                let dc_n_c = intra16_dc_nc(&luma_nonzero, address, macroblocks_wide);
                encode_residual_block(&mut writer, dc_n_c, &dc_levels)?;
                if has_luma_ac {
                    for (block_index, levels) in ac_levels.iter().enumerate() {
                        let n_c = luma_nc(&luma_nonzero, address, block_index, macroblocks_wide);
                        luma_nonzero[address][block_index] =
                            encode_residual_block(&mut writer, n_c, levels)?;
                    }
                }
                if chroma_cbp != 0 {
                    for residual in &chroma_residuals {
                        encode_residual_block(&mut writer, -1, &residual.dc)?;
                    }
                }
                if chroma_cbp == 2 {
                    for (component, residual) in chroma_residuals.iter().enumerate() {
                        for (block_index, levels) in residual.ac.iter().enumerate() {
                            let n_c = chroma_nc(
                                &chroma_nonzero,
                                address,
                                component,
                                block_index,
                                macroblocks_wide,
                            );
                            chroma_nonzero[address][component][block_index] =
                                encode_residual_block(&mut writer, n_c, levels)?;
                        }
                    }
                }
                reconstruct_intra16_luma(
                    &mut reconstructed[0],
                    configuration.coded_width,
                    macroblock_x,
                    macroblock_y,
                    &luma_prediction,
                    &dc_levels,
                    &ac_levels,
                    macroblock_qp,
                    configuration.scaling_matrix.four_by_four(true),
                );
                for (component, prediction) in chroma_predictions.iter().enumerate() {
                    reconstruct_chroma(
                        &mut reconstructed[component + 1],
                        configuration.coded_width / 2,
                        macroblock_x,
                        macroblock_y,
                        prediction,
                        &chroma_residuals[component],
                        chroma_qp,
                        false,
                        Some(chroma_mode),
                        configuration.scaling_matrix.four_by_four(true),
                    );
                }
                continue;
            }
            write_ue(&mut writer, 25)?; // I_PCM
            writer.align_to_byte();
            write_pcm_plane_block(
                &mut writer,
                &padded[0],
                configuration.coded_width,
                macroblock_x * 16,
                macroblock_y * 16,
                16,
                16,
            )?;
            for plane in &padded[1..=2] {
                write_pcm_plane_block(
                    &mut writer,
                    plane,
                    configuration.coded_width / 2,
                    macroblock_x * 8,
                    macroblock_y * 8,
                    8,
                    8,
                )?;
            }
        }
    }
    finish_rbsp(&mut writer)?;
    let coded_reconstruction = if configuration.mode == EncoderMode::Ipcm {
        padded
    } else {
        reconstructed
    };
    let reconstructed = if configuration.mode == EncoderMode::Ipcm {
        frame.clone()
    } else {
        visible_frame(frame, configuration, &coded_reconstruction)
    };
    Ok(EncodedFrame {
        nal: make_nal(0x65, writer.into_bytes()),
        reconstructed,
        coded_reconstruction,
        motion_l0: vec![[None; 16]; macroblocks_wide * macroblocks_high],
        reference_l0_poc: vec![[None; 16]; macroblocks_wide * macroblocks_high],
        macroblock_intra: vec![true; macroblocks_wide * macroblocks_high],
    })
}

fn encode_cabac_ipcm_idr(
    frame: &VideoFrame,
    configuration: &Configuration,
    mut rbsp: Vec<u8>,
    padded: [Vec<u8>; 3],
    macroblocks_wide: usize,
    macroblocks_high: usize,
) -> Result<EncodedFrame> {
    debug_assert_eq!(configuration.mode, EncoderMode::Ipcm);
    let macroblock_count = macroblocks_wide * macroblocks_high;
    let mut contexts = initial_i_macroblock_contexts(configuration.qp)?;
    let mut arithmetic = CabacEncoder::new();
    for macroblock_y in 0..macroblocks_high {
        for macroblock_x in 0..macroblocks_wide {
            let address = macroblock_y * macroblocks_wide + macroblock_x;
            let context_increment = usize::from(macroblock_x != 0) + usize::from(macroblock_y != 0);
            arithmetic.decision(&mut contexts[3 + context_increment], true)?;
            arithmetic.terminate(true)?; // pcm_flag encoded through the termination process
            rbsp.extend_from_slice(&arithmetic.finish()?);

            let mut samples = BitWriter::new();
            write_pcm_plane_block(
                &mut samples,
                &padded[0],
                configuration.coded_width,
                macroblock_x * 16,
                macroblock_y * 16,
                16,
                16,
            )?;
            for plane in &padded[1..=2] {
                write_pcm_plane_block(
                    &mut samples,
                    plane,
                    configuration.coded_width / 2,
                    macroblock_x * 8,
                    macroblock_y * 8,
                    8,
                    8,
                )?;
            }
            rbsp.extend_from_slice(&samples.into_bytes());

            arithmetic = CabacEncoder::new();
            arithmetic.terminate(address + 1 == macroblock_count)?;
        }
    }
    rbsp.extend_from_slice(&arithmetic.finish()?);
    Ok(EncodedFrame {
        nal: make_nal(0x65, rbsp),
        reconstructed: frame.clone(),
        coded_reconstruction: padded,
        motion_l0: vec![[None; 16]; macroblock_count],
        reference_l0_poc: vec![[None; 16]; macroblock_count],
        macroblock_intra: vec![true; macroblock_count],
    })
}

#[derive(Debug)]
struct CabacIntraCodedBlocks {
    patterns: Vec<u8>,
    luma_dc: Vec<bool>,
    chroma_dc: Vec<[bool; 2]>,
    luma_ac: Vec<[bool; 16]>,
    chroma_ac: Vec<[[bool; 4]; 2]>,
}

impl CabacIntraCodedBlocks {
    fn new(macroblock_count: usize) -> Self {
        Self {
            patterns: vec![0; macroblock_count],
            luma_dc: vec![false; macroblock_count],
            chroma_dc: vec![[false; 2]; macroblock_count],
            luma_ac: vec![[false; 16]; macroblock_count],
            chroma_ac: vec![[[false; 4]; 2]; macroblock_count],
        }
    }
}

#[allow(clippy::too_many_lines)]
fn encode_cabac_intra16_idr(
    frame: &VideoFrame,
    configuration: &Configuration,
    mut rbsp: Vec<u8>,
    padded: &[Vec<u8>; 3],
    macroblocks_wide: usize,
    macroblocks_high: usize,
) -> Result<EncodedFrame> {
    debug_assert_eq!(configuration.mode, EncoderMode::Intra16);
    let macroblock_count = macroblocks_wide * macroblocks_high;
    let macroblock_qps = adaptive_macroblock_qps(
        &padded[0],
        configuration.coded_width,
        configuration.coded_height,
        configuration.qp,
        configuration.aq_strength,
    );
    let mut contexts = CabacIContexts::new(configuration.qp)?;
    let mut arithmetic = CabacEncoder::new();
    let mut coded_blocks = CabacIntraCodedBlocks::new(macroblock_count);
    let mut chroma_prediction_modes = vec![0_u8; macroblock_count];
    let mut previous_qp = configuration.qp;
    let mut previous_qp_delta = 0;
    let mut reconstructed: [Vec<u8>; 3] = std::array::from_fn(|component| {
        let divisor = if component == 0 { 1 } else { 2 };
        vec![0; configuration.coded_width / divisor * configuration.coded_height / divisor]
    });

    for macroblock_y in 0..macroblocks_high {
        for macroblock_x in 0..macroblocks_wide {
            let address = macroblock_y * macroblocks_wide + macroblock_x;
            let macroblock_qp = macroblock_qps[address];
            let qp_delta = macroblock_qp_delta(previous_qp, macroblock_qp);
            let (luma_mode, luma_prediction) = select_luma_prediction(
                &padded[0],
                &reconstructed[0],
                configuration.coded_width,
                macroblock_x,
                macroblock_y,
            );
            let (chroma_mode, chroma_predictions) = select_chroma_prediction(
                [&padded[1], &padded[2]],
                [&reconstructed[1], &reconstructed[2]],
                configuration.coded_width / 2,
                macroblock_x,
                macroblock_y,
            );
            let dc_levels = quantize_intra16_luma_dc(
                &padded[0],
                configuration.coded_width,
                macroblock_x,
                macroblock_y,
                &luma_prediction,
                macroblock_qp,
                configuration.scaling_matrix.four_by_four(true),
            );
            let ac_levels = quantize_intra16_luma_ac(
                &padded[0],
                configuration.coded_width,
                macroblock_x,
                macroblock_y,
                &luma_prediction,
                macroblock_qp,
                configuration.scaling_matrix.four_by_four(true),
            );
            let chroma_qp = chroma_qp(macroblock_qp);
            let chroma_residuals: [ChromaResidual; 2] = std::array::from_fn(|component| {
                quantize_chroma_residual(
                    &padded[component + 1],
                    configuration.coded_width / 2,
                    macroblock_x,
                    macroblock_y,
                    &chroma_predictions[component],
                    chroma_qp,
                    false,
                    Some(chroma_mode),
                    configuration.scaling_matrix.four_by_four(true),
                )
            });
            let has_luma_ac = ac_levels.iter().flatten().any(|&level| level != 0);
            let has_chroma_ac = chroma_residuals
                .iter()
                .flat_map(|residual| residual.ac.iter().flatten())
                .any(|&level| level != 0);
            let chroma_cbp = if has_chroma_ac {
                2
            } else {
                u8::from(
                    chroma_residuals
                        .iter()
                        .flat_map(|residual| residual.dc)
                        .any(|level| level != 0),
                )
            };
            let macroblock_type = 1 + luma_mode + chroma_cbp * 4 + u8::from(has_luma_ac) * 12;

            encode_cabac_i_macroblock_type(
                &mut arithmetic,
                &mut contexts,
                macroblock_type,
                usize::from(macroblock_x != 0) + usize::from(macroblock_y != 0),
            )?;
            encode_cabac_chroma_prediction_mode(
                &mut arithmetic,
                &mut contexts,
                chroma_mode,
                address,
                macroblocks_wide,
                &chroma_prediction_modes,
            )?;
            chroma_prediction_modes[address] = chroma_mode;
            encode_cabac_macroblock_qp_delta(
                &mut arithmetic,
                &mut contexts,
                qp_delta,
                previous_qp_delta,
            )?;
            previous_qp_delta = qp_delta;
            previous_qp = macroblock_qp;

            let luma_dc_context =
                cabac_intra_dc_coded_context(address, macroblocks_wide, |neighbor| {
                    coded_blocks.luma_dc[neighbor]
                });
            let luma_dc_coded = dc_levels.iter().any(|&level| level != 0);
            arithmetic.decision(
                &mut contexts.luma_dc_coded_block[luma_dc_context],
                luma_dc_coded,
            )?;
            coded_blocks.luma_dc[address] = luma_dc_coded;
            if luma_dc_coded {
                encode_cabac_residual_block(
                    &mut arithmetic,
                    &mut contexts.luma_dc_significant,
                    &mut contexts.luma_dc_last,
                    &mut contexts.luma_dc_abs_level,
                    &dc_levels,
                )?;
            }

            if has_luma_ac {
                for (block_index, levels) in ac_levels.iter().enumerate() {
                    let coded_context = cabac_intra_luma_ac_coded_context(
                        address,
                        macroblocks_wide,
                        block_index,
                        &coded_blocks.luma_ac,
                    );
                    let coded = levels.iter().any(|&level| level != 0);
                    arithmetic.decision(&mut contexts.luma_ac_coded_block[coded_context], coded)?;
                    coded_blocks.luma_ac[address][block_index] = coded;
                    if coded {
                        encode_cabac_residual_block(
                            &mut arithmetic,
                            &mut contexts.luma_ac_significant,
                            &mut contexts.luma_ac_last,
                            &mut contexts.luma_ac_abs_level,
                            levels,
                        )?;
                    }
                }
            }

            if chroma_cbp != 0 {
                for (component, residual) in chroma_residuals.iter().enumerate() {
                    let coded_context =
                        cabac_intra_dc_coded_context(address, macroblocks_wide, |neighbor| {
                            coded_blocks.chroma_dc[neighbor][component]
                        });
                    let coded = residual.dc.iter().any(|&level| level != 0);
                    arithmetic
                        .decision(&mut contexts.chroma_dc_coded_block[coded_context], coded)?;
                    coded_blocks.chroma_dc[address][component] = coded;
                    if coded {
                        encode_cabac_residual_block(
                            &mut arithmetic,
                            &mut contexts.chroma_dc_significant,
                            &mut contexts.chroma_dc_last,
                            &mut contexts.chroma_dc_abs_level,
                            &residual.dc,
                        )?;
                    }
                }
            }
            if chroma_cbp == 2 {
                for (component, residual) in chroma_residuals.iter().enumerate() {
                    for (block_index, levels) in residual.ac.iter().enumerate() {
                        let coded_context = cabac_intra_chroma_ac_coded_context(
                            address,
                            macroblocks_wide,
                            component,
                            block_index,
                            &coded_blocks.chroma_ac,
                        );
                        let coded = levels.iter().any(|&level| level != 0);
                        arithmetic
                            .decision(&mut contexts.chroma_ac_coded_block[coded_context], coded)?;
                        coded_blocks.chroma_ac[address][component][block_index] = coded;
                        if coded {
                            encode_cabac_residual_block(
                                &mut arithmetic,
                                &mut contexts.chroma_ac_significant,
                                &mut contexts.chroma_ac_last,
                                &mut contexts.chroma_ac_abs_level,
                                levels,
                            )?;
                        }
                    }
                }
            }

            reconstruct_intra16_luma(
                &mut reconstructed[0],
                configuration.coded_width,
                macroblock_x,
                macroblock_y,
                &luma_prediction,
                &dc_levels,
                &ac_levels,
                macroblock_qp,
                configuration.scaling_matrix.four_by_four(true),
            );
            for (component, prediction) in chroma_predictions.iter().enumerate() {
                reconstruct_chroma(
                    &mut reconstructed[component + 1],
                    configuration.coded_width / 2,
                    macroblock_x,
                    macroblock_y,
                    prediction,
                    &chroma_residuals[component],
                    chroma_qp,
                    false,
                    Some(chroma_mode),
                    configuration.scaling_matrix.four_by_four(true),
                );
            }
            arithmetic.terminate(address + 1 == macroblock_count)?;
        }
    }
    rbsp.extend_from_slice(&arithmetic.finish()?);
    let visible = visible_frame(frame, configuration, &reconstructed);
    Ok(EncodedFrame {
        nal: make_nal(0x65, rbsp),
        reconstructed: visible,
        coded_reconstruction: reconstructed,
        motion_l0: vec![[None; 16]; macroblock_count],
        reference_l0_poc: vec![[None; 16]; macroblock_count],
        macroblock_intra: vec![true; macroblock_count],
    })
}

#[allow(clippy::too_many_lines)]
fn encode_cabac_intra4_idr(
    frame: &VideoFrame,
    configuration: &Configuration,
    mut rbsp: Vec<u8>,
    source: &[Vec<u8>; 3],
    macroblocks_wide: usize,
    macroblocks_high: usize,
) -> Result<EncodedFrame> {
    debug_assert!(matches!(
        configuration.mode,
        EncoderMode::Intra4 | EncoderMode::Inter
    ));
    let macroblock_count = macroblocks_wide * macroblocks_high;
    let macroblock_qps = adaptive_macroblock_qps(
        &source[0],
        configuration.coded_width,
        configuration.coded_height,
        configuration.qp,
        configuration.aq_strength,
    );
    let mut contexts = CabacIContexts::new(configuration.qp)?;
    let mut arithmetic = CabacEncoder::new();
    let mut coded_blocks = CabacIntraCodedBlocks::new(macroblock_count);
    let mut luma_modes = vec![[2_u8; 16]; macroblock_count];
    let mut chroma_prediction_modes = vec![0_u8; macroblock_count];
    let transform_8x8 = vec![false; macroblock_count];
    let mut previous_qp = configuration.qp;
    let mut previous_qp_delta = 0;
    let mut reconstructed: [Vec<u8>; 3] = std::array::from_fn(|component| {
        let divisor = if component == 0 { 1 } else { 2 };
        vec![0; configuration.coded_width / divisor * configuration.coded_height / divisor]
    });

    for macroblock_y in 0..macroblocks_high {
        for macroblock_x in 0..macroblocks_wide {
            let address = macroblock_y * macroblocks_wide + macroblock_x;
            let macroblock_qp = macroblock_qps[address];
            let qp_delta = macroblock_qp_delta(previous_qp, macroblock_qp);
            let mut levels = [[0_i32; 16]; 16];
            for block_index in 0..16 {
                let (block_x, block_y) = luma_block_position(block_index);
                let origin_x = macroblock_x * 16 + block_x * 4;
                let origin_y = macroblock_y * 16 + block_y * 4;
                let (mode, prediction) = (0..=8)
                    .filter_map(|mode| {
                        intra4_prediction(
                            &reconstructed[0],
                            configuration.coded_width,
                            origin_x,
                            origin_y,
                            block_index,
                            mode,
                            macroblock_y > 0,
                            macroblock_x > 0,
                        )
                        .map(|prediction| {
                            let sad = block_sad(
                                &source[0],
                                configuration.coded_width,
                                origin_x,
                                origin_y,
                                4,
                                &prediction,
                            );
                            (mode, prediction, sad)
                        })
                    })
                    .min_by_key(|(_, _, sad)| *sad)
                    .map(|(mode, prediction, _)| (mode, prediction))
                    .expect("Intra4 DC prediction is always available");
                luma_modes[address][block_index] = mode;
                levels[block_index] = quantize_intra4_luma_block(
                    &source[0],
                    configuration.coded_width,
                    origin_x,
                    origin_y,
                    &prediction,
                    macroblock_qp,
                    configuration.transform_bypass,
                    mode,
                    configuration.scaling_matrix.four_by_four(true),
                );
                if configuration.transform_bypass {
                    let samples: [u8; 16] = std::array::from_fn(|index| {
                        source[0][(origin_y + index / 4) * configuration.coded_width
                            + origin_x
                            + index % 4]
                    });
                    place_block(
                        &mut reconstructed[0],
                        configuration.coded_width,
                        origin_x,
                        origin_y,
                        4,
                        &samples,
                    );
                } else {
                    reconstruct_intra4_luma_block(
                        &mut reconstructed[0],
                        configuration.coded_width,
                        origin_x,
                        origin_y,
                        &prediction,
                        &levels[block_index],
                        macroblock_qp,
                        configuration.scaling_matrix.four_by_four(true),
                    );
                }
            }

            let (chroma_mode, chroma_predictions) = select_chroma_prediction(
                [&source[1], &source[2]],
                [&reconstructed[1], &reconstructed[2]],
                configuration.coded_width / 2,
                macroblock_x,
                macroblock_y,
            );
            let chroma_qp = chroma_qp(macroblock_qp);
            let chroma_residuals: [ChromaResidual; 2] = std::array::from_fn(|component| {
                quantize_chroma_residual(
                    &source[component + 1],
                    configuration.coded_width / 2,
                    macroblock_x,
                    macroblock_y,
                    &chroma_predictions[component],
                    chroma_qp,
                    configuration.transform_bypass,
                    Some(chroma_mode),
                    configuration.scaling_matrix.four_by_four(true),
                )
            });
            let luma_pattern = luma_coded_block_pattern(&levels);
            let chroma_pattern = chroma_coded_block_pattern(&chroma_residuals);
            let coded_block_pattern = luma_pattern | chroma_pattern << 4;

            encode_cabac_i_macroblock_type(&mut arithmetic, &mut contexts, 0, 0)?;
            if configuration.transform_8x8_mode {
                let left = !address.is_multiple_of(macroblocks_wide) && transform_8x8[address - 1];
                let top = address >= macroblocks_wide && transform_8x8[address - macroblocks_wide];
                arithmetic.decision(
                    &mut contexts.transform_size_8x8[usize::from(left) + usize::from(top)],
                    false,
                )?;
            }
            for block_index in 0..16 {
                let predicted =
                    predicted_intra4_mode(&luma_modes, address, block_index, macroblocks_wide);
                let mode = luma_modes[address][block_index];
                arithmetic.decision(&mut contexts.intra4_prediction_mode[0], mode == predicted)?;
                if mode != predicted {
                    let remaining = mode - u8::from(mode > predicted);
                    for bit in 0..3 {
                        arithmetic.decision(
                            &mut contexts.intra4_prediction_mode[1],
                            remaining & (1 << bit) != 0,
                        )?;
                    }
                }
            }
            encode_cabac_chroma_prediction_mode(
                &mut arithmetic,
                &mut contexts,
                chroma_mode,
                address,
                macroblocks_wide,
                &chroma_prediction_modes,
            )?;
            chroma_prediction_modes[address] = chroma_mode;
            encode_cabac_coded_block_pattern(
                &mut arithmetic,
                &mut contexts,
                coded_block_pattern,
                address,
                macroblocks_wide,
                &coded_blocks.patterns,
            )?;
            coded_blocks.patterns[address] = coded_block_pattern;
            if coded_block_pattern == 0 {
                previous_qp_delta = 0;
            } else {
                encode_cabac_macroblock_qp_delta(
                    &mut arithmetic,
                    &mut contexts,
                    qp_delta,
                    previous_qp_delta,
                )?;
                previous_qp_delta = qp_delta;
                previous_qp = macroblock_qp;
            }

            for group in 0..4 {
                if luma_pattern & (1 << group) == 0 {
                    continue;
                }
                for (block_index, block_levels) in levels.iter().enumerate().skip(group * 4).take(4)
                {
                    let coded_context = cabac_intra_luma_ac_coded_context(
                        address,
                        macroblocks_wide,
                        block_index,
                        &coded_blocks.luma_ac,
                    );
                    let coded = block_levels.iter().any(|&level| level != 0);
                    arithmetic
                        .decision(&mut contexts.luma_4x4_coded_block[coded_context], coded)?;
                    coded_blocks.luma_ac[address][block_index] = coded;
                    if coded {
                        encode_cabac_residual_block(
                            &mut arithmetic,
                            &mut contexts.luma_4x4_significant,
                            &mut contexts.luma_4x4_last,
                            &mut contexts.luma_4x4_abs_level,
                            block_levels,
                        )?;
                    }
                }
            }
            encode_cabac_chroma_residuals(
                &mut arithmetic,
                &mut contexts,
                &chroma_residuals,
                chroma_pattern,
                address,
                macroblocks_wide,
                &mut coded_blocks,
            )?;

            for (component, prediction) in chroma_predictions.iter().enumerate() {
                reconstruct_chroma(
                    &mut reconstructed[component + 1],
                    configuration.coded_width / 2,
                    macroblock_x,
                    macroblock_y,
                    prediction,
                    &chroma_residuals[component],
                    chroma_qp,
                    configuration.transform_bypass,
                    Some(chroma_mode),
                    configuration.scaling_matrix.four_by_four(true),
                );
            }
            arithmetic.terminate(address + 1 == macroblock_count)?;
        }
    }
    rbsp.extend_from_slice(&arithmetic.finish()?);
    let visible = visible_frame(frame, configuration, &reconstructed);
    Ok(EncodedFrame {
        nal: make_nal(0x65, rbsp),
        reconstructed: visible,
        coded_reconstruction: reconstructed,
        motion_l0: vec![[None; 16]; macroblock_count],
        reference_l0_poc: vec![[None; 16]; macroblock_count],
        macroblock_intra: vec![true; macroblock_count],
    })
}

#[allow(clippy::too_many_lines)]
fn encode_cabac_intra8_idr(
    frame: &VideoFrame,
    configuration: &Configuration,
    mut rbsp: Vec<u8>,
    source: &[Vec<u8>; 3],
    macroblocks_wide: usize,
    macroblocks_high: usize,
) -> Result<EncodedFrame> {
    debug_assert_eq!(configuration.mode, EncoderMode::Intra8);
    let macroblock_count = macroblocks_wide * macroblocks_high;
    let macroblock_qps = adaptive_macroblock_qps(
        &source[0],
        configuration.coded_width,
        configuration.coded_height,
        configuration.qp,
        configuration.aq_strength,
    );
    let mut contexts = CabacIContexts::new(configuration.qp)?;
    let mut arithmetic = CabacEncoder::new();
    let mut coded_blocks = CabacIntraCodedBlocks::new(macroblock_count);
    let mut luma_modes = vec![[2_u8; 16]; macroblock_count];
    let mut chroma_prediction_modes = vec![0_u8; macroblock_count];
    let mut transform_8x8 = vec![false; macroblock_count];
    let mut previous_qp = configuration.qp;
    let mut previous_qp_delta = 0;
    let mut reconstructed: [Vec<u8>; 3] = std::array::from_fn(|component| {
        let divisor = if component == 0 { 1 } else { 2 };
        vec![0; configuration.coded_width / divisor * configuration.coded_height / divisor]
    });

    for macroblock_y in 0..macroblocks_high {
        for macroblock_x in 0..macroblocks_wide {
            let address = macroblock_y * macroblocks_wide + macroblock_x;
            let macroblock_qp = macroblock_qps[address];
            let qp_delta = macroblock_qp_delta(previous_qp, macroblock_qp);
            let mut levels = [[0_i32; 64]; 4];
            for (group, group_levels) in levels.iter_mut().enumerate() {
                let origin_x = macroblock_x * 16 + (group % 2) * 8;
                let origin_y = macroblock_y * 16 + (group / 2) * 8;
                let (mode, prediction) = (0..=8)
                    .filter_map(|mode| {
                        intra8_prediction(
                            &reconstructed[0],
                            configuration.coded_width,
                            origin_x,
                            origin_y,
                            group,
                            mode,
                            macroblock_y > 0,
                            macroblock_x > 0,
                        )
                        .map(|prediction| {
                            let sad = block_sad(
                                &source[0],
                                configuration.coded_width,
                                origin_x,
                                origin_y,
                                8,
                                &prediction,
                            );
                            (mode, prediction, sad)
                        })
                    })
                    .min_by_key(|(_, _, sad)| *sad)
                    .map(|(mode, prediction, _)| (mode, prediction))
                    .expect("Intra8 DC prediction is always available");
                luma_modes[address][group * 4..group * 4 + 4].fill(mode);
                *group_levels = quantize_intra8_luma_block(
                    &source[0],
                    configuration.coded_width,
                    origin_x,
                    origin_y,
                    &prediction,
                    macroblock_qp,
                    configuration.scaling_matrix.eight_by_eight(true),
                );
                reconstruct_intra8_luma_block(
                    &mut reconstructed[0],
                    configuration.coded_width,
                    origin_x,
                    origin_y,
                    &prediction,
                    group_levels,
                    macroblock_qp,
                    configuration.scaling_matrix.eight_by_eight(true),
                );
            }

            let (chroma_mode, chroma_predictions) = select_chroma_prediction(
                [&source[1], &source[2]],
                [&reconstructed[1], &reconstructed[2]],
                configuration.coded_width / 2,
                macroblock_x,
                macroblock_y,
            );
            let chroma_qp = chroma_qp(macroblock_qp);
            let chroma_residuals: [ChromaResidual; 2] = std::array::from_fn(|component| {
                quantize_chroma_residual(
                    &source[component + 1],
                    configuration.coded_width / 2,
                    macroblock_x,
                    macroblock_y,
                    &chroma_predictions[component],
                    chroma_qp,
                    false,
                    Some(chroma_mode),
                    configuration.scaling_matrix.four_by_four(true),
                )
            });
            let luma_pattern = (0..4).fold(0_u8, |pattern, group| {
                pattern | (u8::from(levels[group].iter().any(|&level| level != 0)) << group)
            });
            let chroma_pattern = chroma_coded_block_pattern(&chroma_residuals);
            let coded_block_pattern = luma_pattern | chroma_pattern << 4;

            encode_cabac_i_macroblock_type(&mut arithmetic, &mut contexts, 0, 0)?;
            let transform_context = usize::from(macroblock_x != 0 && transform_8x8[address - 1])
                + usize::from(macroblock_y != 0 && transform_8x8[address - macroblocks_wide]);
            arithmetic.decision(&mut contexts.transform_size_8x8[transform_context], true)?;
            transform_8x8[address] = true;
            for group in 0..4 {
                let block_index = group * 4;
                let predicted =
                    predicted_intra4_mode(&luma_modes, address, block_index, macroblocks_wide);
                let mode = luma_modes[address][block_index];
                arithmetic.decision(&mut contexts.intra4_prediction_mode[0], mode == predicted)?;
                if mode != predicted {
                    let remaining = mode - u8::from(mode > predicted);
                    for bit in 0..3 {
                        arithmetic.decision(
                            &mut contexts.intra4_prediction_mode[1],
                            remaining & (1 << bit) != 0,
                        )?;
                    }
                }
            }
            encode_cabac_chroma_prediction_mode(
                &mut arithmetic,
                &mut contexts,
                chroma_mode,
                address,
                macroblocks_wide,
                &chroma_prediction_modes,
            )?;
            chroma_prediction_modes[address] = chroma_mode;
            encode_cabac_coded_block_pattern(
                &mut arithmetic,
                &mut contexts,
                coded_block_pattern,
                address,
                macroblocks_wide,
                &coded_blocks.patterns,
            )?;
            coded_blocks.patterns[address] = coded_block_pattern;
            if coded_block_pattern == 0 {
                previous_qp_delta = 0;
            } else {
                encode_cabac_macroblock_qp_delta(
                    &mut arithmetic,
                    &mut contexts,
                    qp_delta,
                    previous_qp_delta,
                )?;
                previous_qp_delta = qp_delta;
                previous_qp = macroblock_qp;
            }

            for (group, group_levels) in levels.iter().enumerate() {
                if luma_pattern & (1 << group) == 0 {
                    continue;
                }
                encode_cabac_residual_8x8(
                    &mut arithmetic,
                    &mut contexts.luma_8x8_significant,
                    &mut contexts.luma_8x8_last,
                    &mut contexts.luma_8x8_abs_level,
                    group_levels,
                )?;
                coded_blocks.luma_ac[address][group * 4..group * 4 + 4].fill(true);
            }
            encode_cabac_chroma_residuals(
                &mut arithmetic,
                &mut contexts,
                &chroma_residuals,
                chroma_pattern,
                address,
                macroblocks_wide,
                &mut coded_blocks,
            )?;

            for (component, prediction) in chroma_predictions.iter().enumerate() {
                reconstruct_chroma(
                    &mut reconstructed[component + 1],
                    configuration.coded_width / 2,
                    macroblock_x,
                    macroblock_y,
                    prediction,
                    &chroma_residuals[component],
                    chroma_qp,
                    false,
                    Some(chroma_mode),
                    configuration.scaling_matrix.four_by_four(true),
                );
            }
            arithmetic.terminate(address + 1 == macroblock_count)?;
        }
    }
    rbsp.extend_from_slice(&arithmetic.finish()?);
    let visible = visible_frame(frame, configuration, &reconstructed);
    Ok(EncodedFrame {
        nal: make_nal(0x65, rbsp),
        reconstructed: visible,
        coded_reconstruction: reconstructed,
        motion_l0: vec![[None; 16]; macroblock_count],
        reference_l0_poc: vec![[None; 16]; macroblock_count],
        macroblock_intra: vec![true; macroblock_count],
    })
}

#[allow(clippy::too_many_arguments)]
fn encode_cabac_chroma_residuals(
    arithmetic: &mut CabacEncoder,
    contexts: &mut CabacIContexts,
    residuals: &[ChromaResidual; 2],
    chroma_pattern: u8,
    address: usize,
    macroblocks_wide: usize,
    coded_blocks: &mut CabacIntraCodedBlocks,
) -> Result<()> {
    if chroma_pattern != 0 {
        for (component, residual) in residuals.iter().enumerate() {
            let coded_context =
                cabac_intra_dc_coded_context(address, macroblocks_wide, |neighbor| {
                    coded_blocks.chroma_dc[neighbor][component]
                });
            let coded = residual.dc.iter().any(|&level| level != 0);
            arithmetic.decision(&mut contexts.chroma_dc_coded_block[coded_context], coded)?;
            coded_blocks.chroma_dc[address][component] = coded;
            if coded {
                encode_cabac_residual_block(
                    arithmetic,
                    &mut contexts.chroma_dc_significant,
                    &mut contexts.chroma_dc_last,
                    &mut contexts.chroma_dc_abs_level,
                    &residual.dc,
                )?;
            }
        }
    }
    if chroma_pattern == 2 {
        for (component, residual) in residuals.iter().enumerate() {
            for (block_index, levels) in residual.ac.iter().enumerate() {
                let coded_context = cabac_intra_chroma_ac_coded_context(
                    address,
                    macroblocks_wide,
                    component,
                    block_index,
                    &coded_blocks.chroma_ac,
                );
                let coded = levels.iter().any(|&level| level != 0);
                arithmetic.decision(&mut contexts.chroma_ac_coded_block[coded_context], coded)?;
                coded_blocks.chroma_ac[address][component][block_index] = coded;
                if coded {
                    encode_cabac_residual_block(
                        arithmetic,
                        &mut contexts.chroma_ac_significant,
                        &mut contexts.chroma_ac_last,
                        &mut contexts.chroma_ac_abs_level,
                        levels,
                    )?;
                }
            }
        }
    }
    Ok(())
}

fn encode_cabac_coded_block_pattern(
    arithmetic: &mut CabacEncoder,
    contexts: &mut CabacIContexts,
    pattern: u8,
    address: usize,
    macroblocks_wide: usize,
    patterns: &[u8],
) -> Result<()> {
    let left = if address.is_multiple_of(macroblocks_wide) {
        15
    } else {
        patterns[address - 1]
    };
    let top = if address >= macroblocks_wide {
        patterns[address - macroblocks_wide]
    } else {
        15
    };
    let mut luma = 0_u8;
    for (bit, context_index) in [
        usize::from(left & 0x02 == 0) + 2 * usize::from(top & 0x04 == 0),
        usize::from(pattern & 0x01 == 0) + 2 * usize::from(top & 0x08 == 0),
        usize::from(left & 0x08 == 0) + 2 * usize::from(pattern & 0x01 == 0),
        usize::from(pattern & 0x04 == 0) + 2 * usize::from(pattern & 0x02 == 0),
    ]
    .into_iter()
    .enumerate()
    {
        let value = pattern & (1 << bit) != 0;
        arithmetic.decision(&mut contexts.coded_block_pattern_luma[context_index], value)?;
        luma |= u8::from(value) << bit;
    }
    debug_assert_eq!(luma, pattern & 15);

    let chroma = pattern >> 4;
    let left_chroma = left >> 4;
    let top_chroma = top >> 4;
    let first_context = usize::from(left_chroma > 0) + 2 * usize::from(top_chroma > 0);
    arithmetic.decision(
        &mut contexts.coded_block_pattern_chroma[first_context],
        chroma != 0,
    )?;
    if chroma != 0 {
        let second_context = 4 + usize::from(left_chroma == 2) + 2 * usize::from(top_chroma == 2);
        arithmetic.decision(
            &mut contexts.coded_block_pattern_chroma[second_context],
            chroma == 2,
        )?;
    }
    Ok(())
}

fn encode_cabac_i_macroblock_type(
    arithmetic: &mut CabacEncoder,
    contexts: &mut CabacIContexts,
    macroblock_type: u8,
    context_increment: usize,
) -> Result<()> {
    debug_assert!(macroblock_type <= 24);
    arithmetic.decision(
        &mut contexts.macroblock_type[3 + context_increment],
        macroblock_type != 0,
    )?;
    if macroblock_type == 0 {
        return Ok(());
    }
    arithmetic.terminate(false)?;
    let code = macroblock_type - 1;
    arithmetic.decision(&mut contexts.macroblock_type[6], code >= 12)?;
    let remainder = code % 12;
    arithmetic.decision(&mut contexts.macroblock_type[7], remainder >= 4)?;
    if remainder >= 4 {
        arithmetic.decision(&mut contexts.macroblock_type[8], remainder >= 8)?;
    }
    arithmetic.decision(&mut contexts.macroblock_type[9], remainder % 4 >= 2)?;
    arithmetic.decision(
        &mut contexts.macroblock_type[10],
        !remainder.is_multiple_of(2),
    )
}

fn encode_cabac_chroma_prediction_mode(
    arithmetic: &mut CabacEncoder,
    contexts: &mut CabacIContexts,
    mode: u8,
    address: usize,
    macroblocks_wide: usize,
    modes: &[u8],
) -> Result<()> {
    debug_assert!(mode <= 3);
    let context_increment =
        usize::from(!address.is_multiple_of(macroblocks_wide) && modes[address - 1] != 0)
            + usize::from(address >= macroblocks_wide && modes[address - macroblocks_wide] != 0);
    arithmetic.decision(
        &mut contexts.chroma_prediction_mode[context_increment],
        mode != 0,
    )?;
    if mode != 0 {
        arithmetic.decision(&mut contexts.chroma_prediction_mode[3], mode != 1)?;
        if mode > 1 {
            arithmetic.decision(&mut contexts.chroma_prediction_mode[3], mode == 3)?;
        }
    }
    Ok(())
}

fn encode_cabac_macroblock_qp_delta(
    arithmetic: &mut CabacEncoder,
    contexts: &mut CabacIContexts,
    delta: i32,
    previous_delta: i32,
) -> Result<()> {
    arithmetic.decision(
        &mut contexts.macroblock_qp_delta[usize::from(previous_delta != 0)],
        delta != 0,
    )?;
    if delta == 0 {
        return Ok(());
    }
    let magnitude = delta.unsigned_abs();
    let code = if delta > 0 {
        magnitude * 2 - 1
    } else {
        magnitude * 2
    };
    for index in 1..code {
        let context_index = if index == 1 { 2 } else { 3 };
        arithmetic.decision(&mut contexts.macroblock_qp_delta[context_index], true)?;
    }
    let context_index = if code == 1 { 2 } else { 3 };
    arithmetic.decision(&mut contexts.macroblock_qp_delta[context_index], false)
}

fn encode_cabac_residual_block(
    arithmetic: &mut CabacEncoder,
    significant_contexts: &mut [crate::cabac::ContextState],
    last_contexts: &mut [crate::cabac::ContextState],
    absolute_level_contexts: &mut [crate::cabac::ContextState],
    levels: &[i32],
) -> Result<()> {
    let last_nonzero = levels
        .iter()
        .rposition(|&level| level != 0)
        .expect("coded CABAC residual block contains a coefficient");
    for (position, &level) in levels.iter().enumerate().take(levels.len() - 1) {
        let significant = level != 0;
        arithmetic.decision(&mut significant_contexts[position], significant)?;
        if significant {
            let last = position == last_nonzero;
            arithmetic.decision(&mut last_contexts[position], last)?;
            if last {
                break;
            }
        }
    }

    encode_cabac_coefficient_levels(
        arithmetic,
        absolute_level_contexts,
        &levels[..=last_nonzero],
    )
}

fn encode_cabac_residual_8x8(
    arithmetic: &mut CabacEncoder,
    significant_contexts: &mut [crate::cabac::ContextState; 15],
    last_contexts: &mut [crate::cabac::ContextState; 9],
    absolute_level_contexts: &mut [crate::cabac::ContextState; 10],
    levels: &[i32; 64],
) -> Result<()> {
    const SIGNIFICANT_CONTEXT: [usize; 63] = [
        0, 1, 2, 3, 4, 5, 5, 4, 4, 3, 3, 4, 4, 4, 5, 5, 4, 4, 4, 4, 3, 3, 6, 7, 7, 7, 8, 9, 10, 9,
        8, 7, 7, 6, 11, 12, 13, 11, 6, 7, 8, 9, 14, 10, 9, 8, 6, 11, 12, 13, 11, 6, 9, 14, 10, 9,
        11, 12, 13, 11, 14, 10, 12,
    ];
    const LAST_CONTEXT: [usize; 63] = [
        0, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2,
        2, 2, 3, 3, 3, 3, 3, 3, 3, 3, 4, 4, 4, 4, 4, 4, 4, 4, 5, 5, 5, 5, 6, 6, 6, 6, 7, 7, 7, 7,
        8, 8, 8,
    ];

    let last_nonzero = levels
        .iter()
        .rposition(|&level| level != 0)
        .expect("coded CABAC 8x8 residual block contains a coefficient");
    for (position, &level) in levels.iter().enumerate().take(63) {
        let significant = level != 0;
        arithmetic.decision(
            &mut significant_contexts[SIGNIFICANT_CONTEXT[position]],
            significant,
        )?;
        if significant {
            let last = position == last_nonzero;
            arithmetic.decision(&mut last_contexts[LAST_CONTEXT[position]], last)?;
            if last {
                break;
            }
        }
    }
    encode_cabac_coefficient_levels(
        arithmetic,
        absolute_level_contexts,
        &levels[..=last_nonzero],
    )
}

fn encode_cabac_coefficient_levels(
    arithmetic: &mut CabacEncoder,
    absolute_level_contexts: &mut [crate::cabac::ContextState],
    levels: &[i32],
) -> Result<()> {
    const LEVEL_ONE_CONTEXT: [usize; 8] = [1, 2, 3, 4, 0, 0, 0, 0];
    const LEVEL_GT_ONE_CONTEXT: [usize; 8] = [5, 5, 5, 5, 6, 7, 8, 9];
    const AFTER_LEVEL_ONE: [usize; 8] = [1, 2, 3, 3, 4, 5, 6, 7];
    const AFTER_LEVEL_GT_ONE: [usize; 8] = [4, 4, 4, 4, 5, 6, 7, 7];

    let mut node_context = 0;
    for &level in levels.iter().rev().filter(|&&level| level != 0) {
        let absolute_level = level.unsigned_abs();
        arithmetic.decision(
            &mut absolute_level_contexts[LEVEL_ONE_CONTEXT[node_context]],
            absolute_level > 1,
        )?;
        if absolute_level > 1 {
            let context_index = LEVEL_GT_ONE_CONTEXT[node_context];
            node_context = AFTER_LEVEL_GT_ONE[node_context];
            for _ in 2..absolute_level.min(15) {
                arithmetic.decision(&mut absolute_level_contexts[context_index], true)?;
            }
            if absolute_level < 15 {
                arithmetic.decision(&mut absolute_level_contexts[context_index], false)?;
            } else {
                encode_cabac_unsigned_bypass(arithmetic, absolute_level - 15)?;
            }
        } else {
            node_context = AFTER_LEVEL_ONE[node_context];
        }
        arithmetic.bypass(level < 0)?;
    }
    Ok(())
}

fn encode_cabac_unsigned_bypass(arithmetic: &mut CabacEncoder, value: u32) -> Result<()> {
    let code_num = value + 1;
    let prefix = u32::BITS - 1 - code_num.leading_zeros();
    for _ in 0..prefix {
        arithmetic.bypass(true)?;
    }
    arithmetic.bypass(false)?;
    for bit in (0..prefix).rev() {
        arithmetic.bypass(code_num & (1 << bit) != 0)?;
    }
    Ok(())
}

fn cabac_intra_dc_coded_context(
    address: usize,
    macroblocks_wide: usize,
    neighbor_is_coded: impl Fn(usize) -> bool,
) -> usize {
    let left = address.is_multiple_of(macroblocks_wide) || neighbor_is_coded(address - 1);
    let top = address < macroblocks_wide || neighbor_is_coded(address - macroblocks_wide);
    usize::from(left) + 2 * usize::from(top)
}

fn cabac_intra_luma_ac_coded_context(
    address: usize,
    macroblocks_wide: usize,
    block_index: usize,
    coded: &[[bool; 16]],
) -> usize {
    let (block_x, block_y) = luma_block_position(block_index);
    let left = if block_x > 0 {
        coded[address][luma_block_index(block_x - 1, block_y)]
    } else if !address.is_multiple_of(macroblocks_wide) {
        coded[address - 1][luma_block_index(3, block_y)]
    } else {
        true
    };
    let top = if block_y > 0 {
        coded[address][luma_block_index(block_x, block_y - 1)]
    } else if address >= macroblocks_wide {
        coded[address - macroblocks_wide][luma_block_index(block_x, 3)]
    } else {
        true
    };
    usize::from(left) + 2 * usize::from(top)
}

fn cabac_intra_chroma_ac_coded_context(
    address: usize,
    macroblocks_wide: usize,
    component: usize,
    block_index: usize,
    coded: &[[[bool; 4]; 2]],
) -> usize {
    let block_x = block_index % 2;
    let block_y = block_index / 2;
    let left = if block_x > 0 {
        coded[address][component][block_index - 1]
    } else if !address.is_multiple_of(macroblocks_wide) {
        coded[address - 1][component][block_y * 2 + 1]
    } else {
        true
    };
    let top = if block_y > 0 {
        coded[address][component][block_index - 2]
    } else if address >= macroblocks_wide {
        coded[address - macroblocks_wide][component][2 + block_x]
    } else {
        true
    };
    usize::from(left) + 2 * usize::from(top)
}

#[allow(clippy::too_many_lines)]
fn encode_p_picture(
    frame: &VideoFrame,
    configuration: &Configuration,
    references: &VecDeque<EncoderReference>,
    frame_num: u8,
    pic_order_cnt_lsb: u8,
) -> Result<EncodedFrame> {
    let reference = references
        .front()
        .expect("P picture has at least one short-term reference");
    let mut writer = BitWriter::new();
    write_ue(&mut writer, 0)?; // first_mb_in_slice
    write_ue(&mut writer, 0)?; // slice_type: P
    write_ue(&mut writer, 0)?; // pic_parameter_set_id
    writer.write_bits(u64::from(frame_num), 4)?;
    if configuration.pic_order_cnt_type0 {
        writer.write_bits(u64::from(pic_order_cnt_lsb), 4)?;
    }
    let active_references_minus1 =
        u32::try_from(references.len() - 1).expect("bounded H.264 reference count fits u32");
    writer.write_bit(active_references_minus1 != 0)?; // num_ref_idx_active_override_flag
    if active_references_minus1 != 0 {
        write_ue(&mut writer, u64::from(active_references_minus1))?;
    }
    writer.write_bit(false)?; // ref_pic_list_modification_flag_l0
    writer.write_bit(false)?; // adaptive_ref_pic_marking_mode_flag
    if configuration.entropy_coding == EntropyCoding::Cabac {
        write_ue(&mut writer, 0)?; // cabac_init_idc
    }
    write_se(&mut writer, i64::from(configuration.qp - 26))?;
    write_ue(&mut writer, 1)?; // disable_deblocking_filter_idc

    let macroblocks_wide = configuration.coded_width / 16;
    let macroblocks_high = configuration.coded_height / 16;
    let macroblock_count = macroblocks_wide * macroblocks_high;
    let source = padded_planes(frame, configuration);
    if configuration.entropy_coding == EntropyCoding::Cabac {
        writer.align_to_byte_with(true); // cabac_alignment_one_bit
        return encode_cabac_p_picture_data(
            frame,
            configuration,
            references,
            active_references_minus1,
            writer.into_bytes(),
            &source,
            macroblocks_wide,
            macroblocks_high,
        );
    }
    let macroblock_qps = adaptive_macroblock_qps(
        &source[0],
        configuration.coded_width,
        configuration.coded_height,
        configuration.qp,
        configuration.aq_strength,
    );
    let mut previous_qp = configuration.qp;
    let mut reconstructed = reference.planes.clone();
    let mut motions = vec![[None; 16]; macroblock_count];
    let mut luma_nonzero = vec![[0_u8; 16]; macroblock_count];
    let mut chroma_nonzero = vec![[[0_u8; 4]; 2]; macroblock_count];
    let mut skip_run = 0_u64;
    for macroblock_y in 0..macroblocks_high {
        for macroblock_x in 0..macroblocks_wide {
            let address = macroblock_y * macroblocks_wide + macroblock_x;
            let macroblock_qp = macroblock_qps[address];
            let decision = select_inter_partitions_with_analysis(
                &source,
                references,
                configuration.coded_width,
                configuration.coded_height,
                macroblock_x,
                macroblock_y,
                configuration.search_range,
                configuration.analysis,
                &motions,
                address,
                macroblocks_wide,
            );
            let transform_8x8_allowed = configuration.transform_8x8_mode
                && decision.partitions.iter().all(|selected| {
                    selected.partition.block_width >= 2 && selected.partition.block_height >= 2
                });
            let luma_levels = select_inter_luma_residual(
                &source[0],
                configuration.coded_width,
                macroblock_x,
                macroblock_y,
                &decision.luma_prediction,
                macroblock_qp,
                transform_8x8_allowed && configuration.analysis == AnalysisPreset::Full,
                configuration.transform_bypass,
                configuration.scaling_matrix,
            );
            let chroma_qp = chroma_qp(macroblock_qp);
            let chroma_residuals: [ChromaResidual; 2] = std::array::from_fn(|component| {
                quantize_chroma_residual(
                    &source[component + 1],
                    configuration.coded_width / 2,
                    macroblock_x,
                    macroblock_y,
                    &decision.chroma_predictions[component],
                    chroma_qp,
                    configuration.transform_bypass,
                    None,
                    configuration.scaling_matrix.four_by_four(false),
                )
            });
            let luma_pattern = luma_levels.coded_block_pattern();
            let chroma_pattern = chroma_coded_block_pattern(&chroma_residuals);
            let coded_block_pattern = luma_pattern | chroma_pattern << 4;

            reconstruct_inter_luma(
                &mut reconstructed[0],
                configuration.coded_width,
                macroblock_x,
                macroblock_y,
                &decision.luma_prediction,
                &luma_levels,
                macroblock_qp,
                configuration.scaling_matrix,
            );
            for (component, prediction) in decision.chroma_predictions.iter().enumerate() {
                reconstruct_chroma(
                    &mut reconstructed[component + 1],
                    configuration.coded_width / 2,
                    macroblock_x,
                    macroblock_y,
                    prediction,
                    &chroma_residuals[component],
                    chroma_qp,
                    configuration.transform_bypass,
                    None,
                    configuration.scaling_matrix.four_by_four(false),
                );
            }

            let skip_motion = p_skip_motion(&motions, address, macroblocks_wide);
            if coded_block_pattern == 0
                && decision.macroblock_type == 0
                && decision.partitions[0].reference_index == 0
                && decision.partitions[0].motion == skip_motion
            {
                skip_run += 1;
                set_partition_motion(
                    &mut motions[address],
                    InterPartition {
                        block_x: 0,
                        block_y: 0,
                        block_width: 4,
                        block_height: 4,
                        prediction_kind: MotionPredictionKind::Normal,
                    },
                    MotionState {
                        vector: skip_motion,
                        reference_index: Some(0),
                    },
                );
                continue;
            }
            write_ue(&mut writer, skip_run)?;
            skip_run = 0;
            write_ue(&mut writer, u64::from(decision.macroblock_type))?;
            if decision.macroblock_type == 3 {
                for sub_type in decision.sub_macroblock_types {
                    write_ue(&mut writer, u64::from(sub_type))?;
                }
            }
            let reference_partition_count = match decision.macroblock_type {
                0 => 1,
                1 | 2 => 2,
                3 => 4,
                _ => unreachable!("encoded P macroblock type is in 0..=3"),
            };
            for _ in 0..reference_partition_count {
                write_reference_index(
                    &mut writer,
                    decision.partitions[0].reference_index,
                    active_references_minus1,
                )?;
            }
            for selected in &decision.partitions {
                write_se(
                    &mut writer,
                    i64::from(selected.motion.x - selected.predicted.x),
                )?;
                write_se(
                    &mut writer,
                    i64::from(selected.motion.y - selected.predicted.y),
                )?;
            }
            let pattern_code = INTER_CODED_BLOCK_PATTERN
                .iter()
                .position(|&pattern| pattern == coded_block_pattern)
                .expect("every inter coded-block pattern has an Exp-Golomb mapping");
            write_ue(&mut writer, pattern_code as u64)?;
            if transform_8x8_allowed && luma_pattern != 0 {
                writer.write_bit(luma_levels.uses_8x8())?; // transform_size_8x8_flag
            }
            if coded_block_pattern != 0 {
                let qp_delta = macroblock_qp_delta(previous_qp, macroblock_qp);
                write_se(&mut writer, i64::from(qp_delta))?;
                previous_qp = macroblock_qp;
            }
            encode_inter_residuals(
                &mut writer,
                &luma_levels,
                &chroma_residuals,
                luma_pattern,
                chroma_pattern,
                &mut luma_nonzero,
                &mut chroma_nonzero,
                address,
                macroblocks_wide,
            )?;
            for selected in decision.partitions {
                set_partition_motion(
                    &mut motions[address],
                    selected.partition,
                    MotionState {
                        vector: selected.motion,
                        reference_index: Some(selected.reference_index),
                    },
                );
            }
        }
    }
    if skip_run != 0 {
        write_ue(&mut writer, skip_run)?;
    }
    finish_rbsp(&mut writer)?;
    let reconstructed_frame = visible_frame(frame, configuration, &reconstructed);
    let reference_l0_poc = motions
        .iter()
        .map(|blocks| {
            std::array::from_fn(|block| {
                blocks[block]
                    .and_then(|motion| motion.reference_index)
                    .and_then(|index| references.get(usize::from(index)))
                    .map(|reference| reference.pic_order_count)
            })
        })
        .collect();
    Ok(EncodedFrame {
        nal: make_nal(0x41, writer.into_bytes()),
        reconstructed: reconstructed_frame,
        coded_reconstruction: reconstructed,
        motion_l0: motions,
        reference_l0_poc,
        macroblock_intra: vec![false; macroblock_count],
    })
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn encode_cabac_p_picture_data(
    frame: &VideoFrame,
    configuration: &Configuration,
    references: &VecDeque<EncoderReference>,
    active_references_minus1: u32,
    mut rbsp: Vec<u8>,
    source: &[Vec<u8>; 3],
    macroblocks_wide: usize,
    macroblocks_high: usize,
) -> Result<EncodedFrame> {
    let macroblock_count = macroblocks_wide * macroblocks_high;
    let macroblock_qps = adaptive_macroblock_qps(
        &source[0],
        configuration.coded_width,
        configuration.coded_height,
        configuration.qp,
        configuration.aq_strength,
    );
    let mut contexts = CabacPContexts::new(configuration.qp, 0)?;
    let mut arithmetic = CabacEncoder::new();
    let mut coded_blocks = CabacIntraCodedBlocks::new(macroblock_count);
    let mut skipped = vec![false; macroblock_count];
    let mut transform_8x8 = vec![false; macroblock_count];
    let mut reference_indices = vec![[None; 16]; macroblock_count];
    let mut motion_differences = vec![[MotionVector::default(); 16]; macroblock_count];
    let mut motions = vec![[None; 16]; macroblock_count];
    let mut reconstructed = references
        .front()
        .expect("P picture has at least one short-term reference")
        .planes
        .clone();
    let mut previous_qp = configuration.qp;
    let mut previous_qp_delta = 0;

    for macroblock_y in 0..macroblocks_high {
        for macroblock_x in 0..macroblocks_wide {
            let address = macroblock_y * macroblocks_wide + macroblock_x;
            let macroblock_qp = macroblock_qps[address];
            let decision = select_inter_partitions_with_analysis(
                source,
                references,
                configuration.coded_width,
                configuration.coded_height,
                macroblock_x,
                macroblock_y,
                configuration.search_range,
                configuration.analysis,
                &motions,
                address,
                macroblocks_wide,
            );
            let transform_allowed = configuration.transform_8x8_mode
                && decision.partitions.iter().all(|selected| {
                    selected.partition.block_width >= 2 && selected.partition.block_height >= 2
                });
            let luma_levels = select_inter_luma_residual(
                &source[0],
                configuration.coded_width,
                macroblock_x,
                macroblock_y,
                &decision.luma_prediction,
                macroblock_qp,
                transform_allowed && configuration.analysis == AnalysisPreset::Full,
                configuration.transform_bypass,
                configuration.scaling_matrix,
            );
            let chroma_qp = chroma_qp(macroblock_qp);
            let chroma_residuals: [ChromaResidual; 2] = std::array::from_fn(|component| {
                quantize_chroma_residual(
                    &source[component + 1],
                    configuration.coded_width / 2,
                    macroblock_x,
                    macroblock_y,
                    &decision.chroma_predictions[component],
                    chroma_qp,
                    configuration.transform_bypass,
                    None,
                    configuration.scaling_matrix.four_by_four(false),
                )
            });
            let luma_pattern = luma_levels.coded_block_pattern();
            let chroma_pattern = chroma_coded_block_pattern(&chroma_residuals);
            let coded_block_pattern = luma_pattern | chroma_pattern << 4;

            reconstruct_inter_luma(
                &mut reconstructed[0],
                configuration.coded_width,
                macroblock_x,
                macroblock_y,
                &decision.luma_prediction,
                &luma_levels,
                macroblock_qp,
                configuration.scaling_matrix,
            );
            for (component, prediction) in decision.chroma_predictions.iter().enumerate() {
                reconstruct_chroma(
                    &mut reconstructed[component + 1],
                    configuration.coded_width / 2,
                    macroblock_x,
                    macroblock_y,
                    prediction,
                    &chroma_residuals[component],
                    chroma_qp,
                    configuration.transform_bypass,
                    None,
                    configuration.scaling_matrix.four_by_four(false),
                );
            }

            let skip_motion = p_skip_motion(&motions, address, macroblocks_wide);
            let is_skipped = coded_block_pattern == 0
                && decision.macroblock_type == 0
                && decision.partitions[0].reference_index == 0
                && decision.partitions[0].motion == skip_motion;
            let skip_context =
                usize::from(!address.is_multiple_of(macroblocks_wide) && !skipped[address - 1])
                    + usize::from(
                        address >= macroblocks_wide && !skipped[address - macroblocks_wide],
                    );
            arithmetic.decision(&mut contexts.skip[skip_context], is_skipped)?;
            if is_skipped {
                skipped[address] = true;
                previous_qp_delta = 0;
                reference_indices[address].fill(Some(0));
                set_partition_motion(
                    &mut motions[address],
                    InterPartition {
                        block_x: 0,
                        block_y: 0,
                        block_width: 4,
                        block_height: 4,
                        prediction_kind: MotionPredictionKind::Normal,
                    },
                    MotionState {
                        vector: skip_motion,
                        reference_index: Some(0),
                    },
                );
                arithmetic.terminate(address + 1 == macroblock_count)?;
                continue;
            }

            encode_cabac_p_macroblock_type(
                &mut arithmetic,
                &mut contexts,
                decision.macroblock_type,
            )?;
            if decision.macroblock_type == 3 {
                for sub_type in decision.sub_macroblock_types {
                    encode_cabac_p_sub_macroblock_type(&mut arithmetic, &mut contexts, sub_type)?;
                }
            }
            for partition in p_reference_partitions(decision.macroblock_type) {
                let selected = decision
                    .partitions
                    .iter()
                    .find(|selected| {
                        selected.partition.block_x == partition.block_x
                            && selected.partition.block_y == partition.block_y
                    })
                    .expect("P reference partition starts with an encoded motion partition");
                let context_increment = cabac_reference_index_context_encoder(
                    address,
                    macroblocks_wide,
                    partition.block_x,
                    partition.block_y,
                    &reference_indices,
                );
                encode_cabac_reference_index(
                    &mut arithmetic,
                    &mut contexts,
                    context_increment,
                    selected.reference_index,
                    active_references_minus1,
                )?;
                set_cabac_partition_reference_encoder(
                    &mut reference_indices,
                    address,
                    partition,
                    selected.reference_index,
                );
            }
            for selected in &decision.partitions {
                let difference = MotionVector {
                    x: selected.motion.x - selected.predicted.x,
                    y: selected.motion.y - selected.predicted.y,
                };
                encode_cabac_partition_motion_difference(
                    &mut arithmetic,
                    &mut contexts,
                    address,
                    macroblocks_wide,
                    selected.partition,
                    difference,
                    &mut motion_differences,
                )?;
            }
            encode_cabac_p_coded_block_pattern(
                &mut arithmetic,
                &mut contexts,
                coded_block_pattern,
                address,
                macroblocks_wide,
                &coded_blocks.patterns,
            )?;
            coded_blocks.patterns[address] = coded_block_pattern;
            if transform_allowed && luma_pattern != 0 {
                let left = !address.is_multiple_of(macroblocks_wide) && transform_8x8[address - 1];
                let top = address >= macroblocks_wide && transform_8x8[address - macroblocks_wide];
                arithmetic.decision(
                    &mut contexts.transform_size_8x8[usize::from(left) + usize::from(top)],
                    luma_levels.uses_8x8(),
                )?;
                transform_8x8[address] = luma_levels.uses_8x8();
            }
            if coded_block_pattern == 0 {
                previous_qp_delta = 0;
            } else {
                let qp_delta = macroblock_qp_delta(previous_qp, macroblock_qp);
                encode_cabac_p_macroblock_qp_delta(
                    &mut arithmetic,
                    &mut contexts,
                    qp_delta,
                    previous_qp_delta,
                )?;
                previous_qp_delta = qp_delta;
                previous_qp = macroblock_qp;
            }
            encode_cabac_p_residuals(
                &mut arithmetic,
                &mut contexts,
                &luma_levels,
                &chroma_residuals,
                luma_pattern,
                chroma_pattern,
                address,
                macroblocks_wide,
                &mut coded_blocks,
            )?;
            for selected in decision.partitions {
                set_partition_motion(
                    &mut motions[address],
                    selected.partition,
                    MotionState {
                        vector: selected.motion,
                        reference_index: Some(selected.reference_index),
                    },
                );
            }
            arithmetic.terminate(address + 1 == macroblock_count)?;
        }
    }
    rbsp.extend_from_slice(&arithmetic.finish()?);
    let reference_l0_poc = motions
        .iter()
        .map(|blocks| {
            std::array::from_fn(|block| {
                blocks[block]
                    .and_then(|motion| motion.reference_index)
                    .and_then(|index| references.get(usize::from(index)))
                    .map(|reference| reference.pic_order_count)
            })
        })
        .collect();
    Ok(EncodedFrame {
        nal: make_nal(0x41, rbsp),
        reconstructed: visible_frame(frame, configuration, &reconstructed),
        coded_reconstruction: reconstructed,
        motion_l0: motions,
        reference_l0_poc,
        macroblock_intra: vec![false; macroblock_count],
    })
}

fn encode_cabac_p_macroblock_type(
    arithmetic: &mut CabacEncoder,
    contexts: &mut CabacPContexts,
    macroblock_type: u8,
) -> Result<()> {
    debug_assert!(macroblock_type <= 3);
    arithmetic.decision(&mut contexts.macroblock_type[0], false)?;
    match macroblock_type {
        0 => {
            arithmetic.decision(&mut contexts.macroblock_type[1], false)?;
            arithmetic.decision(&mut contexts.macroblock_type[2], false)
        }
        1 => {
            arithmetic.decision(&mut contexts.macroblock_type[1], true)?;
            arithmetic.decision(&mut contexts.macroblock_type[3], true)
        }
        2 => {
            arithmetic.decision(&mut contexts.macroblock_type[1], true)?;
            arithmetic.decision(&mut contexts.macroblock_type[3], false)
        }
        3 => {
            arithmetic.decision(&mut contexts.macroblock_type[1], false)?;
            arithmetic.decision(&mut contexts.macroblock_type[2], true)
        }
        _ => unreachable!("P macroblock type is in 0..=3"),
    }
}

fn encode_cabac_p_sub_macroblock_type(
    arithmetic: &mut CabacEncoder,
    contexts: &mut CabacPContexts,
    sub_type: u8,
) -> Result<()> {
    debug_assert!(sub_type <= 3);
    arithmetic.decision(&mut contexts.sub_macroblock_type[0], sub_type == 0)?;
    if sub_type == 0 {
        return Ok(());
    }
    arithmetic.decision(&mut contexts.sub_macroblock_type[1], sub_type >= 2)?;
    if sub_type >= 2 {
        arithmetic.decision(&mut contexts.sub_macroblock_type[2], sub_type == 2)?;
    }
    Ok(())
}

fn p_reference_partitions(macroblock_type: u8) -> Vec<InterPartition> {
    match macroblock_type {
        0 => vec![InterPartition {
            block_x: 0,
            block_y: 0,
            block_width: 4,
            block_height: 4,
            prediction_kind: MotionPredictionKind::Normal,
        }],
        1 => vec![
            InterPartition {
                block_x: 0,
                block_y: 0,
                block_width: 4,
                block_height: 2,
                prediction_kind: MotionPredictionKind::Top16x8,
            },
            InterPartition {
                block_x: 0,
                block_y: 2,
                block_width: 4,
                block_height: 2,
                prediction_kind: MotionPredictionKind::Bottom16x8,
            },
        ],
        2 => vec![
            InterPartition {
                block_x: 0,
                block_y: 0,
                block_width: 2,
                block_height: 4,
                prediction_kind: MotionPredictionKind::Left8x16,
            },
            InterPartition {
                block_x: 2,
                block_y: 0,
                block_width: 2,
                block_height: 4,
                prediction_kind: MotionPredictionKind::Right8x16,
            },
        ],
        3 => (0..4)
            .map(|sub_index| InterPartition {
                block_x: (sub_index % 2) * 2,
                block_y: (sub_index / 2) * 2,
                block_width: 2,
                block_height: 2,
                prediction_kind: MotionPredictionKind::Normal,
            })
            .collect(),
        _ => unreachable!("P macroblock type is in 0..=3"),
    }
}

fn encode_cabac_reference_index(
    arithmetic: &mut CabacEncoder,
    contexts: &mut CabacPContexts,
    context_increment: usize,
    reference_index: u8,
    active_references_minus1: u32,
) -> Result<()> {
    if active_references_minus1 == 0 {
        return Ok(());
    }
    debug_assert!(u32::from(reference_index) <= active_references_minus1);
    let mut context_index = context_increment;
    for _ in 0..reference_index {
        arithmetic.decision(&mut contexts.reference_index[context_index], true)?;
        context_index = (context_index >> 2) + 4;
    }
    arithmetic.decision(&mut contexts.reference_index[context_index], false)
}

fn cabac_reference_index_context_encoder(
    address: usize,
    macroblocks_wide: usize,
    block_x: usize,
    block_y: usize,
    reference_indices: &[[Option<u8>; 16]],
) -> usize {
    let macroblock_x = address % macroblocks_wide;
    let macroblock_y = address / macroblocks_wide;
    let absolute_x = macroblock_x * 4 + block_x;
    let absolute_y = macroblock_y * 4 + block_y;
    usize::from(
        cabac_reference_at_encoder(
            absolute_x.cast_signed() - 1,
            absolute_y.cast_signed(),
            macroblocks_wide,
            reference_indices,
        )
        .is_some_and(|index| index > 0),
    ) + 2 * usize::from(
        cabac_reference_at_encoder(
            absolute_x.cast_signed(),
            absolute_y.cast_signed() - 1,
            macroblocks_wide,
            reference_indices,
        )
        .is_some_and(|index| index > 0),
    )
}

fn cabac_reference_at_encoder(
    block_x: isize,
    block_y: isize,
    macroblocks_wide: usize,
    reference_indices: &[[Option<u8>; 16]],
) -> Option<u8> {
    let blocks_wide = macroblocks_wide * 4;
    let blocks_high = reference_indices.len().div_ceil(macroblocks_wide) * 4;
    let block_x = usize::try_from(block_x).ok().filter(|&x| x < blocks_wide)?;
    let block_y = usize::try_from(block_y).ok().filter(|&y| y < blocks_high)?;
    let address = (block_y / 4) * macroblocks_wide + block_x / 4;
    reference_indices.get(address)?[luma_block_index(block_x % 4, block_y % 4)]
}

fn set_cabac_partition_reference_encoder(
    reference_indices: &mut [[Option<u8>; 16]],
    address: usize,
    partition: InterPartition,
    reference_index: u8,
) {
    for block_y in partition.block_y..partition.block_y + partition.block_height {
        for block_x in partition.block_x..partition.block_x + partition.block_width {
            reference_indices[address][luma_block_index(block_x, block_y)] = Some(reference_index);
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn encode_cabac_partition_motion_difference(
    arithmetic: &mut CabacEncoder,
    contexts: &mut CabacPContexts,
    address: usize,
    macroblocks_wide: usize,
    partition: InterPartition,
    difference: MotionVector,
    motion_differences: &mut [[MotionVector; 16]],
) -> Result<()> {
    let horizontal_context = cabac_mvd_neighbor_sum_encoder(
        address,
        macroblocks_wide,
        partition.block_x,
        partition.block_y,
        motion_differences,
        |vector| vector.x,
    );
    let vertical_context = cabac_mvd_neighbor_sum_encoder(
        address,
        macroblocks_wide,
        partition.block_x,
        partition.block_y,
        motion_differences,
        |vector| vector.y,
    );
    encode_cabac_motion_difference(
        arithmetic,
        &mut contexts.motion_x,
        horizontal_context,
        difference.x,
    )?;
    encode_cabac_motion_difference(
        arithmetic,
        &mut contexts.motion_y,
        vertical_context,
        difference.y,
    )?;
    for block_y in partition.block_y..partition.block_y + partition.block_height {
        for block_x in partition.block_x..partition.block_x + partition.block_width {
            motion_differences[address][luma_block_index(block_x, block_y)] = difference;
        }
    }
    Ok(())
}

fn encode_cabac_motion_difference(
    arithmetic: &mut CabacEncoder,
    contexts: &mut [crate::cabac::ContextState; 7],
    neighboring_absolute_sum: u32,
    difference: i32,
) -> Result<()> {
    let magnitude = difference.unsigned_abs();
    let initial_context =
        usize::from(neighboring_absolute_sum > 2) + usize::from(neighboring_absolute_sum > 32);
    arithmetic.decision(&mut contexts[initial_context], magnitude != 0)?;
    if magnitude == 0 {
        return Ok(());
    }
    let mut encoded_magnitude = 1_u32;
    let mut context_index = 3;
    while encoded_magnitude < magnitude.min(9) {
        arithmetic.decision(&mut contexts[context_index], true)?;
        if encoded_magnitude < 4 {
            context_index += 1;
        }
        encoded_magnitude += 1;
    }
    if magnitude < 9 {
        arithmetic.decision(&mut contexts[context_index], false)?;
    } else {
        encode_cabac_unsigned_bypass_order(arithmetic, magnitude - 9, 3)?;
    }
    arithmetic.bypass(difference < 0)
}

fn encode_cabac_unsigned_bypass_order(
    arithmetic: &mut CabacEncoder,
    mut value: u32,
    order: u8,
) -> Result<()> {
    let mut suffix_bits = u32::from(order);
    while value >= 1_u32 << suffix_bits {
        arithmetic.bypass(true)?;
        value -= 1_u32 << suffix_bits;
        suffix_bits += 1;
    }
    arithmetic.bypass(false)?;
    for bit in (0..suffix_bits).rev() {
        arithmetic.bypass(value & (1 << bit) != 0)?;
    }
    Ok(())
}

fn cabac_mvd_neighbor_sum_encoder(
    address: usize,
    macroblocks_wide: usize,
    block_x: usize,
    block_y: usize,
    differences: &[[MotionVector; 16]],
    component: impl Fn(MotionVector) -> i32,
) -> u32 {
    let left = if block_x > 0 {
        component(differences[address][luma_block_index(block_x - 1, block_y)]).unsigned_abs()
    } else if address.is_multiple_of(macroblocks_wide) {
        0
    } else {
        component(differences[address - 1][luma_block_index(3, block_y)]).unsigned_abs()
    };
    let top = if block_y > 0 {
        component(differences[address][luma_block_index(block_x, block_y - 1)]).unsigned_abs()
    } else if address < macroblocks_wide {
        0
    } else {
        component(differences[address - macroblocks_wide][luma_block_index(block_x, 3)])
            .unsigned_abs()
    };
    left.saturating_add(top)
}

fn encode_cabac_p_coded_block_pattern(
    arithmetic: &mut CabacEncoder,
    contexts: &mut CabacPContexts,
    pattern: u8,
    address: usize,
    macroblocks_wide: usize,
    patterns: &[u8],
) -> Result<()> {
    let left = if address.is_multiple_of(macroblocks_wide) {
        15
    } else {
        patterns[address - 1]
    };
    let top = if address >= macroblocks_wide {
        patterns[address - macroblocks_wide]
    } else {
        15
    };
    for (bit, context_index) in [
        usize::from(left & 0x02 == 0) + 2 * usize::from(top & 0x04 == 0),
        usize::from(pattern & 0x01 == 0) + 2 * usize::from(top & 0x08 == 0),
        usize::from(left & 0x08 == 0) + 2 * usize::from(pattern & 0x01 == 0),
        usize::from(pattern & 0x04 == 0) + 2 * usize::from(pattern & 0x02 == 0),
    ]
    .into_iter()
    .enumerate()
    {
        arithmetic.decision(
            &mut contexts.coded_block_pattern_luma[context_index],
            pattern & (1 << bit) != 0,
        )?;
    }
    let chroma = pattern >> 4;
    let left_chroma = left >> 4;
    let top_chroma = top >> 4;
    let first_context = usize::from(left_chroma > 0) + 2 * usize::from(top_chroma > 0);
    arithmetic.decision(
        &mut contexts.coded_block_pattern_chroma[first_context],
        chroma != 0,
    )?;
    if chroma != 0 {
        let second_context = 4 + usize::from(left_chroma == 2) + 2 * usize::from(top_chroma == 2);
        arithmetic.decision(
            &mut contexts.coded_block_pattern_chroma[second_context],
            chroma == 2,
        )?;
    }
    Ok(())
}

fn encode_cabac_p_macroblock_qp_delta(
    arithmetic: &mut CabacEncoder,
    contexts: &mut CabacPContexts,
    delta: i32,
    previous_delta: i32,
) -> Result<()> {
    arithmetic.decision(
        &mut contexts.macroblock_qp_delta[usize::from(previous_delta != 0)],
        delta != 0,
    )?;
    if delta == 0 {
        return Ok(());
    }
    let magnitude = delta.unsigned_abs();
    let code = if delta > 0 {
        magnitude * 2 - 1
    } else {
        magnitude * 2
    };
    for index in 1..code {
        let context_index = if index == 1 { 2 } else { 3 };
        arithmetic.decision(&mut contexts.macroblock_qp_delta[context_index], true)?;
    }
    let context_index = if code == 1 { 2 } else { 3 };
    arithmetic.decision(&mut contexts.macroblock_qp_delta[context_index], false)
}

#[allow(clippy::too_many_arguments)]
fn encode_cabac_p_residuals(
    arithmetic: &mut CabacEncoder,
    contexts: &mut CabacPContexts,
    luma_levels: &InterLumaResidual,
    chroma_residuals: &[ChromaResidual; 2],
    luma_pattern: u8,
    chroma_pattern: u8,
    address: usize,
    macroblocks_wide: usize,
    coded_blocks: &mut CabacIntraCodedBlocks,
) -> Result<()> {
    match luma_levels {
        InterLumaResidual::FourByFour(levels) | InterLumaResidual::BypassFourByFour(levels) => {
            for group in 0..4 {
                if luma_pattern & (1 << group) == 0 {
                    continue;
                }
                for block_index in group * 4..group * 4 + 4 {
                    let block_levels = &levels[block_index];
                    let coded_context = cabac_inter_luma_coded_context_encoder(
                        address,
                        macroblocks_wide,
                        block_index,
                        &coded_blocks.luma_ac,
                    );
                    let coded = block_levels.iter().any(|&level| level != 0);
                    arithmetic.decision(&mut contexts.luma_coded_block[coded_context], coded)?;
                    coded_blocks.luma_ac[address][block_index] = coded;
                    if coded {
                        encode_cabac_residual_block(
                            arithmetic,
                            &mut contexts.luma_significant,
                            &mut contexts.luma_last,
                            &mut contexts.luma_abs_level,
                            block_levels,
                        )?;
                    }
                }
            }
        }
        InterLumaResidual::EightByEight(levels) => {
            for (group, group_levels) in levels.iter().enumerate() {
                if luma_pattern & (1 << group) == 0 {
                    continue;
                }
                encode_cabac_residual_8x8(
                    arithmetic,
                    &mut contexts.luma_8x8_significant,
                    &mut contexts.luma_8x8_last,
                    &mut contexts.luma_8x8_abs_level,
                    group_levels,
                )?;
                for block in group * 4..group * 4 + 4 {
                    coded_blocks.luma_ac[address][block] = true;
                }
            }
        }
    }
    if chroma_pattern != 0 {
        for (component, residual) in chroma_residuals.iter().enumerate() {
            let coded_context =
                cabac_inter_dc_coded_context_encoder(address, macroblocks_wide, |neighbor| {
                    coded_blocks.chroma_dc[neighbor][component]
                });
            let coded = residual.dc.iter().any(|&level| level != 0);
            arithmetic.decision(&mut contexts.chroma_dc_coded_block[coded_context], coded)?;
            coded_blocks.chroma_dc[address][component] = coded;
            if coded {
                encode_cabac_residual_block(
                    arithmetic,
                    &mut contexts.chroma_dc_significant,
                    &mut contexts.chroma_dc_last,
                    &mut contexts.chroma_dc_abs_level,
                    &residual.dc,
                )?;
            }
        }
    }
    if chroma_pattern == 2 {
        for (component, residual) in chroma_residuals.iter().enumerate() {
            for (block_index, block_levels) in residual.ac.iter().enumerate() {
                let coded_context = cabac_inter_chroma_coded_context_encoder(
                    address,
                    macroblocks_wide,
                    component,
                    block_index,
                    &coded_blocks.chroma_ac,
                );
                let coded = block_levels.iter().any(|&level| level != 0);
                arithmetic.decision(&mut contexts.chroma_ac_coded_block[coded_context], coded)?;
                coded_blocks.chroma_ac[address][component][block_index] = coded;
                if coded {
                    encode_cabac_residual_block(
                        arithmetic,
                        &mut contexts.chroma_ac_significant,
                        &mut contexts.chroma_ac_last,
                        &mut contexts.chroma_ac_abs_level,
                        block_levels,
                    )?;
                }
            }
        }
    }
    Ok(())
}

fn cabac_inter_dc_coded_context_encoder(
    address: usize,
    macroblocks_wide: usize,
    neighbor_is_coded: impl Fn(usize) -> bool,
) -> usize {
    let left = !address.is_multiple_of(macroblocks_wide) && neighbor_is_coded(address - 1);
    let top = address >= macroblocks_wide && neighbor_is_coded(address - macroblocks_wide);
    usize::from(left) + 2 * usize::from(top)
}

fn cabac_inter_luma_coded_context_encoder(
    address: usize,
    macroblocks_wide: usize,
    block_index: usize,
    coded: &[[bool; 16]],
) -> usize {
    let (block_x, block_y) = luma_block_position(block_index);
    let left = if block_x > 0 {
        coded[address][luma_block_index(block_x - 1, block_y)]
    } else {
        !address.is_multiple_of(macroblocks_wide)
            && coded[address - 1][luma_block_index(3, block_y)]
    };
    let top = if block_y > 0 {
        coded[address][luma_block_index(block_x, block_y - 1)]
    } else {
        address >= macroblocks_wide
            && coded[address - macroblocks_wide][luma_block_index(block_x, 3)]
    };
    usize::from(left) + 2 * usize::from(top)
}

fn cabac_inter_chroma_coded_context_encoder(
    address: usize,
    macroblocks_wide: usize,
    component: usize,
    block_index: usize,
    coded: &[[[bool; 4]; 2]],
) -> usize {
    let block_x = block_index % 2;
    let block_y = block_index / 2;
    let left = if block_x > 0 {
        coded[address][component][block_index - 1]
    } else {
        !address.is_multiple_of(macroblocks_wide) && coded[address - 1][component][block_y * 2 + 1]
    };
    let top = if block_y > 0 {
        coded[address][component][block_index - 2]
    } else {
        address >= macroblocks_wide && coded[address - macroblocks_wide][component][2 + block_x]
    };
    usize::from(left) + 2 * usize::from(top)
}

#[allow(clippy::too_many_lines)]
fn encode_b_picture(
    frame: &VideoFrame,
    configuration: &Configuration,
    previous: &EncoderReference,
    future: &EncoderReference,
    frame_num: u8,
    pic_order_cnt_lsb: u8,
    picture_order_count: i32,
) -> Result<EncodedFrame> {
    let mut writer = BitWriter::new();
    write_ue(&mut writer, 0)?; // first_mb_in_slice
    write_ue(&mut writer, 1)?; // slice_type: B
    write_ue(&mut writer, 0)?; // pic_parameter_set_id
    writer.write_bits(u64::from(frame_num), 4)?;
    writer.write_bits(u64::from(pic_order_cnt_lsb), 4)?;
    writer.write_bit(configuration.b_direct_mode == BDirectMode::Spatial)?;
    writer.write_bit(false)?; // num_ref_idx_active_override_flag
    writer.write_bit(false)?; // ref_pic_list_modification_flag_l0
    writer.write_bit(false)?; // ref_pic_list_modification_flag_l1
    if configuration.entropy_coding == EntropyCoding::Cabac {
        write_ue(&mut writer, 0)?; // cabac_init_idc
    }
    write_se(&mut writer, i64::from(configuration.qp - 26))?;
    write_ue(&mut writer, 1)?; // disable_deblocking_filter_idc

    let macroblocks_wide = configuration.coded_width / 16;
    let macroblocks_high = configuration.coded_height / 16;
    let macroblock_count = macroblocks_wide * macroblocks_high;
    let source = padded_planes(frame, configuration);
    if configuration.entropy_coding == EntropyCoding::Cabac {
        writer.align_to_byte_with(true); // cabac_alignment_one_bit
        return encode_cabac_b_picture_data(
            frame,
            configuration,
            previous,
            future,
            picture_order_count,
            writer.into_bytes(),
            &source,
            macroblocks_wide,
            macroblocks_high,
        );
    }
    let macroblock_qps = adaptive_macroblock_qps(
        &source[0],
        configuration.coded_width,
        configuration.coded_height,
        configuration.qp,
        configuration.aq_strength,
    );
    let mut previous_qp = configuration.qp;
    let mut reconstructed = previous.planes.clone();
    let mut motions_l0 = vec![[None; 16]; macroblock_count];
    let mut motions_l1 = vec![[None; 16]; macroblock_count];
    let mut luma_nonzero = vec![[0_u8; 16]; macroblock_count];
    let mut chroma_nonzero = vec![[[0_u8; 4]; 2]; macroblock_count];
    let mut skip_run = 0_u64;
    for macroblock_y in 0..macroblocks_high {
        for macroblock_x in 0..macroblocks_wide {
            let address = macroblock_y * macroblocks_wide + macroblock_x;
            let macroblock_qp = macroblock_qps[address];
            let decision = select_b_inter_partitions_with_analysis(
                &source,
                previous,
                future,
                configuration.coded_width,
                configuration.coded_height,
                macroblock_x,
                macroblock_y,
                configuration.search_range,
                configuration.analysis,
                &motions_l0,
                &motions_l1,
                address,
                macroblocks_wide,
                BDirectContext {
                    mode: configuration.b_direct_mode,
                    picture_order_count,
                },
            );
            let transform_8x8_allowed = configuration.transform_8x8_mode
                && decision.partitions.iter().all(|selected| {
                    let partition = selected.partition();
                    partition.block_width >= 2 && partition.block_height >= 2
                });
            let luma_levels = select_inter_luma_residual(
                &source[0],
                configuration.coded_width,
                macroblock_x,
                macroblock_y,
                &decision.luma_prediction,
                macroblock_qp,
                transform_8x8_allowed && configuration.analysis == AnalysisPreset::Full,
                configuration.transform_bypass,
                configuration.scaling_matrix,
            );
            let chroma_qp = chroma_qp(macroblock_qp);
            let chroma_residuals: [ChromaResidual; 2] = std::array::from_fn(|component| {
                quantize_chroma_residual(
                    &source[component + 1],
                    configuration.coded_width / 2,
                    macroblock_x,
                    macroblock_y,
                    &decision.chroma_predictions[component],
                    chroma_qp,
                    configuration.transform_bypass,
                    None,
                    configuration.scaling_matrix.four_by_four(false),
                )
            });
            let luma_pattern = luma_levels.coded_block_pattern();
            let chroma_pattern = chroma_coded_block_pattern(&chroma_residuals);
            let coded_block_pattern = luma_pattern | chroma_pattern << 4;

            reconstruct_inter_luma(
                &mut reconstructed[0],
                configuration.coded_width,
                macroblock_x,
                macroblock_y,
                &decision.luma_prediction,
                &luma_levels,
                macroblock_qp,
                configuration.scaling_matrix,
            );
            for (component, prediction) in decision.chroma_predictions.iter().enumerate() {
                reconstruct_chroma(
                    &mut reconstructed[component + 1],
                    configuration.coded_width / 2,
                    macroblock_x,
                    macroblock_y,
                    prediction,
                    &chroma_residuals[component],
                    chroma_qp,
                    configuration.transform_bypass,
                    None,
                    configuration.scaling_matrix.four_by_four(false),
                );
            }

            if decision.direct && coded_block_pattern == 0 {
                skip_run += 1;
                commit_b_motion_history(
                    &decision,
                    &mut motions_l0[address],
                    &mut motions_l1[address],
                );
                continue;
            }
            write_ue(&mut writer, skip_run)?;
            skip_run = 0;
            write_ue(&mut writer, u64::from(decision.macroblock_type))?;
            if decision.macroblock_type == 22 {
                for sub_type in decision.sub_macroblock_types {
                    write_ue(&mut writer, u64::from(sub_type))?;
                }
            }
            if !decision.direct {
                for selected in decision
                    .partitions
                    .iter()
                    .filter(|partition| !partition.direct)
                    .filter_map(|partition| partition.list0)
                {
                    write_motion_difference(&mut writer, selected)?;
                }
                for selected in decision
                    .partitions
                    .iter()
                    .filter(|partition| !partition.direct)
                    .filter_map(|partition| partition.list1)
                {
                    write_motion_difference(&mut writer, selected)?;
                }
            }
            commit_b_motion_history(
                &decision,
                &mut motions_l0[address],
                &mut motions_l1[address],
            );
            let pattern_code = INTER_CODED_BLOCK_PATTERN
                .iter()
                .position(|&pattern| pattern == coded_block_pattern)
                .expect("every inter coded-block pattern has an Exp-Golomb mapping");
            write_ue(&mut writer, pattern_code as u64)?;
            if transform_8x8_allowed && luma_pattern != 0 {
                writer.write_bit(luma_levels.uses_8x8())?; // transform_size_8x8_flag
            }
            if coded_block_pattern != 0 {
                let qp_delta = macroblock_qp_delta(previous_qp, macroblock_qp);
                write_se(&mut writer, i64::from(qp_delta))?;
                previous_qp = macroblock_qp;
            }
            encode_inter_residuals(
                &mut writer,
                &luma_levels,
                &chroma_residuals,
                luma_pattern,
                chroma_pattern,
                &mut luma_nonzero,
                &mut chroma_nonzero,
                address,
                macroblocks_wide,
            )?;
        }
    }
    if skip_run != 0 {
        write_ue(&mut writer, skip_run)?;
    }
    finish_rbsp(&mut writer)?;
    let reconstructed_frame = visible_frame(frame, configuration, &reconstructed);
    Ok(EncodedFrame {
        nal: make_nal(0x01, writer.into_bytes()),
        reconstructed: reconstructed_frame,
        coded_reconstruction: reconstructed,
        motion_l0: Vec::new(),
        reference_l0_poc: Vec::new(),
        macroblock_intra: Vec::new(),
    })
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn encode_cabac_b_picture_data(
    frame: &VideoFrame,
    configuration: &Configuration,
    previous: &EncoderReference,
    future: &EncoderReference,
    picture_order_count: i32,
    mut rbsp: Vec<u8>,
    source: &[Vec<u8>; 3],
    macroblocks_wide: usize,
    macroblocks_high: usize,
) -> Result<EncodedFrame> {
    let macroblock_count = macroblocks_wide * macroblocks_high;
    let macroblock_qps = adaptive_macroblock_qps(
        &source[0],
        configuration.coded_width,
        configuration.coded_height,
        configuration.qp,
        configuration.aq_strength,
    );
    let mut contexts = CabacBContexts::new(configuration.qp, 0)?;
    let mut arithmetic = CabacEncoder::new();
    let mut coded_blocks = CabacIntraCodedBlocks::new(macroblock_count);
    let mut skipped = vec![false; macroblock_count];
    let mut direct = vec![false; macroblock_count];
    let mut transform_8x8 = vec![false; macroblock_count];
    let mut motion_differences_l0 = vec![[MotionVector::default(); 16]; macroblock_count];
    let mut motion_differences_l1 = vec![[MotionVector::default(); 16]; macroblock_count];
    let mut motions_l0 = vec![[None; 16]; macroblock_count];
    let mut motions_l1 = vec![[None; 16]; macroblock_count];
    let mut reconstructed = previous.planes.clone();
    let mut previous_qp = configuration.qp;
    let mut previous_qp_delta = 0;

    for macroblock_y in 0..macroblocks_high {
        for macroblock_x in 0..macroblocks_wide {
            let address = macroblock_y * macroblocks_wide + macroblock_x;
            let macroblock_qp = macroblock_qps[address];
            let decision = select_b_inter_partitions_with_analysis(
                source,
                previous,
                future,
                configuration.coded_width,
                configuration.coded_height,
                macroblock_x,
                macroblock_y,
                configuration.search_range,
                configuration.analysis,
                &motions_l0,
                &motions_l1,
                address,
                macroblocks_wide,
                BDirectContext {
                    mode: configuration.b_direct_mode,
                    picture_order_count,
                },
            );
            let transform_allowed = configuration.transform_8x8_mode
                && decision.partitions.iter().all(|selected| {
                    let partition = selected.partition();
                    partition.block_width >= 2 && partition.block_height >= 2
                });
            let luma_levels = select_inter_luma_residual(
                &source[0],
                configuration.coded_width,
                macroblock_x,
                macroblock_y,
                &decision.luma_prediction,
                macroblock_qp,
                transform_allowed && configuration.analysis == AnalysisPreset::Full,
                configuration.transform_bypass,
                configuration.scaling_matrix,
            );
            let chroma_qp = chroma_qp(macroblock_qp);
            let chroma_residuals: [ChromaResidual; 2] = std::array::from_fn(|component| {
                quantize_chroma_residual(
                    &source[component + 1],
                    configuration.coded_width / 2,
                    macroblock_x,
                    macroblock_y,
                    &decision.chroma_predictions[component],
                    chroma_qp,
                    configuration.transform_bypass,
                    None,
                    configuration.scaling_matrix.four_by_four(false),
                )
            });
            let luma_pattern = luma_levels.coded_block_pattern();
            let chroma_pattern = chroma_coded_block_pattern(&chroma_residuals);
            let coded_block_pattern = luma_pattern | chroma_pattern << 4;

            reconstruct_inter_luma(
                &mut reconstructed[0],
                configuration.coded_width,
                macroblock_x,
                macroblock_y,
                &decision.luma_prediction,
                &luma_levels,
                macroblock_qp,
                configuration.scaling_matrix,
            );
            for (component, prediction) in decision.chroma_predictions.iter().enumerate() {
                reconstruct_chroma(
                    &mut reconstructed[component + 1],
                    configuration.coded_width / 2,
                    macroblock_x,
                    macroblock_y,
                    prediction,
                    &chroma_residuals[component],
                    chroma_qp,
                    configuration.transform_bypass,
                    None,
                    configuration.scaling_matrix.four_by_four(false),
                );
            }

            let is_skipped = decision.direct && coded_block_pattern == 0;
            let skip_context =
                usize::from(!address.is_multiple_of(macroblocks_wide) && !skipped[address - 1])
                    + usize::from(
                        address >= macroblocks_wide && !skipped[address - macroblocks_wide],
                    );
            arithmetic.decision(&mut contexts.skip[skip_context], is_skipped)?;
            if is_skipped {
                skipped[address] = true;
                direct[address] = true;
                previous_qp_delta = 0;
                commit_b_motion_history(
                    &decision,
                    &mut motions_l0[address],
                    &mut motions_l1[address],
                );
                arithmetic.terminate(address + 1 == macroblock_count)?;
                continue;
            }

            let type_context =
                usize::from(!address.is_multiple_of(macroblocks_wide) && !direct[address - 1])
                    + usize::from(
                        address >= macroblocks_wide && !direct[address - macroblocks_wide],
                    );
            encode_cabac_b_macroblock_type(
                &mut arithmetic,
                &mut contexts,
                decision.macroblock_type,
                type_context,
            )?;
            if decision.macroblock_type == 22 {
                for sub_type in decision.sub_macroblock_types {
                    encode_cabac_b_sub_macroblock_type(&mut arithmetic, &mut contexts, sub_type)?;
                }
            }
            for selected in decision
                .partitions
                .iter()
                .filter(|selected| !selected.direct)
                .filter_map(|selected| selected.list0)
            {
                encode_cabac_partition_motion_difference(
                    &mut arithmetic,
                    &mut contexts.inter,
                    address,
                    macroblocks_wide,
                    selected.partition,
                    MotionVector {
                        x: selected.motion.x - selected.predicted.x,
                        y: selected.motion.y - selected.predicted.y,
                    },
                    &mut motion_differences_l0,
                )?;
            }
            for selected in decision
                .partitions
                .iter()
                .filter(|selected| !selected.direct)
                .filter_map(|selected| selected.list1)
            {
                encode_cabac_partition_motion_difference(
                    &mut arithmetic,
                    &mut contexts.inter,
                    address,
                    macroblocks_wide,
                    selected.partition,
                    MotionVector {
                        x: selected.motion.x - selected.predicted.x,
                        y: selected.motion.y - selected.predicted.y,
                    },
                    &mut motion_differences_l1,
                )?;
            }
            let all_direct = decision.macroblock_type == 0
                || decision.macroblock_type == 22
                    && decision.partitions.iter().all(|selected| selected.direct);
            direct[address] = all_direct;
            commit_b_motion_history(
                &decision,
                &mut motions_l0[address],
                &mut motions_l1[address],
            );
            encode_cabac_p_coded_block_pattern(
                &mut arithmetic,
                &mut contexts.inter,
                coded_block_pattern,
                address,
                macroblocks_wide,
                &coded_blocks.patterns,
            )?;
            coded_blocks.patterns[address] = coded_block_pattern;
            if transform_allowed && luma_pattern != 0 {
                let left = !address.is_multiple_of(macroblocks_wide) && transform_8x8[address - 1];
                let top = address >= macroblocks_wide && transform_8x8[address - macroblocks_wide];
                arithmetic.decision(
                    &mut contexts.inter.transform_size_8x8[usize::from(left) + usize::from(top)],
                    luma_levels.uses_8x8(),
                )?;
                transform_8x8[address] = luma_levels.uses_8x8();
            }
            if coded_block_pattern == 0 {
                previous_qp_delta = 0;
            } else {
                let qp_delta = macroblock_qp_delta(previous_qp, macroblock_qp);
                encode_cabac_p_macroblock_qp_delta(
                    &mut arithmetic,
                    &mut contexts.inter,
                    qp_delta,
                    previous_qp_delta,
                )?;
                previous_qp_delta = qp_delta;
                previous_qp = macroblock_qp;
            }
            encode_cabac_p_residuals(
                &mut arithmetic,
                &mut contexts.inter,
                &luma_levels,
                &chroma_residuals,
                luma_pattern,
                chroma_pattern,
                address,
                macroblocks_wide,
                &mut coded_blocks,
            )?;
            arithmetic.terminate(address + 1 == macroblock_count)?;
        }
    }
    rbsp.extend_from_slice(&arithmetic.finish()?);
    Ok(EncodedFrame {
        nal: make_nal(0x01, rbsp),
        reconstructed: visible_frame(frame, configuration, &reconstructed),
        coded_reconstruction: reconstructed,
        motion_l0: Vec::new(),
        reference_l0_poc: Vec::new(),
        macroblock_intra: Vec::new(),
    })
}

fn encode_cabac_b_macroblock_type(
    arithmetic: &mut CabacEncoder,
    contexts: &mut CabacBContexts,
    macroblock_type: u8,
    context_increment: usize,
) -> Result<()> {
    debug_assert!(macroblock_type <= 22);
    arithmetic.decision(
        &mut contexts.macroblock_type[context_increment],
        macroblock_type != 0,
    )?;
    if macroblock_type == 0 {
        return Ok(());
    }
    arithmetic.decision(&mut contexts.macroblock_type[3], macroblock_type >= 3)?;
    if macroblock_type <= 2 {
        return arithmetic.decision(&mut contexts.macroblock_type[5], macroblock_type == 2);
    }
    let (bits, extra) = match macroblock_type {
        3..=10 => (macroblock_type - 3, None),
        11 => (14, None),
        12..=21 => (
            macroblock_type / 2 + 2,
            Some(!macroblock_type.is_multiple_of(2)),
        ),
        22 => (15, None),
        _ => unreachable!("B macroblock type is in 0..=22"),
    };
    arithmetic.decision(&mut contexts.macroblock_type[4], bits & 8 != 0)?;
    for shift in (0..3).rev() {
        arithmetic.decision(&mut contexts.macroblock_type[5], bits & (1 << shift) != 0)?;
    }
    if let Some(extra) = extra {
        arithmetic.decision(&mut contexts.macroblock_type[5], extra)?;
    }
    Ok(())
}

fn encode_cabac_b_sub_macroblock_type(
    arithmetic: &mut CabacEncoder,
    contexts: &mut CabacBContexts,
    sub_type: u8,
) -> Result<()> {
    debug_assert!(sub_type <= 12);
    arithmetic.decision(&mut contexts.sub_macroblock_type[0], sub_type != 0)?;
    if sub_type == 0 {
        return Ok(());
    }
    arithmetic.decision(&mut contexts.sub_macroblock_type[1], sub_type >= 3)?;
    if sub_type <= 2 {
        return arithmetic.decision(&mut contexts.sub_macroblock_type[3], sub_type == 2);
    }
    arithmetic.decision(&mut contexts.sub_macroblock_type[2], sub_type >= 7)?;
    if sub_type >= 7 {
        arithmetic.decision(&mut contexts.sub_macroblock_type[3], sub_type >= 11)?;
        if sub_type >= 11 {
            return arithmetic.decision(&mut contexts.sub_macroblock_type[3], sub_type == 12);
        }
    }
    let remainder = if sub_type >= 7 {
        sub_type - 7
    } else {
        sub_type - 3
    };
    arithmetic.decision(&mut contexts.sub_macroblock_type[3], remainder & 2 != 0)?;
    arithmetic.decision(&mut contexts.sub_macroblock_type[3], remainder & 1 != 0)
}

#[cfg(test)]
#[allow(clippy::too_many_arguments)]
fn select_b_inter_partitions(
    source: &[Vec<u8>; 3],
    previous: &EncoderReference,
    future: &EncoderReference,
    coded_width: usize,
    coded_height: usize,
    macroblock_x: usize,
    macroblock_y: usize,
    search_range: i32,
    history_l0: &[[Option<MotionState>; 16]],
    history_l1: &[[Option<MotionState>; 16]],
    address: usize,
    macroblocks_wide: usize,
    direct_context: BDirectContext,
) -> BInterDecision {
    select_b_inter_partitions_with_analysis(
        source,
        previous,
        future,
        coded_width,
        coded_height,
        macroblock_x,
        macroblock_y,
        search_range,
        AnalysisPreset::Full,
        history_l0,
        history_l1,
        address,
        macroblocks_wide,
        direct_context,
    )
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn select_b_inter_partitions_with_analysis(
    source: &[Vec<u8>; 3],
    previous: &EncoderReference,
    future: &EncoderReference,
    coded_width: usize,
    coded_height: usize,
    macroblock_x: usize,
    macroblock_y: usize,
    search_range: i32,
    analysis: AnalysisPreset,
    history_l0: &[[Option<MotionState>; 16]],
    history_l1: &[[Option<MotionState>; 16]],
    address: usize,
    macroblocks_wide: usize,
    direct_context: BDirectContext,
) -> BInterDecision {
    let direct = match direct_context.mode {
        BDirectMode::Spatial => Some(select_spatial_direct_b_prediction(
            source,
            previous,
            future,
            coded_width,
            coded_height,
            macroblock_x,
            macroblock_y,
            history_l0,
            history_l1,
            address,
            macroblocks_wide,
        )),
        BDirectMode::Temporal => select_temporal_direct_b_prediction(
            source,
            previous,
            future,
            coded_width,
            coded_height,
            macroblock_x,
            macroblock_y,
            address,
            direct_context.picture_order_count,
        ),
    };
    if analysis == AnalysisPreset::Fast && direct.as_ref().is_some_and(|(cost, _)| *cost <= 4_096) {
        return direct.expect("low-error fast direct candidate exists").1;
    }
    if analysis == AnalysisPreset::Fast {
        let mut decisions = (1_u8..=3)
            .map(|macroblock_type| {
                select_fast_b16_candidate(
                    source,
                    previous,
                    future,
                    coded_width,
                    coded_height,
                    macroblock_x,
                    macroblock_y,
                    search_range,
                    history_l0,
                    history_l1,
                    address,
                    macroblocks_wide,
                    macroblock_type,
                )
            })
            .collect::<Vec<_>>();
        decisions.extend(direct);
        return decisions
            .into_iter()
            .min_by_key(|(cost, decision)| (*cost, decision.macroblock_type))
            .map(|(_, decision)| decision)
            .expect("fast B inter prediction candidates exist");
    }
    let mut decisions = (1_u8..=21)
        .map(|macroblock_type| {
            let mut trial_l0 = history_l0.to_vec();
            let mut trial_l1 = history_l1.to_vec();
            let mut prediction = [vec![0; 256], vec![0; 64], vec![0; 64]];
            let mut selected = Vec::new();
            let mut motion_rate = 0_u64;
            let mut bidirectional_count = 0_u64;
            for (partition, direction) in b_inter_partitions(macroblock_type) {
                let (selection, rate) = select_and_place_b_partition(
                    source,
                    previous,
                    future,
                    coded_width,
                    coded_height,
                    macroblock_x,
                    macroblock_y,
                    search_range,
                    analysis,
                    &mut trial_l0,
                    &mut trial_l1,
                    address,
                    macroblocks_wide,
                    &mut prediction,
                    direction,
                    partition,
                );
                motion_rate += rate;
                bidirectional_count += u64::from(matches!(direction, BPrediction::Bi));
                selected.push(selection);
            }
            let prediction_sad = block_sad(
                &source[0],
                coded_width,
                macroblock_x * 16,
                macroblock_y * 16,
                16,
                &prediction[0],
            );
            let split_penalty =
                u64::try_from(selected.len() - 1).expect("B partition count fits u64") * 16;
            let cost = prediction_sad + motion_rate * 2 + split_penalty + bidirectional_count * 4;
            (
                cost,
                BInterDecision {
                    macroblock_type,
                    direct: false,
                    sub_macroblock_types: [0; 4],
                    partitions: selected,
                    luma_prediction: prediction[0].clone(),
                    chroma_predictions: [prediction[1].clone(), prediction[2].clone()],
                },
            )
        })
        .collect::<Vec<_>>();
    decisions.push(select_b8x8_partitions(
        source,
        previous,
        future,
        coded_width,
        coded_height,
        macroblock_x,
        macroblock_y,
        search_range,
        history_l0,
        history_l1,
        address,
        macroblocks_wide,
        direct_context,
    ));
    decisions.extend(direct);
    decisions
        .into_iter()
        .min_by_key(|(cost, decision)| (*cost, decision.macroblock_type))
        .map(|(_, decision)| decision)
        .expect("B inter prediction candidates exist")
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn select_fast_b16_candidate(
    source: &[Vec<u8>; 3],
    previous: &EncoderReference,
    future: &EncoderReference,
    coded_width: usize,
    coded_height: usize,
    macroblock_x: usize,
    macroblock_y: usize,
    search_range: i32,
    history_l0: &[[Option<MotionState>; 16]],
    history_l1: &[[Option<MotionState>; 16]],
    address: usize,
    macroblocks_wide: usize,
    macroblock_type: u8,
) -> (u64, BInterDecision) {
    let direction = match macroblock_type {
        1 => BPrediction::L0,
        2 => BPrediction::L1,
        3 => BPrediction::Bi,
        _ => unreachable!("fast B16 macroblock type is in 1..=3"),
    };
    let partition = InterPartition {
        block_x: 0,
        block_y: 0,
        block_width: 4,
        block_height: 4,
        prediction_kind: MotionPredictionKind::Normal,
    };
    let list0 = direction.uses_l0().then(|| {
        select_b_partition_motion(
            source,
            &previous.planes,
            coded_width,
            coded_height,
            macroblock_x,
            macroblock_y,
            search_range,
            AnalysisPreset::Fast,
            history_l0,
            address,
            macroblocks_wide,
            partition,
        )
    });
    let list1 = direction.uses_l1().then(|| {
        select_b_partition_motion(
            source,
            &future.planes,
            coded_width,
            coded_height,
            macroblock_x,
            macroblock_y,
            search_range,
            AnalysisPreset::Fast,
            history_l1,
            address,
            macroblocks_wide,
            partition,
        )
    });
    let prediction_l0 = list0.map(|selected| {
        inter_prediction_for_partition(
            &previous.planes,
            coded_width,
            coded_height,
            macroblock_x,
            macroblock_y,
            partition,
            selected.motion,
        )
    });
    let prediction_l1 = list1.map(|selected| {
        inter_prediction_for_partition(
            &future.planes,
            coded_width,
            coded_height,
            macroblock_x,
            macroblock_y,
            partition,
            selected.motion,
        )
    });
    let mut prediction = [vec![0; 256], vec![0; 64], vec![0; 64]];
    place_b_partition_prediction(
        &mut prediction,
        prediction_l0.as_ref(),
        prediction_l1.as_ref(),
        partition,
    );
    let motion_rate = [list0, list1]
        .into_iter()
        .flatten()
        .map(|selected| motion_difference_rate(selected.motion, selected.predicted))
        .sum::<u64>();
    let cost = block_sad(
        &source[0],
        coded_width,
        macroblock_x * 16,
        macroblock_y * 16,
        16,
        &prediction[0],
    ) + motion_rate * 2
        + u64::from(matches!(direction, BPrediction::Bi)) * 4;
    (
        cost,
        BInterDecision {
            macroblock_type,
            direct: false,
            sub_macroblock_types: [0; 4],
            partitions: vec![BSelectedPartition {
                list0,
                list1,
                direct: false,
            }],
            luma_prediction: prediction[0].clone(),
            chroma_predictions: [prediction[1].clone(), prediction[2].clone()],
        },
    )
}

#[allow(clippy::too_many_arguments)]
fn select_spatial_direct_b_prediction(
    source: &[Vec<u8>; 3],
    previous: &EncoderReference,
    future: &EncoderReference,
    coded_width: usize,
    coded_height: usize,
    macroblock_x: usize,
    macroblock_y: usize,
    history_l0: &[[Option<MotionState>; 16]],
    history_l1: &[[Option<MotionState>; 16]],
    address: usize,
    macroblocks_wide: usize,
) -> (u64, BInterDecision) {
    let whole = InterPartition {
        block_x: 0,
        block_y: 0,
        block_width: 4,
        block_height: 4,
        prediction_kind: MotionPredictionKind::Normal,
    };
    let spatial_motion =
        spatial_direct_motion(history_l0, history_l1, address, macroblocks_wide, whole);
    let mut prediction = [vec![0; 256], vec![0; 64], vec![0; 64]];
    let mut partitions = Vec::with_capacity(4);
    for sub_index in 0..4 {
        let partition = direct_8x8_partition(sub_index);
        let selected = spatial_direct_partition(spatial_motion, future, address, partition);
        place_selected_b_prediction(
            &mut prediction,
            previous,
            future,
            coded_width,
            coded_height,
            macroblock_x,
            macroblock_y,
            selected,
        );
        partitions.push(selected);
    }
    let cost = block_sad(
        &source[0],
        coded_width,
        macroblock_x * 16,
        macroblock_y * 16,
        16,
        &prediction[0],
    );
    (
        cost,
        BInterDecision {
            macroblock_type: 0,
            direct: true,
            sub_macroblock_types: [0; 4],
            partitions,
            luma_prediction: prediction[0].clone(),
            chroma_predictions: [prediction[1].clone(), prediction[2].clone()],
        },
    )
}

#[allow(clippy::too_many_arguments)]
fn select_temporal_direct_b_prediction(
    source: &[Vec<u8>; 3],
    previous: &EncoderReference,
    future: &EncoderReference,
    coded_width: usize,
    coded_height: usize,
    macroblock_x: usize,
    macroblock_y: usize,
    address: usize,
    picture_order_count: i32,
) -> Option<(u64, BInterDecision)> {
    let mut prediction = [vec![0; 256], vec![0; 64], vec![0; 64]];
    let mut partitions = Vec::with_capacity(4);
    for sub_index in 0..4 {
        let partition = direct_8x8_partition(sub_index);
        let selected =
            temporal_direct_partition(previous, future, address, partition, picture_order_count)?;
        place_selected_b_prediction(
            &mut prediction,
            previous,
            future,
            coded_width,
            coded_height,
            macroblock_x,
            macroblock_y,
            selected,
        );
        partitions.push(selected);
    }
    let cost = block_sad(
        &source[0],
        coded_width,
        macroblock_x * 16,
        macroblock_y * 16,
        16,
        &prediction[0],
    );
    Some((
        cost,
        BInterDecision {
            macroblock_type: 0,
            direct: true,
            sub_macroblock_types: [0; 4],
            partitions,
            luma_prediction: prediction[0].clone(),
            chroma_predictions: [prediction[1].clone(), prediction[2].clone()],
        },
    ))
}

fn temporal_direct_partition(
    previous: &EncoderReference,
    future: &EncoderReference,
    address: usize,
    partition: InterPartition,
    picture_order_count: i32,
) -> Option<BSelectedPartition> {
    let (colocated_x, colocated_y) = direct_colocated_block(partition);
    let (motion_l0, motion_l1) = match future.colocated_motion(address, colocated_x, colocated_y) {
        Some((colocated, reference_poc)) => {
            if reference_poc != previous.pic_order_count {
                return None;
            }
            temporal_direct_motion_vectors(
                colocated,
                picture_order_count,
                future.pic_order_count,
                reference_poc,
            )
        }
        None => (MotionVector::default(), MotionVector::default()),
    };
    let selected = |motion| {
        Some(SelectedPartition {
            partition,
            predicted: motion,
            motion,
            reference_index: 0,
        })
    };
    Some(BSelectedPartition {
        list0: selected(motion_l0),
        list1: selected(motion_l1),
        direct: true,
    })
}

fn temporal_direct_motion_vectors(
    colocated: MotionVector,
    picture_order_count: i32,
    colocated_picture_order_count: i32,
    reference_picture_order_count: i32,
) -> (MotionVector, MotionVector) {
    let td = (i64::from(colocated_picture_order_count) - i64::from(reference_picture_order_count))
        .clamp(-128, 127);
    let tb = (i64::from(picture_order_count) - i64::from(reference_picture_order_count))
        .clamp(-128, 127);
    let scale = if td == 0 {
        256
    } else {
        let tx = (16_384 + td.abs() / 2) / td;
        ((tb * tx + 32) >> 6).clamp(-1_024, 1_023)
    };
    let scale_component = |component: i32| {
        i32::try_from((scale * i64::from(component) + 128) >> 8)
            .expect("bounded temporal-direct motion fits i32")
    };
    let list0 = MotionVector {
        x: scale_component(colocated.x),
        y: scale_component(colocated.y),
    };
    let list1 = MotionVector {
        x: list0.x - colocated.x,
        y: list0.y - colocated.y,
    };
    (list0, list1)
}

type EncoderSpatialDirectMotion = (Option<(u8, MotionVector)>, Option<(u8, MotionVector)>);

fn spatial_direct_motion(
    history_l0: &[[Option<MotionState>; 16]],
    history_l1: &[[Option<MotionState>; 16]],
    address: usize,
    macroblocks_wide: usize,
    partition: InterPartition,
) -> EncoderSpatialDirectMotion {
    let mut index_l0 = spatial_direct_reference_index(history_l0, address, macroblocks_wide);
    let mut index_l1 = spatial_direct_reference_index(history_l1, address, macroblocks_wide);
    if index_l0.is_none() && index_l1.is_none() {
        index_l0 = Some(0);
        index_l1 = Some(0);
    }
    let motion_l0 = index_l0.map(|reference_index| {
        (
            reference_index,
            partition_motion_predictor(
                history_l0,
                address,
                macroblocks_wide,
                partition,
                reference_index,
            ),
        )
    });
    let motion_l1 = index_l1.map(|reference_index| {
        (
            reference_index,
            partition_motion_predictor(
                history_l1,
                address,
                macroblocks_wide,
                partition,
                reference_index,
            ),
        )
    });
    (motion_l0, motion_l1)
}

fn spatial_direct_partition(
    spatial_motion: EncoderSpatialDirectMotion,
    future: &EncoderReference,
    address: usize,
    partition: InterPartition,
) -> BSelectedPartition {
    let (colocated_x, colocated_y) = direct_colocated_block(partition);
    let colocated_zero = future.colocated_zero(address, colocated_x, colocated_y);
    let selected = |motion: Option<(u8, MotionVector)>| {
        motion.map(|(reference_index, predicted)| SelectedPartition {
            partition,
            predicted,
            motion: if colocated_zero && reference_index == 0 {
                MotionVector::default()
            } else {
                predicted
            },
            reference_index,
        })
    };
    BSelectedPartition {
        list0: selected(spatial_motion.0),
        list1: selected(spatial_motion.1),
        direct: true,
    }
}

const fn direct_8x8_partition(sub_index: usize) -> InterPartition {
    InterPartition {
        block_x: (sub_index % 2) * 2,
        block_y: (sub_index / 2) * 2,
        block_width: 2,
        block_height: 2,
        prediction_kind: MotionPredictionKind::Normal,
    }
}

fn direct_colocated_block(partition: InterPartition) -> (usize, usize) {
    let representative =
        |coordinate: usize, size: usize| coordinate + usize::from(size == 2 && coordinate != 0);
    (
        representative(partition.block_x, partition.block_width),
        representative(partition.block_y, partition.block_height),
    )
}

fn spatial_direct_reference_index(
    history: &[[Option<MotionState>; 16]],
    address: usize,
    macroblocks_wide: usize,
) -> Option<u8> {
    let macroblock_x = address % macroblocks_wide;
    let macroblock_y = address / macroblocks_wide;
    let block_x = (macroblock_x * 4).cast_signed();
    let block_y = (macroblock_y * 4).cast_signed();
    [
        motion_at(history, macroblocks_wide, block_x - 1, block_y),
        motion_at(history, macroblocks_wide, block_x, block_y - 1),
        motion_at(history, macroblocks_wide, block_x + 4, block_y - 1),
    ]
    .into_iter()
    .flatten()
    .filter_map(|motion| motion.reference_index)
    .min()
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn select_b8x8_partitions(
    source: &[Vec<u8>; 3],
    previous: &EncoderReference,
    future: &EncoderReference,
    coded_width: usize,
    coded_height: usize,
    macroblock_x: usize,
    macroblock_y: usize,
    search_range: i32,
    history_l0: &[[Option<MotionState>; 16]],
    history_l1: &[[Option<MotionState>; 16]],
    address: usize,
    macroblocks_wide: usize,
    direct_context: BDirectContext,
) -> (u64, BInterDecision) {
    let mut candidate_l0 = history_l0.to_vec();
    let mut candidate_l1 = history_l1.to_vec();
    let mut prediction = [vec![0; 256], vec![0; 64], vec![0; 64]];
    let mut sub_macroblock_types = [1_u8; 4];
    let mut selected_partitions = Vec::new();
    let mut total_cost = 32_u64;
    let spatial_motion = (direct_context.mode == BDirectMode::Spatial).then(|| {
        spatial_direct_motion(
            history_l0,
            history_l1,
            address,
            macroblocks_wide,
            InterPartition {
                block_x: 0,
                block_y: 0,
                block_width: 4,
                block_height: 4,
                prediction_kind: MotionPredictionKind::Normal,
            },
        )
    });
    for (sub_index, selected_sub_type) in sub_macroblock_types.iter_mut().enumerate() {
        let direct_partition = direct_8x8_partition(sub_index);
        let sub_x = (sub_index % 2) * 8;
        let sub_y = (sub_index / 2) * 8;
        let direct = match direct_context.mode {
            BDirectMode::Spatial => Some(spatial_direct_partition(
                spatial_motion.expect("spatial direct motion was derived"),
                future,
                address,
                direct_partition,
            )),
            BDirectMode::Temporal => temporal_direct_partition(
                previous,
                future,
                address,
                direct_partition,
                direct_context.picture_order_count,
            ),
        };
        let mut best = direct.map(|direct| {
            let mut direct_l0 = candidate_l0.clone();
            let mut direct_l1 = candidate_l1.clone();
            set_partition_motion(
                &mut direct_l0[address],
                direct_partition,
                b_motion_state(direct.list0),
            );
            set_partition_motion(
                &mut direct_l1[address],
                direct_partition,
                b_motion_state(direct.list1),
            );
            let mut direct_prediction = prediction.clone();
            place_selected_b_prediction(
                &mut direct_prediction,
                previous,
                future,
                coded_width,
                coded_height,
                macroblock_x,
                macroblock_y,
                direct,
            );
            let direct_cost = prediction_region_sad(
                &source[0],
                coded_width,
                macroblock_x * 16 + sub_x,
                macroblock_y * 16 + sub_y,
                &direct_prediction[0],
                16,
                sub_x,
                sub_y,
                8,
                8,
            );
            B8x8Candidate {
                cost: direct_cost,
                sub_type: 0,
                history_l0: direct_l0,
                history_l1: direct_l1,
                prediction: direct_prediction,
                partitions: vec![direct],
            }
        });
        for sub_type in 1_u8..=12 {
            let mut trial_l0 = candidate_l0.clone();
            let mut trial_l1 = candidate_l1.clone();
            let mut trial_prediction = prediction.clone();
            let mut trial_partitions = Vec::new();
            let mut motion_rate = 0_u64;
            let mut bidirectional_count = 0_u64;
            for (partition, direction) in b_sub_partitions(sub_index, sub_type) {
                let (selection, rate) = select_and_place_b_partition(
                    source,
                    previous,
                    future,
                    coded_width,
                    coded_height,
                    macroblock_x,
                    macroblock_y,
                    search_range,
                    AnalysisPreset::Full,
                    &mut trial_l0,
                    &mut trial_l1,
                    address,
                    macroblocks_wide,
                    &mut trial_prediction,
                    direction,
                    partition,
                );
                motion_rate += rate;
                bidirectional_count += u64::from(matches!(direction, BPrediction::Bi));
                trial_partitions.push(selection);
            }
            let prediction_sad = prediction_region_sad(
                &source[0],
                coded_width,
                macroblock_x * 16 + sub_x,
                macroblock_y * 16 + sub_y,
                &trial_prediction[0],
                16,
                sub_x,
                sub_y,
                8,
                8,
            );
            let partition_penalty = u64::try_from(trial_partitions.len() - 1)
                .expect("B subpartition count fits u64")
                * 8;
            let cost =
                prediction_sad + motion_rate * 2 + partition_penalty + bidirectional_count * 4;
            if best
                .as_ref()
                .is_none_or(|candidate| (cost, sub_type) < (candidate.cost, candidate.sub_type))
            {
                best = Some(B8x8Candidate {
                    cost,
                    sub_type,
                    history_l0: trial_l0,
                    history_l1: trial_l1,
                    prediction: trial_prediction,
                    partitions: trial_partitions,
                });
            }
        }
        let best = best.expect("each B8x8 sub-macroblock has prediction candidates");
        total_cost += best.cost;
        *selected_sub_type = best.sub_type;
        candidate_l0 = best.history_l0;
        candidate_l1 = best.history_l1;
        prediction = best.prediction;
        selected_partitions.extend(best.partitions);
    }
    (
        total_cost,
        BInterDecision {
            macroblock_type: 22,
            direct: false,
            sub_macroblock_types,
            partitions: selected_partitions,
            luma_prediction: prediction[0].clone(),
            chroma_predictions: [prediction[1].clone(), prediction[2].clone()],
        },
    )
}

#[allow(clippy::too_many_arguments)]
fn select_and_place_b_partition(
    source: &[Vec<u8>; 3],
    previous: &EncoderReference,
    future: &EncoderReference,
    coded_width: usize,
    coded_height: usize,
    macroblock_x: usize,
    macroblock_y: usize,
    search_range: i32,
    analysis: AnalysisPreset,
    history_l0: &mut [[Option<MotionState>; 16]],
    history_l1: &mut [[Option<MotionState>; 16]],
    address: usize,
    macroblocks_wide: usize,
    prediction: &mut [Vec<u8>; 3],
    direction: BPrediction,
    partition: InterPartition,
) -> (BSelectedPartition, u64) {
    let list0 = direction.uses_l0().then(|| {
        select_b_partition_motion(
            source,
            &previous.planes,
            coded_width,
            coded_height,
            macroblock_x,
            macroblock_y,
            search_range,
            analysis,
            history_l0,
            address,
            macroblocks_wide,
            partition,
        )
    });
    let list1 = direction.uses_l1().then(|| {
        select_b_partition_motion(
            source,
            &future.planes,
            coded_width,
            coded_height,
            macroblock_x,
            macroblock_y,
            search_range,
            analysis,
            history_l1,
            address,
            macroblocks_wide,
            partition,
        )
    });
    let prediction_l0 = list0.map(|selected| {
        inter_prediction_for_partition(
            &previous.planes,
            coded_width,
            coded_height,
            macroblock_x,
            macroblock_y,
            partition,
            selected.motion,
        )
    });
    let prediction_l1 = list1.map(|selected| {
        inter_prediction_for_partition(
            &future.planes,
            coded_width,
            coded_height,
            macroblock_x,
            macroblock_y,
            partition,
            selected.motion,
        )
    });
    place_b_partition_prediction(
        prediction,
        prediction_l0.as_ref(),
        prediction_l1.as_ref(),
        partition,
    );
    let mut motion_rate = 0;
    for (selection, history) in [(list0, history_l0), (list1, history_l1)] {
        if let Some(selection) = selection {
            motion_rate += motion_difference_rate(selection.motion, selection.predicted);
        }
        set_partition_motion(&mut history[address], partition, b_motion_state(selection));
    }
    (
        BSelectedPartition {
            list0,
            list1,
            direct: false,
        },
        motion_rate,
    )
}

#[allow(clippy::too_many_arguments)]
fn select_b_partition_motion(
    source: &[Vec<u8>; 3],
    reference: &[Vec<u8>; 3],
    coded_width: usize,
    coded_height: usize,
    macroblock_x: usize,
    macroblock_y: usize,
    search_range: i32,
    analysis: AnalysisPreset,
    history: &[[Option<MotionState>; 16]],
    address: usize,
    macroblocks_wide: usize,
    partition: InterPartition,
) -> SelectedPartition {
    let predicted = partition_motion_predictor(history, address, macroblocks_wide, partition, 0);
    let motion = estimate_partition_motion(
        &source[0],
        &reference[0],
        coded_width,
        coded_height,
        macroblock_x * 16 + partition.block_x * 4,
        macroblock_y * 16 + partition.block_y * 4,
        partition.block_width * 4,
        partition.block_height * 4,
        search_range,
        predicted,
        analysis,
    );
    SelectedPartition {
        partition,
        predicted,
        motion,
        reference_index: 0,
    }
}

fn b_inter_partitions(macroblock_type: u8) -> Vec<(InterPartition, BPrediction)> {
    let single = match macroblock_type {
        1 => Some(BPrediction::L0),
        2 => Some(BPrediction::L1),
        3 => Some(BPrediction::Bi),
        _ => None,
    };
    if let Some(prediction) = single {
        return vec![(
            InterPartition {
                block_x: 0,
                block_y: 0,
                block_width: 4,
                block_height: 4,
                prediction_kind: MotionPredictionKind::Normal,
            },
            prediction,
        )];
    }
    let combinations = [
        [BPrediction::L0, BPrediction::L0],
        [BPrediction::L1, BPrediction::L1],
        [BPrediction::L0, BPrediction::L1],
        [BPrediction::L1, BPrediction::L0],
        [BPrediction::L0, BPrediction::Bi],
        [BPrediction::L1, BPrediction::Bi],
        [BPrediction::Bi, BPrediction::L0],
        [BPrediction::Bi, BPrediction::L1],
        [BPrediction::Bi, BPrediction::Bi],
    ];
    let offset = usize::from(macroblock_type - 4);
    let predictions = combinations[offset / 2];
    if offset.is_multiple_of(2) {
        vec![
            (
                InterPartition {
                    block_x: 0,
                    block_y: 0,
                    block_width: 4,
                    block_height: 2,
                    prediction_kind: MotionPredictionKind::Top16x8,
                },
                predictions[0],
            ),
            (
                InterPartition {
                    block_x: 0,
                    block_y: 2,
                    block_width: 4,
                    block_height: 2,
                    prediction_kind: MotionPredictionKind::Bottom16x8,
                },
                predictions[1],
            ),
        ]
    } else {
        vec![
            (
                InterPartition {
                    block_x: 0,
                    block_y: 0,
                    block_width: 2,
                    block_height: 4,
                    prediction_kind: MotionPredictionKind::Left8x16,
                },
                predictions[0],
            ),
            (
                InterPartition {
                    block_x: 2,
                    block_y: 0,
                    block_width: 2,
                    block_height: 4,
                    prediction_kind: MotionPredictionKind::Right8x16,
                },
                predictions[1],
            ),
        ]
    }
}

fn b_sub_partitions(sub_index: usize, sub_type: u8) -> Vec<(InterPartition, BPrediction)> {
    let (direction, shape) = match sub_type {
        1 => (BPrediction::L0, 0),
        2 => (BPrediction::L1, 0),
        3 => (BPrediction::Bi, 0),
        4 => (BPrediction::L0, 1),
        5 => (BPrediction::L0, 2),
        6 => (BPrediction::L1, 1),
        7 => (BPrediction::L1, 2),
        8 => (BPrediction::Bi, 1),
        9 => (BPrediction::Bi, 2),
        10 => (BPrediction::L0, 3),
        11 => (BPrediction::L1, 3),
        12 => (BPrediction::Bi, 3),
        _ => unreachable!("non-direct B sub-macroblock type is in 1..=12"),
    };
    let base_x = (sub_index % 2) * 2;
    let base_y = (sub_index / 2) * 2;
    let partition = |block_x, block_y, block_width, block_height| {
        (
            InterPartition {
                block_x,
                block_y,
                block_width,
                block_height,
                prediction_kind: MotionPredictionKind::Normal,
            },
            direction,
        )
    };
    match shape {
        0 => vec![partition(base_x, base_y, 2, 2)],
        1 => vec![
            partition(base_x, base_y, 2, 1),
            partition(base_x, base_y + 1, 2, 1),
        ],
        2 => vec![
            partition(base_x, base_y, 1, 2),
            partition(base_x + 1, base_y, 1, 2),
        ],
        3 => (0..2)
            .flat_map(|y| (0..2).map(move |x| partition(base_x + x, base_y + y, 1, 1)))
            .collect(),
        _ => unreachable!("validated B sub-macroblock shape"),
    }
}

#[allow(clippy::too_many_arguments)]
fn inter_prediction_for_partition(
    reference: &[Vec<u8>; 3],
    coded_width: usize,
    coded_height: usize,
    macroblock_x: usize,
    macroblock_y: usize,
    partition: InterPartition,
    motion: MotionVector,
) -> [Vec<u8>; 3] {
    let mut prediction = [vec![0; 256], vec![0; 64], vec![0; 64]];
    predict_inter_luma_partition(
        &mut prediction[0],
        &reference[0],
        coded_width,
        coded_height,
        macroblock_x,
        macroblock_y,
        partition,
        motion,
    );
    for component in 0..2 {
        predict_inter_chroma_partition(
            &mut prediction[component + 1],
            &reference[component + 1],
            coded_width / 2,
            coded_height / 2,
            macroblock_x,
            macroblock_y,
            partition,
            motion,
        );
    }
    prediction
}

fn place_b_partition_prediction(
    destination: &mut [Vec<u8>; 3],
    list0: Option<&[Vec<u8>; 3]>,
    list1: Option<&[Vec<u8>; 3]>,
    partition: InterPartition,
) {
    debug_assert!(list0.is_some() || list1.is_some());
    for component in 0..3 {
        let block_scale = if component == 0 { 4 } else { 2 };
        let stride = if component == 0 { 16 } else { 8 };
        let origin_x = partition.block_x * block_scale;
        let origin_y = partition.block_y * block_scale;
        let width = partition.block_width * block_scale;
        let height = partition.block_height * block_scale;
        for y in origin_y..origin_y + height {
            for x in origin_x..origin_x + width {
                let index = y * stride + x;
                destination[component][index] = match (list0, list1) {
                    (Some(list0), Some(list1)) => u8::try_from(
                        (u16::from(list0[component][index])
                            + u16::from(list1[component][index])
                            + 1)
                            >> 1,
                    )
                    .expect("average of two u8 samples fits u8"),
                    (Some(list0), None) => list0[component][index],
                    (None, Some(list1)) => list1[component][index],
                    (None, None) => unreachable!("B partition uses at least one reference list"),
                };
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn place_selected_b_prediction(
    destination: &mut [Vec<u8>; 3],
    previous: &EncoderReference,
    future: &EncoderReference,
    coded_width: usize,
    coded_height: usize,
    macroblock_x: usize,
    macroblock_y: usize,
    selected: BSelectedPartition,
) {
    let partition = selected.partition();
    let prediction_l0 = selected.list0.map(|selected| {
        inter_prediction_for_partition(
            &previous.planes,
            coded_width,
            coded_height,
            macroblock_x,
            macroblock_y,
            partition,
            selected.motion,
        )
    });
    let prediction_l1 = selected.list1.map(|selected| {
        inter_prediction_for_partition(
            &future.planes,
            coded_width,
            coded_height,
            macroblock_x,
            macroblock_y,
            partition,
            selected.motion,
        )
    });
    place_b_partition_prediction(
        destination,
        prediction_l0.as_ref(),
        prediction_l1.as_ref(),
        partition,
    );
}

fn motion_difference_rate(motion: MotionVector, predicted: MotionVector) -> u64 {
    u64::from((motion.x - predicted.x).unsigned_abs())
        + u64::from((motion.y - predicted.y).unsigned_abs())
}

fn b_motion_state(selected: Option<SelectedPartition>) -> MotionState {
    selected.map_or(
        MotionState {
            vector: MotionVector::default(),
            reference_index: None,
        },
        |selected| MotionState {
            vector: selected.motion,
            reference_index: Some(0),
        },
    )
}

fn commit_b_motion_history(
    decision: &BInterDecision,
    history_l0: &mut [Option<MotionState>; 16],
    history_l1: &mut [Option<MotionState>; 16],
) {
    for selected in &decision.partitions {
        let partition = selected.partition();
        set_partition_motion(history_l0, partition, b_motion_state(selected.list0));
        set_partition_motion(history_l1, partition, b_motion_state(selected.list1));
    }
}

fn write_motion_difference(writer: &mut BitWriter, selected: SelectedPartition) -> Result<()> {
    write_se(writer, i64::from(selected.motion.x - selected.predicted.x))?;
    write_se(writer, i64::from(selected.motion.y - selected.predicted.y))
}

#[allow(clippy::too_many_arguments)]
fn encode_inter_residuals(
    writer: &mut BitWriter,
    luma_levels: &InterLumaResidual,
    chroma_residuals: &[ChromaResidual; 2],
    luma_pattern: u8,
    chroma_pattern: u8,
    luma_nonzero: &mut [[u8; 16]],
    chroma_nonzero: &mut [[[u8; 4]; 2]],
    address: usize,
    macroblocks_wide: usize,
) -> Result<()> {
    match luma_levels {
        InterLumaResidual::FourByFour(levels) | InterLumaResidual::BypassFourByFour(levels) => {
            for group in 0..4 {
                if luma_pattern & (1 << group) != 0 {
                    for block_index in group * 4..group * 4 + 4 {
                        let n_c = luma_nc(luma_nonzero, address, block_index, macroblocks_wide);
                        luma_nonzero[address][block_index] =
                            encode_residual_block(writer, n_c, &levels[block_index])?;
                    }
                }
            }
        }
        InterLumaResidual::EightByEight(levels) => {
            for (group, group_levels) in levels.iter().enumerate() {
                if luma_pattern & (1 << group) != 0 {
                    for sub_block in 0..4 {
                        let block_index = group * 4 + sub_block;
                        let block_levels: [i32; 16] = std::array::from_fn(|coefficient| {
                            group_levels[coefficient * 4 + sub_block]
                        });
                        let n_c = luma_nc(luma_nonzero, address, block_index, macroblocks_wide);
                        luma_nonzero[address][block_index] =
                            encode_residual_block(writer, n_c, &block_levels)?;
                    }
                }
            }
        }
    }
    if chroma_pattern != 0 {
        for residual in chroma_residuals {
            encode_residual_block(writer, -1, &residual.dc)?;
        }
    }
    if chroma_pattern == 2 {
        for (component, residual) in chroma_residuals.iter().enumerate() {
            for (block_index, levels) in residual.ac.iter().enumerate() {
                let n_c = chroma_nc(
                    chroma_nonzero,
                    address,
                    component,
                    block_index,
                    macroblocks_wide,
                );
                chroma_nonzero[address][component][block_index] =
                    encode_residual_block(writer, n_c, levels)?;
            }
        }
    }
    Ok(())
}

const INTER_CODED_BLOCK_PATTERN: [u8; 48] = [
    0, 16, 1, 2, 4, 8, 32, 3, 5, 10, 12, 15, 47, 7, 11, 13, 14, 6, 9, 31, 35, 37, 42, 44, 33, 34,
    36, 40, 39, 43, 45, 46, 17, 18, 20, 24, 19, 21, 26, 28, 23, 27, 29, 30, 22, 25, 38, 41,
];

#[cfg(test)]
#[allow(clippy::too_many_arguments)]
fn select_inter_partitions(
    source: &[Vec<u8>; 3],
    references: &VecDeque<EncoderReference>,
    coded_width: usize,
    coded_height: usize,
    macroblock_x: usize,
    macroblock_y: usize,
    search_range: i32,
    motions: &[[Option<MotionState>; 16]],
    address: usize,
    macroblocks_wide: usize,
) -> InterDecision {
    select_inter_partitions_with_analysis(
        source,
        references,
        coded_width,
        coded_height,
        macroblock_x,
        macroblock_y,
        search_range,
        AnalysisPreset::Full,
        motions,
        address,
        macroblocks_wide,
    )
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn select_inter_partitions_with_analysis(
    source: &[Vec<u8>; 3],
    references: &VecDeque<EncoderReference>,
    coded_width: usize,
    coded_height: usize,
    macroblock_x: usize,
    macroblock_y: usize,
    search_range: i32,
    analysis: AnalysisPreset,
    motions: &[[Option<MotionState>; 16]],
    address: usize,
    macroblocks_wide: usize,
) -> InterDecision {
    references
        .iter()
        .enumerate()
        .take(if analysis == AnalysisPreset::Fast {
            1
        } else {
            references.len()
        })
        .map(|(reference_index, reference)| {
            let reference_index =
                u8::try_from(reference_index).expect("configured H.264 reference index fits u8");
            let (cost, decision) = select_inter_partitions_for_reference(
                source,
                &reference.planes,
                reference_index,
                coded_width,
                coded_height,
                macroblock_x,
                macroblock_y,
                search_range,
                analysis,
                motions,
                address,
                macroblocks_wide,
            );
            (cost + u64::from(reference_index) * 4, decision)
        })
        .min_by_key(|(cost, decision)| {
            (
                *cost,
                decision.partitions[0].reference_index,
                decision.macroblock_type,
            )
        })
        .map(|(_, decision)| decision)
        .expect("P picture has an active reference")
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn select_inter_partitions_for_reference(
    source: &[Vec<u8>; 3],
    reference: &[Vec<u8>; 3],
    reference_index: u8,
    coded_width: usize,
    coded_height: usize,
    macroblock_x: usize,
    macroblock_y: usize,
    search_range: i32,
    analysis: AnalysisPreset,
    motions: &[[Option<MotionState>; 16]],
    address: usize,
    macroblocks_wide: usize,
) -> (u64, InterDecision) {
    const FAST_SPLIT_SAD_THRESHOLD: u64 = 4_096;
    let fast_whole = if analysis == AnalysisPreset::Fast {
        let partition = InterPartition {
            block_x: 0,
            block_y: 0,
            block_width: 4,
            block_height: 4,
            prediction_kind: MotionPredictionKind::Normal,
        };
        let predicted = partition_motion_predictor(
            motions,
            address,
            macroblocks_wide,
            partition,
            reference_index,
        );
        let motion = estimate_partition_motion(
            &source[0],
            &reference[0],
            coded_width,
            coded_height,
            macroblock_x * 16,
            macroblock_y * 16,
            16,
            16,
            search_range,
            predicted,
            analysis,
        );
        let mut luma_prediction = vec![0; 256];
        let mut chroma_predictions: [Vec<u8>; 2] = std::array::from_fn(|_| vec![0; 64]);
        predict_inter_luma_partition(
            &mut luma_prediction,
            &reference[0],
            coded_width,
            coded_height,
            macroblock_x,
            macroblock_y,
            partition,
            motion,
        );
        for component in 0..2 {
            predict_inter_chroma_partition(
                &mut chroma_predictions[component],
                &reference[component + 1],
                coded_width / 2,
                coded_height / 2,
                macroblock_x,
                macroblock_y,
                partition,
                motion,
            );
        }
        let cost = block_sad(
            &source[0],
            coded_width,
            macroblock_x * 16,
            macroblock_y * 16,
            16,
            &luma_prediction,
        ) + u64::from((motion.x - predicted.x).unsigned_abs()) * 2
            + u64::from((motion.y - predicted.y).unsigned_abs()) * 2;
        Some((
            cost,
            InterDecision {
                macroblock_type: 0,
                sub_macroblock_types: [0; 4],
                partitions: vec![SelectedPartition {
                    partition,
                    predicted,
                    motion,
                    reference_index,
                }],
                luma_prediction,
                chroma_predictions,
            },
        ))
    } else {
        None
    };
    if fast_whole
        .as_ref()
        .is_some_and(|(cost, _)| *cost <= FAST_SPLIT_SAD_THRESHOLD)
    {
        return fast_whole.expect("fast whole-macroblock candidate exists");
    }
    let candidates = [
        (
            0,
            vec![InterPartition {
                block_x: 0,
                block_y: 0,
                block_width: 4,
                block_height: 4,
                prediction_kind: MotionPredictionKind::Normal,
            }],
        ),
        (
            1,
            vec![
                InterPartition {
                    block_x: 0,
                    block_y: 0,
                    block_width: 4,
                    block_height: 2,
                    prediction_kind: MotionPredictionKind::Top16x8,
                },
                InterPartition {
                    block_x: 0,
                    block_y: 2,
                    block_width: 4,
                    block_height: 2,
                    prediction_kind: MotionPredictionKind::Bottom16x8,
                },
            ],
        ),
        (
            2,
            vec![
                InterPartition {
                    block_x: 0,
                    block_y: 0,
                    block_width: 2,
                    block_height: 4,
                    prediction_kind: MotionPredictionKind::Left8x16,
                },
                InterPartition {
                    block_x: 2,
                    block_y: 0,
                    block_width: 2,
                    block_height: 4,
                    prediction_kind: MotionPredictionKind::Right8x16,
                },
            ],
        ),
    ];
    let mut decisions = candidates
        .into_iter()
        .filter(|(macroblock_type, _)| analysis == AnalysisPreset::Full || *macroblock_type != 0)
        .map(|(macroblock_type, partitions)| {
            let mut candidate_motions = [None; 16];
            let mut selected = Vec::with_capacity(partitions.len());
            let mut luma_prediction = vec![0; 256];
            let mut chroma_predictions: [Vec<u8>; 2] = std::array::from_fn(|_| vec![0; 64]);
            let mut motion_rate = 0_u64;
            for partition in partitions {
                let predicted = partition_motion_predictor_with_current(
                    &candidate_motions,
                    motions,
                    address,
                    macroblocks_wide,
                    partition,
                    reference_index,
                );
                let motion = estimate_partition_motion(
                    &source[0],
                    &reference[0],
                    coded_width,
                    coded_height,
                    macroblock_x * 16 + partition.block_x * 4,
                    macroblock_y * 16 + partition.block_y * 4,
                    partition.block_width * 4,
                    partition.block_height * 4,
                    search_range,
                    predicted,
                    analysis,
                );
                motion_rate += u64::from((motion.x - predicted.x).unsigned_abs())
                    + u64::from((motion.y - predicted.y).unsigned_abs());
                predict_inter_luma_partition(
                    &mut luma_prediction,
                    &reference[0],
                    coded_width,
                    coded_height,
                    macroblock_x,
                    macroblock_y,
                    partition,
                    motion,
                );
                for component in 0..2 {
                    predict_inter_chroma_partition(
                        &mut chroma_predictions[component],
                        &reference[component + 1],
                        coded_width / 2,
                        coded_height / 2,
                        macroblock_x,
                        macroblock_y,
                        partition,
                        motion,
                    );
                }
                set_partition_motion(
                    &mut candidate_motions,
                    partition,
                    MotionState {
                        vector: motion,
                        reference_index: Some(reference_index),
                    },
                );
                selected.push(SelectedPartition {
                    partition,
                    predicted,
                    motion,
                    reference_index,
                });
            }
            let prediction_sad = block_sad(
                &source[0],
                coded_width,
                macroblock_x * 16,
                macroblock_y * 16,
                16,
                &luma_prediction,
            );
            let split_penalty =
                u64::try_from(selected.len() - 1).expect("partition count fits") * 16;
            let cost = prediction_sad + motion_rate * 2 + split_penalty;
            (
                cost,
                InterDecision {
                    macroblock_type,
                    sub_macroblock_types: [0; 4],
                    partitions: selected,
                    luma_prediction,
                    chroma_predictions,
                },
            )
        })
        .collect::<Vec<_>>();
    decisions.extend(fast_whole);
    decisions.push(select_p8x8_partitions(
        source,
        reference,
        reference_index,
        coded_width,
        coded_height,
        macroblock_x,
        macroblock_y,
        search_range,
        analysis,
        motions,
        address,
        macroblocks_wide,
    ));
    decisions
        .into_iter()
        .min_by_key(|(cost, decision)| (*cost, decision.macroblock_type))
        .expect("at least one inter partition mode exists")
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn select_p8x8_partitions(
    source: &[Vec<u8>; 3],
    reference: &[Vec<u8>; 3],
    reference_index: u8,
    coded_width: usize,
    coded_height: usize,
    macroblock_x: usize,
    macroblock_y: usize,
    search_range: i32,
    analysis: AnalysisPreset,
    motions: &[[Option<MotionState>; 16]],
    address: usize,
    macroblocks_wide: usize,
) -> (u64, InterDecision) {
    let mut candidate_motions = [None; 16];
    let mut luma_prediction = vec![0; 256];
    let mut chroma_predictions: [Vec<u8>; 2] = std::array::from_fn(|_| vec![0; 64]);
    let mut sub_macroblock_types = [0_u8; 4];
    let mut selected_partitions = Vec::new();
    let mut total_cost = 32_u64;
    for (sub_index, sub_macroblock_type) in sub_macroblock_types.iter_mut().enumerate() {
        let mut best: Option<P8x8Candidate> = None;
        for sub_type in 0..=3 {
            let mut trial_motions = candidate_motions;
            let mut trial_luma = luma_prediction.clone();
            let mut trial_chroma = chroma_predictions.clone();
            let mut trial_selected = Vec::new();
            let mut motion_rate = 0_u64;
            for partition in p_sub_partitions(sub_index, sub_type) {
                let predicted = partition_motion_predictor_with_current(
                    &trial_motions,
                    motions,
                    address,
                    macroblocks_wide,
                    partition,
                    reference_index,
                );
                let motion = estimate_partition_motion(
                    &source[0],
                    &reference[0],
                    coded_width,
                    coded_height,
                    macroblock_x * 16 + partition.block_x * 4,
                    macroblock_y * 16 + partition.block_y * 4,
                    partition.block_width * 4,
                    partition.block_height * 4,
                    search_range,
                    predicted,
                    analysis,
                );
                motion_rate += u64::from((motion.x - predicted.x).unsigned_abs())
                    + u64::from((motion.y - predicted.y).unsigned_abs());
                predict_inter_luma_partition(
                    &mut trial_luma,
                    &reference[0],
                    coded_width,
                    coded_height,
                    macroblock_x,
                    macroblock_y,
                    partition,
                    motion,
                );
                for component in 0..2 {
                    predict_inter_chroma_partition(
                        &mut trial_chroma[component],
                        &reference[component + 1],
                        coded_width / 2,
                        coded_height / 2,
                        macroblock_x,
                        macroblock_y,
                        partition,
                        motion,
                    );
                }
                set_partition_motion(
                    &mut trial_motions,
                    partition,
                    MotionState {
                        vector: motion,
                        reference_index: Some(reference_index),
                    },
                );
                trial_selected.push(SelectedPartition {
                    partition,
                    predicted,
                    motion,
                    reference_index,
                });
            }
            let sub_x = (sub_index % 2) * 8;
            let sub_y = (sub_index / 2) * 8;
            let prediction_sad = prediction_region_sad(
                &source[0],
                coded_width,
                macroblock_x * 16 + sub_x,
                macroblock_y * 16 + sub_y,
                &trial_luma,
                16,
                sub_x,
                sub_y,
                8,
                8,
            );
            let partition_penalty =
                u64::try_from(trial_selected.len() - 1).expect("subpartition count fits") * 8;
            let cost = prediction_sad + motion_rate * 2 + partition_penalty;
            if best
                .as_ref()
                .is_none_or(|candidate| (cost, sub_type) < (candidate.cost, candidate.sub_type))
            {
                best = Some(P8x8Candidate {
                    cost,
                    sub_type,
                    motions: trial_motions,
                    luma_prediction: trial_luma,
                    chroma_predictions: trial_chroma,
                    partitions: trial_selected,
                });
            }
        }
        let best = best.expect("each P8x8 sub-macroblock has prediction candidates");
        total_cost += best.cost;
        *sub_macroblock_type = best.sub_type;
        candidate_motions = best.motions;
        luma_prediction = best.luma_prediction;
        chroma_predictions = best.chroma_predictions;
        selected_partitions.extend(best.partitions);
    }
    (
        total_cost,
        InterDecision {
            macroblock_type: 3,
            sub_macroblock_types,
            partitions: selected_partitions,
            luma_prediction,
            chroma_predictions,
        },
    )
}

fn p_sub_partitions(sub_index: usize, sub_type: u8) -> Vec<InterPartition> {
    let base_x = (sub_index % 2) * 2;
    let base_y = (sub_index / 2) * 2;
    let partition = |block_x, block_y, block_width, block_height| InterPartition {
        block_x,
        block_y,
        block_width,
        block_height,
        prediction_kind: MotionPredictionKind::Normal,
    };
    match sub_type {
        0 => vec![partition(base_x, base_y, 2, 2)],
        1 => vec![
            partition(base_x, base_y, 2, 1),
            partition(base_x, base_y + 1, 2, 1),
        ],
        2 => vec![
            partition(base_x, base_y, 1, 2),
            partition(base_x + 1, base_y, 1, 2),
        ],
        3 => (0..2)
            .flat_map(|y| (0..2).map(move |x| partition(base_x + x, base_y + y, 1, 1)))
            .collect(),
        _ => unreachable!("P sub-macroblock type is in 0..=3"),
    }
}

#[allow(clippy::too_many_arguments)]
fn prediction_region_sad(
    source: &[u8],
    source_stride: usize,
    source_x: usize,
    source_y: usize,
    prediction: &[u8],
    prediction_stride: usize,
    prediction_x: usize,
    prediction_y: usize,
    width: usize,
    height: usize,
) -> u64 {
    (0..height)
        .flat_map(|y| {
            (0..width).map(move |x| {
                u64::from(
                    source[(source_y + y) * source_stride + source_x + x].abs_diff(
                        prediction[(prediction_y + y) * prediction_stride + prediction_x + x],
                    ),
                )
            })
        })
        .sum()
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn estimate_partition_motion(
    source: &[u8],
    reference: &[u8],
    width: usize,
    height: usize,
    origin_x: usize,
    origin_y: usize,
    partition_width: usize,
    partition_height: usize,
    search_range: i32,
    predicted: MotionVector,
    analysis: AnalysisPreset,
) -> MotionVector {
    if analysis == AnalysisPreset::Full {
        return estimate_partition_motion_full(
            source,
            reference,
            width,
            height,
            origin_x,
            origin_y,
            partition_width,
            partition_height,
            search_range,
            predicted,
        );
    }
    let limit = search_range * 4;
    let predicted_integer = MotionVector {
        x: round_to_integer_sample(predicted.x).clamp(-limit, limit),
        y: round_to_integer_sample(predicted.y).clamp(-limit, limit),
    };
    let mut best = (u64::MAX, i32::MAX, i32::MAX, 0_i32, 0_i32);
    evaluate_integer_motion_candidate(
        source,
        reference,
        width,
        height,
        origin_x,
        origin_y,
        partition_width,
        partition_height,
        predicted,
        predicted_integer,
        analysis,
        &mut best,
    );
    evaluate_integer_motion_candidate(
        source,
        reference,
        width,
        height,
        origin_x,
        origin_y,
        partition_width,
        partition_height,
        predicted,
        MotionVector { x: 0, y: 0 },
        analysis,
        &mut best,
    );

    let mut step = 1_i32;
    while step <= search_range / 2 {
        step *= 2;
    }
    while step != 0 {
        loop {
            let center = MotionVector {
                x: best.4,
                y: best.3,
            };
            for (dx, dy) in [(0, -step), (-step, 0), (step, 0), (0, step)] {
                let candidate = MotionVector {
                    x: center.x + dx * 4,
                    y: center.y + dy * 4,
                };
                if candidate.x.abs() <= limit && candidate.y.abs() <= limit {
                    evaluate_integer_motion_candidate(
                        source,
                        reference,
                        width,
                        height,
                        origin_x,
                        origin_y,
                        partition_width,
                        partition_height,
                        predicted,
                        candidate,
                        analysis,
                        &mut best,
                    );
                }
            }
            if (best.4, best.3) == (center.x, center.y) {
                break;
            }
        }
        step /= 2;
    }

    for subpel_step in [2, 1] {
        let center = MotionVector {
            x: best.4,
            y: best.3,
        };
        for (dx, dy) in [
            (0, -subpel_step),
            (-subpel_step, 0),
            (subpel_step, 0),
            (0, subpel_step),
            (-subpel_step, -subpel_step),
            (subpel_step, -subpel_step),
            (-subpel_step, subpel_step),
            (subpel_step, subpel_step),
        ] {
            let motion = MotionVector {
                x: center.x + dx,
                y: center.y + dy,
            };
            if motion.x.abs() > limit || motion.y.abs() > limit {
                continue;
            }
            let sad = if analysis == AnalysisPreset::Fast {
                partition_bilinear_motion_sad_bounded(
                    source,
                    reference,
                    width,
                    height,
                    origin_x,
                    origin_y,
                    partition_width,
                    partition_height,
                    motion,
                    best.0,
                )
            } else {
                partition_motion_sad_bounded(
                    source,
                    reference,
                    width,
                    height,
                    origin_x,
                    origin_y,
                    partition_width,
                    partition_height,
                    motion,
                    best.0,
                )
            };
            let candidate = motion_candidate(sad, motion, predicted);
            if candidate < best {
                best = candidate;
            }
        }
    }
    MotionVector {
        x: best.4,
        y: best.3,
    }
}

#[allow(clippy::too_many_arguments)]
fn estimate_partition_motion_full(
    source: &[u8],
    reference: &[u8],
    width: usize,
    height: usize,
    origin_x: usize,
    origin_y: usize,
    partition_width: usize,
    partition_height: usize,
    search_range: i32,
    predicted: MotionVector,
) -> MotionVector {
    let mut best = (u64::MAX, i32::MAX, i32::MAX, 0_i32, 0_i32);
    for dy in -search_range..=search_range {
        for dx in -search_range..=search_range {
            let motion = MotionVector {
                x: dx * 4,
                y: dy * 4,
            };
            let sad = partition_integer_motion_sad_bounded(
                source,
                reference,
                width,
                height,
                origin_x,
                origin_y,
                partition_width,
                partition_height,
                motion,
                best.0,
            );
            let candidate = motion_candidate(sad, motion, predicted);
            if candidate < best {
                best = candidate;
            }
        }
    }
    let integer_best = MotionVector {
        x: best.4,
        y: best.3,
    };
    let limit = search_range * 4;
    for motion_y in (integer_best.y - 3).max(-limit)..=(integer_best.y + 3).min(limit) {
        for motion_x in (integer_best.x - 3).max(-limit)..=(integer_best.x + 3).min(limit) {
            let motion = MotionVector {
                x: motion_x,
                y: motion_y,
            };
            let sad = partition_motion_sad_bounded(
                source,
                reference,
                width,
                height,
                origin_x,
                origin_y,
                partition_width,
                partition_height,
                motion,
                best.0,
            );
            let candidate = motion_candidate(sad, motion, predicted);
            if candidate < best {
                best = candidate;
            }
        }
    }
    MotionVector {
        x: best.4,
        y: best.3,
    }
}

fn round_to_integer_sample(value: i32) -> i32 {
    if value >= 0 {
        (value + 2) / 4 * 4
    } else {
        (value - 2) / 4 * 4
    }
}

#[allow(clippy::too_many_arguments)]
fn evaluate_integer_motion_candidate(
    source: &[u8],
    reference: &[u8],
    width: usize,
    height: usize,
    origin_x: usize,
    origin_y: usize,
    partition_width: usize,
    partition_height: usize,
    predicted: MotionVector,
    motion: MotionVector,
    analysis: AnalysisPreset,
    best: &mut (u64, i32, i32, i32, i32),
) {
    let sad = if analysis == AnalysisPreset::Fast {
        partition_fast_integer_motion_sad_bounded(
            source,
            reference,
            width,
            height,
            origin_x,
            origin_y,
            partition_width,
            partition_height,
            motion,
            best.0,
        )
    } else {
        partition_integer_motion_sad_bounded(
            source,
            reference,
            width,
            height,
            origin_x,
            origin_y,
            partition_width,
            partition_height,
            motion,
            best.0,
        )
    };
    let candidate = motion_candidate(sad, motion, predicted);
    if candidate < *best {
        *best = candidate;
    }
}

fn motion_candidate(
    sad: u64,
    motion: MotionVector,
    predicted: MotionVector,
) -> (u64, i32, i32, i32, i32) {
    let rate_proxy = (motion.x - predicted.x).abs() + (motion.y - predicted.y).abs();
    let displacement = motion.x.abs() + motion.y.abs();
    (sad, rate_proxy, displacement, motion.y, motion.x)
}

#[allow(clippy::too_many_arguments)]
fn partition_integer_motion_sad_bounded(
    source: &[u8],
    reference: &[u8],
    width: usize,
    height: usize,
    origin_x: usize,
    origin_y: usize,
    partition_width: usize,
    partition_height: usize,
    motion: MotionVector,
    maximum: u64,
) -> u64 {
    debug_assert_eq!(motion.x.rem_euclid(4), 0);
    debug_assert_eq!(motion.y.rem_euclid(4), 0);
    let offset_x = motion.x / 4;
    let offset_y = motion.y / 4;
    let mut sad = 0_u64;
    for y in 0..partition_height {
        let reference_y = i32::try_from(origin_y + y).expect("coded coordinate fits") + offset_y;
        for x in 0..partition_width {
            let source_sample = source[(origin_y + y) * width + origin_x + x];
            let reference_x =
                i32::try_from(origin_x + x).expect("coded coordinate fits") + offset_x;
            sad += u64::from(source_sample.abs_diff(reference_sample(
                reference,
                width,
                height,
                reference_x,
                reference_y,
            )));
        }
        if sad > maximum {
            break;
        }
    }
    sad
}

#[allow(clippy::too_many_arguments)]
fn partition_fast_integer_motion_sad_bounded(
    source: &[u8],
    reference: &[u8],
    width: usize,
    height: usize,
    origin_x: usize,
    origin_y: usize,
    partition_width: usize,
    partition_height: usize,
    motion: MotionVector,
    maximum: u64,
) -> u64 {
    debug_assert_eq!(motion.x.rem_euclid(4), 0);
    debug_assert_eq!(motion.y.rem_euclid(4), 0);
    let offset_x = motion.x / 4;
    let offset_y = motion.y / 4;
    let sample_step = if partition_width >= 16 && partition_height >= 16 {
        4
    } else {
        2
    };
    let scale = u64::try_from(sample_step * sample_step).expect("sample step fits u64");
    let mut sad = 0_u64;
    for y in (0..partition_height).step_by(sample_step) {
        let reference_y = i32::try_from(origin_y + y).expect("coded coordinate fits") + offset_y;
        for x in (0..partition_width).step_by(sample_step) {
            let source_sample = source[(origin_y + y) * width + origin_x + x];
            let reference_x =
                i32::try_from(origin_x + x).expect("coded coordinate fits") + offset_x;
            sad += u64::from(source_sample.abs_diff(reference_sample(
                reference,
                width,
                height,
                reference_x,
                reference_y,
            ))) * scale;
        }
        if sad > maximum {
            break;
        }
    }
    sad
}

#[allow(clippy::too_many_arguments)]
fn partition_motion_sad_bounded(
    source: &[u8],
    reference: &[u8],
    width: usize,
    height: usize,
    origin_x: usize,
    origin_y: usize,
    partition_width: usize,
    partition_height: usize,
    motion: MotionVector,
    maximum: u64,
) -> u64 {
    let mut sad = 0_u64;
    for y in 0..partition_height {
        for x in 0..partition_width {
            let source_sample = source[(origin_y + y) * width + origin_x + x];
            let x_q4 = i32::try_from(origin_x + x).expect("coded coordinate fits") * 4 + motion.x;
            let y_q4 = i32::try_from(origin_y + y).expect("coded coordinate fits") * 4 + motion.y;
            sad +=
                u64::from(source_sample.abs_diff(luma_qpel(reference, width, height, x_q4, y_q4)));
        }
        if sad > maximum {
            break;
        }
    }
    sad
}

#[allow(clippy::too_many_arguments)]
fn partition_bilinear_motion_sad_bounded(
    source: &[u8],
    reference: &[u8],
    width: usize,
    height: usize,
    origin_x: usize,
    origin_y: usize,
    partition_width: usize,
    partition_height: usize,
    motion: MotionVector,
    maximum: u64,
) -> u64 {
    let sample_step = if partition_width >= 16 && partition_height >= 16 {
        4
    } else {
        2
    };
    let scale = u64::try_from(sample_step * sample_step).expect("sample step fits u64");
    let mut sad = 0_u64;
    for y in (0..partition_height).step_by(sample_step) {
        for x in (0..partition_width).step_by(sample_step) {
            let source_sample = source[(origin_y + y) * width + origin_x + x];
            let x_q4 = i32::try_from(origin_x + x).expect("coded coordinate fits") * 4 + motion.x;
            let y_q4 = i32::try_from(origin_y + y).expect("coded coordinate fits") * 4 + motion.y;
            sad += u64::from(
                source_sample.abs_diff(luma_bilinear_qpel(reference, width, height, x_q4, y_q4)),
            ) * scale;
        }
        if sad > maximum {
            break;
        }
    }
    sad
}

fn luma_bilinear_qpel(plane: &[u8], width: usize, height: usize, x_q4: i32, y_q4: i32) -> u8 {
    let x = x_q4.div_euclid(4);
    let y = y_q4.div_euclid(4);
    let fraction_x = x_q4.rem_euclid(4);
    let fraction_y = y_q4.rem_euclid(4);
    let top_left = i32::from(reference_sample(plane, width, height, x, y));
    let top_right = i32::from(reference_sample(plane, width, height, x + 1, y));
    let bottom_left = i32::from(reference_sample(plane, width, height, x, y + 1));
    let bottom_right = i32::from(reference_sample(plane, width, height, x + 1, y + 1));
    u8::try_from(
        (((4 - fraction_x) * (4 - fraction_y) * top_left
            + fraction_x * (4 - fraction_y) * top_right
            + (4 - fraction_x) * fraction_y * bottom_left
            + fraction_x * fraction_y * bottom_right
            + 8)
            >> 4)
            .clamp(0, 255),
    )
    .expect("clamped luma interpolation fits u8")
}

#[allow(clippy::too_many_arguments)]
fn predict_inter_luma_partition(
    output: &mut [u8],
    reference: &[u8],
    width: usize,
    height: usize,
    macroblock_x: usize,
    macroblock_y: usize,
    partition: InterPartition,
    motion: MotionVector,
) {
    let partition_x = partition.block_x * 4;
    let partition_y = partition.block_y * 4;
    if motion.x.rem_euclid(4) == 0 && motion.y.rem_euclid(4) == 0 {
        copy_integer_prediction(
            output,
            16,
            partition_x,
            partition_y,
            reference,
            width,
            height,
            i32::try_from(macroblock_x * 16 + partition_x).expect("coded width is bounded")
                + motion.x / 4,
            i32::try_from(macroblock_y * 16 + partition_y).expect("coded height is bounded")
                + motion.y / 4,
            partition.block_width * 4,
            partition.block_height * 4,
        );
        return;
    }
    for y in 0..partition.block_height * 4 {
        for x in 0..partition.block_width * 4 {
            let destination_x = partition_x + x;
            let destination_y = partition_y + y;
            output[destination_y * 16 + destination_x] = luma_qpel(
                reference,
                width,
                height,
                i32::try_from(macroblock_x * 16 + destination_x).expect("coded width is bounded")
                    * 4
                    + motion.x,
                i32::try_from(macroblock_y * 16 + destination_y).expect("coded height is bounded")
                    * 4
                    + motion.y,
            );
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn predict_inter_chroma_partition(
    output: &mut [u8],
    reference: &[u8],
    width: usize,
    height: usize,
    macroblock_x: usize,
    macroblock_y: usize,
    partition: InterPartition,
    motion: MotionVector,
) {
    let partition_x = partition.block_x * 2;
    let partition_y = partition.block_y * 2;
    if motion.x.rem_euclid(8) == 0 && motion.y.rem_euclid(8) == 0 {
        copy_integer_prediction(
            output,
            8,
            partition_x,
            partition_y,
            reference,
            width,
            height,
            i32::try_from(macroblock_x * 8 + partition_x).expect("coded width is bounded")
                + motion.x / 8,
            i32::try_from(macroblock_y * 8 + partition_y).expect("coded height is bounded")
                + motion.y / 8,
            partition.block_width * 2,
            partition.block_height * 2,
        );
        return;
    }
    for y in 0..partition.block_height * 2 {
        for x in 0..partition.block_width * 2 {
            let destination_x = partition_x + x;
            let destination_y = partition_y + y;
            output[destination_y * 8 + destination_x] = chroma_epel(
                reference,
                width,
                height,
                i32::try_from(macroblock_x * 8 + destination_x).expect("coded width is bounded")
                    * 8
                    + motion.x,
                i32::try_from(macroblock_y * 8 + destination_y).expect("coded height is bounded")
                    * 8
                    + motion.y,
            );
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn copy_integer_prediction(
    output: &mut [u8],
    output_stride: usize,
    destination_x: usize,
    destination_y: usize,
    reference: &[u8],
    reference_stride: usize,
    reference_height: usize,
    reference_x: i32,
    reference_y: i32,
    block_width: usize,
    block_height: usize,
) {
    let contiguous_x = usize::try_from(reference_x)
        .ok()
        .filter(|&x| x.saturating_add(block_width) <= reference_stride);
    let maximum_y = i32::try_from(reference_height - 1).expect("reference height fits i32");
    let maximum_x = i32::try_from(reference_stride - 1).expect("reference width fits i32");
    for y in 0..block_height {
        let source_y = usize::try_from(
            (reference_y + i32::try_from(y).expect("block height fits i32")).clamp(0, maximum_y),
        )
        .expect("clamped source row is non-negative");
        let destination = (destination_y + y) * output_stride + destination_x;
        if let Some(source_x) = contiguous_x {
            let source = source_y * reference_stride + source_x;
            output[destination..destination + block_width]
                .copy_from_slice(&reference[source..source + block_width]);
        } else {
            for x in 0..block_width {
                let source_x = usize::try_from(
                    (reference_x + i32::try_from(x).expect("block width fits i32"))
                        .clamp(0, maximum_x),
                )
                .expect("clamped source column is non-negative");
                output[destination + x] = reference[source_y * reference_stride + source_x];
            }
        }
    }
}

fn chroma_epel(plane: &[u8], width: usize, height: usize, x_q8: i32, y_q8: i32) -> u8 {
    let integer_x = x_q8.div_euclid(8);
    let integer_y = y_q8.div_euclid(8);
    let fraction_x = x_q8.rem_euclid(8);
    let fraction_y = y_q8.rem_euclid(8);
    if let (Ok(x), Ok(y)) = (usize::try_from(integer_x), usize::try_from(integer_y))
        && x + 1 < width
        && y + 1 < height
    {
        let top = y * width + x;
        return chroma_bilinear_sample(
            plane[top],
            plane[top + 1],
            plane[top + width],
            plane[top + width + 1],
            fraction_x,
            fraction_y,
        );
    }
    let top_left = i32::from(reference_sample(plane, width, height, integer_x, integer_y));
    let top_right = i32::from(reference_sample(
        plane,
        width,
        height,
        integer_x + 1,
        integer_y,
    ));
    let bottom_left = i32::from(reference_sample(
        plane,
        width,
        height,
        integer_x,
        integer_y + 1,
    ));
    let bottom_right = i32::from(reference_sample(
        plane,
        width,
        height,
        integer_x + 1,
        integer_y + 1,
    ));
    chroma_bilinear_sample(
        u8::try_from(top_left).expect("reference sample fits u8"),
        u8::try_from(top_right).expect("reference sample fits u8"),
        u8::try_from(bottom_left).expect("reference sample fits u8"),
        u8::try_from(bottom_right).expect("reference sample fits u8"),
        fraction_x,
        fraction_y,
    )
}

fn chroma_bilinear_sample(
    top_left: u8,
    top_right: u8,
    bottom_left: u8,
    bottom_right: u8,
    fraction_x: i32,
    fraction_y: i32,
) -> u8 {
    let top_left = i32::from(top_left);
    let top_right = i32::from(top_right);
    let bottom_left = i32::from(bottom_left);
    let bottom_right = i32::from(bottom_right);
    u8::try_from(
        (((8 - fraction_x) * (8 - fraction_y) * top_left
            + fraction_x * (8 - fraction_y) * top_right
            + (8 - fraction_x) * fraction_y * bottom_left
            + fraction_x * fraction_y * bottom_right
            + 32)
            >> 6)
            .clamp(0, 255),
    )
    .expect("clamped chroma interpolation fits u8")
}

fn reference_sample(plane: &[u8], width: usize, height: usize, x: i32, y: i32) -> u8 {
    if let (Ok(x), Ok(y)) = (usize::try_from(x), usize::try_from(y))
        && x < width
        && y < height
    {
        return plane[y * width + x];
    }
    let x = usize::try_from(x.clamp(0, i32::try_from(width - 1).expect("width fits i32")))
        .expect("clamped coordinate is non-negative");
    let y = usize::try_from(y.clamp(0, i32::try_from(height - 1).expect("height fits i32")))
        .expect("clamped coordinate is non-negative");
    plane[y * width + x]
}

fn luma_qpel(plane: &[u8], width: usize, height: usize, x_q4: i32, y_q4: i32) -> u8 {
    let x = x_q4.div_euclid(4);
    let y = y_q4.div_euclid(4);
    let fraction_x = x_q4.rem_euclid(4);
    let fraction_y = y_q4.rem_euclid(4);
    let integer = reference_sample(plane, width, height, x, y);
    match (fraction_x, fraction_y) {
        (0, 0) => integer,
        (1, 0) => average(integer, half_horizontal(plane, width, height, x, y)),
        (2, 0) => half_horizontal(plane, width, height, x, y),
        (3, 0) => average(
            half_horizontal(plane, width, height, x, y),
            reference_sample(plane, width, height, x + 1, y),
        ),
        (0, 1) => average(integer, half_vertical(plane, width, height, x, y)),
        (0, 2) => half_vertical(plane, width, height, x, y),
        (0, 3) => average(
            half_vertical(plane, width, height, x, y),
            reference_sample(plane, width, height, x, y + 1),
        ),
        (1, 1) => average(
            half_horizontal(plane, width, height, x, y),
            half_vertical(plane, width, height, x, y),
        ),
        (2, 1) => average(
            half_horizontal(plane, width, height, x, y),
            half_diagonal(plane, width, height, x, y),
        ),
        (3, 1) => average(
            half_horizontal(plane, width, height, x, y),
            half_vertical(plane, width, height, x + 1, y),
        ),
        (1, 2) => average(
            half_vertical(plane, width, height, x, y),
            half_diagonal(plane, width, height, x, y),
        ),
        (2, 2) => half_diagonal(plane, width, height, x, y),
        (3, 2) => average(
            half_diagonal(plane, width, height, x, y),
            half_vertical(plane, width, height, x + 1, y),
        ),
        (1, 3) => average(
            half_vertical(plane, width, height, x, y),
            half_horizontal(plane, width, height, x, y + 1),
        ),
        (2, 3) => average(
            half_diagonal(plane, width, height, x, y),
            half_horizontal(plane, width, height, x, y + 1),
        ),
        (3, 3) => average(
            half_vertical(plane, width, height, x + 1, y),
            half_horizontal(plane, width, height, x, y + 1),
        ),
        _ => unreachable!("quarter-sample fractions are in 0..4"),
    }
}

fn half_horizontal(plane: &[u8], width: usize, height: usize, x: i32, y: i32) -> u8 {
    clip_u8((half_horizontal_raw(plane, width, height, x, y) + 16) >> 5)
}

fn half_horizontal_raw(plane: &[u8], width: usize, height: usize, x: i32, y: i32) -> i32 {
    if let (Ok(x), Ok(y)) = (usize::try_from(x), usize::try_from(y))
        && x >= 2
        && x + 3 < width
        && y < height
    {
        let index = y * width + x;
        return i32::from(plane[index - 2]) - 5 * i32::from(plane[index - 1])
            + 20 * i32::from(plane[index])
            + 20 * i32::from(plane[index + 1])
            - 5 * i32::from(plane[index + 2])
            + i32::from(plane[index + 3]);
    }
    let taps = [-2, -1, 0, 1, 2, 3]
        .map(|offset| i32::from(reference_sample(plane, width, height, x + offset, y)));
    taps[0] - 5 * taps[1] + 20 * taps[2] + 20 * taps[3] - 5 * taps[4] + taps[5]
}

fn half_vertical(plane: &[u8], width: usize, height: usize, x: i32, y: i32) -> u8 {
    if let (Ok(x), Ok(y)) = (usize::try_from(x), usize::try_from(y))
        && x < width
        && y >= 2
        && y + 3 < height
    {
        let index = y * width + x;
        return clip_u8(
            (i32::from(plane[index - 2 * width]) - 5 * i32::from(plane[index - width])
                + 20 * i32::from(plane[index])
                + 20 * i32::from(plane[index + width])
                - 5 * i32::from(plane[index + 2 * width])
                + i32::from(plane[index + 3 * width])
                + 16)
                >> 5,
        );
    }
    let taps = [-2, -1, 0, 1, 2, 3]
        .map(|offset| i32::from(reference_sample(plane, width, height, x, y + offset)));
    clip_u8((taps[0] - 5 * taps[1] + 20 * taps[2] + 20 * taps[3] - 5 * taps[4] + taps[5] + 16) >> 5)
}

fn half_diagonal(plane: &[u8], width: usize, height: usize, x: i32, y: i32) -> u8 {
    let taps =
        [-2, -1, 0, 1, 2, 3].map(|offset| half_horizontal_raw(plane, width, height, x, y + offset));
    clip_u8(
        (taps[0] - 5 * taps[1] + 20 * taps[2] + 20 * taps[3] - 5 * taps[4] + taps[5] + 512) >> 10,
    )
}

fn clip_u8(value: i32) -> u8 {
    u8::try_from(value.clamp(0, 255)).expect("clamped H.264 sample fits u8")
}

fn partition_motion_predictor(
    motions: &[[Option<MotionState>; 16]],
    address: usize,
    macroblocks_wide: usize,
    partition: InterPartition,
    reference_index: u8,
) -> MotionVector {
    let macroblock_x = address % macroblocks_wide;
    let macroblock_y = address / macroblocks_wide;
    let block_x = (macroblock_x * 4 + partition.block_x).cast_signed();
    let block_y = (macroblock_y * 4 + partition.block_y).cast_signed();
    let a = motion_at(motions, macroblocks_wide, block_x - 1, block_y);
    let mut b = motion_at(motions, macroblocks_wide, block_x, block_y - 1);
    let mut c = motion_at(
        motions,
        macroblocks_wide,
        block_x + partition.block_width.cast_signed(),
        block_y - 1,
    )
    .or_else(|| motion_at(motions, macroblocks_wide, block_x - 1, block_y - 1));
    if b.is_none() && c.is_none() && a.is_some() {
        b = a;
        c = a;
    }
    let preferred = match partition.prediction_kind {
        MotionPredictionKind::Top16x8 => b,
        MotionPredictionKind::Bottom16x8 | MotionPredictionKind::Left8x16 => a,
        MotionPredictionKind::Right8x16 => c,
        MotionPredictionKind::Normal => None,
    };
    if let Some(preferred) =
        preferred.filter(|motion| motion.reference_index == Some(reference_index))
    {
        return preferred.vector;
    }
    let candidates = [a, b, c];
    let mut matching = candidates
        .into_iter()
        .flatten()
        .filter(|candidate| candidate.reference_index == Some(reference_index));
    if let Some(candidate) = matching.next()
        && matching.next().is_none()
    {
        return candidate.vector;
    }
    let candidates = candidates.map(|motion| motion.map_or(MotionVector::default(), |v| v.vector));
    MotionVector {
        x: median3(candidates.map(|motion| motion.x)),
        y: median3(candidates.map(|motion| motion.y)),
    }
}

fn partition_motion_predictor_with_current(
    current: &[Option<MotionState>; 16],
    motions: &[[Option<MotionState>; 16]],
    address: usize,
    macroblocks_wide: usize,
    partition: InterPartition,
    reference_index: u8,
) -> MotionVector {
    let macroblock_x = address % macroblocks_wide;
    let macroblock_y = address / macroblocks_wide;
    let block_x = (macroblock_x * 4 + partition.block_x).cast_signed();
    let block_y = (macroblock_y * 4 + partition.block_y).cast_signed();
    let at = |x, y| motion_at_with_current(motions, current, address, macroblocks_wide, x, y);
    let a = at(block_x - 1, block_y);
    let mut b = at(block_x, block_y - 1);
    let mut c = at(block_x + partition.block_width.cast_signed(), block_y - 1)
        .or_else(|| at(block_x - 1, block_y - 1));
    if b.is_none() && c.is_none() && a.is_some() {
        b = a;
        c = a;
    }
    let preferred = match partition.prediction_kind {
        MotionPredictionKind::Top16x8 => b,
        MotionPredictionKind::Bottom16x8 | MotionPredictionKind::Left8x16 => a,
        MotionPredictionKind::Right8x16 => c,
        MotionPredictionKind::Normal => None,
    };
    if let Some(preferred) =
        preferred.filter(|motion| motion.reference_index == Some(reference_index))
    {
        return preferred.vector;
    }
    let candidates = [a, b, c];
    let mut matching = candidates
        .into_iter()
        .flatten()
        .filter(|candidate| candidate.reference_index == Some(reference_index));
    if let Some(candidate) = matching.next()
        && matching.next().is_none()
    {
        return candidate.vector;
    }
    let candidates = candidates.map(|motion| motion.map_or(MotionVector::default(), |v| v.vector));
    MotionVector {
        x: median3(candidates.map(|motion| motion.x)),
        y: median3(candidates.map(|motion| motion.y)),
    }
}

fn p_skip_motion(
    motions: &[[Option<MotionState>; 16]],
    address: usize,
    macroblocks_wide: usize,
) -> MotionVector {
    let macroblock_x = address % macroblocks_wide;
    let macroblock_y = address / macroblocks_wide;
    let block_x = (macroblock_x * 4).cast_signed();
    let block_y = (macroblock_y * 4).cast_signed();
    let left = motion_at(motions, macroblocks_wide, block_x - 1, block_y);
    let top = motion_at(motions, macroblocks_wide, block_x, block_y - 1);
    let zero = MotionState {
        vector: MotionVector::default(),
        reference_index: Some(0),
    };
    if left.is_none() || top.is_none() || left == Some(zero) || top == Some(zero) {
        MotionVector::default()
    } else {
        partition_motion_predictor(
            motions,
            address,
            macroblocks_wide,
            InterPartition {
                block_x: 0,
                block_y: 0,
                block_width: 4,
                block_height: 4,
                prediction_kind: MotionPredictionKind::Normal,
            },
            0,
        )
    }
}

fn motion_at(
    motions: &[[Option<MotionState>; 16]],
    macroblocks_wide: usize,
    block_x: isize,
    block_y: isize,
) -> Option<MotionState> {
    let blocks_wide = macroblocks_wide * 4;
    let blocks_high = motions.len().div_ceil(macroblocks_wide) * 4;
    let block_x = usize::try_from(block_x).ok().filter(|&x| x < blocks_wide)?;
    let block_y = usize::try_from(block_y).ok().filter(|&y| y < blocks_high)?;
    let address = (block_y / 4) * macroblocks_wide + block_x / 4;
    motions[address][luma_block_index(block_x % 4, block_y % 4)]
}

fn motion_at_with_current(
    motions: &[[Option<MotionState>; 16]],
    current: &[Option<MotionState>; 16],
    current_address: usize,
    macroblocks_wide: usize,
    block_x: isize,
    block_y: isize,
) -> Option<MotionState> {
    let blocks_wide = macroblocks_wide * 4;
    let blocks_high = motions.len().div_ceil(macroblocks_wide) * 4;
    let block_x = usize::try_from(block_x).ok().filter(|&x| x < blocks_wide)?;
    let block_y = usize::try_from(block_y).ok().filter(|&y| y < blocks_high)?;
    let address = (block_y / 4) * macroblocks_wide + block_x / 4;
    let block = luma_block_index(block_x % 4, block_y % 4);
    if address == current_address {
        current[block]
    } else {
        motions[address][block]
    }
}

fn set_partition_motion(
    motions: &mut [Option<MotionState>; 16],
    partition: InterPartition,
    motion: MotionState,
) {
    for block_y in partition.block_y..partition.block_y + partition.block_height {
        for block_x in partition.block_x..partition.block_x + partition.block_width {
            motions[luma_block_index(block_x, block_y)] = Some(motion);
        }
    }
}

fn median3(mut values: [i32; 3]) -> i32 {
    values.sort_unstable();
    values[1]
}

fn quantize_inter_luma_residual(
    source: &[u8],
    stride: usize,
    macroblock_x: usize,
    macroblock_y: usize,
    prediction: &[u8],
    qp: i32,
    scaling_list: &[u8; 16],
) -> [[i32; 16]; 16] {
    let origin_x = macroblock_x * 16;
    let origin_y = macroblock_y * 16;
    let mut output = [[0_i32; 16]; 16];
    for block_y in 0..4 {
        for block_x in 0..4 {
            let mut residual = [0_i32; 16];
            for y in 0..4 {
                for x in 0..4 {
                    residual[y * 4 + x] = i32::from(
                        source[(origin_y + block_y * 4 + y) * stride + origin_x + block_x * 4 + x],
                    ) - i32::from(
                        prediction[(block_y * 4 + y) * 16 + block_x * 4 + x],
                    );
                }
            }
            let transformed = forward_transform_4x4(&residual);
            let block_index = luma_block_index(block_x, block_y);
            for (destination, &(row, column)) in output[block_index].iter_mut().zip(&ZIG_ZAG_4X4) {
                *destination = quantize_coefficient(
                    transformed[row * 4 + column],
                    qp,
                    row,
                    column,
                    scaling_list,
                );
            }
        }
    }
    output
}

fn quantize_inter_luma_residual_8x8(
    source: &[u8],
    stride: usize,
    macroblock_x: usize,
    macroblock_y: usize,
    prediction: &[u8],
    qp: i32,
    scaling_list: &[u8; 64],
) -> [[i32; 64]; 4] {
    let origin_x = macroblock_x * 16;
    let origin_y = macroblock_y * 16;
    std::array::from_fn(|group| {
        let block_x = (group % 2) * 8;
        let block_y = (group / 2) * 8;
        let block_prediction: [u8; 64] = std::array::from_fn(|index| {
            prediction[(block_y + index / 8) * 16 + block_x + index % 8]
        });
        quantize_intra8_luma_block(
            source,
            stride,
            origin_x + block_x,
            origin_y + block_y,
            &block_prediction,
            qp,
            scaling_list,
        )
    })
}

#[allow(clippy::too_many_arguments)]
fn select_inter_luma_residual(
    source: &[u8],
    stride: usize,
    macroblock_x: usize,
    macroblock_y: usize,
    prediction: &[u8],
    qp: i32,
    transform_8x8_allowed: bool,
    transform_bypass: bool,
    scaling_matrix: ScalingMatrixPreset,
) -> InterLumaResidual {
    if transform_bypass {
        return InterLumaResidual::BypassFourByFour(Box::new(quantize_inter_luma_residual_bypass(
            source,
            stride,
            macroblock_x,
            macroblock_y,
            prediction,
        )));
    }
    let four_by_four = InterLumaResidual::FourByFour(Box::new(quantize_inter_luma_residual(
        source,
        stride,
        macroblock_x,
        macroblock_y,
        prediction,
        qp,
        scaling_matrix.four_by_four(false),
    )));
    if !transform_8x8_allowed {
        return four_by_four;
    }
    let eight_by_eight =
        InterLumaResidual::EightByEight(Box::new(quantize_inter_luma_residual_8x8(
            source,
            stride,
            macroblock_x,
            macroblock_y,
            prediction,
            qp,
            scaling_matrix.eight_by_eight(false),
        )));
    let four_cost = inter_luma_residual_cost(
        source,
        stride,
        macroblock_x,
        macroblock_y,
        prediction,
        &four_by_four,
        qp,
        scaling_matrix,
    );
    let eight_cost = inter_luma_residual_cost(
        source,
        stride,
        macroblock_x,
        macroblock_y,
        prediction,
        &eight_by_eight,
        qp,
        scaling_matrix,
    );
    if eight_cost <= four_cost {
        eight_by_eight
    } else {
        four_by_four
    }
}

fn quantize_inter_luma_residual_bypass(
    source: &[u8],
    stride: usize,
    macroblock_x: usize,
    macroblock_y: usize,
    prediction: &[u8],
) -> [[i32; 16]; 16] {
    let origin_x = macroblock_x * 16;
    let origin_y = macroblock_y * 16;
    std::array::from_fn(|block_index| {
        let (block_x, block_y) = luma_block_position(block_index);
        std::array::from_fn(|coefficient| {
            let (row, column) = ZIG_ZAG_4X4[coefficient];
            i32::from(
                source[(origin_y + block_y * 4 + row) * stride + origin_x + block_x * 4 + column],
            ) - i32::from(prediction[(block_y * 4 + row) * 16 + block_x * 4 + column])
        })
    })
}

#[allow(clippy::too_many_arguments)]
fn inter_luma_residual_cost(
    source: &[u8],
    stride: usize,
    macroblock_x: usize,
    macroblock_y: usize,
    prediction: &[u8],
    levels: &InterLumaResidual,
    qp: i32,
    scaling_matrix: ScalingMatrixPreset,
) -> u64 {
    let reconstructed = reconstruct_inter_luma_prediction(prediction, levels, qp, scaling_matrix);
    let origin_x = macroblock_x * 16;
    let origin_y = macroblock_y * 16;
    let mut distortion = 0_u64;
    for y in 0..16 {
        for x in 0..16 {
            let difference = i32::from(source[(origin_y + y) * stride + origin_x + x])
                - i32::from(reconstructed[y * 16 + x]);
            distortion = distortion.saturating_add(u64::from(difference.unsigned_abs()).pow(2));
        }
    }
    let nonzero = match levels {
        InterLumaResidual::FourByFour(levels) | InterLumaResidual::BypassFourByFour(levels) => {
            levels.iter().flatten().filter(|&&x| x != 0).count()
        }
        InterLumaResidual::EightByEight(levels) => {
            levels.iter().flatten().filter(|&&x| x != 0).count()
        }
    };
    let lambda = u64::try_from(qp + 1).expect("non-negative QP").pow(2);
    distortion.saturating_add(
        u64::try_from(nonzero)
            .expect("bounded coefficient count")
            .saturating_mul(lambda),
    )
}

fn luma_coded_block_pattern(levels: &[[i32; 16]; 16]) -> u8 {
    (0..4).fold(0_u8, |pattern, group| {
        pattern
            | (u8::from(
                levels[group * 4..group * 4 + 4]
                    .iter()
                    .flatten()
                    .any(|&level| level != 0),
            ) << group)
    })
}

fn chroma_coded_block_pattern(residuals: &[ChromaResidual; 2]) -> u8 {
    if residuals
        .iter()
        .flat_map(|residual| residual.ac.iter().flatten())
        .any(|&level| level != 0)
    {
        2
    } else {
        u8::from(
            residuals
                .iter()
                .flat_map(|residual| residual.dc)
                .any(|level| level != 0),
        )
    }
}

#[allow(clippy::too_many_arguments)]
fn reconstruct_inter_luma(
    destination: &mut [u8],
    stride: usize,
    macroblock_x: usize,
    macroblock_y: usize,
    prediction: &[u8],
    levels: &InterLumaResidual,
    qp: i32,
    scaling_matrix: ScalingMatrixPreset,
) {
    let reconstructed = reconstruct_inter_luma_prediction(prediction, levels, qp, scaling_matrix);
    place_block(
        destination,
        stride,
        macroblock_x * 16,
        macroblock_y * 16,
        16,
        &reconstructed,
    );
}

fn reconstruct_inter_luma_prediction(
    prediction: &[u8],
    levels: &InterLumaResidual,
    qp: i32,
    scaling_matrix: ScalingMatrixPreset,
) -> Vec<u8> {
    let mut reconstructed = prediction.to_vec();
    match levels {
        InterLumaResidual::FourByFour(levels) => {
            for block_y in 0..4 {
                for block_x in 0..4 {
                    let block_index = luma_block_index(block_x, block_y);
                    let mut scaled = [0_i64; 16];
                    for (&level, &(row, column)) in levels[block_index].iter().zip(&ZIG_ZAG_4X4) {
                        scaled[row * 4 + column] = inverse_quantized_coefficient(
                            level,
                            qp,
                            row,
                            column,
                            scaling_matrix.four_by_four(false),
                        );
                    }
                    let residual = inverse_transform_4x4(&scaled);
                    for y in 0..4 {
                        for x in 0..4 {
                            let index = (block_y * 4 + y) * 16 + block_x * 4 + x;
                            reconstructed[index] = u8::try_from(
                                (i64::from(reconstructed[index]) + residual[y * 4 + x])
                                    .clamp(0, 255),
                            )
                            .expect("clamped inter reconstruction fits u8");
                        }
                    }
                }
            }
        }
        InterLumaResidual::BypassFourByFour(levels) => {
            for block_y in 0..4 {
                for block_x in 0..4 {
                    let block_index = luma_block_index(block_x, block_y);
                    for (coefficient, &(row, column)) in ZIG_ZAG_4X4.iter().enumerate() {
                        let index = (block_y * 4 + row) * 16 + block_x * 4 + column;
                        reconstructed[index] = u8::try_from(
                            (i32::from(reconstructed[index]) + levels[block_index][coefficient])
                                .clamp(0, 255),
                        )
                        .expect("clamped transform-bypass reconstruction fits u8");
                    }
                }
            }
        }
        InterLumaResidual::EightByEight(levels) => {
            for (group, group_levels) in levels.iter().enumerate() {
                let residual = inverse_quantized_transform_8x8(
                    group_levels,
                    qp,
                    scaling_matrix.eight_by_eight(false),
                );
                let block_x = (group % 2) * 8;
                let block_y = (group / 2) * 8;
                for y in 0..8 {
                    for x in 0..8 {
                        let index = (block_y + y) * 16 + block_x + x;
                        reconstructed[index] = u8::try_from(
                            (i64::from(reconstructed[index]) + residual[y * 8 + x]).clamp(0, 255),
                        )
                        .expect("clamped inter 8x8 reconstruction fits u8");
                    }
                }
            }
        }
    }
    reconstructed
}

fn write_pcm_plane_block(
    writer: &mut BitWriter,
    plane: &[u8],
    stride: usize,
    origin_x: usize,
    origin_y: usize,
    width: usize,
    height: usize,
) -> Result<()> {
    for y in origin_y..origin_y + height {
        for x in origin_x..origin_x + width {
            writer.write_bits(u64::from(plane[y * stride + x]), 8)?;
        }
    }
    Ok(())
}

fn padded_planes(frame: &VideoFrame, configuration: &Configuration) -> [Vec<u8>; 3] {
    std::array::from_fn(|component| {
        let plane = &frame.planes[component];
        let coded_width = if component == 0 {
            configuration.coded_width
        } else {
            configuration.coded_width / 2
        };
        let coded_height = if component == 0 {
            configuration.coded_height
        } else {
            configuration.coded_height / 2
        };
        let mut output = vec![0; coded_width * coded_height];
        for y in 0..coded_height {
            let source_y = y.min(plane.height - 1);
            for x in 0..coded_width {
                let source_x = x.min(plane.width - 1);
                output[y * coded_width + x] = plane.data[source_y * plane.stride + source_x];
            }
        }
        output
    })
}

fn adaptive_macroblock_qps(
    luma: &[u8],
    coded_width: usize,
    coded_height: usize,
    base_qp: i32,
    strength: u8,
) -> Vec<i32> {
    let macroblocks_wide = coded_width / 16;
    let macroblocks_high = coded_height / 16;
    if strength == 0 {
        return vec![base_qp; macroblocks_wide * macroblocks_high];
    }
    let activities = (0..macroblocks_high)
        .flat_map(|macroblock_y| {
            (0..macroblocks_wide).map(move |macroblock_x| {
                let sample_sum = (0..16)
                    .flat_map(|y| {
                        (0..16).map(move |x| {
                            u64::from(
                                luma[(macroblock_y * 16 + y) * coded_width + macroblock_x * 16 + x],
                            )
                        })
                    })
                    .sum::<u64>();
                let mean = sample_sum / 256;
                (0..16)
                    .flat_map(|y| {
                        (0..16).map(move |x| {
                            let sample = u64::from(
                                luma[(macroblock_y * 16 + y) * coded_width + macroblock_x * 16 + x],
                            );
                            sample.abs_diff(mean)
                        })
                    })
                    .sum::<u64>()
            })
        })
        .collect::<Vec<_>>();
    let frame_activity = activities.iter().sum::<u64>() / activities.len() as u64;
    activities
        .into_iter()
        .map(|activity| {
            let denominator = activity.max(frame_activity).max(1);
            let difference = i64::try_from(activity).expect("macroblock activity fits i64")
                - i64::try_from(frame_activity).expect("frame activity fits i64");
            let denominator = i64::try_from(denominator).expect("macroblock activity fits i64");
            let offset = round_div(i64::from(strength) * difference, denominator);
            (base_qp + i32::try_from(offset).expect("bounded AQ offset fits i32")).clamp(0, 51)
        })
        .collect()
}

fn macroblock_qp_delta(previous_qp: i32, desired_qp: i32) -> i32 {
    let difference = desired_qp - previous_qp;
    if difference < -26 {
        difference + 52
    } else if difference > 25 {
        difference - 52
    } else {
        difference
    }
}

fn scene_cut_detected(
    frame: &VideoFrame,
    configuration: &Configuration,
    reference: &[Vec<u8>; 3],
) -> bool {
    let threshold = configuration.scene_cut_threshold;
    if threshold == 0 {
        return false;
    }
    let source = &frame.planes[0];
    let difference = (0..source.height)
        .flat_map(|y| {
            (0..source.width).map(move |x| {
                u64::from(
                    source.data[y * source.stride + x]
                        .abs_diff(reference[0][y * configuration.coded_width + x]),
                )
            })
        })
        .sum::<u64>();
    difference
        >= u64::from(threshold)
            * u64::try_from(source.width * source.height).expect("validated frame area fits u64")
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn encode_intra8_macroblock(
    writer: &mut BitWriter,
    source: &[Vec<u8>; 3],
    reconstructed: &mut [Vec<u8>; 3],
    luma_modes: &mut [[u8; 16]],
    luma_nonzero: &mut [[u8; 16]],
    chroma_nonzero: &mut [[[u8; 4]; 2]],
    address: usize,
    macroblocks_wide: usize,
    macroblock_x: usize,
    macroblock_y: usize,
    coded_width: usize,
    qp: i32,
    qp_delta: i32,
    scaling_matrix: ScalingMatrixPreset,
) -> Result<bool> {
    let mut levels = [[0_i32; 64]; 4];
    for (group, group_levels) in levels.iter_mut().enumerate() {
        let origin_x = macroblock_x * 16 + (group % 2) * 8;
        let origin_y = macroblock_y * 16 + (group / 2) * 8;
        let (mode, prediction) = (0..=8)
            .filter_map(|mode| {
                intra8_prediction(
                    &reconstructed[0],
                    coded_width,
                    origin_x,
                    origin_y,
                    group,
                    mode,
                    macroblock_y > 0,
                    macroblock_x > 0,
                )
                .map(|prediction| {
                    let sad =
                        block_sad(&source[0], coded_width, origin_x, origin_y, 8, &prediction);
                    (mode, prediction, sad)
                })
            })
            .min_by_key(|(_, _, sad)| *sad)
            .map(|(mode, prediction, _)| (mode, prediction))
            .expect("Intra8 DC prediction is always available");
        luma_modes[address][group * 4..group * 4 + 4].fill(mode);
        *group_levels = quantize_intra8_luma_block(
            &source[0],
            coded_width,
            origin_x,
            origin_y,
            &prediction,
            qp,
            scaling_matrix.eight_by_eight(true),
        );
        reconstruct_intra8_luma_block(
            &mut reconstructed[0],
            coded_width,
            origin_x,
            origin_y,
            &prediction,
            group_levels,
            qp,
            scaling_matrix.eight_by_eight(true),
        );
    }

    let (chroma_mode, chroma_predictions) = select_chroma_prediction(
        [&source[1], &source[2]],
        [&reconstructed[1], &reconstructed[2]],
        coded_width / 2,
        macroblock_x,
        macroblock_y,
    );
    let chroma_qp = chroma_qp(qp);
    let chroma_residuals: [ChromaResidual; 2] = std::array::from_fn(|component| {
        quantize_chroma_residual(
            &source[component + 1],
            coded_width / 2,
            macroblock_x,
            macroblock_y,
            &chroma_predictions[component],
            chroma_qp,
            false,
            Some(chroma_mode),
            scaling_matrix.four_by_four(true),
        )
    });
    let luma_pattern = (0..4).fold(0_u8, |pattern, group| {
        pattern | (u8::from(levels[group].iter().any(|&level| level != 0)) << group)
    });
    let has_chroma_ac = chroma_residuals
        .iter()
        .flat_map(|residual| residual.ac.iter().flatten())
        .any(|&level| level != 0);
    let chroma_pattern = if has_chroma_ac {
        2
    } else {
        u8::from(
            chroma_residuals
                .iter()
                .flat_map(|residual| residual.dc)
                .any(|level| level != 0),
        )
    };
    let coded_block_pattern = luma_pattern | chroma_pattern << 4;
    let pattern_code = INTRA4_CODED_BLOCK_PATTERN
        .iter()
        .position(|&pattern| pattern == coded_block_pattern)
        .expect("every Intra8 coded-block pattern has an Exp-Golomb mapping");

    write_ue(writer, 0)?; // I_NxN
    writer.write_bit(true)?; // transform_size_8x8_flag
    for group in 0..4 {
        let block_index = group * 4;
        let predicted = predicted_intra4_mode(luma_modes, address, block_index, macroblocks_wide);
        let mode = luma_modes[address][block_index];
        writer.write_bit(mode == predicted)?;
        if mode != predicted {
            let remaining = mode - u8::from(mode > predicted);
            writer.write_bits(u64::from(remaining), 3)?;
        }
    }
    write_ue(writer, u64::from(chroma_mode))?;
    write_ue(writer, pattern_code as u64)?;
    if coded_block_pattern != 0 {
        write_se(writer, i64::from(qp_delta))?;
    }
    for (group, group_levels) in levels.iter().enumerate() {
        if luma_pattern & (1 << group) != 0 {
            for sub_block in 0..4 {
                let block_index = group * 4 + sub_block;
                let block_levels: [i32; 16] =
                    std::array::from_fn(|coefficient| group_levels[coefficient * 4 + sub_block]);
                let n_c = luma_nc(luma_nonzero, address, block_index, macroblocks_wide);
                luma_nonzero[address][block_index] =
                    encode_residual_block(writer, n_c, &block_levels)?;
            }
        }
    }
    if chroma_pattern != 0 {
        for residual in &chroma_residuals {
            encode_residual_block(writer, -1, &residual.dc)?;
        }
    }
    if chroma_pattern == 2 {
        for (component, residual) in chroma_residuals.iter().enumerate() {
            for (block_index, block_levels) in residual.ac.iter().enumerate() {
                let n_c = chroma_nc(
                    chroma_nonzero,
                    address,
                    component,
                    block_index,
                    macroblocks_wide,
                );
                chroma_nonzero[address][component][block_index] =
                    encode_residual_block(writer, n_c, block_levels)?;
            }
        }
    }
    for (component, prediction) in chroma_predictions.iter().enumerate() {
        reconstruct_chroma(
            &mut reconstructed[component + 1],
            coded_width / 2,
            macroblock_x,
            macroblock_y,
            prediction,
            &chroma_residuals[component],
            chroma_qp,
            false,
            Some(chroma_mode),
            scaling_matrix.four_by_four(true),
        );
    }
    Ok(coded_block_pattern != 0)
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn encode_intra4_macroblock(
    writer: &mut BitWriter,
    source: &[Vec<u8>; 3],
    reconstructed: &mut [Vec<u8>; 3],
    luma_modes: &mut [[u8; 16]],
    luma_nonzero: &mut [[u8; 16]],
    chroma_nonzero: &mut [[[u8; 4]; 2]],
    address: usize,
    macroblocks_wide: usize,
    macroblock_x: usize,
    macroblock_y: usize,
    coded_width: usize,
    qp: i32,
    qp_delta: i32,
    transform_8x8_mode: bool,
    transform_bypass: bool,
    scaling_matrix: ScalingMatrixPreset,
) -> Result<bool> {
    let mut levels = [[0_i32; 16]; 16];
    for block_index in 0..16 {
        let (block_x, block_y) = luma_block_position(block_index);
        let origin_x = macroblock_x * 16 + block_x * 4;
        let origin_y = macroblock_y * 16 + block_y * 4;
        let (mode, prediction) = (0..=8)
            .filter_map(|mode| {
                intra4_prediction(
                    &reconstructed[0],
                    coded_width,
                    origin_x,
                    origin_y,
                    block_index,
                    mode,
                    macroblock_y > 0,
                    macroblock_x > 0,
                )
                .map(|prediction| {
                    let sad =
                        block_sad(&source[0], coded_width, origin_x, origin_y, 4, &prediction);
                    (mode, prediction, sad)
                })
            })
            .min_by_key(|(_, _, sad)| *sad)
            .map(|(mode, prediction, _)| (mode, prediction))
            .expect("Intra4 DC prediction is always available");
        luma_modes[address][block_index] = mode;
        levels[block_index] = quantize_intra4_luma_block(
            &source[0],
            coded_width,
            origin_x,
            origin_y,
            &prediction,
            qp,
            transform_bypass,
            mode,
            scaling_matrix.four_by_four(true),
        );
        if transform_bypass {
            let samples: [u8; 16] = std::array::from_fn(|index| {
                source[0][(origin_y + index / 4) * coded_width + origin_x + index % 4]
            });
            place_block(
                &mut reconstructed[0],
                coded_width,
                origin_x,
                origin_y,
                4,
                &samples,
            );
        } else {
            reconstruct_intra4_luma_block(
                &mut reconstructed[0],
                coded_width,
                origin_x,
                origin_y,
                &prediction,
                &levels[block_index],
                qp,
                scaling_matrix.four_by_four(true),
            );
        }
    }

    let (chroma_mode, chroma_predictions) = select_chroma_prediction(
        [&source[1], &source[2]],
        [&reconstructed[1], &reconstructed[2]],
        coded_width / 2,
        macroblock_x,
        macroblock_y,
    );
    let chroma_qp = chroma_qp(qp);
    let chroma_residuals: [ChromaResidual; 2] = std::array::from_fn(|component| {
        quantize_chroma_residual(
            &source[component + 1],
            coded_width / 2,
            macroblock_x,
            macroblock_y,
            &chroma_predictions[component],
            chroma_qp,
            transform_bypass,
            Some(chroma_mode),
            scaling_matrix.four_by_four(true),
        )
    });
    let luma_pattern = (0..4).fold(0_u8, |pattern, group| {
        pattern
            | (u8::from(
                levels[group * 4..group * 4 + 4]
                    .iter()
                    .flatten()
                    .any(|&level| level != 0),
            ) << group)
    });
    let has_chroma_ac = chroma_residuals
        .iter()
        .flat_map(|residual| residual.ac.iter().flatten())
        .any(|&level| level != 0);
    let chroma_pattern = if has_chroma_ac {
        2
    } else {
        u8::from(
            chroma_residuals
                .iter()
                .flat_map(|residual| residual.dc)
                .any(|level| level != 0),
        )
    };
    let coded_block_pattern = luma_pattern | chroma_pattern << 4;
    let pattern_code = INTRA4_CODED_BLOCK_PATTERN
        .iter()
        .position(|&pattern| pattern == coded_block_pattern)
        .expect("every Intra4 coded-block pattern has an Exp-Golomb mapping");

    write_ue(writer, 0)?; // I_NxN
    if transform_8x8_mode {
        writer.write_bit(false)?; // transform_size_8x8_flag
    }
    for block_index in 0..16 {
        let predicted = predicted_intra4_mode(luma_modes, address, block_index, macroblocks_wide);
        let mode = luma_modes[address][block_index];
        writer.write_bit(mode == predicted)?;
        if mode != predicted {
            let remaining = mode - u8::from(mode > predicted);
            writer.write_bits(u64::from(remaining), 3)?;
        }
    }
    write_ue(writer, u64::from(chroma_mode))?;
    write_ue(writer, pattern_code as u64)?;
    if coded_block_pattern != 0 {
        write_se(writer, i64::from(qp_delta))?;
    }
    for group in 0..4 {
        if luma_pattern & (1 << group) != 0 {
            for block_index in group * 4..group * 4 + 4 {
                let n_c = luma_nc(luma_nonzero, address, block_index, macroblocks_wide);
                luma_nonzero[address][block_index] =
                    encode_residual_block(writer, n_c, &levels[block_index])?;
            }
        }
    }
    if chroma_pattern != 0 {
        for residual in &chroma_residuals {
            encode_residual_block(writer, -1, &residual.dc)?;
        }
    }
    if chroma_pattern == 2 {
        for (component, residual) in chroma_residuals.iter().enumerate() {
            for (block_index, block_levels) in residual.ac.iter().enumerate() {
                let n_c = chroma_nc(
                    chroma_nonzero,
                    address,
                    component,
                    block_index,
                    macroblocks_wide,
                );
                chroma_nonzero[address][component][block_index] =
                    encode_residual_block(writer, n_c, block_levels)?;
            }
        }
    }
    for (component, prediction) in chroma_predictions.iter().enumerate() {
        reconstruct_chroma(
            &mut reconstructed[component + 1],
            coded_width / 2,
            macroblock_x,
            macroblock_y,
            prediction,
            &chroma_residuals[component],
            chroma_qp,
            transform_bypass,
            Some(chroma_mode),
            scaling_matrix.four_by_four(true),
        );
    }
    Ok(coded_block_pattern != 0)
}

fn predicted_intra4_mode(
    modes: &[[u8; 16]],
    address: usize,
    block_index: usize,
    macroblocks_wide: usize,
) -> u8 {
    let (block_x, block_y) = luma_block_position(block_index);
    let left = if block_x > 0 {
        Some(modes[address][luma_block_index(block_x - 1, block_y)])
    } else if !address.is_multiple_of(macroblocks_wide) {
        Some(modes[address - 1][luma_block_index(3, block_y)])
    } else {
        None
    };
    let top = if block_y > 0 {
        Some(modes[address][luma_block_index(block_x, block_y - 1)])
    } else if address >= macroblocks_wide {
        Some(modes[address - macroblocks_wide][luma_block_index(block_x, 3)])
    } else {
        None
    };
    match (left, top) {
        (Some(left), Some(top)) => left.min(top),
        _ => 2,
    }
}

const INTRA4_CODED_BLOCK_PATTERN: [u8; 48] = [
    47, 31, 15, 0, 23, 27, 29, 30, 7, 11, 13, 14, 39, 43, 45, 46, 16, 3, 5, 10, 12, 19, 21, 26, 28,
    35, 37, 42, 44, 1, 2, 4, 8, 17, 18, 20, 24, 6, 9, 22, 25, 32, 33, 34, 36, 40, 38, 41,
];

#[allow(clippy::too_many_arguments)]
#[allow(clippy::too_many_lines)]
fn intra8_prediction(
    plane: &[u8],
    stride: usize,
    origin_x: usize,
    origin_y: usize,
    group: usize,
    mode: u8,
    top_macroblock_available: bool,
    left_macroblock_available: bool,
) -> Option<[u8; 64]> {
    let top_available = group / 2 > 0 || top_macroblock_available;
    let left_available = !group.is_multiple_of(2) || left_macroblock_available;
    let top = top_available.then(|| {
        let mut samples = [0_u8; 16];
        for x in 0..8 {
            samples[x] = plane[(origin_y - 1) * stride + origin_x + x];
        }
        for x in 8..16 {
            samples[x] = if group == 3 || origin_x + x >= stride {
                samples[7]
            } else {
                plane[(origin_y - 1) * stride + origin_x + x]
            };
        }
        samples
    });
    let left: Option<[u8; 8]> = left_available
        .then(|| std::array::from_fn(|y| plane[(origin_y + y) * stride + origin_x - 1]));
    let corner =
        (top_available && left_available).then(|| plane[(origin_y - 1) * stride + origin_x - 1]);
    let top = top.map(|samples| {
        let mut filtered = [0_u8; 16];
        filtered[0] = corner.map_or_else(
            || average_1_3(samples[1], samples[0]),
            |corner| filter_1_2_1(corner, samples[0], samples[1]),
        );
        for index in 1..15 {
            filtered[index] = filter_1_2_1(samples[index - 1], samples[index], samples[index + 1]);
        }
        filtered[15] = average_1_3(samples[14], samples[15]);
        filtered
    });
    let left = left.map(|samples| {
        let mut filtered = [0_u8; 8];
        filtered[0] = corner.map_or_else(
            || average_1_3(samples[1], samples[0]),
            |corner| filter_1_2_1(corner, samples[0], samples[1]),
        );
        for index in 1..7 {
            filtered[index] = filter_1_2_1(samples[index - 1], samples[index], samples[index + 1]);
        }
        filtered[7] = average_1_3(samples[6], samples[7]);
        filtered
    });
    let mut output = [0_u8; 64];
    match mode {
        0 => {
            let top = top?;
            for row in output.as_chunks_mut::<8>().0 {
                row.copy_from_slice(&top[..8]);
            }
        }
        1 => {
            let left = left?;
            for (row, sample) in output.as_chunks_mut::<8>().0.iter_mut().zip(left) {
                row.fill(sample);
            }
        }
        2 => output.fill(dc_value(
            top.as_ref().map(|samples| &samples[..8]),
            left.as_ref().map(<[u8; 8]>::as_slice),
        )),
        3 => {
            let top = top?;
            for y in 0..8 {
                for x in 0..8 {
                    output[y * 8 + x] = if x == 7 && y == 7 {
                        average_1_3(top[14], top[15])
                    } else {
                        filter_1_2_1(top[x + y], top[x + y + 1], top[x + y + 2])
                    };
                }
            }
        }
        4 => {
            let top = top?;
            let left = left?;
            let corner = corner?;
            for y in 0..8 {
                for x in 0..8 {
                    let x_coordinate = i32::try_from(x).expect("Intra8 coordinate fits i32");
                    let y_coordinate = i32::try_from(y).expect("Intra8 coordinate fits i32");
                    output[y * 8 + x] = match x_coordinate.cmp(&y_coordinate) {
                        std::cmp::Ordering::Greater => filter_1_2_1(
                            top_or_corner_intra8(top, corner, x_coordinate - y_coordinate - 2),
                            top_or_corner_intra8(top, corner, x_coordinate - y_coordinate - 1),
                            top_or_corner_intra8(top, corner, x_coordinate - y_coordinate),
                        ),
                        std::cmp::Ordering::Less => filter_1_2_1(
                            left_or_corner_intra8(left, corner, y_coordinate - x_coordinate - 2),
                            left_or_corner_intra8(left, corner, y_coordinate - x_coordinate - 1),
                            left_or_corner_intra8(left, corner, y_coordinate - x_coordinate),
                        ),
                        std::cmp::Ordering::Equal => filter_1_2_1(top[0], corner, left[0]),
                    };
                }
            }
        }
        5 => predict_vertical_right_intra8(&mut output, top?, left?, corner?),
        6 => predict_horizontal_down_intra8(&mut output, top?, left?, corner?),
        7 => {
            let top = top?;
            for y in 0..8 {
                for x in 0..8 {
                    let start = x + y / 2;
                    output[y * 8 + x] = if y.is_multiple_of(2) {
                        average(top[start], top[start + 1])
                    } else {
                        filter_1_2_1(top[start], top[start + 1], top[start + 2])
                    };
                }
            }
        }
        8 => {
            let left = left?;
            for y in 0..8 {
                for x in 0..8 {
                    let z = x + 2 * y;
                    output[y * 8 + x] = match z.cmp(&13) {
                        std::cmp::Ordering::Less if z.is_multiple_of(2) => {
                            average(left[z / 2], left[z / 2 + 1])
                        }
                        std::cmp::Ordering::Less => {
                            filter_1_2_1(left[z / 2], left[z / 2 + 1], left[z / 2 + 2])
                        }
                        std::cmp::Ordering::Equal => filter_1_2_1(left[6], left[7], left[7]),
                        std::cmp::Ordering::Greater => left[7],
                    };
                }
            }
        }
        _ => return None,
    }
    Some(output)
}

fn top_or_corner_intra8(top: [u8; 16], corner: u8, index: i32) -> u8 {
    if index < 0 {
        corner
    } else {
        top[usize::try_from(index).expect("bounded Intra8 top index")]
    }
}

fn left_or_corner_intra8(left: [u8; 8], corner: u8, index: i32) -> u8 {
    if index < 0 {
        corner
    } else {
        left[usize::try_from(index).expect("bounded Intra8 left index")]
    }
}

fn predict_vertical_right_intra8(output: &mut [u8; 64], top: [u8; 16], left: [u8; 8], corner: u8) {
    for y in 0..8_i32 {
        for x in 0..8_i32 {
            let z = 2 * x - y;
            let value = if z >= 0 {
                if z % 2 == 0 {
                    average(
                        top_or_corner_intra8(top, corner, x - y / 2 - 1),
                        top_or_corner_intra8(top, corner, x - y / 2),
                    )
                } else {
                    filter_1_2_1(
                        top_or_corner_intra8(top, corner, x - y / 2 - 2),
                        top_or_corner_intra8(top, corner, x - y / 2 - 1),
                        top_or_corner_intra8(top, corner, x - y / 2),
                    )
                }
            } else if z == -1 {
                filter_1_2_1(left[0], corner, top[0])
            } else {
                filter_1_2_1(
                    left_or_corner_intra8(left, corner, y - 2 * x - 1),
                    left_or_corner_intra8(left, corner, y - 2 * x - 2),
                    left_or_corner_intra8(left, corner, y - 2 * x - 3),
                )
            };
            output[usize::try_from(y * 8 + x).expect("Intra8 index fits")] = value;
        }
    }
}

fn predict_horizontal_down_intra8(output: &mut [u8; 64], top: [u8; 16], left: [u8; 8], corner: u8) {
    for y in 0..8_i32 {
        for x in 0..8_i32 {
            let z = 2 * y - x;
            let value = if z >= 0 {
                if z % 2 == 0 {
                    average(
                        left_or_corner_intra8(left, corner, y - x / 2 - 1),
                        left_or_corner_intra8(left, corner, y - x / 2),
                    )
                } else {
                    filter_1_2_1(
                        left_or_corner_intra8(left, corner, y - x / 2 - 2),
                        left_or_corner_intra8(left, corner, y - x / 2 - 1),
                        left_or_corner_intra8(left, corner, y - x / 2),
                    )
                }
            } else if z == -1 {
                filter_1_2_1(top[0], corner, left[0])
            } else {
                filter_1_2_1(
                    top_or_corner_intra8(top, corner, x - 2 * y - 1),
                    top_or_corner_intra8(top, corner, x - 2 * y - 2),
                    top_or_corner_intra8(top, corner, x - 2 * y - 3),
                )
            };
            output[usize::try_from(y * 8 + x).expect("Intra8 index fits")] = value;
        }
    }
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn intra4_prediction(
    plane: &[u8],
    stride: usize,
    origin_x: usize,
    origin_y: usize,
    block_index: usize,
    mode: u8,
    top_macroblock_available: bool,
    left_macroblock_available: bool,
) -> Option<[u8; 16]> {
    let block_x = origin_x % 16;
    let block_y = origin_y % 16;
    let top_available = block_y > 0 || top_macroblock_available;
    let left_available = block_x > 0 || left_macroblock_available;
    let top = top_available.then(|| {
        let mut samples = [0_u8; 8];
        samples[..4].copy_from_slice(&plane[(origin_y - 1) * stride + origin_x..][..4]);
        for x in 4..8 {
            let crosses_into_future_macroblock =
                !origin_y.is_multiple_of(16) && origin_x % 16 + x >= 16;
            samples[x] = if matches!(block_index, 3 | 11)
                || crosses_into_future_macroblock
                || origin_x + x >= stride
            {
                samples[3]
            } else {
                plane[(origin_y - 1) * stride + origin_x + x]
            };
        }
        samples
    });
    let left = left_available
        .then(|| std::array::from_fn(|y| plane[(origin_y + y) * stride + origin_x - 1]));
    let corner =
        (top_available && left_available).then(|| plane[(origin_y - 1) * stride + origin_x - 1]);
    let mut output = [0_u8; 16];
    match mode {
        0 => {
            let top = top?;
            for row in output.as_chunks_mut::<4>().0 {
                row.copy_from_slice(&top[..4]);
            }
        }
        1 => {
            let left = left?;
            for (row, sample) in output.as_chunks_mut::<4>().0.iter_mut().zip(left) {
                row.fill(sample);
            }
        }
        2 => output.fill(dc_value(
            top.as_ref().map(|samples| &samples[..4]),
            left.as_ref().map(<[u8; 4]>::as_slice),
        )),
        3 => {
            let top = top?;
            for y in 0_usize..4 {
                for x in 0_usize..4 {
                    output[y * 4 + x] = if x == 3 && y == 3 {
                        average_1_3(top[6], top[7])
                    } else {
                        filter_1_2_1(top[x + y], top[x + y + 1], top[x + y + 2])
                    };
                }
            }
        }
        4 => {
            let top = top?;
            let left = left?;
            let corner = corner?;
            for y in 0_usize..4 {
                for x in 0_usize..4 {
                    let x = i32::try_from(x).expect("Intra4 coordinate fits i32");
                    let y = i32::try_from(y).expect("Intra4 coordinate fits i32");
                    output[usize::try_from(y * 4 + x).expect("Intra4 index fits")] = match x.cmp(&y)
                    {
                        std::cmp::Ordering::Greater => filter_1_2_1(
                            top_or_corner(top, corner, x - y - 2),
                            top_or_corner(top, corner, x - y - 1),
                            top_or_corner(top, corner, x - y),
                        ),
                        std::cmp::Ordering::Less => filter_1_2_1(
                            left_or_corner(left, corner, y - x - 2),
                            left_or_corner(left, corner, y - x - 1),
                            left_or_corner(left, corner, y - x),
                        ),
                        std::cmp::Ordering::Equal => filter_1_2_1(top[0], corner, left[0]),
                    };
                }
            }
        }
        5 => predict_vertical_right(&mut output, top?, left?, corner?),
        6 => predict_horizontal_down(&mut output, top?, left?, corner?),
        7 => {
            let top = top?;
            for y in 0..4 {
                for x in 0..4 {
                    let start = x + y / 2;
                    output[y * 4 + x] = if y.is_multiple_of(2) {
                        average(top[start], top[start + 1])
                    } else {
                        filter_1_2_1(top[start], top[start + 1], top[start + 2])
                    };
                }
            }
        }
        8 => {
            let left = left?;
            for y in 0..4 {
                for x in 0..4 {
                    let z = x + 2 * y;
                    output[y * 4 + x] = match z {
                        0 | 2 | 4 => average(left[y + x / 2], left[y + x / 2 + 1]),
                        1 | 3 => {
                            filter_1_2_1(left[y + x / 2], left[y + x / 2 + 1], left[y + x / 2 + 2])
                        }
                        5 => average_1_3(left[2], left[3]),
                        _ => left[3],
                    };
                }
            }
        }
        _ => return None,
    }
    Some(output)
}

fn predict_vertical_right(output: &mut [u8; 16], top: [u8; 8], left: [u8; 4], corner: u8) {
    for y in 0_i32..4 {
        for x in 0_i32..4 {
            let z = 2 * x - y;
            output[usize::try_from(y * 4 + x).expect("Intra4 index fits")] = match z {
                0 | 2 | 4 | 6 => average(
                    top_or_corner(top, corner, x - y / 2 - 1),
                    top_or_corner(top, corner, x - y / 2),
                ),
                1 | 3 | 5 => filter_1_2_1(
                    top_or_corner(top, corner, x - y / 2 - 2),
                    top_or_corner(top, corner, x - y / 2 - 1),
                    top_or_corner(top, corner, x - y / 2),
                ),
                -1 => filter_1_2_1(left[0], corner, top[0]),
                _ => filter_1_2_1(
                    left_or_corner(left, corner, y - 1),
                    left_or_corner(left, corner, y - 2),
                    left_or_corner(left, corner, y - 3),
                ),
            };
        }
    }
}

fn predict_horizontal_down(output: &mut [u8; 16], top: [u8; 8], left: [u8; 4], corner: u8) {
    for y in 0_i32..4 {
        for x in 0_i32..4 {
            let z = 2 * y - x;
            output[usize::try_from(y * 4 + x).expect("Intra4 index fits")] = match z {
                0 | 2 | 4 | 6 => average(
                    left_or_corner(left, corner, y - x / 2 - 1),
                    left_or_corner(left, corner, y - x / 2),
                ),
                1 | 3 | 5 => filter_1_2_1(
                    left_or_corner(left, corner, y - x / 2 - 2),
                    left_or_corner(left, corner, y - x / 2 - 1),
                    left_or_corner(left, corner, y - x / 2),
                ),
                -1 => filter_1_2_1(top[0], corner, left[0]),
                _ => filter_1_2_1(
                    top_or_corner(top, corner, x - 1),
                    top_or_corner(top, corner, x - 2),
                    top_or_corner(top, corner, x - 3),
                ),
            };
        }
    }
}

fn top_or_corner(top: [u8; 8], corner: u8, index: i32) -> u8 {
    if index < 0 {
        corner
    } else {
        top[usize::try_from(index).expect("bounded Intra4 top index")]
    }
}

fn left_or_corner(left: [u8; 4], corner: u8, index: i32) -> u8 {
    if index < 0 {
        corner
    } else {
        left[usize::try_from(index).expect("bounded Intra4 left index")]
    }
}

fn average(a: u8, b: u8) -> u8 {
    u8::try_from((u16::from(a) + u16::from(b) + 1) >> 1).expect("u8 average fits")
}

fn filter_1_2_1(a: u8, b: u8, c: u8) -> u8 {
    u8::try_from((u16::from(a) + 2 * u16::from(b) + u16::from(c) + 2) >> 2)
        .expect("u8 weighted average fits")
}

fn average_1_3(a: u8, b: u8) -> u8 {
    u8::try_from((u16::from(a) + 3 * u16::from(b) + 2) >> 2).expect("u8 weighted average fits")
}

fn select_luma_prediction(
    source: &[u8],
    reconstructed: &[u8],
    stride: usize,
    macroblock_x: usize,
    macroblock_y: usize,
) -> (u8, Vec<u8>) {
    let origin_x = macroblock_x * 16;
    let origin_y = macroblock_y * 16;
    let top =
        (macroblock_y > 0).then(|| &reconstructed[(origin_y - 1) * stride + origin_x..][..16]);
    let left = (macroblock_x > 0).then(|| {
        (0..16)
            .map(|y| reconstructed[(origin_y + y) * stride + origin_x - 1])
            .collect::<Vec<_>>()
    });
    let mut candidates = Vec::with_capacity(3);
    if let Some(top) = top {
        candidates.push((
            0,
            (0..16)
                .flat_map(|_| top.iter().copied())
                .collect::<Vec<_>>(),
        ));
    }
    if let Some(left) = &left {
        candidates.push((
            1,
            left.iter()
                .flat_map(|&sample| std::iter::repeat_n(sample, 16))
                .collect::<Vec<_>>(),
        ));
    }
    candidates.push((2, vec![dc_value(top, left.as_deref()); 256]));
    candidates
        .into_iter()
        .min_by_key(|(_, prediction)| block_sad(source, stride, origin_x, origin_y, 16, prediction))
        .expect("DC prediction is always available")
}

fn select_chroma_prediction(
    sources: [&[u8]; 2],
    reconstructed: [&[u8]; 2],
    stride: usize,
    macroblock_x: usize,
    macroblock_y: usize,
) -> (u8, [Vec<u8>; 2]) {
    let origin_x = macroblock_x * 8;
    let origin_y = macroblock_y * 8;
    let mut modes = vec![0];
    if macroblock_x > 0 {
        modes.push(1);
    }
    if macroblock_y > 0 {
        modes.push(2);
    }
    modes
        .into_iter()
        .map(|mode| {
            let predictions = std::array::from_fn(|component| {
                chroma_prediction(
                    reconstructed[component],
                    stride,
                    macroblock_x,
                    macroblock_y,
                    mode,
                )
            });
            let sad = predictions
                .iter()
                .enumerate()
                .map(|(component, prediction)| {
                    block_sad(
                        sources[component],
                        stride,
                        origin_x,
                        origin_y,
                        8,
                        prediction,
                    )
                })
                .sum::<u64>();
            (mode, predictions, sad)
        })
        .min_by_key(|(_, _, sad)| *sad)
        .map(|(mode, predictions, _)| (mode, predictions))
        .expect("chroma DC prediction is always available")
}

fn chroma_prediction(
    plane: &[u8],
    stride: usize,
    macroblock_x: usize,
    macroblock_y: usize,
    mode: u8,
) -> Vec<u8> {
    let origin_x = macroblock_x * 8;
    let origin_y = macroblock_y * 8;
    let top = (macroblock_y > 0).then(|| &plane[(origin_y - 1) * stride + origin_x..][..8]);
    let left = (macroblock_x > 0).then(|| {
        (0..8)
            .map(|y| plane[(origin_y + y) * stride + origin_x - 1])
            .collect::<Vec<_>>()
    });
    match mode {
        0 => predict_chroma_dc(top, left.as_deref()),
        1 => left
            .expect("horizontal chroma mode requires left samples")
            .into_iter()
            .flat_map(|sample| std::iter::repeat_n(sample, 8))
            .collect(),
        2 => (0..8)
            .flat_map(|_| {
                top.expect("vertical chroma mode requires top samples")
                    .iter()
                    .copied()
            })
            .collect(),
        _ => unreachable!("encoder considers only DC, horizontal, and vertical chroma modes"),
    }
}

fn predict_chroma_dc(top: Option<&[u8]>, left: Option<&[u8]>) -> Vec<u8> {
    let mut output = vec![0; 64];
    for block_y in 0..2 {
        for block_x in 0..2 {
            let top_samples = top.map(|samples| &samples[block_x * 4..][..4]);
            let left_samples = left.map(|samples| &samples[block_y * 4..][..4]);
            let use_top =
                top_samples.is_some() && (left_samples.is_none() || block_y == 0 || block_x == 1);
            let use_left =
                left_samples.is_some() && (top_samples.is_none() || block_x == 0 || block_y == 1);
            let value = dc_value(
                use_top.then_some(top_samples).flatten(),
                use_left.then_some(left_samples).flatten(),
            );
            for y in block_y * 4..block_y * 4 + 4 {
                output[y * 8 + block_x * 4..y * 8 + block_x * 4 + 4].fill(value);
            }
        }
    }
    output
}

fn dc_value(top: Option<&[u8]>, left: Option<&[u8]>) -> u8 {
    let (sum, count) = top
        .into_iter()
        .chain(left)
        .flatten()
        .fold((0_u32, 0_u32), |(sum, count), &sample| {
            (sum + u32::from(sample), count + 1)
        });
    let average = (sum + count / 2).checked_div(count).unwrap_or(128);
    u8::try_from(average).expect("average of u8 samples fits u8")
}

fn block_sad(
    source: &[u8],
    stride: usize,
    origin_x: usize,
    origin_y: usize,
    size: usize,
    prediction: &[u8],
) -> u64 {
    (0..size)
        .flat_map(|y| {
            (0..size).map(move |x| {
                u64::from(
                    source[(origin_y + y) * stride + origin_x + x]
                        .abs_diff(prediction[y * size + x]),
                )
            })
        })
        .sum()
}

const ZIG_ZAG_4X4: [(usize, usize); 16] = [
    (0, 0),
    (0, 1),
    (1, 0),
    (2, 0),
    (1, 1),
    (0, 2),
    (0, 3),
    (1, 2),
    (2, 1),
    (3, 0),
    (3, 1),
    (2, 2),
    (1, 3),
    (2, 3),
    (3, 2),
    (3, 3),
];

const ZIG_ZAG_8X8: [(usize, usize); 64] = [
    (0, 0),
    (0, 1),
    (1, 0),
    (2, 0),
    (1, 1),
    (0, 2),
    (0, 3),
    (1, 2),
    (2, 1),
    (3, 0),
    (4, 0),
    (3, 1),
    (2, 2),
    (1, 3),
    (0, 4),
    (0, 5),
    (1, 4),
    (2, 3),
    (3, 2),
    (4, 1),
    (5, 0),
    (6, 0),
    (5, 1),
    (4, 2),
    (3, 3),
    (2, 4),
    (1, 5),
    (0, 6),
    (0, 7),
    (1, 6),
    (2, 5),
    (3, 4),
    (4, 3),
    (5, 2),
    (6, 1),
    (7, 0),
    (7, 1),
    (6, 2),
    (5, 3),
    (4, 4),
    (3, 5),
    (2, 6),
    (1, 7),
    (2, 7),
    (3, 6),
    (4, 5),
    (5, 4),
    (6, 3),
    (7, 2),
    (7, 3),
    (6, 4),
    (5, 5),
    (4, 6),
    (3, 7),
    (4, 7),
    (5, 6),
    (6, 5),
    (7, 4),
    (7, 5),
    (6, 6),
    (5, 7),
    (6, 7),
    (7, 6),
    (7, 7),
];
const HADAMARD_4X4: [[i32; 4]; 4] = [[1, 1, 1, 1], [1, 1, -1, -1], [1, -1, -1, 1], [1, -1, 1, -1]];

fn quantize_intra16_luma_dc(
    source: &[u8],
    stride: usize,
    macroblock_x: usize,
    macroblock_y: usize,
    prediction: &[u8],
    qp: i32,
    scaling_list: &[u8; 16],
) -> [i32; 16] {
    let origin_x = macroblock_x * 16;
    let origin_y = macroblock_y * 16;
    let mut desired_dc = [0_i32; 16];
    for block_y in 0..4 {
        for block_x in 0..4 {
            let mut sum = 0_i64;
            for y in 0..4 {
                for x in 0..4 {
                    let macroblock_offset = (block_y * 4 + y) * 16 + block_x * 4 + x;
                    let source_offset =
                        (origin_y + block_y * 4 + y) * stride + origin_x + block_x * 4 + x;
                    sum +=
                        i64::from(source[source_offset]) - i64::from(prediction[macroblock_offset]);
                }
            }
            desired_dc[block_y * 4 + block_x] =
                i32::try_from(round_div(sum, 16) * 64).expect("bounded residual fits i32");
        }
    }
    let mut natural_levels = [0_i32; 16];
    for row in 0..4 {
        for column in 0..4 {
            let transformed = (0..4)
                .flat_map(|inner_row| {
                    (0..4).map(move |inner_column| {
                        i64::from(HADAMARD_4X4[row][inner_row])
                            * i64::from(desired_dc[inner_row * 4 + inner_column])
                            * i64::from(HADAMARD_4X4[inner_column][column])
                    })
                })
                .sum();
            let scale = i64::from(level_scale_4x4(qp, 0, 0, scaling_list));
            let level = if qp >= 36 {
                round_div(transformed, 16 * scale * (1_i64 << (qp / 6 - 6)))
            } else {
                round_div(transformed * (1_i64 << (6 - qp / 6)), 16 * scale)
            };
            natural_levels[row * 4 + column] = i32::try_from(level).expect("quantized DC fits i32");
        }
    }
    std::array::from_fn(|index| {
        let (row, column) = ZIG_ZAG_4X4[index];
        natural_levels[row * 4 + column]
    })
}

fn quantize_intra16_luma_ac(
    source: &[u8],
    stride: usize,
    macroblock_x: usize,
    macroblock_y: usize,
    prediction: &[u8],
    qp: i32,
    scaling_list: &[u8; 16],
) -> [[i32; 15]; 16] {
    let origin_x = macroblock_x * 16;
    let origin_y = macroblock_y * 16;
    let mut output = [[0_i32; 15]; 16];
    for block_y in 0..4 {
        for block_x in 0..4 {
            let mut residual = [0_i32; 16];
            for y in 0..4 {
                for x in 0..4 {
                    let macroblock_offset = (block_y * 4 + y) * 16 + block_x * 4 + x;
                    let source_offset =
                        (origin_y + block_y * 4 + y) * stride + origin_x + block_x * 4 + x;
                    residual[y * 4 + x] =
                        i32::from(source[source_offset]) - i32::from(prediction[macroblock_offset]);
                }
            }
            let transformed = forward_transform_4x4(&residual);
            let block_index = luma_block_index(block_x, block_y);
            for (destination, &(row, column)) in
                output[block_index].iter_mut().zip(&ZIG_ZAG_4X4[1..])
            {
                *destination = quantize_coefficient(
                    transformed[row * 4 + column],
                    qp,
                    row,
                    column,
                    scaling_list,
                );
            }
        }
    }
    output
}

#[allow(clippy::too_many_arguments)]
fn quantize_chroma_residual(
    source: &[u8],
    stride: usize,
    macroblock_x: usize,
    macroblock_y: usize,
    prediction: &[u8],
    qp: i32,
    transform_bypass: bool,
    intra_mode: Option<u8>,
    scaling_list: &[u8; 16],
) -> ChromaResidual {
    let origin_x = macroblock_x * 8;
    let origin_y = macroblock_y * 8;
    if transform_bypass {
        let mut residual: [i32; 64] = std::array::from_fn(|index| {
            let x = index % 8;
            let y = index / 8;
            i32::from(source[(origin_y + y) * stride + origin_x + x]) - i32::from(prediction[index])
        });
        match intra_mode {
            Some(1) => {
                for y in 0..8 {
                    for x in (1..8).rev() {
                        residual[y * 8 + x] -= residual[y * 8 + x - 1];
                    }
                }
            }
            Some(2) => {
                for y in (1..8).rev() {
                    for x in 0..8 {
                        residual[y * 8 + x] -= residual[(y - 1) * 8 + x];
                    }
                }
            }
            _ => {}
        }
        let mut dc = [0_i32; 4];
        let mut ac = [[0_i32; 15]; 4];
        for block_y in 0..2 {
            for block_x in 0..2 {
                let block_index = block_y * 2 + block_x;
                for (coefficient, &(row, column)) in ZIG_ZAG_4X4.iter().enumerate() {
                    let level = residual[(block_y * 4 + row) * 8 + block_x * 4 + column];
                    if coefficient == 0 {
                        dc[block_index] = level;
                    } else {
                        ac[block_index][coefficient - 1] = level;
                    }
                }
            }
        }
        return ChromaResidual { dc, ac };
    }
    let mut transformed_blocks = [[0_i32; 16]; 4];
    let mut ac = [[0_i32; 15]; 4];
    for block_y in 0..2 {
        for block_x in 0..2 {
            let block_index = block_y * 2 + block_x;
            let mut residual = [0_i32; 16];
            for y in 0..4 {
                for x in 0..4 {
                    let macroblock_offset = (block_y * 4 + y) * 8 + block_x * 4 + x;
                    let source_offset =
                        (origin_y + block_y * 4 + y) * stride + origin_x + block_x * 4 + x;
                    residual[y * 4 + x] =
                        i32::from(source[source_offset]) - i32::from(prediction[macroblock_offset]);
                }
            }
            transformed_blocks[block_index] = forward_transform_4x4(&residual);
            for (destination, &(row, column)) in ac[block_index].iter_mut().zip(&ZIG_ZAG_4X4[1..]) {
                *destination = quantize_coefficient(
                    transformed_blocks[block_index][row * 4 + column],
                    qp,
                    row,
                    column,
                    scaling_list,
                );
            }
        }
    }
    let values = transformed_blocks.map(|block| block[0]);
    let transformed_dc = [
        values[0] + values[1] + values[2] + values[3],
        values[0] - values[1] + values[2] - values[3],
        values[0] + values[1] - values[2] - values[3],
        values[0] - values[1] - values[2] + values[3],
    ];
    let dc_divisor = 4 * i64::from(level_scale_4x4(qp, 0, 0, scaling_list)) * (1_i64 << (qp / 6));
    let dc = transformed_dc.map(|value| {
        i32::try_from(round_div(i64::from(value) * 32, dc_divisor))
            .expect("quantized chroma DC fits i32")
    });
    ChromaResidual { dc, ac }
}

fn forward_transform_4x4(residual: &[i32; 16]) -> [i32; 16] {
    let mut horizontal = [0_i32; 16];
    for row in 0..4 {
        let offset = row * 4;
        let s0 = residual[offset] + residual[offset + 3];
        let s1 = residual[offset + 1] + residual[offset + 2];
        let s2 = residual[offset + 1] - residual[offset + 2];
        let s3 = residual[offset] - residual[offset + 3];
        horizontal[offset] = s0 + s1;
        horizontal[offset + 1] = s3 + s2;
        horizontal[offset + 2] = s0 - s1;
        horizontal[offset + 3] = s3 - s2;
    }
    let mut output = [0_i32; 16];
    for column in 0..4 {
        let s0 = horizontal[column] + horizontal[12 + column];
        let s1 = horizontal[4 + column] + horizontal[8 + column];
        let s2 = horizontal[4 + column] - horizontal[8 + column];
        let s3 = horizontal[column] - horizontal[12 + column];
        output[column] = s0 + s1;
        output[4 + column] = s3 + s2;
        output[8 + column] = s0 - s1;
        output[12 + column] = s3 - s2;
    }
    output
}

fn forward_transform_8x8(residual: &[i32; 64]) -> [i32; 64] {
    let mut horizontal = [0_i32; 64];
    for row in 0..8 {
        let input: [i32; 8] = std::array::from_fn(|column| residual[row * 8 + column]);
        let output = forward_transform_8(&input);
        horizontal[row * 8..row * 8 + 8].copy_from_slice(&output);
    }
    let mut transformed = [0_i32; 64];
    for column in 0..8 {
        let input: [i32; 8] = std::array::from_fn(|row| horizontal[row * 8 + column]);
        let output = forward_transform_8(&input);
        for row in 0..8 {
            transformed[row * 8 + column] = output[row];
        }
    }
    transformed
}

fn forward_transform_8(input: &[i32; 8]) -> [i32; 8] {
    let s07 = input[0] + input[7];
    let s16 = input[1] + input[6];
    let s25 = input[2] + input[5];
    let s34 = input[3] + input[4];
    let a0 = s07 + s34;
    let a1 = s16 + s25;
    let a2 = s07 - s34;
    let a3 = s16 - s25;
    let d07 = input[0] - input[7];
    let d16 = input[1] - input[6];
    let d25 = input[2] - input[5];
    let d34 = input[3] - input[4];
    let a4 = d16 + d25 + d07 + (d07 >> 1);
    let a5 = d07 - d34 - d25 - (d25 >> 1);
    let a6 = d07 + d34 - d16 - (d16 >> 1);
    let a7 = d16 - d25 + d34 + (d34 >> 1);
    [
        a0 + a1,
        a4 + (a7 >> 2),
        a2 + (a3 >> 1),
        a5 + (a6 >> 2),
        a0 - a1,
        a6 - (a5 >> 2),
        (a2 >> 1) - a3,
        (a4 >> 2) - a7,
    ]
}

fn quantize_intra8_luma_block(
    source: &[u8],
    stride: usize,
    origin_x: usize,
    origin_y: usize,
    prediction: &[u8; 64],
    qp: i32,
    scaling_list: &[u8; 64],
) -> [i32; 64] {
    let mut residual = [0_i32; 64];
    for y in 0..8 {
        for x in 0..8 {
            residual[y * 8 + x] = i32::from(source[(origin_y + y) * stride + origin_x + x])
                - i32::from(prediction[y * 8 + x]);
        }
    }
    let transformed = forward_transform_8x8(&residual);
    std::array::from_fn(|index| {
        let (row, column) = ZIG_ZAG_8X8[index];
        quantize_coefficient_8x8(transformed[row * 8 + column], qp, row, column, scaling_list)
    })
}

fn quantize_coefficient_8x8(
    coefficient: i32,
    qp: i32,
    row: usize,
    column: usize,
    scaling_list: &[u8; 64],
) -> i32 {
    const MULTIPLIERS: [[i64; 6]; 6] = [
        [13_107, 11_428, 20_972, 12_222, 16_777, 15_481],
        [11_916, 10_826, 19_174, 11_058, 14_980, 14_290],
        [10_082, 8_943, 15_978, 9_675, 12_710, 11_985],
        [9_362, 8_228, 14_913, 8_931, 11_984, 11_259],
        [8_192, 7_346, 13_159, 7_740, 10_486, 9_777],
        [7_282, 6_428, 11_570, 6_830, 9_118, 8_640],
    ];
    let category = coefficient_category_8x8(row, column);
    let qp_mod = usize::try_from(qp % 6).expect("non-negative QP");
    let q_bits = u32::try_from(16 + qp / 6).expect("bounded QP shift");
    let matrix = i64::from(scaling_list[row * 8 + column]);
    let divisor = (1_i64 << q_bits) * matrix;
    let numerator = i64::from(coefficient.unsigned_abs()) * MULTIPLIERS[qp_mod][category] * 16;
    let level = (numerator + divisor / 3) / divisor;
    let level = i32::try_from(level).expect("bounded 8x8 coefficient quantizes into i32");
    if coefficient < 0 { -level } else { level }
}

#[allow(clippy::too_many_arguments)]
fn reconstruct_intra8_luma_block(
    destination: &mut [u8],
    stride: usize,
    origin_x: usize,
    origin_y: usize,
    prediction: &[u8; 64],
    levels: &[i32; 64],
    qp: i32,
    scaling_list: &[u8; 64],
) {
    let residual = inverse_quantized_transform_8x8(levels, qp, scaling_list);
    let reconstructed: [u8; 64] = std::array::from_fn(|index| {
        u8::try_from((i64::from(prediction[index]) + residual[index]).clamp(0, 255))
            .expect("clamped Intra8 reconstruction fits u8")
    });
    place_block(destination, stride, origin_x, origin_y, 8, &reconstructed);
}

fn inverse_quantized_transform_8x8(
    levels: &[i32; 64],
    qp: i32,
    scaling_list: &[u8; 64],
) -> [i64; 64] {
    let mut scaled = [0_i64; 64];
    for (&level, &(row, column)) in levels.iter().zip(&ZIG_ZAG_8X8) {
        let value = i64::from(level) * i64::from(level_scale_8x8(qp, row, column, scaling_list));
        let shift = qp / 6;
        scaled[row * 8 + column] = if shift >= 6 {
            value << (shift - 6)
        } else {
            (value + (1_i64 << (5 - shift))) >> (6 - shift)
        };
    }
    scaled[0] += 32;
    let mut horizontal = [0_i64; 64];
    for row in 0..8 {
        let input: [i64; 8] = std::array::from_fn(|column| scaled[row * 8 + column]);
        let output = inverse_transform_8(&input);
        horizontal[row * 8..row * 8 + 8].copy_from_slice(&output);
    }
    let mut transformed = [0_i64; 64];
    for column in 0..8 {
        let input: [i64; 8] = std::array::from_fn(|row| horizontal[row * 8 + column]);
        let output = inverse_transform_8(&input);
        for row in 0..8 {
            transformed[row * 8 + column] = output[row] >> 6;
        }
    }
    transformed
}

fn inverse_transform_8(input: &[i64; 8]) -> [i64; 8] {
    let a0 = input[0] + input[4];
    let a2 = input[0] - input[4];
    let a4 = (input[2] >> 1) - input[6];
    let a6 = input[2] + (input[6] >> 1);
    let b0 = a0 + a6;
    let b2 = a2 + a4;
    let b4 = a2 - a4;
    let b6 = a0 - a6;
    let a1 = -input[3] + input[5] - input[7] - (input[7] >> 1);
    let a3 = input[1] + input[7] - input[3] - (input[3] >> 1);
    let a5 = -input[1] + input[7] + input[5] + (input[5] >> 1);
    let a7 = input[3] + input[5] + input[1] + (input[1] >> 1);
    let b1 = a1 + (a7 >> 2);
    let b7 = a7 - (a1 >> 2);
    let b3 = a3 + (a5 >> 2);
    let b5 = (a3 >> 2) - a5;
    [
        b0 + b7,
        b2 + b5,
        b4 + b3,
        b6 + b1,
        b6 - b1,
        b4 - b3,
        b2 - b5,
        b0 - b7,
    ]
}

fn coefficient_category_8x8(row: usize, column: usize) -> usize {
    let row = row % 4;
    let column = column % 4;
    match (row.is_multiple_of(2), column.is_multiple_of(2)) {
        (false, false) => 1,
        (false, true) | (true, false) if row == 2 || column == 2 => 5,
        (false, true) | (true, false) => 3,
        (true, true) if row == 0 && column == 0 => 0,
        (true, true) if row == 2 && column == 2 => 2,
        (true, true) => 4,
    }
}

fn level_scale_8x8(qp: i32, row: usize, column: usize, scaling_list: &[u8; 64]) -> i32 {
    const NORM_ADJUST: [[i32; 6]; 6] = [
        [20, 18, 32, 19, 25, 24],
        [22, 19, 35, 21, 28, 26],
        [26, 23, 42, 24, 33, 31],
        [28, 25, 45, 26, 35, 33],
        [32, 28, 51, 30, 40, 38],
        [36, 32, 58, 34, 46, 43],
    ];
    NORM_ADJUST[usize::try_from(qp % 6).expect("non-negative QP")]
        [coefficient_category_8x8(row, column)]
        * i32::from(scaling_list[row * 8 + column])
}

#[allow(clippy::too_many_arguments)]
fn quantize_intra4_luma_block(
    source: &[u8],
    stride: usize,
    origin_x: usize,
    origin_y: usize,
    prediction: &[u8; 16],
    qp: i32,
    transform_bypass: bool,
    mode: u8,
    scaling_list: &[u8; 16],
) -> [i32; 16] {
    let mut residual = [0_i32; 16];
    for y in 0..4 {
        for x in 0..4 {
            residual[y * 4 + x] = i32::from(source[(origin_y + y) * stride + origin_x + x])
                - i32::from(prediction[y * 4 + x]);
        }
    }
    if transform_bypass {
        let differentials = transform_bypass_differentials_4x4(residual, mode);
        return std::array::from_fn(|index| {
            let (row, column) = ZIG_ZAG_4X4[index];
            differentials[row * 4 + column]
        });
    }
    let transformed = forward_transform_4x4(&residual);
    std::array::from_fn(|index| {
        let (row, column) = ZIG_ZAG_4X4[index];
        quantize_coefficient(transformed[row * 4 + column], qp, row, column, scaling_list)
    })
}

fn transform_bypass_differentials_4x4(mut residual: [i32; 16], mode: u8) -> [i32; 16] {
    match mode {
        0 => {
            for row in (1..4).rev() {
                for column in 0..4 {
                    residual[row * 4 + column] -= residual[(row - 1) * 4 + column];
                }
            }
        }
        1 => {
            for row in 0..4 {
                for column in (1..4).rev() {
                    residual[row * 4 + column] -= residual[row * 4 + column - 1];
                }
            }
        }
        _ => {}
    }
    residual
}

#[allow(clippy::too_many_arguments)]
fn reconstruct_intra4_luma_block(
    destination: &mut [u8],
    stride: usize,
    origin_x: usize,
    origin_y: usize,
    prediction: &[u8; 16],
    levels: &[i32; 16],
    qp: i32,
    scaling_list: &[u8; 16],
) {
    let mut scaled = [0_i64; 16];
    for (&level, &(row, column)) in levels.iter().zip(&ZIG_ZAG_4X4) {
        scaled[row * 4 + column] =
            inverse_quantized_coefficient(level, qp, row, column, scaling_list);
    }
    let residual = inverse_transform_4x4(&scaled);
    let reconstructed: [u8; 16] = std::array::from_fn(|index| {
        u8::try_from((i64::from(prediction[index]) + residual[index]).clamp(0, 255))
            .expect("clamped Intra4 reconstruction fits u8")
    });
    place_block(destination, stride, origin_x, origin_y, 4, &reconstructed);
}

fn quantize_coefficient(
    coefficient: i32,
    qp: i32,
    row: usize,
    column: usize,
    scaling_list: &[u8; 16],
) -> i32 {
    const MULTIPLIERS: [[i64; 3]; 6] = [
        [13_107, 5_243, 8_066],
        [11_916, 4_660, 7_490],
        [10_082, 4_194, 6_554],
        [9_362, 3_647, 5_825],
        [8_192, 3_355, 5_243],
        [7_282, 2_893, 4_559],
    ];
    let category = coefficient_category(row, column);
    let qp_mod = usize::try_from(qp % 6).expect("non-negative QP");
    let q_bits = u32::try_from(15 + qp / 6).expect("bounded QP shift");
    let matrix = i64::from(scaling_list[row * 4 + column]);
    let divisor = (1_i64 << q_bits) * matrix;
    let numerator = i64::from(coefficient.unsigned_abs()) * MULTIPLIERS[qp_mod][category] * 16;
    let level = (numerator + divisor / 3) / divisor;
    let level = i32::try_from(level).expect("bounded 4x4 coefficient quantizes into i32");
    if coefficient < 0 { -level } else { level }
}

#[allow(clippy::too_many_arguments)]
fn reconstruct_intra16_luma(
    destination: &mut [u8],
    stride: usize,
    macroblock_x: usize,
    macroblock_y: usize,
    prediction: &[u8],
    levels: &[i32; 16],
    ac_levels: &[[i32; 15]; 16],
    qp: i32,
    scaling_list: &[u8; 16],
) {
    let mut natural_levels = [0_i64; 16];
    for (index, &(row, column)) in ZIG_ZAG_4X4.iter().enumerate() {
        natural_levels[row * 4 + column] = i64::from(levels[index]);
    }
    let mut dc_values = [0_i64; 16];
    for row in 0..4 {
        for column in 0..4 {
            let transformed = (0..4)
                .flat_map(|inner_row| {
                    (0..4).map(move |inner_column| {
                        i64::from(HADAMARD_4X4[row][inner_row])
                            * natural_levels[inner_row * 4 + inner_column]
                            * i64::from(HADAMARD_4X4[inner_column][column])
                    })
                })
                .sum::<i64>();
            let scale = i64::from(level_scale_4x4(qp, 0, 0, scaling_list));
            dc_values[row * 4 + column] = if qp >= 36 {
                (transformed * scale) << (qp / 6 - 6)
            } else {
                (transformed * scale + (1_i64 << (5 - qp / 6))) >> (6 - qp / 6)
            };
        }
    }
    let mut reconstructed = prediction.to_vec();
    for block_y in 0..4 {
        for block_x in 0..4 {
            let block_index = luma_block_index(block_x, block_y);
            let mut scaled = [0_i64; 16];
            scaled[0] = dc_values[block_y * 4 + block_x];
            for (&level, &(row, column)) in ac_levels[block_index].iter().zip(&ZIG_ZAG_4X4[1..]) {
                scaled[row * 4 + column] =
                    inverse_quantized_coefficient(level, qp, row, column, scaling_list);
            }
            let residual = inverse_transform_4x4(&scaled);
            for y in 0..4 {
                for x in 0..4 {
                    let index = (block_y * 4 + y) * 16 + block_x * 4 + x;
                    reconstructed[index] = u8::try_from(
                        (i64::from(reconstructed[index]) + residual[y * 4 + x]).clamp(0, 255),
                    )
                    .expect("clamped reconstruction fits u8");
                }
            }
        }
    }
    place_block(
        destination,
        stride,
        macroblock_x * 16,
        macroblock_y * 16,
        16,
        &reconstructed,
    );
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn reconstruct_chroma(
    destination: &mut [u8],
    stride: usize,
    macroblock_x: usize,
    macroblock_y: usize,
    prediction: &[u8],
    residual: &ChromaResidual,
    qp: i32,
    transform_bypass: bool,
    intra_mode: Option<u8>,
    scaling_list: &[u8; 16],
) {
    if transform_bypass {
        let mut blocks = [[0_i32; 16]; 4];
        for (block_index, block) in blocks.iter_mut().enumerate() {
            for (coefficient, &(row, column)) in ZIG_ZAG_4X4.iter().enumerate() {
                block[row * 4 + column] = if coefficient == 0 {
                    residual.dc[block_index]
                } else {
                    residual.ac[block_index][coefficient - 1]
                };
            }
            let mode = match intra_mode {
                Some(1) => 1,
                Some(2) => 0,
                _ => 2,
            };
            match mode {
                0 => {
                    for row in 1..4 {
                        for column in 0..4 {
                            block[row * 4 + column] += block[(row - 1) * 4 + column];
                        }
                    }
                }
                1 => {
                    for row in 0..4 {
                        for column in 1..4 {
                            block[row * 4 + column] += block[row * 4 + column - 1];
                        }
                    }
                }
                _ => {}
            }
        }
        match intra_mode {
            Some(1) => {
                for right in [1, 3] {
                    let left = right - 1;
                    for row in 0..4 {
                        let continuation = blocks[left][row * 4 + 3];
                        for column in 0..4 {
                            blocks[right][row * 4 + column] += continuation;
                        }
                    }
                }
            }
            Some(2) => {
                for bottom in 2..4 {
                    let top = bottom - 2;
                    for row in 0..4 {
                        for column in 0..4 {
                            blocks[bottom][row * 4 + column] += blocks[top][12 + column];
                        }
                    }
                }
            }
            _ => {}
        }
        let mut reconstructed = prediction.to_vec();
        for (block_index, block) in blocks.iter().enumerate() {
            let block_x = block_index % 2;
            let block_y = block_index / 2;
            for row in 0..4 {
                for column in 0..4 {
                    let index = (block_y * 4 + row) * 8 + block_x * 4 + column;
                    reconstructed[index] = u8::try_from(
                        (i32::from(reconstructed[index]) + block[row * 4 + column]).clamp(0, 255),
                    )
                    .expect("clamped transform-bypass chroma reconstruction fits u8");
                }
            }
        }
        place_block(
            destination,
            stride,
            macroblock_x * 8,
            macroblock_y * 8,
            8,
            &reconstructed,
        );
        return;
    }
    let dc = [
        residual.dc[0] + residual.dc[1] + residual.dc[2] + residual.dc[3],
        residual.dc[0] - residual.dc[1] + residual.dc[2] - residual.dc[3],
        residual.dc[0] + residual.dc[1] - residual.dc[2] - residual.dc[3],
        residual.dc[0] - residual.dc[1] - residual.dc[2] + residual.dc[3],
    ]
    .map(|value| {
        ((i64::from(value) * i64::from(level_scale_4x4(qp, 0, 0, scaling_list))) << (qp / 6)) >> 5
    });
    let mut reconstructed = prediction.to_vec();
    for block_y in 0..2 {
        for block_x in 0..2 {
            let block_index = block_y * 2 + block_x;
            let mut scaled = [0_i64; 16];
            scaled[0] = dc[block_index];
            for (&level, &(row, column)) in residual.ac[block_index].iter().zip(&ZIG_ZAG_4X4[1..]) {
                scaled[row * 4 + column] =
                    inverse_quantized_coefficient(level, qp, row, column, scaling_list);
            }
            let block = inverse_transform_4x4(&scaled);
            for y in 0..4 {
                for x in 0..4 {
                    let index = (block_y * 4 + y) * 8 + block_x * 4 + x;
                    reconstructed[index] = u8::try_from(
                        (i64::from(reconstructed[index]) + block[y * 4 + x]).clamp(0, 255),
                    )
                    .expect("clamped chroma reconstruction fits u8");
                }
            }
        }
    }
    place_block(
        destination,
        stride,
        macroblock_x * 8,
        macroblock_y * 8,
        8,
        &reconstructed,
    );
}

fn coefficient_category(row: usize, column: usize) -> usize {
    if row.is_multiple_of(2) && column.is_multiple_of(2) {
        0
    } else if !row.is_multiple_of(2) && !column.is_multiple_of(2) {
        1
    } else {
        2
    }
}

fn level_scale_4x4(qp: i32, row: usize, column: usize, scaling_list: &[u8; 16]) -> i32 {
    const NORM_ADJUST: [[i32; 3]; 6] = [
        [10, 16, 13],
        [11, 18, 14],
        [13, 20, 16],
        [14, 23, 18],
        [16, 25, 20],
        [18, 29, 23],
    ];
    NORM_ADJUST[usize::try_from(qp % 6).expect("non-negative QP")]
        [coefficient_category(row, column)]
        * i32::from(scaling_list[row * 4 + column])
}

fn inverse_quantized_coefficient(
    level: i32,
    qp: i32,
    row: usize,
    column: usize,
    scaling_list: &[u8; 16],
) -> i64 {
    let value = i64::from(level) * i64::from(level_scale_4x4(qp, row, column, scaling_list));
    if qp >= 24 {
        value << (qp / 6 - 4)
    } else {
        (value + (1_i64 << (3 - qp / 6))) >> (4 - qp / 6)
    }
}

fn chroma_qp(luma_qp: i32) -> i32 {
    const QP_TABLE: [i32; 22] = [
        29, 30, 31, 32, 32, 33, 34, 34, 35, 35, 36, 36, 37, 37, 37, 38, 38, 38, 39, 39, 39, 39,
    ];
    if luma_qp < 30 {
        luma_qp
    } else {
        QP_TABLE[usize::try_from(luma_qp - 30).expect("bounded chroma QP")]
    }
}

fn inverse_transform_4x4(scaled: &[i64; 16]) -> [i64; 16] {
    let mut horizontal = [0_i64; 16];
    for row in 0..4 {
        let base = row * 4;
        let e0 = scaled[base] + scaled[base + 2];
        let e1 = scaled[base] - scaled[base + 2];
        let e2 = (scaled[base + 1] >> 1) - scaled[base + 3];
        let e3 = scaled[base + 1] + (scaled[base + 3] >> 1);
        horizontal[base] = e0 + e3;
        horizontal[base + 1] = e1 + e2;
        horizontal[base + 2] = e1 - e2;
        horizontal[base + 3] = e0 - e3;
    }
    let mut output = [0_i64; 16];
    for column in 0..4 {
        let g0 = horizontal[column] + horizontal[8 + column];
        let g1 = horizontal[column] - horizontal[8 + column];
        let g2 = (horizontal[4 + column] >> 1) - horizontal[12 + column];
        let g3 = horizontal[4 + column] + (horizontal[12 + column] >> 1);
        for (row, value) in [g0 + g3, g1 + g2, g1 - g2, g0 - g3].into_iter().enumerate() {
            output[row * 4 + column] = (value + 32) >> 6;
        }
    }
    output
}

fn intra16_dc_nc(nonzero: &[[u8; 16]], address: usize, macroblocks_wide: usize) -> i8 {
    let macroblock_x = address % macroblocks_wide;
    let left = (macroblock_x > 0).then(|| nonzero[address - 1][5]);
    let top = (address >= macroblocks_wide).then(|| nonzero[address - macroblocks_wide][10]);
    combined_nc(left, top)
}

fn luma_nc(
    nonzero: &[[u8; 16]],
    address: usize,
    block_index: usize,
    macroblocks_wide: usize,
) -> i8 {
    let (block_x, block_y) = luma_block_position(block_index);
    let left = if block_x > 0 {
        Some(nonzero[address][luma_block_index(block_x - 1, block_y)])
    } else if !address.is_multiple_of(macroblocks_wide) {
        Some(nonzero[address - 1][luma_block_index(3, block_y)])
    } else {
        None
    };
    let top = if block_y > 0 {
        Some(nonzero[address][luma_block_index(block_x, block_y - 1)])
    } else if address >= macroblocks_wide {
        Some(nonzero[address - macroblocks_wide][luma_block_index(block_x, 3)])
    } else {
        None
    };
    combined_nc(left, top)
}

fn chroma_nc(
    nonzero: &[[[u8; 4]; 2]],
    address: usize,
    component: usize,
    block_index: usize,
    macroblocks_wide: usize,
) -> i8 {
    let block_x = block_index % 2;
    let block_y = block_index / 2;
    let left = if block_x > 0 {
        Some(nonzero[address][component][block_index - 1])
    } else if !address.is_multiple_of(macroblocks_wide) {
        Some(nonzero[address - 1][component][block_y * 2 + 1])
    } else {
        None
    };
    let top = if block_y > 0 {
        Some(nonzero[address][component][block_index - 2])
    } else if address >= macroblocks_wide {
        Some(nonzero[address - macroblocks_wide][component][2 + block_x])
    } else {
        None
    };
    combined_nc(left, top)
}

fn combined_nc(left: Option<u8>, top: Option<u8>) -> i8 {
    let value = match (left, top) {
        (Some(left), Some(top)) => (left + top).div_ceil(2),
        (Some(value), None) | (None, Some(value)) => value,
        (None, None) => 0,
    };
    i8::try_from(value).expect("CAVLC nonzero count is at most 16")
}

const fn luma_block_position(index: usize) -> (usize, usize) {
    let group = index / 4;
    let within_group = index % 4;
    (
        (group % 2) * 2 + within_group % 2,
        (group / 2) * 2 + within_group / 2,
    )
}

const fn luma_block_index(x: usize, y: usize) -> usize {
    (y / 2) * 8 + (x / 2) * 4 + (y % 2) * 2 + x % 2
}

fn round_div(value: i64, divisor: i64) -> i64 {
    if value >= 0 {
        (value + divisor / 2) / divisor
    } else {
        -((-value + divisor / 2) / divisor)
    }
}

fn place_block(
    plane: &mut [u8],
    stride: usize,
    origin_x: usize,
    origin_y: usize,
    size: usize,
    samples: &[u8],
) {
    for y in 0..size {
        plane[(origin_y + y) * stride + origin_x..][..size]
            .copy_from_slice(&samples[y * size..][..size]);
    }
}

fn visible_frame(
    source: &VideoFrame,
    configuration: &Configuration,
    reconstructed: &[Vec<u8>; 3],
) -> VideoFrame {
    let planes = reconstructed
        .iter()
        .enumerate()
        .map(|(component, plane)| {
            let divisor = if component == 0 { 1 } else { 2 };
            let width = configuration.width / divisor;
            let height = configuration.height / divisor;
            let coded_width = configuration.coded_width / divisor;
            let mut data = Vec::with_capacity(width * height);
            for y in 0..height {
                data.extend_from_slice(&plane[y * coded_width..][..width]);
            }
            Plane {
                data,
                stride: width,
                width,
                height,
            }
        })
        .collect();
    VideoFrame {
        format: source.format,
        width: source.width,
        height: source.height,
        planes,
        timing: source.timing,
        color: source.color.clone(),
        field_order: source.field_order,
    }
}

fn encode_avcc(sps: &[u8], pps: &[u8]) -> Result<Vec<u8>> {
    let sps_identifiers = sps
        .get(1..4)
        .ok_or_else(|| Error::InvalidData("H.264 SPS is too short for avcC identifiers".into()))?;
    let sps_length = u16::try_from(sps.len())
        .map_err(|_| Error::InvalidData("H.264 SPS exceeds avcC length field".into()))?;
    let pps_length = u16::try_from(pps.len())
        .map_err(|_| Error::InvalidData("H.264 PPS exceeds avcC length field".into()))?;
    let mut output = vec![
        1,
        sps_identifiers[0],
        sps_identifiers[1],
        sps_identifiers[2],
        0xfc | (NAL_LENGTH_SIZE - 1),
        0xe1,
    ];
    output.extend_from_slice(&sps_length.to_be_bytes());
    output.extend_from_slice(sps);
    output.push(1);
    output.extend_from_slice(&pps_length.to_be_bytes());
    output.extend_from_slice(pps);
    Ok(output)
}

fn make_nal(header: u8, rbsp: Vec<u8>) -> Vec<u8> {
    let mut output = Vec::with_capacity(rbsp.len() + 1);
    output.push(header);
    let mut zero_count = 0_u8;
    for byte in rbsp {
        if zero_count == 2 && byte <= 3 {
            output.push(3);
            zero_count = 0;
        }
        output.push(byte);
        zero_count = if byte == 0 { zero_count + 1 } else { 0 };
    }
    output
}

fn append_length_prefixed_nal(output: &mut Vec<u8>, nal: &[u8]) -> Result<()> {
    let length = u32::try_from(nal.len())
        .map_err(|_| Error::InvalidData("H.264 encoded NAL exceeds four-byte length".into()))?;
    output.extend_from_slice(&length.to_be_bytes());
    output.extend_from_slice(nal);
    Ok(())
}

fn encode_hrd_sei(
    hrd: &EncoderHrd,
    buffering_period: bool,
    cpb_removal_delay: u32,
    dpb_output_delay: u32,
) -> Result<Vec<u8>> {
    let mut rbsp = Vec::new();
    if buffering_period {
        let mut payload = BitWriter::new();
        write_ue(&mut payload, 0)?; // seq_parameter_set_id
        payload.write_bits(
            u64::from(hrd.initial_cpb_removal_delay),
            HrdState::DELAY_BITS,
        )?;
        payload.write_bits(0, HrdState::DELAY_BITS)?; // initial_cpb_removal_delay_offset
        payload.write_bit(true)?; // payload_bit_equal_to_one
        payload.align_to_byte();
        append_sei_payload(&mut rbsp, 0, &payload.into_bytes());
    }

    let mut payload = BitWriter::new();
    payload.write_bits(u64::from(cpb_removal_delay), HrdState::DELAY_BITS)?;
    payload.write_bits(u64::from(dpb_output_delay), HrdState::DELAY_BITS)?;
    payload.write_bit(true)?; // payload_bit_equal_to_one
    payload.align_to_byte();
    append_sei_payload(&mut rbsp, 1, &payload.into_bytes());
    rbsp.push(0x80); // rbsp_trailing_bits
    Ok(make_nal(0x06, rbsp))
}

fn append_sei_payload(rbsp: &mut Vec<u8>, payload_type: usize, payload: &[u8]) {
    append_sei_extended_value(rbsp, payload_type);
    append_sei_extended_value(rbsp, payload.len());
    rbsp.extend_from_slice(payload);
}

fn append_sei_extended_value(output: &mut Vec<u8>, mut value: usize) {
    while value >= 0xff {
        output.push(0xff);
        value -= 0xff;
    }
    output.push(u8::try_from(value).expect("SEI extended-value remainder fits u8"));
}

fn write_ue(writer: &mut BitWriter, value: u64) -> Result<()> {
    let code_num = value
        .checked_add(1)
        .ok_or_else(|| Error::InvalidData("H.264 unsigned Exp-Golomb value overflows".into()))?;
    let bit_count = u8::try_from(u64::BITS - code_num.leading_zeros())
        .map_err(|_| Error::InvalidData("H.264 Exp-Golomb code length overflows".into()))?;
    writer.write_bits(0, bit_count - 1)?;
    writer.write_bits(code_num, bit_count)
}

fn write_se(writer: &mut BitWriter, value: i64) -> Result<()> {
    let code_num = if value <= 0 {
        value
            .unsigned_abs()
            .checked_mul(2)
            .ok_or_else(|| Error::InvalidData("H.264 signed Exp-Golomb value overflows".into()))?
    } else {
        value
            .unsigned_abs()
            .checked_mul(2)
            .and_then(|value| value.checked_sub(1))
            .ok_or_else(|| Error::InvalidData("H.264 signed Exp-Golomb value overflows".into()))?
    };
    write_ue(writer, code_num)
}

fn write_reference_index(
    writer: &mut BitWriter,
    reference_index: u8,
    active_references_minus1: u32,
) -> Result<()> {
    let reference_index = u32::from(reference_index);
    if reference_index > active_references_minus1 {
        return Err(Error::InvalidData(
            "selected H.264 reference index exceeds the active list".into(),
        ));
    }
    match active_references_minus1 {
        0 => Ok(()),
        1 => writer.write_bit(reference_index == 0),
        _ => write_ue(writer, u64::from(reference_index)),
    }
}

fn finish_rbsp(writer: &mut BitWriter) -> Result<()> {
    writer.write_bit(true)?;
    writer.align_to_byte();
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::{
        collections::BTreeMap,
        io::Write,
        process::{Command, Stdio},
    };

    use mmrecode_bitstream::BitReader;
    use mmrecode_core::{ColorDescription, Decoder, FrameTiming, Rational, Timestamp};

    use super::*;
    use crate::{
        AvcDecoderConfigurationRecord, H264Decoder, NalUnitType, Sps, parse_pps, parse_sps,
        remove_emulation_prevention,
    };

    #[test]
    fn encodes_deterministic_lossless_cropped_ipcm_idr() {
        let settings = settings(18, 20);
        let mut first = H264Encoder::default();
        let descriptor = first.configure(&settings).unwrap();
        let avcc = AvcDecoderConfigurationRecord::parse(&descriptor.configuration).unwrap();
        assert_eq!(avcc.length_size, 4);
        assert_eq!(avcc.sequence_parameter_sets.len(), 1);
        assert_eq!(avcc.picture_parameter_sets.len(), 1);
        let sps = parse_sps(&avcc.sequence_parameter_sets[0]).unwrap();
        assert_eq!((sps.width, sps.height), (18, 20));
        assert_eq!((sps.coded_width, sps.coded_height), (32, 32));

        let frame = patterned_frame(18, 20);
        first.send_frame(frame.clone()).unwrap();
        let packet = first.receive_packet().unwrap().unwrap();
        assert!(packet.flags.contains(PacketFlags::KEY));
        assert_eq!(packet.pts, frame.timing.pts);
        assert_eq!(packet.duration, frame.timing.duration);
        let units = crate::length_prefixed_nal_units(&packet.data, 4).unwrap();
        assert_eq!(units.len(), 1);
        assert_eq!(units[0].header.unit_type, NalUnitType::IdrSlice);

        let mut second = H264Encoder::default();
        second.configure(&settings).unwrap();
        second.send_frame(frame.clone()).unwrap();
        assert_eq!(packet.data, second.receive_packet().unwrap().unwrap().data);
        assert_eq!(
            first.receive_reconstructed_frame().unwrap(),
            Some(frame.clone())
        );

        let mut decoder = H264Decoder::default();
        decoder.configure(&descriptor).unwrap();
        decoder.send_packet(packet).unwrap();
        let decoded_frame = decoder.receive_frame().unwrap().unwrap();
        assert_eq!(
            (decoded_frame.width, decoded_frame.height),
            (frame.width, frame.height)
        );
        for (actual, expected) in decoded_frame.planes.iter().zip(&frame.planes) {
            assert_eq!(actual.width, expected.width);
            assert_eq!(actual.height, expected.height);
            for y in 0..actual.height {
                assert_eq!(
                    &actual.data[y * actual.stride..y * actual.stride + actual.width],
                    &expected.data[y * expected.stride..y * expected.stride + expected.width]
                );
            }
        }
        verify_with_ffmpeg(&avcc, &packet_bytes(&mut second, &frame), &frame);
    }

    #[test]
    fn cabac_ipcm_round_trips_cropped_multi_macroblock_idr() {
        let mut encoder_settings = settings(18, 20);
        encoder_settings
            .options
            .insert("entropy".into(), "cabac".into());
        let mut encoder = H264Encoder::default();
        let descriptor = encoder.configure(&encoder_settings).unwrap();
        let avcc = AvcDecoderConfigurationRecord::parse(&descriptor.configuration).unwrap();
        let sps = parse_sps(&avcc.sequence_parameter_sets[0]).unwrap();
        let pps = parse_pps(&avcc.picture_parameter_sets[0]).unwrap();
        assert_eq!(sps.profile_idc, PROFILE_MAIN);
        assert!(pps.entropy_coding_mode);

        let frame = patterned_frame(18, 20);
        encoder.send_frame(frame.clone()).unwrap();
        let packet = encoder.receive_packet().unwrap().unwrap();
        assert_frames_pixels_equal(
            &encoder.receive_reconstructed_frame().unwrap().unwrap(),
            &frame,
        );

        let mut decoder = H264Decoder::default();
        decoder.configure(&descriptor).unwrap();
        decoder.send_packet(packet.clone()).unwrap();
        assert_frames_pixels_equal(&decoder.receive_frame().unwrap().unwrap(), &frame);
        verify_with_ffmpeg(&avcc, &packet.data, &frame);
    }

    #[test]
    fn cabac_rejects_baseline_and_unknown_entropy_names() {
        let mut invalid = settings(16, 16);
        invalid.options.insert("profile".into(), "baseline".into());
        invalid.options.insert("entropy".into(), "cabac".into());
        assert!(H264Encoder::default().configure(&invalid).is_err());

        let mut invalid = settings(16, 16);
        invalid
            .options
            .insert("entropy".into(), "arithmetic".into());
        assert!(H264Encoder::default().configure(&invalid).is_err());
    }

    #[test]
    fn signals_bt709_limited_range_in_vui() {
        let mut encoder_settings = settings(16, 16);
        encoder_settings
            .options
            .insert("color".into(), "bt709".into());
        let mut encoder = H264Encoder::default();
        let descriptor = encoder.configure(&encoder_settings).unwrap();
        let avcc = AvcDecoderConfigurationRecord::parse(&descriptor.configuration).unwrap();
        let vui = parse_sps(&avcc.sequence_parameter_sets[0])
            .unwrap()
            .vui
            .unwrap();
        assert_eq!(vui.video_full_range, Some(false));
        assert_eq!(vui.colour_primaries, Some(1));
        assert_eq!(vui.transfer_characteristics, Some(1));
        assert_eq!(vui.matrix_coefficients, Some(1));
    }

    #[test]
    fn cabac_intra16_round_trips_residuals_qp_changes_and_scaling_matrices() {
        for (qp, adaptive, scaling_matrix) in
            [(12, false, "flat"), (38, true, "flat"), (27, false, "jvt")]
        {
            let frame = if adaptive {
                mixed_activity_frame(32, 32, 0)
            } else {
                patterned_frame(32, 32)
            };
            let mut encoder_settings = settings(32, 32);
            encoder_settings
                .options
                .insert("mode".into(), "intra16".into());
            encoder_settings
                .options
                .insert("entropy".into(), "cabac".into());
            encoder_settings.options.insert("qp".into(), qp.to_string());
            encoder_settings
                .options
                .insert("scaling_matrix".into(), scaling_matrix.into());
            if adaptive {
                encoder_settings
                    .options
                    .insert("aq_strength".into(), "6".into());
            }
            let mut encoder = H264Encoder::default();
            let descriptor = encoder.configure(&encoder_settings).unwrap();
            let avcc = AvcDecoderConfigurationRecord::parse(&descriptor.configuration).unwrap();
            assert!(
                parse_pps(&avcc.picture_parameter_sets[0])
                    .unwrap()
                    .entropy_coding_mode
            );
            encoder.send_frame(frame).unwrap();
            let packet = encoder.receive_packet().unwrap().unwrap();
            let reconstruction = encoder.receive_reconstructed_frame().unwrap().unwrap();

            let mut decoder = H264Decoder::default();
            decoder.configure(&descriptor).unwrap();
            decoder.send_packet(packet.clone()).unwrap();
            assert_frames_pixels_equal(&decoder.receive_frame().unwrap().unwrap(), &reconstruction);
            verify_with_ffmpeg(&avcc, &packet.data, &reconstruction);
        }
    }

    #[test]
    fn cabac_intra4_round_trips_prediction_residuals_and_lossless_bypass() {
        for (qp, adaptive, scaling_matrix, bypass) in [
            (12, false, "flat", false),
            (38, true, "flat", false),
            (27, false, "jvt", false),
            (0, false, "flat", true),
        ] {
            let frame = if adaptive {
                mixed_activity_frame(32, 32, 0)
            } else {
                patterned_frame(32, 32)
            };
            let mut encoder_settings = settings(32, 32);
            encoder_settings
                .options
                .insert("mode".into(), "intra4".into());
            encoder_settings
                .options
                .insert("entropy".into(), "cabac".into());
            encoder_settings.options.insert("qp".into(), qp.to_string());
            encoder_settings
                .options
                .insert("scaling_matrix".into(), scaling_matrix.into());
            if adaptive {
                encoder_settings
                    .options
                    .insert("aq_strength".into(), "6".into());
            }
            if bypass {
                encoder_settings
                    .options
                    .insert("transform_bypass".into(), "true".into());
            }
            let mut encoder = H264Encoder::default();
            let descriptor = encoder.configure(&encoder_settings).unwrap();
            let avcc = AvcDecoderConfigurationRecord::parse(&descriptor.configuration).unwrap();
            encoder.send_frame(frame.clone()).unwrap();
            let packet = encoder.receive_packet().unwrap().unwrap();
            let reconstruction = encoder.receive_reconstructed_frame().unwrap().unwrap();
            if bypass {
                assert_frames_pixels_equal(&reconstruction, &frame);
            }

            let mut decoder = H264Decoder::default();
            decoder.configure(&descriptor).unwrap();
            decoder.send_packet(packet.clone()).unwrap();
            assert_frames_pixels_equal(&decoder.receive_frame().unwrap().unwrap(), &reconstruction);
            verify_with_ffmpeg(&avcc, &packet.data, &reconstruction);
        }
    }

    #[test]
    fn cabac_intra8_round_trips_transform_contexts_aq_and_scaling_matrices() {
        for (qp, adaptive, scaling_matrix) in
            [(12, false, "flat"), (38, true, "flat"), (27, false, "jvt")]
        {
            let frame = if adaptive {
                mixed_activity_frame(32, 32, 0)
            } else {
                patterned_frame(32, 32)
            };
            let mut encoder_settings = settings(32, 32);
            encoder_settings
                .options
                .insert("mode".into(), "intra8".into());
            encoder_settings
                .options
                .insert("entropy".into(), "cabac".into());
            encoder_settings.options.insert("qp".into(), qp.to_string());
            encoder_settings
                .options
                .insert("scaling_matrix".into(), scaling_matrix.into());
            if adaptive {
                encoder_settings
                    .options
                    .insert("aq_strength".into(), "6".into());
            }
            let mut encoder = H264Encoder::default();
            let descriptor = encoder.configure(&encoder_settings).unwrap();
            let avcc = AvcDecoderConfigurationRecord::parse(&descriptor.configuration).unwrap();
            assert_eq!(
                parse_sps(&avcc.sequence_parameter_sets[0])
                    .unwrap()
                    .profile_idc,
                PROFILE_HIGH
            );
            encoder.send_frame(frame).unwrap();
            let packet = encoder.receive_packet().unwrap().unwrap();
            let reconstruction = encoder.receive_reconstructed_frame().unwrap().unwrap();

            let mut decoder = H264Decoder::default();
            decoder.configure(&descriptor).unwrap();
            decoder.send_packet(packet.clone()).unwrap();
            assert_frames_pixels_equal(&decoder.receive_frame().unwrap().unwrap(), &reconstruction);
            verify_with_ffmpeg(&avcc, &packet.data, &reconstruction);
        }
    }

    #[test]
    fn cabac_p_slices_round_trip_skip_partitions_references_and_residual_transforms() {
        for (profile, qp, bypass, scaling_matrix) in [
            ("main", 24, false, "flat"),
            ("high", 27, false, "jvt"),
            ("high", 0, true, "flat"),
        ] {
            let mut encoder_settings = settings(32, 32);
            encoder_settings
                .options
                .insert("mode".into(), "inter".into());
            encoder_settings
                .options
                .insert("entropy".into(), "cabac".into());
            encoder_settings
                .options
                .insert("profile".into(), profile.into());
            encoder_settings.options.insert("qp".into(), qp.to_string());
            encoder_settings
                .options
                .insert("max_refs".into(), "2".into());
            encoder_settings
                .options
                .insert("search_range".into(), "8".into());
            encoder_settings
                .options
                .insert("scaling_matrix".into(), scaling_matrix.into());
            if bypass {
                encoder_settings
                    .options
                    .insert("transform_bypass".into(), "true".into());
            }

            let mut encoder = H264Encoder::default();
            let descriptor = encoder.configure(&encoder_settings).unwrap();
            let avcc = AvcDecoderConfigurationRecord::parse(&descriptor.configuration).unwrap();
            assert!(
                parse_pps(&avcc.picture_parameter_sets[0])
                    .unwrap()
                    .entropy_coding_mode
            );
            let first = frame_with_pts(patterned_frame(32, 32), 0);
            encoder.send_frame(first).unwrap();
            let mut packets = vec![encoder.receive_packet().unwrap().unwrap()];
            let mut reconstructions = vec![encoder.receive_reconstructed_frame().unwrap().unwrap()];
            let inputs = [
                frame_with_pts(reconstructions[0].clone(), 1),
                split_motion_frame(&reconstructions[0], true, 2),
                subpartition_motion_frame(&reconstructions[0], 3),
            ];
            for input in inputs {
                encoder.send_frame(input).unwrap();
                packets.push(encoder.receive_packet().unwrap().unwrap());
                reconstructions.push(encoder.receive_reconstructed_frame().unwrap().unwrap());
            }
            assert!(packets[0].flags.contains(PacketFlags::KEY));
            assert!(
                packets[1..]
                    .iter()
                    .all(|packet| !packet.flags.contains(PacketFlags::KEY))
            );

            let mut decoder = H264Decoder::default();
            decoder.configure(&descriptor).unwrap();
            for (packet, expected) in packets.iter().cloned().zip(&reconstructions) {
                decoder.send_packet(packet).unwrap();
                assert_frames_pixels_equal(&decoder.receive_frame().unwrap().unwrap(), expected);
            }
            verify_sequence_with_ffmpeg(&avcc, &packets, &reconstructions);
        }
    }

    #[test]
    fn cabac_b_slices_round_trip_direct_partitioned_and_lossless_modes() {
        for (direct_mode, qp, bypass, scaling_matrix, adaptive) in [
            ("spatial", 24, false, "flat", true),
            ("temporal", 27, false, "jvt", false),
            ("spatial", 0, true, "flat", false),
        ] {
            let mut encoder_settings = settings(32, 16);
            for (name, value) in [
                ("mode", "inter"),
                ("entropy", "cabac"),
                ("profile", "high"),
                ("b_frames", "1"),
                ("max_refs", "2"),
                ("search_range", "8"),
                ("b_direct", direct_mode),
                ("scaling_matrix", scaling_matrix),
            ] {
                encoder_settings.options.insert(name.into(), value.into());
            }
            encoder_settings.options.insert("qp".into(), qp.to_string());
            if bypass {
                encoder_settings
                    .options
                    .insert("transform_bypass".into(), "true".into());
            }
            if adaptive {
                encoder_settings
                    .options
                    .insert("aq_strength".into(), "6".into());
            }

            let mut encoder = H264Encoder::default();
            let descriptor = encoder.configure(&encoder_settings).unwrap();
            let avcc = AvcDecoderConfigurationRecord::parse(&descriptor.configuration).unwrap();
            assert!(
                parse_pps(&avcc.picture_parameter_sets[0])
                    .unwrap()
                    .entropy_coding_mode
            );
            let first = frame_with_pts(patterned_frame(32, 16), 0);
            encoder.send_frame(first).unwrap();
            let idr_packet = encoder.receive_packet().unwrap().unwrap();
            let idr_reconstruction = encoder.receive_reconstructed_frame().unwrap().unwrap();
            let middle = if direct_mode == "temporal" {
                shifted_frame(&idr_reconstruction, 1, 0, 1)
            } else {
                subpartition_motion_frame(&idr_reconstruction, 1)
            };
            let future = shifted_frame(&idr_reconstruction, 2, 0, 2);
            encoder.send_frame(middle).unwrap();
            encoder.send_frame(future).unwrap();
            let future_packet = encoder.receive_packet().unwrap().unwrap();
            let b_packet = encoder.receive_packet().unwrap().unwrap();
            let future_reconstruction = encoder.receive_reconstructed_frame().unwrap().unwrap();
            let b_reconstruction = encoder.receive_reconstructed_frame().unwrap().unwrap();
            let packets = [idr_packet, future_packet, b_packet];

            let mut decoder = H264Decoder::default();
            decoder.configure(&descriptor).unwrap();
            for (packet, expected) in packets.iter().cloned().zip([
                &idr_reconstruction,
                &future_reconstruction,
                &b_reconstruction,
            ]) {
                decoder.send_packet(packet).unwrap();
                assert_frames_pixels_equal(&decoder.receive_frame().unwrap().unwrap(), expected);
            }
            verify_sequence_with_ffmpeg(
                &avcc,
                &packets,
                &[idr_reconstruction, b_reconstruction, future_reconstruction],
            );
        }
    }

    #[test]
    fn cabac_b_type_trees_round_trip_every_inter_code() {
        for context_increment in 0..=2 {
            for macroblock_type in 0..=22 {
                let mut encoder_contexts = CabacBContexts::new(26, 0).unwrap();
                let mut arithmetic = CabacEncoder::new();
                encode_cabac_b_macroblock_type(
                    &mut arithmetic,
                    &mut encoder_contexts,
                    macroblock_type,
                    context_increment,
                )
                .unwrap();
                arithmetic.terminate(true).unwrap();
                let bytes = arithmetic.finish().unwrap();
                let mut bits = BitReader::new(&bytes);
                let mut decoder = crate::cabac::CabacDecoder::new(&mut bits).unwrap();
                let mut decoder_contexts = CabacBContexts::new(26, 0).unwrap();
                assert_eq!(
                    crate::decoder::decode_cabac_b_macroblock_type(
                        &mut decoder,
                        &mut decoder_contexts.macroblock_type,
                        context_increment,
                    )
                    .unwrap(),
                    u32::from(macroblock_type)
                );
                assert!(decoder.terminate().unwrap());
            }
        }
        for sub_type in 0..=12 {
            let mut encoder_contexts = CabacBContexts::new(26, 0).unwrap();
            let mut arithmetic = CabacEncoder::new();
            encode_cabac_b_sub_macroblock_type(&mut arithmetic, &mut encoder_contexts, sub_type)
                .unwrap();
            arithmetic.terminate(true).unwrap();
            let bytes = arithmetic.finish().unwrap();
            let mut bits = BitReader::new(&bytes);
            let mut decoder = crate::cabac::CabacDecoder::new(&mut bits).unwrap();
            let mut decoder_contexts = CabacBContexts::new(26, 0).unwrap();
            assert_eq!(
                crate::decoder::decode_cabac_b_sub_macroblock_type(
                    &mut decoder,
                    &mut decoder_contexts.sub_macroblock_type,
                )
                .unwrap(),
                u32::from(sub_type)
            );
            assert!(decoder.terminate().unwrap());
        }
    }

    #[test]
    fn rejects_invalid_format_dimensions_and_planes() {
        let mut encoder = H264Encoder::default();
        let mut invalid = settings(17, 16);
        assert!(encoder.configure(&invalid).is_err());
        invalid.width = 16;
        invalid.pixel_format = PixelFormat::Rgb24;
        assert!(encoder.configure(&invalid).is_err());

        for (name, value) in [
            ("gop_size", "0"),
            ("search_range", "65"),
            ("analysis", "slow"),
            ("scene_cut_threshold", "256"),
            ("max_refs", "0"),
            ("max_refs", "5"),
            ("b_frames", "4"),
            ("b_direct", "diagonal"),
            ("aq_strength", "13"),
            ("vbv_buffer_ms", "0"),
            ("vbv_buffer_ms", "60001"),
            ("profile", "extended"),
            ("level", "7"),
            ("frame_duration_ticks", "0"),
        ] {
            let mut invalid_option = settings(16, 16);
            invalid_option.options.insert(name.into(), value.into());
            assert!(encoder.configure(&invalid_option).is_err());
        }
        let mut inter_only_option = settings(16, 16);
        inter_only_option
            .options
            .insert("max_refs".into(), "2".into());
        assert!(encoder.configure(&inter_only_option).is_err());
        let mut temporal_without_b = settings(16, 16);
        temporal_without_b
            .options
            .insert("mode".into(), "inter".into());
        temporal_without_b
            .options
            .insert("b_direct".into(), "temporal".into());
        assert!(encoder.configure(&temporal_without_b).is_err());
        let mut lossless_aq = settings(16, 16);
        lossless_aq.options.insert("aq_strength".into(), "1".into());
        assert!(encoder.configure(&lossless_aq).is_err());
        let mut vbv_without_bitrate = settings(16, 16);
        vbv_without_bitrate
            .options
            .insert("vbv_buffer_ms".into(), "1000".into());
        assert!(encoder.configure(&vbv_without_bitrate).is_err());
        let mut baseline_b = settings(16, 16);
        baseline_b.options.insert("mode".into(), "inter".into());
        baseline_b.options.insert("b_frames".into(), "1".into());
        baseline_b
            .options
            .insert("profile".into(), "baseline".into());
        assert!(encoder.configure(&baseline_b).is_err());
        let mut main_intra8 = settings(16, 16);
        main_intra8.options.insert("mode".into(), "intra8".into());
        main_intra8.options.insert("profile".into(), "main".into());
        assert!(encoder.configure(&main_intra8).is_err());

        let mut lossless_bitrate = settings(16, 16);
        lossless_bitrate.bitrate = Some(100_000);
        assert!(encoder.configure(&lossless_bitrate).is_err());
        let mut zero_bitrate = settings(16, 16);
        zero_bitrate.options.insert("mode".into(), "intra4".into());
        zero_bitrate.bitrate = Some(0);
        assert!(encoder.configure(&zero_bitrate).is_err());
        let mut negative_time_base = zero_bitrate;
        negative_time_base.bitrate = Some(100_000);
        negative_time_base.time_base = Rational::new(-1, 25).unwrap();
        assert!(encoder.configure(&negative_time_base).is_err());

        encoder.configure(&settings(16, 16)).unwrap();
        let mut frame = patterned_frame(16, 16);
        frame.planes[0].data.truncate(8);
        assert!(encoder.send_frame(frame).is_err());
    }

    #[test]
    fn negotiates_baseline_and_main_profiles_and_keeps_avcc_consistent() {
        let baseline_descriptor = H264Encoder::default().configure(&settings(16, 16)).unwrap();
        let baseline_avcc =
            AvcDecoderConfigurationRecord::parse(&baseline_descriptor.configuration).unwrap();
        let baseline_sps = parse_sps(&baseline_avcc.sequence_parameter_sets[0]).unwrap();
        assert_eq!(baseline_sps.profile_idc, PROFILE_BASELINE);
        assert_eq!(baseline_sps.constraint_flags, BASELINE_COMPATIBILITY);
        assert_eq!(baseline_avcc.profile_indication, baseline_sps.profile_idc);
        assert_eq!(
            baseline_avcc.profile_compatibility,
            baseline_sps.constraint_flags
        );

        let mut automatic_main = settings(16, 16);
        automatic_main.options.insert("mode".into(), "inter".into());
        automatic_main.options.insert("b_frames".into(), "1".into());
        let main_descriptor = H264Encoder::default().configure(&automatic_main).unwrap();
        let main_avcc =
            AvcDecoderConfigurationRecord::parse(&main_descriptor.configuration).unwrap();
        let main_sps = parse_sps(&main_avcc.sequence_parameter_sets[0]).unwrap();
        assert_eq!(main_sps.profile_idc, PROFILE_MAIN);
        assert_eq!(main_sps.constraint_flags, 0);
        assert_eq!(main_avcc.profile_indication, main_sps.profile_idc);
        assert_eq!(main_avcc.profile_compatibility, 0);

        let mut explicit_main = settings(16, 16);
        explicit_main
            .options
            .insert("profile".into(), "main".into());
        let descriptor = H264Encoder::default().configure(&explicit_main).unwrap();
        let avcc = AvcDecoderConfigurationRecord::parse(&descriptor.configuration).unwrap();
        assert_eq!(
            parse_sps(&avcc.sequence_parameter_sets[0])
                .unwrap()
                .profile_idc,
            PROFILE_MAIN
        );
    }

    #[test]
    fn negotiates_and_enforces_annex_a_levels() {
        fn parameter_sets(settings: &VideoEncoderSettings) -> (Sps, AvcDecoderConfigurationRecord) {
            let descriptor = H264Encoder::default().configure(settings).unwrap();
            let avcc = AvcDecoderConfigurationRecord::parse(&descriptor.configuration).unwrap();
            let sps = parse_sps(&avcc.sequence_parameter_sets[0]).unwrap();
            (sps, avcc)
        }

        let (level_one, avcc) = parameter_sets(&settings(16, 16));
        assert_eq!(level_one.level_idc, 10);
        assert_eq!(avcc.level_indication, level_one.level_idc);

        let mut level_one_b = settings(16, 16);
        level_one_b.options.insert("mode".into(), "intra4".into());
        level_one_b.bitrate = Some(100_000);
        let (level_one_b, avcc) = parameter_sets(&level_one_b);
        assert_eq!(level_one_b.level_idc, 11);
        assert_ne!(level_one_b.constraint_flags & 0x10, 0);
        assert_eq!(avcc.profile_compatibility, level_one_b.constraint_flags);

        let mut cif_thirty = settings(176, 144);
        cif_thirty.time_base = Rational::new(1, 30).unwrap();
        assert_eq!(parameter_sets(&cif_thirty).0.level_idc, 11);
        assert_eq!(parameter_sets(&cif_thirty).0.constraint_flags & 0x10, 0);

        let mut full_hd = settings(1_920, 1_080);
        full_hd.time_base = Rational::new(1, 30).unwrap();
        assert_eq!(parameter_sets(&full_hd).0.level_idc, 40);
        full_hd.time_base = Rational::new(1, 60).unwrap();
        assert_eq!(parameter_sets(&full_hd).0.level_idc, 42);

        let mut clock_based = settings(1_920, 1_080);
        clock_based.time_base = Rational::new(1, 90_000).unwrap();
        clock_based
            .options
            .insert("frame_duration_ticks".into(), "3000".into());
        assert_eq!(parameter_sets(&clock_based).0.level_idc, 40);

        let mut reference_heavy = settings(1_920, 1_080);
        reference_heavy.time_base = Rational::new(1, 30).unwrap();
        reference_heavy
            .options
            .insert("mode".into(), "inter".into());
        reference_heavy
            .options
            .insert("max_refs".into(), "4".into());
        reference_heavy
            .options
            .insert("b_frames".into(), "1".into());
        assert_eq!(parameter_sets(&reference_heavy).0.level_idc, 50);

        let mut large_cpb = settings(1_920, 1_080);
        large_cpb.time_base = Rational::new(1, 30).unwrap();
        large_cpb.options.insert("mode".into(), "intra4".into());
        large_cpb
            .options
            .insert("vbv_buffer_ms".into(), "2000".into());
        large_cpb.bitrate = Some(20_000_000);
        assert_eq!(parameter_sets(&large_cpb).0.level_idc, 41);

        let mut constrained = settings(1_920, 1_080);
        constrained.time_base = Rational::new(1, 30).unwrap();
        constrained.options.insert("level".into(), "3.2".into());
        let error = H264Encoder::default().configure(&constrained).unwrap_err();
        assert!(error.to_string().contains("level 3.2"));

        let mut explicit = settings(16, 16);
        explicit.options.insert("level".into(), "4.1".into());
        assert_eq!(parameter_sets(&explicit).0.level_idc, 41);

        let mut timed = settings(176, 144);
        timed.time_base = Rational::new(1, 15).unwrap();
        let mut encoder = H264Encoder::default();
        encoder.configure(&timed).unwrap();
        let too_fast = patterned_frame(176, 144);
        assert!(encoder.send_frame(too_fast).is_err());
        let mut conforming = patterned_frame(176, 144);
        conforming.timing.duration = Some(Timestamp {
            value: 1,
            time_base: Rational::new(1, 15).unwrap(),
        });
        encoder.send_frame(conforming).unwrap();
    }

    #[test]
    fn rate_control_tracks_packet_pressure_and_resets_on_reconfigure() {
        let time_base = Rational::new(1, 25).unwrap();
        let mut rate_control = RateControl::new(25_000, time_base, 26, None).unwrap();
        assert_eq!(rate_control.target_bits_per_frame, 1_000);
        for _ in 0..4 {
            rate_control.observe(4_000, None);
        }
        assert!(rate_control.current_qp > 26);
        assert_eq!(rate_control.buffer_fullness_bits, 4_000);
        let raised_qp = rate_control.current_qp;
        for _ in 0..12 {
            rate_control.observe(0, None);
        }
        assert!(rate_control.current_qp < raised_qp);
        assert_eq!(rate_control.buffer_fullness_bits, -4_000);

        let clock = Rational::new(1, 90_000).unwrap();
        let mut duration_aware = RateControl::new(25_000, clock, 26, None).unwrap();
        duration_aware.observe(
            4_000,
            Some(Timestamp {
                value: 3_600,
                time_base: clock,
            }),
        );
        assert_eq!(duration_aware.buffer_fullness_bits, 3_000);
        assert_eq!(duration_aware.buffer_capacity_bits, 8_000);

        let mut encoder_settings = settings(16, 16);
        encoder_settings
            .options
            .insert("mode".into(), "intra4".into());
        encoder_settings.bitrate = Some(25_000);
        let mut encoder = H264Encoder::default();
        encoder.configure(&encoder_settings).unwrap();
        encoder.rate_control.as_mut().unwrap().observe(10_000, None);
        assert!(encoder.rate_control.as_ref().unwrap().current_qp > 26);
        encoder.configure(&encoder_settings).unwrap();
        assert_eq!(encoder.rate_control.as_ref().unwrap().current_qp, 26);
    }

    #[test]
    fn target_bitrate_changes_size_and_quality_with_conformant_reconstruction() {
        fn encode_sequence(
            bitrate: u64,
        ) -> (CodecDescriptor, Vec<Packet>, Vec<VideoFrame>, i64, i32) {
            let mut encoder_settings = settings(32, 32);
            encoder_settings
                .options
                .insert("mode".into(), "intra4".into());
            encoder_settings.bitrate = Some(bitrate);
            let mut encoder = H264Encoder::default();
            let descriptor = encoder.configure(&encoder_settings).unwrap();
            let mut packets = Vec::new();
            let mut reconstructions = Vec::new();
            let mut squared_error = 0;
            for pts in 0..10 {
                let frame = frame_with_pts(patterned_frame(32, 32), pts);
                encoder.send_frame(frame.clone()).unwrap();
                packets.push(encoder.receive_packet().unwrap().unwrap());
                let reconstruction = encoder.receive_reconstructed_frame().unwrap().unwrap();
                squared_error += luma_squared_error(&frame, &reconstruction);
                reconstructions.push(reconstruction);
            }
            let final_qp = encoder.rate_control.as_ref().unwrap().current_qp;
            (
                descriptor,
                packets,
                reconstructions,
                squared_error,
                final_qp,
            )
        }

        let (low_descriptor, low_packets, low_reconstructions, low_error, low_qp) =
            encode_sequence(10_000);
        let (high_descriptor, high_packets, high_reconstructions, high_error, high_qp) =
            encode_sequence(5_000_000);
        let low_bytes: usize = low_packets.iter().map(|packet| packet.data.len()).sum();
        let high_bytes: usize = high_packets.iter().map(|packet| packet.data.len()).sum();
        assert!(low_qp > high_qp, "low={low_qp}, high={high_qp}");
        assert!(low_bytes < high_bytes, "low={low_bytes}, high={high_bytes}");
        assert!(high_error < low_error, "low={low_error}, high={high_error}");

        for (descriptor, packets, reconstructions) in [
            (&low_descriptor, &low_packets, &low_reconstructions),
            (&high_descriptor, &high_packets, &high_reconstructions),
        ] {
            let mut decoder = H264Decoder::default();
            decoder.configure(descriptor).unwrap();
            for (packet, expected) in packets.iter().cloned().zip(reconstructions) {
                decoder.send_packet(packet).unwrap();
                assert_frames_pixels_equal(&decoder.receive_frame().unwrap().unwrap(), expected);
            }
            let avcc = AvcDecoderConfigurationRecord::parse(&descriptor.configuration).unwrap();
            verify_sequence_with_ffmpeg(&avcc, packets, reconstructions);
        }
    }

    #[test]
    fn hrd_vbv_signals_and_schedules_reordered_picture_delays() {
        let mut encoder_settings = settings(32, 16);
        encoder_settings
            .options
            .insert("mode".into(), "inter".into());
        encoder_settings
            .options
            .insert("b_frames".into(), "1".into());
        encoder_settings
            .options
            .insert("search_range".into(), "0".into());
        encoder_settings
            .options
            .insert("vbv_buffer_ms".into(), "1000".into());
        encoder_settings.bitrate = Some(25_000);
        encoder_settings.time_base = Rational::new(1, 90_000).unwrap();
        let mut encoder = H264Encoder::default();
        let descriptor = encoder.configure(&encoder_settings).unwrap();
        let avcc = AvcDecoderConfigurationRecord::parse(&descriptor.configuration).unwrap();
        let sps = parse_sps(&avcc.sequence_parameter_sets[0]).unwrap();
        let vui = sps.vui.unwrap();
        assert_eq!(vui.num_units_in_tick, Some(1));
        assert_eq!(vui.time_scale, Some(180_000));
        assert_eq!(vui.fixed_frame_rate, Some(false));
        let hrd = vui.nal_hrd.unwrap();
        assert_eq!(hrd.cpb_count, 1);
        assert!((25_000..25_064).contains(&hrd.bit_rate));
        assert!((25_000..25_016).contains(&hrd.cpb_size));
        assert!(!hrd.cbr);
        assert_eq!(hrd.initial_cpb_removal_delay_length, 24);
        assert_eq!(hrd.cpb_removal_delay_length, 24);
        assert_eq!(hrd.dpb_output_delay_length, 24);
        assert_eq!(
            encoder.rate_control.as_ref().unwrap().buffer_capacity_bits,
            i128::from(hrd.cpb_size)
        );

        let first = frame_with_pts(patterned_frame(32, 16), 0);
        let middle = shifted_frame(&first, 1, 0, 1);
        let future = shifted_frame(&first, 2, 0, 2);
        encoder.send_frame(first).unwrap();
        let idr_packet = encoder.receive_packet().unwrap().unwrap();
        let idr_reconstruction = encoder.receive_reconstructed_frame().unwrap().unwrap();
        encoder.send_frame(middle).unwrap();
        encoder.send_frame(future).unwrap();
        let p_packet = encoder.receive_packet().unwrap().unwrap();
        let b_packet = encoder.receive_packet().unwrap().unwrap();
        let p_reconstruction = encoder.receive_reconstructed_frame().unwrap().unwrap();
        let b_reconstruction = encoder.receive_reconstructed_frame().unwrap().unwrap();

        let packets = [idr_packet, p_packet, b_packet];
        let expected_delays = [(0, 7_200), (7_200, 14_400), (14_400, 0)];
        for (index, (packet, (expected_cpb, expected_dpb))) in
            packets.iter().zip(expected_delays).enumerate()
        {
            let units = crate::length_prefixed_nal_units(&packet.data, 4).unwrap();
            assert_eq!(units.len(), 2);
            assert_eq!(units[0].header.unit_type, NalUnitType::Sei);
            let timing = crate::parse_hrd_sei(units[0].data, &vui).unwrap();
            assert_eq!(timing.cpb_removal_delay, Some(expected_cpb));
            assert_eq!(timing.dpb_output_delay, Some(expected_dpb));
            if index == 0 {
                assert_eq!(timing.sequence_parameter_set_id, Some(0));
                assert_eq!(
                    timing.initial_cpb_removal_delay,
                    Some(
                        encoder
                            .hrd_state
                            .as_ref()
                            .unwrap()
                            .parameters
                            .initial_cpb_removal_delay
                    )
                );
            } else {
                assert_eq!(timing.sequence_parameter_set_id, None);
            }
        }

        let decode_order = [
            idr_reconstruction.clone(),
            p_reconstruction.clone(),
            b_reconstruction.clone(),
        ];
        let mut decoder = H264Decoder::default();
        decoder.configure(&descriptor).unwrap();
        for (packet, expected) in packets.iter().cloned().zip(&decode_order) {
            decoder.send_packet(packet).unwrap();
            assert_frames_pixels_equal(&decoder.receive_frame().unwrap().unwrap(), expected);
        }
        verify_sequence_with_ffmpeg(
            &avcc,
            &packets,
            &[idr_reconstruction, b_reconstruction, p_reconstruction],
        );
    }

    #[test]
    fn hrd_vbv_rejects_an_access_unit_larger_than_the_cpb() {
        let mut undersized = settings(16, 16);
        undersized.options.insert("mode".into(), "intra4".into());
        undersized
            .options
            .insert("vbv_buffer_ms".into(), "1".into());
        undersized.bitrate = Some(1_000);
        let mut constrained = H264Encoder::default();
        constrained.configure(&undersized).unwrap();
        assert!(constrained.send_frame(patterned_frame(16, 16)).is_err());
        assert!(constrained.receive_packet().unwrap().is_none());
    }

    #[test]
    fn flush_closes_input_until_reconfigured() {
        let mut encoder = H264Encoder::default();
        encoder.configure(&settings(16, 16)).unwrap();
        encoder.flush().unwrap();
        assert!(encoder.send_frame(patterned_frame(16, 16)).is_err());
        encoder.configure(&settings(16, 16)).unwrap();
        encoder.send_frame(patterned_frame(16, 16)).unwrap();
    }

    #[test]
    fn intra16_mode_codes_full_residuals_and_reports_normative_reconstruction() {
        let mut options = BTreeMap::new();
        options.insert("mode".into(), "intra16".into());
        let mut settings = settings(32, 16);
        settings.options = options;
        let mut encoder = H264Encoder::default();
        let descriptor = encoder.configure(&settings).unwrap();
        let frame = constant_frame(32, 16, [128, 128, 128]);
        encoder.send_frame(frame.clone()).unwrap();
        let packet = encoder.receive_packet().unwrap().unwrap();
        assert!(
            packet.data.len() < 32,
            "flat Intra16 packet was not compact"
        );

        let avcc = AvcDecoderConfigurationRecord::parse(&descriptor.configuration).unwrap();
        verify_with_ffmpeg(&avcc, &packet.data, &frame);
        let mut decoder = H264Decoder::default();
        decoder.configure(&descriptor).unwrap();
        decoder.send_packet(packet).unwrap();
        let decoded_frame = decoder.receive_frame().unwrap().unwrap();
        assert_eq!(decoded_frame.planes[0].data, vec![128; 32 * 16]);
        assert_eq!(decoded_frame.planes[1].data, vec![128; 16 * 8]);
        assert_eq!(decoded_frame.planes[2].data, vec![128; 16 * 8]);

        let mut encoder = H264Encoder::default();
        encoder.configure(&settings).unwrap();
        let patterned = patterned_frame(32, 16);
        encoder.send_frame(patterned.clone()).unwrap();
        let packet = encoder.receive_packet().unwrap().unwrap();
        let reconstructed = encoder.receive_reconstructed_frame().unwrap().unwrap();
        assert!(
            packet.data.len() < 384,
            "Intra16 packet did not compress luma"
        );
        assert_ne!(reconstructed.planes[0].data, patterned.planes[0].data);
        assert!(
            luma_squared_error(&patterned, &reconstructed)
                < luma_squared_error_from_constant(&patterned, 128)
        );
        verify_with_ffmpeg(&avcc, &packet.data, &reconstructed);

        let mut decoder = H264Decoder::default();
        decoder.configure(&descriptor).unwrap();
        decoder.send_packet(packet).unwrap();
        let decoded_frame = decoder.receive_frame().unwrap().unwrap();
        assert_frames_pixels_equal(&decoded_frame, &reconstructed);
    }

    #[test]
    fn intra4_mode_round_trips_normative_reconstruction_and_is_deterministic() {
        let mut settings = settings(32, 32);
        settings.options.insert("mode".into(), "intra4".into());
        let frame = patterned_frame(32, 32);

        let mut encoder = H264Encoder::default();
        let descriptor = encoder.configure(&settings).unwrap();
        encoder.send_frame(frame.clone()).unwrap();
        let packet = encoder.receive_packet().unwrap().unwrap();
        let reconstructed = encoder.receive_reconstructed_frame().unwrap().unwrap();
        assert_ne!(reconstructed.planes[0].data, frame.planes[0].data);
        assert!(
            luma_squared_error(&frame, &reconstructed)
                < luma_squared_error_from_constant(&frame, 128)
        );

        let mut second = H264Encoder::default();
        second.configure(&settings).unwrap();
        second.send_frame(frame).unwrap();
        assert_eq!(packet.data, second.receive_packet().unwrap().unwrap().data);

        let avcc = AvcDecoderConfigurationRecord::parse(&descriptor.configuration).unwrap();
        verify_with_ffmpeg(&avcc, &packet.data, &reconstructed);
        let mut decoder = H264Decoder::default();
        decoder.configure(&descriptor).unwrap();
        decoder.send_packet(packet).unwrap();
        let decoded_frame = decoder.receive_frame().unwrap().unwrap();
        assert_frames_pixels_equal(&decoded_frame, &reconstructed);
    }

    #[test]
    fn high_profile_intra8_round_trips_normative_reconstruction() {
        let mut settings = settings(32, 32);
        settings.options.insert("mode".into(), "intra8".into());
        let frame = patterned_frame(32, 32);

        let mut encoder = H264Encoder::default();
        let descriptor = encoder.configure(&settings).unwrap();
        let avcc = AvcDecoderConfigurationRecord::parse(&descriptor.configuration).unwrap();
        let sps = parse_sps(&avcc.sequence_parameter_sets[0]).unwrap();
        let pps = parse_pps(&avcc.picture_parameter_sets[0]).unwrap();
        assert_eq!(sps.profile_idc, PROFILE_HIGH);
        assert_eq!(sps.chroma_format_idc, 1);
        assert_eq!((sps.bit_depth_luma, sps.bit_depth_chroma), (8, 8));
        assert!(pps.transform_8x8_mode);
        assert_eq!(avcc.profile_indication, PROFILE_HIGH);

        encoder.send_frame(frame.clone()).unwrap();
        let packet = encoder.receive_packet().unwrap().unwrap();
        let reconstructed = encoder.receive_reconstructed_frame().unwrap().unwrap();
        assert_ne!(reconstructed.planes[0].data, frame.planes[0].data);
        assert!(
            luma_squared_error(&frame, &reconstructed)
                < luma_squared_error_from_constant(&frame, 128)
        );

        let mut repeat = H264Encoder::default();
        repeat.configure(&settings).unwrap();
        repeat.send_frame(frame).unwrap();
        assert_eq!(packet.data, repeat.receive_packet().unwrap().unwrap().data);

        verify_with_ffmpeg(&avcc, &packet.data, &reconstructed);
        let mut decoder = H264Decoder::default();
        decoder.configure(&descriptor).unwrap();
        decoder.send_packet(packet).unwrap();
        let decoded_frame = decoder.receive_frame().unwrap().unwrap();
        assert_frames_pixels_equal(&decoded_frame, &reconstructed);
    }

    #[test]
    fn all_nine_intra4_predictions_are_available_with_complete_neighbors() {
        let stride = 32;
        let plane = (0..stride * stride)
            .map(|index| u8::try_from((index * 37 + index / stride * 13) & 0xff).unwrap())
            .collect::<Vec<_>>();
        for mode in 0..=8 {
            assert!(
                intra4_prediction(&plane, stride, 20, 20, 12, mode, true, true).is_some(),
                "mode {mode} rejected complete neighbor samples"
            );
        }
        assert!(intra4_prediction(&plane, stride, 0, 0, 0, 0, false, false).is_none());
        assert!(intra4_prediction(&plane, stride, 0, 0, 0, 1, false, false).is_none());
        assert!(intra4_prediction(&plane, stride, 0, 0, 0, 2, false, false).is_some());
    }

    #[test]
    fn all_nine_intra8_predictions_are_available_with_complete_neighbors() {
        let stride = 48;
        let plane = (0..stride * stride)
            .map(|index| u8::try_from((index * 29 + index / stride * 17) & 0xff).unwrap())
            .collect::<Vec<_>>();
        for mode in 0..=8 {
            assert!(
                intra8_prediction(&plane, stride, 20, 20, 0, mode, true, true).is_some(),
                "mode {mode} rejected complete neighbor samples"
            );
        }
        assert!(intra8_prediction(&plane, stride, 0, 0, 0, 0, false, false).is_none());
        assert!(intra8_prediction(&plane, stride, 0, 0, 0, 1, false, false).is_none());
        assert!(intra8_prediction(&plane, stride, 0, 0, 0, 2, false, false).is_some());
        assert!(intra8_prediction(&plane, stride, 0, 0, 0, 3, false, false).is_none());
        assert!(intra8_prediction(&plane, stride, 0, 0, 0, 8, false, false).is_none());
    }

    #[test]
    fn adaptive_inter_transform_selects_between_4x4_and_8x8() {
        let source = vec![152_u8; 16 * 16];
        let prediction = vec![96_u8; 16 * 16];
        let four_by_four = select_inter_luma_residual(
            &source,
            16,
            0,
            0,
            &prediction,
            26,
            false,
            false,
            ScalingMatrixPreset::Flat,
        );
        let adaptive = select_inter_luma_residual(
            &source,
            16,
            0,
            0,
            &prediction,
            26,
            true,
            false,
            ScalingMatrixPreset::Flat,
        );
        assert!(!four_by_four.uses_8x8());
        assert!(adaptive.uses_8x8());
        assert_ne!(adaptive.coded_block_pattern(), 0);

        let mut localized_source = prediction.clone();
        for y in 0..4 {
            localized_source[y * 16..y * 16 + 4].fill(152);
        }
        let localized = select_inter_luma_residual(
            &localized_source,
            16,
            0,
            0,
            &prediction,
            26,
            true,
            false,
            ScalingMatrixPreset::Flat,
        );
        assert!(!localized.uses_8x8());
    }

    #[test]
    fn high_profile_inter_8x8_round_trips_p_and_b_pictures() {
        let mut settings = settings(32, 16);
        settings.options.insert("mode".into(), "inter".into());
        settings.options.insert("profile".into(), "high".into());
        settings.options.insert("b_frames".into(), "1".into());
        settings.options.insert("search_range".into(), "0".into());
        let frames = [
            frame_with_pts(constant_frame(32, 16, [64, 96, 160]), 0),
            frame_with_pts(constant_frame(32, 16, [150, 120, 140]), 1),
            frame_with_pts(constant_frame(32, 16, [192, 128, 128]), 2),
        ];

        let mut encoder = H264Encoder::default();
        let descriptor = encoder.configure(&settings).unwrap();
        let avcc = AvcDecoderConfigurationRecord::parse(&descriptor.configuration).unwrap();
        assert_eq!(
            parse_sps(&avcc.sequence_parameter_sets[0])
                .unwrap()
                .profile_idc,
            PROFILE_HIGH
        );
        assert!(
            parse_pps(&avcc.picture_parameter_sets[0])
                .unwrap()
                .transform_8x8_mode
        );
        for frame in &frames {
            encoder.send_frame(frame.clone()).unwrap();
        }
        encoder.flush().unwrap();
        let mut packets = Vec::new();
        let mut reconstructions = Vec::new();
        while let Some(packet) = encoder.receive_packet().unwrap() {
            packets.push(packet);
            reconstructions.push(encoder.receive_reconstructed_frame().unwrap().unwrap());
        }
        assert_eq!(packets.len(), frames.len());

        let mut decoder = H264Decoder::default();
        decoder.configure(&descriptor).unwrap();
        for (packet, expected) in packets.iter().cloned().zip(&reconstructions) {
            decoder.send_packet(packet).unwrap();
            assert_frames_pixels_equal(&decoder.receive_frame().unwrap().unwrap(), expected);
        }
        let mut display_order = packets
            .iter()
            .zip(&reconstructions)
            .map(|(packet, frame)| (packet.pts.unwrap().value, frame.clone()))
            .collect::<Vec<_>>();
        display_order.sort_by_key(|(pts, _)| *pts);
        let display_order = display_order
            .into_iter()
            .map(|(_, frame)| frame)
            .collect::<Vec<_>>();
        verify_sequence_with_ffmpeg(&avcc, &packets, &display_order);
    }

    #[test]
    fn high_profile_transform_bypass_is_lossless_for_intra_p_and_b_pictures() {
        let mut intra_settings = settings(32, 32);
        intra_settings
            .options
            .insert("mode".into(), "intra4".into());
        intra_settings.options.insert("qp".into(), "0".into());
        intra_settings
            .options
            .insert("transform_bypass".into(), "true".into());
        let intra_frame = patterned_frame(32, 32);
        let mut intra_encoder = H264Encoder::default();
        let intra_descriptor = intra_encoder.configure(&intra_settings).unwrap();
        let intra_avcc =
            AvcDecoderConfigurationRecord::parse(&intra_descriptor.configuration).unwrap();
        let intra_sps = parse_sps(&intra_avcc.sequence_parameter_sets[0]).unwrap();
        assert_eq!(intra_sps.profile_idc, PROFILE_HIGH);
        assert!(intra_sps.qpprime_y_zero_transform_bypass);
        intra_encoder.send_frame(intra_frame.clone()).unwrap();
        let intra_packet = intra_encoder.receive_packet().unwrap().unwrap();
        let intra_reconstruction = intra_encoder
            .receive_reconstructed_frame()
            .unwrap()
            .unwrap();
        assert_frames_pixels_equal(&intra_reconstruction, &intra_frame);
        verify_with_ffmpeg(&intra_avcc, &intra_packet.data, &intra_frame);
        let mut intra_decoder = H264Decoder::default();
        intra_decoder.configure(&intra_descriptor).unwrap();
        intra_decoder.send_packet(intra_packet).unwrap();
        assert_frames_pixels_equal(
            &intra_decoder.receive_frame().unwrap().unwrap(),
            &intra_frame,
        );

        let mut inter_settings = settings(32, 16);
        inter_settings.options.insert("mode".into(), "inter".into());
        inter_settings.options.insert("qp".into(), "0".into());
        inter_settings
            .options
            .insert("transform_bypass".into(), "true".into());
        inter_settings.options.insert("b_frames".into(), "1".into());
        inter_settings
            .options
            .insert("search_range".into(), "2".into());
        let first = frame_with_pts(patterned_frame(32, 16), 0);
        let frames = [
            first.clone(),
            shifted_frame(&first, 1, 0, 1),
            shifted_frame(&first, 2, 0, 2),
        ];
        let mut inter_encoder = H264Encoder::default();
        let inter_descriptor = inter_encoder.configure(&inter_settings).unwrap();
        let inter_avcc =
            AvcDecoderConfigurationRecord::parse(&inter_descriptor.configuration).unwrap();
        let inter_sps = parse_sps(&inter_avcc.sequence_parameter_sets[0]).unwrap();
        let inter_picture_pps = parse_pps(&inter_avcc.picture_parameter_sets[0]).unwrap();
        assert!(inter_sps.qpprime_y_zero_transform_bypass);
        assert!(!inter_picture_pps.transform_8x8_mode);
        for frame in &frames {
            inter_encoder.send_frame(frame.clone()).unwrap();
        }
        inter_encoder.flush().unwrap();
        let mut packets = Vec::new();
        let mut reconstructions = Vec::new();
        while let Some(packet) = inter_encoder.receive_packet().unwrap() {
            let reconstruction = inter_encoder
                .receive_reconstructed_frame()
                .unwrap()
                .unwrap();
            let expected = &frames[usize::try_from(packet.pts.unwrap().value).unwrap()];
            assert_frames_pixels_equal(&reconstruction, expected);
            packets.push(packet);
            reconstructions.push(reconstruction);
        }
        let mut inter_decoder = H264Decoder::default();
        inter_decoder.configure(&inter_descriptor).unwrap();
        for (packet, expected) in packets.iter().cloned().zip(&reconstructions) {
            inter_decoder.send_packet(packet).unwrap();
            assert_frames_pixels_equal(&inter_decoder.receive_frame().unwrap().unwrap(), expected);
        }
        verify_sequence_with_ffmpeg(&inter_avcc, &packets, &frames);
    }

    #[test]
    fn transform_bypass_rejects_incompatible_configurations() {
        for (mode, profile, qp) in [
            ("intra4", "main", "0"),
            ("intra8", "high", "0"),
            ("inter", "high", "1"),
        ] {
            let mut invalid = settings(16, 16);
            invalid.options.insert("mode".into(), mode.into());
            invalid.options.insert("profile".into(), profile.into());
            invalid.options.insert("qp".into(), qp.into());
            invalid
                .options
                .insert("transform_bypass".into(), "true".into());
            assert!(H264Encoder::default().configure(&invalid).is_err());
        }
    }

    #[test]
    fn jvt_scaling_matrices_round_trip_all_transform_paths() {
        for mode in ["intra16", "intra4", "intra8"] {
            let mut encoder_settings = settings(32, 32);
            encoder_settings.options.insert("mode".into(), mode.into());
            encoder_settings
                .options
                .insert("scaling_matrix".into(), "jvt".into());
            let frame = patterned_frame(32, 32);
            let mut encoder = H264Encoder::default();
            let descriptor = encoder.configure(&encoder_settings).unwrap();
            let avcc = AvcDecoderConfigurationRecord::parse(&descriptor.configuration).unwrap();
            let sps = parse_sps(&avcc.sequence_parameter_sets[0]).unwrap();
            assert_eq!(sps.profile_idc, PROFILE_HIGH);
            assert!(sps.scaling_matrix_present);
            assert_eq!(sps.scaling_matrices.four_by_four[0], JVT_4X4_INTRA);
            assert_eq!(sps.scaling_matrices.four_by_four[3], JVT_4X4_INTER);
            assert_eq!(sps.scaling_matrices.eight_by_eight[0], JVT_8X8_INTRA);
            assert_eq!(sps.scaling_matrices.eight_by_eight[1], JVT_8X8_INTER);

            encoder.send_frame(frame).unwrap();
            let packet = encoder.receive_packet().unwrap().unwrap();
            let reconstruction = encoder.receive_reconstructed_frame().unwrap().unwrap();
            verify_with_ffmpeg(&avcc, &packet.data, &reconstruction);
            let mut decoder = H264Decoder::default();
            decoder.configure(&descriptor).unwrap();
            decoder.send_packet(packet).unwrap();
            assert_frames_pixels_equal(&decoder.receive_frame().unwrap().unwrap(), &reconstruction);
        }

        let mut encoder_settings = settings(32, 16);
        encoder_settings
            .options
            .insert("mode".into(), "inter".into());
        encoder_settings
            .options
            .insert("b_frames".into(), "1".into());
        encoder_settings
            .options
            .insert("scaling_matrix".into(), "jvt".into());
        encoder_settings
            .options
            .insert("search_range".into(), "0".into());
        let frames = [
            frame_with_pts(constant_frame(32, 16, [40, 80, 160]), 0),
            frame_with_pts(constant_frame(32, 16, [137, 111, 143]), 1),
            frame_with_pts(constant_frame(32, 16, [216, 144, 112]), 2),
        ];
        let mut encoder = H264Encoder::default();
        let descriptor = encoder.configure(&encoder_settings).unwrap();
        let avcc = AvcDecoderConfigurationRecord::parse(&descriptor.configuration).unwrap();
        for frame in &frames {
            encoder.send_frame(frame.clone()).unwrap();
        }
        encoder.flush().unwrap();
        let mut packets = Vec::new();
        let mut reconstructions = Vec::new();
        while let Some(packet) = encoder.receive_packet().unwrap() {
            packets.push(packet);
            reconstructions.push(encoder.receive_reconstructed_frame().unwrap().unwrap());
        }
        let mut decoder = H264Decoder::default();
        decoder.configure(&descriptor).unwrap();
        for (packet, expected) in packets.iter().cloned().zip(&reconstructions) {
            decoder.send_packet(packet).unwrap();
            assert_frames_pixels_equal(&decoder.receive_frame().unwrap().unwrap(), expected);
        }
        let mut display_order = packets
            .iter()
            .zip(&reconstructions)
            .map(|(packet, frame)| (packet.pts.unwrap().value, frame.clone()))
            .collect::<Vec<_>>();
        display_order.sort_by_key(|(pts, _)| *pts);
        let display_order = display_order
            .into_iter()
            .map(|(_, frame)| frame)
            .collect::<Vec<_>>();
        verify_sequence_with_ffmpeg(&avcc, &packets, &display_order);
    }

    #[test]
    fn jvt_scaling_matrices_reject_non_high_and_bypass_modes() {
        for (profile, bypass) in [("main", false), ("high", true)] {
            let mut invalid = settings(16, 16);
            invalid.options.insert("mode".into(), "intra4".into());
            invalid.options.insert("profile".into(), profile.into());
            invalid.options.insert("qp".into(), "0".into());
            invalid
                .options
                .insert("scaling_matrix".into(), "jvt".into());
            invalid
                .options
                .insert("transform_bypass".into(), bypass.to_string());
            assert!(H264Encoder::default().configure(&invalid).is_err());
        }
    }

    #[test]
    fn configurable_qp_round_trips_for_compressed_intra_modes() {
        let frame = patterned_frame(16, 16);
        let mut errors = Vec::new();
        for (mode, qp) in [
            ("intra4", 12),
            ("intra4", 38),
            ("intra16", 12),
            ("intra16", 38),
            ("intra4", 0),
            ("intra16", 51),
            ("intra8", 12),
            ("intra8", 38),
        ] {
            let mut settings = settings(16, 16);
            settings.options.insert("mode".into(), mode.into());
            settings.options.insert("qp".into(), qp.to_string());
            let mut encoder = H264Encoder::default();
            let descriptor = encoder.configure(&settings).unwrap();
            encoder.send_frame(frame.clone()).unwrap();
            let packet = encoder.receive_packet().unwrap().unwrap();
            let reconstructed = encoder.receive_reconstructed_frame().unwrap().unwrap();

            let avcc = AvcDecoderConfigurationRecord::parse(&descriptor.configuration).unwrap();
            verify_with_ffmpeg(&avcc, &packet.data, &reconstructed);
            let mut decoder = H264Decoder::default();
            decoder.configure(&descriptor).unwrap();
            decoder.send_packet(packet).unwrap();
            assert_frames_pixels_equal(&decoder.receive_frame().unwrap().unwrap(), &reconstructed);
            errors.push((mode, qp, luma_squared_error(&frame, &reconstructed)));
        }
        assert!(
            errors[0].2 < errors[1].2,
            "lower Intra4 QP should preserve more luma detail"
        );
        assert!(
            errors[2].2 < errors[3].2,
            "lower Intra16 QP should preserve more luma detail"
        );
        assert!(
            errors[6].2 < errors[7].2,
            "lower Intra8 QP should preserve more luma detail"
        );

        for invalid in ["-1", "52", "not-a-number"] {
            let mut settings = settings(16, 16);
            settings.options.insert("qp".into(), invalid.into());
            assert!(H264Encoder::default().configure(&settings).is_err());
        }
    }

    #[test]
    fn adaptive_quantization_distinguishes_activity_and_wraps_qp_deltas() {
        let frame = mixed_activity_frame(32, 16, 0);
        let mut encoder_settings = settings(32, 16);
        encoder_settings
            .options
            .insert("mode".into(), "intra4".into());
        encoder_settings
            .options
            .insert("aq_strength".into(), "6".into());
        let mut encoder = H264Encoder::default();
        encoder.configure(&encoder_settings).unwrap();
        let configuration = encoder.configuration.as_ref().unwrap();
        let padded = padded_planes(&frame, configuration);
        let qps = adaptive_macroblock_qps(
            &padded[0],
            configuration.coded_width,
            configuration.coded_height,
            configuration.qp,
            configuration.aq_strength,
        );
        assert_eq!(qps.len(), 2);
        assert!(qps[0] < configuration.qp, "{qps:?}");
        assert!(qps[1] > configuration.qp, "{qps:?}");
        assert_eq!(macroblock_qp_delta(51, 0), 1);
        assert_eq!(macroblock_qp_delta(0, 51), -1);
    }

    #[test]
    fn adaptive_quantization_round_trips_intra_p_and_b_pictures() {
        let source = mixed_activity_frame(32, 16, 0);
        for mode in ["intra16", "intra4", "intra8"] {
            let mut encoder_settings = settings(32, 16);
            encoder_settings.options.insert("mode".into(), mode.into());
            encoder_settings
                .options
                .insert("aq_strength".into(), "6".into());
            let mut encoder = H264Encoder::default();
            let descriptor = encoder.configure(&encoder_settings).unwrap();
            encoder.send_frame(source.clone()).unwrap();
            let packet = encoder.receive_packet().unwrap().unwrap();
            let reconstruction = encoder.receive_reconstructed_frame().unwrap().unwrap();

            let mut decoder = H264Decoder::default();
            decoder.configure(&descriptor).unwrap();
            decoder.send_packet(packet.clone()).unwrap();
            assert_frames_pixels_equal(&decoder.receive_frame().unwrap().unwrap(), &reconstruction);
            let avcc = AvcDecoderConfigurationRecord::parse(&descriptor.configuration).unwrap();
            verify_with_ffmpeg(&avcc, &packet.data, &reconstruction);
        }

        let mut encoder_settings = settings(32, 16);
        encoder_settings
            .options
            .insert("mode".into(), "inter".into());
        encoder_settings
            .options
            .insert("b_frames".into(), "1".into());
        encoder_settings
            .options
            .insert("search_range".into(), "0".into());
        encoder_settings
            .options
            .insert("aq_strength".into(), "6".into());
        let mut encoder = H264Encoder::default();
        let descriptor = encoder.configure(&encoder_settings).unwrap();
        let mut middle = mixed_activity_frame(32, 16, 1);
        for plane in &mut middle.planes {
            for y in 0..plane.height {
                for sample in &mut plane.data[y * plane.stride..][..plane.width] {
                    *sample = sample.saturating_add(37);
                }
            }
        }
        let future = frame_with_pts(source.clone(), 2);
        encoder.send_frame(source).unwrap();
        let idr_packet = encoder.receive_packet().unwrap().unwrap();
        let idr_reconstruction = encoder.receive_reconstructed_frame().unwrap().unwrap();
        encoder.send_frame(middle).unwrap();
        encoder.send_frame(future).unwrap();
        let p_packet = encoder.receive_packet().unwrap().unwrap();
        let b_packet = encoder.receive_packet().unwrap().unwrap();
        let p_reconstruction = encoder.receive_reconstructed_frame().unwrap().unwrap();
        let b_reconstruction = encoder.receive_reconstructed_frame().unwrap().unwrap();
        let packets = [idr_packet, p_packet, b_packet];
        let decode_order = [
            idr_reconstruction.clone(),
            p_reconstruction.clone(),
            b_reconstruction.clone(),
        ];
        let mut decoder = H264Decoder::default();
        decoder.configure(&descriptor).unwrap();
        for (packet, expected) in packets.iter().cloned().zip(&decode_order) {
            decoder.send_packet(packet).unwrap();
            assert_frames_pixels_equal(&decoder.receive_frame().unwrap().unwrap(), expected);
        }
        let avcc = AvcDecoderConfigurationRecord::parse(&descriptor.configuration).unwrap();
        verify_sequence_with_ffmpeg(
            &avcc,
            &packets,
            &[idr_reconstruction, b_reconstruction, p_reconstruction],
        );
    }

    #[test]
    fn inter_mode_encodes_motion_skip_and_periodic_idrs() {
        let mut settings = settings(32, 32);
        settings.options.insert("mode".into(), "inter".into());
        settings.options.insert("gop_size".into(), "3".into());
        settings.options.insert("search_range".into(), "4".into());
        let first = patterned_frame(32, 32);
        let second = shifted_frame(&first, 2, 0, 8);
        let third = frame_with_pts(second.clone(), 9);
        let fourth = shifted_frame(&first, 4, 2, 10);
        let frames = [first.clone(), second.clone(), third, fourth];

        let mut encoder = H264Encoder::default();
        let descriptor = encoder.configure(&settings).unwrap();
        let mut packets = Vec::new();
        let mut reconstructions = Vec::new();
        for frame in &frames {
            encoder.send_frame(frame.clone()).unwrap();
            packets.push(encoder.receive_packet().unwrap().unwrap());
            reconstructions.push(encoder.receive_reconstructed_frame().unwrap().unwrap());
        }
        assert_eq!(
            packets
                .iter()
                .map(|packet| packet.flags.contains(PacketFlags::KEY))
                .collect::<Vec<_>>(),
            [true, false, false, true]
        );
        assert_eq!(packet_nal_type(&packets[0]), NalUnitType::IdrSlice);
        assert_eq!(packet_nal_type(&packets[1]), NalUnitType::CodedSlice);
        assert_eq!(packet_nal_type(&packets[2]), NalUnitType::CodedSlice);
        assert_eq!(packet_nal_type(&packets[3]), NalUnitType::IdrSlice);
        assert!(
            packets[2].data.len() < packets[1].data.len(),
            "an unchanged reference picture should collapse into P-skip runs"
        );

        let avcc = AvcDecoderConfigurationRecord::parse(&descriptor.configuration).unwrap();
        verify_sequence_with_ffmpeg(&avcc, &packets, &reconstructions);
        let mut decoder = H264Decoder::default();
        decoder.configure(&descriptor).unwrap();
        for (packet, expected) in packets.iter().cloned().zip(&reconstructions) {
            decoder.send_packet(packet).unwrap();
            assert_frames_pixels_equal(&decoder.receive_frame().unwrap().unwrap(), expected);
        }

        let mut repeat = H264Encoder::default();
        repeat.configure(&settings).unwrap();
        for (frame, expected_packet) in frames.iter().zip(&packets) {
            repeat.send_frame(frame.clone()).unwrap();
            let packet = repeat.receive_packet().unwrap().unwrap();
            assert_eq!(packet.data, expected_packet.data);
            assert_eq!(packet.flags, expected_packet.flags);
        }

        let mut zero_search_settings = settings.clone();
        zero_search_settings
            .options
            .insert("search_range".into(), "0".into());
        let mut zero_search = H264Encoder::default();
        zero_search.configure(&zero_search_settings).unwrap();
        zero_search.send_frame(first).unwrap();
        let _first_packet = zero_search.receive_packet().unwrap().unwrap();
        zero_search.send_frame(second).unwrap();
        let zero_search_packet = zero_search.receive_packet().unwrap().unwrap();
        assert!(
            packets[1].data.len() < zero_search_packet.data.len(),
            "integer motion search should reduce shifted-frame residual coding"
        );
    }

    #[test]
    fn inter_mode_wraps_frame_num_and_preserves_cropped_dimensions() {
        let mut settings = settings(18, 20);
        settings.options.insert("mode".into(), "inter".into());
        settings.options.insert("gop_size".into(), "20".into());
        settings.options.insert("search_range".into(), "0".into());
        let mut encoder = H264Encoder::default();
        let descriptor = encoder.configure(&settings).unwrap();
        let mut packets = Vec::new();
        let mut reconstructions = Vec::new();
        for pts in 0..18 {
            let frame = frame_with_pts(constant_frame(18, 20, [128, 96, 160]), pts);
            encoder.send_frame(frame).unwrap();
            packets.push(encoder.receive_packet().unwrap().unwrap());
            reconstructions.push(encoder.receive_reconstructed_frame().unwrap().unwrap());
        }
        assert!(packets[0].flags.contains(PacketFlags::KEY));
        assert!(
            packets[1..]
                .iter()
                .all(|packet| !packet.flags.contains(PacketFlags::KEY))
        );
        assert!(
            reconstructions
                .iter()
                .all(|frame| (frame.width, frame.height) == (18, 20))
        );

        let mut decoder = H264Decoder::default();
        decoder.configure(&descriptor).unwrap();
        for (packet, expected) in packets.into_iter().zip(&reconstructions) {
            decoder.send_packet(packet).unwrap();
            assert_frames_pixels_equal(&decoder.receive_frame().unwrap().unwrap(), expected);
        }
    }

    #[test]
    fn inter_mode_selects_and_decodes_both_split_partition_shapes() {
        for (horizontal_split, expected_type) in [(true, 1), (false, 2)] {
            let mut settings = settings(16, 16);
            settings.options.insert("mode".into(), "inter".into());
            settings.options.insert("gop_size".into(), "10".into());
            settings.options.insert("search_range".into(), "4".into());
            let mut encoder = H264Encoder::default();
            let descriptor = encoder.configure(&settings).unwrap();
            encoder.send_frame(patterned_frame(16, 16)).unwrap();
            let idr_packet = encoder.receive_packet().unwrap().unwrap();
            let idr_reconstruction = encoder.receive_reconstructed_frame().unwrap().unwrap();
            let split_frame = split_motion_frame(&idr_reconstruction, horizontal_split, 8);

            let configuration = encoder.configuration.as_ref().unwrap();
            let source = padded_planes(&split_frame, configuration);
            let decision = select_inter_partitions(
                &source,
                &encoder.references,
                configuration.coded_width,
                configuration.coded_height,
                0,
                0,
                configuration.search_range,
                &[[None; 16]],
                0,
                1,
            );
            assert_eq!(decision.macroblock_type, expected_type);

            encoder.send_frame(split_frame).unwrap();
            let p_packet = encoder.receive_packet().unwrap().unwrap();
            let p_reconstruction = encoder.receive_reconstructed_frame().unwrap().unwrap();
            let packets = [idr_packet, p_packet];
            let reconstructions = [idr_reconstruction, p_reconstruction];
            let avcc = AvcDecoderConfigurationRecord::parse(&descriptor.configuration).unwrap();
            verify_sequence_with_ffmpeg(&avcc, &packets, &reconstructions);

            let mut decoder = H264Decoder::default();
            decoder.configure(&descriptor).unwrap();
            for (packet, expected) in packets.into_iter().zip(&reconstructions) {
                decoder.send_packet(packet).unwrap();
                assert_frames_pixels_equal(&decoder.receive_frame().unwrap().unwrap(), expected);
            }
        }
    }

    #[test]
    fn inter_mode_can_insert_scene_cut_idrs() {
        let mut settings = settings(16, 16);
        settings.options.insert("mode".into(), "inter".into());
        settings.options.insert("gop_size".into(), "30".into());
        settings
            .options
            .insert("scene_cut_threshold".into(), "32".into());
        let frames = [
            frame_with_pts(constant_frame(16, 16, [16, 128, 128]), 0),
            frame_with_pts(constant_frame(16, 16, [240, 128, 128]), 1),
        ];
        let mut encoder = H264Encoder::default();
        let descriptor = encoder.configure(&settings).unwrap();
        let mut packets = Vec::new();
        let mut reconstructions = Vec::new();
        for frame in &frames {
            encoder.send_frame(frame.clone()).unwrap();
            packets.push(encoder.receive_packet().unwrap().unwrap());
            reconstructions.push(encoder.receive_reconstructed_frame().unwrap().unwrap());
        }
        assert!(
            packets
                .iter()
                .all(|packet| packet.flags.contains(PacketFlags::KEY))
        );
        let avcc = AvcDecoderConfigurationRecord::parse(&descriptor.configuration).unwrap();
        verify_sequence_with_ffmpeg(&avcc, &packets, &reconstructions);
        let mut decoder = H264Decoder::default();
        decoder.configure(&descriptor).unwrap();
        for (packet, expected) in packets.into_iter().zip(&reconstructions) {
            decoder.send_packet(packet).unwrap();
            assert_frames_pixels_equal(&decoder.receive_frame().unwrap().unwrap(), expected);
        }

        settings
            .options
            .insert("scene_cut_threshold".into(), "0".into());
        let mut fixed_gop = H264Encoder::default();
        fixed_gop.configure(&settings).unwrap();
        for frame in frames {
            fixed_gop.send_frame(frame).unwrap();
        }
        assert!(
            fixed_gop
                .receive_packet()
                .unwrap()
                .unwrap()
                .flags
                .contains(PacketFlags::KEY)
        );
        assert!(
            !fixed_gop
                .receive_packet()
                .unwrap()
                .unwrap()
                .flags
                .contains(PacketFlags::KEY)
        );
    }

    #[test]
    fn inter_mode_encodes_every_p8x8_subpartition_shape() {
        let mut settings = settings(16, 16);
        settings.options.insert("mode".into(), "inter".into());
        settings.options.insert("search_range".into(), "4".into());
        let mut encoder = H264Encoder::default();
        let descriptor = encoder.configure(&settings).unwrap();
        encoder.send_frame(patterned_frame(16, 16)).unwrap();
        let idr_packet = encoder.receive_packet().unwrap().unwrap();
        let idr_reconstruction = encoder.receive_reconstructed_frame().unwrap().unwrap();
        let partitioned = subpartition_motion_frame(&idr_reconstruction, 8);

        let configuration = encoder.configuration.as_ref().unwrap();
        let source = padded_planes(&partitioned, configuration);
        let decision = select_inter_partitions(
            &source,
            &encoder.references,
            16,
            16,
            0,
            0,
            4,
            &[[None; 16]],
            0,
            1,
        );
        assert_eq!(decision.macroblock_type, 3);
        assert_eq!(decision.sub_macroblock_types, [0, 1, 2, 3]);

        encoder.send_frame(partitioned).unwrap();
        let p_packet = encoder.receive_packet().unwrap().unwrap();
        let p_reconstruction = encoder.receive_reconstructed_frame().unwrap().unwrap();
        let packets = [idr_packet, p_packet];
        let reconstructions = [idr_reconstruction, p_reconstruction];
        let avcc = AvcDecoderConfigurationRecord::parse(&descriptor.configuration).unwrap();
        verify_sequence_with_ffmpeg(&avcc, &packets, &reconstructions);
        let mut decoder = H264Decoder::default();
        decoder.configure(&descriptor).unwrap();
        for (packet, expected) in packets.into_iter().zip(&reconstructions) {
            decoder.send_packet(packet).unwrap();
            assert_frames_pixels_equal(&decoder.receive_frame().unwrap().unwrap(), expected);
        }
    }

    #[test]
    fn inter_mode_refines_integer_search_to_quarter_pixel_motion() {
        let mut settings = settings(16, 16);
        settings.options.insert("mode".into(), "inter".into());
        settings.options.insert("search_range".into(), "2".into());
        let mut encoder = H264Encoder::default();
        let descriptor = encoder.configure(&settings).unwrap();
        encoder.send_frame(patterned_frame(16, 16)).unwrap();
        let idr_packet = encoder.receive_packet().unwrap().unwrap();
        let idr_reconstruction = encoder.receive_reconstructed_frame().unwrap().unwrap();
        let fractional =
            fractional_motion_frame(&idr_reconstruction, MotionVector { x: 1, y: 2 }, 8);

        let configuration = encoder.configuration.as_ref().unwrap();
        let source = padded_planes(&fractional, configuration);
        let decision = select_inter_partitions(
            &source,
            &encoder.references,
            16,
            16,
            0,
            0,
            2,
            &[[None; 16]],
            0,
            1,
        );
        assert_eq!(decision.macroblock_type, 0);
        assert_eq!(decision.partitions[0].motion, MotionVector { x: 1, y: 2 });

        encoder.send_frame(fractional).unwrap();
        let p_packet = encoder.receive_packet().unwrap().unwrap();
        let p_reconstruction = encoder.receive_reconstructed_frame().unwrap().unwrap();
        let packets = [idr_packet, p_packet];
        let reconstructions = [idr_reconstruction, p_reconstruction];
        let avcc = AvcDecoderConfigurationRecord::parse(&descriptor.configuration).unwrap();
        verify_sequence_with_ffmpeg(&avcc, &packets, &reconstructions);
        let mut decoder = H264Decoder::default();
        decoder.configure(&descriptor).unwrap();
        for (packet, expected) in packets.into_iter().zip(&reconstructions) {
            decoder.send_packet(packet).unwrap();
            assert_frames_pixels_equal(&decoder.receive_frame().unwrap().unwrap(), expected);
        }
    }

    #[test]
    fn inter_mode_selects_an_older_short_term_reference() {
        let mut settings = settings(16, 16);
        settings.options.insert("mode".into(), "inter".into());
        settings.options.insert("max_refs".into(), "2".into());
        settings.options.insert("search_range".into(), "0".into());
        let mut encoder = H264Encoder::default();
        let descriptor = encoder.configure(&settings).unwrap();

        let first = frame_with_pts(patterned_frame(16, 16), 0);
        encoder.send_frame(first).unwrap();
        let first_packet = encoder.receive_packet().unwrap().unwrap();
        let first_reconstruction = encoder.receive_reconstructed_frame().unwrap().unwrap();
        let second = frame_with_pts(constant_frame(16, 16, [220, 32, 224]), 1);
        encoder.send_frame(second).unwrap();
        let second_packet = encoder.receive_packet().unwrap().unwrap();
        let second_reconstruction = encoder.receive_reconstructed_frame().unwrap().unwrap();

        assert_eq!(encoder.references.len(), 2);
        let third = frame_with_pts(first_reconstruction.clone(), 2);
        let configuration = encoder.configuration.as_ref().unwrap();
        let source = padded_planes(&third, configuration);
        let decision = select_inter_partitions(
            &source,
            &encoder.references,
            16,
            16,
            0,
            0,
            0,
            &[[None; 16]],
            0,
            1,
        );
        assert_eq!(decision.partitions[0].reference_index, 1);

        encoder.send_frame(third).unwrap();
        let third_packet = encoder.receive_packet().unwrap().unwrap();
        let third_reconstruction = encoder.receive_reconstructed_frame().unwrap().unwrap();
        assert_frames_pixels_equal(&third_reconstruction, &first_reconstruction);
        let packets = [first_packet, second_packet, third_packet];
        let reconstructions = [
            first_reconstruction,
            second_reconstruction,
            third_reconstruction,
        ];
        let avcc = AvcDecoderConfigurationRecord::parse(&descriptor.configuration).unwrap();
        verify_sequence_with_ffmpeg(&avcc, &packets, &reconstructions);
        let mut decoder = H264Decoder::default();
        decoder.configure(&descriptor).unwrap();
        for (packet, expected) in packets.into_iter().zip(&reconstructions) {
            decoder.send_packet(packet).unwrap();
            assert_frames_pixels_equal(&decoder.receive_frame().unwrap().unwrap(), expected);
        }
    }

    #[test]
    fn inter_mode_reorders_and_round_trips_bidirectional_b_pictures() {
        let mut settings = settings(16, 16);
        settings.options.insert("mode".into(), "inter".into());
        settings.options.insert("max_refs".into(), "2".into());
        settings.options.insert("b_frames".into(), "1".into());
        settings.options.insert("gop_size".into(), "12".into());
        settings.options.insert("search_range".into(), "4".into());
        settings.bitrate = Some(1_000);
        let mut encoder = H264Encoder::default();
        let descriptor = encoder.configure(&settings).unwrap();
        let avcc = AvcDecoderConfigurationRecord::parse(&descriptor.configuration).unwrap();
        let sps = parse_sps(&avcc.sequence_parameter_sets[0]).unwrap();
        assert_eq!(sps.max_num_ref_frames, 2);
        assert!(matches!(
            sps.pic_order_cnt_type,
            crate::PictureOrderCountType::Type0 { .. }
        ));

        let first = frame_with_pts(patterned_frame(16, 16), 0);
        let middle = shifted_frame(&first, 1, 0, 1);
        let future = shifted_frame(&first, 2, 0, 2);
        encoder.send_frame(first).unwrap();
        let idr_packet = encoder.receive_packet().unwrap().unwrap();
        let idr_reconstruction = encoder.receive_reconstructed_frame().unwrap().unwrap();
        encoder.send_frame(middle).unwrap();
        assert!(encoder.receive_packet().unwrap().is_none());
        assert!(encoder.receive_reconstructed_frame().unwrap().is_none());
        encoder.send_frame(future).unwrap();
        let future_packet = encoder.receive_packet().unwrap().unwrap();
        let b_packet = encoder.receive_packet().unwrap().unwrap();
        let future_reconstruction = encoder.receive_reconstructed_frame().unwrap().unwrap();
        let b_reconstruction = encoder.receive_reconstructed_frame().unwrap().unwrap();
        assert!(encoder.rate_control.as_ref().unwrap().current_qp > 26);

        assert_eq!(
            [
                idr_packet.pts.unwrap().value,
                future_packet.pts.unwrap().value,
                b_packet.pts.unwrap().value
            ],
            [0, 2, 1]
        );
        assert_eq!(
            [
                idr_packet.dts.unwrap().value,
                future_packet.dts.unwrap().value,
                b_packet.dts.unwrap().value
            ],
            [0, 1, 2]
        );
        let b_units = crate::length_prefixed_nal_units(&b_packet.data, 4).unwrap();
        assert_eq!(b_units[0].header.reference_idc, 0);

        let decode_order_packets = [idr_packet, future_packet, b_packet];
        let decode_order_reconstructions = [
            idr_reconstruction.clone(),
            future_reconstruction.clone(),
            b_reconstruction.clone(),
        ];
        let mut decoder = H264Decoder::default();
        decoder.configure(&descriptor).unwrap();
        for (packet, expected) in decode_order_packets
            .iter()
            .cloned()
            .zip(&decode_order_reconstructions)
        {
            decoder.send_packet(packet).unwrap();
            assert_frames_pixels_equal(&decoder.receive_frame().unwrap().unwrap(), expected);
        }
        verify_sequence_with_ffmpeg(
            &avcc,
            &decode_order_packets,
            &[idr_reconstruction, b_reconstruction, future_reconstruction],
        );
    }

    #[test]
    fn fast_analysis_round_trips_hierarchical_p_and_direct_b_pictures() {
        let mut settings = settings(32, 16);
        for (name, value) in [
            ("mode", "inter"),
            ("profile", "high"),
            ("entropy", "cabac"),
            ("analysis", "fast"),
            ("max_refs", "2"),
            ("b_frames", "1"),
            ("search_range", "8"),
        ] {
            settings.options.insert(name.into(), value.into());
        }
        let mut encoder = H264Encoder::default();
        let descriptor = encoder.configure(&settings).unwrap();
        let first = frame_with_pts(patterned_frame(32, 16), 0);
        let middle = shifted_frame(&first, 1, 0, 1);
        let future = shifted_frame(&first, 2, 0, 2);
        encoder.send_frame(first).unwrap();
        let idr_packet = encoder.receive_packet().unwrap().unwrap();
        let idr_reconstruction = encoder.receive_reconstructed_frame().unwrap().unwrap();
        encoder.send_frame(middle).unwrap();
        encoder.send_frame(future).unwrap();
        let p_packet = encoder.receive_packet().unwrap().unwrap();
        let b_packet = encoder.receive_packet().unwrap().unwrap();
        let p_reconstruction = encoder.receive_reconstructed_frame().unwrap().unwrap();
        let b_reconstruction = encoder.receive_reconstructed_frame().unwrap().unwrap();
        let packets = [idr_packet, p_packet, b_packet];
        let mut decoder = H264Decoder::default();
        decoder.configure(&descriptor).unwrap();
        for (packet, expected) in
            packets
                .iter()
                .cloned()
                .zip([&idr_reconstruction, &p_reconstruction, &b_reconstruction])
        {
            decoder.send_packet(packet).unwrap();
            assert_frames_pixels_equal(&decoder.receive_frame().unwrap().unwrap(), expected);
        }
        let avcc = AvcDecoderConfigurationRecord::parse(&descriptor.configuration).unwrap();
        verify_sequence_with_ffmpeg(
            &avcc,
            &packets,
            &[idr_reconstruction, b_reconstruction, p_reconstruction],
        );
    }

    #[test]
    fn fast_analysis_splits_high_error_partial_motion_instead_of_leaving_ghosts() {
        let mut settings = settings(32, 16);
        for (name, value) in [
            ("mode", "inter"),
            ("profile", "high"),
            ("entropy", "cabac"),
            ("analysis", "fast"),
            ("search_range", "8"),
        ] {
            settings.options.insert(name.into(), value.into());
        }
        let mut first = frame_with_pts(constant_frame(32, 16, [16, 128, 128]), 0);
        let first_stride = first.planes[0].stride;
        for y in 4..12 {
            for x in 0..8 {
                first.planes[0].data[y * first_stride + x] = 235;
            }
        }
        let mut encoder = H264Encoder::default();
        encoder.configure(&settings).unwrap();
        encoder.send_frame(first).unwrap();
        encoder.receive_packet().unwrap().unwrap();
        encoder.receive_reconstructed_frame().unwrap().unwrap();

        let mut moved = frame_with_pts(constant_frame(32, 16, [16, 128, 128]), 1);
        let moved_stride = moved.planes[0].stride;
        for y in 4..12 {
            for x in 8..16 {
                moved.planes[0].data[y * moved_stride + x] = 235;
            }
        }
        let configuration = encoder.configuration.as_ref().unwrap();
        let source = padded_planes(&moved, configuration);
        let decision = select_inter_partitions_with_analysis(
            &source,
            &encoder.references,
            configuration.coded_width,
            configuration.coded_height,
            0,
            0,
            configuration.search_range,
            AnalysisPreset::Fast,
            &[[None; 16]; 2],
            0,
            2,
        );
        assert_ne!(decision.macroblock_type, 0);
        assert!(decision.partitions.len() > 1);
    }

    #[test]
    fn inter_mode_reorders_three_consecutive_b_pictures() {
        let mut settings = settings(16, 16);
        settings.options.insert("mode".into(), "inter".into());
        settings.options.insert("b_frames".into(), "3".into());
        settings.options.insert("gop_size".into(), "20".into());
        settings.options.insert("search_range".into(), "2".into());
        let mut encoder = H264Encoder::default();
        let descriptor = encoder.configure(&settings).unwrap();
        let first = frame_with_pts(patterned_frame(16, 16), 0);
        let frames = [
            first.clone(),
            shifted_frame(&first, 1, 0, 1),
            shifted_frame(&first, 2, 0, 2),
            shifted_frame(&first, 3, 0, 3),
            shifted_frame(&first, 4, 0, 4),
        ];
        let mut packets = Vec::new();
        let mut reconstructions = Vec::new();
        for (index, frame) in frames.into_iter().enumerate() {
            encoder.send_frame(frame).unwrap();
            while let Some(packet) = encoder.receive_packet().unwrap() {
                packets.push(packet);
                reconstructions.push(encoder.receive_reconstructed_frame().unwrap().unwrap());
            }
            if (1..=3).contains(&index) {
                assert_eq!(packets.len(), 1);
            }
        }
        assert_eq!(
            packets
                .iter()
                .map(|packet| packet.pts.unwrap().value)
                .collect::<Vec<_>>(),
            [0, 4, 1, 2, 3]
        );
        assert_eq!(
            packets
                .iter()
                .map(|packet| packet.dts.unwrap().value)
                .collect::<Vec<_>>(),
            [0, 1, 2, 3, 4]
        );
        let mut decoder = H264Decoder::default();
        decoder.configure(&descriptor).unwrap();
        for (packet, expected) in packets.iter().cloned().zip(&reconstructions) {
            decoder.send_packet(packet).unwrap();
            assert_frames_pixels_equal(&decoder.receive_frame().unwrap().unwrap(), expected);
        }
        let mut display_order = reconstructions.clone();
        display_order.sort_by_key(|frame| frame.timing.pts.unwrap().value);
        let avcc = AvcDecoderConfigurationRecord::parse(&descriptor.configuration).unwrap();
        verify_sequence_with_ffmpeg(&avcc, &packets, &display_order);
    }

    #[test]
    fn delayed_b_input_drains_safely_at_flush_and_gop_boundaries() {
        let mut settings = settings(16, 16);
        settings.options.insert("mode".into(), "inter".into());
        settings.options.insert("b_frames".into(), "3".into());
        let mut draining = H264Encoder::default();
        draining.configure(&settings).unwrap();
        draining
            .send_frame(frame_with_pts(patterned_frame(16, 16), 0))
            .unwrap();
        draining.receive_packet().unwrap().unwrap();
        draining.receive_reconstructed_frame().unwrap().unwrap();
        for pts in 1..=3 {
            draining
                .send_frame(frame_with_pts(patterned_frame(16, 16), pts))
                .unwrap();
        }
        assert!(draining.receive_packet().unwrap().is_none());
        draining.flush().unwrap();
        let drained = std::iter::from_fn(|| draining.receive_packet().unwrap()).collect::<Vec<_>>();
        assert_eq!(
            drained
                .iter()
                .map(|packet| (packet.pts.unwrap().value, packet.dts.unwrap().value))
                .collect::<Vec<_>>(),
            [(1, 1), (2, 2), (3, 3)]
        );

        let mut boundary_settings = settings.clone();
        boundary_settings
            .options
            .insert("gop_size".into(), "2".into());
        let mut boundary = H264Encoder::default();
        boundary.configure(&boundary_settings).unwrap();
        for pts in 0..3 {
            boundary
                .send_frame(frame_with_pts(patterned_frame(16, 16), pts))
                .unwrap();
        }
        let boundary_packets =
            std::iter::from_fn(|| boundary.receive_packet().unwrap()).collect::<Vec<_>>();
        assert_eq!(boundary_packets.len(), 3);
        assert_eq!(
            boundary_packets
                .iter()
                .map(|packet| packet.pts.unwrap().value)
                .collect::<Vec<_>>(),
            [0, 1, 2]
        );
        assert!(!boundary_packets[1].flags.contains(PacketFlags::KEY));
        assert!(boundary_packets[2].flags.contains(PacketFlags::KEY));
    }

    #[test]
    fn b_motion_selector_supports_direct_unidirectional_and_split_prediction() {
        let mut encoder_settings = settings(16, 16);
        encoder_settings
            .options
            .insert("mode".into(), "inter".into());
        encoder_settings
            .options
            .insert("b_frames".into(), "1".into());
        encoder_settings
            .options
            .insert("search_range".into(), "0".into());
        let mut encoder = H264Encoder::default();
        encoder.configure(&encoder_settings).unwrap();
        let configuration = encoder.configuration.as_ref().unwrap();
        let previous_frame = constant_frame(16, 16, [20, 80, 100]);
        let future_frame = constant_frame(16, 16, [220, 160, 180]);
        let previous = EncoderReference {
            planes: padded_planes(&previous_frame, configuration),
            pic_order_count: 0,
            motion_l0: vec![[None; 16]],
            reference_l0_poc: vec![[None; 16]],
            macroblock_intra: vec![true],
        };
        let future = EncoderReference {
            planes: padded_planes(&future_frame, configuration),
            pic_order_count: 4,
            motion_l0: vec![[None; 16]],
            reference_l0_poc: vec![[None; 16]],
            macroblock_intra: vec![true],
        };
        let horizontal_split = split_between_frames(&previous_frame, &future_frame, true);
        let vertical_split = split_between_frames(&previous_frame, &future_frame, false);
        for (source_frame, expected_type) in [
            (previous_frame.clone(), 1),
            (future_frame.clone(), 2),
            (constant_frame(16, 16, [120, 120, 140]), 0),
            (horizontal_split, 8),
            (vertical_split, 9),
        ] {
            let source = padded_planes(&source_frame, configuration);
            let decision = select_b_inter_partitions(
                &source,
                &previous,
                &future,
                16,
                16,
                0,
                0,
                0,
                &[[None; 16]],
                &[[None; 16]],
                0,
                1,
                BDirectContext::SPATIAL,
            );
            assert_eq!(decision.macroblock_type, expected_type);
            assert_eq!(decision.direct, expected_type == 0);
        }
    }

    #[test]
    fn spatial_direct_uses_nonzero_neighbors_and_colocated_zero_overrides() {
        let previous_frame = constant_frame(32, 16, [20, 80, 100]);
        let future_frame = constant_frame(32, 16, [220, 160, 180]);
        let average = constant_frame(32, 16, [120, 120, 140]);
        let mut wide_encoder = H264Encoder::default();
        wide_encoder.configure(&settings(32, 16)).unwrap();
        let wide_configuration = wide_encoder.configuration.as_ref().unwrap();
        let previous = EncoderReference {
            planes: padded_planes(&previous_frame, wide_configuration),
            pic_order_count: 0,
            motion_l0: vec![[None; 16]; 2],
            reference_l0_poc: vec![[None; 16]; 2],
            macroblock_intra: vec![true; 2],
        };
        let future_motion = Some(MotionState {
            vector: MotionVector { x: 8, y: 0 },
            reference_index: Some(0),
        });
        let future = EncoderReference {
            planes: padded_planes(&future_frame, wide_configuration),
            pic_order_count: 4,
            motion_l0: vec![[future_motion; 16]; 2],
            reference_l0_poc: vec![[Some(0); 16]; 2],
            macroblock_intra: vec![false; 2],
        };
        let neighbor = Some(MotionState {
            vector: MotionVector { x: 4, y: 0 },
            reference_index: Some(0),
        });
        let decision = select_b_inter_partitions(
            &padded_planes(&average, wide_configuration),
            &previous,
            &future,
            32,
            16,
            1,
            0,
            0,
            &[[neighbor; 16], [None; 16]],
            &[[neighbor; 16], [None; 16]],
            1,
            2,
            BDirectContext::SPATIAL,
        );
        assert_eq!(decision.macroblock_type, 0);
        assert!(decision.direct);
        assert!(decision.partitions.iter().all(|partition| {
            partition
                .list0
                .is_some_and(|selected| selected.motion == MotionVector { x: 4, y: 0 })
                && partition
                    .list1
                    .is_some_and(|selected| selected.motion == MotionVector { x: 4, y: 0 })
        }));

        let mut future_with_colocated_zero = future.clone();
        future_with_colocated_zero.motion_l0[1][luma_block_index(0, 0)] = Some(MotionState {
            vector: MotionVector::default(),
            reference_index: Some(0),
        });
        let (_, direct) = select_spatial_direct_b_prediction(
            &padded_planes(&average, wide_configuration),
            &previous,
            &future_with_colocated_zero,
            32,
            16,
            1,
            0,
            &[[neighbor; 16], [None; 16]],
            &[[neighbor; 16], [None; 16]],
            1,
            2,
        );
        assert_eq!(
            direct.partitions[0].list0.unwrap().motion,
            MotionVector::default()
        );
        assert!(direct.partitions[1..].iter().all(|partition| {
            partition
                .list0
                .is_some_and(|selected| selected.motion == MotionVector { x: 4, y: 0 })
        }));
    }

    #[test]
    fn inter_mode_encodes_spatial_direct_b_skip_runs() {
        let mut settings = settings(32, 16);
        settings.options.insert("mode".into(), "inter".into());
        settings.options.insert("max_refs".into(), "2".into());
        settings.options.insert("b_frames".into(), "1".into());
        settings.options.insert("search_range".into(), "0".into());
        let mut encoder = H264Encoder::default();
        let descriptor = encoder.configure(&settings).unwrap();
        let stable = constant_frame(32, 16, [96, 128, 160]);

        encoder
            .send_frame(frame_with_pts(stable.clone(), 0))
            .unwrap();
        let idr_packet = encoder.receive_packet().unwrap().unwrap();
        let idr_reconstruction = encoder.receive_reconstructed_frame().unwrap().unwrap();
        encoder
            .send_frame(frame_with_pts(idr_reconstruction.clone(), 1))
            .unwrap();
        encoder
            .send_frame(frame_with_pts(idr_reconstruction.clone(), 2))
            .unwrap();
        let future_packet = encoder.receive_packet().unwrap().unwrap();
        let b_packet = encoder.receive_packet().unwrap().unwrap();
        let future_reconstruction = encoder.receive_reconstructed_frame().unwrap().unwrap();
        let b_reconstruction = encoder.receive_reconstructed_frame().unwrap().unwrap();

        let source = padded_planes(&idr_reconstruction, encoder.configuration.as_ref().unwrap());
        let decision = select_b_inter_partitions(
            &source,
            &encoder.references[1],
            &encoder.references[0],
            32,
            16,
            0,
            0,
            0,
            &[[None; 16]; 2],
            &[[None; 16]; 2],
            0,
            2,
            BDirectContext::SPATIAL,
        );
        assert!(decision.direct);

        let units = crate::length_prefixed_nal_units(&b_packet.data, 4).unwrap();
        let rbsp = remove_emulation_prevention(&units[0].data[1..]);
        let mut bits = BitReader::new(&rbsp);
        assert_eq!(read_test_ue(&mut bits), 0); // first_mb_in_slice
        assert_eq!(read_test_ue(&mut bits), 1); // B slice
        assert_eq!(read_test_ue(&mut bits), 0); // PPS
        bits.skip_bits(8).unwrap(); // frame_num and POC LSB
        assert!(bits.read_bit().unwrap()); // spatial direct
        assert!(!bits.read_bit().unwrap()); // default reference counts
        assert!(!bits.read_bit().unwrap()); // list 0 modification
        assert!(!bits.read_bit().unwrap()); // list 1 modification
        assert_eq!(read_test_ue(&mut bits), 0); // slice_qp_delta
        assert_eq!(read_test_ue(&mut bits), 1); // deblocking disabled
        assert_eq!(read_test_ue(&mut bits), 2); // two skipped macroblocks

        let packets = [idr_packet, future_packet, b_packet];
        let mut decoder = H264Decoder::default();
        decoder.configure(&descriptor).unwrap();
        for (packet, expected) in packets.iter().cloned().zip([
            &idr_reconstruction,
            &future_reconstruction,
            &b_reconstruction,
        ]) {
            decoder.send_packet(packet).unwrap();
            assert_frames_pixels_equal(&decoder.receive_frame().unwrap().unwrap(), expected);
        }
        let avcc = AvcDecoderConfigurationRecord::parse(&descriptor.configuration).unwrap();
        verify_sequence_with_ffmpeg(
            &avcc,
            &packets,
            &[idr_reconstruction, b_reconstruction, future_reconstruction],
        );
    }

    #[test]
    fn inter_mode_encodes_spatial_direct_residuals() {
        let mut settings = settings(16, 16);
        settings.options.insert("mode".into(), "inter".into());
        settings.options.insert("max_refs".into(), "2".into());
        settings.options.insert("b_frames".into(), "1".into());
        settings.options.insert("search_range".into(), "0".into());
        settings.options.insert("qp".into(), "0".into());
        let mut encoder = H264Encoder::default();
        let descriptor = encoder.configure(&settings).unwrap();

        encoder
            .send_frame(frame_with_pts(constant_frame(16, 16, [96, 128, 160]), 0))
            .unwrap();
        let idr_packet = encoder.receive_packet().unwrap().unwrap();
        let idr_reconstruction = encoder.receive_reconstructed_frame().unwrap().unwrap();
        let middle = frame_with_pts(constant_frame(16, 16, [120, 144, 176]), 1);
        encoder.send_frame(middle.clone()).unwrap();
        encoder
            .send_frame(frame_with_pts(idr_reconstruction.clone(), 2))
            .unwrap();
        let future_packet = encoder.receive_packet().unwrap().unwrap();
        let b_packet = encoder.receive_packet().unwrap().unwrap();
        let future_reconstruction = encoder.receive_reconstructed_frame().unwrap().unwrap();
        let b_reconstruction = encoder.receive_reconstructed_frame().unwrap().unwrap();

        let decision = select_b_inter_partitions(
            &padded_planes(&middle, encoder.configuration.as_ref().unwrap()),
            &encoder.references[1],
            &encoder.references[0],
            16,
            16,
            0,
            0,
            0,
            &[[None; 16]],
            &[[None; 16]],
            0,
            1,
            BDirectContext::SPATIAL,
        );
        assert!(decision.direct);

        let units = crate::length_prefixed_nal_units(&b_packet.data, 4).unwrap();
        let rbsp = remove_emulation_prevention(&units[0].data[1..]);
        let mut bits = BitReader::new(&rbsp);
        assert_eq!(read_test_ue(&mut bits), 0);
        assert_eq!(read_test_ue(&mut bits), 1);
        assert_eq!(read_test_ue(&mut bits), 0);
        bits.skip_bits(8).unwrap();
        for expected in [true, false, false, false] {
            assert_eq!(bits.read_bit().unwrap(), expected);
        }
        assert_eq!(read_test_ue(&mut bits), 52); // slice_qp_delta = -26
        assert_eq!(read_test_ue(&mut bits), 1);
        assert_eq!(read_test_ue(&mut bits), 0); // no skipped macroblock
        assert_eq!(read_test_ue(&mut bits), 0); // B_Direct_16x16
        assert_ne!(read_test_ue(&mut bits), 0); // nonzero coded-block pattern

        let packets = [idr_packet, future_packet, b_packet];
        let mut decoder = H264Decoder::default();
        decoder.configure(&descriptor).unwrap();
        for (packet, expected) in packets.iter().cloned().zip([
            &idr_reconstruction,
            &future_reconstruction,
            &b_reconstruction,
        ]) {
            decoder.send_packet(packet).unwrap();
            assert_frames_pixels_equal(&decoder.receive_frame().unwrap().unwrap(), expected);
        }
        let avcc = AvcDecoderConfigurationRecord::parse(&descriptor.configuration).unwrap();
        verify_sequence_with_ffmpeg(
            &avcc,
            &packets,
            &[idr_reconstruction, b_reconstruction, future_reconstruction],
        );
    }

    #[test]
    fn inter_mode_encodes_nonzero_spatial_direct_motion() {
        let mut settings = settings(32, 16);
        settings.options.insert("mode".into(), "inter".into());
        settings.options.insert("max_refs".into(), "2".into());
        settings.options.insert("b_frames".into(), "1".into());
        settings.options.insert("search_range".into(), "4".into());
        let mut encoder = H264Encoder::default();
        let descriptor = encoder.configure(&settings).unwrap();
        let first = frame_with_pts(patterned_frame(32, 16), 0);
        let middle = shifted_frame(&first, 1, 0, 1);
        let future = shifted_frame(&first, 2, 0, 2);

        encoder.send_frame(first).unwrap();
        let idr_packet = encoder.receive_packet().unwrap().unwrap();
        let idr_reconstruction = encoder.receive_reconstructed_frame().unwrap().unwrap();
        encoder.send_frame(middle.clone()).unwrap();
        encoder.send_frame(future).unwrap();
        let future_packet = encoder.receive_packet().unwrap().unwrap();
        let b_packet = encoder.receive_packet().unwrap().unwrap();
        let future_reconstruction = encoder.receive_reconstructed_frame().unwrap().unwrap();
        let b_reconstruction = encoder.receive_reconstructed_frame().unwrap().unwrap();

        let source = padded_planes(&middle, encoder.configuration.as_ref().unwrap());
        let mut history_l0 = vec![[None; 16]; 2];
        let mut history_l1 = vec![[None; 16]; 2];
        let first_decision = select_b_inter_partitions(
            &source,
            &encoder.references[1],
            &encoder.references[0],
            32,
            16,
            0,
            0,
            4,
            &history_l0,
            &history_l1,
            0,
            2,
            BDirectContext::SPATIAL,
        );
        commit_b_motion_history(&first_decision, &mut history_l0[0], &mut history_l1[0]);
        let second_decision = select_b_inter_partitions(
            &source,
            &encoder.references[1],
            &encoder.references[0],
            32,
            16,
            1,
            0,
            4,
            &history_l0,
            &history_l1,
            1,
            2,
            BDirectContext::SPATIAL,
        );
        assert!(second_decision.direct, "{second_decision:?}");
        assert!(second_decision.partitions.iter().any(|partition| {
            partition
                .list0
                .is_some_and(|selected| selected.motion != MotionVector::default())
                || partition
                    .list1
                    .is_some_and(|selected| selected.motion != MotionVector::default())
        }));

        let packets = [idr_packet, future_packet, b_packet];
        let mut decoder = H264Decoder::default();
        decoder.configure(&descriptor).unwrap();
        for (packet, expected) in packets.iter().cloned().zip([
            &idr_reconstruction,
            &future_reconstruction,
            &b_reconstruction,
        ]) {
            decoder.send_packet(packet).unwrap();
            assert_frames_pixels_equal(&decoder.receive_frame().unwrap().unwrap(), expected);
        }
        let avcc = AvcDecoderConfigurationRecord::parse(&descriptor.configuration).unwrap();
        verify_sequence_with_ffmpeg(
            &avcc,
            &packets,
            &[idr_reconstruction, b_reconstruction, future_reconstruction],
        );
    }

    #[test]
    fn temporal_direct_scales_colocated_motion_and_requires_reference_identity() {
        assert_eq!(
            temporal_direct_motion_vectors(MotionVector { x: 8, y: -4 }, 2, 4, 0),
            (MotionVector { x: 4, y: -2 }, MotionVector { x: -4, y: 2 })
        );
        let mut encoder = H264Encoder::default();
        encoder.configure(&settings(16, 16)).unwrap();
        let configuration = encoder.configuration.as_ref().unwrap();
        let previous = EncoderReference {
            planes: padded_planes(&patterned_frame(16, 16), configuration),
            pic_order_count: 0,
            motion_l0: vec![[None; 16]],
            reference_l0_poc: vec![[None; 16]],
            macroblock_intra: vec![true],
        };
        let colocated = Some(MotionState {
            vector: MotionVector { x: 8, y: -4 },
            reference_index: Some(0),
        });
        let mut future = EncoderReference {
            planes: previous.planes.clone(),
            pic_order_count: 4,
            motion_l0: vec![[colocated; 16]],
            reference_l0_poc: vec![[Some(0); 16]],
            macroblock_intra: vec![false],
        };
        let partition =
            temporal_direct_partition(&previous, &future, 0, direct_8x8_partition(0), 2).unwrap();
        assert_eq!(
            partition.list0.unwrap().motion,
            MotionVector { x: 4, y: -2 }
        );
        assert_eq!(
            partition.list1.unwrap().motion,
            MotionVector { x: -4, y: 2 }
        );
        future.reference_l0_poc[0].fill(Some(-2));
        assert!(
            temporal_direct_partition(&previous, &future, 0, direct_8x8_partition(0), 2).is_none()
        );
    }

    #[test]
    fn inter_mode_encodes_temporal_direct_b_skip() {
        let mut settings = settings(16, 16);
        settings.options.insert("mode".into(), "inter".into());
        settings.options.insert("max_refs".into(), "2".into());
        settings.options.insert("b_frames".into(), "1".into());
        settings
            .options
            .insert("b_direct".into(), "temporal".into());
        settings.options.insert("search_range".into(), "4".into());
        let first = frame_with_pts(patterned_frame(16, 16), 0);
        let future = shifted_frame(&first, 2, 0, 2);
        let middle = temporal_direct_test_frame(&settings, &first, &future);
        let mut encoder = H264Encoder::default();
        let descriptor = encoder.configure(&settings).unwrap();

        encoder.send_frame(first).unwrap();
        let idr_packet = encoder.receive_packet().unwrap().unwrap();
        let idr_reconstruction = encoder.receive_reconstructed_frame().unwrap().unwrap();
        encoder.send_frame(middle.clone()).unwrap();
        encoder.send_frame(future).unwrap();
        let future_packet = encoder.receive_packet().unwrap().unwrap();
        let b_packet = encoder.receive_packet().unwrap().unwrap();
        let future_reconstruction = encoder.receive_reconstructed_frame().unwrap().unwrap();
        let b_reconstruction = encoder.receive_reconstructed_frame().unwrap().unwrap();

        let decision = select_b_inter_partitions(
            &padded_planes(&middle, encoder.configuration.as_ref().unwrap()),
            &encoder.references[1],
            &encoder.references[0],
            16,
            16,
            0,
            0,
            4,
            &[[None; 16]],
            &[[None; 16]],
            0,
            1,
            BDirectContext {
                mode: BDirectMode::Temporal,
                picture_order_count: 2,
            },
        );
        assert!(decision.direct);
        assert!(decision.partitions.iter().any(|partition| {
            partition
                .list0
                .is_some_and(|selected| selected.motion != MotionVector::default())
        }));

        let units = crate::length_prefixed_nal_units(&b_packet.data, 4).unwrap();
        let rbsp = remove_emulation_prevention(&units[0].data[1..]);
        let mut bits = BitReader::new(&rbsp);
        assert_eq!(read_test_ue(&mut bits), 0);
        assert_eq!(read_test_ue(&mut bits), 1);
        assert_eq!(read_test_ue(&mut bits), 0);
        bits.skip_bits(8).unwrap();
        assert!(!bits.read_bit().unwrap()); // temporal direct
        for _ in 0..3 {
            assert!(!bits.read_bit().unwrap());
        }
        assert_eq!(read_test_ue(&mut bits), 0);
        assert_eq!(read_test_ue(&mut bits), 1);
        assert_eq!(read_test_ue(&mut bits), 1);

        let packets = [idr_packet, future_packet, b_packet];
        let mut decoder = H264Decoder::default();
        decoder.configure(&descriptor).unwrap();
        for (packet, expected) in packets.iter().cloned().zip([
            &idr_reconstruction,
            &future_reconstruction,
            &b_reconstruction,
        ]) {
            decoder.send_packet(packet).unwrap();
            assert_frames_pixels_equal(&decoder.receive_frame().unwrap().unwrap(), expected);
        }
        let avcc = AvcDecoderConfigurationRecord::parse(&descriptor.configuration).unwrap();
        verify_sequence_with_ffmpeg(
            &avcc,
            &packets,
            &[idr_reconstruction, b_reconstruction, future_reconstruction],
        );
    }

    #[test]
    fn inter_mode_encodes_both_b_split_partition_shapes() {
        for (horizontal, expected_type) in [(true, 8), (false, 9)] {
            let mut settings = settings(16, 16);
            settings.options.insert("mode".into(), "inter".into());
            settings.options.insert("max_refs".into(), "2".into());
            settings.options.insert("b_frames".into(), "1".into());
            settings.options.insert("search_range".into(), "0".into());
            let mut encoder = H264Encoder::default();
            let descriptor = encoder.configure(&settings).unwrap();
            let previous = frame_with_pts(constant_frame(16, 16, [20, 80, 100]), 0);
            let future = frame_with_pts(constant_frame(16, 16, [220, 160, 180]), 2);
            let middle = frame_with_pts(split_between_frames(&previous, &future, horizontal), 1);
            encoder.send_frame(previous).unwrap();
            let idr_packet = encoder.receive_packet().unwrap().unwrap();
            let idr_reconstruction = encoder.receive_reconstructed_frame().unwrap().unwrap();
            encoder.send_frame(middle.clone()).unwrap();
            encoder.send_frame(future).unwrap();
            let future_packet = encoder.receive_packet().unwrap().unwrap();
            let b_packet = encoder.receive_packet().unwrap().unwrap();
            let future_reconstruction = encoder.receive_reconstructed_frame().unwrap().unwrap();
            let b_reconstruction = encoder.receive_reconstructed_frame().unwrap().unwrap();

            let configuration = encoder.configuration.as_ref().unwrap();
            let source = padded_planes(&middle, configuration);
            let decision = select_b_inter_partitions(
                &source,
                &encoder.references[1],
                &encoder.references[0],
                16,
                16,
                0,
                0,
                0,
                &[[None; 16]],
                &[[None; 16]],
                0,
                1,
                BDirectContext::SPATIAL,
            );
            assert_eq!(decision.macroblock_type, expected_type);

            let packets = [idr_packet, future_packet, b_packet];
            let decode_order = [
                idr_reconstruction.clone(),
                future_reconstruction.clone(),
                b_reconstruction.clone(),
            ];
            let mut decoder = H264Decoder::default();
            decoder.configure(&descriptor).unwrap();
            for (packet, expected) in packets.iter().cloned().zip(&decode_order) {
                decoder.send_packet(packet).unwrap();
                assert_frames_pixels_equal(&decoder.receive_frame().unwrap().unwrap(), expected);
            }
            let avcc = AvcDecoderConfigurationRecord::parse(&descriptor.configuration).unwrap();
            verify_sequence_with_ffmpeg(
                &avcc,
                &packets,
                &[idr_reconstruction, b_reconstruction, future_reconstruction],
            );
        }
    }

    #[test]
    fn inter_mode_encodes_every_b8x8_subpartition_shape() {
        let mut settings = settings(16, 16);
        settings.options.insert("mode".into(), "inter".into());
        settings.options.insert("max_refs".into(), "2".into());
        settings.options.insert("b_frames".into(), "1".into());
        settings.options.insert("search_range".into(), "4".into());
        let mut encoder = H264Encoder::default();
        let descriptor = encoder.configure(&settings).unwrap();
        encoder
            .send_frame(frame_with_pts(patterned_frame(16, 16), 0))
            .unwrap();
        let idr_packet = encoder.receive_packet().unwrap().unwrap();
        let idr_reconstruction = encoder.receive_reconstructed_frame().unwrap().unwrap();
        let middle = subpartition_motion_frame(&idr_reconstruction, 1);
        let future = frame_with_pts(idr_reconstruction.clone(), 2);
        encoder.send_frame(middle.clone()).unwrap();
        encoder.send_frame(future).unwrap();
        let future_packet = encoder.receive_packet().unwrap().unwrap();
        let b_packet = encoder.receive_packet().unwrap().unwrap();
        let future_reconstruction = encoder.receive_reconstructed_frame().unwrap().unwrap();
        let b_reconstruction = encoder.receive_reconstructed_frame().unwrap().unwrap();

        let configuration = encoder.configuration.as_ref().unwrap();
        let source = padded_planes(&middle, configuration);
        let decision = select_b_inter_partitions(
            &source,
            &encoder.references[1],
            &encoder.references[0],
            16,
            16,
            0,
            0,
            4,
            &[[None; 16]],
            &[[None; 16]],
            0,
            1,
            BDirectContext::SPATIAL,
        );
        assert_eq!(decision.macroblock_type, 22);
        assert_eq!(decision.sub_macroblock_types, [1, 4, 5, 11]);

        let packets = [idr_packet, future_packet, b_packet];
        let mut decoder = H264Decoder::default();
        decoder.configure(&descriptor).unwrap();
        for (packet, expected) in packets.iter().cloned().zip([
            &idr_reconstruction,
            &future_reconstruction,
            &b_reconstruction,
        ]) {
            decoder.send_packet(packet).unwrap();
            assert_frames_pixels_equal(&decoder.receive_frame().unwrap().unwrap(), expected);
        }
        let avcc = AvcDecoderConfigurationRecord::parse(&descriptor.configuration).unwrap();
        verify_sequence_with_ffmpeg(
            &avcc,
            &packets,
            &[idr_reconstruction, b_reconstruction, future_reconstruction],
        );
    }

    #[test]
    fn inter_mode_encodes_mixed_direct_b8x8_submacroblocks() {
        let mut settings = settings(16, 16);
        settings.options.insert("mode".into(), "inter".into());
        settings.options.insert("max_refs".into(), "2".into());
        settings.options.insert("b_frames".into(), "1".into());
        settings.options.insert("search_range".into(), "4".into());
        let mut encoder = H264Encoder::default();
        let descriptor = encoder.configure(&settings).unwrap();
        encoder
            .send_frame(frame_with_pts(patterned_frame(16, 16), 0))
            .unwrap();
        let idr_packet = encoder.receive_packet().unwrap().unwrap();
        let idr_reconstruction = encoder.receive_reconstructed_frame().unwrap().unwrap();
        let middle = direct_subpartition_motion_frame(&idr_reconstruction, 1);
        let future = frame_with_pts(idr_reconstruction.clone(), 2);
        encoder.send_frame(middle.clone()).unwrap();
        encoder.send_frame(future).unwrap();
        let future_packet = encoder.receive_packet().unwrap().unwrap();
        let b_packet = encoder.receive_packet().unwrap().unwrap();
        let future_reconstruction = encoder.receive_reconstructed_frame().unwrap().unwrap();
        let b_reconstruction = encoder.receive_reconstructed_frame().unwrap().unwrap();

        let configuration = encoder.configuration.as_ref().unwrap();
        let decision = select_b_inter_partitions(
            &padded_planes(&middle, configuration),
            &encoder.references[1],
            &encoder.references[0],
            16,
            16,
            0,
            0,
            4,
            &[[None; 16]],
            &[[None; 16]],
            0,
            1,
            BDirectContext::SPATIAL,
        );
        assert_eq!(decision.macroblock_type, 22);
        assert_eq!(decision.sub_macroblock_types[0], 0);
        assert!(decision.partitions[0].direct);
        assert!(
            decision.sub_macroblock_types[1..]
                .iter()
                .all(|&kind| kind != 0)
        );

        let packets = [idr_packet, future_packet, b_packet];
        let mut decoder = H264Decoder::default();
        decoder.configure(&descriptor).unwrap();
        for (packet, expected) in packets.iter().cloned().zip([
            &idr_reconstruction,
            &future_reconstruction,
            &b_reconstruction,
        ]) {
            decoder.send_packet(packet).unwrap();
            assert_frames_pixels_equal(&decoder.receive_frame().unwrap().unwrap(), expected);
        }
        let avcc = AvcDecoderConfigurationRecord::parse(&descriptor.configuration).unwrap();
        verify_sequence_with_ffmpeg(
            &avcc,
            &packets,
            &[idr_reconstruction, b_reconstruction, future_reconstruction],
        );
    }

    fn settings(width: usize, height: usize) -> VideoEncoderSettings {
        VideoEncoderSettings {
            width,
            height,
            pixel_format: PixelFormat::Yuv420p8,
            time_base: Rational::new(1, 25).unwrap(),
            bitrate: None,
            options: BTreeMap::new(),
        }
    }

    fn read_test_ue(reader: &mut BitReader<'_>) -> u64 {
        let mut leading_zeroes = 0_u8;
        while !reader.read_bit().unwrap() {
            leading_zeroes += 1;
        }
        if leading_zeroes == 0 {
            0
        } else {
            (1_u64 << leading_zeroes) - 1 + reader.read_bits(leading_zeroes).unwrap()
        }
    }

    fn patterned_frame(width: usize, height: usize) -> VideoFrame {
        let dimensions = [
            (width, height),
            (width / 2, height / 2),
            (width / 2, height / 2),
        ];
        let planes = dimensions
            .into_iter()
            .enumerate()
            .map(|(component, (plane_width, plane_height))| {
                let stride = plane_width + 3;
                let mut data = vec![0; stride * plane_height];
                for y in 0..plane_height {
                    for x in 0..plane_width {
                        data[y * stride + x] =
                            u8::try_from((x * 17 + y * 29 + component * 67) & 0xff).unwrap();
                    }
                }
                Plane {
                    data,
                    stride,
                    width: plane_width,
                    height: plane_height,
                }
            })
            .collect();
        let time_base = Rational::new(1, 25).unwrap();
        VideoFrame {
            format: PixelFormat::Yuv420p8,
            width,
            height,
            planes,
            timing: FrameTiming {
                pts: Some(Timestamp {
                    value: 7,
                    time_base,
                }),
                duration: Some(Timestamp {
                    value: 1,
                    time_base,
                }),
            },
            color: ColorDescription::default(),
            field_order: FieldOrder::Progressive,
        }
    }

    fn constant_frame(width: usize, height: usize, samples: [u8; 3]) -> VideoFrame {
        let mut frame = patterned_frame(width, height);
        for (plane, sample) in frame.planes.iter_mut().zip(samples) {
            plane.data.fill(sample);
        }
        frame
    }

    fn mixed_activity_frame(width: usize, height: usize, pts: i64) -> VideoFrame {
        let texture = patterned_frame(width, height);
        let mut output = constant_frame(width, height, [96, 128, 128]);
        for (destination, source) in output.planes.iter_mut().zip(&texture.planes) {
            for y in 0..destination.height {
                for x in destination.width / 2..destination.width {
                    destination.data[y * destination.stride + x] =
                        source.data[y * source.stride + x];
                }
            }
        }
        frame_with_pts(output, pts)
    }

    fn shifted_frame(source: &VideoFrame, shift_x: usize, shift_y: usize, pts: i64) -> VideoFrame {
        let mut output = source.clone();
        for (component, (destination, source_plane)) in
            output.planes.iter_mut().zip(&source.planes).enumerate()
        {
            let divisor = if component == 0 { 1 } else { 2 };
            let shift_x = shift_x / divisor;
            let shift_y = shift_y / divisor;
            for y in 0..destination.height {
                for x in 0..destination.width {
                    let source_x = x.saturating_sub(shift_x);
                    let source_y = y.saturating_sub(shift_y);
                    destination.data[y * destination.stride + x] =
                        source_plane.data[source_y * source_plane.stride + source_x];
                }
            }
        }
        frame_with_pts(output, pts)
    }

    fn temporal_direct_test_frame(
        settings: &VideoEncoderSettings,
        first: &VideoFrame,
        future: &VideoFrame,
    ) -> VideoFrame {
        let mut probe = H264Encoder::default();
        probe.configure(settings).unwrap();
        probe.send_frame(first.clone()).unwrap();
        probe.receive_packet().unwrap().unwrap();
        probe.receive_reconstructed_frame().unwrap().unwrap();
        probe.send_frame(frame_with_pts(first.clone(), 1)).unwrap();
        probe.send_frame(future.clone()).unwrap();

        let configuration = probe.configuration.as_ref().unwrap();
        let source = padded_planes(first, configuration);
        let (_, direct) = select_temporal_direct_b_prediction(
            &source,
            &probe.references[1],
            &probe.references[0],
            16,
            16,
            0,
            0,
            0,
            2,
        )
        .unwrap();
        assert!(direct.partitions.iter().any(|partition| {
            partition
                .list0
                .is_some_and(|selected| selected.motion != MotionVector::default())
        }));

        let mut output = first.clone();
        for (component, prediction) in std::iter::once(&direct.luma_prediction)
            .chain(direct.chroma_predictions.iter())
            .enumerate()
        {
            let width = if component == 0 { 16 } else { 8 };
            let plane = &mut output.planes[component];
            for y in 0..width {
                plane.data[y * plane.stride..y * plane.stride + width]
                    .copy_from_slice(&prediction[y * width..(y + 1) * width]);
            }
        }
        frame_with_pts(output, 1)
    }

    fn split_between_frames(
        first: &VideoFrame,
        second: &VideoFrame,
        horizontal: bool,
    ) -> VideoFrame {
        let mut output = first.clone();
        for ((destination, first_plane), second_plane) in output
            .planes
            .iter_mut()
            .zip(&first.planes)
            .zip(&second.planes)
        {
            for y in 0..destination.height {
                for x in 0..destination.width {
                    let use_first = if horizontal {
                        y < destination.height / 2
                    } else {
                        x < destination.width / 2
                    };
                    let source = if use_first { first_plane } else { second_plane };
                    destination.data[y * destination.stride + x] =
                        source.data[y * source.stride + x];
                }
            }
        }
        output
    }

    fn split_motion_frame(source: &VideoFrame, horizontal_split: bool, pts: i64) -> VideoFrame {
        let mut output = source.clone();
        let source_plane = &source.planes[0];
        let destination = &mut output.planes[0];
        for y in 0..destination.height {
            for x in 0..destination.width {
                let first_half = if horizontal_split {
                    y < destination.height / 2
                } else {
                    x < destination.width / 2
                };
                let source_x = if first_half {
                    x.saturating_sub(3)
                } else {
                    (x + 3).min(source_plane.width - 1)
                };
                destination.data[y * destination.stride + x] =
                    source_plane.data[y * source_plane.stride + source_x];
            }
        }
        frame_with_pts(output, pts)
    }

    fn subpartition_motion_frame(source: &VideoFrame, pts: i64) -> VideoFrame {
        let mut output = source.clone();
        let source_plane = &source.planes[0];
        let destination = &mut output.planes[0];
        for y in 0..16 {
            for x in 0..16 {
                let sub_index = (y / 8) * 2 + x / 8;
                let local_x = x % 8;
                let local_y = y % 8;
                let displacement = match sub_index {
                    0 => -3,
                    1 => {
                        if local_y < 4 {
                            -3
                        } else {
                            3
                        }
                    }
                    2 => {
                        if local_x < 4 {
                            -3
                        } else {
                            3
                        }
                    }
                    3 => [-3, -1, 1, 3][(local_y / 4) * 2 + local_x / 4],
                    _ => unreachable!("16x16 frame has four 8x8 blocks"),
                };
                let source_x = usize::try_from(
                    (i32::try_from(x).expect("test coordinate fits") + displacement).clamp(0, 15),
                )
                .unwrap();
                destination.data[y * destination.stride + x] =
                    source_plane.data[y * source_plane.stride + source_x];
            }
        }
        frame_with_pts(output, pts)
    }

    fn direct_subpartition_motion_frame(source: &VideoFrame, pts: i64) -> VideoFrame {
        let mut output = subpartition_motion_frame(source, pts);
        let output_stride = output.planes[0].stride;
        let destination = &mut output.planes[0].data;
        for y in 0..8 {
            for x in 0..8 {
                destination[y * output_stride + x] =
                    source.planes[0].data[y * source.planes[0].stride + x];
            }
        }
        output
    }

    fn fractional_motion_frame(source: &VideoFrame, motion: MotionVector, pts: i64) -> VideoFrame {
        let mut output = source.clone();
        for (component, (destination, source_plane)) in
            output.planes.iter_mut().zip(&source.planes).enumerate()
        {
            for y in 0..destination.height {
                for x in 0..destination.width {
                    destination.data[y * destination.stride + x] = if component == 0 {
                        luma_qpel(
                            &source_plane.data,
                            source_plane.width,
                            source_plane.height,
                            i32::try_from(x).expect("test coordinate fits") * 4 + motion.x,
                            i32::try_from(y).expect("test coordinate fits") * 4 + motion.y,
                        )
                    } else {
                        chroma_epel(
                            &source_plane.data,
                            source_plane.width,
                            source_plane.height,
                            i32::try_from(x).expect("test coordinate fits") * 8 + motion.x,
                            i32::try_from(y).expect("test coordinate fits") * 8 + motion.y,
                        )
                    };
                }
            }
        }
        frame_with_pts(output, pts)
    }

    fn frame_with_pts(mut frame: VideoFrame, pts: i64) -> VideoFrame {
        frame.timing.pts.as_mut().expect("test frame has PTS").value = pts;
        frame
    }

    fn packet_nal_type(packet: &Packet) -> NalUnitType {
        crate::length_prefixed_nal_units(&packet.data, 4).unwrap()[0]
            .header
            .unit_type
    }

    fn luma_squared_error(source: &VideoFrame, reconstructed: &VideoFrame) -> i64 {
        let source = &source.planes[0];
        let reconstructed = &reconstructed.planes[0];
        (0..source.height)
            .flat_map(|y| {
                (0..source.width).map(move |x| {
                    let difference = i64::from(source.data[y * source.stride + x])
                        - i64::from(reconstructed.data[y * reconstructed.stride + x]);
                    difference * difference
                })
            })
            .sum()
    }

    fn luma_squared_error_from_constant(source: &VideoFrame, value: u8) -> i64 {
        let plane = &source.planes[0];
        (0..plane.height)
            .flat_map(|y| {
                (0..plane.width).map(move |x| {
                    let difference = i64::from(plane.data[y * plane.stride + x]) - i64::from(value);
                    difference * difference
                })
            })
            .sum()
    }

    fn assert_frames_pixels_equal(actual: &VideoFrame, expected: &VideoFrame) {
        assert_eq!(
            (actual.width, actual.height),
            (expected.width, expected.height)
        );
        for (actual, expected) in actual.planes.iter().zip(&expected.planes) {
            for y in 0..actual.height {
                assert_eq!(
                    &actual.data[y * actual.stride..][..actual.width],
                    &expected.data[y * expected.stride..][..expected.width]
                );
            }
        }
    }

    fn packet_bytes(encoder: &mut H264Encoder, frame: &VideoFrame) -> Vec<u8> {
        encoder
            .configure(&settings(frame.width, frame.height))
            .unwrap();
        encoder.send_frame(frame.clone()).unwrap();
        encoder.receive_packet().unwrap().unwrap().data
    }

    fn verify_with_ffmpeg(
        avcc: &AvcDecoderConfigurationRecord,
        packet: &[u8],
        expected: &VideoFrame,
    ) {
        let mut annex_b = avcc.parameter_sets_annex_b();
        annex_b.extend_from_slice(&crate::nal_units_to_annex_b(packet, 4).unwrap());
        let mut child = match Command::new("ffmpeg")
            .args([
                "-v",
                "error",
                "-f",
                "h264",
                "-i",
                "pipe:0",
                "-frames:v",
                "1",
                "-f",
                "rawvideo",
                "-pix_fmt",
                "yuv420p",
                "pipe:1",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
        {
            Ok(child) => child,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return,
            Err(error) => panic!("start FFmpeg reference decoder: {error}"),
        };
        child.stdin.take().unwrap().write_all(&annex_b).unwrap();
        let output = child.wait_with_output().unwrap();
        assert!(
            output.status.success(),
            "FFmpeg rejected encoded H.264: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let expected_bytes = expected
            .planes
            .iter()
            .flat_map(|plane| {
                (0..plane.height).flat_map(move |y| {
                    plane.data[y * plane.stride..y * plane.stride + plane.width]
                        .iter()
                        .copied()
                })
            })
            .collect::<Vec<_>>();
        assert_eq!(output.stdout, expected_bytes);
    }

    fn verify_sequence_with_ffmpeg(
        avcc: &AvcDecoderConfigurationRecord,
        packets: &[Packet],
        expected: &[VideoFrame],
    ) {
        let mut annex_b = avcc.parameter_sets_annex_b();
        for packet in packets {
            annex_b.extend_from_slice(&crate::nal_units_to_annex_b(&packet.data, 4).unwrap());
        }
        let mut child = match Command::new("ffmpeg")
            .args([
                "-v", "error", "-f", "h264", "-i", "pipe:0", "-f", "rawvideo", "-pix_fmt",
                "yuv420p", "pipe:1",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
        {
            Ok(child) => child,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return,
            Err(error) => panic!("start FFmpeg reference decoder: {error}"),
        };
        child.stdin.take().unwrap().write_all(&annex_b).unwrap();
        let output = child.wait_with_output().unwrap();
        assert!(
            output.status.success(),
            "FFmpeg rejected encoded H.264 sequence: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let expected_bytes = expected
            .iter()
            .flat_map(|frame| {
                frame.planes.iter().flat_map(|plane| {
                    (0..plane.height).flat_map(move |y| {
                        plane.data[y * plane.stride..y * plane.stride + plane.width]
                            .iter()
                            .copied()
                    })
                })
            })
            .collect::<Vec<_>>();
        assert_eq!(output.stdout, expected_bytes);
    }
}
