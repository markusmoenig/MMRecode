const BLOCK_SIDE: usize = 8;
const IDCT_MATRIX: [[f64; BLOCK_SIDE]; BLOCK_SIDE] = [
    [
        0.353_553_390_593_273_8,
        0.490_392_640_201_615_2,
        0.461_939_766_255_643_4,
        0.415_734_806_151_272_6,
        0.353_553_390_593_273_8,
        0.277_785_116_509_801_1,
        0.191_341_716_182_544_9,
        0.097_545_161_008_064_17,
    ],
    [
        0.353_553_390_593_273_8,
        0.415_734_806_151_272_6,
        0.191_341_716_182_544_9,
        -0.097_545_161_008_064_1,
        -0.353_553_390_593_273_7,
        -0.490_392_640_201_615_2,
        -0.461_939_766_255_643_4,
        -0.277_785_116_509_801_1,
    ],
    [
        0.353_553_390_593_273_8,
        0.277_785_116_509_801_1,
        -0.191_341_716_182_544_8,
        -0.490_392_640_201_615_2,
        -0.353_553_390_593_273_8,
        0.097_545_161_008_064_15,
        0.461_939_766_255_643_3,
        0.415_734_806_151_272_7,
    ],
    [
        0.353_553_390_593_273_8,
        0.097_545_161_008_064_17,
        -0.461_939_766_255_643_4,
        -0.277_785_116_509_801_1,
        0.353_553_390_593_273_7,
        0.415_734_806_151_272_7,
        -0.191_341_716_182_545,
        -0.490_392_640_201_615_2,
    ],
    [
        0.353_553_390_593_273_8,
        -0.097_545_161_008_064_1,
        -0.461_939_766_255_643_4,
        0.277_785_116_509_800_9,
        0.353_553_390_593_273_8,
        -0.415_734_806_151_272_6,
        -0.191_341_716_182_544_9,
        0.490_392_640_201_615_2,
    ],
    [
        0.353_553_390_593_273_8,
        -0.277_785_116_509_801,
        -0.191_341_716_182_545_2,
        0.490_392_640_201_615_2,
        -0.353_553_390_593_273_3,
        -0.097_545_161_008_064_0,
        0.461_939_766_255_643_7,
        -0.415_734_806_151_272,
    ],
    [
        0.353_553_390_593_273_8,
        -0.415_734_806_151_272_7,
        0.191_341_716_182_545,
        0.097_545_161_008_064_0,
        -0.353_553_390_593_273_6,
        0.490_392_640_201_615_3,
        -0.461_939_766_255_643_5,
        0.277_785_116_509_800_8,
    ],
    [
        0.353_553_390_593_273_8,
        -0.490_392_640_201_615_2,
        0.461_939_766_255_643_3,
        -0.415_734_806_151_272,
        0.353_553_390_593_273_2,
        -0.277_785_116_509_800_8,
        0.191_341_716_182_543_1,
        -0.097_545_161_008_062_5,
    ],
];

#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
pub(crate) fn inverse_dct(coefficients: &[i32; 64]) -> [u8; 64] {
    let mut temporary = [0.0_f64; 64];
    for v in 0..BLOCK_SIDE {
        for x in 0..BLOCK_SIDE {
            for u in 0..BLOCK_SIDE {
                temporary[v * BLOCK_SIDE + x] +=
                    IDCT_MATRIX[x][u] * f64::from(coefficients[v * BLOCK_SIDE + u]);
            }
        }
    }

    let mut output = [0_u8; 64];
    for y in 0..BLOCK_SIDE {
        for x in 0..BLOCK_SIDE {
            let mut sum = 0.0_f64;
            for v in 0..BLOCK_SIDE {
                sum += IDCT_MATRIX[y][v] * temporary[v * BLOCK_SIDE + x];
            }
            output[y * BLOCK_SIDE + x] = (sum + 128.0).round().clamp(0.0, 255.0) as u8;
        }
    }
    output
}

#[allow(clippy::cast_possible_truncation)]
pub(crate) fn forward_dct_quantize(samples: &[u8; 64], quantization: &[u8; 64]) -> [i32; 64] {
    let mut temporary = [0.0_f64; 64];
    for y in 0..BLOCK_SIDE {
        for u in 0..BLOCK_SIDE {
            for x in 0..BLOCK_SIDE {
                temporary[y * BLOCK_SIDE + u] +=
                    IDCT_MATRIX[x][u] * (f64::from(samples[y * BLOCK_SIDE + x]) - 128.0);
            }
        }
    }

    let mut coefficients = [0_i32; 64];
    for v in 0..BLOCK_SIDE {
        for u in 0..BLOCK_SIDE {
            let mut value = 0.0_f64;
            for y in 0..BLOCK_SIDE {
                value += IDCT_MATRIX[y][v] * temporary[y * BLOCK_SIDE + u];
            }
            coefficients[v * BLOCK_SIDE + u] =
                (value / f64::from(quantization[v * BLOCK_SIDE + u])).round() as i32;
        }
    }
    coefficients
}

#[cfg(test)]
mod tests {
    use super::{forward_dct_quantize, inverse_dct};

    #[test]
    fn reconstructs_constant_dc_blocks() {
        let mut coefficients = [0_i32; 64];
        assert_eq!(inverse_dct(&coefficients), [128; 64]);
        coefficients[0] = 80;
        assert_eq!(inverse_dct(&coefficients), [138; 64]);
        coefficients[0] = -1_024;
        assert_eq!(inverse_dct(&coefficients), [0; 64]);
    }

    #[test]
    fn transforms_constant_blocks_to_dc_only() {
        let coefficients = forward_dct_quantize(&[138; 64], &[1; 64]);
        assert_eq!(coefficients[0], 80);
        assert!(coefficients[1..].iter().all(|&value| value == 0));
    }
}
