//! Codec-independent render planning and packet-copy execution.
//!
//! The first vertical slice handles cuts and concatenation of independently coded video packets.
//! Plans remain explicit about copying, timestamp rewriting, regeneration, and muxing so later
//! inter-frame codecs can reuse the operation vocabulary without hiding codec decisions.

use std::ops::Range;

use mmrecode_core::{
    AccessUnitInfo, CodecId, Error, MediaType, Packet, ParameterFingerprint, RandomAccessKind,
    Result, StreamDescriptor, StreamId, Timestamp, TimestampRounding,
};
use mmrecode_edit::{Clip, ClipId, EditSequence, SourceId, TimeRange, Track, TrackId};

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
    /// Packets in presentation order for the independent-frame slice.
    pub packets: Vec<AnalyzedPacket>,
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
        /// Sequence interval whose dependency chain must be regenerated.
        timeline_range: TimeRange,
    },
    /// Encodes a complete changed interval without packet reuse.
    FullEncode {
        /// Clip requiring full encoding.
        clip_id: ClipId,
        /// Sequence interval to encode.
        timeline_range: TimeRange,
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
