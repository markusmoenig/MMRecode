//! Portable MPEG-2 inverse DCT reference path.

#![allow(clippy::unreadable_literal)]

const SIDE: usize = 8;
const MATRIX: [[f64; SIDE]; SIDE] = [
    [
        0.3535533905932738,
        0.4903926402016152,
        0.4619397662556434,
        0.4157348061512726,
        0.3535533905932738,
        0.2777851165098011,
        0.1913417161825449,
        0.09754516100806417,
    ],
    [
        0.3535533905932738,
        0.4157348061512726,
        0.1913417161825449,
        -0.0975451610080641,
        -0.3535533905932737,
        -0.4903926402016152,
        -0.4619397662556434,
        -0.2777851165098011,
    ],
    [
        0.3535533905932738,
        0.2777851165098011,
        -0.1913417161825448,
        -0.4903926402016152,
        -0.3535533905932738,
        0.09754516100806415,
        0.4619397662556433,
        0.4157348061512727,
    ],
    [
        0.3535533905932738,
        0.09754516100806417,
        -0.4619397662556434,
        -0.2777851165098011,
        0.3535533905932737,
        0.4157348061512727,
        -0.191341716182545,
        -0.4903926402016152,
    ],
    [
        0.3535533905932738,
        -0.0975451610080641,
        -0.4619397662556434,
        0.2777851165098009,
        0.3535533905932738,
        -0.4157348061512726,
        -0.1913417161825449,
        0.4903926402016152,
    ],
    [
        0.3535533905932738,
        -0.277785116509801,
        -0.1913417161825452,
        0.4903926402016152,
        -0.3535533905932733,
        -0.097545161008064,
        0.4619397662556437,
        -0.415734806151272,
    ],
    [
        0.3535533905932738,
        -0.4157348061512727,
        0.191341716182545,
        0.097545161008064,
        -0.3535533905932736,
        0.4903926402016153,
        -0.4619397662556435,
        0.2777851165098008,
    ],
    [
        0.3535533905932738,
        -0.4903926402016152,
        0.4619397662556433,
        -0.415734806151272,
        0.3535533905932732,
        -0.2777851165098008,
        0.1913417161825431,
        -0.0975451610080625,
    ],
];

#[allow(clippy::cast_possible_truncation)]
pub(crate) fn inverse_dct(coefficients: &[i32; 64]) -> [i16; 64] {
    let mut temporary = [0.0_f64; 64];
    for v in 0..SIDE {
        for x in 0..SIDE {
            for u in 0..SIDE {
                temporary[v * SIDE + x] += MATRIX[x][u] * f64::from(coefficients[v * SIDE + u]);
            }
        }
    }
    let mut output = [0_i16; 64];
    for y in 0..SIDE {
        for x in 0..SIDE {
            let mut sum = 0.0_f64;
            for v in 0..SIDE {
                sum += MATRIX[y][v] * temporary[v * SIDE + x];
            }
            output[y * SIDE + x] = sum.round().clamp(-256.0, 255.0) as i16;
        }
    }
    output
}

#[allow(clippy::cast_possible_truncation)]
pub(crate) fn forward_dct(samples: &[i16; 64]) -> [i32; 64] {
    let mut temporary = [0.0_f64; 64];
    for y in 0..SIDE {
        for u in 0..SIDE {
            for x in 0..SIDE {
                temporary[y * SIDE + u] += MATRIX[x][u] * f64::from(samples[y * SIDE + x]);
            }
        }
    }
    let mut coefficients = [0_i32; 64];
    for v in 0..SIDE {
        for u in 0..SIDE {
            let mut value = 0.0_f64;
            for y in 0..SIDE {
                value += MATRIX[y][v] * temporary[y * SIDE + u];
            }
            coefficients[v * SIDE + u] = value.round() as i32;
        }
    }
    coefficients
}

#[cfg(test)]
mod tests {
    use super::{forward_dct, inverse_dct};

    #[test]
    fn reconstructs_neutral_and_residual_dc_blocks() {
        let mut coefficients = [0_i32; 64];
        assert_eq!(inverse_dct(&coefficients), [0; 64]);
        coefficients[0] = 1_024;
        assert_eq!(inverse_dct(&coefficients), [128; 64]);
        coefficients[0] = 80;
        assert_eq!(inverse_dct(&coefficients), [10; 64]);
    }

    #[test]
    fn transforms_constant_blocks_to_dc_only() {
        let coefficients = forward_dct(&[128; 64]);
        assert_eq!(coefficients[0], 1_024);
        assert!(coefficients[1..].iter().all(|&value| value == 0));
    }
}
