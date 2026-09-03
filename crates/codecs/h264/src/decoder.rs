use std::collections::{BTreeMap, VecDeque};

use mmrecode_bitstream::BitReader;
use mmrecode_core::{
    CodecDescriptor, ColorDescription, ColorRange, Decoder, Error, FieldOrder, FrameTiming,
    MediaType, Packet, PixelFormat, Plane, Result, VideoFrame,
};

use crate::{
    AvcDecoderConfigurationRecord, NalUnit, NalUnitType, PictureOrderCountType, Pps,
    ScalingMatrices, Sps,
    cabac::{CabacDecoder, ContextState, initial_contexts, initial_i_macroblock_contexts},
    cavlc::decode_residual_block,
    deblock::{
        MotionInfo, Parameters as DeblockingParameters, Picture as DeblockingPicture,
        filter_picture,
    },
    length_prefixed_nal_units, parse_pps, parse_sps, remove_emulation_prevention,
};

type ChromaDcLevels = [Vec<i32>; 2];
type ChromaAcLevels = [[Vec<i32>; 4]; 2];

/// Native H.264 decoder under incremental construction.
///
/// The first normative reconstruction slices accept one frame-coded, 8-bit 4:2:0 slice per
/// picture. IDR macroblocks may use `I_PCM`, CAVLC `Intra_16x16`, or CAVLC `Intra_4x4`, including
/// luma/chroma residual transforms, all prediction modes, and in-loop deblocking. The CAVLC P-slice
/// path retains one list-0 reference and supports skip, 16x16, 16x8, 8x16, and sub-macroblock
/// partitions down to 4x4, quarter-sample luma/eighth-sample chroma motion compensation, inter
/// residuals, explicit weighted prediction, mixed intra macroblocks, and inter-picture deblocking.
/// Baseline and High Profile streams using CAVLC and 4x4 transforms share this path. CABAC context
/// arithmetic, `I_PCM`, Intra16, and Intra4 macroblocks with luma/chroma
/// DC and AC residuals are also native. CABAC P slices support skip, 16x16, 16x8, 8x16, and
/// 8x8 partitions down to 4x4, mixed Intra4/Intra16/PCM macroblocks, motion, residuals, and
/// filtering. SPS/PPS scaling lists feed native 4x4 and luma 8x8 inverse quantization. High Profile
/// QP-zero transform bypass is native for lossless Intra4 and inter residuals.
/// This establishes native parameter activation, slice
/// traversal, prediction, reference retention, macroblock placement, filtering, cropping, timing,
/// and the shared [`Decoder`] contract without substituting another codec library. Other conforming
/// H.264 tools return [`Error::Unsupported`] and can be routed to an optional fallback by an
/// application.
#[derive(Debug, Default)]
pub struct H264Decoder {
    configuration: Option<AvcDecoderConfigurationRecord>,
    sequence_parameter_sets: BTreeMap<u32, Sps>,
    picture_parameter_sets: BTreeMap<u32, Pps>,
    frames: VecDeque<VideoFrame>,
    reference: Option<ReferenceFrame>,
}

impl Decoder for H264Decoder {
    fn configure(&mut self, descriptor: &CodecDescriptor) -> Result<()> {
        if descriptor.codec_id.as_str() != crate::CODEC_NAME
            || descriptor.media_type != MediaType::Video
        {
            return Err(Error::InvalidData(
                "H.264 decoder requires a video/h264 video descriptor".into(),
            ));
        }
        let configuration = AvcDecoderConfigurationRecord::parse(&descriptor.configuration)?;
        let mut sequence_parameter_sets = BTreeMap::new();
        for bytes in &configuration.sequence_parameter_sets {
            let sps = parse_sps(bytes)?;
            sequence_parameter_sets.insert(sps.id, sps);
        }
        let mut picture_parameter_sets = BTreeMap::new();
        for bytes in &configuration.picture_parameter_sets {
            let pps = parse_pps(bytes)?;
            picture_parameter_sets.insert(pps.id, pps);
        }
        self.configuration = Some(configuration);
        self.sequence_parameter_sets = sequence_parameter_sets;
        self.picture_parameter_sets = picture_parameter_sets;
        self.frames.clear();
        self.reference = None;
        Ok(())
    }

    fn send_packet(&mut self, packet: Packet) -> Result<()> {
        let length_size = self
            .configuration
            .as_ref()
            .ok_or_else(|| {
                Error::InvalidState("H.264 decoder must be configured before input".into())
            })?
            .length_size;
        let units = length_prefixed_nal_units(&packet.data, length_size)?;
        for unit in &units {
            match unit.header.unit_type {
                NalUnitType::Sps => {
                    let sps = parse_sps(unit.data)?;
                    self.sequence_parameter_sets.insert(sps.id, sps);
                }
                NalUnitType::Pps => {
                    let pps = parse_pps(unit.data)?;
                    self.picture_parameter_sets.insert(pps.id, pps);
                }
                _ => {}
            }
        }
        let slices = units
            .iter()
            .filter(|unit| {
                matches!(
                    unit.header.unit_type,
                    NalUnitType::CodedSlice | NalUnitType::IdrSlice
                )
            })
            .collect::<Vec<_>>();
        if slices.is_empty() {
            return Ok(());
        }
        if slices.len() != 1 {
            return Err(Error::Unsupported(
                "native H.264 reconstruction currently requires one slice per picture".into(),
            ));
        }
        let timing = FrameTiming {
            pts: packet.pts,
            duration: packet.duration,
        };
        let frame = match slices[0].header.unit_type {
            NalUnitType::IdrSlice => self.decode_idr(slices[0], timing)?,
            NalUnitType::CodedSlice => self.decode_p_picture(slices[0], timing)?,
            _ => unreachable!("coded-slice filter accepts only slice NAL units"),
        };
        self.frames.push_back(frame);
        Ok(())
    }

    fn receive_frame(&mut self) -> Result<Option<VideoFrame>> {
        if self.configuration.is_none() {
            return Err(Error::InvalidState(
                "H.264 decoder must be configured before output".into(),
            ));
        }
        Ok(self.frames.pop_front())
    }

    fn flush(&mut self) -> Result<()> {
        if self.configuration.is_none() {
            return Err(Error::InvalidState(
                "H.264 decoder must be configured before flushing".into(),
            ));
        }
        Ok(())
    }
}

impl H264Decoder {
    fn decode_idr(&mut self, unit: &NalUnit<'_>, timing: FrameTiming) -> Result<VideoFrame> {
        require_reference_idr(unit)?;
        let rbsp = remove_emulation_prevention(
            unit.data
                .get(1..)
                .ok_or_else(|| Error::InvalidData("empty H.264 slice".into()))?,
        );
        let mut reader = SyntaxReader::new(&rbsp);
        let first_mb = reader.ue()?;
        let slice_type = reader.ue()?;
        if slice_type % 5 != 2 {
            return Err(Error::Unsupported(
                "native H.264 reconstruction currently supports I slices only".into(),
            ));
        }
        if first_mb != 0 {
            return Err(Error::Unsupported(
                "native H.264 reconstruction currently requires a full-picture slice".into(),
            ));
        }
        let pps_id = reader.ue()?;
        let pps = self.picture_parameter_sets.get(&pps_id).ok_or_else(|| {
            Error::InvalidData(format!("H.264 slice references unknown PPS {pps_id}"))
        })?;
        let sps = self
            .sequence_parameter_sets
            .get(&pps.sequence_parameter_set_id)
            .ok_or_else(|| {
                Error::InvalidData(format!(
                    "H.264 PPS {pps_id} references unknown SPS {}",
                    pps.sequence_parameter_set_id
                ))
            })?;
        validate_native_profile(sps, pps)?;
        if sps.separate_colour_plane {
            let _colour_plane_id = reader.bits(2)?;
        }
        let frame_num = u32::try_from(reader.bits(sps.log2_max_frame_num)?)
            .map_err(|_| Error::InvalidData("H.264 frame_num overflows".into()))?;
        let _idr_pic_id = reader.ue()?;
        match sps.pic_order_cnt_type {
            PictureOrderCountType::Type0 {
                log2_max_pic_order_cnt_lsb,
            } => {
                let _pic_order_cnt_lsb = reader.bits(log2_max_pic_order_cnt_lsb)?;
                if pps.bottom_field_pic_order_in_frame_present {
                    let _delta_pic_order_cnt_bottom = reader.se()?;
                }
            }
            PictureOrderCountType::Type1 {
                delta_pic_order_always_zero,
            } if !delta_pic_order_always_zero => {
                let _delta_pic_order_cnt0 = reader.se()?;
                if pps.bottom_field_pic_order_in_frame_present {
                    let _delta_pic_order_cnt1 = reader.se()?;
                }
            }
            PictureOrderCountType::Type1 { .. } | PictureOrderCountType::Type2 => {}
        }
        if pps.redundant_pic_cnt_present {
            let _redundant_pic_cnt = reader.ue()?;
        }
        let _no_output_of_prior_pics_flag = reader.bit()?;
        let _long_term_reference_flag = reader.bit()?;
        let slice_qp_delta = reader.se()?;
        let mut luma_qp = 26_i32
            .checked_add(pps.pic_init_qp_minus26)
            .and_then(|value| value.checked_add(slice_qp_delta))
            .filter(|value| (0..=51).contains(value))
            .ok_or_else(|| Error::InvalidData("invalid H.264 initial slice QP".into()))?;
        let deblocking = read_deblocking_parameters(&mut reader, pps)?;
        let mut buffer = FrameBuffer::new(sps, pps)?;
        if pps.entropy_coding_mode {
            decode_cabac_i_macroblocks(
                &mut reader.bits,
                &mut buffer,
                &mut luma_qp,
                pps.chroma_qp_index_offset,
                pps.transform_8x8_mode,
            )?;
        } else {
            decode_idr_macroblocks(&mut reader, &mut buffer, pps, &mut luma_qp)?;
            reader.finish_rbsp()?;
        }
        if let Some(params) = deblocking {
            buffer.deblock(
                [
                    pps.chroma_qp_index_offset,
                    pps.second_chroma_qp_index_offset,
                ],
                params,
            )?;
        }
        let frame = buffer.to_frame(sps, timing)?;
        self.reference = Some(buffer.into_reference(frame_num));
        Ok(frame)
    }

    #[allow(clippy::too_many_lines)]
    fn decode_p_picture(&mut self, unit: &NalUnit<'_>, timing: FrameTiming) -> Result<VideoFrame> {
        let rbsp = remove_emulation_prevention(
            unit.data
                .get(1..)
                .ok_or_else(|| Error::InvalidData("empty H.264 slice".into()))?,
        );
        let mut reader = SyntaxReader::new(&rbsp);
        let first_mb = reader.ue()?;
        let slice_type = reader.ue()?;
        if slice_type % 5 != 0 {
            return Err(Error::Unsupported(
                "native non-IDR H.264 reconstruction currently supports P slices only".into(),
            ));
        }
        if first_mb != 0 {
            return Err(Error::Unsupported(
                "native H.264 reconstruction currently requires a full-picture slice".into(),
            ));
        }
        let pps_id = reader.ue()?;
        let pps = self.picture_parameter_sets.get(&pps_id).ok_or_else(|| {
            Error::InvalidData(format!("H.264 slice references unknown PPS {pps_id}"))
        })?;
        let sps = self
            .sequence_parameter_sets
            .get(&pps.sequence_parameter_set_id)
            .ok_or_else(|| {
                Error::InvalidData(format!(
                    "H.264 PPS {pps_id} references unknown SPS {}",
                    pps.sequence_parameter_set_id
                ))
            })?;
        validate_native_profile(sps, pps)?;
        if sps.separate_colour_plane {
            let _colour_plane_id = reader.bits(2)?;
        }
        let frame_num = u32::try_from(reader.bits(sps.log2_max_frame_num)?)
            .map_err(|_| Error::InvalidData("H.264 frame_num overflows".into()))?;
        read_picture_order_count(&mut reader, sps, pps)?;
        if pps.redundant_pic_cnt_present {
            let _redundant_pic_cnt = reader.ue()?;
        }
        let active_references_minus1 = if reader.bit()? {
            reader.ue()?
        } else {
            pps.num_ref_idx_l0_default_active_minus1
        };
        if active_references_minus1 != 0 {
            return Err(Error::Unsupported(
                "native H.264 P-slice reconstruction currently supports one list-0 reference"
                    .into(),
            ));
        }
        if reader.bit()? {
            return Err(Error::Unsupported(
                "native H.264 reference-list modification is not implemented yet".into(),
            ));
        }
        let prediction_weights = if pps.weighted_pred {
            read_prediction_weights(&mut reader)?
        } else {
            PredictionWeights::identity()
        };
        if unit.header.reference_idc != 0 && reader.bit()? {
            return Err(Error::Unsupported(
                "native H.264 adaptive reference marking is not implemented yet".into(),
            ));
        }
        let cabac_init_idc = if pps.entropy_coding_mode {
            let value = reader.ue()?;
            if value > 2 {
                return Err(Error::InvalidData(format!(
                    "invalid H.264 cabac_init_idc {value}"
                )));
            }
            Some(value)
        } else {
            None
        };
        let slice_qp_delta = reader.se()?;
        let luma_qp = 26_i32
            .checked_add(pps.pic_init_qp_minus26)
            .and_then(|value| value.checked_add(slice_qp_delta))
            .filter(|value| (0..=51).contains(value))
            .ok_or_else(|| Error::InvalidData("invalid H.264 initial slice QP".into()))?;
        let deblocking = read_deblocking_parameters(&mut reader, pps)?;
        let reference = self.reference.as_ref().ok_or_else(|| {
            Error::InvalidData("H.264 P picture has no decoded reference picture".into())
        })?;
        let mut buffer = FrameBuffer::from_reference(sps, pps, reference, luma_qp)?;
        let mut current_qp = luma_qp;
        if let Some(cabac_init_idc) = cabac_init_idc {
            decode_cabac_p_macroblocks(
                &mut reader.bits,
                &mut buffer,
                reference,
                prediction_weights,
                &mut current_qp,
                cabac_init_idc,
                pps.chroma_qp_index_offset,
                pps.transform_8x8_mode,
            )?;
        } else {
            decode_p_macroblocks(
                &mut reader,
                &mut buffer,
                reference,
                pps,
                prediction_weights,
                &mut current_qp,
            )?;
            reader.finish_rbsp()?;
        }
        if let Some(params) = deblocking {
            buffer.deblock(
                [
                    pps.chroma_qp_index_offset,
                    pps.second_chroma_qp_index_offset,
                ],
                params,
            )?;
        }
        let frame = buffer.to_frame(sps, timing)?;
        if unit.header.reference_idc != 0 {
            self.reference = Some(buffer.into_reference(frame_num));
        }
        Ok(frame)
    }
}

fn decode_p_macroblocks(
    reader: &mut SyntaxReader<'_>,
    buffer: &mut FrameBuffer,
    reference: &ReferenceFrame,
    pps: &Pps,
    prediction_weights: PredictionWeights,
    luma_qp: &mut i32,
) -> Result<()> {
    let mut address = 0;
    while address < buffer.macroblock_count() {
        let skip_run = usize::try_from(reader.ue()?)
            .map_err(|_| Error::InvalidData("H.264 P-slice skip run overflows".into()))?;
        let skip_end = address
            .checked_add(skip_run)
            .filter(|end| *end <= buffer.macroblock_count())
            .ok_or_else(|| Error::InvalidData("H.264 P-slice skip run exceeds picture".into()))?;
        while address < skip_end {
            buffer.predict_p_skip(reference, address, prediction_weights, *luma_qp)?;
            address += 1;
        }
        if address == buffer.macroblock_count() {
            break;
        }
        let macroblock_type = reader.ue()?;
        match macroblock_type {
            0..=4 => decode_p_l0_macroblock(
                reader,
                buffer,
                reference,
                address,
                macroblock_type,
                luma_qp,
                pps.chroma_qp_index_offset,
                prediction_weights,
            )?,
            5 => decode_intra4x4(reader, buffer, address, luma_qp, pps.chroma_qp_index_offset)?,
            6..=29 => decode_intra16x16(
                reader,
                buffer,
                address,
                macroblock_type - 5,
                luma_qp,
                pps.chroma_qp_index_offset,
            )?,
            30 => {
                reader.align_zero_to_byte()?;
                buffer.read_macroblock(reader, address)?;
                buffer.mark_pcm(address);
            }
            _ => {
                return Err(Error::Unsupported(format!(
                    "native H.264 reconstruction does not support P-slice macroblock type {macroblock_type} at macroblock {address}"
                )));
            }
        }
        address += 1;
    }
    Ok(())
}

const CABAC_P_SKIP_INIT: [[(i8, i8); 3]; 3] = [
    [(23, 33), (23, 2), (21, 0)],
    [(22, 25), (34, 0), (16, 0)],
    [(29, 16), (25, 0), (14, 0)],
];
const CABAC_P_MACROBLOCK_TYPE_INIT: [[(i8, i8); 7]; 3] = [
    [
        (1, 9),
        (0, 49),
        (-37, 118),
        (5, 57),
        (-13, 78),
        (-11, 65),
        (1, 62),
    ],
    [
        (-2, 9),
        (4, 41),
        (-29, 118),
        (2, 65),
        (-6, 71),
        (-13, 79),
        (5, 52),
    ],
    [
        (-10, 51),
        (-3, 62),
        (-27, 99),
        (26, 16),
        (-4, 85),
        (-24, 102),
        (5, 57),
    ],
];
const CABAC_P_SUB_MACROBLOCK_TYPE_INIT: [[(i8, i8); 3]; 3] = [
    [(12, 49), (-4, 73), (17, 50)],
    [(9, 50), (-3, 70), (10, 54)],
    [(6, 57), (-17, 73), (14, 57)],
];
const CABAC_P_MVD_X_INIT: [[(i8, i8); 7]; 3] = [
    [
        (-3, 69),
        (-6, 81),
        (-11, 96),
        (6, 55),
        (7, 67),
        (-5, 86),
        (2, 88),
    ],
    [
        (-2, 69),
        (-5, 82),
        (-10, 96),
        (2, 59),
        (2, 75),
        (-3, 87),
        (-3, 100),
    ],
    [
        (-11, 89),
        (-15, 103),
        (-21, 116),
        (19, 57),
        (20, 58),
        (4, 84),
        (6, 96),
    ],
];
const CABAC_P_MVD_Y_INIT: [[(i8, i8); 7]; 3] = [
    [
        (0, 58),
        (-3, 76),
        (-10, 94),
        (5, 54),
        (4, 69),
        (-3, 81),
        (0, 88),
    ],
    [
        (1, 56),
        (-3, 74),
        (-6, 85),
        (0, 59),
        (-3, 81),
        (-7, 86),
        (-5, 95),
    ],
    [
        (1, 63),
        (-5, 85),
        (-13, 106),
        (5, 63),
        (6, 75),
        (-3, 90),
        (-1, 101),
    ],
];
const CABAC_P_CODED_BLOCK_PATTERN_LUMA_INIT: [[(i8, i8); 4]; 3] = [
    [(-27, 126), (-28, 98), (-25, 101), (-23, 67)],
    [(-39, 127), (-18, 91), (-17, 96), (-26, 81)],
    [(-36, 127), (-17, 91), (-14, 95), (-25, 84)],
];
const CABAC_P_CODED_BLOCK_PATTERN_CHROMA_INIT: [[(i8, i8); 8]; 3] = [
    [
        (-28, 82),
        (-20, 94),
        (-16, 83),
        (-22, 110),
        (-21, 91),
        (-18, 102),
        (-13, 93),
        (-29, 127),
    ],
    [
        (-35, 98),
        (-24, 102),
        (-23, 97),
        (-27, 119),
        (-24, 99),
        (-21, 110),
        (-18, 102),
        (-36, 127),
    ],
    [
        (-25, 86),
        (-12, 89),
        (-17, 91),
        (-31, 127),
        (-14, 76),
        (-18, 103),
        (-13, 90),
        (-37, 127),
    ],
];
const CABAC_P_LUMA_CBF_INIT: [[(i8, i8); 4]; 3] = [
    [(-3, 74), (-9, 92), (-8, 87), (-23, 126)],
    [(-2, 73), (-12, 104), (-9, 91), (-31, 127)],
    [(-5, 79), (-11, 104), (-11, 91), (-30, 127)],
];
const CABAC_P_CHROMA_DC_CBF_INIT: [[(i8, i8); 4]; 3] = [
    [(5, 54), (6, 60), (6, 59), (6, 69)],
    [(3, 55), (7, 56), (7, 55), (8, 61)],
    [(0, 65), (-2, 79), (0, 72), (-4, 92)],
];
const CABAC_P_CHROMA_AC_CBF_INIT: [[(i8, i8); 4]; 3] = [
    [(-1, 48), (0, 68), (-4, 69), (-8, 88)],
    [(-3, 53), (0, 68), (-7, 74), (-9, 88)],
    [(-6, 56), (3, 68), (-8, 71), (-13, 98)],
];
const CABAC_P_LUMA_DC_CBF_INIT: [[(i8, i8); 4]; 3] = [
    [(-7, 92), (-5, 89), (-7, 96), (-13, 108)],
    [(0, 80), (-5, 89), (-7, 94), (-4, 92)],
    [(11, 80), (5, 76), (2, 84), (5, 78)],
];
const CABAC_P_LUMA_AC_CBF_INIT: [[(i8, i8); 4]; 3] = [
    [(-3, 46), (-1, 65), (-1, 57), (-9, 93)],
    [(0, 39), (0, 65), (-15, 84), (-35, 127)],
    [(-6, 55), (4, 61), (-14, 83), (-37, 127)],
];
const CABAC_P_LUMA_DC_SIG_INIT: [[(i8, i8); 15]; 3] = [
    [
        (-2, 85),
        (-6, 78),
        (-1, 75),
        (-7, 77),
        (2, 54),
        (5, 50),
        (-3, 68),
        (1, 50),
        (6, 42),
        (-4, 81),
        (1, 63),
        (-4, 70),
        (0, 67),
        (2, 57),
        (-2, 76),
    ],
    [
        (-13, 103),
        (-13, 91),
        (-9, 89),
        (-14, 92),
        (-8, 76),
        (-12, 87),
        (-23, 110),
        (-24, 105),
        (-10, 78),
        (-20, 112),
        (-17, 99),
        (-78, 127),
        (-70, 127),
        (-50, 127),
        (-46, 127),
    ],
    [
        (-4, 86),
        (-12, 88),
        (-5, 82),
        (-3, 72),
        (-4, 67),
        (-8, 72),
        (-16, 89),
        (-9, 69),
        (-1, 59),
        (5, 66),
        (4, 57),
        (-4, 71),
        (-2, 71),
        (2, 58),
        (-1, 74),
    ],
];
const CABAC_P_LUMA_AC_SIG_INIT: [[(i8, i8); 14]; 3] = [
    [
        (11, 35),
        (4, 64),
        (1, 61),
        (11, 35),
        (18, 25),
        (12, 24),
        (13, 29),
        (13, 36),
        (-10, 93),
        (-7, 73),
        (-2, 73),
        (13, 46),
        (9, 49),
        (-7, 100),
    ],
    [
        (-4, 66),
        (-5, 78),
        (-4, 71),
        (-8, 72),
        (2, 59),
        (-1, 55),
        (-7, 70),
        (-6, 75),
        (-8, 89),
        (-34, 119),
        (-3, 75),
        (32, 20),
        (30, 22),
        (-44, 127),
    ],
    [
        (-4, 44),
        (-1, 69),
        (0, 62),
        (-7, 51),
        (-4, 47),
        (-6, 42),
        (-3, 41),
        (-6, 53),
        (8, 76),
        (-9, 78),
        (-11, 83),
        (9, 52),
        (0, 67),
        (-5, 90),
    ],
];
const CABAC_P_LUMA_DC_LAST_INIT: [[(i8, i8); 15]; 3] = [
    [
        (11, 28),
        (2, 40),
        (3, 44),
        (0, 49),
        (0, 46),
        (2, 44),
        (2, 51),
        (0, 47),
        (4, 39),
        (2, 62),
        (6, 46),
        (0, 54),
        (3, 54),
        (2, 58),
        (4, 63),
    ],
    [
        (4, 45),
        (10, 28),
        (10, 31),
        (33, -11),
        (52, -43),
        (18, 15),
        (28, 0),
        (35, -22),
        (38, -25),
        (34, 0),
        (39, -18),
        (32, -12),
        (102, -94),
        (0, 0),
        (56, -15),
    ],
    [
        (4, 39),
        (0, 42),
        (7, 34),
        (11, 29),
        (8, 31),
        (6, 37),
        (7, 42),
        (3, 40),
        (8, 33),
        (13, 43),
        (13, 36),
        (4, 47),
        (3, 55),
        (2, 58),
        (6, 60),
    ],
];
const CABAC_P_LUMA_AC_LAST_INIT: [[(i8, i8); 14]; 3] = [
    [
        (6, 51),
        (6, 57),
        (7, 53),
        (6, 52),
        (6, 55),
        (11, 45),
        (14, 36),
        (8, 53),
        (-1, 82),
        (7, 55),
        (-3, 78),
        (15, 46),
        (22, 31),
        (-1, 84),
    ],
    [
        (33, -4),
        (29, 10),
        (37, -5),
        (51, -29),
        (39, -9),
        (52, -34),
        (69, -58),
        (67, -63),
        (44, -5),
        (32, 7),
        (55, -29),
        (32, 1),
        (0, 0),
        (27, 36),
    ],
    [
        (8, 44),
        (11, 44),
        (14, 42),
        (7, 48),
        (4, 56),
        (4, 52),
        (13, 37),
        (9, 49),
        (19, 58),
        (10, 48),
        (12, 45),
        (0, 69),
        (20, 33),
        (8, 63),
    ],
];
const CABAC_P_LUMA_DC_ABS_INIT: [[(i8, i8); 10]; 3] = [
    [
        (-6, 76),
        (-2, 44),
        (0, 45),
        (0, 52),
        (-3, 64),
        (-2, 59),
        (-4, 70),
        (-4, 75),
        (-8, 82),
        (-17, 102),
    ],
    [
        (-23, 112),
        (-15, 71),
        (-7, 61),
        (0, 53),
        (-5, 66),
        (-11, 77),
        (-9, 80),
        (-9, 84),
        (-10, 87),
        (-34, 127),
    ],
    [
        (-24, 115),
        (-22, 82),
        (-9, 62),
        (0, 53),
        (0, 59),
        (-14, 85),
        (-13, 89),
        (-13, 94),
        (-11, 92),
        (-29, 127),
    ],
];
const CABAC_P_LUMA_AC_ABS_INIT: [[(i8, i8); 10]; 3] = [
    [
        (-9, 77),
        (3, 24),
        (0, 42),
        (0, 48),
        (0, 55),
        (-6, 59),
        (-7, 71),
        (-12, 83),
        (-11, 87),
        (-30, 119),
    ],
    [
        (-21, 101),
        (-3, 39),
        (-5, 53),
        (-7, 61),
        (-11, 75),
        (-15, 77),
        (-17, 91),
        (-25, 107),
        (-25, 111),
        (-28, 122),
    ],
    [
        (-21, 100),
        (-14, 57),
        (-12, 67),
        (-11, 71),
        (-10, 77),
        (-21, 85),
        (-16, 88),
        (-23, 104),
        (-15, 98),
        (-37, 127),
    ],
];
const CABAC_P_LUMA_SIG_INIT: [[(i8, i8); 15]; 3] = [
    [
        (9, 53),
        (2, 53),
        (5, 53),
        (-2, 61),
        (0, 56),
        (0, 56),
        (-13, 63),
        (-5, 60),
        (-1, 62),
        (4, 57),
        (-6, 69),
        (4, 57),
        (14, 39),
        (4, 51),
        (13, 68),
    ],
    [
        (0, 54),
        (-5, 61),
        (0, 58),
        (-1, 60),
        (-3, 61),
        (-8, 67),
        (-25, 84),
        (-14, 74),
        (-5, 65),
        (5, 52),
        (2, 57),
        (0, 61),
        (-9, 69),
        (-11, 70),
        (18, 55),
    ],
    [
        (1, 67),
        (-15, 72),
        (-5, 75),
        (-8, 80),
        (-21, 83),
        (-21, 64),
        (-13, 31),
        (-25, 64),
        (-29, 94),
        (9, 75),
        (17, 63),
        (-8, 74),
        (-5, 35),
        (-2, 27),
        (13, 91),
    ],
];
const CABAC_P_CHROMA_DC_SIG_INIT: [[(i8, i8); 4]; 3] = [
    [(3, 64), (1, 61), (9, 63), (7, 50)],
    [(-4, 71), (0, 58), (7, 61), (9, 41)],
    [(3, 65), (-7, 69), (8, 77), (-10, 66)],
];
const CABAC_P_CHROMA_AC_SIG_INIT: [[(i8, i8); 14]; 3] = [
    [
        (7, 50),
        (16, 39),
        (5, 44),
        (4, 52),
        (11, 48),
        (-5, 60),
        (-1, 59),
        (0, 59),
        (22, 33),
        (5, 44),
        (14, 43),
        (-1, 78),
        (0, 60),
        (9, 69),
    ],
    [
        (9, 41),
        (18, 25),
        (9, 32),
        (5, 43),
        (9, 47),
        (0, 44),
        (0, 51),
        (2, 46),
        (19, 38),
        (-4, 66),
        (15, 38),
        (12, 42),
        (9, 34),
        (0, 89),
    ],
    [
        (-10, 66),
        (3, 62),
        (-3, 68),
        (-20, 81),
        (0, 30),
        (1, 7),
        (-3, 23),
        (-21, 74),
        (16, 66),
        (-23, 124),
        (17, 37),
        (44, -18),
        (50, -34),
        (-22, 127),
    ],
];
const CABAC_P_LUMA_LAST_INIT: [[(i8, i8); 15]; 3] = [
    [
        (25, 7),
        (30, -7),
        (28, 3),
        (28, 4),
        (32, 0),
        (34, -1),
        (30, 6),
        (30, 6),
        (32, 9),
        (31, 19),
        (26, 27),
        (26, 30),
        (37, 20),
        (28, 34),
        (17, 70),
    ],
    [
        (33, -25),
        (34, -30),
        (36, -28),
        (38, -28),
        (38, -27),
        (34, -18),
        (35, -16),
        (34, -14),
        (32, -8),
        (37, -6),
        (35, 0),
        (30, 10),
        (28, 18),
        (26, 25),
        (29, 41),
    ],
    [
        (35, -18),
        (33, -25),
        (28, -3),
        (24, 10),
        (27, 0),
        (34, -14),
        (52, -44),
        (39, -24),
        (19, 17),
        (31, 25),
        (36, 29),
        (24, 33),
        (34, 15),
        (30, 20),
        (22, 73),
    ],
];
const CABAC_P_CHROMA_DC_LAST_INIT: [[(i8, i8); 4]; 3] = [
    [(1, 67), (5, 59), (9, 67), (16, 30)],
    [(0, 75), (2, 72), (8, 77), (14, 35)],
    [(20, 34), (19, 31), (27, 44), (19, 16)],
];
const CABAC_P_CHROMA_AC_LAST_INIT: [[(i8, i8); 14]; 3] = [
    [
        (16, 30),
        (18, 32),
        (18, 35),
        (22, 29),
        (24, 31),
        (23, 38),
        (18, 43),
        (20, 41),
        (11, 63),
        (9, 59),
        (9, 64),
        (-1, 94),
        (-2, 89),
        (-9, 108),
    ],
    [
        (14, 35),
        (18, 31),
        (17, 35),
        (21, 30),
        (17, 45),
        (20, 42),
        (18, 45),
        (27, 26),
        (16, 54),
        (7, 66),
        (16, 56),
        (11, 73),
        (10, 67),
        (-10, 116),
    ],
    [
        (19, 16),
        (15, 36),
        (15, 36),
        (21, 28),
        (25, 21),
        (30, 20),
        (31, 12),
        (27, 16),
        (24, 42),
        (0, 93),
        (14, 56),
        (15, 57),
        (26, 38),
        (-24, 127),
    ],
];
const CABAC_P_LUMA_ABS_INIT: [[(i8, i8); 10]; 3] = [
    [
        (1, 58),
        (-3, 29),
        (-1, 36),
        (1, 38),
        (2, 43),
        (-6, 55),
        (0, 58),
        (0, 64),
        (-3, 74),
        (-10, 90),
    ],
    [
        (-11, 76),
        (-10, 44),
        (-10, 52),
        (-10, 57),
        (-9, 58),
        (-16, 72),
        (-7, 69),
        (-4, 69),
        (-5, 74),
        (-9, 86),
    ],
    [
        (-10, 82),
        (-8, 48),
        (-8, 61),
        (-8, 66),
        (-7, 70),
        (-14, 75),
        (-10, 79),
        (-9, 83),
        (-12, 92),
        (-18, 108),
    ],
];
const CABAC_P_TRANSFORM_SIZE_8X8_INIT: [[(i8, i8); 3]; 3] = [
    [(12, 40), (11, 51), (14, 59)],
    [(25, 32), (21, 49), (21, 54)],
    [(21, 33), (19, 50), (17, 61)],
];
const CABAC_P_LUMA_8X8_SIG_INIT: [[(i8, i8); 15]; 3] = [
    [
        (-4, 79),
        (-7, 71),
        (-5, 69),
        (-9, 70),
        (-8, 66),
        (-10, 68),
        (-19, 73),
        (-12, 69),
        (-16, 70),
        (-15, 67),
        (-20, 62),
        (-19, 70),
        (-16, 66),
        (-22, 65),
        (-20, 63),
    ],
    [
        (-5, 85),
        (-6, 81),
        (-10, 77),
        (-7, 81),
        (-17, 80),
        (-18, 73),
        (-4, 74),
        (-10, 83),
        (-9, 71),
        (-9, 67),
        (-1, 61),
        (-8, 66),
        (-14, 66),
        (0, 59),
        (2, 59),
    ],
    [
        (-3, 78),
        (-8, 74),
        (-9, 72),
        (-10, 72),
        (-18, 75),
        (-12, 71),
        (-11, 63),
        (-5, 70),
        (-17, 75),
        (-14, 72),
        (-16, 67),
        (-8, 53),
        (-14, 59),
        (-9, 52),
        (-11, 68),
    ],
];
const CABAC_P_LUMA_8X8_LAST_INIT: [[(i8, i8); 9]; 3] = [
    [
        (9, -2),
        (26, -9),
        (33, -9),
        (39, -7),
        (41, -2),
        (45, 3),
        (49, 9),
        (45, 27),
        (36, 59),
    ],
    [
        (17, -10),
        (32, -13),
        (42, -9),
        (49, -5),
        (53, 0),
        (64, 3),
        (68, 10),
        (66, 27),
        (47, 57),
    ],
    [
        (9, -2),
        (30, -10),
        (31, -4),
        (33, -1),
        (33, 7),
        (31, 12),
        (37, 23),
        (31, 38),
        (20, 64),
    ],
];
const CABAC_P_LUMA_8X8_ABS_INIT: [[(i8, i8); 10]; 3] = [
    [
        (-6, 66),
        (-7, 35),
        (-7, 42),
        (-8, 45),
        (-5, 48),
        (-12, 56),
        (-6, 60),
        (-5, 62),
        (-8, 66),
        (-8, 76),
    ],
    [
        (-5, 71),
        (0, 24),
        (-1, 36),
        (-2, 42),
        (-2, 52),
        (-9, 57),
        (-6, 63),
        (-4, 65),
        (-4, 67),
        (-7, 82),
    ],
    [
        (-9, 71),
        (-7, 37),
        (-8, 44),
        (-11, 49),
        (-10, 56),
        (-12, 59),
        (-8, 63),
        (-9, 67),
        (-6, 68),
        (-10, 79),
    ],
];
const CABAC_P_CHROMA_DC_ABS_INIT: [[(i8, i8); 10]; 3] = [
    [
        (0, 70),
        (-4, 29),
        (5, 31),
        (7, 42),
        (1, 59),
        (-2, 58),
        (-3, 72),
        (-3, 81),
        (-11, 97),
        (0, 58),
    ],
    [
        (2, 66),
        (-9, 34),
        (1, 32),
        (11, 31),
        (5, 52),
        (-2, 55),
        (-2, 67),
        (0, 73),
        (-8, 89),
        (3, 52),
    ],
    [
        (-4, 79),
        (-22, 69),
        (-16, 75),
        (-2, 58),
        (1, 58),
        (-13, 78),
        (-9, 83),
        (-4, 81),
        (-13, 99),
        (-13, 81),
    ],
];
const CABAC_P_CHROMA_AC_ABS_INIT: [[(i8, i8); 10]; 3] = [
    [
        (0, 58),
        (8, 5),
        (10, 14),
        (14, 18),
        (13, 27),
        (2, 40),
        (0, 58),
        (-3, 70),
        (-6, 79),
        (-8, 85),
    ],
    [
        (3, 52),
        (7, 4),
        (10, 8),
        (17, 8),
        (16, 19),
        (3, 37),
        (-1, 61),
        (-5, 73),
        (-1, 70),
        (-4, 78),
    ],
    [
        (-13, 81),
        (-6, 38),
        (-13, 62),
        (-6, 58),
        (-2, 59),
        (-16, 73),
        (-10, 76),
        (-13, 86),
        (-9, 83),
        (-10, 87),
    ],
];

struct CabacPContexts {
    skip: [ContextState; 3],
    macroblock_type: [ContextState; 7],
    sub_macroblock_type: [ContextState; 3],
    motion_x: [ContextState; 7],
    motion_y: [ContextState; 7],
    coded_block_pattern_luma: [ContextState; 4],
    coded_block_pattern_chroma: [ContextState; 8],
    macroblock_qp_delta: [ContextState; 4],
    luma_coded_block: [ContextState; 4],
    chroma_dc_coded_block: [ContextState; 4],
    chroma_ac_coded_block: [ContextState; 4],
    luma_significant: [ContextState; 15],
    luma_last: [ContextState; 15],
    luma_abs_level: [ContextState; 10],
    chroma_dc_significant: [ContextState; 4],
    chroma_dc_last: [ContextState; 4],
    chroma_dc_abs_level: [ContextState; 10],
    chroma_ac_significant: [ContextState; 14],
    chroma_ac_last: [ContextState; 14],
    chroma_ac_abs_level: [ContextState; 10],
    chroma_prediction_mode: [ContextState; 4],
    intra4_prediction_mode: [ContextState; 2],
    luma_dc_coded_block: [ContextState; 4],
    luma_ac_coded_block: [ContextState; 4],
    luma_dc_significant: [ContextState; 15],
    luma_dc_last: [ContextState; 15],
    luma_dc_abs_level: [ContextState; 10],
    luma_ac_significant: [ContextState; 14],
    luma_ac_last: [ContextState; 14],
    luma_ac_abs_level: [ContextState; 10],
    transform_size_8x8: [ContextState; 3],
    luma_8x8_significant: [ContextState; 15],
    luma_8x8_last: [ContextState; 9],
    luma_8x8_abs_level: [ContextState; 10],
}

impl CabacPContexts {
    fn new(slice_qp_y: i32, cabac_init_idc: u32) -> Result<Self> {
        let index = usize::try_from(cabac_init_idc).expect("CABAC initialization idc fits usize");
        if index >= CABAC_P_SKIP_INIT.len() {
            return Err(Error::InvalidData("invalid H.264 cabac_init_idc".into()));
        }
        Ok(Self {
            skip: initial_contexts(&CABAC_P_SKIP_INIT[index], slice_qp_y)?,
            macroblock_type: initial_contexts(&CABAC_P_MACROBLOCK_TYPE_INIT[index], slice_qp_y)?,
            sub_macroblock_type: initial_contexts(
                &CABAC_P_SUB_MACROBLOCK_TYPE_INIT[index],
                slice_qp_y,
            )?,
            motion_x: initial_contexts(&CABAC_P_MVD_X_INIT[index], slice_qp_y)?,
            motion_y: initial_contexts(&CABAC_P_MVD_Y_INIT[index], slice_qp_y)?,
            coded_block_pattern_luma: initial_contexts(
                &CABAC_P_CODED_BLOCK_PATTERN_LUMA_INIT[index],
                slice_qp_y,
            )?,
            coded_block_pattern_chroma: initial_contexts(
                &CABAC_P_CODED_BLOCK_PATTERN_CHROMA_INIT[index],
                slice_qp_y,
            )?,
            macroblock_qp_delta: initial_contexts(&CABAC_I_MB_QP_DELTA_INIT, slice_qp_y)?,
            luma_coded_block: initial_contexts(&CABAC_P_LUMA_CBF_INIT[index], slice_qp_y)?,
            chroma_dc_coded_block: initial_contexts(
                &CABAC_P_CHROMA_DC_CBF_INIT[index],
                slice_qp_y,
            )?,
            chroma_ac_coded_block: initial_contexts(
                &CABAC_P_CHROMA_AC_CBF_INIT[index],
                slice_qp_y,
            )?,
            luma_significant: initial_contexts(&CABAC_P_LUMA_SIG_INIT[index], slice_qp_y)?,
            luma_last: initial_contexts(&CABAC_P_LUMA_LAST_INIT[index], slice_qp_y)?,
            luma_abs_level: initial_contexts(&CABAC_P_LUMA_ABS_INIT[index], slice_qp_y)?,
            chroma_dc_significant: initial_contexts(
                &CABAC_P_CHROMA_DC_SIG_INIT[index],
                slice_qp_y,
            )?,
            chroma_dc_last: initial_contexts(&CABAC_P_CHROMA_DC_LAST_INIT[index], slice_qp_y)?,
            chroma_dc_abs_level: initial_contexts(&CABAC_P_CHROMA_DC_ABS_INIT[index], slice_qp_y)?,
            chroma_ac_significant: initial_contexts(
                &CABAC_P_CHROMA_AC_SIG_INIT[index],
                slice_qp_y,
            )?,
            chroma_ac_last: initial_contexts(&CABAC_P_CHROMA_AC_LAST_INIT[index], slice_qp_y)?,
            chroma_ac_abs_level: initial_contexts(&CABAC_P_CHROMA_AC_ABS_INIT[index], slice_qp_y)?,
            chroma_prediction_mode: initial_contexts(&CABAC_I_CHROMA_PRED_MODE_INIT, slice_qp_y)?,
            intra4_prediction_mode: initial_contexts(&CABAC_I_INTRA4_PRED_MODE_INIT, slice_qp_y)?,
            luma_dc_coded_block: initial_contexts(&CABAC_P_LUMA_DC_CBF_INIT[index], slice_qp_y)?,
            luma_ac_coded_block: initial_contexts(&CABAC_P_LUMA_AC_CBF_INIT[index], slice_qp_y)?,
            luma_dc_significant: initial_contexts(&CABAC_P_LUMA_DC_SIG_INIT[index], slice_qp_y)?,
            luma_dc_last: initial_contexts(&CABAC_P_LUMA_DC_LAST_INIT[index], slice_qp_y)?,
            luma_dc_abs_level: initial_contexts(&CABAC_P_LUMA_DC_ABS_INIT[index], slice_qp_y)?,
            luma_ac_significant: initial_contexts(&CABAC_P_LUMA_AC_SIG_INIT[index], slice_qp_y)?,
            luma_ac_last: initial_contexts(&CABAC_P_LUMA_AC_LAST_INIT[index], slice_qp_y)?,
            luma_ac_abs_level: initial_contexts(&CABAC_P_LUMA_AC_ABS_INIT[index], slice_qp_y)?,
            transform_size_8x8: initial_contexts(
                &CABAC_P_TRANSFORM_SIZE_8X8_INIT[index],
                slice_qp_y,
            )?,
            luma_8x8_significant: initial_contexts(&CABAC_P_LUMA_8X8_SIG_INIT[index], slice_qp_y)?,
            luma_8x8_last: initial_contexts(&CABAC_P_LUMA_8X8_LAST_INIT[index], slice_qp_y)?,
            luma_8x8_abs_level: initial_contexts(&CABAC_P_LUMA_8X8_ABS_INIT[index], slice_qp_y)?,
        })
    }
}

#[allow(clippy::too_many_arguments)]
fn decode_cabac_p_macroblocks(
    bits: &mut BitReader<'_>,
    buffer: &mut FrameBuffer,
    reference: &ReferenceFrame,
    prediction_weights: PredictionWeights,
    luma_qp: &mut i32,
    cabac_init_idc: u32,
    chroma_qp_offset: i32,
    transform_8x8_mode: bool,
) -> Result<()> {
    let mut contexts = CabacPContexts::new(*luma_qp, cabac_init_idc)?;
    let mut decoder = CabacDecoder::new(bits)?;
    let macroblocks_wide = buffer.coded_width / 16;
    let mut skipped = vec![false; buffer.macroblock_count()];
    let mut coded_blocks = CabacICodedBlocks::new(buffer.macroblock_count());
    let mut motion_differences = vec![[MotionVector::default(); 16]; buffer.macroblock_count()];
    let mut chroma_prediction_modes = vec![0_u32; buffer.macroblock_count()];
    let mut transform_8x8 = vec![false; buffer.macroblock_count()];
    let mut previous_qp_delta = 0;
    for address in 0..buffer.macroblock_count() {
        let context_increment =
            usize::from(!address.is_multiple_of(macroblocks_wide) && !skipped[address - 1])
                + usize::from(address >= macroblocks_wide && !skipped[address - macroblocks_wide]);
        if decoder.decision(&mut contexts.skip[context_increment])? {
            skipped[address] = true;
            previous_qp_delta = 0;
            buffer.predict_p_skip(reference, address, prediction_weights, *luma_qp)?;
        } else {
            decode_cabac_p_macroblock(
                &mut decoder,
                &mut contexts,
                buffer,
                reference,
                address,
                macroblocks_wide,
                prediction_weights,
                luma_qp,
                &mut previous_qp_delta,
                &mut coded_blocks,
                &mut motion_differences,
                chroma_qp_offset,
                &mut chroma_prediction_modes,
                transform_8x8_mode,
                &mut transform_8x8,
            )?;
        }
        if decoder.terminate()? {
            if address + 1 != buffer.macroblock_count() {
                return Err(Error::InvalidData(
                    "H.264 CABAC P slice ended before the complete picture".into(),
                ));
            }
            return Ok(());
        }
    }
    Err(Error::InvalidData(
        "H.264 CABAC P slice is missing end_of_slice_flag".into(),
    ))
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn decode_cabac_p_macroblock(
    decoder: &mut CabacDecoder<'_, '_>,
    contexts: &mut CabacPContexts,
    buffer: &mut FrameBuffer,
    reference: &ReferenceFrame,
    address: usize,
    macroblocks_wide: usize,
    prediction_weights: PredictionWeights,
    luma_qp: &mut i32,
    previous_qp_delta: &mut i32,
    coded_blocks: &mut CabacICodedBlocks,
    motion_differences: &mut [[MotionVector; 16]],
    chroma_qp_offset: i32,
    chroma_prediction_modes: &mut [u32],
    transform_8x8_mode: bool,
    transform_8x8: &mut [bool],
) -> Result<()> {
    if decoder.decision(&mut contexts.macroblock_type[0])? {
        let macroblock_type = decode_cabac_p_intra_macroblock_type(decoder, contexts)?;
        return match macroblock_type {
            0 => {
                let use_8x8 = transform_8x8_mode
                    && decode_cabac_transform_size_8x8(
                        decoder,
                        &mut contexts.transform_size_8x8,
                        address,
                        macroblocks_wide,
                        transform_8x8,
                    )?;
                transform_8x8[address] = use_8x8;
                buffer.transform_8x8[address] = use_8x8;
                if use_8x8 {
                    decode_cabac_p_intra8(
                        decoder,
                        contexts,
                        buffer,
                        address,
                        macroblocks_wide,
                        chroma_qp_offset,
                        previous_qp_delta,
                        luma_qp,
                        chroma_prediction_modes,
                        coded_blocks,
                    )
                } else {
                    decode_cabac_p_intra4(
                        decoder,
                        contexts,
                        buffer,
                        address,
                        macroblocks_wide,
                        chroma_qp_offset,
                        previous_qp_delta,
                        luma_qp,
                        chroma_prediction_modes,
                        coded_blocks,
                    )
                }
            }
            1..=24 => decode_cabac_p_intra16(
                decoder,
                contexts,
                buffer,
                address,
                macroblocks_wide,
                macroblock_type,
                chroma_qp_offset,
                previous_qp_delta,
                luma_qp,
                chroma_prediction_modes,
                coded_blocks,
            ),
            25 => {
                let samples = decoder.pcm_samples(384)?;
                buffer.place_pcm_macroblock(address, &samples)?;
                buffer.mark_pcm(address);
                coded_blocks.mark_pcm(address);
                *previous_qp_delta = 0;
                Ok(())
            }
            _ => Err(Error::Unsupported(format!(
                "native H.264 CABAC P-slice intra macroblock type {macroblock_type} is not implemented yet"
            ))),
        };
    }
    let partitions = if !decoder.decision(&mut contexts.macroblock_type[1])? {
        if decoder.decision(&mut contexts.macroblock_type[2])? {
            decode_cabac_p8x8_partitions(decoder, &mut contexts.sub_macroblock_type)?
        } else {
            vec![InterPartition::new(
                0,
                0,
                4,
                4,
                MotionPredictionKind::Normal,
            )]
        }
    } else if decoder.decision(&mut contexts.macroblock_type[3])? {
        vec![
            InterPartition::new(0, 0, 4, 2, MotionPredictionKind::Top16x8),
            InterPartition::new(0, 2, 4, 2, MotionPredictionKind::Bottom16x8),
        ]
    } else {
        vec![
            InterPartition::new(0, 0, 2, 4, MotionPredictionKind::Left8x16),
            InterPartition::new(2, 0, 2, 4, MotionPredictionKind::Right8x16),
        ]
    };
    let transform_allowed = transform_8x8_mode
        && partitions
            .iter()
            .all(|partition| partition.block_width >= 2 && partition.block_height >= 2);
    for partition in partitions {
        decode_cabac_inter_partition(
            decoder,
            contexts,
            buffer,
            reference,
            address,
            macroblocks_wide,
            partition,
            prediction_weights,
            motion_differences,
        )?;
    }

    let pattern = decode_cabac_coded_block_pattern(
        decoder,
        &mut contexts.coded_block_pattern_luma,
        &mut contexts.coded_block_pattern_chroma,
        address,
        macroblocks_wide,
        &coded_blocks.patterns,
    )?;
    coded_blocks.patterns[address] = pattern;
    let use_8x8 = transform_allowed
        && pattern & 15 != 0
        && decode_cabac_transform_size_8x8(
            decoder,
            &mut contexts.transform_size_8x8,
            address,
            macroblocks_wide,
            transform_8x8,
        )?;
    transform_8x8[address] = use_8x8;
    buffer.transform_8x8[address] = use_8x8;
    decode_cabac_p16_residuals(
        decoder,
        contexts,
        buffer,
        address,
        macroblocks_wide,
        pattern,
        luma_qp,
        previous_qp_delta,
        coded_blocks,
        chroma_qp_offset,
        use_8x8,
    )?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn decode_cabac_p_intra4(
    decoder: &mut CabacDecoder<'_, '_>,
    contexts: &mut CabacPContexts,
    buffer: &mut FrameBuffer,
    address: usize,
    macroblocks_wide: usize,
    chroma_qp_offset: i32,
    previous_qp_delta: &mut i32,
    luma_qp: &mut i32,
    chroma_prediction_modes: &mut [u32],
    coded_blocks: &mut CabacICodedBlocks,
) -> Result<()> {
    buffer.mark_intra(address);
    let modes = decode_cabac_intra4_prediction_modes(
        decoder,
        &mut contexts.intra4_prediction_mode,
        buffer,
        address,
    )?;
    let chroma_mode = decode_cabac_chroma_prediction_mode(
        decoder,
        &mut contexts.chroma_prediction_mode,
        address,
        macroblocks_wide,
        chroma_prediction_modes,
    )?;
    chroma_prediction_modes[address] = chroma_mode;
    let pattern = decode_cabac_coded_block_pattern(
        decoder,
        &mut contexts.coded_block_pattern_luma,
        &mut contexts.coded_block_pattern_chroma,
        address,
        macroblocks_wide,
        &coded_blocks.patterns,
    )?;
    coded_blocks.patterns[address] = pattern;
    update_cabac_inter_qp(
        decoder,
        &mut contexts.macroblock_qp_delta,
        pattern,
        luma_qp,
        previous_qp_delta,
    )?;

    let luma_blocks = decode_cabac_p_intra4_luma(
        decoder,
        contexts,
        buffer,
        address,
        macroblocks_wide,
        pattern & 15,
        coded_blocks,
    )?;
    let (chroma_dc_levels, chroma_blocks) = decode_cabac_p_intra_chroma(
        decoder,
        contexts,
        buffer,
        address,
        macroblocks_wide,
        u32::from(pattern >> 4),
        coded_blocks,
    )?;
    for (block_index, (mode, levels)) in modes.iter().zip(&luma_blocks).enumerate() {
        buffer.reconstruct_intra4_luma_block(address, block_index, *mode, levels, *luma_qp)?;
    }
    buffer.predict_chroma_macroblock(address, chroma_mode)?;
    buffer.add_chroma_residual(
        address,
        &chroma_dc_levels,
        &chroma_blocks,
        chroma_qp(*luma_qp, chroma_qp_offset),
    )?;
    buffer.set_luma_qp(address, *luma_qp);
    Ok(())
}

fn decode_cabac_p_intra4_luma(
    decoder: &mut CabacDecoder<'_, '_>,
    contexts: &mut CabacPContexts,
    buffer: &mut FrameBuffer,
    address: usize,
    macroblocks_wide: usize,
    coded_block_pattern: u8,
    coded_blocks: &mut CabacICodedBlocks,
) -> Result<[Vec<i32>; 16]> {
    let mut blocks: [Vec<i32>; 16] = std::array::from_fn(|_| vec![0; 16]);
    for group in 0..4 {
        for within_group in 0..4 {
            let block_index = group * 4 + within_group;
            if coded_block_pattern & (1 << group) == 0 {
                buffer.set_luma_nonzero(address, block_index, 0);
                continue;
            }
            let coded_context = cabac_intra_luma_ac_coded_context(
                address,
                macroblocks_wide,
                block_index,
                &coded_blocks.luma_ac,
            );
            coded_blocks.luma_ac[address][block_index] =
                decoder.decision(&mut contexts.luma_coded_block[coded_context])?;
            if coded_blocks.luma_ac[address][block_index] {
                blocks[block_index] = decode_cabac_residual_block(
                    decoder,
                    &mut contexts.luma_significant,
                    &mut contexts.luma_last,
                    &mut contexts.luma_abs_level,
                    16,
                )?;
                buffer.set_luma_nonzero(
                    address,
                    block_index,
                    nonzero_coefficient_count(&blocks[block_index]),
                );
            }
        }
    }
    Ok(blocks)
}

#[allow(clippy::too_many_arguments)]
fn decode_cabac_p_intra8(
    decoder: &mut CabacDecoder<'_, '_>,
    contexts: &mut CabacPContexts,
    buffer: &mut FrameBuffer,
    address: usize,
    macroblocks_wide: usize,
    chroma_qp_offset: i32,
    previous_qp_delta: &mut i32,
    luma_qp: &mut i32,
    chroma_prediction_modes: &mut [u32],
    coded_blocks: &mut CabacICodedBlocks,
) -> Result<()> {
    buffer.mark_intra(address);
    let modes = decode_cabac_intra8_prediction_modes_with_contexts(
        decoder,
        &mut contexts.intra4_prediction_mode,
        buffer,
        address,
    )?;
    let chroma_mode = decode_cabac_chroma_prediction_mode(
        decoder,
        &mut contexts.chroma_prediction_mode,
        address,
        macroblocks_wide,
        chroma_prediction_modes,
    )?;
    chroma_prediction_modes[address] = chroma_mode;
    let pattern = decode_cabac_coded_block_pattern(
        decoder,
        &mut contexts.coded_block_pattern_luma,
        &mut contexts.coded_block_pattern_chroma,
        address,
        macroblocks_wide,
        &coded_blocks.patterns,
    )?;
    coded_blocks.patterns[address] = pattern;
    update_cabac_inter_qp(
        decoder,
        &mut contexts.macroblock_qp_delta,
        pattern,
        luma_qp,
        previous_qp_delta,
    )?;
    let luma_blocks = decode_cabac_p_luma_8x8(
        decoder,
        contexts,
        buffer,
        address,
        pattern & 15,
        coded_blocks,
    )?;
    let (chroma_dc_levels, chroma_blocks) = decode_cabac_p_intra_chroma(
        decoder,
        contexts,
        buffer,
        address,
        macroblocks_wide,
        u32::from(pattern >> 4),
        coded_blocks,
    )?;
    for group in 0..4 {
        buffer.reconstruct_intra8_luma_block(
            address,
            group,
            modes[group],
            &luma_blocks[group],
            *luma_qp,
        )?;
    }
    buffer.predict_chroma_macroblock(address, chroma_mode)?;
    buffer.add_chroma_residual(
        address,
        &chroma_dc_levels,
        &chroma_blocks,
        chroma_qp(*luma_qp, chroma_qp_offset),
    )?;
    buffer.set_luma_qp(address, *luma_qp);
    Ok(())
}

fn decode_cabac_p_luma_8x8(
    decoder: &mut CabacDecoder<'_, '_>,
    contexts: &mut CabacPContexts,
    buffer: &mut FrameBuffer,
    address: usize,
    coded_block_pattern: u8,
    coded_blocks: &mut CabacICodedBlocks,
) -> Result<[Vec<i32>; 4]> {
    let mut blocks: [Vec<i32>; 4] = std::array::from_fn(|_| vec![0; 64]);
    for (group, levels) in blocks.iter_mut().enumerate() {
        if coded_block_pattern & (1 << group) != 0 {
            *levels = decode_cabac_residual_8x8(
                decoder,
                &mut contexts.luma_8x8_significant,
                &mut contexts.luma_8x8_last,
                &mut contexts.luma_8x8_abs_level,
            )?;
        }
        let nonzero = nonzero_coefficient_count(levels);
        for block in group * 4..group * 4 + 4 {
            buffer.set_luma_nonzero(address, block, nonzero);
            coded_blocks.luma_ac[address][block] = nonzero != 0;
        }
    }
    Ok(blocks)
}

fn decode_cabac_p_intra_macroblock_type(
    decoder: &mut CabacDecoder<'_, '_>,
    contexts: &mut CabacPContexts,
) -> Result<u32> {
    if !decoder.decision(&mut contexts.macroblock_type[3])? {
        return Ok(0);
    }
    if decoder.terminate()? {
        return Ok(25);
    }
    let mut macroblock_type = 1;
    macroblock_type += 12 * u32::from(decoder.decision(&mut contexts.macroblock_type[4])?);
    if decoder.decision(&mut contexts.macroblock_type[5])? {
        macroblock_type += 4 + 4 * u32::from(decoder.decision(&mut contexts.macroblock_type[5])?);
    }
    macroblock_type += 2 * u32::from(decoder.decision(&mut contexts.macroblock_type[6])?);
    macroblock_type += u32::from(decoder.decision(&mut contexts.macroblock_type[6])?);
    Ok(macroblock_type)
}

#[allow(clippy::too_many_arguments)]
fn decode_cabac_p_intra16(
    decoder: &mut CabacDecoder<'_, '_>,
    contexts: &mut CabacPContexts,
    buffer: &mut FrameBuffer,
    address: usize,
    macroblocks_wide: usize,
    macroblock_type: u32,
    chroma_qp_offset: i32,
    previous_qp_delta: &mut i32,
    luma_qp: &mut i32,
    chroma_prediction_modes: &mut [u32],
    coded_blocks: &mut CabacICodedBlocks,
) -> Result<()> {
    let code = macroblock_type - 1;
    let luma_prediction_mode = code % 4;
    let chroma_pattern = (code / 4) % 3;
    let luma_pattern = u32::from(code >= 12) * 15;
    coded_blocks.patterns[address] =
        u8::try_from(luma_pattern | (chroma_pattern << 4)).expect("coded block pattern fits u8");
    let chroma_mode = decode_cabac_chroma_prediction_mode(
        decoder,
        &mut contexts.chroma_prediction_mode,
        address,
        macroblocks_wide,
        chroma_prediction_modes,
    )?;
    chroma_prediction_modes[address] = chroma_mode;
    let delta = decode_cabac_macroblock_qp_delta(
        decoder,
        &mut contexts.macroblock_qp_delta,
        *previous_qp_delta,
    )?;
    *previous_qp_delta = delta;
    *luma_qp = (*luma_qp + delta).rem_euclid(52);
    let (luma_dc_levels, luma_blocks) = decode_cabac_p_intra16_luma(
        decoder,
        contexts,
        buffer,
        address,
        macroblocks_wide,
        luma_pattern,
        coded_blocks,
    )?;
    let (chroma_dc_levels, chroma_blocks) = decode_cabac_p_intra_chroma(
        decoder,
        contexts,
        buffer,
        address,
        macroblocks_wide,
        chroma_pattern,
        coded_blocks,
    )?;
    buffer.mark_intra(address);
    buffer.predict_intra16_macroblock(address, luma_prediction_mode, chroma_mode)?;
    buffer.add_intra16_luma_residual(address, &luma_dc_levels, &luma_blocks, *luma_qp)?;
    buffer.add_chroma_residual(
        address,
        &chroma_dc_levels,
        &chroma_blocks,
        chroma_qp(*luma_qp, chroma_qp_offset),
    )?;
    buffer.set_luma_qp(address, *luma_qp);
    Ok(())
}

fn decode_cabac_p_intra16_luma(
    decoder: &mut CabacDecoder<'_, '_>,
    contexts: &mut CabacPContexts,
    buffer: &mut FrameBuffer,
    address: usize,
    macroblocks_wide: usize,
    coded_block_pattern: u32,
    coded_blocks: &mut CabacICodedBlocks,
) -> Result<(Vec<i32>, [Vec<i32>; 16])> {
    let coded_context = cabac_intra_dc_coded_context(address, macroblocks_wide, |neighbor| {
        coded_blocks.luma_dc[neighbor]
    });
    coded_blocks.luma_dc[address] =
        decoder.decision(&mut contexts.luma_dc_coded_block[coded_context])?;
    let dc_levels = if coded_blocks.luma_dc[address] {
        decode_cabac_residual_block(
            decoder,
            &mut contexts.luma_dc_significant,
            &mut contexts.luma_dc_last,
            &mut contexts.luma_dc_abs_level,
            16,
        )?
    } else {
        vec![0; 16]
    };
    let mut blocks: [Vec<i32>; 16] = std::array::from_fn(|_| vec![0; 15]);
    if coded_block_pattern == 0 {
        buffer.mark_zero_ac(address);
        return Ok((dc_levels, blocks));
    }
    for (block_index, levels) in blocks.iter_mut().enumerate() {
        let coded_context = cabac_intra_luma_ac_coded_context(
            address,
            macroblocks_wide,
            block_index,
            &coded_blocks.luma_ac,
        );
        coded_blocks.luma_ac[address][block_index] =
            decoder.decision(&mut contexts.luma_ac_coded_block[coded_context])?;
        if coded_blocks.luma_ac[address][block_index] {
            *levels = decode_cabac_residual_block(
                decoder,
                &mut contexts.luma_ac_significant,
                &mut contexts.luma_ac_last,
                &mut contexts.luma_ac_abs_level,
                15,
            )?;
            buffer.set_luma_nonzero(address, block_index, nonzero_coefficient_count(levels));
        }
    }
    Ok((dc_levels, blocks))
}

fn decode_cabac_p_intra_chroma(
    decoder: &mut CabacDecoder<'_, '_>,
    contexts: &mut CabacPContexts,
    buffer: &mut FrameBuffer,
    address: usize,
    macroblocks_wide: usize,
    coded_block_pattern: u32,
    coded_blocks: &mut CabacICodedBlocks,
) -> Result<(ChromaDcLevels, ChromaAcLevels)> {
    let mut dc_levels: ChromaDcLevels = std::array::from_fn(|_| vec![0; 4]);
    if coded_block_pattern != 0 {
        for (component, levels) in dc_levels.iter_mut().enumerate() {
            let coded_context =
                cabac_intra_dc_coded_context(address, macroblocks_wide, |neighbor| {
                    coded_blocks.chroma_dc[neighbor][component]
                });
            coded_blocks.chroma_dc[address][component] =
                decoder.decision(&mut contexts.chroma_dc_coded_block[coded_context])?;
            if coded_blocks.chroma_dc[address][component] {
                *levels = decode_cabac_residual_block(
                    decoder,
                    &mut contexts.chroma_dc_significant,
                    &mut contexts.chroma_dc_last,
                    &mut contexts.chroma_dc_abs_level,
                    4,
                )?;
            }
        }
    }
    let mut blocks: ChromaAcLevels = std::array::from_fn(|_| std::array::from_fn(|_| vec![0; 15]));
    if coded_block_pattern != 2 {
        buffer.mark_zero_chroma_ac(address);
        return Ok((dc_levels, blocks));
    }
    for (component, component_blocks) in blocks.iter_mut().enumerate() {
        for (block_index, levels) in component_blocks.iter_mut().enumerate() {
            let coded_context = cabac_intra_chroma_ac_coded_context(
                address,
                macroblocks_wide,
                component,
                block_index,
                &coded_blocks.chroma_ac,
            );
            coded_blocks.chroma_ac[address][component][block_index] =
                decoder.decision(&mut contexts.chroma_ac_coded_block[coded_context])?;
            if coded_blocks.chroma_ac[address][component][block_index] {
                *levels = decode_cabac_residual_block(
                    decoder,
                    &mut contexts.chroma_ac_significant,
                    &mut contexts.chroma_ac_last,
                    &mut contexts.chroma_ac_abs_level,
                    15,
                )?;
                buffer.set_chroma_nonzero(
                    address,
                    component,
                    block_index,
                    nonzero_coefficient_count(levels),
                );
            }
        }
    }
    Ok((dc_levels, blocks))
}

fn decode_cabac_p8x8_partitions(
    decoder: &mut CabacDecoder<'_, '_>,
    contexts: &mut [ContextState; 3],
) -> Result<Vec<InterPartition>> {
    let mut sub_types = [0_u8; 4];
    for sub_type in &mut sub_types {
        *sub_type = if decoder.decision(&mut contexts[0])? {
            0
        } else if !decoder.decision(&mut contexts[1])? {
            1
        } else if decoder.decision(&mut contexts[2])? {
            2
        } else {
            3
        };
    }
    Ok(p8x8_partitions(sub_types))
}

fn p8x8_partitions(sub_types: [u8; 4]) -> Vec<InterPartition> {
    let mut partitions = Vec::with_capacity(16);
    for (sub_index, sub_type) in sub_types.into_iter().enumerate() {
        let base_x = (sub_index % 2) * 2;
        let base_y = (sub_index / 2) * 2;
        match sub_type {
            0 => partitions.push(InterPartition::new(
                base_x,
                base_y,
                2,
                2,
                MotionPredictionKind::Normal,
            )),
            1 => {
                partitions.push(InterPartition::new(
                    base_x,
                    base_y,
                    2,
                    1,
                    MotionPredictionKind::Normal,
                ));
                partitions.push(InterPartition::new(
                    base_x,
                    base_y + 1,
                    2,
                    1,
                    MotionPredictionKind::Normal,
                ));
            }
            2 => {
                partitions.push(InterPartition::new(
                    base_x,
                    base_y,
                    1,
                    2,
                    MotionPredictionKind::Normal,
                ));
                partitions.push(InterPartition::new(
                    base_x + 1,
                    base_y,
                    1,
                    2,
                    MotionPredictionKind::Normal,
                ));
            }
            3 => {
                for y in 0..2 {
                    for x in 0..2 {
                        partitions.push(InterPartition::new(
                            base_x + x,
                            base_y + y,
                            1,
                            1,
                            MotionPredictionKind::Normal,
                        ));
                    }
                }
            }
            _ => unreachable!("CABAC P sub-macroblock type is in 0..=3"),
        }
    }
    partitions
}

#[allow(clippy::too_many_arguments)]
fn decode_cabac_inter_partition(
    decoder: &mut CabacDecoder<'_, '_>,
    contexts: &mut CabacPContexts,
    buffer: &mut FrameBuffer,
    reference: &ReferenceFrame,
    address: usize,
    macroblocks_wide: usize,
    partition: InterPartition,
    prediction_weights: PredictionWeights,
    motion_differences: &mut [[MotionVector; 16]],
) -> Result<()> {
    let predictor = buffer.partition_motion_vector_predictor(
        address,
        partition.block_x,
        partition.block_y,
        partition.block_width,
        partition.prediction_kind,
    );
    let horizontal_context = cabac_mvd_neighbor_sum(
        address,
        macroblocks_wide,
        partition.block_x,
        partition.block_y,
        motion_differences,
        |vector| vector.x,
    );
    let vertical_context = cabac_mvd_neighbor_sum(
        address,
        macroblocks_wide,
        partition.block_x,
        partition.block_y,
        motion_differences,
        |vector| vector.y,
    );
    let difference = MotionVector {
        x: decode_cabac_motion_difference(decoder, &mut contexts.motion_x, horizontal_context)?,
        y: decode_cabac_motion_difference(decoder, &mut contexts.motion_y, vertical_context)?,
    };
    for y in partition.block_y..partition.block_y + partition.block_height {
        for x in partition.block_x..partition.block_x + partition.block_width {
            motion_differences[address][luma_block_index(x, y)] = difference;
        }
    }
    let vector =
        MotionVector {
            x: predictor.x.checked_add(difference.x).ok_or_else(|| {
                Error::InvalidData("H.264 horizontal motion vector overflows".into())
            })?,
            y: predictor.y.checked_add(difference.y).ok_or_else(|| {
                Error::InvalidData("H.264 vertical motion vector overflows".into())
            })?,
        };
    buffer.predict_inter_partition(
        reference,
        address,
        partition.block_x,
        partition.block_y,
        partition.block_width,
        partition.block_height,
        vector,
        prediction_weights,
    )?;
    buffer.set_partition_motion(
        address,
        partition.block_x,
        partition.block_y,
        partition.block_width,
        partition.block_height,
        vector,
    );
    Ok(())
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn decode_cabac_p16_residuals(
    decoder: &mut CabacDecoder<'_, '_>,
    contexts: &mut CabacPContexts,
    buffer: &mut FrameBuffer,
    address: usize,
    macroblocks_wide: usize,
    pattern: u8,
    luma_qp: &mut i32,
    previous_qp_delta: &mut i32,
    coded_blocks: &mut CabacICodedBlocks,
    chroma_qp_offset: i32,
    transform_8x8: bool,
) -> Result<()> {
    update_cabac_inter_qp(
        decoder,
        &mut contexts.macroblock_qp_delta,
        pattern,
        luma_qp,
        previous_qp_delta,
    )?;

    let (luma_blocks, luma_8x8_blocks) = if transform_8x8 {
        (
            std::array::from_fn(|_| vec![0; 16]),
            Some(decode_cabac_p_luma_8x8(
                decoder,
                contexts,
                buffer,
                address,
                pattern & 15,
                coded_blocks,
            )?),
        )
    } else {
        (
            decode_cabac_p_inter_luma(
                decoder,
                contexts,
                buffer,
                address,
                macroblocks_wide,
                pattern & 15,
                coded_blocks,
            )?,
            None,
        )
    };

    let chroma_pattern = pattern >> 4;
    let mut chroma_dc_levels: ChromaDcLevels = std::array::from_fn(|_| vec![0; 4]);
    if chroma_pattern != 0 {
        for (component, levels) in chroma_dc_levels.iter_mut().enumerate() {
            let coded_context =
                cabac_inter_dc_coded_context(address, macroblocks_wide, |neighbor| {
                    coded_blocks.chroma_dc[neighbor][component]
                });
            coded_blocks.chroma_dc[address][component] =
                decoder.decision(&mut contexts.chroma_dc_coded_block[coded_context])?;
            if coded_blocks.chroma_dc[address][component] {
                *levels = decode_cabac_residual_block(
                    decoder,
                    &mut contexts.chroma_dc_significant,
                    &mut contexts.chroma_dc_last,
                    &mut contexts.chroma_dc_abs_level,
                    4,
                )?;
            }
        }
    }
    let mut chroma_blocks: ChromaAcLevels =
        std::array::from_fn(|_| std::array::from_fn(|_| vec![0; 15]));
    if chroma_pattern == 2 {
        for (component, component_blocks) in chroma_blocks.iter_mut().enumerate() {
            for (block_index, levels) in component_blocks.iter_mut().enumerate() {
                let coded_context = cabac_inter_chroma_coded_context(
                    address,
                    macroblocks_wide,
                    component,
                    block_index,
                    &coded_blocks.chroma_ac,
                );
                coded_blocks.chroma_ac[address][component][block_index] =
                    decoder.decision(&mut contexts.chroma_ac_coded_block[coded_context])?;
                if coded_blocks.chroma_ac[address][component][block_index] {
                    *levels = decode_cabac_residual_block(
                        decoder,
                        &mut contexts.chroma_ac_significant,
                        &mut contexts.chroma_ac_last,
                        &mut contexts.chroma_ac_abs_level,
                        15,
                    )?;
                    buffer.set_chroma_nonzero(
                        address,
                        component,
                        block_index,
                        nonzero_coefficient_count(levels),
                    );
                }
            }
        }
    } else {
        buffer.mark_zero_chroma_ac(address);
    }
    if let Some(luma_8x8_blocks) = luma_8x8_blocks {
        buffer.add_luma_residual_8x8_blocks(address, &luma_8x8_blocks, *luma_qp)?;
        buffer.add_chroma_residual(
            address,
            &chroma_dc_levels,
            &chroma_blocks,
            chroma_qp(*luma_qp, chroma_qp_offset),
        )?;
    } else {
        add_cabac_inter_residuals(
            buffer,
            address,
            &luma_blocks,
            &chroma_dc_levels,
            &chroma_blocks,
            *luma_qp,
            chroma_qp_offset,
        )?;
    }
    buffer.set_luma_qp(address, *luma_qp);
    Ok(())
}

fn decode_cabac_p_inter_luma(
    decoder: &mut CabacDecoder<'_, '_>,
    contexts: &mut CabacPContexts,
    buffer: &mut FrameBuffer,
    address: usize,
    macroblocks_wide: usize,
    coded_block_pattern: u8,
    coded_blocks: &mut CabacICodedBlocks,
) -> Result<[Vec<i32>; 16]> {
    let mut blocks: [Vec<i32>; 16] = std::array::from_fn(|_| vec![0; 16]);
    for group in 0..4 {
        for within_group in 0..4 {
            let block_index = group * 4 + within_group;
            if coded_block_pattern & (1 << group) == 0 {
                buffer.set_luma_nonzero(address, block_index, 0);
                continue;
            }
            let coded_context = cabac_inter_luma_coded_context(
                address,
                macroblocks_wide,
                block_index,
                &coded_blocks.luma_ac,
            );
            coded_blocks.luma_ac[address][block_index] =
                decoder.decision(&mut contexts.luma_coded_block[coded_context])?;
            if coded_blocks.luma_ac[address][block_index] {
                blocks[block_index] = decode_cabac_residual_block(
                    decoder,
                    &mut contexts.luma_significant,
                    &mut contexts.luma_last,
                    &mut contexts.luma_abs_level,
                    16,
                )?;
                buffer.set_luma_nonzero(
                    address,
                    block_index,
                    nonzero_coefficient_count(&blocks[block_index]),
                );
            }
        }
    }
    Ok(blocks)
}

fn add_cabac_inter_residuals(
    buffer: &mut FrameBuffer,
    address: usize,
    luma_blocks: &[Vec<i32>; 16],
    chroma_dc_levels: &ChromaDcLevels,
    chroma_blocks: &ChromaAcLevels,
    luma_qp: i32,
    chroma_qp_offset: i32,
) -> Result<()> {
    buffer.add_luma_residual_blocks(address, luma_blocks, luma_qp)?;
    buffer.add_chroma_residual(
        address,
        chroma_dc_levels,
        chroma_blocks,
        chroma_qp(luma_qp, chroma_qp_offset),
    )
}

fn update_cabac_inter_qp(
    decoder: &mut CabacDecoder<'_, '_>,
    contexts: &mut [ContextState; 4],
    coded_block_pattern: u8,
    luma_qp: &mut i32,
    previous_qp_delta: &mut i32,
) -> Result<()> {
    if coded_block_pattern == 0 {
        *previous_qp_delta = 0;
        return Ok(());
    }
    let delta = decode_cabac_macroblock_qp_delta(decoder, contexts, *previous_qp_delta)?;
    *previous_qp_delta = delta;
    *luma_qp = (*luma_qp + delta).rem_euclid(52);
    Ok(())
}

fn cabac_inter_dc_coded_context(
    address: usize,
    macroblocks_wide: usize,
    neighbor_is_coded: impl Fn(usize) -> bool,
) -> usize {
    let left = !address.is_multiple_of(macroblocks_wide) && neighbor_is_coded(address - 1);
    let top = address >= macroblocks_wide && neighbor_is_coded(address - macroblocks_wide);
    usize::from(left) + 2 * usize::from(top)
}

fn cabac_inter_luma_coded_context(
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

fn cabac_inter_chroma_coded_context(
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

fn cabac_mvd_neighbor_sum(
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

fn decode_cabac_motion_difference(
    decoder: &mut CabacDecoder<'_, '_>,
    contexts: &mut [ContextState; 7],
    neighboring_absolute_sum: u32,
) -> Result<i32> {
    let initial_context =
        usize::from(neighboring_absolute_sum > 2) + usize::from(neighboring_absolute_sum > 32);
    if !decoder.decision(&mut contexts[initial_context])? {
        return Ok(0);
    }
    let mut magnitude = 1_u32;
    let mut context = 3;
    while magnitude < 9 && decoder.decision(&mut contexts[context])? {
        if magnitude < 4 {
            context += 1;
        }
        magnitude += 1;
    }
    if magnitude >= 9 {
        let mut order = 3_u8;
        while decoder.bypass()? {
            magnitude = magnitude
                .checked_add(1_u32 << order)
                .ok_or_else(|| Error::InvalidData("H.264 CABAC MVD overflows".into()))?;
            order = order
                .checked_add(1)
                .filter(|&value| value <= 24)
                .ok_or_else(|| Error::InvalidData("H.264 CABAC MVD prefix is too long".into()))?;
        }
        while order > 0 {
            order -= 1;
            magnitude = magnitude
                .checked_add(u32::from(decoder.bypass()?) << order)
                .ok_or_else(|| Error::InvalidData("H.264 CABAC MVD overflows".into()))?;
        }
    }
    let magnitude = i32::try_from(magnitude)
        .map_err(|_| Error::InvalidData("H.264 CABAC MVD overflows".into()))?;
    Ok(if decoder.bypass()? {
        -magnitude
    } else {
        magnitude
    })
}

const CABAC_I_MB_QP_DELTA_INIT: [(i8, i8); 4] = [(0, 41), (0, 63), (0, 63), (0, 63)];
const CABAC_I_CHROMA_PRED_MODE_INIT: [(i8, i8); 4] = [(-9, 83), (4, 86), (0, 97), (-7, 72)];
const CABAC_I_INTRA4_PRED_MODE_INIT: [(i8, i8); 2] = [(13, 41), (3, 62)];
const CABAC_I_TRANSFORM_SIZE_8X8_INIT: [(i8, i8); 3] = [(31, 21), (31, 31), (25, 50)];
const CABAC_I_LUMA_8X8_SIGNIFICANT_INIT: [(i8, i8); 15] = [
    (-17, 120),
    (-20, 112),
    (-18, 114),
    (-11, 85),
    (-15, 92),
    (-14, 89),
    (-26, 71),
    (-15, 81),
    (-14, 80),
    (0, 68),
    (-14, 70),
    (-24, 56),
    (-23, 68),
    (-24, 50),
    (-11, 74),
];
const CABAC_I_LUMA_8X8_LAST_INIT: [(i8, i8); 9] = [
    (23, -13),
    (26, -13),
    (40, -15),
    (49, -14),
    (44, 3),
    (45, 6),
    (44, 34),
    (33, 54),
    (19, 82),
];
const CABAC_I_LUMA_8X8_ABS_INIT: [(i8, i8); 10] = [
    (-3, 75),
    (-1, 23),
    (1, 34),
    (1, 43),
    (0, 54),
    (-2, 55),
    (0, 61),
    (1, 64),
    (0, 68),
    (-9, 92),
];
const CABAC_I_CODED_BLOCK_PATTERN_LUMA_INIT: [(i8, i8); 4] =
    [(-17, 127), (-13, 102), (0, 82), (-7, 74)];
const CABAC_I_CODED_BLOCK_PATTERN_CHROMA_INIT: [(i8, i8); 8] = [
    (-21, 107),
    (-27, 127),
    (-31, 127),
    (-24, 127),
    (-18, 95),
    (-27, 127),
    (-21, 114),
    (-30, 127),
];
const CABAC_I_LUMA_DC_CODED_BLOCK_INIT: [(i8, i8); 4] =
    [(-17, 123), (-12, 115), (-16, 122), (-11, 115)];
const CABAC_I_CHROMA_DC_CODED_BLOCK_INIT: [(i8, i8); 4] =
    [(-1, 74), (-6, 97), (-7, 91), (-20, 127)];
const CABAC_I_LUMA_AC_CODED_BLOCK_INIT: [(i8, i8); 4] =
    [(-12, 63), (-2, 68), (-15, 84), (-13, 104)];
const CABAC_I_CHROMA_AC_CODED_BLOCK_INIT: [(i8, i8); 4] =
    [(-4, 56), (-5, 82), (-7, 76), (-22, 125)];
const CABAC_I_LUMA_4X4_CODED_BLOCK_INIT: [(i8, i8); 4] =
    [(-3, 70), (-8, 93), (-10, 90), (-30, 127)];
const CABAC_I_LUMA_DC_SIGNIFICANT_INIT: [(i8, i8); 15] = [
    (-7, 93),
    (-11, 87),
    (-3, 77),
    (-5, 71),
    (-4, 63),
    (-4, 68),
    (-12, 84),
    (-7, 62),
    (-7, 65),
    (8, 61),
    (5, 56),
    (-2, 66),
    (1, 64),
    (0, 61),
    (-2, 78),
];
const CABAC_I_LUMA_DC_LAST_INIT: [(i8, i8); 15] = [
    (24, 0),
    (15, 9),
    (8, 25),
    (13, 18),
    (15, 9),
    (13, 19),
    (10, 37),
    (12, 18),
    (6, 29),
    (20, 33),
    (15, 30),
    (4, 45),
    (1, 58),
    (0, 62),
    (7, 61),
];
const CABAC_I_LUMA_DC_ABS_LEVEL_INIT: [(i8, i8); 10] = [
    (-3, 71),
    (-6, 42),
    (-5, 50),
    (-3, 54),
    (-2, 62),
    (0, 58),
    (1, 63),
    (-2, 72),
    (-1, 74),
    (-9, 91),
];
const CABAC_I_CHROMA_DC_SIGNIFICANT_INIT: [(i8, i8); 4] =
    [(-8, 102), (-15, 100), (0, 95), (-4, 75)];
const CABAC_I_CHROMA_DC_LAST_INIT: [(i8, i8); 4] = [(30, -6), (27, 3), (26, 22), (37, -16)];
const CABAC_I_CHROMA_DC_ABS_LEVEL_INIT: [(i8, i8); 10] = [
    (-11, 97),
    (-20, 84),
    (-11, 79),
    (-6, 73),
    (-4, 74),
    (-13, 86),
    (-13, 96),
    (-11, 97),
    (-19, 117),
    (-8, 78),
];
const CABAC_I_LUMA_AC_SIGNIFICANT_INIT: [(i8, i8); 14] = [
    (1, 50),
    (7, 52),
    (10, 35),
    (0, 44),
    (11, 38),
    (1, 45),
    (0, 46),
    (5, 44),
    (31, 17),
    (1, 51),
    (7, 50),
    (28, 19),
    (16, 33),
    (14, 62),
];
const CABAC_I_LUMA_AC_LAST_INIT: [(i8, i8); 14] = [
    (12, 38),
    (11, 45),
    (15, 39),
    (11, 42),
    (13, 44),
    (16, 45),
    (12, 41),
    (10, 49),
    (30, 34),
    (18, 42),
    (10, 55),
    (17, 51),
    (17, 46),
    (0, 89),
];
const CABAC_I_LUMA_AC_ABS_LEVEL_INIT: [(i8, i8); 10] = [
    (-5, 67),
    (-5, 27),
    (-3, 39),
    (-2, 44),
    (0, 46),
    (-16, 64),
    (-8, 68),
    (-10, 78),
    (-6, 77),
    (-10, 86),
];
const CABAC_I_CHROMA_AC_SIGNIFICANT_INIT: [(i8, i8); 14] = [
    (-4, 75),
    (2, 72),
    (-11, 75),
    (-3, 71),
    (15, 46),
    (-13, 69),
    (0, 62),
    (0, 65),
    (21, 37),
    (-15, 72),
    (9, 57),
    (16, 54),
    (0, 62),
    (12, 72),
];
const CABAC_I_CHROMA_AC_LAST_INIT: [(i8, i8); 14] = [
    (37, -16),
    (35, -4),
    (38, -8),
    (38, -3),
    (37, 3),
    (38, 5),
    (42, 0),
    (35, 16),
    (39, 22),
    (14, 48),
    (27, 37),
    (21, 60),
    (12, 68),
    (2, 97),
];
const CABAC_I_CHROMA_AC_ABS_LEVEL_INIT: [(i8, i8); 10] = [
    (-8, 78),
    (-5, 33),
    (-4, 48),
    (-2, 53),
    (-3, 62),
    (-13, 71),
    (-10, 79),
    (-12, 86),
    (-13, 90),
    (-14, 97),
];
const CABAC_I_LUMA_4X4_SIGNIFICANT_INIT: [(i8, i8); 15] = [
    (-13, 108),
    (-15, 100),
    (-13, 101),
    (-13, 91),
    (-12, 94),
    (-10, 88),
    (-16, 84),
    (-10, 86),
    (-7, 83),
    (-13, 87),
    (-19, 94),
    (1, 70),
    (0, 72),
    (-5, 74),
    (18, 59),
];
const CABAC_I_LUMA_4X4_LAST_INIT: [(i8, i8); 15] = [
    (26, -19),
    (22, -17),
    (26, -17),
    (30, -25),
    (28, -20),
    (33, -23),
    (37, -27),
    (33, -23),
    (40, -28),
    (38, -17),
    (33, -11),
    (40, -15),
    (41, -6),
    (38, 1),
    (41, 17),
];
const CABAC_I_LUMA_4X4_ABS_LEVEL_INIT: [(i8, i8); 10] = [
    (-12, 92),
    (-15, 55),
    (-10, 60),
    (-6, 62),
    (-4, 65),
    (-12, 73),
    (-8, 76),
    (-7, 80),
    (-9, 88),
    (-17, 110),
];

struct CabacIContexts {
    macroblock_type: [ContextState; 11],
    macroblock_qp_delta: [ContextState; 4],
    chroma_prediction_mode: [ContextState; 4],
    intra4_prediction_mode: [ContextState; 2],
    transform_size_8x8: [ContextState; 3],
    coded_block_pattern_luma: [ContextState; 4],
    coded_block_pattern_chroma: [ContextState; 8],
    luma_dc_coded_block: [ContextState; 4],
    chroma_dc_coded_block: [ContextState; 4],
    luma_ac_coded_block: [ContextState; 4],
    chroma_ac_coded_block: [ContextState; 4],
    luma_dc_significant: [ContextState; 15],
    luma_dc_last: [ContextState; 15],
    luma_dc_abs_level: [ContextState; 10],
    chroma_dc_significant: [ContextState; 4],
    chroma_dc_last: [ContextState; 4],
    chroma_dc_abs_level: [ContextState; 10],
    luma_ac_significant: [ContextState; 14],
    luma_ac_last: [ContextState; 14],
    luma_ac_abs_level: [ContextState; 10],
    chroma_ac_significant: [ContextState; 14],
    chroma_ac_last: [ContextState; 14],
    chroma_ac_abs_level: [ContextState; 10],
    luma_4x4_coded_block: [ContextState; 4],
    luma_4x4_significant: [ContextState; 15],
    luma_4x4_last: [ContextState; 15],
    luma_4x4_abs_level: [ContextState; 10],
    luma_8x8_significant: [ContextState; 15],
    luma_8x8_last: [ContextState; 9],
    luma_8x8_abs_level: [ContextState; 10],
}

impl CabacIContexts {
    fn new(slice_qp_y: i32) -> Result<Self> {
        Ok(Self {
            macroblock_type: initial_i_macroblock_contexts(slice_qp_y)?,
            macroblock_qp_delta: initial_contexts(&CABAC_I_MB_QP_DELTA_INIT, slice_qp_y)?,
            chroma_prediction_mode: initial_contexts(&CABAC_I_CHROMA_PRED_MODE_INIT, slice_qp_y)?,
            intra4_prediction_mode: initial_contexts(&CABAC_I_INTRA4_PRED_MODE_INIT, slice_qp_y)?,
            transform_size_8x8: initial_contexts(&CABAC_I_TRANSFORM_SIZE_8X8_INIT, slice_qp_y)?,
            coded_block_pattern_luma: initial_contexts(
                &CABAC_I_CODED_BLOCK_PATTERN_LUMA_INIT,
                slice_qp_y,
            )?,
            coded_block_pattern_chroma: initial_contexts(
                &CABAC_I_CODED_BLOCK_PATTERN_CHROMA_INIT,
                slice_qp_y,
            )?,
            luma_dc_coded_block: initial_contexts(&CABAC_I_LUMA_DC_CODED_BLOCK_INIT, slice_qp_y)?,
            chroma_dc_coded_block: initial_contexts(
                &CABAC_I_CHROMA_DC_CODED_BLOCK_INIT,
                slice_qp_y,
            )?,
            luma_ac_coded_block: initial_contexts(&CABAC_I_LUMA_AC_CODED_BLOCK_INIT, slice_qp_y)?,
            chroma_ac_coded_block: initial_contexts(
                &CABAC_I_CHROMA_AC_CODED_BLOCK_INIT,
                slice_qp_y,
            )?,
            luma_dc_significant: initial_contexts(&CABAC_I_LUMA_DC_SIGNIFICANT_INIT, slice_qp_y)?,
            luma_dc_last: initial_contexts(&CABAC_I_LUMA_DC_LAST_INIT, slice_qp_y)?,
            luma_dc_abs_level: initial_contexts(&CABAC_I_LUMA_DC_ABS_LEVEL_INIT, slice_qp_y)?,
            chroma_dc_significant: initial_contexts(
                &CABAC_I_CHROMA_DC_SIGNIFICANT_INIT,
                slice_qp_y,
            )?,
            chroma_dc_last: initial_contexts(&CABAC_I_CHROMA_DC_LAST_INIT, slice_qp_y)?,
            chroma_dc_abs_level: initial_contexts(&CABAC_I_CHROMA_DC_ABS_LEVEL_INIT, slice_qp_y)?,
            luma_ac_significant: initial_contexts(&CABAC_I_LUMA_AC_SIGNIFICANT_INIT, slice_qp_y)?,
            luma_ac_last: initial_contexts(&CABAC_I_LUMA_AC_LAST_INIT, slice_qp_y)?,
            luma_ac_abs_level: initial_contexts(&CABAC_I_LUMA_AC_ABS_LEVEL_INIT, slice_qp_y)?,
            chroma_ac_significant: initial_contexts(
                &CABAC_I_CHROMA_AC_SIGNIFICANT_INIT,
                slice_qp_y,
            )?,
            chroma_ac_last: initial_contexts(&CABAC_I_CHROMA_AC_LAST_INIT, slice_qp_y)?,
            chroma_ac_abs_level: initial_contexts(&CABAC_I_CHROMA_AC_ABS_LEVEL_INIT, slice_qp_y)?,
            luma_4x4_coded_block: initial_contexts(&CABAC_I_LUMA_4X4_CODED_BLOCK_INIT, slice_qp_y)?,
            luma_4x4_significant: initial_contexts(&CABAC_I_LUMA_4X4_SIGNIFICANT_INIT, slice_qp_y)?,
            luma_4x4_last: initial_contexts(&CABAC_I_LUMA_4X4_LAST_INIT, slice_qp_y)?,
            luma_4x4_abs_level: initial_contexts(&CABAC_I_LUMA_4X4_ABS_LEVEL_INIT, slice_qp_y)?,
            luma_8x8_significant: initial_contexts(&CABAC_I_LUMA_8X8_SIGNIFICANT_INIT, slice_qp_y)?,
            luma_8x8_last: initial_contexts(&CABAC_I_LUMA_8X8_LAST_INIT, slice_qp_y)?,
            luma_8x8_abs_level: initial_contexts(&CABAC_I_LUMA_8X8_ABS_INIT, slice_qp_y)?,
        })
    }
}

struct CabacICodedBlocks {
    patterns: Vec<u8>,
    luma_dc: Vec<bool>,
    chroma_dc: Vec<[bool; 2]>,
    luma_ac: Vec<[bool; 16]>,
    chroma_ac: Vec<[[bool; 4]; 2]>,
}

impl CabacICodedBlocks {
    fn new(macroblock_count: usize) -> Self {
        Self {
            patterns: vec![0; macroblock_count],
            luma_dc: vec![false; macroblock_count],
            chroma_dc: vec![[false; 2]; macroblock_count],
            luma_ac: vec![[false; 16]; macroblock_count],
            chroma_ac: vec![[[false; 4]; 2]; macroblock_count],
        }
    }

    fn mark_pcm(&mut self, address: usize) {
        self.patterns[address] = 47;
        self.luma_dc[address] = true;
        self.chroma_dc[address] = [true; 2];
        self.luma_ac[address].fill(true);
        self.chroma_ac[address] = [[true; 4]; 2];
    }
}

fn decode_cabac_i_macroblocks(
    bits: &mut BitReader<'_>,
    buffer: &mut FrameBuffer,
    luma_qp: &mut i32,
    chroma_qp_offset: i32,
    transform_8x8_mode: bool,
) -> Result<()> {
    let mut contexts = CabacIContexts::new(*luma_qp)?;
    let mut decoder = CabacDecoder::new(bits)?;
    let macroblocks_wide = buffer.coded_width / 16;
    let mut intra16_or_pcm = vec![false; buffer.macroblock_count()];
    let mut chroma_prediction_modes = vec![0_u32; buffer.macroblock_count()];
    let mut coded_blocks = CabacICodedBlocks::new(buffer.macroblock_count());
    let mut previous_qp_delta = 0;
    let mut transform_8x8 = vec![false; buffer.macroblock_count()];
    for address in 0..buffer.macroblock_count() {
        let context_increment =
            usize::from(!address.is_multiple_of(macroblocks_wide) && intra16_or_pcm[address - 1])
                + usize::from(
                    address >= macroblocks_wide && intra16_or_pcm[address - macroblocks_wide],
                );
        let macroblock_type = decode_cabac_i_macroblock_type(
            &mut decoder,
            &mut contexts.macroblock_type,
            context_increment,
        )?;
        match macroblock_type {
            0 => {
                let use_8x8 = transform_8x8_mode
                    && decode_cabac_transform_size_8x8(
                        &mut decoder,
                        &mut contexts.transform_size_8x8,
                        address,
                        macroblocks_wide,
                        &transform_8x8,
                    )?;
                transform_8x8[address] = use_8x8;
                buffer.transform_8x8[address] = use_8x8;
                if use_8x8 {
                    decode_cabac_intra8(
                        &mut decoder,
                        &mut contexts,
                        buffer,
                        address,
                        macroblocks_wide,
                        chroma_qp_offset,
                        &mut previous_qp_delta,
                        luma_qp,
                        &mut chroma_prediction_modes,
                        &mut coded_blocks,
                    )?;
                } else {
                    decode_cabac_intra4(
                        &mut decoder,
                        &mut contexts,
                        buffer,
                        address,
                        macroblocks_wide,
                        chroma_qp_offset,
                        &mut previous_qp_delta,
                        luma_qp,
                        &mut chroma_prediction_modes,
                        &mut coded_blocks,
                    )?;
                }
            }
            1..=24 => {
                decode_cabac_intra16(
                    &mut decoder,
                    &mut contexts,
                    buffer,
                    address,
                    macroblocks_wide,
                    macroblock_type,
                    chroma_qp_offset,
                    &mut previous_qp_delta,
                    luma_qp,
                    &mut chroma_prediction_modes,
                    &mut coded_blocks,
                )?;
                intra16_or_pcm[address] = true;
            }
            25 => {
                let samples = decoder.pcm_samples(384)?;
                buffer.place_pcm_macroblock(address, &samples)?;
                buffer.mark_pcm(address);
                intra16_or_pcm[address] = true;
                coded_blocks.mark_pcm(address);
                previous_qp_delta = 0;
            }
            _ => unreachable!("CABAC I macroblock type is in 0..=25"),
        }

        let end_of_slice = decoder.terminate()?;
        if end_of_slice {
            if address + 1 != buffer.macroblock_count() {
                return Err(Error::InvalidData(
                    "H.264 CABAC I slice ended before the complete picture".into(),
                ));
            }
            return Ok(());
        }
    }
    Err(Error::InvalidData(
        "H.264 CABAC I slice is missing end_of_slice_flag".into(),
    ))
}

fn decode_cabac_i_macroblock_type(
    decoder: &mut CabacDecoder<'_, '_>,
    contexts: &mut [ContextState; 11],
    context_increment: usize,
) -> Result<u32> {
    if !decoder.decision(&mut contexts[3 + context_increment])? {
        return Ok(0);
    }
    if decoder.terminate()? {
        return Ok(25);
    }
    let mut macroblock_type = 1;
    macroblock_type += 12 * u32::from(decoder.decision(&mut contexts[6])?);
    if decoder.decision(&mut contexts[7])? {
        macroblock_type += 4 + 4 * u32::from(decoder.decision(&mut contexts[8])?);
    }
    macroblock_type += 2 * u32::from(decoder.decision(&mut contexts[9])?);
    macroblock_type += u32::from(decoder.decision(&mut contexts[10])?);
    Ok(macroblock_type)
}

fn decode_cabac_transform_size_8x8(
    decoder: &mut CabacDecoder<'_, '_>,
    contexts: &mut [ContextState; 3],
    address: usize,
    macroblocks_wide: usize,
    transform_8x8: &[bool],
) -> Result<bool> {
    let left = !address.is_multiple_of(macroblocks_wide) && transform_8x8[address - 1];
    let top = address >= macroblocks_wide && transform_8x8[address - macroblocks_wide];
    decoder.decision(&mut contexts[usize::from(left) + usize::from(top)])
}

#[allow(clippy::too_many_arguments)]
fn decode_cabac_intra16(
    decoder: &mut CabacDecoder<'_, '_>,
    contexts: &mut CabacIContexts,
    buffer: &mut FrameBuffer,
    address: usize,
    macroblocks_wide: usize,
    macroblock_type: u32,
    chroma_qp_offset: i32,
    previous_qp_delta: &mut i32,
    luma_qp: &mut i32,
    chroma_prediction_modes: &mut [u32],
    coded_blocks: &mut CabacICodedBlocks,
) -> Result<()> {
    let code = macroblock_type - 1;
    let luma_prediction_mode = code % 4;
    let coded_block_pattern_chroma = (code / 4) % 3;
    let coded_block_pattern_luma = u32::from(code >= 12) * 15;
    coded_blocks.patterns[address] =
        u8::try_from(coded_block_pattern_luma | (coded_block_pattern_chroma << 4))
            .expect("CABAC Intra16 coded block pattern fits u8");
    let chroma_prediction_mode = decode_cabac_chroma_prediction_mode(
        decoder,
        &mut contexts.chroma_prediction_mode,
        address,
        macroblocks_wide,
        chroma_prediction_modes,
    )?;
    chroma_prediction_modes[address] = chroma_prediction_mode;
    let qp_delta = decode_cabac_macroblock_qp_delta(
        decoder,
        &mut contexts.macroblock_qp_delta,
        *previous_qp_delta,
    )?;
    *previous_qp_delta = qp_delta;
    *luma_qp = (*luma_qp + qp_delta).rem_euclid(52);
    let (luma_dc_levels, luma_blocks) = decode_cabac_intra16_luma_residuals(
        decoder,
        contexts,
        buffer,
        address,
        macroblocks_wide,
        coded_block_pattern_luma,
        coded_blocks,
    )?;
    let (chroma_dc_levels, chroma_blocks) = decode_cabac_intra_chroma_residuals(
        decoder,
        contexts,
        buffer,
        address,
        macroblocks_wide,
        coded_block_pattern_chroma,
        coded_blocks,
    )?;

    buffer.mark_intra(address);
    buffer.predict_intra16_macroblock(address, luma_prediction_mode, chroma_prediction_mode)?;
    buffer.add_intra16_luma_residual(address, &luma_dc_levels, &luma_blocks, *luma_qp)?;
    buffer.add_chroma_residual(
        address,
        &chroma_dc_levels,
        &chroma_blocks,
        chroma_qp(*luma_qp, chroma_qp_offset),
    )?;
    buffer.set_luma_qp(address, *luma_qp);
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn decode_cabac_intra4(
    decoder: &mut CabacDecoder<'_, '_>,
    contexts: &mut CabacIContexts,
    buffer: &mut FrameBuffer,
    address: usize,
    macroblocks_wide: usize,
    chroma_qp_offset: i32,
    previous_qp_delta: &mut i32,
    luma_qp: &mut i32,
    chroma_prediction_modes: &mut [u32],
    coded_blocks: &mut CabacICodedBlocks,
) -> Result<()> {
    buffer.mark_intra(address);
    let modes = decode_cabac_intra4_prediction_modes(
        decoder,
        &mut contexts.intra4_prediction_mode,
        buffer,
        address,
    )?;
    let chroma_mode = decode_cabac_chroma_prediction_mode(
        decoder,
        &mut contexts.chroma_prediction_mode,
        address,
        macroblocks_wide,
        chroma_prediction_modes,
    )?;
    chroma_prediction_modes[address] = chroma_mode;
    let pattern = decode_cabac_coded_block_pattern(
        decoder,
        &mut contexts.coded_block_pattern_luma,
        &mut contexts.coded_block_pattern_chroma,
        address,
        macroblocks_wide,
        &coded_blocks.patterns,
    )?;
    coded_blocks.patterns[address] = pattern;
    if pattern == 0 {
        *previous_qp_delta = 0;
    } else {
        let qp_delta = decode_cabac_macroblock_qp_delta(
            decoder,
            &mut contexts.macroblock_qp_delta,
            *previous_qp_delta,
        )?;
        *previous_qp_delta = qp_delta;
        *luma_qp = (*luma_qp + qp_delta).rem_euclid(52);
    }

    let luma_blocks = decode_cabac_intra4_luma_residuals(
        decoder,
        contexts,
        buffer,
        address,
        macroblocks_wide,
        pattern & 15,
        coded_blocks,
    )?;
    let (chroma_dc_levels, chroma_blocks) = decode_cabac_intra_chroma_residuals(
        decoder,
        contexts,
        buffer,
        address,
        macroblocks_wide,
        u32::from(pattern >> 4),
        coded_blocks,
    )?;
    for (block_index, (mode, levels)) in modes.iter().zip(&luma_blocks).enumerate() {
        buffer.reconstruct_intra4_luma_block(address, block_index, *mode, levels, *luma_qp)?;
    }
    buffer.predict_chroma_macroblock(address, chroma_mode)?;
    buffer.add_chroma_residual(
        address,
        &chroma_dc_levels,
        &chroma_blocks,
        chroma_qp(*luma_qp, chroma_qp_offset),
    )?;
    buffer.set_luma_qp(address, *luma_qp);
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn decode_cabac_intra8(
    decoder: &mut CabacDecoder<'_, '_>,
    contexts: &mut CabacIContexts,
    buffer: &mut FrameBuffer,
    address: usize,
    macroblocks_wide: usize,
    chroma_qp_offset: i32,
    previous_qp_delta: &mut i32,
    luma_qp: &mut i32,
    chroma_prediction_modes: &mut [u32],
    coded_blocks: &mut CabacICodedBlocks,
) -> Result<()> {
    buffer.mark_intra(address);
    let modes = decode_cabac_intra8_prediction_modes(decoder, contexts, buffer, address)?;
    let chroma_mode = decode_cabac_chroma_prediction_mode(
        decoder,
        &mut contexts.chroma_prediction_mode,
        address,
        macroblocks_wide,
        chroma_prediction_modes,
    )?;
    chroma_prediction_modes[address] = chroma_mode;
    let pattern = decode_cabac_coded_block_pattern(
        decoder,
        &mut contexts.coded_block_pattern_luma,
        &mut contexts.coded_block_pattern_chroma,
        address,
        macroblocks_wide,
        &coded_blocks.patterns,
    )?;
    coded_blocks.patterns[address] = pattern;
    update_cabac_inter_qp(
        decoder,
        &mut contexts.macroblock_qp_delta,
        pattern,
        luma_qp,
        previous_qp_delta,
    )?;

    let mut luma_blocks: [Vec<i32>; 4] = std::array::from_fn(|_| vec![0; 64]);
    for (group, levels) in luma_blocks.iter_mut().enumerate() {
        if pattern & (1 << group) == 0 {
            for block in group * 4..group * 4 + 4 {
                buffer.set_luma_nonzero(address, block, 0);
            }
            continue;
        }
        *levels = decode_cabac_residual_8x8(
            decoder,
            &mut contexts.luma_8x8_significant,
            &mut contexts.luma_8x8_last,
            &mut contexts.luma_8x8_abs_level,
        )?;
        let nonzero = nonzero_coefficient_count(levels);
        for block in group * 4..group * 4 + 4 {
            buffer.set_luma_nonzero(address, block, nonzero);
            coded_blocks.luma_ac[address][block] = nonzero != 0;
        }
    }
    let (chroma_dc_levels, chroma_blocks) = decode_cabac_intra_chroma_residuals(
        decoder,
        contexts,
        buffer,
        address,
        macroblocks_wide,
        u32::from(pattern >> 4),
        coded_blocks,
    )?;
    for group in 0..4 {
        buffer.reconstruct_intra8_luma_block(
            address,
            group,
            modes[group],
            &luma_blocks[group],
            *luma_qp,
        )?;
    }
    buffer.predict_chroma_macroblock(address, chroma_mode)?;
    buffer.add_chroma_residual(
        address,
        &chroma_dc_levels,
        &chroma_blocks,
        chroma_qp(*luma_qp, chroma_qp_offset),
    )?;
    buffer.set_luma_qp(address, *luma_qp);
    Ok(())
}

fn decode_cabac_intra8_prediction_modes(
    decoder: &mut CabacDecoder<'_, '_>,
    contexts: &mut CabacIContexts,
    buffer: &mut FrameBuffer,
    address: usize,
) -> Result<[u8; 4]> {
    decode_cabac_intra8_prediction_modes_with_contexts(
        decoder,
        &mut contexts.intra4_prediction_mode,
        buffer,
        address,
    )
}

fn decode_cabac_intra8_prediction_modes_with_contexts(
    decoder: &mut CabacDecoder<'_, '_>,
    contexts: &mut [ContextState; 2],
    buffer: &mut FrameBuffer,
    address: usize,
) -> Result<[u8; 4]> {
    let mut modes = [0_u8; 4];
    for (group, mode) in modes.iter_mut().enumerate() {
        let base = group * 4;
        let predicted = buffer.predicted_intra4_mode(address, base);
        *mode = if decoder.decision(&mut contexts[0])? {
            predicted
        } else {
            let mut remaining = 0_u8;
            for bit in 0..3 {
                remaining |= u8::from(decoder.decision(&mut contexts[1])?) << bit;
            }
            remaining + u8::from(remaining >= predicted)
        };
        if *mode > 8 {
            return Err(Error::InvalidData(format!(
                "invalid H.264 CABAC Intra8x8 prediction mode {}",
                *mode
            )));
        }
        for block in base..base + 4 {
            buffer.set_intra4_mode(address, block, *mode);
        }
    }
    Ok(modes)
}

fn decode_cabac_intra4_prediction_modes(
    decoder: &mut CabacDecoder<'_, '_>,
    contexts: &mut [ContextState; 2],
    buffer: &mut FrameBuffer,
    address: usize,
) -> Result<[u8; 16]> {
    let mut modes = [0_u8; 16];
    for (block_index, mode) in modes.iter_mut().enumerate() {
        let predicted = buffer.predicted_intra4_mode(address, block_index);
        *mode = if decoder.decision(&mut contexts[0])? {
            predicted
        } else {
            let mut remaining = 0_u8;
            for bit in 0..3 {
                remaining |= u8::from(decoder.decision(&mut contexts[1])?) << bit;
            }
            remaining + u8::from(remaining >= predicted)
        };
        if *mode > 8 {
            return Err(Error::InvalidData(format!(
                "invalid H.264 CABAC Intra4x4 prediction mode {}",
                *mode
            )));
        }
        buffer.set_intra4_mode(address, block_index, *mode);
    }
    Ok(modes)
}

fn decode_cabac_coded_block_pattern(
    decoder: &mut CabacDecoder<'_, '_>,
    luma_contexts: &mut [ContextState; 4],
    chroma_contexts: &mut [ContextState; 8],
    address: usize,
    macroblocks_wide: usize,
    patterns: &[u8],
) -> Result<u8> {
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
    let mut luma = u8::from(decoder.decision(
        &mut luma_contexts[usize::from(left & 0x02 == 0) + 2 * usize::from(top & 0x04 == 0)],
    )?);
    luma |= u8::from(decoder.decision(
        &mut luma_contexts[usize::from(luma & 0x01 == 0) + 2 * usize::from(top & 0x08 == 0)],
    )?) << 1;
    luma |= u8::from(decoder.decision(
        &mut luma_contexts[usize::from(left & 0x08 == 0) + 2 * usize::from(luma & 0x01 == 0)],
    )?) << 2;
    luma |= u8::from(decoder.decision(
        &mut luma_contexts[usize::from(luma & 0x04 == 0) + 2 * usize::from(luma & 0x02 == 0)],
    )?) << 3;

    let left_chroma = left >> 4;
    let top_chroma = top >> 4;
    let first_context = usize::from(left_chroma > 0) + 2 * usize::from(top_chroma > 0);
    if !decoder.decision(&mut chroma_contexts[first_context])? {
        return Ok(luma);
    }
    let second_context = 4 + usize::from(left_chroma == 2) + 2 * usize::from(top_chroma == 2);
    let chroma = 1 + u8::from(decoder.decision(&mut chroma_contexts[second_context])?);
    Ok(luma | (chroma << 4))
}

fn decode_cabac_intra4_luma_residuals(
    decoder: &mut CabacDecoder<'_, '_>,
    contexts: &mut CabacIContexts,
    buffer: &mut FrameBuffer,
    address: usize,
    macroblocks_wide: usize,
    coded_block_pattern: u8,
    coded_blocks: &mut CabacICodedBlocks,
) -> Result<[Vec<i32>; 16]> {
    let mut blocks: [Vec<i32>; 16] = std::array::from_fn(|_| vec![0; 16]);
    for group in 0..4 {
        for within_group in 0..4 {
            let block_index = group * 4 + within_group;
            if coded_block_pattern & (1 << group) == 0 {
                buffer.set_luma_nonzero(address, block_index, 0);
                continue;
            }
            let coded_context = cabac_intra_luma_ac_coded_context(
                address,
                macroblocks_wide,
                block_index,
                &coded_blocks.luma_ac,
            );
            coded_blocks.luma_ac[address][block_index] =
                decoder.decision(&mut contexts.luma_4x4_coded_block[coded_context])?;
            if coded_blocks.luma_ac[address][block_index] {
                blocks[block_index] = decode_cabac_residual_block(
                    decoder,
                    &mut contexts.luma_4x4_significant,
                    &mut contexts.luma_4x4_last,
                    &mut contexts.luma_4x4_abs_level,
                    16,
                )?;
                buffer.set_luma_nonzero(
                    address,
                    block_index,
                    nonzero_coefficient_count(&blocks[block_index]),
                );
            }
        }
    }
    Ok(blocks)
}

fn decode_cabac_intra16_luma_residuals(
    decoder: &mut CabacDecoder<'_, '_>,
    contexts: &mut CabacIContexts,
    buffer: &mut FrameBuffer,
    address: usize,
    macroblocks_wide: usize,
    coded_block_pattern: u32,
    coded_blocks: &mut CabacICodedBlocks,
) -> Result<(Vec<i32>, [Vec<i32>; 16])> {
    let coded_context = cabac_intra_dc_coded_context(address, macroblocks_wide, |neighbor| {
        coded_blocks.luma_dc[neighbor]
    });
    coded_blocks.luma_dc[address] =
        decoder.decision(&mut contexts.luma_dc_coded_block[coded_context])?;
    let dc_levels = if coded_blocks.luma_dc[address] {
        decode_cabac_residual_block(
            decoder,
            &mut contexts.luma_dc_significant,
            &mut contexts.luma_dc_last,
            &mut contexts.luma_dc_abs_level,
            16,
        )?
    } else {
        vec![0; 16]
    };
    let mut blocks: [Vec<i32>; 16] = std::array::from_fn(|_| vec![0; 15]);
    if coded_block_pattern == 0 {
        buffer.mark_zero_ac(address);
        return Ok((dc_levels, blocks));
    }
    for (block_index, levels) in blocks.iter_mut().enumerate() {
        let coded_context = cabac_intra_luma_ac_coded_context(
            address,
            macroblocks_wide,
            block_index,
            &coded_blocks.luma_ac,
        );
        coded_blocks.luma_ac[address][block_index] =
            decoder.decision(&mut contexts.luma_ac_coded_block[coded_context])?;
        if coded_blocks.luma_ac[address][block_index] {
            *levels = decode_cabac_residual_block(
                decoder,
                &mut contexts.luma_ac_significant,
                &mut contexts.luma_ac_last,
                &mut contexts.luma_ac_abs_level,
                15,
            )?;
            buffer.set_luma_nonzero(address, block_index, nonzero_coefficient_count(levels));
        }
    }
    Ok((dc_levels, blocks))
}

fn decode_cabac_intra_chroma_residuals(
    decoder: &mut CabacDecoder<'_, '_>,
    contexts: &mut CabacIContexts,
    buffer: &mut FrameBuffer,
    address: usize,
    macroblocks_wide: usize,
    coded_block_pattern: u32,
    coded_blocks: &mut CabacICodedBlocks,
) -> Result<(ChromaDcLevels, ChromaAcLevels)> {
    let mut dc_levels: ChromaDcLevels = std::array::from_fn(|_| vec![0; 4]);
    if coded_block_pattern != 0 {
        for (component, levels) in dc_levels.iter_mut().enumerate() {
            let coded_context =
                cabac_intra_dc_coded_context(address, macroblocks_wide, |neighbor| {
                    coded_blocks.chroma_dc[neighbor][component]
                });
            coded_blocks.chroma_dc[address][component] =
                decoder.decision(&mut contexts.chroma_dc_coded_block[coded_context])?;
            if coded_blocks.chroma_dc[address][component] {
                *levels = decode_cabac_residual_block(
                    decoder,
                    &mut contexts.chroma_dc_significant,
                    &mut contexts.chroma_dc_last,
                    &mut contexts.chroma_dc_abs_level,
                    4,
                )?;
            }
        }
    }
    let mut blocks: ChromaAcLevels = std::array::from_fn(|_| std::array::from_fn(|_| vec![0; 15]));
    if coded_block_pattern != 2 {
        buffer.mark_zero_chroma_ac(address);
        return Ok((dc_levels, blocks));
    }
    for (component, component_blocks) in blocks.iter_mut().enumerate() {
        for (block_index, levels) in component_blocks.iter_mut().enumerate() {
            let coded_context = cabac_intra_chroma_ac_coded_context(
                address,
                macroblocks_wide,
                component,
                block_index,
                &coded_blocks.chroma_ac,
            );
            coded_blocks.chroma_ac[address][component][block_index] =
                decoder.decision(&mut contexts.chroma_ac_coded_block[coded_context])?;
            if coded_blocks.chroma_ac[address][component][block_index] {
                *levels = decode_cabac_residual_block(
                    decoder,
                    &mut contexts.chroma_ac_significant,
                    &mut contexts.chroma_ac_last,
                    &mut contexts.chroma_ac_abs_level,
                    15,
                )?;
                buffer.set_chroma_nonzero(
                    address,
                    component,
                    block_index,
                    nonzero_coefficient_count(levels),
                );
            }
        }
    }
    Ok((dc_levels, blocks))
}

fn nonzero_coefficient_count(levels: &[i32]) -> u8 {
    u8::try_from(levels.iter().filter(|&&level| level != 0).count())
        .expect("a transform block has at most 64 coefficients")
}

fn cabac_intra_dc_coded_context(
    address: usize,
    macroblocks_wide: usize,
    neighbor_is_coded: impl Fn(usize) -> bool,
) -> usize {
    // For coded_block_flag in an intra macroblock, an unavailable neighboring
    // transform block contributes a condition flag of one (H.264 9.3.3.1.1.9).
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

fn decode_cabac_residual_block(
    decoder: &mut CabacDecoder<'_, '_>,
    significant_contexts: &mut [ContextState],
    last_contexts: &mut [ContextState],
    absolute_level_contexts: &mut [ContextState],
    coefficient_count: usize,
) -> Result<Vec<i32>> {
    const LEVEL_ONE_CONTEXT: [usize; 8] = [1, 2, 3, 4, 0, 0, 0, 0];
    const LEVEL_GT_ONE_CONTEXT: [usize; 8] = [5, 5, 5, 5, 6, 7, 8, 9];
    const AFTER_LEVEL_ONE: [usize; 8] = [1, 2, 3, 3, 4, 5, 6, 7];
    const AFTER_LEVEL_GT_ONE: [usize; 8] = [4, 4, 4, 4, 5, 6, 7, 7];

    let mut significant_positions = Vec::with_capacity(coefficient_count);
    let mut explicit_last = false;
    for position in 0..coefficient_count - 1 {
        let significant = decoder.decision(&mut significant_contexts[position])?;
        if significant {
            significant_positions.push(position);
            let last = decoder.decision(&mut last_contexts[position])?;
            if last {
                explicit_last = true;
                break;
            }
        }
    }
    if !explicit_last {
        significant_positions.push(coefficient_count - 1);
    }

    let mut coefficients = vec![0_i32; coefficient_count];
    let mut node_context = 0;
    for &position in significant_positions.iter().rev() {
        let absolute_level = if decoder
            .decision(&mut absolute_level_contexts[LEVEL_ONE_CONTEXT[node_context]])?
        {
            let context_index = LEVEL_GT_ONE_CONTEXT[node_context];
            node_context = AFTER_LEVEL_GT_ONE[node_context];
            let mut absolute_level = 2_u32;
            while absolute_level < 15
                && decoder.decision(&mut absolute_level_contexts[context_index])?
            {
                absolute_level += 1;
            }
            if absolute_level >= 15 {
                let mut prefix = 0_u8;
                while decoder.bypass()? {
                    prefix = prefix
                        .checked_add(1)
                        .filter(|&value| value <= 23)
                        .ok_or_else(|| {
                            Error::InvalidData("H.264 CABAC coefficient prefix is too long".into())
                        })?;
                }
                let mut suffix = 0_u32;
                for _ in 0..prefix {
                    suffix = (suffix << 1) | u32::from(decoder.bypass()?);
                }
                absolute_level = 14 + (1_u32 << prefix) + suffix;
            }
            absolute_level
        } else {
            node_context = AFTER_LEVEL_ONE[node_context];
            1_u32
        };
        let absolute_level = i32::try_from(absolute_level)
            .map_err(|_| Error::InvalidData("H.264 CABAC coefficient level overflows".into()))?;
        coefficients[position] = if decoder.bypass()? {
            -absolute_level
        } else {
            absolute_level
        };
    }
    Ok(coefficients)
}

fn decode_cabac_residual_8x8(
    decoder: &mut CabacDecoder<'_, '_>,
    significant_contexts: &mut [ContextState; 15],
    last_contexts: &mut [ContextState; 9],
    absolute_level_contexts: &mut [ContextState; 10],
) -> Result<Vec<i32>> {
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
    const LEVEL_ONE_CONTEXT: [usize; 8] = [1, 2, 3, 4, 0, 0, 0, 0];
    const LEVEL_GT_ONE_CONTEXT: [usize; 8] = [5, 5, 5, 5, 6, 7, 8, 9];
    const AFTER_LEVEL_ONE: [usize; 8] = [1, 2, 3, 3, 4, 5, 6, 7];
    const AFTER_LEVEL_GT_ONE: [usize; 8] = [4, 4, 4, 4, 5, 6, 7, 7];

    let mut significant_positions = Vec::with_capacity(64);
    let mut explicit_last = false;
    for position in 0..63 {
        if decoder.decision(&mut significant_contexts[SIGNIFICANT_CONTEXT[position]])? {
            significant_positions.push(position);
            if decoder.decision(&mut last_contexts[LAST_CONTEXT[position]])? {
                explicit_last = true;
                break;
            }
        }
    }
    if !explicit_last {
        significant_positions.push(63);
    }

    let mut coefficients = vec![0_i32; 64];
    let mut node_context = 0;
    for &position in significant_positions.iter().rev() {
        let absolute_level = if decoder
            .decision(&mut absolute_level_contexts[LEVEL_ONE_CONTEXT[node_context]])?
        {
            let context_index = LEVEL_GT_ONE_CONTEXT[node_context];
            node_context = AFTER_LEVEL_GT_ONE[node_context];
            let mut absolute_level = 2_u32;
            while absolute_level < 15
                && decoder.decision(&mut absolute_level_contexts[context_index])?
            {
                absolute_level += 1;
            }
            if absolute_level >= 15 {
                let mut prefix = 0_u8;
                while decoder.bypass()? {
                    prefix = prefix
                        .checked_add(1)
                        .filter(|&value| value <= 23)
                        .ok_or_else(|| {
                            Error::InvalidData("H.264 CABAC coefficient prefix is too long".into())
                        })?;
                }
                let mut suffix = 0_u32;
                for _ in 0..prefix {
                    suffix = (suffix << 1) | u32::from(decoder.bypass()?);
                }
                absolute_level = 14 + (1_u32 << prefix) + suffix;
            }
            absolute_level
        } else {
            node_context = AFTER_LEVEL_ONE[node_context];
            1
        };
        let absolute_level = i32::try_from(absolute_level)
            .map_err(|_| Error::InvalidData("H.264 CABAC coefficient level overflows".into()))?;
        coefficients[position] = if decoder.bypass()? {
            -absolute_level
        } else {
            absolute_level
        };
    }
    Ok(coefficients)
}

fn decode_cabac_chroma_prediction_mode(
    decoder: &mut CabacDecoder<'_, '_>,
    contexts: &mut [ContextState; 4],
    address: usize,
    macroblocks_wide: usize,
    modes: &[u32],
) -> Result<u32> {
    let context_increment =
        usize::from(!address.is_multiple_of(macroblocks_wide) && modes[address - 1] != 0)
            + usize::from(address >= macroblocks_wide && modes[address - macroblocks_wide] != 0);
    if !decoder.decision(&mut contexts[context_increment])? {
        return Ok(0);
    }
    if !decoder.decision(&mut contexts[3])? {
        return Ok(1);
    }
    Ok(if decoder.decision(&mut contexts[3])? {
        3
    } else {
        2
    })
}

fn decode_cabac_macroblock_qp_delta(
    decoder: &mut CabacDecoder<'_, '_>,
    contexts: &mut [ContextState; 4],
    previous_delta: i32,
) -> Result<i32> {
    if !decoder.decision(&mut contexts[usize::from(previous_delta != 0)])? {
        return Ok(0);
    }
    let mut code = 1_u32;
    let mut context = 2;
    while decoder.decision(&mut contexts[context])? {
        context = 3;
        code = code
            .checked_add(1)
            .filter(|&value| value <= 102)
            .ok_or_else(|| Error::InvalidData("H.264 CABAC mb_qp_delta is too large".into()))?;
    }
    let magnitude = i32::try_from(code.div_ceil(2)).expect("bounded CABAC QP delta fits i32");
    Ok(if code.is_multiple_of(2) {
        -magnitude
    } else {
        magnitude
    })
}

#[allow(clippy::too_many_arguments)]
fn decode_p_l0_macroblock(
    reader: &mut SyntaxReader<'_>,
    buffer: &mut FrameBuffer,
    reference: &ReferenceFrame,
    macroblock_address: usize,
    macroblock_type: u32,
    luma_qp: &mut i32,
    chroma_qp_offset: i32,
    prediction_weights: PredictionWeights,
) -> Result<()> {
    let partitions = read_inter_partitions(reader, macroblock_type)?;
    for partition in partitions {
        let predictor = buffer.partition_motion_vector_predictor(
            macroblock_address,
            partition.block_x,
            partition.block_y,
            partition.block_width,
            partition.prediction_kind,
        );
        let vector = MotionVector {
            x: predictor.x.checked_add(reader.se()?).ok_or_else(|| {
                Error::InvalidData("H.264 horizontal motion vector overflows".into())
            })?,
            y: predictor.y.checked_add(reader.se()?).ok_or_else(|| {
                Error::InvalidData("H.264 vertical motion vector overflows".into())
            })?,
        };
        buffer.predict_inter_partition(
            reference,
            macroblock_address,
            partition.block_x,
            partition.block_y,
            partition.block_width,
            partition.block_height,
            vector,
            prediction_weights,
        )?;
        buffer.set_partition_motion(
            macroblock_address,
            partition.block_x,
            partition.block_y,
            partition.block_width,
            partition.block_height,
            vector,
        );
    }

    let pattern_code = usize::try_from(reader.ue()?)
        .ok()
        .filter(|&code| code < INTER_CODED_BLOCK_PATTERN.len())
        .ok_or_else(|| Error::InvalidData("invalid H.264 inter coded block pattern".into()))?;
    let pattern = INTER_CODED_BLOCK_PATTERN[pattern_code];
    let luma_pattern = pattern & 15;
    let chroma_pattern = pattern >> 4;
    if pattern != 0 {
        *luma_qp = (*luma_qp + reader.se()?).rem_euclid(52);
    }
    let mut luma_blocks: [Vec<i32>; 16] = std::array::from_fn(|_| vec![0; 16]);
    for group in 0..4 {
        for within_group in 0..4 {
            let block_index = group * 4 + within_group;
            if luma_pattern & (1 << group) != 0 {
                let n_c = buffer.luma_nc(macroblock_address, block_index);
                let decoded = decode_residual_block(&mut reader.bits, n_c, 16)?;
                luma_blocks[block_index] = decoded.coefficients;
                buffer.set_luma_nonzero(macroblock_address, block_index, decoded.total_coeff);
            } else {
                buffer.set_luma_nonzero(macroblock_address, block_index, 0);
            }
        }
    }
    let (chroma_dc, chroma_blocks) =
        decode_chroma_blocks(reader, buffer, macroblock_address, chroma_pattern)?;
    buffer.add_luma_residual_blocks(macroblock_address, &luma_blocks, *luma_qp)?;
    buffer.add_chroma_residual(
        macroblock_address,
        &chroma_dc,
        &chroma_blocks,
        chroma_qp(*luma_qp, chroma_qp_offset),
    )?;
    buffer.set_luma_qp(macroblock_address, *luma_qp);
    Ok(())
}

fn read_inter_partitions(
    reader: &mut SyntaxReader<'_>,
    macroblock_type: u32,
) -> Result<Vec<InterPartition>> {
    Ok(match macroblock_type {
        0 => vec![InterPartition::new(
            0,
            0,
            4,
            4,
            MotionPredictionKind::Normal,
        )],
        1 => vec![
            InterPartition::new(0, 0, 4, 2, MotionPredictionKind::Top16x8),
            InterPartition::new(0, 2, 4, 2, MotionPredictionKind::Bottom16x8),
        ],
        2 => vec![
            InterPartition::new(0, 0, 2, 4, MotionPredictionKind::Left8x16),
            InterPartition::new(2, 0, 2, 4, MotionPredictionKind::Right8x16),
        ],
        3 | 4 => read_p8x8_partitions(reader)?,
        _ => unreachable!("P_L0 macroblock type is in 0..=4"),
    })
}

fn read_p8x8_partitions(reader: &mut SyntaxReader<'_>) -> Result<Vec<InterPartition>> {
    let mut sub_types = [0_u32; 4];
    for sub_type in &mut sub_types {
        *sub_type = reader.ue()?;
        if *sub_type > 3 {
            return Err(Error::InvalidData(format!(
                "invalid H.264 P sub-macroblock type {sub_type}"
            )));
        }
    }
    let mut partitions = Vec::with_capacity(16);
    for (sub_index, sub_type) in sub_types.into_iter().enumerate() {
        let base_x = (sub_index % 2) * 2;
        let base_y = (sub_index / 2) * 2;
        match sub_type {
            0 => partitions.push(InterPartition::new(
                base_x,
                base_y,
                2,
                2,
                MotionPredictionKind::Normal,
            )),
            1 => {
                partitions.push(InterPartition::new(
                    base_x,
                    base_y,
                    2,
                    1,
                    MotionPredictionKind::Normal,
                ));
                partitions.push(InterPartition::new(
                    base_x,
                    base_y + 1,
                    2,
                    1,
                    MotionPredictionKind::Normal,
                ));
            }
            2 => {
                partitions.push(InterPartition::new(
                    base_x,
                    base_y,
                    1,
                    2,
                    MotionPredictionKind::Normal,
                ));
                partitions.push(InterPartition::new(
                    base_x + 1,
                    base_y,
                    1,
                    2,
                    MotionPredictionKind::Normal,
                ));
            }
            3 => {
                for y in 0..2 {
                    for x in 0..2 {
                        partitions.push(InterPartition::new(
                            base_x + x,
                            base_y + y,
                            1,
                            1,
                            MotionPredictionKind::Normal,
                        ));
                    }
                }
            }
            _ => unreachable!("validated P sub-macroblock type"),
        }
    }
    Ok(partitions)
}

fn read_picture_order_count(reader: &mut SyntaxReader<'_>, sps: &Sps, pps: &Pps) -> Result<()> {
    match sps.pic_order_cnt_type {
        PictureOrderCountType::Type0 {
            log2_max_pic_order_cnt_lsb,
        } => {
            let _pic_order_cnt_lsb = reader.bits(log2_max_pic_order_cnt_lsb)?;
            if pps.bottom_field_pic_order_in_frame_present {
                let _delta_pic_order_cnt_bottom = reader.se()?;
            }
        }
        PictureOrderCountType::Type1 {
            delta_pic_order_always_zero,
        } if !delta_pic_order_always_zero => {
            let _delta_pic_order_cnt0 = reader.se()?;
            if pps.bottom_field_pic_order_in_frame_present {
                let _delta_pic_order_cnt1 = reader.se()?;
            }
        }
        PictureOrderCountType::Type1 { .. } | PictureOrderCountType::Type2 => {}
    }
    Ok(())
}

#[derive(Clone, Debug)]
struct ReferenceFrame {
    _frame_num: u32,
    coded_width: usize,
    coded_height: usize,
    luma: Vec<u8>,
    cb: Vec<u8>,
    cr: Vec<u8>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct MotionVector {
    x: i32,
    y: i32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct PredictionWeight {
    denominator: u8,
    weight: i32,
    offset: i32,
}

impl PredictionWeight {
    const fn identity() -> Self {
        Self {
            denominator: 0,
            weight: 1,
            offset: 0,
        }
    }

    const fn default_for_denominator(denominator: u8) -> Self {
        Self {
            denominator,
            weight: 1_i32 << denominator,
            offset: 0,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct PredictionWeights {
    luma: PredictionWeight,
    cb: PredictionWeight,
    cr: PredictionWeight,
}

impl PredictionWeights {
    const fn identity() -> Self {
        Self {
            luma: PredictionWeight::identity(),
            cb: PredictionWeight::identity(),
            cr: PredictionWeight::identity(),
        }
    }
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

impl InterPartition {
    const fn new(
        block_x: usize,
        block_y: usize,
        block_width: usize,
        block_height: usize,
        prediction_kind: MotionPredictionKind,
    ) -> Self {
        Self {
            block_x,
            block_y,
            block_width,
            block_height,
            prediction_kind,
        }
    }
}

fn read_prediction_weights(reader: &mut SyntaxReader<'_>) -> Result<PredictionWeights> {
    let luma_denominator = read_weight_denominator(reader, "luma")?;
    let chroma_denominator = read_weight_denominator(reader, "chroma")?;
    let mut weights = PredictionWeights {
        luma: PredictionWeight::default_for_denominator(luma_denominator),
        cb: PredictionWeight::default_for_denominator(chroma_denominator),
        cr: PredictionWeight::default_for_denominator(chroma_denominator),
    };
    if reader.bit()? {
        weights.luma = read_prediction_weight(reader, luma_denominator, "luma")?;
    }
    if reader.bit()? {
        weights.cb = read_prediction_weight(reader, chroma_denominator, "Cb")?;
        weights.cr = read_prediction_weight(reader, chroma_denominator, "Cr")?;
    }
    Ok(weights)
}

fn read_weight_denominator(reader: &mut SyntaxReader<'_>, component: &str) -> Result<u8> {
    u8::try_from(reader.ue()?)
        .ok()
        .filter(|&value| value <= 7)
        .ok_or_else(|| {
            Error::InvalidData(format!(
                "H.264 {component} prediction weight denominator exceeds 7"
            ))
        })
}

fn read_prediction_weight(
    reader: &mut SyntaxReader<'_>,
    denominator: u8,
    component: &str,
) -> Result<PredictionWeight> {
    let weight = reader.se()?;
    let offset = reader.se()?;
    if !(-128..=127).contains(&weight) || !(-128..=127).contains(&offset) {
        return Err(Error::InvalidData(format!(
            "H.264 {component} prediction weight or offset is out of range"
        )));
    }
    Ok(PredictionWeight {
        denominator,
        weight,
        offset,
    })
}

fn read_deblocking_parameters(
    reader: &mut SyntaxReader<'_>,
    pps: &Pps,
) -> Result<Option<DeblockingParameters>> {
    if !pps.deblocking_filter_control_present {
        return Ok(Some(DeblockingParameters {
            offset_a: 0,
            offset_b: 0,
        }));
    }
    let disable = reader.ue()?;
    if disable > 2 {
        return Err(Error::InvalidData(format!(
            "invalid H.264 disable_deblocking_filter_idc {disable}"
        )));
    }
    if disable == 1 {
        return Ok(None);
    }
    let alpha_div2 = reader.se()?;
    let beta_div2 = reader.se()?;
    if !(-6..=6).contains(&alpha_div2) || !(-6..=6).contains(&beta_div2) {
        return Err(Error::InvalidData(
            "H.264 deblocking slice offsets must be in -6..=6".into(),
        ));
    }
    Ok(Some(DeblockingParameters {
        offset_a: alpha_div2 * 2,
        offset_b: beta_div2 * 2,
    }))
}

fn decode_idr_macroblocks(
    reader: &mut SyntaxReader<'_>,
    buffer: &mut FrameBuffer,
    pps: &Pps,
    luma_qp: &mut i32,
) -> Result<()> {
    for macroblock_address in 0..buffer.macroblock_count() {
        let macroblock_type = reader.ue()?;
        match macroblock_type {
            0 => decode_intra4x4(
                reader,
                buffer,
                macroblock_address,
                luma_qp,
                pps.chroma_qp_index_offset,
            )?,
            25 => {
                reader.align_zero_to_byte()?;
                buffer.read_macroblock(reader, macroblock_address)?;
                buffer.mark_pcm(macroblock_address);
            }
            1..=24 => decode_intra16x16(
                reader,
                buffer,
                macroblock_address,
                macroblock_type,
                luma_qp,
                pps.chroma_qp_index_offset,
            )?,
            _ => {
                return Err(Error::Unsupported(format!(
                    "native H.264 reconstruction does not support I-slice macroblock type {macroblock_type} at macroblock {macroblock_address}"
                )));
            }
        }
    }
    Ok(())
}

fn require_reference_idr(unit: &NalUnit<'_>) -> Result<()> {
    if unit.header.unit_type != NalUnitType::IdrSlice || unit.header.reference_idc == 0 {
        return Err(Error::Unsupported(
            "native H.264 reconstruction currently begins at reference IDR pictures".into(),
        ));
    }
    Ok(())
}

const INTRA4_CODED_BLOCK_PATTERN: [u8; 48] = [
    47, 31, 15, 0, 23, 27, 29, 30, 7, 11, 13, 14, 39, 43, 45, 46, 16, 3, 5, 10, 12, 19, 21, 26, 28,
    35, 37, 42, 44, 1, 2, 4, 8, 17, 18, 20, 24, 6, 9, 22, 25, 32, 33, 34, 36, 40, 38, 41,
];

const INTER_CODED_BLOCK_PATTERN: [u8; 48] = [
    0, 16, 1, 2, 4, 8, 32, 3, 5, 10, 12, 15, 47, 7, 11, 13, 14, 6, 9, 31, 35, 37, 42, 44, 33, 34,
    36, 40, 39, 43, 45, 46, 17, 18, 20, 24, 19, 21, 26, 28, 23, 27, 29, 30, 22, 25, 38, 41,
];

fn decode_intra4x4(
    reader: &mut SyntaxReader<'_>,
    buffer: &mut FrameBuffer,
    macroblock_address: usize,
    luma_qp: &mut i32,
    chroma_qp_offset: i32,
) -> Result<()> {
    buffer.mark_intra(macroblock_address);
    let mut modes = [0_u8; 16];
    for (block_index, mode) in modes.iter_mut().enumerate() {
        let predicted = buffer.predicted_intra4_mode(macroblock_address, block_index);
        *mode = if reader.bit()? {
            predicted
        } else {
            let remaining = u8::try_from(reader.bits(3)?)
                .map_err(|_| Error::InvalidData("H.264 Intra4x4 mode overflows".into()))?;
            remaining + u8::from(remaining >= predicted)
        };
        if *mode > 8 {
            return Err(Error::InvalidData(format!(
                "invalid H.264 Intra4x4 prediction mode {}",
                *mode
            )));
        }
        buffer.set_intra4_mode(macroblock_address, block_index, *mode);
    }
    let chroma_mode = reader.ue()?;
    if chroma_mode > 3 {
        return Err(Error::InvalidData(format!(
            "invalid H.264 intra_chroma_pred_mode {chroma_mode}"
        )));
    }
    let pattern_code = usize::try_from(reader.ue()?)
        .ok()
        .filter(|&code| code < INTRA4_CODED_BLOCK_PATTERN.len())
        .ok_or_else(|| Error::InvalidData("invalid H.264 Intra4x4 coded block pattern".into()))?;
    let pattern = INTRA4_CODED_BLOCK_PATTERN[pattern_code];
    let luma_pattern = pattern & 15;
    let chroma_pattern = pattern >> 4;
    if pattern != 0 {
        *luma_qp = (*luma_qp + reader.se()?).rem_euclid(52);
    }

    let mut luma_blocks: [Vec<i32>; 16] = std::array::from_fn(|_| vec![0; 16]);
    for group in 0..4 {
        for within_group in 0..4 {
            let block_index = group * 4 + within_group;
            if luma_pattern & (1 << group) != 0 {
                let n_c = buffer.luma_nc(macroblock_address, block_index);
                let decoded = decode_residual_block(&mut reader.bits, n_c, 16)?;
                luma_blocks[block_index] = decoded.coefficients;
                buffer.set_luma_nonzero(macroblock_address, block_index, decoded.total_coeff);
            } else {
                buffer.set_luma_nonzero(macroblock_address, block_index, 0);
            }
        }
    }

    let (chroma_dc, chroma_blocks) =
        decode_chroma_blocks(reader, buffer, macroblock_address, chroma_pattern)?;
    for block_index in 0..16 {
        buffer.reconstruct_intra4_luma_block(
            macroblock_address,
            block_index,
            modes[block_index],
            &luma_blocks[block_index],
            *luma_qp,
        )?;
    }
    buffer.predict_chroma_macroblock(macroblock_address, chroma_mode)?;
    buffer.add_chroma_residual(
        macroblock_address,
        &chroma_dc,
        &chroma_blocks,
        chroma_qp(*luma_qp, chroma_qp_offset),
    )?;
    buffer.set_luma_qp(macroblock_address, *luma_qp);
    Ok(())
}

fn decode_chroma_blocks(
    reader: &mut SyntaxReader<'_>,
    buffer: &mut FrameBuffer,
    macroblock_address: usize,
    coded_block_pattern: u8,
) -> Result<(ChromaDcLevels, ChromaAcLevels)> {
    let mut dc: ChromaDcLevels = std::array::from_fn(|_| vec![0; 4]);
    if coded_block_pattern != 0 {
        for levels in &mut dc {
            *levels = decode_residual_block(&mut reader.bits, -1, 4)?.coefficients;
        }
    }
    let mut blocks: ChromaAcLevels = std::array::from_fn(|_| std::array::from_fn(|_| vec![0; 15]));
    if coded_block_pattern == 2 {
        for (component, component_blocks) in blocks.iter_mut().enumerate() {
            for (block_index, levels) in component_blocks.iter_mut().enumerate() {
                let n_c = buffer.chroma_nc(macroblock_address, component, block_index);
                let decoded = decode_residual_block(&mut reader.bits, n_c, 15)?;
                *levels = decoded.coefficients;
                buffer.set_chroma_nonzero(
                    macroblock_address,
                    component,
                    block_index,
                    decoded.total_coeff,
                );
            }
        }
    } else {
        buffer.mark_zero_chroma_ac(macroblock_address);
    }
    Ok((dc, blocks))
}

fn decode_intra16x16(
    reader: &mut SyntaxReader<'_>,
    buffer: &mut FrameBuffer,
    macroblock_address: usize,
    macroblock_type: u32,
    luma_qp: &mut i32,
    chroma_qp_offset: i32,
) -> Result<()> {
    buffer.mark_intra(macroblock_address);
    let intra16x16_pred_mode = (macroblock_type - 1) % 4;
    let coded_block_pattern_chroma = ((macroblock_type - 1) / 4) % 3;
    let coded_block_pattern_luma = if macroblock_type >= 13 { 15 } else { 0 };
    let intra_chroma_pred_mode = reader.ue()?;
    if intra_chroma_pred_mode > 3 {
        return Err(Error::InvalidData(format!(
            "invalid H.264 intra_chroma_pred_mode {intra_chroma_pred_mode}"
        )));
    }
    let mb_qp_delta = reader.se()?;
    *luma_qp = (*luma_qp + mb_qp_delta).rem_euclid(52);
    let n_c = buffer.intra16_dc_nc(macroblock_address);
    let dc_levels = decode_residual_block(&mut reader.bits, n_c, 16)?;
    let mut ac_levels: [Vec<i32>; 16] = std::array::from_fn(|_| vec![0; 15]);
    if coded_block_pattern_luma != 0 {
        for (block_index, levels) in ac_levels.iter_mut().enumerate() {
            let n_c = buffer.luma_nc(macroblock_address, block_index);
            let decoded = decode_residual_block(&mut reader.bits, n_c, 15)?;
            *levels = decoded.coefficients;
            buffer.set_luma_nonzero(macroblock_address, block_index, decoded.total_coeff);
        }
    } else {
        buffer.mark_zero_ac(macroblock_address);
    }
    let mut chroma_dc: ChromaDcLevels = std::array::from_fn(|_| vec![0; 4]);
    if coded_block_pattern_chroma != 0 {
        for levels in &mut chroma_dc {
            let decoded = decode_residual_block(&mut reader.bits, -1, 4)?;
            *levels = decoded.coefficients;
        }
    }
    let mut chroma_blocks: ChromaAcLevels =
        std::array::from_fn(|_| std::array::from_fn(|_| vec![0; 15]));
    if coded_block_pattern_chroma == 2 {
        for (component, blocks) in chroma_blocks.iter_mut().enumerate() {
            for (block_index, levels) in blocks.iter_mut().enumerate() {
                let n_c = buffer.chroma_nc(macroblock_address, component, block_index);
                let decoded = decode_residual_block(&mut reader.bits, n_c, 15)?;
                *levels = decoded.coefficients;
                buffer.set_chroma_nonzero(
                    macroblock_address,
                    component,
                    block_index,
                    decoded.total_coeff,
                );
            }
        }
    } else {
        buffer.mark_zero_chroma_ac(macroblock_address);
    }
    buffer.predict_intra16_macroblock(
        macroblock_address,
        intra16x16_pred_mode,
        intra_chroma_pred_mode,
    )?;
    buffer.add_intra16_luma_residual(
        macroblock_address,
        &dc_levels.coefficients,
        &ac_levels,
        *luma_qp,
    )?;
    buffer.add_chroma_residual(
        macroblock_address,
        &chroma_dc,
        &chroma_blocks,
        chroma_qp(*luma_qp, chroma_qp_offset),
    )?;
    buffer.set_luma_qp(macroblock_address, *luma_qp);
    Ok(())
}

fn validate_native_profile(sps: &Sps, pps: &Pps) -> Result<()> {
    if sps.chroma_format_idc != 1
        || sps.separate_colour_plane
        || sps.bit_depth_luma != 8
        || sps.bit_depth_chroma != 8
    {
        return Err(Error::Unsupported(
            "native H.264 reconstruction currently supports 8-bit 4:2:0 video only".into(),
        ));
    }
    if !sps.frame_mbs_only || sps.mb_adaptive_frame_field {
        return Err(Error::Unsupported(
            "native H.264 reconstruction currently supports frame-coded pictures only".into(),
        ));
    }
    if pps.num_slice_groups_minus1 != 0 {
        return Err(Error::Unsupported(
            "native H.264 slice groups are not implemented".into(),
        ));
    }
    Ok(())
}

struct FrameBuffer {
    coded_width: usize,
    coded_height: usize,
    luma: Vec<u8>,
    cb: Vec<u8>,
    cr: Vec<u8>,
    luma_nonzero: Vec<[u8; 16]>,
    chroma_nonzero: Vec<[[u8; 4]; 2]>,
    luma_intra_modes: Vec<[u8; 16]>,
    luma_qp: Vec<i32>,
    motion: Vec<[MotionInfo; 16]>,
    motion_available: Vec<[bool; 16]>,
    macroblock_intra: Vec<bool>,
    transform_8x8: Vec<bool>,
    transform_bypass_at_qp_zero: bool,
    chroma_intra_modes: Vec<Option<u8>>,
    scaling_matrices: ScalingMatrices,
}

impl FrameBuffer {
    fn new(sps: &Sps, pps: &Pps) -> Result<Self> {
        let coded_width = usize::try_from(sps.coded_width)
            .map_err(|_| Error::InvalidData("H.264 coded width overflows".into()))?;
        let coded_height = usize::try_from(sps.coded_height)
            .map_err(|_| Error::InvalidData("H.264 coded height overflows".into()))?;
        let luma_len = coded_width
            .checked_mul(coded_height)
            .ok_or_else(|| Error::InvalidData("H.264 luma plane size overflows".into()))?;
        let chroma_len = (coded_width / 2)
            .checked_mul(coded_height / 2)
            .ok_or_else(|| Error::InvalidData("H.264 chroma plane size overflows".into()))?;
        let macroblock_count = (coded_width / 16)
            .checked_mul(coded_height / 16)
            .ok_or_else(|| Error::InvalidData("H.264 macroblock count overflows".into()))?;
        Ok(Self {
            coded_width,
            coded_height,
            luma: vec![0; luma_len],
            cb: vec![0; chroma_len],
            cr: vec![0; chroma_len],
            luma_nonzero: vec![[0; 16]; macroblock_count],
            chroma_nonzero: vec![[[0; 4]; 2]; macroblock_count],
            luma_intra_modes: vec![[2; 16]; macroblock_count],
            luma_qp: vec![26; macroblock_count],
            motion: vec![[MotionInfo::default(); 16]; macroblock_count],
            motion_available: vec![[false; 16]; macroblock_count],
            macroblock_intra: vec![false; macroblock_count],
            transform_8x8: vec![false; macroblock_count],
            transform_bypass_at_qp_zero: sps.qpprime_y_zero_transform_bypass,
            chroma_intra_modes: vec![None; macroblock_count],
            scaling_matrices: pps.resolve_scaling_matrices(sps),
        })
    }

    fn from_reference(
        sps: &Sps,
        pps: &Pps,
        reference: &ReferenceFrame,
        luma_qp: i32,
    ) -> Result<Self> {
        let mut buffer = Self::new(sps, pps)?;
        if reference.coded_width != buffer.coded_width
            || reference.coded_height != buffer.coded_height
            || reference.luma.len() != buffer.luma.len()
            || reference.cb.len() != buffer.cb.len()
            || reference.cr.len() != buffer.cr.len()
        {
            return Err(Error::Unsupported(
                "native H.264 reference-picture resolution changes are not implemented yet".into(),
            ));
        }
        buffer.luma.copy_from_slice(&reference.luma);
        buffer.cb.copy_from_slice(&reference.cb);
        buffer.cr.copy_from_slice(&reference.cr);
        buffer.luma_qp.fill(luma_qp);
        Ok(buffer)
    }

    const fn macroblock_count(&self) -> usize {
        (self.coded_width / 16) * (self.coded_height / 16)
    }

    fn read_macroblock(&mut self, reader: &mut SyntaxReader<'_>, address: usize) -> Result<()> {
        let mut samples = [0_u8; 384];
        for sample in &mut samples {
            *sample = reader.sample()?;
        }
        self.place_pcm_macroblock(address, &samples)
    }

    fn place_pcm_macroblock(&mut self, address: usize, samples: &[u8]) -> Result<()> {
        if samples.len() != 384 {
            return Err(Error::InvalidData(
                "H.264 8-bit 4:2:0 I_PCM macroblock does not contain 384 samples".into(),
            ));
        }
        let macroblocks_wide = self.coded_width / 16;
        let macroblock_x = address % macroblocks_wide;
        let macroblock_y = address / macroblocks_wide;
        for y in 0..16 {
            let destination = (macroblock_y * 16 + y) * self.coded_width + macroblock_x * 16;
            self.luma[destination..destination + 16]
                .copy_from_slice(&samples[y * 16..(y + 1) * 16]);
        }
        let chroma_stride = self.coded_width / 2;
        for (component, plane) in [&mut self.cb, &mut self.cr].into_iter().enumerate() {
            let component_start = 256 + component * 64;
            for y in 0..8 {
                let destination = (macroblock_y * 8 + y) * chroma_stride + macroblock_x * 8;
                let source = component_start + y * 8;
                plane[destination..destination + 8].copy_from_slice(&samples[source..source + 8]);
            }
        }
        Ok(())
    }

    fn mark_pcm(&mut self, address: usize) {
        self.luma_nonzero[address].fill(16);
        self.chroma_nonzero[address] = [[16; 4]; 2];
        self.luma_qp[address] = 0;
        self.motion_available[address].fill(true);
        self.macroblock_intra[address] = true;
    }

    fn mark_intra(&mut self, address: usize) {
        self.motion_available[address].fill(true);
        self.macroblock_intra[address] = true;
    }

    fn set_luma_qp(&mut self, address: usize, qp: i32) {
        self.luma_qp[address] = qp;
    }

    fn predict_p_skip(
        &mut self,
        reference: &ReferenceFrame,
        address: usize,
        prediction_weights: PredictionWeights,
        luma_qp: i32,
    ) -> Result<()> {
        let macroblocks_wide = self.coded_width / 16;
        let block_x = (address % macroblocks_wide) * 4;
        let block_y = (address / macroblocks_wide) * 4;
        let left = self.motion_at(block_x.cast_signed() - 1, block_y.cast_signed());
        let top = self.motion_at(block_x.cast_signed(), block_y.cast_signed() - 1);
        let zero = MotionInfo {
            x: 0,
            y: 0,
            reference_index: Some(0),
        };
        let vector = if left.is_none() || top.is_none() || left == Some(zero) || top == Some(zero) {
            MotionVector::default()
        } else {
            self.partition_motion_vector_predictor(address, 0, 0, 4, MotionPredictionKind::Normal)
        };
        self.predict_inter_partition(reference, address, 0, 0, 4, 4, vector, prediction_weights)?;
        self.set_partition_motion(address, 0, 0, 4, 4, vector);
        self.luma_qp[address] = luma_qp;
        Ok(())
    }

    fn partition_motion_vector_predictor(
        &self,
        address: usize,
        partition_x: usize,
        partition_y: usize,
        partition_width: usize,
        kind: MotionPredictionKind,
    ) -> MotionVector {
        let macroblocks_wide = self.coded_width / 16;
        let block_x = (address % macroblocks_wide) * 4 + partition_x;
        let block_y = (address / macroblocks_wide) * 4 + partition_y;
        let block_x = block_x.cast_signed();
        let block_y = block_y.cast_signed();
        let a = self.motion_at(block_x - 1, block_y);
        let b = self.motion_at(block_x, block_y - 1);
        let c = self
            .motion_at(block_x + partition_width.cast_signed(), block_y - 1)
            .or_else(|| self.motion_at(block_x - 1, block_y - 1));
        let preferred = match kind {
            MotionPredictionKind::Top16x8 => b,
            MotionPredictionKind::Bottom16x8 | MotionPredictionKind::Left8x16 => a,
            MotionPredictionKind::Right8x16 => c,
            MotionPredictionKind::Normal => None,
        };
        if let Some(preferred) = preferred.filter(|motion| motion.reference_index == Some(0)) {
            return MotionVector {
                x: preferred.x,
                y: preferred.y,
            };
        }
        let candidates = [a, b, c];
        let mut matching = candidates
            .into_iter()
            .flatten()
            .filter(|candidate| candidate.reference_index == Some(0));
        if let Some(candidate) = matching.next()
            && matching.next().is_none()
        {
            return MotionVector {
                x: candidate.x,
                y: candidate.y,
            };
        }
        let vectors = candidates.map(|candidate| {
            let candidate = candidate.unwrap_or_default();
            MotionVector {
                x: candidate.x,
                y: candidate.y,
            }
        });
        MotionVector {
            x: median(vectors[0].x, vectors[1].x, vectors[2].x),
            y: median(vectors[0].y, vectors[1].y, vectors[2].y),
        }
    }

    fn motion_at(&self, block_x: isize, block_y: isize) -> Option<MotionInfo> {
        let blocks_wide = self.coded_width / 4;
        let blocks_high = self.coded_height / 4;
        let block_x = usize::try_from(block_x).ok().filter(|&x| x < blocks_wide)?;
        let block_y = usize::try_from(block_y).ok().filter(|&y| y < blocks_high)?;
        let macroblock_x = block_x / 4;
        let macroblock_y = block_y / 4;
        let address = macroblock_y * (self.coded_width / 16) + macroblock_x;
        let block_index = luma_block_index(block_x % 4, block_y % 4);
        self.motion_available[address][block_index].then_some(self.motion[address][block_index])
    }

    fn set_partition_motion(
        &mut self,
        address: usize,
        partition_x: usize,
        partition_y: usize,
        partition_width: usize,
        partition_height: usize,
        vector: MotionVector,
    ) {
        for y in partition_y..partition_y + partition_height {
            for x in partition_x..partition_x + partition_width {
                self.motion[address][luma_block_index(x, y)] = MotionInfo {
                    x: vector.x,
                    y: vector.y,
                    reference_index: Some(0),
                };
                self.motion_available[address][luma_block_index(x, y)] = true;
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn predict_inter_partition(
        &mut self,
        reference: &ReferenceFrame,
        address: usize,
        partition_x: usize,
        partition_y: usize,
        partition_width: usize,
        partition_height: usize,
        vector: MotionVector,
        prediction_weights: PredictionWeights,
    ) -> Result<()> {
        let macroblocks_wide = self.coded_width / 16;
        let origin_x = (address % macroblocks_wide) * 16 + partition_x * 4;
        let origin_y = (address / macroblocks_wide) * 16 + partition_y * 4;
        let pixel_width = partition_width * 4;
        let pixel_height = partition_height * 4;
        for y in 0..pixel_height {
            for x in 0..pixel_width {
                let x_q4 = quarter_coordinate(origin_x + x, vector.x)?;
                let y_q4 = quarter_coordinate(origin_y + y, vector.y)?;
                let prediction = luma_qpel(
                    &reference.luma,
                    reference.coded_width,
                    reference.coded_height,
                    x_q4,
                    y_q4,
                );
                self.luma[(origin_y + y) * self.coded_width + origin_x + x] =
                    apply_prediction_weight(prediction, prediction_weights.luma);
            }
        }
        let chroma_stride = self.coded_width / 2;
        let chroma_height = self.coded_height / 2;
        let chroma_origin_x = (address % macroblocks_wide) * 8 + partition_x * 2;
        let chroma_origin_y = (address / macroblocks_wide) * 8 + partition_y * 2;
        let chroma_width = partition_width * 2;
        let chroma_partition_height = partition_height * 2;
        for (destination, source, weight) in [
            (&mut self.cb, &reference.cb, prediction_weights.cb),
            (&mut self.cr, &reference.cr, prediction_weights.cr),
        ] {
            for y in 0..chroma_partition_height {
                for x in 0..chroma_width {
                    let x_q8 = eighth_coordinate(chroma_origin_x + x, vector.x)?;
                    let y_q8 = eighth_coordinate(chroma_origin_y + y, vector.y)?;
                    let prediction = chroma_epel(source, chroma_stride, chroma_height, x_q8, y_q8);
                    destination[(chroma_origin_y + y) * chroma_stride + chroma_origin_x + x] =
                        apply_prediction_weight(prediction, weight);
                }
            }
        }
        Ok(())
    }

    fn mark_zero_ac(&mut self, address: usize) {
        self.luma_nonzero[address].fill(0);
    }

    fn intra16_dc_nc(&self, address: usize) -> i8 {
        let macroblocks_wide = self.coded_width / 16;
        let left =
            (!address.is_multiple_of(macroblocks_wide)).then(|| self.luma_nonzero[address - 1][5]);
        let top = (address >= macroblocks_wide)
            .then(|| self.luma_nonzero[address - macroblocks_wide][10]);
        let value = match (left, top) {
            (Some(left), Some(top)) => (left + top).div_ceil(2),
            (Some(value), None) | (None, Some(value)) => value,
            (None, None) => 0,
        };
        i8::try_from(value).expect("CAVLC nonzero count is at most 16")
    }

    fn luma_nc(&self, address: usize, block_index: usize) -> i8 {
        let macroblocks_wide = self.coded_width / 16;
        let (block_x, block_y) = luma_block_position(block_index);
        let left = if block_x > 0 {
            Some(self.luma_nonzero[address][luma_block_index(block_x - 1, block_y)])
        } else if !address.is_multiple_of(macroblocks_wide) {
            Some(self.luma_nonzero[address - 1][luma_block_index(3, block_y)])
        } else {
            None
        };
        let top = if block_y > 0 {
            Some(self.luma_nonzero[address][luma_block_index(block_x, block_y - 1)])
        } else if address >= macroblocks_wide {
            Some(self.luma_nonzero[address - macroblocks_wide][luma_block_index(block_x, 3)])
        } else {
            None
        };
        let value = match (left, top) {
            (Some(left), Some(top)) => (left + top).div_ceil(2),
            (Some(value), None) | (None, Some(value)) => value,
            (None, None) => 0,
        };
        i8::try_from(value).expect("CAVLC nonzero count is at most 16")
    }

    fn set_luma_nonzero(&mut self, address: usize, block_index: usize, count: u8) {
        self.luma_nonzero[address][block_index] = count;
    }

    fn chroma_nc(&self, address: usize, component: usize, block_index: usize) -> i8 {
        let macroblocks_wide = self.coded_width / 16;
        let block_x = block_index % 2;
        let block_y = block_index / 2;
        let left = if block_x > 0 {
            Some(self.chroma_nonzero[address][component][block_index - 1])
        } else if !address.is_multiple_of(macroblocks_wide) {
            Some(self.chroma_nonzero[address - 1][component][block_y * 2 + 1])
        } else {
            None
        };
        let top = if block_y > 0 {
            Some(self.chroma_nonzero[address][component][block_index - 2])
        } else if address >= macroblocks_wide {
            Some(self.chroma_nonzero[address - macroblocks_wide][component][2 + block_x])
        } else {
            None
        };
        let value = match (left, top) {
            (Some(left), Some(top)) => (left + top).div_ceil(2),
            (Some(value), None) | (None, Some(value)) => value,
            (None, None) => 0,
        };
        i8::try_from(value).expect("CAVLC nonzero count is at most 16")
    }

    fn set_chroma_nonzero(
        &mut self,
        address: usize,
        component: usize,
        block_index: usize,
        count: u8,
    ) {
        self.chroma_nonzero[address][component][block_index] = count;
    }

    fn mark_zero_chroma_ac(&mut self, address: usize) {
        self.chroma_nonzero[address] = [[0; 4]; 2];
    }

    fn predict_intra16_macroblock(
        &mut self,
        address: usize,
        luma_mode: u32,
        chroma_mode: u32,
    ) -> Result<()> {
        let macroblocks_wide = self.coded_width / 16;
        let macroblock_x = address % macroblocks_wide;
        let macroblock_y = address / macroblocks_wide;
        let luma_prediction = predict_block(
            &self.luma,
            self.coded_width,
            macroblock_x * 16,
            macroblock_y * 16,
            16,
            luma_mode,
        )?;
        place_block(
            &mut self.luma,
            self.coded_width,
            macroblock_x * 16,
            macroblock_y * 16,
            16,
            &luma_prediction,
        );
        self.predict_chroma_macroblock(address, chroma_mode)
    }

    fn predict_chroma_macroblock(&mut self, address: usize, chroma_mode: u32) -> Result<()> {
        self.chroma_intra_modes[address] =
            Some(u8::try_from(chroma_mode).map_err(|_| {
                Error::InvalidData("H.264 chroma prediction mode overflows".into())
            })?);
        let macroblocks_wide = self.coded_width / 16;
        let macroblock_x = address % macroblocks_wide;
        let macroblock_y = address / macroblocks_wide;
        let chroma_stride = self.coded_width / 2;
        for plane in [&mut self.cb, &mut self.cr] {
            let prediction = if chroma_mode == 0 {
                predict_chroma_dc(plane, chroma_stride, macroblock_x * 8, macroblock_y * 8)
            } else {
                let block_mode = if chroma_mode == 2 { 0 } else { chroma_mode };
                predict_block(
                    plane,
                    chroma_stride,
                    macroblock_x * 8,
                    macroblock_y * 8,
                    8,
                    block_mode,
                )?
            };
            place_block(
                plane,
                chroma_stride,
                macroblock_x * 8,
                macroblock_y * 8,
                8,
                &prediction,
            );
        }
        Ok(())
    }

    fn predicted_intra4_mode(&self, address: usize, block_index: usize) -> u8 {
        let macroblocks_wide = self.coded_width / 16;
        let (block_x, block_y) = luma_block_position(block_index);
        let left = if block_x > 0 {
            Some(self.luma_intra_modes[address][luma_block_index(block_x - 1, block_y)])
        } else if !address.is_multiple_of(macroblocks_wide) {
            Some(self.luma_intra_modes[address - 1][luma_block_index(3, block_y)])
        } else {
            None
        };
        let top = if block_y > 0 {
            Some(self.luma_intra_modes[address][luma_block_index(block_x, block_y - 1)])
        } else if address >= macroblocks_wide {
            Some(self.luma_intra_modes[address - macroblocks_wide][luma_block_index(block_x, 3)])
        } else {
            None
        };
        match (left, top) {
            (Some(left), Some(top)) => left.min(top),
            _ => 2,
        }
    }

    fn set_intra4_mode(&mut self, address: usize, block_index: usize, mode: u8) {
        self.luma_intra_modes[address][block_index] = mode;
    }

    fn reconstruct_intra4_luma_block(
        &mut self,
        address: usize,
        block_index: usize,
        mode: u8,
        levels: &[i32],
        qp: i32,
    ) -> Result<()> {
        let macroblocks_wide = self.coded_width / 16;
        let macroblock_x = address % macroblocks_wide;
        let macroblock_y = address / macroblocks_wide;
        let (block_x, block_y) = luma_block_position(block_index);
        let origin_x = macroblock_x * 16 + block_x * 4;
        let origin_y = macroblock_y * 16 + block_y * 4;
        let prediction = predict_intra4_block(
            &self.luma,
            self.coded_width,
            origin_x,
            origin_y,
            block_index,
            mode,
        )?;
        place_block(
            &mut self.luma,
            self.coded_width,
            origin_x,
            origin_y,
            4,
            &prediction,
        );
        let levels: &[i32; 16] = levels
            .try_into()
            .map_err(|_| Error::InvalidData("invalid H.264 Intra4x4 coefficient count".into()))?;
        let residual = if self.transform_bypass_at_qp_zero && qp == 0 {
            transform_bypass_residual_4x4(levels, mode)?
        } else {
            transform_residual_4x4(levels, qp, false, &self.scaling_matrices.four_by_four[0])?
        };
        add_residual_block(
            &mut self.luma,
            self.coded_width,
            origin_x,
            origin_y,
            &residual,
        );
        Ok(())
    }

    fn reconstruct_intra8_luma_block(
        &mut self,
        address: usize,
        group: usize,
        mode: u8,
        levels: &[i32],
        qp: i32,
    ) -> Result<()> {
        let macroblocks_wide = self.coded_width / 16;
        let macroblock_x = address % macroblocks_wide;
        let macroblock_y = address / macroblocks_wide;
        let origin_x = macroblock_x * 16 + (group % 2) * 8;
        let origin_y = macroblock_y * 16 + (group / 2) * 8;
        let prediction = predict_intra8_block(
            &self.luma,
            self.coded_width,
            origin_x,
            origin_y,
            group,
            mode,
        )?;
        place_block(
            &mut self.luma,
            self.coded_width,
            origin_x,
            origin_y,
            8,
            &prediction,
        );
        let levels: &[i32; 64] = levels
            .try_into()
            .map_err(|_| Error::InvalidData("invalid H.264 Intra8x8 coefficient count".into()))?;
        let residual =
            transform_residual_8x8(levels, qp, &self.scaling_matrices.eight_by_eight[0])?;
        add_residual_block_8x8(
            &mut self.luma,
            self.coded_width,
            origin_x,
            origin_y,
            &residual,
        );
        Ok(())
    }

    fn add_intra16_luma_residual(
        &mut self,
        address: usize,
        dc_levels: &[i32],
        ac_levels: &[Vec<i32>; 16],
        luma_qp: i32,
    ) -> Result<()> {
        let scaling_list = &self.scaling_matrices.four_by_four[0];
        let dc_values = transform_intra16_luma_dc(dc_levels, luma_qp, scaling_list)?;
        let macroblocks_wide = self.coded_width / 16;
        let macroblock_x = address % macroblocks_wide;
        let macroblock_y = address / macroblocks_wide;
        for block_y in 0..4 {
            for block_x in 0..4 {
                let mut coefficients = [0_i32; 16];
                coefficients[0] = dc_values[block_y * 4 + block_x];
                let block_index = luma_block_index(block_x, block_y);
                coefficients[1..].copy_from_slice(&ac_levels[block_index]);
                let residual = transform_residual_4x4(&coefficients, luma_qp, true, scaling_list)?;
                let origin_x = macroblock_x * 16 + block_x * 4;
                let origin_y = macroblock_y * 16 + block_y * 4;
                add_residual_block(
                    &mut self.luma,
                    self.coded_width,
                    origin_x,
                    origin_y,
                    &residual,
                );
            }
        }
        Ok(())
    }

    fn add_luma_residual_blocks(
        &mut self,
        address: usize,
        levels: &[Vec<i32>; 16],
        qp: i32,
    ) -> Result<()> {
        let macroblocks_wide = self.coded_width / 16;
        let macroblock_x = address % macroblocks_wide;
        let macroblock_y = address / macroblocks_wide;
        for (block_index, block_levels) in levels.iter().enumerate() {
            let coefficients: &[i32; 16] = block_levels
                .as_slice()
                .try_into()
                .map_err(|_| Error::InvalidData("invalid H.264 luma coefficient count".into()))?;
            let residual = if self.transform_bypass_at_qp_zero && qp == 0 {
                transform_bypass_residual_4x4(coefficients, 2)?
            } else {
                transform_residual_4x4(
                    coefficients,
                    qp,
                    false,
                    &self.scaling_matrices.four_by_four[3],
                )?
            };
            let (block_x, block_y) = luma_block_position(block_index);
            add_residual_block(
                &mut self.luma,
                self.coded_width,
                macroblock_x * 16 + block_x * 4,
                macroblock_y * 16 + block_y * 4,
                &residual,
            );
        }
        Ok(())
    }

    fn add_luma_residual_8x8_blocks(
        &mut self,
        address: usize,
        levels: &[Vec<i32>; 4],
        qp: i32,
    ) -> Result<()> {
        let macroblocks_wide = self.coded_width / 16;
        let macroblock_x = address % macroblocks_wide;
        let macroblock_y = address / macroblocks_wide;
        for (group, block_levels) in levels.iter().enumerate() {
            let coefficients: &[i32; 64] = block_levels
                .as_slice()
                .try_into()
                .map_err(|_| Error::InvalidData("invalid H.264 8x8 coefficient count".into()))?;
            let residual =
                transform_residual_8x8(coefficients, qp, &self.scaling_matrices.eight_by_eight[1])?;
            add_residual_block_8x8(
                &mut self.luma,
                self.coded_width,
                macroblock_x * 16 + (group % 2) * 8,
                macroblock_y * 16 + (group / 2) * 8,
                &residual,
            );
        }
        Ok(())
    }

    fn add_chroma_residual(
        &mut self,
        address: usize,
        dc_levels: &ChromaDcLevels,
        ac_levels: &ChromaAcLevels,
        qp: i32,
    ) -> Result<()> {
        let macroblocks_wide = self.coded_width / 16;
        let macroblock_x = address % macroblocks_wide;
        let macroblock_y = address / macroblocks_wide;
        let stride = self.coded_width / 2;
        let bypass_mode = self.chroma_intra_modes[address].map_or(2, |mode| match mode {
            1 => 1,
            2 => 0,
            _ => 2,
        });
        let bypass = self.transform_bypass_at_qp_zero && qp == 0;
        let matrix_base = if self.macroblock_intra[address] { 0 } else { 3 };
        for (component, plane) in [&mut self.cb, &mut self.cr].into_iter().enumerate() {
            let scaling_list = &self.scaling_matrices.four_by_four[matrix_base + component + 1];
            let dc_values = if bypass {
                dc_levels[component]
                    .as_slice()
                    .try_into()
                    .map_err(|_| Error::InvalidData("invalid H.264 chroma DC count".into()))?
            } else {
                transform_chroma_dc(&dc_levels[component], qp, scaling_list)?
            };
            let mut residuals = [[0_i32; 16]; 4];
            for block_index in 0..4 {
                let mut coefficients = [0_i32; 16];
                coefficients[0] = dc_values[block_index];
                coefficients[1..].copy_from_slice(&ac_levels[component][block_index]);
                residuals[block_index] = if bypass {
                    transform_bypass_residual_4x4(&coefficients, bypass_mode)?
                } else {
                    transform_residual_4x4(&coefficients, qp, true, scaling_list)?
                };
            }
            if bypass {
                continue_transform_bypass_chroma(&mut residuals, bypass_mode)?;
            }
            for (block_index, residual) in residuals.iter().enumerate() {
                let block_x = block_index % 2;
                let block_y = block_index / 2;
                add_residual_block(
                    plane,
                    stride,
                    macroblock_x * 8 + block_x * 4,
                    macroblock_y * 8 + block_y * 4,
                    residual,
                );
            }
        }
        Ok(())
    }

    fn deblock(&mut self, chroma_qp_offsets: [i32; 2], params: DeblockingParameters) -> Result<()> {
        filter_picture(
            &mut DeblockingPicture {
                luma: &mut self.luma,
                cb: &mut self.cb,
                cr: &mut self.cr,
                coded_width: self.coded_width,
                coded_height: self.coded_height,
                luma_qp: &self.luma_qp,
                chroma_qp_offset_cb: chroma_qp_offsets[0],
                chroma_qp_offset_cr: chroma_qp_offsets[1],
                macroblock_intra: &self.macroblock_intra,
                transform_8x8: &self.transform_8x8,
                luma_nonzero: &self.luma_nonzero,
                motion: &self.motion,
            },
            params,
        )
    }

    fn to_frame(&self, sps: &Sps, timing: FrameTiming) -> Result<VideoFrame> {
        let width = usize::try_from(sps.width)
            .map_err(|_| Error::InvalidData("H.264 display width overflows".into()))?;
        let height = usize::try_from(sps.height)
            .map_err(|_| Error::InvalidData("H.264 display height overflows".into()))?;
        let crop_left = usize::try_from(sps.crop_left)
            .map_err(|_| Error::InvalidData("H.264 horizontal crop overflows".into()))?;
        let crop_top = usize::try_from(sps.crop_top)
            .map_err(|_| Error::InvalidData("H.264 vertical crop overflows".into()))?;
        let chroma_width = width.div_ceil(2);
        let chroma_height = height.div_ceil(2);
        let planes = vec![
            crop_plane(
                &self.luma,
                self.coded_width,
                crop_left,
                crop_top,
                width,
                height,
            )?,
            crop_plane(
                &self.cb,
                self.coded_width / 2,
                crop_left / 2,
                crop_top / 2,
                chroma_width,
                chroma_height,
            )?,
            crop_plane(
                &self.cr,
                self.coded_width / 2,
                crop_left / 2,
                crop_top / 2,
                chroma_width,
                chroma_height,
            )?,
        ];
        Ok(VideoFrame {
            format: PixelFormat::Yuv420p8,
            width,
            height,
            planes,
            timing,
            color: color_description(sps),
            field_order: FieldOrder::Progressive,
        })
    }

    fn into_reference(self, frame_num: u32) -> ReferenceFrame {
        ReferenceFrame {
            _frame_num: frame_num,
            coded_width: self.coded_width,
            coded_height: self.coded_height,
            luma: self.luma,
            cb: self.cb,
            cr: self.cr,
        }
    }
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

#[allow(clippy::too_many_lines)]
fn predict_intra8_block(
    plane: &[u8],
    stride: usize,
    origin_x: usize,
    origin_y: usize,
    group: usize,
    mode: u8,
) -> Result<Vec<u8>> {
    let top = (origin_y > 0).then(|| {
        let mut samples = [0_u8; 16];
        for x in 0..8 {
            samples[x] = plane[(origin_y - 1) * stride + origin_x + x];
        }
        for x in 8..16 {
            samples[x] = if group % 2 == 1 || origin_x + x >= stride {
                samples[7]
            } else {
                plane[(origin_y - 1) * stride + origin_x + x]
            };
        }
        samples
    });
    let left = (origin_x > 0).then(|| {
        let mut samples = [0_u8; 8];
        for y in 0..8 {
            samples[y] = plane[(origin_y + y) * stride + origin_x - 1];
        }
        samples
    });
    let corner =
        (origin_x > 0 && origin_y > 0).then(|| plane[(origin_y - 1) * stride + origin_x - 1]);
    let (top, left, corner) = filter_intra8_neighbors(top, left, corner);
    let mut output = vec![0_u8; 64];
    match mode {
        0 => {
            let top = top.ok_or_else(|| {
                Error::InvalidData("H.264 Intra8x8 vertical top samples are unavailable".into())
            })?;
            for row in output.as_chunks_mut::<8>().0 {
                row.copy_from_slice(&top[..8]);
            }
        }
        1 => {
            let left = left.ok_or_else(|| {
                Error::InvalidData("H.264 Intra8x8 horizontal left samples are unavailable".into())
            })?;
            for (row, sample) in output.as_chunks_mut::<8>().0.iter_mut().zip(left) {
                row.fill(sample);
            }
        }
        2 => output.copy_from_slice(&dc_prediction(
            top.as_ref().map(|value| &value[..8]),
            left.as_ref().map(<[u8; 8]>::as_slice),
            8,
        )),
        3 => {
            let top = top.ok_or_else(|| {
                Error::InvalidData(
                    "H.264 Intra8x8 diagonal-down-left samples are unavailable".into(),
                )
            })?;
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
            let top = require_intra8(top, "diagonal-down-right top")?;
            let left = require_intra8(left, "diagonal-down-right left")?;
            let corner = corner.ok_or_else(|| {
                Error::InvalidData("H.264 Intra8x8 diagonal-down-right corner unavailable".into())
            })?;
            for y in 0..8 {
                for x in 0..8 {
                    output[usize::try_from(y * 8 + x).expect("Intra8 index fits")] = match x.cmp(&y)
                    {
                        std::cmp::Ordering::Greater => filter_1_2_1(
                            top_or_corner_8(top, corner, x - y - 2),
                            top_or_corner_8(top, corner, x - y - 1),
                            top_or_corner_8(top, corner, x - y),
                        ),
                        std::cmp::Ordering::Less => filter_1_2_1(
                            left_or_corner_8(left, corner, y - x - 2),
                            left_or_corner_8(left, corner, y - x - 1),
                            left_or_corner_8(left, corner, y - x),
                        ),
                        std::cmp::Ordering::Equal => filter_1_2_1(top[0], corner, left[0]),
                    };
                }
            }
        }
        5 => predict_vertical_right_8(&mut output, top, left, corner)?,
        6 => predict_horizontal_down_8(&mut output, top, left, corner)?,
        7 => {
            let top = require_intra8(top, "vertical-left top")?;
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
            let left = require_intra8(left, "horizontal-up left")?;
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
        _ => {
            return Err(Error::InvalidData(format!(
                "invalid H.264 Intra8x8 prediction mode {mode}"
            )));
        }
    }
    Ok(output)
}

fn filter_intra8_neighbors(
    top: Option<[u8; 16]>,
    left: Option<[u8; 8]>,
    corner: Option<u8>,
) -> (Option<[u8; 16]>, Option<[u8; 8]>, Option<u8>) {
    let filtered_corner = match (top, left, corner) {
        (Some(top), Some(left), Some(corner)) => Some(filter_1_2_1(left[0], corner, top[0])),
        (_, _, value) => value,
    };
    let filtered_top = top.map(|samples| {
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
    let filtered_left = left.map(|samples| {
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
    (filtered_top, filtered_left, filtered_corner)
}

fn require_intra8<const N: usize>(samples: Option<[u8; N]>, name: &str) -> Result<[u8; N]> {
    samples.ok_or_else(|| Error::InvalidData(format!("H.264 Intra8x8 {name} unavailable")))
}

fn top_or_corner_8(top: [u8; 16], corner: u8, index: i32) -> u8 {
    if index < 0 {
        corner
    } else {
        top[usize::try_from(index).expect("bounded Intra8 top index")]
    }
}

fn left_or_corner_8(left: [u8; 8], corner: u8, index: i32) -> u8 {
    if index < 0 {
        corner
    } else {
        left[usize::try_from(index).expect("bounded Intra8 left index")]
    }
}

fn predict_vertical_right_8(
    output: &mut [u8],
    top: Option<[u8; 16]>,
    left: Option<[u8; 8]>,
    corner: Option<u8>,
) -> Result<()> {
    let top = require_intra8(top, "vertical-right top")?;
    let left = require_intra8(left, "vertical-right left")?;
    let corner = corner.ok_or_else(|| {
        Error::InvalidData("H.264 Intra8x8 vertical-right corner unavailable".into())
    })?;
    for y in 0..8_i32 {
        for x in 0..8_i32 {
            let z = 2 * x - y;
            let value = if z >= 0 {
                if z % 2 == 0 {
                    average(
                        top_or_corner_8(top, corner, x - y / 2 - 1),
                        top_or_corner_8(top, corner, x - y / 2),
                    )
                } else {
                    filter_1_2_1(
                        top_or_corner_8(top, corner, x - y / 2 - 2),
                        top_or_corner_8(top, corner, x - y / 2 - 1),
                        top_or_corner_8(top, corner, x - y / 2),
                    )
                }
            } else if z == -1 {
                filter_1_2_1(left[0], corner, top[0])
            } else {
                filter_1_2_1(
                    left_or_corner_8(left, corner, y - 2 * x - 1),
                    left_or_corner_8(left, corner, y - 2 * x - 2),
                    left_or_corner_8(left, corner, y - 2 * x - 3),
                )
            };
            output[usize::try_from(y * 8 + x).expect("Intra8 index fits")] = value;
        }
    }
    Ok(())
}

fn predict_horizontal_down_8(
    output: &mut [u8],
    top: Option<[u8; 16]>,
    left: Option<[u8; 8]>,
    corner: Option<u8>,
) -> Result<()> {
    let top = require_intra8(top, "horizontal-down top")?;
    let left = require_intra8(left, "horizontal-down left")?;
    let corner = corner.ok_or_else(|| {
        Error::InvalidData("H.264 Intra8x8 horizontal-down corner unavailable".into())
    })?;
    for y in 0..8_i32 {
        for x in 0..8_i32 {
            let z = 2 * y - x;
            let value = if z >= 0 {
                if z % 2 == 0 {
                    average(
                        left_or_corner_8(left, corner, y - x / 2 - 1),
                        left_or_corner_8(left, corner, y - x / 2),
                    )
                } else {
                    filter_1_2_1(
                        left_or_corner_8(left, corner, y - x / 2 - 2),
                        left_or_corner_8(left, corner, y - x / 2 - 1),
                        left_or_corner_8(left, corner, y - x / 2),
                    )
                }
            } else if z == -1 {
                filter_1_2_1(top[0], corner, left[0])
            } else {
                filter_1_2_1(
                    top_or_corner_8(top, corner, x - 2 * y - 1),
                    top_or_corner_8(top, corner, x - 2 * y - 2),
                    top_or_corner_8(top, corner, x - 2 * y - 3),
                )
            };
            output[usize::try_from(y * 8 + x).expect("Intra8 index fits")] = value;
        }
    }
    Ok(())
}

#[allow(clippy::too_many_lines)]
fn predict_intra4_block(
    plane: &[u8],
    stride: usize,
    origin_x: usize,
    origin_y: usize,
    block_index: usize,
    mode: u8,
) -> Result<Vec<u8>> {
    let top = (origin_y > 0).then(|| {
        let mut samples = [0_u8; 8];
        for x in 0..4 {
            samples[x] = plane[(origin_y - 1) * stride + origin_x + x];
        }
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
    let left = (origin_x > 0).then(|| {
        let mut samples = [0_u8; 4];
        for y in 0..4 {
            samples[y] = plane[(origin_y + y) * stride + origin_x - 1];
        }
        samples
    });
    let top_left =
        (origin_x > 0 && origin_y > 0).then(|| plane[(origin_y - 1) * stride + origin_x - 1]);
    let mut output = vec![0_u8; 16];
    match mode {
        0 => {
            let top = require_intra4(top.as_ref(), "vertical top")?;
            for row in output.as_chunks_mut::<4>().0 {
                row.copy_from_slice(&top[..4]);
            }
        }
        1 => {
            let left = require_intra4(left.as_ref(), "horizontal left")?;
            for (row, sample) in output.as_chunks_mut::<4>().0.iter_mut().zip(left) {
                row.fill(sample);
            }
        }
        2 => {
            let prediction = dc_prediction(
                top.as_ref().map(|samples| &samples[..4]),
                left.as_ref().map(<[u8; 4]>::as_slice),
                4,
            );
            output.copy_from_slice(&prediction);
        }
        3 => {
            let top = require_intra4(top.as_ref(), "diagonal-down-left top")?;
            for y in 0..4 {
                for x in 0..4 {
                    output[y * 4 + x] = if x == 3 && y == 3 {
                        average_1_3(top[6], top[7])
                    } else {
                        filter_1_2_1(top[x + y], top[x + y + 1], top[x + y + 2])
                    };
                }
            }
        }
        4 => {
            let top = require_intra4(top.as_ref(), "diagonal-down-right top")?;
            let left = require_intra4(left.as_ref(), "diagonal-down-right left")?;
            let corner = top_left.ok_or_else(|| {
                Error::InvalidData("H.264 diagonal-down-right top-left is unavailable".into())
            })?;
            for y in 0..4 {
                for x in 0..4 {
                    let x_i32 = i32::try_from(x).expect("Intra4x4 x coordinate is bounded");
                    let y_i32 = i32::try_from(y).expect("Intra4x4 y coordinate is bounded");
                    output[y * 4 + x] = match x.cmp(&y) {
                        std::cmp::Ordering::Greater => filter_1_2_1(
                            top_or_corner(top, corner, x_i32 - y_i32 - 2),
                            top_or_corner(top, corner, x_i32 - y_i32 - 1),
                            top_or_corner(top, corner, x_i32 - y_i32),
                        ),
                        std::cmp::Ordering::Less => filter_1_2_1(
                            left_or_corner(left, corner, y_i32 - x_i32 - 2),
                            left_or_corner(left, corner, y_i32 - x_i32 - 1),
                            left_or_corner(left, corner, y_i32 - x_i32),
                        ),
                        std::cmp::Ordering::Equal => filter_1_2_1(top[0], corner, left[0]),
                    };
                }
            }
        }
        5 => predict_vertical_right(&mut output, top, left, top_left)?,
        6 => predict_horizontal_down(&mut output, top, left, top_left)?,
        7 => {
            let top = require_intra4(top.as_ref(), "vertical-left top")?;
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
            let left = require_intra4(left.as_ref(), "horizontal-up left")?;
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
        _ => {
            return Err(Error::InvalidData(format!(
                "invalid H.264 Intra4x4 prediction mode {mode}"
            )));
        }
    }
    Ok(output)
}

fn predict_vertical_right(
    output: &mut [u8],
    top: Option<[u8; 8]>,
    left: Option<[u8; 4]>,
    corner: Option<u8>,
) -> Result<()> {
    let top = require_intra4(top.as_ref(), "vertical-right top")?;
    let left = require_intra4(left.as_ref(), "vertical-right left")?;
    let corner = corner
        .ok_or_else(|| Error::InvalidData("H.264 vertical-right top-left is unavailable".into()))?;
    for y in 0..4 {
        for x in 0..4 {
            let x_i32 = i32::try_from(x).expect("Intra4x4 x coordinate is bounded");
            let y_i32 = i32::try_from(y).expect("Intra4x4 y coordinate is bounded");
            let z = 2 * x_i32 - y_i32;
            output[y * 4 + x] = match z {
                0 | 2 | 4 | 6 => average(
                    top_or_corner(top, corner, x_i32 - y_i32 / 2 - 1),
                    top_or_corner(top, corner, x_i32 - y_i32 / 2),
                ),
                1 | 3 | 5 => filter_1_2_1(
                    top_or_corner(top, corner, x_i32 - y_i32 / 2 - 2),
                    top_or_corner(top, corner, x_i32 - y_i32 / 2 - 1),
                    top_or_corner(top, corner, x_i32 - y_i32 / 2),
                ),
                -1 => filter_1_2_1(left[0], corner, top[0]),
                _ => filter_1_2_1(
                    left_or_corner(left, corner, y_i32 - 1),
                    left_or_corner(left, corner, y_i32 - 2),
                    left_or_corner(left, corner, y_i32 - 3),
                ),
            };
        }
    }
    Ok(())
}

fn predict_horizontal_down(
    output: &mut [u8],
    top: Option<[u8; 8]>,
    left: Option<[u8; 4]>,
    corner: Option<u8>,
) -> Result<()> {
    let top = require_intra4(top.as_ref(), "horizontal-down top")?;
    let left = require_intra4(left.as_ref(), "horizontal-down left")?;
    let corner = corner.ok_or_else(|| {
        Error::InvalidData("H.264 horizontal-down top-left is unavailable".into())
    })?;
    for y in 0..4 {
        for x in 0..4 {
            let x_i32 = i32::try_from(x).expect("Intra4x4 x coordinate is bounded");
            let y_i32 = i32::try_from(y).expect("Intra4x4 y coordinate is bounded");
            let z = 2 * y_i32 - x_i32;
            output[y * 4 + x] = match z {
                0 | 2 | 4 | 6 => average(
                    left_or_corner(left, corner, y_i32 - x_i32 / 2 - 1),
                    left_or_corner(left, corner, y_i32 - x_i32 / 2),
                ),
                1 | 3 | 5 => filter_1_2_1(
                    left_or_corner(left, corner, y_i32 - x_i32 / 2 - 2),
                    left_or_corner(left, corner, y_i32 - x_i32 / 2 - 1),
                    left_or_corner(left, corner, y_i32 - x_i32 / 2),
                ),
                -1 => filter_1_2_1(top[0], corner, left[0]),
                _ => filter_1_2_1(
                    top_or_corner(top, corner, x_i32 - 1),
                    top_or_corner(top, corner, x_i32 - 2),
                    top_or_corner(top, corner, x_i32 - 3),
                ),
            };
        }
    }
    Ok(())
}

fn require_intra4<const N: usize>(samples: Option<&[u8; N]>, name: &str) -> Result<[u8; N]> {
    samples
        .copied()
        .ok_or_else(|| Error::InvalidData(format!("H.264 Intra4x4 {name} samples are unavailable")))
}

fn top_or_corner(top: [u8; 8], corner: u8, index: i32) -> u8 {
    if index < 0 {
        corner
    } else {
        top[usize::try_from(index).expect("bounded Intra4x4 top index")]
    }
}

fn left_or_corner(left: [u8; 4], corner: u8, index: i32) -> u8 {
    if index < 0 {
        corner
    } else {
        left[usize::try_from(index).expect("bounded Intra4x4 left index")]
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

fn median(a: i32, b: i32, c: i32) -> i32 {
    let mut values = [a, b, c];
    values.sort_unstable();
    values[1]
}

fn quarter_coordinate(pixel: usize, motion: i32) -> Result<i32> {
    i32::try_from(pixel)
        .ok()
        .and_then(|value| value.checked_mul(4))
        .and_then(|value| value.checked_add(motion))
        .ok_or_else(|| Error::InvalidData("H.264 quarter-sample coordinate overflows".into()))
}

fn eighth_coordinate(pixel: usize, motion: i32) -> Result<i32> {
    i32::try_from(pixel)
        .ok()
        .and_then(|value| value.checked_mul(8))
        .and_then(|value| value.checked_add(motion))
        .ok_or_else(|| Error::InvalidData("H.264 eighth-sample coordinate overflows".into()))
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
    let taps = [-2, -1, 0, 1, 2, 3]
        .map(|offset| i32::from(reference_sample(plane, width, height, x + offset, y)));
    taps[0] - 5 * taps[1] + 20 * taps[2] + 20 * taps[3] - 5 * taps[4] + taps[5]
}

fn half_vertical(plane: &[u8], width: usize, height: usize, x: i32, y: i32) -> u8 {
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

fn chroma_epel(plane: &[u8], width: usize, height: usize, x_q8: i32, y_q8: i32) -> u8 {
    let integer_x = x_q8.div_euclid(8);
    let integer_y = y_q8.div_euclid(8);
    let fraction_x = x_q8.rem_euclid(8);
    let fraction_y = y_q8.rem_euclid(8);
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
    clip_u8(
        ((8 - fraction_x) * (8 - fraction_y) * top_left
            + fraction_x * (8 - fraction_y) * top_right
            + (8 - fraction_x) * fraction_y * bottom_left
            + fraction_x * fraction_y * bottom_right
            + 32)
            >> 6,
    )
}

fn reference_sample(plane: &[u8], width: usize, height: usize, x: i32, y: i32) -> u8 {
    let x = usize::try_from(x.clamp(0, i32::try_from(width - 1).expect("width fits i32")))
        .expect("clamped coordinate is non-negative");
    let y = usize::try_from(y.clamp(0, i32::try_from(height - 1).expect("height fits i32")))
        .expect("clamped coordinate is non-negative");
    plane[y * width + x]
}

fn clip_u8(value: i32) -> u8 {
    u8::try_from(value.clamp(0, 255)).expect("clamped H.264 sample fits u8")
}

fn apply_prediction_weight(sample: u8, prediction_weight: PredictionWeight) -> u8 {
    let weighted = prediction_weight.weight * i32::from(sample);
    let scaled = if prediction_weight.denominator == 0 {
        weighted
    } else {
        (weighted + (1_i32 << (prediction_weight.denominator - 1))) >> prediction_weight.denominator
    };
    clip_u8(scaled + prediction_weight.offset)
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

const HADAMARD_4X4: [[i64; 4]; 4] = [[1, 1, 1, 1], [1, 1, -1, -1], [1, -1, -1, 1], [1, -1, 1, -1]];

fn chroma_qp(luma_qp: i32, offset: i32) -> i32 {
    const QP_TABLE: [i32; 22] = [
        29, 30, 31, 32, 32, 33, 34, 34, 35, 35, 36, 36, 37, 37, 37, 38, 38, 38, 39, 39, 39, 39,
    ];
    let index = (luma_qp + offset).clamp(0, 51);
    if index < 30 {
        index
    } else {
        QP_TABLE[usize::try_from(index - 30).expect("clamped chroma QP index")]
    }
}

fn transform_chroma_dc(levels: &[i32], qp: i32, scaling_list: &[u8; 16]) -> Result<[i32; 4]> {
    if levels.len() != 4 || !(0..=39).contains(&qp) {
        return Err(Error::InvalidData(
            "invalid H.264 4:2:0 chroma DC transform input".into(),
        ));
    }
    let c = [
        i64::from(levels[0]),
        i64::from(levels[1]),
        i64::from(levels[2]),
        i64::from(levels[3]),
    ];
    let f = [
        c[0] + c[1] + c[2] + c[3],
        c[0] - c[1] + c[2] - c[3],
        c[0] + c[1] - c[2] - c[3],
        c[0] - c[1] - c[2] + c[3],
    ];
    let scale = i64::from(level_scale_4x4(qp, 0, 0, scaling_list));
    let mut output = [0_i32; 4];
    for (destination, value) in output.iter_mut().zip(f) {
        let scaled = ((value * scale) << (qp / 6)) >> 5;
        *destination = i32::try_from(scaled)
            .map_err(|_| Error::InvalidData("H.264 chroma DC coefficient overflows".into()))?;
    }
    Ok(output)
}

fn transform_intra16_luma_dc(
    levels: &[i32],
    qp: i32,
    scaling_list: &[u8; 16],
) -> Result<[i32; 16]> {
    if levels.len() != 16 || !(0..=51).contains(&qp) {
        return Err(Error::InvalidData(
            "invalid H.264 Intra16 luma DC transform input".into(),
        ));
    }
    let mut coefficients = [0_i64; 16];
    for (index, &(row, column)) in ZIG_ZAG_4X4.iter().enumerate() {
        coefficients[row * 4 + column] = i64::from(levels[index]);
    }
    let mut horizontal = [0_i64; 16];
    for row in 0..4 {
        for column in 0..4 {
            horizontal[row * 4 + column] = (0..4)
                .map(|index| HADAMARD_4X4[row][index] * coefficients[index * 4 + column])
                .sum();
        }
    }
    let mut output = [0_i32; 16];
    let scale = i64::from(level_scale_4x4(qp, 0, 0, scaling_list));
    for row in 0..4 {
        for column in 0..4 {
            let transformed: i64 = (0..4)
                .map(|index| horizontal[row * 4 + index] * HADAMARD_4X4[index][column])
                .sum();
            let scaled = if qp >= 36 {
                (transformed * scale) << (qp / 6 - 6)
            } else {
                (transformed * scale + (1_i64 << (5 - qp / 6))) >> (6 - qp / 6)
            };
            output[row * 4 + column] = i32::try_from(scaled).map_err(|_| {
                Error::InvalidData("H.264 Intra16 luma DC coefficient overflows".into())
            })?;
        }
    }
    Ok(output)
}

fn transform_residual_4x4(
    levels: &[i32; 16],
    qp: i32,
    dc_already_scaled: bool,
    scaling_list: &[u8; 16],
) -> Result<[i32; 16]> {
    if !(0..=51).contains(&qp) {
        return Err(Error::InvalidData("invalid H.264 luma QP".into()));
    }
    let mut scaled = [0_i64; 16];
    for (index, &(row, column)) in ZIG_ZAG_4X4.iter().enumerate() {
        let value = i64::from(levels[index]);
        scaled[row * 4 + column] = if index == 0 && dc_already_scaled {
            value
        } else {
            let scale = i64::from(level_scale_4x4(qp, row, column, scaling_list));
            if qp >= 24 {
                (value * scale) << (qp / 6 - 4)
            } else {
                (value * scale + (1_i64 << (3 - qp / 6))) >> (4 - qp / 6)
            }
        };
    }
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
    let mut output = [0_i32; 16];
    for column in 0..4 {
        let g0 = horizontal[column] + horizontal[8 + column];
        let g1 = horizontal[column] - horizontal[8 + column];
        let g2 = (horizontal[4 + column] >> 1) - horizontal[12 + column];
        let g3 = horizontal[4 + column] + (horizontal[12 + column] >> 1);
        for (row, value) in [g0 + g3, g1 + g2, g1 - g2, g0 - g3].into_iter().enumerate() {
            output[row * 4 + column] = i32::try_from((value + 32) >> 6)
                .map_err(|_| Error::InvalidData("H.264 4x4 residual sample overflows".into()))?;
        }
    }
    Ok(output)
}

fn transform_residual_8x8(
    levels: &[i32; 64],
    qp: i32,
    scaling_list: &[u8; 64],
) -> Result<[i32; 64]> {
    if !(0..=51).contains(&qp) {
        return Err(Error::InvalidData("invalid H.264 luma QP".into()));
    }
    let mut scaled = [0_i64; 64];
    for (index, &(row, column)) in ZIG_ZAG_8X8.iter().enumerate() {
        let scale = i64::from(level_scale_8x8(qp, row, column, scaling_list));
        let value = i64::from(levels[index]) * scale;
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
        inverse_transform_8(
            &scaled[row * 8..row * 8 + 8],
            &mut horizontal[row * 8..row * 8 + 8],
        );
    }
    let mut transformed = [0_i64; 64];
    for column in 0..8 {
        let input: [i64; 8] = std::array::from_fn(|row| horizontal[row * 8 + column]);
        let mut output = [0_i64; 8];
        inverse_transform_8(&input, &mut output);
        for row in 0..8 {
            transformed[row * 8 + column] = output[row];
        }
    }
    let mut residual = [0_i32; 64];
    for (destination, value) in residual.iter_mut().zip(transformed) {
        *destination = i32::try_from(value >> 6)
            .map_err(|_| Error::InvalidData("H.264 8x8 residual sample overflows".into()))?;
    }
    Ok(residual)
}

fn inverse_transform_8(input: &[i64], output: &mut [i64]) {
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
    output.copy_from_slice(&[
        b0 + b7,
        b2 + b5,
        b4 + b3,
        b6 + b1,
        b6 - b1,
        b4 - b3,
        b2 - b5,
        b0 - b7,
    ]);
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
    let row_mod = row % 4;
    let column_mod = column % 4;
    let category = match (row_mod.is_multiple_of(2), column_mod.is_multiple_of(2)) {
        (false, false) => 1,
        (false, true) | (true, false) => {
            if row_mod == 2 || column_mod == 2 {
                5
            } else {
                3
            }
        }
        (true, true) => match (row_mod, column_mod) {
            (0, 0) => 0,
            (2, 2) => 2,
            _ => 4,
        },
    };
    NORM_ADJUST[usize::try_from(qp % 6).expect("non-negative QP")][category]
        * i32::from(scaling_list[row * 8 + column])
}

fn transform_bypass_residual_4x4(levels: &[i32; 16], intra_mode: u8) -> Result<[i32; 16]> {
    let mut residual = [0_i32; 16];
    for (index, &(row, column)) in ZIG_ZAG_4X4.iter().enumerate() {
        residual[row * 4 + column] = levels[index];
    }
    match intra_mode {
        0 => {
            for row in 1..4 {
                for column in 0..4 {
                    let index = row * 4 + column;
                    residual[index] = residual[index]
                        .checked_add(residual[index - 4])
                        .ok_or_else(|| {
                            Error::InvalidData(
                                "H.264 transform-bypass vertical residual overflows".into(),
                            )
                        })?;
                }
            }
        }
        1 => {
            for row in 0..4 {
                for column in 1..4 {
                    let index = row * 4 + column;
                    residual[index] = residual[index]
                        .checked_add(residual[index - 1])
                        .ok_or_else(|| {
                            Error::InvalidData(
                                "H.264 transform-bypass horizontal residual overflows".into(),
                            )
                        })?;
                }
            }
        }
        _ => {}
    }
    Ok(residual)
}

fn continue_transform_bypass_chroma(residuals: &mut [[i32; 16]; 4], intra_mode: u8) -> Result<()> {
    match intra_mode {
        0 => {
            for bottom_block in 2..4 {
                let top_block = bottom_block - 2;
                for row in 0..4 {
                    for column in 0..4 {
                        let index = row * 4 + column;
                        residuals[bottom_block][index] = residuals[bottom_block][index]
                            .checked_add(residuals[top_block][12 + column])
                            .ok_or_else(|| {
                                Error::InvalidData(
                                    "H.264 transform-bypass chroma residual overflows".into(),
                                )
                            })?;
                    }
                }
            }
        }
        1 => {
            for right_block in [1, 3] {
                let left_block = right_block - 1;
                for row in 0..4 {
                    let continuation = residuals[left_block][row * 4 + 3];
                    for column in 0..4 {
                        let index = row * 4 + column;
                        residuals[right_block][index] = residuals[right_block][index]
                            .checked_add(continuation)
                            .ok_or_else(|| {
                                Error::InvalidData(
                                    "H.264 transform-bypass chroma residual overflows".into(),
                                )
                            })?;
                    }
                }
            }
        }
        _ => {}
    }
    Ok(())
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
    let category = if row.is_multiple_of(2) && column.is_multiple_of(2) {
        0
    } else if !row.is_multiple_of(2) && !column.is_multiple_of(2) {
        1
    } else {
        2
    };
    NORM_ADJUST[usize::try_from(qp % 6).expect("non-negative QP")][category]
        * i32::from(scaling_list[row * 4 + column])
}

fn add_residual_block(
    plane: &mut [u8],
    stride: usize,
    origin_x: usize,
    origin_y: usize,
    residual: &[i32; 16],
) {
    for y in 0..4 {
        for x in 0..4 {
            let index = (origin_y + y) * stride + origin_x + x;
            let value = i32::from(plane[index]) + residual[y * 4 + x];
            plane[index] = u8::try_from(value.clamp(0, 255)).expect("clamped luma sample fits u8");
        }
    }
}

fn add_residual_block_8x8(
    plane: &mut [u8],
    stride: usize,
    origin_x: usize,
    origin_y: usize,
    residual: &[i32; 64],
) {
    for y in 0..8 {
        for x in 0..8 {
            let index = (origin_y + y) * stride + origin_x + x;
            let value = i32::from(plane[index]) + residual[y * 8 + x];
            plane[index] = u8::try_from(value.clamp(0, 255)).expect("clamped luma sample fits u8");
        }
    }
}

fn predict_chroma_dc(plane: &[u8], stride: usize, origin_x: usize, origin_y: usize) -> Vec<u8> {
    let top = (origin_y > 0).then(|| {
        (0..8)
            .map(|x| plane[(origin_y - 1) * stride + origin_x + x])
            .collect::<Vec<_>>()
    });
    let left = (origin_x > 0).then(|| {
        (0..8)
            .map(|y| plane[(origin_y + y) * stride + origin_x - 1])
            .collect::<Vec<_>>()
    });
    let mut output = vec![0; 64];
    for block_y in 0..2 {
        for block_x in 0..2 {
            let top_samples = top.as_ref().map(|samples| &samples[block_x * 4..][..4]);
            let left_samples = left.as_ref().map(|samples| &samples[block_y * 4..][..4]);
            let use_top =
                top_samples.is_some() && (left_samples.is_none() || block_y == 0 || block_x == 1);
            let use_left =
                left_samples.is_some() && (top_samples.is_none() || block_x == 0 || block_y == 1);
            let value = dc_value(
                if use_top { top_samples } else { None },
                if use_left { left_samples } else { None },
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

fn predict_block(
    plane: &[u8],
    stride: usize,
    origin_x: usize,
    origin_y: usize,
    size: usize,
    mode: u32,
) -> Result<Vec<u8>> {
    let top = (origin_y > 0).then(|| {
        (0..size)
            .map(|x| plane[(origin_y - 1) * stride + origin_x + x])
            .collect::<Vec<_>>()
    });
    let left = (origin_x > 0).then(|| {
        (0..size)
            .map(|y| plane[(origin_y + y) * stride + origin_x - 1])
            .collect::<Vec<_>>()
    });
    match mode {
        0 => {
            let top = top.ok_or_else(|| {
                Error::InvalidData("vertical intra prediction has no top samples".into())
            })?;
            Ok((0..size).flat_map(|_| top.iter().copied()).collect())
        }
        1 => {
            let left = left.ok_or_else(|| {
                Error::InvalidData("horizontal intra prediction has no left samples".into())
            })?;
            Ok(left
                .into_iter()
                .flat_map(|sample| std::iter::repeat_n(sample, size))
                .collect())
        }
        2 => Ok(dc_prediction(top.as_deref(), left.as_deref(), size)),
        3 => plane_prediction(plane, stride, origin_x, origin_y, size, top, left),
        _ => Err(Error::InvalidData(format!(
            "invalid H.264 intra prediction mode {mode}"
        ))),
    }
}

fn dc_prediction(top: Option<&[u8]>, left: Option<&[u8]>, size: usize) -> Vec<u8> {
    let (sum, divisor, rounding) = match (top, left) {
        (Some(top), Some(left)) => (
            top.iter()
                .chain(left)
                .map(|&sample| u32::from(sample))
                .sum::<u32>(),
            u32::try_from(size * 2).expect("prediction block size is bounded"),
            u32::try_from(size).expect("prediction block size is bounded"),
        ),
        (Some(samples), None) | (None, Some(samples)) => (
            samples.iter().map(|&sample| u32::from(sample)).sum(),
            u32::try_from(size).expect("prediction block size is bounded"),
            u32::try_from(size / 2).expect("prediction block size is bounded"),
        ),
        (None, None) => return vec![128; size * size],
    };
    let value = u8::try_from((sum + rounding) / divisor).expect("average of u8 samples fits u8");
    vec![value; size * size]
}

fn plane_prediction(
    plane: &[u8],
    stride: usize,
    origin_x: usize,
    origin_y: usize,
    size: usize,
    top: Option<Vec<u8>>,
    left: Option<Vec<u8>>,
) -> Result<Vec<u8>> {
    let top =
        top.ok_or_else(|| Error::InvalidData("plane intra prediction has no top samples".into()))?;
    let left = left
        .ok_or_else(|| Error::InvalidData("plane intra prediction has no left samples".into()))?;
    let top_left = i32::from(plane[(origin_y - 1) * stride + origin_x - 1]);
    let half = size / 2;
    let mut horizontal = 0_i32;
    let mut vertical = 0_i32;
    for index in 1..=half {
        let earlier_top = if index == half {
            top_left
        } else {
            i32::from(top[half - 1 - index])
        };
        let earlier_left = if index == half {
            top_left
        } else {
            i32::from(left[half - 1 - index])
        };
        horizontal += i32::try_from(index).expect("prediction block size is bounded")
            * (i32::from(top[half - 1 + index]) - earlier_top);
        vertical += i32::try_from(index).expect("prediction block size is bounded")
            * (i32::from(left[half - 1 + index]) - earlier_left);
    }
    let (scale, shift) = if size == 16 { (5, 6) } else { (17, 5) };
    let a = 16 * (i32::from(top[size - 1]) + i32::from(left[size - 1]));
    let b = (scale * horizontal + (1 << (shift - 1))) >> shift;
    let c = (scale * vertical + (1 << (shift - 1))) >> shift;
    let center = i32::try_from(half - 1).expect("prediction block size is bounded");
    Ok((0..size)
        .flat_map(|y| {
            (0..size).map(move |x| {
                let value = (a
                    + b * (i32::try_from(x).expect("bounded x") - center)
                    + c * (i32::try_from(y).expect("bounded y") - center)
                    + 16)
                    >> 5;
                u8::try_from(value.clamp(0, 255)).expect("clamped prediction fits u8")
            })
        })
        .collect())
}

fn place_block(
    plane: &mut [u8],
    stride: usize,
    origin_x: usize,
    origin_y: usize,
    size: usize,
    values: &[u8],
) {
    for y in 0..size {
        let destination = (origin_y + y) * stride + origin_x;
        plane[destination..destination + size].copy_from_slice(&values[y * size..(y + 1) * size]);
    }
}

fn crop_plane(
    source: &[u8],
    source_stride: usize,
    left: usize,
    top: usize,
    width: usize,
    height: usize,
) -> Result<Plane> {
    let mut data = Vec::with_capacity(
        width
            .checked_mul(height)
            .ok_or_else(|| Error::InvalidData("H.264 cropped plane size overflows".into()))?,
    );
    for row in top..top + height {
        let start = row
            .checked_mul(source_stride)
            .and_then(|offset| offset.checked_add(left))
            .ok_or_else(|| Error::InvalidData("H.264 crop offset overflows".into()))?;
        let end = start
            .checked_add(width)
            .filter(|end| *end <= source.len())
            .ok_or_else(|| Error::InvalidData("H.264 crop exceeds coded plane".into()))?;
        data.extend_from_slice(&source[start..end]);
    }
    Ok(Plane {
        data,
        stride: width,
        width,
        height,
    })
}

fn color_description(sps: &Sps) -> ColorDescription {
    let vui = sps.vui;
    ColorDescription {
        range: match vui.and_then(|value| value.video_full_range) {
            Some(true) => ColorRange::Full,
            Some(false) => ColorRange::Limited,
            None => ColorRange::Unspecified,
        },
        primaries: vui
            .and_then(|value| value.colour_primaries)
            .map(|value| format!("H.264 colour-primaries code {value}")),
        transfer: vui
            .and_then(|value| value.transfer_characteristics)
            .map(|value| format!("H.264 transfer-characteristics code {value}")),
        matrix: vui
            .and_then(|value| value.matrix_coefficients)
            .map(|value| format!("H.264 matrix-coefficients code {value}")),
    }
}

struct SyntaxReader<'a> {
    bits: BitReader<'a>,
}

impl<'a> SyntaxReader<'a> {
    const fn new(data: &'a [u8]) -> Self {
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
                .filter(|count| *count <= 31)
                .ok_or_else(|| Error::InvalidData("H.264 Exp-Golomb prefix overflows".into()))?;
        }
        let suffix = if leading_zero_bits == 0 {
            0
        } else {
            u32::try_from(self.bits(leading_zero_bits)?)
                .map_err(|_| Error::InvalidData("H.264 Exp-Golomb suffix overflows".into()))?
        };
        Ok(((1_u32 << leading_zero_bits) - 1) + suffix)
    }

    fn se(&mut self) -> Result<i32> {
        let code_num = self.ue()?;
        let magnitude = i32::try_from(code_num.div_ceil(2))
            .map_err(|_| Error::InvalidData("H.264 signed Exp-Golomb overflows".into()))?;
        Ok(if code_num.is_multiple_of(2) {
            -magnitude
        } else {
            magnitude
        })
    }

    fn align_zero_to_byte(&mut self) -> Result<()> {
        while !self.bits.bit_position().is_multiple_of(8) {
            if self.bit()? {
                return Err(Error::InvalidData(
                    "non-zero H.264 pcm_alignment_zero_bit".into(),
                ));
            }
        }
        Ok(())
    }

    fn sample(&mut self) -> Result<u8> {
        u8::try_from(self.bits(8)?)
            .map_err(|_| Error::InvalidData("H.264 PCM sample overflows".into()))
    }

    fn finish_rbsp(&mut self) -> Result<()> {
        if !self.bit()? {
            return Err(Error::InvalidData("missing H.264 rbsp_stop_one_bit".into()));
        }
        while self.bits.bits_remaining() > 0 {
            if self.bit()? {
                return Err(Error::InvalidData(
                    "non-zero H.264 rbsp_alignment_zero_bit".into(),
                ));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::{
        io::Write as _,
        process::{Command, Stdio},
    };

    use mmrecode_bitstream::BitWriter;
    use mmrecode_core::{
        CodecDescriptor, CodecId, Decoder, FourCc, MediaType, Packet, PacketFlags, Rational,
        StreamId, Timestamp,
    };

    use super::{H264Decoder, PredictionWeight, apply_prediction_weight};

    #[test]
    fn applies_explicit_prediction_weights_with_normative_rounding() {
        assert_eq!(
            apply_prediction_weight(
                100,
                PredictionWeight {
                    denominator: 2,
                    weight: 3,
                    offset: -4,
                },
            ),
            71
        );
        assert_eq!(
            apply_prediction_weight(
                200,
                PredictionWeight {
                    denominator: 0,
                    weight: 2,
                    offset: 10,
                },
            ),
            255
        );
    }

    #[test]
    fn reconstructs_one_native_ipcm_idr_picture() {
        let sps = sps();
        let pps = pps();
        let configuration = avcc(&sps, &pps);
        let descriptor = CodecDescriptor {
            codec_id: CodecId::new(crate::CODEC_NAME),
            codec_tag: Some(FourCc(*b"avc1")),
            media_type: MediaType::Video,
            configuration,
        };
        let time_base = Rational::new(1, 30).unwrap();
        let mut decoder = H264Decoder::default();
        decoder.configure(&descriptor).unwrap();
        decoder
            .send_packet(Packet {
                stream_id: StreamId(0),
                data: length_prefixed(&ipcm_slice()),
                pts: Some(Timestamp {
                    value: 7,
                    time_base,
                }),
                dts: Some(Timestamp {
                    value: 7,
                    time_base,
                }),
                duration: Some(Timestamp {
                    value: 1,
                    time_base,
                }),
                flags: PacketFlags::KEY,
                side_data: Vec::new(),
            })
            .unwrap();
        let frame = decoder.receive_frame().unwrap().unwrap();
        assert_eq!((frame.width, frame.height), (16, 16));
        assert_eq!(frame.planes.len(), 3);
        assert!(frame.planes[0].data.iter().all(|&sample| sample == 42));
        assert!(frame.planes[1].data.iter().all(|&sample| sample == 90));
        assert!(frame.planes[2].data.iter().all(|&sample| sample == 160));
        assert_eq!(frame.timing.pts.unwrap().value, 7);
        assert_eq!(frame.timing.duration.unwrap().value, 1);
        assert!(decoder.receive_frame().unwrap().is_none());

        verify_with_ffmpeg(&sps, &pps, &ipcm_slice(), [42, 90, 160]);
    }

    #[test]
    fn reconstructs_encoder_style_zero_residual_intra16x16() {
        let sps = sps();
        let pps = pps();
        let descriptor = CodecDescriptor {
            codec_id: CodecId::new(crate::CODEC_NAME),
            codec_tag: Some(FourCc(*b"avc1")),
            media_type: MediaType::Video,
            configuration: avcc(&sps, &pps),
        };
        let time_base = Rational::new(1, 25).unwrap();
        let slice = zero_residual_intra16x16_slice();
        let mut decoder = H264Decoder::default();
        decoder.configure(&descriptor).unwrap();
        decoder
            .send_packet(Packet {
                stream_id: StreamId(0),
                data: length_prefixed(&slice),
                pts: Some(Timestamp {
                    value: 0,
                    time_base,
                }),
                dts: Some(Timestamp {
                    value: 0,
                    time_base,
                }),
                duration: Some(Timestamp {
                    value: 1,
                    time_base,
                }),
                flags: PacketFlags::KEY,
                side_data: Vec::new(),
            })
            .unwrap();
        let frame = decoder.receive_frame().unwrap().unwrap();
        assert!(
            frame
                .planes
                .iter()
                .all(|plane| plane.data.iter().all(|&sample| sample == 128))
        );
        verify_with_ffmpeg(&sps, &pps, &slice, [128, 128, 128]);
    }

    #[test]
    fn reconstructs_nonzero_intra16_luma_dc_residual() {
        let sps = sps();
        let pps = pps();
        let descriptor = CodecDescriptor {
            codec_id: CodecId::new(crate::CODEC_NAME),
            codec_tag: Some(FourCc(*b"avc1")),
            media_type: MediaType::Video,
            configuration: avcc(&sps, &pps),
        };
        let slice = nonzero_luma_dc_intra16x16_slice();
        let mut decoder = H264Decoder::default();
        decoder.configure(&descriptor).unwrap();
        decoder
            .send_packet(Packet {
                stream_id: StreamId(0),
                data: length_prefixed(&slice),
                pts: None,
                dts: None,
                duration: None,
                flags: PacketFlags::KEY,
                side_data: Vec::new(),
            })
            .unwrap();
        let frame = decoder.receive_frame().unwrap().unwrap();
        assert!(frame.planes[0].data.iter().all(|&sample| sample == 130));
        assert!(frame.planes[1].data.iter().all(|&sample| sample == 128));
        assert!(frame.planes[2].data.iter().all(|&sample| sample == 128));
        verify_with_ffmpeg(&sps, &pps, &slice, [130, 128, 128]);
    }

    #[test]
    fn reconstructs_nonzero_intra16_luma_ac_residual() {
        let sps = sps();
        let pps = pps();
        let descriptor = CodecDescriptor {
            codec_id: CodecId::new(crate::CODEC_NAME),
            codec_tag: Some(FourCc(*b"avc1")),
            media_type: MediaType::Video,
            configuration: avcc(&sps, &pps),
        };
        let slice = nonzero_luma_ac_intra16x16_slice();
        let mut decoder = H264Decoder::default();
        decoder.configure(&descriptor).unwrap();
        decoder
            .send_packet(Packet {
                stream_id: StreamId(0),
                data: length_prefixed(&slice),
                pts: None,
                dts: None,
                duration: None,
                flags: PacketFlags::KEY,
                side_data: Vec::new(),
            })
            .unwrap();
        let frame = decoder.receive_frame().unwrap().unwrap();
        let native = frame
            .planes
            .iter()
            .flat_map(|plane| plane.data.iter().copied())
            .collect::<Vec<_>>();
        if let Some(independent) = decode_with_ffmpeg(&sps, &pps, &slice) {
            assert_eq!(native, independent);
        }
        assert_eq!(&frame.planes[0].data[..4], &[136, 132, 124, 120]);
    }

    #[test]
    fn reconstructs_nonzero_intra16_chroma_dc_residual() {
        let sps = sps();
        let pps = pps();
        let descriptor = CodecDescriptor {
            codec_id: CodecId::new(crate::CODEC_NAME),
            codec_tag: Some(FourCc(*b"avc1")),
            media_type: MediaType::Video,
            configuration: avcc(&sps, &pps),
        };
        let slice = nonzero_chroma_dc_intra16x16_slice();
        let mut decoder = H264Decoder::default();
        decoder.configure(&descriptor).unwrap();
        decoder
            .send_packet(Packet {
                stream_id: StreamId(0),
                data: length_prefixed(&slice),
                pts: None,
                dts: None,
                duration: None,
                flags: PacketFlags::KEY,
                side_data: Vec::new(),
            })
            .unwrap();
        let frame = decoder.receive_frame().unwrap().unwrap();
        let native = frame
            .planes
            .iter()
            .flat_map(|plane| plane.data.iter().copied())
            .collect::<Vec<_>>();
        if let Some(independent) = decode_with_ffmpeg(&sps, &pps, &slice) {
            assert_eq!(native, independent);
        }
        assert!(frame.planes[1].data.iter().all(|&sample| sample == 131));
    }

    #[test]
    fn reconstructs_nonzero_intra16_chroma_ac_residual() {
        let sps = sps();
        let pps = pps();
        let descriptor = CodecDescriptor {
            codec_id: CodecId::new(crate::CODEC_NAME),
            codec_tag: Some(FourCc(*b"avc1")),
            media_type: MediaType::Video,
            configuration: avcc(&sps, &pps),
        };
        let slice = nonzero_chroma_ac_intra16x16_slice();
        let mut decoder = H264Decoder::default();
        decoder.configure(&descriptor).unwrap();
        decoder
            .send_packet(Packet {
                stream_id: StreamId(0),
                data: length_prefixed(&slice),
                pts: None,
                dts: None,
                duration: None,
                flags: PacketFlags::KEY,
                side_data: Vec::new(),
            })
            .unwrap();
        let frame = decoder.receive_frame().unwrap().unwrap();
        let native = frame
            .planes
            .iter()
            .flat_map(|plane| plane.data.iter().copied())
            .collect::<Vec<_>>();
        if let Some(independent) = decode_with_ffmpeg(&sps, &pps, &slice) {
            assert_eq!(native, independent);
        }
        assert_eq!(&frame.planes[1].data[..4], &[136, 132, 124, 120]);
    }

    fn sps() -> Vec<u8> {
        let mut writer = BitWriter::new();
        writer.write_bits(66, 8).unwrap();
        writer.write_bits(0, 8).unwrap();
        writer.write_bits(10, 8).unwrap();
        write_ue(&mut writer, 0);
        write_ue(&mut writer, 0);
        write_ue(&mut writer, 0);
        write_ue(&mut writer, 0);
        write_ue(&mut writer, 0);
        writer.write_bit(false).unwrap();
        write_ue(&mut writer, 0);
        write_ue(&mut writer, 0);
        writer.write_bit(true).unwrap();
        writer.write_bit(true).unwrap();
        writer.write_bit(false).unwrap();
        writer.write_bit(false).unwrap();
        finish_rbsp(&mut writer);
        [vec![0x67], writer.into_bytes()].concat()
    }

    fn pps() -> Vec<u8> {
        let mut writer = BitWriter::new();
        write_ue(&mut writer, 0);
        write_ue(&mut writer, 0);
        writer.write_bit(false).unwrap();
        writer.write_bit(false).unwrap();
        write_ue(&mut writer, 0);
        write_ue(&mut writer, 0);
        write_ue(&mut writer, 0);
        writer.write_bit(false).unwrap();
        writer.write_bits(0, 2).unwrap();
        write_se(&mut writer, 0);
        write_se(&mut writer, 0);
        write_se(&mut writer, 0);
        writer.write_bit(true).unwrap();
        writer.write_bit(false).unwrap();
        writer.write_bit(false).unwrap();
        finish_rbsp(&mut writer);
        [vec![0x68], writer.into_bytes()].concat()
    }

    fn ipcm_slice() -> Vec<u8> {
        let mut writer = BitWriter::new();
        write_ue(&mut writer, 0);
        write_ue(&mut writer, 2);
        write_ue(&mut writer, 0);
        writer.write_bits(0, 4).unwrap();
        write_ue(&mut writer, 0);
        writer.write_bits(0, 4).unwrap();
        writer.write_bit(false).unwrap();
        writer.write_bit(false).unwrap();
        write_se(&mut writer, 0);
        write_ue(&mut writer, 1);
        write_ue(&mut writer, 25);
        writer.align_to_byte();
        for value in std::iter::repeat_n(42, 256)
            .chain(std::iter::repeat_n(90, 64))
            .chain(std::iter::repeat_n(160, 64))
        {
            writer.write_bits(value, 8).unwrap();
        }
        finish_rbsp(&mut writer);
        [vec![0x65], writer.into_bytes()].concat()
    }

    fn zero_residual_intra16x16_slice() -> Vec<u8> {
        let mut writer = BitWriter::new();
        write_ue(&mut writer, 0);
        write_ue(&mut writer, 2);
        write_ue(&mut writer, 0);
        writer.write_bits(0, 4).unwrap();
        write_ue(&mut writer, 0);
        writer.write_bits(0, 4).unwrap();
        writer.write_bit(false).unwrap();
        writer.write_bit(false).unwrap();
        write_se(&mut writer, 0);
        write_ue(&mut writer, 1);
        write_ue(&mut writer, 3);
        write_ue(&mut writer, 0);
        write_se(&mut writer, 0);
        writer.write_bit(true).unwrap();
        finish_rbsp(&mut writer);
        [vec![0x65], writer.into_bytes()].concat()
    }

    fn nonzero_luma_dc_intra16x16_slice() -> Vec<u8> {
        let mut writer = BitWriter::new();
        write_ue(&mut writer, 0);
        write_ue(&mut writer, 2);
        write_ue(&mut writer, 0);
        writer.write_bits(0, 4).unwrap();
        write_ue(&mut writer, 0);
        writer.write_bits(0, 4).unwrap();
        writer.write_bit(false).unwrap();
        writer.write_bit(false).unwrap();
        write_se(&mut writer, 0);
        write_ue(&mut writer, 1);
        write_ue(&mut writer, 3);
        write_ue(&mut writer, 0);
        write_se(&mut writer, 0);
        // coeff_token=(TotalCoeff 1, TrailingOnes 0), level +2, total_zeros 0.
        writer.write_bits(0b0001_0111, 8).unwrap();
        finish_rbsp(&mut writer);
        [vec![0x65], writer.into_bytes()].concat()
    }

    fn nonzero_luma_ac_intra16x16_slice() -> Vec<u8> {
        let mut writer = BitWriter::new();
        write_ue(&mut writer, 0);
        write_ue(&mut writer, 2);
        write_ue(&mut writer, 0);
        writer.write_bits(0, 4).unwrap();
        write_ue(&mut writer, 0);
        writer.write_bits(0, 4).unwrap();
        writer.write_bit(false).unwrap();
        writer.write_bit(false).unwrap();
        write_se(&mut writer, 0);
        write_ue(&mut writer, 1);
        write_ue(&mut writer, 15);
        write_ue(&mut writer, 0);
        write_se(&mut writer, 0);
        writer.write_bit(true).unwrap();
        // First AC list: TotalCoeff=1, level +2 at the first AC scan position.
        writer.write_bits(0b0001_0111, 8).unwrap();
        for _ in 1..16 {
            writer.write_bit(true).unwrap();
        }
        finish_rbsp(&mut writer);
        [vec![0x65], writer.into_bytes()].concat()
    }

    fn nonzero_chroma_dc_intra16x16_slice() -> Vec<u8> {
        let mut writer = BitWriter::new();
        write_ue(&mut writer, 0);
        write_ue(&mut writer, 2);
        write_ue(&mut writer, 0);
        writer.write_bits(0, 4).unwrap();
        write_ue(&mut writer, 0);
        writer.write_bits(0, 4).unwrap();
        writer.write_bit(false).unwrap();
        writer.write_bit(false).unwrap();
        write_se(&mut writer, 0);
        write_ue(&mut writer, 1);
        write_ue(&mut writer, 7);
        write_ue(&mut writer, 0);
        write_se(&mut writer, 0);
        writer.write_bit(true).unwrap();
        // Cb DC: TotalCoeff=1, level +2 at DC position zero.
        writer.write_bits(0b0001_1111, 8).unwrap();
        // Cr DC: TotalCoeff=0 for the nC=-1 chroma DC table.
        writer.write_bits(0b01, 2).unwrap();
        finish_rbsp(&mut writer);
        [vec![0x65], writer.into_bytes()].concat()
    }

    fn nonzero_chroma_ac_intra16x16_slice() -> Vec<u8> {
        let mut writer = BitWriter::new();
        write_ue(&mut writer, 0);
        write_ue(&mut writer, 2);
        write_ue(&mut writer, 0);
        writer.write_bits(0, 4).unwrap();
        write_ue(&mut writer, 0);
        writer.write_bits(0, 4).unwrap();
        writer.write_bit(false).unwrap();
        writer.write_bit(false).unwrap();
        write_se(&mut writer, 0);
        write_ue(&mut writer, 1);
        write_ue(&mut writer, 11);
        write_ue(&mut writer, 0);
        write_se(&mut writer, 0);
        writer.write_bit(true).unwrap();
        writer.write_bits(0b01, 2).unwrap();
        writer.write_bits(0b01, 2).unwrap();
        // Cb block zero contains one +2 coefficient at its first AC position.
        writer.write_bits(0b0001_0111, 8).unwrap();
        for _ in 1..8 {
            writer.write_bit(true).unwrap();
        }
        finish_rbsp(&mut writer);
        [vec![0x65], writer.into_bytes()].concat()
    }

    fn avcc(sps: &[u8], pps: &[u8]) -> Vec<u8> {
        let mut output = vec![1, 66, 0, 10, 0xff, 0xe1];
        output.extend(u16::try_from(sps.len()).unwrap().to_be_bytes());
        output.extend(sps);
        output.push(1);
        output.extend(u16::try_from(pps.len()).unwrap().to_be_bytes());
        output.extend(pps);
        output
    }

    fn length_prefixed(nal: &[u8]) -> Vec<u8> {
        let mut output = Vec::new();
        output.extend(u32::try_from(nal.len()).unwrap().to_be_bytes());
        output.extend(nal);
        output
    }

    fn write_ue(writer: &mut BitWriter, value: u32) {
        let code_num = u64::from(value) + 1;
        let bits = 64 - code_num.leading_zeros();
        for _ in 1..bits {
            writer.write_bit(false).unwrap();
        }
        writer
            .write_bits(code_num, u8::try_from(bits).unwrap())
            .unwrap();
    }

    fn write_se(writer: &mut BitWriter, value: i32) {
        let code_num = if value <= 0 {
            value.unsigned_abs() * 2
        } else {
            u32::try_from(value).unwrap() * 2 - 1
        };
        write_ue(writer, code_num);
    }

    fn finish_rbsp(writer: &mut BitWriter) {
        writer.write_bit(true).unwrap();
        writer.align_to_byte();
    }

    fn verify_with_ffmpeg(sps: &[u8], pps: &[u8], slice: &[u8], expected: [u8; 3]) {
        let Some(decoded) = decode_with_ffmpeg(sps, pps, slice) else {
            return;
        };
        assert_eq!(decoded.len(), 384);
        assert!(decoded[..256].iter().all(|&sample| sample == expected[0]));
        assert!(
            decoded[256..320]
                .iter()
                .all(|&sample| sample == expected[1])
        );
        assert!(decoded[320..].iter().all(|&sample| sample == expected[2]));
    }

    fn decode_with_ffmpeg(sps: &[u8], pps: &[u8], slice: &[u8]) -> Option<Vec<u8>> {
        if Command::new("ffmpeg").arg("-version").output().is_err() {
            return None;
        }
        let mut annex_b = Vec::new();
        for nal in [sps, pps, slice] {
            annex_b.extend([0, 0, 0, 1]);
            annex_b.extend(nal);
        }
        let mut child = Command::new("ffmpeg")
            .args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-f",
                "h264",
                "-i",
                "pipe:0",
                "-frames:v",
                "1",
                "-pix_fmt",
                "yuv420p",
                "-f",
                "rawvideo",
                "pipe:1",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(&annex_b).unwrap();
        let output = child.wait_with_output().unwrap();
        assert!(
            output.status.success(),
            "independent H.264 decode failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        Some(output.stdout)
    }
}
