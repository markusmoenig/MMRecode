use mmrecode_core::{Error, Result};

const ALPHA: [i32; 52] = [
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 4, 4, 5, 6, 7, 8, 9, 10, 12, 13, 15, 17, 20,
    22, 25, 28, 32, 36, 40, 45, 50, 56, 63, 71, 80, 90, 101, 113, 127, 144, 162, 182, 203, 226,
    255, 255,
];

const BETA: [i32; 52] = [
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 6, 6, 7, 7, 8, 8,
    9, 9, 10, 10, 11, 11, 12, 12, 13, 13, 14, 14, 15, 15, 16, 16, 17, 17, 18, 18,
];

const TC0: [[i32; 52]; 3] = [
    [
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 1, 1, 1,
        1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 4, 4, 4, 5, 6, 6, 7, 8, 9, 10, 11, 13,
    ],
    [
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 1, 1, 1, 1, 1,
        1, 2, 2, 2, 2, 3, 3, 3, 4, 4, 5, 5, 6, 7, 8, 8, 10, 11, 12, 13, 15, 17,
    ],
    [
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 2, 2, 2,
        2, 3, 3, 3, 4, 4, 4, 5, 6, 6, 7, 8, 9, 10, 11, 13, 14, 16, 18, 20, 23, 25,
    ],
];

#[derive(Clone, Copy, Debug)]
pub(crate) struct Parameters {
    pub(crate) offset_a: i32,
    pub(crate) offset_b: i32,
}

pub(crate) struct Picture<'a> {
    pub(crate) luma: &'a mut [u8],
    pub(crate) cb: &'a mut [u8],
    pub(crate) cr: &'a mut [u8],
    pub(crate) coded_width: usize,
    pub(crate) coded_height: usize,
    pub(crate) luma_qp: &'a [i32],
    pub(crate) chroma_qp_offset_cb: i32,
    pub(crate) chroma_qp_offset_cr: i32,
    pub(crate) macroblock_intra: &'a [bool],
    pub(crate) luma_nonzero: &'a [[u8; 16]],
    pub(crate) motion: &'a [[MotionInfo; 16]],
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct MotionInfo {
    pub(crate) x: i32,
    pub(crate) y: i32,
    pub(crate) reference_index: Option<u8>,
}

pub(crate) fn filter_picture(picture: &mut Picture<'_>, params: Parameters) -> Result<()> {
    let macroblocks_wide = picture.coded_width / 16;
    let macroblocks_high = picture.coded_height / 16;
    let expected_qps = macroblocks_wide
        .checked_mul(macroblocks_high)
        .ok_or_else(|| Error::InvalidData("H.264 deblocking macroblock count overflows".into()))?;
    if picture.luma_qp.len() != expected_qps
        || picture.macroblock_intra.len() != expected_qps
        || picture.luma_nonzero.len() != expected_qps
        || picture.motion.len() != expected_qps
    {
        return Err(Error::InvalidData(
            "H.264 deblocking QP map does not match the picture".into(),
        ));
    }
    for address in 0..expected_qps {
        filter_luma_macroblock(picture, address, macroblocks_wide, params);
        filter_chroma_macroblock(
            picture.cb,
            picture.coded_width / 2,
            address,
            macroblocks_wide,
            picture.luma_qp,
            picture.chroma_qp_offset_cb,
            picture.macroblock_intra,
            picture.luma_nonzero,
            picture.motion,
            params,
        );
        filter_chroma_macroblock(
            picture.cr,
            picture.coded_width / 2,
            address,
            macroblocks_wide,
            picture.luma_qp,
            picture.chroma_qp_offset_cr,
            picture.macroblock_intra,
            picture.luma_nonzero,
            picture.motion,
            params,
        );
    }
    Ok(())
}

fn filter_luma_macroblock(
    picture: &mut Picture<'_>,
    address: usize,
    macroblocks_wide: usize,
    params: Parameters,
) {
    let macroblock_x = address % macroblocks_wide;
    let macroblock_y = address / macroblocks_wide;
    let origin_x = macroblock_x * 16;
    let origin_y = macroblock_y * 16;
    for edge in 0..4 {
        if edge == 0 && macroblock_x == 0 {
            continue;
        }
        let p_address = if edge == 0 { address - 1 } else { address };
        let qp = average_qp(picture.luma_qp[p_address], picture.luma_qp[address]);
        for segment in 0..4 {
            let p_block = luma_block_index(if edge == 0 { 3 } else { edge - 1 }, segment);
            let q_block = luma_block_index(if edge == 0 { 0 } else { edge }, segment);
            let strength = boundary_strength(
                picture.macroblock_intra,
                picture.luma_nonzero,
                picture.motion,
                p_address,
                p_block,
                address,
                q_block,
                edge == 0,
            );
            filter_vertical_edge(
                picture.luma,
                picture.coded_width,
                origin_x + edge * 4,
                origin_y + segment * 4,
                4,
                qp,
                strength,
                false,
                params,
            );
        }
    }
    for edge in 0..4 {
        if edge == 0 && macroblock_y == 0 {
            continue;
        }
        let p_address = if edge == 0 {
            address - macroblocks_wide
        } else {
            address
        };
        let qp = average_qp(picture.luma_qp[p_address], picture.luma_qp[address]);
        for segment in 0..4 {
            let p_block = luma_block_index(segment, if edge == 0 { 3 } else { edge - 1 });
            let q_block = luma_block_index(segment, if edge == 0 { 0 } else { edge });
            let strength = boundary_strength(
                picture.macroblock_intra,
                picture.luma_nonzero,
                picture.motion,
                p_address,
                p_block,
                address,
                q_block,
                edge == 0,
            );
            filter_horizontal_edge(
                picture.luma,
                picture.coded_width,
                origin_x + segment * 4,
                origin_y + edge * 4,
                4,
                qp,
                strength,
                false,
                params,
            );
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn filter_chroma_macroblock(
    plane: &mut [u8],
    stride: usize,
    address: usize,
    macroblocks_wide: usize,
    luma_qp: &[i32],
    chroma_qp_offset: i32,
    macroblock_intra: &[bool],
    luma_nonzero: &[[u8; 16]],
    motion: &[[MotionInfo; 16]],
    params: Parameters,
) {
    let macroblock_x = address % macroblocks_wide;
    let macroblock_y = address / macroblocks_wide;
    let origin_x = macroblock_x * 8;
    let origin_y = macroblock_y * 8;
    for edge in 0..2 {
        if edge == 0 && macroblock_x == 0 {
            continue;
        }
        let p_address = if edge == 0 { address - 1 } else { address };
        let qp = average_qp(
            chroma_qp(luma_qp[p_address], chroma_qp_offset),
            chroma_qp(luma_qp[address], chroma_qp_offset),
        );
        for segment in 0..4 {
            let p_block = luma_block_index(if edge == 0 { 3 } else { 1 }, segment);
            let q_block = luma_block_index(if edge == 0 { 0 } else { 2 }, segment);
            let strength = boundary_strength(
                macroblock_intra,
                luma_nonzero,
                motion,
                p_address,
                p_block,
                address,
                q_block,
                edge == 0,
            );
            filter_vertical_edge(
                plane,
                stride,
                origin_x + edge * 4,
                origin_y + segment * 2,
                2,
                qp,
                strength,
                true,
                params,
            );
        }
    }
    for edge in 0..2 {
        if edge == 0 && macroblock_y == 0 {
            continue;
        }
        let p_address = if edge == 0 {
            address - macroblocks_wide
        } else {
            address
        };
        let qp = average_qp(
            chroma_qp(luma_qp[p_address], chroma_qp_offset),
            chroma_qp(luma_qp[address], chroma_qp_offset),
        );
        for segment in 0..4 {
            let p_block = luma_block_index(segment, if edge == 0 { 3 } else { 1 });
            let q_block = luma_block_index(segment, if edge == 0 { 0 } else { 2 });
            let strength = boundary_strength(
                macroblock_intra,
                luma_nonzero,
                motion,
                p_address,
                p_block,
                address,
                q_block,
                edge == 0,
            );
            filter_horizontal_edge(
                plane,
                stride,
                origin_x + segment * 2,
                origin_y + edge * 4,
                2,
                qp,
                strength,
                true,
                params,
            );
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn boundary_strength(
    macroblock_intra: &[bool],
    luma_nonzero: &[[u8; 16]],
    motion: &[[MotionInfo; 16]],
    p_address: usize,
    p_block: usize,
    q_address: usize,
    q_block: usize,
    macroblock_edge: bool,
) -> usize {
    if macroblock_intra[p_address] || macroblock_intra[q_address] {
        return if macroblock_edge { 4 } else { 3 };
    }
    if luma_nonzero[p_address][p_block] != 0 || luma_nonzero[q_address][q_block] != 0 {
        return 2;
    }
    let p_motion = motion[p_address][p_block];
    let q_motion = motion[q_address][q_block];
    usize::from(
        p_motion.reference_index != q_motion.reference_index
            || (p_motion.x - q_motion.x).abs() >= 4
            || (p_motion.y - q_motion.y).abs() >= 4,
    )
}

const fn luma_block_index(x: usize, y: usize) -> usize {
    (y / 2) * 8 + (x / 2) * 4 + (y % 2) * 2 + x % 2
}

#[allow(clippy::too_many_arguments)]
fn filter_vertical_edge(
    plane: &mut [u8],
    stride: usize,
    x: usize,
    y: usize,
    length: usize,
    qp: i32,
    boundary_strength: usize,
    chroma: bool,
    params: Parameters,
) {
    for offset in 0..length {
        let q0 = (y + offset) * stride + x;
        filter_samples(
            plane,
            [q0 - 1, q0 - 2, q0 - 3, q0 - 4],
            [q0, q0 + 1, q0 + 2, q0 + 3],
            qp,
            boundary_strength,
            chroma,
            params,
        );
    }
}

#[allow(clippy::too_many_arguments)]
fn filter_horizontal_edge(
    plane: &mut [u8],
    stride: usize,
    x: usize,
    y: usize,
    length: usize,
    qp: i32,
    boundary_strength: usize,
    chroma: bool,
    params: Parameters,
) {
    for offset in 0..length {
        let q0 = y * stride + x + offset;
        filter_samples(
            plane,
            [
                q0 - stride,
                q0 - 2 * stride,
                q0 - 3 * stride,
                q0 - 4 * stride,
            ],
            [q0, q0 + stride, q0 + 2 * stride, q0 + 3 * stride],
            qp,
            boundary_strength,
            chroma,
            params,
        );
    }
}

fn filter_samples(
    plane: &mut [u8],
    p_indices: [usize; 4],
    q_indices: [usize; 4],
    qp: i32,
    boundary_strength: usize,
    chroma: bool,
    params: Parameters,
) {
    let index_a = usize::try_from((qp + params.offset_a).clamp(0, 51)).expect("clamped index");
    let index_b = usize::try_from((qp + params.offset_b).clamp(0, 51)).expect("clamped index");
    let alpha = ALPHA[index_a];
    let beta = BETA[index_b];
    let p = p_indices.map(|index| i32::from(plane[index]));
    let q = q_indices.map(|index| i32::from(plane[index]));
    if boundary_strength == 0
        || (p[0] - q[0]).abs() >= alpha
        || (p[1] - p[0]).abs() >= beta
        || (q[1] - q[0]).abs() >= beta
    {
        return;
    }
    let (filtered_p, filtered_q) = if boundary_strength == 4 {
        strong_filter(p, q, alpha, beta, chroma)
    } else {
        normal_filter(p, q, beta, index_a, boundary_strength, chroma)
    };
    for index in 0..3 {
        plane[p_indices[index]] = clip_sample(filtered_p[index]);
        plane[q_indices[index]] = clip_sample(filtered_q[index]);
    }
}

fn strong_filter(
    p: [i32; 4],
    q: [i32; 4],
    alpha: i32,
    beta: i32,
    chroma: bool,
) -> ([i32; 3], [i32; 3]) {
    let strong_p = !chroma && (p[2] - p[0]).abs() < beta && (p[0] - q[0]).abs() < (alpha >> 2) + 2;
    let strong_q = !chroma && (q[2] - q[0]).abs() < beta && (p[0] - q[0]).abs() < (alpha >> 2) + 2;
    let filtered_p = if strong_p {
        [
            (p[2] + 2 * p[1] + 2 * p[0] + 2 * q[0] + q[1] + 4) >> 3,
            (p[2] + p[1] + p[0] + q[0] + 2) >> 2,
            (2 * p[3] + 3 * p[2] + p[1] + p[0] + q[0] + 4) >> 3,
        ]
    } else {
        [(2 * p[1] + p[0] + q[1] + 2) >> 2, p[1], p[2]]
    };
    let filtered_q = if strong_q {
        [
            (p[1] + 2 * p[0] + 2 * q[0] + 2 * q[1] + q[2] + 4) >> 3,
            (p[0] + q[0] + q[1] + q[2] + 2) >> 2,
            (2 * q[3] + 3 * q[2] + q[1] + q[0] + p[0] + 4) >> 3,
        ]
    } else {
        [(2 * q[1] + q[0] + p[1] + 2) >> 2, q[1], q[2]]
    };
    (filtered_p, filtered_q)
}

fn normal_filter(
    p: [i32; 4],
    q: [i32; 4],
    beta: i32,
    index_a: usize,
    boundary_strength: usize,
    chroma: bool,
) -> ([i32; 3], [i32; 3]) {
    let tc0 = TC0[boundary_strength - 1][index_a];
    let ap = (p[2] - p[0]).abs();
    let aq = (q[2] - q[0]).abs();
    let tc = tc0
        + if chroma {
            1
        } else {
            i32::from(ap < beta) + i32::from(aq < beta)
        };
    let delta = ((((q[0] - p[0]) << 2) + (p[1] - q[1]) + 4) >> 3).clamp(-tc, tc);
    let mut filtered_p = [p[0] + delta, p[1], p[2]];
    let mut filtered_q = [q[0] - delta, q[1], q[2]];
    if !chroma && ap < beta {
        filtered_p[1] =
            p[1] + ((p[2] + ((p[0] + q[0] + 1) >> 1) - (p[1] << 1)) >> 1).clamp(-tc0, tc0);
    }
    if !chroma && aq < beta {
        filtered_q[1] =
            q[1] + ((q[2] + ((p[0] + q[0] + 1) >> 1) - (q[1] << 1)) >> 1).clamp(-tc0, tc0);
    }
    (filtered_p, filtered_q)
}

const fn average_qp(p: i32, q: i32) -> i32 {
    (p + q + 1) >> 1
}

fn chroma_qp(luma_qp: i32, offset: i32) -> i32 {
    const QP_TABLE: [i32; 22] = [
        29, 30, 31, 32, 32, 33, 34, 34, 35, 35, 36, 36, 37, 37, 37, 38, 38, 38, 39, 39, 39, 39,
    ];
    let index = (luma_qp + offset).clamp(0, 51);
    if index < 30 {
        index
    } else {
        QP_TABLE[usize::try_from(index - 30).expect("clamped chroma QP index")]
    }
}

fn clip_sample(value: i32) -> u8 {
    u8::try_from(value.clamp(0, 255)).expect("clamped H.264 sample fits u8")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strong_filter_smooths_an_intra_boundary() {
        let mut plane = [90, 91, 92, 93, 99, 100, 101, 102];
        filter_samples(
            &mut plane,
            [3, 2, 1, 0],
            [4, 5, 6, 7],
            40,
            4,
            false,
            Parameters {
                offset_a: 0,
                offset_b: 0,
            },
        );
        assert_eq!(plane, [90, 92, 94, 95, 97, 98, 100, 102]);
    }

    #[test]
    fn zero_threshold_leaves_an_edge_unchanged() {
        let mut plane = [0, 0, 0, 0, 255, 255, 255, 255];
        let original = plane;
        filter_samples(
            &mut plane,
            [3, 2, 1, 0],
            [4, 5, 6, 7],
            0,
            4,
            false,
            Parameters {
                offset_a: 0,
                offset_b: 0,
            },
        );
        assert_eq!(plane, original);
    }
}
