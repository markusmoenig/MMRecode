use std::{
    io::{BufReader, Cursor},
    ops::Range,
    path::{Path, PathBuf},
};

use mmrecode_core::{
    ColorDescription, ColorRange, FieldOrder, FrameTiming, PixelFormat, Plane, VideoFrame,
};
use mmrecode_dv::{DifSection, DvIssue, DvPackData, DvProfile, Timecode};
use mmrecode_mjpeg::JpegImage;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum MediaKind {
    MotionJpeg,
    RawDv,
    Y4m,
}

impl MediaKind {
    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::MotionJpeg => "Motion JPEG",
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
}

impl Document {
    pub(crate) fn load(path: &Path) -> Result<Self, String> {
        let bytes = std::fs::read(path)
            .map_err(|error| format!("cannot read '{}': {error}", path.display()))?;
        if bytes.starts_with(b"YUV4MPEG2 ") {
            Self::load_y4m(path, &bytes)
        } else if bytes.starts_with(&[0xff, 0xd8]) {
            Self::load_mjpeg(path, &bytes)
        } else if mmrecode_dv::detect_profile_prefix(&bytes).is_ok() {
            Self::load_dv(path, &bytes)
        } else if bytes.is_empty() {
            Err("input file is empty".into())
        } else {
            Err("input is neither raw DV, YUV4MPEG2, nor a JPEG/MJPEG stream".into())
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
        for (index, data) in bytes.chunks_exact(profile.frame_size).enumerate() {
            let parsed = mmrecode_dv::parse_frame(data)
                .map_err(|error| format!("frame {}: {error}", index + 1))?;
            let timecode = parsed.packs().iter().find_map(|pack| match pack.data {
                DvPackData::Timecode(value) => Some(value),
                _ => None,
            });
            let audio = mmrecode_dv::extract_audio(&parsed).ok().and_then(|frames| {
                frames
                    .first()
                    .map(|first| (frames.len(), first.sample_rate, first.samples_per_channel))
            });
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
            });
        }
        Ok(Self {
            path: path.to_owned(),
            byte_length: bytes.len(),
            kind: MediaKind::RawDv,
            frames,
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
            });
        }
        if frames.is_empty() {
            return Err("Y4M input contains no frames".into());
        }
        Ok(Self {
            path: path.to_owned(),
            byte_length,
            kind: MediaKind::Y4m,
            frames,
        })
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
    }
}
