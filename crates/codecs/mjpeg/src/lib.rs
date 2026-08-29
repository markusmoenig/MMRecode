//! Motion JPEG parsing, decoding, encoding, and dependency analysis.
//!
//! Motion JPEG is the bootstrap codec for `MMRecode`. The implementation will keep JPEG syntax
//! separate from container-specific Motion JPEG conventions.

use mmrecode_core::{
    CodecDescriptor, CodecId, Decoder, Encoder, Error, MediaType, Packet, Result,
    VideoEncoderSettings, VideoFrame,
};

/// Canonical `MMRecode` codec identifier for Motion JPEG.
pub const CODEC_NAME: &str = "video/mjpeg";

/// Placeholder decoder used while the first codec vertical slice is implemented.
#[derive(Debug, Default)]
pub struct MjpegDecoder;

impl Decoder for MjpegDecoder {
    fn configure(&mut self, descriptor: &CodecDescriptor) -> Result<()> {
        if descriptor.codec_id.as_str() != CODEC_NAME {
            return Err(Error::InvalidData(format!(
                "expected {CODEC_NAME}, received {}",
                descriptor.codec_id.as_str()
            )));
        }
        Ok(())
    }

    fn send_packet(&mut self, _packet: Packet) -> Result<()> {
        Err(Error::Unsupported(
            "Motion JPEG decoding is not implemented yet".into(),
        ))
    }

    fn receive_frame(&mut self) -> Result<Option<VideoFrame>> {
        Ok(None)
    }

    fn flush(&mut self) -> Result<()> {
        Ok(())
    }
}

/// Placeholder encoder used while the first codec vertical slice is implemented.
#[derive(Debug, Default)]
pub struct MjpegEncoder;

impl Encoder for MjpegEncoder {
    fn configure(&mut self, _settings: &VideoEncoderSettings) -> Result<CodecDescriptor> {
        Ok(CodecDescriptor {
            codec_id: CodecId::new(CODEC_NAME),
            codec_tag: None,
            media_type: MediaType::Video,
            configuration: Vec::new(),
        })
    }

    fn send_frame(&mut self, _frame: VideoFrame) -> Result<()> {
        Err(Error::Unsupported(
            "Motion JPEG encoding is not implemented yet".into(),
        ))
    }

    fn receive_packet(&mut self) -> Result<Option<Packet>> {
        Ok(None)
    }

    fn flush(&mut self) -> Result<()> {
        Ok(())
    }
}
