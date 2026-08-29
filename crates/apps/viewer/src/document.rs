use std::{
    io::{BufReader, Cursor},
    ops::Range,
    path::{Path, PathBuf},
};

use mmrecode_core::VideoFrame;
use mmrecode_mjpeg::JpegImage;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum MediaKind {
    MotionJpeg,
    Y4m,
}

impl MediaKind {
    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::MotionJpeg => "Motion JPEG",
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
        } else if bytes.is_empty() {
            Err("input file is empty".into())
        } else {
            Err("input is neither YUV4MPEG2 nor a JPEG/MJPEG stream".into())
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

    fn load_y4m(path: &Path, bytes: &[u8]) -> Result<Self, String> {
        let byte_length = bytes.len();
        let mut reader = mmrecode_y4m::Y4mReader::new(BufReader::new(Cursor::new(bytes)));
        let mut frames = Vec::new();
        while let Some(frame) = reader
            .read_frame()
            .map_err(|error| format!("frame {}: {error}", frames.len() + 1))?
        {
            frames.push(FrameRecord { frame, jpeg: None });
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
    fn loads_multiple_y4m_frames() {
        let bytes = include_bytes!("../../../../testdata/y4m/valid/two-frame-420.y4m");
        let document = Document::load_y4m(Path::new("two.y4m"), bytes).expect("valid Y4M stream");
        assert_eq!(document.frames.len(), 2);
        assert_eq!(document.kind, MediaKind::Y4m);
    }
}
