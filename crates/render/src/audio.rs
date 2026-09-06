//! Deterministic sample-domain audio placement, resampling, channel mapping, and mixing.

use mmrecode_core::{
    AudioFrame, AudioSampleFormat, Error, FrameTiming, Rational, Result, Timestamp,
    TimestampRounding,
};

/// One decoded PCM region placed on the output timeline.
#[derive(Clone, Copy, Debug)]
pub struct AudioPlacement<'a> {
    /// Complete decoded source PCM.
    pub source: &'a AudioFrame,
    /// Output-timeline position of the first selected source sample.
    pub timeline_start: Timestamp,
    /// First selected sample per channel in `source`.
    pub source_start: usize,
    /// Number of selected source samples per channel.
    pub source_samples: usize,
    /// Linear amplitude applied before summing.
    pub gain: f32,
}

/// Resolved format and duration of one audio mix.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AudioMixSettings {
    /// Output samples per second for each channel.
    pub sample_rate: u32,
    /// Mono or stereo output channel count.
    pub channels: u16,
    /// Exact output duration in samples per channel.
    pub samples_per_channel: usize,
}

/// Mixes decoded mono/stereo PCM placements into one signed-16 output buffer.
///
/// Placement starts are rounded to the nearest output sample with ties away from zero. Differing
/// sample rates use deterministic linear interpolation. Mono is duplicated into stereo; stereo is
/// averaged into mono; equal layouts retain their channels. Accumulation happens in floating point
/// and saturates only once at the final signed-16 boundary.
///
/// # Errors
///
/// Returns an error for invalid frames/ranges, non-finite gain, unsupported channel layouts, or
/// timing and allocation overflow.
#[allow(clippy::cast_possible_truncation, clippy::too_many_lines)]
pub fn mix_audio_timeline(
    placements: &[AudioPlacement<'_>],
    settings: AudioMixSettings,
) -> Result<AudioFrame> {
    if settings.sample_rate == 0 || !matches!(settings.channels, 1 | 2) {
        return Err(Error::Unsupported(
            "audio mixing currently requires positive-rate mono or stereo output".into(),
        ));
    }
    let output_channels = usize::from(settings.channels);
    let output_samples = settings
        .samples_per_channel
        .checked_mul(output_channels)
        .ok_or_else(|| Error::InvalidData("audio mix allocation overflows".into()))?;
    let output_time_base = Rational::new(1, i64::from(settings.sample_rate))?;
    let mut mixed = vec![0.0_f64; output_samples];
    for placement in placements {
        placement.source.validate()?;
        if placement.source.format != AudioSampleFormat::I16Interleaved
            || !matches!(placement.source.channels, 1 | 2)
        {
            return Err(Error::Unsupported(
                "audio mixing supports signed-16 mono/stereo sources".into(),
            ));
        }
        if !placement.gain.is_finite() || placement.gain < 0.0 {
            return Err(Error::InvalidData(
                "audio placement gain must be finite and non-negative".into(),
            ));
        }
        let source_end = placement
            .source_start
            .checked_add(placement.source_samples)
            .filter(|end| *end <= placement.source.samples_per_channel)
            .ok_or_else(|| Error::InvalidData("audio placement exceeds its source range".into()))?;
        let start = placement
            .timeline_start
            .rescale(output_time_base, TimestampRounding::NearestTiesAway)?
            .value;
        let destination_length = u128::try_from(placement.source_samples)
            .map_err(|_| Error::InvalidData("audio source duration exceeds u128".into()))?
            .checked_mul(u128::from(settings.sample_rate))
            .ok_or_else(|| Error::InvalidData("resampled audio duration overflows".into()))?
            .div_ceil(u128::from(placement.source.sample_rate));
        let destination_length = i64::try_from(destination_length)
            .map_err(|_| Error::InvalidData("resampled audio duration exceeds i64".into()))?;
        let visible_start = start.max(0);
        let visible_end = start
            .checked_add(destination_length)
            .ok_or_else(|| Error::InvalidData("audio placement end overflows".into()))?
            .min(
                i64::try_from(settings.samples_per_channel)
                    .map_err(|_| Error::InvalidData("audio output duration exceeds i64".into()))?,
            );
        if visible_start >= visible_end {
            continue;
        }
        let source_channels = usize::from(placement.source.channels);
        for destination in visible_start..visible_end {
            let local = u128::try_from(destination - start)
                .map_err(|_| Error::InvalidData("audio local position is negative".into()))?;
            let numerator = local
                .checked_mul(u128::from(placement.source.sample_rate))
                .ok_or_else(|| Error::InvalidData("audio resample position overflows".into()))?;
            let base = usize::try_from(numerator / u128::from(settings.sample_rate))
                .map_err(|_| Error::InvalidData("audio source position exceeds usize".into()))?;
            let remainder = u32::try_from(numerator % u128::from(settings.sample_rate))
                .map_err(|_| Error::InvalidData("audio resample remainder exceeds u32".into()))?;
            let first = placement.source_start + base.min(placement.source_samples - 1);
            let second = (first + 1).min(source_end - 1);
            let fraction = f64::from(remainder) / f64::from(settings.sample_rate);
            let destination = usize::try_from(destination)
                .map_err(|_| Error::InvalidData("audio output position exceeds usize".into()))?;
            for output_channel in 0..output_channels {
                let sample = mapped_sample(
                    placement.source,
                    first,
                    second,
                    fraction,
                    source_channels,
                    output_channels,
                    output_channel,
                );
                mixed[destination * output_channels + output_channel] +=
                    sample * f64::from(placement.gain);
            }
        }
    }
    let samples = mixed
        .into_iter()
        .map(|sample| {
            sample
                .round()
                .clamp(f64::from(i16::MIN), f64::from(i16::MAX)) as i16
        })
        .collect();
    Ok(AudioFrame {
        format: AudioSampleFormat::I16Interleaved,
        sample_rate: settings.sample_rate,
        channels: settings.channels,
        samples_per_channel: settings.samples_per_channel,
        samples,
        timing: FrameTiming {
            pts: Some(Timestamp {
                value: 0,
                time_base: output_time_base,
            }),
            duration: Some(Timestamp {
                value: i64::try_from(settings.samples_per_channel)
                    .map_err(|_| Error::InvalidData("audio mix duration exceeds i64".into()))?,
                time_base: output_time_base,
            }),
        },
    })
}

#[allow(clippy::too_many_arguments)]
fn mapped_sample(
    source: &AudioFrame,
    first: usize,
    second: usize,
    fraction: f64,
    source_channels: usize,
    output_channels: usize,
    output_channel: usize,
) -> f64 {
    let interpolate = |channel: usize| {
        let first = f64::from(source.samples[first * source_channels + channel]);
        let second = f64::from(source.samples[second * source_channels + channel]);
        first + (second - first) * fraction
    };
    match (source_channels, output_channels) {
        (1, _) => interpolate(0),
        (2, 1) => f64::midpoint(interpolate(0), interpolate(1)),
        (2, 2) => interpolate(output_channel),
        _ => unreachable!("validated mono/stereo layouts"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(sample_rate: u32, channels: u16, samples: Vec<i16>) -> AudioFrame {
        AudioFrame {
            format: AudioSampleFormat::I16Interleaved,
            sample_rate,
            channels,
            samples_per_channel: samples.len() / usize::from(channels),
            samples,
            timing: FrameTiming::default(),
        }
    }

    #[test]
    fn resamples_places_and_duplicates_mono_into_stereo() {
        let source = frame(24_000, 1, vec![0, 1_000, 2_000, 3_000]);
        let mixed = mix_audio_timeline(
            &[AudioPlacement {
                source: &source,
                timeline_start: Timestamp {
                    value: 2,
                    time_base: Rational::new(1, 48_000).unwrap(),
                },
                source_start: 0,
                source_samples: 4,
                gain: 1.0,
            }],
            AudioMixSettings {
                sample_rate: 48_000,
                channels: 2,
                samples_per_channel: 10,
            },
        )
        .unwrap();
        assert_eq!(
            mixed.samples,
            [
                0, 0, 0, 0, 0, 0, 500, 500, 1_000, 1_000, 1_500, 1_500, 2_000, 2_000, 2_500, 2_500,
                3_000, 3_000, 3_000, 3_000
            ]
        );
    }

    #[test]
    fn overlaps_with_gain_maps_stereo_to_mono_and_saturates_once() {
        let first = frame(48_000, 2, vec![20_000, 10_000, 20_000, 10_000]);
        let second = frame(48_000, 1, vec![20_000, 20_000]);
        let start = Timestamp {
            value: 0,
            time_base: Rational::new(1, 48_000).unwrap(),
        };
        let mixed = mix_audio_timeline(
            &[
                AudioPlacement {
                    source: &first,
                    timeline_start: start,
                    source_start: 0,
                    source_samples: 2,
                    gain: 1.0,
                },
                AudioPlacement {
                    source: &second,
                    timeline_start: start,
                    source_start: 0,
                    source_samples: 2,
                    gain: 1.0,
                },
            ],
            AudioMixSettings {
                sample_rate: 48_000,
                channels: 1,
                samples_per_channel: 2,
            },
        )
        .unwrap();
        assert_eq!(mixed.samples, [i16::MAX, i16::MAX]);
    }
}
