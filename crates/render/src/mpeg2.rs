//! Optional native MPEG-2 bridge execution.

use std::collections::{BTreeMap, BTreeSet};

use mmrecode_core::{
    Error, Packet, PacketFlags, RandomAccessKind, Rational, Result, StreamId, Timestamp,
    TimestampRounding, VideoFrame,
};
use mmrecode_edit::{SourceId, TimeRange};
use mmrecode_mpeg2::{
    FrameRate, Mpeg2EncodeOptions, Mpeg2QuantMatrices, Mpeg2SequenceSettings, Mpeg2Stream,
    PictureType, SequenceParameters, analyze_dependencies, decode_stream, encode_stream,
    parse_stream,
};

use crate::{PacketSource, RenderOperation, RenderPlan};

/// Parses and dependency-indexes one complete MPEG-2 elementary stream for rendering.
///
/// Packets are emitted in codec decode order. Each packet owns the bytes from the end of the
/// preceding picture packet through the end of its picture, so concatenating packet payloads
/// reproduces the original elementary stream exactly.
///
/// # Errors
///
/// Returns an error when MPEG-2 parsing or dependency analysis fails, picture ranges are
/// inconsistent, or the stream contains no pictures.
pub fn analyze_mpeg2_source(
    data: &[u8],
    source_id: SourceId,
    stream_id: StreamId,
) -> Result<PacketSource> {
    let stream = parse_stream(data)?;
    let dependencies = analyze_dependencies(&stream)?;
    let first = stream
        .pictures()
        .first()
        .ok_or_else(|| Error::InvalidData("MPEG-2 stream contains no pictures".into()))?;
    let frame_rate = first.sequence.frame_rate;
    let time_base = Rational::new(frame_rate.denominator(), frame_rate.numerator())?;
    let mut packets = Vec::with_capacity(stream.pictures().len());
    for (index, (picture, access_unit)) in stream.pictures().iter().zip(dependencies).enumerate() {
        let start = if index == 0 {
            0
        } else {
            stream.pictures()[index - 1].source_range.end
        };
        let end = if index + 1 == stream.pictures().len() {
            data.len()
        } else {
            picture.source_range.end
        };
        let packet_data = data.get(start..end).ok_or_else(|| {
            Error::InvalidData(format!(
                "MPEG-2 picture packet {index} has invalid byte range {start}..{end}"
            ))
        })?;
        let mut flags = PacketFlags::empty();
        if access_unit.random_access == RandomAccessKind::Clean {
            flags.insert(PacketFlags::KEY);
        }
        packets.push(crate::AnalyzedPacket {
            packet: Packet {
                stream_id,
                data: packet_data.to_vec(),
                pts: Some(Timestamp {
                    value: access_unit.presentation_order,
                    time_base,
                }),
                dts: Some(Timestamp {
                    value: access_unit.decode_order,
                    time_base,
                }),
                duration: Some(Timestamp {
                    value: 1,
                    time_base,
                }),
                flags,
                side_data: Vec::new(),
            },
            access_unit,
        });
    }
    Ok(PacketSource {
        source_id,
        stream_id,
        packets,
    })
}

/// Quality and search controls for the reference MPEG-2 bridge encoder.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Mpeg2BridgeOptions {
    /// Linear quantizer-scale code used for regenerated pictures.
    pub quantiser_scale_code: u8,
    /// Integer-pixel P-picture motion-search radius.
    pub motion_search_range: usize,
}

impl Default for Mpeg2BridgeOptions {
    fn default() -> Self {
        Self {
            quantiser_scale_code: 8,
            motion_search_range: 4,
        }
    }
}

/// How bridge encoding handles one class of source metadata.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum Mpeg2SpliceAction {
    /// The encoded bridge carries the same semantic value as the source.
    Preserved,
    /// The source did not signal this optional metadata.
    Absent,
    /// A value is regenerated from the source origin and output timeline.
    Recomputed,
    /// A source value is replaced because the reference encoder cannot preserve it honestly.
    Rewritten,
}

/// Explainable MPEG-2 metadata policy applied to regenerated bridge GOPs.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Mpeg2SpliceReport {
    /// Number of separately regenerated closed-GOP runs.
    pub regenerated_runs: usize,
    /// Aspect-ratio information handling.
    pub aspect_ratio: Mpeg2SpliceAction,
    /// Sequence-display and colour-description handling.
    pub display_metadata: Mpeg2SpliceAction,
    /// Luma and chroma quantizer-matrix handling.
    pub quantizer_matrices: Mpeg2SpliceAction,
    /// Profile-and-level indication handling.
    pub profile_and_level: Mpeg2SpliceAction,
    /// Declared sequence bitrate handling.
    pub bit_rate: Mpeg2SpliceAction,
    /// Source declared bitrate, when present.
    pub source_bit_rate: Option<u64>,
    /// Bitrate declared by regenerated sequence headers.
    pub encoded_bit_rate: u64,
    /// Sequence VBV-buffer-size handling.
    pub vbv_buffer_size: Mpeg2SpliceAction,
    /// Source VBV buffer size in bits.
    pub source_vbv_buffer_size_bits: u64,
    /// VBV buffer size declared by regenerated sequence headers.
    pub encoded_vbv_buffer_size_bits: u64,
    /// GOP timecodes are regenerated as closed-GOP timeline positions.
    pub gop_timecode: Mpeg2SpliceAction,
    /// Picture VBV delay handling.
    pub picture_vbv_delay: Mpeg2SpliceAction,
}

impl Mpeg2SpliceReport {
    /// Returns a compact multi-line explanation for render inspection tools.
    #[must_use]
    pub fn explanation(&self) -> String {
        format!(
            "MPEG-2 splice metadata: aspect {:?}, display {:?}, matrices {:?}, profile/level {:?}\nRate signalling: bitrate {:?} ({:?} -> {} bit/s), VBV {:?} ({} -> {} bits)\nGOP policy: timecode {:?} from source origin; closed GOP; picture vbv_delay {:?} to 0xffff",
            self.aspect_ratio,
            self.display_metadata,
            self.quantizer_matrices,
            self.profile_and_level,
            self.bit_rate,
            self.source_bit_rate,
            self.encoded_bit_rate,
            self.vbv_buffer_size,
            self.source_vbv_buffer_size_bits,
            self.encoded_vbv_buffer_size_bits,
            self.gop_timecode,
            self.picture_vbv_delay,
        )
    }
}

/// Executed MPEG-2 packets plus splice-metadata accounting.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Mpeg2RenderOutput {
    /// Complete output packets in codec decode order.
    pub packets: Vec<Packet>,
    /// Metadata policy applied to regenerated runs.
    pub splice: Mpeg2SpliceReport,
}

/// One compositor-produced frame replacing a directly changed timeline picture.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Mpeg2FrameReplacement {
    /// Exact output-timeline PTS of the replacement.
    pub timeline_pts: Timestamp,
    /// Replacement pixels to encode.
    pub frame: VideoFrame,
}

#[derive(Clone, Copy, Debug)]
struct OutputTiming {
    pts: Timestamp,
    dts: Timestamp,
    duration: Timestamp,
}

/// Executes a generic inter-frame plan with the native MPEG-2 reference encoder.
///
/// Copied packets retain their encoded payload, flags, and side data. Each bridge/full-encode run
/// is reconstructed from the original elementary stream, patched with frame replacements for
/// every `ApplyEffects` interval, encoded as a closed GOP, and placed into its reserved packet
/// slots. The resulting elementary stream is parsed, dependency-checked, and decoded before it is
/// returned.
///
/// The executor accepts frame-aligned ranges from one or more compatible fixed-frame-rate MPEG-2
/// packet sources. It intentionally emits a fresh sequence header at each regenerated run and
/// reports which metadata was preserved, recomputed, or rewritten. The first packet source
/// establishes regenerated sequence metadata and the bridge timecode origin; copied GOP headers
/// remain byte-preserved.
///
/// # Errors
///
/// Returns an error when the plan is malformed, required replacements are absent, input packets do
/// not reassemble into the analyzed MPEG-2 stream, encoder parameters are unsupported, regenerated
/// packet counts differ from reserved slots, or final splice validation fails.
#[allow(clippy::too_many_lines)]
pub fn execute_mpeg2_plan(
    plan: &RenderPlan,
    sources: &[PacketSource],
    replacements: &[Mpeg2FrameReplacement],
    options: Mpeg2BridgeOptions,
) -> Result<Vec<Packet>> {
    execute_mpeg2_plan_with_report(plan, sources, replacements, options)
        .map(|output| output.packets)
}

/// Executes a generic inter-frame plan and returns MPEG-2 splice-metadata accounting.
///
/// This is the reporting form of [`execute_mpeg2_plan`]. It uses the same bounded native bridge
/// executor while exposing which sequence/GOP fields were preserved, recomputed, or rewritten.
///
/// # Errors
///
/// Returns the same structural, replacement, encoder, and splice-validation errors as
/// [`execute_mpeg2_plan`].
#[allow(clippy::too_many_lines)]
pub fn execute_mpeg2_plan_with_report(
    plan: &RenderPlan,
    sources: &[PacketSource],
    replacements: &[Mpeg2FrameReplacement],
    options: Mpeg2BridgeOptions,
) -> Result<Mpeg2RenderOutput> {
    if sources.is_empty() {
        return Err(Error::InvalidData(
            "MPEG-2 bridge execution requires at least one packet source".into(),
        ));
    }
    let source_keys = plan_source_keys(plan)?;
    let render_sources = source_keys
        .iter()
        .map(|&(source_id, stream_id)| locate_packet_source(sources, source_id, stream_id))
        .collect::<Result<Vec<_>>>()?;
    let output_count = plan
        .summary
        .copied_packets
        .checked_add(plan.summary.encoded_frames)
        .ok_or_else(|| Error::InvalidData("MPEG-2 output picture count overflows".into()))?;
    if output_count == 0 {
        return Err(Error::InvalidData(
            "MPEG-2 render plan contains no output pictures".into(),
        ));
    }
    let output_time_base = plan_output_time_base(plan)?;
    let source_bytes = render_sources
        .iter()
        .map(|source| {
            source
                .packets
                .iter()
                .flat_map(|analyzed| analyzed.packet.data.iter().copied())
                .collect::<Vec<_>>()
        })
        .collect::<Vec<_>>();
    let source_streams = source_bytes
        .iter()
        .map(|bytes| parse_stream(bytes))
        .collect::<Result<Vec<_>>>()?;
    for (source, stream) in render_sources.iter().zip(&source_streams) {
        validate_source_analysis(source, stream)?;
    }
    validate_source_compatibility(&source_streams)?;
    let canonical_stream = &source_streams[0];
    let (sequence_settings, mut splice_report) = bridge_sequence_settings(canonical_stream)?;
    let decoded_by_source = source_bytes
        .iter()
        .map(|bytes| {
            Ok(decode_stream(bytes)?
                .into_iter()
                .map(|picture| (picture.presentation_order, picture.frame))
                .collect::<BTreeMap<_, _>>())
        })
        .collect::<Result<Vec<_>>>()?;
    let replacement_map = replacement_map(replacements, output_time_base)?;
    let effect_ranges = effect_ranges(plan);
    let mut used_replacements = BTreeSet::new();
    let mut regenerated_slots = BTreeSet::new();
    let mut output: Vec<Option<Packet>> = vec![None; output_count];
    let mut timing: Vec<Option<OutputTiming>> = vec![None; output_count];
    let mut reached_mux = false;

    for operation in &plan.operations {
        if reached_mux {
            return Err(Error::InvalidState(
                "MPEG-2 render plan contains operations after mux".into(),
            ));
        }
        match operation {
            RenderOperation::CopyPackets {
                source_id,
                source_stream_id,
                source_packet_range,
                output_packet_range,
                output_stream_id,
            } => {
                let source = locate_packet_source(sources, *source_id, *source_stream_id)?;
                copy_packets(
                    source,
                    *source_id,
                    *source_stream_id,
                    source_packet_range.clone(),
                    output_packet_range.clone(),
                    *output_stream_id,
                    &mut output,
                )?;
            }
            RenderOperation::RewriteTimestamps {
                output_packet_range,
                source_start,
                output_start,
                output_time_base: operation_time_base,
            } => {
                if *operation_time_base != output_time_base {
                    return Err(Error::InvalidState(
                        "MPEG-2 plan contains inconsistent output time bases".into(),
                    ));
                }
                ensure_filled(&output, output_packet_range.clone(), "timestamp rewrite")?;
                assign_copy_timing(
                    &output,
                    &mut timing,
                    output_packet_range.clone(),
                    *source_start,
                    *output_start,
                    output_time_base,
                )?;
            }
            RenderOperation::Decode { .. } | RenderOperation::ApplyEffects { .. } => {}
            RenderOperation::BridgeEncode {
                source_id,
                source_stream_id,
                source_packet_indices,
                timeline_range,
                output_packet_range,
                output_stream_id,
                output_time_base: operation_time_base,
                ..
            }
            | RenderOperation::FullEncode {
                source_id,
                source_stream_id,
                source_packet_indices,
                timeline_range,
                output_packet_range,
                output_stream_id,
                output_time_base: operation_time_base,
                ..
            } => {
                if *operation_time_base != output_time_base {
                    return Err(Error::InvalidState(
                        "MPEG-2 plan contains inconsistent regeneration time bases".into(),
                    ));
                }
                let source_index =
                    locate_source_key_index(&source_keys, *source_id, *source_stream_id)?;
                let source = render_sources[source_index];
                assign_regeneration_timing(
                    source,
                    source_packet_indices,
                    output_packet_range.clone(),
                    *timeline_range,
                    output_time_base,
                    &mut timing,
                )?;
                regenerate_packets(
                    source,
                    &source_streams[source_index],
                    &decoded_by_source[source_index],
                    &replacement_map,
                    &effect_ranges,
                    &mut used_replacements,
                    source_packet_indices,
                    output_packet_range.clone(),
                    *output_stream_id,
                    &timing,
                    options,
                    sequence_settings,
                    &mut output,
                )?;
                splice_report.regenerated_runs = splice_report
                    .regenerated_runs
                    .checked_add(1)
                    .ok_or_else(|| {
                        Error::InvalidData("MPEG-2 bridge run count overflows".into())
                    })?;
                regenerated_slots.extend(output_packet_range.clone());
            }
            RenderOperation::Mux => reached_mux = true,
        }
    }
    if !reached_mux {
        return Err(Error::InvalidState(
            "MPEG-2 render plan does not terminate in mux".into(),
        ));
    }
    if used_replacements.len() != replacement_map.len() {
        return Err(Error::InvalidData(
            "one or more MPEG-2 replacement frames do not belong to a changed picture".into(),
        ));
    }
    let timing = finalize_output_timing(timing, output_time_base)?;
    let packets = output
        .into_iter()
        .enumerate()
        .map(|(index, packet)| {
            let mut packet = packet.ok_or_else(|| {
                Error::InvalidState(format!("MPEG-2 output packet slot {index} was not filled"))
            })?;
            if !regenerated_slots.contains(&index) {
                packet.pts = Some(timing[index].pts);
                packet.duration = Some(timing[index].duration);
            }
            packet.dts = Some(timing[index].dts);
            Ok(packet)
        })
        .collect::<Result<Vec<_>>>()?;
    validate_splice(&packets, canonical_stream)?;
    Ok(Mpeg2RenderOutput {
        packets,
        splice: splice_report,
    })
}

fn plan_output_time_base(plan: &RenderPlan) -> Result<Rational> {
    let mut found = None;
    for operation in &plan.operations {
        let candidate = match operation {
            RenderOperation::RewriteTimestamps {
                output_time_base, ..
            }
            | RenderOperation::BridgeEncode {
                output_time_base, ..
            }
            | RenderOperation::FullEncode {
                output_time_base, ..
            } => Some(*output_time_base),
            _ => None,
        };
        if let Some(candidate) = candidate {
            if found.is_some_and(|existing| existing != candidate) {
                return Err(Error::InvalidState(
                    "render plan contains multiple output time bases".into(),
                ));
            }
            found = Some(candidate);
        }
    }
    found.ok_or_else(|| Error::InvalidState("render plan has no timestamp operation".into()))
}

fn plan_source_keys(plan: &RenderPlan) -> Result<Vec<(SourceId, StreamId)>> {
    let mut keys = Vec::new();
    for operation in &plan.operations {
        let key = match operation {
            RenderOperation::CopyPackets {
                source_id,
                source_stream_id,
                ..
            }
            | RenderOperation::BridgeEncode {
                source_id,
                source_stream_id,
                ..
            }
            | RenderOperation::FullEncode {
                source_id,
                source_stream_id,
                ..
            } => Some((*source_id, *source_stream_id)),
            _ => None,
        };
        if let Some(key) = key
            && !keys.contains(&key)
        {
            keys.push(key);
        }
    }
    if keys.is_empty() {
        return Err(Error::InvalidState(
            "MPEG-2 render plan references no packet sources".into(),
        ));
    }
    Ok(keys)
}

fn locate_packet_source(
    sources: &[PacketSource],
    source_id: SourceId,
    stream_id: StreamId,
) -> Result<&PacketSource> {
    let index = locate_packet_source_index(sources, source_id, stream_id)?;
    Ok(&sources[index])
}

fn locate_packet_source_index(
    sources: &[PacketSource],
    source_id: SourceId,
    stream_id: StreamId,
) -> Result<usize> {
    sources
        .iter()
        .position(|source| source.source_id == source_id && source.stream_id == stream_id)
        .ok_or_else(|| {
            Error::InvalidData(format!(
                "MPEG-2 source {source_id:?} stream {stream_id:?} is unavailable"
            ))
        })
}

fn locate_source_key_index(
    keys: &[(SourceId, StreamId)],
    source_id: SourceId,
    stream_id: StreamId,
) -> Result<usize> {
    keys.iter()
        .position(|key| *key == (source_id, stream_id))
        .ok_or_else(|| {
            Error::InvalidState(format!(
                "MPEG-2 plan source {source_id:?} stream {stream_id:?} was not indexed"
            ))
        })
}

fn assign_copy_timing(
    output: &[Option<Packet>],
    timing: &mut [Option<OutputTiming>],
    output_range: std::ops::Range<usize>,
    source_start: Timestamp,
    output_start: Timestamp,
    output_time_base: Rational,
) -> Result<()> {
    let packets = output
        .get(output_range.clone())
        .ok_or_else(|| Error::InvalidState("MPEG-2 copy timing range is out of bounds".into()))?;
    let slots = timing
        .get_mut(output_range)
        .ok_or_else(|| Error::InvalidState("MPEG-2 copy timing slots are out of bounds".into()))?;
    for (slot, packet) in slots.iter_mut().zip(packets) {
        if slot.is_some() {
            return Err(Error::InvalidState(
                "MPEG-2 output timing slot is written more than once".into(),
            ));
        }
        let packet = packet.as_ref().ok_or_else(|| {
            Error::InvalidState("MPEG-2 copy timing references an empty packet slot".into())
        })?;
        let pts = packet
            .pts
            .ok_or_else(|| Error::InvalidData("MPEG-2 packet has no PTS".into()))?;
        let duration = packet
            .duration
            .ok_or_else(|| Error::InvalidData("MPEG-2 packet has no duration".into()))?
            .rescale(output_time_base, TimestampRounding::Exact)?;
        if duration.value <= 0 {
            return Err(Error::InvalidData(
                "MPEG-2 packet duration must be positive".into(),
            ));
        }
        *slot = Some(OutputTiming {
            pts: crate::rewrite_timestamp(pts, source_start, output_start)?,
            dts: Timestamp {
                value: 0,
                time_base: output_time_base,
            },
            duration,
        });
    }
    Ok(())
}

fn assign_regeneration_timing(
    source: &PacketSource,
    source_packet_indices: &[usize],
    output_range: std::ops::Range<usize>,
    timeline_range: TimeRange,
    output_time_base: Rational,
    timing: &mut [Option<OutputTiming>],
) -> Result<()> {
    if source_packet_indices.len() != output_range.len() || source_packet_indices.is_empty() {
        return Err(Error::InvalidState(
            "MPEG-2 regeneration packet mapping has the wrong length".into(),
        ));
    }
    let output_start = timeline_range
        .start
        .rescale(output_time_base, TimestampRounding::Exact)?;
    let output_end = timeline_range
        .end
        .rescale(output_time_base, TimestampRounding::Exact)?;
    let mut source_timings = Vec::with_capacity(source_packet_indices.len());
    for &source_index in source_packet_indices {
        let packet = &source
            .packets
            .get(source_index)
            .ok_or_else(|| {
                Error::InvalidData("MPEG-2 regeneration source index is out of bounds".into())
            })?
            .packet;
        let pts = packet
            .pts
            .ok_or_else(|| Error::InvalidData("MPEG-2 packet has no PTS".into()))?
            .rescale(output_time_base, TimestampRounding::Exact)?;
        let duration = packet
            .duration
            .ok_or_else(|| Error::InvalidData("MPEG-2 packet has no duration".into()))?
            .rescale(output_time_base, TimestampRounding::Exact)?;
        if duration.value <= 0 {
            return Err(Error::InvalidData(
                "MPEG-2 packet duration must be positive".into(),
            ));
        }
        source_timings.push((pts, duration));
    }
    let source_anchor = source_timings
        .iter()
        .map(|(pts, _)| pts.value)
        .min()
        .ok_or_else(|| Error::InvalidState("MPEG-2 regeneration has no source PTS".into()))?;
    let slots = timing
        .get_mut(output_range)
        .ok_or_else(|| Error::InvalidState("MPEG-2 regeneration timing is out of bounds".into()))?;
    let mut presentation_ranges = Vec::with_capacity(slots.len());
    for (slot, (source_pts, duration)) in slots.iter_mut().zip(source_timings) {
        if slot.is_some() {
            return Err(Error::InvalidState(
                "MPEG-2 output timing slot is written more than once".into(),
            ));
        }
        let delta = source_pts
            .value
            .checked_sub(source_anchor)
            .ok_or_else(|| Error::InvalidData("MPEG-2 source PTS offset overflows".into()))?;
        let pts_value = output_start
            .value
            .checked_add(delta)
            .ok_or_else(|| Error::InvalidData("MPEG-2 output PTS overflows".into()))?;
        let end_value = pts_value
            .checked_add(duration.value)
            .ok_or_else(|| Error::InvalidData("MPEG-2 output PTS end overflows".into()))?;
        presentation_ranges.push((pts_value, end_value));
        *slot = Some(OutputTiming {
            pts: Timestamp {
                value: pts_value,
                time_base: output_time_base,
            },
            dts: Timestamp {
                value: 0,
                time_base: output_time_base,
            },
            duration,
        });
    }
    presentation_ranges.sort_unstable();
    if presentation_ranges.first().map(|range| range.0) != Some(output_start.value)
        || presentation_ranges.last().map(|range| range.1) != Some(output_end.value)
        || presentation_ranges
            .windows(2)
            .any(|window| window[0].1 != window[1].0)
    {
        return Err(Error::InvalidState(
            "MPEG-2 regeneration timing does not cover its timeline range".into(),
        ));
    }
    Ok(())
}

fn finalize_output_timing(
    timing: Vec<Option<OutputTiming>>,
    output_time_base: Rational,
) -> Result<Vec<OutputTiming>> {
    let mut decode_cursor = 0_i64;
    timing
        .into_iter()
        .enumerate()
        .map(|(index, timing)| {
            let mut timing = timing.ok_or_else(|| {
                Error::InvalidState(format!("MPEG-2 output timing slot {index} was not filled"))
            })?;
            timing.dts = Timestamp {
                value: decode_cursor,
                time_base: output_time_base,
            };
            decode_cursor = decode_cursor
                .checked_add(timing.duration.value)
                .ok_or_else(|| Error::InvalidData("MPEG-2 output DTS overflows".into()))?;
            Ok(timing)
        })
        .collect()
}

fn replacement_map(
    replacements: &[Mpeg2FrameReplacement],
    output_time_base: Rational,
) -> Result<BTreeMap<i64, &VideoFrame>> {
    let mut map = BTreeMap::new();
    for replacement in replacements {
        let pts = replacement
            .timeline_pts
            .rescale(output_time_base, TimestampRounding::Exact)?;
        if map.insert(pts.value, &replacement.frame).is_some() {
            return Err(Error::InvalidData(format!(
                "duplicate MPEG-2 replacement at output PTS {}",
                pts.value
            )));
        }
    }
    Ok(map)
}

fn effect_ranges(plan: &RenderPlan) -> Vec<TimeRange> {
    plan.operations
        .iter()
        .filter_map(|operation| match operation {
            RenderOperation::ApplyEffects { timeline_range, .. } => Some(*timeline_range),
            _ => None,
        })
        .collect()
}

#[allow(clippy::too_many_arguments)]
fn copy_packets(
    source: &PacketSource,
    source_id: mmrecode_edit::SourceId,
    source_stream_id: StreamId,
    source_range: std::ops::Range<usize>,
    output_range: std::ops::Range<usize>,
    output_stream_id: StreamId,
    output: &mut [Option<Packet>],
) -> Result<()> {
    if source.source_id != source_id || source.stream_id != source_stream_id {
        return Err(Error::InvalidData(
            "MPEG-2 copy operation references a different packet source".into(),
        ));
    }
    if source_range.len() != output_range.len() {
        return Err(Error::InvalidState(
            "MPEG-2 copy operation changes the packet count".into(),
        ));
    }
    let packets = source
        .packets
        .get(source_range)
        .ok_or_else(|| Error::InvalidData("MPEG-2 copy range is out of bounds".into()))?;
    let slots = output
        .get_mut(output_range)
        .ok_or_else(|| Error::InvalidState("MPEG-2 copy output range is out of bounds".into()))?;
    for (slot, analyzed) in slots.iter_mut().zip(packets) {
        if slot.is_some() {
            return Err(Error::InvalidState(
                "MPEG-2 output packet slot is written more than once".into(),
            ));
        }
        let mut packet = analyzed.packet.clone();
        packet.stream_id = output_stream_id;
        *slot = Some(packet);
    }
    Ok(())
}

fn ensure_filled(
    output: &[Option<Packet>],
    range: std::ops::Range<usize>,
    label: &str,
) -> Result<()> {
    let slots = output
        .get(range)
        .ok_or_else(|| Error::InvalidState(format!("{label} range is out of bounds")))?;
    if slots.iter().any(Option::is_none) {
        return Err(Error::InvalidState(format!(
            "{label} references an unfilled packet slot"
        )));
    }
    Ok(())
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn regenerate_packets(
    source: &PacketSource,
    source_stream: &Mpeg2Stream<'_>,
    decoded: &BTreeMap<i64, VideoFrame>,
    replacements: &BTreeMap<i64, &VideoFrame>,
    effect_ranges: &[TimeRange],
    used_replacements: &mut BTreeSet<i64>,
    source_packet_indices: &[usize],
    output_range: std::ops::Range<usize>,
    output_stream_id: StreamId,
    timing: &[Option<OutputTiming>],
    bridge_options: Mpeg2BridgeOptions,
    mut sequence_settings: Mpeg2SequenceSettings,
    output: &mut [Option<Packet>],
) -> Result<()> {
    if output_range.is_empty()
        || output_range.end > output.len()
        || source_packet_indices.len() != output_range.len()
    {
        return Err(Error::InvalidState(
            "MPEG-2 regeneration range or source mapping is invalid".into(),
        ));
    }
    ensure_empty(output, output_range.clone())?;
    let mut presentation_output_indices: Vec<_> = output_range.clone().collect();
    presentation_output_indices.sort_by_key(|&index| {
        timing[index]
            .as_ref()
            .map_or(i64::MAX, |timing| timing.pts.value)
    });
    let mut frames = Vec::with_capacity(output_range.len());
    for &output_index in &presentation_output_indices {
        let local_index = output_index - output_range.start;
        let source_index = source_packet_indices[local_index];
        let access = &source.packets[source_index].access_unit;
        let mut frame = decoded
            .get(&access.presentation_order)
            .ok_or_else(|| {
                Error::InvalidData(format!(
                    "decoded MPEG-2 picture {} is unavailable",
                    access.presentation_order
                ))
            })?
            .clone();
        let output_timing = timing_at(timing, output_index)?;
        let output_pts = output_timing.pts;
        let mut directly_changed = false;
        for &range in effect_ranges {
            directly_changed |= range_contains(range, output_pts)?;
        }
        if directly_changed {
            let replacement = replacements.get(&output_pts.value).ok_or_else(|| {
                Error::InvalidData(format!(
                    "changed MPEG-2 picture at output PTS {} has no replacement frame",
                    output_pts.value
                ))
            })?;
            frame = (*replacement).clone();
            used_replacements.insert(output_pts.value);
        }
        frame.timing.pts = Some(output_pts);
        frame.timing.duration = Some(output_timing.duration);
        frames.push(frame);
    }

    let source_parameters = &source_stream.pictures()[0].sequence;
    let b_frames = source_b_frames(source_stream).min(frames.len().saturating_sub(1));
    let first_timing = timing_at(
        timing,
        *presentation_output_indices.first().ok_or_else(|| {
            Error::InvalidState("MPEG-2 bridge has no presentation pictures".into())
        })?,
    )?;
    if first_timing.pts.value < 0 || first_timing.pts.value % first_timing.duration.value != 0 {
        return Err(Error::InvalidData(
            "MPEG-2 bridge start is not an exact non-negative frame position".into(),
        ));
    }
    let timeline_frame = u64::try_from(first_timing.pts.value / first_timing.duration.value)
        .map_err(|_| Error::InvalidData("MPEG-2 bridge frame position exceeds u64".into()))?;
    sequence_settings.timecode_start_frame = sequence_settings
        .timecode_start_frame
        .checked_add(timeline_frame)
        .ok_or_else(|| Error::InvalidData("MPEG-2 bridge timecode origin overflows".into()))?;
    let encoded = encode_stream(
        &frames,
        Mpeg2EncodeOptions {
            frame_rate: frame_rate(source_parameters.frame_rate)?,
            gop_size: frames.len(),
            b_frames,
            quantiser_scale_code: bridge_options.quantiser_scale_code,
            motion_search_range: bridge_options.motion_search_range,
            progressive: source_parameters.progressive_sequence,
            top_field_first: source_stream.pictures()[source_packet_indices[0]]
                .coding_extension
                .top_field_first,
            sequence: sequence_settings,
        },
    )?;
    let encoded_stream = parse_stream(&encoded.data)?;
    validate_sequence_compatibility(source_parameters, &encoded_stream.pictures()[0].sequence)?;
    if encoded_stream.sequence_headers()[0].aspect_ratio_information
        != source_stream.sequence_headers()[0].aspect_ratio_information
    {
        return Err(Error::InvalidState(
            "MPEG-2 bridge did not preserve aspect-ratio information".into(),
        ));
    }
    let dependencies = analyze_dependencies(&encoded_stream)?;
    if dependencies.len() != output_range.len() {
        return Err(Error::InvalidState(format!(
            "MPEG-2 bridge produced {} pictures for {} reserved slots",
            dependencies.len(),
            output_range.len()
        )));
    }
    for (local_decode_index, (picture, access)) in encoded_stream
        .pictures()
        .iter()
        .zip(&dependencies)
        .enumerate()
    {
        let output_index = output_range.start + local_decode_index;
        let presentation_index = usize::try_from(access.presentation_order)
            .map_err(|_| Error::InvalidData("negative MPEG-2 bridge presentation order".into()))?;
        let presentation_output_index = *presentation_output_indices
            .get(presentation_index)
            .ok_or_else(|| {
                Error::InvalidState("MPEG-2 bridge presentation order exceeds frame set".into())
            })?;
        let data_start = if local_decode_index == 0 {
            0
        } else {
            encoded_stream.pictures()[local_decode_index - 1]
                .source_range
                .end
        };
        let data_end =
            if local_decode_index + 1 == dependencies.len() && output_range.end == output.len() {
                encoded.data.len()
            } else {
                picture.source_range.end
            };
        let mut flags = PacketFlags::empty();
        if access.random_access == RandomAccessKind::Clean {
            flags.insert(PacketFlags::KEY);
        }
        output[output_index] = Some(Packet {
            stream_id: output_stream_id,
            data: encoded.data[data_start..data_end].to_vec(),
            pts: Some(timing_at(timing, presentation_output_index)?.pts),
            dts: Some(timing_at(timing, output_index)?.dts),
            duration: Some(timing_at(timing, presentation_output_index)?.duration),
            flags,
            side_data: Vec::new(),
        });
    }
    Ok(())
}

fn timing_at(timing: &[Option<OutputTiming>], index: usize) -> Result<OutputTiming> {
    timing
        .get(index)
        .and_then(Option::as_ref)
        .copied()
        .ok_or_else(|| {
            Error::InvalidState(format!("MPEG-2 output timing slot {index} is unavailable"))
        })
}

fn ensure_empty(output: &[Option<Packet>], range: std::ops::Range<usize>) -> Result<()> {
    let slots = output
        .get(range)
        .ok_or_else(|| Error::InvalidState("MPEG-2 regeneration range is out of bounds".into()))?;
    if slots.iter().any(Option::is_some) {
        return Err(Error::InvalidState(
            "MPEG-2 regeneration overwrites an existing packet slot".into(),
        ));
    }
    Ok(())
}

fn range_contains(range: TimeRange, timestamp: Timestamp) -> Result<bool> {
    let timestamp = timestamp.rescale(range.start.time_base, TimestampRounding::Exact)?;
    Ok(range.start.value <= timestamp.value && timestamp.value < range.end.value)
}

fn bridge_sequence_settings(
    stream: &Mpeg2Stream<'_>,
) -> Result<(Mpeg2SequenceSettings, Mpeg2SpliceReport)> {
    let parameters = &stream
        .pictures()
        .first()
        .ok_or_else(|| Error::InvalidData("MPEG-2 source contains no pictures".into()))?
        .sequence;
    let header = stream
        .sequence_headers()
        .first()
        .ok_or_else(|| Error::InvalidData("MPEG-2 source contains no sequence header".into()))?;
    if stream
        .sequence_headers()
        .iter()
        .any(|candidate| candidate.aspect_ratio_information != header.aspect_ratio_information)
    {
        return Err(Error::Unsupported(
            "MPEG-2 bridge source changes aspect-ratio information".into(),
        ));
    }
    if parameters.profile_and_level_indication != 0x48 {
        return Err(Error::Unsupported(format!(
            "MPEG-2 bridge source profile/level 0x{:02x} is not Main Profile at Main Level",
            parameters.profile_and_level_indication
        )));
    }
    let drop_frame_timecode = stream
        .groups()
        .first()
        .is_some_and(|group| group.drop_frame_flag);
    if stream
        .groups()
        .iter()
        .any(|group| group.drop_frame_flag != drop_frame_timecode)
    {
        return Err(Error::Unsupported(
            "MPEG-2 bridge source changes GOP drop-frame timecode mode".into(),
        ));
    }
    let source_timecode_origin = stream.groups().first().map_or(Ok(0), |group| {
        group_timecode_frame(*group, frame_rate(parameters.frame_rate)?)
    })?;
    let settings = Mpeg2SequenceSettings {
        aspect_ratio_information: header.aspect_ratio_information,
        profile_and_level_indication: parameters.profile_and_level_indication,
        display: parameters.display,
        quant_matrices: Mpeg2QuantMatrices {
            intra: parameters.intra_quantizer_matrix,
            non_intra: parameters.non_intra_quantizer_matrix,
            chroma_intra: parameters.chroma_intra_quantizer_matrix,
            chroma_non_intra: parameters.chroma_non_intra_quantizer_matrix,
        },
        timecode_start_frame: source_timecode_origin,
        drop_frame_timecode,
        ..Mpeg2SequenceSettings::default()
    };
    let source_bit_rate = parameters.bit_rate;
    let report = Mpeg2SpliceReport {
        regenerated_runs: 0,
        aspect_ratio: Mpeg2SpliceAction::Preserved,
        display_metadata: if parameters.display.is_some() {
            Mpeg2SpliceAction::Preserved
        } else {
            Mpeg2SpliceAction::Absent
        },
        quantizer_matrices: Mpeg2SpliceAction::Preserved,
        profile_and_level: Mpeg2SpliceAction::Preserved,
        bit_rate: if source_bit_rate == Some(settings.bit_rate) {
            Mpeg2SpliceAction::Preserved
        } else {
            Mpeg2SpliceAction::Rewritten
        },
        source_bit_rate,
        encoded_bit_rate: settings.bit_rate,
        vbv_buffer_size: if parameters.vbv_buffer_size_bits == settings.vbv_buffer_size_bits {
            Mpeg2SpliceAction::Preserved
        } else {
            Mpeg2SpliceAction::Rewritten
        },
        source_vbv_buffer_size_bits: parameters.vbv_buffer_size_bits,
        encoded_vbv_buffer_size_bits: settings.vbv_buffer_size_bits,
        gop_timecode: Mpeg2SpliceAction::Recomputed,
        picture_vbv_delay: if stream
            .pictures()
            .iter()
            .all(|picture| picture.header.vbv_delay == 0xffff)
        {
            Mpeg2SpliceAction::Preserved
        } else {
            Mpeg2SpliceAction::Rewritten
        },
    };
    Ok((settings, report))
}

fn group_timecode_frame(group: mmrecode_mpeg2::GroupHeader, frame_rate: FrameRate) -> Result<u64> {
    let nominal_fps = match frame_rate {
        FrameRate::Fps23_976 | FrameRate::Fps24 => 24_u64,
        FrameRate::Fps25 => 25,
        FrameRate::Fps29_97 | FrameRate::Fps30 => 30,
        FrameRate::Fps50 => 50,
        FrameRate::Fps59_94 | FrameRate::Fps60 => 60,
    };
    if group.drop_frame_flag && frame_rate != FrameRate::Fps29_97 {
        return Err(Error::Unsupported(
            "MPEG-2 drop-frame GOP timecode requires 30000/1001 fps".into(),
        ));
    }
    if u64::from(group.pictures) >= nominal_fps {
        return Err(Error::InvalidData(
            "MPEG-2 GOP timecode picture number exceeds nominal frame rate".into(),
        ));
    }
    let total_minutes = u64::from(group.hours)
        .checked_mul(60)
        .and_then(|value| value.checked_add(u64::from(group.minutes)))
        .ok_or_else(|| Error::InvalidData("MPEG-2 GOP timecode overflows".into()))?;
    let nominal = total_minutes
        .checked_mul(60)
        .and_then(|value| value.checked_add(u64::from(group.seconds)))
        .and_then(|value| value.checked_mul(nominal_fps))
        .and_then(|value| value.checked_add(u64::from(group.pictures)))
        .ok_or_else(|| Error::InvalidData("MPEG-2 GOP timecode overflows".into()))?;
    if group.drop_frame_flag {
        let dropped = 2_u64
            .checked_mul(total_minutes - total_minutes / 10)
            .ok_or_else(|| Error::InvalidData("MPEG-2 drop-frame timecode overflows".into()))?;
        nominal.checked_sub(dropped).ok_or_else(|| {
            Error::InvalidData("invalid MPEG-2 drop-frame GOP timecode label".into())
        })
    } else {
        Ok(nominal)
    }
}

fn source_b_frames(stream: &Mpeg2Stream<'_>) -> usize {
    let mut current = 0_usize;
    let mut maximum = 0_usize;
    for picture in stream.pictures() {
        if picture.header.picture_coding_type == PictureType::B {
            current += 1;
            maximum = maximum.max(current);
        } else {
            current = 0;
        }
    }
    maximum
}

fn frame_rate(rate: Rational) -> Result<FrameRate> {
    for candidate in [
        FrameRate::Fps23_976,
        FrameRate::Fps24,
        FrameRate::Fps25,
        FrameRate::Fps29_97,
        FrameRate::Fps30,
        FrameRate::Fps50,
        FrameRate::Fps59_94,
        FrameRate::Fps60,
    ] {
        if candidate.rational() == rate {
            return Ok(candidate);
        }
    }
    Err(Error::Unsupported(format!(
        "MPEG-2 bridge encoder does not support frame rate {}/{}",
        rate.numerator(),
        rate.denominator()
    )))
}

fn validate_source_analysis(source: &PacketSource, stream: &Mpeg2Stream<'_>) -> Result<()> {
    let dependencies = analyze_dependencies(stream)?;
    if dependencies.len() != source.packets.len() {
        return Err(Error::InvalidData(
            "MPEG-2 packet source does not match reassembled picture count".into(),
        ));
    }
    for (indexed, parsed) in source.packets.iter().zip(dependencies) {
        if indexed.access_unit.decode_order != parsed.decode_order
            || indexed.access_unit.presentation_order != parsed.presentation_order
            || indexed.access_unit.picture_kind != parsed.picture_kind
            || indexed.access_unit.references != parsed.references
            || indexed.access_unit.parameters != parsed.parameters
        {
            return Err(Error::InvalidData(
                "MPEG-2 packet dependency metadata does not match its payload".into(),
            ));
        }
    }
    Ok(())
}

fn validate_source_compatibility(streams: &[Mpeg2Stream<'_>]) -> Result<()> {
    let canonical = streams
        .first()
        .ok_or_else(|| Error::InvalidData("MPEG-2 render has no parsed sources".into()))?;
    let canonical_parameters = &canonical
        .pictures()
        .first()
        .ok_or_else(|| Error::InvalidData("MPEG-2 source contains no pictures".into()))?
        .sequence;
    let canonical_aspect = canonical
        .sequence_headers()
        .first()
        .ok_or_else(|| Error::InvalidData("MPEG-2 source contains no sequence header".into()))?
        .aspect_ratio_information;
    for stream in streams {
        if stream
            .sequence_headers()
            .iter()
            .any(|header| header.aspect_ratio_information != canonical_aspect)
        {
            return Err(Error::Unsupported(
                "MPEG-2 edit sources use incompatible aspect-ratio information".into(),
            ));
        }
        for picture in stream.pictures() {
            validate_sequence_compatibility(canonical_parameters, &picture.sequence)?;
        }
    }
    Ok(())
}

fn validate_sequence_compatibility(
    source: &SequenceParameters,
    encoded: &SequenceParameters,
) -> Result<()> {
    if source.width != encoded.width
        || source.height != encoded.height
        || source.chroma_format != encoded.chroma_format
        || source.progressive_sequence != encoded.progressive_sequence
        || source.frame_rate != encoded.frame_rate
        || source.profile_and_level_indication != encoded.profile_and_level_indication
        || source.display != encoded.display
        || source.intra_quantizer_matrix != encoded.intra_quantizer_matrix
        || source.non_intra_quantizer_matrix != encoded.non_intra_quantizer_matrix
        || source.chroma_intra_quantizer_matrix != encoded.chroma_intra_quantizer_matrix
        || source.chroma_non_intra_quantizer_matrix != encoded.chroma_non_intra_quantizer_matrix
    {
        return Err(Error::Unsupported(
            "MPEG-2 bridge encoder cannot match source sequence parameters".into(),
        ));
    }
    Ok(())
}

fn validate_splice(packets: &[Packet], source: &Mpeg2Stream<'_>) -> Result<()> {
    let bytes = packets
        .iter()
        .flat_map(|packet| packet.data.iter().copied())
        .collect::<Vec<_>>();
    let output = parse_stream(&bytes)?;
    if output.pictures().len() != packets.len() {
        return Err(Error::InvalidData(
            "MPEG-2 splice picture count differs from its packet count".into(),
        ));
    }
    let source_parameters = &source.pictures()[0].sequence;
    let source_aspect = source.sequence_headers()[0].aspect_ratio_information;
    if output
        .sequence_headers()
        .iter()
        .any(|header| header.aspect_ratio_information != source_aspect)
    {
        return Err(Error::InvalidData(
            "MPEG-2 splice changes aspect-ratio information".into(),
        ));
    }
    for picture in output.pictures() {
        validate_sequence_compatibility(source_parameters, &picture.sequence)?;
    }
    analyze_dependencies(&output)?;
    let decoded = decode_stream(&bytes)?;
    if decoded.len() != packets.len() {
        return Err(Error::InvalidData(
            "MPEG-2 splice decoder returned the wrong picture count".into(),
        ));
    }
    Ok(())
}
