use std::{
    io::{BufReader, Cursor},
    ops::Range,
    path::{Path, PathBuf},
};

use mmrecode_core::{
    ColorDescription, ColorRange, FieldOrder, FrameTiming, PixelFormat, Plane, RandomAccessKind,
    Rational, Timestamp, VideoFrame,
};
use mmrecode_dv::{DifSection, DvIssue, DvPackData, DvProfile, Timecode};
use mmrecode_mjpeg::JpegImage;
use mmrecode_mpeg2::{
    MacroblockCoding, MacroblockInfo, MotionType, PictureStructure, PictureType, SequenceParameters,
};

use crate::audio::{AudioTrack, decode_mpeg_layer2};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum MediaKind {
    MotionJpeg,
    Mpeg2Elementary,
    Mpeg2Transport,
    RawDv,
    Y4m,
}

impl MediaKind {
    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::MotionJpeg => "Motion JPEG",
            Self::Mpeg2Elementary => "MPEG-2 Video",
            Self::Mpeg2Transport => "MPEG-2 Transport Stream",
            Self::RawDv => "Raw DV",
            Self::Y4m => "YUV4MPEG2",
        }
    }
}

pub(crate) struct JpegInspection {
    pub(crate) source_range: Range<usize>,
    pub(crate) image: JpegImage,
}

pub(crate) struct FrameRecord {
    pub(crate) frame: VideoFrame,
    pub(crate) jpeg: Option<JpegInspection>,
    pub(crate) dv: Option<DvInspection>,
    pub(crate) mpeg2: Option<Mpeg2Inspection>,
}

pub(crate) struct TransportInspection {
    pub(crate) packet_count: usize,
    pub(crate) pat_count: usize,
    pub(crate) pmt_count: usize,
    pub(crate) pes_count: usize,
    pub(crate) pcr_count: usize,
    pub(crate) programs: Vec<TransportProgramInspection>,
    pub(crate) mpeg_audio: Option<MpegAudioInspection>,
}

pub(crate) struct MpegAudioInspection {
    pub(crate) frame_count: usize,
    pub(crate) sample_rate: u32,
    pub(crate) channels: u8,
    pub(crate) bit_rate: u32,
}

pub(crate) struct TransportProgramInspection {
    pub(crate) program_number: u16,
    pub(crate) pmt_pid: u16,
    pub(crate) pcr_pid: u16,
    pub(crate) streams: Vec<(u16, u8)>,
}

pub(crate) struct Mpeg2Inspection {
    pub(crate) source_range: Range<usize>,
    pub(crate) sequence: SequenceParameters,
    pub(crate) picture_type: PictureType,
    pub(crate) picture_structure: PictureStructure,
    pub(crate) temporal_reference: u16,
    pub(crate) decode_order: i64,
    pub(crate) presentation_order: i64,
    pub(crate) references: Vec<u64>,
    pub(crate) random_access: RandomAccessKind,
    pub(crate) slice_count: usize,
    pub(crate) macroblocks: Vec<MacroblockInfo>,
    pub(crate) macroblock_map: VideoFrame,
    pub(crate) progressive_frame: bool,
    pub(crate) top_field_first: bool,
    pub(crate) repeat_first_field: bool,
}

pub(crate) struct DvInspection {
    pub(crate) source_range: Range<usize>,
    pub(crate) profile: DvProfile,
    pub(crate) issues: Vec<DvIssue>,
    pub(crate) pack_count: usize,
    pub(crate) timecode: Option<Timecode>,
    pub(crate) audio: Option<(usize, u32, usize)>,
    pub(crate) dif_map: VideoFrame,
    pub(crate) concealed_video_segments: usize,
}

pub(crate) struct Document {
    pub(crate) path: PathBuf,
    pub(crate) byte_length: usize,
    pub(crate) kind: MediaKind,
    pub(crate) frames: Vec<FrameRecord>,
    pub(crate) transport: Option<TransportInspection>,
    pub(crate) frame_rate: Rational,
    pub(crate) frame_rate_assumed: bool,
    pub(crate) audio: Option<AudioTrack>,
}

impl Document {
    pub(crate) fn load(path: &Path) -> Result<Self, String> {
        let bytes = std::fs::read(path)
            .map_err(|error| format!("cannot read '{}': {error}", path.display()))?;
        if bytes.starts_with(b"YUV4MPEG2 ") {
            Self::load_y4m(path, &bytes)
        } else if bytes.starts_with(&[0xff, 0xd8]) {
            Self::load_mjpeg(path, &bytes)
        } else if bytes.len() >= mmrecode_mpegts::TS_PACKET_SIZE && bytes[0] == 0x47 {
            Self::load_mpegts(path, &bytes)
        } else if mmrecode_mpeg2::parse_stream(&bytes).is_ok() {
            Self::load_mpeg2(path, &bytes)
        } else if mmrecode_dv::detect_profile_prefix(&bytes).is_ok() {
            Self::load_dv(path, &bytes)
        } else if bytes.is_empty() {
            Err("input file is empty".into())
        } else {
            Err("input is neither MPEG-TS, MPEG-2 Video, raw DV, YUV4MPEG2, nor a JPEG/MJPEG stream".into())
        }
    }

    fn load_mjpeg(path: &Path, bytes: &[u8]) -> Result<Self, String> {
        let byte_length = bytes.len();
        let mut remaining = bytes;
        let mut file_offset = 0_usize;
        let mut frames = Vec::new();
        while !remaining.is_empty() {
            let mut image = mmrecode_mjpeg::parse_jpeg(remaining)
                .map_err(|error| format!("frame {}: {error}", frames.len() + 1))?;
            let consumed = remaining.len() - image.trailing_data.len();
            if consumed == 0 {
                return Err("JPEG parser consumed no input".into());
            }
            image.trailing_data = Vec::new();
            let frame = mmrecode_mjpeg::decode_jpeg(&remaining[..consumed])
                .and_then(mmrecode_mjpeg::DecodedJpeg::into_video_frame)
                .map_err(|error| format!("frame {}: {error}", frames.len() + 1))?;
            let end = file_offset
                .checked_add(consumed)
                .ok_or_else(|| "JPEG source offset overflow".to_owned())?;
            frames.push(FrameRecord {
                frame,
                jpeg: Some(JpegInspection {
                    source_range: file_offset..end,
                    image,
                }),
                dv: None,
                mpeg2: None,
            });
            file_offset = end;
            remaining = &remaining[consumed..];
        }
        if frames.is_empty() {
            return Err("input contains no JPEG frames".into());
        }
        Ok(Self {
            path: path.to_owned(),
            byte_length,
            kind: MediaKind::MotionJpeg,
            frames,
            transport: None,
            frame_rate: Rational::new(25, 1).expect("constant frame rate is valid"),
            frame_rate_assumed: true,
            audio: None,
        })
    }

    fn load_dv(path: &Path, bytes: &[u8]) -> Result<Self, String> {
        let profile =
            mmrecode_dv::detect_profile_prefix(bytes).map_err(|error| error.to_string())?;
        if !bytes.len().is_multiple_of(profile.frame_size) {
            return Err(format!(
                "raw DV stream has {} trailing byte(s)",
                bytes.len() % profile.frame_size
            ));
        }
        let mut frames = Vec::new();
        let mut audio_chunks = Vec::new();
        let mut complete_audio = true;
        for (index, data) in bytes.chunks_exact(profile.frame_size).enumerate() {
            let parsed = mmrecode_dv::parse_frame(data)
                .map_err(|error| format!("frame {}: {error}", index + 1))?;
            let timecode = parsed.packs().iter().find_map(|pack| match pack.data {
                DvPackData::Timecode(value) => Some(value),
                _ => None,
            });
            let extracted_audio = mmrecode_dv::extract_audio(&parsed).ok();
            let audio = extracted_audio.as_ref().and_then(|frames| {
                frames
                    .first()
                    .map(|first| (frames.len(), first.sample_rate, first.samples_per_channel))
            });
            if let Some(audio) = extracted_audio {
                audio_chunks.push(audio);
            } else {
                complete_audio = false;
            }
            let decoded = mmrecode_dv::decode_video_with_options(
                &parsed,
                mmrecode_dv::DvVideoDecodeOptions {
                    conceal_errors: true,
                },
            )
            .map_err(|error| format!("frame {} video: {error}", index + 1))?;
            frames.push(FrameRecord {
                frame: decoded.frame,
                jpeg: None,
                dv: Some(DvInspection {
                    source_range: index * profile.frame_size..(index + 1) * profile.frame_size,
                    profile,
                    issues: parsed.issues().to_vec(),
                    pack_count: parsed.packs().len(),
                    timecode,
                    audio,
                    dif_map: dif_map(&parsed),
                    concealed_video_segments: decoded.concealed_segments.len(),
                }),
                mpeg2: None,
            });
        }
        let audio = if complete_audio {
            combine_dv_audio(&audio_chunks).ok()
        } else {
            None
        };
        Ok(Self {
            path: path.to_owned(),
            byte_length: bytes.len(),
            kind: MediaKind::RawDv,
            frames,
            transport: None,
            frame_rate: profile.frame_rate(),
            frame_rate_assumed: false,
            audio,
        })
    }

    fn load_y4m(path: &Path, bytes: &[u8]) -> Result<Self, String> {
        let byte_length = bytes.len();
        let mut reader = mmrecode_y4m::Y4mReader::new(BufReader::new(Cursor::new(bytes)));
        let mut frames = Vec::new();
        while let Some(frame) = reader
            .read_frame()
            .map_err(|error| format!("frame {}: {error}", frames.len() + 1))?
        {
            frames.push(FrameRecord {
                frame,
                jpeg: None,
                dv: None,
                mpeg2: None,
            });
        }
        if frames.is_empty() {
            return Err("Y4M input contains no frames".into());
        }
        let declared_frame_rate = reader.header().and_then(|header| header.frame_rate);
        Ok(Self {
            path: path.to_owned(),
            byte_length,
            kind: MediaKind::Y4m,
            frames,
            transport: None,
            frame_rate: declared_frame_rate
                .unwrap_or_else(|| Rational::new(25, 1).expect("constant frame rate is valid")),
            frame_rate_assumed: declared_frame_rate.is_none(),
            audio: None,
        })
    }

    fn load_mpeg2(path: &Path, bytes: &[u8]) -> Result<Self, String> {
        let stream = mmrecode_mpeg2::parse_stream(bytes).map_err(|error| error.to_string())?;
        let dependencies =
            mmrecode_mpeg2::analyze_dependencies(&stream).map_err(|error| error.to_string())?;
        let mut decoded =
            mmrecode_mpeg2::decode_stream(bytes).map_err(|error| error.to_string())?;
        decoded.sort_by_key(|picture| picture.decode_order);
        if decoded.len() != stream.pictures().len() || decoded.len() != dependencies.len() {
            return Err(
                "MPEG-2 parser, dependency analyzer, and decoder disagree on picture count".into(),
            );
        }

        let mut frames = Vec::with_capacity(decoded.len());
        for ((picture, access), decoded) in stream.pictures().iter().zip(&dependencies).zip(decoded)
        {
            let macroblock_map = macroblock_map(
                picture.sequence.width,
                picture.sequence.height,
                picture.header.picture_coding_type,
                &decoded.macroblocks,
            );
            frames.push(FrameRecord {
                frame: decoded.frame,
                jpeg: None,
                dv: None,
                mpeg2: Some(Mpeg2Inspection {
                    source_range: picture.source_range.clone(),
                    sequence: picture.sequence.clone(),
                    picture_type: picture.header.picture_coding_type,
                    picture_structure: picture.coding_extension.picture_structure,
                    temporal_reference: picture.header.temporal_reference,
                    decode_order: access.decode_order,
                    presentation_order: access.presentation_order,
                    references: access
                        .references
                        .iter()
                        .map(|reference| reference.0)
                        .collect(),
                    random_access: access.random_access,
                    slice_count: picture.slices.len(),
                    macroblocks: decoded.macroblocks,
                    macroblock_map,
                    progressive_frame: picture.coding_extension.progressive_frame,
                    top_field_first: picture.coding_extension.top_field_first,
                    repeat_first_field: picture.coding_extension.repeat_first_field,
                }),
            });
        }
        frames.sort_by_key(|record| {
            record
                .mpeg2
                .as_ref()
                .map_or(i64::MAX, |mpeg2| mpeg2.presentation_order)
        });
        let frame_rate = frames
            .first()
            .and_then(|record| record.mpeg2.as_ref())
            .map(|mpeg2| mpeg2.sequence.frame_rate)
            .ok_or_else(|| "MPEG-2 stream contains no displayable pictures".to_owned())?;
        Ok(Self {
            path: path.to_owned(),
            byte_length: bytes.len(),
            kind: MediaKind::Mpeg2Elementary,
            frames,
            transport: None,
            frame_rate,
            frame_rate_assumed: false,
            audio: None,
        })
    }

    fn load_mpegts(path: &Path, bytes: &[u8]) -> Result<Self, String> {
        let transport =
            mmrecode_mpegts::demux_transport_stream(bytes).map_err(|error| error.to_string())?;
        let elementary = transport
            .mpeg2_video_bytes()
            .map_err(|error| error.to_string())?;
        let programs = transport
            .program_map_tables
            .iter()
            .map(|table| TransportProgramInspection {
                program_number: table.program_number,
                pmt_pid: table.pid,
                pcr_pid: table.pcr_pid,
                streams: table
                    .streams
                    .iter()
                    .map(|stream| (stream.elementary_pid, stream.stream_type))
                    .collect(),
            })
            .collect();
        let audio_data = transport.mpeg1_audio_bytes().ok();
        let audio_frames = audio_data
            .as_deref()
            .and_then(|audio| mmrecode_mpegaudio::parse_layer2_stream(audio).ok());
        let inspection = TransportInspection {
            packet_count: transport.packets.len(),
            pat_count: transport.program_association_tables.len(),
            pmt_count: transport.program_map_tables.len(),
            pes_count: transport.elementary_packets.len(),
            pcr_count: transport
                .packets
                .iter()
                .filter(|packet| packet.pcr.is_some())
                .count(),
            programs,
            mpeg_audio: audio_frames.as_ref().and_then(|frames| {
                frames.first().map(|frame| MpegAudioInspection {
                    frame_count: frames.len(),
                    sample_rate: frame.header.sample_rate,
                    channels: frame.header.channels,
                    bit_rate: frame.header.bit_rate,
                })
            }),
        };
        let video_pts = first_pts(&transport, "video/mpeg2");
        let audio_pts = first_pts(&transport, "audio/mpeg1");
        let mut playback_audio = audio_data
            .as_deref()
            .and_then(|audio| decode_mpeg_layer2(audio).ok());
        if let (Some(audio), Some(audio_pts), Some(video_pts)) =
            (&mut playback_audio, audio_pts, video_pts)
        {
            audio.align_to_video(timestamp_seconds(audio_pts) - timestamp_seconds(video_pts));
        }
        let mut document = Self::load_mpeg2(path, &elementary)?;
        document.byte_length = bytes.len();
        document.kind = MediaKind::Mpeg2Transport;
        document.transport = Some(inspection);
        document.audio = playback_audio;
        Ok(document)
    }
}

fn combine_dv_audio(chunks: &[Vec<mmrecode_core::AudioFrame>]) -> Result<AudioTrack, String> {
    let first = chunks
        .first()
        .and_then(|chunk| chunk.first())
        .ok_or_else(|| "DV stream contains no playable audio".to_owned())?;
    let pair_count = chunks[0].len();
    let channels = u16::try_from(pair_count.saturating_mul(2))
        .map_err(|_| "DV audio channel count does not fit u16".to_owned())?;
    let sample_rate = first.sample_rate;
    let mut samples = Vec::new();
    for chunk in chunks {
        if chunk.len() != pair_count
            || chunk.iter().any(|frame| {
                frame.sample_rate != sample_rate
                    || frame.channels != 2
                    || frame.samples_per_channel != chunk[0].samples_per_channel
            })
        {
            return Err("DV audio layout changes between frames".into());
        }
        for sample_index in 0..chunk[0].samples_per_channel {
            for pair in chunk {
                let start = sample_index * 2;
                samples.extend_from_slice(&pair.samples[start..start + 2]);
            }
        }
    }
    AudioTrack::from_i16(sample_rate, channels, samples)
}

fn first_pts(transport: &mmrecode_mpegts::TransportStream, codec_id: &str) -> Option<Timestamp> {
    let stream_id = transport
        .streams
        .iter()
        .find(|stream| stream.codec.codec_id.as_str() == codec_id)?
        .id;
    transport
        .elementary_packets
        .iter()
        .find(|packet| packet.stream_id == stream_id)
        .and_then(|packet| packet.pts)
}

#[allow(clippy::cast_precision_loss)]
fn timestamp_seconds(timestamp: Timestamp) -> f64 {
    timestamp.value as f64 * timestamp.time_base.numerator() as f64
        / timestamp.time_base.denominator() as f64
}

fn macroblock_map(
    width: usize,
    height: usize,
    picture_type: PictureType,
    macroblocks: &[MacroblockInfo],
) -> VideoFrame {
    let mut data = vec![0_u8; width * height * 3];
    for macroblock in macroblocks {
        let color = macroblock_color(picture_type, macroblock);
        let start_x = macroblock.x * 16;
        let start_y = macroblock.y * 16;
        for y in start_y..(start_y + 16).min(height) {
            for x in start_x..(start_x + 16).min(width) {
                let border = x == start_x || y == start_y;
                let pixel = if border { [25, 25, 25] } else { color };
                let offset = (y * width + x) * 3;
                data[offset..offset + 3].copy_from_slice(&pixel);
            }
        }
    }
    VideoFrame {
        format: PixelFormat::Rgb24,
        width,
        height,
        planes: vec![Plane {
            data,
            stride: width * 3,
            width,
            height,
        }],
        timing: FrameTiming::default(),
        color: ColorDescription {
            range: ColorRange::Full,
            primaries: None,
            transfer: None,
            matrix: None,
        },
        field_order: FieldOrder::Unspecified,
    }
}

fn macroblock_color(picture_type: PictureType, macroblock: &MacroblockInfo) -> [u8; 3] {
    match (macroblock.coding, picture_type, macroblock.motion_type) {
        (MacroblockCoding::Intra, _, _) => [55, 190, 105],
        (MacroblockCoding::Skipped, PictureType::B, _) => [95, 70, 155],
        (MacroblockCoding::Skipped, _, _) => [70, 110, 175],
        (MacroblockCoding::Predicted, PictureType::B, MotionType::Field) => [180, 80, 210],
        (MacroblockCoding::Predicted, PictureType::B, _) => [145, 85, 195],
        (MacroblockCoding::Predicted, _, MotionType::Field) => [55, 175, 205],
        (MacroblockCoding::Predicted, _, MotionType::DualPrime) => [220, 90, 150],
        (MacroblockCoding::Predicted, _, _) => [235, 155, 55],
    }
}

fn dif_map(parsed: &mmrecode_dv::DvFrame<'_>) -> VideoFrame {
    let width = mmrecode_dv::DIF_BLOCKS_PER_SEQUENCE;
    let height = parsed.profile().dif_sequences;
    let mut data = Vec::with_capacity(width * height * 3);
    for block in parsed.blocks() {
        let color = match block.id.section {
            DifSection::Header => [65, 130, 255],
            DifSection::Subcode => [190, 90, 230],
            DifSection::Vaux => [55, 190, 180],
            DifSection::Audio => [250, 175, 55],
            DifSection::Video => [75, 175, 85],
            DifSection::Reserved(_) => [230, 65, 65],
        };
        data.extend_from_slice(&color);
    }
    VideoFrame {
        format: PixelFormat::Rgb24,
        width,
        height,
        planes: vec![Plane {
            data,
            stride: width * 3,
            width,
            height,
        }],
        timing: FrameTiming::default(),
        color: ColorDescription {
            range: ColorRange::Full,
            primaries: None,
            transfer: None,
            matrix: None,
        },
        field_order: FieldOrder::Unspecified,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loads_concatenated_mjpeg_frames() {
        let frame = include_bytes!("../../../../testdata/jpeg/valid/baseline-420.jpg");
        let mut stream = frame.to_vec();
        stream.extend_from_slice(frame);
        let document = Document::load_mjpeg(Path::new("two.mjpg"), &stream).expect("valid MJPEG");
        assert_eq!(document.frames.len(), 2);
        assert_eq!(
            document.frames[1]
                .jpeg
                .as_ref()
                .expect("JPEG inspection")
                .source_range
                .start,
            frame.len()
        );
        assert!(document.frame_rate_assumed);
        assert_eq!(document.frame_rate, Rational::new(25, 1).unwrap());
    }

    #[test]
    fn creates_a_dif_map_for_raw_dv() {
        let data = include_bytes!("../../../../testdata/dv/valid/dv25-525-60-one-frame.dv");
        let document = Document::load_dv(Path::new("frame.dv"), data).expect("valid DV map");
        assert_eq!(document.kind, MediaKind::RawDv);
        assert_eq!(document.frames[0].frame.width, DvProfile::DV25_525_60.width);
        let inspection = document.frames[0].dv.as_ref().expect("DV inspection");
        assert_eq!(inspection.dif_map.width, 150);
        assert_eq!(inspection.dif_map.height, 10);
    }

    #[test]
    fn loads_multiple_y4m_frames() {
        let bytes = include_bytes!("../../../../testdata/y4m/valid/two-frame-420.y4m");
        let document = Document::load_y4m(Path::new("two.y4m"), bytes).expect("valid Y4M stream");
        assert_eq!(document.frames.len(), 2);
        assert_eq!(document.kind, MediaKind::Y4m);
        assert_eq!(document.frame_rate, Rational::new(1, 1).unwrap());
        assert!(!document.frame_rate_assumed);
    }

    #[test]
    fn loads_mpeg2_in_presentation_order_with_macroblock_maps() {
        let bytes = include_bytes!("../../../../testdata/mpeg2/valid/main-ml-progressive-ibp.m2v");
        let document =
            Document::load_mpeg2(Path::new("progressive.m2v"), bytes).expect("valid MPEG-2");
        assert_eq!(document.kind, MediaKind::Mpeg2Elementary);
        assert_eq!(document.frames.len(), 12);
        let presentation: Vec<_> = document
            .frames
            .iter()
            .map(|frame| frame.mpeg2.as_ref().unwrap().presentation_order)
            .collect();
        assert_eq!(presentation, (0..12).collect::<Vec<_>>());
        let first = document.frames[0].mpeg2.as_ref().unwrap();
        assert_eq!(first.picture_type, PictureType::I);
        assert_eq!(first.macroblock_map.width, 96);
        assert_eq!(first.macroblock_map.height, 64);
        assert!(!first.macroblocks.is_empty());
    }

    #[test]
    fn loads_mpegts_with_container_and_video_inspection() {
        let bytes = include_bytes!("../../../../testdata/mpegts/valid/single-program-mpeg2-mp2.ts");
        let document =
            Document::load_mpegts(Path::new("program.ts"), bytes).expect("valid MPEG-TS");
        assert_eq!(document.kind, MediaKind::Mpeg2Transport);
        assert_eq!(document.frames.len(), 12);
        let transport = document.transport.as_ref().expect("transport inspection");
        assert!(transport.packet_count > 10);
        assert_eq!(transport.programs[0].pmt_pid, 0x1000);
        assert_eq!(
            transport.programs[0].streams,
            vec![(0x0100, 0x02), (0x0101, 0x03)]
        );
        let audio = transport.mpeg_audio.as_ref().expect("audio inspection");
        assert_eq!(audio.frame_count, 20);
        assert_eq!(audio.sample_rate, 48_000);
        assert_eq!(audio.channels, 2);
        let playback_audio = document.audio.as_ref().expect("decoded playback audio");
        assert_eq!(playback_audio.sample_rate, 48_000);
        assert_eq!(playback_audio.channels, 2);
        // The independent vector starts audio 481 samples before video; playback trims that lead.
        assert_eq!(playback_audio.samples.len(), 45_118);
        assert_eq!(playback_audio.duration().as_micros(), 469_979);
    }
}
