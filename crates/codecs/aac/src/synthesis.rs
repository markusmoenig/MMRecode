//! FFT-backed AAC-LC synthesis filterbank. Transform normalization is in signed-16 PCM units.

use std::{f64::consts::PI, sync::OnceLock};

use crate::syntax::SpectralChannel;

#[derive(Debug)]
struct Windows {
    long: [Vec<f64>; 2],
    short: [Vec<f64>; 2],
}

#[derive(Debug)]
struct Dct4Plan {
    pre_rotation: Vec<[f64; 2]>,
    post_rotation: Vec<[f64; 2]>,
}

#[allow(clippy::cast_precision_loss)] // AAC transform indices are at most 1024.
fn dct4_plan(length: usize) -> &'static Dct4Plan {
    fn build(length: usize) -> Dct4Plan {
        let pre_rotation = (0..length)
            .map(|index| {
                let angle = PI * index as f64 / (2.0 * length as f64);
                [angle.cos(), angle.sin()]
            })
            .collect();
        let post_rotation = (0..length)
            .map(|index| {
                let angle = PI * (2 * index + 1) as f64 / (4.0 * length as f64);
                [angle.cos(), angle.sin()]
            })
            .collect();
        Dct4Plan {
            pre_rotation,
            post_rotation,
        }
    }
    static LONG: OnceLock<Dct4Plan> = OnceLock::new();
    static SHORT: OnceLock<Dct4Plan> = OnceLock::new();
    match length {
        1_024 => LONG.get_or_init(|| build(1_024)),
        128 => SHORT.get_or_init(|| build(128)),
        _ => unreachable!("AAC-LC uses 1024- or 128-point transforms"),
    }
}

fn windows() -> &'static Windows {
    fn build() -> Windows {
        Windows {
            long: [sine(1_024), kbd(1_024, 4.0)],
            short: [sine(128), kbd(128, 6.0)],
        }
    }
    static WINDOWS: OnceLock<Windows> = OnceLock::new();
    WINDOWS.get_or_init(build)
}

#[derive(Debug)]
pub(crate) struct Filterbank {
    overlap: Vec<f64>,
    previous_kbd: bool,
}

impl Default for Filterbank {
    fn default() -> Self {
        Self {
            overlap: vec![0.0; 1_024],
            previous_kbd: false,
        }
    }
}

impl Filterbank {
    pub(crate) fn synthesize(&mut self, channel: &SpectralChannel) -> Vec<f64> {
        debug_assert_eq!(channel.coefficients.len(), 1_024);
        let windows = windows();
        let previous = usize::from(self.previous_kbd);
        let current = usize::from(channel.kbd);
        let mut block = vec![0.0; 2_048];
        if channel.sequence == 2 {
            for window in 0..8 {
                let transformed = imdct(&channel.coefficients[window * 128..(window + 1) * 128]);
                let left = &windows.short[if window == 0 { previous } else { current }];
                let right = &windows.short[current];
                let offset = 448 + window * 128;
                for index in 0..128 {
                    block[offset + index] += transformed[index] * left[index];
                    block[offset + 128 + index] += transformed[128 + index] * right[127 - index];
                }
            }
        } else {
            block = imdct(&channel.coefficients);
            for index in 0..1_024 {
                let left = if channel.sequence == 3 {
                    if index < 448 {
                        0.0
                    } else if index < 576 {
                        windows.short[previous][index - 448]
                    } else {
                        1.0
                    }
                } else {
                    windows.long[previous][index]
                };
                let right = if channel.sequence == 1 {
                    if index < 448 {
                        1.0
                    } else if index < 576 {
                        windows.short[current][575 - index]
                    } else {
                        0.0
                    }
                } else {
                    windows.long[current][1_023 - index]
                };
                block[index] *= left;
                block[1_024 + index] *= right;
            }
        }
        for (sample, previous) in block[..1_024].iter_mut().zip(&self.overlap) {
            *sample += previous;
        }
        self.overlap.copy_from_slice(&block[1_024..]);
        self.previous_kbd = channel.kbd;
        block.truncate(1_024);
        block
    }
}

// A DCT-IV is the first half of one 2N-point inverse FFT of a pre-rotated real input. IMDCT
// symmetry expands its N results into the required 2N samples. AAC-LC transform lengths are powers
// of two, so a compact radix-2 implementation avoids both the former O(N²) synthesis cost and an
// external transform dependency.
#[allow(clippy::cast_precision_loss)] // AAC transform lengths and indices are at most 1024.
fn imdct(spectrum: &[f64]) -> Vec<f64> {
    let length = spectrum.len();
    let plan = dct4_plan(length);
    let mut work: Vec<_> = spectrum
        .iter()
        .zip(&plan.pre_rotation)
        .map(|(&value, rotation)| [value * rotation[0], value * rotation[1]])
        .collect();
    work.resize(length * 2, [0.0, 0.0]);
    inverse_fft(&mut work);
    let scale = 1.0 / length as f64;
    let dct: Vec<_> = work[..length]
        .iter()
        .zip(&plan.post_rotation)
        .map(|(value, rotation)| (value[0] * rotation[0] - value[1] * rotation[1]) * scale)
        .collect();
    let half = length / 2;
    let mut output = Vec::with_capacity(length * 2);
    output.extend_from_slice(&dct[half..]);
    output.extend(dct.iter().rev().map(|value| -*value));
    output.extend(dct[..half].iter().map(|value| -*value));
    output
}

#[allow(clippy::cast_precision_loss)] // FFT widths are at most 2048.
fn inverse_fft(values: &mut [[f64; 2]]) {
    debug_assert!(values.len().is_power_of_two());
    let bits = values.len().trailing_zeros();
    for index in 0..values.len() {
        let reversed = index.reverse_bits() >> (usize::BITS - bits);
        if reversed > index {
            values.swap(index, reversed);
        }
    }
    let mut width = 2;
    while width <= values.len() {
        let half = width / 2;
        let angle = 2.0 * PI / width as f64;
        let step = [angle.cos(), angle.sin()];
        for base in (0..values.len()).step_by(width) {
            let mut rotation = [1.0, 0.0];
            for index in 0..half {
                let left = values[base + index];
                let right = values[base + index + half];
                let product = [
                    right[0] * rotation[0] - right[1] * rotation[1],
                    right[0] * rotation[1] + right[1] * rotation[0],
                ];
                values[base + index] = [left[0] + product[0], left[1] + product[1]];
                values[base + index + half] = [left[0] - product[0], left[1] - product[1]];
                rotation = [
                    rotation[0] * step[0] - rotation[1] * step[1],
                    rotation[0] * step[1] + rotation[1] * step[0],
                ];
            }
        }
        width *= 2;
    }
}

#[allow(clippy::cast_precision_loss)] // Bounded transform indices, not media timestamps.
fn sine(length: usize) -> Vec<f64> {
    (0..length)
        .map(|index| (PI * (index as f64 + 0.5) / (2.0 * length as f64)).sin())
        .collect()
}

// I0's power series converges rapidly for the bounded AAC alpha=4/6 arguments.
fn bessel_i0(value: f64) -> f64 {
    let argument = value * value / 4.0;
    let mut sum = 1.0;
    let mut term = 1.0;
    for order in 1..=100 {
        term *= argument / f64::from(order * order);
        sum += term;
        if term <= sum * f64::EPSILON {
            break;
        }
    }
    sum
}

#[allow(clippy::cast_precision_loss)] // Bounded transform indices.
fn kbd(length: usize, alpha: f64) -> Vec<f64> {
    let weights: Vec<_> = (0..=length)
        .map(|index| {
            let position = 2.0 * index as f64 / length as f64 - 1.0;
            bessel_i0(PI * alpha * (1.0 - position * position).max(0.0).sqrt())
        })
        .collect();
    let total: f64 = weights.iter().sum();
    let mut cumulative = 0.0;
    weights[..length]
        .iter()
        .map(|weight| {
            cumulative += weight;
            (cumulative / total).sqrt()
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sine_and_kbd_windows_obey_power_complementarity() {
        for window in windows().long.iter().chain(&windows().short) {
            for (&left, &right) in window.iter().zip(window.iter().rev()) {
                assert!((left * left + right * right - 1.0).abs() < 1e-12);
            }
            assert!(window.windows(2).all(|pair| pair[0] < pair[1]));
        }
    }

    #[test]
    #[allow(clippy::cast_precision_loss)]
    fn recurrence_matches_direct_imdct_definition() {
        for length in [128_usize, 1_024] {
            let spectrum: Vec<_> = (0..length)
                .map(|index| f64::from(u8::try_from(index % 31).unwrap()) - 15.0)
                .collect();
            for (sample, actual) in imdct(&spectrum).iter().enumerate() {
                let expected: f64 = spectrum
                    .iter()
                    .enumerate()
                    .map(|(index, coefficient)| {
                        coefficient
                            * (PI / length as f64
                                * (sample as f64 + 0.5 + length as f64 / 2.0)
                                * (index as f64 + 0.5))
                                .cos()
                            / length as f64
                    })
                    .sum();
                assert!(
                    (actual - expected).abs() < 1e-9,
                    "{length}: {sample}: {actual} vs {expected}"
                );
            }
        }
    }

    #[test]
    fn zero_spectrum_after_nonzero_frame_preserves_overlap_tail() {
        let mut filter = Filterbank::default();
        let mut channel = SpectralChannel {
            sequence: 0,
            kbd: false,
            coefficients: vec![0.0; 1024],
        };
        channel.coefficients[2] = 1000.0;
        assert!(filter.synthesize(&channel).iter().any(|v| v.abs() > 0.1));
        channel.coefficients.fill(0.0);
        assert!(filter.synthesize(&channel).iter().any(|v| v.abs() > 0.1));
        assert!(filter.synthesize(&channel).iter().all(|v| *v == 0.0));
    }
}
