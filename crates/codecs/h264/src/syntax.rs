use std::collections::{BTreeMap, VecDeque};

use mmrecode_bitstream::BitReader;
use mmrecode_core::{Error, Result};

use crate::{NalUnit, NalUnitType, remove_emulation_prevention};

/// H.264 picture-order-count syntax selected by the SPS.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PictureOrderCountType {
    /// POC LSB and MSB syntax.
    Type0 {
        /// Width of the POC LSB syntax element.
        log2_max_pic_order_cnt_lsb: u8,
    },
    /// Delta-based POC cycle syntax.
    Type1 {
        /// Whether slice headers omit both delta POC syntax elements.
        delta_pic_order_always_zero: bool,
    },
    /// POC derived from frame number.
    Type2,
}

/// Sample aspect ratio signalled by VUI.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AspectRatio {
    /// Horizontal sample spacing.
    pub width: u16,
    /// Vertical sample spacing.
    pub height: u16,
}

/// VUI fields required for editing and presentation.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct VuiParameters {
    /// Sample aspect ratio, when signalled and recognized.
    pub aspect_ratio: Option<AspectRatio>,
    /// Whether the video uses full-range sample values.
    pub video_full_range: Option<bool>,
    /// ISO/IEC 23091-2 colour primaries code.
    pub colour_primaries: Option<u8>,
    /// ISO/IEC 23091-2 transfer-characteristics code.
    pub transfer_characteristics: Option<u8>,
    /// ISO/IEC 23091-2 matrix-coefficients code.
    pub matrix_coefficients: Option<u8>,
    /// VUI timing numerator in clock ticks.
    pub num_units_in_tick: Option<u32>,
    /// VUI clock frequency.
    pub time_scale: Option<u32>,
    /// Whether each coded picture uses a fixed frame-rate cadence.
    pub fixed_frame_rate: Option<bool>,
}

/// Parsed H.264 sequence parameter set.
#[derive(Clone, Debug, Eq, PartialEq)]
#[allow(clippy::struct_excessive_bools)]
pub struct Sps {
    /// `seq_parameter_set_id`.
    pub id: u32,
    /// `profile_idc`.
    pub profile_idc: u8,
    /// Constraint-set and reserved flag byte.
    pub constraint_flags: u8,
    /// `level_idc`.
    pub level_idc: u8,
    /// Chroma array format value.
    pub chroma_format_idc: u32,
    /// Whether 4:4:4 colour planes are coded separately.
    pub separate_colour_plane: bool,
    /// Luma bit depth.
    pub bit_depth_luma: u8,
    /// Chroma bit depth.
    pub bit_depth_chroma: u8,
    /// Whether QP-zero macroblocks bypass the inverse transform and scaling processes.
    pub qpprime_y_zero_transform_bypass: bool,
    /// Whether the SPS carries sequence scaling-list syntax.
    pub scaling_matrix_present: bool,
    /// Frame-number field width.
    pub log2_max_frame_num: u8,
    /// Picture-order-count mode.
    pub pic_order_cnt_type: PictureOrderCountType,
    /// Maximum decoded reference-frame count.
    pub max_num_ref_frames: u32,
    /// Whether coded pictures may be split into fields.
    pub frame_mbs_only: bool,
    /// Whether macroblock-adaptive frame/field coding is allowed.
    pub mb_adaptive_frame_field: bool,
    /// Whether direct motion vectors use 8x8 inference.
    pub direct_8x8_inference: bool,
    /// Display width after frame cropping.
    pub width: u32,
    /// Display height after frame cropping.
    pub height: u32,
    /// Width of the complete coded macroblock canvas before cropping.
    pub coded_width: u32,
    /// Height of the complete coded macroblock canvas before cropping.
    pub coded_height: u32,
    /// Left crop offset in luma samples.
    pub crop_left: u32,
    /// Top crop offset in luma samples.
    pub crop_top: u32,
    /// Optional presentation metadata.
    pub vui: Option<VuiParameters>,
}

/// Parsed H.264 picture parameter set fields required for slice interpretation.
#[derive(Clone, Debug, Eq, PartialEq)]
#[allow(clippy::struct_excessive_bools)]
pub struct Pps {
    /// `pic_parameter_set_id`.
    pub id: u32,
    /// Referenced sequence parameter set.
    pub sequence_parameter_set_id: u32,
    /// Whether CABAC is used for slice data.
    pub entropy_coding_mode: bool,
    /// Whether bottom-field POC is present in frame slice headers.
    pub bottom_field_pic_order_in_frame_present: bool,
    /// Number of slice groups minus one.
    pub num_slice_groups_minus1: u32,
    /// Default number of active list-0 references minus one.
    pub num_ref_idx_l0_default_active_minus1: u32,
    /// Whether P/SP slices carry weighted-prediction syntax.
    pub weighted_pred: bool,
    /// Initial luma quantization offset from 26.
    pub pic_init_qp_minus26: i32,
    /// Chroma quantization offset for the Cb component.
    pub chroma_qp_index_offset: i32,
    /// Whether redundant picture count is present.
    pub redundant_pic_cnt_present: bool,
    /// Whether slice headers carry deblocking-filter control syntax.
    pub deblocking_filter_control_present: bool,
    /// Whether macroblocks may select 8x8 luma transforms.
    pub transform_8x8_mode: bool,
    /// Whether the PPS carries picture scaling-list syntax.
    pub scaling_matrix_present: bool,
}

/// Broad picture role derived from `slice_type`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PictureType {
    /// Predictive slice.
    P,
    /// Bi-predictive slice.
    B,
    /// Intra slice.
    I,
    /// Switching P slice.
    Sp,
    /// Switching I slice.
    Si,
}

/// Parsed leading portion of an H.264 slice header.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SliceHeader {
    /// First macroblock address.
    pub first_mb_in_slice: u32,
    /// Normalized slice type.
    pub picture_type: PictureType,
    /// Referenced PPS identifier.
    pub picture_parameter_set_id: u32,
    /// Wrapping frame number.
    pub frame_num: u32,
    /// True for a field picture.
    pub field_pic: bool,
    /// True for a bottom field.
    pub bottom_field: bool,
    /// IDR picture identifier when this is an IDR NAL.
    pub idr_pic_id: Option<u32>,
    /// POC LSB for POC type 0.
    pub pic_order_cnt_lsb: Option<u32>,
    /// Delta bottom POC for POC type 0.
    pub delta_pic_order_cnt_bottom: Option<i32>,
    /// First delta POC for POC type 1.
    pub delta_pic_order_cnt0: Option<i32>,
    /// Second delta POC for POC type 1.
    pub delta_pic_order_cnt1: Option<i32>,
    /// Redundant picture count, when signalled.
    pub redundant_pic_cnt: Option<u32>,
}

struct SyntaxReader<'a> {
    bits: BitReader<'a>,
}

impl<'a> SyntaxReader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self {
            bits: BitReader::new(data),
        }
    }

    fn bit(&mut self) -> Result<bool> {
        self.bits.read_bit()
    }

    fn bits(&mut self, count: u8) -> Result<u64> {
        self.bits.read_bits(count)
    }

    fn ue(&mut self) -> Result<u32> {
        let mut leading_zero_bits = 0_u8;
        while !self.bit()? {
            leading_zero_bits = leading_zero_bits
                .checked_add(1)
                .ok_or_else(|| Error::InvalidData("Exp-Golomb prefix overflows".into()))?;
            if leading_zero_bits > 31 {
                return Err(Error::InvalidData("Exp-Golomb value exceeds u32".into()));
            }
        }
        if leading_zero_bits == 0 {
            return Ok(0);
        }
        let suffix = u32::try_from(self.bits(leading_zero_bits)?)
            .map_err(|_| Error::InvalidData("Exp-Golomb suffix overflows".into()))?;
        Ok(((1_u32 << leading_zero_bits) - 1) + suffix)
    }

    fn se(&mut self) -> Result<i32> {
        let code_num = self.ue()?;
        let magnitude = i32::try_from(code_num.div_ceil(2))
            .map_err(|_| Error::InvalidData("signed Exp-Golomb value overflows".into()))?;
        Ok(if code_num % 2 == 0 {
            -magnitude
        } else {
            magnitude
        })
    }
}

/// Parses an SPS NAL unit, including its one-byte NAL header.
///
/// # Errors
///
/// Returns an error for malformed, truncated, inconsistent, or unsupported SPS syntax.
#[allow(clippy::too_many_lines)]
pub fn parse_sps(nal: &[u8]) -> Result<Sps> {
    require_type(nal, NalUnitType::Sps)?;
    let rbsp = remove_emulation_prevention(&nal[1..]);
    let mut reader = SyntaxReader::new(&rbsp);
    let profile_idc = read_u8(&mut reader, 8, "profile_idc")?;
    let constraint_flags = read_u8(&mut reader, 8, "constraint flags")?;
    let level_idc = read_u8(&mut reader, 8, "level_idc")?;
    let id = reader.ue()?;

    let high_profile = matches!(
        profile_idc,
        44 | 83 | 86 | 100 | 110 | 118 | 122 | 128 | 134 | 135 | 138 | 139 | 144 | 244
    );
    let mut chroma_format_idc = 1;
    let mut separate_colour_plane = false;
    let mut bit_depth_luma = 8;
    let mut bit_depth_chroma = 8;
    let mut qpprime_y_zero_transform_bypass = false;
    let mut scaling_matrix_present = false;
    if high_profile {
        chroma_format_idc = reader.ue()?;
        if chroma_format_idc > 3 {
            return Err(Error::InvalidData("invalid H.264 chroma_format_idc".into()));
        }
        if chroma_format_idc == 3 {
            separate_colour_plane = reader.bit()?;
        }
        bit_depth_luma = u8::try_from(8_u32 + reader.ue()?)
            .map_err(|_| Error::InvalidData("H.264 luma bit depth overflows".into()))?;
        bit_depth_chroma = u8::try_from(8_u32 + reader.ue()?)
            .map_err(|_| Error::InvalidData("H.264 chroma bit depth overflows".into()))?;
        qpprime_y_zero_transform_bypass = reader.bit()?;
        scaling_matrix_present = reader.bit()?;
        if scaling_matrix_present {
            let list_count = if chroma_format_idc == 3 { 12 } else { 8 };
            for index in 0..list_count {
                if reader.bit()? {
                    skip_scaling_list(&mut reader, if index < 6 { 16 } else { 64 })?;
                }
            }
        }
    }

    let log2_max_frame_num_minus4 = reader.ue()?;
    let log2_max_frame_num = u8::try_from(log2_max_frame_num_minus4 + 4)
        .map_err(|_| Error::InvalidData("H.264 frame_num width overflows".into()))?;
    if log2_max_frame_num > 16 {
        return Err(Error::InvalidData("invalid H.264 frame_num width".into()));
    }
    let pic_order_cnt_type_value = reader.ue()?;
    let pic_order_cnt_type = match pic_order_cnt_type_value {
        0 => {
            let width = u8::try_from(reader.ue()? + 4)
                .map_err(|_| Error::InvalidData("H.264 POC width overflows".into()))?;
            if width > 16 {
                return Err(Error::InvalidData("invalid H.264 POC LSB width".into()));
            }
            PictureOrderCountType::Type0 {
                log2_max_pic_order_cnt_lsb: width,
            }
        }
        1 => {
            let delta_pic_order_always_zero = reader.bit()?;
            let _offset_for_non_ref_pic = reader.se()?;
            let _offset_for_top_to_bottom_field = reader.se()?;
            let cycle = reader.ue()?;
            if cycle > 255 {
                return Err(Error::Unsupported(
                    "H.264 POC cycle longer than 255 entries".into(),
                ));
            }
            for _ in 0..cycle {
                let _offset_for_ref_frame = reader.se()?;
            }
            PictureOrderCountType::Type1 {
                delta_pic_order_always_zero,
            }
        }
        2 => PictureOrderCountType::Type2,
        _ => {
            return Err(Error::InvalidData(
                "invalid H.264 pic_order_cnt_type".into(),
            ));
        }
    };
    let max_num_ref_frames = reader.ue()?;
    let _gaps_in_frame_num_value_allowed = reader.bit()?;
    let pic_width_in_mbs_minus1 = reader.ue()?;
    let pic_height_in_map_units_minus1 = reader.ue()?;
    let frame_mbs_only = reader.bit()?;
    let mb_adaptive_frame_field = !frame_mbs_only && reader.bit()?;
    let direct_8x8_inference = reader.bit()?;
    let cropping = if reader.bit()? {
        Some((reader.ue()?, reader.ue()?, reader.ue()?, reader.ue()?))
    } else {
        None
    };
    let vui = reader.bit()?.then(|| parse_vui(&mut reader)).transpose()?;

    let coded_width = pic_width_in_mbs_minus1
        .checked_add(1)
        .and_then(|value| value.checked_mul(16))
        .ok_or_else(|| Error::InvalidData("H.264 coded width overflows".into()))?;
    let coded_height = pic_height_in_map_units_minus1
        .checked_add(1)
        .and_then(|value| value.checked_mul(if frame_mbs_only { 16 } else { 32 }))
        .ok_or_else(|| Error::InvalidData("H.264 coded height overflows".into()))?;
    let chroma_array_type = if separate_colour_plane {
        0
    } else {
        chroma_format_idc
    };
    let (crop_unit_x, crop_unit_y) = crop_units(chroma_array_type, frame_mbs_only);
    let (crop_left, crop_right, crop_top, crop_bottom) = cropping.unwrap_or((0, 0, 0, 0));
    let crop_left = crop_left * crop_unit_x;
    let crop_top = crop_top * crop_unit_y;
    let crop_x = crop_left + crop_right * crop_unit_x;
    let crop_y = crop_top + crop_bottom * crop_unit_y;
    let width = coded_width
        .checked_sub(crop_x)
        .filter(|value| *value > 0)
        .ok_or_else(|| Error::InvalidData("H.264 frame crop removes coded width".into()))?;
    let height = coded_height
        .checked_sub(crop_y)
        .filter(|value| *value > 0)
        .ok_or_else(|| Error::InvalidData("H.264 frame crop removes coded height".into()))?;

    Ok(Sps {
        id,
        profile_idc,
        constraint_flags,
        level_idc,
        chroma_format_idc,
        separate_colour_plane,
        bit_depth_luma,
        bit_depth_chroma,
        qpprime_y_zero_transform_bypass,
        scaling_matrix_present,
        log2_max_frame_num,
        pic_order_cnt_type,
        max_num_ref_frames,
        frame_mbs_only,
        mb_adaptive_frame_field,
        direct_8x8_inference,
        width,
        height,
        coded_width,
        coded_height,
        crop_left,
        crop_top,
        vui,
    })
}

fn crop_units(chroma_array_type: u32, frame_mbs_only: bool) -> (u32, u32) {
    let frame_factor = if frame_mbs_only { 1 } else { 2 };
    match chroma_array_type {
        1 => (2, 2 * frame_factor),
        2 => (2, frame_factor),
        _ => (1, frame_factor),
    }
}

fn skip_scaling_list(reader: &mut SyntaxReader<'_>, size: usize) -> Result<()> {
    let mut last_scale = 8_i32;
    let mut next_scale = 8_i32;
    for _ in 0..size {
        if next_scale != 0 {
            let delta_scale = reader.se()?;
            next_scale = (last_scale + delta_scale + 256) % 256;
        }
        if next_scale != 0 {
            last_scale = next_scale;
        }
    }
    Ok(())
}

fn parse_vui(reader: &mut SyntaxReader<'_>) -> Result<VuiParameters> {
    let mut vui = VuiParameters::default();
    if reader.bit()? {
        let aspect_ratio_idc = read_u8(reader, 8, "aspect_ratio_idc")?;
        vui.aspect_ratio = if aspect_ratio_idc == 255 {
            let width = read_u16(reader, 16, "sar_width")?;
            let height = read_u16(reader, 16, "sar_height")?;
            (width > 0 && height > 0).then_some(AspectRatio { width, height })
        } else {
            standard_aspect_ratio(aspect_ratio_idc)
        };
    }
    if reader.bit()? {
        let _overscan_appropriate = reader.bit()?;
    }
    if reader.bit()? {
        let _video_format = reader.bits(3)?;
        vui.video_full_range = Some(reader.bit()?);
        if reader.bit()? {
            vui.colour_primaries = Some(read_u8(reader, 8, "colour_primaries")?);
            vui.transfer_characteristics = Some(read_u8(reader, 8, "transfer_characteristics")?);
            vui.matrix_coefficients = Some(read_u8(reader, 8, "matrix_coefficients")?);
        }
    }
    if reader.bit()? {
        let _chroma_sample_loc_type_top_field = reader.ue()?;
        let _chroma_sample_loc_type_bottom_field = reader.ue()?;
    }
    if reader.bit()? {
        vui.num_units_in_tick = Some(read_u32(reader, 32, "num_units_in_tick")?);
        vui.time_scale = Some(read_u32(reader, 32, "time_scale")?);
        vui.fixed_frame_rate = Some(reader.bit()?);
    }
    Ok(vui)
}

fn standard_aspect_ratio(idc: u8) -> Option<AspectRatio> {
    let (width, height) = match idc {
        1 => (1, 1),
        2 => (12, 11),
        3 => (10, 11),
        4 => (16, 11),
        5 => (40, 33),
        6 => (24, 11),
        7 => (20, 11),
        8 => (32, 11),
        9 => (80, 33),
        10 => (18, 11),
        11 => (15, 11),
        12 => (64, 33),
        13 => (160, 99),
        14 => (4, 3),
        15 => (3, 2),
        16 => (2, 1),
        _ => return None,
    };
    Some(AspectRatio { width, height })
}

/// Parses a PPS NAL unit, including its one-byte NAL header.
///
/// # Errors
///
/// Returns an error for malformed, truncated, or unsupported PPS syntax.
pub fn parse_pps(nal: &[u8]) -> Result<Pps> {
    require_type(nal, NalUnitType::Pps)?;
    let rbsp = remove_emulation_prevention(&nal[1..]);
    let mut reader = SyntaxReader::new(&rbsp);
    let id = reader.ue()?;
    let sequence_parameter_set_id = reader.ue()?;
    let entropy_coding_mode = reader.bit()?;
    let bottom_field_pic_order_in_frame_present = reader.bit()?;
    let num_slice_groups_minus1 = reader.ue()?;
    if num_slice_groups_minus1 > 0 {
        skip_slice_groups(&mut reader, num_slice_groups_minus1)?;
    }
    let num_ref_idx_l0_default_active_minus1 = reader.ue()?;
    let _num_ref_idx_l1_default_active_minus1 = reader.ue()?;
    let weighted_pred = reader.bit()?;
    let _weighted_bipred_idc = reader.bits(2)?;
    let pic_init_qp_minus26 = reader.se()?;
    let _pic_init_qs_minus26 = reader.se()?;
    let chroma_qp_index_offset = reader.se()?;
    let deblocking_filter_control_present = reader.bit()?;
    let _constrained_intra_pred = reader.bit()?;
    let redundant_pic_cnt_present = reader.bit()?;
    let (transform_8x8_mode, scaling_matrix_present) = if more_rbsp_data(&reader)? {
        (reader.bit()?, reader.bit()?)
    } else {
        (false, false)
    };
    Ok(Pps {
        id,
        sequence_parameter_set_id,
        entropy_coding_mode,
        bottom_field_pic_order_in_frame_present,
        num_slice_groups_minus1,
        num_ref_idx_l0_default_active_minus1,
        weighted_pred,
        pic_init_qp_minus26,
        chroma_qp_index_offset,
        redundant_pic_cnt_present,
        deblocking_filter_control_present,
        transform_8x8_mode,
        scaling_matrix_present,
    })
}

fn more_rbsp_data(reader: &SyntaxReader<'_>) -> Result<bool> {
    let remaining = reader.bits.bits_remaining();
    if remaining == 0 {
        return Ok(false);
    }
    if remaining > 8 {
        return Ok(true);
    }
    let remaining = u8::try_from(remaining).expect("at most eight trailing bits");
    let value = reader.bits.peek_bits(remaining)?;
    Ok(value != 1_u64 << (remaining - 1))
}

fn skip_slice_groups(reader: &mut SyntaxReader<'_>, count_minus1: u32) -> Result<()> {
    match reader.ue()? {
        0 => {
            for _ in 0..=count_minus1 {
                let _run_length_minus1 = reader.ue()?;
            }
        }
        2 => {
            for _ in 0..count_minus1 {
                let _top_left = reader.ue()?;
                let _bottom_right = reader.ue()?;
            }
        }
        1 => {}
        3..=5 => {
            let _slice_group_change_direction = reader.bit()?;
            let _slice_group_change_rate_minus1 = reader.ue()?;
        }
        6 => {
            let pic_size_in_map_units_minus1 = reader.ue()?;
            let groups = count_minus1 + 1;
            let bits = u8::try_from(32 - (groups - 1).leading_zeros())
                .map_err(|_| Error::InvalidData("slice-group selector width overflows".into()))?;
            for _ in 0..=pic_size_in_map_units_minus1 {
                let _slice_group_id = reader.bits(bits)?;
            }
        }
        _ => {
            return Err(Error::InvalidData(
                "invalid H.264 slice_group_map_type".into(),
            ));
        }
    }
    Ok(())
}

/// Parses the leading slice header using active SPS/PPS state.
///
/// # Errors
///
/// Returns an error when the NAL is not a supported coded slice, syntax is malformed, or the
/// supplied parameter sets do not match its identifiers.
pub fn parse_slice_header(nal: &[u8], sps: &Sps, pps: &Pps) -> Result<SliceHeader> {
    let header = nal
        .first()
        .copied()
        .ok_or_else(|| Error::InvalidData("empty H.264 slice NAL".into()))?;
    let unit_type = crate::NalUnitHeader::parse(header)?.unit_type;
    if !matches!(unit_type, NalUnitType::CodedSlice | NalUnitType::IdrSlice) {
        return Err(Error::InvalidData(
            "NAL unit is not an H.264 coded slice".into(),
        ));
    }
    if pps.sequence_parameter_set_id != sps.id {
        return Err(Error::InvalidData(
            "PPS does not reference supplied SPS".into(),
        ));
    }
    let rbsp = remove_emulation_prevention(&nal[1..]);
    let mut reader = SyntaxReader::new(&rbsp);
    let first_mb_in_slice = reader.ue()?;
    let raw_slice_type = reader.ue()?;
    let picture_type = match raw_slice_type % 5 {
        0 => PictureType::P,
        1 => PictureType::B,
        2 => PictureType::I,
        3 => PictureType::Sp,
        _ => PictureType::Si,
    };
    let picture_parameter_set_id = reader.ue()?;
    if picture_parameter_set_id != pps.id {
        return Err(Error::InvalidData(format!(
            "slice references PPS {picture_parameter_set_id}, not supplied PPS {}",
            pps.id
        )));
    }
    if sps.separate_colour_plane {
        let _colour_plane_id = reader.bits(2)?;
    }
    let frame_num = u32::try_from(reader.bits(sps.log2_max_frame_num)?)
        .map_err(|_| Error::InvalidData("H.264 frame_num overflows".into()))?;
    let field_pic = !sps.frame_mbs_only && reader.bit()?;
    let bottom_field = field_pic && reader.bit()?;
    let idr_pic_id = matches!(unit_type, NalUnitType::IdrSlice)
        .then(|| reader.ue())
        .transpose()?;
    let mut pic_order_cnt_lsb = None;
    let mut delta_pic_order_cnt_bottom = None;
    let mut delta_pic_order_cnt0 = None;
    let mut delta_pic_order_cnt1 = None;
    match sps.pic_order_cnt_type {
        PictureOrderCountType::Type0 {
            log2_max_pic_order_cnt_lsb,
        } => {
            pic_order_cnt_lsb = Some(
                u32::try_from(reader.bits(log2_max_pic_order_cnt_lsb)?)
                    .map_err(|_| Error::InvalidData("H.264 POC LSB overflows".into()))?,
            );
            if pps.bottom_field_pic_order_in_frame_present && !field_pic {
                delta_pic_order_cnt_bottom = Some(reader.se()?);
            }
        }
        PictureOrderCountType::Type1 {
            delta_pic_order_always_zero,
        } => {
            if !delta_pic_order_always_zero {
                delta_pic_order_cnt0 = Some(reader.se()?);
                if pps.bottom_field_pic_order_in_frame_present && !field_pic {
                    delta_pic_order_cnt1 = Some(reader.se()?);
                }
            }
        }
        PictureOrderCountType::Type2 => {}
    }
    let redundant_pic_cnt = pps
        .redundant_pic_cnt_present
        .then(|| reader.ue())
        .transpose()?;
    Ok(SliceHeader {
        first_mb_in_slice,
        picture_type,
        picture_parameter_set_id,
        frame_num,
        field_pic,
        bottom_field,
        idr_pic_id,
        pic_order_cnt_lsb,
        delta_pic_order_cnt_bottom,
        delta_pic_order_cnt0,
        delta_pic_order_cnt1,
        redundant_pic_cnt,
    })
}

fn require_type(nal: &[u8], expected: NalUnitType) -> Result<()> {
    let header = nal
        .first()
        .copied()
        .ok_or_else(|| Error::InvalidData("empty H.264 NAL unit".into()))?;
    let actual = crate::NalUnitHeader::parse(header)?.unit_type;
    if actual != expected {
        return Err(Error::InvalidData(format!(
            "expected H.264 NAL type {}, found {}",
            expected.value(),
            actual.value()
        )));
    }
    Ok(())
}

fn read_u8(reader: &mut SyntaxReader<'_>, count: u8, name: &str) -> Result<u8> {
    u8::try_from(reader.bits(count)?)
        .map_err(|_| Error::InvalidData(format!("H.264 {name} overflows")))
}

fn read_u16(reader: &mut SyntaxReader<'_>, count: u8, name: &str) -> Result<u16> {
    u16::try_from(reader.bits(count)?)
        .map_err(|_| Error::InvalidData(format!("H.264 {name} overflows")))
}

fn read_u32(reader: &mut SyntaxReader<'_>, count: u8, name: &str) -> Result<u32> {
    u32::try_from(reader.bits(count)?)
        .map_err(|_| Error::InvalidData(format!("H.264 {name} overflows")))
}

/// Container-provided timing for one coded sample.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PictureTiming {
    /// Decode timestamp in container time-base units.
    pub dts: i64,
    /// Presentation timestamp in container time-base units.
    pub pts: i64,
    /// Sample duration in container time-base units.
    pub duration: u32,
}

/// One indexed coded picture.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PictureUnit {
    /// Zero-based container sample index.
    pub sample_index: usize,
    /// Container timing.
    pub timing: PictureTiming,
    /// Parsed primary slice header.
    pub slice: SliceHeader,
    /// Whether the access unit contains an IDR slice.
    pub is_idr: bool,
    /// Whether this picture participates as a decoding reference.
    pub is_reference: bool,
    /// Conservative decode dependencies expressed as earlier sample indexes.
    pub dependencies: Vec<usize>,
}

/// Syntax summary for one H.264 access unit.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct H264AccessUnit {
    /// Indexed picture, if this sample contains a primary coded picture.
    pub picture: Option<PictureUnit>,
    /// NAL types in sample order.
    pub nal_types: Vec<NalUnitType>,
}

/// Presentation/decode index plus active parameter sets.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct H264StreamIndex {
    /// Parsed sequence parameter sets by identifier.
    pub sequence_parameter_sets: BTreeMap<u32, Sps>,
    /// Parsed picture parameter sets by identifier.
    pub picture_parameter_sets: BTreeMap<u32, Pps>,
    /// Access units in decode/container order.
    pub access_units: Vec<H264AccessUnit>,
}

/// Stateful H.264 parameter-set and conservative dependency indexer.
#[derive(Clone, Debug, Default)]
pub struct H264StreamIndexer {
    index: H264StreamIndex,
    active_references: VecDeque<usize>,
}

impl H264StreamIndexer {
    /// Adds out-of-band parameter sets from an `avcC` record.
    ///
    /// # Errors
    ///
    /// Returns an error when an SPS or PPS is malformed or unsupported.
    pub fn configure_avcc(
        &mut self,
        configuration: &crate::AvcDecoderConfigurationRecord,
    ) -> Result<()> {
        for nal in &configuration.sequence_parameter_sets {
            let sps = parse_sps(nal)?;
            self.index.sequence_parameter_sets.insert(sps.id, sps);
        }
        for nal in &configuration.picture_parameter_sets {
            let pps = parse_pps(nal)?;
            self.index.picture_parameter_sets.insert(pps.id, pps);
        }
        Ok(())
    }

    /// Indexes one container-framed access unit.
    ///
    /// # Errors
    ///
    /// Returns an error for malformed NAL syntax or references to unavailable parameter sets.
    pub fn push_access_unit(
        &mut self,
        sample_index: usize,
        timing: PictureTiming,
        nal_units: &[NalUnit<'_>],
    ) -> Result<()> {
        let mut primary_slice = None;
        let mut is_idr = false;
        let mut is_reference = false;
        let mut nal_types = Vec::with_capacity(nal_units.len());
        for unit in nal_units {
            nal_types.push(unit.header.unit_type);
            match unit.header.unit_type {
                NalUnitType::Sps => {
                    let sps = parse_sps(unit.data)?;
                    self.index.sequence_parameter_sets.insert(sps.id, sps);
                }
                NalUnitType::Pps => {
                    let pps = parse_pps(unit.data)?;
                    self.index.picture_parameter_sets.insert(pps.id, pps);
                }
                NalUnitType::CodedSlice | NalUnitType::IdrSlice if primary_slice.is_none() => {
                    let pps_id = peek_slice_pps_id(unit.data)?;
                    let pps = self
                        .index
                        .picture_parameter_sets
                        .get(&pps_id)
                        .ok_or_else(|| {
                            Error::InvalidData(format!("slice references unknown PPS {pps_id}"))
                        })?;
                    let sps = self
                        .index
                        .sequence_parameter_sets
                        .get(&pps.sequence_parameter_set_id)
                        .ok_or_else(|| {
                            Error::InvalidData(format!(
                                "PPS {pps_id} references unknown SPS {}",
                                pps.sequence_parameter_set_id
                            ))
                        })?;
                    primary_slice = Some(parse_slice_header(unit.data, sps, pps)?);
                    is_idr = unit.header.unit_type == NalUnitType::IdrSlice;
                    is_reference = unit.header.reference_idc != 0;
                }
                _ => {}
            }
        }
        let picture = if let Some(slice) = primary_slice {
            if is_idr {
                self.active_references.clear();
            }
            let dependencies =
                if is_idr || matches!(slice.picture_type, PictureType::I | PictureType::Si) {
                    Vec::new()
                } else {
                    self.active_references.iter().copied().collect()
                };
            if is_reference {
                let max_references = self
                    .index
                    .picture_parameter_sets
                    .get(&slice.picture_parameter_set_id)
                    .and_then(|pps| {
                        self.index
                            .sequence_parameter_sets
                            .get(&pps.sequence_parameter_set_id)
                    })
                    .map_or(16, |sps| {
                        usize::try_from(sps.max_num_ref_frames).unwrap_or(16)
                    });
                self.active_references.push_back(sample_index);
                while self.active_references.len() > max_references {
                    self.active_references.pop_front();
                }
            }
            Some(PictureUnit {
                sample_index,
                timing,
                slice,
                is_idr,
                is_reference,
                dependencies,
            })
        } else {
            None
        };
        self.index
            .access_units
            .push(H264AccessUnit { picture, nal_types });
        Ok(())
    }

    /// Returns the accumulated index.
    #[must_use]
    pub const fn index(&self) -> &H264StreamIndex {
        &self.index
    }

    /// Consumes the indexer and returns the completed index.
    #[must_use]
    pub fn finish(self) -> H264StreamIndex {
        self.index
    }
}

fn peek_slice_pps_id(nal: &[u8]) -> Result<u32> {
    let rbsp = remove_emulation_prevention(
        nal.get(1..)
            .ok_or_else(|| Error::InvalidData("empty H.264 slice NAL".into()))?,
    );
    let mut reader = SyntaxReader::new(&rbsp);
    let _first_mb_in_slice = reader.ue()?;
    let _slice_type = reader.ue()?;
    reader.ue()
}

#[cfg(test)]
mod tests {
    use super::{PictureOrderCountType, PictureType, parse_pps, parse_slice_header, parse_sps};

    // Baseline 1280x720 SPS/PPS emitted by a conventional x264 stream.
    const SPS: &[u8] = &[
        0x67, 0x42, 0xc0, 0x1f, 0xda, 0x01, 0x40, 0x16, 0xec, 0x04, 0x40, 0x00, 0x00, 0x03, 0x00,
        0x40, 0x00, 0x00, 0x0c, 0x83, 0xc6, 0x0c, 0x65, 0x80,
    ];
    const PPS: &[u8] = &[0x68, 0xce, 0x3c, 0x80];

    #[test]
    fn parses_baseline_parameter_sets() {
        let sps = parse_sps(SPS).unwrap();
        assert_eq!(sps.profile_idc, 66);
        assert!(!sps.scaling_matrix_present);
        assert_eq!((sps.width, sps.height), (1280, 720));
        assert!(matches!(
            sps.pic_order_cnt_type,
            PictureOrderCountType::Type0 { .. }
                | PictureOrderCountType::Type1 { .. }
                | PictureOrderCountType::Type2
        ));
        let pps = parse_pps(PPS).unwrap();
        assert_eq!(pps.sequence_parameter_set_id, sps.id);
        assert!(!pps.transform_8x8_mode);
        assert!(!pps.scaling_matrix_present);
    }

    #[test]
    fn parses_minimal_idr_slice_header() {
        // first_mb=0, I slice, pps=0, frame_num=0, idr_pic_id=0, poc_lsb=0.
        let nal = [0x65, 0xb8, 0x40];
        let sps = parse_sps(SPS).unwrap();
        let pps = parse_pps(PPS).unwrap();
        let slice = parse_slice_header(&nal, &sps, &pps).unwrap();
        assert_eq!(slice.picture_type, PictureType::I);
        assert_eq!(slice.frame_num, 0);
        assert_eq!(slice.idr_pic_id, Some(0));
    }
}
