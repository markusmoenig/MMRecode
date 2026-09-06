//! Native AAC-LC PCM-to-packet boundary.

use std::{collections::VecDeque, f64::consts::PI, sync::OnceLock};

use mmrecode_bitstream::BitWriter;
use mmrecode_core::{
    AudioEncoder, AudioEncoderSettings, AudioFrame, AudioSampleFormat, CodecDescriptor, CodecId,
    Error, FourCc, MediaType, Packet, PacketFlags, Rational, Result, StreamId, Timestamp,
};

use crate::SAMPLE_RATES;

const SAMPLES_PER_FRAME: usize = 1_024;

/// Algorithmic delay of the long-window AAC-LC analysis filterbank.
pub const AAC_LC_PRIMING_SAMPLES: u32 = 1_024;

#[derive(Clone, Copy, Debug)]
struct Configuration {
    sample_rate: u32,
    channels: u16,
    time_base: Rational,
    bitrate: u64,
}

/// Native AAC-LC encoder foundation for mono/stereo signed-16 PCM.
///
/// The current deterministic mode uses sine long windows, independent stereo channels, uniform
/// scalefactors, escape-codebook spectral data, and packet-budget-driven quantization. It has no
/// psychoacoustic model or short-window transient decisions yet. One priming frame precedes the
/// program and one tail frame is emitted by [`AudioEncoder::flush`]; containers must signal the
/// priming trim.
#[derive(Debug, Default)]
pub struct AacLcEncoder {
    configuration: Option<Configuration>,
    packets: VecDeque<Packet>,
    next_pts: i64,
    drained: bool,
    previous: Vec<i16>,
    received_input: bool,
}

impl AudioEncoder for AacLcEncoder {
    fn configure(&mut self, settings: &AudioEncoderSettings) -> Result<CodecDescriptor> {
        if settings.sample_format != AudioSampleFormat::I16Interleaved {
            return Err(Error::Unsupported(
                "native AAC-LC encoding requires interleaved signed 16-bit PCM".into(),
            ));
        }
        if !matches!(settings.channels, 1 | 2) {
            return Err(Error::Unsupported(
                "native AAC-LC encoding currently supports mono or stereo".into(),
            ));
        }
        let rate_index = SAMPLE_RATES
            .iter()
            .position(|rate| *rate == settings.sample_rate)
            .ok_or_else(|| Error::Unsupported("unsupported AAC-LC sample rate".into()))?;
        let bitrate = settings
            .bitrate
            .unwrap_or_else(|| 96_000 * u64::from(settings.channels));
        if !(16_000..=576_000).contains(&bitrate) {
            return Err(Error::Unsupported(
                "AAC-LC bitrate must be from 16000 through 576000 bits per second".into(),
            ));
        }
        if let Some((name, _)) = settings.options.first_key_value() {
            return Err(Error::Unsupported(format!(
                "unknown AAC-LC encoder option '{name}'"
            )));
        }
        let time_base = Rational::new(1, i64::from(settings.sample_rate))?;
        self.configuration = Some(Configuration {
            sample_rate: settings.sample_rate,
            channels: settings.channels,
            time_base,
            bitrate,
        });
        self.packets.clear();
        self.next_pts = 0;
        self.drained = false;
        self.previous = vec![0; SAMPLES_PER_FRAME * usize::from(settings.channels)];
        self.received_input = false;
        Ok(CodecDescriptor {
            codec_id: CodecId::new("audio/aac"),
            codec_tag: Some(FourCc(*b"mp4a")),
            media_type: MediaType::Audio,
            configuration: audio_specific_config(
                u8::try_from(rate_index).expect("AAC rate table has fewer than 16 entries"),
                u8::try_from(settings.channels).expect("mono/stereo fits u8"),
            )?,
        })
    }

    fn send_frame(&mut self, frame: AudioFrame) -> Result<()> {
        let configuration = self.configuration.ok_or_else(|| {
            Error::InvalidState("AAC-LC encoder must be configured before input".into())
        })?;
        if self.drained {
            return Err(Error::InvalidState(
                "AAC-LC encoder requires reconfiguration after flush".into(),
            ));
        }
        frame.validate()?;
        if frame.format != AudioSampleFormat::I16Interleaved
            || frame.sample_rate != configuration.sample_rate
            || frame.channels != configuration.channels
            || frame.samples_per_channel != SAMPLES_PER_FRAME
        {
            return Err(Error::InvalidData(
                "AAC-LC input must match the configured format and contain 1024 samples per channel"
                    .into(),
            ));
        }
        let pts = match frame.timing.pts {
            Some(pts) => {
                if pts.time_base != configuration.time_base {
                    return Err(Error::InvalidData(
                        "AAC-LC frame PTS must use the 1/sample_rate time base".into(),
                    ));
                }
                if pts.value != self.next_pts {
                    return Err(Error::InvalidData(
                        "AAC-LC input frames must have contiguous presentation timestamps".into(),
                    ));
                }
                pts
            }
            None => Timestamp {
                value: self.next_pts,
                time_base: configuration.time_base,
            },
        };
        let duration = Timestamp {
            value: i64::try_from(SAMPLES_PER_FRAME).expect("AAC frame size fits i64"),
            time_base: configuration.time_base,
        };
        if frame.timing.duration.is_some_and(|value| value != duration) {
            return Err(Error::InvalidData(
                "AAC-LC frame duration must be exactly 1024 samples".into(),
            ));
        }
        self.packets.push_back(Packet {
            stream_id: StreamId(1),
            data: encode_access_unit(&self.previous, &frame.samples, configuration)?,
            pts: Some(pts),
            dts: Some(pts),
            duration: Some(duration),
            flags: PacketFlags::KEY,
            side_data: Vec::new(),
        });
        self.next_pts = self
            .next_pts
            .checked_add(duration.value)
            .ok_or_else(|| Error::InvalidData("AAC-LC timestamp overflows".into()))?;
        self.previous = frame.samples;
        self.received_input = true;
        Ok(())
    }

    fn receive_packet(&mut self) -> Result<Option<Packet>> {
        if self.configuration.is_none() {
            return Err(Error::InvalidState(
                "AAC-LC encoder must be configured before output".into(),
            ));
        }
        Ok(self.packets.pop_front())
    }

    fn flush(&mut self) -> Result<()> {
        let configuration = self.configuration.ok_or_else(|| {
            Error::InvalidState("AAC-LC encoder must be configured before flushing".into())
        })?;
        if self.drained {
            return Ok(());
        }
        if self.received_input {
            let duration = Timestamp {
                value: i64::try_from(SAMPLES_PER_FRAME).expect("AAC frame size fits i64"),
                time_base: configuration.time_base,
            };
            let pts = Timestamp {
                value: self.next_pts,
                time_base: configuration.time_base,
            };
            let silence = vec![0; self.previous.len()];
            self.packets.push_back(Packet {
                stream_id: StreamId(1),
                data: encode_access_unit(&self.previous, &silence, configuration)?,
                pts: Some(pts),
                dts: Some(pts),
                duration: Some(duration),
                flags: PacketFlags::KEY,
                side_data: Vec::new(),
            });
            self.next_pts = self
                .next_pts
                .checked_add(duration.value)
                .ok_or_else(|| Error::InvalidData("AAC-LC timestamp overflows".into()))?;
        }
        self.drained = true;
        Ok(())
    }
}

fn encode_access_unit(
    previous: &[i16],
    current: &[i16],
    configuration: Configuration,
) -> Result<Vec<u8>> {
    if previous.iter().all(|sample| *sample == 0) && current.iter().all(|sample| *sample == 0) {
        return silent_access_unit(configuration.channels);
    }
    let channels = usize::from(configuration.channels);
    let spectra = (0..channels)
        .map(|channel| analyze_channel(previous, current, channels, channel))
        .collect::<Vec<_>>();
    let mut gains = spectra
        .iter()
        .map(|spectrum| minimum_global_gain(spectrum))
        .collect::<Vec<_>>();
    let budget = configuration
        .bitrate
        .saturating_mul(u64::try_from(SAMPLES_PER_FRAME).expect("AAC frame size fits u64"))
        .div_ceil(u64::from(configuration.sample_rate));
    loop {
        let rate_index = SAMPLE_RATES
            .iter()
            .position(|rate| *rate == configuration.sample_rate)
            .expect("configured AAC sample rate is indexed");
        let data = compressed_access_unit(&spectra, &gains, rate_index)?;
        let bits = u64::try_from(data.len())
            .map_err(|_| Error::Unsupported("AAC access unit length exceeds u64".into()))?
            .saturating_mul(8);
        if bits <= budget || gains.iter().all(|gain| *gain == u8::MAX) {
            return Ok(data);
        }
        for gain in &mut gains {
            *gain = gain.saturating_add(1);
        }
    }
}

#[allow(clippy::cast_precision_loss)]
fn analyze_channel(previous: &[i16], current: &[i16], channels: usize, channel: usize) -> Vec<f64> {
    let window = sine_window();
    let mut samples = Vec::with_capacity(SAMPLES_PER_FRAME * 2);
    samples.extend(
        previous[channel..]
            .iter()
            .step_by(channels)
            .map(|sample| f64::from(*sample)),
    );
    samples.extend(
        current[channel..]
            .iter()
            .step_by(channels)
            .map(|sample| f64::from(*sample)),
    );
    let length = SAMPLES_PER_FRAME as f64;
    (0..SAMPLES_PER_FRAME)
        .map(|coefficient| {
            let step = PI / length * (coefficient as f64 + 0.5);
            let initial = step * (0.5 + length / 2.0);
            let (mut sine, mut cosine) = initial.sin_cos();
            let (step_sine, step_cosine) = step.sin_cos();
            let mut sum = 0.0;
            for (sample, window) in samples.iter().zip(window) {
                sum += sample * window * cosine;
                let next_cosine = cosine * step_cosine - sine * step_sine;
                sine = sine * step_cosine + cosine * step_sine;
                cosine = next_cosine;
            }
            sum * 2.0
        })
        .collect()
}

#[allow(clippy::cast_precision_loss)]
fn sine_window() -> &'static [f64] {
    static WINDOW: OnceLock<Vec<f64>> = OnceLock::new();
    WINDOW.get_or_init(|| {
        (0..SAMPLES_PER_FRAME * 2)
            .map(|index| (PI * (index as f64 + 0.5) / (2.0 * SAMPLES_PER_FRAME as f64)).sin())
            .collect()
    })
}

#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn minimum_global_gain(spectrum: &[f64]) -> u8 {
    let maximum = spectrum
        .iter()
        .fold(0.0_f64, |value, coefficient| value.max(coefficient.abs()));
    if maximum == 0.0 {
        return 0;
    }
    let required_scale = maximum / 8_191_f64.powf(4.0 / 3.0);
    (100.0 + 4.0 * required_scale.log2())
        .ceil()
        .clamp(0.0, 255.0) as u8
}

fn compressed_access_unit(
    spectra: &[Vec<f64>],
    gains: &[u8],
    rate_index: usize,
) -> Result<Vec<u8>> {
    let mut writer = BitWriter::new();
    writer.write_bits(u64::from(spectra.len() == 2), 3)?; // SCE or CPE
    writer.write_bits(0, 4)?;
    if spectra.len() == 2 {
        writer.write_bit(false)?; // independent channel windows
    }
    for (spectrum, &gain) in spectra.iter().zip(gains) {
        write_channel(&mut writer, spectrum, gain, rate_index)?;
    }
    writer.write_bits(7, 3)?;
    writer.align_to_byte();
    Ok(writer.into_bytes())
}

fn write_channel(
    writer: &mut BitWriter,
    spectrum: &[f64],
    gain: u8,
    rate_index: usize,
) -> Result<()> {
    let offsets = crate::tables::BANDS_1024[rate_index];
    let max_sfb = offsets.len() - 1;
    let scale = 2_f64.powf((f64::from(gain) - 100.0) / 4.0);
    let quantized = spectrum
        .iter()
        .map(|&coefficient| quantize(coefficient, scale))
        .collect::<Vec<_>>();
    let codebooks = offsets
        .windows(2)
        .map(|band| {
            if quantized[band[0]..band[1]]
                .iter()
                .all(|coefficient| *coefficient == 0)
            {
                0
            } else {
                11
            }
        })
        .collect::<Vec<u8>>();
    writer.write_bits(u64::from(gain), 8)?;
    writer.write_bit(false)?;
    writer.write_bits(0, 2)?;
    writer.write_bit(false)?;
    writer.write_bits(
        u64::try_from(max_sfb).expect("AAC long-window band count fits u64"),
        6,
    )?;
    writer.write_bit(false)?;
    let mut section_start = 0;
    while section_start < max_sfb {
        let codebook = codebooks[section_start];
        let mut section_end = section_start + 1;
        while section_end < max_sfb && codebooks[section_end] == codebook {
            section_end += 1;
        }
        writer.write_bits(u64::from(codebook), 4)?;
        let mut remaining = section_end - section_start;
        while remaining >= 31 {
            writer.write_bits(31, 5)?;
            remaining -= 31;
        }
        writer.write_bits(
            u64::try_from(remaining).expect("AAC section remainder fits u64"),
            5,
        )?;
        section_start = section_end;
    }
    let (scale_code, scale_length) = crate::tables::SCALEFACTORS[60];
    for &codebook in &codebooks {
        if codebook != 0 {
            writer.write_bits(u64::from(scale_code), scale_length)?;
        }
    }
    writer.write_bit(false)?;
    writer.write_bit(false)?;
    writer.write_bit(false)?;
    for (band, &codebook) in offsets.windows(2).zip(&codebooks) {
        if codebook == 0 {
            continue;
        }
        for pair in quantized[band[0]..band[1]].as_chunks::<2>().0 {
            write_escape_pair(writer, *pair)?;
        }
    }
    Ok(())
}

#[allow(clippy::cast_possible_truncation)]
fn quantize(value: f64, scale: f64) -> i16 {
    let magnitude = (value.abs() / scale)
        .powf(3.0 / 4.0)
        .round()
        .clamp(0.0, 8_191.0) as i16;
    magnitude * if value.is_sign_negative() { -1 } else { 1 }
}

fn write_escape_pair(writer: &mut BitWriter, values: [i16; 2]) -> Result<()> {
    let first = usize::from(values[0].unsigned_abs().min(16));
    let second = usize::from(values[1].unsigned_abs().min(16));
    let symbol = first * 17 + second;
    let (code, length) = crate::tables::BOOK_11[symbol];
    writer.write_bits(u64::from(code), length)?;
    for value in values {
        if value != 0 {
            writer.write_bit(value < 0)?;
        }
    }
    for value in values {
        let magnitude = value.unsigned_abs();
        if magnitude >= 16 {
            let width = u8::try_from(u16::BITS - 1 - magnitude.leading_zeros())
                .expect("AAC escape width fits u8");
            for _ in 4..width {
                writer.write_bit(true)?;
            }
            writer.write_bit(false)?;
            writer.write_bits(u64::from(magnitude - (1_u16 << width)), width)?;
        }
    }
    Ok(())
}

fn audio_specific_config(rate_index: u8, channels: u8) -> Result<Vec<u8>> {
    let mut writer = BitWriter::new();
    writer.write_bits(2, 5)?;
    writer.write_bits(u64::from(rate_index), 4)?;
    writer.write_bits(u64::from(channels), 4)?;
    writer.write_bit(false)?; // frameLengthFlag: 1024 samples
    writer.write_bit(false)?; // dependsOnCoreCoder
    writer.write_bit(false)?; // extensionFlag
    Ok(writer.into_bytes())
}

fn silent_access_unit(channels: u16) -> Result<Vec<u8>> {
    let mut writer = BitWriter::new();
    writer.write_bits(u64::from(channels != 1), 3)?; // SCE or CPE
    writer.write_bits(0, 4)?; // element_instance_tag
    if channels == 2 {
        writer.write_bit(false)?; // common_window
    }
    for _ in 0..channels {
        writer.write_bits(0, 8)?; // global_gain
        writer.write_bit(false)?; // ics_reserved_bit
        writer.write_bits(0, 2)?; // ONLY_LONG_SEQUENCE
        writer.write_bit(false)?; // sine window
        writer.write_bits(0, 6)?; // max_sfb: all ZERO_HCB
        writer.write_bit(false)?; // predictor_data_present
        writer.write_bit(false)?; // pulse_data_present
        writer.write_bit(false)?; // tns_data_present
        writer.write_bit(false)?; // gain_control_data_present
    }
    writer.write_bits(7, 3)?; // ID_END
    writer.align_to_byte();
    Ok(writer.into_bytes())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use mmrecode_core::{AudioDecoder, FrameTiming};

    use super::*;
    use crate::AacLcDecoder;

    #[test]
    fn silent_stereo_round_trips_through_the_native_decoder() {
        let mut encoder = AacLcEncoder::default();
        let descriptor = encoder
            .configure(&AudioEncoderSettings {
                sample_rate: 48_000,
                channels: 2,
                sample_format: AudioSampleFormat::I16Interleaved,
                bitrate: None,
                options: BTreeMap::new(),
            })
            .unwrap();
        assert_eq!(descriptor.configuration, [0x11, 0x90]);
        encoder
            .send_frame(AudioFrame {
                format: AudioSampleFormat::I16Interleaved,
                sample_rate: 48_000,
                channels: 2,
                samples_per_channel: 1_024,
                samples: vec![0; 2_048],
                timing: FrameTiming::default(),
            })
            .unwrap();
        let packet = encoder.receive_packet().unwrap().unwrap();
        assert_eq!(packet.duration.unwrap().value, 1_024);

        let mut decoder = AacLcDecoder::default();
        decoder.configure(&descriptor).unwrap();
        decoder.send_packet(packet).unwrap();
        let decoded_audio = decoder.receive_frame().unwrap().unwrap();
        assert_eq!(decoded_audio.samples, vec![0; 2_048]);
    }

    #[test]
    #[allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)]
    fn encodes_nonzero_long_window_pcm_with_bounded_error() {
        let mut encoder = AacLcEncoder::default();
        let descriptor = encoder
            .configure(&AudioEncoderSettings {
                sample_rate: 48_000,
                channels: 1,
                sample_format: AudioSampleFormat::I16Interleaved,
                bitrate: Some(128_000),
                options: BTreeMap::new(),
            })
            .unwrap();
        let input = (0..SAMPLES_PER_FRAME * 4)
            .map(|sample| {
                (8_000.0 * (2.0 * PI * 440.0 * sample as f64 / 48_000.0).sin()).round() as i16
            })
            .collect::<Vec<_>>();
        let mut packets = Vec::new();
        for samples in input.as_chunks::<SAMPLES_PER_FRAME>().0 {
            encoder
                .send_frame(AudioFrame {
                    format: AudioSampleFormat::I16Interleaved,
                    sample_rate: 48_000,
                    channels: 1,
                    samples_per_channel: SAMPLES_PER_FRAME,
                    samples: samples.to_vec(),
                    timing: FrameTiming::default(),
                })
                .unwrap();
            packets.push(encoder.receive_packet().unwrap().unwrap());
        }
        encoder.flush().unwrap();
        packets.push(encoder.receive_packet().unwrap().unwrap());
        assert_eq!(packets.len(), 5);
        assert!(packets.iter().all(|packet| packet.data.len() <= 342));

        let mut decoder = AacLcDecoder::default();
        decoder.configure(&descriptor).unwrap();
        let mut output = Vec::new();
        for packet in packets {
            decoder.send_packet(packet).unwrap();
            output.extend(decoder.receive_frame().unwrap().unwrap().samples);
        }
        let output = &output[SAMPLES_PER_FRAME..SAMPLES_PER_FRAME + input.len()];
        let mse = input
            .iter()
            .zip(output)
            .map(|(&expected, &actual)| {
                let difference = f64::from(expected) - f64::from(actual);
                difference * difference
            })
            .sum::<f64>()
            / input.len() as f64;
        assert!(mse < 2_500.0, "AAC-LC round-trip MSE {mse}");
    }

    #[test]
    fn rejects_discontinuous_timestamps_without_emitting_a_packet() {
        let mut encoder = AacLcEncoder::default();
        encoder
            .configure(&AudioEncoderSettings {
                sample_rate: 48_000,
                channels: 1,
                sample_format: AudioSampleFormat::I16Interleaved,
                bitrate: None,
                options: BTreeMap::new(),
            })
            .unwrap();
        let error = encoder
            .send_frame(AudioFrame {
                format: AudioSampleFormat::I16Interleaved,
                sample_rate: 48_000,
                channels: 1,
                samples_per_channel: 1_024,
                samples: vec![0; 1_024],
                timing: FrameTiming {
                    pts: Some(Timestamp {
                        value: 1,
                        time_base: Rational::new(1, 48_000).unwrap(),
                    }),
                    duration: None,
                },
            })
            .unwrap_err();
        assert!(error.to_string().contains("contiguous"));
        assert!(encoder.receive_packet().unwrap().is_none());
    }

    #[test]
    #[allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)]
    fn packet_budget_selects_coarser_quantization_for_lower_bitrates() {
        let input = (0..SAMPLES_PER_FRAME)
            .map(|sample| {
                let fundamental = 7_000.0 * (2.0 * PI * 997.0 * sample as f64 / 48_000.0).sin();
                let overtone = 3_000.0 * (2.0 * PI * 6_100.0 * sample as f64 / 48_000.0).sin();
                (fundamental + overtone).round() as i16
            })
            .collect::<Vec<_>>();
        let silence = vec![0; SAMPLES_PER_FRAME];
        let time_base = Rational::new(1, 48_000).unwrap();
        let low = encode_access_unit(
            &silence,
            &input,
            Configuration {
                sample_rate: 48_000,
                channels: 1,
                time_base,
                bitrate: 64_000,
            },
        )
        .unwrap();
        let high = encode_access_unit(
            &silence,
            &input,
            Configuration {
                sample_rate: 48_000,
                channels: 1,
                time_base,
                bitrate: 192_000,
            },
        )
        .unwrap();
        assert!(low.len() <= 171);
        assert!(high.len() <= 512);
        assert!(low.len() < high.len());
    }
}
