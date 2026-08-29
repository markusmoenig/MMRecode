use std::collections::VecDeque;

use mmrecode_core::{
    CodecDescriptor, CodecId, Decoder, Encoder, Error, FrameTiming, MediaType, Packet, PacketFlags,
    PixelFormat, Result, StreamId, VideoEncoderSettings, VideoFrame,
};

use crate::{JpegEncodeOptions, decode_jpeg, encode_jpeg};

/// Canonical `MMRecode` codec identifier for Motion JPEG.
pub const CODEC_NAME: &str = "video/mjpeg";

/// Stateful decoder for independently coded baseline JPEG packets.
#[derive(Debug, Default)]
pub struct MjpegDecoder {
    configured: bool,
    frames: VecDeque<VideoFrame>,
}

impl Decoder for MjpegDecoder {
    fn configure(&mut self, descriptor: &CodecDescriptor) -> Result<()> {
        if descriptor.codec_id.as_str() != CODEC_NAME {
            return Err(Error::InvalidData(format!(
                "expected {CODEC_NAME}, received {}",
                descriptor.codec_id.as_str()
            )));
        }
        if descriptor.media_type != MediaType::Video {
            return Err(Error::InvalidData(
                "Motion JPEG decoder requires a video stream descriptor".into(),
            ));
        }
        self.frames.clear();
        self.configured = true;
        Ok(())
    }

    fn send_packet(&mut self, packet: Packet) -> Result<()> {
        if !self.configured {
            return Err(Error::InvalidState(
                "Motion JPEG decoder must be configured before receiving packets".into(),
            ));
        }
        let timing = FrameTiming {
            pts: packet.pts,
            duration: packet.duration,
        };
        let mut frame = decode_jpeg(&packet.data)?.into_video_frame()?;
        frame.timing = timing;
        self.frames.push_back(frame);
        Ok(())
    }

    fn receive_frame(&mut self) -> Result<Option<VideoFrame>> {
        if !self.configured {
            return Err(Error::InvalidState(
                "Motion JPEG decoder must be configured before receiving frames".into(),
            ));
        }
        Ok(self.frames.pop_front())
    }

    fn flush(&mut self) -> Result<()> {
        if !self.configured {
            return Err(Error::InvalidState(
                "Motion JPEG decoder must be configured before flushing".into(),
            ));
        }
        Ok(())
    }
}

/// Stateful constrained baseline Motion JPEG encoder.
#[derive(Debug, Default)]
pub struct MjpegEncoder {
    configuration: Option<EncoderConfiguration>,
    packets: VecDeque<Packet>,
    reconstructed_frames: VecDeque<VideoFrame>,
}

#[derive(Clone, Debug)]
struct EncoderConfiguration {
    width: usize,
    height: usize,
    pixel_format: PixelFormat,
    options: JpegEncodeOptions,
}

impl MjpegEncoder {
    /// Receives an internally reconstructed frame corresponding to an encoded packet.
    ///
    /// # Errors
    ///
    /// Returns an error when the encoder has not been configured.
    pub fn receive_reconstructed_frame(&mut self) -> Result<Option<VideoFrame>> {
        if self.configuration.is_none() {
            return Err(Error::InvalidState(
                "Motion JPEG encoder must be configured before receiving reconstruction".into(),
            ));
        }
        Ok(self.reconstructed_frames.pop_front())
    }
}

impl Encoder for MjpegEncoder {
    fn configure(&mut self, settings: &VideoEncoderSettings) -> Result<CodecDescriptor> {
        if settings.width == 0
            || settings.height == 0
            || settings.width > usize::from(u16::MAX)
            || settings.height > usize::from(u16::MAX)
        {
            return Err(Error::InvalidData(
                "Motion JPEG dimensions must be between 1 and 65535".into(),
            ));
        }
        if !matches!(
            settings.pixel_format,
            PixelFormat::Gray8
                | PixelFormat::Yuv420p8
                | PixelFormat::Yuv422p8
                | PixelFormat::Yuv444p8
        ) {
            return Err(Error::Unsupported(
                "Motion JPEG encoder requires planar grayscale or YCbCr input".into(),
            ));
        }
        let quality = settings.options.get("quality").map_or(Ok(75), |value| {
            value
                .parse::<u8>()
                .map_err(|_| Error::InvalidData(format!("invalid Motion JPEG quality {value:?}")))
        })?;
        if !(1..=100).contains(&quality) {
            return Err(Error::InvalidData(
                "Motion JPEG quality must be between 1 and 100".into(),
            ));
        }
        self.configuration = Some(EncoderConfiguration {
            width: settings.width,
            height: settings.height,
            pixel_format: settings.pixel_format,
            options: JpegEncodeOptions { quality },
        });
        self.packets.clear();
        self.reconstructed_frames.clear();
        Ok(CodecDescriptor {
            codec_id: CodecId::new(CODEC_NAME),
            codec_tag: None,
            media_type: MediaType::Video,
            configuration: Vec::new(),
        })
    }

    fn send_frame(&mut self, frame: VideoFrame) -> Result<()> {
        let configuration = self.configuration.clone().ok_or_else(|| {
            Error::InvalidState(
                "Motion JPEG encoder must be configured before receiving frames".into(),
            )
        })?;
        if (frame.width, frame.height, frame.format)
            != (
                configuration.width,
                configuration.height,
                configuration.pixel_format,
            )
        {
            return Err(Error::InvalidData(
                "Motion JPEG input frame does not match configured dimensions or format".into(),
            ));
        }
        let encoded = encode_jpeg(&frame, configuration.options)?;
        let mut flags = PacketFlags::empty();
        flags.insert(PacketFlags::KEY);
        self.packets.push_back(Packet {
            stream_id: StreamId(0),
            data: encoded.data,
            pts: frame.timing.pts,
            dts: frame.timing.pts,
            duration: frame.timing.duration,
            flags,
            side_data: Vec::new(),
        });
        self.reconstructed_frames.push_back(encoded.reconstructed);
        Ok(())
    }

    fn receive_packet(&mut self) -> Result<Option<Packet>> {
        if self.configuration.is_none() {
            return Err(Error::InvalidState(
                "Motion JPEG encoder must be configured before receiving packets".into(),
            ));
        }
        Ok(self.packets.pop_front())
    }

    fn flush(&mut self) -> Result<()> {
        if self.configuration.is_none() {
            return Err(Error::InvalidState(
                "Motion JPEG encoder must be configured before flushing".into(),
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use mmrecode_core::{
        CodecDescriptor, CodecId, Decoder, Encoder, MediaType, Packet, PacketFlags, PixelFormat,
        Rational, StreamId, VideoEncoderSettings,
    };

    use super::{CODEC_NAME, MjpegDecoder, MjpegEncoder};

    const BASELINE_420_JPEG: &[u8] =
        include_bytes!("../../../../testdata/jpeg/valid/baseline-420.jpg");

    #[test]
    fn streaming_decoder_produces_a_video_frame() {
        let mut decoder = MjpegDecoder::default();
        decoder
            .configure(&CodecDescriptor {
                codec_id: CodecId::new(CODEC_NAME),
                codec_tag: None,
                media_type: MediaType::Video,
                configuration: Vec::new(),
            })
            .unwrap();
        decoder
            .send_packet(Packet {
                stream_id: StreamId(0),
                data: BASELINE_420_JPEG.to_vec(),
                pts: None,
                dts: None,
                duration: None,
                flags: PacketFlags::empty(),
                side_data: Vec::new(),
            })
            .unwrap();
        let frame = decoder.receive_frame().unwrap().expect("decoded frame");
        assert_eq!((frame.width, frame.height), (16, 16));
        assert_eq!(frame.format, PixelFormat::Yuv420p8);
        assert!(decoder.receive_frame().unwrap().is_none());
    }

    #[test]
    fn streaming_encoder_produces_packets_and_reconstruction() {
        let frame = crate::decode_jpeg(BASELINE_420_JPEG)
            .and_then(crate::DecodedJpeg::into_video_frame)
            .unwrap();
        let mut encoder = MjpegEncoder::default();
        let mut options = BTreeMap::new();
        options.insert("quality".to_owned(), "80".to_owned());
        encoder
            .configure(&VideoEncoderSettings {
                width: frame.width,
                height: frame.height,
                pixel_format: frame.format,
                time_base: Rational::new(1, 25).unwrap(),
                bitrate: None,
                options,
            })
            .unwrap();
        encoder.send_frame(frame).unwrap();
        let packet = encoder.receive_packet().unwrap().expect("encoded packet");
        assert!(packet.flags.contains(PacketFlags::KEY));
        assert!(crate::parse_jpeg(&packet.data).is_ok());
        assert!(encoder.receive_reconstructed_frame().unwrap().is_some());
    }
}
