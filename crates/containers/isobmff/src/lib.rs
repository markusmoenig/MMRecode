//! Minimal ISO Base Media File Format and QuickTime/MOV demuxing.
//!
//! The first slice reads complete seekable files, discovers tracks, preserves opaque sample-entry
//! configuration, expands timing/chunk/sample tables, and emits packets without interpreting codec
//! syntax. Fragmented movies remain outside this milestone; a deliberately small single-video-
//! track muxer supports packet-preserving H.264/AAC Fast Start output.

mod mux;

pub use mux::{TrackMuxEdit, TrackMuxInput, mux_tracks, mux_video_track};

use std::{cmp::Ordering, ops::Range};

use mmrecode_core::{
    CodecDescriptor, CodecId, Demuxer, Error, FourCc, MediaType, Packet, PacketFlags, Rational,
    Result, SeekResult, StreamDescriptor, StreamId, Timestamp, TimestampRounding,
};

/// One indexed media sample.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Sample {
    /// Byte range in the source file.
    pub source_range: Range<usize>,
    /// Decode timestamp in track timescale units.
    pub dts: i64,
    /// Presentation timestamp in track timescale units.
    pub pts: i64,
    /// Duration in track timescale units.
    pub duration: u32,
    /// Whether the sample table marks this as a sync sample.
    pub is_sync: bool,
}

/// Pixel aspect ratio from a `pasp` box.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PixelAspectRatio {
    /// Horizontal sample spacing.
    pub horizontal_spacing: u32,
    /// Vertical sample spacing.
    pub vertical_spacing: u32,
}

/// NCLX/NCLC colour declaration from a `colr` box.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ColourInformation {
    /// Colour primaries code.
    pub primaries: u16,
    /// Transfer characteristics code.
    pub transfer_characteristics: u16,
    /// Matrix coefficients code.
    pub matrix_coefficients: u16,
    /// Full-range flag, present only for NCLX.
    pub full_range: Option<bool>,
}

/// Track-level presentation metadata kept outside the generic stream descriptor.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Track {
    /// Generic stream descriptor and opaque codec configuration.
    pub descriptor: StreamDescriptor,
    /// Track header identifier.
    pub track_id: u32,
    /// Media handler type such as `vide` or `soun`.
    pub handler_type: FourCc,
    /// Display width from the sample entry or track header.
    pub width: Option<u32>,
    /// Display height from the sample entry or track header.
    pub height: Option<u32>,
    /// Optional pixel aspect ratio.
    pub pixel_aspect: Option<PixelAspectRatio>,
    /// Optional colour declaration.
    pub colour: Option<ColourInformation>,
    /// Clockwise display rotation in degrees for recognized track matrices.
    pub rotation_degrees: i16,
    /// Audio channel count from the sample entry.
    pub channel_count: Option<u16>,
    /// Audio sample rate from the sample entry.
    pub sample_rate: Option<u32>,
    /// Track presentation duration after applying a supported edit list, in track time-base ticks.
    pub presentation_duration: Option<u64>,
    /// Samples in decode order.
    pub samples: Vec<Sample>,
}

/// Parsed non-fragmented ISO-BMFF/QuickTime file.
#[derive(Clone, Debug)]
pub struct IsoBmffFile {
    data: Vec<u8>,
    tracks: Vec<Track>,
    streams: Vec<StreamDescriptor>,
    packet_order: Vec<(usize, usize)>,
    cursor: usize,
}

impl IsoBmffFile {
    /// Parses a complete seekable `.mp4` or `.mov` file image.
    ///
    /// # Errors
    ///
    /// Returns an error for malformed boxes/sample tables or an unsupported movie organization.
    pub fn parse(data: Vec<u8>) -> Result<Self> {
        let top = boxes_in(&data, 0..data.len())?;
        let moov = top
            .iter()
            .find(|header| header.kind == *b"moov")
            .ok_or_else(|| Error::InvalidData("ISO-BMFF file has no moov box".into()))?;
        if top.iter().any(|header| header.kind == *b"moof") {
            return Err(Error::Unsupported(
                "fragmented ISO-BMFF movies are not implemented".into(),
            ));
        }
        let children = boxes_in(&data, moov.payload.clone())?;
        let movie_timescale = children
            .iter()
            .find(|header| header.kind == *b"mvhd")
            .map(|header| parse_mvhd(&data, header))
            .transpose()?;
        let mut tracks = Vec::new();
        for trak in children.iter().filter(|header| header.kind == *b"trak") {
            match parse_track(&data, trak, tracks.len(), movie_timescale) {
                Ok(track) => tracks.push(track),
                Err(Error::Unsupported(_)) => {}
                Err(error) => return Err(error),
            }
        }
        if tracks.is_empty() {
            return Err(Error::Unsupported(
                "ISO-BMFF movie has no supported sample-table tracks".into(),
            ));
        }
        let streams = tracks
            .iter()
            .map(|track| track.descriptor.clone())
            .collect();
        let mut packet_order = tracks
            .iter()
            .enumerate()
            .flat_map(|(track_index, track)| {
                track
                    .samples
                    .iter()
                    .enumerate()
                    .map(move |(sample_index, _)| (track_index, sample_index))
            })
            .collect::<Vec<_>>();
        packet_order.sort_by(|&(left_track, left_sample), &(right_track, right_sample)| {
            tracks[left_track].samples[left_sample]
                .source_range
                .start
                .cmp(&tracks[right_track].samples[right_sample].source_range.start)
                .then_with(|| left_track.cmp(&right_track))
        });
        Ok(Self {
            data,
            tracks,
            streams,
            packet_order,
            cursor: 0,
        })
    }

    /// Reads and parses a complete file from disk.
    ///
    /// # Errors
    ///
    /// Returns an I/O error or any parsing error reported by [`Self::parse`].
    pub fn open(path: impl AsRef<std::path::Path>) -> Result<Self> {
        Self::parse(std::fs::read(path)?)
    }

    /// Returns parsed tracks and sample indexes.
    #[must_use]
    pub fn tracks(&self) -> &[Track] {
        &self.tracks
    }

    /// Returns the first H.264 video track.
    #[must_use]
    pub fn h264_track(&self) -> Option<&Track> {
        self.tracks
            .iter()
            .find(|track| track.descriptor.codec.codec_id.as_str() == "video/h264")
    }

    /// Returns the first AAC audio track.
    #[must_use]
    pub fn aac_track(&self) -> Option<&Track> {
        self.tracks
            .iter()
            .find(|track| track.descriptor.codec.codec_id.as_str() == "audio/aac")
    }

    /// Returns the source bytes for one indexed sample.
    ///
    /// # Errors
    ///
    /// Returns an error when `sample` does not describe a range inside this file.
    pub fn sample_data(&self, sample: &Sample) -> Result<&[u8]> {
        self.data
            .get(sample.source_range.clone())
            .ok_or_else(|| Error::InvalidData("sample range lies outside source file".into()))
    }
}

impl Demuxer for IsoBmffFile {
    fn streams(&self) -> &[StreamDescriptor] {
        &self.streams
    }

    fn read_packet(&mut self) -> Result<Option<Packet>> {
        let Some(&(track_index, sample_index)) = self.packet_order.get(self.cursor) else {
            return Ok(None);
        };
        self.cursor += 1;
        let track = &self.tracks[track_index];
        let sample = &track.samples[sample_index];
        let mut flags = PacketFlags::empty();
        if sample.is_sync {
            flags.insert(PacketFlags::KEY);
        }
        let time_base = track.descriptor.time_base;
        Ok(Some(Packet {
            stream_id: track.descriptor.id,
            data: self.sample_data(sample)?.to_vec(),
            pts: Some(Timestamp {
                value: sample.pts,
                time_base,
            }),
            dts: Some(Timestamp {
                value: sample.dts,
                time_base,
            }),
            duration: Some(Timestamp {
                value: i64::from(sample.duration),
                time_base,
            }),
            flags,
            side_data: Vec::new(),
        }))
    }

    fn seek(&mut self, target: Timestamp) -> Result<SeekResult> {
        let (track_index, track) = self
            .tracks
            .iter()
            .enumerate()
            .find(|(_, track)| track.descriptor.codec.media_type == MediaType::Video)
            .or_else(|| self.tracks.iter().enumerate().next())
            .ok_or_else(|| Error::InvalidState("ISO-BMFF demuxer has no tracks".into()))?;
        let scaled = target.rescale(track.descriptor.time_base, TimestampRounding::Floor)?;
        let sample_index = track
            .samples
            .iter()
            .enumerate()
            .filter(|(_, sample)| sample.pts <= scaled.value && sample.is_sync)
            .map(|(index, _)| index)
            .next_back()
            .unwrap_or(0);
        let chosen = &track.samples[sample_index];
        self.cursor = self
            .packet_order
            .iter()
            .position(|entry| *entry == (track_index, sample_index))
            .unwrap_or(0);
        Ok(SeekResult {
            requested: target,
            actual: Timestamp {
                value: chosen.pts,
                time_base: track.descriptor.time_base,
            },
        })
    }
}

#[derive(Clone, Debug)]
struct BoxHeader {
    kind: [u8; 4],
    payload: Range<usize>,
}

fn boxes_in(data: &[u8], range: Range<usize>) -> Result<Vec<BoxHeader>> {
    if range.end > data.len() || range.start > range.end {
        return Err(Error::InvalidData("invalid ISO-BMFF box range".into()));
    }
    let mut boxes = Vec::new();
    let mut offset = range.start;
    while offset < range.end {
        if range.end - offset < 8 && data[offset..range.end].iter().all(|&byte| byte == 0) {
            break;
        }
        let base = data.get(offset..offset + 8).ok_or_else(|| {
            Error::InvalidData(format!("truncated ISO-BMFF box header at byte {offset}"))
        })?;
        let size32 = u32::from_be_bytes(base[..4].try_into().expect("four bytes"));
        let kind = base[4..8].try_into().expect("four bytes");
        let (header_size, box_size) = match size32 {
            0 => (8_usize, range.end - offset),
            1 => {
                let extended = read_u64(data, offset + 8, "extended box size")?;
                let size = usize::try_from(extended)
                    .map_err(|_| Error::InvalidData("ISO-BMFF box size exceeds platform".into()))?;
                (16, size)
            }
            size => (8, usize::try_from(size).expect("u32 fits usize")),
        };
        if box_size < header_size {
            return Err(Error::InvalidData(format!(
                "ISO-BMFF box {} at byte {offset} is smaller than its header",
                fourcc_text(kind)
            )));
        }
        let end = offset
            .checked_add(box_size)
            .filter(|end| *end <= range.end)
            .ok_or_else(|| {
                Error::InvalidData(format!(
                    "ISO-BMFF box {} at byte {offset} exceeds its parent",
                    fourcc_text(kind)
                ))
            })?;
        boxes.push(BoxHeader {
            kind,
            payload: offset + header_size..end,
        });
        offset = end;
    }
    Ok(boxes)
}

#[derive(Default)]
struct TrackHeader {
    track_id: u32,
    width: u32,
    height: u32,
    rotation_degrees: i16,
}

#[allow(clippy::too_many_lines)]
fn parse_track(
    data: &[u8],
    trak: &BoxHeader,
    ordinal: usize,
    movie_timescale: Option<u32>,
) -> Result<Track> {
    let children = boxes_in(data, trak.payload.clone())?;
    let tkhd = children
        .iter()
        .find(|header| header.kind == *b"tkhd")
        .map_or_else(
            || Ok(TrackHeader::default()),
            |header| parse_tkhd(data, header),
        )?;
    let mdia = child(&children, *b"mdia", "trak")?;
    let mdia_children = boxes_in(data, mdia.payload.clone())?;
    let timescale = parse_mdhd(data, child(&mdia_children, *b"mdhd", "mdia")?)?;
    if timescale == 0 {
        return Err(Error::InvalidData(
            "ISO-BMFF track timescale is zero".into(),
        ));
    }
    let edit = parse_track_edit(data, &children, movie_timescale, timescale)?;
    let handler_type = parse_hdlr(data, child(&mdia_children, *b"hdlr", "mdia")?)?;
    let media_type = match &handler_type {
        b"vide" => MediaType::Video,
        b"soun" => MediaType::Audio,
        _ => MediaType::Data,
    };
    let minf = child(&mdia_children, *b"minf", "mdia")?;
    let minf_children = boxes_in(data, minf.payload.clone())?;
    let stbl = child(&minf_children, *b"stbl", "minf")?;
    let table_boxes = boxes_in(data, stbl.payload.clone())?;
    let sample_entry = parse_stsd(data, child(&table_boxes, *b"stsd", "stbl")?, media_type)?;
    let durations = parse_stts(data, child(&table_boxes, *b"stts", "stbl")?)?;
    let composition_offsets = table_boxes
        .iter()
        .find(|header| header.kind == *b"ctts")
        .map_or_else(|| Ok(Vec::new()), |header| parse_ctts(data, header))?;
    let chunk_map = parse_stsc(data, child(&table_boxes, *b"stsc", "stbl")?)?;
    let sample_sizes = parse_stsz(data, child(&table_boxes, *b"stsz", "stbl")?)?;
    if durations.len() != sample_sizes.len() {
        return Err(Error::InvalidData(format!(
            "ISO-BMFF stts has {} samples but stsz has {}",
            durations.len(),
            sample_sizes.len()
        )));
    }
    if !composition_offsets.is_empty() && composition_offsets.len() != sample_sizes.len() {
        return Err(Error::InvalidData(
            "ISO-BMFF ctts sample count differs from stsz".into(),
        ));
    }
    let chunk_offsets =
        if let Some(stco) = table_boxes.iter().find(|header| header.kind == *b"stco") {
            parse_chunk_offsets(data, stco, false)?
        } else {
            parse_chunk_offsets(data, child(&table_boxes, *b"co64", "stbl")?, true)?
        };
    let sync_samples = table_boxes
        .iter()
        .find(|header| header.kind == *b"stss")
        .map(|header| parse_stss(data, header))
        .transpose()?;
    let ranges = expand_sample_offsets(&chunk_offsets, &chunk_map, &sample_sizes, data.len())?;
    let mut dts = edit.timestamp_offset;
    let samples = ranges
        .into_iter()
        .zip(durations)
        .enumerate()
        .map(|(index, (source_range, duration))| {
            let composition_offset = composition_offsets.get(index).copied().unwrap_or(0);
            let pts = dts.checked_add(composition_offset).ok_or_else(|| {
                Error::InvalidData("ISO-BMFF composition timestamp overflows".into())
            })?;
            let sample = Sample {
                source_range,
                dts,
                pts,
                duration,
                is_sync: sync_samples.as_ref().is_none_or(|samples| {
                    samples
                        .binary_search(&(u32::try_from(index).unwrap_or(u32::MAX) + 1))
                        .is_ok()
                }),
            };
            dts = dts
                .checked_add(i64::from(duration))
                .ok_or_else(|| Error::InvalidData("ISO-BMFF decode timestamp overflows".into()))?;
            Ok(sample)
        })
        .collect::<Result<Vec<_>>>()?;
    if samples.is_empty() {
        return Err(Error::Unsupported(
            "ISO-BMFF track has no media samples".into(),
        ));
    }
    let track_id = if tkhd.track_id == 0 {
        u32::try_from(ordinal + 1)
            .map_err(|_| Error::InvalidData("too many ISO-BMFF tracks".into()))?
    } else {
        tkhd.track_id
    };
    let codec_id = codec_id(sample_entry.format, media_type);
    let width = sample_entry
        .width
        .or((tkhd.width > 0).then_some(tkhd.width));
    let height = sample_entry
        .height
        .or((tkhd.height > 0).then_some(tkhd.height));
    let descriptor = StreamDescriptor {
        id: StreamId(track_id),
        codec: CodecDescriptor {
            codec_id,
            codec_tag: Some(FourCc(sample_entry.format)),
            media_type,
            configuration: sample_entry.configuration,
        },
        time_base: Rational::new(1, i64::from(timescale))?,
    };
    Ok(Track {
        descriptor,
        track_id,
        handler_type: FourCc(handler_type),
        width,
        height,
        pixel_aspect: sample_entry.pixel_aspect,
        colour: sample_entry.colour,
        rotation_degrees: tkhd.rotation_degrees,
        channel_count: sample_entry.channel_count,
        sample_rate: sample_entry.sample_rate,
        presentation_duration: edit.presentation_duration,
        samples,
    })
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct TrackEdit {
    timestamp_offset: i64,
    presentation_duration: Option<u64>,
}

fn parse_track_edit(
    data: &[u8],
    track_children: &[BoxHeader],
    movie_timescale: Option<u32>,
    track_timescale: u32,
) -> Result<TrackEdit> {
    let Some(edts) = track_children.iter().find(|header| header.kind == *b"edts") else {
        return Ok(TrackEdit::default());
    };
    let movie_timescale = movie_timescale.ok_or_else(|| {
        Error::InvalidData(
            "ISO-BMFF track has an edit list but movie timescale is unavailable".into(),
        )
    })?;
    if movie_timescale == 0 {
        return Err(Error::InvalidData(
            "ISO-BMFF movie timescale is zero".into(),
        ));
    }
    let children = boxes_in(data, edts.payload.clone())?;
    let elst = child(&children, *b"elst", "edts")?;
    parse_elst(payload(data, elst)?, movie_timescale, track_timescale)
}

fn parse_elst(data: &[u8], movie_timescale: u32, track_timescale: u32) -> Result<TrackEdit> {
    let version = *data
        .first()
        .ok_or_else(|| Error::InvalidData("empty elst".into()))?;
    if version > 1 {
        return Err(Error::Unsupported(format!(
            "ISO-BMFF elst version {version} is not implemented"
        )));
    }
    let entry_count =
        usize::try_from(read_u32(data, 4, "elst entry count")?).expect("u32 fits usize");
    let entry_size = if version == 1 { 20 } else { 12 };
    let mut offset = 8_usize;
    let mut empty_duration = 0_u64;
    let mut presentation_duration = 0_u64;
    let mut media_time = None;
    for _ in 0..entry_count {
        let segment_duration = if version == 1 {
            read_u64(data, offset, "elst segment duration")?
        } else {
            u64::from(read_u32(data, offset, "elst segment duration")?)
        };
        let time_offset = if version == 1 { 8 } else { 4 };
        let current_media_time = if version == 1 {
            read_i64(data, offset + time_offset, "elst media time")?
        } else {
            i64::from(read_i32(data, offset + time_offset, "elst media time")?)
        };
        let rate_offset = offset + time_offset + if version == 1 { 8 } else { 4 };
        let rate_integer = read_i16(data, rate_offset, "elst media rate integer")?;
        let rate_fraction = read_i16(data, rate_offset + 2, "elst media rate fraction")?;
        if rate_integer != 1 || rate_fraction != 0 {
            return Err(Error::Unsupported(
                "ISO-BMFF edit-list media rates other than 1.0 are not implemented".into(),
            ));
        }
        presentation_duration = presentation_duration
            .checked_add(segment_duration)
            .ok_or_else(|| Error::InvalidData("elst presentation duration overflows".into()))?;
        if current_media_time == -1 {
            if media_time.is_some() {
                return Err(Error::Unsupported(
                    "ISO-BMFF empty edits after media edits are not implemented".into(),
                ));
            }
            empty_duration = empty_duration
                .checked_add(segment_duration)
                .ok_or_else(|| Error::InvalidData("elst empty-edit duration overflows".into()))?;
        } else if current_media_time >= 0 && media_time.replace(current_media_time).is_some() {
            return Err(Error::Unsupported(
                "multiple ISO-BMFF media edits are not implemented".into(),
            ));
        } else if current_media_time < -1 {
            return Err(Error::InvalidData(format!(
                "invalid negative elst media time {current_media_time}"
            )));
        }
        offset = offset
            .checked_add(entry_size)
            .ok_or_else(|| Error::InvalidData("elst entry offset overflows".into()))?;
    }
    let media_time = media_time.ok_or_else(|| {
        Error::Unsupported("ISO-BMFF edit list has no playable media edit".into())
    })?;
    let empty_ticks = rescale_edit_duration(empty_duration, movie_timescale, track_timescale)?;
    let duration_ticks =
        rescale_edit_duration(presentation_duration, movie_timescale, track_timescale)?;
    let timestamp_offset = i64::try_from(empty_ticks)
        .map_err(|_| Error::InvalidData("elst empty-edit duration exceeds i64".into()))?
        .checked_sub(media_time)
        .ok_or_else(|| Error::InvalidData("elst timestamp offset overflows".into()))?;
    Ok(TrackEdit {
        timestamp_offset,
        presentation_duration: Some(duration_ticks),
    })
}

fn rescale_edit_duration(value: u64, source_scale: u32, target_scale: u32) -> Result<u64> {
    let numerator = u128::from(value)
        .checked_mul(u128::from(target_scale))
        .ok_or_else(|| Error::InvalidData("elst duration rescale overflows".into()))?;
    let rounded = numerator
        .checked_add(u128::from(source_scale) / 2)
        .ok_or_else(|| Error::InvalidData("elst duration rounding overflows".into()))?
        / u128::from(source_scale);
    u64::try_from(rounded)
        .map_err(|_| Error::InvalidData("elst duration exceeds track time base".into()))
}

fn codec_id(format: [u8; 4], media_type: MediaType) -> CodecId {
    match &format {
        b"avc1" | b"avc3" => CodecId::new("video/h264"),
        b"mp4a" => CodecId::new("audio/aac"),
        _ => CodecId::new(format!(
            "{}/isobmff-{}",
            match media_type {
                MediaType::Video => "video",
                MediaType::Audio => "audio",
                _ => "data",
            },
            fourcc_text(format).to_ascii_lowercase()
        )),
    }
}

fn child<'a>(children: &'a [BoxHeader], kind: [u8; 4], parent: &str) -> Result<&'a BoxHeader> {
    children
        .iter()
        .find(|header| header.kind == kind)
        .ok_or_else(|| {
            Error::InvalidData(format!(
                "ISO-BMFF {parent} has no required {} box",
                fourcc_text(kind)
            ))
        })
}

fn parse_tkhd(data: &[u8], header: &BoxHeader) -> Result<TrackHeader> {
    let bytes = payload(data, header)?;
    let version = *bytes
        .first()
        .ok_or_else(|| Error::InvalidData("empty tkhd".into()))?;
    let (track_id_offset, matrix_offset) = if version == 1 { (20, 52) } else { (12, 40) };
    let track_id = read_u32(bytes, track_id_offset, "tkhd track ID")?;
    let matrix = bytes
        .get(matrix_offset..matrix_offset + 36)
        .ok_or_else(|| Error::InvalidData("truncated tkhd transformation matrix".into()))?;
    let a = read_i32(matrix, 0, "tkhd matrix a")?;
    let b = read_i32(matrix, 4, "tkhd matrix b")?;
    let c = read_i32(matrix, 12, "tkhd matrix c")?;
    let d = read_i32(matrix, 16, "tkhd matrix d")?;
    let rotation_degrees = match (a.signum(), b.signum(), c.signum(), d.signum()) {
        (1, 0, 0, 1) => 0,
        (0, 1, -1, 0) => 90,
        (-1, 0, 0, -1) => 180,
        (0, -1, 1, 0) => 270,
        _ => 0,
    };
    let width_offset = bytes
        .len()
        .checked_sub(8)
        .ok_or_else(|| Error::InvalidData("truncated tkhd".into()))?;
    let width = read_u32(bytes, width_offset, "tkhd width")? >> 16;
    let height = read_u32(bytes, width_offset + 4, "tkhd height")? >> 16;
    Ok(TrackHeader {
        track_id,
        width,
        height,
        rotation_degrees,
    })
}

fn parse_mdhd(data: &[u8], header: &BoxHeader) -> Result<u32> {
    let bytes = payload(data, header)?;
    let version = *bytes
        .first()
        .ok_or_else(|| Error::InvalidData("empty mdhd".into()))?;
    read_u32(bytes, if version == 1 { 20 } else { 12 }, "mdhd timescale")
}

fn parse_mvhd(data: &[u8], header: &BoxHeader) -> Result<u32> {
    let bytes = payload(data, header)?;
    let version = *bytes
        .first()
        .ok_or_else(|| Error::InvalidData("empty mvhd".into()))?;
    let timescale = read_u32(bytes, if version == 1 { 20 } else { 12 }, "mvhd timescale")?;
    if timescale == 0 {
        return Err(Error::InvalidData(
            "ISO-BMFF movie timescale is zero".into(),
        ));
    }
    Ok(timescale)
}

fn parse_hdlr(data: &[u8], header: &BoxHeader) -> Result<[u8; 4]> {
    let bytes = payload(data, header)?;
    bytes
        .get(8..12)
        .and_then(|value| value.try_into().ok())
        .ok_or_else(|| Error::InvalidData("truncated hdlr handler type".into()))
}

#[derive(Default)]
struct SampleEntry {
    format: [u8; 4],
    configuration: Vec<u8>,
    width: Option<u32>,
    height: Option<u32>,
    pixel_aspect: Option<PixelAspectRatio>,
    colour: Option<ColourInformation>,
    channel_count: Option<u16>,
    sample_rate: Option<u32>,
}

fn parse_stsd(data: &[u8], header: &BoxHeader, media_type: MediaType) -> Result<SampleEntry> {
    let bytes = payload(data, header)?;
    let entry_count = read_u32(bytes, 4, "stsd entry count")?;
    if entry_count == 0 {
        return Err(Error::InvalidData(
            "ISO-BMFF stsd has no sample entry".into(),
        ));
    }
    if entry_count != 1 {
        return Err(Error::Unsupported(
            "multiple ISO-BMFF sample descriptions are not implemented".into(),
        ));
    }
    let entry_size =
        usize::try_from(read_u32(bytes, 8, "sample-entry size")?).expect("u32 fits usize");
    if entry_size < 16 || 8 + entry_size > bytes.len() {
        return Err(Error::InvalidData(
            "invalid ISO-BMFF sample-entry size".into(),
        ));
    }
    let format: [u8; 4] = bytes[12..16].try_into().expect("four bytes");
    let entry_payload = &bytes[16..8 + entry_size];
    let mut result = SampleEntry {
        format,
        ..SampleEntry::default()
    };
    let child_start = match media_type {
        MediaType::Video => {
            result.width = Some(u32::from(read_u16(entry_payload, 24, "visual width")?));
            result.height = Some(u32::from(read_u16(entry_payload, 26, "visual height")?));
            78
        }
        MediaType::Audio => {
            result.channel_count = Some(read_u16(entry_payload, 16, "audio channel count")?);
            result.sample_rate = Some(read_u32(entry_payload, 24, "audio sample rate")? >> 16);
            28
        }
        _ => 8,
    };
    if child_start <= entry_payload.len() {
        for child in boxes_in(entry_payload, child_start..entry_payload.len())? {
            match &child.kind {
                b"avcC" | b"hvcC" | b"av1C" => {
                    result.configuration = entry_payload[child.payload].to_vec();
                }
                b"esds" if format == *b"mp4a" => {
                    result.configuration =
                        parse_esds_audio_specific_config(&entry_payload[child.payload])?.to_vec();
                }
                b"pasp" => {
                    let value = &entry_payload[child.payload];
                    let horizontal_spacing = read_u32(value, 0, "pasp hSpacing")?;
                    let vertical_spacing = read_u32(value, 4, "pasp vSpacing")?;
                    if horizontal_spacing > 0 && vertical_spacing > 0 {
                        result.pixel_aspect = Some(PixelAspectRatio {
                            horizontal_spacing,
                            vertical_spacing,
                        });
                    }
                }
                b"colr" => result.colour = parse_colr(&entry_payload[child.payload])?,
                _ => {}
            }
        }
    }
    Ok(result)
}

fn parse_esds_audio_specific_config(data: &[u8]) -> Result<&[u8]> {
    let descriptors = data
        .get(4..)
        .ok_or_else(|| Error::InvalidData("truncated esds full-box header".into()))?;
    let (tag, es) = descriptor(descriptors, "esds ES_Descriptor")?;
    if tag != 0x03 {
        return Err(Error::InvalidData(format!(
            "esds begins with descriptor tag 0x{tag:02x}, expected ES_Descriptor"
        )));
    }
    let flags = *es
        .get(2)
        .ok_or_else(|| Error::InvalidData("truncated esds ES_Descriptor header".into()))?;
    let mut offset = 3_usize;
    if flags & 0x80 != 0 {
        offset = offset.saturating_add(2);
    }
    if flags & 0x40 != 0 {
        let url_length = usize::from(
            *es.get(offset)
                .ok_or_else(|| Error::InvalidData("truncated esds URL length".into()))?,
        );
        offset = offset.saturating_add(1).saturating_add(url_length);
    }
    if flags & 0x20 != 0 {
        offset = offset.saturating_add(2);
    }
    let decoder_bytes = es
        .get(offset..)
        .ok_or_else(|| Error::InvalidData("truncated esds ES_Descriptor flags".into()))?;
    let (tag, decoder) = descriptor(decoder_bytes, "esds DecoderConfigDescriptor")?;
    if tag != 0x04 {
        return Err(Error::InvalidData(format!(
            "esds contains descriptor tag 0x{tag:02x}, expected DecoderConfigDescriptor"
        )));
    }
    let children = decoder
        .get(13..)
        .ok_or_else(|| Error::InvalidData("truncated esds DecoderConfigDescriptor".into()))?;
    let (tag, config) = descriptor(children, "esds DecoderSpecificInfo")?;
    if tag != 0x05 {
        return Err(Error::InvalidData(format!(
            "esds contains descriptor tag 0x{tag:02x}, expected DecoderSpecificInfo"
        )));
    }
    if config.is_empty() {
        return Err(Error::InvalidData(
            "esds DecoderSpecificInfo is empty".into(),
        ));
    }
    Ok(config)
}

fn descriptor<'a>(data: &'a [u8], name: &str) -> Result<(u8, &'a [u8])> {
    let tag = *data
        .first()
        .ok_or_else(|| Error::InvalidData(format!("truncated {name} tag")))?;
    let mut length = 0_usize;
    let mut length_bytes = 0_usize;
    for byte in data.get(1..).unwrap_or_default().iter().take(4) {
        length = length
            .checked_mul(128)
            .and_then(|value| value.checked_add(usize::from(byte & 0x7f)))
            .ok_or_else(|| Error::InvalidData(format!("{name} length overflows")))?;
        length_bytes += 1;
        if byte & 0x80 == 0 {
            let start = 1 + length_bytes;
            let end = start
                .checked_add(length)
                .ok_or_else(|| Error::InvalidData(format!("{name} range overflows")))?;
            let payload = data
                .get(start..end)
                .ok_or_else(|| Error::InvalidData(format!("truncated {name} payload")))?;
            return Ok((tag, payload));
        }
    }
    Err(Error::InvalidData(format!("invalid {name} length")))
}

fn parse_colr(data: &[u8]) -> Result<Option<ColourInformation>> {
    let Some(kind) = data.get(..4) else {
        return Ok(None);
    };
    if kind != b"nclx" && kind != b"nclc" {
        return Ok(None);
    }
    let primaries = read_u16(data, 4, "colr primaries")?;
    let transfer_characteristics = read_u16(data, 6, "colr transfer")?;
    let matrix_coefficients = read_u16(data, 8, "colr matrix")?;
    let full_range = (kind == b"nclx").then(|| data.get(10).is_some_and(|byte| byte & 0x80 != 0));
    Ok(Some(ColourInformation {
        primaries,
        transfer_characteristics,
        matrix_coefficients,
        full_range,
    }))
}

fn parse_stts(data: &[u8], header: &BoxHeader) -> Result<Vec<u32>> {
    let bytes = payload(data, header)?;
    let count = entry_count(bytes, "stts")?;
    let mut durations = Vec::new();
    let mut offset = 8;
    for _ in 0..count {
        let sample_count = read_u32(bytes, offset, "stts sample count")?;
        let sample_delta = read_u32(bytes, offset + 4, "stts sample delta")?;
        let new_len = durations
            .len()
            .checked_add(usize::try_from(sample_count).expect("u32 fits usize"))
            .ok_or_else(|| Error::InvalidData("stts sample count overflows".into()))?;
        if new_len > 100_000_000 {
            return Err(Error::Unsupported(
                "ISO-BMFF track has over 100 million samples".into(),
            ));
        }
        durations.resize(new_len, sample_delta);
        offset += 8;
    }
    Ok(durations)
}

fn parse_ctts(data: &[u8], header: &BoxHeader) -> Result<Vec<i64>> {
    let bytes = payload(data, header)?;
    let version = *bytes
        .first()
        .ok_or_else(|| Error::InvalidData("empty ctts".into()))?;
    let count = entry_count(bytes, "ctts")?;
    let mut offsets = Vec::new();
    let mut offset = 8;
    for _ in 0..count {
        let sample_count = read_u32(bytes, offset, "ctts sample count")?;
        let raw = read_u32(bytes, offset + 4, "ctts sample offset")?;
        let value = if version == 1 {
            i64::from(i32::from_be_bytes(raw.to_be_bytes()))
        } else {
            i64::from(raw)
        };
        let new_len = offsets
            .len()
            .checked_add(usize::try_from(sample_count).expect("u32 fits usize"))
            .ok_or_else(|| Error::InvalidData("ctts sample count overflows".into()))?;
        if new_len > 100_000_000 {
            return Err(Error::Unsupported(
                "ISO-BMFF track has over 100 million samples".into(),
            ));
        }
        offsets.resize(new_len, value);
        offset += 8;
    }
    Ok(offsets)
}

#[derive(Clone, Copy)]
struct SampleToChunk {
    first_chunk: u32,
    samples_per_chunk: u32,
}

fn parse_stsc(data: &[u8], header: &BoxHeader) -> Result<Vec<SampleToChunk>> {
    let bytes = payload(data, header)?;
    let count = entry_count(bytes, "stsc")?;
    let mut entries = Vec::with_capacity(count);
    let mut offset = 8;
    for _ in 0..count {
        let first_chunk = read_u32(bytes, offset, "stsc first chunk")?;
        let samples_per_chunk = read_u32(bytes, offset + 4, "stsc samples per chunk")?;
        let _sample_description_index = read_u32(bytes, offset + 8, "stsc description index")?;
        if first_chunk == 0 || samples_per_chunk == 0 {
            return Err(Error::InvalidData(
                "stsc uses zero chunk/sample index".into(),
            ));
        }
        entries.push(SampleToChunk {
            first_chunk,
            samples_per_chunk,
        });
        offset += 12;
    }
    if entries.first().is_none_or(|entry| entry.first_chunk != 1)
        || !entries
            .windows(2)
            .all(|pair| pair[0].first_chunk < pair[1].first_chunk)
    {
        return Err(Error::InvalidData("invalid ISO-BMFF stsc ordering".into()));
    }
    Ok(entries)
}

fn parse_stsz(data: &[u8], header: &BoxHeader) -> Result<Vec<u32>> {
    let bytes = payload(data, header)?;
    let uniform_size = read_u32(bytes, 4, "stsz sample size")?;
    let sample_count =
        usize::try_from(read_u32(bytes, 8, "stsz sample count")?).expect("u32 fits usize");
    if sample_count > 100_000_000 {
        return Err(Error::Unsupported(
            "ISO-BMFF track has over 100 million samples".into(),
        ));
    }
    if uniform_size != 0 {
        return Ok(vec![uniform_size; sample_count]);
    }
    (0..sample_count)
        .map(|index| read_u32(bytes, 12 + index * 4, "stsz entry"))
        .collect()
}

fn parse_chunk_offsets(data: &[u8], header: &BoxHeader, wide: bool) -> Result<Vec<u64>> {
    let bytes = payload(data, header)?;
    let count = entry_count(bytes, if wide { "co64" } else { "stco" })?;
    let stride = if wide { 8 } else { 4 };
    (0..count)
        .map(|index| {
            let offset = 8 + index * stride;
            if wide {
                read_u64(bytes, offset, "co64 entry")
            } else {
                read_u32(bytes, offset, "stco entry").map(u64::from)
            }
        })
        .collect()
}

fn parse_stss(data: &[u8], header: &BoxHeader) -> Result<Vec<u32>> {
    let bytes = payload(data, header)?;
    let count = entry_count(bytes, "stss")?;
    let mut entries = (0..count)
        .map(|index| read_u32(bytes, 8 + index * 4, "stss entry"))
        .collect::<Result<Vec<_>>>()?;
    entries.sort_unstable();
    entries.dedup();
    Ok(entries)
}

fn expand_sample_offsets(
    chunks: &[u64],
    mapping: &[SampleToChunk],
    sample_sizes: &[u32],
    file_len: usize,
) -> Result<Vec<Range<usize>>> {
    let mut ranges = Vec::with_capacity(sample_sizes.len());
    let mut sample_index = 0;
    for (chunk_index, &chunk_offset) in chunks.iter().enumerate() {
        let chunk_number = u32::try_from(chunk_index + 1)
            .map_err(|_| Error::InvalidData("ISO-BMFF chunk index overflows".into()))?;
        let map = mapping
            .iter()
            .rev()
            .find(|entry| entry.first_chunk <= chunk_number)
            .ok_or_else(|| Error::InvalidData("stsc does not describe a chunk".into()))?;
        let mut offset = usize::try_from(chunk_offset)
            .map_err(|_| Error::InvalidData("chunk offset exceeds platform".into()))?;
        for _ in 0..map.samples_per_chunk {
            let size = usize::try_from(*sample_sizes.get(sample_index).ok_or_else(|| {
                Error::InvalidData("stsc describes more samples than stsz".into())
            })?)
            .expect("u32 fits usize");
            let end = offset
                .checked_add(size)
                .filter(|end| *end <= file_len)
                .ok_or_else(|| Error::InvalidData("sample lies outside ISO-BMFF file".into()))?;
            ranges.push(offset..end);
            offset = end;
            sample_index += 1;
        }
    }
    match sample_index.cmp(&sample_sizes.len()) {
        Ordering::Equal => Ok(ranges),
        Ordering::Less => Err(Error::InvalidData(
            "stsc describes fewer samples than stsz".into(),
        )),
        Ordering::Greater => unreachable!("extra samples rejected while expanding"),
    }
}

fn payload<'a>(data: &'a [u8], header: &BoxHeader) -> Result<&'a [u8]> {
    data.get(header.payload.clone())
        .ok_or_else(|| Error::InvalidData("ISO-BMFF box payload lies outside file".into()))
}

fn entry_count(bytes: &[u8], name: &str) -> Result<usize> {
    usize::try_from(read_u32(bytes, 4, &format!("{name} entry count"))?)
        .map_err(|_| Error::InvalidData(format!("{name} entry count exceeds platform")))
}

fn read_u16(data: &[u8], offset: usize, name: &str) -> Result<u16> {
    let bytes = data
        .get(offset..offset + 2)
        .ok_or_else(|| Error::InvalidData(format!("truncated {name}")))?;
    Ok(u16::from_be_bytes(bytes.try_into().expect("two bytes")))
}

fn read_u32(data: &[u8], offset: usize, name: &str) -> Result<u32> {
    let bytes = data
        .get(offset..offset + 4)
        .ok_or_else(|| Error::InvalidData(format!("truncated {name}")))?;
    Ok(u32::from_be_bytes(bytes.try_into().expect("four bytes")))
}

fn read_i32(data: &[u8], offset: usize, name: &str) -> Result<i32> {
    read_u32(data, offset, name).map(|value| i32::from_be_bytes(value.to_be_bytes()))
}

fn read_i16(data: &[u8], offset: usize, name: &str) -> Result<i16> {
    let bytes = data
        .get(offset..offset + 2)
        .ok_or_else(|| Error::InvalidData(format!("truncated {name}")))?;
    Ok(i16::from_be_bytes(bytes.try_into().expect("two bytes")))
}

fn read_i64(data: &[u8], offset: usize, name: &str) -> Result<i64> {
    read_u64(data, offset, name).map(|value| i64::from_be_bytes(value.to_be_bytes()))
}

fn read_u64(data: &[u8], offset: usize, name: &str) -> Result<u64> {
    let bytes = data
        .get(offset..offset + 8)
        .ok_or_else(|| Error::InvalidData(format!("truncated {name}")))?;
    Ok(u64::from_be_bytes(bytes.try_into().expect("eight bytes")))
}

fn fourcc_text(value: [u8; 4]) -> String {
    value
        .iter()
        .map(|byte| {
            if byte.is_ascii_graphic() {
                char::from(*byte)
            } else {
                '?'
            }
        })
        .collect()
}

#[cfg(test)]
#[allow(clippy::similar_names)]
mod tests {
    use mmrecode_core::{Demuxer, PacketFlags, Rational, Timestamp};

    use super::{IsoBmffFile, boxes_in, parse_elst, parse_esds_audio_specific_config};

    #[test]
    fn accepts_zero_alignment_after_sample_entry_children() {
        let mut data = atom(*b"avcC", vec![1, 2, 3]);
        data.extend([0; 4]);
        let boxes = boxes_in(&data, 0..data.len()).unwrap();
        assert_eq!(boxes.len(), 1);
        assert_eq!(boxes[0].kind, *b"avcC");

        let last = data.len() - 1;
        data[last] = 1;
        assert!(boxes_in(&data, 0..data.len()).is_err());
    }

    #[test]
    fn parses_timed_h264_samples_and_seeks_to_sync_sample() {
        let file = tiny_avc_file();
        let mut movie = IsoBmffFile::parse(file).unwrap();
        let track = movie.h264_track().unwrap();
        assert_eq!((track.width, track.height), (Some(16), Some(16)));
        assert_eq!(track.samples.len(), 3);
        assert_eq!((track.samples[1].dts, track.samples[1].pts), (1000, 2000));
        let first = movie.read_packet().unwrap().unwrap();
        assert!(first.flags.contains(PacketFlags::KEY));
        let seek = movie
            .seek(Timestamp {
                value: 2500,
                time_base: Rational::new(1, 1000).unwrap(),
            })
            .unwrap();
        assert_eq!(seek.actual.value, 0);
    }

    #[test]
    fn extracts_audio_specific_config_from_esds() {
        let esds = [
            0, 0, 0, 0, // FullBox
            0x03, 0x19, 0, 1, 0, // ES descriptor
            0x04, 0x11, 0x40, 0x15, 0, 0, 0, 0, 1, 0xf4, 0, 0, 1, 0xf4, 0, // decoder
            0x05, 0x02, 0x12, 0x10, // AAC-LC, 44.1 kHz, stereo
            0x06, 0x01, 0x02,
        ];
        assert_eq!(
            parse_esds_audio_specific_config(&esds).unwrap(),
            [0x12, 0x10]
        );

        let mut truncated = esds;
        truncated[5] = 0x7f;
        assert!(parse_esds_audio_specific_config(&truncated).is_err());
    }

    #[test]
    fn maps_empty_and_media_edits_into_track_timestamps() {
        let mut elst = vec![0; 4];
        elst.extend(2_u32.to_be_bytes());
        elst.extend(829_u32.to_be_bytes());
        elst.extend((-1_i32).to_be_bytes());
        elst.extend(1_i16.to_be_bytes());
        elst.extend(0_i16.to_be_bytes());
        elst.extend(1_129_472_u32.to_be_bytes());
        elst.extend(2_112_i32.to_be_bytes());
        elst.extend(1_i16.to_be_bytes());
        elst.extend(0_i16.to_be_bytes());

        let edit = parse_elst(&elst, 44_100, 44_100).unwrap();
        assert_eq!(edit.timestamp_offset, -1_283);
        assert_eq!(edit.presentation_duration, Some(1_130_301));
    }

    fn tiny_avc_file() -> Vec<u8> {
        let samples = vec![
            vec![0, 0, 0, 1, 0x65],
            vec![0, 0, 0, 1, 0x41],
            vec![0, 0, 0, 1, 0x41],
        ];
        let mdat_payload = samples.concat();
        let ftyp = atom(*b"ftyp", b"isom\0\0\0\0isom".to_vec());
        let mdat_offset = ftyp.len() + 8;
        let stsd_entry = {
            let mut payload = vec![0; 78];
            payload[24..26].copy_from_slice(&16_u16.to_be_bytes());
            payload[26..28].copy_from_slice(&16_u16.to_be_bytes());
            payload.extend(atom(*b"avcC", vec![1, 66, 0, 30, 0xff, 0xe0, 0]));
            atom(*b"avc1", payload)
        };
        let mut stsd_payload = vec![0; 4];
        stsd_payload.extend(1_u32.to_be_bytes());
        stsd_payload.extend(stsd_entry);
        let stsd = atom(*b"stsd", stsd_payload);
        let stts = full_table(*b"stts", &[(3, 1000)]);
        let ctts = full_table(*b"ctts", &[(1, 0), (1, 1000), (1, 0)]);
        let stsc = full_table3(*b"stsc", &[(1, 3, 1)]);
        let mut stsz_payload = vec![0; 4];
        stsz_payload.extend(0_u32.to_be_bytes());
        stsz_payload.extend(3_u32.to_be_bytes());
        for sample in &samples {
            stsz_payload.extend(u32::try_from(sample.len()).unwrap().to_be_bytes());
        }
        let stsz = atom(*b"stsz", stsz_payload);
        let stco = full_values(*b"stco", &[u32::try_from(mdat_offset).unwrap()]);
        let stss = full_values(*b"stss", &[1]);
        let stbl = atom(
            *b"stbl",
            [stsd, stts, ctts, stsc, stsz, stco, stss].concat(),
        );
        let minf = atom(*b"minf", stbl);
        let mut mdhd_payload = vec![0; 12];
        mdhd_payload.extend(1000_u32.to_be_bytes());
        mdhd_payload.extend(3000_u32.to_be_bytes());
        let mdhd = atom(*b"mdhd", mdhd_payload);
        let mut hdlr_payload = vec![0; 8];
        hdlr_payload.extend(b"vide");
        let hdlr = atom(*b"hdlr", hdlr_payload);
        let mdia = atom(*b"mdia", [mdhd, hdlr, minf].concat());
        let mut tkhd_payload = vec![0; 84];
        tkhd_payload[12..16].copy_from_slice(&1_u32.to_be_bytes());
        tkhd_payload[40..44].copy_from_slice(&0x0001_0000_u32.to_be_bytes());
        tkhd_payload[56..60].copy_from_slice(&0x0001_0000_u32.to_be_bytes());
        tkhd_payload[76..80].copy_from_slice(&0x0010_0000_u32.to_be_bytes());
        tkhd_payload[80..84].copy_from_slice(&0x0010_0000_u32.to_be_bytes());
        let trak = atom(*b"trak", [atom(*b"tkhd", tkhd_payload), mdia].concat());
        let moov = atom(*b"moov", trak);
        [ftyp, atom(*b"mdat", mdat_payload), moov].concat()
    }

    fn atom(kind: [u8; 4], payload: Vec<u8>) -> Vec<u8> {
        let mut output = Vec::new();
        output.extend(u32::try_from(payload.len() + 8).unwrap().to_be_bytes());
        output.extend(kind);
        output.extend(payload);
        output
    }

    fn full_table(kind: [u8; 4], entries: &[(u32, u32)]) -> Vec<u8> {
        let mut payload = vec![0; 4];
        payload.extend(u32::try_from(entries.len()).unwrap().to_be_bytes());
        for &(left, right) in entries {
            payload.extend(left.to_be_bytes());
            payload.extend(right.to_be_bytes());
        }
        atom(kind, payload)
    }

    fn full_table3(kind: [u8; 4], entries: &[(u32, u32, u32)]) -> Vec<u8> {
        let mut payload = vec![0; 4];
        payload.extend(u32::try_from(entries.len()).unwrap().to_be_bytes());
        for &(a, b, c) in entries {
            payload.extend(a.to_be_bytes());
            payload.extend(b.to_be_bytes());
            payload.extend(c.to_be_bytes());
        }
        atom(kind, payload)
    }

    fn full_values(kind: [u8; 4], entries: &[u32]) -> Vec<u8> {
        let mut payload = vec![0; 4];
        payload.extend(u32::try_from(entries.len()).unwrap().to_be_bytes());
        for &entry in entries {
            payload.extend(entry.to_be_bytes());
        }
        atom(kind, payload)
    }
}
