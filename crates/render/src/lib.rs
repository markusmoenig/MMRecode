//! Codec-independent render planning and packet-copy execution.
//!
//! The first vertical slice handles cuts and concatenation of independently coded video packets.
//! The inter-frame planner additionally maps generic reference graphs and exact changed ranges to
//! copy, decode, effect, and regeneration operations. Plans remain explicit so codec and editor
//! decisions stay inspectable.

use std::{
    collections::{BTreeMap, BTreeSet},
    ops::Range,
};

use mmrecode_core::{
    AccessUnitInfo, CodecId, Error, MediaType, Packet, ParameterFingerprint, RandomAccessKind,
    Result, StreamDescriptor, StreamId, Timestamp, TimestampRounding,
};
use mmrecode_edit::{Clip, ClipId, EditSequence, SourceId, TimeRange, Track, TrackId};

#[cfg(feature = "mpeg2")]
mod mpeg2;
#[cfg(feature = "mpeg2")]
pub use mpeg2::{
    Mpeg2BridgeOptions, Mpeg2FrameReplacement, Mpeg2RenderOutput, Mpeg2SpliceAction,
    Mpeg2SpliceReport, analyze_mpeg2_source, execute_mpeg2_plan, execute_mpeg2_plan_with_report,
};

#[cfg(feature = "mpegts")]
mod mpegts;
#[cfg(feature = "mpegts")]
pub use mpegts::{
    AudioBoundaryPolicy, Layer2AudioInput, MpegTsRenderOptions, MpegTsRenderOutput,
    MpegTsRenderPlan, MpegTsRenderReport, execute_mpeg2_mpegts, plan_mpeg2_mpegts,
};

/// One encoded packet paired with codec dependency analysis.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AnalyzedPacket {
    /// Encoded packet and all side data to preserve.
    pub packet: Packet,
    /// Codec-independent access-unit dependency information.
    pub access_unit: AccessUnitInfo,
}

/// Indexed packets for one source stream.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PacketSource {
    /// Edit source containing the stream.
    pub source_id: SourceId,
    /// Source stream represented by `packets`.
    pub stream_id: StreamId,
    /// Packets in codec decode/container order.
    ///
    /// Independent-frame planning additionally requires this to equal presentation order.
    pub packets: Vec<AnalyzedPacket>,
}

/// One frame-aligned timeline interval whose decoded pixels or coding must change.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct VideoChange {
    /// Clip containing the changed pictures.
    pub clip_id: ClipId,
    /// Changed half-open interval in the sequence time base.
    pub timeline_range: TimeRange,
}

/// Explicit operation in an `MMRecode` render plan.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum RenderOperation {
    /// Copies a source packet range without changing encoded payload bytes.
    CopyPackets {
        /// Source containing the packets.
        source_id: SourceId,
        /// Source stream containing the packets.
        source_stream_id: StreamId,
        /// Half-open source packet-index range.
        source_packet_range: Range<usize>,
        /// Half-open range occupied by the packets in the intermediate output.
        output_packet_range: Range<usize>,
        /// Stream identifier assigned in the output.
        output_stream_id: StreamId,
    },
    /// Rebases timestamps of packets already copied into the intermediate output.
    RewriteTimestamps {
        /// Half-open intermediate output packet range to rewrite.
        output_packet_range: Range<usize>,
        /// Source time corresponding to the beginning of the clip.
        source_start: Timestamp,
        /// Output time corresponding to `source_start`.
        output_start: Timestamp,
        /// Time base required on output packet timestamps.
        output_time_base: mmrecode_core::Rational,
    },
    /// Decodes a source interval into uncompressed frames.
    Decode {
        /// Clip requiring decoding.
        clip_id: ClipId,
        /// Source containing the decode prerequisites.
        source_id: SourceId,
        /// Source stream containing the decode prerequisites.
        source_stream_id: StreamId,
        /// Source interval to decode.
        source_range: TimeRange,
    },
    /// Applies clip effects to an uncompressed timeline interval.
    ApplyEffects {
        /// Clip owning the effects.
        clip_id: ClipId,
        /// Sequence interval to process.
        timeline_range: TimeRange,
    },
    /// Encodes a dependency bridge around a locally changed interval.
    BridgeEncode {
        /// Clip requiring bridge encoding.
        clip_id: ClipId,
        /// Source containing the selected pictures.
        source_id: SourceId,
        /// Source stream containing the selected pictures.
        source_stream_id: StreamId,
        /// Source packet indices corresponding one-to-one with the output packet slots.
        source_packet_indices: Vec<usize>,
        /// Source interval that supplies the affected pictures and decode prerequisites.
        source_range: TimeRange,
        /// Sequence interval whose dependency chain must be regenerated.
        timeline_range: TimeRange,
        /// Half-open packet slots filled by the bridge encoder.
        output_packet_range: Range<usize>,
        /// Stream identifier assigned to regenerated packets.
        output_stream_id: StreamId,
        /// Time base required on regenerated packet timestamps.
        output_time_base: mmrecode_core::Rational,
    },
    /// Encodes a complete changed interval without packet reuse.
    FullEncode {
        /// Clip requiring full encoding.
        clip_id: ClipId,
        /// Source containing the selected pictures.
        source_id: SourceId,
        /// Source stream containing the selected pictures.
        source_stream_id: StreamId,
        /// Source packet indices corresponding one-to-one with the output packet slots.
        source_packet_indices: Vec<usize>,
        /// Source interval supplying the changed pictures.
        source_range: TimeRange,
        /// Sequence interval to encode.
        timeline_range: TimeRange,
        /// Half-open packet slots filled by the encoder.
        output_packet_range: Range<usize>,
        /// Stream identifier assigned to regenerated packets.
        output_stream_id: StreamId,
        /// Time base required on regenerated packet timestamps.
        output_time_base: mmrecode_core::Rational,
    },
    /// Hands the planned output packet streams to a container muxer.
    Mux,
}

/// High-level disposition selected for one clip.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum RenderDisposition {
    /// Every selected encoded packet is preserved.
    Copy,
    /// Some dependent packets around the changed interval must be regenerated.
    BridgeEncode,
    /// The complete clip interval must be regenerated.
    FullEncode,
}

/// Explainable render decision for one clip.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RenderDecision {
    /// Track containing the clip.
    pub track_id: TrackId,
    /// Clip to which this decision applies.
    pub clip_id: ClipId,
    /// Selected render disposition.
    pub disposition: RenderDisposition,
    /// Human-readable reason suitable for inspection tools.
    pub reason: String,
}

/// Aggregate work estimated by a render plan.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct RenderSummary {
    /// Encoded packets copied without payload changes.
    pub copied_packets: usize,
    /// Packet timestamps rewritten.
    pub rewritten_timestamps: usize,
    /// Frames expected to be decoded.
    pub decoded_frames: usize,
    /// Frames expected to be encoded.
    pub encoded_frames: usize,
}

/// Explicit, inspectable render plan.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RenderPlan {
    /// Operations in execution order.
    pub operations: Vec<RenderOperation>,
    /// Per-clip explanations.
    pub decisions: Vec<RenderDecision>,
    /// Aggregate operation counts.
    pub summary: RenderSummary,
}

/// Plans frame-accurate cuts, concatenation, and localized changes for inter-frame video.
///
/// The sequence must contain one gap-free video track, but that track may select frame-aligned
/// ranges from multiple compatible packet sources. Packets remain in source decode order while
/// exact packet mappings place them onto the output timeline. Pictures whose references cross a
/// cut boundary are regenerated and dependency propagation continues until encoded packet copying
/// is safe again. Explicit [`VideoChange`] values and clip effects add decoded-pixel changes on top
/// of the boundary work.
///
/// # Errors
///
/// Returns an error when the sequence is structurally invalid, changes are not frame-aligned,
/// dependencies are incomplete or out of decode order, selected pictures are not frame-aligned,
/// clips are not gap-free, or coding parameters are incompatible.
pub fn plan_interframe_video(
    sequence: &EditSequence,
    sources: &[PacketSource],
    changes: &[VideoChange],
) -> Result<RenderPlan> {
    sequence.validate()?;
    let track = interframe_video_track(sequence)?;
    for change in changes {
        if !track.clips.iter().any(|clip| clip.id == change.clip_id) {
            return Err(Error::InvalidData(format!(
                "change references clip {:?}, which is not on the inter-frame track",
                change.clip_id
            )));
        }
    }
    let mut builder = InterframePlanBuilder::new(sequence, sources, track, changes);
    for clip in &track.clips {
        builder.plan_clip(clip)?;
    }
    builder.finish()
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum InterframeDisposition {
    Copy,
    Edited,
    Boundary,
    Bridge,
}

#[derive(Clone, Copy, Debug)]
struct PacketTiming {
    source: TimeRange,
    timeline: TimeRange,
}

struct InterframePlanBuilder<'a> {
    sequence: &'a EditSequence,
    sources: &'a [PacketSource],
    track: &'a Track,
    changes: &'a [VideoChange],
    operations: Vec<RenderOperation>,
    decisions: Vec<RenderDecision>,
    summary: RenderSummary,
    output_packet_start: usize,
    expected_timeline_start: i64,
    codec: Option<CodecId>,
    parameters: Option<ParameterFingerprint>,
}

impl<'a> InterframePlanBuilder<'a> {
    fn new(
        sequence: &'a EditSequence,
        sources: &'a [PacketSource],
        track: &'a Track,
        changes: &'a [VideoChange],
    ) -> Self {
        Self {
            sequence,
            sources,
            track,
            changes,
            operations: Vec::new(),
            decisions: Vec::with_capacity(track.clips.len()),
            summary: RenderSummary::default(),
            output_packet_start: 0,
            expected_timeline_start: 0,
            codec: None,
            parameters: None,
        }
    }

    fn plan_clip(&mut self, clip: &Clip) -> Result<()> {
        self.validate_clip_timing(clip)?;
        let (stream, packet_source) = locate_clip_stream(self.sequence, self.sources, clip)?;
        self.validate_compatibility(clip, stream, packet_source)?;
        let timings = index_interframe_source(clip, packet_source)?;
        let picture_indices = validate_dependency_graph(packet_source)?;
        let selected = select_interframe_packets(&timings, clip.source_range)?;
        let directly_changed =
            select_changed_packets(self.sequence, clip, &timings, &selected, self.changes)?;
        let selected_pictures = selected
            .iter()
            .map(|&index| packet_source.packets[index].access_unit.picture_id)
            .collect::<BTreeSet<_>>();
        let mut affected = BTreeSet::new();
        let mut dispositions = Vec::with_capacity(selected.len());
        let mut boundary_count = 0_usize;
        let mut bridge_count = 0_usize;
        for &source_index in &selected {
            let analyzed = &packet_source.packets[source_index];
            let crosses_boundary = analyzed
                .access_unit
                .references
                .iter()
                .any(|reference| !selected_pictures.contains(reference));
            let has_affected_reference = analyzed
                .access_unit
                .references
                .iter()
                .any(|reference| affected.contains(reference));
            let disposition = if directly_changed.contains(&source_index) {
                InterframeDisposition::Edited
            } else if crosses_boundary {
                boundary_count = checked_increment(boundary_count, "boundary picture count")?;
                InterframeDisposition::Boundary
            } else if has_affected_reference {
                bridge_count = checked_increment(bridge_count, "bridge picture count")?;
                InterframeDisposition::Bridge
            } else {
                InterframeDisposition::Copy
            };
            if disposition != InterframeDisposition::Copy {
                affected.insert(analyzed.access_unit.picture_id);
            }
            dispositions.push(disposition);
        }

        let mut decoded = BTreeSet::new();
        append_interframe_runs(
            &mut self.operations,
            &mut decoded,
            clip,
            self.sequence,
            packet_source,
            &timings,
            &picture_indices,
            &selected,
            &dispositions,
            self.output_packet_start,
        )?;
        let encoded_frames = affected.len();
        let copied_packets = selected
            .len()
            .checked_sub(encoded_frames)
            .ok_or_else(|| Error::InvalidState("affected picture count exceeds clip".into()))?;
        self.append_decision(
            clip,
            directly_changed.len(),
            boundary_count,
            bridge_count,
            copied_packets,
            encoded_frames,
        );
        self.summary.copied_packets = checked_add(
            self.summary.copied_packets,
            copied_packets,
            "copied packet count",
        )?;
        self.summary.rewritten_timestamps = checked_add(
            self.summary.rewritten_timestamps,
            copied_packets,
            "rewritten timestamp count",
        )?;
        self.summary.decoded_frames = checked_add(
            self.summary.decoded_frames,
            decoded.len(),
            "decoded frame count",
        )?;
        self.summary.encoded_frames = checked_add(
            self.summary.encoded_frames,
            encoded_frames,
            "encoded frame count",
        )?;
        self.output_packet_start = checked_add(
            self.output_packet_start,
            selected.len(),
            "output packet count",
        )?;
        Ok(())
    }

    fn validate_clip_timing(&mut self, clip: &Clip) -> Result<()> {
        if clip.timeline_range.start.value != self.expected_timeline_start {
            return Err(Error::Unsupported(format!(
                "clip {:?} does not continue the gap-free inter-frame timeline at {}",
                clip.id, self.expected_timeline_start
            )));
        }
        validate_equal_duration(clip.source_range, clip.timeline_range)?;
        self.expected_timeline_start = clip.timeline_range.end.value;
        Ok(())
    }

    fn validate_compatibility(
        &mut self,
        clip: &Clip,
        stream: &StreamDescriptor,
        packet_source: &PacketSource,
    ) -> Result<()> {
        validate_output_codec(self.sequence, stream, clip)?;
        if packet_source.packets.is_empty() {
            return Err(Error::InvalidData(
                "inter-frame packet source contains no access units".into(),
            ));
        }
        if let Some(codec) = &self.codec {
            if codec != &stream.codec.codec_id {
                return Err(Error::Unsupported(
                    "inter-frame clips use incompatible video codecs".into(),
                ));
            }
        } else {
            self.codec = Some(stream.codec.codec_id.clone());
        }
        let parameters = validate_parameters(packet_source)?;
        if self
            .parameters
            .is_some_and(|expected| expected != parameters)
        {
            return Err(Error::Unsupported(
                "inter-frame clips use incompatible codec parameters".into(),
            ));
        }
        self.parameters = Some(parameters);
        Ok(())
    }

    fn append_decision(
        &mut self,
        clip: &Clip,
        changed: usize,
        boundary: usize,
        bridge: usize,
        copied: usize,
        encoded: usize,
    ) {
        let disposition = if boundary > 0 || bridge > 0 {
            RenderDisposition::BridgeEncode
        } else if encoded > 0 {
            RenderDisposition::FullEncode
        } else {
            RenderDisposition::Copy
        };
        let reason = match disposition {
            RenderDisposition::Copy => {
                "frame-aligned boundaries are independently decodable; every selected access unit remains reusable".into()
            }
            RenderDisposition::BridgeEncode => format!(
                "{changed} changed and {boundary} cut-boundary picture(s) propagate through {bridge} dependent picture(s); {copied} selected picture(s) remain copyable"
            ),
            RenderDisposition::FullEncode => format!(
                "{changed} directly changed picture(s) require encoding; {copied} selected picture(s) remain copyable"
            ),
        };
        self.decisions.push(RenderDecision {
            track_id: self.track.id,
            clip_id: clip.id,
            disposition,
            reason,
        });
    }

    fn finish(mut self) -> Result<RenderPlan> {
        if self.output_packet_start == 0 {
            return Err(Error::InvalidData(
                "inter-frame edit sequence contains no selected pictures".into(),
            ));
        }
        self.operations.push(RenderOperation::Mux);
        Ok(RenderPlan {
            operations: self.operations,
            decisions: self.decisions,
            summary: self.summary,
        })
    }
}

fn checked_increment(value: usize, label: &str) -> Result<usize> {
    checked_add(value, 1, label)
}

fn checked_add(left: usize, right: usize, label: &str) -> Result<usize> {
    left.checked_add(right)
        .ok_or_else(|| Error::InvalidData(format!("{label} overflows")))
}

fn interframe_video_track(sequence: &EditSequence) -> Result<&Track> {
    let mut populated_tracks = sequence
        .tracks
        .iter()
        .filter(|track| !track.clips.is_empty());
    let track = populated_tracks
        .next()
        .ok_or_else(|| Error::InvalidData("edit sequence contains no clips".into()))?;
    if populated_tracks.next().is_some() || track.media_type != MediaType::Video {
        return Err(Error::Unsupported(
            "inter-frame planning supports exactly one populated video track".into(),
        ));
    }
    if !track.transitions.is_empty() {
        return Err(Error::Unsupported(
            "inter-frame transitions require decoded-frame rendering".into(),
        ));
    }
    Ok(track)
}

fn locate_clip_stream<'a>(
    sequence: &'a EditSequence,
    sources: &'a [PacketSource],
    clip: &Clip,
) -> Result<(&'a StreamDescriptor, &'a PacketSource)> {
    let source = sequence
        .sources
        .iter()
        .find(|source| source.id == clip.source_id)
        .ok_or_else(|| Error::InvalidData("validated clip source disappeared".into()))?;
    let stream = source
        .streams
        .iter()
        .find(|stream| stream.id == clip.source_stream_id)
        .ok_or_else(|| Error::InvalidData("validated clip stream disappeared".into()))?;
    let packet_source = sources
        .iter()
        .find(|packet_source| {
            packet_source.source_id == clip.source_id
                && packet_source.stream_id == clip.source_stream_id
        })
        .ok_or_else(|| {
            Error::InvalidData(format!(
                "no packet index for source {:?} stream {:?}",
                clip.source_id, clip.source_stream_id
            ))
        })?;
    Ok((stream, packet_source))
}

fn validate_output_codec(
    sequence: &EditSequence,
    stream: &StreamDescriptor,
    clip: &Clip,
) -> Result<()> {
    if let Some(requested) = &sequence.output.video_codec
        && requested != &stream.codec.codec_id
    {
        return Err(Error::Unsupported(format!(
            "clip {:?} codec {} does not match requested output codec {}",
            clip.id,
            stream.codec.codec_id.as_str(),
            requested.as_str()
        )));
    }
    Ok(())
}

fn index_interframe_source(clip: &Clip, packet_source: &PacketSource) -> Result<Vec<PacketTiming>> {
    let mut timings = Vec::with_capacity(packet_source.packets.len());
    for analyzed in &packet_source.packets {
        if analyzed.packet.stream_id != clip.source_stream_id {
            return Err(Error::InvalidData(format!(
                "indexed packet stream {:?} does not match source stream {:?}",
                analyzed.packet.stream_id, clip.source_stream_id
            )));
        }
        let (start, end) = packet_interval(analyzed, clip.source_range.start.time_base)?;
        let source = TimeRange::new(
            Timestamp {
                value: start,
                time_base: clip.source_range.start.time_base,
            },
            Timestamp {
                value: end,
                time_base: clip.source_range.start.time_base,
            },
        )?;
        let timeline = TimeRange::new(
            rewrite_timestamp(
                source.start,
                clip.source_range.start,
                clip.timeline_range.start,
            )?,
            rewrite_timestamp(
                source.end,
                clip.source_range.start,
                clip.timeline_range.start,
            )?,
        )?;
        timings.push(PacketTiming { source, timeline });
    }
    let mut timestamp_order: Vec<_> = (0..timings.len()).collect();
    timestamp_order.sort_by_key(|&index| timings[index].source.start.value);
    let mut presentation_order: Vec<_> = (0..timings.len()).collect();
    presentation_order
        .sort_by_key(|&index| packet_source.packets[index].access_unit.presentation_order);
    if presentation_order.windows(2).any(|window| {
        packet_source.packets[window[0]]
            .access_unit
            .presentation_order
            == packet_source.packets[window[1]]
                .access_unit
                .presentation_order
    }) || presentation_order != timestamp_order
    {
        return Err(Error::InvalidData(
            "access-unit presentation order does not match packet timestamps".into(),
        ));
    }
    let first = timestamp_order
        .first()
        .ok_or_else(|| Error::InvalidData("inter-frame packet source is empty".into()))?;
    let last = timestamp_order
        .last()
        .ok_or_else(|| Error::InvalidData("inter-frame packet source is empty".into()))?;
    validate_contiguous_coverage(
        timestamp_order.iter().map(|&index| timings[index].source),
        TimeRange::new(timings[*first].source.start, timings[*last].source.end)?,
        "inter-frame packet source",
    )?;
    Ok(timings)
}

fn select_interframe_packets(timings: &[PacketTiming], requested: TimeRange) -> Result<Vec<usize>> {
    let selected = timings
        .iter()
        .enumerate()
        .filter(|(_, timing)| {
            requested.start.value <= timing.source.start.value
                && timing.source.end.value <= requested.end.value
        })
        .map(|(index, _)| index)
        .collect::<Vec<_>>();
    validate_contiguous_coverage(
        selected.iter().map(|&index| timings[index].source),
        requested,
        "inter-frame clip",
    )?;
    Ok(selected)
}

fn validate_dependency_graph(
    packet_source: &PacketSource,
) -> Result<BTreeMap<mmrecode_core::PictureId, usize>> {
    let mut indices = BTreeMap::new();
    let mut previous_decode_order = None;
    for (index, analyzed) in packet_source.packets.iter().enumerate() {
        if previous_decode_order
            .is_some_and(|previous| analyzed.access_unit.decode_order <= previous)
        {
            return Err(Error::InvalidData(
                "inter-frame packets are not in strictly increasing decode order".into(),
            ));
        }
        previous_decode_order = Some(analyzed.access_unit.decode_order);
        if indices
            .insert(analyzed.access_unit.picture_id, index)
            .is_some()
        {
            return Err(Error::InvalidData(
                "inter-frame dependency graph contains duplicate picture identifiers".into(),
            ));
        }
        for reference in &analyzed.access_unit.references {
            let Some(&reference_index) = indices.get(reference) else {
                return Err(Error::InvalidData(format!(
                    "picture {:?} references unavailable or later picture {reference:?}",
                    analyzed.access_unit.picture_id
                )));
            };
            if reference_index >= index {
                return Err(Error::InvalidData(
                    "inter-frame reference does not precede its dependent in decode order".into(),
                ));
            }
        }
    }
    Ok(indices)
}

fn validate_parameters(packet_source: &PacketSource) -> Result<ParameterFingerprint> {
    let expected = packet_source.packets[0].access_unit.parameters;
    if packet_source
        .packets
        .iter()
        .any(|packet| packet.access_unit.parameters != expected)
    {
        return Err(Error::Unsupported(
            "initial inter-frame bridge planning requires constant codec parameters".into(),
        ));
    }
    Ok(expected)
}

fn select_changed_packets(
    sequence: &EditSequence,
    clip: &Clip,
    timings: &[PacketTiming],
    selected_indices: &[usize],
    changes: &[VideoChange],
) -> Result<BTreeSet<usize>> {
    let mut selected = BTreeSet::new();
    let mut effective_changes = changes
        .iter()
        .filter(|change| change.clip_id == clip.id)
        .copied()
        .collect::<Vec<_>>();
    if !clip.effects.is_empty() {
        effective_changes.push(VideoChange {
            clip_id: clip.id,
            timeline_range: clip.timeline_range,
        });
    }
    for change in effective_changes {
        if change.timeline_range.start.time_base != sequence.time_base
            || change.timeline_range.end.time_base != sequence.time_base
        {
            return Err(Error::InvalidData(
                "inter-frame change does not use the sequence time base".into(),
            ));
        }
        if change.timeline_range.start.value < clip.timeline_range.start.value
            || change.timeline_range.end.value > clip.timeline_range.end.value
        {
            return Err(Error::InvalidData(
                "inter-frame change lies outside its clip".into(),
            ));
        }
        let matching: Vec<_> = selected_indices
            .iter()
            .filter_map(|&index| {
                let timing = &timings[index];
                (timing.timeline.start.value >= change.timeline_range.start.value
                    && timing.timeline.end.value <= change.timeline_range.end.value)
                    .then_some((index, timing))
            })
            .collect();
        validate_contiguous_coverage(
            matching.iter().map(|(_, timing)| timing.timeline),
            change.timeline_range,
            "inter-frame change",
        )?;
        selected.extend(matching.into_iter().map(|(index, _)| index));
    }
    Ok(selected)
}

fn validate_contiguous_coverage(
    ranges: impl IntoIterator<Item = TimeRange>,
    expected: TimeRange,
    label: &str,
) -> Result<()> {
    let mut ranges: Vec<_> = ranges.into_iter().collect();
    ranges.sort_by_key(|range| range.start.value);
    let Some(first) = ranges.first() else {
        return Err(Error::Unsupported(format!(
            "{label} does not align with any complete access unit"
        )));
    };
    if first.start != expected.start {
        return Err(Error::Unsupported(format!(
            "{label} start does not align with an access-unit boundary"
        )));
    }
    let mut end = first.end;
    for range in &ranges[1..] {
        if range.start != end {
            return Err(Error::Unsupported(format!(
                "{label} contains a timestamp gap or overlap"
            )));
        }
        end = range.end;
    }
    if end != expected.end {
        return Err(Error::Unsupported(format!(
            "{label} end does not align with an access-unit boundary"
        )));
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn append_interframe_runs(
    operations: &mut Vec<RenderOperation>,
    decoded: &mut BTreeSet<usize>,
    clip: &Clip,
    sequence: &EditSequence,
    packet_source: &PacketSource,
    timings: &[PacketTiming],
    picture_indices: &BTreeMap<mmrecode_core::PictureId, usize>,
    selected: &[usize],
    dispositions: &[InterframeDisposition],
    output_packet_start: usize,
) -> Result<()> {
    let mut start = 0_usize;
    while start < dispositions.len() {
        let copying = dispositions[start] == InterframeDisposition::Copy;
        let mut end = start + 1;
        while end < dispositions.len()
            && (dispositions[end] == InterframeDisposition::Copy) == copying
            && (!copying || selected[end] == selected[end - 1] + 1)
        {
            end += 1;
        }
        let output_start = checked_add(output_packet_start, start, "output packet range")?;
        let output_end = checked_add(output_packet_start, end, "output packet range")?;
        if copying {
            let output_packet_range = output_start..output_end;
            operations.push(RenderOperation::CopyPackets {
                source_id: clip.source_id,
                source_stream_id: clip.source_stream_id,
                source_packet_range: selected[start]..selected[end - 1] + 1,
                output_packet_range: output_packet_range.clone(),
                output_stream_id: StreamId(0),
            });
            operations.push(RenderOperation::RewriteTimestamps {
                output_packet_range,
                source_start: clip.source_range.start,
                output_start: clip
                    .timeline_range
                    .start
                    .rescale(sequence.output.time_base, TimestampRounding::Exact)?,
                output_time_base: sequence.output.time_base,
            });
        } else {
            let source_packet_indices = selected[start..end].to_vec();
            let prerequisites =
                decode_prerequisites(packet_source, picture_indices, &source_packet_indices)?;
            decoded.extend(&prerequisites);
            let source_range =
                enclosing_range(prerequisites.iter().map(|&index| timings[index].source))?;
            let timeline_range = enclosing_range(
                source_packet_indices
                    .iter()
                    .map(|&index| timings[index].timeline),
            )?;
            operations.push(RenderOperation::Decode {
                clip_id: clip.id,
                source_id: clip.source_id,
                source_stream_id: clip.source_stream_id,
                source_range,
            });
            let edited_ranges = coalesce_ranges(
                (start..end)
                    .filter(|&index| dispositions[index] == InterframeDisposition::Edited)
                    .map(|index| timings[selected[index]].timeline),
            );
            operations.extend(edited_ranges.into_iter().map(|timeline_range| {
                RenderOperation::ApplyEffects {
                    clip_id: clip.id,
                    timeline_range,
                }
            }));
            let output_packet_range = output_start..output_end;
            if dispositions[start..end].contains(&InterframeDisposition::Boundary)
                || dispositions[start..end].contains(&InterframeDisposition::Bridge)
            {
                operations.push(RenderOperation::BridgeEncode {
                    clip_id: clip.id,
                    source_id: clip.source_id,
                    source_stream_id: clip.source_stream_id,
                    source_packet_indices,
                    source_range,
                    timeline_range,
                    output_packet_range,
                    output_stream_id: StreamId(0),
                    output_time_base: sequence.output.time_base,
                });
            } else {
                operations.push(RenderOperation::FullEncode {
                    clip_id: clip.id,
                    source_id: clip.source_id,
                    source_stream_id: clip.source_stream_id,
                    source_packet_indices,
                    source_range,
                    timeline_range,
                    output_packet_range,
                    output_stream_id: StreamId(0),
                    output_time_base: sequence.output.time_base,
                });
            }
        }
        start = end;
    }
    Ok(())
}

fn decode_prerequisites(
    packet_source: &PacketSource,
    picture_indices: &BTreeMap<mmrecode_core::PictureId, usize>,
    encoded_indices: &[usize],
) -> Result<BTreeSet<usize>> {
    let mut required: BTreeSet<_> = encoded_indices.iter().copied().collect();
    let mut pending: Vec<_> = required.iter().copied().collect();
    while let Some(index) = pending.pop() {
        for reference in &packet_source.packets[index].access_unit.references {
            let reference_index = *picture_indices.get(reference).ok_or_else(|| {
                Error::InvalidData(format!("decode prerequisite {reference:?} is unavailable"))
            })?;
            if required.insert(reference_index) {
                pending.push(reference_index);
            }
        }
    }
    Ok(required)
}

fn enclosing_range(ranges: impl IntoIterator<Item = TimeRange>) -> Result<TimeRange> {
    let mut ranges = ranges.into_iter();
    let first = ranges
        .next()
        .ok_or_else(|| Error::InvalidState("cannot enclose an empty time-range set".into()))?;
    let mut start = first.start;
    let mut end = first.end;
    for range in ranges {
        if range.start.time_base != start.time_base {
            return Err(Error::InvalidData(
                "cannot enclose ranges with different time bases".into(),
            ));
        }
        if range.start.value < start.value {
            start = range.start;
        }
        if range.end.value > end.value {
            end = range.end;
        }
    }
    TimeRange::new(start, end)
}

fn coalesce_ranges(ranges: impl IntoIterator<Item = TimeRange>) -> Vec<TimeRange> {
    let mut ranges: Vec<_> = ranges.into_iter().collect();
    ranges.sort_by_key(|range| range.start.value);
    let mut result: Vec<TimeRange> = Vec::new();
    for range in ranges {
        if let Some(previous) = result.last_mut()
            && previous.end == range.start
        {
            previous.end = range.end;
        } else {
            result.push(range);
        }
    }
    result
}

/// Plans a packet-copy-only cut/concatenation for one independently coded video track.
///
/// The supported path requires a gap-free sequence beginning at zero, no audio or data clips, no
/// effects or transitions, exact source-to-timeline duration mapping, clean reference-free access
/// units, packet-aligned clip boundaries, and compatible codecs/parameters across all clips.
///
/// # Errors
///
/// Returns an error when the edit is structurally invalid, required packet analysis is missing,
/// or the edit cannot be represented without decoding and encoding.
pub fn plan_independent_video(
    sequence: &EditSequence,
    sources: &[PacketSource],
) -> Result<RenderPlan> {
    sequence.validate()?;
    let track = independent_video_track(sequence)?;
    let mut builder = IndependentPlanBuilder::new(sequence, sources, track);
    for clip in &track.clips {
        builder.plan_clip(clip)?;
    }
    builder.operations.push(RenderOperation::Mux);
    Ok(RenderPlan {
        operations: builder.operations,
        decisions: builder.decisions,
        summary: builder.summary,
    })
}

fn independent_video_track(sequence: &EditSequence) -> Result<&Track> {
    let mut populated_tracks = sequence
        .tracks
        .iter()
        .filter(|track| !track.clips.is_empty());
    let track = populated_tracks
        .next()
        .ok_or_else(|| Error::InvalidData("edit sequence contains no clips".into()))?;
    if populated_tracks.next().is_some() || track.media_type != MediaType::Video {
        return Err(Error::Unsupported(
            "independent packet rendering supports exactly one populated video track".into(),
        ));
    }
    if !track.transitions.is_empty() {
        return Err(Error::Unsupported(
            "transitions require decoded-frame rendering".into(),
        ));
    }
    Ok(track)
}

struct IndependentPlanBuilder<'a> {
    sequence: &'a EditSequence,
    sources: &'a [PacketSource],
    track: &'a Track,
    operations: Vec<RenderOperation>,
    decisions: Vec<RenderDecision>,
    summary: RenderSummary,
    output_packet_start: usize,
    expected_timeline_start: i64,
    copied_codec: Option<CodecId>,
    copied_parameters: Option<ParameterFingerprint>,
}

impl<'a> IndependentPlanBuilder<'a> {
    fn new(sequence: &'a EditSequence, sources: &'a [PacketSource], track: &'a Track) -> Self {
        Self {
            sequence,
            sources,
            track,
            operations: Vec::with_capacity(track.clips.len() * 2 + 1),
            decisions: Vec::with_capacity(track.clips.len()),
            summary: RenderSummary::default(),
            output_packet_start: 0,
            expected_timeline_start: 0,
            copied_codec: None,
            copied_parameters: None,
        }
    }

    fn plan_clip(&mut self, clip: &Clip) -> Result<()> {
        self.validate_clip_timing(clip)?;
        let (stream, packet_source) = self.locate_clip_stream(clip)?;
        self.validate_codec(clip, stream)?;
        let selection = select_packets(packet_source, clip.source_range)?;
        self.validate_selected_packets(clip, packet_source, selection.clone())?;
        self.append_copy_operations(clip, selection)
    }

    fn validate_clip_timing(&mut self, clip: &Clip) -> Result<()> {
        if !clip.effects.is_empty() {
            return Err(Error::Unsupported(format!(
                "clip {:?} effects require decoded-frame rendering",
                clip.id
            )));
        }
        if clip.timeline_range.start.value != self.expected_timeline_start {
            return Err(Error::Unsupported(format!(
                "clip {:?} does not continue the gap-free packet-copy timeline at {}",
                clip.id, self.expected_timeline_start
            )));
        }
        self.expected_timeline_start = clip.timeline_range.end.value;
        validate_equal_duration(clip.source_range, clip.timeline_range)
    }

    fn locate_clip_stream(&self, clip: &Clip) -> Result<(&'a StreamDescriptor, &'a PacketSource)> {
        let source = self
            .sequence
            .sources
            .iter()
            .find(|source| source.id == clip.source_id)
            .ok_or_else(|| Error::InvalidData("validated clip source disappeared".into()))?;
        let stream = source
            .streams
            .iter()
            .find(|stream| stream.id == clip.source_stream_id)
            .ok_or_else(|| Error::InvalidData("validated clip stream disappeared".into()))?;
        let packet_source = self
            .sources
            .iter()
            .find(|packet_source| {
                packet_source.source_id == clip.source_id
                    && packet_source.stream_id == clip.source_stream_id
            })
            .ok_or_else(|| {
                Error::InvalidData(format!(
                    "no packet index for source {:?} stream {:?}",
                    clip.source_id, clip.source_stream_id
                ))
            })?;
        Ok((stream, packet_source))
    }

    fn validate_codec(&mut self, clip: &Clip, stream: &StreamDescriptor) -> Result<()> {
        if let Some(requested) = &self.sequence.output.video_codec
            && requested != &stream.codec.codec_id
        {
            return Err(Error::Unsupported(format!(
                "clip {:?} codec {} does not match requested output codec {}",
                clip.id,
                stream.codec.codec_id.as_str(),
                requested.as_str()
            )));
        }
        if let Some(existing) = &self.copied_codec {
            if existing != &stream.codec.codec_id {
                return Err(Error::Unsupported(
                    "packet-copy clips use incompatible video codecs".into(),
                ));
            }
        } else {
            self.copied_codec = Some(stream.codec.codec_id.clone());
        }
        Ok(())
    }

    fn validate_selected_packets(
        &mut self,
        clip: &Clip,
        packet_source: &PacketSource,
        selection: Range<usize>,
    ) -> Result<()> {
        for analyzed in &packet_source.packets[selection] {
            if analyzed.packet.stream_id != clip.source_stream_id {
                return Err(Error::InvalidData(format!(
                    "indexed packet stream {:?} does not match source stream {:?}",
                    analyzed.packet.stream_id, clip.source_stream_id
                )));
            }
            if analyzed.access_unit.random_access != RandomAccessKind::Clean
                || !analyzed.access_unit.references.is_empty()
            {
                return Err(Error::Unsupported(format!(
                    "clip {:?} contains an access unit that is not independently decodable",
                    clip.id
                )));
            }
            if let Some(parameters) = self.copied_parameters {
                if parameters != analyzed.access_unit.parameters {
                    return Err(Error::Unsupported(
                        "packet-copy clips have incompatible codec parameters".into(),
                    ));
                }
            } else {
                self.copied_parameters = Some(analyzed.access_unit.parameters);
            }
        }
        Ok(())
    }

    fn append_copy_operations(&mut self, clip: &Clip, selection: Range<usize>) -> Result<()> {
        let packet_count = selection.len();
        let output_packet_end = self
            .output_packet_start
            .checked_add(packet_count)
            .ok_or_else(|| Error::InvalidData("render packet count overflows".into()))?;
        let output_packet_range = self.output_packet_start..output_packet_end;
        let output_start = clip
            .timeline_range
            .start
            .rescale(self.sequence.output.time_base, TimestampRounding::Exact)?;
        self.operations.push(RenderOperation::CopyPackets {
            source_id: clip.source_id,
            source_stream_id: clip.source_stream_id,
            source_packet_range: selection,
            output_packet_range: output_packet_range.clone(),
            output_stream_id: StreamId(0),
        });
        self.operations.push(RenderOperation::RewriteTimestamps {
            output_packet_range,
            source_start: clip.source_range.start,
            output_start,
            output_time_base: self.sequence.output.time_base,
        });
        self.decisions.push(RenderDecision {
            track_id: self.track.id,
            clip_id: clip.id,
            disposition: RenderDisposition::Copy,
            reason: "clean reference-free access units, packet-aligned boundaries, no effects, and compatible codec parameters".into(),
        });
        self.summary.copied_packets = self
            .summary
            .copied_packets
            .checked_add(packet_count)
            .ok_or_else(|| Error::InvalidData("render packet count overflows".into()))?;
        self.summary.rewritten_timestamps = self
            .summary
            .rewritten_timestamps
            .checked_add(packet_count)
            .ok_or_else(|| Error::InvalidData("render timestamp count overflows".into()))?;
        self.output_packet_start = output_packet_end;
        Ok(())
    }
}

/// Executes the copy and timestamp operations of an independent-frame render plan.
///
/// Returned packets are ordered, timestamped, and stream-mapped for delivery to a compatible
/// [`mmrecode_core::Muxer`]. Encoded payload bytes, flags, and side data are preserved.
///
/// # Errors
///
/// Returns an error when the plan references unavailable packets, produces inconsistent output
/// ranges, contains regeneration operations, or cannot rewrite timestamps exactly.
pub fn execute_packet_plan(plan: &RenderPlan, sources: &[PacketSource]) -> Result<Vec<Packet>> {
    let mut output = Vec::with_capacity(plan.summary.copied_packets);
    let mut reached_mux = false;
    for operation in &plan.operations {
        if reached_mux {
            return Err(Error::InvalidState(
                "render plan contains operations after mux".into(),
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
                if output_packet_range.start != output.len()
                    || output_packet_range.len() != source_packet_range.len()
                {
                    return Err(Error::InvalidState(
                        "copy operation output range is not contiguous or has the wrong length"
                            .into(),
                    ));
                }
                let source = sources
                    .iter()
                    .find(|source| {
                        source.source_id == *source_id && source.stream_id == *source_stream_id
                    })
                    .ok_or_else(|| {
                        Error::InvalidData(format!(
                            "render source {source_id:?} stream {source_stream_id:?} is unavailable"
                        ))
                    })?;
                let selected =
                    source
                        .packets
                        .get(source_packet_range.clone())
                        .ok_or_else(|| {
                            Error::InvalidData("copy packet range is out of bounds".into())
                        })?;
                output.extend(selected.iter().map(|analyzed| {
                    let mut packet = analyzed.packet.clone();
                    packet.stream_id = *output_stream_id;
                    packet
                }));
            }
            RenderOperation::RewriteTimestamps {
                output_packet_range,
                source_start,
                output_start,
                output_time_base,
            } => {
                let packets = output.get_mut(output_packet_range.clone()).ok_or_else(|| {
                    Error::InvalidState("timestamp rewrite range is unavailable".into())
                })?;
                for packet in packets {
                    packet.pts = packet
                        .pts
                        .map(|value| rewrite_timestamp(value, *source_start, *output_start))
                        .transpose()?;
                    packet.dts = packet
                        .dts
                        .map(|value| rewrite_timestamp(value, *source_start, *output_start))
                        .transpose()?;
                    packet.duration = packet
                        .duration
                        .map(|value| value.rescale(*output_time_base, TimestampRounding::Exact))
                        .transpose()?;
                }
            }
            RenderOperation::Mux => reached_mux = true,
            RenderOperation::Decode { .. }
            | RenderOperation::ApplyEffects { .. }
            | RenderOperation::BridgeEncode { .. }
            | RenderOperation::FullEncode { .. } => {
                return Err(Error::Unsupported(
                    "packet executor cannot execute decode or encode operations".into(),
                ));
            }
        }
    }
    if !reached_mux {
        return Err(Error::InvalidState(
            "render plan does not terminate in mux".into(),
        ));
    }
    if output.len() != plan.summary.copied_packets {
        return Err(Error::InvalidState(
            "render output packet count does not match plan summary".into(),
        ));
    }
    Ok(output)
}

fn validate_equal_duration(source: TimeRange, timeline: TimeRange) -> Result<()> {
    let source_duration = source
        .duration()?
        .rescale(timeline.start.time_base, TimestampRounding::Exact)?;
    let timeline_duration = timeline.duration()?;
    if source_duration.value != timeline_duration.value {
        return Err(Error::Unsupported(
            "packet-copy clips cannot change playback speed or duration".into(),
        ));
    }
    Ok(())
}

fn select_packets(source: &PacketSource, requested: TimeRange) -> Result<Range<usize>> {
    let intervals: Vec<_> = source
        .packets
        .iter()
        .map(|packet| packet_interval(packet, requested.start.time_base))
        .collect::<Result<_>>()?;
    if intervals.windows(2).any(|window| window[1].0 < window[0].0) {
        return Err(Error::InvalidData(
            "independent packet index is not in presentation order".into(),
        ));
    }
    let start = intervals
        .iter()
        .position(|&(start, _)| start == requested.start.value)
        .ok_or_else(|| {
            Error::Unsupported("clip start does not align with a packet boundary".into())
        })?;
    let mut expected_start = requested.start.value;
    for (index, &(packet_start, packet_end)) in intervals.iter().enumerate().skip(start) {
        if packet_start != expected_start {
            return Err(Error::Unsupported(
                "packet-copy clip contains a timestamp gap".into(),
            ));
        }
        if packet_end > requested.end.value {
            break;
        }
        if packet_end == requested.end.value {
            return Ok(start..index + 1);
        }
        expected_start = packet_end;
    }
    Err(Error::Unsupported(
        "clip end does not align with a packet boundary".into(),
    ))
}

fn packet_interval(
    analyzed: &AnalyzedPacket,
    time_base: mmrecode_core::Rational,
) -> Result<(i64, i64)> {
    let pts = analyzed
        .packet
        .pts
        .ok_or_else(|| Error::InvalidData("independent packet has no PTS".into()))?
        .rescale(time_base, TimestampRounding::Exact)?;
    let duration = analyzed
        .packet
        .duration
        .ok_or_else(|| Error::InvalidData("independent packet has no duration".into()))?
        .rescale(time_base, TimestampRounding::Exact)?;
    if duration.value <= 0 {
        return Err(Error::InvalidData(
            "independent packet duration must be positive".into(),
        ));
    }
    let end = pts
        .value
        .checked_add(duration.value)
        .ok_or_else(|| Error::InvalidData("packet end timestamp overflows".into()))?;
    Ok((pts.value, end))
}

fn rewrite_timestamp(
    value: Timestamp,
    source_start: Timestamp,
    output_start: Timestamp,
) -> Result<Timestamp> {
    let source_start = source_start.rescale(value.time_base, TimestampRounding::Exact)?;
    let delta = value
        .value
        .checked_sub(source_start.value)
        .ok_or_else(|| Error::InvalidData("timestamp source offset overflows".into()))?;
    let delta = Timestamp {
        value: delta,
        time_base: value.time_base,
    }
    .rescale(output_start.time_base, TimestampRounding::Exact)?;
    let value = output_start
        .value
        .checked_add(delta.value)
        .ok_or_else(|| Error::InvalidData("rewritten timestamp overflows".into()))?;
    Ok(Timestamp {
        value,
        time_base: output_start.time_base,
    })
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use mmrecode_core::{
        CodecDescriptor, PacketFlags, PictureId, PictureKind, Rational, StreamDescriptor,
    };
    use mmrecode_edit::{Clip, MediaSource, OutputIntent, Track};

    use super::*;

    fn time_base() -> mmrecode_core::Rational {
        Rational::new(1, 25).unwrap()
    }

    fn range(start: i64, end: i64) -> TimeRange {
        TimeRange::new(
            Timestamp {
                value: start,
                time_base: time_base(),
            },
            Timestamp {
                value: end,
                time_base: time_base(),
            },
        )
        .unwrap()
    }

    fn analyzed_packet(index: i64) -> AnalyzedPacket {
        AnalyzedPacket {
            packet: Packet {
                stream_id: StreamId(2),
                data: vec![u8::try_from(index).unwrap()],
                pts: Some(Timestamp {
                    value: index,
                    time_base: time_base(),
                }),
                dts: Some(Timestamp {
                    value: index,
                    time_base: time_base(),
                }),
                duration: Some(Timestamp {
                    value: 1,
                    time_base: time_base(),
                }),
                flags: PacketFlags::KEY,
                side_data: Vec::new(),
            },
            access_unit: AccessUnitInfo {
                picture_id: PictureId(u64::try_from(index).unwrap()),
                picture_kind: PictureKind::Intra,
                decode_order: index,
                presentation_order: index,
                references: Vec::new(),
                random_access: RandomAccessKind::Clean,
                parameters: ParameterFingerprint(11),
            },
        }
    }

    fn sequence() -> EditSequence {
        EditSequence {
            time_base: time_base(),
            sources: vec![MediaSource {
                id: SourceId(1),
                locator: "source.dv".into(),
                streams: vec![StreamDescriptor {
                    id: StreamId(2),
                    codec: CodecDescriptor {
                        codec_id: CodecId::new("video/dv"),
                        codec_tag: None,
                        media_type: MediaType::Video,
                        configuration: Vec::new(),
                    },
                    time_base: time_base(),
                }],
            }],
            tracks: vec![Track {
                id: TrackId(0),
                media_type: MediaType::Video,
                clips: vec![Clip {
                    id: ClipId(0),
                    source_id: SourceId(1),
                    source_stream_id: StreamId(2),
                    source_range: range(1, 3),
                    timeline_range: range(0, 2),
                    effects: Vec::new(),
                }],
                transitions: Vec::new(),
            }],
            output: OutputIntent {
                time_base: time_base(),
                container: Some("container/raw-dv".into()),
                video_codec: None,
                audio_codec: None,
            },
        }
    }

    fn packet_source() -> PacketSource {
        PacketSource {
            source_id: SourceId(1),
            stream_id: StreamId(2),
            packets: (0..4).map(analyzed_packet).collect(),
        }
    }

    #[test]
    fn plans_and_executes_packet_aligned_copy() {
        let sources = vec![packet_source()];
        let plan = plan_independent_video(&sequence(), &sources).unwrap();
        assert_eq!(plan.summary.copied_packets, 2);
        assert_eq!(plan.summary.encoded_frames, 0);
        assert_eq!(plan.decisions[0].disposition, RenderDisposition::Copy);

        let output = execute_packet_plan(&plan, &sources).unwrap();
        assert_eq!(output.len(), 2);
        assert_eq!(output[0].data, vec![1]);
        assert_eq!(output[1].data, vec![2]);
        assert_eq!(output[0].pts.unwrap().value, 0);
        assert_eq!(output[1].pts.unwrap().value, 1);
        assert_eq!(output[0].stream_id, StreamId(0));
    }

    #[test]
    fn rejects_effects_in_packet_copy_path() {
        let mut sequence = sequence();
        sequence.tracks[0].clips[0]
            .effects
            .push(mmrecode_edit::Effect {
                kind: "video/fade".into(),
                parameters: BTreeMap::new(),
            });
        assert!(plan_independent_video(&sequence, &[packet_source()]).is_err());
    }

    #[test]
    fn rejects_non_packet_aligned_boundary() {
        let mut sequence = sequence();
        let half = Rational::new(1, 50).unwrap();
        sequence.tracks[0].clips[0].source_range = TimeRange::new(
            Timestamp {
                value: 1,
                time_base: half,
            },
            Timestamp {
                value: 5,
                time_base: half,
            },
        )
        .unwrap();
        sequence.sources[0].streams[0].time_base = half;
        assert!(plan_independent_video(&sequence, &[packet_source()]).is_err());
    }
}
