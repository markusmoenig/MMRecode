//! Shared streaming codec API adapters.

use std::collections::VecDeque;

use mmrecode_core::{
    CodecDescriptor, CodecId, Decoder, Encoder, Error, MediaType, Packet, PacketFlags, PixelFormat,
    Rational, Result, StreamId, VideoEncoderSettings, VideoFrame,
};

use crate::{CODEC_NAME, FrameRate, Mpeg2EncodeOptions, decode_stream, encode_stream};

/// Stateful elementary-stream MPEG-2 decoder.
#[derive(Debug, Default)]
pub struct Mpeg2Decoder {
    configured: bool,
    frames: VecDeque<VideoFrame>,
}

impl Decoder for Mpeg2Decoder {
    fn configure(&mut self, descriptor: &CodecDescriptor) -> Result<()> {
        if descriptor.codec_id.as_str() != CODEC_NAME || descriptor.media_type != MediaType::Video {
            return Err(Error::InvalidData(
                "MPEG-2 decoder requires a video/mpeg2 video descriptor".into(),
            ));
        }
        self.frames.clear();
        self.configured = true;
        Ok(())
    }

    fn send_packet(&mut self, packet: Packet) -> Result<()> {
        if !self.configured {
            return Err(Error::InvalidState(
                "MPEG-2 decoder must be configured before receiving packets".into(),
            ));
        }
        for picture in decode_stream(&packet.data)? {
            self.frames.push_back(picture.frame);
        }
        Ok(())
    }

    fn receive_frame(&mut self) -> Result<Option<VideoFrame>> {
        if !self.configured {
            return Err(Error::InvalidState(
                "MPEG-2 decoder must be configured before receiving frames".into(),
            ));
        }
        Ok(self.frames.pop_front())
    }

    fn flush(&mut self) -> Result<()> {
        if !self.configured {
            return Err(Error::InvalidState(
                "MPEG-2 decoder must be configured before flushing".into(),
            ));
        }
        Ok(())
    }
}

/// Stateful MPEG-2 sequence encoder.
#[derive(Debug, Default)]
pub struct Mpeg2Encoder {
    options: Option<Mpeg2EncodeOptions>,
    dimensions: Option<(usize, usize)>,
    frames: Vec<VideoFrame>,
    packets: VecDeque<Packet>,
    reconstructions: VecDeque<VideoFrame>,
    flushed: bool,
}

impl Mpeg2Encoder {
    /// Receives the normative reconstruction associated with the encoded sequence.
    ///
    /// # Errors
    ///
    /// Returns an error when the encoder has not been configured.
    pub fn receive_reconstructed_frame(&mut self) -> Result<Option<VideoFrame>> {
        if self.options.is_none() {
            return Err(Error::InvalidState(
                "MPEG-2 encoder must be configured before receiving reconstruction".into(),
            ));
        }
        Ok(self.reconstructions.pop_front())
    }
}

impl Encoder for Mpeg2Encoder {
    fn configure(&mut self, settings: &VideoEncoderSettings) -> Result<CodecDescriptor> {
        if settings.pixel_format != PixelFormat::Yuv420p8 {
            return Err(Error::Unsupported(
                "MPEG-2 encoder requires Yuv420p8 input".into(),
            ));
        }
        let mut options = Mpeg2EncodeOptions {
            frame_rate: frame_rate_from_time_base(settings.time_base)?,
            ..Mpeg2EncodeOptions::default()
        };
        if let Some(value) = settings.options.get("gop_size") {
            options.gop_size = parse_option(value, "gop_size")?;
        }
        if let Some(value) = settings.options.get("b_frames") {
            options.b_frames = parse_option(value, "b_frames")?;
        }
        if let Some(value) = settings.options.get("quantiser_scale_code") {
            options.quantiser_scale_code = parse_option(value, "quantiser_scale_code")?;
        }
        if let Some(value) = settings.options.get("motion_search_range") {
            options.motion_search_range = parse_option(value, "motion_search_range")?;
        }
        if let Some(value) = settings.options.get("progressive") {
            options.progressive = parse_bool(value, "progressive")?;
        }
        if let Some(value) = settings.options.get("top_field_first") {
            options.top_field_first = parse_bool(value, "top_field_first")?;
        }
        self.options = Some(options);
        self.dimensions = Some((settings.width, settings.height));
        self.frames.clear();
        self.packets.clear();
        self.reconstructions.clear();
        self.flushed = false;
        Ok(CodecDescriptor {
            codec_id: CodecId::new(CODEC_NAME),
            codec_tag: None,
            media_type: MediaType::Video,
            configuration: Vec::new(),
        })
    }

    fn send_frame(&mut self, frame: VideoFrame) -> Result<()> {
        let dimensions = self.dimensions.ok_or_else(|| {
            Error::InvalidState("MPEG-2 encoder must be configured before receiving frames".into())
        })?;
        if self.flushed {
            return Err(Error::InvalidState(
                "MPEG-2 encoder cannot receive frames after flush".into(),
            ));
        }
        if (frame.width, frame.height) != dimensions || frame.format != PixelFormat::Yuv420p8 {
            return Err(Error::InvalidData(
                "MPEG-2 input frame does not match configured sequence".into(),
            ));
        }
        self.frames.push(frame);
        Ok(())
    }

    fn receive_packet(&mut self) -> Result<Option<Packet>> {
        if self.options.is_none() {
            return Err(Error::InvalidState(
                "MPEG-2 encoder must be configured before receiving packets".into(),
            ));
        }
        Ok(self.packets.pop_front())
    }

    fn flush(&mut self) -> Result<()> {
        let options = self.options.ok_or_else(|| {
            Error::InvalidState("MPEG-2 encoder must be configured before flushing".into())
        })?;
        if self.flushed {
            return Ok(());
        }
        let encoded = encode_stream(&self.frames, options)?;
        self.reconstructions.extend(encoded.reconstructed);
        let mut flags = PacketFlags::empty();
        flags.insert(PacketFlags::KEY);
        self.packets.push_back(Packet {
            stream_id: StreamId(0),
            data: encoded.data,
            pts: self.frames.first().and_then(|frame| frame.timing.pts),
            dts: self.frames.first().and_then(|frame| frame.timing.pts),
            duration: None,
            flags,
            side_data: Vec::new(),
        });
        self.flushed = true;
        Ok(())
    }
}

fn frame_rate_from_time_base(time_base: Rational) -> Result<FrameRate> {
    let pair = (time_base.numerator(), time_base.denominator());
    match pair {
        (1_001, 24_000) => Ok(FrameRate::Fps23_976),
        (1, 24) => Ok(FrameRate::Fps24),
        (1, 25) => Ok(FrameRate::Fps25),
        (1_001, 30_000) => Ok(FrameRate::Fps29_97),
        (1, 30) => Ok(FrameRate::Fps30),
        (1, 50) => Ok(FrameRate::Fps50),
        (1_001, 60_000) => Ok(FrameRate::Fps59_94),
        (1, 60) => Ok(FrameRate::Fps60),
        _ => Err(Error::Unsupported(format!(
            "unsupported MPEG-2 frame time base {}/{}",
            time_base.numerator(),
            time_base.denominator()
        ))),
    }
}

fn parse_option<T: std::str::FromStr>(value: &str, name: &str) -> Result<T> {
    value
        .parse()
        .map_err(|_| Error::InvalidData(format!("invalid MPEG-2 {name} option '{value}'")))
}

fn parse_bool(value: &str, name: &str) -> Result<bool> {
    match value {
        "1" | "true" | "yes" => Ok(true),
        "0" | "false" | "no" => Ok(false),
        _ => Err(Error::InvalidData(format!(
            "invalid MPEG-2 {name} option '{value}'"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use mmrecode_core::PacketSideData;

    use super::*;
    use crate::parse_stream;

    const PROGRESSIVE: &[u8] =
        include_bytes!("../../../../testdata/mpeg2/valid/main-ml-progressive-ibp.m2v");

    #[test]
    fn streaming_decoder_queues_presentation_order_frames() {
        let mut decoder = Mpeg2Decoder::default();
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
                data: PROGRESSIVE.to_vec(),
                pts: None,
                dts: None,
                duration: None,
                flags: PacketFlags::empty(),
                side_data: Vec::<PacketSideData>::new(),
            })
            .unwrap();
        let mut count = 0;
        while decoder.receive_frame().unwrap().is_some() {
            count += 1;
        }
        assert_eq!(count, 12);
    }

    #[test]
    fn streaming_encoder_flushes_one_sequence_and_reconstructions() {
        let frames: Vec<_> = decode_stream(PROGRESSIVE)
            .unwrap()
            .into_iter()
            .take(4)
            .map(|picture| picture.frame)
            .collect();
        let mut encoder = Mpeg2Encoder::default();
        let descriptor = encoder
            .configure(&VideoEncoderSettings {
                width: 96,
                height: 64,
                pixel_format: PixelFormat::Yuv420p8,
                time_base: Rational::new(1, 25).unwrap(),
                bitrate: None,
                options: BTreeMap::new(),
            })
            .unwrap();
        assert_eq!(descriptor.codec_id.as_str(), CODEC_NAME);
        for frame in frames {
            encoder.send_frame(frame).unwrap();
        }
        encoder.flush().unwrap();
        encoder.flush().unwrap();
        let packet = encoder.receive_packet().unwrap().expect("encoded sequence");
        assert!(packet.flags.contains(PacketFlags::KEY));
        assert_eq!(parse_stream(&packet.data).unwrap().pictures().len(), 4);
        assert!(encoder.receive_packet().unwrap().is_none());
        let mut reconstruction_count = 0;
        while encoder.receive_reconstructed_frame().unwrap().is_some() {
            reconstruction_count += 1;
        }
        assert_eq!(reconstruction_count, 4);
    }
}
