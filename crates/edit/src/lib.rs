//! Codec-independent editing intent.
//!
//! This crate records what an edit means: sources, tracks, clips, time ranges, effects,
//! transitions, and an output intent. It does not inspect codec dependencies or choose render
//! operations.

use std::collections::{BTreeMap, BTreeSet};

use mmrecode_core::{
    CodecId, Error, MediaType, Rational, Result, StreamDescriptor, StreamId, Timestamp,
};

mod command;
mod project;
mod project_file;

pub use command::{
    CommandOutput, EDITOR_COMMAND_NAMES, EDITOR_MANUAL_TOPICS, EXPORT_PRESET_NAMES, EditCommand,
    EditorSession, FrameValue, ImportedMedia, PROJECT_SETTING_NAMES, format_compact_timecode,
    parse_command,
};
pub use project::{
    MediaId, MediaKind, MediaLink, MediaLinkId, MediaListing, MediaNode, MediaOrigin, MediaPath,
    MediaProject, ProjectColorSpace, ProjectRateConformPolicy, ProjectRateConformReport,
    ProjectScanMode, ProjectSettings, VisualScaleMode,
};
pub use project_file::{load_project_file, save_project_file, save_project_file_from};

/// Stable identifier for one media source in an edit sequence.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct SourceId(pub u32);

/// Stable identifier for one track in an edit sequence.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct TrackId(pub u32);

/// Stable identifier for one clip in an edit sequence.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ClipId(pub u32);

/// Exact half-open time range whose endpoints share one time base.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TimeRange {
    /// Inclusive start timestamp.
    pub start: Timestamp,
    /// Exclusive end timestamp.
    pub end: Timestamp,
}

impl TimeRange {
    /// Creates a non-empty range with a positive shared time base.
    ///
    /// # Errors
    ///
    /// Returns an error when time bases differ or are not positive, or when the range is empty or
    /// reversed.
    pub fn new(start: Timestamp, end: Timestamp) -> Result<Self> {
        if start.time_base != end.time_base {
            return Err(Error::InvalidData(
                "time-range endpoints must use the same time base".into(),
            ));
        }
        if start.time_base.numerator() <= 0 {
            return Err(Error::InvalidData(
                "time-range time base must be positive".into(),
            ));
        }
        if start.value >= end.value {
            return Err(Error::InvalidData(
                "time range must have positive duration".into(),
            ));
        }
        Ok(Self { start, end })
    }

    /// Returns the range duration in the range time base.
    ///
    /// # Errors
    ///
    /// Returns an error if the range was constructed without validation and its duration
    /// overflows.
    pub fn duration(self) -> Result<Timestamp> {
        let value = self
            .end
            .value
            .checked_sub(self.start.value)
            .ok_or_else(|| Error::InvalidData("time-range duration overflows".into()))?;
        Ok(Timestamp {
            value,
            time_base: self.start.time_base,
        })
    }

    fn contains(self, other: Self) -> bool {
        self.start.time_base == other.start.time_base
            && self.start.value <= other.start.value
            && other.end.value <= self.end.value
    }
}

/// One input media source and its container-discovered streams.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MediaSource {
    /// Source identifier used by clips.
    pub id: SourceId,
    /// Application-defined locator, such as a path, URL, or asset identifier.
    pub locator: String,
    /// Streams available from the source.
    pub streams: Vec<StreamDescriptor>,
}

/// Codec-independent effect request attached to a clip.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Effect {
    /// Stable namespaced effect kind.
    pub kind: String,
    /// Versioned effect parameters represented as strings in the initial slice.
    pub parameters: BTreeMap<String, String>,
}

/// One placement of a source stream on a sequence track.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Clip {
    /// Clip identifier, unique within the sequence.
    pub id: ClipId,
    /// Source containing the referenced stream.
    pub source_id: SourceId,
    /// Stream selected from the source.
    pub source_stream_id: StreamId,
    /// Half-open range in the source stream time base.
    pub source_range: TimeRange,
    /// Half-open placement in the sequence time base.
    pub timeline_range: TimeRange,
    /// Effects evaluated in list order.
    pub effects: Vec<Effect>,
}

/// Codec-independent transition between two clips on the same track.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Transition {
    /// Clip visible before or underneath the transition.
    pub from_clip: ClipId,
    /// Clip visible after or above the transition.
    pub to_clip: ClipId,
    /// Sequence interval occupied by the transition.
    pub timeline_range: TimeRange,
    /// Stable namespaced transition kind.
    pub kind: String,
    /// Versioned transition parameters represented as strings in the initial slice.
    pub parameters: BTreeMap<String, String>,
}

/// Ordered clips of one media type.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Track {
    /// Track identifier.
    pub id: TrackId,
    /// Media type carried by this track.
    pub media_type: MediaType,
    /// Clips placed on the track.
    pub clips: Vec<Clip>,
    /// Transitions between clips on this track.
    pub transitions: Vec<Transition>,
}

/// High-level output request without encoder-specific settings.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OutputIntent {
    /// Desired packet time base for the output timeline.
    pub time_base: Rational,
    /// Optional namespaced container identifier, such as `container/avi`.
    pub container: Option<String>,
    /// Optional desired video codec; `None` permits packet-copy preservation.
    pub video_codec: Option<CodecId>,
    /// Optional desired audio codec; `None` permits packet-copy preservation.
    pub audio_codec: Option<CodecId>,
}

/// Complete codec-independent editing intent.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EditSequence {
    /// Exact time base used by every clip placement and transition.
    pub time_base: Rational,
    /// Referenced media sources.
    pub sources: Vec<MediaSource>,
    /// Media tracks in application-defined compositing/mixing order.
    pub tracks: Vec<Track>,
    /// Requested output characteristics.
    pub output: OutputIntent,
}

impl EditSequence {
    /// Validates identifiers, references, media types, ranges, and time-base ownership.
    ///
    /// This validation establishes structural correctness only. Renderers impose additional
    /// restrictions for paths such as packet-copy-only concatenation.
    ///
    /// # Errors
    ///
    /// Returns an error describing the first structural inconsistency.
    pub fn validate(&self) -> Result<()> {
        if self.time_base.numerator() <= 0 || self.output.time_base.numerator() <= 0 {
            return Err(Error::InvalidData(
                "sequence and output time bases must be positive".into(),
            ));
        }

        let mut source_ids = BTreeSet::new();
        for source in &self.sources {
            if !source_ids.insert(source.id) {
                return Err(Error::InvalidData(format!(
                    "duplicate source identifier {:?}",
                    source.id
                )));
            }
            if source.locator.is_empty() {
                return Err(Error::InvalidData(format!(
                    "source {:?} has an empty locator",
                    source.id
                )));
            }
            let mut stream_ids = BTreeSet::new();
            for stream in &source.streams {
                if !stream_ids.insert(stream.id) {
                    return Err(Error::InvalidData(format!(
                        "source {:?} contains duplicate stream {:?}",
                        source.id, stream.id
                    )));
                }
            }
        }

        let mut track_ids = BTreeSet::new();
        let mut clip_ids = BTreeSet::new();
        for track in &self.tracks {
            if !track_ids.insert(track.id) {
                return Err(Error::InvalidData(format!(
                    "duplicate track identifier {:?}",
                    track.id
                )));
            }
            for clip in &track.clips {
                if !clip_ids.insert(clip.id) {
                    return Err(Error::InvalidData(format!(
                        "duplicate clip identifier {:?}",
                        clip.id
                    )));
                }
                validate_range(clip.source_range)?;
                validate_range(clip.timeline_range)?;
                if clip.timeline_range.start.time_base != self.time_base {
                    return Err(Error::InvalidData(format!(
                        "clip {:?} placement does not use the sequence time base",
                        clip.id
                    )));
                }
                let source = self
                    .sources
                    .iter()
                    .find(|source| source.id == clip.source_id)
                    .ok_or_else(|| {
                        Error::InvalidData(format!(
                            "clip {:?} references missing source {:?}",
                            clip.id, clip.source_id
                        ))
                    })?;
                let stream = source
                    .streams
                    .iter()
                    .find(|stream| stream.id == clip.source_stream_id)
                    .ok_or_else(|| {
                        Error::InvalidData(format!(
                            "clip {:?} references missing stream {:?}",
                            clip.id, clip.source_stream_id
                        ))
                    })?;
                if stream.codec.media_type != track.media_type {
                    return Err(Error::InvalidData(format!(
                        "clip {:?} stream media type does not match track {:?}",
                        clip.id, track.id
                    )));
                }
                if clip.source_range.start.time_base != stream.time_base {
                    return Err(Error::InvalidData(format!(
                        "clip {:?} source range does not use stream {:?} time base",
                        clip.id, stream.id
                    )));
                }
                if clip.effects.iter().any(|effect| effect.kind.is_empty()) {
                    return Err(Error::InvalidData(format!(
                        "clip {:?} contains an effect with an empty kind",
                        clip.id
                    )));
                }
            }
            validate_transitions(track, self.time_base)?;
        }
        Ok(())
    }
}

fn validate_range(range: TimeRange) -> Result<()> {
    TimeRange::new(range.start, range.end).map(|_| ())
}

fn validate_transitions(track: &Track, sequence_time_base: Rational) -> Result<()> {
    for transition in &track.transitions {
        validate_range(transition.timeline_range)?;
        if transition.kind.is_empty() {
            return Err(Error::InvalidData(format!(
                "track {:?} contains a transition with an empty kind",
                track.id
            )));
        }
        if transition.timeline_range.start.time_base != sequence_time_base {
            return Err(Error::InvalidData(format!(
                "track {:?} transition does not use the sequence time base",
                track.id
            )));
        }
        let from = track
            .clips
            .iter()
            .find(|clip| clip.id == transition.from_clip)
            .ok_or_else(|| {
                Error::InvalidData(format!(
                    "track {:?} transition references missing clip {:?}",
                    track.id, transition.from_clip
                ))
            })?;
        let to = track
            .clips
            .iter()
            .find(|clip| clip.id == transition.to_clip)
            .ok_or_else(|| {
                Error::InvalidData(format!(
                    "track {:?} transition references missing clip {:?}",
                    track.id, transition.to_clip
                ))
            })?;
        if !from.timeline_range.contains(transition.timeline_range)
            || !to.timeline_range.contains(transition.timeline_range)
        {
            return Err(Error::InvalidData(format!(
                "track {:?} transition range is not contained by both clips",
                track.id
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use mmrecode_core::{CodecDescriptor, MediaType};

    fn time_base() -> Rational {
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

    fn sequence() -> EditSequence {
        EditSequence {
            time_base: time_base(),
            sources: vec![MediaSource {
                id: SourceId(7),
                locator: "source.dv".into(),
                streams: vec![StreamDescriptor {
                    id: StreamId(3),
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
                id: TrackId(1),
                media_type: MediaType::Video,
                clips: vec![Clip {
                    id: ClipId(2),
                    source_id: SourceId(7),
                    source_stream_id: StreamId(3),
                    source_range: range(4, 8),
                    timeline_range: range(0, 4),
                    effects: Vec::new(),
                }],
                transitions: Vec::new(),
            }],
            output: OutputIntent {
                time_base: time_base(),
                container: None,
                video_codec: None,
                audio_codec: None,
            },
        }
    }

    #[test]
    fn accepts_structurally_valid_sequence() {
        sequence().validate().unwrap();
    }

    #[test]
    fn rejects_wrong_source_time_base() {
        let mut sequence = sequence();
        sequence.tracks[0].clips[0].source_range = TimeRange::new(
            Timestamp {
                value: 4,
                time_base: Rational::new(1, 50).unwrap(),
            },
            Timestamp {
                value: 8,
                time_base: Rational::new(1, 50).unwrap(),
            },
        )
        .unwrap();
        assert!(sequence.validate().is_err());
    }

    #[test]
    fn rejects_duplicate_clip_identifiers_across_tracks() {
        let mut sequence = sequence();
        sequence.tracks.push(sequence.tracks[0].clone());
        sequence.tracks[1].id = TrackId(9);
        assert!(sequence.validate().is_err());
    }
}
