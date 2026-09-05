use std::{
    collections::{BTreeMap, VecDeque},
    sync::Arc,
};

use mmrecode_bitstream::BitReader;
use mmrecode_core::{
    CodecDescriptor, ColorDescription, ColorRange, Decoder, Error, FieldOrder, FrameTiming,
    MediaType, Packet, PixelFormat, Plane, Result, Timestamp, VideoFrame,
};

use crate::{
    AvcDecoderConfigurationRecord, NalUnit, NalUnitType, PictureOrderCountType, Pps,
    ScalingMatrices, Sps,
    cabac::{CabacDecoder, ContextState, initial_contexts, initial_i_macroblock_contexts},
    cavlc::decode_residual_block,
    deblock::{
        BlockMotion as DeblockingMotion, MacroblockParameters as DeblockingMacroblockParameters,
        MotionInfo, Parameters as DeblockingParameters, Picture as DeblockingPicture,
        ReferenceMotion as DeblockingReferenceMotion, filter_picture,
    },
    length_prefixed_nal_units, parse_pps, parse_sps, remove_emulation_prevention,
};

type ChromaDcLevels = [Vec<i32>; 2];
type ChromaAcLevels = [[Vec<i32>; 4]; 2];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct SliceDeblocking {
    parameters: Option<DeblockingParameters>,
    filter_across_slice_boundaries: bool,
}

/// Native H.264 decoder under incremental construction.
///
/// The first normative reconstruction slices accept frame-coded, 8-bit 4:2:0 pictures. IDR
/// macroblocks may use `I_PCM`, CAVLC `Intra_16x16`, or CAVLC `Intra_4x4`, including
/// luma/chroma residual transforms, all prediction modes, and in-loop deblocking. The CAVLC P-slice
/// path retains a bounded short-term decoded-picture buffer and supports default-list reference
/// indices for skip, 16x16, 16x8, 8x16, and sub-macroblock partitions down to 4x4,
/// quarter-sample luma/eighth-sample chroma motion compensation, inter residuals, single-reference
/// explicit weighted prediction, mixed intra macroblocks, and inter-picture deblocking.
/// Baseline and High Profile streams using CAVLC and 4x4 transforms share this path. CABAC context
/// arithmetic, `I_PCM`, Intra16, and Intra4 macroblocks with luma/chroma
/// DC and AC residuals are also native for IDR and non-IDR I pictures. CABAC P slices support skip, 16x16, 16x8, 8x16, and
/// 8x8 partitions down to 4x4, mixed Intra4/Intra16/PCM macroblocks, motion, residuals, and
/// filtering with default-list multiple-reference selection and short-term list-0 reordering.
/// Frame-picture decoded-reference marking supports sliding-window and adaptive MMCO operations,
/// long-term references, and IDR long-term assignment. SPS/PPS scaling lists feed native 4x4 and
/// luma 8x8 inverse quantization. High Profile QP-zero transform bypass is native for lossless
/// Intra4 and inter residuals.
/// CAVLC B pictures using any frame-picture POC mode can reconstruct 16x16 list-0, list-1, and unweighted
/// bidirectional prediction, all explicit 16x8/8x16 L0/L1/Bi combinations, all explicit `B_8x8`
/// sub-macroblocks down to 4x4, plus spatial and temporal direct prediction for whole, skipped, and
/// `B_Direct_8x8` macroblocks. Explicit and implicit weighted biprediction plus two-list in-loop
/// deblocking are native for CAVLC B slices. CABAC B slices support the same inter/direct machinery,
/// embedded intra macroblocks, implicit weighting, and High Profile 8x8 residuals.
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
    references: VecDeque<ReferenceFrame>,
    field_references: VecDeque<ReferenceFrame>,
    max_long_term_frame_idx: Option<u32>,
    previous_reference_poc: Option<PictureOrderState>,
    recovery_mode: bool,
    pending_field: Option<PendingField>,
    independent_non_reference_only: bool,
}

#[derive(Debug)]
struct PendingField {
    structure: PictureStructure,
    frame_num: u32,
    picture_order: PictureOrder,
    long_term_reference: bool,
    reference_marking: Option<ReferenceMarking>,
    timing: FrameTiming,
    buffer: FrameBuffer,
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
        self.references.clear();
        self.field_references.clear();
        self.max_long_term_frame_idx = None;
        self.previous_reference_poc = None;
        self.recovery_mode = false;
        self.pending_field = None;
        self.independent_non_reference_only = false;
        Ok(())
    }

    #[allow(clippy::too_many_lines)]
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
        if self.independent_non_reference_only
            && slices.iter().any(|slice| slice.header.reference_idc != 0)
        {
            return Err(Error::InvalidState(
                "an independent H.264 decoder fork accepts only non-reference pictures".into(),
            ));
        }
        let timing = FrameTiming {
            pts: packet.pts,
            duration: packet.duration,
        };
        let all_idr = slices
            .iter()
            .all(|slice| slice.header.unit_type == NalUnitType::IdrSlice);
        let all_non_idr_i = slices
            .iter()
            .all(|slice| slice.header.unit_type == NalUnitType::CodedSlice)
            && slices
                .iter()
                .map(|slice| non_idr_slice_type(slice))
                .collect::<Result<Vec<_>>>()?
                .iter()
                .all(|&slice_type| slice_type == 2);
        let all_non_idr_p = slices
            .iter()
            .all(|slice| slice.header.unit_type == NalUnitType::CodedSlice)
            && slices
                .iter()
                .map(|slice| non_idr_slice_type(slice))
                .collect::<Result<Vec<_>>>()?
                .iter()
                .all(|&slice_type| slice_type == 0);
        let all_non_idr_b = slices
            .iter()
            .all(|slice| slice.header.unit_type == NalUnitType::CodedSlice)
            && slices
                .iter()
                .map(|slice| non_idr_slice_type(slice))
                .collect::<Result<Vec<_>>>()?
                .iter()
                .all(|&slice_type| slice_type == 1);
        if slices.len() > 1 && (all_idr || all_non_idr_i) {
            if let Some(frame) = self.decode_i_slices(&slices, timing)? {
                self.frames.push_back(frame);
            }
            return Ok(());
        }
        if slices.len() > 1 && all_non_idr_p {
            if let Some(frame) = self.decode_p_slices(&slices, timing)? {
                self.frames.push_back(frame);
            }
            return Ok(());
        }
        if slices.len() > 1 && all_non_idr_b {
            if let Some(frame) = self.decode_b_slices(&slices, timing)? {
                self.frames.push_back(frame);
            }
            return Ok(());
        }
        if slices.len() > 1 {
            return Err(Error::Unsupported(
                "native H.264 multi-slice reconstruction supports I/P/B pictures".into(),
            ));
        }
        let frame = match slices[0].header.unit_type {
            NalUnitType::IdrSlice => self.decode_idr(slices[0], timing)?,
            NalUnitType::CodedSlice => match non_idr_slice_type(slices[0])? {
                0 => self.decode_p_picture(slices[0], timing)?,
                1 => self.decode_b_picture(slices[0], timing)?,
                2 => Some(self.decode_i_picture(slices[0], timing)?),
                slice_type => {
                    return Err(Error::Unsupported(format!(
                        "native non-IDR H.264 reconstruction does not support slice type {slice_type}"
                    )));
                }
            },
            _ => unreachable!("coded-slice filter accepts only slice NAL units"),
        };
        if let Some(frame) = frame {
            self.frames.push_back(frame);
        }
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
        if self.pending_field.is_some() {
            return Err(Error::InvalidData(
                "H.264 stream ended with an unpaired field picture".into(),
            ));
        }
        Ok(())
    }
}

impl H264Decoder {
    /// Creates a cheap independent decoder snapshot for one non-reference picture.
    ///
    /// Reference pixels and motion metadata are shared immutably with the parent decoder. The
    /// returned decoder has an empty output queue and rejects reference pictures, ensuring that
    /// speculative reconstruction cannot create a divergent decoded-picture buffer.
    ///
    /// # Errors
    ///
    /// Returns an error before configuration or while a complementary field pair is pending.
    pub fn fork_for_non_reference_picture(&self) -> Result<Self> {
        if self.configuration.is_none() {
            return Err(Error::InvalidState(
                "H.264 decoder must be configured before it can be forked".into(),
            ));
        }
        if self.pending_field.is_some() {
            return Err(Error::InvalidState(
                "H.264 decoder cannot be forked between complementary fields".into(),
            ));
        }
        Ok(Self {
            configuration: self.configuration.clone(),
            sequence_parameter_sets: self.sequence_parameter_sets.clone(),
            picture_parameter_sets: self.picture_parameter_sets.clone(),
            frames: VecDeque::new(),
            references: self.references.clone(),
            field_references: self.field_references.clone(),
            max_long_term_frame_idx: self.max_long_term_frame_idx,
            previous_reference_poc: self.previous_reference_poc,
            recovery_mode: self.recovery_mode,
            pending_field: None,
            independent_non_reference_only: true,
        })
    }

    /// Resets decoded-picture state for random access at a recovery-point access unit.
    ///
    /// While recovering, missing short-term reference pictures are represented by bounded neutral
    /// pictures. The bitstream's recovery constraints guarantee that their influence has ended at
    /// the reference picture identified by `recovery_frame_count`.
    ///
    /// # Errors
    ///
    /// Returns an error when the decoder has not been configured.
    pub fn begin_recovery(&mut self, _recovery_point: crate::RecoveryPoint) -> Result<()> {
        if self.configuration.is_none() {
            return Err(Error::InvalidState(
                "H.264 decoder must be configured before recovery".into(),
            ));
        }
        self.references.clear();
        self.field_references.clear();
        self.max_long_term_frame_idx = None;
        self.previous_reference_poc = None;
        self.recovery_mode = true;
        self.pending_field = None;
        Ok(())
    }

    #[allow(clippy::too_many_lines)]
    fn decode_idr(
        &mut self,
        unit: &NalUnit<'_>,
        timing: FrameTiming,
    ) -> Result<Option<VideoFrame>> {
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
        let pps = self
            .picture_parameter_sets
            .get(&pps_id)
            .cloned()
            .ok_or_else(|| {
                Error::InvalidData(format!("H.264 slice references unknown PPS {pps_id}"))
            })?;
        let sps = self
            .sequence_parameter_sets
            .get(&pps.sequence_parameter_set_id)
            .cloned()
            .ok_or_else(|| {
                Error::InvalidData(format!(
                    "H.264 PPS {pps_id} references unknown SPS {}",
                    pps.sequence_parameter_set_id
                ))
            })?;
        validate_native_profile(&sps, &pps)?;
        if sps.separate_colour_plane {
            let _colour_plane_id = reader.bits(2)?;
        }
        let frame_num = u32::try_from(reader.bits(sps.log2_max_frame_num)?)
            .map_err(|_| Error::InvalidData("H.264 frame_num overflows".into()))?;
        let structure = read_picture_structure(&mut reader, &sps)?;
        validate_native_intra_picture_mode(&sps, &pps, structure)?;
        let _idr_pic_id = reader.ue()?;
        let picture_order = read_picture_order_count(
            &mut reader,
            &sps,
            &pps,
            frame_num,
            structure,
            true,
            true,
            self.previous_reference_poc,
        )?;
        if pps.redundant_pic_cnt_present {
            let _redundant_pic_cnt = reader.ue()?;
        }
        let _no_output_of_prior_pics_flag = reader.bit()?;
        let long_term_reference = reader.bit()?;
        let slice_qp_delta = reader.se()?;
        let mut luma_qp = 26_i32
            .checked_add(pps.pic_init_qp_minus26)
            .and_then(|value| value.checked_add(slice_qp_delta))
            .filter(|value| (0..=51).contains(value))
            .ok_or_else(|| Error::InvalidData("invalid H.264 initial slice QP".into()))?;
        let deblocking = read_deblocking_parameters(&mut reader, &pps)?;
        let mut buffer = FrameBuffer::new_for_structure(&sps, &pps, structure)?;
        let macroblock_count = buffer.macroblock_count();
        if pps.entropy_coding_mode {
            decode_cabac_i_macroblocks(
                &mut reader.bits,
                &mut buffer,
                &mut luma_qp,
                pps.chroma_qp_index_offset,
                pps.transform_8x8_mode,
                0,
                macroblock_count,
            )?;
        } else {
            decode_idr_macroblocks(
                &mut reader,
                &mut buffer,
                &pps,
                &mut luma_qp,
                0,
                macroblock_count,
                sps.mb_adaptive_frame_field && structure == PictureStructure::Frame,
            )?;
            reader.finish_rbsp()?;
        }
        buffer.deblock(
            [
                pps.chroma_qp_index_offset,
                pps.second_chroma_qp_index_offset,
            ],
            deblocking,
        )?;
        if structure != PictureStructure::Frame {
            return self.finish_idr_field(
                buffer,
                &sps,
                &pps,
                structure,
                frame_num,
                picture_order,
                long_term_reference,
                timing,
            );
        }
        let frame = buffer.to_frame(&sps, timing)?;
        self.pending_field = None;
        self.references.clear();
        self.field_references.clear();
        self.recovery_mode = false;
        self.max_long_term_frame_idx = long_term_reference.then_some(0);
        self.previous_reference_poc = picture_order.reference_state;
        let mut reference = buffer.into_reference(frame_num, picture_order.value);
        reference.long_term_frame_idx = long_term_reference.then_some(0);
        retain_reference(&mut self.references, reference, sps.max_num_ref_frames);
        Ok(Some(frame))
    }

    #[allow(clippy::too_many_arguments)]
    fn finish_idr_field(
        &mut self,
        buffer: FrameBuffer,
        sps: &Sps,
        pps: &Pps,
        structure: PictureStructure,
        frame_num: u32,
        picture_order: PictureOrder,
        long_term_reference: bool,
        timing: FrameTiming,
    ) -> Result<Option<VideoFrame>> {
        let mut field_reference = buffer.to_reference(frame_num, picture_order.value, structure);
        field_reference.long_term_frame_idx = long_term_reference.then_some(0);
        let complementary = self.pending_field.take().filter(|pending| {
            pending.frame_num == frame_num
                && pending.structure != structure
                && pending.long_term_reference == long_term_reference
        });
        let Some(first) = complementary else {
            self.references.clear();
            self.field_references.clear();
            retain_field_reference(
                &mut self.field_references,
                field_reference,
                sps.max_num_ref_frames,
            );
            self.recovery_mode = false;
            self.max_long_term_frame_idx = None;
            self.previous_reference_poc = picture_order.reference_state;
            self.pending_field = Some(PendingField {
                structure,
                frame_num,
                picture_order,
                long_term_reference,
                reference_marking: None,
                timing,
                buffer,
            });
            return Ok(None);
        };
        self.field_references.clear();
        retain_field_reference(
            &mut self.field_references,
            field_reference,
            sps.max_num_ref_frames,
        );
        let (top, bottom, top_order, bottom_order) = if structure == PictureStructure::TopField {
            (
                buffer,
                first.buffer,
                picture_order.value,
                first.picture_order.value,
            )
        } else {
            (
                first.buffer,
                buffer,
                first.picture_order.value,
                picture_order.value,
            )
        };
        let buffer = FrameBuffer::weave_fields(sps, pps, &top, &bottom)?;
        let timing = combine_field_timing(first.timing, timing)?;
        let field_order = if top_order <= bottom_order {
            FieldOrder::TopFirst
        } else {
            FieldOrder::BottomFirst
        };
        let frame = buffer.to_frame_with_field_order(sps, timing, field_order)?;
        self.references.clear();
        self.recovery_mode = false;
        self.max_long_term_frame_idx = long_term_reference.then_some(0);
        self.previous_reference_poc = picture_order.reference_state;
        let mut reference = buffer.into_reference(frame_num, top_order.min(bottom_order));
        reference.long_term_frame_idx = long_term_reference.then_some(0);
        retain_reference(&mut self.references, reference, sps.max_num_ref_frames);
        Ok(Some(frame))
    }

    #[allow(clippy::too_many_arguments)]
    fn finish_inter_field(
        &mut self,
        buffer: FrameBuffer,
        sps: &Sps,
        pps: &Pps,
        structure: PictureStructure,
        frame_num: u32,
        picture_order: PictureOrder,
        reference_marking: Option<ReferenceMarking>,
        timing: FrameTiming,
    ) -> Result<Option<VideoFrame>> {
        let field_reference = reference_marking
            .as_ref()
            .map(|_| buffer.to_reference(frame_num, picture_order.value, structure));
        let complementary = self
            .pending_field
            .take()
            .filter(|pending| pending.frame_num == frame_num && pending.structure != structure);
        let Some(first) = complementary else {
            if let (Some(reference), Some(marking)) = (field_reference, reference_marking.clone()) {
                apply_field_reference_marking(
                    &mut self.field_references,
                    reference,
                    sps.max_num_ref_frames,
                    sps.log2_max_frame_num,
                    marking,
                    &mut self.max_long_term_frame_idx,
                )?;
                self.previous_reference_poc = picture_order.reference_state;
            }
            self.pending_field = Some(PendingField {
                structure,
                frame_num,
                picture_order,
                long_term_reference: false,
                reference_marking,
                timing,
                buffer,
            });
            return Ok(None);
        };
        if let (Some(reference), Some(marking)) = (field_reference, reference_marking.clone()) {
            apply_field_reference_marking(
                &mut self.field_references,
                reference,
                sps.max_num_ref_frames,
                sps.log2_max_frame_num,
                marking,
                &mut self.max_long_term_frame_idx,
            )?;
            self.previous_reference_poc = picture_order.reference_state;
        }
        let (top, bottom, top_order, bottom_order) = if structure == PictureStructure::TopField {
            (
                buffer,
                first.buffer,
                picture_order.value,
                first.picture_order.value,
            )
        } else {
            (
                first.buffer,
                buffer,
                first.picture_order.value,
                picture_order.value,
            )
        };
        let buffer = FrameBuffer::weave_fields(sps, pps, &top, &bottom)?;
        let timing = combine_field_timing(first.timing, timing)?;
        let field_order = if top_order <= bottom_order {
            FieldOrder::TopFirst
        } else {
            FieldOrder::BottomFirst
        };
        let frame = buffer.to_frame_with_field_order(sps, timing, field_order)?;
        if first.reference_marking.is_some() || reference_marking.is_some() {
            synchronize_frame_references(
                &mut self.references,
                &self.field_references,
                buffer.into_reference(frame_num, top_order.min(bottom_order)),
                sps.max_num_ref_frames,
            );
        }
        Ok(Some(frame))
    }

    #[allow(clippy::too_many_lines)]
    fn decode_i_slices(
        &mut self,
        units: &[&NalUnit<'_>],
        timing: FrameTiming,
    ) -> Result<Option<VideoFrame>> {
        let is_idr = units[0].header.unit_type == NalUnitType::IdrSlice;
        let is_reference = units[0].header.reference_idc != 0;
        if units.iter().any(|unit| {
            (unit.header.unit_type == NalUnitType::IdrSlice) != is_idr
                || (unit.header.reference_idc != 0) != is_reference
        }) {
            return Err(Error::InvalidData(
                "H.264 I slices disagree on IDR or reference status".into(),
            ));
        }
        if is_idr && !is_reference {
            return Err(Error::InvalidData(
                "H.264 IDR picture is not marked as a reference".into(),
            ));
        }
        let prefixes = units
            .iter()
            .map(|unit| i_slice_prefix(unit))
            .collect::<Result<Vec<_>>>()?;
        if prefixes.first().map(|prefix| prefix.0) != Some(0)
            || prefixes.windows(2).any(|pair| pair[0].0 >= pair[1].0)
        {
            return Err(Error::InvalidData(
                "H.264 slices are not in increasing macroblock order from zero".into(),
            ));
        }
        let pps_id = prefixes[0].1;
        if prefixes.iter().any(|prefix| prefix.1 != pps_id) {
            return Err(Error::Unsupported(
                "native H.264 multi-slice pictures require one active PPS".into(),
            ));
        }
        let pps = self
            .picture_parameter_sets
            .get(&pps_id)
            .cloned()
            .ok_or_else(|| {
                Error::InvalidData(format!("H.264 slice references unknown PPS {pps_id}"))
            })?;
        let sps = self
            .sequence_parameter_sets
            .get(&pps.sequence_parameter_set_id)
            .cloned()
            .ok_or_else(|| {
                Error::InvalidData(format!(
                    "H.264 PPS {pps_id} references unknown SPS {}",
                    pps.sequence_parameter_set_id
                ))
            })?;
        validate_native_profile(&sps, &pps)?;
        let structure = slice_picture_structure(units[0], &sps)?;
        validate_native_picture_mode(&sps, structure)?;
        let mut buffer = FrameBuffer::new_for_structure(&sps, &pps, structure)?;
        if prefixes
            .last()
            .is_some_and(|prefix| prefix.0 >= buffer.macroblock_count())
        {
            return Err(Error::InvalidData(
                "H.264 slice starts outside the coded picture".into(),
            ));
        }
        let mut picture_frame_num = None;
        let mut picture_id = None;
        let mut picture_order = None;
        let mut long_term_reference = None;
        let mut picture_reference_marking = None;
        let mut macroblock_deblocking = vec![
            DeblockingMacroblockParameters {
                parameters: None,
                slice_id: 0,
                filter_across_slice_boundaries: false,
            };
            buffer.macroblock_count()
        ];
        for (index, unit) in units.iter().enumerate() {
            let rbsp = remove_emulation_prevention(
                unit.data
                    .get(1..)
                    .ok_or_else(|| Error::InvalidData("empty H.264 slice NAL".into()))?,
            );
            let mut reader = SyntaxReader::new(&rbsp);
            let first_macroblock = usize::try_from(reader.ue()?)
                .map_err(|_| Error::InvalidData("H.264 first macroblock overflows".into()))?;
            let slice_type = reader.ue()? % 5;
            let current_pps_id = reader.ue()?;
            if slice_type != 2 || current_pps_id != pps_id || first_macroblock != prefixes[index].0
            {
                return Err(Error::InvalidData(
                    "inconsistent H.264 I-slice prefix".into(),
                ));
            }
            if sps.separate_colour_plane {
                let _colour_plane_id = reader.bits(2)?;
            }
            let frame_num = u32::try_from(reader.bits(sps.log2_max_frame_num)?)
                .map_err(|_| Error::InvalidData("H.264 frame_num overflows".into()))?;
            let slice_structure = read_picture_structure(&mut reader, &sps)?;
            let idr_pic_id = is_idr.then(|| reader.ue()).transpose()?;
            let order = read_picture_order_count(
                &mut reader,
                &sps,
                &pps,
                frame_num,
                slice_structure,
                is_idr,
                is_reference,
                self.previous_reference_poc,
            )?;
            if pps.redundant_pic_cnt_present {
                let _redundant_pic_cnt = reader.ue()?;
            }
            let is_long_term = if is_idr {
                let _no_output_of_prior_pics_flag = reader.bit()?;
                Some(reader.bit()?)
            } else {
                None
            };
            let reference_marking = (!is_idr && is_reference)
                .then(|| read_reference_marking(&mut reader))
                .transpose()?;
            if slice_structure != structure
                || picture_frame_num.is_some_and(|value| value != frame_num)
                || picture_id.is_some_and(|value| Some(value) != idr_pic_id)
                || picture_order.is_some_and(|value: PictureOrder| value != order)
                || long_term_reference.is_some_and(|value| Some(value) != is_long_term)
                || picture_reference_marking
                    .as_ref()
                    .is_some_and(|value| Some(value) != reference_marking.as_ref())
            {
                return Err(Error::InvalidData(
                    "H.264 slices do not belong to one I picture".into(),
                ));
            }
            picture_frame_num.get_or_insert(frame_num);
            if let Some(idr_pic_id) = idr_pic_id {
                picture_id.get_or_insert(idr_pic_id);
            }
            picture_order.get_or_insert(order);
            if let Some(is_long_term) = is_long_term {
                long_term_reference.get_or_insert(is_long_term);
            }
            if let Some(reference_marking) = reference_marking {
                picture_reference_marking.get_or_insert(reference_marking);
            }
            let slice_qp_delta = reader.se()?;
            let mut luma_qp = 26_i32
                .checked_add(pps.pic_init_qp_minus26)
                .and_then(|value| value.checked_add(slice_qp_delta))
                .filter(|value| (0..=51).contains(value))
                .ok_or_else(|| Error::InvalidData("invalid H.264 initial slice QP".into()))?;
            let deblocking = read_deblocking_parameters(&mut reader, &pps)?;
            let end_macroblock = prefixes
                .get(index + 1)
                .map_or(buffer.macroblock_count(), |prefix| prefix.0);
            macroblock_deblocking[first_macroblock..end_macroblock].fill(
                DeblockingMacroblockParameters {
                    parameters: deblocking.parameters,
                    slice_id: index,
                    filter_across_slice_boundaries: deblocking.filter_across_slice_boundaries,
                },
            );
            buffer.begin_slice(first_macroblock)?;
            if pps.entropy_coding_mode {
                decode_cabac_i_macroblocks(
                    &mut reader.bits,
                    &mut buffer,
                    &mut luma_qp,
                    pps.chroma_qp_index_offset,
                    pps.transform_8x8_mode,
                    first_macroblock,
                    end_macroblock,
                )?;
            } else {
                decode_idr_macroblocks(
                    &mut reader,
                    &mut buffer,
                    &pps,
                    &mut luma_qp,
                    first_macroblock,
                    end_macroblock,
                    false,
                )?;
                reader.finish_rbsp()?;
            }
        }
        buffer.deblock_slices(
            [
                pps.chroma_qp_index_offset,
                pps.second_chroma_qp_index_offset,
            ],
            &macroblock_deblocking,
        )?;
        let frame_num = picture_frame_num.expect("non-empty I-slice list has frame number");
        let order = picture_order.expect("non-empty I-slice list has POC");
        if structure != PictureStructure::Frame {
            if is_idr {
                return self.finish_idr_field(
                    buffer,
                    &sps,
                    &pps,
                    structure,
                    frame_num,
                    order,
                    long_term_reference.unwrap_or(false),
                    timing,
                );
            }
            return self.finish_inter_field(
                buffer,
                &sps,
                &pps,
                structure,
                frame_num,
                order,
                picture_reference_marking,
                timing,
            );
        }
        let frame = buffer.to_frame(&sps, timing)?;
        if is_idr {
            let is_long_term = long_term_reference.unwrap_or(false);
            self.references.clear();
            self.recovery_mode = false;
            self.max_long_term_frame_idx = is_long_term.then_some(0);
            self.previous_reference_poc = order.reference_state;
            let mut reference = buffer.into_reference(frame_num, order.value);
            reference.long_term_frame_idx = is_long_term.then_some(0);
            retain_reference(&mut self.references, reference, sps.max_num_ref_frames);
        } else if let Some(reference_marking) = picture_reference_marking {
            let resets_picture_order = reference_marking.resets_picture_order();
            apply_reference_marking(
                &mut self.references,
                buffer.into_reference(frame_num, order.value),
                sps.max_num_ref_frames,
                sps.log2_max_frame_num,
                reference_marking,
                &mut self.max_long_term_frame_idx,
            )?;
            self.previous_reference_poc = if resets_picture_order {
                order.reset_reference_state
            } else {
                order.reference_state
            };
        }
        Ok(Some(frame))
    }

    #[allow(clippy::too_many_lines)]
    fn decode_i_picture(&mut self, unit: &NalUnit<'_>, timing: FrameTiming) -> Result<VideoFrame> {
        let rbsp = remove_emulation_prevention(
            unit.data
                .get(1..)
                .ok_or_else(|| Error::InvalidData("empty H.264 slice NAL".into()))?,
        );
        let mut reader = SyntaxReader::new(&rbsp);
        let first_mb = reader.ue()?;
        if first_mb != 0 {
            return Err(Error::Unsupported(
                "native H.264 reconstruction currently requires a full-picture slice".into(),
            ));
        }
        let slice_type = reader.ue()? % 5;
        if slice_type != 2 {
            return Err(Error::InvalidData(format!(
                "expected H.264 I slice, found normalized type {slice_type}"
            )));
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
        let structure = read_picture_structure(&mut reader, sps)?;
        validate_native_intra_picture_mode(sps, pps, structure)?;
        require_native_frame_picture(structure)?;
        let picture_order = read_picture_order_count(
            &mut reader,
            sps,
            pps,
            frame_num,
            structure,
            false,
            unit.header.reference_idc != 0,
            self.previous_reference_poc,
        )?;
        if pps.redundant_pic_cnt_present {
            let _redundant_pic_cnt = reader.ue()?;
        }
        let reference_marking = (unit.header.reference_idc != 0)
            .then(|| read_reference_marking(&mut reader))
            .transpose()?;
        let slice_qp_delta = reader.se()?;
        let mut luma_qp = 26_i32
            .checked_add(pps.pic_init_qp_minus26)
            .and_then(|value| value.checked_add(slice_qp_delta))
            .filter(|value| (0..=51).contains(value))
            .ok_or_else(|| Error::InvalidData("invalid H.264 initial slice QP".into()))?;
        let deblocking = read_deblocking_parameters(&mut reader, pps)?;
        let mut buffer = FrameBuffer::new(sps, pps)?;
        let macroblock_count = buffer.macroblock_count();
        if pps.entropy_coding_mode {
            decode_cabac_i_macroblocks(
                &mut reader.bits,
                &mut buffer,
                &mut luma_qp,
                pps.chroma_qp_index_offset,
                pps.transform_8x8_mode,
                0,
                macroblock_count,
            )?;
        } else {
            decode_idr_macroblocks(
                &mut reader,
                &mut buffer,
                pps,
                &mut luma_qp,
                0,
                macroblock_count,
                sps.mb_adaptive_frame_field && structure == PictureStructure::Frame,
            )?;
            reader.finish_rbsp()?;
        }
        buffer.deblock(
            [
                pps.chroma_qp_index_offset,
                pps.second_chroma_qp_index_offset,
            ],
            deblocking,
        )?;
        let frame = buffer.to_frame(sps, timing)?;
        if let Some(reference_marking) = reference_marking {
            let resets_picture_order = reference_marking.resets_picture_order();
            apply_reference_marking(
                &mut self.references,
                buffer.into_reference(frame_num, picture_order.value),
                sps.max_num_ref_frames,
                sps.log2_max_frame_num,
                reference_marking,
                &mut self.max_long_term_frame_idx,
            )?;
            self.previous_reference_poc = if resets_picture_order {
                picture_order.reset_reference_state
            } else {
                picture_order.reference_state
            };
        }
        Ok(frame)
    }

    #[allow(clippy::too_many_lines)]
    fn decode_p_picture(
        &mut self,
        unit: &NalUnit<'_>,
        timing: FrameTiming,
    ) -> Result<Option<VideoFrame>> {
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
        let pps = self
            .picture_parameter_sets
            .get(&pps_id)
            .cloned()
            .ok_or_else(|| {
                Error::InvalidData(format!("H.264 slice references unknown PPS {pps_id}"))
            })?;
        let sps = self
            .sequence_parameter_sets
            .get(&pps.sequence_parameter_set_id)
            .cloned()
            .ok_or_else(|| {
                Error::InvalidData(format!(
                    "H.264 PPS {pps_id} references unknown SPS {}",
                    pps.sequence_parameter_set_id
                ))
            })?;
        validate_native_profile(&sps, &pps)?;
        if sps.separate_colour_plane {
            let _colour_plane_id = reader.bits(2)?;
        }
        let frame_num = u32::try_from(reader.bits(sps.log2_max_frame_num)?)
            .map_err(|_| Error::InvalidData("H.264 frame_num overflows".into()))?;
        let structure = read_picture_structure(&mut reader, &sps)?;
        validate_native_inter_picture_mode(&sps, &pps, structure)?;
        let picture_order = read_picture_order_count(
            &mut reader,
            &sps,
            &pps,
            frame_num,
            structure,
            false,
            unit.header.reference_idc != 0,
            self.previous_reference_poc,
        )?;
        if pps.redundant_pic_cnt_present {
            let _redundant_pic_cnt = reader.ue()?;
        }
        let active_references_minus1 = if reader.bit()? {
            reader.ue()?
        } else {
            pps.num_ref_idx_l0_default_active_minus1
        };
        let active_reference_count = usize::try_from(active_references_minus1)
            .ok()
            .and_then(|value| value.checked_add(1))
            .ok_or_else(|| Error::InvalidData("H.264 active reference count overflows".into()))?;
        if self.recovery_mode {
            ensure_recovery_references(
                &mut self.references,
                &sps,
                frame_num,
                active_reference_count,
            )?;
        }
        let references = if structure == PictureStructure::Frame {
            read_reference_list0(
                &mut reader,
                &self.references,
                frame_num,
                sps.log2_max_frame_num,
                active_reference_count,
            )?
        } else {
            read_field_reference_list0(
                &mut reader,
                &self.field_references,
                frame_num,
                structure,
                sps.log2_max_frame_num,
                active_reference_count,
            )?
        };
        if pps.weighted_pred && active_reference_count != 1 {
            return Err(Error::Unsupported(
                "native H.264 weighted prediction currently supports one active list-0 reference"
                    .into(),
            ));
        }
        let prediction_weights = if pps.weighted_pred {
            read_prediction_weights(&mut reader)?
        } else {
            PredictionWeights::identity()
        };
        let reference_marking = (unit.header.reference_idc != 0)
            .then(|| read_reference_marking(&mut reader))
            .transpose()?;
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
        let deblocking = read_deblocking_parameters(&mut reader, &pps)?;
        let reference = references.first().copied().ok_or_else(|| {
            Error::InvalidData("H.264 P picture has no decoded reference picture".into())
        })?;
        let mut buffer =
            FrameBuffer::from_reference_for_structure(&sps, &pps, reference, luma_qp, structure)?;
        let macroblock_count = buffer.macroblock_count();
        let mut current_qp = luma_qp;
        if let Some(cabac_init_idc) = cabac_init_idc {
            decode_cabac_p_macroblocks(
                &mut reader.bits,
                &mut buffer,
                &references,
                active_references_minus1,
                prediction_weights,
                &mut current_qp,
                cabac_init_idc,
                pps.chroma_qp_index_offset,
                pps.transform_8x8_mode,
                0,
                macroblock_count,
            )?;
        } else {
            decode_p_macroblocks(
                &mut reader,
                &mut buffer,
                &references,
                active_references_minus1,
                &pps,
                prediction_weights,
                &mut current_qp,
                0,
                macroblock_count,
                sps.mb_adaptive_frame_field && structure == PictureStructure::Frame,
            )?;
            reader.finish_rbsp()?;
        }
        buffer.deblock(
            [
                pps.chroma_qp_index_offset,
                pps.second_chroma_qp_index_offset,
            ],
            deblocking,
        )?;
        if structure != PictureStructure::Frame {
            return self.finish_inter_field(
                buffer,
                &sps,
                &pps,
                structure,
                frame_num,
                picture_order,
                reference_marking,
                timing,
            );
        }
        let frame = buffer.to_frame(&sps, timing)?;
        if let Some(reference_marking) = reference_marking {
            let resets_picture_order = reference_marking.resets_picture_order();
            apply_reference_marking(
                &mut self.references,
                buffer.into_reference(frame_num, picture_order.value),
                sps.max_num_ref_frames,
                sps.log2_max_frame_num,
                reference_marking,
                &mut self.max_long_term_frame_idx,
            )?;
            self.previous_reference_poc = if resets_picture_order {
                picture_order.reset_reference_state
            } else {
                picture_order.reference_state
            };
        }
        Ok(Some(frame))
    }

    #[allow(clippy::too_many_lines)]
    fn decode_p_slices(
        &mut self,
        units: &[&NalUnit<'_>],
        timing: FrameTiming,
    ) -> Result<Option<VideoFrame>> {
        let prefixes = units
            .iter()
            .map(|unit| p_slice_prefix(unit))
            .collect::<Result<Vec<_>>>()?;
        if prefixes.first().map(|prefix| prefix.0) != Some(0)
            || prefixes.windows(2).any(|pair| pair[0].0 >= pair[1].0)
        {
            return Err(Error::InvalidData(
                "H.264 P slices are not in increasing macroblock order from zero".into(),
            ));
        }
        let pps_id = prefixes[0].1;
        if prefixes.iter().any(|prefix| prefix.1 != pps_id) {
            return Err(Error::Unsupported(
                "native H.264 multi-slice pictures require one active PPS".into(),
            ));
        }
        let pps = self
            .picture_parameter_sets
            .get(&pps_id)
            .cloned()
            .ok_or_else(|| {
                Error::InvalidData(format!("H.264 slice references unknown PPS {pps_id}"))
            })?;
        let sps = self
            .sequence_parameter_sets
            .get(&pps.sequence_parameter_set_id)
            .cloned()
            .ok_or_else(|| {
                Error::InvalidData(format!(
                    "H.264 PPS {pps_id} references unknown SPS {}",
                    pps.sequence_parameter_set_id
                ))
            })?;
        validate_native_profile(&sps, &pps)?;
        let structure = slice_picture_structure(units[0], &sps)?;
        validate_native_picture_mode(&sps, structure)?;
        let mut buffer = FrameBuffer::new_for_structure(&sps, &pps, structure)?;
        if prefixes
            .last()
            .is_some_and(|prefix| prefix.0 >= buffer.macroblock_count())
        {
            return Err(Error::InvalidData(
                "H.264 P slice starts outside the coded picture".into(),
            ));
        }
        let is_reference = units[0].header.reference_idc != 0;
        if units.iter().any(|unit| {
            unit.header.unit_type != NalUnitType::CodedSlice
                || (unit.header.reference_idc != 0) != is_reference
        }) {
            return Err(Error::InvalidData(
                "H.264 P slices disagree on NAL or reference status".into(),
            ));
        }
        let mut picture_frame_num = None;
        let mut picture_order = None;
        let mut picture_reference_marking = None;
        let mut macroblock_deblocking = vec![
            DeblockingMacroblockParameters {
                parameters: None,
                slice_id: 0,
                filter_across_slice_boundaries: false,
            };
            buffer.macroblock_count()
        ];
        for (index, unit) in units.iter().enumerate() {
            let rbsp = remove_emulation_prevention(
                unit.data
                    .get(1..)
                    .ok_or_else(|| Error::InvalidData("empty H.264 P slice NAL".into()))?,
            );
            let mut reader = SyntaxReader::new(&rbsp);
            let first_macroblock = usize::try_from(reader.ue()?)
                .map_err(|_| Error::InvalidData("H.264 first macroblock overflows".into()))?;
            let slice_type = reader.ue()? % 5;
            let current_pps_id = reader.ue()?;
            if slice_type != 0 || current_pps_id != pps_id || first_macroblock != prefixes[index].0
            {
                return Err(Error::InvalidData(
                    "inconsistent H.264 P-slice prefix".into(),
                ));
            }
            if sps.separate_colour_plane {
                let _colour_plane_id = reader.bits(2)?;
            }
            let frame_num = u32::try_from(reader.bits(sps.log2_max_frame_num)?)
                .map_err(|_| Error::InvalidData("H.264 frame_num overflows".into()))?;
            let slice_structure = read_picture_structure(&mut reader, &sps)?;
            let order = read_picture_order_count(
                &mut reader,
                &sps,
                &pps,
                frame_num,
                slice_structure,
                false,
                is_reference,
                self.previous_reference_poc,
            )?;
            if pps.redundant_pic_cnt_present {
                let _redundant_pic_cnt = reader.ue()?;
            }
            let active_references_minus1 = if reader.bit()? {
                reader.ue()?
            } else {
                pps.num_ref_idx_l0_default_active_minus1
            };
            let active_reference_count = active_reference_count(active_references_minus1)?;
            if self.recovery_mode {
                ensure_recovery_references(
                    &mut self.references,
                    &sps,
                    frame_num,
                    active_reference_count,
                )?;
            }
            let references = if slice_structure == PictureStructure::Frame {
                read_reference_list0(
                    &mut reader,
                    &self.references,
                    frame_num,
                    sps.log2_max_frame_num,
                    active_reference_count,
                )?
            } else {
                read_field_reference_list0(
                    &mut reader,
                    &self.field_references,
                    frame_num,
                    slice_structure,
                    sps.log2_max_frame_num,
                    active_reference_count,
                )?
            };
            if references.is_empty() {
                return Err(Error::InvalidData(
                    "H.264 P picture has no decoded reference picture".into(),
                ));
            }
            if pps.weighted_pred && active_reference_count != 1 {
                return Err(Error::Unsupported(
                    "native H.264 weighted prediction currently supports one active list-0 reference"
                        .into(),
                ));
            }
            let prediction_weights = if pps.weighted_pred {
                read_prediction_weights(&mut reader)?
            } else {
                PredictionWeights::identity()
            };
            let reference_marking = is_reference
                .then(|| read_reference_marking(&mut reader))
                .transpose()?;
            if slice_structure != structure
                || picture_frame_num.is_some_and(|value| value != frame_num)
                || picture_order.is_some_and(|value: PictureOrder| value != order)
                || picture_reference_marking
                    .as_ref()
                    .is_some_and(|value| Some(value) != reference_marking.as_ref())
            {
                return Err(Error::InvalidData(
                    "H.264 slices do not belong to one P picture".into(),
                ));
            }
            picture_frame_num.get_or_insert(frame_num);
            picture_order.get_or_insert(order);
            if let Some(reference_marking) = reference_marking {
                picture_reference_marking.get_or_insert(reference_marking);
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
            let mut luma_qp = 26_i32
                .checked_add(pps.pic_init_qp_minus26)
                .and_then(|value| value.checked_add(slice_qp_delta))
                .filter(|value| (0..=51).contains(value))
                .ok_or_else(|| Error::InvalidData("invalid H.264 initial slice QP".into()))?;
            let deblocking = read_deblocking_parameters(&mut reader, &pps)?;
            let end_macroblock = prefixes
                .get(index + 1)
                .map_or(buffer.macroblock_count(), |prefix| prefix.0);
            macroblock_deblocking[first_macroblock..end_macroblock].fill(
                DeblockingMacroblockParameters {
                    parameters: deblocking.parameters,
                    slice_id: index,
                    filter_across_slice_boundaries: deblocking.filter_across_slice_boundaries,
                },
            );
            buffer.begin_slice(first_macroblock)?;
            if let Some(cabac_init_idc) = cabac_init_idc {
                decode_cabac_p_macroblocks(
                    &mut reader.bits,
                    &mut buffer,
                    &references,
                    active_references_minus1,
                    prediction_weights,
                    &mut luma_qp,
                    cabac_init_idc,
                    pps.chroma_qp_index_offset,
                    pps.transform_8x8_mode,
                    first_macroblock,
                    end_macroblock,
                )?;
            } else {
                decode_p_macroblocks(
                    &mut reader,
                    &mut buffer,
                    &references,
                    active_references_minus1,
                    &pps,
                    prediction_weights,
                    &mut luma_qp,
                    first_macroblock,
                    end_macroblock,
                    false,
                )?;
                reader.finish_rbsp()?;
            }
        }
        buffer.deblock_slices(
            [
                pps.chroma_qp_index_offset,
                pps.second_chroma_qp_index_offset,
            ],
            &macroblock_deblocking,
        )?;
        let frame_num = picture_frame_num.expect("non-empty P-slice list has frame number");
        let order = picture_order.expect("non-empty P-slice list has POC");
        if structure != PictureStructure::Frame {
            return self.finish_inter_field(
                buffer,
                &sps,
                &pps,
                structure,
                frame_num,
                order,
                picture_reference_marking,
                timing,
            );
        }
        let Some(reference_marking) = picture_reference_marking else {
            return Ok(Some(buffer.into_frame(&sps, timing)?));
        };
        let frame = buffer.to_frame(&sps, timing)?;
        let resets_picture_order = reference_marking.resets_picture_order();
        apply_reference_marking(
            &mut self.references,
            buffer.into_reference(frame_num, order.value),
            sps.max_num_ref_frames,
            sps.log2_max_frame_num,
            reference_marking,
            &mut self.max_long_term_frame_idx,
        )?;
        self.previous_reference_poc = if resets_picture_order {
            order.reset_reference_state
        } else {
            order.reference_state
        };
        Ok(Some(frame))
    }

    #[allow(clippy::too_many_lines)]
    fn decode_b_slices(
        &mut self,
        units: &[&NalUnit<'_>],
        timing: FrameTiming,
    ) -> Result<Option<VideoFrame>> {
        let prefixes = units
            .iter()
            .map(|unit| b_slice_prefix(unit))
            .collect::<Result<Vec<_>>>()?;
        if prefixes.first().map(|prefix| prefix.0) != Some(0)
            || prefixes.windows(2).any(|pair| pair[0].0 >= pair[1].0)
        {
            return Err(Error::InvalidData(
                "H.264 B slices are not in increasing macroblock order from zero".into(),
            ));
        }
        let pps_id = prefixes[0].1;
        if prefixes.iter().any(|prefix| prefix.1 != pps_id) {
            return Err(Error::Unsupported(
                "native H.264 multi-slice pictures require one active PPS".into(),
            ));
        }
        let pps = self
            .picture_parameter_sets
            .get(&pps_id)
            .cloned()
            .ok_or_else(|| {
                Error::InvalidData(format!("H.264 slice references unknown PPS {pps_id}"))
            })?;
        let sps = self
            .sequence_parameter_sets
            .get(&pps.sequence_parameter_set_id)
            .cloned()
            .ok_or_else(|| {
                Error::InvalidData(format!(
                    "H.264 PPS {pps_id} references unknown SPS {}",
                    pps.sequence_parameter_set_id
                ))
            })?;
        validate_native_profile(&sps, &pps)?;
        let structure = slice_picture_structure(units[0], &sps)?;
        validate_native_picture_mode(&sps, structure)?;
        let mut buffer = FrameBuffer::new_for_structure(&sps, &pps, structure)?;
        if prefixes
            .last()
            .is_some_and(|prefix| prefix.0 >= buffer.macroblock_count())
        {
            return Err(Error::InvalidData(
                "H.264 B slice starts outside the coded picture".into(),
            ));
        }
        let is_reference = units[0].header.reference_idc != 0;
        if units.iter().any(|unit| {
            unit.header.unit_type != NalUnitType::CodedSlice
                || (unit.header.reference_idc != 0) != is_reference
        }) {
            return Err(Error::InvalidData(
                "H.264 B slices disagree on NAL or reference status".into(),
            ));
        }
        let mut picture_frame_num = None;
        let mut picture_order = None;
        let mut picture_reference_marking = None;
        let mut macroblock_deblocking = vec![
            DeblockingMacroblockParameters {
                parameters: None,
                slice_id: 0,
                filter_across_slice_boundaries: false,
            };
            buffer.macroblock_count()
        ];
        for (index, unit) in units.iter().enumerate() {
            let rbsp = remove_emulation_prevention(
                unit.data
                    .get(1..)
                    .ok_or_else(|| Error::InvalidData("empty H.264 B slice NAL".into()))?,
            );
            let mut reader = SyntaxReader::new(&rbsp);
            let first_macroblock = usize::try_from(reader.ue()?)
                .map_err(|_| Error::InvalidData("H.264 first macroblock overflows".into()))?;
            let slice_type = reader.ue()? % 5;
            let current_pps_id = reader.ue()?;
            if slice_type != 1 || current_pps_id != pps_id || first_macroblock != prefixes[index].0
            {
                return Err(Error::InvalidData(
                    "inconsistent H.264 B-slice prefix".into(),
                ));
            }
            if sps.separate_colour_plane {
                let _colour_plane_id = reader.bits(2)?;
            }
            let frame_num = u32::try_from(reader.bits(sps.log2_max_frame_num)?)
                .map_err(|_| Error::InvalidData("H.264 frame_num overflows".into()))?;
            let slice_structure = read_picture_structure(&mut reader, &sps)?;
            let order = read_picture_order_count(
                &mut reader,
                &sps,
                &pps,
                frame_num,
                slice_structure,
                false,
                is_reference,
                self.previous_reference_poc,
            )?;
            if pps.redundant_pic_cnt_present {
                let _redundant_pic_cnt = reader.ue()?;
            }
            let direct_spatial_mv_pred = reader.bit()?;
            let (active_l0_minus1, active_l1_minus1) = if reader.bit()? {
                (reader.ue()?, reader.ue()?)
            } else {
                (
                    pps.num_ref_idx_l0_default_active_minus1,
                    pps.num_ref_idx_l1_default_active_minus1,
                )
            };
            let active_l0_count = active_reference_count(active_l0_minus1)?;
            let active_l1_count = active_reference_count(active_l1_minus1)?;
            let (list0, list1) = if slice_structure == PictureStructure::Frame {
                read_b_reference_lists(
                    &mut reader,
                    &self.references,
                    frame_num,
                    order.value,
                    sps.log2_max_frame_num,
                    active_l0_count,
                    active_l1_count,
                )?
            } else {
                read_field_b_reference_lists(
                    &mut reader,
                    &self.field_references,
                    frame_num,
                    slice_structure,
                    order.value,
                    sps.log2_max_frame_num,
                    active_l0_count,
                    active_l1_count,
                )?
            };
            if list0.is_empty() || list1.is_empty() {
                return Err(Error::InvalidData(
                    "H.264 B picture has an empty decoded reference list".into(),
                ));
            }
            let prediction_weights = match pps.weighted_bipred_idc {
                0 => BPredictionWeights::identity(active_l0_count, active_l1_count),
                1 => read_b_prediction_weights(&mut reader, active_l0_count, active_l1_count)?,
                2 => BPredictionWeights::implicit(active_l0_count, active_l1_count, order.value),
                _ => unreachable!("weighted_bipred_idc is a two-bit field"),
            };
            let reference_marking = is_reference
                .then(|| read_reference_marking(&mut reader))
                .transpose()?;
            if slice_structure != structure
                || picture_frame_num.is_some_and(|value| value != frame_num)
                || picture_order.is_some_and(|value: PictureOrder| value != order)
                || picture_reference_marking
                    .as_ref()
                    .is_some_and(|value| Some(value) != reference_marking.as_ref())
            {
                return Err(Error::InvalidData(
                    "H.264 slices do not belong to one B picture".into(),
                ));
            }
            picture_frame_num.get_or_insert(frame_num);
            picture_order.get_or_insert(order);
            if let Some(reference_marking) = reference_marking {
                picture_reference_marking.get_or_insert(reference_marking);
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
            let mut luma_qp = 26_i32
                .checked_add(pps.pic_init_qp_minus26)
                .and_then(|value| value.checked_add(slice_qp_delta))
                .filter(|value| (0..=51).contains(value))
                .ok_or_else(|| Error::InvalidData("invalid H.264 initial slice QP".into()))?;
            let deblocking = read_deblocking_parameters(&mut reader, &pps)?;
            let end_macroblock = prefixes
                .get(index + 1)
                .map_or(buffer.macroblock_count(), |prefix| prefix.0);
            macroblock_deblocking[first_macroblock..end_macroblock].fill(
                DeblockingMacroblockParameters {
                    parameters: deblocking.parameters,
                    slice_id: index,
                    filter_across_slice_boundaries: deblocking.filter_across_slice_boundaries,
                },
            );
            buffer.begin_slice(first_macroblock)?;
            if let Some(cabac_init_idc) = cabac_init_idc {
                decode_cabac_b_macroblocks(
                    &mut reader.bits,
                    &mut buffer,
                    &list0,
                    &list1,
                    &prediction_weights,
                    active_l0_minus1,
                    active_l1_minus1,
                    direct_spatial_mv_pred,
                    sps.direct_8x8_inference,
                    order.value,
                    &mut luma_qp,
                    cabac_init_idc,
                    pps.chroma_qp_index_offset,
                    pps.transform_8x8_mode,
                    first_macroblock,
                    end_macroblock,
                )?;
            } else {
                decode_b_macroblocks(
                    &mut reader,
                    &mut buffer,
                    &list0,
                    &list1,
                    &prediction_weights,
                    active_l0_minus1,
                    active_l1_minus1,
                    direct_spatial_mv_pred,
                    sps.direct_8x8_inference,
                    order.value,
                    &mut luma_qp,
                    pps.chroma_qp_index_offset,
                    pps.transform_8x8_mode,
                    first_macroblock,
                    end_macroblock,
                    false,
                )?;
                reader.finish_rbsp()?;
            }
        }
        buffer.deblock_slices(
            [
                pps.chroma_qp_index_offset,
                pps.second_chroma_qp_index_offset,
            ],
            &macroblock_deblocking,
        )?;
        let frame_num = picture_frame_num.expect("non-empty B-slice list has frame number");
        let order = picture_order.expect("non-empty B-slice list has POC");
        if structure != PictureStructure::Frame {
            return self.finish_inter_field(
                buffer,
                &sps,
                &pps,
                structure,
                frame_num,
                order,
                picture_reference_marking,
                timing,
            );
        }
        let Some(reference_marking) = picture_reference_marking else {
            return Ok(Some(buffer.into_frame(&sps, timing)?));
        };
        let frame = buffer.to_frame(&sps, timing)?;
        let resets_picture_order = reference_marking.resets_picture_order();
        apply_reference_marking(
            &mut self.references,
            buffer.into_reference(frame_num, order.value),
            sps.max_num_ref_frames,
            sps.log2_max_frame_num,
            reference_marking,
            &mut self.max_long_term_frame_idx,
        )?;
        self.previous_reference_poc = if resets_picture_order {
            order.reset_reference_state
        } else {
            order.reference_state
        };
        Ok(Some(frame))
    }

    #[allow(clippy::too_many_lines)]
    fn decode_b_picture(
        &mut self,
        unit: &NalUnit<'_>,
        timing: FrameTiming,
    ) -> Result<Option<VideoFrame>> {
        let rbsp = remove_emulation_prevention(
            unit.data
                .get(1..)
                .ok_or_else(|| Error::InvalidData("empty H.264 slice".into()))?,
        );
        let mut reader = SyntaxReader::new(&rbsp);
        let first_mb = reader.ue()?;
        let slice_type = reader.ue()?;
        if slice_type % 5 != 1 {
            return Err(Error::InvalidData("expected an H.264 B slice".into()));
        }
        if first_mb != 0 {
            return Err(Error::Unsupported(
                "native H.264 reconstruction currently requires a full-picture slice".into(),
            ));
        }
        let pps_id = reader.ue()?;
        let pps = self
            .picture_parameter_sets
            .get(&pps_id)
            .cloned()
            .ok_or_else(|| {
                Error::InvalidData(format!("H.264 slice references unknown PPS {pps_id}"))
            })?;
        let sps = self
            .sequence_parameter_sets
            .get(&pps.sequence_parameter_set_id)
            .cloned()
            .ok_or_else(|| {
                Error::InvalidData(format!(
                    "H.264 PPS {pps_id} references unknown SPS {}",
                    pps.sequence_parameter_set_id
                ))
            })?;
        validate_native_profile(&sps, &pps)?;
        if sps.separate_colour_plane {
            let _colour_plane_id = reader.bits(2)?;
        }
        let frame_num = u32::try_from(reader.bits(sps.log2_max_frame_num)?)
            .map_err(|_| Error::InvalidData("H.264 frame_num overflows".into()))?;
        let structure = read_picture_structure(&mut reader, &sps)?;
        validate_native_inter_picture_mode(&sps, &pps, structure)?;
        let picture_order = read_picture_order_count(
            &mut reader,
            &sps,
            &pps,
            frame_num,
            structure,
            false,
            unit.header.reference_idc != 0,
            self.previous_reference_poc,
        )?;
        if pps.redundant_pic_cnt_present {
            let _redundant_pic_cnt = reader.ue()?;
        }
        let direct_spatial_mv_pred = reader.bit()?;
        let (active_l0_minus1, active_l1_minus1) = if reader.bit()? {
            (reader.ue()?, reader.ue()?)
        } else {
            (
                pps.num_ref_idx_l0_default_active_minus1,
                pps.num_ref_idx_l1_default_active_minus1,
            )
        };
        let active_l0_count = active_reference_count(active_l0_minus1)?;
        let active_l1_count = active_reference_count(active_l1_minus1)?;
        let (list0, list1) = if structure == PictureStructure::Frame {
            read_b_reference_lists(
                &mut reader,
                &self.references,
                frame_num,
                picture_order.value,
                sps.log2_max_frame_num,
                active_l0_count,
                active_l1_count,
            )?
        } else {
            read_field_b_reference_lists(
                &mut reader,
                &self.field_references,
                frame_num,
                structure,
                picture_order.value,
                sps.log2_max_frame_num,
                active_l0_count,
                active_l1_count,
            )?
        };
        let prediction_weights = match pps.weighted_bipred_idc {
            0 => BPredictionWeights::identity(active_l0_count, active_l1_count),
            1 => read_b_prediction_weights(&mut reader, active_l0_count, active_l1_count)?,
            2 => {
                BPredictionWeights::implicit(active_l0_count, active_l1_count, picture_order.value)
            }
            _ => unreachable!("weighted_bipred_idc is a two-bit field"),
        };
        let reference_marking = (unit.header.reference_idc != 0)
            .then(|| read_reference_marking(&mut reader))
            .transpose()?;
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
        let mut luma_qp = 26_i32
            .checked_add(pps.pic_init_qp_minus26)
            .and_then(|value| value.checked_add(slice_qp_delta))
            .filter(|value| (0..=51).contains(value))
            .ok_or_else(|| Error::InvalidData("invalid H.264 initial slice QP".into()))?;
        let deblocking = read_deblocking_parameters(&mut reader, &pps)?;
        let initial_reference = list0.first().copied().ok_or_else(|| {
            Error::InvalidData("H.264 B picture has no decoded list-0 reference".into())
        })?;
        let mut buffer = FrameBuffer::from_reference_for_structure(
            &sps,
            &pps,
            initial_reference,
            luma_qp,
            structure,
        )?;
        let macroblock_count = buffer.macroblock_count();
        if let Some(cabac_init_idc) = cabac_init_idc {
            decode_cabac_b_macroblocks(
                &mut reader.bits,
                &mut buffer,
                &list0,
                &list1,
                &prediction_weights,
                active_l0_minus1,
                active_l1_minus1,
                direct_spatial_mv_pred,
                sps.direct_8x8_inference,
                picture_order.value,
                &mut luma_qp,
                cabac_init_idc,
                pps.chroma_qp_index_offset,
                pps.transform_8x8_mode,
                0,
                macroblock_count,
            )?;
        } else {
            decode_b_macroblocks(
                &mut reader,
                &mut buffer,
                &list0,
                &list1,
                &prediction_weights,
                active_l0_minus1,
                active_l1_minus1,
                direct_spatial_mv_pred,
                sps.direct_8x8_inference,
                picture_order.value,
                &mut luma_qp,
                pps.chroma_qp_index_offset,
                pps.transform_8x8_mode,
                0,
                macroblock_count,
                sps.mb_adaptive_frame_field && structure == PictureStructure::Frame,
            )?;
            reader.finish_rbsp()?;
        }
        buffer.deblock(
            [
                pps.chroma_qp_index_offset,
                pps.second_chroma_qp_index_offset,
            ],
            deblocking,
        )?;
        if structure != PictureStructure::Frame {
            return self.finish_inter_field(
                buffer,
                &sps,
                &pps,
                structure,
                frame_num,
                picture_order,
                reference_marking,
                timing,
            );
        }
        let Some(reference_marking) = reference_marking else {
            return Ok(Some(buffer.into_frame(&sps, timing)?));
        };
        let frame = buffer.to_frame(&sps, timing)?;
        let resets_picture_order = reference_marking.resets_picture_order();
        apply_reference_marking(
            &mut self.references,
            buffer.into_reference(frame_num, picture_order.value),
            sps.max_num_ref_frames,
            sps.log2_max_frame_num,
            reference_marking,
            &mut self.max_long_term_frame_idx,
        )?;
        self.previous_reference_poc = if resets_picture_order {
            picture_order.reset_reference_state
        } else {
            picture_order.reference_state
        };
        Ok(Some(frame))
    }
}

fn active_reference_count(minus_one: u32) -> Result<usize> {
    usize::try_from(minus_one)
        .ok()
        .and_then(|value| value.checked_add(1))
        .ok_or_else(|| Error::InvalidData("H.264 active reference count overflows".into()))
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn decode_p_macroblocks(
    reader: &mut SyntaxReader<'_>,
    buffer: &mut FrameBuffer,
    references: &[&ReferenceFrame],
    active_references_minus1: u32,
    pps: &Pps,
    prediction_weights: PredictionWeights,
    luma_qp: &mut i32,
    first_macroblock: usize,
    end_macroblock: usize,
    mbaff_frame: bool,
) -> Result<()> {
    let macroblocks_wide = buffer.coded_width / 16;
    let mut bitstream_address = first_macroblock;
    let mut field_coded_pair = false;
    while bitstream_address < end_macroblock {
        let skip_run = usize::try_from(reader.ue()?)
            .map_err(|_| Error::InvalidData("H.264 P-slice skip run overflows".into()))?;
        let skip_end = bitstream_address
            .checked_add(skip_run)
            .filter(|end| *end <= end_macroblock)
            .ok_or_else(|| {
                Error::InvalidData(format!(
                    "H.264 P-slice skip run {skip_run} at macroblock {bitstream_address} exceeds slice ending at {end_macroblock}"
                ))
            })?;
        while bitstream_address < skip_end {
            let address = if mbaff_frame {
                mbaff_raster_macroblock_address(bitstream_address, macroblocks_wide)
            } else {
                bitstream_address
            };
            buffer.predict_p_skip(references[0], address, prediction_weights, *luma_qp)?;
            bitstream_address += 1;
        }
        if bitstream_address == end_macroblock {
            break;
        }
        if mbaff_frame && (bitstream_address.is_multiple_of(2) || skip_run != 0) {
            field_coded_pair = reader.bit()?;
            if bitstream_address % 2 == 1 && skip_run != 0 {
                let paired_address =
                    mbaff_raster_macroblock_address(bitstream_address - 1, macroblocks_wide);
                buffer.mbaff_field_coded[paired_address] = field_coded_pair;
            }
        }
        let address = if mbaff_frame {
            mbaff_raster_macroblock_address(bitstream_address, macroblocks_wide)
        } else {
            bitstream_address
        };
        buffer.mbaff_field_coded[address] = field_coded_pair;
        let macroblock_type = reader.ue()?;
        if field_coded_pair && !matches!(macroblock_type, 0..=5 | 30) {
            return Err(Error::Unsupported(format!(
                "native H.264 MBAFF field-coded P macroblock type {macroblock_type} at macroblock {bitstream_address} is not implemented"
            )));
        }
        match macroblock_type {
            0..=4 if field_coded_pair => decode_p_field_l0_macroblock(
                reader,
                buffer,
                references,
                address,
                macroblock_type,
                active_references_minus1,
                luma_qp,
                pps.chroma_qp_index_offset,
                prediction_weights,
            )?,
            0..=4 => decode_p_l0_macroblock(
                reader,
                buffer,
                references,
                address,
                macroblock_type,
                active_references_minus1,
                luma_qp,
                pps.chroma_qp_index_offset,
                prediction_weights,
                pps.transform_8x8_mode,
            )?,
            5 => decode_intra_nxn(
                reader,
                buffer,
                address,
                luma_qp,
                pps.chroma_qp_index_offset,
                pps.transform_8x8_mode,
            )?,
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
                if field_coded_pair {
                    buffer.read_mbaff_field_macroblock(reader, bitstream_address)?;
                } else {
                    buffer.read_macroblock(reader, address)?;
                }
                buffer.mark_pcm(address);
            }
            _ => {
                return Err(Error::Unsupported(format!(
                    "native H.264 reconstruction does not support P-slice macroblock type {macroblock_type} at macroblock {address}"
                )));
            }
        }
        bitstream_address += 1;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn decode_p_field_l0_macroblock(
    reader: &mut SyntaxReader<'_>,
    buffer: &mut FrameBuffer,
    references: &[&ReferenceFrame],
    address: usize,
    macroblock_type: u32,
    active_references_minus1: u32,
    luma_qp: &mut i32,
    chroma_qp_offset: i32,
    prediction_weights: PredictionWeights,
) -> Result<()> {
    let field_active_minus1 = mbaff_field_active_references_minus1(active_references_minus1)?;
    let partitions = read_inter_partitions(reader, macroblock_type)?;
    let reference_partition_count = match macroblock_type {
        0 => 1,
        1 | 2 => 2,
        3 | 4 => 4,
        _ => unreachable!("field P_L0 macroblock type is in 0..=4"),
    };
    let reference_indices = if macroblock_type == 4 {
        vec![0; reference_partition_count]
    } else {
        (0..reference_partition_count)
            .map(|_| read_reference_index(reader, field_active_minus1))
            .collect::<Result<Vec<_>>>()?
    };
    let current_parity = (address / (buffer.coded_width / 16)) % 2;
    for partition in partitions {
        let reference_slot = match macroblock_type {
            0 => 0,
            1 => partition.block_y / 2,
            2 => partition.block_x / 2,
            3 | 4 => (partition.block_y / 2) * 2 + partition.block_x / 2,
            _ => unreachable!("field P_L0 macroblock type is in 0..=4"),
        };
        let field_reference_index = reference_indices[reference_slot];
        let reference = references
            .get(usize::from(field_reference_index / 2))
            .copied()
            .ok_or_else(|| {
                Error::InvalidData(format!(
                    "H.264 MBAFF macroblock {address} selects unavailable field reference {field_reference_index}"
                ))
            })?;
        let predictor = buffer.partition_motion_vector_predictor_mbaff_field(
            address,
            partition.block_x,
            partition.block_y,
            partition.block_width,
            partition.prediction_kind,
            field_reference_index,
        );
        let vector = MotionVector {
            x: predictor.x.checked_add(reader.se()?).ok_or_else(|| {
                Error::InvalidData("H.264 horizontal motion vector overflows".into())
            })?,
            y: predictor.y.checked_add(reader.se()?).ok_or_else(|| {
                Error::InvalidData("H.264 vertical motion vector overflows".into())
            })?,
        };
        let reference_parity = if field_reference_index.is_multiple_of(2) {
            current_parity
        } else {
            1 - current_parity
        };
        buffer.predict_mbaff_field_inter_partition(
            reference,
            address,
            reference_parity,
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
            field_reference_index,
            reference,
        );
    }
    decode_inter_residuals(reader, buffer, address, luma_qp, chroma_qp_offset, false)
}

#[allow(
    clippy::fn_params_excessive_bools,
    clippy::too_many_arguments,
    clippy::too_many_lines
)]
fn decode_b_macroblocks(
    reader: &mut SyntaxReader<'_>,
    buffer: &mut FrameBuffer,
    list0: &[&ReferenceFrame],
    list1: &[&ReferenceFrame],
    prediction_weights: &BPredictionWeights,
    active_l0_minus1: u32,
    active_l1_minus1: u32,
    direct_spatial_mv_pred: bool,
    direct_8x8_inference: bool,
    picture_order_count: i32,
    luma_qp: &mut i32,
    chroma_qp_offset: i32,
    transform_8x8_mode: bool,
    first_macroblock: usize,
    end_macroblock: usize,
    mbaff_frame: bool,
) -> Result<()> {
    let macroblocks_wide = buffer.coded_width / 16;
    let mut bitstream_address = first_macroblock;
    let mut field_coded_pair = false;
    while bitstream_address < end_macroblock {
        let skip_run = usize::try_from(reader.ue()?)
            .map_err(|_| Error::InvalidData("H.264 B-slice skip run overflows".into()))?;
        let skip_end = bitstream_address
            .checked_add(skip_run)
            .filter(|end| *end <= end_macroblock)
            .ok_or_else(|| Error::InvalidData("H.264 B-slice skip run exceeds slice".into()))?;
        while bitstream_address < skip_end {
            let address = if mbaff_frame {
                mbaff_raster_macroblock_address(bitstream_address, macroblocks_wide)
            } else {
                bitstream_address
            };
            decode_spatial_direct_macroblock(
                buffer,
                list0,
                list1,
                prediction_weights,
                address,
                direct_spatial_mv_pred,
                direct_8x8_inference,
                picture_order_count,
            )?;
            buffer.set_luma_qp(address, *luma_qp);
            bitstream_address += 1;
        }
        if bitstream_address == end_macroblock {
            break;
        }
        if mbaff_frame && (bitstream_address.is_multiple_of(2) || skip_run != 0) {
            field_coded_pair = reader.bit()?;
        }
        let address = if mbaff_frame {
            mbaff_raster_macroblock_address(bitstream_address, macroblocks_wide)
        } else {
            bitstream_address
        };
        buffer.mbaff_field_coded[address] = field_coded_pair;
        let macroblock_type = reader.ue()?;
        if field_coded_pair && macroblock_type == 0 {
            decode_mbaff_field_direct_macroblock(
                buffer,
                list0,
                list1,
                prediction_weights,
                address,
                direct_spatial_mv_pred,
            )?;
            decode_inter_residuals(reader, buffer, address, luma_qp, chroma_qp_offset, false)?;
            bitstream_address += 1;
            continue;
        }
        if field_coded_pair && (1..=21).contains(&macroblock_type) {
            decode_mbaff_field_b_inter_macroblock(
                reader,
                buffer,
                list0,
                list1,
                prediction_weights,
                address,
                macroblock_type,
                active_l0_minus1,
                active_l1_minus1,
            )?;
            decode_inter_residuals(reader, buffer, address, luma_qp, chroma_qp_offset, false)?;
            bitstream_address += 1;
            continue;
        }
        if field_coded_pair && macroblock_type == 22 {
            decode_mbaff_field_b8x8_macroblock(
                reader,
                buffer,
                list0,
                list1,
                prediction_weights,
                address,
                active_l0_minus1,
                active_l1_minus1,
                direct_spatial_mv_pred,
                direct_8x8_inference,
            )?;
            decode_inter_residuals(reader, buffer, address, luma_qp, chroma_qp_offset, false)?;
            bitstream_address += 1;
            continue;
        }
        if field_coded_pair {
            return Err(Error::Unsupported(format!(
                "native H.264 MBAFF field-coded B macroblock type {macroblock_type} at macroblock {bitstream_address} is not implemented"
            )));
        }
        if macroblock_type == 0 {
            decode_spatial_direct_macroblock(
                buffer,
                list0,
                list1,
                prediction_weights,
                address,
                direct_spatial_mv_pred,
                direct_8x8_inference,
                picture_order_count,
            )?;
            decode_inter_residuals(
                reader,
                buffer,
                address,
                luma_qp,
                chroma_qp_offset,
                transform_8x8_mode && direct_8x8_inference,
            )?;
            bitstream_address += 1;
            continue;
        }
        if macroblock_type == 22 {
            let transform_allowed = decode_b8x8_macroblock(
                reader,
                buffer,
                list0,
                list1,
                prediction_weights,
                address,
                active_l0_minus1,
                active_l1_minus1,
                direct_spatial_mv_pred,
                direct_8x8_inference,
                picture_order_count,
            )?;
            decode_inter_residuals(
                reader,
                buffer,
                address,
                luma_qp,
                chroma_qp_offset,
                transform_8x8_mode && transform_allowed,
            )?;
            bitstream_address += 1;
            continue;
        }
        if !(1..=21).contains(&macroblock_type) {
            return Err(Error::Unsupported(format!(
                "native H.264 reconstruction does not support B-slice macroblock type {macroblock_type} at macroblock {address}"
            )));
        }
        let partitions = b_inter_partitions(macroblock_type);
        let reference_indices_l0 = partitions
            .iter()
            .map(|(_, prediction)| {
                prediction
                    .uses_l0()
                    .then(|| read_reference_index(reader, active_l0_minus1))
                    .transpose()
            })
            .collect::<Result<Vec<_>>>()?;
        let reference_indices_l1 = partitions
            .iter()
            .map(|(_, prediction)| {
                prediction
                    .uses_l1()
                    .then(|| read_reference_index(reader, active_l1_minus1))
                    .transpose()
            })
            .collect::<Result<Vec<_>>>()?;
        let differences_l0 = partitions
            .iter()
            .map(|(_, prediction)| {
                prediction
                    .uses_l0()
                    .then(|| read_motion_difference(reader))
                    .transpose()
            })
            .collect::<Result<Vec<_>>>()?;
        let differences_l1 = partitions
            .iter()
            .map(|(_, prediction)| {
                prediction
                    .uses_l1()
                    .then(|| read_motion_difference(reader))
                    .transpose()
            })
            .collect::<Result<Vec<_>>>()?;
        for (partition_index, (partition, _)) in partitions.into_iter().enumerate() {
            let motion_l0 = reference_indices_l0[partition_index]
                .zip(differences_l0[partition_index])
                .map(|(reference_index, difference)| {
                    let predictor = buffer.partition_motion_vector_predictor(
                        address,
                        partition.block_x,
                        partition.block_y,
                        partition.block_width,
                        partition.prediction_kind,
                        reference_index,
                    );
                    checked_motion_sum(predictor, difference)
                        .map(|vector| (reference_index, vector))
                })
                .transpose()?;
            let motion_l1 = reference_indices_l1[partition_index]
                .zip(differences_l1[partition_index])
                .map(|(reference_index, difference)| {
                    let predictor = buffer.partition_motion_vector_predictor_l1(
                        address,
                        partition.block_x,
                        partition.block_y,
                        partition.block_width,
                        partition.prediction_kind,
                        reference_index,
                    );
                    checked_motion_sum(predictor, difference)
                        .map(|vector| (reference_index, vector))
                })
                .transpose()?;
            predict_b_partition(
                buffer,
                list0,
                list1,
                prediction_weights,
                address,
                partition,
                motion_l0,
                motion_l1,
            )?;
        }
        decode_inter_residuals(
            reader,
            buffer,
            address,
            luma_qp,
            chroma_qp_offset,
            transform_8x8_mode,
        )?;
        bitstream_address += 1;
    }
    Ok(())
}

fn decode_mbaff_field_direct_macroblock(
    buffer: &mut FrameBuffer,
    list0: &[&ReferenceFrame],
    list1: &[&ReferenceFrame],
    prediction_weights: &BPredictionWeights,
    address: usize,
    direct_spatial_mv_pred: bool,
) -> Result<()> {
    decode_mbaff_field_direct_partition(
        buffer,
        list0,
        list1,
        prediction_weights,
        address,
        InterPartition::new(0, 0, 4, 4, MotionPredictionKind::Normal),
        direct_spatial_mv_pred,
    )
}

fn decode_mbaff_field_direct_partition(
    buffer: &mut FrameBuffer,
    list0: &[&ReferenceFrame],
    list1: &[&ReferenceFrame],
    prediction_weights: &BPredictionWeights,
    address: usize,
    partition: InterPartition,
    direct_spatial_mv_pred: bool,
) -> Result<()> {
    if !direct_spatial_mv_pred {
        return Err(Error::Unsupported(
            "native H.264 MBAFF field-coded temporal-direct prediction is not implemented".into(),
        ));
    }
    let mut index0 = buffer.spatial_direct_reference_index_mbaff_field(address, false);
    let mut index1 = buffer.spatial_direct_reference_index_mbaff_field(address, true);
    if index0.is_none() && index1.is_none() {
        index0 = Some(0);
        index1 = Some(0);
    }
    let (colocated_x, colocated_y) = direct_colocated_block(partition);
    let colocated_zero = list1
        .first()
        .is_some_and(|reference| reference.colocated_zero(address, colocated_x, colocated_y));
    let motion0 = index0.map(|index| {
        let vector = buffer.partition_motion_vector_predictor_mbaff_field(
            address,
            0,
            0,
            4,
            MotionPredictionKind::Normal,
            index,
        );
        (
            index,
            if colocated_zero && index == 0 {
                MotionVector::default()
            } else {
                vector
            },
        )
    });
    let motion1 = index1.map(|index| {
        let vector = buffer.partition_motion_vector_predictor_mbaff_field_l1(
            address,
            0,
            0,
            4,
            MotionPredictionKind::Normal,
            index,
        );
        (
            index,
            if colocated_zero && index == 0 {
                MotionVector::default()
            } else {
                vector
            },
        )
    });
    predict_mbaff_field_b_partition(
        buffer,
        list0,
        list1,
        prediction_weights,
        address,
        partition,
        motion0,
        motion1,
    )
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn decode_mbaff_field_b_inter_macroblock(
    reader: &mut SyntaxReader<'_>,
    buffer: &mut FrameBuffer,
    list0: &[&ReferenceFrame],
    list1: &[&ReferenceFrame],
    prediction_weights: &BPredictionWeights,
    address: usize,
    macroblock_type: u32,
    active_l0_minus1: u32,
    active_l1_minus1: u32,
) -> Result<()> {
    let field_active_l0_minus1 = mbaff_field_active_references_minus1(active_l0_minus1)?;
    let field_active_l1_minus1 = mbaff_field_active_references_minus1(active_l1_minus1)?;
    let partitions = b_inter_partitions(macroblock_type);
    let reference_indices_l0 = partitions
        .iter()
        .map(|(_, prediction)| {
            prediction
                .uses_l0()
                .then(|| read_reference_index(reader, field_active_l0_minus1))
                .transpose()
        })
        .collect::<Result<Vec<_>>>()?;
    let reference_indices_l1 = partitions
        .iter()
        .map(|(_, prediction)| {
            prediction
                .uses_l1()
                .then(|| read_reference_index(reader, field_active_l1_minus1))
                .transpose()
        })
        .collect::<Result<Vec<_>>>()?;
    let differences_l0 = partitions
        .iter()
        .map(|(_, prediction)| {
            prediction
                .uses_l0()
                .then(|| read_motion_difference(reader))
                .transpose()
        })
        .collect::<Result<Vec<_>>>()?;
    let differences_l1 = partitions
        .iter()
        .map(|(_, prediction)| {
            prediction
                .uses_l1()
                .then(|| read_motion_difference(reader))
                .transpose()
        })
        .collect::<Result<Vec<_>>>()?;
    for (partition_index, (partition, _)) in partitions.into_iter().enumerate() {
        let motion_l0 = reference_indices_l0[partition_index]
            .zip(differences_l0[partition_index])
            .map(|(reference_index, difference)| {
                let predictor = buffer.partition_motion_vector_predictor_mbaff_field(
                    address,
                    partition.block_x,
                    partition.block_y,
                    partition.block_width,
                    partition.prediction_kind,
                    reference_index,
                );
                checked_motion_sum(predictor, difference).map(|vector| (reference_index, vector))
            })
            .transpose()?;
        let motion_l1 = reference_indices_l1[partition_index]
            .zip(differences_l1[partition_index])
            .map(|(reference_index, difference)| {
                let predictor = buffer.partition_motion_vector_predictor_mbaff_field_l1(
                    address,
                    partition.block_x,
                    partition.block_y,
                    partition.block_width,
                    partition.prediction_kind,
                    reference_index,
                );
                checked_motion_sum(predictor, difference).map(|vector| (reference_index, vector))
            })
            .transpose()?;
        predict_mbaff_field_b_partition(
            buffer,
            list0,
            list1,
            prediction_weights,
            address,
            partition,
            motion_l0,
            motion_l1,
        )?;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn decode_mbaff_field_b8x8_macroblock(
    reader: &mut SyntaxReader<'_>,
    buffer: &mut FrameBuffer,
    list0: &[&ReferenceFrame],
    list1: &[&ReferenceFrame],
    prediction_weights: &BPredictionWeights,
    address: usize,
    active_l0_minus1: u32,
    active_l1_minus1: u32,
    direct_spatial_mv_pred: bool,
    direct_8x8_inference: bool,
) -> Result<()> {
    let sub_macroblocks = (0..4)
        .map(|index| b_sub_macroblock(index, reader.ue()?, direct_8x8_inference))
        .collect::<Result<Vec<_>>>()?;
    let field_active_l0_minus1 = mbaff_field_active_references_minus1(active_l0_minus1)?;
    let field_active_l1_minus1 = mbaff_field_active_references_minus1(active_l1_minus1)?;
    let reference_indices_l0 = sub_macroblocks
        .iter()
        .map(|sub| {
            sub.prediction
                .uses_l0()
                .then(|| read_reference_index(reader, field_active_l0_minus1))
                .transpose()
        })
        .collect::<Result<Vec<_>>>()?;
    let reference_indices_l1 = sub_macroblocks
        .iter()
        .map(|sub| {
            sub.prediction
                .uses_l1()
                .then(|| read_reference_index(reader, field_active_l1_minus1))
                .transpose()
        })
        .collect::<Result<Vec<_>>>()?;
    let differences_l0 = read_b_sub_motion_differences(reader, &sub_macroblocks, false)?;
    let differences_l1 = read_b_sub_motion_differences(reader, &sub_macroblocks, true)?;
    for (sub_index, sub) in sub_macroblocks.into_iter().enumerate() {
        for (partition_index, partition) in sub.partitions.into_iter().enumerate() {
            if matches!(sub.prediction, BPrediction::Direct) {
                decode_mbaff_field_direct_partition(
                    buffer,
                    list0,
                    list1,
                    prediction_weights,
                    address,
                    partition,
                    direct_spatial_mv_pred,
                )?;
                continue;
            }
            let motion_l0 = reference_indices_l0[sub_index]
                .zip(differences_l0[sub_index].get(partition_index).copied())
                .map(|(reference_index, difference)| {
                    let predictor = buffer.partition_motion_vector_predictor_mbaff_field(
                        address,
                        partition.block_x,
                        partition.block_y,
                        partition.block_width,
                        partition.prediction_kind,
                        reference_index,
                    );
                    checked_motion_sum(predictor, difference)
                        .map(|vector| (reference_index, vector))
                })
                .transpose()?;
            let motion_l1 = reference_indices_l1[sub_index]
                .zip(differences_l1[sub_index].get(partition_index).copied())
                .map(|(reference_index, difference)| {
                    let predictor = buffer.partition_motion_vector_predictor_mbaff_field_l1(
                        address,
                        partition.block_x,
                        partition.block_y,
                        partition.block_width,
                        partition.prediction_kind,
                        reference_index,
                    );
                    checked_motion_sum(predictor, difference)
                        .map(|vector| (reference_index, vector))
                })
                .transpose()?;
            predict_mbaff_field_b_partition(
                buffer,
                list0,
                list1,
                prediction_weights,
                address,
                partition,
                motion_l0,
                motion_l1,
            )?;
        }
    }
    Ok(())
}

fn mbaff_field_active_references_minus1(frame_active_minus1: u32) -> Result<u32> {
    frame_active_minus1
        .checked_add(1)
        .and_then(|count| count.checked_mul(2))
        .and_then(|count| count.checked_sub(1))
        .ok_or_else(|| Error::InvalidData("H.264 MBAFF active reference count overflows".into()))
}

fn mbaff_field_reference<'a>(
    references: &[&'a ReferenceFrame],
    field_index: u8,
    current_parity: usize,
    address: usize,
    list_name: &str,
) -> Result<(&'a ReferenceFrame, u8, usize)> {
    let frame_index = field_index / 2;
    let reference = reference_from_list(references, frame_index, address, list_name)?;
    let parity = if field_index.is_multiple_of(2) {
        current_parity
    } else {
        1 - current_parity
    };
    Ok((reference, frame_index, parity))
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn predict_mbaff_field_b_partition(
    buffer: &mut FrameBuffer,
    list0: &[&ReferenceFrame],
    list1: &[&ReferenceFrame],
    prediction_weights: &BPredictionWeights,
    address: usize,
    partition: InterPartition,
    motion_l0: Option<(u8, MotionVector)>,
    motion_l1: Option<(u8, MotionVector)>,
) -> Result<()> {
    let current_parity = (address / (buffer.coded_width / 16)) % 2;
    match (motion_l0, motion_l1) {
        (Some((field_index0, vector0)), Some((field_index1, vector1))) => {
            let (reference0, frame_index0, parity0) =
                mbaff_field_reference(list0, field_index0, current_parity, address, "list-0")?;
            let (reference1, frame_index1, parity1) =
                mbaff_field_reference(list1, field_index1, current_parity, address, "list-1")?;
            let (weights0, weights1, weighted) = if prediction_weights.implicit {
                let (weights0, weights1) = implicit_b_prediction_weights(
                    reference0,
                    reference1,
                    prediction_weights.picture_order_count,
                );
                (weights0, weights1, true)
            } else {
                (
                    prediction_weights.list0(frame_index0)?,
                    prediction_weights.list1(frame_index1)?,
                    prediction_weights.explicit,
                )
            };
            buffer.predict_mbaff_field_bi_inter_partition(
                reference0,
                reference1,
                address,
                parity0,
                parity1,
                partition.block_x,
                partition.block_y,
                partition.block_width,
                partition.block_height,
                vector0,
                vector1,
                weights0,
                weights1,
                weighted,
            )?;
            buffer.set_partition_motion(
                address,
                partition.block_x,
                partition.block_y,
                partition.block_width,
                partition.block_height,
                vector0,
                field_index0,
                reference0,
            );
            buffer.set_partition_motion_l1(
                address,
                partition.block_x,
                partition.block_y,
                partition.block_width,
                partition.block_height,
                vector1,
                field_index1,
                reference1,
            );
        }
        (Some((field_index, vector)), None) => {
            let (reference, frame_index, parity) =
                mbaff_field_reference(list0, field_index, current_parity, address, "list-0")?;
            buffer.predict_mbaff_field_inter_partition(
                reference,
                address,
                parity,
                partition.block_x,
                partition.block_y,
                partition.block_width,
                partition.block_height,
                vector,
                prediction_weights.list0(frame_index)?,
            )?;
            buffer.set_partition_motion(
                address,
                partition.block_x,
                partition.block_y,
                partition.block_width,
                partition.block_height,
                vector,
                field_index,
                reference,
            );
            buffer.set_partition_motion_unused(address, partition, true);
        }
        (None, Some((field_index, vector))) => {
            let (reference, frame_index, parity) =
                mbaff_field_reference(list1, field_index, current_parity, address, "list-1")?;
            buffer.predict_mbaff_field_inter_partition(
                reference,
                address,
                parity,
                partition.block_x,
                partition.block_y,
                partition.block_width,
                partition.block_height,
                vector,
                prediction_weights.list1(frame_index)?,
            )?;
            buffer.set_partition_motion_unused(address, partition, false);
            buffer.set_partition_motion_l1(
                address,
                partition.block_x,
                partition.block_y,
                partition.block_width,
                partition.block_height,
                vector,
                field_index,
                reference,
            );
        }
        (None, None) => {
            return Err(Error::InvalidData(
                "H.264 field B prediction has no usable reference list".into(),
            ));
        }
    }
    Ok(())
}

#[derive(Clone, Copy)]
enum BPrediction {
    Direct,
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

fn b_inter_partitions(macroblock_type: u32) -> Vec<(InterPartition, BPrediction)> {
    let single = match macroblock_type {
        1 => Some(BPrediction::L0),
        2 => Some(BPrediction::L1),
        3 => Some(BPrediction::Bi),
        _ => None,
    };
    if let Some(prediction) = single {
        return vec![(
            InterPartition::new(0, 0, 4, 4, MotionPredictionKind::Normal),
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
    let offset = usize::try_from(macroblock_type - 4).expect("B type in 4..=21 fits usize");
    let predictions = combinations[offset / 2];
    if offset.is_multiple_of(2) {
        vec![
            (
                InterPartition::new(0, 0, 4, 2, MotionPredictionKind::Top16x8),
                predictions[0],
            ),
            (
                InterPartition::new(0, 2, 4, 2, MotionPredictionKind::Bottom16x8),
                predictions[1],
            ),
        ]
    } else {
        vec![
            (
                InterPartition::new(0, 0, 2, 4, MotionPredictionKind::Left8x16),
                predictions[0],
            ),
            (
                InterPartition::new(2, 0, 2, 4, MotionPredictionKind::Right8x16),
                predictions[1],
            ),
        ]
    }
}

struct BSubMacroblock {
    prediction: BPrediction,
    partitions: Vec<InterPartition>,
}

#[allow(clippy::too_many_arguments)]
fn decode_b8x8_macroblock(
    reader: &mut SyntaxReader<'_>,
    buffer: &mut FrameBuffer,
    list0: &[&ReferenceFrame],
    list1: &[&ReferenceFrame],
    prediction_weights: &BPredictionWeights,
    address: usize,
    active_l0_minus1: u32,
    active_l1_minus1: u32,
    direct_spatial_mv_pred: bool,
    direct_8x8_inference: bool,
    picture_order_count: i32,
) -> Result<bool> {
    let subtypes = (0..4).map(|_| reader.ue()).collect::<Result<Vec<_>>>()?;
    let sub_macroblocks = subtypes
        .into_iter()
        .enumerate()
        .map(|(index, subtype)| b_sub_macroblock(index, subtype, direct_8x8_inference))
        .collect::<Result<Vec<_>>>()?;
    let transform_allowed = sub_macroblocks.iter().all(|sub| {
        sub.partitions
            .iter()
            .all(|partition| partition.block_width >= 2 && partition.block_height >= 2)
    });
    let reference_indices_l0 = sub_macroblocks
        .iter()
        .map(|sub| {
            sub.prediction
                .uses_l0()
                .then(|| read_reference_index(reader, active_l0_minus1))
                .transpose()
        })
        .collect::<Result<Vec<_>>>()?;
    let reference_indices_l1 = sub_macroblocks
        .iter()
        .map(|sub| {
            sub.prediction
                .uses_l1()
                .then(|| read_reference_index(reader, active_l1_minus1))
                .transpose()
        })
        .collect::<Result<Vec<_>>>()?;
    let differences_l0 = read_b_sub_motion_differences(reader, &sub_macroblocks, false)?;
    let differences_l1 = read_b_sub_motion_differences(reader, &sub_macroblocks, true)?;
    for (sub_index, sub) in sub_macroblocks.into_iter().enumerate() {
        for (partition_index, partition) in sub.partitions.into_iter().enumerate() {
            if matches!(sub.prediction, BPrediction::Direct) {
                decode_spatial_direct_partition(
                    buffer,
                    list0,
                    list1,
                    prediction_weights,
                    address,
                    partition,
                    direct_spatial_mv_pred,
                    picture_order_count,
                )?;
                continue;
            }
            let motion_l0 = reference_indices_l0[sub_index]
                .zip(differences_l0[sub_index].get(partition_index).copied())
                .map(|(reference_index, difference)| {
                    let predictor = buffer.partition_motion_vector_predictor(
                        address,
                        partition.block_x,
                        partition.block_y,
                        partition.block_width,
                        partition.prediction_kind,
                        reference_index,
                    );
                    checked_motion_sum(predictor, difference)
                        .map(|vector| (reference_index, vector))
                })
                .transpose()?;
            let motion_l1 = reference_indices_l1[sub_index]
                .zip(differences_l1[sub_index].get(partition_index).copied())
                .map(|(reference_index, difference)| {
                    let predictor = buffer.partition_motion_vector_predictor_l1(
                        address,
                        partition.block_x,
                        partition.block_y,
                        partition.block_width,
                        partition.prediction_kind,
                        reference_index,
                    );
                    checked_motion_sum(predictor, difference)
                        .map(|vector| (reference_index, vector))
                })
                .transpose()?;
            predict_b_partition(
                buffer,
                list0,
                list1,
                prediction_weights,
                address,
                partition,
                motion_l0,
                motion_l1,
            )?;
        }
    }
    Ok(transform_allowed)
}

fn read_b_sub_motion_differences(
    reader: &mut SyntaxReader<'_>,
    sub_macroblocks: &[BSubMacroblock],
    list1: bool,
) -> Result<Vec<Vec<MotionVector>>> {
    sub_macroblocks
        .iter()
        .map(|sub| {
            let uses_list = if list1 {
                sub.prediction.uses_l1()
            } else {
                sub.prediction.uses_l0()
            };
            if uses_list {
                (0..sub.partitions.len())
                    .map(|_| read_motion_difference(reader))
                    .collect()
            } else {
                Ok(Vec::new())
            }
        })
        .collect()
}

fn b_sub_macroblock(
    index: usize,
    sub_type: u32,
    direct_8x8_inference: bool,
) -> Result<BSubMacroblock> {
    let (prediction, shape) = match sub_type {
        0 => (
            BPrediction::Direct,
            if direct_8x8_inference { 0 } else { 3 },
        ),
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
        _ => {
            return Err(Error::InvalidData(format!(
                "invalid H.264 B sub-macroblock type {sub_type}"
            )));
        }
    };
    let base_x = (index % 2) * 2;
    let base_y = (index / 2) * 2;
    let partitions = match shape {
        0 => vec![InterPartition::new(
            base_x,
            base_y,
            2,
            2,
            MotionPredictionKind::Normal,
        )],
        1 => vec![
            InterPartition::new(base_x, base_y, 2, 1, MotionPredictionKind::Normal),
            InterPartition::new(base_x, base_y + 1, 2, 1, MotionPredictionKind::Normal),
        ],
        2 => vec![
            InterPartition::new(base_x, base_y, 1, 2, MotionPredictionKind::Normal),
            InterPartition::new(base_x + 1, base_y, 1, 2, MotionPredictionKind::Normal),
        ],
        3 => (0..2)
            .flat_map(|y| {
                (0..2).map(move |x| {
                    InterPartition::new(base_x + x, base_y + y, 1, 1, MotionPredictionKind::Normal)
                })
            })
            .collect(),
        _ => unreachable!("validated B sub-macroblock shape"),
    };
    Ok(BSubMacroblock {
        prediction,
        partitions,
    })
}

#[allow(clippy::too_many_arguments)]
fn decode_spatial_direct_macroblock(
    buffer: &mut FrameBuffer,
    list0: &[&ReferenceFrame],
    list1: &[&ReferenceFrame],
    prediction_weights: &BPredictionWeights,
    address: usize,
    direct_spatial_mv_pred: bool,
    direct_8x8_inference: bool,
    picture_order_count: i32,
) -> Result<()> {
    if direct_spatial_mv_pred {
        let spatial_motion = spatial_direct_motion(buffer, address);
        for sub_index in 0..4 {
            let (partitions, count) = direct_sub_partitions(sub_index, direct_8x8_inference);
            for partition in partitions.into_iter().take(count) {
                predict_spatial_direct_partition(
                    buffer,
                    list0,
                    list1,
                    prediction_weights,
                    address,
                    partition,
                    spatial_motion,
                )?;
            }
        }
        return Ok(());
    }
    for sub_index in 0..4 {
        let (partitions, count) = direct_sub_partitions(sub_index, direct_8x8_inference);
        for partition in partitions.into_iter().take(count) {
            decode_temporal_direct_partition(
                buffer,
                list0,
                list1,
                prediction_weights,
                address,
                partition,
                picture_order_count,
            )?;
        }
    }
    Ok(())
}

fn direct_sub_partitions(
    sub_index: usize,
    direct_8x8_inference: bool,
) -> ([InterPartition; 4], usize) {
    let base_x = (sub_index % 2) * 2;
    let base_y = (sub_index / 2) * 2;
    let whole = InterPartition::new(base_x, base_y, 2, 2, MotionPredictionKind::Normal);
    if direct_8x8_inference {
        return ([whole; 4], 1);
    }
    (
        [
            InterPartition::new(base_x, base_y, 1, 1, MotionPredictionKind::Normal),
            InterPartition::new(base_x + 1, base_y, 1, 1, MotionPredictionKind::Normal),
            InterPartition::new(base_x, base_y + 1, 1, 1, MotionPredictionKind::Normal),
            InterPartition::new(base_x + 1, base_y + 1, 1, 1, MotionPredictionKind::Normal),
        ],
        4,
    )
}

type SpatialDirectMotion = (Option<(u8, MotionVector)>, Option<(u8, MotionVector)>);

fn spatial_direct_motion(buffer: &FrameBuffer, address: usize) -> SpatialDirectMotion {
    let mut index0 = buffer.spatial_direct_reference_index(address, 0, 0, 4, false);
    let mut index1 = buffer.spatial_direct_reference_index(address, 0, 0, 4, true);
    if index0.is_none() && index1.is_none() {
        index0 = Some(0);
        index1 = Some(0);
    }
    let motion0 = index0.map(|index| {
        (
            index,
            buffer.partition_motion_vector_predictor(
                address,
                0,
                0,
                4,
                MotionPredictionKind::Normal,
                index,
            ),
        )
    });
    let motion1 = index1.map(|index| {
        (
            index,
            buffer.partition_motion_vector_predictor_l1(
                address,
                0,
                0,
                4,
                MotionPredictionKind::Normal,
                index,
            ),
        )
    });
    (motion0, motion1)
}

#[allow(clippy::too_many_arguments)]
fn decode_spatial_direct_partition(
    buffer: &mut FrameBuffer,
    list0: &[&ReferenceFrame],
    list1: &[&ReferenceFrame],
    prediction_weights: &BPredictionWeights,
    address: usize,
    partition: InterPartition,
    direct_spatial_mv_pred: bool,
    picture_order_count: i32,
) -> Result<()> {
    if !direct_spatial_mv_pred {
        return decode_temporal_direct_partition(
            buffer,
            list0,
            list1,
            prediction_weights,
            address,
            partition,
            picture_order_count,
        );
    }
    let spatial_motion = spatial_direct_motion(buffer, address);
    predict_spatial_direct_partition(
        buffer,
        list0,
        list1,
        prediction_weights,
        address,
        partition,
        spatial_motion,
    )
}

#[allow(clippy::too_many_arguments)]
fn predict_spatial_direct_partition(
    buffer: &mut FrameBuffer,
    list0: &[&ReferenceFrame],
    list1: &[&ReferenceFrame],
    prediction_weights: &BPredictionWeights,
    address: usize,
    partition: InterPartition,
    spatial_motion: SpatialDirectMotion,
) -> Result<()> {
    let (colocated_x, colocated_y) = direct_colocated_block(partition);
    let colocated_zero = list1
        .first()
        .is_some_and(|reference| reference.colocated_zero(address, colocated_x, colocated_y));
    let motion0 = spatial_motion.0.map(|(index, vector)| {
        (
            index,
            if colocated_zero && index == 0 {
                MotionVector::default()
            } else {
                vector
            },
        )
    });
    let motion1 = spatial_motion.1.map(|(index, vector)| {
        (
            index,
            if colocated_zero && index == 0 {
                MotionVector::default()
            } else {
                vector
            },
        )
    });
    predict_b_partition(
        buffer,
        list0,
        list1,
        prediction_weights,
        address,
        partition,
        motion0,
        motion1,
    )
}

fn decode_temporal_direct_partition(
    buffer: &mut FrameBuffer,
    list0: &[&ReferenceFrame],
    list1: &[&ReferenceFrame],
    prediction_weights: &BPredictionWeights,
    address: usize,
    partition: InterPartition,
    picture_order_count: i32,
) -> Result<()> {
    let colocated = list1.first().copied().ok_or_else(|| {
        Error::InvalidData("H.264 temporal-direct prediction has no colocated picture".into())
    })?;
    let (colocated_x, colocated_y) = direct_colocated_block(partition);
    let Some((colocated_motion, colocated_reference)) =
        colocated.colocated_motion(address, colocated_x, colocated_y)
    else {
        return predict_b_partition(
            buffer,
            list0,
            list1,
            prediction_weights,
            address,
            partition,
            Some((0, MotionVector::default())),
            Some((0, MotionVector::default())),
        );
    };
    let reference_index = list0
        .iter()
        .position(|reference| reference.pic_order_count == colocated_reference.pic_order_count)
        .and_then(|index| u8::try_from(index).ok())
        .ok_or_else(|| {
            Error::Unsupported(
                "temporal-direct colocated reference is absent from the current list-0".into(),
            )
        })?;
    let (motion0, motion1) = temporal_direct_motion_vectors(
        colocated_motion,
        picture_order_count,
        colocated.pic_order_count,
        colocated_reference,
    )?;
    predict_b_partition(
        buffer,
        list0,
        list1,
        prediction_weights,
        address,
        partition,
        Some((reference_index, motion0)),
        Some((0, motion1)),
    )
}

fn direct_colocated_block(partition: InterPartition) -> (usize, usize) {
    let representative =
        |coordinate: usize, size: usize| coordinate + usize::from(size == 2 && coordinate != 0);
    (
        representative(partition.block_x, partition.block_width),
        representative(partition.block_y, partition.block_height),
    )
}

fn temporal_direct_motion_vectors(
    colocated: MotionVector,
    picture_order_count: i32,
    colocated_picture_order_count: i32,
    reference: MotionReference,
) -> Result<(MotionVector, MotionVector)> {
    let td = (i64::from(colocated_picture_order_count) - i64::from(reference.pic_order_count))
        .clamp(-128, 127);
    let tb =
        (i64::from(picture_order_count) - i64::from(reference.pic_order_count)).clamp(-128, 127);
    let scale = if reference.long_term || td == 0 {
        256
    } else {
        let tx = (16_384 + td.abs() / 2) / td;
        ((tb * tx + 32) >> 6).clamp(-1_024, 1_023)
    };
    let scale_component = |component: i32| {
        i32::try_from((scale * i64::from(component) + 128) >> 8)
            .map_err(|_| Error::InvalidData("H.264 temporal-direct motion vector overflows".into()))
    };
    let list0 = MotionVector {
        x: scale_component(colocated.x)?,
        y: scale_component(colocated.y)?,
    };
    let list1 = MotionVector {
        x: list0.x.checked_sub(colocated.x).ok_or_else(|| {
            Error::InvalidData("H.264 temporal-direct horizontal motion overflows".into())
        })?,
        y: list0.y.checked_sub(colocated.y).ok_or_else(|| {
            Error::InvalidData("H.264 temporal-direct vertical motion overflows".into())
        })?,
    };
    Ok((list0, list1))
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn predict_b_partition(
    buffer: &mut FrameBuffer,
    list0: &[&ReferenceFrame],
    list1: &[&ReferenceFrame],
    prediction_weights: &BPredictionWeights,
    address: usize,
    partition: InterPartition,
    motion_l0: Option<(u8, MotionVector)>,
    motion_l1: Option<(u8, MotionVector)>,
) -> Result<()> {
    match (motion_l0, motion_l1) {
        (Some((index0, vector0)), Some((index1, vector1))) => {
            let reference0 = reference_from_list(list0, index0, address, "list-0")?;
            let reference1 = reference_from_list(list1, index1, address, "list-1")?;
            let (weights0, weights1, weighted) = if prediction_weights.implicit {
                let (weights0, weights1) = implicit_b_prediction_weights(
                    reference0,
                    reference1,
                    prediction_weights.picture_order_count,
                );
                (weights0, weights1, true)
            } else {
                (
                    prediction_weights.list0(index0)?,
                    prediction_weights.list1(index1)?,
                    prediction_weights.explicit,
                )
            };
            buffer.predict_bi_inter_partition(
                reference0,
                reference1,
                address,
                partition.block_x,
                partition.block_y,
                partition.block_width,
                partition.block_height,
                vector0,
                vector1,
                weights0,
                weights1,
                weighted,
            )?;
            buffer.set_partition_motion(
                address,
                partition.block_x,
                partition.block_y,
                partition.block_width,
                partition.block_height,
                vector0,
                index0,
                reference0,
            );
            buffer.set_partition_motion_l1(
                address,
                partition.block_x,
                partition.block_y,
                partition.block_width,
                partition.block_height,
                vector1,
                index1,
                reference1,
            );
        }
        (Some((index, vector)), None) => {
            let reference = reference_from_list(list0, index, address, "list-0")?;
            buffer.predict_inter_partition(
                reference,
                address,
                partition.block_x,
                partition.block_y,
                partition.block_width,
                partition.block_height,
                vector,
                prediction_weights.list0(index)?,
            )?;
            buffer.set_partition_motion(
                address,
                partition.block_x,
                partition.block_y,
                partition.block_width,
                partition.block_height,
                vector,
                index,
                reference,
            );
            buffer.set_partition_motion_unused(address, partition, true);
        }
        (None, Some((index, vector))) => {
            let reference = reference_from_list(list1, index, address, "list-1")?;
            buffer.predict_inter_partition(
                reference,
                address,
                partition.block_x,
                partition.block_y,
                partition.block_width,
                partition.block_height,
                vector,
                prediction_weights.list1(index)?,
            )?;
            buffer.set_partition_motion_unused(address, partition, false);
            buffer.set_partition_motion_l1(
                address,
                partition.block_x,
                partition.block_y,
                partition.block_width,
                partition.block_height,
                vector,
                index,
                reference,
            );
        }
        (None, None) => {
            return Err(Error::InvalidData(
                "H.264 B prediction has no usable reference list".into(),
            ));
        }
    }
    Ok(())
}

fn read_motion_difference(reader: &mut SyntaxReader<'_>) -> Result<MotionVector> {
    Ok(MotionVector {
        x: reader.se()?,
        y: reader.se()?,
    })
}

fn checked_motion_sum(predictor: MotionVector, difference: MotionVector) -> Result<MotionVector> {
    Ok(MotionVector {
        x: predictor
            .x
            .checked_add(difference.x)
            .ok_or_else(|| Error::InvalidData("H.264 horizontal motion vector overflows".into()))?,
        y: predictor
            .y
            .checked_add(difference.y)
            .ok_or_else(|| Error::InvalidData("H.264 vertical motion vector overflows".into()))?,
    })
}

fn average_samples(left: u8, right: u8) -> u8 {
    let average = (u16::from(left) + u16::from(right) + 1) >> 1;
    u8::try_from(average).unwrap_or(u8::MAX)
}

fn implicit_b_prediction_weights(
    reference0: &ReferenceFrame,
    reference1: &ReferenceFrame,
    picture_order_count: i32,
) -> (PredictionWeights, PredictionWeights) {
    let td = (i64::from(reference1.pic_order_count) - i64::from(reference0.pic_order_count))
        .clamp(-128, 127);
    let weight1 = if reference0.long_term_frame_idx.is_some()
        || reference1.long_term_frame_idx.is_some()
        || td == 0
    {
        32
    } else {
        let tb = (i64::from(picture_order_count) - i64::from(reference0.pic_order_count))
            .clamp(-128, 127);
        let tx = (16_384 + td.abs() / 2) / td;
        let distance_scale = ((tb * tx + 32) >> 6).clamp(-1_024, 1_023);
        let candidate = distance_scale >> 2;
        if (-64..=128).contains(&candidate) {
            i32::try_from(candidate).expect("bounded implicit B weight fits i32")
        } else {
            32
        }
    };
    let component_weights = |weight| PredictionWeights {
        luma: PredictionWeight {
            denominator: 5,
            weight,
            offset: 0,
        },
        cb: PredictionWeight {
            denominator: 5,
            weight,
            offset: 0,
        },
        cr: PredictionWeight {
            denominator: 5,
            weight,
            offset: 0,
        },
    };
    (component_weights(64 - weight1), component_weights(weight1))
}

fn reference_from_list<'a>(
    references: &[&'a ReferenceFrame],
    index: u8,
    macroblock_address: usize,
    list_name: &str,
) -> Result<&'a ReferenceFrame> {
    references.get(usize::from(index)).copied().ok_or_else(|| {
        Error::InvalidData(format!(
            "H.264 macroblock {macroblock_address} selects unavailable {list_name} reference {index}"
        ))
    })
}

const CABAC_P_SKIP_INIT: [[(i8, i8); 3]; 3] = [
    [(23, 33), (23, 2), (21, 0)],
    [(22, 25), (34, 0), (16, 0)],
    [(29, 16), (25, 0), (14, 0)],
];
const CABAC_B_SKIP_INIT: [[(i8, i8); 3]; 3] = [
    [(18, 64), (9, 43), (29, 0)],
    [(26, 34), (19, 22), (40, 0)],
    [(20, 40), (20, 10), (29, 0)],
];
const CABAC_B_MACROBLOCK_TYPE_INIT: [[(i8, i8); 9]; 3] = [
    [
        (26, 67),
        (16, 90),
        (9, 104),
        (-46, 127),
        (-20, 104),
        (1, 67),
        (-13, 78),
        (-11, 65),
        (1, 62),
    ],
    [
        (57, 2),
        (41, 36),
        (26, 69),
        (-45, 127),
        (-15, 101),
        (-4, 76),
        (-6, 71),
        (-13, 79),
        (5, 52),
    ],
    [
        (54, 0),
        (37, 42),
        (12, 97),
        (-32, 127),
        (-22, 117),
        (-2, 74),
        (-4, 85),
        (-24, 102),
        (5, 57),
    ],
];
const CABAC_B_SUB_MACROBLOCK_TYPE_INIT: [[(i8, i8); 4]; 3] = [
    [(-6, 86), (-17, 95), (-6, 61), (9, 45)],
    [(6, 69), (-13, 90), (0, 52), (8, 43)],
    [(-6, 93), (-14, 88), (-6, 44), (4, 55)],
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
const CABAC_P_REFERENCE_INDEX_INIT: [[(i8, i8); 6]; 3] = [
    [(-7, 67), (-5, 74), (-4, 74), (-5, 80), (-7, 72), (1, 58)],
    [(-1, 66), (-1, 77), (1, 70), (-2, 86), (-5, 72), (0, 61)],
    [(3, 55), (-4, 79), (-2, 75), (-12, 97), (-7, 50), (1, 60)],
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
    reference_index: [ContextState; 6],
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
            reference_index: initial_contexts(&CABAC_P_REFERENCE_INDEX_INIT[index], slice_qp_y)?,
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

struct CabacBContexts {
    skip: [ContextState; 3],
    macroblock_type: [ContextState; 9],
    sub_macroblock_type: [ContextState; 4],
    inter: CabacPContexts,
}

impl CabacBContexts {
    fn new(slice_qp_y: i32, cabac_init_idc: u32) -> Result<Self> {
        let index = usize::try_from(cabac_init_idc).expect("CABAC initialization idc fits usize");
        if index >= CABAC_B_SKIP_INIT.len() {
            return Err(Error::InvalidData("invalid H.264 cabac_init_idc".into()));
        }
        Ok(Self {
            skip: initial_contexts(&CABAC_B_SKIP_INIT[index], slice_qp_y)?,
            macroblock_type: initial_contexts(&CABAC_B_MACROBLOCK_TYPE_INIT[index], slice_qp_y)?,
            sub_macroblock_type: initial_contexts(
                &CABAC_B_SUB_MACROBLOCK_TYPE_INIT[index],
                slice_qp_y,
            )?,
            inter: CabacPContexts::new(slice_qp_y, cabac_init_idc)?,
        })
    }
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn decode_cabac_b_macroblocks(
    bits: &mut BitReader<'_>,
    buffer: &mut FrameBuffer,
    list0: &[&ReferenceFrame],
    list1: &[&ReferenceFrame],
    prediction_weights: &BPredictionWeights,
    active_l0_minus1: u32,
    active_l1_minus1: u32,
    direct_spatial_mv_pred: bool,
    direct_8x8_inference: bool,
    picture_order_count: i32,
    luma_qp: &mut i32,
    cabac_init_idc: u32,
    chroma_qp_offset: i32,
    transform_8x8_mode: bool,
    first_macroblock: usize,
    end_macroblock: usize,
) -> Result<()> {
    let mut contexts = CabacBContexts::new(*luma_qp, cabac_init_idc)?;
    let mut decoder = CabacDecoder::new(bits)?;
    let macroblocks_wide = buffer.coded_width / 16;
    let mut skipped = vec![false; buffer.macroblock_count()];
    let mut direct = vec![false; buffer.macroblock_count()];
    let mut coded_blocks = CabacICodedBlocks::new(buffer.macroblock_count());
    let mut transform_8x8 = vec![false; buffer.macroblock_count()];
    let mut motion_differences_l0 = vec![[MotionVector::default(); 16]; buffer.macroblock_count()];
    let mut motion_differences_l1 = vec![[MotionVector::default(); 16]; buffer.macroblock_count()];
    let mut reference_indices_l0 = vec![[None; 16]; buffer.macroblock_count()];
    let mut reference_indices_l1 = vec![[None; 16]; buffer.macroblock_count()];
    let mut chroma_prediction_modes = vec![0_u32; buffer.macroblock_count()];
    let mut previous_qp_delta = 0;
    skipped[..first_macroblock].fill(true);
    direct[..first_macroblock].fill(true);
    coded_blocks.begin_slice(first_macroblock);
    for address in first_macroblock..end_macroblock {
        let skip_context =
            usize::from(!address.is_multiple_of(macroblocks_wide) && !skipped[address - 1])
                + usize::from(address >= macroblocks_wide && !skipped[address - macroblocks_wide]);
        if decoder.decision(&mut contexts.skip[skip_context])? {
            skipped[address] = true;
            direct[address] = true;
            previous_qp_delta = 0;
            decode_spatial_direct_macroblock(
                buffer,
                list0,
                list1,
                prediction_weights,
                address,
                direct_spatial_mv_pred,
                direct_8x8_inference,
                picture_order_count,
            )?;
            buffer.set_luma_qp(address, *luma_qp);
        } else {
            let type_context =
                usize::from(!address.is_multiple_of(macroblocks_wide) && !direct[address - 1])
                    + usize::from(
                        address >= macroblocks_wide && !direct[address - macroblocks_wide],
                    );
            let macroblock_type = decode_cabac_b_macroblock_type(
                &mut decoder,
                &mut contexts.macroblock_type,
                type_context,
            )?;
            let mut handled_intra = false;
            let transform_allowed = if macroblock_type == 0 {
                direct[address] = true;
                decode_spatial_direct_macroblock(
                    buffer,
                    list0,
                    list1,
                    prediction_weights,
                    address,
                    direct_spatial_mv_pred,
                    direct_8x8_inference,
                    picture_order_count,
                )?;
                direct_8x8_inference
            } else if (1..=21).contains(&macroblock_type) {
                let partitions = b_inter_partitions(macroblock_type);
                let transform_allowed = partitions.iter().all(|(partition, _)| {
                    partition.block_width >= 2 && partition.block_height >= 2
                });
                decode_cabac_b_inter_macroblock(
                    &mut decoder,
                    &mut contexts.inter,
                    buffer,
                    list0,
                    list1,
                    prediction_weights,
                    active_l0_minus1,
                    active_l1_minus1,
                    address,
                    macroblocks_wide,
                    partitions,
                    &mut reference_indices_l0,
                    &mut reference_indices_l1,
                    &mut motion_differences_l0,
                    &mut motion_differences_l1,
                )?;
                transform_allowed
            } else if macroblock_type == 22 {
                let (transform_allowed, all_direct) = decode_cabac_b8x8_macroblock(
                    &mut decoder,
                    &mut contexts,
                    buffer,
                    list0,
                    list1,
                    prediction_weights,
                    active_l0_minus1,
                    active_l1_minus1,
                    address,
                    macroblocks_wide,
                    direct_spatial_mv_pred,
                    direct_8x8_inference,
                    picture_order_count,
                    &mut reference_indices_l0,
                    &mut reference_indices_l1,
                    &mut motion_differences_l0,
                    &mut motion_differences_l1,
                )?;
                direct[address] = all_direct;
                transform_allowed
            } else if (23..=48).contains(&macroblock_type) {
                handled_intra = true;
                let intra_type = macroblock_type - 23;
                match intra_type {
                    0 => {
                        let use_8x8 = transform_8x8_mode
                            && decode_cabac_transform_size_8x8(
                                &mut decoder,
                                &mut contexts.inter.transform_size_8x8,
                                address,
                                macroblocks_wide,
                                &transform_8x8,
                            )?;
                        transform_8x8[address] = use_8x8;
                        buffer.transform_8x8[address] = use_8x8;
                        if use_8x8 {
                            decode_cabac_p_intra8(
                                &mut decoder,
                                &mut contexts.inter,
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
                            decode_cabac_p_intra4(
                                &mut decoder,
                                &mut contexts.inter,
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
                    1..=24 => decode_cabac_p_intra16(
                        &mut decoder,
                        &mut contexts.inter,
                        buffer,
                        address,
                        macroblocks_wide,
                        intra_type,
                        chroma_qp_offset,
                        &mut previous_qp_delta,
                        luma_qp,
                        &mut chroma_prediction_modes,
                        &mut coded_blocks,
                    )?,
                    25 => {
                        let samples = decoder.pcm_samples(384)?;
                        buffer.place_pcm_macroblock(address, &samples)?;
                        buffer.mark_pcm(address);
                        coded_blocks.mark_pcm(address);
                        previous_qp_delta = 0;
                    }
                    _ => unreachable!("CABAC B intra macroblock type is in 0..=25"),
                }
                false
            } else {
                return Err(Error::Unsupported(format!(
                    "native H.264 CABAC B-slice macroblock type {macroblock_type} is not implemented yet"
                )));
            };
            if !handled_intra {
                let pattern = decode_cabac_coded_block_pattern(
                    &mut decoder,
                    &mut contexts.inter.coded_block_pattern_luma,
                    &mut contexts.inter.coded_block_pattern_chroma,
                    address,
                    macroblocks_wide,
                    &coded_blocks.patterns,
                )?;
                coded_blocks.patterns[address] = pattern;
                let use_8x8 = transform_8x8_mode
                    && transform_allowed
                    && pattern & 15 != 0
                    && decode_cabac_transform_size_8x8(
                        &mut decoder,
                        &mut contexts.inter.transform_size_8x8,
                        address,
                        macroblocks_wide,
                        &transform_8x8,
                    )?;
                transform_8x8[address] = use_8x8;
                buffer.transform_8x8[address] = use_8x8;
                decode_cabac_p16_residuals(
                    &mut decoder,
                    &mut contexts.inter,
                    buffer,
                    address,
                    macroblocks_wide,
                    pattern,
                    luma_qp,
                    &mut previous_qp_delta,
                    &mut coded_blocks,
                    chroma_qp_offset,
                    use_8x8,
                )?;
            }
        }
        if decoder.terminate()? {
            if address + 1 != end_macroblock {
                return Err(Error::InvalidData(
                    "H.264 CABAC B slice ended at an unexpected macroblock".into(),
                ));
            }
            return Ok(());
        }
    }
    Err(Error::InvalidData(format!(
        "H.264 CABAC B slice {first_macroblock}..{end_macroblock} is missing end_of_slice_flag"
    )))
}

fn decode_cabac_b_macroblock_type(
    decoder: &mut CabacDecoder<'_, '_>,
    contexts: &mut [ContextState; 9],
    context_increment: usize,
) -> Result<u32> {
    if !decoder.decision(&mut contexts[context_increment])? {
        return Ok(0);
    }
    if !decoder.decision(&mut contexts[3])? {
        return Ok(1 + u32::from(decoder.decision(&mut contexts[5])?));
    }
    let mut bits = u32::from(decoder.decision(&mut contexts[4])?) << 3;
    for shift in (0..3).rev() {
        bits |= u32::from(decoder.decision(&mut contexts[5])?) << shift;
    }
    match bits {
        0..=7 => Ok(bits + 3),
        13 => Ok(23 + decode_cabac_b_intra_macroblock_type(decoder, contexts)?),
        14 => Ok(11),
        15 => Ok(22),
        8..=12 => Ok((bits << 1) + u32::from(decoder.decision(&mut contexts[5])?) - 4),
        _ => unreachable!("four CABAC macroblock-type bins fit in 0..=15"),
    }
}

fn decode_cabac_b_intra_macroblock_type(
    decoder: &mut CabacDecoder<'_, '_>,
    contexts: &mut [ContextState; 9],
) -> Result<u32> {
    if !decoder.decision(&mut contexts[5])? {
        return Ok(0);
    }
    if decoder.terminate()? {
        return Ok(25);
    }
    let mut macroblock_type = 1;
    macroblock_type += 12 * u32::from(decoder.decision(&mut contexts[6])?);
    if decoder.decision(&mut contexts[7])? {
        macroblock_type += 4 + 4 * u32::from(decoder.decision(&mut contexts[7])?);
    }
    macroblock_type += 2 * u32::from(decoder.decision(&mut contexts[8])?);
    macroblock_type += u32::from(decoder.decision(&mut contexts[8])?);
    Ok(macroblock_type)
}

fn decode_cabac_b_sub_macroblock_type(
    decoder: &mut CabacDecoder<'_, '_>,
    contexts: &mut [ContextState; 4],
) -> Result<u32> {
    if !decoder.decision(&mut contexts[0])? {
        return Ok(0);
    }
    if !decoder.decision(&mut contexts[1])? {
        return Ok(1 + u32::from(decoder.decision(&mut contexts[3])?));
    }
    let mut sub_type = 3;
    if decoder.decision(&mut contexts[2])? {
        if decoder.decision(&mut contexts[3])? {
            return Ok(11 + u32::from(decoder.decision(&mut contexts[3])?));
        }
        sub_type += 4;
    }
    sub_type += 2 * u32::from(decoder.decision(&mut contexts[3])?);
    sub_type += u32::from(decoder.decision(&mut contexts[3])?);
    Ok(sub_type)
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn decode_cabac_b8x8_macroblock(
    decoder: &mut CabacDecoder<'_, '_>,
    contexts: &mut CabacBContexts,
    buffer: &mut FrameBuffer,
    list0: &[&ReferenceFrame],
    list1: &[&ReferenceFrame],
    prediction_weights: &BPredictionWeights,
    active_l0_minus1: u32,
    active_l1_minus1: u32,
    address: usize,
    macroblocks_wide: usize,
    direct_spatial_mv_pred: bool,
    direct_8x8_inference: bool,
    picture_order_count: i32,
    reference_indices_l0: &mut [[Option<u8>; 16]],
    reference_indices_l1: &mut [[Option<u8>; 16]],
    motion_differences_l0: &mut [[MotionVector; 16]],
    motion_differences_l1: &mut [[MotionVector; 16]],
) -> Result<(bool, bool)> {
    let sub_macroblocks = (0..4)
        .map(|index| {
            b_sub_macroblock(
                index,
                decode_cabac_b_sub_macroblock_type(decoder, &mut contexts.sub_macroblock_type)?,
                direct_8x8_inference,
            )
        })
        .collect::<Result<Vec<_>>>()?;
    let mut indices_l0 = [None; 4];
    let mut indices_l1 = [None; 4];
    for (sub_index, sub) in sub_macroblocks.iter().enumerate() {
        if sub.prediction.uses_l0() {
            let partition = sub.partitions[0];
            let context_increment = cabac_reference_index_context(
                address,
                macroblocks_wide,
                partition.block_x,
                partition.block_y,
                reference_indices_l0,
            );
            let index = decode_cabac_reference_index(
                decoder,
                &mut contexts.inter.reference_index,
                context_increment,
                active_l0_minus1,
            )?;
            for partition in &sub.partitions {
                set_cabac_partition_reference(reference_indices_l0, address, *partition, index);
            }
            indices_l0[sub_index] = Some(index);
        }
    }
    for (sub_index, sub) in sub_macroblocks.iter().enumerate() {
        if sub.prediction.uses_l1() {
            let partition = sub.partitions[0];
            let context_increment = cabac_reference_index_context(
                address,
                macroblocks_wide,
                partition.block_x,
                partition.block_y,
                reference_indices_l1,
            );
            let index = decode_cabac_reference_index(
                decoder,
                &mut contexts.inter.reference_index,
                context_increment,
                active_l1_minus1,
            )?;
            for partition in &sub.partitions {
                set_cabac_partition_reference(reference_indices_l1, address, *partition, index);
            }
            indices_l1[sub_index] = Some(index);
        }
    }
    let mut differences_l0: [Vec<MotionVector>; 4] = std::array::from_fn(|_| Vec::new());
    let mut differences_l1: [Vec<MotionVector>; 4] = std::array::from_fn(|_| Vec::new());
    for (sub_index, sub) in sub_macroblocks.iter().enumerate() {
        if sub.prediction.uses_l0() {
            for partition in &sub.partitions {
                differences_l0[sub_index].push(decode_cabac_partition_motion_difference(
                    decoder,
                    &mut contexts.inter,
                    address,
                    macroblocks_wide,
                    *partition,
                    motion_differences_l0,
                )?);
            }
        }
    }
    for (sub_index, sub) in sub_macroblocks.iter().enumerate() {
        if sub.prediction.uses_l1() {
            for partition in &sub.partitions {
                differences_l1[sub_index].push(decode_cabac_partition_motion_difference(
                    decoder,
                    &mut contexts.inter,
                    address,
                    macroblocks_wide,
                    *partition,
                    motion_differences_l1,
                )?);
            }
        }
    }
    let transform_allowed = sub_macroblocks.iter().all(|sub| {
        matches!(sub.prediction, BPrediction::Direct) && direct_8x8_inference
            || sub
                .partitions
                .iter()
                .all(|partition| partition.block_width >= 2 && partition.block_height >= 2)
    });
    let all_direct = sub_macroblocks
        .iter()
        .all(|sub| matches!(sub.prediction, BPrediction::Direct));
    for (sub_index, sub) in sub_macroblocks.into_iter().enumerate() {
        for (partition_index, partition) in sub.partitions.into_iter().enumerate() {
            if matches!(sub.prediction, BPrediction::Direct) {
                decode_spatial_direct_partition(
                    buffer,
                    list0,
                    list1,
                    prediction_weights,
                    address,
                    partition,
                    direct_spatial_mv_pred,
                    picture_order_count,
                )?;
                continue;
            }
            let motion_l0 = indices_l0[sub_index]
                .zip(differences_l0[sub_index].get(partition_index).copied())
                .map(|(index, difference)| {
                    checked_motion_sum(
                        buffer.partition_motion_vector_predictor(
                            address,
                            partition.block_x,
                            partition.block_y,
                            partition.block_width,
                            partition.prediction_kind,
                            index,
                        ),
                        difference,
                    )
                    .map(|vector| (index, vector))
                })
                .transpose()?;
            let motion_l1 = indices_l1[sub_index]
                .zip(differences_l1[sub_index].get(partition_index).copied())
                .map(|(index, difference)| {
                    checked_motion_sum(
                        buffer.partition_motion_vector_predictor_l1(
                            address,
                            partition.block_x,
                            partition.block_y,
                            partition.block_width,
                            partition.prediction_kind,
                            index,
                        ),
                        difference,
                    )
                    .map(|vector| (index, vector))
                })
                .transpose()?;
            predict_b_partition(
                buffer,
                list0,
                list1,
                prediction_weights,
                address,
                partition,
                motion_l0,
                motion_l1,
            )?;
        }
    }
    Ok((transform_allowed, all_direct))
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn decode_cabac_b_inter_macroblock(
    decoder: &mut CabacDecoder<'_, '_>,
    contexts: &mut CabacPContexts,
    buffer: &mut FrameBuffer,
    list0: &[&ReferenceFrame],
    list1: &[&ReferenceFrame],
    prediction_weights: &BPredictionWeights,
    active_l0_minus1: u32,
    active_l1_minus1: u32,
    address: usize,
    macroblocks_wide: usize,
    partitions: Vec<(InterPartition, BPrediction)>,
    reference_indices_l0: &mut [[Option<u8>; 16]],
    reference_indices_l1: &mut [[Option<u8>; 16]],
    motion_differences_l0: &mut [[MotionVector; 16]],
    motion_differences_l1: &mut [[MotionVector; 16]],
) -> Result<()> {
    let mut indices_l0 = vec![None; partitions.len()];
    let mut indices_l1 = vec![None; partitions.len()];
    for (partition_index, (partition, prediction)) in partitions.iter().enumerate() {
        if prediction.uses_l0() {
            let context_increment = cabac_reference_index_context(
                address,
                macroblocks_wide,
                partition.block_x,
                partition.block_y,
                reference_indices_l0,
            );
            let index = decode_cabac_reference_index(
                decoder,
                &mut contexts.reference_index,
                context_increment,
                active_l0_minus1,
            )?;
            set_cabac_partition_reference(reference_indices_l0, address, *partition, index);
            indices_l0[partition_index] = Some(index);
        }
    }
    for (partition_index, (partition, prediction)) in partitions.iter().enumerate() {
        if prediction.uses_l1() {
            let context_increment = cabac_reference_index_context(
                address,
                macroblocks_wide,
                partition.block_x,
                partition.block_y,
                reference_indices_l1,
            );
            let index = decode_cabac_reference_index(
                decoder,
                &mut contexts.reference_index,
                context_increment,
                active_l1_minus1,
            )?;
            set_cabac_partition_reference(reference_indices_l1, address, *partition, index);
            indices_l1[partition_index] = Some(index);
        }
    }
    let mut differences_l0 = vec![None; partitions.len()];
    let mut differences_l1 = vec![None; partitions.len()];
    for (partition_index, (partition, prediction)) in partitions.iter().enumerate() {
        if prediction.uses_l0() {
            differences_l0[partition_index] = Some(decode_cabac_partition_motion_difference(
                decoder,
                contexts,
                address,
                macroblocks_wide,
                *partition,
                motion_differences_l0,
            )?);
        }
    }
    for (partition_index, (partition, prediction)) in partitions.iter().enumerate() {
        if prediction.uses_l1() {
            differences_l1[partition_index] = Some(decode_cabac_partition_motion_difference(
                decoder,
                contexts,
                address,
                macroblocks_wide,
                *partition,
                motion_differences_l1,
            )?);
        }
    }
    for (partition_index, (partition, _)) in partitions.into_iter().enumerate() {
        let motion_l0 = indices_l0[partition_index]
            .zip(differences_l0[partition_index])
            .map(|(index, difference)| {
                checked_motion_sum(
                    buffer.partition_motion_vector_predictor(
                        address,
                        partition.block_x,
                        partition.block_y,
                        partition.block_width,
                        partition.prediction_kind,
                        index,
                    ),
                    difference,
                )
                .map(|vector| (index, vector))
            })
            .transpose()?;
        let motion_l1 = indices_l1[partition_index]
            .zip(differences_l1[partition_index])
            .map(|(index, difference)| {
                checked_motion_sum(
                    buffer.partition_motion_vector_predictor_l1(
                        address,
                        partition.block_x,
                        partition.block_y,
                        partition.block_width,
                        partition.prediction_kind,
                        index,
                    ),
                    difference,
                )
                .map(|vector| (index, vector))
            })
            .transpose()?;
        predict_b_partition(
            buffer,
            list0,
            list1,
            prediction_weights,
            address,
            partition,
            motion_l0,
            motion_l1,
        )?;
    }
    Ok(())
}

fn decode_cabac_partition_motion_difference(
    decoder: &mut CabacDecoder<'_, '_>,
    contexts: &mut CabacPContexts,
    address: usize,
    macroblocks_wide: usize,
    partition: InterPartition,
    motion_differences: &mut [[MotionVector; 16]],
) -> Result<MotionVector> {
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
    Ok(difference)
}

#[allow(clippy::too_many_arguments)]
fn decode_cabac_p_macroblocks(
    bits: &mut BitReader<'_>,
    buffer: &mut FrameBuffer,
    references: &[&ReferenceFrame],
    active_references_minus1: u32,
    prediction_weights: PredictionWeights,
    luma_qp: &mut i32,
    cabac_init_idc: u32,
    chroma_qp_offset: i32,
    transform_8x8_mode: bool,
    first_macroblock: usize,
    end_macroblock: usize,
) -> Result<()> {
    let mut contexts = CabacPContexts::new(*luma_qp, cabac_init_idc)?;
    let mut decoder = CabacDecoder::new(bits)?;
    let macroblocks_wide = buffer.coded_width / 16;
    let mut skipped = vec![false; buffer.macroblock_count()];
    let mut coded_blocks = CabacICodedBlocks::new(buffer.macroblock_count());
    let mut motion_differences = vec![[MotionVector::default(); 16]; buffer.macroblock_count()];
    let mut reference_indices = vec![[None; 16]; buffer.macroblock_count()];
    let mut chroma_prediction_modes = vec![0_u32; buffer.macroblock_count()];
    let mut transform_8x8 = vec![false; buffer.macroblock_count()];
    let mut previous_qp_delta = 0;
    skipped[..first_macroblock].fill(true);
    coded_blocks.begin_slice(first_macroblock);
    for address in first_macroblock..end_macroblock {
        let context_increment =
            usize::from(!address.is_multiple_of(macroblocks_wide) && !skipped[address - 1])
                + usize::from(address >= macroblocks_wide && !skipped[address - macroblocks_wide]);
        if decoder.decision(&mut contexts.skip[context_increment])? {
            skipped[address] = true;
            previous_qp_delta = 0;
            reference_indices[address].fill(Some(0));
            buffer.predict_p_skip(references[0], address, prediction_weights, *luma_qp)?;
        } else {
            decode_cabac_p_macroblock(
                &mut decoder,
                &mut contexts,
                buffer,
                references,
                active_references_minus1,
                address,
                macroblocks_wide,
                prediction_weights,
                luma_qp,
                &mut previous_qp_delta,
                &mut coded_blocks,
                &mut motion_differences,
                &mut reference_indices,
                chroma_qp_offset,
                &mut chroma_prediction_modes,
                transform_8x8_mode,
                &mut transform_8x8,
            )?;
        }
        if decoder.terminate()? {
            if address + 1 != end_macroblock {
                return Err(Error::InvalidData(
                    "H.264 CABAC P slice ended at an unexpected macroblock".into(),
                ));
            }
            return Ok(());
        }
    }
    Err(Error::InvalidData(format!(
        "H.264 CABAC P slice {first_macroblock}..{end_macroblock} is missing end_of_slice_flag"
    )))
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn decode_cabac_p_macroblock(
    decoder: &mut CabacDecoder<'_, '_>,
    contexts: &mut CabacPContexts,
    buffer: &mut FrameBuffer,
    references: &[&ReferenceFrame],
    active_references_minus1: u32,
    address: usize,
    macroblocks_wide: usize,
    prediction_weights: PredictionWeights,
    luma_qp: &mut i32,
    previous_qp_delta: &mut i32,
    coded_blocks: &mut CabacICodedBlocks,
    motion_differences: &mut [[MotionVector; 16]],
    reference_indices: &mut [[Option<u8>; 16]],
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
    let (partitions, reference_partitions) =
        if !decoder.decision(&mut contexts.macroblock_type[1])? {
            if decoder.decision(&mut contexts.macroblock_type[2])? {
                (
                    decode_cabac_p8x8_partitions(decoder, &mut contexts.sub_macroblock_type)?,
                    vec![
                        InterPartition::new(0, 0, 2, 2, MotionPredictionKind::Normal),
                        InterPartition::new(2, 0, 2, 2, MotionPredictionKind::Normal),
                        InterPartition::new(0, 2, 2, 2, MotionPredictionKind::Normal),
                        InterPartition::new(2, 2, 2, 2, MotionPredictionKind::Normal),
                    ],
                )
            } else {
                let partition = InterPartition::new(0, 0, 4, 4, MotionPredictionKind::Normal);
                (vec![partition], vec![partition])
            }
        } else if decoder.decision(&mut contexts.macroblock_type[3])? {
            let partitions = vec![
                InterPartition::new(0, 0, 4, 2, MotionPredictionKind::Top16x8),
                InterPartition::new(0, 2, 4, 2, MotionPredictionKind::Bottom16x8),
            ];
            (partitions.clone(), partitions)
        } else {
            let partitions = vec![
                InterPartition::new(0, 0, 2, 4, MotionPredictionKind::Left8x16),
                InterPartition::new(2, 0, 2, 4, MotionPredictionKind::Right8x16),
            ];
            (partitions.clone(), partitions)
        };
    let transform_allowed = transform_8x8_mode
        && partitions
            .iter()
            .all(|partition| partition.block_width >= 2 && partition.block_height >= 2);
    for partition in reference_partitions {
        let context_increment = cabac_reference_index_context(
            address,
            macroblocks_wide,
            partition.block_x,
            partition.block_y,
            reference_indices,
        );
        let reference_index = decode_cabac_reference_index(
            decoder,
            &mut contexts.reference_index,
            context_increment,
            active_references_minus1,
        )?;
        set_cabac_partition_reference(reference_indices, address, partition, reference_index);
    }
    for partition in partitions {
        let reference_index = reference_indices[address]
            [luma_block_index(partition.block_x, partition.block_y)]
        .expect("CABAC inter partition reference was decoded");
        let reference = references
            .get(usize::from(reference_index))
            .copied()
            .ok_or_else(|| {
                Error::InvalidData(format!(
                    "H.264 macroblock {address} selects unavailable list-0 reference {reference_index}"
                ))
            })?;
        decode_cabac_inter_partition(
            decoder,
            contexts,
            buffer,
            reference,
            reference_index,
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
                coded_blocks.slice_start,
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
    let coded_context = cabac_intra_dc_coded_context(
        address,
        macroblocks_wide,
        coded_blocks.slice_start,
        |neighbor| coded_blocks.luma_dc[neighbor],
    );
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
            coded_blocks.slice_start,
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
            let coded_context = cabac_intra_dc_coded_context(
                address,
                macroblocks_wide,
                coded_blocks.slice_start,
                |neighbor| coded_blocks.chroma_dc[neighbor][component],
            );
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
                coded_blocks.slice_start,
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

fn decode_cabac_reference_index(
    decoder: &mut CabacDecoder<'_, '_>,
    contexts: &mut [ContextState; 6],
    context_increment: usize,
    active_references_minus1: u32,
) -> Result<u8> {
    if active_references_minus1 == 0 {
        return Ok(0);
    }
    let mut reference_index = 0_u32;
    let mut context_index = context_increment;
    while decoder.decision(&mut contexts[context_index])? {
        reference_index = reference_index
            .checked_add(1)
            .ok_or_else(|| Error::InvalidData("H.264 CABAC reference index overflows".into()))?;
        if reference_index > active_references_minus1 {
            return Err(Error::InvalidData(format!(
                "H.264 list-0 reference index {reference_index} exceeds active maximum {active_references_minus1}"
            )));
        }
        context_index = (context_index >> 2) + 4;
    }
    u8::try_from(reference_index)
        .map_err(|_| Error::Unsupported("more than 256 active H.264 references".into()))
}

fn cabac_reference_index_context(
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
        cabac_reference_at(
            absolute_x.cast_signed() - 1,
            absolute_y.cast_signed(),
            macroblocks_wide,
            reference_indices,
        )
        .is_some_and(|index| index > 0),
    ) + 2 * usize::from(
        cabac_reference_at(
            absolute_x.cast_signed(),
            absolute_y.cast_signed() - 1,
            macroblocks_wide,
            reference_indices,
        )
        .is_some_and(|index| index > 0),
    )
}

fn cabac_reference_at(
    block_x: isize,
    block_y: isize,
    macroblocks_wide: usize,
    reference_indices: &[[Option<u8>; 16]],
) -> Option<u8> {
    let blocks_wide = macroblocks_wide * 4;
    let macroblocks_high = reference_indices.len().div_ceil(macroblocks_wide);
    let blocks_high = macroblocks_high * 4;
    let block_x = usize::try_from(block_x).ok().filter(|&x| x < blocks_wide)?;
    let block_y = usize::try_from(block_y).ok().filter(|&y| y < blocks_high)?;
    let address = (block_y / 4) * macroblocks_wide + block_x / 4;
    reference_indices.get(address)?[luma_block_index(block_x % 4, block_y % 4)]
}

fn set_cabac_partition_reference(
    reference_indices: &mut [[Option<u8>; 16]],
    address: usize,
    partition: InterPartition,
    reference_index: u8,
) {
    for y in partition.block_y..partition.block_y + partition.block_height {
        for x in partition.block_x..partition.block_x + partition.block_width {
            reference_indices[address][luma_block_index(x, y)] = Some(reference_index);
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn decode_cabac_inter_partition(
    decoder: &mut CabacDecoder<'_, '_>,
    contexts: &mut CabacPContexts,
    buffer: &mut FrameBuffer,
    reference: &ReferenceFrame,
    reference_index: u8,
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
        reference_index,
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
        reference_index,
        reference,
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

pub(crate) struct CabacIContexts {
    pub(crate) macroblock_type: [ContextState; 11],
    pub(crate) macroblock_qp_delta: [ContextState; 4],
    pub(crate) chroma_prediction_mode: [ContextState; 4],
    intra4_prediction_mode: [ContextState; 2],
    transform_size_8x8: [ContextState; 3],
    coded_block_pattern_luma: [ContextState; 4],
    coded_block_pattern_chroma: [ContextState; 8],
    pub(crate) luma_dc_coded_block: [ContextState; 4],
    pub(crate) chroma_dc_coded_block: [ContextState; 4],
    pub(crate) luma_ac_coded_block: [ContextState; 4],
    pub(crate) chroma_ac_coded_block: [ContextState; 4],
    pub(crate) luma_dc_significant: [ContextState; 15],
    pub(crate) luma_dc_last: [ContextState; 15],
    pub(crate) luma_dc_abs_level: [ContextState; 10],
    pub(crate) chroma_dc_significant: [ContextState; 4],
    pub(crate) chroma_dc_last: [ContextState; 4],
    pub(crate) chroma_dc_abs_level: [ContextState; 10],
    pub(crate) luma_ac_significant: [ContextState; 14],
    pub(crate) luma_ac_last: [ContextState; 14],
    pub(crate) luma_ac_abs_level: [ContextState; 10],
    pub(crate) chroma_ac_significant: [ContextState; 14],
    pub(crate) chroma_ac_last: [ContextState; 14],
    pub(crate) chroma_ac_abs_level: [ContextState; 10],
    luma_4x4_coded_block: [ContextState; 4],
    luma_4x4_significant: [ContextState; 15],
    luma_4x4_last: [ContextState; 15],
    luma_4x4_abs_level: [ContextState; 10],
    luma_8x8_significant: [ContextState; 15],
    luma_8x8_last: [ContextState; 9],
    luma_8x8_abs_level: [ContextState; 10],
}

impl CabacIContexts {
    pub(crate) fn new(slice_qp_y: i32) -> Result<Self> {
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
    slice_start: usize,
    patterns: Vec<u8>,
    luma_dc: Vec<bool>,
    chroma_dc: Vec<[bool; 2]>,
    luma_ac: Vec<[bool; 16]>,
    chroma_ac: Vec<[[bool; 4]; 2]>,
}

impl CabacICodedBlocks {
    fn new(macroblock_count: usize) -> Self {
        Self {
            slice_start: 0,
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

    fn begin_slice(&mut self, first_macroblock: usize) {
        self.slice_start = first_macroblock;
        self.patterns[..first_macroblock].fill(15);
    }
}

fn decode_cabac_i_macroblocks(
    bits: &mut BitReader<'_>,
    buffer: &mut FrameBuffer,
    luma_qp: &mut i32,
    chroma_qp_offset: i32,
    transform_8x8_mode: bool,
    first_macroblock: usize,
    end_macroblock: usize,
) -> Result<()> {
    let mut contexts = CabacIContexts::new(*luma_qp)?;
    let mut decoder = CabacDecoder::new(bits)?;
    let macroblocks_wide = buffer.coded_width / 16;
    let mut intra16_or_pcm = vec![false; buffer.macroblock_count()];
    let mut chroma_prediction_modes = vec![0_u32; buffer.macroblock_count()];
    let mut coded_blocks = CabacICodedBlocks::new(buffer.macroblock_count());
    coded_blocks.begin_slice(first_macroblock);
    let mut previous_qp_delta = 0;
    let mut transform_8x8 = vec![false; buffer.macroblock_count()];
    for address in first_macroblock..end_macroblock {
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
            if address + 1 != end_macroblock {
                return Err(Error::InvalidData(
                    "H.264 CABAC I slice ended at an unexpected macroblock".into(),
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
                coded_blocks.slice_start,
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
    let coded_context = cabac_intra_dc_coded_context(
        address,
        macroblocks_wide,
        coded_blocks.slice_start,
        |neighbor| coded_blocks.luma_dc[neighbor],
    );
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
            coded_blocks.slice_start,
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
            let coded_context = cabac_intra_dc_coded_context(
                address,
                macroblocks_wide,
                coded_blocks.slice_start,
                |neighbor| coded_blocks.chroma_dc[neighbor][component],
            );
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
                coded_blocks.slice_start,
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
    slice_start: usize,
    neighbor_is_coded: impl Fn(usize) -> bool,
) -> usize {
    // For coded_block_flag in an intra macroblock, an unavailable neighboring
    // transform block contributes a condition flag of one (H.264 9.3.3.1.1.9).
    let left = address.is_multiple_of(macroblocks_wide)
        || address <= slice_start
        || neighbor_is_coded(address - 1);
    let top = address < macroblocks_wide
        || address - macroblocks_wide < slice_start
        || neighbor_is_coded(address - macroblocks_wide);
    usize::from(left) + 2 * usize::from(top)
}

fn cabac_intra_luma_ac_coded_context(
    address: usize,
    macroblocks_wide: usize,
    slice_start: usize,
    block_index: usize,
    coded: &[[bool; 16]],
) -> usize {
    let (block_x, block_y) = luma_block_position(block_index);
    let left = if block_x > 0 {
        coded[address][luma_block_index(block_x - 1, block_y)]
    } else if !address.is_multiple_of(macroblocks_wide) && address > slice_start {
        coded[address - 1][luma_block_index(3, block_y)]
    } else {
        true
    };
    let top = if block_y > 0 {
        coded[address][luma_block_index(block_x, block_y - 1)]
    } else if address >= macroblocks_wide && address - macroblocks_wide >= slice_start {
        coded[address - macroblocks_wide][luma_block_index(block_x, 3)]
    } else {
        true
    };
    usize::from(left) + 2 * usize::from(top)
}

fn cabac_intra_chroma_ac_coded_context(
    address: usize,
    macroblocks_wide: usize,
    slice_start: usize,
    component: usize,
    block_index: usize,
    coded: &[[[bool; 4]; 2]],
) -> usize {
    let block_x = block_index % 2;
    let block_y = block_index / 2;
    let left = if block_x > 0 {
        coded[address][component][block_index - 1]
    } else if !address.is_multiple_of(macroblocks_wide) && address > slice_start {
        coded[address - 1][component][block_y * 2 + 1]
    } else {
        true
    };
    let top = if block_y > 0 {
        coded[address][component][block_index - 2]
    } else if address >= macroblocks_wide && address - macroblocks_wide >= slice_start {
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

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn decode_p_l0_macroblock(
    reader: &mut SyntaxReader<'_>,
    buffer: &mut FrameBuffer,
    references: &[&ReferenceFrame],
    macroblock_address: usize,
    macroblock_type: u32,
    active_references_minus1: u32,
    luma_qp: &mut i32,
    chroma_qp_offset: i32,
    prediction_weights: PredictionWeights,
    transform_8x8_mode: bool,
) -> Result<()> {
    let partitions = read_inter_partitions(reader, macroblock_type)?;
    let transform_8x8_allowed = transform_8x8_mode
        && partitions
            .iter()
            .all(|partition| partition.block_width >= 2 && partition.block_height >= 2);
    let reference_partition_count = match macroblock_type {
        0 => 1,
        1 | 2 => 2,
        3 | 4 => 4,
        _ => unreachable!("P_L0 macroblock type is in 0..=4"),
    };
    let reference_indices = if macroblock_type == 4 {
        vec![0; reference_partition_count]
    } else {
        (0..reference_partition_count)
            .map(|_| read_reference_index(reader, active_references_minus1))
            .collect::<Result<Vec<_>>>()?
    };
    for partition in partitions {
        let reference_slot = match macroblock_type {
            0 => 0,
            1 => partition.block_y / 2,
            2 => partition.block_x / 2,
            3 | 4 => (partition.block_y / 2) * 2 + partition.block_x / 2,
            _ => unreachable!("P_L0 macroblock type is in 0..=4"),
        };
        let reference_index = reference_indices[reference_slot];
        let reference = references
            .get(usize::from(reference_index))
            .copied()
            .ok_or_else(|| {
                Error::InvalidData(format!(
                    "H.264 macroblock {macroblock_address} selects unavailable list-0 reference {reference_index}"
                ))
            })?;
        let predictor = buffer.partition_motion_vector_predictor(
            macroblock_address,
            partition.block_x,
            partition.block_y,
            partition.block_width,
            partition.prediction_kind,
            reference_index,
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
            reference_index,
            reference,
        );
    }

    decode_inter_residuals(
        reader,
        buffer,
        macroblock_address,
        luma_qp,
        chroma_qp_offset,
        transform_8x8_allowed,
    )
}

fn decode_inter_residuals(
    reader: &mut SyntaxReader<'_>,
    buffer: &mut FrameBuffer,
    macroblock_address: usize,
    luma_qp: &mut i32,
    chroma_qp_offset: i32,
    transform_8x8_allowed: bool,
) -> Result<()> {
    let pattern_code = usize::try_from(reader.ue()?)
        .ok()
        .filter(|&code| code < INTER_CODED_BLOCK_PATTERN.len())
        .ok_or_else(|| Error::InvalidData("invalid H.264 inter coded block pattern".into()))?;
    let pattern = INTER_CODED_BLOCK_PATTERN[pattern_code];
    let luma_pattern = pattern & 15;
    let chroma_pattern = pattern >> 4;
    let transform_8x8 = transform_8x8_allowed && luma_pattern != 0 && reader.bit()?;
    buffer.transform_8x8[macroblock_address] = transform_8x8;
    if pattern != 0 {
        *luma_qp = (*luma_qp + reader.se()?).rem_euclid(52);
    }
    let luma_blocks = if transform_8x8 {
        None
    } else {
        let mut blocks: [Vec<i32>; 16] = std::array::from_fn(|_| vec![0; 16]);
        for group in 0..4 {
            for within_group in 0..4 {
                let block_index = group * 4 + within_group;
                if luma_pattern & (1 << group) != 0 {
                    let n_c = buffer.luma_nc(macroblock_address, block_index);
                    let decoded = decode_residual_block(&mut reader.bits, n_c, 16)?;
                    blocks[block_index] = decoded.coefficients;
                    buffer.set_luma_nonzero(macroblock_address, block_index, decoded.total_coeff);
                } else {
                    buffer.set_luma_nonzero(macroblock_address, block_index, 0);
                }
            }
        }
        Some(blocks)
    };
    let luma_8x8_blocks = if transform_8x8 {
        let mut blocks: [Vec<i32>; 4] = std::array::from_fn(|_| vec![0; 64]);
        for (group, levels) in blocks.iter_mut().enumerate() {
            for sub_block in 0..4 {
                let block_index = group * 4 + sub_block;
                if luma_pattern & (1 << group) != 0 {
                    let n_c = buffer.luma_nc(macroblock_address, block_index);
                    let decoded = decode_residual_block(&mut reader.bits, n_c, 16)?;
                    for (coefficient, level) in decoded.coefficients.into_iter().enumerate() {
                        levels[coefficient * 4 + sub_block] = level;
                    }
                    buffer.set_luma_nonzero(macroblock_address, block_index, decoded.total_coeff);
                } else {
                    buffer.set_luma_nonzero(macroblock_address, block_index, 0);
                }
            }
        }
        Some(blocks)
    } else {
        None
    };
    let (chroma_dc, chroma_blocks) =
        decode_chroma_blocks(reader, buffer, macroblock_address, chroma_pattern)?;
    if let Some(luma_blocks) = luma_blocks {
        buffer.add_luma_residual_blocks(macroblock_address, &luma_blocks, *luma_qp)?;
    }
    if let Some(luma_8x8_blocks) = luma_8x8_blocks {
        buffer.add_luma_residual_8x8_blocks(macroblock_address, &luma_8x8_blocks, *luma_qp)?;
    }
    buffer.add_chroma_residual(
        macroblock_address,
        &chroma_dc,
        &chroma_blocks,
        chroma_qp(*luma_qp, chroma_qp_offset),
    )?;
    buffer.set_luma_qp(macroblock_address, *luma_qp);
    Ok(())
}

fn read_reference_index(
    reader: &mut SyntaxReader<'_>,
    active_references_minus1: u32,
) -> Result<u8> {
    let value = match active_references_minus1 {
        0 => 0,
        1 => u32::from(!reader.bit()?),
        _ => reader.ue()?,
    };
    if value > active_references_minus1 {
        return Err(Error::InvalidData(format!(
            "H.264 list-0 reference index {value} exceeds active maximum {active_references_minus1}"
        )));
    }
    u8::try_from(value)
        .map_err(|_| Error::Unsupported("more than 256 active H.264 references".into()))
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PictureOrderState {
    Type0 { msb: i32, lsb: u32 },
    FrameNum { frame_num: u32, offset: u32 },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct PictureOrder {
    value: i32,
    reference_state: Option<PictureOrderState>,
    reset_reference_state: Option<PictureOrderState>,
}

fn combine_field_timing(first: FrameTiming, second: FrameTiming) -> Result<FrameTiming> {
    let duration = match (first.duration, second.duration) {
        (Some(first), Some(second)) if first.time_base == second.time_base => Some(Timestamp {
            value: first
                .value
                .checked_add(second.value)
                .ok_or_else(|| Error::InvalidData("H.264 field duration overflows".into()))?,
            time_base: first.time_base,
        }),
        (Some(_), Some(_)) => {
            return Err(Error::InvalidData(
                "H.264 complementary fields use different duration time bases".into(),
            ));
        }
        (duration, None) | (None, duration) => duration,
    };
    Ok(FrameTiming {
        pts: first.pts.or(second.pts),
        duration,
    })
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PictureStructure {
    Frame,
    TopField,
    BottomField,
}

#[allow(clippy::too_many_lines)]
#[allow(clippy::too_many_arguments)]
fn read_picture_order_count(
    reader: &mut SyntaxReader<'_>,
    sps: &Sps,
    pps: &Pps,
    frame_num: u32,
    structure: PictureStructure,
    is_idr: bool,
    is_reference: bool,
    previous_reference: Option<PictureOrderState>,
) -> Result<PictureOrder> {
    match &sps.pic_order_cnt_type {
        PictureOrderCountType::Type0 {
            log2_max_pic_order_cnt_lsb,
        } => {
            let lsb = u32::try_from(reader.bits(*log2_max_pic_order_cnt_lsb)?)
                .map_err(|_| Error::InvalidData("H.264 POC LSB overflows".into()))?;
            let delta_bottom = if structure == PictureStructure::Frame
                && pps.bottom_field_pic_order_in_frame_present
            {
                reader.se()?
            } else {
                0
            };
            let maximum = 1_i32 << *log2_max_pic_order_cnt_lsb;
            let previous = if is_idr {
                (0, 0)
            } else {
                match previous_reference {
                    Some(PictureOrderState::Type0 { msb, lsb }) => (msb, lsb),
                    _ => (0, 0),
                }
            };
            let lsb_i32 = i32::try_from(lsb).expect("at most 16-bit H.264 POC LSB fits i32");
            let previous_lsb =
                i32::try_from(previous.1).expect("at most 16-bit H.264 POC LSB fits i32");
            let msb = if lsb_i32 < previous_lsb && previous_lsb - lsb_i32 >= maximum / 2 {
                previous.0.checked_add(maximum).ok_or_else(|| {
                    Error::InvalidData("H.264 picture order count overflows".into())
                })?
            } else if lsb_i32 > previous_lsb && lsb_i32 - previous_lsb > maximum / 2 {
                previous.0.checked_sub(maximum).ok_or_else(|| {
                    Error::InvalidData("H.264 picture order count overflows".into())
                })?
            } else {
                previous.0
            };
            let top = msb
                .checked_add(lsb_i32)
                .ok_or_else(|| Error::InvalidData("H.264 picture order count overflows".into()))?;
            let bottom = if structure == PictureStructure::BottomField {
                top
            } else {
                top.checked_add(delta_bottom).ok_or_else(|| {
                    Error::InvalidData("H.264 bottom-field picture order count overflows".into())
                })?
            };
            let value = match structure {
                PictureStructure::Frame => top.min(bottom),
                PictureStructure::TopField => top,
                PictureStructure::BottomField => bottom,
            };
            let reset_lsb = u32::try_from(top.checked_sub(value).ok_or_else(|| {
                Error::InvalidData("H.264 MMCO 5 picture order count overflows".into())
            })?)
            .map_err(|_| Error::InvalidData("invalid H.264 MMCO 5 picture order count".into()))?;
            Ok(PictureOrder {
                value,
                reference_state: is_reference.then_some(PictureOrderState::Type0 { msb, lsb }),
                reset_reference_state: is_reference.then_some(PictureOrderState::Type0 {
                    msb: 0,
                    lsb: reset_lsb,
                }),
            })
        }
        PictureOrderCountType::Type1 {
            delta_pic_order_always_zero,
            offset_for_non_ref_pic,
            offset_for_top_to_bottom_field,
            offset_for_ref_frame,
        } => {
            let delta0 = if *delta_pic_order_always_zero {
                0
            } else {
                reader.se()?
            };
            let delta1 = if structure == PictureStructure::Frame
                && !*delta_pic_order_always_zero
                && pps.bottom_field_pic_order_in_frame_present
            {
                reader.se()?
            } else {
                0
            };
            let frame_num_offset =
                picture_frame_num_offset(sps, frame_num, is_idr, previous_reference)?;
            let mut absolute_frame_num = if offset_for_ref_frame.is_empty() {
                0_u64
            } else {
                u64::from(frame_num_offset) + u64::from(frame_num)
            };
            if !is_reference && absolute_frame_num > 0 {
                absolute_frame_num -= 1;
            }
            let mut expected = 0_i64;
            if absolute_frame_num > 0 {
                let cycle_length = u64::try_from(offset_for_ref_frame.len())
                    .expect("H.264 POC cycle length fits u64");
                let cycle_count = (absolute_frame_num - 1) / cycle_length;
                let frame_in_cycle = usize::try_from((absolute_frame_num - 1) % cycle_length)
                    .expect("H.264 frame-in-cycle index fits usize");
                let cycle_delta = offset_for_ref_frame
                    .iter()
                    .try_fold(0_i64, |sum, &offset| {
                        sum.checked_add(i64::from(offset)).ok_or_else(|| {
                            Error::InvalidData("H.264 POC cycle delta overflows".into())
                        })
                    })?;
                expected = i64::try_from(cycle_count)
                    .ok()
                    .and_then(|count| count.checked_mul(cycle_delta))
                    .ok_or_else(|| {
                        Error::InvalidData("H.264 picture order count overflows".into())
                    })?;
                for &offset in &offset_for_ref_frame[..=frame_in_cycle] {
                    expected = expected.checked_add(i64::from(offset)).ok_or_else(|| {
                        Error::InvalidData("H.264 picture order count overflows".into())
                    })?;
                }
            }
            if !is_reference {
                expected = expected
                    .checked_add(i64::from(*offset_for_non_ref_pic))
                    .ok_or_else(|| {
                        Error::InvalidData("H.264 picture order count overflows".into())
                    })?;
            }
            let top = expected
                .checked_add(i64::from(delta0))
                .ok_or_else(|| Error::InvalidData("H.264 picture order count overflows".into()))?;
            let bottom_delta1 = if structure == PictureStructure::Frame {
                i64::from(delta1)
            } else {
                0
            };
            let bottom = expected
                .checked_add(i64::from(*offset_for_top_to_bottom_field))
                .and_then(|value| value.checked_add(i64::from(delta0)))
                .and_then(|value| value.checked_add(bottom_delta1))
                .ok_or_else(|| Error::InvalidData("H.264 picture order count overflows".into()))?;
            Ok(PictureOrder {
                value: picture_order_i32(match structure {
                    PictureStructure::Frame => top.min(bottom),
                    PictureStructure::TopField => top,
                    PictureStructure::BottomField => bottom,
                })?,
                reference_state: is_reference.then_some(PictureOrderState::FrameNum {
                    frame_num,
                    offset: frame_num_offset,
                }),
                reset_reference_state: is_reference.then_some(PictureOrderState::FrameNum {
                    frame_num: 0,
                    offset: 0,
                }),
            })
        }
        PictureOrderCountType::Type2 => {
            let frame_num_offset =
                picture_frame_num_offset(sps, frame_num, is_idr, previous_reference)?;
            let absolute_frame_num = u64::from(frame_num_offset) + u64::from(frame_num);
            let doubled = absolute_frame_num
                .checked_mul(2)
                .ok_or_else(|| Error::InvalidData("H.264 picture order count overflows".into()))?;
            let value = if !is_reference && !is_idr {
                i64::try_from(doubled)
                    .ok()
                    .and_then(|value| value.checked_sub(1))
            } else {
                i64::try_from(doubled).ok()
            }
            .ok_or_else(|| Error::InvalidData("H.264 picture order count overflows".into()))?;
            Ok(PictureOrder {
                value: picture_order_i32(value)?,
                reference_state: is_reference.then_some(PictureOrderState::FrameNum {
                    frame_num,
                    offset: frame_num_offset,
                }),
                reset_reference_state: is_reference.then_some(PictureOrderState::FrameNum {
                    frame_num: 0,
                    offset: 0,
                }),
            })
        }
    }
}

fn picture_frame_num_offset(
    sps: &Sps,
    frame_num: u32,
    is_idr: bool,
    previous_reference: Option<PictureOrderState>,
) -> Result<u32> {
    if is_idr {
        return Ok(0);
    }
    let (previous_frame_num, previous_offset) = match previous_reference {
        Some(PictureOrderState::FrameNum { frame_num, offset }) => (frame_num, offset),
        _ => (0, 0),
    };
    if previous_frame_num > frame_num {
        previous_offset
            .checked_add(1_u32 << sps.log2_max_frame_num)
            .ok_or_else(|| Error::InvalidData("H.264 frame-number offset overflows".into()))
    } else {
        Ok(previous_offset)
    }
}

fn picture_order_i32(value: i64) -> Result<i32> {
    i32::try_from(value)
        .map_err(|_| Error::InvalidData("H.264 picture order count overflows".into()))
}

#[derive(Clone, Debug)]
struct ReferenceFrame {
    structure: PictureStructure,
    frame_num: u32,
    pic_order_count: i32,
    long_term_frame_idx: Option<u32>,
    coded_width: usize,
    coded_height: usize,
    luma: Arc<[u8]>,
    cb: Arc<[u8]>,
    cr: Arc<[u8]>,
    motion_l0: Arc<[[MotionInfo; 16]]>,
    motion_l0_available: Arc<[[bool; 16]]>,
    reference_l0: Arc<[[Option<MotionReference>; 16]]>,
    motion_l1: Arc<[[MotionInfo; 16]]>,
    motion_l1_available: Arc<[[bool; 16]]>,
    reference_l1: Arc<[[Option<MotionReference>; 16]]>,
    macroblock_intra: Arc<[bool]>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct MotionReference {
    pic_order_count: i32,
    long_term: bool,
}

impl From<&ReferenceFrame> for MotionReference {
    fn from(reference: &ReferenceFrame) -> Self {
        Self {
            pic_order_count: reference.pic_order_count,
            long_term: reference.long_term_frame_idx.is_some(),
        }
    }
}

impl ReferenceFrame {
    fn unavailable(sps: &Sps, frame_num: u32) -> Result<Self> {
        let coded_width = usize::try_from(sps.coded_width)
            .map_err(|_| Error::InvalidData("H.264 recovery width overflows".into()))?;
        let coded_height = usize::try_from(sps.coded_height)
            .map_err(|_| Error::InvalidData("H.264 recovery height overflows".into()))?;
        let luma_len = coded_width
            .checked_mul(coded_height)
            .ok_or_else(|| Error::InvalidData("H.264 recovery luma size overflows".into()))?;
        let chroma_len = (coded_width / 2)
            .checked_mul(coded_height / 2)
            .ok_or_else(|| Error::InvalidData("H.264 recovery chroma size overflows".into()))?;
        let macroblock_count = (coded_width / 16)
            .checked_mul(coded_height / 16)
            .ok_or_else(|| {
                Error::InvalidData("H.264 recovery macroblock count overflows".into())
            })?;
        Ok(Self {
            structure: PictureStructure::Frame,
            frame_num,
            pic_order_count: 0,
            long_term_frame_idx: None,
            coded_width,
            coded_height,
            luma: vec![128; luma_len].into(),
            cb: vec![128; chroma_len].into(),
            cr: vec![128; chroma_len].into(),
            motion_l0: vec![[MotionInfo::default(); 16]; macroblock_count].into(),
            motion_l0_available: vec![[false; 16]; macroblock_count].into(),
            reference_l0: vec![[None; 16]; macroblock_count].into(),
            motion_l1: vec![[MotionInfo::default(); 16]; macroblock_count].into(),
            motion_l1_available: vec![[false; 16]; macroblock_count].into(),
            reference_l1: vec![[None; 16]; macroblock_count].into(),
            macroblock_intra: vec![true; macroblock_count].into(),
        })
    }

    fn colocated_motion(
        &self,
        address: usize,
        block_x: usize,
        block_y: usize,
    ) -> Option<(MotionVector, MotionReference)> {
        if self.macroblock_intra.get(address).copied().unwrap_or(true) {
            return None;
        }
        let block = luma_block_index(block_x, block_y);
        let from_list = |motion: &[[MotionInfo; 16]],
                         available: &[[bool; 16]],
                         references: &[[Option<MotionReference>; 16]]| {
            available
                .get(address)
                .is_some_and(|blocks| blocks[block])
                .then(|| motion.get(address).map(|blocks| blocks[block]))
                .flatten()
                .filter(|motion| motion.reference_index.is_some())
                .zip(references.get(address).and_then(|blocks| blocks[block]))
                .map(|(motion, reference)| {
                    (
                        MotionVector {
                            x: motion.x,
                            y: motion.y,
                        },
                        reference,
                    )
                })
        };
        from_list(
            &self.motion_l0,
            &self.motion_l0_available,
            &self.reference_l0,
        )
        .or_else(|| {
            from_list(
                &self.motion_l1,
                &self.motion_l1_available,
                &self.reference_l1,
            )
        })
    }

    fn colocated_zero(&self, address: usize, block_x: usize, block_y: usize) -> bool {
        if self.long_term_frame_idx.is_some()
            || self.macroblock_intra.get(address).copied().unwrap_or(true)
        {
            return false;
        }
        let small_zero = |motion: MotionInfo| {
            motion.reference_index == Some(0)
                && motion.x.unsigned_abs() <= 1
                && motion.y.unsigned_abs() <= 1
        };
        let block = luma_block_index(block_x, block_y);
        let l0_available = self
            .motion_l0_available
            .get(address)
            .is_some_and(|blocks| blocks[block]);
        if l0_available
            && let Some(motion) = self.motion_l0.get(address).map(|blocks| blocks[block])
            && motion.reference_index.is_some()
        {
            return small_zero(motion);
        }
        self.motion_l1_available
            .get(address)
            .is_some_and(|blocks| blocks[block])
            && self
                .motion_l1
                .get(address)
                .is_some_and(|blocks| small_zero(blocks[block]))
    }
}

#[allow(clippy::too_many_lines)]
fn read_reference_list0<'a>(
    reader: &mut SyntaxReader<'_>,
    decoded_references: &'a VecDeque<ReferenceFrame>,
    current_frame_num: u32,
    log2_max_frame_num: u8,
    active_reference_count: usize,
) -> Result<Vec<&'a ReferenceFrame>> {
    let max_frame_num = 1_i64 << log2_max_frame_num;
    let mut references = decoded_references
        .iter()
        .filter(|reference| reference.long_term_frame_idx.is_none())
        .collect::<Vec<_>>();
    references.sort_by_key(|reference| {
        std::cmp::Reverse(short_term_picture_number(
            reference.frame_num,
            current_frame_num,
            max_frame_num,
        ))
    });
    let mut long_term_references = decoded_references
        .iter()
        .filter(|reference| reference.long_term_frame_idx.is_some())
        .collect::<Vec<_>>();
    long_term_references.sort_by_key(|reference| reference.long_term_frame_idx);
    references.extend(long_term_references);
    modify_reference_list(
        reader,
        decoded_references,
        current_frame_num,
        log2_max_frame_num,
        active_reference_count,
        &mut references,
        "list-0",
    )?;
    Ok(references)
}

fn read_field_reference_list0<'a>(
    reader: &mut SyntaxReader<'_>,
    decoded_references: &'a VecDeque<ReferenceFrame>,
    current_frame_num: u32,
    current_structure: PictureStructure,
    log2_max_frame_num: u8,
    active_reference_count: usize,
) -> Result<Vec<&'a ReferenceFrame>> {
    let max_frame_num = 1_i64 << log2_max_frame_num;
    let mut short_term = group_field_references(decoded_references, false);
    short_term.sort_by_key(|group| {
        std::cmp::Reverse(short_term_picture_number(
            group.key,
            current_frame_num,
            max_frame_num,
        ))
    });
    let mut long_term = group_field_references(decoded_references, true);
    long_term.sort_by_key(|group| group.key);
    let mut references = expand_field_reference_groups(&short_term, current_structure);
    references.extend(expand_field_reference_groups(&long_term, current_structure));
    modify_field_reference_list(
        reader,
        decoded_references,
        current_frame_num,
        current_structure,
        log2_max_frame_num,
        active_reference_count,
        &mut references,
        "list-0",
    )?;
    Ok(references)
}

#[derive(Clone, Copy)]
struct FieldReferenceGroup<'a> {
    key: u32,
    pic_order_count: i32,
    top: Option<&'a ReferenceFrame>,
    bottom: Option<&'a ReferenceFrame>,
}

fn group_field_references(
    decoded_references: &VecDeque<ReferenceFrame>,
    long_term: bool,
) -> Vec<FieldReferenceGroup<'_>> {
    let mut groups = Vec::<FieldReferenceGroup<'_>>::new();
    for reference in decoded_references.iter().filter(|reference| {
        reference.structure != PictureStructure::Frame
            && reference.long_term_frame_idx.is_some() == long_term
    }) {
        let key = reference.long_term_frame_idx.unwrap_or(reference.frame_num);
        let index = groups.iter().position(|group| group.key == key);
        let group = if let Some(index) = index {
            &mut groups[index]
        } else {
            groups.push(FieldReferenceGroup {
                key,
                pic_order_count: reference.pic_order_count,
                top: None,
                bottom: None,
            });
            groups.last_mut().expect("field-reference group was added")
        };
        group.pic_order_count = group.pic_order_count.min(reference.pic_order_count);
        match reference.structure {
            PictureStructure::TopField if group.top.is_none() => group.top = Some(reference),
            PictureStructure::BottomField if group.bottom.is_none() => {
                group.bottom = Some(reference);
            }
            _ => {}
        }
    }
    groups
}

fn field_from_group<'a>(
    group: &FieldReferenceGroup<'a>,
    structure: PictureStructure,
) -> Option<&'a ReferenceFrame> {
    match structure {
        PictureStructure::TopField => group.top,
        PictureStructure::BottomField => group.bottom,
        PictureStructure::Frame => None,
    }
}

fn opposite_field(structure: PictureStructure) -> PictureStructure {
    match structure {
        PictureStructure::TopField => PictureStructure::BottomField,
        PictureStructure::BottomField => PictureStructure::TopField,
        PictureStructure::Frame => PictureStructure::Frame,
    }
}

fn expand_field_reference_groups<'a>(
    groups: &[FieldReferenceGroup<'a>],
    current_structure: PictureStructure,
) -> Vec<&'a ReferenceFrame> {
    let opposite = opposite_field(current_structure);
    let mut indices = [0_usize; 2];
    let mut references = Vec::new();
    while indices[0] < groups.len() || indices[1] < groups.len() {
        for (cursor, structure) in [current_structure, opposite].into_iter().enumerate() {
            while indices[cursor] < groups.len()
                && field_from_group(&groups[indices[cursor]], structure).is_none()
            {
                indices[cursor] += 1;
            }
            if let Some(group) = groups.get(indices[cursor]) {
                references.push(
                    field_from_group(group, structure)
                        .expect("field-reference cursor stopped on an available field"),
                );
                indices[cursor] += 1;
            }
        }
    }
    references
}

#[allow(clippy::too_many_arguments)]
fn read_field_b_reference_lists<'a>(
    reader: &mut SyntaxReader<'_>,
    decoded_references: &'a VecDeque<ReferenceFrame>,
    current_frame_num: u32,
    current_structure: PictureStructure,
    current_pic_order_count: i32,
    log2_max_frame_num: u8,
    active_l0_count: usize,
    active_l1_count: usize,
) -> Result<(Vec<&'a ReferenceFrame>, Vec<&'a ReferenceFrame>)> {
    let short_term = group_field_references(decoded_references, false);
    let mut before = short_term
        .iter()
        .copied()
        .filter(|group| group.pic_order_count < current_pic_order_count)
        .collect::<Vec<_>>();
    before.sort_by_key(|group| std::cmp::Reverse(group.pic_order_count));
    let mut after = short_term
        .iter()
        .copied()
        .filter(|group| group.pic_order_count > current_pic_order_count)
        .collect::<Vec<_>>();
    after.sort_by_key(|group| group.pic_order_count);
    let mut long_term = group_field_references(decoded_references, true);
    long_term.sort_by_key(|group| group.key);

    let mut l0_groups = before.clone();
    l0_groups.extend(after.iter().copied());
    let mut l1_groups = after;
    l1_groups.extend(before);
    let mut list0 = expand_field_reference_groups(&l0_groups, current_structure);
    let mut list1 = expand_field_reference_groups(&l1_groups, current_structure);
    let long_term = expand_field_reference_groups(&long_term, current_structure);
    list0.extend(long_term.iter().copied());
    list1.extend(long_term);
    if list1.len() > 1
        && list0.len() == list1.len()
        && list0
            .iter()
            .zip(&list1)
            .all(|(left, right)| std::ptr::eq(*left, *right))
    {
        list1.swap(0, 1);
    }
    modify_field_reference_list(
        reader,
        decoded_references,
        current_frame_num,
        current_structure,
        log2_max_frame_num,
        active_l0_count,
        &mut list0,
        "list-0",
    )?;
    modify_field_reference_list(
        reader,
        decoded_references,
        current_frame_num,
        current_structure,
        log2_max_frame_num,
        active_l1_count,
        &mut list1,
        "list-1",
    )?;
    Ok((list0, list1))
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn modify_field_reference_list<'a>(
    reader: &mut SyntaxReader<'_>,
    decoded_references: &'a VecDeque<ReferenceFrame>,
    current_frame_num: u32,
    current_structure: PictureStructure,
    log2_max_frame_num: u8,
    active_reference_count: usize,
    references: &mut Vec<&'a ReferenceFrame>,
    list_name: &str,
) -> Result<()> {
    let max_pic_num = 2_i64 << log2_max_frame_num;
    let current_pic_num = i64::from(current_frame_num)
        .checked_mul(2)
        .and_then(|value| value.checked_add(1))
        .ok_or_else(|| Error::InvalidData("H.264 field picture number overflows".into()))?;
    if reader.bit()? {
        let mut predicted_pic_num = current_pic_num;
        let mut reference_index = 0_usize;
        loop {
            let modification = reader.ue()?;
            if modification == 3 {
                break;
            }
            if reference_index >= active_reference_count {
                return Err(Error::InvalidData(format!(
                    "H.264 field {list_name} modification exceeds the active reference count"
                )));
            }
            let selected = match modification {
                0 | 1 => {
                    let difference = i64::from(reader.ue()?) + 1;
                    if difference > max_pic_num {
                        return Err(Error::InvalidData(format!(
                            "H.264 field {list_name} picture-number difference exceeds MaxPicNum"
                        )));
                    }
                    let picture_number_no_wrap = if modification == 0 {
                        (predicted_pic_num - difference).rem_euclid(max_pic_num)
                    } else {
                        (predicted_pic_num + difference).rem_euclid(max_pic_num)
                    };
                    predicted_pic_num = picture_number_no_wrap;
                    let frame_num = u32::try_from(picture_number_no_wrap >> 1)
                        .expect("bounded H.264 field frame number fits u32");
                    let structure = if picture_number_no_wrap & 1 == 0 {
                        opposite_field(current_structure)
                    } else {
                        current_structure
                    };
                    decoded_references
                        .iter()
                        .find(|reference| {
                            reference.long_term_frame_idx.is_none()
                                && reference.frame_num == frame_num
                                && reference.structure == structure
                        })
                        .ok_or_else(|| {
                            let picture_number = if picture_number_no_wrap > current_pic_num {
                                picture_number_no_wrap - max_pic_num
                            } else {
                                picture_number_no_wrap
                            };
                            Error::InvalidData(format!(
                                "H.264 field {list_name} modification selects unavailable short-term picture {picture_number}"
                            ))
                        })?
                }
                2 => {
                    let long_term_pic_num = reader.ue()?;
                    let long_term_frame_idx = long_term_pic_num >> 1;
                    let structure = if long_term_pic_num & 1 == 0 {
                        opposite_field(current_structure)
                    } else {
                        current_structure
                    };
                    decoded_references
                        .iter()
                        .find(|reference| {
                            reference.long_term_frame_idx == Some(long_term_frame_idx)
                                && reference.structure == structure
                        })
                        .ok_or_else(|| {
                            Error::InvalidData(format!(
                                "H.264 field {list_name} modification selects unavailable long-term picture {long_term_pic_num}"
                            ))
                        })?
                }
                _ => {
                    return Err(Error::InvalidData(format!(
                        "invalid H.264 field {list_name} modification idc {modification}"
                    )));
                }
            };
            references.insert(reference_index, selected);
            let mut duplicate_index = reference_index + 1;
            while duplicate_index < references.len() {
                if std::ptr::eq(references[duplicate_index], selected) {
                    references.remove(duplicate_index);
                } else {
                    duplicate_index += 1;
                }
            }
            reference_index += 1;
        }
    }
    references.truncate(active_reference_count);
    if references.len() != active_reference_count {
        return Err(Error::InvalidData(format!(
            "H.264 field picture requests {active_reference_count} {list_name} references, but only {} fields are decoded",
            references.len()
        )));
    }
    Ok(())
}

fn read_b_reference_lists<'a>(
    reader: &mut SyntaxReader<'_>,
    decoded_references: &'a VecDeque<ReferenceFrame>,
    current_frame_num: u32,
    current_pic_order_count: i32,
    log2_max_frame_num: u8,
    active_l0_count: usize,
    active_l1_count: usize,
) -> Result<(Vec<&'a ReferenceFrame>, Vec<&'a ReferenceFrame>)> {
    let mut before = decoded_references
        .iter()
        .filter(|reference| {
            reference.long_term_frame_idx.is_none()
                && reference.pic_order_count < current_pic_order_count
        })
        .collect::<Vec<_>>();
    before.sort_by_key(|reference| std::cmp::Reverse(reference.pic_order_count));
    let mut after = decoded_references
        .iter()
        .filter(|reference| {
            reference.long_term_frame_idx.is_none()
                && reference.pic_order_count > current_pic_order_count
        })
        .collect::<Vec<_>>();
    after.sort_by_key(|reference| reference.pic_order_count);
    let mut long_term = decoded_references
        .iter()
        .filter(|reference| reference.long_term_frame_idx.is_some())
        .collect::<Vec<_>>();
    long_term.sort_by_key(|reference| reference.long_term_frame_idx);

    let mut list0 = before.clone();
    list0.extend(after.iter().copied());
    list0.extend(long_term.iter().copied());
    let mut list1 = after;
    list1.extend(before);
    list1.extend(long_term);
    if list1.len() > 1
        && list0.len() == list1.len()
        && list0
            .iter()
            .zip(&list1)
            .all(|(left, right)| std::ptr::eq(*left, *right))
    {
        list1.swap(0, 1);
    }
    modify_reference_list(
        reader,
        decoded_references,
        current_frame_num,
        log2_max_frame_num,
        active_l0_count,
        &mut list0,
        "list-0",
    )?;
    modify_reference_list(
        reader,
        decoded_references,
        current_frame_num,
        log2_max_frame_num,
        active_l1_count,
        &mut list1,
        "list-1",
    )?;
    Ok((list0, list1))
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn modify_reference_list<'a>(
    reader: &mut SyntaxReader<'_>,
    decoded_references: &'a VecDeque<ReferenceFrame>,
    current_frame_num: u32,
    log2_max_frame_num: u8,
    active_reference_count: usize,
    references: &mut Vec<&'a ReferenceFrame>,
    list_name: &str,
) -> Result<()> {
    let max_frame_num = 1_i64 << log2_max_frame_num;
    let current_pic_num = i64::from(current_frame_num);
    if reader.bit()? {
        let mut predicted_pic_num = current_pic_num;
        let mut reference_index = 0_usize;
        loop {
            let modification = reader.ue()?;
            if modification == 3 {
                break;
            }
            if reference_index >= active_reference_count {
                return Err(Error::InvalidData(format!(
                    "H.264 {list_name} modification exceeds the active reference count"
                )));
            }
            let selected = match modification {
                0 | 1 => {
                    let difference = i64::from(reader.ue()?) + 1;
                    if difference > max_frame_num {
                        return Err(Error::InvalidData(
                            "H.264 list-0 picture-number difference exceeds MaxFrameNum".into(),
                        ));
                    }
                    let picture_number_no_wrap = if modification == 0 {
                        let value = predicted_pic_num - difference;
                        if value < 0 {
                            value + max_frame_num
                        } else {
                            value
                        }
                    } else {
                        let value = predicted_pic_num + difference;
                        if value >= max_frame_num {
                            value - max_frame_num
                        } else {
                            value
                        }
                    };
                    predicted_pic_num = picture_number_no_wrap;
                    let picture_number = if picture_number_no_wrap > current_pic_num {
                        picture_number_no_wrap - max_frame_num
                    } else {
                        picture_number_no_wrap
                    };
                    decoded_references
                        .iter()
                        .find(|reference| {
                            reference.long_term_frame_idx.is_none()
                                && short_term_picture_number(
                                    reference.frame_num,
                                    current_frame_num,
                                    max_frame_num,
                                ) == picture_number
                        })
                        .ok_or_else(|| {
                            Error::InvalidData(format!(
                                "H.264 list-0 modification selects unavailable short-term picture {picture_number}"
                            ))
                        })?
                }
                2 => {
                    let long_term_pic_num = reader.ue()?;
                    decoded_references
                        .iter()
                        .find(|reference| {
                            reference.long_term_frame_idx == Some(long_term_pic_num)
                        })
                        .ok_or_else(|| {
                            Error::InvalidData(format!(
                                "H.264 list-0 modification selects unavailable long-term picture {long_term_pic_num}"
                            ))
                        })?
                }
                _ => {
                    return Err(Error::InvalidData(format!(
                        "invalid H.264 {list_name} modification idc {modification}"
                    )));
                }
            };
            references.insert(reference_index, selected);
            let mut duplicate_index = reference_index + 1;
            while duplicate_index < references.len() {
                if std::ptr::eq(references[duplicate_index], selected) {
                    references.remove(duplicate_index);
                } else {
                    duplicate_index += 1;
                }
            }
            reference_index += 1;
        }
    }
    references.truncate(active_reference_count);
    if references.len() != active_reference_count {
        return Err(Error::InvalidData(format!(
            "H.264 picture requests {active_reference_count} {list_name} references, but only {} references are decoded",
            references.len()
        )));
    }
    Ok(())
}

fn short_term_picture_number(frame_num: u32, current_frame_num: u32, max_frame_num: i64) -> i64 {
    let frame_num = i64::from(frame_num);
    if frame_num > i64::from(current_frame_num) {
        frame_num - max_frame_num
    } else {
        frame_num
    }
}

fn retain_reference(
    references: &mut VecDeque<ReferenceFrame>,
    frame: ReferenceFrame,
    max_num_ref_frames: u32,
) {
    let maximum = usize::try_from(max_num_ref_frames).unwrap_or(usize::MAX);
    if maximum == 0 {
        references.clear();
        return;
    }
    references.push_front(frame);
    references.truncate(maximum);
}

fn retain_field_reference(
    references: &mut VecDeque<ReferenceFrame>,
    field: ReferenceFrame,
    max_num_ref_frames: u32,
) {
    let maximum = usize::try_from(max_num_ref_frames)
        .unwrap_or(usize::MAX / 2)
        .saturating_mul(2);
    if maximum == 0 {
        references.clear();
        return;
    }
    references.push_front(field);
    references.truncate(maximum);
}

fn synchronize_frame_references(
    frame_references: &mut VecDeque<ReferenceFrame>,
    field_references: &VecDeque<ReferenceFrame>,
    mut current: ReferenceFrame,
    max_num_ref_frames: u32,
) {
    let paired_long_term_index = |frame_num| {
        let top = field_references.iter().find(|reference| {
            reference.frame_num == frame_num && reference.structure == PictureStructure::TopField
        })?;
        let bottom = field_references.iter().find(|reference| {
            reference.frame_num == frame_num && reference.structure == PictureStructure::BottomField
        })?;
        (top.long_term_frame_idx == bottom.long_term_frame_idx).then_some(top.long_term_frame_idx)
    };
    frame_references.retain(|reference| {
        paired_long_term_index(reference.frame_num)
            .is_some_and(|index| index == reference.long_term_frame_idx)
    });
    let Some(long_term_frame_idx) = paired_long_term_index(current.frame_num) else {
        return;
    };
    current.long_term_frame_idx = long_term_frame_idx;
    frame_references.retain(|reference| {
        reference.frame_num != current.frame_num
            && long_term_frame_idx.is_none_or(|index| reference.long_term_frame_idx != Some(index))
    });
    retain_reference(frame_references, current, max_num_ref_frames);
}

#[allow(clippy::too_many_lines)]
fn apply_field_reference_marking(
    references: &mut VecDeque<ReferenceFrame>,
    mut current: ReferenceFrame,
    max_num_ref_frames: u32,
    log2_max_frame_num: u8,
    marking: ReferenceMarking,
    max_long_term_frame_idx: &mut Option<u32>,
) -> Result<()> {
    let maximum = usize::try_from(max_num_ref_frames)
        .unwrap_or(usize::MAX / 2)
        .saturating_mul(2);
    if maximum == 0 {
        references.clear();
        *max_long_term_frame_idx = None;
        return Ok(());
    }
    let mut updated_references = references.clone();
    let mut updated_max_long_term_frame_idx = *max_long_term_frame_idx;
    match marking {
        ReferenceMarking::SlidingWindow => {
            if updated_references.len() >= maximum {
                let oldest_short_term = updated_references
                    .iter()
                    .rposition(|reference| reference.long_term_frame_idx.is_none())
                    .ok_or_else(|| {
                        Error::InvalidData(
                            "H.264 field sliding-window DPB is full of long-term references".into(),
                        )
                    })?;
                updated_references.remove(oldest_short_term);
            }
        }
        ReferenceMarking::Adaptive(operations) => {
            let max_pic_num = 2_u32 << log2_max_frame_num;
            let mut current_long_term_frame_idx = None;
            for operation in operations {
                match operation {
                    MemoryManagementControlOperation::ForgetShortTerm {
                        difference_of_pic_nums_minus1,
                    } => {
                        let (frame_num, structure) = mmco_short_term_field(
                            current.frame_num,
                            current.structure,
                            difference_of_pic_nums_minus1,
                            max_pic_num,
                        )?;
                        remove_short_term_field(&mut updated_references, frame_num, structure)?;
                    }
                    MemoryManagementControlOperation::ForgetLongTerm { long_term_pic_num } => {
                        let (long_term_frame_idx, structure) =
                            long_term_field(long_term_pic_num, current.structure)?;
                        remove_long_term_field(
                            &mut updated_references,
                            long_term_frame_idx,
                            structure,
                        )?;
                    }
                    MemoryManagementControlOperation::ShortTermToLongTerm {
                        difference_of_pic_nums_minus1,
                        long_term_frame_idx,
                    } => {
                        require_allowed_long_term_index(
                            long_term_frame_idx,
                            updated_max_long_term_frame_idx,
                        )?;
                        let (frame_num, structure) = mmco_short_term_field(
                            current.frame_num,
                            current.structure,
                            difference_of_pic_nums_minus1,
                            max_pic_num,
                        )?;
                        let position = updated_references
                            .iter()
                            .position(|reference| {
                                reference.long_term_frame_idx.is_none()
                                    && reference.frame_num == frame_num
                                    && reference.structure == structure
                            })
                            .ok_or_else(|| {
                                Error::InvalidData(format!(
                                    "H.264 MMCO 3 selects unavailable short-term field {frame_num:?}/{structure:?}"
                                ))
                            })?;
                        let mut reference = updated_references
                            .remove(position)
                            .expect("located H.264 field reference remains present");
                        updated_references.retain(|candidate| {
                            candidate.long_term_frame_idx != Some(long_term_frame_idx)
                        });
                        reference.long_term_frame_idx = Some(long_term_frame_idx);
                        updated_references.push_back(reference);
                    }
                    MemoryManagementControlOperation::SetMaxLongTermFrameIdx {
                        max_long_term_frame_idx: maximum_index,
                    } => {
                        updated_max_long_term_frame_idx = maximum_index;
                        updated_references.retain(|reference| {
                            reference.long_term_frame_idx.is_none()
                                || reference.long_term_frame_idx <= maximum_index
                        });
                    }
                    MemoryManagementControlOperation::Reset => {
                        updated_references.clear();
                        updated_max_long_term_frame_idx = None;
                        current.frame_num = 0;
                        current.pic_order_count = 0;
                    }
                    MemoryManagementControlOperation::MarkCurrentLongTerm {
                        long_term_frame_idx,
                    } => {
                        require_allowed_long_term_index(
                            long_term_frame_idx,
                            updated_max_long_term_frame_idx,
                        )?;
                        updated_references.retain(|reference| {
                            reference.long_term_frame_idx != Some(long_term_frame_idx)
                                || (reference.frame_num == current.frame_num
                                    && reference.structure != current.structure)
                        });
                        current_long_term_frame_idx = Some(long_term_frame_idx);
                    }
                }
            }
            current.long_term_frame_idx = current_long_term_frame_idx;
        }
    }
    updated_references.push_front(current);
    if updated_references.len() > maximum {
        return Err(Error::InvalidData(format!(
            "H.264 field decoded-picture buffer exceeds twice max_num_ref_frames {max_num_ref_frames}"
        )));
    }
    *references = updated_references;
    *max_long_term_frame_idx = updated_max_long_term_frame_idx;
    Ok(())
}

fn mmco_short_term_field(
    current_frame_num: u32,
    current_structure: PictureStructure,
    difference_of_pic_nums_minus1: u32,
    max_pic_num: u32,
) -> Result<(u32, PictureStructure)> {
    let difference = difference_of_pic_nums_minus1
        .checked_add(1)
        .filter(|&value| value <= max_pic_num)
        .ok_or_else(|| {
            Error::InvalidData("H.264 MMCO field-picture difference exceeds MaxPicNum".into())
        })?;
    let current_pic_num = current_frame_num
        .checked_mul(2)
        .and_then(|value| value.checked_add(1))
        .ok_or_else(|| Error::InvalidData("H.264 current field picture number overflows".into()))?;
    let picture_number = (current_pic_num + max_pic_num - difference) % max_pic_num;
    let structure = if picture_number.is_multiple_of(2) {
        opposite_field(current_structure)
    } else {
        current_structure
    };
    Ok((picture_number >> 1, structure))
}

fn long_term_field(
    long_term_pic_num: u32,
    current_structure: PictureStructure,
) -> Result<(u32, PictureStructure)> {
    if long_term_pic_num >= 32 {
        return Err(Error::InvalidData(format!(
            "H.264 long_term_pic_num {long_term_pic_num} exceeds the field-picture limit"
        )));
    }
    let structure = if long_term_pic_num.is_multiple_of(2) {
        opposite_field(current_structure)
    } else {
        current_structure
    };
    Ok((long_term_pic_num >> 1, structure))
}

fn remove_short_term_field(
    references: &mut VecDeque<ReferenceFrame>,
    frame_num: u32,
    structure: PictureStructure,
) -> Result<()> {
    let position = references
        .iter()
        .position(|reference| {
            reference.long_term_frame_idx.is_none()
                && reference.frame_num == frame_num
                && reference.structure == structure
        })
        .ok_or_else(|| {
            Error::InvalidData(format!(
                "H.264 MMCO selects unavailable short-term field {frame_num:?}/{structure:?}"
            ))
        })?;
    references.remove(position);
    Ok(())
}

fn remove_long_term_field(
    references: &mut VecDeque<ReferenceFrame>,
    long_term_frame_idx: u32,
    structure: PictureStructure,
) -> Result<()> {
    let position = references
        .iter()
        .position(|reference| {
            reference.long_term_frame_idx == Some(long_term_frame_idx)
                && reference.structure == structure
        })
        .ok_or_else(|| {
            Error::InvalidData(format!(
                "H.264 MMCO selects unavailable long-term field {long_term_frame_idx:?}/{structure:?}"
            ))
        })?;
    references.remove(position);
    Ok(())
}

fn ensure_recovery_references(
    references: &mut VecDeque<ReferenceFrame>,
    sps: &Sps,
    current_frame_num: u32,
    required: usize,
) -> Result<()> {
    let max_frame_num = 1_u32 << sps.log2_max_frame_num;
    while references.len() < required {
        let distance = u32::try_from(references.len() + 1)
            .map_err(|_| Error::InvalidData("H.264 recovery reference count overflows".into()))?;
        let frame_num = current_frame_num
            .checked_add(max_frame_num)
            .and_then(|value| value.checked_sub(distance))
            .ok_or_else(|| Error::InvalidData("H.264 recovery frame number underflows".into()))?
            % max_frame_num;
        references.push_back(ReferenceFrame::unavailable(sps, frame_num)?);
    }
    Ok(())
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum ReferenceMarking {
    SlidingWindow,
    Adaptive(Vec<MemoryManagementControlOperation>),
}

impl ReferenceMarking {
    fn resets_picture_order(&self) -> bool {
        matches!(
            self,
            Self::Adaptive(operations)
                if operations.contains(&MemoryManagementControlOperation::Reset)
        )
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum MemoryManagementControlOperation {
    ForgetShortTerm {
        difference_of_pic_nums_minus1: u32,
    },
    ForgetLongTerm {
        long_term_pic_num: u32,
    },
    ShortTermToLongTerm {
        difference_of_pic_nums_minus1: u32,
        long_term_frame_idx: u32,
    },
    SetMaxLongTermFrameIdx {
        max_long_term_frame_idx: Option<u32>,
    },
    Reset,
    MarkCurrentLongTerm {
        long_term_frame_idx: u32,
    },
}

fn read_reference_marking(reader: &mut SyntaxReader<'_>) -> Result<ReferenceMarking> {
    if !reader.bit()? {
        return Ok(ReferenceMarking::SlidingWindow);
    }
    let mut operations = Vec::new();
    let mut saw_set_max = false;
    let mut saw_reset = false;
    let mut saw_current_long_term = false;
    loop {
        let operation = reader.ue()?;
        let operation = match operation {
            0 => break,
            1 => MemoryManagementControlOperation::ForgetShortTerm {
                difference_of_pic_nums_minus1: reader.ue()?,
            },
            2 => MemoryManagementControlOperation::ForgetLongTerm {
                long_term_pic_num: read_long_term_picture_number(reader)?,
            },
            3 => MemoryManagementControlOperation::ShortTermToLongTerm {
                difference_of_pic_nums_minus1: reader.ue()?,
                long_term_frame_idx: read_long_term_index(reader, "long_term_frame_idx")?,
            },
            4 => {
                if saw_set_max {
                    return Err(Error::InvalidData(
                        "H.264 decoded-reference marking repeats MMCO 4".into(),
                    ));
                }
                saw_set_max = true;
                let plus1 = reader.ue()?;
                if plus1 > 16 {
                    return Err(Error::InvalidData(
                        "H.264 max_long_term_frame_idx_plus1 exceeds 16".into(),
                    ));
                }
                MemoryManagementControlOperation::SetMaxLongTermFrameIdx {
                    max_long_term_frame_idx: plus1.checked_sub(1),
                }
            }
            5 => {
                if saw_reset
                    || saw_current_long_term
                    || operations.iter().any(|operation| {
                        matches!(
                            operation,
                            MemoryManagementControlOperation::ForgetShortTerm { .. }
                                | MemoryManagementControlOperation::ForgetLongTerm { .. }
                                | MemoryManagementControlOperation::ShortTermToLongTerm { .. }
                        )
                    })
                {
                    return Err(Error::InvalidData(
                        "invalid H.264 MMCO 5 operation ordering".into(),
                    ));
                }
                saw_reset = true;
                MemoryManagementControlOperation::Reset
            }
            6 => {
                if saw_current_long_term {
                    return Err(Error::InvalidData(
                        "H.264 decoded-reference marking repeats MMCO 6".into(),
                    ));
                }
                saw_current_long_term = true;
                MemoryManagementControlOperation::MarkCurrentLongTerm {
                    long_term_frame_idx: read_long_term_index(reader, "long_term_frame_idx")?,
                }
            }
            _ => {
                return Err(Error::InvalidData(format!(
                    "invalid H.264 memory_management_control_operation {operation}"
                )));
            }
        };
        operations.push(operation);
        if operations.len() > 32 {
            return Err(Error::InvalidData(
                "too many H.264 memory-management control operations".into(),
            ));
        }
    }
    Ok(ReferenceMarking::Adaptive(operations))
}

fn read_long_term_index(reader: &mut SyntaxReader<'_>, name: &str) -> Result<u32> {
    let value = reader.ue()?;
    if value >= 16 {
        return Err(Error::InvalidData(format!(
            "H.264 {name} {value} exceeds the frame-picture limit"
        )));
    }
    Ok(value)
}

fn read_long_term_picture_number(reader: &mut SyntaxReader<'_>) -> Result<u32> {
    let value = reader.ue()?;
    if value >= 32 {
        return Err(Error::InvalidData(format!(
            "H.264 long_term_pic_num {value} exceeds the field-picture limit"
        )));
    }
    Ok(value)
}

#[allow(clippy::too_many_lines)]
fn apply_reference_marking(
    references: &mut VecDeque<ReferenceFrame>,
    mut current: ReferenceFrame,
    max_num_ref_frames: u32,
    log2_max_frame_num: u8,
    marking: ReferenceMarking,
    max_long_term_frame_idx: &mut Option<u32>,
) -> Result<()> {
    let maximum = usize::try_from(max_num_ref_frames).unwrap_or(usize::MAX);
    if maximum == 0 {
        references.clear();
        *max_long_term_frame_idx = None;
        return Ok(());
    }
    let mut updated_references = references.clone();
    let mut updated_max_long_term_frame_idx = *max_long_term_frame_idx;
    match marking {
        ReferenceMarking::SlidingWindow => {
            if updated_references.len() >= maximum {
                let oldest_short_term = updated_references
                    .iter()
                    .rposition(|reference| reference.long_term_frame_idx.is_none())
                    .ok_or_else(|| {
                        Error::InvalidData(
                            "H.264 sliding-window DPB is full of long-term references".into(),
                        )
                    })?;
                updated_references.remove(oldest_short_term);
            }
        }
        ReferenceMarking::Adaptive(operations) => {
            let max_frame_num = 1_u32 << log2_max_frame_num;
            let mut current_long_term_frame_idx = None;
            for operation in operations {
                match operation {
                    MemoryManagementControlOperation::ForgetShortTerm {
                        difference_of_pic_nums_minus1,
                    } => {
                        let frame_num = mmco_short_term_frame_num(
                            current.frame_num,
                            difference_of_pic_nums_minus1,
                            max_frame_num,
                        )?;
                        remove_short_term_reference(&mut updated_references, frame_num)?;
                    }
                    MemoryManagementControlOperation::ForgetLongTerm { long_term_pic_num } => {
                        if current_long_term_frame_idx == Some(long_term_pic_num) {
                            return Err(Error::InvalidData(
                                "H.264 MMCO 2 cannot discard the current long-term picture".into(),
                            ));
                        }
                        remove_long_term_reference(&mut updated_references, long_term_pic_num)?;
                    }
                    MemoryManagementControlOperation::ShortTermToLongTerm {
                        difference_of_pic_nums_minus1,
                        long_term_frame_idx,
                    } => {
                        if current_long_term_frame_idx == Some(long_term_frame_idx) {
                            return Err(Error::InvalidData(
                                "H.264 MMCO 3 cannot replace the current long-term picture".into(),
                            ));
                        }
                        require_allowed_long_term_index(
                            long_term_frame_idx,
                            updated_max_long_term_frame_idx,
                        )?;
                        let frame_num = mmco_short_term_frame_num(
                            current.frame_num,
                            difference_of_pic_nums_minus1,
                            max_frame_num,
                        )?;
                        let position = updated_references
                            .iter()
                            .position(|reference| {
                                reference.long_term_frame_idx.is_none()
                                    && reference.frame_num == frame_num
                            })
                            .ok_or_else(|| {
                                Error::InvalidData(format!(
                                    "H.264 MMCO 3 selects unavailable short-term frame {frame_num}"
                                ))
                            })?;
                        let mut reference = updated_references
                            .remove(position)
                            .expect("located H.264 reference remains present");
                        updated_references.retain(|candidate| {
                            candidate.long_term_frame_idx != Some(long_term_frame_idx)
                        });
                        reference.long_term_frame_idx = Some(long_term_frame_idx);
                        updated_references.push_back(reference);
                    }
                    MemoryManagementControlOperation::SetMaxLongTermFrameIdx {
                        max_long_term_frame_idx: maximum_index,
                    } => {
                        if current_long_term_frame_idx.is_some_and(|index| {
                            maximum_index.is_none_or(|maximum| index > maximum)
                        }) {
                            return Err(Error::InvalidData(
                                "H.264 MMCO 4 cannot exclude the current long-term picture".into(),
                            ));
                        }
                        updated_max_long_term_frame_idx = maximum_index;
                        updated_references.retain(|reference| {
                            reference.long_term_frame_idx.is_none()
                                || reference.long_term_frame_idx <= maximum_index
                        });
                    }
                    MemoryManagementControlOperation::Reset => {
                        updated_references.clear();
                        updated_max_long_term_frame_idx = None;
                        current.frame_num = 0;
                        current.pic_order_count = 0;
                    }
                    MemoryManagementControlOperation::MarkCurrentLongTerm {
                        long_term_frame_idx,
                    } => {
                        require_allowed_long_term_index(
                            long_term_frame_idx,
                            updated_max_long_term_frame_idx,
                        )?;
                        updated_references.retain(|reference| {
                            reference.long_term_frame_idx != Some(long_term_frame_idx)
                        });
                        current_long_term_frame_idx = Some(long_term_frame_idx);
                    }
                }
            }
            current.long_term_frame_idx = current_long_term_frame_idx;
        }
    }
    updated_references.push_front(current);
    if updated_references.len() > maximum {
        return Err(Error::InvalidData(format!(
            "H.264 decoded-picture buffer exceeds max_num_ref_frames {max_num_ref_frames}"
        )));
    }
    *references = updated_references;
    *max_long_term_frame_idx = updated_max_long_term_frame_idx;
    Ok(())
}

fn mmco_short_term_frame_num(
    current_frame_num: u32,
    difference_of_pic_nums_minus1: u32,
    max_frame_num: u32,
) -> Result<u32> {
    let difference = difference_of_pic_nums_minus1
        .checked_add(1)
        .filter(|&value| value <= max_frame_num)
        .ok_or_else(|| {
            Error::InvalidData("H.264 MMCO picture-number difference exceeds MaxFrameNum".into())
        })?;
    Ok((current_frame_num + max_frame_num - difference) % max_frame_num)
}

fn remove_short_term_reference(
    references: &mut VecDeque<ReferenceFrame>,
    frame_num: u32,
) -> Result<()> {
    let position = references
        .iter()
        .position(|reference| {
            reference.long_term_frame_idx.is_none() && reference.frame_num == frame_num
        })
        .ok_or_else(|| {
            Error::InvalidData(format!(
                "H.264 MMCO selects unavailable short-term frame {frame_num}"
            ))
        })?;
    references.remove(position);
    Ok(())
}

fn remove_long_term_reference(
    references: &mut VecDeque<ReferenceFrame>,
    long_term_pic_num: u32,
) -> Result<()> {
    let position = references
        .iter()
        .position(|reference| reference.long_term_frame_idx == Some(long_term_pic_num))
        .ok_or_else(|| {
            Error::InvalidData(format!(
                "H.264 MMCO selects unavailable long-term picture {long_term_pic_num}"
            ))
        })?;
    references.remove(position);
    Ok(())
}

fn require_allowed_long_term_index(index: u32, maximum: Option<u32>) -> Result<()> {
    if maximum.is_some_and(|maximum| index <= maximum) {
        Ok(())
    } else {
        Err(Error::InvalidData(format!(
            "H.264 long-term frame index {index} exceeds MaxLongTermFrameIdx"
        )))
    }
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

#[derive(Clone, Debug, Eq, PartialEq)]
struct BPredictionWeights {
    list0: Vec<PredictionWeights>,
    list1: Vec<PredictionWeights>,
    explicit: bool,
    implicit: bool,
    picture_order_count: i32,
}

impl BPredictionWeights {
    fn identity(list0_count: usize, list1_count: usize) -> Self {
        Self {
            list0: vec![PredictionWeights::identity(); list0_count],
            list1: vec![PredictionWeights::identity(); list1_count],
            explicit: false,
            implicit: false,
            picture_order_count: 0,
        }
    }

    fn implicit(list0_count: usize, list1_count: usize, picture_order_count: i32) -> Self {
        Self {
            list0: vec![PredictionWeights::identity(); list0_count],
            list1: vec![PredictionWeights::identity(); list1_count],
            explicit: false,
            implicit: true,
            picture_order_count,
        }
    }

    fn list0(&self, index: u8) -> Result<PredictionWeights> {
        self.list0
            .get(usize::from(index))
            .copied()
            .ok_or_else(|| Error::InvalidData("missing H.264 list-0 prediction weight".into()))
    }

    fn list1(&self, index: u8) -> Result<PredictionWeights> {
        self.list1
            .get(usize::from(index))
            .copied()
            .ok_or_else(|| Error::InvalidData("missing H.264 list-1 prediction weight".into()))
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

fn read_b_prediction_weights(
    reader: &mut SyntaxReader<'_>,
    list0_count: usize,
    list1_count: usize,
) -> Result<BPredictionWeights> {
    let luma_denominator = read_weight_denominator(reader, "luma")?;
    let chroma_denominator = read_weight_denominator(reader, "chroma")?;
    let read_list = |reader: &mut SyntaxReader<'_>, count: usize| {
        (0..count)
            .map(|_| {
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
            })
            .collect::<Result<Vec<_>>>()
    };
    Ok(BPredictionWeights {
        list0: read_list(reader, list0_count)?,
        list1: read_list(reader, list1_count)?,
        explicit: true,
        implicit: false,
        picture_order_count: 0,
    })
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

fn read_deblocking_parameters(reader: &mut SyntaxReader<'_>, pps: &Pps) -> Result<SliceDeblocking> {
    if !pps.deblocking_filter_control_present {
        return Ok(SliceDeblocking {
            parameters: Some(DeblockingParameters {
                offset_a: 0,
                offset_b: 0,
            }),
            filter_across_slice_boundaries: true,
        });
    }
    let disable = reader.ue()?;
    if disable > 2 {
        return Err(Error::InvalidData(format!(
            "invalid H.264 disable_deblocking_filter_idc {disable}"
        )));
    }
    if disable == 1 {
        return Ok(SliceDeblocking {
            parameters: None,
            filter_across_slice_boundaries: false,
        });
    }
    let alpha_div2 = reader.se()?;
    let beta_div2 = reader.se()?;
    if !(-6..=6).contains(&alpha_div2) || !(-6..=6).contains(&beta_div2) {
        return Err(Error::InvalidData(
            "H.264 deblocking slice offsets must be in -6..=6".into(),
        ));
    }
    Ok(SliceDeblocking {
        parameters: Some(DeblockingParameters {
            offset_a: alpha_div2 * 2,
            offset_b: beta_div2 * 2,
        }),
        filter_across_slice_boundaries: disable == 0,
    })
}

fn decode_idr_macroblocks(
    reader: &mut SyntaxReader<'_>,
    buffer: &mut FrameBuffer,
    pps: &Pps,
    luma_qp: &mut i32,
    first_macroblock: usize,
    end_macroblock: usize,
    mbaff_frame: bool,
) -> Result<()> {
    if first_macroblock >= end_macroblock || end_macroblock > buffer.macroblock_count() {
        return Err(Error::InvalidData(
            "invalid H.264 I-slice macroblock range".into(),
        ));
    }
    let macroblocks_wide = buffer.coded_width / 16;
    let mut field_coded_pair = false;
    for bitstream_address in first_macroblock..end_macroblock {
        if mbaff_frame && bitstream_address.is_multiple_of(2) {
            field_coded_pair = reader.bit()?;
        }
        let macroblock_address = if mbaff_frame {
            mbaff_raster_macroblock_address(bitstream_address, macroblocks_wide)
        } else {
            bitstream_address
        };
        buffer.mbaff_field_coded[macroblock_address] = field_coded_pair;
        let macroblock_type = reader.ue()?;
        match macroblock_type {
            0 => decode_intra_nxn(
                reader,
                buffer,
                macroblock_address,
                luma_qp,
                pps.chroma_qp_index_offset,
                pps.transform_8x8_mode,
            )?,
            25 => {
                reader.align_zero_to_byte()?;
                if field_coded_pair {
                    buffer.read_mbaff_field_macroblock(reader, bitstream_address)?;
                } else {
                    buffer.read_macroblock(reader, macroblock_address)?;
                }
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

fn mbaff_raster_macroblock_address(address: usize, macroblocks_wide: usize) -> usize {
    let pair = address / 2;
    let macroblock_x = pair % macroblocks_wide;
    let macroblock_y = (pair / macroblocks_wide) * 2 + address % 2;
    macroblock_y * macroblocks_wide + macroblock_x
}

fn require_reference_idr(unit: &NalUnit<'_>) -> Result<()> {
    if unit.header.unit_type != NalUnitType::IdrSlice || unit.header.reference_idc == 0 {
        return Err(Error::Unsupported(
            "native H.264 reconstruction currently begins at reference IDR pictures".into(),
        ));
    }
    Ok(())
}

fn non_idr_slice_type(unit: &NalUnit<'_>) -> Result<u32> {
    let rbsp = remove_emulation_prevention(
        unit.data
            .get(1..)
            .ok_or_else(|| Error::InvalidData("empty H.264 slice".into()))?,
    );
    let mut reader = SyntaxReader::new(&rbsp);
    let _first_mb_in_slice = reader.ue()?;
    Ok(reader.ue()? % 5)
}

fn i_slice_prefix(unit: &NalUnit<'_>) -> Result<(usize, u32)> {
    slice_prefix(unit, 2, "I")
}

fn p_slice_prefix(unit: &NalUnit<'_>) -> Result<(usize, u32)> {
    slice_prefix(unit, 0, "P")
}

fn b_slice_prefix(unit: &NalUnit<'_>) -> Result<(usize, u32)> {
    slice_prefix(unit, 1, "B")
}

fn slice_prefix(
    unit: &NalUnit<'_>,
    expected_slice_type: u32,
    expected_name: &str,
) -> Result<(usize, u32)> {
    let rbsp = remove_emulation_prevention(
        unit.data
            .get(1..)
            .ok_or_else(|| Error::InvalidData("empty H.264 slice".into()))?,
    );
    let mut reader = SyntaxReader::new(&rbsp);
    let first_macroblock = usize::try_from(reader.ue()?)
        .map_err(|_| Error::InvalidData("H.264 first macroblock overflows".into()))?;
    let slice_type = reader.ue()? % 5;
    if slice_type != expected_slice_type {
        return Err(Error::InvalidData(format!(
            "expected H.264 {expected_name} slice, found normalized type {slice_type}"
        )));
    }
    Ok((first_macroblock, reader.ue()?))
}

fn slice_picture_structure(unit: &NalUnit<'_>, sps: &Sps) -> Result<PictureStructure> {
    let rbsp = remove_emulation_prevention(
        unit.data
            .get(1..)
            .ok_or_else(|| Error::InvalidData("empty H.264 slice".into()))?,
    );
    let mut reader = SyntaxReader::new(&rbsp);
    let _first_macroblock = reader.ue()?;
    let _slice_type = reader.ue()?;
    let _pps_id = reader.ue()?;
    if sps.separate_colour_plane {
        let _colour_plane_id = reader.bits(2)?;
    }
    let _frame_num = reader.bits(sps.log2_max_frame_num)?;
    read_picture_structure(&mut reader, sps)
}

const INTRA4_CODED_BLOCK_PATTERN: [u8; 48] = [
    47, 31, 15, 0, 23, 27, 29, 30, 7, 11, 13, 14, 39, 43, 45, 46, 16, 3, 5, 10, 12, 19, 21, 26, 28,
    35, 37, 42, 44, 1, 2, 4, 8, 17, 18, 20, 24, 6, 9, 22, 25, 32, 33, 34, 36, 40, 38, 41,
];

const INTER_CODED_BLOCK_PATTERN: [u8; 48] = [
    0, 16, 1, 2, 4, 8, 32, 3, 5, 10, 12, 15, 47, 7, 11, 13, 14, 6, 9, 31, 35, 37, 42, 44, 33, 34,
    36, 40, 39, 43, 45, 46, 17, 18, 20, 24, 19, 21, 26, 28, 23, 27, 29, 30, 22, 25, 38, 41,
];

fn decode_intra_nxn(
    reader: &mut SyntaxReader<'_>,
    buffer: &mut FrameBuffer,
    macroblock_address: usize,
    luma_qp: &mut i32,
    chroma_qp_offset: i32,
    transform_8x8_mode: bool,
) -> Result<()> {
    if transform_8x8_mode && reader.bit()? {
        return decode_intra8x8(
            reader,
            buffer,
            macroblock_address,
            luma_qp,
            chroma_qp_offset,
        );
    }
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

fn decode_intra8x8(
    reader: &mut SyntaxReader<'_>,
    buffer: &mut FrameBuffer,
    macroblock_address: usize,
    luma_qp: &mut i32,
    chroma_qp_offset: i32,
) -> Result<()> {
    buffer.mark_intra(macroblock_address);
    let mut modes = [0_u8; 4];
    for (group, mode) in modes.iter_mut().enumerate() {
        let base = group * 4;
        let predicted = buffer.predicted_intra4_mode(macroblock_address, base);
        *mode = if reader.bit()? {
            predicted
        } else {
            let remaining = u8::try_from(reader.bits(3)?)
                .map_err(|_| Error::InvalidData("H.264 Intra8x8 mode overflows".into()))?;
            remaining + u8::from(remaining >= predicted)
        };
        if *mode > 8 {
            return Err(Error::InvalidData(format!(
                "invalid H.264 Intra8x8 prediction mode {}",
                *mode
            )));
        }
        for block in base..base + 4 {
            buffer.set_intra4_mode(macroblock_address, block, *mode);
        }
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
        .ok_or_else(|| Error::InvalidData("invalid H.264 Intra8x8 coded block pattern".into()))?;
    let pattern = INTRA4_CODED_BLOCK_PATTERN[pattern_code];
    let luma_pattern = pattern & 15;
    let chroma_pattern = pattern >> 4;
    if pattern != 0 {
        *luma_qp = (*luma_qp + reader.se()?).rem_euclid(52);
    }

    let mut luma_blocks: [Vec<i32>; 4] = std::array::from_fn(|_| vec![0; 64]);
    for (group, levels) in luma_blocks.iter_mut().enumerate() {
        for sub_block in 0..4 {
            let block_index = group * 4 + sub_block;
            if luma_pattern & (1 << group) != 0 {
                let n_c = buffer.luma_nc(macroblock_address, block_index);
                let decoded = decode_residual_block(&mut reader.bits, n_c, 16)?;
                for (coefficient, level) in decoded.coefficients.into_iter().enumerate() {
                    levels[coefficient * 4 + sub_block] = level;
                }
                buffer.set_luma_nonzero(macroblock_address, block_index, decoded.total_coeff);
            } else {
                buffer.set_luma_nonzero(macroblock_address, block_index, 0);
            }
        }
    }

    let (chroma_dc, chroma_blocks) =
        decode_chroma_blocks(reader, buffer, macroblock_address, chroma_pattern)?;
    for group in 0..4 {
        buffer.reconstruct_intra8_luma_block(
            macroblock_address,
            group,
            modes[group],
            &luma_blocks[group],
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
    if pps.num_slice_groups_minus1 != 0 {
        return Err(Error::Unsupported(
            "native H.264 slice groups are not implemented".into(),
        ));
    }
    Ok(())
}

fn validate_native_picture_mode(sps: &Sps, structure: PictureStructure) -> Result<()> {
    if sps.mb_adaptive_frame_field && structure == PictureStructure::Frame {
        return Err(Error::Unsupported(
            "native H.264 macroblock-adaptive frame-picture reconstruction is not implemented"
                .into(),
        ));
    }
    Ok(())
}

fn validate_native_intra_picture_mode(
    sps: &Sps,
    pps: &Pps,
    structure: PictureStructure,
) -> Result<()> {
    if sps.mb_adaptive_frame_field
        && structure == PictureStructure::Frame
        && pps.entropy_coding_mode
    {
        return Err(Error::Unsupported(
            "native H.264 CABAC MBAFF frame-picture reconstruction is not implemented".into(),
        ));
    }
    Ok(())
}

fn validate_native_inter_picture_mode(
    sps: &Sps,
    pps: &Pps,
    structure: PictureStructure,
) -> Result<()> {
    if sps.mb_adaptive_frame_field
        && structure == PictureStructure::Frame
        && pps.entropy_coding_mode
    {
        return Err(Error::Unsupported(
            "native H.264 CABAC MBAFF inter-picture reconstruction is not implemented".into(),
        ));
    }
    Ok(())
}

fn read_picture_structure(reader: &mut SyntaxReader<'_>, sps: &Sps) -> Result<PictureStructure> {
    if sps.frame_mbs_only || !reader.bit()? {
        return Ok(PictureStructure::Frame);
    }
    Ok(if reader.bit()? {
        PictureStructure::BottomField
    } else {
        PictureStructure::TopField
    })
}

fn require_native_frame_picture(structure: PictureStructure) -> Result<()> {
    match structure {
        PictureStructure::Frame => Ok(()),
        PictureStructure::TopField => Err(Error::Unsupported(
            "native H.264 top field-picture reconstruction is not implemented".into(),
        )),
        PictureStructure::BottomField => Err(Error::Unsupported(
            "native H.264 bottom field-picture reconstruction is not implemented".into(),
        )),
    }
}

#[derive(Debug)]
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
    reference_l0: Vec<[Option<MotionReference>; 16]>,
    motion_l1: Vec<[MotionInfo; 16]>,
    motion_l1_available: Vec<[bool; 16]>,
    reference_l1: Vec<[Option<MotionReference>; 16]>,
    macroblock_intra: Vec<bool>,
    transform_8x8: Vec<bool>,
    mbaff_frame: bool,
    mbaff_field_coded: Vec<bool>,
    transform_bypass_at_qp_zero: bool,
    chroma_intra_modes: Vec<Option<u8>>,
    scaling_matrices: ScalingMatrices,
    slice_start: usize,
}

impl FrameBuffer {
    fn new(sps: &Sps, pps: &Pps) -> Result<Self> {
        let mut buffer = Self::new_with_coded_height(sps, pps, sps.coded_height)?;
        buffer.mbaff_frame = sps.mb_adaptive_frame_field;
        Ok(buffer)
    }

    fn new_for_structure(sps: &Sps, pps: &Pps, structure: PictureStructure) -> Result<Self> {
        let coded_height = if structure == PictureStructure::Frame {
            sps.coded_height
        } else {
            sps.coded_height / 2
        };
        let mut buffer = Self::new_with_coded_height(sps, pps, coded_height)?;
        buffer.mbaff_frame = sps.mb_adaptive_frame_field && structure == PictureStructure::Frame;
        Ok(buffer)
    }

    fn new_with_coded_height(sps: &Sps, pps: &Pps, coded_height: u32) -> Result<Self> {
        let coded_width = usize::try_from(sps.coded_width)
            .map_err(|_| Error::InvalidData("H.264 coded width overflows".into()))?;
        let coded_height = usize::try_from(coded_height)
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
            reference_l0: vec![[None; 16]; macroblock_count],
            motion_l1: vec![[MotionInfo::default(); 16]; macroblock_count],
            motion_l1_available: vec![[false; 16]; macroblock_count],
            reference_l1: vec![[None; 16]; macroblock_count],
            macroblock_intra: vec![false; macroblock_count],
            transform_8x8: vec![false; macroblock_count],
            mbaff_frame: false,
            mbaff_field_coded: vec![false; macroblock_count],
            transform_bypass_at_qp_zero: sps.qpprime_y_zero_transform_bypass,
            chroma_intra_modes: vec![None; macroblock_count],
            scaling_matrices: pps.resolve_scaling_matrices(sps),
            slice_start: 0,
        })
    }

    fn weave_fields(sps: &Sps, pps: &Pps, top: &Self, bottom: &Self) -> Result<Self> {
        if top.coded_width != bottom.coded_width
            || top.coded_height != bottom.coded_height
            || top.coded_width
                != usize::try_from(sps.coded_width)
                    .map_err(|_| Error::InvalidData("H.264 field width overflows".into()))?
            || top.coded_height.checked_mul(2)
                != Some(
                    usize::try_from(sps.coded_height)
                        .map_err(|_| Error::InvalidData("H.264 field height overflows".into()))?,
                )
        {
            return Err(Error::InvalidData(
                "H.264 complementary fields have incompatible dimensions".into(),
            ));
        }
        let mut frame = Self::new(sps, pps)?;
        weave_field_plane(&mut frame.luma, frame.coded_width, &top.luma, &bottom.luma)?;
        weave_field_plane(&mut frame.cb, frame.coded_width / 2, &top.cb, &bottom.cb)?;
        weave_field_plane(&mut frame.cr, frame.coded_width / 2, &top.cr, &bottom.cr)?;
        frame.macroblock_intra.fill(true);
        Ok(frame)
    }

    fn from_reference_for_structure(
        sps: &Sps,
        pps: &Pps,
        reference: &ReferenceFrame,
        luma_qp: i32,
        structure: PictureStructure,
    ) -> Result<Self> {
        let mut buffer = Self::new_for_structure(sps, pps, structure)?;
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
        if structure != PictureStructure::Frame || sps.mb_adaptive_frame_field {
            buffer.luma.copy_from_slice(&reference.luma);
            buffer.cb.copy_from_slice(&reference.cb);
            buffer.cr.copy_from_slice(&reference.cr);
        }
        buffer.luma_qp.fill(luma_qp);
        Ok(buffer)
    }

    const fn macroblock_count(&self) -> usize {
        (self.coded_width / 16) * (self.coded_height / 16)
    }

    fn begin_slice(&mut self, first_macroblock: usize) -> Result<()> {
        if first_macroblock >= self.macroblock_count() {
            return Err(Error::InvalidData(
                "H.264 slice starts outside the coded picture".into(),
            ));
        }
        self.slice_start = first_macroblock;
        Ok(())
    }

    fn left_macroblock_available(&self, address: usize) -> bool {
        let macroblocks_wide = self.coded_width / 16;
        !address.is_multiple_of(macroblocks_wide) && address > self.slice_start
    }

    fn top_macroblock_available(&self, address: usize) -> bool {
        self.top_macroblock_address(address)
            .is_some_and(|top| top >= self.slice_start)
    }

    fn top_macroblock_address(&self, address: usize) -> Option<usize> {
        let macroblocks_wide = self.coded_width / 16;
        let distance = if self.mbaff_field_coded[address] {
            macroblocks_wide * 2
        } else {
            macroblocks_wide
        };
        address.checked_sub(distance)
    }

    fn mbaff_bitstream_address(&self, raster_address: usize) -> usize {
        let macroblocks_wide = self.coded_width / 16;
        let macroblock_x = raster_address % macroblocks_wide;
        let macroblock_y = raster_address / macroblocks_wide;
        ((macroblock_y / 2) * macroblocks_wide + macroblock_x) * 2 + macroblock_y % 2
    }

    fn mbaff_neighbor_location(
        &self,
        raster_address: usize,
        x_n: isize,
        y_n: isize,
        max_w: usize,
        max_h: usize,
    ) -> Option<(usize, usize, usize)> {
        debug_assert!(self.mbaff_frame);
        let neighbor_width = isize::try_from(max_w).ok()?;
        let neighbor_height = isize::try_from(max_h).ok()?;
        debug_assert!(x_n < 0 || y_n < 0 || x_n >= neighbor_width || y_n >= neighbor_height);
        let macroblocks_wide = self.coded_width / 16;
        let current = self.mbaff_bitstream_address(raster_address);
        let pair = current / 2;
        let pair_x = pair % macroblocks_wide;
        let pair_y = pair / macroblocks_wide;
        let current_frame = !self.mbaff_field_coded[raster_address];
        let current_top = current.is_multiple_of(2);

        let (neighbor, y_m) = if x_n < 0 && y_n < 0 {
            if current_frame || pair_x == 0 || pair_y == 0 {
                return None;
            }
            let address_d = (pair - macroblocks_wide - 1) * 2;
            if current_top {
                let d_raster = mbaff_raster_macroblock_address(address_d, macroblocks_wide);
                if self.mbaff_field_coded[d_raster] {
                    (address_d, y_n)
                } else {
                    (address_d + 1, y_n * 2)
                }
            } else {
                (address_d + 1, y_n)
            }
        } else if x_n < 0 && y_n >= 0 {
            if pair_x == 0 {
                return None;
            }
            let address_a = (pair - 1) * 2;
            let a_raster = mbaff_raster_macroblock_address(address_a, macroblocks_wide);
            let neighbor_frame = !self.mbaff_field_coded[a_raster];
            let y = y_n;
            match (current_frame, current_top, neighbor_frame) {
                (true, true, true) | (false, true, false) => (address_a, y),
                (true, true, false) => (address_a + usize::try_from(y % 2).ok()?, y / 2),
                (true, false, true) | (false, false, false) => (address_a + 1, y),
                (true, false, false) => (
                    address_a + usize::try_from(y % 2).ok()?,
                    y.midpoint(neighbor_height),
                ),
                (false, true, true) if y < neighbor_height / 2 => (address_a, y * 2),
                (false, true, true) => (address_a + 1, y * 2 - neighbor_height),
                (false, false, true) if y < neighbor_height / 2 => (address_a, y * 2 + 1),
                (false, false, true) => (address_a + 1, y * 2 + 1 - neighbor_height),
            }
        } else if x_n >= neighbor_width && y_n < 0 {
            if current_frame || pair_x + 1 >= macroblocks_wide || pair_y == 0 {
                return None;
            }
            let address_c = (pair - macroblocks_wide + 1) * 2;
            if current_top {
                let c_raster = mbaff_raster_macroblock_address(address_c, macroblocks_wide);
                if self.mbaff_field_coded[c_raster] {
                    (address_c, y_n)
                } else {
                    (address_c + 1, y_n * 2)
                }
            } else {
                (address_c + 1, y_n)
            }
        } else if x_n >= 0 && y_n < 0 {
            if current_frame && !current_top {
                (current - 1, y_n)
            } else {
                if pair_y == 0 {
                    return None;
                }
                let address_b = (pair - macroblocks_wide) * 2;
                let b_raster = mbaff_raster_macroblock_address(address_b, macroblocks_wide);
                let neighbor_frame = !self.mbaff_field_coded[b_raster];
                match (current_frame, current_top, neighbor_frame) {
                    (true, true, _) | (false, false, _) => (address_b + 1, y_n),
                    (false, true, true) => (address_b + 1, y_n * 2),
                    (false, true, false) => (address_b, y_n),
                    (true, false, _) => unreachable!("handled frame-coded bottom macroblock"),
                }
            }
        } else {
            return None;
        };
        if neighbor < self.slice_start {
            return None;
        }
        let neighbor_raster = mbaff_raster_macroblock_address(neighbor, macroblocks_wide);
        let x_w = x_n.rem_euclid(neighbor_width) as usize;
        let y_w = y_m.rem_euclid(neighbor_height) as usize;
        Some((neighbor_raster, x_w, y_w))
    }

    fn mbaff_field_geometry(
        &self,
        address: usize,
        macroblock_size: usize,
    ) -> (usize, usize, usize) {
        let macroblocks_wide = self.coded_width / 16;
        let macroblock_x = address % macroblocks_wide;
        let raster_y = address / macroblocks_wide;
        (
            macroblock_x * macroblock_size,
            (raster_y / 2) * macroblock_size,
            raster_y % 2,
        )
    }

    fn read_macroblock(&mut self, reader: &mut SyntaxReader<'_>, address: usize) -> Result<()> {
        let mut samples = [0_u8; 384];
        for sample in &mut samples {
            *sample = reader.sample()?;
        }
        self.place_pcm_macroblock(address, &samples)
    }

    fn read_mbaff_field_macroblock(
        &mut self,
        reader: &mut SyntaxReader<'_>,
        bitstream_address: usize,
    ) -> Result<()> {
        let mut samples = [0_u8; 384];
        for sample in &mut samples {
            *sample = reader.sample()?;
        }
        let macroblocks_wide = self.coded_width / 16;
        let pair = bitstream_address / 2;
        let macroblock_x = pair % macroblocks_wide;
        let pair_y = pair / macroblocks_wide;
        let parity = bitstream_address % 2;
        for y in 0..16 {
            let destination = (pair_y * 32 + y * 2 + parity) * self.coded_width + macroblock_x * 16;
            self.luma[destination..destination + 16]
                .copy_from_slice(&samples[y * 16..(y + 1) * 16]);
        }
        let chroma_stride = self.coded_width / 2;
        for (component, plane) in [&mut self.cb, &mut self.cr].into_iter().enumerate() {
            let component_start = 256 + component * 64;
            for y in 0..8 {
                let destination = (pair_y * 16 + y * 2 + parity) * chroma_stride + macroblock_x * 8;
                let source = component_start + y * 8;
                plane[destination..destination + 8].copy_from_slice(&samples[source..source + 8]);
            }
        }
        Ok(())
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
        self.motion_l1_available[address].fill(true);
        self.macroblock_intra[address] = true;
    }

    fn mark_intra(&mut self, address: usize) {
        self.motion_available[address].fill(true);
        self.motion_l1_available[address].fill(true);
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
            self.partition_motion_vector_predictor(
                address,
                0,
                0,
                4,
                MotionPredictionKind::Normal,
                0,
            )
        };
        self.predict_inter_partition(reference, address, 0, 0, 4, 4, vector, prediction_weights)?;
        self.set_partition_motion(address, 0, 0, 4, 4, vector, 0, reference);
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
        reference_index: u8,
    ) -> MotionVector {
        if self.mbaff_frame && !self.mbaff_field_coded[address] {
            return self.partition_motion_vector_predictor_mbaff_frame_from(
                &self.motion,
                &self.motion_available,
                address,
                partition_x,
                partition_y,
                partition_width,
                kind,
                reference_index,
            );
        }
        self.partition_motion_vector_predictor_from(
            &self.motion,
            &self.motion_available,
            address,
            partition_x,
            partition_y,
            partition_width,
            kind,
            reference_index,
        )
    }

    fn partition_motion_vector_predictor_l1(
        &self,
        address: usize,
        partition_x: usize,
        partition_y: usize,
        partition_width: usize,
        kind: MotionPredictionKind,
        reference_index: u8,
    ) -> MotionVector {
        if self.mbaff_frame && !self.mbaff_field_coded[address] {
            return self.partition_motion_vector_predictor_mbaff_frame_from(
                &self.motion_l1,
                &self.motion_l1_available,
                address,
                partition_x,
                partition_y,
                partition_width,
                kind,
                reference_index,
            );
        }
        self.partition_motion_vector_predictor_from(
            &self.motion_l1,
            &self.motion_l1_available,
            address,
            partition_x,
            partition_y,
            partition_width,
            kind,
            reference_index,
        )
    }

    fn partition_motion_vector_predictor_mbaff_field(
        &self,
        address: usize,
        partition_x: usize,
        partition_y: usize,
        partition_width: usize,
        kind: MotionPredictionKind,
        reference_index: u8,
    ) -> MotionVector {
        self.partition_motion_vector_predictor_mbaff_field_from(
            &self.motion,
            &self.motion_available,
            address,
            partition_x,
            partition_y,
            partition_width,
            kind,
            reference_index,
        )
    }

    fn partition_motion_vector_predictor_mbaff_field_l1(
        &self,
        address: usize,
        partition_x: usize,
        partition_y: usize,
        partition_width: usize,
        kind: MotionPredictionKind,
        reference_index: u8,
    ) -> MotionVector {
        self.partition_motion_vector_predictor_mbaff_field_from(
            &self.motion_l1,
            &self.motion_l1_available,
            address,
            partition_x,
            partition_y,
            partition_width,
            kind,
            reference_index,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn partition_motion_vector_predictor_mbaff_frame_from(
        &self,
        motion: &[[MotionInfo; 16]],
        available: &[[bool; 16]],
        address: usize,
        partition_x: usize,
        partition_y: usize,
        partition_width: usize,
        kind: MotionPredictionKind,
        reference_index: u8,
    ) -> MotionVector {
        let sample_x = (partition_x * 4).cast_signed();
        let sample_y = (partition_y * 4).cast_signed();
        let left =
            self.mbaff_frame_motion_at_from(motion, available, address, sample_x - 1, sample_y);
        let mut top =
            self.mbaff_frame_motion_at_from(motion, available, address, sample_x, sample_y - 1);
        let mut top_right = self
            .mbaff_frame_motion_at_from(
                motion,
                available,
                address,
                sample_x + (partition_width * 4).cast_signed(),
                sample_y - 1,
            )
            .or_else(|| {
                self.mbaff_frame_motion_at_from(
                    motion,
                    available,
                    address,
                    sample_x - 1,
                    sample_y - 1,
                )
            });
        if top.is_none() && top_right.is_none() && left.is_some() {
            top = left;
            top_right = left;
        }
        motion_vector_predictor_from_candidates(left, top, top_right, kind, reference_index)
    }

    #[allow(clippy::too_many_arguments)]
    fn mbaff_frame_motion_at_from(
        &self,
        motion: &[[MotionInfo; 16]],
        available: &[[bool; 16]],
        address: usize,
        x_n: isize,
        y_n: isize,
    ) -> Option<MotionInfo> {
        let macroblocks_wide = self.coded_width / 16;
        let macroblock_x = address % macroblocks_wide;
        let macroblock_y = address / macroblocks_wide;
        let global_x = isize::try_from(macroblock_x * 16).ok()?.checked_add(x_n)?;
        let global_y = isize::try_from(macroblock_y * 16).ok()?.checked_add(y_n)?;
        let global_x = usize::try_from(global_x)
            .ok()
            .filter(|&x| x < self.coded_width)?;
        let global_y = usize::try_from(global_y)
            .ok()
            .filter(|&y| y < self.coded_height)?;
        let raster_neighbor = (global_y / 16) * macroblocks_wide + global_x / 16;
        let (neighbor, x, y) = if self.mbaff_field_coded[raster_neighbor] {
            self.mbaff_neighbor_location(address, x_n, y_n, 16, 16)?
        } else {
            (raster_neighbor, global_x % 16, global_y % 16)
        };
        let block = luma_block_index(x / 4, y / 4);
        if !available[neighbor][block] {
            return None;
        }
        let mut candidate = motion[neighbor][block];
        if self.mbaff_field_coded[neighbor] {
            candidate.y = candidate.y.saturating_mul(2);
            candidate.reference_index = candidate.reference_index.map(|index| index / 2);
        }
        Some(candidate)
    }

    #[allow(clippy::too_many_arguments)]
    fn partition_motion_vector_predictor_mbaff_field_from(
        &self,
        motion: &[[MotionInfo; 16]],
        available: &[[bool; 16]],
        address: usize,
        partition_x: usize,
        partition_y: usize,
        partition_width: usize,
        kind: MotionPredictionKind,
        reference_index: u8,
    ) -> MotionVector {
        let sample_x = (partition_x * 4).cast_signed();
        let sample_y = (partition_y * 4).cast_signed();
        let left =
            self.mbaff_field_motion_at_from(motion, available, address, sample_x - 1, sample_y);
        let mut top =
            self.mbaff_field_motion_at_from(motion, available, address, sample_x, sample_y - 1);
        let mut top_right = self
            .mbaff_field_motion_at_from(
                motion,
                available,
                address,
                sample_x + (partition_width * 4).cast_signed(),
                sample_y - 1,
            )
            .or_else(|| {
                self.mbaff_field_motion_at_from(
                    motion,
                    available,
                    address,
                    sample_x - 1,
                    sample_y - 1,
                )
            });
        if top.is_none() && top_right.is_none() && left.is_some() {
            top = left;
            top_right = left;
        }
        motion_vector_predictor_from_candidates(left, top, top_right, kind, reference_index)
    }

    #[allow(clippy::too_many_arguments)]
    fn mbaff_field_motion_at_from(
        &self,
        motion: &[[MotionInfo; 16]],
        available: &[[bool; 16]],
        address: usize,
        x_n: isize,
        y_n: isize,
    ) -> Option<MotionInfo> {
        let (neighbor, x, y) = if (0..16).contains(&x_n) && (0..16).contains(&y_n) {
            (
                address,
                usize::try_from(x_n).ok()?,
                usize::try_from(y_n).ok()?,
            )
        } else {
            self.mbaff_neighbor_location(address, x_n, y_n, 16, 16)?
        };
        let block = luma_block_index(x / 4, y / 4);
        if !available[neighbor][block] {
            return None;
        }
        let mut motion = motion[neighbor][block];
        if !self.mbaff_field_coded[neighbor] {
            motion.y /= 2;
            motion.reference_index = motion
                .reference_index
                .and_then(|index| index.checked_mul(2));
        }
        Some(motion)
    }

    fn spatial_direct_reference_index(
        &self,
        address: usize,
        partition_x: usize,
        partition_y: usize,
        partition_width: usize,
        list1: bool,
    ) -> Option<u8> {
        let macroblocks_wide = self.coded_width / 16;
        let block_x = ((address % macroblocks_wide) * 4 + partition_x).cast_signed();
        let block_y = ((address / macroblocks_wide) * 4 + partition_y).cast_signed();
        let (motion, available) = if list1 {
            (&self.motion_l1[..], &self.motion_l1_available[..])
        } else {
            (&self.motion[..], &self.motion_available[..])
        };
        let (left, top, top_right) = if self.mbaff_frame && !self.mbaff_field_coded[address] {
            let sample_x = (partition_x * 4).cast_signed();
            let sample_y = (partition_y * 4).cast_signed();
            let left =
                self.mbaff_frame_motion_at_from(motion, available, address, sample_x - 1, sample_y);
            let top =
                self.mbaff_frame_motion_at_from(motion, available, address, sample_x, sample_y - 1);
            let top_right = self
                .mbaff_frame_motion_at_from(
                    motion,
                    available,
                    address,
                    sample_x + (partition_width * 4).cast_signed(),
                    sample_y - 1,
                )
                .or_else(|| {
                    self.mbaff_frame_motion_at_from(
                        motion,
                        available,
                        address,
                        sample_x - 1,
                        sample_y - 1,
                    )
                });
            (left, top, top_right)
        } else {
            let left = self.motion_at_from(motion, available, block_x - 1, block_y);
            let top = self.motion_at_from(motion, available, block_x, block_y - 1);
            let top_right = self
                .motion_at_from(
                    motion,
                    available,
                    block_x + partition_width.cast_signed(),
                    block_y - 1,
                )
                .or_else(|| self.motion_at_from(motion, available, block_x - 1, block_y - 1));
            (left, top, top_right)
        };
        [left, top, top_right]
            .into_iter()
            .flatten()
            .filter_map(|candidate| candidate.reference_index)
            .min()
    }

    fn spatial_direct_reference_index_mbaff_field(
        &self,
        address: usize,
        list1: bool,
    ) -> Option<u8> {
        let (motion, available) = if list1 {
            (&self.motion_l1[..], &self.motion_l1_available[..])
        } else {
            (&self.motion[..], &self.motion_available[..])
        };
        let sample_x = 0;
        let sample_y = 0;
        let left =
            self.mbaff_field_motion_at_from(motion, available, address, sample_x - 1, sample_y);
        let top =
            self.mbaff_field_motion_at_from(motion, available, address, sample_x, sample_y - 1);
        let top_right = self
            .mbaff_field_motion_at_from(motion, available, address, 16, sample_y - 1)
            .or_else(|| {
                self.mbaff_field_motion_at_from(
                    motion,
                    available,
                    address,
                    sample_x - 1,
                    sample_y - 1,
                )
            });
        [left, top, top_right]
            .into_iter()
            .flatten()
            .filter_map(|candidate| candidate.reference_index)
            .min()
    }

    #[allow(clippy::too_many_arguments)]
    fn partition_motion_vector_predictor_from(
        &self,
        motion: &[[MotionInfo; 16]],
        available: &[[bool; 16]],
        address: usize,
        partition_x: usize,
        partition_y: usize,
        partition_width: usize,
        kind: MotionPredictionKind,
        reference_index: u8,
    ) -> MotionVector {
        let macroblocks_wide = self.coded_width / 16;
        let block_x = (address % macroblocks_wide) * 4 + partition_x;
        let block_y = (address / macroblocks_wide) * 4 + partition_y;
        let block_x = block_x.cast_signed();
        let block_y = block_y.cast_signed();
        let a = self.motion_at_from(motion, available, block_x - 1, block_y);
        let mut b = self.motion_at_from(motion, available, block_x, block_y - 1);
        let mut c = self
            .motion_at_from(
                motion,
                available,
                block_x + partition_width.cast_signed(),
                block_y - 1,
            )
            .or_else(|| self.motion_at_from(motion, available, block_x - 1, block_y - 1));
        if b.is_none() && c.is_none() && a.is_some() {
            b = a;
            c = a;
        }
        let preferred = match kind {
            MotionPredictionKind::Top16x8 => b,
            MotionPredictionKind::Bottom16x8 | MotionPredictionKind::Left8x16 => a,
            MotionPredictionKind::Right8x16 => c,
            MotionPredictionKind::Normal => None,
        };
        if let Some(preferred) =
            preferred.filter(|motion| motion.reference_index == Some(reference_index))
        {
            return MotionVector {
                x: preferred.x,
                y: preferred.y,
            };
        }
        let candidates = [a, b, c];
        let mut matching = candidates
            .into_iter()
            .flatten()
            .filter(|candidate| candidate.reference_index == Some(reference_index));
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
        self.motion_at_from(&self.motion, &self.motion_available, block_x, block_y)
    }

    fn motion_at_from(
        &self,
        motion: &[[MotionInfo; 16]],
        available: &[[bool; 16]],
        block_x: isize,
        block_y: isize,
    ) -> Option<MotionInfo> {
        let blocks_wide = self.coded_width / 4;
        let blocks_high = self.coded_height / 4;
        let block_x = usize::try_from(block_x).ok().filter(|&x| x < blocks_wide)?;
        let block_y = usize::try_from(block_y).ok().filter(|&y| y < blocks_high)?;
        let macroblock_x = block_x / 4;
        let macroblock_y = block_y / 4;
        let address = macroblock_y * (self.coded_width / 16) + macroblock_x;
        if address < self.slice_start {
            return None;
        }
        let block_index = luma_block_index(block_x % 4, block_y % 4);
        available[address][block_index].then_some(motion[address][block_index])
    }

    #[allow(clippy::too_many_arguments)]
    fn set_partition_motion(
        &mut self,
        address: usize,
        partition_x: usize,
        partition_y: usize,
        partition_width: usize,
        partition_height: usize,
        vector: MotionVector,
        reference_index: u8,
        reference: &ReferenceFrame,
    ) {
        let reference = Some(MotionReference::from(reference));
        for y in partition_y..partition_y + partition_height {
            for x in partition_x..partition_x + partition_width {
                let block = luma_block_index(x, y);
                self.motion[address][block] = MotionInfo {
                    x: vector.x,
                    y: vector.y,
                    reference_index: Some(reference_index),
                };
                self.motion_available[address][block] = true;
                self.reference_l0[address][block] = reference;
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn set_partition_motion_l1(
        &mut self,
        address: usize,
        partition_x: usize,
        partition_y: usize,
        partition_width: usize,
        partition_height: usize,
        vector: MotionVector,
        reference_index: u8,
        reference: &ReferenceFrame,
    ) {
        let reference = Some(MotionReference::from(reference));
        for y in partition_y..partition_y + partition_height {
            for x in partition_x..partition_x + partition_width {
                let block = luma_block_index(x, y);
                self.motion_l1[address][block] = MotionInfo {
                    x: vector.x,
                    y: vector.y,
                    reference_index: Some(reference_index),
                };
                self.motion_l1_available[address][block] = true;
                self.reference_l1[address][block] = reference;
                if !self.motion_available[address][block] {
                    self.motion[address][block] = MotionInfo {
                        x: vector.x,
                        y: vector.y,
                        reference_index: Some(reference_index.saturating_add(128)),
                    };
                }
            }
        }
    }

    fn set_partition_motion_unused(
        &mut self,
        address: usize,
        partition: InterPartition,
        list1: bool,
    ) {
        let (motion, available) = if list1 {
            (&mut self.motion_l1, &mut self.motion_l1_available)
        } else {
            (&mut self.motion, &mut self.motion_available)
        };
        for y in partition.block_y..partition.block_y + partition.block_height {
            for x in partition.block_x..partition.block_x + partition.block_width {
                let block = luma_block_index(x, y);
                motion[address][block] = MotionInfo::default();
                available[address][block] = true;
                if list1 {
                    self.reference_l1[address][block] = None;
                } else {
                    self.reference_l0[address][block] = None;
                }
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
        predict_weighted_luma_partition(
            &mut self.luma,
            self.coded_width,
            origin_x,
            origin_y,
            pixel_width,
            pixel_height,
            &reference.luma,
            reference.coded_width,
            reference.coded_height,
            vector,
            prediction_weights.luma,
        )?;
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
            predict_weighted_chroma_partition(
                destination,
                chroma_stride,
                chroma_origin_x,
                chroma_origin_y,
                chroma_width,
                chroma_partition_height,
                source,
                chroma_stride,
                chroma_height,
                vector,
                weight,
            )?;
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn predict_mbaff_field_inter_partition(
        &mut self,
        reference: &ReferenceFrame,
        address: usize,
        reference_parity: usize,
        partition_x: usize,
        partition_y: usize,
        partition_width: usize,
        partition_height: usize,
        vector: MotionVector,
        prediction_weights: PredictionWeights,
    ) -> Result<()> {
        let (macroblock_x, macroblock_y, destination_parity) =
            self.mbaff_field_geometry(address, 16);
        let origin_x = macroblock_x + partition_x * 4;
        let origin_y = macroblock_y + partition_y * 4;
        let pixel_width = partition_width * 4;
        let pixel_height = partition_height * 4;
        let reference_luma =
            extract_field_plane(&reference.luma, reference.coded_width, reference_parity);
        let field_height = reference.coded_height / 2;
        let mut prediction = vec![0_u8; pixel_width * pixel_height];
        for y in 0..pixel_height {
            for x in 0..pixel_width {
                prediction[y * pixel_width + x] = apply_prediction_weight(
                    luma_qpel(
                        &reference_luma,
                        reference.coded_width,
                        field_height,
                        quarter_coordinate(origin_x + x, vector.x)?,
                        quarter_coordinate(origin_y + y, vector.y)?,
                    ),
                    prediction_weights.luma,
                );
            }
        }
        place_field_block(
            &mut self.luma,
            self.coded_width,
            origin_x,
            origin_y,
            destination_parity,
            pixel_width,
            pixel_height,
            &prediction,
        );

        let chroma_stride = self.coded_width / 2;
        let chroma_field_height = reference.coded_height / 4;
        let (macroblock_chroma_x, macroblock_chroma_y, _) = self.mbaff_field_geometry(address, 8);
        let chroma_x = macroblock_chroma_x + partition_x * 2;
        let chroma_y = macroblock_chroma_y + partition_y * 2;
        let chroma_width = partition_width * 2;
        let chroma_height = partition_height * 2;
        let chroma_vector_y = vector
            .y
            .checked_add(field_chroma_vertical_offset(
                destination_parity,
                reference_parity,
            )?)
            .ok_or_else(|| Error::InvalidData("H.264 chroma motion vector overflows".into()))?;
        for (destination, source, weight) in [
            (&mut self.cb, &reference.cb, prediction_weights.cb),
            (&mut self.cr, &reference.cr, prediction_weights.cr),
        ] {
            let reference_field = extract_field_plane(source, chroma_stride, reference_parity);
            let mut prediction = vec![0_u8; chroma_width * chroma_height];
            for y in 0..chroma_height {
                for x in 0..chroma_width {
                    prediction[y * chroma_width + x] = apply_prediction_weight(
                        chroma_epel(
                            &reference_field,
                            chroma_stride,
                            chroma_field_height,
                            eighth_coordinate(chroma_x + x, vector.x)?,
                            eighth_coordinate(chroma_y + y, chroma_vector_y)?,
                        ),
                        weight,
                    );
                }
            }
            place_field_block(
                destination,
                chroma_stride,
                chroma_x,
                chroma_y,
                destination_parity,
                chroma_width,
                chroma_height,
                &prediction,
            );
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments, clippy::too_many_lines)]
    fn predict_mbaff_field_bi_inter_partition(
        &mut self,
        reference0: &ReferenceFrame,
        reference1: &ReferenceFrame,
        address: usize,
        reference_parity0: usize,
        reference_parity1: usize,
        partition_x: usize,
        partition_y: usize,
        partition_width: usize,
        partition_height: usize,
        vector0: MotionVector,
        vector1: MotionVector,
        weights0: PredictionWeights,
        weights1: PredictionWeights,
        weighted: bool,
    ) -> Result<()> {
        let (macroblock_x, macroblock_y, destination_parity) =
            self.mbaff_field_geometry(address, 16);
        let origin_x = macroblock_x + partition_x * 4;
        let origin_y = macroblock_y + partition_y * 4;
        let pixel_width = partition_width * 4;
        let pixel_height = partition_height * 4;
        let field0 =
            extract_field_plane(&reference0.luma, reference0.coded_width, reference_parity0);
        let field1 =
            extract_field_plane(&reference1.luma, reference1.coded_width, reference_parity1);
        let field_height0 = reference0.coded_height / 2;
        let field_height1 = reference1.coded_height / 2;
        let mut prediction = vec![0_u8; pixel_width * pixel_height];
        for y in 0..pixel_height {
            for x in 0..pixel_width {
                let sample0 = luma_qpel(
                    &field0,
                    reference0.coded_width,
                    field_height0,
                    quarter_coordinate(origin_x + x, vector0.x)?,
                    quarter_coordinate(origin_y + y, vector0.y)?,
                );
                let sample1 = luma_qpel(
                    &field1,
                    reference1.coded_width,
                    field_height1,
                    quarter_coordinate(origin_x + x, vector1.x)?,
                    quarter_coordinate(origin_y + y, vector1.y)?,
                );
                prediction[y * pixel_width + x] = apply_bi_prediction_weight(
                    sample0,
                    sample1,
                    weights0.luma,
                    weights1.luma,
                    weighted,
                );
            }
        }
        place_field_block(
            &mut self.luma,
            self.coded_width,
            origin_x,
            origin_y,
            destination_parity,
            pixel_width,
            pixel_height,
            &prediction,
        );

        let chroma_stride = self.coded_width / 2;
        let (macroblock_chroma_x, macroblock_chroma_y, _) = self.mbaff_field_geometry(address, 8);
        let chroma_x = macroblock_chroma_x + partition_x * 2;
        let chroma_y = macroblock_chroma_y + partition_y * 2;
        let chroma_width = partition_width * 2;
        let chroma_height = partition_height * 2;
        let chroma_vector_y0 = vector0
            .y
            .checked_add(field_chroma_vertical_offset(
                destination_parity,
                reference_parity0,
            )?)
            .ok_or_else(|| Error::InvalidData("H.264 chroma motion vector overflows".into()))?;
        let chroma_vector_y1 = vector1
            .y
            .checked_add(field_chroma_vertical_offset(
                destination_parity,
                reference_parity1,
            )?)
            .ok_or_else(|| Error::InvalidData("H.264 chroma motion vector overflows".into()))?;
        for (destination, source0, source1, weight0, weight1) in [
            (
                &mut self.cb,
                &reference0.cb,
                &reference1.cb,
                weights0.cb,
                weights1.cb,
            ),
            (
                &mut self.cr,
                &reference0.cr,
                &reference1.cr,
                weights0.cr,
                weights1.cr,
            ),
        ] {
            let field0 = extract_field_plane(source0, chroma_stride, reference_parity0);
            let field1 = extract_field_plane(source1, chroma_stride, reference_parity1);
            let field_height0 = reference0.coded_height / 4;
            let field_height1 = reference1.coded_height / 4;
            let mut prediction = vec![0_u8; chroma_width * chroma_height];
            for y in 0..chroma_height {
                for x in 0..chroma_width {
                    let sample0 = chroma_epel(
                        &field0,
                        chroma_stride,
                        field_height0,
                        eighth_coordinate(chroma_x + x, vector0.x)?,
                        eighth_coordinate(chroma_y + y, chroma_vector_y0)?,
                    );
                    let sample1 = chroma_epel(
                        &field1,
                        chroma_stride,
                        field_height1,
                        eighth_coordinate(chroma_x + x, vector1.x)?,
                        eighth_coordinate(chroma_y + y, chroma_vector_y1)?,
                    );
                    prediction[y * chroma_width + x] =
                        apply_bi_prediction_weight(sample0, sample1, weight0, weight1, weighted);
                }
            }
            place_field_block(
                destination,
                chroma_stride,
                chroma_x,
                chroma_y,
                destination_parity,
                chroma_width,
                chroma_height,
                &prediction,
            );
        }
        Ok(())
    }

    #[allow(
        clippy::similar_names,
        clippy::too_many_arguments,
        clippy::too_many_lines
    )]
    fn predict_bi_inter_partition(
        &mut self,
        reference0: &ReferenceFrame,
        reference1: &ReferenceFrame,
        address: usize,
        partition_x: usize,
        partition_y: usize,
        partition_width: usize,
        partition_height: usize,
        vector0: MotionVector,
        vector1: MotionVector,
        weights0: PredictionWeights,
        weights1: PredictionWeights,
        explicit_weights: bool,
    ) -> Result<()> {
        let macroblocks_wide = self.coded_width / 16;
        let origin_x = (address % macroblocks_wide) * 16 + partition_x * 4;
        let origin_y = (address / macroblocks_wide) * 16 + partition_y * 4;
        let base_x0_q4 = quarter_coordinate(origin_x, vector0.x)?;
        let base_y0_q4 = quarter_coordinate(origin_y, vector0.y)?;
        let base_x1_q4 = quarter_coordinate(origin_x, vector1.x)?;
        let base_y1_q4 = quarter_coordinate(origin_y, vector1.y)?;
        let integer0 = base_x0_q4.rem_euclid(4) == 0 && base_y0_q4.rem_euclid(4) == 0;
        let integer1 = base_x1_q4.rem_euclid(4) == 0 && base_y1_q4.rem_euclid(4) == 0;
        let luma_width = partition_width * 4;
        let luma_height = partition_height * 4;
        let direct_luma = integer_partition_origin(
            base_x0_q4,
            base_y0_q4,
            4,
            luma_width,
            luma_height,
            reference0.coded_width,
            reference0.coded_height,
        )
        .zip(integer_partition_origin(
            base_x1_q4,
            base_y1_q4,
            4,
            luma_width,
            luma_height,
            reference1.coded_width,
            reference1.coded_height,
        ));
        if let Some(((source_x0, source_y0), (source_x1, source_y1))) = direct_luma {
            for y in 0..luma_height {
                let source_row0 = (source_y0 + y) * reference0.coded_width + source_x0;
                let source_row1 = (source_y1 + y) * reference1.coded_width + source_x1;
                let destination_row = (origin_y + y) * self.coded_width + origin_x;
                for x in 0..luma_width {
                    self.luma[destination_row + x] = apply_bi_prediction_weight(
                        reference0.luma[source_row0 + x],
                        reference1.luma[source_row1 + x],
                        weights0.luma,
                        weights1.luma,
                        explicit_weights,
                    );
                }
            }
        } else {
            for y in 0..luma_height {
                let offset_y = i32::try_from(y).expect("partition height fits i32") * 4;
                for x in 0..luma_width {
                    let offset_x = i32::try_from(x).expect("partition width fits i32") * 4;
                    let x0_q4 = base_x0_q4 + offset_x;
                    let y0_q4 = base_y0_q4 + offset_y;
                    let x1_q4 = base_x1_q4 + offset_x;
                    let y1_q4 = base_y1_q4 + offset_y;
                    let sample0 = if integer0 {
                        reference_sample(
                            &reference0.luma,
                            reference0.coded_width,
                            reference0.coded_height,
                            x0_q4.div_euclid(4),
                            y0_q4.div_euclid(4),
                        )
                    } else {
                        luma_qpel(
                            &reference0.luma,
                            reference0.coded_width,
                            reference0.coded_height,
                            x0_q4,
                            y0_q4,
                        )
                    };
                    let sample1 = if integer1 {
                        reference_sample(
                            &reference1.luma,
                            reference1.coded_width,
                            reference1.coded_height,
                            x1_q4.div_euclid(4),
                            y1_q4.div_euclid(4),
                        )
                    } else {
                        luma_qpel(
                            &reference1.luma,
                            reference1.coded_width,
                            reference1.coded_height,
                            x1_q4,
                            y1_q4,
                        )
                    };
                    self.luma[(origin_y + y) * self.coded_width + origin_x + x] =
                        apply_bi_prediction_weight(
                            sample0,
                            sample1,
                            weights0.luma,
                            weights1.luma,
                            explicit_weights,
                        );
                }
            }
        }
        let chroma_stride = self.coded_width / 2;
        let chroma_height = self.coded_height / 2;
        let chroma_x = (address % macroblocks_wide) * 8 + partition_x * 2;
        let chroma_y = (address / macroblocks_wide) * 8 + partition_y * 2;
        let base_x0_q8 = eighth_coordinate(chroma_x, vector0.x)?;
        let base_y0_q8 = eighth_coordinate(chroma_y, vector0.y)?;
        let base_x1_q8 = eighth_coordinate(chroma_x, vector1.x)?;
        let base_y1_q8 = eighth_coordinate(chroma_y, vector1.y)?;
        let chroma_integer0 = base_x0_q8.rem_euclid(8) == 0 && base_y0_q8.rem_euclid(8) == 0;
        let chroma_integer1 = base_x1_q8.rem_euclid(8) == 0 && base_y1_q8.rem_euclid(8) == 0;
        for (destination, source0, source1, weight0, weight1) in [
            (
                &mut self.cb,
                &reference0.cb,
                &reference1.cb,
                weights0.cb,
                weights1.cb,
            ),
            (
                &mut self.cr,
                &reference0.cr,
                &reference1.cr,
                weights0.cr,
                weights1.cr,
            ),
        ] {
            let width = partition_width * 2;
            let height = partition_height * 2;
            let direct = integer_partition_origin(
                base_x0_q8,
                base_y0_q8,
                8,
                width,
                height,
                chroma_stride,
                chroma_height,
            )
            .zip(integer_partition_origin(
                base_x1_q8,
                base_y1_q8,
                8,
                width,
                height,
                chroma_stride,
                chroma_height,
            ));
            if let Some(((source_x0, source_y0), (source_x1, source_y1))) = direct {
                for y in 0..height {
                    let source_row0 = (source_y0 + y) * chroma_stride + source_x0;
                    let source_row1 = (source_y1 + y) * chroma_stride + source_x1;
                    let destination_row = (chroma_y + y) * chroma_stride + chroma_x;
                    for x in 0..width {
                        destination[destination_row + x] = apply_bi_prediction_weight(
                            source0[source_row0 + x],
                            source1[source_row1 + x],
                            weight0,
                            weight1,
                            explicit_weights,
                        );
                    }
                }
            } else {
                for y in 0..height {
                    let offset_y = i32::try_from(y).expect("partition height fits i32") * 8;
                    for x in 0..width {
                        let offset_x = i32::try_from(x).expect("partition width fits i32") * 8;
                        let x0_q8 = base_x0_q8 + offset_x;
                        let y0_q8 = base_y0_q8 + offset_y;
                        let x1_q8 = base_x1_q8 + offset_x;
                        let y1_q8 = base_y1_q8 + offset_y;
                        let sample0 = if chroma_integer0 {
                            reference_sample(
                                source0,
                                chroma_stride,
                                chroma_height,
                                x0_q8.div_euclid(8),
                                y0_q8.div_euclid(8),
                            )
                        } else {
                            chroma_epel(source0, chroma_stride, chroma_height, x0_q8, y0_q8)
                        };
                        let sample1 = if chroma_integer1 {
                            reference_sample(
                                source1,
                                chroma_stride,
                                chroma_height,
                                x1_q8.div_euclid(8),
                                y1_q8.div_euclid(8),
                            )
                        } else {
                            chroma_epel(source1, chroma_stride, chroma_height, x1_q8, y1_q8)
                        };
                        destination[(chroma_y + y) * chroma_stride + chroma_x + x] =
                            apply_bi_prediction_weight(
                                sample0,
                                sample1,
                                weight0,
                                weight1,
                                explicit_weights,
                            );
                    }
                }
            }
        }
        Ok(())
    }

    fn mark_zero_ac(&mut self, address: usize) {
        self.luma_nonzero[address].fill(0);
    }

    fn intra16_dc_nc(&self, address: usize) -> i8 {
        let left = self
            .left_macroblock_available(address)
            .then(|| self.luma_nonzero[address - 1][5]);
        let top = self
            .top_macroblock_address(address)
            .filter(|&top| top >= self.slice_start)
            .map(|top| self.luma_nonzero[top][10]);
        let value = match (left, top) {
            (Some(left), Some(top)) => (left + top).div_ceil(2),
            (Some(value), None) | (None, Some(value)) => value,
            (None, None) => 0,
        };
        i8::try_from(value).expect("CAVLC nonzero count is at most 16")
    }

    fn luma_nc(&self, address: usize, block_index: usize) -> i8 {
        let (block_x, block_y) = luma_block_position(block_index);
        let left = if block_x > 0 {
            Some(self.luma_nonzero[address][luma_block_index(block_x - 1, block_y)])
        } else if self.mbaff_frame {
            self.mbaff_neighbor_location(address, -1, (block_y * 4).cast_signed(), 16, 16)
                .map(|(neighbor, x, y)| self.luma_nonzero[neighbor][luma_block_index(x / 4, y / 4)])
        } else if self.left_macroblock_available(address) {
            Some(self.luma_nonzero[address - 1][luma_block_index(3, block_y)])
        } else {
            None
        };
        let top = if block_y > 0 {
            Some(self.luma_nonzero[address][luma_block_index(block_x, block_y - 1)])
        } else if self.mbaff_frame {
            self.mbaff_neighbor_location(address, (block_x * 4).cast_signed(), -1, 16, 16)
                .map(|(neighbor, x, y)| self.luma_nonzero[neighbor][luma_block_index(x / 4, y / 4)])
        } else {
            self.top_macroblock_address(address)
                .filter(|&top| top >= self.slice_start)
                .map(|top| self.luma_nonzero[top][luma_block_index(block_x, 3)])
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
        let block_x = block_index % 2;
        let block_y = block_index / 2;
        let left = if block_x > 0 {
            Some(self.chroma_nonzero[address][component][block_index - 1])
        } else if self.mbaff_frame {
            self.mbaff_neighbor_location(address, -1, (block_y * 4).cast_signed(), 8, 8)
                .map(|(neighbor, x, y)| {
                    self.chroma_nonzero[neighbor][component][(y / 4) * 2 + x / 4]
                })
        } else if self.left_macroblock_available(address) {
            Some(self.chroma_nonzero[address - 1][component][block_y * 2 + 1])
        } else {
            None
        };
        let top = if block_y > 0 {
            Some(self.chroma_nonzero[address][component][block_index - 2])
        } else if self.mbaff_frame {
            self.mbaff_neighbor_location(address, (block_x * 4).cast_signed(), -1, 8, 8)
                .map(|(neighbor, x, y)| {
                    self.chroma_nonzero[neighbor][component][(y / 4) * 2 + x / 4]
                })
        } else {
            self.top_macroblock_address(address)
                .filter(|&top| top >= self.slice_start)
                .map(|top| self.chroma_nonzero[top][component][2 + block_x])
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
        let top_available = self.top_macroblock_available(address);
        let left_available = self.left_macroblock_available(address);
        if self.mbaff_field_coded[address] {
            let (origin_x, origin_y, parity) = self.mbaff_field_geometry(address, 16);
            let field = extract_field_plane(&self.luma, self.coded_width, parity);
            let luma_prediction = predict_block(
                &field,
                self.coded_width,
                origin_x,
                origin_y,
                16,
                luma_mode,
                top_available,
                left_available,
            )?;
            place_field_block(
                &mut self.luma,
                self.coded_width,
                origin_x,
                origin_y,
                parity,
                16,
                16,
                &luma_prediction,
            );
        } else {
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
                top_available,
                left_available,
            )?;
            place_block(
                &mut self.luma,
                self.coded_width,
                macroblock_x * 16,
                macroblock_y * 16,
                16,
                &luma_prediction,
            );
        }
        self.predict_chroma_macroblock(address, chroma_mode)
    }

    fn predict_chroma_macroblock(&mut self, address: usize, chroma_mode: u32) -> Result<()> {
        self.chroma_intra_modes[address] =
            Some(u8::try_from(chroma_mode).map_err(|_| {
                Error::InvalidData("H.264 chroma prediction mode overflows".into())
            })?);
        let chroma_stride = self.coded_width / 2;
        let top_available = self.top_macroblock_available(address);
        let left_available = self.left_macroblock_available(address);
        let field_geometry =
            self.mbaff_field_coded[address].then(|| self.mbaff_field_geometry(address, 8));
        let macroblocks_wide = self.coded_width / 16;
        let macroblock_x = address % macroblocks_wide;
        let macroblock_y = address / macroblocks_wide;
        for plane in [&mut self.cb, &mut self.cr] {
            let (origin_x, origin_y, parity) = if let Some((x, y, parity)) = field_geometry {
                (x, y, Some(parity))
            } else {
                (macroblock_x * 8, macroblock_y * 8, None)
            };
            let prediction = if let Some(parity) = parity {
                let field = extract_field_plane(plane, chroma_stride, parity);
                predict_chroma_block(
                    &field,
                    chroma_stride,
                    origin_x,
                    origin_y,
                    chroma_mode,
                    top_available,
                    left_available,
                )?
            } else {
                predict_chroma_block(
                    plane,
                    chroma_stride,
                    origin_x,
                    origin_y,
                    chroma_mode,
                    top_available,
                    left_available,
                )?
            };
            if let Some(parity) = parity {
                place_field_block(
                    plane,
                    chroma_stride,
                    origin_x,
                    origin_y,
                    parity,
                    8,
                    8,
                    &prediction,
                );
            } else {
                place_block(plane, chroma_stride, origin_x, origin_y, 8, &prediction);
            }
        }
        Ok(())
    }

    fn predicted_intra4_mode(&self, address: usize, block_index: usize) -> u8 {
        let (block_x, block_y) = luma_block_position(block_index);
        let left = if block_x > 0 {
            Some(self.luma_intra_modes[address][luma_block_index(block_x - 1, block_y)])
        } else if self.mbaff_frame {
            self.mbaff_neighbor_location(address, -1, (block_y * 4).cast_signed(), 16, 16)
                .map(|(neighbor, x, y)| {
                    self.luma_intra_modes[neighbor][luma_block_index(x / 4, y / 4)]
                })
        } else if self.left_macroblock_available(address) {
            Some(self.luma_intra_modes[address - 1][luma_block_index(3, block_y)])
        } else {
            None
        };
        let top = if block_y > 0 {
            Some(self.luma_intra_modes[address][luma_block_index(block_x, block_y - 1)])
        } else if self.mbaff_frame {
            self.mbaff_neighbor_location(address, (block_x * 4).cast_signed(), -1, 16, 16)
                .map(|(neighbor, x, y)| {
                    self.luma_intra_modes[neighbor][luma_block_index(x / 4, y / 4)]
                })
        } else if self.top_macroblock_available(address) {
            self.top_macroblock_address(address)
                .map(|top| self.luma_intra_modes[top][luma_block_index(block_x, 3)])
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
        let top_macroblock_available = self.top_macroblock_available(address);
        let left_macroblock_available = self.left_macroblock_available(address);
        let field_geometry =
            self.mbaff_field_coded[address].then(|| self.mbaff_field_geometry(address, 16));
        let (origin_x, origin_y, prediction) =
            if let Some((field_x, field_y, parity)) = field_geometry {
                let field = extract_field_plane(&self.luma, self.coded_width, parity);
                let origin_x = field_x + block_x * 4;
                let origin_y = field_y + block_y * 4;
                let prediction = predict_intra4_block(
                    &field,
                    self.coded_width,
                    origin_x,
                    origin_y,
                    block_index,
                    mode,
                    top_macroblock_available,
                    left_macroblock_available,
                )?;
                (origin_x, origin_y, prediction)
            } else {
                let origin_x = macroblock_x * 16 + block_x * 4;
                let origin_y = macroblock_y * 16 + block_y * 4;
                let prediction = predict_intra4_block(
                    &self.luma,
                    self.coded_width,
                    origin_x,
                    origin_y,
                    block_index,
                    mode,
                    top_macroblock_available,
                    left_macroblock_available,
                )?;
                (origin_x, origin_y, prediction)
            };
        let levels: &[i32; 16] = levels
            .try_into()
            .map_err(|_| Error::InvalidData("invalid H.264 Intra4x4 coefficient count".into()))?;
        let scan = if field_geometry.is_some() {
            &FIELD_SCAN_4X4
        } else {
            &ZIG_ZAG_4X4
        };
        let residual = if self.transform_bypass_at_qp_zero && qp == 0 {
            transform_bypass_residual_4x4(levels, mode, scan)?
        } else {
            transform_residual_4x4(
                levels,
                qp,
                false,
                &self.scaling_matrices.four_by_four[0],
                scan,
            )?
        };
        if let Some((_, _, parity)) = field_geometry {
            place_field_block(
                &mut self.luma,
                self.coded_width,
                origin_x,
                origin_y,
                parity,
                4,
                4,
                &prediction,
            );
            add_residual_block_with_step(
                &mut self.luma,
                self.coded_width,
                origin_x,
                origin_y * 2 + parity,
                2,
                &residual,
            );
        } else {
            place_block(
                &mut self.luma,
                self.coded_width,
                origin_x,
                origin_y,
                4,
                &prediction,
            );
            add_residual_block(
                &mut self.luma,
                self.coded_width,
                origin_x,
                origin_y,
                &residual,
            );
        }
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
        let top_macroblock_available = self.top_macroblock_available(address);
        let left_macroblock_available = self.left_macroblock_available(address);
        let field_geometry =
            self.mbaff_field_coded[address].then(|| self.mbaff_field_geometry(address, 16));
        let (origin_x, origin_y, prediction) =
            if let Some((field_x, field_y, parity)) = field_geometry {
                let field = extract_field_plane(&self.luma, self.coded_width, parity);
                let origin_x = field_x + (group % 2) * 8;
                let origin_y = field_y + (group / 2) * 8;
                let prediction = predict_intra8_block(
                    &field,
                    self.coded_width,
                    origin_x,
                    origin_y,
                    group,
                    mode,
                    top_macroblock_available,
                    left_macroblock_available,
                )?;
                (origin_x, origin_y, prediction)
            } else {
                let origin_x = macroblock_x * 16 + (group % 2) * 8;
                let origin_y = macroblock_y * 16 + (group / 2) * 8;
                let prediction = predict_intra8_block(
                    &self.luma,
                    self.coded_width,
                    origin_x,
                    origin_y,
                    group,
                    mode,
                    top_macroblock_available,
                    left_macroblock_available,
                )?;
                (origin_x, origin_y, prediction)
            };
        if let Some((_, _, parity)) = field_geometry {
            place_field_block(
                &mut self.luma,
                self.coded_width,
                origin_x,
                origin_y,
                parity,
                8,
                8,
                &prediction,
            );
        } else {
            place_block(
                &mut self.luma,
                self.coded_width,
                origin_x,
                origin_y,
                8,
                &prediction,
            );
        }
        let levels: &[i32; 64] = levels
            .try_into()
            .map_err(|_| Error::InvalidData("invalid H.264 Intra8x8 coefficient count".into()))?;
        let scan = if field_geometry.is_some() {
            &FIELD_SCAN_8X8
        } else {
            &ZIG_ZAG_8X8
        };
        let residual =
            transform_residual_8x8(levels, qp, &self.scaling_matrices.eight_by_eight[0], scan)?;
        if let Some((_, _, parity)) = field_geometry {
            add_residual_block_8x8_with_step(
                &mut self.luma,
                self.coded_width,
                origin_x,
                origin_y * 2 + parity,
                2,
                &residual,
            );
        } else {
            add_residual_block_8x8(
                &mut self.luma,
                self.coded_width,
                origin_x,
                origin_y,
                &residual,
            );
        }
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
        let scan = if self.mbaff_field_coded[address] {
            &FIELD_SCAN_4X4
        } else {
            &ZIG_ZAG_4X4
        };
        let dc_values = transform_intra16_luma_dc(dc_levels, luma_qp, scaling_list, scan)?;
        let macroblocks_wide = self.coded_width / 16;
        let macroblock_x = address % macroblocks_wide;
        let macroblock_y = address / macroblocks_wide;
        let field_geometry =
            self.mbaff_field_coded[address].then(|| self.mbaff_field_geometry(address, 16));
        for block_y in 0..4 {
            for block_x in 0..4 {
                let mut coefficients = [0_i32; 16];
                coefficients[0] = dc_values[block_y * 4 + block_x];
                let block_index = luma_block_index(block_x, block_y);
                coefficients[1..].copy_from_slice(&ac_levels[block_index]);
                let residual =
                    transform_residual_4x4(&coefficients, luma_qp, true, scaling_list, scan)?;
                if let Some((field_x, field_y, parity)) = field_geometry {
                    add_residual_block_with_step(
                        &mut self.luma,
                        self.coded_width,
                        field_x + block_x * 4,
                        field_y * 2 + parity + block_y * 8,
                        2,
                        &residual,
                    );
                } else {
                    add_residual_block(
                        &mut self.luma,
                        self.coded_width,
                        macroblock_x * 16 + block_x * 4,
                        macroblock_y * 16 + block_y * 4,
                        &residual,
                    );
                }
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
        let field_geometry =
            self.mbaff_field_coded[address].then(|| self.mbaff_field_geometry(address, 16));
        let scan = if field_geometry.is_some() {
            &FIELD_SCAN_4X4
        } else {
            &ZIG_ZAG_4X4
        };
        for (block_index, block_levels) in levels.iter().enumerate() {
            let coefficients: &[i32; 16] = block_levels
                .as_slice()
                .try_into()
                .map_err(|_| Error::InvalidData("invalid H.264 luma coefficient count".into()))?;
            let residual = if self.transform_bypass_at_qp_zero && qp == 0 {
                transform_bypass_residual_4x4(coefficients, 2, scan)?
            } else {
                transform_residual_4x4(
                    coefficients,
                    qp,
                    false,
                    &self.scaling_matrices.four_by_four[3],
                    scan,
                )?
            };
            let (block_x, block_y) = luma_block_position(block_index);
            if let Some((field_x, field_y, parity)) = field_geometry {
                add_residual_block_with_step(
                    &mut self.luma,
                    self.coded_width,
                    field_x + block_x * 4,
                    field_y * 2 + parity + block_y * 8,
                    2,
                    &residual,
                );
            } else {
                add_residual_block(
                    &mut self.luma,
                    self.coded_width,
                    macroblock_x * 16 + block_x * 4,
                    macroblock_y * 16 + block_y * 4,
                    &residual,
                );
            }
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
            let residual = transform_residual_8x8(
                coefficients,
                qp,
                &self.scaling_matrices.eight_by_eight[1],
                &ZIG_ZAG_8X8,
            )?;
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
        let field_geometry =
            self.mbaff_field_coded[address].then(|| self.mbaff_field_geometry(address, 8));
        let scan = if field_geometry.is_some() {
            &FIELD_SCAN_4X4
        } else {
            &ZIG_ZAG_4X4
        };
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
                    transform_bypass_residual_4x4(&coefficients, bypass_mode, scan)?
                } else {
                    transform_residual_4x4(&coefficients, qp, true, scaling_list, scan)?
                };
            }
            if bypass {
                continue_transform_bypass_chroma(&mut residuals, bypass_mode)?;
            }
            for (block_index, residual) in residuals.iter().enumerate() {
                let block_x = block_index % 2;
                let block_y = block_index / 2;
                if let Some((field_x, field_y, parity)) = field_geometry {
                    add_residual_block_with_step(
                        plane,
                        stride,
                        field_x + block_x * 4,
                        field_y * 2 + parity + block_y * 8,
                        2,
                        residual,
                    );
                } else {
                    add_residual_block(
                        plane,
                        stride,
                        macroblock_x * 8 + block_x * 4,
                        macroblock_y * 8 + block_y * 4,
                        residual,
                    );
                }
            }
        }
        Ok(())
    }

    fn deblock(&mut self, chroma_qp_offsets: [i32; 2], params: SliceDeblocking) -> Result<()> {
        let macroblock_parameters = vec![
            DeblockingMacroblockParameters {
                parameters: params.parameters,
                slice_id: 0,
                filter_across_slice_boundaries: params.filter_across_slice_boundaries,
            };
            self.macroblock_count()
        ];
        self.deblock_slices(chroma_qp_offsets, &macroblock_parameters)
    }

    fn deblock_slices(
        &mut self,
        chroma_qp_offsets: [i32; 2],
        macroblock_parameters: &[DeblockingMacroblockParameters],
    ) -> Result<()> {
        let motion = self
            .motion
            .iter()
            .zip(&self.motion_l1)
            .zip(&self.reference_l0)
            .zip(&self.reference_l1)
            .map(|(((list0, list1), references0), references1)| {
                std::array::from_fn(|block| DeblockingMotion {
                    list0: DeblockingReferenceMotion {
                        x: list0[block].x,
                        y: list0[block].y,
                        index: list0[block].reference_index,
                        reference: references0[block].map(|reference| reference.pic_order_count),
                    },
                    list1: DeblockingReferenceMotion {
                        x: list1[block].x,
                        y: list1[block].y,
                        index: list1[block].reference_index,
                        reference: references1[block].map(|reference| reference.pic_order_count),
                    },
                })
            })
            .collect::<Vec<_>>();
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
                motion: &motion,
            },
            macroblock_parameters,
        )
    }

    fn to_frame(&self, sps: &Sps, timing: FrameTiming) -> Result<VideoFrame> {
        self.to_frame_with_field_order(
            sps,
            timing,
            if sps.frame_mbs_only {
                FieldOrder::Progressive
            } else {
                FieldOrder::Unspecified
            },
        )
    }

    fn into_frame(self, sps: &Sps, timing: FrameTiming) -> Result<VideoFrame> {
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
            crop_owned_plane(
                self.luma,
                self.coded_width,
                crop_left,
                crop_top,
                width,
                height,
            )?,
            crop_owned_plane(
                self.cb,
                self.coded_width / 2,
                crop_left / 2,
                crop_top / 2,
                chroma_width,
                chroma_height,
            )?,
            crop_owned_plane(
                self.cr,
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
            field_order: if sps.frame_mbs_only {
                FieldOrder::Progressive
            } else {
                FieldOrder::Unspecified
            },
        })
    }

    fn to_frame_with_field_order(
        &self,
        sps: &Sps,
        timing: FrameTiming,
        field_order: FieldOrder,
    ) -> Result<VideoFrame> {
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
            field_order,
        })
    }

    fn to_reference(
        &self,
        frame_num: u32,
        pic_order_count: i32,
        structure: PictureStructure,
    ) -> ReferenceFrame {
        ReferenceFrame {
            structure,
            frame_num,
            pic_order_count,
            long_term_frame_idx: None,
            coded_width: self.coded_width,
            coded_height: self.coded_height,
            luma: self.luma.clone().into(),
            cb: self.cb.clone().into(),
            cr: self.cr.clone().into(),
            motion_l0: self.motion.clone().into(),
            motion_l0_available: self.motion_available.clone().into(),
            reference_l0: self.reference_l0.clone().into(),
            motion_l1: self.motion_l1.clone().into(),
            motion_l1_available: self.motion_l1_available.clone().into(),
            reference_l1: self.reference_l1.clone().into(),
            macroblock_intra: self.macroblock_intra.clone().into(),
        }
    }

    fn into_reference(self, frame_num: u32, pic_order_count: i32) -> ReferenceFrame {
        ReferenceFrame {
            structure: PictureStructure::Frame,
            frame_num,
            pic_order_count,
            long_term_frame_idx: None,
            coded_width: self.coded_width,
            coded_height: self.coded_height,
            luma: self.luma.into(),
            cb: self.cb.into(),
            cr: self.cr.into(),
            motion_l0: self.motion.into(),
            motion_l0_available: self.motion_available.into(),
            reference_l0: self.reference_l0.into(),
            motion_l1: self.motion_l1.into(),
            motion_l1_available: self.motion_l1_available.into(),
            reference_l1: self.reference_l1.into(),
            macroblock_intra: self.macroblock_intra.into(),
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
#[allow(clippy::too_many_arguments)]
fn predict_intra8_block(
    plane: &[u8],
    stride: usize,
    origin_x: usize,
    origin_y: usize,
    group: usize,
    mode: u8,
    top_macroblock_available: bool,
    left_macroblock_available: bool,
) -> Result<Vec<u8>> {
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
    let left = left_available.then(|| {
        let mut samples = [0_u8; 8];
        for y in 0..8 {
            samples[y] = plane[(origin_y + y) * stride + origin_x - 1];
        }
        samples
    });
    let corner =
        (top_available && left_available).then(|| plane[(origin_y - 1) * stride + origin_x - 1]);
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
#[allow(clippy::too_many_arguments)]
fn predict_intra4_block(
    plane: &[u8],
    stride: usize,
    origin_x: usize,
    origin_y: usize,
    block_index: usize,
    mode: u8,
    top_macroblock_available: bool,
    left_macroblock_available: bool,
) -> Result<Vec<u8>> {
    let block_x = origin_x % 16;
    let block_y = origin_y % 16;
    let top_available = block_y > 0 || top_macroblock_available;
    let left_available = block_x > 0 || left_macroblock_available;
    let top = top_available.then(|| {
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
    let left = left_available.then(|| {
        let mut samples = [0_u8; 4];
        for y in 0..4 {
            samples[y] = plane[(origin_y + y) * stride + origin_x - 1];
        }
        samples
    });
    let top_left =
        (top_available && left_available).then(|| plane[(origin_y - 1) * stride + origin_x - 1]);
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

fn motion_vector_predictor_from_candidates(
    left: Option<MotionInfo>,
    top: Option<MotionInfo>,
    top_right: Option<MotionInfo>,
    kind: MotionPredictionKind,
    reference_index: u8,
) -> MotionVector {
    let preferred = match kind {
        MotionPredictionKind::Top16x8 => top,
        MotionPredictionKind::Bottom16x8 | MotionPredictionKind::Left8x16 => left,
        MotionPredictionKind::Right8x16 => top_right,
        MotionPredictionKind::Normal => None,
    };
    if let Some(preferred) =
        preferred.filter(|motion| motion.reference_index == Some(reference_index))
    {
        return MotionVector {
            x: preferred.x,
            y: preferred.y,
        };
    }
    let candidates = [left, top, top_right];
    let mut matching = candidates
        .into_iter()
        .flatten()
        .filter(|candidate| candidate.reference_index == Some(reference_index));
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

fn integer_partition_origin(
    base_x: i32,
    base_y: i32,
    denominator: i32,
    width: usize,
    height: usize,
    source_stride: usize,
    source_height: usize,
) -> Option<(usize, usize)> {
    if base_x.rem_euclid(denominator) != 0 || base_y.rem_euclid(denominator) != 0 {
        return None;
    }
    let source_x = usize::try_from(base_x.div_euclid(denominator)).ok()?;
    let source_y = usize::try_from(base_y.div_euclid(denominator)).ok()?;
    (source_x.checked_add(width)? <= source_stride
        && source_y.checked_add(height)? <= source_height)
        .then_some((source_x, source_y))
}

#[allow(clippy::similar_names, clippy::too_many_arguments)]
fn predict_weighted_luma_partition(
    destination: &mut [u8],
    destination_stride: usize,
    destination_x: usize,
    destination_y: usize,
    width: usize,
    height: usize,
    source: &[u8],
    source_stride: usize,
    source_height: usize,
    vector: MotionVector,
    weight: PredictionWeight,
) -> Result<()> {
    let base_x_q4 = quarter_coordinate(destination_x, vector.x)?;
    let base_y_q4 = quarter_coordinate(destination_y, vector.y)?;
    if weight == PredictionWeight::identity()
        && base_x_q4.rem_euclid(4) == 0
        && base_y_q4.rem_euclid(4) == 0
    {
        let source_x = base_x_q4.div_euclid(4);
        let source_y = base_y_q4.div_euclid(4);
        if let (Ok(source_x), Ok(source_y)) = (usize::try_from(source_x), usize::try_from(source_y))
            && source_x + width <= source_stride
            && source_y + height <= source_height
        {
            for row in 0..height {
                let source_start = (source_y + row) * source_stride + source_x;
                let destination_start = (destination_y + row) * destination_stride + destination_x;
                destination[destination_start..destination_start + width]
                    .copy_from_slice(&source[source_start..source_start + width]);
            }
            return Ok(());
        }
    }
    let integer = base_x_q4.rem_euclid(4) == 0 && base_y_q4.rem_euclid(4) == 0;
    for y in 0..height {
        let y_q4 = base_y_q4 + i32::try_from(y).expect("partition height fits i32") * 4;
        for x in 0..width {
            let x_q4 = base_x_q4 + i32::try_from(x).expect("partition width fits i32") * 4;
            let prediction = if integer {
                reference_sample(
                    source,
                    source_stride,
                    source_height,
                    x_q4.div_euclid(4),
                    y_q4.div_euclid(4),
                )
            } else {
                luma_qpel(source, source_stride, source_height, x_q4, y_q4)
            };
            destination[(destination_y + y) * destination_stride + destination_x + x] =
                apply_prediction_weight(prediction, weight);
        }
    }
    Ok(())
}

#[allow(clippy::similar_names, clippy::too_many_arguments)]
fn predict_weighted_chroma_partition(
    destination: &mut [u8],
    destination_stride: usize,
    destination_x: usize,
    destination_y: usize,
    width: usize,
    height: usize,
    source: &[u8],
    source_stride: usize,
    source_height: usize,
    vector: MotionVector,
    weight: PredictionWeight,
) -> Result<()> {
    let base_x_q8 = eighth_coordinate(destination_x, vector.x)?;
    let base_y_q8 = eighth_coordinate(destination_y, vector.y)?;
    if weight == PredictionWeight::identity()
        && base_x_q8.rem_euclid(8) == 0
        && base_y_q8.rem_euclid(8) == 0
    {
        let source_x = base_x_q8.div_euclid(8);
        let source_y = base_y_q8.div_euclid(8);
        if let (Ok(source_x), Ok(source_y)) = (usize::try_from(source_x), usize::try_from(source_y))
            && source_x + width <= source_stride
            && source_y + height <= source_height
        {
            for row in 0..height {
                let source_start = (source_y + row) * source_stride + source_x;
                let destination_start = (destination_y + row) * destination_stride + destination_x;
                destination[destination_start..destination_start + width]
                    .copy_from_slice(&source[source_start..source_start + width]);
            }
            return Ok(());
        }
    }
    let integer = base_x_q8.rem_euclid(8) == 0 && base_y_q8.rem_euclid(8) == 0;
    for y in 0..height {
        let y_q8 = base_y_q8 + i32::try_from(y).expect("partition height fits i32") * 8;
        for x in 0..width {
            let x_q8 = base_x_q8 + i32::try_from(x).expect("partition width fits i32") * 8;
            let prediction = if integer {
                reference_sample(
                    source,
                    source_stride,
                    source_height,
                    x_q8.div_euclid(8),
                    y_q8.div_euclid(8),
                )
            } else {
                chroma_epel(source, source_stride, source_height, x_q8, y_q8)
            };
            destination[(destination_y + y) * destination_stride + destination_x + x] =
                apply_prediction_weight(prediction, weight);
        }
    }
    Ok(())
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

fn apply_bi_prediction_weight(
    sample0: u8,
    sample1: u8,
    weight0: PredictionWeight,
    weight1: PredictionWeight,
    explicit: bool,
) -> u8 {
    if !explicit {
        return average_samples(sample0, sample1);
    }
    debug_assert_eq!(weight0.denominator, weight1.denominator);
    let denominator = weight0.denominator;
    let weighted = weight0.weight * i32::from(sample0) + weight1.weight * i32::from(sample1);
    let scaled = (weighted + (1_i32 << denominator)) >> (denominator + 1);
    let offset = (weight0.offset + weight1.offset + 1) >> 1;
    clip_u8(scaled + offset)
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

const FIELD_SCAN_4X4: [(usize, usize); 16] = [
    (0, 0),
    (1, 0),
    (0, 1),
    (2, 0),
    (3, 0),
    (1, 1),
    (2, 1),
    (3, 1),
    (0, 2),
    (1, 2),
    (2, 2),
    (3, 2),
    (0, 3),
    (1, 3),
    (2, 3),
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

const FIELD_SCAN_8X8: [(usize, usize); 64] = [
    (0, 0),
    (1, 0),
    (2, 0),
    (0, 1),
    (1, 1),
    (3, 0),
    (4, 0),
    (2, 1),
    (0, 2),
    (3, 1),
    (5, 0),
    (6, 0),
    (7, 0),
    (4, 1),
    (1, 2),
    (0, 3),
    (2, 2),
    (5, 1),
    (6, 1),
    (7, 1),
    (3, 2),
    (1, 3),
    (0, 4),
    (2, 3),
    (4, 2),
    (5, 2),
    (6, 2),
    (7, 2),
    (3, 3),
    (1, 4),
    (0, 5),
    (2, 4),
    (4, 3),
    (5, 3),
    (6, 3),
    (7, 3),
    (3, 4),
    (1, 5),
    (0, 6),
    (2, 5),
    (4, 4),
    (5, 4),
    (6, 4),
    (7, 4),
    (3, 5),
    (1, 6),
    (2, 6),
    (4, 5),
    (5, 5),
    (6, 5),
    (7, 5),
    (3, 6),
    (0, 7),
    (1, 7),
    (4, 6),
    (5, 6),
    (6, 6),
    (7, 6),
    (2, 7),
    (3, 7),
    (4, 7),
    (5, 7),
    (6, 7),
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
    scan: &[(usize, usize); 16],
) -> Result<[i32; 16]> {
    if levels.len() != 16 || !(0..=51).contains(&qp) {
        return Err(Error::InvalidData(
            "invalid H.264 Intra16 luma DC transform input".into(),
        ));
    }
    let mut coefficients = [0_i64; 16];
    for (index, &(row, column)) in scan.iter().enumerate() {
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
    scan: &[(usize, usize); 16],
) -> Result<[i32; 16]> {
    if !(0..=51).contains(&qp) {
        return Err(Error::InvalidData("invalid H.264 luma QP".into()));
    }
    let mut scaled = [0_i64; 16];
    for (index, &(row, column)) in scan.iter().enumerate() {
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
    scan: &[(usize, usize); 64],
) -> Result<[i32; 64]> {
    if !(0..=51).contains(&qp) {
        return Err(Error::InvalidData("invalid H.264 luma QP".into()));
    }
    let mut scaled = [0_i64; 64];
    for (index, &(row, column)) in scan.iter().enumerate() {
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

fn transform_bypass_residual_4x4(
    levels: &[i32; 16],
    intra_mode: u8,
    scan: &[(usize, usize); 16],
) -> Result<[i32; 16]> {
    let mut residual = [0_i32; 16];
    for (index, &(row, column)) in scan.iter().enumerate() {
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
    add_residual_block_with_step(plane, stride, origin_x, origin_y, 1, residual);
}

fn add_residual_block_with_step(
    plane: &mut [u8],
    stride: usize,
    origin_x: usize,
    origin_y: usize,
    row_step: usize,
    residual: &[i32; 16],
) {
    for y in 0..4 {
        for x in 0..4 {
            let index = (origin_y + y * row_step) * stride + origin_x + x;
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
    add_residual_block_8x8_with_step(plane, stride, origin_x, origin_y, 1, residual);
}

fn add_residual_block_8x8_with_step(
    plane: &mut [u8],
    stride: usize,
    origin_x: usize,
    origin_y: usize,
    row_step: usize,
    residual: &[i32; 64],
) {
    for y in 0..8 {
        for x in 0..8 {
            let index = (origin_y + y * row_step) * stride + origin_x + x;
            let value = i32::from(plane[index]) + residual[y * 8 + x];
            plane[index] = u8::try_from(value.clamp(0, 255)).expect("clamped luma sample fits u8");
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn predict_chroma_block(
    plane: &[u8],
    stride: usize,
    origin_x: usize,
    origin_y: usize,
    mode: u32,
    top_available: bool,
    left_available: bool,
) -> Result<Vec<u8>> {
    if mode == 0 {
        return Ok(predict_chroma_dc(
            plane,
            stride,
            origin_x,
            origin_y,
            top_available,
            left_available,
        ));
    }
    let block_mode = if mode == 2 { 0 } else { mode };
    predict_block(
        plane,
        stride,
        origin_x,
        origin_y,
        8,
        block_mode,
        top_available,
        left_available,
    )
}

fn predict_chroma_dc(
    plane: &[u8],
    stride: usize,
    origin_x: usize,
    origin_y: usize,
    top_available: bool,
    left_available: bool,
) -> Vec<u8> {
    let top = top_available.then(|| {
        (0..8)
            .map(|x| plane[(origin_y - 1) * stride + origin_x + x])
            .collect::<Vec<_>>()
    });
    let left = left_available.then(|| {
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

#[allow(clippy::too_many_arguments)]
fn predict_block(
    plane: &[u8],
    stride: usize,
    origin_x: usize,
    origin_y: usize,
    size: usize,
    mode: u32,
    top_available: bool,
    left_available: bool,
) -> Result<Vec<u8>> {
    let top = top_available.then(|| {
        (0..size)
            .map(|x| plane[(origin_y - 1) * stride + origin_x + x])
            .collect::<Vec<_>>()
    });
    let left = left_available.then(|| {
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

fn extract_field_plane(plane: &[u8], stride: usize, parity: usize) -> Vec<u8> {
    plane
        .chunks_exact(stride)
        .skip(parity)
        .step_by(2)
        .flatten()
        .copied()
        .collect()
}

fn field_chroma_vertical_offset(destination_parity: usize, reference_parity: usize) -> Result<i32> {
    let destination = i32::try_from(destination_parity)
        .map_err(|_| Error::InvalidData("H.264 destination field parity overflows".into()))?;
    let reference = i32::try_from(reference_parity)
        .map_err(|_| Error::InvalidData("H.264 reference field parity overflows".into()))?;
    destination
        .checked_sub(reference)
        .and_then(|difference| difference.checked_mul(2))
        .ok_or_else(|| Error::InvalidData("H.264 field chroma offset overflows".into()))
}

#[allow(clippy::too_many_arguments)]
fn place_field_block(
    plane: &mut [u8],
    stride: usize,
    origin_x: usize,
    origin_y: usize,
    parity: usize,
    width: usize,
    height: usize,
    values: &[u8],
) {
    for y in 0..height {
        let destination = ((origin_y + y) * 2 + parity) * stride + origin_x;
        plane[destination..destination + width]
            .copy_from_slice(&values[y * width..(y + 1) * width]);
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

fn crop_owned_plane(
    mut source: Vec<u8>,
    source_stride: usize,
    left: usize,
    top: usize,
    width: usize,
    height: usize,
) -> Result<Plane> {
    if left == 0 && top == 0 && width == source_stride {
        let length = width
            .checked_mul(height)
            .filter(|length| *length <= source.len())
            .ok_or_else(|| Error::InvalidData("H.264 crop exceeds coded plane".into()))?;
        source.truncate(length);
        return Ok(Plane {
            data: source,
            stride: width,
            width,
            height,
        });
    }
    crop_plane(&source, source_stride, left, top, width, height)
}

fn weave_field_plane(
    destination: &mut [u8],
    stride: usize,
    top: &[u8],
    bottom: &[u8],
) -> Result<()> {
    if stride == 0
        || top.len() != bottom.len()
        || destination.len() != top.len().saturating_add(bottom.len())
        || !top.len().is_multiple_of(stride)
    {
        return Err(Error::InvalidData(
            "invalid H.264 complementary-field plane layout".into(),
        ));
    }
    for row in 0..top.len() / stride {
        let source = row * stride;
        let top_destination = row * 2 * stride;
        let bottom_destination = top_destination + stride;
        destination[top_destination..top_destination + stride]
            .copy_from_slice(&top[source..source + stride]);
        destination[bottom_destination..bottom_destination + stride]
            .copy_from_slice(&bottom[source..source + stride]);
    }
    Ok(())
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
        collections::VecDeque,
        io::Write as _,
        process::{Command, Stdio},
        sync::Arc,
    };

    use mmrecode_bitstream::BitWriter;
    use mmrecode_core::{
        CodecDescriptor, CodecId, Decoder, Error, FieldOrder, FourCc, MediaType, Packet,
        PacketFlags, Rational, StreamId, Timestamp,
    };

    use super::{
        BPrediction, FrameBuffer, H264Decoder, MemoryManagementControlOperation,
        PictureOrderCountType, PictureOrderState, PictureStructure, PredictionWeight,
        ReferenceFrame, ReferenceMarking, SyntaxReader, apply_field_reference_marking,
        apply_prediction_weight, apply_reference_marking, average_samples, b_inter_partitions,
        b_sub_macroblock, predict_intra8_block, read_picture_order_count, read_reference_list0,
        read_reference_marking,
    };

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
    fn maps_mbaff_neighbors_between_frame_and_field_coded_pairs() {
        let sps =
            crate::parse_sps(&interlaced_sps_with_dimensions_and_mbaff(1, 1, 0, true)).unwrap();
        let pps = crate::parse_pps(&pps()).unwrap();
        let mut buffer =
            FrameBuffer::new_for_structure(&sps, &pps, PictureStructure::Frame).unwrap();

        // The left pair is field-coded and the right pair is frame-coded. A
        // frame macroblock therefore alternates between the two left fields.
        buffer.mbaff_field_coded[0] = true;
        buffer.mbaff_field_coded[2] = true;
        assert_eq!(
            buffer.mbaff_neighbor_location(3, -1, 0, 16, 16),
            Some((0, 15, 8))
        );
        assert_eq!(
            buffer.mbaff_neighbor_location(3, -1, 1, 16, 16),
            Some((2, 15, 8))
        );
        assert_eq!(
            buffer.mbaff_neighbor_location(3, 0, -1, 16, 16),
            Some((1, 0, 15))
        );
        assert_eq!(
            buffer.mbaff_neighbor_location(3, -1, 0, 8, 8),
            Some((0, 7, 4))
        );
        assert_eq!(
            buffer.mbaff_neighbor_location(3, -1, 1, 8, 8),
            Some((2, 7, 4))
        );

        // Reversing the coding modes makes each field select the matching half
        // of the neighboring frame pair.
        buffer.mbaff_field_coded.fill(false);
        buffer.mbaff_field_coded[1] = true;
        buffer.mbaff_field_coded[3] = true;
        assert_eq!(
            buffer.mbaff_neighbor_location(1, -1, 0, 16, 16),
            Some((0, 15, 0))
        );
        assert_eq!(
            buffer.mbaff_neighbor_location(1, -1, 8, 16, 16),
            Some((2, 15, 0))
        );
        assert_eq!(
            buffer.mbaff_neighbor_location(3, -1, 0, 16, 16),
            Some((0, 15, 1))
        );
        assert_eq!(
            buffer.mbaff_neighbor_location(3, -1, 8, 16, 16),
            Some((2, 15, 1))
        );
    }

    #[test]
    fn intra8_top_right_samples_are_available_for_the_top_right_partition() {
        let stride = 32;
        let mut plane = vec![0_u8; stride * 32];
        plane[7 * stride + 14] = 40;
        plane[7 * stride + 15] = 80;
        plane[7 * stride + 16] = 120;
        let top_right = predict_intra8_block(&plane, stride, 8, 8, 1, 0, true, true).unwrap();
        assert_eq!(top_right[7], 80);

        plane[15 * stride + 14] = 40;
        plane[15 * stride + 15] = 80;
        plane[15 * stride + 16] = 120;
        let bottom_right = predict_intra8_block(&plane, stride, 8, 16, 3, 0, true, true).unwrap();
        assert_eq!(bottom_right[7], 70);
    }

    #[test]
    fn derives_type1_picture_order_through_frame_num_wrap() {
        let mut sps = crate::parse_sps(&sps()).unwrap();
        sps.pic_order_cnt_type = PictureOrderCountType::Type1 {
            delta_pic_order_always_zero: false,
            offset_for_non_ref_pic: -1,
            offset_for_top_to_bottom_field: 1,
            offset_for_ref_frame: vec![2, 3],
        };
        let mut pps = crate::parse_pps(&pps()).unwrap();
        pps.bottom_field_pic_order_in_frame_present = true;

        let deltas = |delta0, delta1| {
            let mut writer = BitWriter::new();
            write_se(&mut writer, delta0);
            write_se(&mut writer, delta1);
            writer.into_bytes()
        };
        let idr = read_picture_order_count(
            &mut SyntaxReader::new(&deltas(0, 0)),
            &sps,
            &pps,
            0,
            PictureStructure::Frame,
            true,
            true,
            None,
        )
        .unwrap();
        assert_eq!(idr.value, 0);
        assert_eq!(
            idr.reference_state,
            Some(PictureOrderState::FrameNum {
                frame_num: 0,
                offset: 0
            })
        );

        let first = read_picture_order_count(
            &mut SyntaxReader::new(&deltas(1, -1)),
            &sps,
            &pps,
            1,
            PictureStructure::Frame,
            false,
            true,
            idr.reference_state,
        )
        .unwrap();
        assert_eq!(first.value, 3);

        let wrapped = read_picture_order_count(
            &mut SyntaxReader::new(&deltas(0, 0)),
            &sps,
            &pps,
            0,
            PictureStructure::Frame,
            false,
            true,
            Some(PictureOrderState::FrameNum {
                frame_num: 15,
                offset: 0,
            }),
        )
        .unwrap();
        assert_eq!(wrapped.value, 40);
        assert_eq!(
            wrapped.reference_state,
            Some(PictureOrderState::FrameNum {
                frame_num: 0,
                offset: 16
            })
        );
    }

    #[test]
    fn derives_picture_order_for_top_and_bottom_fields() {
        let sps = crate::parse_sps(&sps()).unwrap();
        let pps = crate::parse_pps(&pps()).unwrap();
        let field_lsb = || {
            let mut writer = BitWriter::new();
            writer.write_bits(3, 4).unwrap();
            writer.into_bytes()
        };
        let top = read_picture_order_count(
            &mut SyntaxReader::new(&field_lsb()),
            &sps,
            &pps,
            0,
            PictureStructure::TopField,
            true,
            true,
            None,
        )
        .unwrap();
        let bottom = read_picture_order_count(
            &mut SyntaxReader::new(&field_lsb()),
            &sps,
            &pps,
            0,
            PictureStructure::BottomField,
            true,
            true,
            None,
        )
        .unwrap();
        assert_eq!(top.value, 3);
        assert_eq!(bottom.value, 3);

        let mut sps = sps;
        sps.pic_order_cnt_type = PictureOrderCountType::Type1 {
            delta_pic_order_always_zero: false,
            offset_for_non_ref_pic: -1,
            offset_for_top_to_bottom_field: 1,
            offset_for_ref_frame: vec![2, 3],
        };
        let delta = |value| {
            let mut writer = BitWriter::new();
            write_se(&mut writer, value);
            writer.into_bytes()
        };
        let top = read_picture_order_count(
            &mut SyntaxReader::new(&delta(2)),
            &sps,
            &pps,
            1,
            PictureStructure::TopField,
            false,
            true,
            None,
        )
        .unwrap();
        let bottom = read_picture_order_count(
            &mut SyntaxReader::new(&delta(2)),
            &sps,
            &pps,
            1,
            PictureStructure::BottomField,
            false,
            true,
            None,
        )
        .unwrap();
        assert_eq!(top.value, 4);
        assert_eq!(bottom.value, 5);
    }

    #[test]
    fn derives_type2_reference_and_non_reference_picture_order() {
        let mut sps = crate::parse_sps(&sps()).unwrap();
        sps.pic_order_cnt_type = PictureOrderCountType::Type2;
        let pps = crate::parse_pps(&pps()).unwrap();
        let previous = Some(PictureOrderState::FrameNum {
            frame_num: 15,
            offset: 0,
        });
        let reference = read_picture_order_count(
            &mut SyntaxReader::new(&[]),
            &sps,
            &pps,
            0,
            PictureStructure::Frame,
            false,
            true,
            previous,
        )
        .unwrap();
        assert_eq!(reference.value, 32);
        assert_eq!(
            reference.reference_state,
            Some(PictureOrderState::FrameNum {
                frame_num: 0,
                offset: 16
            })
        );
        let non_reference = read_picture_order_count(
            &mut SyntaxReader::new(&[]),
            &sps,
            &pps,
            0,
            PictureStructure::Frame,
            false,
            false,
            previous,
        )
        .unwrap();
        assert_eq!(non_reference.value, 31);
        assert_eq!(non_reference.reference_state, None);
    }

    #[test]
    fn reconstructs_a_type2_picture_order_sequence() {
        let sps = sps_type2(1);
        let pps = pps();
        let slices = [ipcm_slice_type2(), p_ipcm_slice_type2(1, [100, 110, 120])];
        let descriptor = CodecDescriptor {
            codec_id: CodecId::new(crate::CODEC_NAME),
            codec_tag: Some(FourCc(*b"avc1")),
            media_type: MediaType::Video,
            configuration: avcc(&sps, &pps),
        };
        let mut decoder = H264Decoder::default();
        decoder.configure(&descriptor).unwrap();
        let mut native = Vec::new();
        for (index, slice) in slices.iter().enumerate() {
            decoder
                .send_packet(Packet {
                    stream_id: StreamId(0),
                    data: length_prefixed(slice),
                    pts: None,
                    dts: None,
                    duration: None,
                    flags: if index == 0 {
                        PacketFlags::KEY
                    } else {
                        PacketFlags::empty()
                    },
                    side_data: Vec::new(),
                })
                .unwrap();
            native.extend(
                decoder
                    .receive_frame()
                    .unwrap()
                    .unwrap()
                    .planes
                    .into_iter()
                    .flat_map(|plane| plane.data),
            );
        }
        assert!(native[..256].iter().all(|&sample| sample == 42));
        assert!(native[384..640].iter().all(|&sample| sample == 100));
        if let Some(independent) =
            decode_sequence_with_ffmpeg(&sps, &pps, &[&slices[0], &slices[1]], 2)
        {
            assert_eq!(native, independent);
        }
    }

    #[test]
    fn reconstructs_type1_b_picture_reference_order() {
        let sps = sps_type1(2);
        let pps = pps();
        let idr = ipcm_slice_type2();
        let future = p_ipcm_slice_type2(1, [100, 110, 120]);
        let b_picture = b_16x16_slice_without_poc(3);
        let descriptor = CodecDescriptor {
            codec_id: CodecId::new(crate::CODEC_NAME),
            codec_tag: Some(FourCc(*b"avc1")),
            media_type: MediaType::Video,
            configuration: avcc(&sps, &pps),
        };
        let mut decoder = H264Decoder::default();
        decoder.configure(&descriptor).unwrap();
        let mut b_frame = None;
        for (slice, flags) in [
            (&idr, PacketFlags::KEY),
            (&future, PacketFlags::empty()),
            (&b_picture, PacketFlags::empty()),
        ] {
            decoder
                .send_packet(Packet {
                    stream_id: StreamId(0),
                    data: length_prefixed(slice),
                    pts: None,
                    dts: None,
                    duration: None,
                    flags,
                    side_data: Vec::new(),
                })
                .unwrap();
            b_frame = decoder.receive_frame().unwrap();
        }
        let native = b_frame
            .unwrap()
            .planes
            .into_iter()
            .flat_map(|plane| plane.data)
            .collect::<Vec<_>>();
        assert!(native[..256].iter().all(|&sample| sample == 71));
        assert!(native[256..320].iter().all(|&sample| sample == 100));
        assert!(native[320..384].iter().all(|&sample| sample == 140));
        if let Some(decoded) =
            decode_sequence_with_ffmpeg(&sps, &pps, &[&idr, &future, &b_picture], 3)
        {
            assert_eq!(native, decoded[384..768]);
        }
    }

    #[test]
    fn reconstructs_a_non_idr_i_picture_without_prior_reference_state() {
        let sps = sps_with_max_references(1);
        let pps = pps();
        let intra = i_ipcm_slice(1, 2, [100, 110, 120]);
        let descriptor = CodecDescriptor {
            codec_id: CodecId::new(crate::CODEC_NAME),
            codec_tag: Some(FourCc(*b"avc1")),
            media_type: MediaType::Video,
            configuration: avcc(&sps, &pps),
        };
        let mut decoder = H264Decoder::default();
        decoder.configure(&descriptor).unwrap();
        decoder
            .send_packet(Packet {
                stream_id: StreamId(0),
                data: length_prefixed(&intra),
                pts: None,
                dts: None,
                duration: None,
                flags: PacketFlags::empty(),
                side_data: Vec::new(),
            })
            .unwrap();
        let native = decoder
            .receive_frame()
            .unwrap()
            .unwrap()
            .planes
            .into_iter()
            .flat_map(|plane| plane.data)
            .collect::<Vec<_>>();
        assert!(native[..256].iter().all(|&sample| sample == 100));
        assert!(native[256..320].iter().all(|&sample| sample == 110));
        assert!(native[320..384].iter().all(|&sample| sample == 120));

        let idr = ipcm_slice();
        if let Some(decoded) = decode_sequence_with_ffmpeg(&sps, &pps, &[&idr, &intra], 2) {
            assert_eq!(native, decoded[384..768]);
        }
    }

    #[test]
    fn reconstructs_a_multislice_non_idr_i_picture() {
        let sps = sps_with_dimensions(1, 1, 0);
        let pps = pps();
        let left = i_ipcm_slice_at(0, 1, 2, [70, 90, 130]);
        let right = i_ipcm_slice_at(1, 1, 2, [170, 110, 150]);
        let descriptor = CodecDescriptor {
            codec_id: CodecId::new(crate::CODEC_NAME),
            codec_tag: Some(FourCc(*b"avc1")),
            media_type: MediaType::Video,
            configuration: avcc(&sps, &pps),
        };
        let mut decoder = H264Decoder::default();
        decoder.configure(&descriptor).unwrap();
        let mut data = length_prefixed(&left);
        data.extend(length_prefixed(&right));
        decoder
            .send_packet(Packet {
                stream_id: StreamId(0),
                data,
                pts: None,
                dts: None,
                duration: None,
                flags: PacketFlags::empty(),
                side_data: Vec::new(),
            })
            .unwrap();
        let frame = decoder.receive_frame().unwrap().unwrap();
        assert_eq!((frame.width, frame.height), (32, 16));
        for row in frame.planes[0].data.as_chunks::<32>().0 {
            assert!(row[..16].iter().all(|&sample| sample == 70));
            assert!(row[16..].iter().all(|&sample| sample == 170));
        }
        let native = frame
            .planes
            .into_iter()
            .flat_map(|plane| plane.data)
            .collect::<Vec<_>>();
        if let Some(decoded) = decode_sequence_with_ffmpeg(&sps, &pps, &[&left, &right], 1) {
            assert_eq!(native, decoded);
        }
    }

    #[test]
    fn reorders_a_short_term_reference_for_p_skip_reconstruction() {
        let sps = sps_with_max_references(2);
        let pps = pps();
        let descriptor = CodecDescriptor {
            codec_id: CodecId::new(crate::CODEC_NAME),
            codec_tag: Some(FourCc(*b"avc1")),
            media_type: MediaType::Video,
            configuration: avcc(&sps, &pps),
        };
        let slices = [
            ipcm_slice(),
            p_ipcm_slice(1, 2, [180, 40, 220]),
            p_reordered_skip_slice(2, 4),
        ];
        let mut decoder = H264Decoder::default();
        decoder.configure(&descriptor).unwrap();
        let mut native = Vec::new();
        for (index, slice) in slices.iter().enumerate() {
            decoder
                .send_packet(Packet {
                    stream_id: StreamId(0),
                    data: length_prefixed(slice),
                    pts: None,
                    dts: None,
                    duration: None,
                    flags: if index == 0 {
                        PacketFlags::KEY
                    } else {
                        PacketFlags::empty()
                    },
                    side_data: Vec::new(),
                })
                .unwrap();
            let frame = decoder.receive_frame().unwrap().unwrap();
            native.extend(
                frame
                    .planes
                    .iter()
                    .flat_map(|plane| plane.data.iter().copied()),
            );
        }
        assert!(native[..256].iter().all(|&sample| sample == 42));
        assert!(native[384..640].iter().all(|&sample| sample == 180));
        assert_eq!(&native[768..1152], &native[..384]);
        if let Some(independent) = decode_sequence_with_ffmpeg(
            &sps,
            &pps,
            &slices.iter().map(Vec::as_slice).collect::<Vec<_>>(),
            slices.len(),
        ) && native != independent
        {
            let mismatch = native
                .iter()
                .zip(&independent)
                .position(|(native, independent)| native != independent)
                .unwrap();
            panic!(
                "reference-list reconstruction differs from FFmpeg at byte {mismatch}: native={}, independent={}",
                native[mismatch], independent[mismatch]
            );
        }
    }

    #[test]
    fn reorders_short_term_references_across_frame_num_wrap() {
        let references =
            VecDeque::from([empty_reference(0), empty_reference(15), empty_reference(14)]);
        let mut writer = BitWriter::new();
        writer.write_bit(true).unwrap();
        write_ue(&mut writer, 0);
        write_ue(&mut writer, 1);
        write_ue(&mut writer, 1);
        write_ue(&mut writer, 0);
        write_ue(&mut writer, 3);
        let bytes = writer.into_bytes();
        let reordered =
            read_reference_list0(&mut SyntaxReader::new(&bytes), &references, 1, 4, 2).unwrap();
        assert_eq!(
            reordered
                .iter()
                .map(|reference| reference.frame_num)
                .collect::<Vec<_>>(),
            vec![15, 0]
        );
    }

    #[test]
    fn reconstructs_from_an_explicit_long_term_reference() {
        let sps = sps_with_max_references(2);
        let pps = pps();
        let descriptor = CodecDescriptor {
            codec_id: CodecId::new(crate::CODEC_NAME),
            codec_tag: Some(FourCc(*b"avc1")),
            media_type: MediaType::Video,
            configuration: avcc(&sps, &pps),
        };
        let slices = [
            ipcm_slice_with_long_term(true),
            p_long_term_ipcm_slice(1, 2, [180, 40, 220]),
            p_long_term_reordered_skip_slice(2, 4),
        ];
        let mut decoder = H264Decoder::default();
        decoder.configure(&descriptor).unwrap();
        let mut native = Vec::new();
        for (index, slice) in slices.iter().enumerate() {
            decoder
                .send_packet(Packet {
                    stream_id: StreamId(0),
                    data: length_prefixed(slice),
                    pts: None,
                    dts: None,
                    duration: None,
                    flags: if index == 0 {
                        PacketFlags::KEY
                    } else {
                        PacketFlags::empty()
                    },
                    side_data: Vec::new(),
                })
                .unwrap();
            let frame = decoder.receive_frame().unwrap().unwrap();
            native.extend(
                frame
                    .planes
                    .iter()
                    .flat_map(|plane| plane.data.iter().copied()),
            );
        }
        assert!(native[384..640].iter().all(|&sample| sample == 180));
        assert_eq!(&native[768..1152], &native[384..768]);
        if let Some(independent) = decode_sequence_with_ffmpeg(
            &sps,
            &pps,
            &slices.iter().map(Vec::as_slice).collect::<Vec<_>>(),
            slices.len(),
        ) {
            assert_eq!(native, independent);
        }
    }

    #[test]
    fn applies_all_adaptive_reference_marking_transitions() {
        let mut short3 = empty_reference(3);
        let short2 = empty_reference(2);
        let mut long0 = empty_reference(0);
        long0.long_term_frame_idx = Some(0);
        let mut long1 = empty_reference(1);
        long1.long_term_frame_idx = Some(1);
        short3.luma = vec![3].into();
        let mut references = VecDeque::from([short3, short2, long0, long1]);
        let mut max_long_term_frame_idx = Some(1);
        apply_reference_marking(
            &mut references,
            empty_reference(4),
            2,
            4,
            ReferenceMarking::Adaptive(vec![
                MemoryManagementControlOperation::ForgetShortTerm {
                    difference_of_pic_nums_minus1: 0,
                },
                MemoryManagementControlOperation::ShortTermToLongTerm {
                    difference_of_pic_nums_minus1: 1,
                    long_term_frame_idx: 1,
                },
                MemoryManagementControlOperation::ForgetLongTerm {
                    long_term_pic_num: 0,
                },
                MemoryManagementControlOperation::SetMaxLongTermFrameIdx {
                    max_long_term_frame_idx: Some(1),
                },
                MemoryManagementControlOperation::MarkCurrentLongTerm {
                    long_term_frame_idx: 0,
                },
            ]),
            &mut max_long_term_frame_idx,
        )
        .unwrap();
        assert_eq!(references.len(), 2);
        assert_eq!(references[0].long_term_frame_idx, Some(0));
        assert_eq!(references[1].frame_num, 2);
        assert_eq!(references[1].long_term_frame_idx, Some(1));

        let mut reset_current = empty_reference(5);
        reset_current.pic_order_count = 42;
        let reset_marking =
            ReferenceMarking::Adaptive(vec![MemoryManagementControlOperation::Reset]);
        assert!(reset_marking.resets_picture_order());
        apply_reference_marking(
            &mut references,
            reset_current,
            2,
            4,
            reset_marking,
            &mut max_long_term_frame_idx,
        )
        .unwrap();
        assert_eq!(references.len(), 1);
        assert_eq!(references[0].frame_num, 0);
        assert_eq!(references[0].pic_order_count, 0);
        assert_eq!(references[0].long_term_frame_idx, None);
        assert_eq!(max_long_term_frame_idx, None);
    }

    #[test]
    fn applies_all_adaptive_field_reference_marking_transitions() {
        let field = |frame_num, structure, long_term_frame_idx| {
            let mut reference = empty_reference(frame_num);
            reference.structure = structure;
            reference.long_term_frame_idx = long_term_frame_idx;
            reference
        };
        let mut references = VecDeque::from([
            field(1, PictureStructure::BottomField, None),
            field(1, PictureStructure::TopField, None),
            field(0, PictureStructure::BottomField, None),
            field(0, PictureStructure::TopField, None),
            field(14, PictureStructure::TopField, Some(0)),
        ]);
        let mut max_long_term_frame_idx = Some(1);
        apply_field_reference_marking(
            &mut references,
            field(2, PictureStructure::TopField, None),
            3,
            4,
            ReferenceMarking::Adaptive(vec![
                MemoryManagementControlOperation::ForgetShortTerm {
                    difference_of_pic_nums_minus1: 4,
                },
                MemoryManagementControlOperation::ForgetLongTerm {
                    long_term_pic_num: 1,
                },
                MemoryManagementControlOperation::ShortTermToLongTerm {
                    difference_of_pic_nums_minus1: 2,
                    long_term_frame_idx: 1,
                },
                MemoryManagementControlOperation::SetMaxLongTermFrameIdx {
                    max_long_term_frame_idx: Some(1),
                },
                MemoryManagementControlOperation::MarkCurrentLongTerm {
                    long_term_frame_idx: 0,
                },
            ]),
            &mut max_long_term_frame_idx,
        )
        .unwrap();
        assert_eq!(references.len(), 4);
        assert!(references.iter().any(|reference| {
            reference.frame_num == 2
                && reference.structure == PictureStructure::TopField
                && reference.long_term_frame_idx == Some(0)
        }));
        assert!(references.iter().any(|reference| {
            reference.frame_num == 1
                && reference.structure == PictureStructure::BottomField
                && reference.long_term_frame_idx == Some(1)
        }));
        assert!(!references.iter().any(|reference| {
            reference.frame_num == 0 && reference.structure == PictureStructure::BottomField
        }));

        apply_field_reference_marking(
            &mut references,
            field(2, PictureStructure::BottomField, None),
            3,
            4,
            ReferenceMarking::Adaptive(vec![
                MemoryManagementControlOperation::MarkCurrentLongTerm {
                    long_term_frame_idx: 0,
                },
            ]),
            &mut max_long_term_frame_idx,
        )
        .unwrap();
        assert_eq!(
            references
                .iter()
                .filter(|reference| reference.long_term_frame_idx == Some(0))
                .count(),
            2
        );

        let mut reset = field(3, PictureStructure::TopField, None);
        reset.pic_order_count = 42;
        apply_field_reference_marking(
            &mut references,
            reset,
            3,
            4,
            ReferenceMarking::Adaptive(vec![MemoryManagementControlOperation::Reset]),
            &mut max_long_term_frame_idx,
        )
        .unwrap();
        assert_eq!(references.len(), 1);
        assert_eq!(references[0].frame_num, 0);
        assert_eq!(references[0].pic_order_count, 0);
        assert_eq!(max_long_term_frame_idx, None);
    }

    #[test]
    fn parses_adaptive_reference_marking_operations() {
        let mut writer = BitWriter::new();
        writer.write_bit(true).unwrap();
        write_ue(&mut writer, 1);
        write_ue(&mut writer, 0);
        write_ue(&mut writer, 3);
        write_ue(&mut writer, 1);
        write_ue(&mut writer, 1);
        write_ue(&mut writer, 4);
        write_ue(&mut writer, 2);
        write_ue(&mut writer, 6);
        write_ue(&mut writer, 0);
        write_ue(&mut writer, 0);
        let bytes = writer.into_bytes();
        let marking = read_reference_marking(&mut SyntaxReader::new(&bytes)).unwrap();
        assert_eq!(
            marking,
            ReferenceMarking::Adaptive(vec![
                MemoryManagementControlOperation::ForgetShortTerm {
                    difference_of_pic_nums_minus1: 0,
                },
                MemoryManagementControlOperation::ShortTermToLongTerm {
                    difference_of_pic_nums_minus1: 1,
                    long_term_frame_idx: 1,
                },
                MemoryManagementControlOperation::SetMaxLongTermFrameIdx {
                    max_long_term_frame_idx: Some(1),
                },
                MemoryManagementControlOperation::MarkCurrentLongTerm {
                    long_term_frame_idx: 0,
                },
            ])
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
    fn reconstructs_and_weaves_complementary_intra_and_predictive_fields() {
        let sps = interlaced_sps();
        let pps = pps();
        let descriptor = CodecDescriptor {
            codec_id: CodecId::new(crate::CODEC_NAME),
            codec_tag: Some(FourCc(*b"avc1")),
            media_type: MediaType::Video,
            configuration: avcc(&sps, &pps),
        };
        let top = ipcm_field_slice(false, 0, [42, 90, 160]);
        let bottom = ipcm_field_slice(true, 1, [84, 100, 170]);
        let mut decoder = H264Decoder::default();
        decoder.configure(&descriptor).unwrap();
        for (index, slice) in [&top, &bottom].into_iter().enumerate() {
            decoder
                .send_packet(Packet {
                    stream_id: StreamId(0),
                    data: length_prefixed(slice),
                    pts: None,
                    dts: None,
                    duration: None,
                    flags: PacketFlags::KEY,
                    side_data: Vec::new(),
                })
                .unwrap();
            if index == 0 {
                assert!(decoder.receive_frame().unwrap().is_none());
                assert!(decoder.flush().is_err());
            }
        }
        let frame = decoder.receive_frame().unwrap().unwrap();
        assert_eq!((frame.width, frame.height), (16, 32));
        assert_eq!(frame.field_order, FieldOrder::TopFirst);
        for row in 0..32 {
            assert!(
                frame.planes[0].data[row * 16..(row + 1) * 16]
                    .iter()
                    .all(|&sample| sample == if row.is_multiple_of(2) { 42 } else { 84 })
            );
        }
        let mut native = frame
            .planes
            .iter()
            .flat_map(|plane| plane.data.iter().copied())
            .collect::<Vec<_>>();
        let p_top = p_skip_field_slice(false, 2);
        let p_bottom = p_skip_field_slice(true, 3);
        for (index, slice) in [&p_top, &p_bottom].into_iter().enumerate() {
            decoder
                .send_packet(Packet {
                    stream_id: StreamId(0),
                    data: length_prefixed(slice),
                    pts: None,
                    dts: None,
                    duration: None,
                    flags: PacketFlags::empty(),
                    side_data: Vec::new(),
                })
                .unwrap();
            if index == 0 {
                assert!(decoder.receive_frame().unwrap().is_none());
            }
        }
        let predicted = decoder.receive_frame().unwrap().unwrap();
        assert_eq!(predicted.field_order, FieldOrder::TopFirst);
        native.extend(
            predicted
                .planes
                .iter()
                .flat_map(|plane| plane.data.iter().copied()),
        );
        if let Some(independent) =
            decode_sequence_with_ffmpeg(&sps, &pps, &[&top, &bottom, &p_top, &p_bottom], 2)
            && native != independent
        {
            let mismatch = native
                .iter()
                .zip(&independent)
                .position(|(native, independent)| native != independent)
                .unwrap();
            panic!(
                "field reconstruction differs from FFmpeg at byte {mismatch}: native={}, independent={}",
                native[mismatch], independent[mismatch]
            );
        }
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn reconstructs_multislice_intra_predictive_and_bipredictive_fields() {
        let sps = interlaced_sps_with_dimensions_and_mbaff(2, 0, 1, true);
        let pps = pps();
        let descriptor = CodecDescriptor {
            codec_id: CodecId::new(crate::CODEC_NAME),
            codec_tag: Some(FourCc(*b"avc1")),
            media_type: MediaType::Video,
            configuration: avcc(&sps, &pps),
        };
        let pictures = [
            (
                vec![
                    ipcm_field_slice_at(0, false, 0, [42, 90, 160]),
                    ipcm_field_slice_at(1, false, 0, [52, 92, 162]),
                ],
                PacketFlags::KEY,
            ),
            (
                vec![
                    ipcm_field_slice_at(0, true, 1, [84, 100, 170]),
                    ipcm_field_slice_at(1, true, 1, [94, 102, 172]),
                ],
                PacketFlags::KEY,
            ),
            (
                vec![
                    p_skip_field_slice_at(0, false, 4),
                    p_skip_field_slice_at(1, false, 4),
                ],
                PacketFlags::empty(),
            ),
            (
                vec![
                    p_skip_field_slice_at(0, true, 5),
                    p_skip_field_slice_at(1, true, 5),
                ],
                PacketFlags::empty(),
            ),
            (
                vec![
                    b_bi_field_slice_at(0, false, 2),
                    b_bi_field_slice_at(1, false, 2),
                ],
                PacketFlags::empty(),
            ),
            (
                vec![
                    b_bi_field_slice_at(0, true, 3),
                    b_bi_field_slice_at(1, true, 3),
                ],
                PacketFlags::empty(),
            ),
        ];
        let mut decoder = H264Decoder::default();
        decoder.configure(&descriptor).unwrap();
        let mut output_frames = Vec::new();
        for (picture, flags) in &pictures {
            let data = picture
                .iter()
                .flat_map(|slice| length_prefixed(slice))
                .collect();
            decoder
                .send_packet(Packet {
                    stream_id: StreamId(0),
                    data,
                    pts: None,
                    dts: None,
                    duration: None,
                    flags: *flags,
                    side_data: Vec::new(),
                })
                .unwrap();
            if let Some(frame) = decoder.receive_frame().unwrap() {
                output_frames.push(frame);
            }
        }
        assert_eq!(output_frames.len(), 3);
        assert_eq!((output_frames[0].width, output_frames[0].height), (16, 64));
        assert_eq!(output_frames[0].field_order, FieldOrder::TopFirst);
        for row in 0_usize..64 {
            let field_row = row / 2;
            let expected = match (row.is_multiple_of(2), field_row < 16) {
                (true, true) => 42,
                (true, false) => 52,
                (false, true) => 84,
                (false, false) => 94,
            };
            assert!(
                output_frames[0].planes[0].data[row * 16..(row + 1) * 16]
                    .iter()
                    .all(|&sample| sample == expected)
            );
        }
        assert_eq!((output_frames[1].width, output_frames[1].height), (16, 64));
        assert_eq!((output_frames[2].width, output_frames[2].height), (16, 64));

        let slices = pictures
            .iter()
            .flat_map(|(picture, _)| picture.iter().map(Vec::as_slice))
            .collect::<Vec<_>>();
        let native = [&output_frames[0], &output_frames[2], &output_frames[1]]
            .into_iter()
            .flat_map(|frame| frame.planes.iter())
            .flat_map(|plane| plane.data.iter().copied())
            .collect::<Vec<_>>();
        if let Some(independent) = decode_sequence_with_ffmpeg(&sps, &pps, &slices, 3) {
            assert_eq!(native, independent);
        }
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn reconstructs_frame_coded_cavlc_pairs_in_mbaff_pictures() {
        let sps = interlaced_sps_with_dimensions_and_mbaff(1, 1, 0, true);
        let pps = pps();
        let slice = mbaff_ipcm_slice(
            [true, false],
            [
                [42, 90, 160],
                [84, 100, 170],
                [142, 110, 180],
                [184, 120, 190],
            ],
        );
        let descriptor = CodecDescriptor {
            codec_id: CodecId::new(crate::CODEC_NAME),
            codec_tag: Some(FourCc(*b"avc1")),
            media_type: MediaType::Video,
            configuration: avcc(&sps, &pps),
        };
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
        assert_eq!((frame.width, frame.height), (32, 32));
        for row in 0_usize..32 {
            let left = if row.is_multiple_of(2) { 42 } else { 84 };
            let right = if row < 16 { 142 } else { 184 };
            assert!(
                frame.planes[0].data[row * 32..row * 32 + 16]
                    .iter()
                    .all(|&sample| sample == left)
            );
            assert!(
                frame.planes[0].data[row * 32 + 16..(row + 1) * 32]
                    .iter()
                    .all(|&sample| sample == right)
            );
        }
        let predicted_slice = mbaff_p_ipcm_slice(
            2,
            [false, true],
            [
                [50, 92, 162],
                [60, 94, 164],
                [150, 112, 182],
                [160, 114, 184],
            ],
        );
        decoder
            .send_packet(Packet {
                stream_id: StreamId(0),
                data: length_prefixed(&predicted_slice),
                pts: None,
                dts: None,
                duration: None,
                flags: PacketFlags::empty(),
                side_data: Vec::new(),
            })
            .unwrap();
        let predicted = decoder.receive_frame().unwrap().unwrap();
        for row in 0_usize..32 {
            let left = if row < 16 { 50 } else { 60 };
            let right = if row.is_multiple_of(2) { 150 } else { 160 };
            assert!(
                predicted.planes[0].data[row * 32..row * 32 + 16]
                    .iter()
                    .all(|&sample| sample == left)
            );
            assert!(
                predicted.planes[0].data[row * 32 + 16..(row + 1) * 32]
                    .iter()
                    .all(|&sample| sample == right)
            );
        }
        let inter_slice = mbaff_p_l0_slice(2, 4);
        decoder
            .send_packet(Packet {
                stream_id: StreamId(0),
                data: length_prefixed(&inter_slice),
                pts: None,
                dts: None,
                duration: None,
                flags: PacketFlags::empty(),
                side_data: Vec::new(),
            })
            .unwrap();
        let inter = decoder.receive_frame().unwrap().unwrap();
        assert_eq!(inter.planes, predicted.planes);
        let b_slice = mbaff_b_bi_slice(3, 1);
        decoder
            .send_packet(Packet {
                stream_id: StreamId(0),
                data: length_prefixed(&b_slice),
                pts: None,
                dts: None,
                duration: None,
                flags: PacketFlags::empty(),
                side_data: Vec::new(),
            })
            .unwrap();
        let b_frame = decoder.receive_frame().unwrap().unwrap();
        let native = [&frame, &b_frame, &predicted, &inter]
            .into_iter()
            .flat_map(|frame| frame.planes.iter())
            .flat_map(|plane| plane.data.iter().copied())
            .collect::<Vec<_>>();
        if let Some(independent) = decode_sequence_with_ffmpeg(
            &sps,
            &pps,
            &[&slice, &predicted_slice, &inter_slice, &b_slice],
            4,
        ) {
            assert_eq!(native, independent);
        }
    }

    #[test]
    fn reconstructs_field_coded_intra4_cavlc_pairs_in_mbaff_pictures() {
        let sps = interlaced_sps_with_dimensions_and_mbaff(1, 1, 0, true);
        let pps = pps();
        let slice = mbaff_intra4_slice();
        let descriptor = CodecDescriptor {
            codec_id: CodecId::new(crate::CODEC_NAME),
            codec_tag: Some(FourCc(*b"avc1")),
            media_type: MediaType::Video,
            configuration: avcc(&sps, &pps),
        };
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
        assert_eq!((frame.width, frame.height), (32, 32));
        assert!(
            frame
                .planes
                .iter()
                .all(|plane| plane.data.iter().all(|&sample| sample == 128))
        );
        let native = frame
            .planes
            .iter()
            .flat_map(|plane| plane.data.iter().copied())
            .collect::<Vec<_>>();
        if let Some(independent) = decode_with_ffmpeg(&sps, &pps, &slice) {
            assert_eq!(native, independent);
        }
    }

    #[test]
    fn reconstructs_field_coded_explicit_bi_cavlc_pairs_in_mbaff_pictures() {
        let sps = interlaced_sps_with_dimensions_and_mbaff(2, 1, 0, true);
        let pps = pps();
        let intra = mbaff_ipcm_slice(
            [true, false],
            [
                [42, 90, 160],
                [84, 100, 170],
                [142, 110, 180],
                [184, 120, 190],
            ],
        );
        let predictive = mbaff_p_ipcm_slice(
            4,
            [false, true],
            [
                [50, 92, 162],
                [60, 94, 164],
                [150, 112, 182],
                [160, 114, 184],
            ],
        );
        let bipredictive = mbaff_b_field_bi_slice(2, 2);
        let descriptor = CodecDescriptor {
            codec_id: CodecId::new(crate::CODEC_NAME),
            codec_tag: Some(FourCc(*b"avc1")),
            media_type: MediaType::Video,
            configuration: avcc(&sps, &pps),
        };
        let mut decoder = H264Decoder::default();
        decoder.configure(&descriptor).unwrap();
        let mut frames = Vec::new();
        for (slice, flags) in [
            (&intra, PacketFlags::KEY),
            (&predictive, PacketFlags::empty()),
            (&bipredictive, PacketFlags::empty()),
        ] {
            decoder
                .send_packet(Packet {
                    stream_id: StreamId(0),
                    data: length_prefixed(slice),
                    pts: None,
                    dts: None,
                    duration: None,
                    flags,
                    side_data: Vec::new(),
                })
                .unwrap();
            frames.push(decoder.receive_frame().unwrap().unwrap());
        }
        let native = [&frames[0], &frames[2], &frames[1]]
            .into_iter()
            .flat_map(|frame| frame.planes.iter())
            .flat_map(|plane| plane.data.iter().copied())
            .collect::<Vec<_>>();
        if let Some(independent) =
            decode_sequence_with_ffmpeg(&sps, &pps, &[&intra, &predictive, &bipredictive], 3)
            && native != independent
        {
            let mismatch = native
                .iter()
                .zip(&independent)
                .position(|(native, independent)| native != independent)
                .unwrap();
            panic!(
                "field explicit-bi reconstruction differs from FFmpeg at byte {mismatch}: native={}, independent={}",
                native[mismatch], independent[mismatch]
            );
        }
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn reconstructs_all_field_coded_explicit_b_partitions_in_mbaff_pictures() {
        let pair_count = 11;
        let sps = interlaced_sps_with_dimensions_and_mbaff(2, pair_count - 1, 0, true);
        let pps = pps();
        let intra = mbaff_constant_ipcm_slice(pair_count, [[40, 90, 160], [80, 100, 170]]);
        let predictive =
            mbaff_constant_p_ipcm_slice(pair_count, 4, [[100, 110, 180], [140, 120, 190]]);
        let macroblock_types = (1..=21).chain(std::iter::once(3)).collect::<Vec<_>>();
        let bipredictive = mbaff_b_field_inter_slice(2, 2, &macroblock_types);
        let descriptor = CodecDescriptor {
            codec_id: CodecId::new(crate::CODEC_NAME),
            codec_tag: Some(FourCc(*b"avc1")),
            media_type: MediaType::Video,
            configuration: avcc(&sps, &pps),
        };
        let mut decoder = H264Decoder::default();
        decoder.configure(&descriptor).unwrap();
        let mut frames = Vec::new();
        for (slice, flags) in [
            (&intra, PacketFlags::KEY),
            (&predictive, PacketFlags::empty()),
            (&bipredictive, PacketFlags::empty()),
        ] {
            decoder
                .send_packet(Packet {
                    stream_id: StreamId(0),
                    data: length_prefixed(slice),
                    pts: None,
                    dts: None,
                    duration: None,
                    flags,
                    side_data: Vec::new(),
                })
                .unwrap();
            frames.push(decoder.receive_frame().unwrap().unwrap());
        }
        let native = [&frames[0], &frames[2], &frames[1]]
            .into_iter()
            .flat_map(|frame| frame.planes.iter())
            .flat_map(|plane| plane.data.iter().copied())
            .collect::<Vec<_>>();
        if let Some(independent) =
            decode_sequence_with_ffmpeg(&sps, &pps, &[&intra, &predictive, &bipredictive], 3)
            && native != independent
        {
            let mismatch = native
                .iter()
                .zip(&independent)
                .position(|(native, independent)| native != independent)
                .unwrap();
            panic!(
                "field explicit-B partition reconstruction differs from FFmpeg at byte {mismatch}: native={}, independent={}",
                native[mismatch], independent[mismatch]
            );
        }
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn reconstructs_all_field_coded_b8x8_subpartitions_in_mbaff_pictures() {
        let sps = interlaced_sps_with_dimensions_and_mbaff(2, 1, 0, true);
        let pps = pps();
        let intra = mbaff_constant_ipcm_slice(2, [[40, 90, 160], [80, 100, 170]]);
        let predictive = mbaff_constant_p_ipcm_slice(2, 4, [[100, 110, 180], [140, 120, 190]]);
        let bipredictive = mbaff_b_field_b8x8_slice(
            2,
            2,
            &[[0, 1, 2, 3], [4, 5, 6, 7], [8, 9, 10, 11], [12, 0, 3, 12]],
        );
        let descriptor = CodecDescriptor {
            codec_id: CodecId::new(crate::CODEC_NAME),
            codec_tag: Some(FourCc(*b"avc1")),
            media_type: MediaType::Video,
            configuration: avcc(&sps, &pps),
        };
        let mut decoder = H264Decoder::default();
        decoder.configure(&descriptor).unwrap();
        let mut frames = Vec::new();
        for (slice, flags) in [
            (&intra, PacketFlags::KEY),
            (&predictive, PacketFlags::empty()),
            (&bipredictive, PacketFlags::empty()),
        ] {
            decoder
                .send_packet(Packet {
                    stream_id: StreamId(0),
                    data: length_prefixed(slice),
                    pts: None,
                    dts: None,
                    duration: None,
                    flags,
                    side_data: Vec::new(),
                })
                .unwrap();
            frames.push(decoder.receive_frame().unwrap().unwrap());
        }
        let native = [&frames[0], &frames[2], &frames[1]]
            .into_iter()
            .flat_map(|frame| frame.planes.iter())
            .flat_map(|plane| plane.data.iter().copied())
            .collect::<Vec<_>>();
        if let Some(independent) =
            decode_sequence_with_ffmpeg(&sps, &pps, &[&intra, &predictive, &bipredictive], 3)
            && native != independent
        {
            let mismatch = native
                .iter()
                .zip(&independent)
                .position(|(native, independent)| native != independent)
                .unwrap();
            panic!(
                "field B_8x8 reconstruction differs from FFmpeg at byte {mismatch}: native={}, independent={}",
                native[mismatch], independent[mismatch]
            );
        }
    }

    #[test]
    fn reconstructs_modified_predictive_field_reference_lists() {
        let sps = interlaced_sps();
        let pps = pps();
        let descriptor = CodecDescriptor {
            codec_id: CodecId::new(crate::CODEC_NAME),
            codec_tag: Some(FourCc(*b"avc1")),
            media_type: MediaType::Video,
            configuration: avcc(&sps, &pps),
        };
        let top = ipcm_field_slice(false, 0, [42, 90, 160]);
        let bottom = ipcm_field_slice(true, 1, [84, 100, 170]);
        let future_top = p_ipcm_field_slice(false, 4, [100, 110, 120]);
        let future_bottom = p_ipcm_field_slice(true, 5, [120, 130, 140]);
        let reordered_top = p_reordered_skip_field_slice(false, 6);
        let reordered_bottom = p_reordered_skip_field_slice(true, 7);
        let mut decoder = H264Decoder::default();
        decoder.configure(&descriptor).unwrap();
        let mut output_frames = Vec::new();
        for (slice, flags) in [
            (&top, PacketFlags::KEY),
            (&bottom, PacketFlags::KEY),
            (&future_top, PacketFlags::empty()),
            (&future_bottom, PacketFlags::empty()),
            (&reordered_top, PacketFlags::empty()),
            (&reordered_bottom, PacketFlags::empty()),
        ] {
            decoder
                .send_packet(Packet {
                    stream_id: StreamId(0),
                    data: length_prefixed(slice),
                    pts: None,
                    dts: None,
                    duration: None,
                    flags,
                    side_data: Vec::new(),
                })
                .unwrap();
            if let Some(frame) = decoder.receive_frame().unwrap() {
                output_frames.push(frame);
            }
        }
        assert_eq!(output_frames.len(), 3);
        assert!(
            output_frames[2].planes[0]
                .data
                .iter()
                .all(|&sample| sample == 84)
        );
        let native = output_frames
            .iter()
            .flat_map(|frame| frame.planes.iter())
            .flat_map(|plane| plane.data.iter().copied())
            .collect::<Vec<_>>();
        if let Some(independent) = decode_sequence_with_ffmpeg(
            &sps,
            &pps,
            &[
                &top,
                &bottom,
                &future_top,
                &future_bottom,
                &reordered_top,
                &reordered_bottom,
            ],
            3,
        ) {
            assert_eq!(native, independent);
        }
    }

    #[test]
    fn applies_adaptive_marking_to_reference_fields() {
        let sps = interlaced_sps_with_max_references(1);
        let pps = pps();
        let descriptor = CodecDescriptor {
            codec_id: CodecId::new(crate::CODEC_NAME),
            codec_tag: Some(FourCc(*b"avc1")),
            media_type: MediaType::Video,
            configuration: avcc(&sps, &pps),
        };
        let top = ipcm_field_slice(false, 0, [42, 90, 160]);
        let bottom = ipcm_field_slice(true, 1, [84, 100, 170]);
        let marked_top = p_forget_previous_ipcm_field_slice(false, 2, [100, 110, 120]);
        let marked_bottom = p_ipcm_field_slice(true, 3, [120, 130, 140]);
        let predicted_top = p_next_skip_field_slice(false, 4);
        let predicted_bottom = p_next_skip_field_slice(true, 5);
        let mut decoder = H264Decoder::default();
        decoder.configure(&descriptor).unwrap();
        let mut output_frames = Vec::new();
        for (slice, flags) in [
            (&top, PacketFlags::KEY),
            (&bottom, PacketFlags::KEY),
            (&marked_top, PacketFlags::empty()),
            (&marked_bottom, PacketFlags::empty()),
            (&predicted_top, PacketFlags::empty()),
            (&predicted_bottom, PacketFlags::empty()),
        ] {
            decoder
                .send_packet(Packet {
                    stream_id: StreamId(0),
                    data: length_prefixed(slice),
                    pts: None,
                    dts: None,
                    duration: None,
                    flags,
                    side_data: Vec::new(),
                })
                .unwrap();
            if let Some(frame) = decoder.receive_frame().unwrap() {
                output_frames.push(frame);
            }
        }
        assert_eq!(output_frames.len(), 3);
        for row in 0..32 {
            assert!(
                output_frames[2].planes[0].data[row * 16..(row + 1) * 16]
                    .iter()
                    .all(|&sample| sample == if row.is_multiple_of(2) { 100 } else { 120 })
            );
        }
        let native = output_frames
            .iter()
            .flat_map(|frame| frame.planes.iter())
            .flat_map(|plane| plane.data.iter().copied())
            .collect::<Vec<_>>();
        if let Some(independent) = decode_sequence_with_ffmpeg(
            &sps,
            &pps,
            &[
                &top,
                &bottom,
                &marked_top,
                &marked_bottom,
                &predicted_top,
                &predicted_bottom,
            ],
            3,
        ) && native != independent
        {
            let mismatch = native
                .iter()
                .zip(&independent)
                .position(|(native, independent)| native != independent)
                .unwrap();
            panic!(
                "adaptive field-marking mismatch at byte {mismatch}: native={}, ffmpeg={}",
                native[mismatch], independent[mismatch]
            );
        }
    }

    #[test]
    fn reconstructs_and_weaves_bipredictive_fields() {
        let sps = interlaced_sps();
        let pps = pps();
        let descriptor = CodecDescriptor {
            codec_id: CodecId::new(crate::CODEC_NAME),
            codec_tag: Some(FourCc(*b"avc1")),
            media_type: MediaType::Video,
            configuration: avcc(&sps, &pps),
        };
        let top = ipcm_field_slice(false, 0, [42, 90, 160]);
        let bottom = ipcm_field_slice(true, 1, [84, 100, 170]);
        let future_top = p_ipcm_field_slice(false, 4, [100, 110, 120]);
        let future_bottom = p_ipcm_field_slice(true, 5, [120, 130, 140]);
        let b_top = b_bi_field_slice(false, 2);
        let b_bottom = b_bi_field_slice(true, 3);
        let mut decoder = H264Decoder::default();
        decoder.configure(&descriptor).unwrap();
        let mut output_frames = Vec::new();
        for (slice, flags) in [
            (&top, PacketFlags::KEY),
            (&bottom, PacketFlags::KEY),
            (&future_top, PacketFlags::empty()),
            (&future_bottom, PacketFlags::empty()),
            (&b_top, PacketFlags::empty()),
            (&b_bottom, PacketFlags::empty()),
        ] {
            decoder
                .send_packet(Packet {
                    stream_id: StreamId(0),
                    data: length_prefixed(slice),
                    pts: None,
                    dts: None,
                    duration: None,
                    flags,
                    side_data: Vec::new(),
                })
                .unwrap();
            if let Some(frame) = decoder.receive_frame().unwrap() {
                output_frames.push(frame);
            }
        }
        assert_eq!(output_frames.len(), 3);
        let native = [&output_frames[0], &output_frames[2], &output_frames[1]]
            .into_iter()
            .flat_map(|frame| frame.planes.iter())
            .flat_map(|plane| plane.data.iter().copied())
            .collect::<Vec<_>>();
        if let Some(independent) = decode_sequence_with_ffmpeg(
            &sps,
            &pps,
            &[
                &top,
                &bottom,
                &future_top,
                &future_bottom,
                &b_top,
                &b_bottom,
            ],
            3,
        ) && native != independent
        {
            let mismatch = native
                .iter()
                .zip(&independent)
                .position(|(native, independent)| native != independent)
                .unwrap();
            panic!(
                "B-field mismatch at byte {mismatch}: native={}, ffmpeg={}",
                native[mismatch], independent[mismatch]
            );
        }
    }

    #[test]
    fn reconstructs_modified_bipredictive_field_reference_lists() {
        let sps = interlaced_sps();
        let pps = pps();
        let descriptor = CodecDescriptor {
            codec_id: CodecId::new(crate::CODEC_NAME),
            codec_tag: Some(FourCc(*b"avc1")),
            media_type: MediaType::Video,
            configuration: avcc(&sps, &pps),
        };
        let top = ipcm_field_slice(false, 0, [42, 90, 160]);
        let bottom = ipcm_field_slice(true, 1, [84, 100, 170]);
        let future_top = p_ipcm_field_slice(false, 4, [100, 110, 120]);
        let future_bottom = p_ipcm_field_slice(true, 5, [120, 130, 140]);
        let b_top = b_bi_reordered_field_slice(false, 2);
        let b_bottom = b_bi_reordered_field_slice(true, 3);
        let mut decoder = H264Decoder::default();
        decoder.configure(&descriptor).unwrap();
        let mut output_frames = Vec::new();
        for (slice, flags) in [
            (&top, PacketFlags::KEY),
            (&bottom, PacketFlags::KEY),
            (&future_top, PacketFlags::empty()),
            (&future_bottom, PacketFlags::empty()),
            (&b_top, PacketFlags::empty()),
            (&b_bottom, PacketFlags::empty()),
        ] {
            decoder
                .send_packet(Packet {
                    stream_id: StreamId(0),
                    data: length_prefixed(slice),
                    pts: None,
                    dts: None,
                    duration: None,
                    flags,
                    side_data: Vec::new(),
                })
                .unwrap();
            if let Some(frame) = decoder.receive_frame().unwrap() {
                output_frames.push(frame);
            }
        }
        assert_eq!(output_frames.len(), 3);
        assert!(
            output_frames[2].planes[0]
                .data
                .iter()
                .all(|&sample| sample == 84)
        );
        let native = [&output_frames[0], &output_frames[2], &output_frames[1]]
            .into_iter()
            .flat_map(|frame| frame.planes.iter())
            .flat_map(|plane| plane.data.iter().copied())
            .collect::<Vec<_>>();
        if let Some(independent) = decode_sequence_with_ffmpeg(
            &sps,
            &pps,
            &[
                &top,
                &bottom,
                &future_top,
                &future_bottom,
                &b_top,
                &b_bottom,
            ],
            3,
        ) {
            assert_eq!(native, independent);
        }
    }

    #[test]
    fn reconstructs_cavlc_b16_predictions_from_both_reference_lists() {
        for (macroblock_type, expected) in [
            (None, [71, 100, 140]),
            (Some(0), [71, 100, 140]),
            (Some(1), [42, 90, 160]),
            (Some(2), [100, 110, 120]),
            (Some(3), [71, 100, 140]),
        ] {
            let sps = sps_with_max_references(2);
            let pps = pps();
            let descriptor = CodecDescriptor {
                codec_id: CodecId::new(crate::CODEC_NAME),
                codec_tag: Some(FourCc(*b"avc1")),
                media_type: MediaType::Video,
                configuration: avcc(&sps, &pps),
            };
            let mut decoder = H264Decoder::default();
            decoder.configure(&descriptor).unwrap();
            let idr = ipcm_slice();
            let future = p_ipcm_slice(1, 4, [100, 110, 120]);
            let b_picture =
                macroblock_type.map_or_else(|| b_skip_slice(2), |value| b_16x16_slice(2, value));
            for (slice, flags) in [
                (idr.clone(), PacketFlags::KEY),
                (future.clone(), PacketFlags::empty()),
                (b_picture.clone(), PacketFlags::empty()),
            ] {
                decoder
                    .send_packet(Packet {
                        stream_id: StreamId(0),
                        data: length_prefixed(&slice),
                        pts: None,
                        dts: None,
                        duration: None,
                        flags,
                        side_data: Vec::new(),
                    })
                    .unwrap();
                let frame = decoder.receive_frame().unwrap().unwrap();
                if slice[0] == 0x01 {
                    assert!(
                        frame.planes[0]
                            .data
                            .iter()
                            .all(|&sample| sample == expected[0])
                    );
                    assert!(
                        frame.planes[1]
                            .data
                            .iter()
                            .all(|&sample| sample == expected[1])
                    );
                    assert!(
                        frame.planes[2]
                            .data
                            .iter()
                            .all(|&sample| sample == expected[2])
                    );
                }
            }
            if let Some(decoded) =
                decode_sequence_with_ffmpeg(&sps, &pps, &[&idr, &future, &b_picture], 3)
            {
                assert_eq!(decoded.len(), 3 * 384);
                let presented_b = &decoded[384..768];
                assert!(
                    presented_b[..256]
                        .iter()
                        .all(|&sample| sample == expected[0])
                );
                assert!(
                    presented_b[256..320]
                        .iter()
                        .all(|&sample| sample == expected[1])
                );
                assert!(
                    presented_b[320..]
                        .iter()
                        .all(|&sample| sample == expected[2])
                );
            }
        }
    }

    #[test]
    fn independent_non_reference_fork_matches_sequential_b_picture() {
        let sps = sps_with_max_references(2);
        let pps = pps();
        let descriptor = CodecDescriptor {
            codec_id: CodecId::new(crate::CODEC_NAME),
            codec_tag: Some(FourCc(*b"avc1")),
            media_type: MediaType::Video,
            configuration: avcc(&sps, &pps),
        };
        let packet = |slice: &[u8], flags| Packet {
            stream_id: StreamId(0),
            data: length_prefixed(slice),
            pts: None,
            dts: None,
            duration: None,
            flags,
            side_data: Vec::new(),
        };
        let idr = ipcm_slice();
        let future = p_ipcm_slice(1, 4, [100, 110, 120]);
        let b_picture = b_skip_slice(2);
        let mut sequential = H264Decoder::default();
        sequential.configure(&descriptor).unwrap();
        sequential
            .send_packet(packet(&idr, PacketFlags::KEY))
            .unwrap();
        sequential.receive_frame().unwrap().unwrap();
        sequential
            .send_packet(packet(&future, PacketFlags::empty()))
            .unwrap();
        sequential.receive_frame().unwrap().unwrap();

        let mut independent = sequential.fork_for_non_reference_picture().unwrap();
        sequential
            .send_packet(packet(&b_picture, PacketFlags::empty()))
            .unwrap();
        independent
            .send_packet(packet(&b_picture, PacketFlags::empty()))
            .unwrap();
        assert_eq!(
            independent.receive_frame().unwrap(),
            sequential.receive_frame().unwrap()
        );
        assert!(matches!(
            independent.send_packet(packet(&future, PacketFlags::empty())),
            Err(Error::InvalidState(_))
        ));
    }

    #[test]
    fn reconstructs_temporal_direct_cavlc_b_skip_with_scaled_motion() {
        let sps = sps_with_max_references(2);
        let pps = pps();
        let descriptor = CodecDescriptor {
            codec_id: CodecId::new(crate::CODEC_NAME),
            codec_tag: Some(FourCc(*b"avc1")),
            media_type: MediaType::Video,
            configuration: avcc(&sps, &pps),
        };
        let idr = patterned_ipcm_slice(true, 0, 0, 0);
        let future = p_motion_slice(1, 4, 8, 0);
        let b_picture = b_skip_slice_with_direct(2, false);
        let mut decoder = H264Decoder::default();
        decoder.configure(&descriptor).unwrap();
        let mut b_frame = None;
        for (slice, flags) in [
            (&idr, PacketFlags::KEY),
            (&future, PacketFlags::empty()),
            (&b_picture, PacketFlags::empty()),
        ] {
            decoder
                .send_packet(Packet {
                    stream_id: StreamId(0),
                    data: length_prefixed(slice),
                    pts: None,
                    dts: None,
                    duration: None,
                    flags,
                    side_data: Vec::new(),
                })
                .unwrap();
            b_frame = decoder.receive_frame().unwrap();
        }
        let native = b_frame
            .unwrap()
            .planes
            .into_iter()
            .flat_map(|plane| plane.data)
            .collect::<Vec<_>>();
        if let Some(decoded) =
            decode_sequence_with_ffmpeg(&sps, &pps, &[&idr, &future, &b_picture], 3)
        {
            assert_eq!(native, decoded[384..768]);
        }
    }

    #[test]
    fn reconstructs_explicit_weighted_cavlc_biprediction() {
        let sps = sps_with_max_references(2);
        let pps = pps_with_weighted_bipred(1);
        let descriptor = CodecDescriptor {
            codec_id: CodecId::new(crate::CODEC_NAME),
            codec_tag: Some(FourCc(*b"avc1")),
            media_type: MediaType::Video,
            configuration: avcc(&sps, &pps),
        };
        let idr = ipcm_slice();
        let future = p_ipcm_slice(1, 4, [100, 110, 120]);
        let b_picture = explicit_weighted_bi_slice(2);
        let mut decoder = H264Decoder::default();
        decoder.configure(&descriptor).unwrap();
        let mut b_frame = None;
        for (slice, flags) in [
            (&idr, PacketFlags::KEY),
            (&future, PacketFlags::empty()),
            (&b_picture, PacketFlags::empty()),
        ] {
            decoder
                .send_packet(Packet {
                    stream_id: StreamId(0),
                    data: length_prefixed(slice),
                    pts: None,
                    dts: None,
                    duration: None,
                    flags,
                    side_data: Vec::new(),
                })
                .unwrap();
            b_frame = decoder.receive_frame().unwrap();
        }
        let frame = b_frame.unwrap();
        for (plane, expected) in frame.planes.iter().zip([88, 105, 150]) {
            assert!(plane.data.iter().all(|&sample| sample == expected));
        }
        if let Some(decoded) =
            decode_sequence_with_ffmpeg(&sps, &pps, &[&idr, &future, &b_picture], 3)
        {
            let native = frame
                .planes
                .iter()
                .flat_map(|plane| plane.data.iter().copied())
                .collect::<Vec<_>>();
            assert_eq!(native, decoded[384..768]);
        }
    }

    #[test]
    fn reconstructs_implicit_weighted_cavlc_biprediction() {
        let sps = sps_with_max_references(2);
        let pps = pps_with_weighted_bipred(2);
        let descriptor = CodecDescriptor {
            codec_id: CodecId::new(crate::CODEC_NAME),
            codec_tag: Some(FourCc(*b"avc1")),
            media_type: MediaType::Video,
            configuration: avcc(&sps, &pps),
        };
        let idr = ipcm_slice();
        let future = p_ipcm_slice(1, 4, [100, 110, 120]);
        let b_picture = b_16x16_slice(1, 3);
        let mut decoder = H264Decoder::default();
        decoder.configure(&descriptor).unwrap();
        let mut b_frame = None;
        for (slice, flags) in [
            (&idr, PacketFlags::KEY),
            (&future, PacketFlags::empty()),
            (&b_picture, PacketFlags::empty()),
        ] {
            decoder
                .send_packet(Packet {
                    stream_id: StreamId(0),
                    data: length_prefixed(slice),
                    pts: None,
                    dts: None,
                    duration: None,
                    flags,
                    side_data: Vec::new(),
                })
                .unwrap();
            b_frame = decoder.receive_frame().unwrap();
        }
        let frame = b_frame.unwrap();
        for (plane, expected) in frame.planes.iter().zip([57, 95, 150]) {
            assert!(plane.data.iter().all(|&sample| sample == expected));
        }
        if let Some(decoded) =
            decode_sequence_with_ffmpeg(&sps, &pps, &[&idr, &future, &b_picture], 3)
        {
            let native = frame
                .planes
                .iter()
                .flat_map(|plane| plane.data.iter().copied())
                .collect::<Vec<_>>();
            assert_eq!(native, decoded[384..768]);
        }
    }

    #[test]
    fn reconstructs_cavlc_b16x8_and_b8x16_mixed_lists() {
        for macroblock_type in 4..=21 {
            let partitions = b_inter_partitions(macroblock_type);
            let predictions = [partitions[0].1, partitions[1].1];
            let vertical_split = !(macroblock_type - 4).is_multiple_of(2);
            let sps = sps_with_max_references(2);
            let pps = pps();
            let descriptor = CodecDescriptor {
                codec_id: CodecId::new(crate::CODEC_NAME),
                codec_tag: Some(FourCc(*b"avc1")),
                media_type: MediaType::Video,
                configuration: avcc(&sps, &pps),
            };
            let idr = ipcm_slice();
            let future = p_ipcm_slice(1, 4, [100, 110, 120]);
            let b_picture = b_two_partition_slice(2, macroblock_type);
            let mut decoder = H264Decoder::default();
            decoder.configure(&descriptor).unwrap();
            let mut b_frame = None;
            for (slice, flags) in [
                (&idr, PacketFlags::KEY),
                (&future, PacketFlags::empty()),
                (&b_picture, PacketFlags::empty()),
            ] {
                decoder
                    .send_packet(Packet {
                        stream_id: StreamId(0),
                        data: length_prefixed(slice),
                        pts: None,
                        dts: None,
                        duration: None,
                        flags,
                        side_data: Vec::new(),
                    })
                    .unwrap();
                b_frame = decoder.receive_frame().unwrap();
            }
            let b_frame = b_frame.unwrap();
            for (plane_index, (past, future)) in
                [(42, 100), (90, 110), (160, 120)].into_iter().enumerate()
            {
                let plane = &b_frame.planes[plane_index];
                for y in 0..plane.height {
                    for x in 0..plane.width {
                        let use_future = if vertical_split {
                            x >= plane.width / 2
                        } else {
                            y >= plane.height / 2
                        };
                        let partition = usize::from(use_future);
                        let expected = match predictions[partition] {
                            BPrediction::Direct => unreachable!("explicit B type is not direct"),
                            BPrediction::L0 => past,
                            BPrediction::L1 => future,
                            BPrediction::Bi => average_samples(past, future),
                        };
                        assert_eq!(plane.data[y * plane.stride + x], expected);
                    }
                }
            }
            if let Some(decoded) =
                decode_sequence_with_ffmpeg(&sps, &pps, &[&idr, &future, &b_picture], 3)
            {
                let native = b_frame
                    .planes
                    .iter()
                    .flat_map(|plane| plane.data.iter().copied())
                    .collect::<Vec<_>>();
                assert_eq!(native, decoded[384..768]);
            }
        }
    }

    #[test]
    fn reconstructs_all_explicit_cavlc_b8x8_sub_macroblocks() {
        let cases = (0..=12)
            .map(|sub_type| [sub_type; 4])
            .chain([[0, 1, 2, 3], [1, 2, 3, 1]]);
        for (direct_8x8_inference, sub_types) in [true, false]
            .into_iter()
            .flat_map(|inference| cases.clone().map(move |sub_types| (inference, sub_types)))
        {
            let sps = sps_with_max_references_and_direct(2, direct_8x8_inference);
            let pps = pps();
            let descriptor = CodecDescriptor {
                codec_id: CodecId::new(crate::CODEC_NAME),
                codec_tag: Some(FourCc(*b"avc1")),
                media_type: MediaType::Video,
                configuration: avcc(&sps, &pps),
            };
            let idr = ipcm_slice();
            let future = p_ipcm_slice(1, 4, [100, 110, 120]);
            let b_picture = b8x8_slice(2, sub_types, direct_8x8_inference);
            let mut decoder = H264Decoder::default();
            decoder.configure(&descriptor).unwrap();
            let mut b_frame = None;
            for (slice, flags) in [
                (&idr, PacketFlags::KEY),
                (&future, PacketFlags::empty()),
                (&b_picture, PacketFlags::empty()),
            ] {
                decoder
                    .send_packet(Packet {
                        stream_id: StreamId(0),
                        data: length_prefixed(slice),
                        pts: None,
                        dts: None,
                        duration: None,
                        flags,
                        side_data: Vec::new(),
                    })
                    .unwrap();
                b_frame = decoder.receive_frame().unwrap();
            }
            let b_frame = b_frame.unwrap();
            let predictions = sub_types.map(|sub_type| {
                b_sub_macroblock(0, sub_type, direct_8x8_inference)
                    .unwrap()
                    .prediction
            });
            for (plane_index, (past, future)) in
                [(42, 100), (90, 110), (160, 120)].into_iter().enumerate()
            {
                let plane = &b_frame.planes[plane_index];
                for y in 0..plane.height {
                    for x in 0..plane.width {
                        let sub_index = usize::from(x >= plane.width / 2)
                            + 2 * usize::from(y >= plane.height / 2);
                        let expected = match predictions[sub_index] {
                            BPrediction::L0 => past,
                            BPrediction::L1 => future,
                            BPrediction::Direct | BPrediction::Bi => average_samples(past, future),
                        };
                        assert_eq!(plane.data[y * plane.stride + x], expected);
                    }
                }
            }
            if let Some(decoded) =
                decode_sequence_with_ffmpeg(&sps, &pps, &[&idr, &future, &b_picture], 3)
            {
                let native = b_frame
                    .planes
                    .iter()
                    .flat_map(|plane| plane.data.iter().copied())
                    .collect::<Vec<_>>();
                assert_eq!(native, decoded[384..768]);
            }
        }
    }

    #[test]
    fn reconstructs_cavlc_b8x8_bi_4x4_nonzero_motion() {
        let sps = sps_with_max_references(2);
        let pps = pps();
        let descriptor = CodecDescriptor {
            codec_id: CodecId::new(crate::CODEC_NAME),
            codec_tag: Some(FourCc(*b"avc1")),
            media_type: MediaType::Video,
            configuration: avcc(&sps, &pps),
        };
        let idr = patterned_ipcm_slice(true, 0, 0, 0);
        let future = patterned_ipcm_slice(false, 1, 4, 37);
        let b_picture = b8x8_bi_4x4_motion_slice(2);
        let mut decoder = H264Decoder::default();
        decoder.configure(&descriptor).unwrap();
        let mut b_frame = None;
        for (slice, flags) in [
            (&idr, PacketFlags::KEY),
            (&future, PacketFlags::empty()),
            (&b_picture, PacketFlags::empty()),
        ] {
            decoder
                .send_packet(Packet {
                    stream_id: StreamId(0),
                    data: length_prefixed(slice),
                    pts: None,
                    dts: None,
                    duration: None,
                    flags,
                    side_data: Vec::new(),
                })
                .unwrap();
            b_frame = decoder.receive_frame().unwrap();
        }
        let native = b_frame
            .unwrap()
            .planes
            .into_iter()
            .flat_map(|plane| plane.data)
            .collect::<Vec<_>>();
        if let Some(decoded) =
            decode_sequence_with_ffmpeg(&sps, &pps, &[&idr, &future, &b_picture], 3)
        {
            assert_eq!(native, decoded[384..768]);
        }
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
        sps_with_max_references(0)
    }

    fn interlaced_sps() -> Vec<u8> {
        interlaced_sps_with_max_references(2)
    }

    fn interlaced_sps_with_max_references(max_num_ref_frames: u32) -> Vec<u8> {
        interlaced_sps_with_dimensions(max_num_ref_frames, 0, 0)
    }

    fn interlaced_sps_with_dimensions(
        max_num_ref_frames: u32,
        pic_width_in_mbs_minus1: u32,
        pic_height_in_map_units_minus1: u32,
    ) -> Vec<u8> {
        interlaced_sps_with_dimensions_and_mbaff(
            max_num_ref_frames,
            pic_width_in_mbs_minus1,
            pic_height_in_map_units_minus1,
            false,
        )
    }

    fn interlaced_sps_with_dimensions_and_mbaff(
        max_num_ref_frames: u32,
        pic_width_in_mbs_minus1: u32,
        pic_height_in_map_units_minus1: u32,
        mb_adaptive_frame_field: bool,
    ) -> Vec<u8> {
        let mut writer = BitWriter::new();
        writer.write_bits(66, 8).unwrap();
        writer.write_bits(0, 8).unwrap();
        writer.write_bits(10, 8).unwrap();
        write_ue(&mut writer, 0);
        write_ue(&mut writer, 0);
        write_ue(&mut writer, 0);
        write_ue(&mut writer, 0);
        write_ue(&mut writer, max_num_ref_frames);
        writer.write_bit(false).unwrap();
        write_ue(&mut writer, pic_width_in_mbs_minus1);
        write_ue(&mut writer, pic_height_in_map_units_minus1);
        writer.write_bit(false).unwrap();
        writer.write_bit(mb_adaptive_frame_field).unwrap();
        writer.write_bit(true).unwrap();
        writer.write_bit(false).unwrap();
        writer.write_bit(false).unwrap();
        finish_rbsp(&mut writer);
        [vec![0x67], writer.into_bytes()].concat()
    }

    fn empty_reference(frame_num: u32) -> ReferenceFrame {
        ReferenceFrame {
            structure: PictureStructure::Frame,
            frame_num,
            pic_order_count: 0,
            long_term_frame_idx: None,
            coded_width: 0,
            coded_height: 0,
            luma: Arc::default(),
            cb: Arc::default(),
            cr: Arc::default(),
            motion_l0: Arc::default(),
            motion_l0_available: Arc::default(),
            reference_l0: Arc::default(),
            motion_l1: Arc::default(),
            motion_l1_available: Arc::default(),
            reference_l1: Arc::default(),
            macroblock_intra: Arc::default(),
        }
    }

    fn sps_with_max_references(max_num_ref_frames: u32) -> Vec<u8> {
        sps_with_max_references_and_direct(max_num_ref_frames, true)
    }

    fn sps_with_max_references_and_direct(
        max_num_ref_frames: u32,
        direct_8x8_inference: bool,
    ) -> Vec<u8> {
        sps_with_dimensions_and_direct(max_num_ref_frames, 0, 0, direct_8x8_inference)
    }

    fn sps_with_dimensions(
        max_num_ref_frames: u32,
        pic_width_in_mbs_minus1: u32,
        pic_height_in_map_units_minus1: u32,
    ) -> Vec<u8> {
        sps_with_dimensions_and_direct(
            max_num_ref_frames,
            pic_width_in_mbs_minus1,
            pic_height_in_map_units_minus1,
            true,
        )
    }

    fn sps_with_dimensions_and_direct(
        max_num_ref_frames: u32,
        pic_width_in_mbs_minus1: u32,
        pic_height_in_map_units_minus1: u32,
        direct_8x8_inference: bool,
    ) -> Vec<u8> {
        let mut writer = BitWriter::new();
        writer.write_bits(66, 8).unwrap();
        writer.write_bits(0, 8).unwrap();
        writer.write_bits(10, 8).unwrap();
        write_ue(&mut writer, 0);
        write_ue(&mut writer, 0);
        write_ue(&mut writer, 0);
        write_ue(&mut writer, 0);
        write_ue(&mut writer, max_num_ref_frames);
        writer.write_bit(false).unwrap();
        write_ue(&mut writer, pic_width_in_mbs_minus1);
        write_ue(&mut writer, pic_height_in_map_units_minus1);
        writer.write_bit(true).unwrap();
        writer.write_bit(direct_8x8_inference).unwrap();
        writer.write_bit(false).unwrap();
        writer.write_bit(false).unwrap();
        finish_rbsp(&mut writer);
        [vec![0x67], writer.into_bytes()].concat()
    }

    fn sps_type2(max_num_ref_frames: u32) -> Vec<u8> {
        let mut writer = BitWriter::new();
        writer.write_bits(66, 8).unwrap();
        writer.write_bits(0, 8).unwrap();
        writer.write_bits(10, 8).unwrap();
        write_ue(&mut writer, 0);
        write_ue(&mut writer, 0);
        write_ue(&mut writer, 2);
        write_ue(&mut writer, max_num_ref_frames);
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

    fn sps_type1(max_num_ref_frames: u32) -> Vec<u8> {
        let mut writer = BitWriter::new();
        writer.write_bits(66, 8).unwrap();
        writer.write_bits(0, 8).unwrap();
        writer.write_bits(10, 8).unwrap();
        write_ue(&mut writer, 0);
        write_ue(&mut writer, 0);
        write_ue(&mut writer, 1);
        writer.write_bit(true).unwrap();
        write_se(&mut writer, -2);
        write_se(&mut writer, 0);
        write_ue(&mut writer, 1);
        write_se(&mut writer, 4);
        write_ue(&mut writer, max_num_ref_frames);
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
        pps_with_weighted_bipred(0)
    }

    fn pps_with_weighted_bipred(weighted_bipred_idc: u64) -> Vec<u8> {
        assert!(weighted_bipred_idc <= 2);
        let mut writer = BitWriter::new();
        write_ue(&mut writer, 0);
        write_ue(&mut writer, 0);
        writer.write_bit(false).unwrap();
        writer.write_bit(false).unwrap();
        write_ue(&mut writer, 0);
        write_ue(&mut writer, 0);
        write_ue(&mut writer, 0);
        writer.write_bit(false).unwrap();
        writer.write_bits(weighted_bipred_idc, 2).unwrap();
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
        ipcm_slice_with_long_term(false)
    }

    fn ipcm_field_slice(bottom: bool, poc_lsb: u64, samples: [u8; 3]) -> Vec<u8> {
        ipcm_field_slice_at(0, bottom, poc_lsb, samples)
    }

    fn ipcm_field_slice_at(
        first_macroblock: u32,
        bottom: bool,
        poc_lsb: u64,
        samples: [u8; 3],
    ) -> Vec<u8> {
        let mut writer = BitWriter::new();
        write_ue(&mut writer, first_macroblock);
        write_ue(&mut writer, 2);
        write_ue(&mut writer, 0);
        writer.write_bits(0, 4).unwrap();
        writer.write_bit(true).unwrap();
        writer.write_bit(bottom).unwrap();
        write_ue(&mut writer, 0);
        writer.write_bits(poc_lsb, 4).unwrap();
        writer.write_bit(false).unwrap();
        writer.write_bit(false).unwrap();
        write_se(&mut writer, 0);
        write_ue(&mut writer, 1);
        write_ue(&mut writer, 25);
        writer.align_to_byte();
        for value in std::iter::repeat_n(samples[0], 256)
            .chain(std::iter::repeat_n(samples[1], 64))
            .chain(std::iter::repeat_n(samples[2], 64))
        {
            writer.write_bits(u64::from(value), 8).unwrap();
        }
        finish_rbsp(&mut writer);
        [vec![0x65], writer.into_bytes()].concat()
    }

    fn mbaff_ipcm_slice(field_coded_pairs: [bool; 2], samples: [[u8; 3]; 4]) -> Vec<u8> {
        let mut writer = BitWriter::new();
        write_ue(&mut writer, 0);
        write_ue(&mut writer, 2);
        write_ue(&mut writer, 0);
        writer.write_bits(0, 4).unwrap();
        writer.write_bit(false).unwrap();
        write_ue(&mut writer, 0);
        writer.write_bits(0, 4).unwrap();
        writer.write_bit(false).unwrap();
        writer.write_bit(false).unwrap();
        write_se(&mut writer, 0);
        write_ue(&mut writer, 1);
        for (address, samples) in samples.into_iter().enumerate() {
            if address.is_multiple_of(2) {
                writer.write_bit(field_coded_pairs[address / 2]).unwrap();
            }
            write_ue(&mut writer, 25);
            writer.align_to_byte();
            for value in std::iter::repeat_n(samples[0], 256)
                .chain(std::iter::repeat_n(samples[1], 64))
                .chain(std::iter::repeat_n(samples[2], 64))
            {
                writer.write_bits(u64::from(value), 8).unwrap();
            }
        }
        finish_rbsp(&mut writer);
        [vec![0x65], writer.into_bytes()].concat()
    }

    fn mbaff_intra4_slice() -> Vec<u8> {
        let mut writer = BitWriter::new();
        write_ue(&mut writer, 0);
        write_ue(&mut writer, 2);
        write_ue(&mut writer, 0);
        writer.write_bits(0, 4).unwrap();
        writer.write_bit(false).unwrap();
        write_ue(&mut writer, 0);
        writer.write_bits(0, 4).unwrap();
        writer.write_bit(false).unwrap();
        writer.write_bit(false).unwrap();
        write_se(&mut writer, 0);
        write_ue(&mut writer, 1);
        for address in 0_usize..4 {
            if address.is_multiple_of(2) {
                writer.write_bit(true).unwrap();
            }
            write_ue(&mut writer, 0);
            for _ in 0..16 {
                writer.write_bit(true).unwrap();
            }
            write_ue(&mut writer, 0);
            write_ue(&mut writer, 3);
        }
        finish_rbsp(&mut writer);
        [vec![0x65], writer.into_bytes()].concat()
    }

    fn mbaff_constant_ipcm_slice(pair_count: u32, samples: [[u8; 3]; 2]) -> Vec<u8> {
        let mut writer = BitWriter::new();
        write_ue(&mut writer, 0);
        write_ue(&mut writer, 2);
        write_ue(&mut writer, 0);
        writer.write_bits(0, 4).unwrap();
        writer.write_bit(false).unwrap();
        write_ue(&mut writer, 0);
        writer.write_bits(0, 4).unwrap();
        writer.write_bit(false).unwrap();
        writer.write_bit(false).unwrap();
        write_se(&mut writer, 0);
        write_ue(&mut writer, 1);
        for address in 0..pair_count * 2 {
            if address.is_multiple_of(2) {
                writer.write_bit(true).unwrap();
            }
            write_ue(&mut writer, 25);
            writer.align_to_byte();
            let samples = samples[usize::try_from(address % 2).unwrap()];
            for value in std::iter::repeat_n(samples[0], 256)
                .chain(std::iter::repeat_n(samples[1], 64))
                .chain(std::iter::repeat_n(samples[2], 64))
            {
                writer.write_bits(u64::from(value), 8).unwrap();
            }
        }
        finish_rbsp(&mut writer);
        [vec![0x65], writer.into_bytes()].concat()
    }

    fn mbaff_constant_p_ipcm_slice(
        pair_count: u32,
        poc_lsb: u64,
        samples: [[u8; 3]; 2],
    ) -> Vec<u8> {
        let mut writer = BitWriter::new();
        write_ue(&mut writer, 0);
        write_ue(&mut writer, 0);
        write_ue(&mut writer, 0);
        writer.write_bits(1, 4).unwrap();
        writer.write_bit(false).unwrap();
        writer.write_bits(poc_lsb, 4).unwrap();
        writer.write_bit(false).unwrap();
        writer.write_bit(false).unwrap();
        writer.write_bit(false).unwrap();
        write_se(&mut writer, 0);
        write_ue(&mut writer, 1);
        for address in 0..pair_count * 2 {
            write_ue(&mut writer, 0);
            if address.is_multiple_of(2) {
                writer.write_bit(true).unwrap();
            }
            write_ue(&mut writer, 30);
            writer.align_to_byte();
            let samples = samples[usize::try_from(address % 2).unwrap()];
            for value in std::iter::repeat_n(samples[0], 256)
                .chain(std::iter::repeat_n(samples[1], 64))
                .chain(std::iter::repeat_n(samples[2], 64))
            {
                writer.write_bits(u64::from(value), 8).unwrap();
            }
        }
        finish_rbsp(&mut writer);
        [vec![0x41], writer.into_bytes()].concat()
    }

    fn mbaff_p_ipcm_slice(
        poc_lsb: u64,
        field_coded_pairs: [bool; 2],
        samples: [[u8; 3]; 4],
    ) -> Vec<u8> {
        let mut writer = BitWriter::new();
        write_ue(&mut writer, 0);
        write_ue(&mut writer, 0);
        write_ue(&mut writer, 0);
        writer.write_bits(1, 4).unwrap();
        writer.write_bit(false).unwrap();
        writer.write_bits(poc_lsb, 4).unwrap();
        writer.write_bit(false).unwrap();
        writer.write_bit(false).unwrap();
        writer.write_bit(false).unwrap();
        write_se(&mut writer, 0);
        write_ue(&mut writer, 1);
        for (address, samples) in samples.into_iter().enumerate() {
            write_ue(&mut writer, 0);
            if address.is_multiple_of(2) {
                writer.write_bit(field_coded_pairs[address / 2]).unwrap();
            }
            write_ue(&mut writer, 30);
            writer.align_to_byte();
            for value in std::iter::repeat_n(samples[0], 256)
                .chain(std::iter::repeat_n(samples[1], 64))
                .chain(std::iter::repeat_n(samples[2], 64))
            {
                writer.write_bits(u64::from(value), 8).unwrap();
            }
        }
        finish_rbsp(&mut writer);
        [vec![0x41], writer.into_bytes()].concat()
    }

    fn mbaff_p_l0_slice(frame_num: u64, poc_lsb: u64) -> Vec<u8> {
        let mut writer = BitWriter::new();
        write_ue(&mut writer, 0);
        write_ue(&mut writer, 0);
        write_ue(&mut writer, 0);
        writer.write_bits(frame_num, 4).unwrap();
        writer.write_bit(false).unwrap();
        writer.write_bits(poc_lsb, 4).unwrap();
        writer.write_bit(false).unwrap();
        writer.write_bit(false).unwrap();
        writer.write_bit(false).unwrap();
        write_se(&mut writer, 0);
        write_ue(&mut writer, 1);
        for address in 0_usize..4 {
            write_ue(&mut writer, 0);
            if address.is_multiple_of(2) {
                writer.write_bit(false).unwrap();
            }
            write_ue(&mut writer, 0);
            write_se(&mut writer, 0);
            write_se(&mut writer, 0);
            write_ue(&mut writer, 0);
        }
        finish_rbsp(&mut writer);
        [vec![0x41], writer.into_bytes()].concat()
    }

    fn mbaff_b_bi_slice(frame_num: u64, poc_lsb: u64) -> Vec<u8> {
        let mut writer = BitWriter::new();
        write_ue(&mut writer, 0);
        write_ue(&mut writer, 1);
        write_ue(&mut writer, 0);
        writer.write_bits(frame_num, 4).unwrap();
        writer.write_bit(false).unwrap();
        writer.write_bits(poc_lsb, 4).unwrap();
        writer.write_bit(true).unwrap();
        writer.write_bit(false).unwrap();
        writer.write_bit(false).unwrap();
        writer.write_bit(false).unwrap();
        write_se(&mut writer, 0);
        write_ue(&mut writer, 1);
        for address in 0_usize..4 {
            write_ue(&mut writer, 0);
            if address.is_multiple_of(2) {
                writer.write_bit(false).unwrap();
            }
            write_ue(&mut writer, 3);
            for _ in 0..4 {
                write_se(&mut writer, 0);
            }
            write_ue(&mut writer, 0);
        }
        finish_rbsp(&mut writer);
        [vec![0x01], writer.into_bytes()].concat()
    }

    fn mbaff_b_field_bi_slice(frame_num: u64, poc_lsb: u64) -> Vec<u8> {
        let mut writer = BitWriter::new();
        write_ue(&mut writer, 0);
        write_ue(&mut writer, 1);
        write_ue(&mut writer, 0);
        writer.write_bits(frame_num, 4).unwrap();
        writer.write_bit(false).unwrap();
        writer.write_bits(poc_lsb, 4).unwrap();
        writer.write_bit(true).unwrap();
        writer.write_bit(false).unwrap();
        writer.write_bit(false).unwrap();
        writer.write_bit(false).unwrap();
        write_se(&mut writer, 0);
        write_ue(&mut writer, 1);
        for address in 0_usize..4 {
            write_ue(&mut writer, 0);
            if address.is_multiple_of(2) {
                writer.write_bit(true).unwrap();
            }
            write_ue(&mut writer, 3);
            writer.write_bit(true).unwrap();
            writer.write_bit(true).unwrap();
            for _ in 0..4 {
                write_se(&mut writer, 0);
            }
            write_ue(&mut writer, 0);
        }
        finish_rbsp(&mut writer);
        [vec![0x01], writer.into_bytes()].concat()
    }

    fn mbaff_b_field_inter_slice(
        frame_num: u64,
        poc_lsb: u64,
        macroblock_types: &[u32],
    ) -> Vec<u8> {
        let mut writer = BitWriter::new();
        write_ue(&mut writer, 0);
        write_ue(&mut writer, 1);
        write_ue(&mut writer, 0);
        writer.write_bits(frame_num, 4).unwrap();
        writer.write_bit(false).unwrap();
        writer.write_bits(poc_lsb, 4).unwrap();
        writer.write_bit(true).unwrap();
        writer.write_bit(false).unwrap();
        writer.write_bit(false).unwrap();
        writer.write_bit(false).unwrap();
        write_se(&mut writer, 0);
        write_ue(&mut writer, 1);
        for (address, &macroblock_type) in macroblock_types.iter().enumerate() {
            write_ue(&mut writer, 0);
            if address.is_multiple_of(2) {
                writer.write_bit(true).unwrap();
            }
            write_ue(&mut writer, macroblock_type);
            let partitions = b_inter_partitions(macroblock_type);
            for (_, prediction) in &partitions {
                if prediction.uses_l0() {
                    writer.write_bit(true).unwrap();
                }
            }
            for (_, prediction) in &partitions {
                if prediction.uses_l1() {
                    writer.write_bit(true).unwrap();
                }
            }
            for (_, prediction) in &partitions {
                if prediction.uses_l0() {
                    write_se(&mut writer, 0);
                    write_se(&mut writer, 0);
                }
            }
            for (_, prediction) in &partitions {
                if prediction.uses_l1() {
                    write_se(&mut writer, 0);
                    write_se(&mut writer, 0);
                }
            }
            write_ue(&mut writer, 0);
        }
        finish_rbsp(&mut writer);
        [vec![0x01], writer.into_bytes()].concat()
    }

    fn mbaff_b_field_b8x8_slice(frame_num: u64, poc_lsb: u64, subtypes: &[[u32; 4]]) -> Vec<u8> {
        let mut writer = BitWriter::new();
        write_ue(&mut writer, 0);
        write_ue(&mut writer, 1);
        write_ue(&mut writer, 0);
        writer.write_bits(frame_num, 4).unwrap();
        writer.write_bit(false).unwrap();
        writer.write_bits(poc_lsb, 4).unwrap();
        writer.write_bit(true).unwrap();
        writer.write_bit(false).unwrap();
        writer.write_bit(false).unwrap();
        writer.write_bit(false).unwrap();
        write_se(&mut writer, 0);
        write_ue(&mut writer, 1);
        for (address, subtypes) in subtypes.iter().enumerate() {
            write_ue(&mut writer, 0);
            if address.is_multiple_of(2) {
                writer.write_bit(true).unwrap();
            }
            write_ue(&mut writer, 22);
            let sub_macroblocks = std::array::from_fn::<_, 4, _>(|index| {
                b_sub_macroblock(index, subtypes[index], true).unwrap()
            });
            for &subtype in subtypes {
                write_ue(&mut writer, subtype);
            }
            for sub in &sub_macroblocks {
                if sub.prediction.uses_l0() {
                    writer.write_bit(true).unwrap();
                }
            }
            for sub in &sub_macroblocks {
                if sub.prediction.uses_l1() {
                    writer.write_bit(true).unwrap();
                }
            }
            for sub in &sub_macroblocks {
                if sub.prediction.uses_l0() {
                    for _ in &sub.partitions {
                        write_se(&mut writer, 0);
                        write_se(&mut writer, 0);
                    }
                }
            }
            for sub in &sub_macroblocks {
                if sub.prediction.uses_l1() {
                    for _ in &sub.partitions {
                        write_se(&mut writer, 0);
                        write_se(&mut writer, 0);
                    }
                }
            }
            write_ue(&mut writer, 0);
        }
        finish_rbsp(&mut writer);
        [vec![0x01], writer.into_bytes()].concat()
    }

    fn p_skip_field_slice(bottom: bool, poc_lsb: u64) -> Vec<u8> {
        p_skip_field_slice_at(0, bottom, poc_lsb)
    }

    fn p_skip_field_slice_at(first_macroblock: u32, bottom: bool, poc_lsb: u64) -> Vec<u8> {
        let mut writer = BitWriter::new();
        write_ue(&mut writer, first_macroblock);
        write_ue(&mut writer, 0);
        write_ue(&mut writer, 0);
        writer.write_bits(1, 4).unwrap();
        writer.write_bit(true).unwrap();
        writer.write_bit(bottom).unwrap();
        writer.write_bits(poc_lsb, 4).unwrap();
        writer.write_bit(false).unwrap();
        writer.write_bit(false).unwrap();
        writer.write_bit(false).unwrap();
        write_se(&mut writer, 0);
        write_ue(&mut writer, 1);
        write_ue(&mut writer, 1);
        finish_rbsp(&mut writer);
        [vec![0x41], writer.into_bytes()].concat()
    }

    fn p_ipcm_field_slice(bottom: bool, poc_lsb: u64, samples: [u8; 3]) -> Vec<u8> {
        let mut writer = BitWriter::new();
        write_ue(&mut writer, 0);
        write_ue(&mut writer, 0);
        write_ue(&mut writer, 0);
        writer.write_bits(1, 4).unwrap();
        writer.write_bit(true).unwrap();
        writer.write_bit(bottom).unwrap();
        writer.write_bits(poc_lsb, 4).unwrap();
        writer.write_bit(false).unwrap();
        writer.write_bit(false).unwrap();
        writer.write_bit(false).unwrap();
        write_se(&mut writer, 0);
        write_ue(&mut writer, 1);
        write_ue(&mut writer, 0);
        write_ue(&mut writer, 30);
        writer.align_to_byte();
        for value in std::iter::repeat_n(samples[0], 256)
            .chain(std::iter::repeat_n(samples[1], 64))
            .chain(std::iter::repeat_n(samples[2], 64))
        {
            writer.write_bits(u64::from(value), 8).unwrap();
        }
        finish_rbsp(&mut writer);
        [vec![0x41], writer.into_bytes()].concat()
    }

    fn p_forget_previous_ipcm_field_slice(bottom: bool, poc_lsb: u64, samples: [u8; 3]) -> Vec<u8> {
        let mut writer = BitWriter::new();
        write_ue(&mut writer, 0);
        write_ue(&mut writer, 0);
        write_ue(&mut writer, 0);
        writer.write_bits(1, 4).unwrap();
        writer.write_bit(true).unwrap();
        writer.write_bit(bottom).unwrap();
        writer.write_bits(poc_lsb, 4).unwrap();
        writer.write_bit(false).unwrap();
        writer.write_bit(false).unwrap();
        writer.write_bit(true).unwrap();
        write_ue(&mut writer, 1);
        write_ue(&mut writer, 2);
        write_ue(&mut writer, 0);
        write_se(&mut writer, 0);
        write_ue(&mut writer, 1);
        write_ue(&mut writer, 0);
        write_ue(&mut writer, 30);
        writer.align_to_byte();
        for value in std::iter::repeat_n(samples[0], 256)
            .chain(std::iter::repeat_n(samples[1], 64))
            .chain(std::iter::repeat_n(samples[2], 64))
        {
            writer.write_bits(u64::from(value), 8).unwrap();
        }
        finish_rbsp(&mut writer);
        [vec![0x41], writer.into_bytes()].concat()
    }

    fn p_next_skip_field_slice(bottom: bool, poc_lsb: u64) -> Vec<u8> {
        let mut writer = BitWriter::new();
        write_ue(&mut writer, 0);
        write_ue(&mut writer, 0);
        write_ue(&mut writer, 0);
        writer.write_bits(2, 4).unwrap();
        writer.write_bit(true).unwrap();
        writer.write_bit(bottom).unwrap();
        writer.write_bits(poc_lsb, 4).unwrap();
        writer.write_bit(false).unwrap();
        writer.write_bit(false).unwrap();
        write_se(&mut writer, 0);
        write_ue(&mut writer, 1);
        write_ue(&mut writer, 1);
        finish_rbsp(&mut writer);
        [vec![0x01], writer.into_bytes()].concat()
    }

    fn p_reordered_skip_field_slice(bottom: bool, poc_lsb: u64) -> Vec<u8> {
        let mut writer = BitWriter::new();
        write_ue(&mut writer, 0);
        write_ue(&mut writer, 0);
        write_ue(&mut writer, 0);
        writer.write_bits(2, 4).unwrap();
        writer.write_bit(true).unwrap();
        writer.write_bit(bottom).unwrap();
        writer.write_bits(poc_lsb, 4).unwrap();
        writer.write_bit(false).unwrap();
        writer.write_bit(true).unwrap();
        write_ue(&mut writer, 0);
        write_ue(&mut writer, if bottom { 3 } else { 4 });
        write_ue(&mut writer, 3);
        write_se(&mut writer, 0);
        write_ue(&mut writer, 1);
        write_ue(&mut writer, 1);
        finish_rbsp(&mut writer);
        [vec![0x01], writer.into_bytes()].concat()
    }

    fn b_bi_field_slice(bottom: bool, poc_lsb: u64) -> Vec<u8> {
        b_bi_field_slice_at(0, bottom, poc_lsb)
    }

    fn b_bi_field_slice_at(first_macroblock: u32, bottom: bool, poc_lsb: u64) -> Vec<u8> {
        b_bi_field_slice_with_reordering(first_macroblock, bottom, poc_lsb, false)
    }

    fn b_bi_reordered_field_slice(bottom: bool, poc_lsb: u64) -> Vec<u8> {
        b_bi_field_slice_with_reordering(0, bottom, poc_lsb, true)
    }

    fn b_bi_field_slice_with_reordering(
        first_macroblock: u32,
        bottom: bool,
        poc_lsb: u64,
        reorder: bool,
    ) -> Vec<u8> {
        let mut writer = BitWriter::new();
        write_ue(&mut writer, first_macroblock);
        write_ue(&mut writer, 1);
        write_ue(&mut writer, 0);
        writer.write_bits(2, 4).unwrap();
        writer.write_bit(true).unwrap();
        writer.write_bit(bottom).unwrap();
        writer.write_bits(poc_lsb, 4).unwrap();
        writer.write_bit(true).unwrap();
        writer.write_bit(false).unwrap();
        for _ in 0..2 {
            writer.write_bit(reorder).unwrap();
            if reorder {
                write_ue(&mut writer, 0);
                write_ue(&mut writer, if bottom { 3 } else { 4 });
                write_ue(&mut writer, 3);
            }
        }
        write_se(&mut writer, 0);
        write_ue(&mut writer, 1);
        write_ue(&mut writer, 0);
        write_ue(&mut writer, 3);
        write_se(&mut writer, 0);
        write_se(&mut writer, 0);
        write_se(&mut writer, 0);
        write_se(&mut writer, 0);
        write_ue(&mut writer, 0);
        finish_rbsp(&mut writer);
        [vec![0x01], writer.into_bytes()].concat()
    }

    fn ipcm_slice_with_long_term(long_term_reference: bool) -> Vec<u8> {
        let mut writer = BitWriter::new();
        write_ue(&mut writer, 0);
        write_ue(&mut writer, 2);
        write_ue(&mut writer, 0);
        writer.write_bits(0, 4).unwrap();
        write_ue(&mut writer, 0);
        writer.write_bits(0, 4).unwrap();
        writer.write_bit(false).unwrap();
        writer.write_bit(long_term_reference).unwrap();
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

    fn ipcm_slice_type2() -> Vec<u8> {
        let mut writer = BitWriter::new();
        write_ue(&mut writer, 0);
        write_ue(&mut writer, 2);
        write_ue(&mut writer, 0);
        writer.write_bits(0, 4).unwrap();
        write_ue(&mut writer, 0);
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

    fn p_ipcm_slice_type2(frame_num: u64, samples: [u8; 3]) -> Vec<u8> {
        let mut writer = BitWriter::new();
        write_ue(&mut writer, 0);
        write_ue(&mut writer, 0);
        write_ue(&mut writer, 0);
        writer.write_bits(frame_num, 4).unwrap();
        writer.write_bit(false).unwrap();
        writer.write_bit(false).unwrap();
        writer.write_bit(false).unwrap();
        write_se(&mut writer, 0);
        write_ue(&mut writer, 1);
        write_ue(&mut writer, 0);
        write_ue(&mut writer, 30);
        writer.align_to_byte();
        for value in std::iter::repeat_n(samples[0], 256)
            .chain(std::iter::repeat_n(samples[1], 64))
            .chain(std::iter::repeat_n(samples[2], 64))
        {
            writer.write_bits(u64::from(value), 8).unwrap();
        }
        finish_rbsp(&mut writer);
        [vec![0x41], writer.into_bytes()].concat()
    }

    fn p_ipcm_slice(frame_num: u64, pic_order_cnt_lsb: u64, samples: [u8; 3]) -> Vec<u8> {
        let mut writer = BitWriter::new();
        write_ue(&mut writer, 0);
        write_ue(&mut writer, 0);
        write_ue(&mut writer, 0);
        writer.write_bits(frame_num, 4).unwrap();
        writer.write_bits(pic_order_cnt_lsb, 4).unwrap();
        writer.write_bit(false).unwrap();
        writer.write_bit(false).unwrap();
        writer.write_bit(false).unwrap();
        write_se(&mut writer, 0);
        write_ue(&mut writer, 1);
        write_ue(&mut writer, 0);
        write_ue(&mut writer, 30);
        writer.align_to_byte();
        for value in std::iter::repeat_n(samples[0], 256)
            .chain(std::iter::repeat_n(samples[1], 64))
            .chain(std::iter::repeat_n(samples[2], 64))
        {
            writer.write_bits(u64::from(value), 8).unwrap();
        }
        finish_rbsp(&mut writer);
        [vec![0x41], writer.into_bytes()].concat()
    }

    fn i_ipcm_slice(frame_num: u64, pic_order_cnt_lsb: u64, samples: [u8; 3]) -> Vec<u8> {
        i_ipcm_slice_at(0, frame_num, pic_order_cnt_lsb, samples)
    }

    fn i_ipcm_slice_at(
        first_macroblock: u32,
        frame_num: u64,
        pic_order_cnt_lsb: u64,
        samples: [u8; 3],
    ) -> Vec<u8> {
        let mut writer = BitWriter::new();
        write_ue(&mut writer, first_macroblock);
        write_ue(&mut writer, 2);
        write_ue(&mut writer, 0);
        writer.write_bits(frame_num, 4).unwrap();
        writer.write_bits(pic_order_cnt_lsb, 4).unwrap();
        writer.write_bit(false).unwrap();
        write_se(&mut writer, 0);
        write_ue(&mut writer, 1);
        write_ue(&mut writer, 25);
        writer.align_to_byte();
        for value in std::iter::repeat_n(samples[0], 256)
            .chain(std::iter::repeat_n(samples[1], 64))
            .chain(std::iter::repeat_n(samples[2], 64))
        {
            writer.write_bits(u64::from(value), 8).unwrap();
        }
        finish_rbsp(&mut writer);
        [vec![0x41], writer.into_bytes()].concat()
    }

    fn p_reordered_skip_slice(frame_num: u64, pic_order_cnt_lsb: u64) -> Vec<u8> {
        let mut writer = BitWriter::new();
        write_ue(&mut writer, 0);
        write_ue(&mut writer, 0);
        write_ue(&mut writer, 0);
        writer.write_bits(frame_num, 4).unwrap();
        writer.write_bits(pic_order_cnt_lsb, 4).unwrap();
        writer.write_bit(false).unwrap();
        writer.write_bit(true).unwrap();
        write_ue(&mut writer, 0);
        write_ue(&mut writer, 1);
        write_ue(&mut writer, 3);
        writer.write_bit(false).unwrap();
        write_se(&mut writer, 0);
        write_ue(&mut writer, 1);
        write_ue(&mut writer, 1);
        finish_rbsp(&mut writer);
        [vec![0x41], writer.into_bytes()].concat()
    }

    fn p_motion_slice(
        frame_num: u64,
        pic_order_cnt_lsb: u64,
        motion_x: i32,
        motion_y: i32,
    ) -> Vec<u8> {
        let mut writer = BitWriter::new();
        write_ue(&mut writer, 0);
        write_ue(&mut writer, 0);
        write_ue(&mut writer, 0);
        writer.write_bits(frame_num, 4).unwrap();
        writer.write_bits(pic_order_cnt_lsb, 4).unwrap();
        writer.write_bit(false).unwrap();
        writer.write_bit(false).unwrap();
        writer.write_bit(false).unwrap();
        write_se(&mut writer, 0);
        write_ue(&mut writer, 1);
        write_ue(&mut writer, 0);
        write_ue(&mut writer, 0);
        write_se(&mut writer, motion_x);
        write_se(&mut writer, motion_y);
        write_ue(&mut writer, 0);
        finish_rbsp(&mut writer);
        [vec![0x41], writer.into_bytes()].concat()
    }

    fn b_16x16_slice(pic_order_cnt_lsb: u64, macroblock_type: u32) -> Vec<u8> {
        let mut writer = BitWriter::new();
        write_ue(&mut writer, 0);
        write_ue(&mut writer, 1);
        write_ue(&mut writer, 0);
        writer.write_bits(2, 4).unwrap();
        writer.write_bits(pic_order_cnt_lsb, 4).unwrap();
        writer.write_bit(true).unwrap();
        writer.write_bit(false).unwrap();
        writer.write_bit(false).unwrap();
        writer.write_bit(false).unwrap();
        write_se(&mut writer, 0);
        write_ue(&mut writer, 1);
        write_ue(&mut writer, 0);
        write_ue(&mut writer, macroblock_type);
        if matches!(macroblock_type, 1 | 3) {
            write_se(&mut writer, 0);
            write_se(&mut writer, 0);
        }
        if matches!(macroblock_type, 2 | 3) {
            write_se(&mut writer, 0);
            write_se(&mut writer, 0);
        }
        write_ue(&mut writer, 0);
        finish_rbsp(&mut writer);
        [vec![0x01], writer.into_bytes()].concat()
    }

    fn b_16x16_slice_without_poc(macroblock_type: u32) -> Vec<u8> {
        let mut writer = BitWriter::new();
        write_ue(&mut writer, 0);
        write_ue(&mut writer, 1);
        write_ue(&mut writer, 0);
        writer.write_bits(2, 4).unwrap();
        writer.write_bit(true).unwrap();
        writer.write_bit(false).unwrap();
        writer.write_bit(false).unwrap();
        writer.write_bit(false).unwrap();
        write_se(&mut writer, 0);
        write_ue(&mut writer, 1);
        write_ue(&mut writer, 0);
        write_ue(&mut writer, macroblock_type);
        if matches!(macroblock_type, 1 | 3) {
            write_se(&mut writer, 0);
            write_se(&mut writer, 0);
        }
        if matches!(macroblock_type, 2 | 3) {
            write_se(&mut writer, 0);
            write_se(&mut writer, 0);
        }
        write_ue(&mut writer, 0);
        finish_rbsp(&mut writer);
        [vec![0x01], writer.into_bytes()].concat()
    }

    fn explicit_weighted_bi_slice(pic_order_cnt_lsb: u64) -> Vec<u8> {
        let mut writer = BitWriter::new();
        write_ue(&mut writer, 0);
        write_ue(&mut writer, 1);
        write_ue(&mut writer, 0);
        writer.write_bits(2, 4).unwrap();
        writer.write_bits(pic_order_cnt_lsb, 4).unwrap();
        writer.write_bit(true).unwrap();
        writer.write_bit(false).unwrap();
        writer.write_bit(false).unwrap();
        writer.write_bit(false).unwrap();
        write_ue(&mut writer, 1);
        write_ue(&mut writer, 1);
        writer.write_bit(true).unwrap();
        write_se(&mut writer, 1);
        write_se(&mut writer, 10);
        writer.write_bit(true).unwrap();
        write_se(&mut writer, 1);
        write_se(&mut writer, 4);
        write_se(&mut writer, 3);
        write_se(&mut writer, -2);
        writer.write_bit(true).unwrap();
        write_se(&mut writer, 3);
        write_se(&mut writer, -6);
        writer.write_bit(true).unwrap();
        write_se(&mut writer, 3);
        write_se(&mut writer, -4);
        write_se(&mut writer, 1);
        write_se(&mut writer, 2);
        write_se(&mut writer, 0);
        write_ue(&mut writer, 1);
        write_ue(&mut writer, 0);
        write_ue(&mut writer, 3);
        write_se(&mut writer, 0);
        write_se(&mut writer, 0);
        write_se(&mut writer, 0);
        write_se(&mut writer, 0);
        write_ue(&mut writer, 0);
        finish_rbsp(&mut writer);
        [vec![0x01], writer.into_bytes()].concat()
    }

    fn b_skip_slice(pic_order_cnt_lsb: u64) -> Vec<u8> {
        b_skip_slice_with_direct(pic_order_cnt_lsb, true)
    }

    fn b_skip_slice_with_direct(pic_order_cnt_lsb: u64, spatial_direct: bool) -> Vec<u8> {
        let mut writer = BitWriter::new();
        write_ue(&mut writer, 0);
        write_ue(&mut writer, 1);
        write_ue(&mut writer, 0);
        writer.write_bits(2, 4).unwrap();
        writer.write_bits(pic_order_cnt_lsb, 4).unwrap();
        writer.write_bit(spatial_direct).unwrap();
        writer.write_bit(false).unwrap();
        writer.write_bit(false).unwrap();
        writer.write_bit(false).unwrap();
        write_se(&mut writer, 0);
        write_ue(&mut writer, 1);
        write_ue(&mut writer, 1);
        finish_rbsp(&mut writer);
        [vec![0x01], writer.into_bytes()].concat()
    }

    fn b_two_partition_slice(pic_order_cnt_lsb: u64, macroblock_type: u32) -> Vec<u8> {
        assert!((4..=21).contains(&macroblock_type));
        let mut writer = BitWriter::new();
        write_ue(&mut writer, 0);
        write_ue(&mut writer, 1);
        write_ue(&mut writer, 0);
        writer.write_bits(2, 4).unwrap();
        writer.write_bits(pic_order_cnt_lsb, 4).unwrap();
        writer.write_bit(true).unwrap();
        writer.write_bit(false).unwrap();
        writer.write_bit(false).unwrap();
        writer.write_bit(false).unwrap();
        write_se(&mut writer, 0);
        write_ue(&mut writer, 1);
        write_ue(&mut writer, 0);
        write_ue(&mut writer, macroblock_type);
        let partitions = b_inter_partitions(macroblock_type);
        for (_, prediction) in &partitions {
            if prediction.uses_l0() {
                write_se(&mut writer, 0);
                write_se(&mut writer, 0);
            }
        }
        for (_, prediction) in &partitions {
            if prediction.uses_l1() {
                write_se(&mut writer, 0);
                write_se(&mut writer, 0);
            }
        }
        write_ue(&mut writer, 0);
        finish_rbsp(&mut writer);
        [vec![0x01], writer.into_bytes()].concat()
    }

    fn b8x8_slice(
        pic_order_cnt_lsb: u64,
        sub_types: [u32; 4],
        direct_8x8_inference: bool,
    ) -> Vec<u8> {
        let sub_macroblocks =
            sub_types.map(|sub_type| b_sub_macroblock(0, sub_type, direct_8x8_inference).unwrap());
        let mut writer = BitWriter::new();
        write_ue(&mut writer, 0);
        write_ue(&mut writer, 1);
        write_ue(&mut writer, 0);
        writer.write_bits(2, 4).unwrap();
        writer.write_bits(pic_order_cnt_lsb, 4).unwrap();
        writer.write_bit(true).unwrap();
        writer.write_bit(false).unwrap();
        writer.write_bit(false).unwrap();
        writer.write_bit(false).unwrap();
        write_se(&mut writer, 0);
        write_ue(&mut writer, 1);
        write_ue(&mut writer, 0);
        write_ue(&mut writer, 22);
        for sub_type in sub_types {
            write_ue(&mut writer, sub_type);
        }
        for sub in &sub_macroblocks {
            if sub.prediction.uses_l0() {
                for _ in &sub.partitions {
                    write_se(&mut writer, 0);
                    write_se(&mut writer, 0);
                }
            }
        }
        for sub in &sub_macroblocks {
            if sub.prediction.uses_l1() {
                for _ in &sub.partitions {
                    write_se(&mut writer, 0);
                    write_se(&mut writer, 0);
                }
            }
        }
        write_ue(&mut writer, 0);
        finish_rbsp(&mut writer);
        [vec![0x01], writer.into_bytes()].concat()
    }

    fn b8x8_bi_4x4_motion_slice(pic_order_cnt_lsb: u64) -> Vec<u8> {
        let mut writer = BitWriter::new();
        write_ue(&mut writer, 0);
        write_ue(&mut writer, 1);
        write_ue(&mut writer, 0);
        writer.write_bits(2, 4).unwrap();
        writer.write_bits(pic_order_cnt_lsb, 4).unwrap();
        writer.write_bit(true).unwrap();
        writer.write_bit(false).unwrap();
        writer.write_bit(false).unwrap();
        writer.write_bit(false).unwrap();
        write_se(&mut writer, 0);
        write_ue(&mut writer, 1);
        write_ue(&mut writer, 0);
        write_ue(&mut writer, 22);
        for _ in 0..4 {
            write_ue(&mut writer, 12);
        }
        for list in 0..2_usize {
            for partition in 0..16_usize {
                let horizontal = if (partition + list).is_multiple_of(2) {
                    4
                } else {
                    -4
                };
                let vertical = if (partition / 2 + list).is_multiple_of(2) {
                    2
                } else {
                    -2
                };
                write_se(&mut writer, horizontal);
                write_se(&mut writer, vertical);
            }
        }
        write_ue(&mut writer, 0);
        finish_rbsp(&mut writer);
        [vec![0x01], writer.into_bytes()].concat()
    }

    fn patterned_ipcm_slice(
        is_idr: bool,
        frame_num: u64,
        pic_order_cnt_lsb: u64,
        phase: u8,
    ) -> Vec<u8> {
        let mut writer = BitWriter::new();
        write_ue(&mut writer, 0);
        write_ue(&mut writer, if is_idr { 2 } else { 0 });
        write_ue(&mut writer, 0);
        writer.write_bits(frame_num, 4).unwrap();
        if is_idr {
            write_ue(&mut writer, 0);
        }
        writer.write_bits(pic_order_cnt_lsb, 4).unwrap();
        if is_idr {
            writer.write_bit(false).unwrap();
            writer.write_bit(false).unwrap();
        } else {
            writer.write_bit(false).unwrap();
            writer.write_bit(false).unwrap();
            writer.write_bit(false).unwrap();
        }
        write_se(&mut writer, 0);
        write_ue(&mut writer, 1);
        write_ue(&mut writer, if is_idr { 25 } else { 0 });
        if !is_idr {
            write_ue(&mut writer, 30);
        }
        writer.align_to_byte();
        for y in 0..16_u16 {
            for x in 0..16_u16 {
                writer
                    .write_bits(
                        u64::from(((x * 11 + y * 7 + u16::from(phase)) & 255) as u8),
                        8,
                    )
                    .unwrap();
            }
        }
        for component in 0..2_u16 {
            for y in 0..8_u16 {
                for x in 0..8_u16 {
                    writer
                        .write_bits(
                            u64::from(
                                ((x * 17 + y * 9 + component * 43 + u16::from(phase)) & 255) as u8,
                            ),
                            8,
                        )
                        .unwrap();
                }
            }
        }
        finish_rbsp(&mut writer);
        [vec![if is_idr { 0x65 } else { 0x41 }], writer.into_bytes()].concat()
    }

    fn p_long_term_ipcm_slice(frame_num: u64, pic_order_cnt_lsb: u64, samples: [u8; 3]) -> Vec<u8> {
        let mut writer = BitWriter::new();
        write_ue(&mut writer, 0);
        write_ue(&mut writer, 0);
        write_ue(&mut writer, 0);
        writer.write_bits(frame_num, 4).unwrap();
        writer.write_bits(pic_order_cnt_lsb, 4).unwrap();
        writer.write_bit(false).unwrap();
        writer.write_bit(false).unwrap();
        writer.write_bit(true).unwrap();
        write_ue(&mut writer, 4);
        write_ue(&mut writer, 2);
        write_ue(&mut writer, 6);
        write_ue(&mut writer, 1);
        write_ue(&mut writer, 0);
        write_se(&mut writer, 0);
        write_ue(&mut writer, 1);
        write_ue(&mut writer, 0);
        write_ue(&mut writer, 30);
        writer.align_to_byte();
        for value in std::iter::repeat_n(samples[0], 256)
            .chain(std::iter::repeat_n(samples[1], 64))
            .chain(std::iter::repeat_n(samples[2], 64))
        {
            writer.write_bits(u64::from(value), 8).unwrap();
        }
        finish_rbsp(&mut writer);
        [vec![0x41], writer.into_bytes()].concat()
    }

    fn p_long_term_reordered_skip_slice(frame_num: u64, pic_order_cnt_lsb: u64) -> Vec<u8> {
        let mut writer = BitWriter::new();
        write_ue(&mut writer, 0);
        write_ue(&mut writer, 0);
        write_ue(&mut writer, 0);
        writer.write_bits(frame_num, 4).unwrap();
        writer.write_bits(pic_order_cnt_lsb, 4).unwrap();
        writer.write_bit(false).unwrap();
        writer.write_bit(true).unwrap();
        write_ue(&mut writer, 2);
        write_ue(&mut writer, 1);
        write_ue(&mut writer, 3);
        writer.write_bit(true).unwrap();
        write_ue(&mut writer, 2);
        write_ue(&mut writer, 0);
        write_ue(&mut writer, 0);
        write_se(&mut writer, 0);
        write_ue(&mut writer, 1);
        write_ue(&mut writer, 1);
        finish_rbsp(&mut writer);
        [vec![0x41], writer.into_bytes()].concat()
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
        decode_sequence_with_ffmpeg(sps, pps, &[slice], 1)
    }

    fn decode_sequence_with_ffmpeg(
        sps: &[u8],
        pps: &[u8],
        slices: &[&[u8]],
        frame_count: usize,
    ) -> Option<Vec<u8>> {
        if Command::new("ffmpeg").arg("-version").output().is_err() {
            return None;
        }
        let mut annex_b = Vec::new();
        for nal in [sps, pps] {
            annex_b.extend([0, 0, 0, 1]);
            annex_b.extend(nal);
        }
        for nal in slices {
            annex_b.extend([0, 0, 0, 1]);
            annex_b.extend(*nal);
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
            ])
            .arg(frame_count.to_string())
            .args(["-pix_fmt", "yuv420p", "-f", "rawvideo", "pipe:1"])
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
