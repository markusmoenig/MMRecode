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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Parameters {
    pub(crate) offset_a: i32,
    pub(crate) offset_b: i32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct MacroblockParameters {
    pub(crate) parameters: Option<Parameters>,
    pub(crate) slice_id: usize,
    pub(crate) filter_across_slice_boundaries: bool,
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
    pub(crate) transform_8x8: &'a [bool],
    pub(crate) luma_nonzero: &'a [[u8; 16]],
    pub(crate) motion: &'a [[BlockMotion; 16]],
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct MotionInfo {
    pub(crate) x: i32,
    pub(crate) y: i32,
    pub(crate) reference_index: Option<u8>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct ReferenceMotion {
    pub(crate) x: i32,
    pub(crate) y: i32,
    pub(crate) index: Option<u8>,
    pub(crate) reference: Option<i32>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct BlockMotion {
    pub(crate) list0: ReferenceMotion,
    pub(crate) list1: ReferenceMotion,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct BoundaryStrengths {
    vertical: [[usize; 4]; 4],
    horizontal: [[usize; 4]; 4],
}

pub(crate) fn filter_picture(
    picture: &mut Picture<'_>,
    macroblock_parameters: &[MacroblockParameters],
) -> Result<()> {
    let macroblocks_wide = picture.coded_width / 16;
    let macroblocks_high = picture.coded_height / 16;
    let expected_qps = macroblocks_wide
        .checked_mul(macroblocks_high)
        .ok_or_else(|| Error::InvalidData("H.264 deblocking macroblock count overflows".into()))?;
    if picture.luma_qp.len() != expected_qps
        || picture.macroblock_intra.len() != expected_qps
        || picture.transform_8x8.len() != expected_qps
        || picture.luma_nonzero.len() != expected_qps
        || picture.motion.len() != expected_qps
        || macroblock_parameters.len() != expected_qps
    {
        return Err(Error::InvalidData(
            "H.264 deblocking QP map does not match the picture".into(),
        ));
    }
    for address in 0..expected_qps {
        let Some(params) = macroblock_parameters[address].parameters else {
            continue;
        };
        let strengths =
            derive_boundary_strengths(picture, address, macroblocks_wide, macroblock_parameters);
        filter_luma_macroblock(
            picture,
            address,
            macroblocks_wide,
            macroblock_parameters,
            params,
            &strengths,
        );
        filter_chroma_macroblock(
            picture.cb,
            picture.coded_width / 2,
            address,
            macroblocks_wide,
            picture.luma_qp,
            picture.chroma_qp_offset_cb,
            macroblock_parameters,
            params,
            &strengths,
        );
        filter_chroma_macroblock(
            picture.cr,
            picture.coded_width / 2,
            address,
            macroblocks_wide,
            picture.luma_qp,
            picture.chroma_qp_offset_cr,
            macroblock_parameters,
            params,
            &strengths,
        );
    }
    Ok(())
}

fn derive_boundary_strengths(
    picture: &Picture<'_>,
    address: usize,
    macroblocks_wide: usize,
    macroblock_parameters: &[MacroblockParameters],
) -> BoundaryStrengths {
    let macroblock_x = address % macroblocks_wide;
    let macroblock_y = address / macroblocks_wide;
    let mut strengths = BoundaryStrengths::default();
    for edge in 0..4 {
        if edge != 0
            || (macroblock_x != 0
                && external_edge_enabled(macroblock_parameters, address - 1, address))
        {
            let p_address = if edge == 0 { address - 1 } else { address };
            for segment in 0..4 {
                strengths.vertical[edge][segment] = boundary_strength(
                    picture.macroblock_intra,
                    picture.luma_nonzero,
                    picture.motion,
                    p_address,
                    luma_block_index(if edge == 0 { 3 } else { edge - 1 }, segment),
                    address,
                    luma_block_index(if edge == 0 { 0 } else { edge }, segment),
                    edge == 0,
                );
            }
        }
        if edge != 0
            || (macroblock_y != 0
                && external_edge_enabled(
                    macroblock_parameters,
                    address - macroblocks_wide,
                    address,
                ))
        {
            let p_address = if edge == 0 {
                address - macroblocks_wide
            } else {
                address
            };
            for segment in 0..4 {
                strengths.horizontal[edge][segment] = boundary_strength(
                    picture.macroblock_intra,
                    picture.luma_nonzero,
                    picture.motion,
                    p_address,
                    luma_block_index(segment, if edge == 0 { 3 } else { edge - 1 }),
                    address,
                    luma_block_index(segment, if edge == 0 { 0 } else { edge }),
                    edge == 0,
                );
            }
        }
    }
    strengths
}

fn filter_luma_macroblock(
    picture: &mut Picture<'_>,
    address: usize,
    macroblocks_wide: usize,
    macroblock_parameters: &[MacroblockParameters],
    params: Parameters,
    strengths: &BoundaryStrengths,
) {
    let macroblock_x = address % macroblocks_wide;
    let macroblock_y = address / macroblocks_wide;
    let origin_x = macroblock_x * 16;
    let origin_y = macroblock_y * 16;
    for edge in 0..4 {
        if edge == 0
            && (macroblock_x == 0
                || !external_edge_enabled(macroblock_parameters, address - 1, address))
        {
            continue;
        }
        if picture.transform_8x8[address] && matches!(edge, 1 | 3) {
            continue;
        }
        let p_address = if edge == 0 { address - 1 } else { address };
        let qp = average_qp(picture.luma_qp[p_address], picture.luma_qp[address]);
        for segment in 0..4 {
            filter_vertical_edge(
                picture.luma,
                picture.coded_width,
                origin_x + edge * 4,
                origin_y + segment * 4,
                4,
                qp,
                strengths.vertical[edge][segment],
                false,
                params,
            );
        }
    }
    for edge in 0..4 {
        if edge == 0
            && (macroblock_y == 0
                || !external_edge_enabled(
                    macroblock_parameters,
                    address - macroblocks_wide,
                    address,
                ))
        {
            continue;
        }
        if picture.transform_8x8[address] && matches!(edge, 1 | 3) {
            continue;
        }
        let p_address = if edge == 0 {
            address - macroblocks_wide
        } else {
            address
        };
        let qp = average_qp(picture.luma_qp[p_address], picture.luma_qp[address]);
        for segment in 0..4 {
            filter_horizontal_edge(
                picture.luma,
                picture.coded_width,
                origin_x + segment * 4,
                origin_y + edge * 4,
                4,
                qp,
                strengths.horizontal[edge][segment],
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
    macroblock_parameters: &[MacroblockParameters],
    params: Parameters,
    strengths: &BoundaryStrengths,
) {
    let macroblock_x = address % macroblocks_wide;
    let macroblock_y = address / macroblocks_wide;
    let origin_x = macroblock_x * 8;
    let origin_y = macroblock_y * 8;
    for edge in 0..2 {
        if edge == 0
            && (macroblock_x == 0
                || !external_edge_enabled(macroblock_parameters, address - 1, address))
        {
            continue;
        }
        let p_address = if edge == 0 { address - 1 } else { address };
        let qp = average_qp(
            chroma_qp(luma_qp[p_address], chroma_qp_offset),
            chroma_qp(luma_qp[address], chroma_qp_offset),
        );
        for segment in 0..4 {
            filter_vertical_edge(
                plane,
                stride,
                origin_x + edge * 4,
                origin_y + segment * 2,
                2,
                qp,
                strengths.vertical[edge * 2][segment],
                true,
                params,
            );
        }
    }
    for edge in 0..2 {
        if edge == 0
            && (macroblock_y == 0
                || !external_edge_enabled(
                    macroblock_parameters,
                    address - macroblocks_wide,
                    address,
                ))
        {
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
            filter_horizontal_edge(
                plane,
                stride,
                origin_x + segment * 2,
                origin_y + edge * 4,
                2,
                qp,
                strengths.horizontal[edge * 2][segment],
                true,
                params,
            );
        }
    }
}

fn external_edge_enabled(
    macroblock_parameters: &[MacroblockParameters],
    p_address: usize,
    q_address: usize,
) -> bool {
    let q = macroblock_parameters[q_address];
    q.filter_across_slice_boundaries || macroblock_parameters[p_address].slice_id == q.slice_id
}

#[allow(clippy::too_many_arguments)]
fn boundary_strength(
    macroblock_intra: &[bool],
    luma_nonzero: &[[u8; 16]],
    motion: &[[BlockMotion; 16]],
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
    usize::from(!motion_pairs_match(
        motion[p_address][p_block],
        motion[q_address][q_block],
    ))
}

fn motion_pairs_match(left: BlockMotion, right: BlockMotion) -> bool {
    if left.list1.reference.is_none() && right.list1.reference.is_none() {
        return indexed_motion_matches(left.list0, right.list0);
    }
    if left.list0.reference.is_none() && right.list0.reference.is_none() {
        return indexed_motion_matches(left.list1, right.list1);
    }
    let same_order =
        motion_matches(left.list0, right.list0) && motion_matches(left.list1, right.list1);
    let swapped_order =
        motion_matches(left.list0, right.list1) && motion_matches(left.list1, right.list0);
    same_order || swapped_order
}

fn indexed_motion_matches(left: ReferenceMotion, right: ReferenceMotion) -> bool {
    left.index == right.index && vector_matches(left, right)
}

fn motion_matches(left: ReferenceMotion, right: ReferenceMotion) -> bool {
    left.reference == right.reference && vector_matches(left, right)
}

fn vector_matches(left: ReferenceMotion, right: ReferenceMotion) -> bool {
    (left.x - right.x).abs() < 4 && (left.y - right.y).abs() < 4
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
    if boundary_strength == 0 {
        return;
    }
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
    if boundary_strength == 0 {
        return;
    }
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
    if (p[0] - q[0]).abs() >= alpha || (p[1] - p[0]).abs() >= beta || (q[1] - q[0]).abs() >= beta {
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

    #[test]
    fn eight_by_eight_transform_skips_internal_four_sample_edges() {
        let original = (0..16)
            .flat_map(|_| (0..16).map(|x| if x < 4 { 100 } else { 110 }))
            .collect::<Vec<_>>();
        let luma_qp = [40];
        let macroblock_intra = [true];
        let luma_nonzero = [[0; 16]];
        let motion = [[BlockMotion::default(); 16]];
        let params = Parameters {
            offset_a: 0,
            offset_b: 0,
        };
        let macroblock_parameters = [MacroblockParameters {
            parameters: Some(params),
            slice_id: 0,
            filter_across_slice_boundaries: true,
        }];

        let mut transformed = original.clone();
        let mut transformed_blue = vec![128; 64];
        let mut transformed_red = vec![128; 64];
        filter_picture(
            &mut Picture {
                luma: &mut transformed,
                cb: &mut transformed_blue,
                cr: &mut transformed_red,
                coded_width: 16,
                coded_height: 16,
                luma_qp: &luma_qp,
                chroma_qp_offset_cb: 0,
                chroma_qp_offset_cr: 0,
                macroblock_intra: &macroblock_intra,
                transform_8x8: &[true],
                luma_nonzero: &luma_nonzero,
                motion: &motion,
            },
            &macroblock_parameters,
        )
        .unwrap();
        assert_eq!(transformed, original);

        let mut four_by_four = original.clone();
        let mut four_by_four_blue = vec![128; 64];
        let mut four_by_four_red = vec![128; 64];
        filter_picture(
            &mut Picture {
                luma: &mut four_by_four,
                cb: &mut four_by_four_blue,
                cr: &mut four_by_four_red,
                coded_width: 16,
                coded_height: 16,
                luma_qp: &luma_qp,
                chroma_qp_offset_cb: 0,
                chroma_qp_offset_cr: 0,
                macroblock_intra: &macroblock_intra,
                transform_8x8: &[false],
                luma_nonzero: &luma_nonzero,
                motion: &motion,
            },
            &macroblock_parameters,
        )
        .unwrap();
        assert_ne!(four_by_four, original);
    }

    #[test]
    fn bidirectional_motion_accepts_swapped_reference_lists() {
        let motion = |reference, x| ReferenceMotion {
            x,
            y: 0,
            index: None,
            reference: Some(reference),
        };
        let left = BlockMotion {
            list0: motion(10, 8),
            list1: motion(20, -4),
        };
        let swapped = BlockMotion {
            list0: motion(20, -4),
            list1: motion(10, 8),
        };
        assert!(motion_pairs_match(left, swapped));
        let changed = BlockMotion {
            list0: motion(20, 0),
            ..swapped
        };
        assert!(!motion_pairs_match(left, changed));
    }

    #[test]
    fn slice_boundary_filtering_obeys_disable_idc_two() {
        fn filtered_luma(filter_across_slice_boundaries: bool) -> Vec<u8> {
            let mut luma = (0..16)
                .flat_map(|_| (0..32).map(|x| if x < 16 { 100 } else { 110 }))
                .collect::<Vec<_>>();
            let mut cb = vec![128; 16 * 8];
            let mut cr = vec![128; 16 * 8];
            let luma_qp = [40; 2];
            let macroblock_intra = [true; 2];
            let transform_8x8 = [false; 2];
            let luma_nonzero = [[0; 16]; 2];
            let motion = [[BlockMotion::default(); 16]; 2];
            let parameters = Some(Parameters {
                offset_a: 0,
                offset_b: 0,
            });
            let macroblock_parameters = [
                MacroblockParameters {
                    parameters,
                    slice_id: 0,
                    filter_across_slice_boundaries: true,
                },
                MacroblockParameters {
                    parameters,
                    slice_id: 1,
                    filter_across_slice_boundaries,
                },
            ];
            filter_picture(
                &mut Picture {
                    luma: &mut luma,
                    cb: &mut cb,
                    cr: &mut cr,
                    coded_width: 32,
                    coded_height: 16,
                    luma_qp: &luma_qp,
                    chroma_qp_offset_cb: 0,
                    chroma_qp_offset_cr: 0,
                    macroblock_intra: &macroblock_intra,
                    transform_8x8: &transform_8x8,
                    luma_nonzero: &luma_nonzero,
                    motion: &motion,
                },
                &macroblock_parameters,
            )
            .unwrap();
            luma
        }

        let original = (0..16)
            .flat_map(|_| (0..32).map(|x| if x < 16 { 100 } else { 110 }))
            .collect::<Vec<_>>();
        assert_eq!(filtered_luma(false), original);
        assert_ne!(filtered_luma(true), original);
    }
}
