//! Native AAC-LC packet-to-PCM boundary.

use mmrecode_core::{
    AudioDecoder, AudioFrame, AudioSampleFormat, CodecDescriptor, Error, FrameTiming, MediaType,
    Packet, PacketFlags, Result,
};

use crate::{AudioSpecificConfig, syntax, synthesis::Filterbank};

/// Pure Rust AAC-LC decoder for the common mono/stereo access-unit subset.
///
/// Supports 1024-sample frames at standard AAC rates, long/start/short/stop windows, common or
/// independent stereo windows, scalefactors, all eleven spectral Huffman books, inverse
/// quantization, PNS, M/S and intensity stereo, pulse reconstruction, TNS, and sine/KBD IMDCT
/// overlap-add. AAC-LC gain-control payloads are consumed (as in the reference decoder); SBR/PS
/// and multichannel layouts return [`Error::Unsupported`].
/// There is no external decoder or silent substitution inside this implementation.
///
/// One output frame may be pending at a time. Drain it before sending another packet. A failed
/// decode poisons the stream until successful reconfiguration: later silent packets must never
/// be accepted after a skipped nonzero packet whose overlap history would be missing.
#[derive(Debug, Default)]
pub struct AacLcDecoder {
    configuration: Option<AudioSpecificConfig>,
    pending: Option<AudioFrame>,
    failed: bool,
    drained: bool,
    filterbanks: Vec<Filterbank>,
    noise_state: u32,
}

impl AudioDecoder for AacLcDecoder {
    fn configure(&mut self, descriptor: &CodecDescriptor) -> Result<()> {
        if descriptor.codec_id.as_str() != "audio/aac" || descriptor.media_type != MediaType::Audio
        {
            return Err(Error::Unsupported(
                "AAC decoder requires an audio/aac descriptor".into(),
            ));
        }
        let configuration = AudioSpecificConfig::parse(&descriptor.configuration)?;
        if configuration.sbr_present
            || configuration.ps_present
            || configuration.samples_per_frame != 1_024
            || !matches!(configuration.channel_configuration, 1 | 2)
            || !crate::SAMPLE_RATES.contains(&configuration.sample_rate)
        {
            return Err(Error::Unsupported(
                "native AAC-LC requires plain mono/stereo, 1024 samples, and a standard rate"
                    .into(),
            ));
        }
        self.filterbanks = (0..configuration.channels)
            .map(|_| Filterbank::default())
            .collect();
        self.configuration = Some(configuration);
        self.pending = None;
        self.failed = false;
        self.drained = false;
        self.noise_state = 0x1f2e_3d4c;
        Ok(())
    }

    fn send_packet(&mut self, packet: Packet) -> Result<()> {
        let configuration = self
            .configuration
            .as_ref()
            .ok_or_else(|| Error::InvalidState("AAC decoder is not configured".into()))?;
        if self.failed || self.drained {
            return Err(Error::InvalidState(
                "AAC decoder requires reconfiguration after error or flush".into(),
            ));
        }
        if self.pending.is_some() {
            return Err(Error::InvalidState(
                "receive the pending AAC frame before sending another packet".into(),
            ));
        }
        self.failed = true;
        if packet.flags.contains(PacketFlags::CORRUPT) {
            return Err(Error::InvalidData("AAC packet is marked corrupt".into()));
        }
        // PNS is stateful, but a malformed packet must not advance its sequence. Decode with a
        // tentative state and commit it only after the complete raw_data_block is valid.
        let mut noise_state = self.noise_state;
        let spectra = syntax::decode_spectrum(&packet.data, configuration, &mut noise_state)?;
        let pcm: Vec<_> = self
            .filterbanks
            .iter_mut()
            .zip(&spectra)
            .map(|(filterbank, spectrum)| filterbank.synthesize(spectrum))
            .collect();
        let samples_per_channel = usize::from(configuration.samples_per_frame);
        let mut samples = Vec::with_capacity(samples_per_channel * pcm.len());
        for index in 0..samples_per_channel {
            for channel in &pcm {
                samples.push(pcm_i16(channel[index]));
            }
        }
        self.pending = Some(AudioFrame {
            format: AudioSampleFormat::I16Interleaved,
            sample_rate: configuration.sample_rate,
            channels: u16::from(configuration.channels),
            samples_per_channel,
            samples,
            timing: FrameTiming {
                pts: packet.pts,
                duration: packet.duration,
            },
        });
        self.noise_state = noise_state;
        self.failed = false;
        Ok(())
    }

    fn receive_frame(&mut self) -> Result<Option<AudioFrame>> {
        Ok(self.pending.take())
    }

    fn flush(&mut self) -> Result<()> {
        if self.configuration.is_none() || self.failed {
            return Err(Error::InvalidState(
                "AAC decoder is unconfigured or failed".into(),
            ));
        }
        // No extra PCM is appended beyond the access units' declared sample count.
        self.drained = true;
        Ok(())
    }
}

#[allow(clippy::cast_possible_truncation)] // Round then saturate at the public signed-16 PCM boundary.
fn pcm_i16(value: f64) -> i16 {
    value
        .round()
        .clamp(f64::from(i16::MIN), f64::from(i16::MAX)) as i16
}

#[cfg(test)]
mod tests {
    use mmrecode_bitstream::BitWriter;
    use mmrecode_core::{CodecId, Rational, StreamId, Timestamp};

    use super::*;

    // Raw stereo AAC-LC access unit independently produced by Apple's encoder in ipad2.MP4.
    const APPLE_SILENCE: [u8; 6] = [0x21, 0x00, 0x03, 0x40, 0x68, 0x1c];

    fn descriptor(channels: u8) -> CodecDescriptor {
        CodecDescriptor {
            codec_id: CodecId::new("audio/aac"),
            codec_tag: None,
            media_type: MediaType::Audio,
            configuration: vec![0x12, channels << 3],
        }
    }

    fn packet(data: &[u8]) -> Packet {
        let time_base = Rational::new(1, 44_100).unwrap();
        Packet {
            stream_id: StreamId(1),
            data: data.to_vec(),
            pts: Some(Timestamp {
                value: -1024,
                time_base,
            }),
            dts: None,
            duration: Some(Timestamp {
                value: 1024,
                time_base,
            }),
            flags: PacketFlags::empty(),
            side_data: vec![],
        }
    }

    fn info(writer: &mut BitWriter, sequence: u64, grouping: u64, max_sfb: u64) {
        writer.write_bit(false).unwrap();
        writer.write_bits(sequence, 2).unwrap();
        writer.write_bit(true).unwrap(); // KBD and sine both map zero to zero
        writer
            .write_bits(max_sfb, if sequence == 2 { 4 } else { 6 })
            .unwrap();
        if sequence == 2 {
            writer.write_bits(grouping, 7).unwrap();
        } else {
            writer.write_bit(false).unwrap();
        }
    }

    fn channel_data(writer: &mut BitWriter, sequence: u64, groups: usize, max_sfb: u64, book: u64) {
        let width = if sequence == 2 { 3 } else { 5 };
        let escape = (1 << width) - 1;
        for _ in 0..groups {
            if max_sfb == 0 {
                continue;
            }
            writer.write_bits(book, 4).unwrap();
            let mut remaining = max_sfb;
            while remaining >= escape {
                writer.write_bits(escape, width).unwrap();
                remaining -= escape;
            }
            writer.write_bits(remaining, width).unwrap();
        }
        writer.write_bits(0, 3).unwrap(); // pulse, TNS, gain control absent
    }

    fn silence(
        channels: u8,
        common: bool,
        sequence: u64,
        grouping: u64,
        max_sfb: u64,
        book: u64,
    ) -> Vec<u8> {
        let mut writer = BitWriter::new();
        let groups = if sequence == 2 {
            8 - grouping.count_ones() as usize
        } else {
            1
        };
        writer.write_bits(u64::from(channels == 2), 3).unwrap();
        writer.write_bits(0, 4).unwrap();
        if channels == 2 {
            writer.write_bit(common).unwrap();
        }
        if common {
            info(&mut writer, sequence, grouping, max_sfb);
            writer.write_bits(1, 2).unwrap(); // explicit M/S mask
            for _ in 0..groups * usize::try_from(max_sfb).unwrap() {
                writer.write_bit(true).unwrap();
            }
        }
        for _ in 0..channels {
            writer.write_bits(100, 8).unwrap();
            if !common {
                info(&mut writer, sequence, grouping, max_sfb);
            }
            channel_data(&mut writer, sequence, groups, max_sfb, book);
        }
        writer.write_bits(7, 3).unwrap();
        writer.into_bytes()
    }

    #[test]
    fn decodes_apple_silence_and_preserves_packet_timing() {
        let mut decoder = AacLcDecoder::default();
        decoder.configure(&descriptor(2)).unwrap();
        for _ in 0..3 {
            let packet = packet(&APPLE_SILENCE);
            let pts = packet.pts;
            let duration = packet.duration;
            decoder.send_packet(packet).unwrap();
            let audio = decoder.receive_frame().unwrap().unwrap();
            audio.validate().unwrap();
            assert_eq!(audio.samples, vec![0; 2048]);
            assert_eq!(audio.samples_per_channel, 1024);
            assert_eq!(audio.sample_rate, 44_100);
            assert_eq!(audio.channels, 2);
            assert_eq!(audio.timing, FrameTiming { pts, duration });
        }
        decoder.flush().unwrap();
        assert!(decoder.receive_frame().unwrap().is_none());
    }

    #[test]
    fn native_window_group_section_and_stereo_matrix() {
        for channels in [1, 2] {
            for common in [false, true] {
                if channels == 1 && common {
                    continue;
                }
                for sequence in 0..4 {
                    for grouping in [0, 0b101_0101, 127] {
                        for max_sfb in [0, 1, if sequence == 2 { 14 } else { 49 }] {
                            let mut decoder = AacLcDecoder::default();
                            decoder.configure(&descriptor(channels)).unwrap();
                            let data = silence(channels, common, sequence, grouping, max_sfb, 0);
                            decoder.send_packet(packet(&data)).unwrap();
                            let audio = decoder.receive_frame().unwrap().unwrap();
                            assert_eq!(audio.samples, vec![0; 1024 * usize::from(channels)]);
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn invalid_syntax_is_never_silence_and_poisons_overlap_history() {
        let mut decoder = AacLcDecoder::default();
        decoder.configure(&descriptor(2)).unwrap();
        let error = decoder
            .send_packet(packet(&silence(2, true, 0, 0, 1, 12)))
            .unwrap_err();
        assert!(matches!(error, Error::InvalidData(_)));
        assert!(decoder.receive_frame().unwrap().is_none());
        assert!(matches!(
            decoder.send_packet(packet(&APPLE_SILENCE)),
            Err(Error::InvalidState(_))
        ));
        assert!(decoder.flush().is_err());
        decoder.configure(&descriptor(2)).unwrap();
        decoder.send_packet(packet(&APPLE_SILENCE)).unwrap();
    }

    #[test]
    fn nonzero_overlap_survives_zero_frames_and_is_cleared_by_reconfiguration() {
        let mut writer = BitWriter::new();
        writer.write_bits(0, 7).unwrap(); // SCE/tag
        writer.write_bits(160, 8).unwrap();
        info(&mut writer, 0, 0, 1);
        writer.write_bits(5, 4).unwrap(); // signed pair book
        writer.write_bits(1, 5).unwrap(); // one band
        writer.write_bit(false).unwrap(); // scale delta zero
        writer.write_bits(0, 3).unwrap(); // no auxiliary tools
        writer.write_bits(9, 4).unwrap(); // (1, 0)
        writer.write_bits(24, 5).unwrap(); // (1, -1)
        writer.write_bits(7, 3).unwrap();
        let nonzero = writer.into_bytes();
        let silent = silence(1, false, 0, 0, 0, 0);
        let mut decoder = AacLcDecoder::default();
        decoder.configure(&descriptor(1)).unwrap();
        decoder.send_packet(packet(&nonzero)).unwrap();
        assert!(
            decoder
                .receive_frame()
                .unwrap()
                .unwrap()
                .samples
                .iter()
                .any(|v| *v != 0)
        );
        decoder.send_packet(packet(&silent)).unwrap();
        assert!(
            decoder
                .receive_frame()
                .unwrap()
                .unwrap()
                .samples
                .iter()
                .any(|v| *v != 0)
        );
        decoder.send_packet(packet(&silent)).unwrap();
        assert!(
            decoder
                .receive_frame()
                .unwrap()
                .unwrap()
                .samples
                .iter()
                .all(|v| *v == 0)
        );
        decoder.send_packet(packet(&nonzero)).unwrap();
        decoder.receive_frame().unwrap();
        assert!(decoder.send_packet(packet(&[])).is_err());
        assert!(decoder.receive_frame().unwrap().is_none());
        assert!(decoder.send_packet(packet(&silent)).is_err());
        decoder.configure(&descriptor(1)).unwrap();
        decoder.send_packet(packet(&silent)).unwrap();
        assert!(
            decoder
                .receive_frame()
                .unwrap()
                .unwrap()
                .samples
                .iter()
                .all(|v| *v == 0)
        );
    }

    #[test]
    fn rejects_truncation_missing_audio_trailing_data_and_layout_mismatch() {
        for length in 0..APPLE_SILENCE.len() {
            let mut decoder = AacLcDecoder::default();
            decoder.configure(&descriptor(2)).unwrap();
            assert!(
                decoder
                    .send_packet(packet(&APPLE_SILENCE[..length]))
                    .is_err()
            );
            assert!(decoder.receive_frame().unwrap().is_none());
        }
        for data in [
            vec![0xe0],
            [APPLE_SILENCE.as_slice(), &[0]].concat(),
            silence(1, false, 0, 0, 0, 0),
        ] {
            let mut decoder = AacLcDecoder::default();
            decoder.configure(&descriptor(2)).unwrap();
            assert!(decoder.send_packet(packet(&data)).is_err());
        }
    }

    #[test]
    fn enforces_queue_backpressure_flush_and_reconfiguration() {
        let mut decoder = AacLcDecoder::default();
        assert!(decoder.send_packet(packet(&APPLE_SILENCE)).is_err());
        assert!(decoder.flush().is_err());
        decoder.configure(&descriptor(2)).unwrap();
        decoder.send_packet(packet(&APPLE_SILENCE)).unwrap();
        assert!(matches!(
            decoder.send_packet(packet(&APPLE_SILENCE)),
            Err(Error::InvalidState(_))
        ));
        decoder.flush().unwrap();
        decoder.flush().unwrap();
        assert!(decoder.receive_frame().unwrap().is_some());
        assert!(decoder.receive_frame().unwrap().is_none());
        assert!(decoder.send_packet(packet(&APPLE_SILENCE)).is_err());
        decoder.configure(&descriptor(2)).unwrap();
        decoder.send_packet(packet(&APPLE_SILENCE)).unwrap();
        decoder.configure(&descriptor(1)).unwrap();
        assert!(decoder.receive_frame().unwrap().is_none());
    }

    #[test]
    fn ignores_bounded_fill_and_data_but_rejects_sbr() {
        let mut prefix = BitWriter::new();
        prefix.write_bits(4, 3).unwrap(); // DSE
        prefix.write_bits(0, 4).unwrap();
        prefix.write_bit(true).unwrap(); // byte aligned
        prefix.write_bits(255, 8).unwrap();
        prefix.write_bits(0, 8).unwrap();
        for _ in 0..255 {
            prefix.write_bits(0xff, 8).unwrap();
        }
        for _ in 0..2 {
            // Escaped-length FIL elements need not start at byte boundaries.
            prefix.write_bits(6, 3).unwrap();
            prefix.write_bits(15, 4).unwrap();
            prefix.write_bits(0, 8).unwrap(); // escaped count = 14
            prefix.write_bits(0, 4).unwrap(); // EXT_FILL
            for _ in 0..13 {
                prefix.write_bits(0, 8).unwrap();
            }
            prefix.write_bits(0, 4).unwrap();
        }
        // Append mono audio syntax at its current bit position, not via byte concatenation.
        prefix.write_bits(0, 3).unwrap();
        prefix.write_bits(0, 4).unwrap();
        prefix.write_bits(100, 8).unwrap();
        info(&mut prefix, 0, 0, 0);
        prefix.write_bits(0, 3).unwrap();
        prefix.write_bits(7, 3).unwrap();
        let mut decoder = AacLcDecoder::default();
        decoder.configure(&descriptor(1)).unwrap();
        decoder.send_packet(packet(&prefix.into_bytes())).unwrap();
        assert!(decoder.receive_frame().unwrap().is_some());
        for extension in [11, 13, 14] {
            let mut writer = BitWriter::new();
            writer.write_bits(6, 3).unwrap();
            writer.write_bits(1, 4).unwrap();
            writer.write_bits(extension, 4).unwrap();
            writer.write_bits(0, 4).unwrap();
            writer.write_bits(7, 3).unwrap();
            decoder.configure(&descriptor(1)).unwrap();
            assert!(matches!(
                decoder.send_packet(packet(&writer.into_bytes())),
                Err(Error::Unsupported(_))
            ));
        }
    }
}
