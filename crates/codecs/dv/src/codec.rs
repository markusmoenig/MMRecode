use std::collections::VecDeque;

use mmrecode_core::{
    AccessUnitInfo, CodecDescriptor, CodecId, Decoder, DependencyAnalyzer, Encoder, Error,
    FrameTiming, MediaType, Packet, PacketFlags, ParameterFingerprint, PictureId, PictureKind,
    RandomAccessKind, Result, StreamId, VideoEncoderSettings, VideoFrame,
};

use crate::{DvProfile, decode_video, encode_video, parse_frame};

/// Canonical `MMRecode` codec identifier for DV video.
pub const CODEC_NAME: &str = "video/dv";

/// Stateful raw-DV packet decoder.
#[derive(Debug, Default)]
pub struct DvDecoder {
    configured: bool,
    frames: VecDeque<VideoFrame>,
}

impl Decoder for DvDecoder {
    fn configure(&mut self, descriptor: &CodecDescriptor) -> Result<()> {
        if descriptor.codec_id.as_str() != CODEC_NAME || descriptor.media_type != MediaType::Video {
            return Err(Error::InvalidData(
                "DV decoder requires a video/dv video descriptor".into(),
            ));
        }
        self.frames.clear();
        self.configured = true;
        Ok(())
    }

    fn send_packet(&mut self, packet: Packet) -> Result<()> {
        if !self.configured {
            return Err(Error::InvalidState(
                "DV decoder must be configured before receiving packets".into(),
            ));
        }
        let parsed = parse_frame(&packet.data)?;
        let mut frame = decode_video(&parsed)?;
        frame.timing = FrameTiming {
            pts: packet.pts,
            duration: packet.duration,
        };
        self.frames.push_back(frame);
        Ok(())
    }

    fn receive_frame(&mut self) -> Result<Option<VideoFrame>> {
        if !self.configured {
            return Err(Error::InvalidState(
                "DV decoder must be configured before receiving frames".into(),
            ));
        }
        Ok(self.frames.pop_front())
    }

    fn flush(&mut self) -> Result<()> {
        if !self.configured {
            return Err(Error::InvalidState(
                "DV decoder must be configured before flushing".into(),
            ));
        }
        Ok(())
    }
}

/// Stateful deterministic DV25 video encoder.
#[derive(Debug, Default)]
pub struct DvEncoder {
    profile: Option<DvProfile>,
    packets: VecDeque<Packet>,
    reconstructions: VecDeque<VideoFrame>,
}

impl DvEncoder {
    /// Receives the reconstruction associated with an encoded packet.
    ///
    /// # Errors
    ///
    /// Returns an error when the encoder has not been configured.
    pub fn receive_reconstructed_frame(&mut self) -> Result<Option<VideoFrame>> {
        if self.profile.is_none() {
            return Err(Error::InvalidState(
                "DV encoder must be configured before receiving reconstruction".into(),
            ));
        }
        Ok(self.reconstructions.pop_front())
    }
}

impl Encoder for DvEncoder {
    fn configure(&mut self, settings: &VideoEncoderSettings) -> Result<CodecDescriptor> {
        let profile = match (settings.width, settings.height, settings.pixel_format) {
            (720, 480, mmrecode_core::PixelFormat::Yuv411p8) => DvProfile::DV25_525_60,
            (720, 576, mmrecode_core::PixelFormat::Yuv420p8) => DvProfile::DV25_625_50,
            _ => {
                return Err(Error::Unsupported(
                    "DV encoder supports 720x480 Yuv411p8 and 720x576 Yuv420p8".into(),
                ));
            }
        };
        self.profile = Some(profile);
        self.packets.clear();
        self.reconstructions.clear();
        Ok(CodecDescriptor {
            codec_id: CodecId::new(CODEC_NAME),
            codec_tag: None,
            media_type: MediaType::Video,
            configuration: Vec::new(),
        })
    }

    fn send_frame(&mut self, frame: VideoFrame) -> Result<()> {
        let profile = self.profile.ok_or_else(|| {
            Error::InvalidState("DV encoder must be configured before receiving frames".into())
        })?;
        if (frame.width, frame.height, frame.format)
            != (profile.width, profile.height, profile.pixel_format)
        {
            return Err(Error::InvalidData(
                "DV input frame does not match the configured profile".into(),
            ));
        }
        let timing = frame.timing;
        let encoded = encode_video(&frame)?;
        let mut flags = PacketFlags::empty();
        flags.insert(PacketFlags::KEY);
        self.packets.push_back(Packet {
            stream_id: StreamId(0),
            data: encoded.data,
            pts: timing.pts,
            dts: timing.pts,
            duration: timing.duration,
            flags,
            side_data: Vec::new(),
        });
        self.reconstructions.push_back(encoded.reconstructed);
        Ok(())
    }

    fn receive_packet(&mut self) -> Result<Option<Packet>> {
        if self.profile.is_none() {
            return Err(Error::InvalidState(
                "DV encoder must be configured before receiving packets".into(),
            ));
        }
        Ok(self.packets.pop_front())
    }

    fn flush(&mut self) -> Result<()> {
        if self.profile.is_none() {
            return Err(Error::InvalidState(
                "DV encoder must be configured before flushing".into(),
            ));
        }
        Ok(())
    }
}

/// Dependency analyzer for independently coded DV frames.
#[derive(Debug, Default)]
pub struct DvDependencyAnalyzer {
    next_picture: u64,
}

impl DependencyAnalyzer for DvDependencyAnalyzer {
    fn analyze_access_unit(&mut self, packet: &Packet) -> Result<AccessUnitInfo> {
        let frame = parse_frame(&packet.data)?;
        frame.validate_strict()?;
        let picture = PictureId(self.next_picture);
        let order = i64::try_from(self.next_picture)
            .map_err(|_| Error::InvalidState("DV picture order exceeds i64".into()))?;
        self.next_picture = self
            .next_picture
            .checked_add(1)
            .ok_or_else(|| Error::InvalidState("DV picture identifier overflow".into()))?;
        let parameters = match frame.profile() {
            DvProfile::DV25_525_60 => ParameterFingerprint(0x4456_3235_3532_3536),
            DvProfile::DV25_625_50 => ParameterFingerprint(0x4456_3235_3632_3530),
            _ => ParameterFingerprint(0),
        };
        Ok(AccessUnitInfo {
            picture_id: picture,
            picture_kind: PictureKind::Intra,
            decode_order: order,
            presentation_order: order,
            references: Vec::new(),
            random_access: RandomAccessKind::Clean,
            parameters,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NTSC: &[u8] = include_bytes!("../../../../testdata/dv/valid/dv25-525-60-one-frame.dv");

    #[test]
    fn dependency_analyzer_reports_clean_intra_picture() {
        let mut analyzer = DvDependencyAnalyzer::default();
        let info = analyzer
            .analyze_access_unit(&Packet {
                stream_id: StreamId(0),
                data: NTSC.to_vec(),
                pts: None,
                dts: None,
                duration: None,
                flags: PacketFlags::empty(),
                side_data: Vec::new(),
            })
            .unwrap();
        assert_eq!(info.picture_kind, PictureKind::Intra);
        assert_eq!(info.random_access, RandomAccessKind::Clean);
        assert!(info.references.is_empty());
    }
}
