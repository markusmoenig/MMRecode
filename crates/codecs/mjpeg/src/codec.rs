use std::collections::VecDeque;

use mmrecode_core::{
    AccessUnitInfo, CodecDescriptor, CodecId, Decoder, DependencyAnalyzer, Encoder, Error,
    FrameTiming, MediaType, Packet, PacketFlags, ParameterFingerprint, PictureId, PictureKind,
    PixelFormat, RandomAccessKind, Result, StreamId, VideoEncoderSettings, VideoFrame,
};

use crate::{JpegEncodeOptions, JpegImage, SegmentData, decode_jpeg, encode_jpeg, parse_jpeg};

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

/// Dependency analyzer for independently coded JPEG pictures.
#[derive(Clone, Debug, Default)]
pub struct MjpegDependencyAnalyzer {
    next_picture: u64,
}

impl DependencyAnalyzer for MjpegDependencyAnalyzer {
    fn analyze_access_unit(&mut self, packet: &Packet) -> Result<AccessUnitInfo> {
        let image = parse_jpeg(&packet.data)?;
        let frame = image
            .frame_header()
            .ok_or_else(|| Error::InvalidData("JPEG access unit has no frame header".into()))?;
        let picture_id = PictureId(self.next_picture);
        let order = i64::try_from(self.next_picture)
            .map_err(|_| Error::InvalidState("Motion JPEG picture order exceeds i64".into()))?;
        self.next_picture = self
            .next_picture
            .checked_add(1)
            .ok_or_else(|| Error::InvalidState("Motion JPEG picture identifier overflow".into()))?;
        Ok(AccessUnitInfo {
            picture_id,
            picture_kind: PictureKind::Intra,
            decode_order: order,
            presentation_order: order,
            references: Vec::new(),
            random_access: RandomAccessKind::Clean,
            parameters: parameter_fingerprint(&image, frame),
        })
    }
}

fn parameter_fingerprint(image: &JpegImage, frame: &crate::FrameHeader) -> ParameterFingerprint {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    hash_value(&mut hash, u64::from(frame.sample_precision));
    hash_value(&mut hash, u64::from(frame.width));
    hash_value(&mut hash, u64::from(frame.height));
    hash_value(&mut hash, frame.components.len() as u64);
    for component in &frame.components {
        hash_value(&mut hash, u64::from(component.id));
        hash_value(&mut hash, u64::from(component.horizontal_sampling));
        hash_value(&mut hash, u64::from(component.vertical_sampling));
    }
    hash_value(&mut hash, u64::from(image.jfif_header().is_some()));
    let adobe_transform = image
        .segments
        .iter()
        .find_map(|segment| match &segment.data {
            SegmentData::Application(application)
                if application.number == 14
                    && application.data.len() >= 12
                    && application.data.starts_with(b"Adobe") =>
            {
                Some(application.data[11])
            }
            _ => None,
        });
    hash_value(&mut hash, adobe_transform.map_or(u64::MAX, u64::from));
    ParameterFingerprint(hash)
}

fn hash_value(hash: &mut u64, value: u64) {
    for byte in value.to_le_bytes() {
        *hash ^= u64::from(byte);
        *hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use mmrecode_core::{
        CodecDescriptor, CodecId, Decoder, DependencyAnalyzer, Encoder, MediaType, Packet,
        PacketFlags, PixelFormat, RandomAccessKind, Rational, StreamId, VideoEncoderSettings,
    };

    use super::{CODEC_NAME, MjpegDecoder, MjpegDependencyAnalyzer, MjpegEncoder};

    const BASELINE_420_JPEG: &[u8] =
        include_bytes!("../../../../testdata/jpeg/valid/baseline-420.jpg");
    const UNKNOWN_APP_JPEG: &[u8] =
        include_bytes!("../../../../testdata/jpeg/valid/unknown-app-marker.jpg");
    const GRAY_JPEG: &[u8] = include_bytes!("../../../../testdata/jpeg/valid/restart-interval.jpg");

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

    #[test]
    fn dependency_analyzer_reports_clean_intra_pictures() {
        let packet = |data: &[u8]| Packet {
            stream_id: StreamId(0),
            data: data.to_vec(),
            pts: None,
            dts: None,
            duration: None,
            flags: PacketFlags::empty(),
            side_data: Vec::new(),
        };
        let mut analyzer = MjpegDependencyAnalyzer::default();
        let first = analyzer
            .analyze_access_unit(&packet(BASELINE_420_JPEG))
            .unwrap();
        let second = analyzer
            .analyze_access_unit(&packet(UNKNOWN_APP_JPEG))
            .unwrap();
        let gray = analyzer.analyze_access_unit(&packet(GRAY_JPEG)).unwrap();

        assert_eq!(first.random_access, RandomAccessKind::Clean);
        assert!(first.references.is_empty());
        assert_eq!(first.presentation_order, 0);
        assert_eq!(second.presentation_order, 1);
        assert_eq!(first.parameters, second.parameters);
        assert_ne!(first.parameters, gray.parameters);
    }
}
