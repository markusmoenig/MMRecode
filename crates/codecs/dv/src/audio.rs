use std::{error::Error as StdError, fmt};

use mmrecode_core::{AudioFrame, AudioSampleFormat, FrameTiming};

use crate::{AudioQuantization, DvFrame, DvPackData, DvProfile};

const SHUFFLE_525: [[u8; 9]; 10] = [
    [0, 30, 60, 20, 50, 80, 10, 40, 70],
    [6, 36, 66, 26, 56, 86, 16, 46, 76],
    [12, 42, 72, 2, 32, 62, 22, 52, 82],
    [18, 48, 78, 8, 38, 68, 28, 58, 88],
    [24, 54, 84, 14, 44, 74, 4, 34, 64],
    [1, 31, 61, 21, 51, 81, 11, 41, 71],
    [7, 37, 67, 27, 57, 87, 17, 47, 77],
    [13, 43, 73, 3, 33, 63, 23, 53, 83],
    [19, 49, 79, 9, 39, 69, 29, 59, 89],
    [25, 55, 85, 15, 45, 75, 5, 35, 65],
];

const SHUFFLE_625: [[u8; 9]; 12] = [
    [0, 36, 72, 26, 62, 98, 16, 52, 88],
    [6, 42, 78, 32, 68, 104, 22, 58, 94],
    [12, 48, 84, 2, 38, 74, 28, 64, 100],
    [18, 54, 90, 8, 44, 80, 34, 70, 106],
    [24, 60, 96, 14, 50, 86, 4, 40, 76],
    [30, 66, 102, 20, 56, 92, 10, 46, 82],
    [1, 37, 73, 27, 63, 99, 17, 53, 89],
    [7, 43, 79, 33, 69, 105, 23, 59, 95],
    [13, 49, 85, 3, 39, 75, 29, 65, 101],
    [19, 55, 91, 9, 45, 81, 35, 71, 107],
    [25, 61, 97, 15, 51, 87, 5, 41, 77],
    [31, 67, 103, 21, 57, 93, 11, 47, 83],
];

/// Failure while interpreting or unshuffling embedded DV audio.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DvAudioError {
    /// The frame carries no AAUX audio-source pack.
    NoAudio,
    /// The AAUX sample-rate index is reserved.
    UnsupportedSampleRate,
    /// The AAUX source type or channel organization is unsupported.
    UnsupportedChannelLayout(u8),
    /// The AAUX quantization value is reserved.
    UnsupportedQuantization(u8),
}

impl fmt::Display for DvAudioError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoAudio => formatter.write_str("DV frame contains no audio-source pack"),
            Self::UnsupportedSampleRate => formatter.write_str("unsupported DV audio sample rate"),
            Self::UnsupportedChannelLayout(value) => {
                write!(formatter, "unsupported DV audio source type {value}")
            }
            Self::UnsupportedQuantization(value) => {
                write!(formatter, "unsupported DV audio quantization {value}")
            }
        }
    }
}

impl StdError for DvAudioError {}

/// Extracts embedded DV audio as one or two stereo, interleaved PCM frames.
///
/// Linear 16-bit DV produces one stereo frame. Nonlinear 12-bit DV stores two
/// stereo pairs and therefore produces two frames. The DV error sample codes
/// are converted to silence.
///
/// # Errors
///
/// Returns an error when no audio-source pack exists or its rate, source type,
/// or quantization is not supported.
pub fn extract_audio(frame: &DvFrame<'_>) -> Result<Vec<AudioFrame>, DvAudioError> {
    let source = frame
        .packs()
        .iter()
        .find_map(|pack| match pack.data {
            DvPackData::AudioSource(source) => Some(source),
            _ => None,
        })
        .ok_or(DvAudioError::NoAudio)?;
    let sample_rate = source
        .sample_rate
        .ok_or(DvAudioError::UnsupportedSampleRate)?;
    let rate_index = match sample_rate {
        48_000 => 0,
        44_100 => 1,
        32_000 => 2,
        _ => return Err(DvAudioError::UnsupportedSampleRate),
    };
    if source.source_type != 0 {
        return Err(DvAudioError::UnsupportedChannelLayout(source.source_type));
    }
    let samples_per_channel =
        frame.profile().audio_min_samples[rate_index] + usize::from(source.sample_count_delta);
    match source.quantization {
        AudioQuantization::Linear16 => Ok(vec![extract_linear16(
            frame,
            sample_rate,
            samples_per_channel,
        )]),
        AudioQuantization::Nonlinear12 => {
            Ok(extract_nonlinear12(frame, sample_rate, samples_per_channel))
        }
        AudioQuantization::Reserved(value) => Err(DvAudioError::UnsupportedQuantization(value)),
    }
}

fn extract_linear16(
    frame: &DvFrame<'_>,
    sample_rate: u32,
    samples_per_channel: usize,
) -> AudioFrame {
    let profile = frame.profile();
    let mut samples = vec![0_i16; samples_per_channel * 2];
    for sequence in 0..profile.dif_sequences {
        for audio_block in 0..9 {
            let block_offset = (sequence * 150 + 6 + audio_block * 16) * 80;
            let block = &frame.data()[block_offset..block_offset + 80];
            for byte_offset in (8..80).step_by(2) {
                let output = shuffle(profile, sequence, audio_block)
                    + (byte_offset - 8) / 2 * profile.audio_stride;
                if output >= samples.len() {
                    continue;
                }
                let value = i16::from_be_bytes([block[byte_offset], block[byte_offset + 1]]);
                samples[output] = if value == i16::MIN { 0 } else { value };
            }
        }
    }
    audio_frame(sample_rate, samples_per_channel, samples)
}

fn extract_nonlinear12(
    frame: &DvFrame<'_>,
    sample_rate: u32,
    samples_per_channel: usize,
) -> Vec<AudioFrame> {
    let profile = frame.profile();
    let half = profile.dif_sequences / 2;
    let mut pairs = [
        vec![0_i16; samples_per_channel * 2],
        vec![0_i16; samples_per_channel * 2],
    ];
    for sequence in 0..profile.dif_sequences {
        let pair = usize::from(sequence >= half);
        let row = sequence % half;
        for audio_block in 0..9 {
            let block_offset = (sequence * 150 + 6 + audio_block * 16) * 80;
            let block = &frame.data()[block_offset..block_offset + 80];
            for byte_offset in (8..80).step_by(3) {
                if byte_offset + 2 >= 80 {
                    break;
                }
                let left =
                    u16::from(block[byte_offset]) << 4 | u16::from(block[byte_offset + 2] >> 4);
                let right = u16::from(block[byte_offset + 1]) << 4
                    | u16::from(block[byte_offset + 2] & 0x0f);
                let sample_group = (byte_offset - 8) / 3;
                let left_output =
                    shuffle(profile, row, audio_block) + sample_group * profile.audio_stride;
                let right_output =
                    shuffle(profile, row + half, audio_block) + sample_group * profile.audio_stride;
                if left_output < pairs[pair].len() {
                    pairs[pair][left_output] = nonlinear_to_i16(left);
                }
                if right_output < pairs[pair].len() {
                    pairs[pair][right_output] = nonlinear_to_i16(right);
                }
            }
        }
    }
    pairs
        .into_iter()
        .map(|samples| audio_frame(sample_rate, samples_per_channel, samples))
        .collect()
}

fn audio_frame(sample_rate: u32, samples_per_channel: usize, samples: Vec<i16>) -> AudioFrame {
    AudioFrame {
        format: AudioSampleFormat::I16Interleaved,
        sample_rate,
        channels: 2,
        samples_per_channel,
        samples,
        timing: FrameTiming::default(),
    }
}

pub(crate) fn shuffle(profile: DvProfile, sequence: usize, audio_block: usize) -> usize {
    match profile.system {
        crate::DvSystem::System525_60 => usize::from(SHUFFLE_525[sequence][audio_block]),
        crate::DvSystem::System625_50 => usize::from(SHUFFLE_625[sequence][audio_block]),
    }
}

#[allow(clippy::cast_possible_wrap)]
fn nonlinear_to_i16(sample: u16) -> i16 {
    if sample == 0x800 {
        return 0;
    }
    let signed = if sample < 0x800 {
        sample
    } else {
        sample | 0xf000
    };
    let mut shift = (signed & 0x0f00) >> 8;
    let result = if !(2..=13).contains(&shift) {
        signed
    } else if shift < 8 {
        shift -= 1;
        signed.wrapping_sub(256 * shift) << shift
    } else {
        shift = 14 - shift;
        (signed.wrapping_add(256 * shift + 1) << shift).wrapping_sub(1)
    };
    result as i16
}

#[cfg(test)]
mod tests {
    use crate::{DvPackData, dif::synthetic_frame, parse_frame};

    use super::*;

    fn frame_with_audio(profile: DvProfile, quantization: u8) -> Vec<u8> {
        let mut data = synthetic_frame(profile);
        // The first audio block carries the source pack in this synthetic vector.
        let offset = 6 * 80 + 3;
        data[offset..offset + 5].copy_from_slice(&[0x50, 0, 0, 0, quantization]);
        data
    }

    #[test]
    fn unshuffles_linear_sentinel_and_sample() {
        let mut data = frame_with_audio(DvProfile::DV25_525_60, 0);
        let audio = 6 * 80;
        data[audio + 8..audio + 12].copy_from_slice(&[0x12, 0x34, 0x80, 0x00]);
        let frame = parse_frame(&data).unwrap();
        assert!(
            frame
                .packs()
                .iter()
                .any(|pack| matches!(pack.data, DvPackData::AudioSource(_)))
        );
        let audio = extract_audio(&frame).unwrap();
        assert_eq!(audio.len(), 1);
        assert_eq!(audio[0].samples_per_channel, 1_580);
        assert_eq!(audio[0].samples[0], 0x1234);
        assert_eq!(audio[0].samples[90], 0);
        audio[0].validate().unwrap();
    }

    #[test]
    fn expands_nonlinear_error_code_to_silence() {
        assert_eq!(nonlinear_to_i16(0x800), 0);
        assert_eq!(nonlinear_to_i16(0x001), 1);
    }
}
