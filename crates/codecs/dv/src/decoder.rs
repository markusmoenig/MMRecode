use std::sync::OnceLock;

use mmrecode_core::{
    ColorDescription, ColorRange, Error, FieldOrder, FrameTiming, Plane, Result, VideoFrame,
};

use crate::{
    DvFrame, DvProfile, DvSystem,
    tables::{
        IWEIGHT_88, IWEIGHT_248, QUANT_OFFSET, QUANT_SHIFTS, VLC_LEN, VLC_LEVEL, VLC_RUN,
        ZIGZAG_248,
    },
};

const BLOCK_BITS: [usize; 6] = [112, 112, 112, 112, 80, 80];
const ZIGZAG_88: [usize; 64] = [
    0, 1, 8, 16, 9, 2, 3, 10, 17, 24, 32, 25, 18, 11, 4, 5, 12, 19, 26, 33, 40, 48, 41, 34, 27, 20,
    13, 6, 7, 14, 21, 28, 35, 42, 49, 56, 57, 50, 43, 36, 29, 22, 15, 23, 30, 37, 44, 51, 58, 59,
    52, 45, 38, 31, 39, 46, 53, 60, 61, 54, 47, 55, 62, 63,
];

#[derive(Clone, Copy)]
struct VlcEntry {
    code: u16,
    len: u8,
    run: u8,
    level: i16,
}

struct DctBlock {
    coefficients: [i32; 64],
    position: usize,
    dct_mode: bool,
    class: usize,
    quantization: usize,
    partial: Vec<u8>,
    complete: bool,
}

/// Damaged-video handling requested for DV reconstruction.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct DvVideoDecodeOptions {
    /// Replace undecodable five-macroblock segments with limited-range black.
    pub conceal_errors: bool,
}

/// One video segment replaced during damaged-frame reconstruction.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConcealedVideoSegment {
    /// DIF sequence number.
    pub sequence: usize,
    /// Video segment slot within the sequence.
    pub slot: usize,
    /// Original decoder diagnostic.
    pub reason: String,
}

/// Reconstructed pixels plus an explicit concealment report.
#[derive(Clone, Debug)]
pub struct DecodedDvVideo {
    /// Reconstructed video frame.
    pub frame: VideoFrame,
    /// Segments replaced with black because decoding failed.
    pub concealed_segments: Vec<ConcealedVideoSegment>,
}

/// Reconstructs the video picture in one parsed DV25 frame.
///
/// The decoder implements DV's three-level coefficient spill hierarchy:
/// unused bits are shared first within a macroblock and then within each
/// five-macroblock video segment.
///
/// # Errors
///
/// Returns an error for malformed coefficient syntax, missing EOB markers, or
/// invalid block placement. Structural DIF issues should be inspected before
/// decoding when damaged-media policy matters to the caller.
pub fn decode_video(frame: &DvFrame<'_>) -> Result<VideoFrame> {
    Ok(decode_video_with_options(frame, DvVideoDecodeOptions::default())?.frame)
}

/// Reconstructs a DV25 picture with an explicit damaged-segment policy.
///
/// # Errors
///
/// Returns an error on the first damaged segment unless concealment is enabled.
pub fn decode_video_with_options(
    frame: &DvFrame<'_>,
    options: DvVideoDecodeOptions,
) -> Result<DecodedDvVideo> {
    let profile = frame.profile();
    let (chroma_width, chroma_height) = match profile.system {
        DvSystem::System525_60 => (profile.width / 4, profile.height),
        DvSystem::System625_50 => (profile.width / 2, profile.height / 2),
    };
    let mut planes = vec![
        Plane {
            data: vec![16; profile.width * profile.height],
            stride: profile.width,
            width: profile.width,
            height: profile.height,
        },
        Plane {
            data: vec![128; chroma_width * chroma_height],
            stride: chroma_width,
            width: chroma_width,
            height: chroma_height,
        },
        Plane {
            data: vec![128; chroma_width * chroma_height],
            stride: chroma_width,
            width: chroma_width,
            height: chroma_height,
        },
    ];

    let mut concealed_segments = Vec::new();
    for sequence in 0..profile.dif_sequences {
        for slot in 0..27 {
            if let Err(error) = decode_segment(frame, &mut planes, sequence, slot) {
                if !options.conceal_errors {
                    return Err(error);
                }
                concealed_segments.push(ConcealedVideoSegment {
                    sequence,
                    slot,
                    reason: error.to_string(),
                });
            }
        }
    }

    Ok(DecodedDvVideo {
        frame: VideoFrame {
            format: profile.pixel_format,
            width: profile.width,
            height: profile.height,
            planes,
            timing: FrameTiming::default(),
            color: ColorDescription {
                range: ColorRange::Limited,
                primaries: Some("BT.601".into()),
                transfer: Some("BT.601".into()),
                matrix: Some("BT.601".into()),
            },
            field_order: FieldOrder::BottomFirst,
        },
        concealed_segments,
    })
}

fn decode_segment(
    frame: &DvFrame<'_>,
    planes: &mut [Plane],
    sequence: usize,
    slot: usize,
) -> Result<()> {
    let profile = frame.profile();
    let mut macroblocks = Vec::with_capacity(5);
    let mut segment_pool = Vec::new();
    for macroblock_index in 0..5 {
        let video_number = slot * 5 + macroblock_index;
        let group = video_number / 15;
        let within_group = video_number % 15;
        let block_index = sequence * 150 + 7 + group * 16 + within_group;
        if frame
            .issues()
            .iter()
            .any(|issue| issue.offset / 80 == block_index)
        {
            return Err(Error::InvalidData(format!(
                "DV video segment {slot} in sequence {sequence} has a damaged DIF identifier"
            )));
        }
        let bytes = frame.block_bytes(&frame.blocks()[block_index]);
        let (blocks, overflow) = decode_macroblock(bytes)?;
        segment_pool.extend(overflow);
        macroblocks.push(blocks);
    }
    consume_pool_for_macroblocks(&mut macroblocks, &segment_pool)?;
    if macroblocks.iter().flatten().any(|block| !block.complete) {
        return Err(Error::InvalidData(format!(
            "DV video segment {slot} in sequence {sequence} has incomplete coefficients"
        )));
    }
    let coordinates = macroblock_coordinates(profile, sequence, slot);
    for (blocks, (mb_x, mb_y)) in macroblocks.iter().zip(coordinates) {
        place_macroblock(profile, planes, blocks, mb_x, mb_y)?;
    }
    Ok(())
}

fn decode_macroblock(bytes: &[u8]) -> Result<(Vec<DctBlock>, Vec<u8>)> {
    let quantization = usize::from(bytes[3] & 0x0f);
    let payload = bytes_to_bits(&bytes[4..]);
    let mut cursor = 0;
    let mut blocks = Vec::with_capacity(6);
    let mut macroblock_pool = Vec::new();
    for bit_count in BLOCK_BITS {
        let region = &payload[cursor..cursor + bit_count];
        cursor += bit_count;
        let dc = signed_bits(&region[..9]);
        let dct_mode = region[9] != 0;
        let class = usize::try_from(bits_value(&region[10..12])).unwrap_or(0);
        let mut block = DctBlock {
            coefficients: [0; 64],
            position: 0,
            dct_mode,
            class,
            quantization,
            partial: Vec::new(),
            complete: false,
        };
        block.coefficients[0] = dc * 4 + 1_024;
        let leftover = consume_ac(&mut block, &region[12..])?;
        if block.complete {
            macroblock_pool.extend(leftover);
        }
        blocks.push(block);
    }
    let remaining = consume_pool(&mut blocks, &macroblock_pool)?;
    Ok((blocks, remaining))
}

fn consume_pool_for_macroblocks(blocks: &mut [Vec<DctBlock>], pool: &[u8]) -> Result<()> {
    let mut remaining = pool.to_vec();
    for block in blocks.iter_mut().flatten() {
        if !block.complete && !remaining.is_empty() {
            remaining = consume_ac(block, &remaining)?;
        }
    }
    Ok(())
}

fn consume_pool(blocks: &mut [DctBlock], pool: &[u8]) -> Result<Vec<u8>> {
    let mut remaining = pool.to_vec();
    for block in blocks {
        if !block.complete && !remaining.is_empty() {
            remaining = consume_ac(block, &remaining)?;
        }
    }
    Ok(remaining)
}

fn consume_ac(block: &mut DctBlock, input: &[u8]) -> Result<Vec<u8>> {
    if block.complete {
        return Ok(input.to_vec());
    }
    let mut bits = std::mem::take(&mut block.partial);
    bits.extend_from_slice(input);
    let mut cursor = 0;
    loop {
        let remaining = &bits[cursor..];
        match decode_vlc(remaining) {
            Some(entry) => {
                cursor += usize::from(entry.len);
                block.position += usize::from(entry.run) + 1;
                if block.position >= 64 {
                    block.complete = true;
                    return Ok(bits[cursor..].to_vec());
                }
                if entry.level != 0 {
                    let factor = inverse_factor(block, block.position);
                    let level = (i32::from(entry.level) * factor + 8_192) >> 14;
                    let scan = if block.dct_mode {
                        &ZIGZAG_248
                    } else {
                        &ZIGZAG_88
                    };
                    block.coefficients[scan[block.position]] = level;
                }
            }
            None if remaining.len() < 16 => {
                block.partial.extend_from_slice(remaining);
                return Ok(Vec::new());
            }
            None => {
                return Err(Error::InvalidData(
                    "invalid DV variable-length coefficient code".into(),
                ));
            }
        }
    }
}

fn inverse_factor(block: &DctBlock, position: usize) -> i32 {
    inverse_factor_for_encoding(block.quantization, block.class, block.dct_mode, position)
}

pub(crate) fn inverse_factor_for_encoding(
    quantization: usize,
    class: usize,
    dct_mode: bool,
    position: usize,
) -> i32 {
    let quantization = quantization + QUANT_OFFSET[class];
    let area = match position {
        0..=5 => 0,
        6..=20 => 1,
        21..=42 => 2,
        _ => 3,
    };
    let weight = if dct_mode {
        IWEIGHT_248[position]
    } else {
        IWEIGHT_88[position]
    };
    let class_shift = usize::from(class == 3);
    weight << (usize::from(QUANT_SHIFTS[quantization][area]) + 1 + class_shift)
}

fn decode_vlc(bits: &[u8]) -> Option<VlcEntry> {
    vlc_entries().iter().copied().find(|entry| {
        let len = usize::from(entry.len);
        bits.len() >= len && bits_value(&bits[..len]) == u32::from(entry.code)
    })
}

fn vlc_entries() -> &'static [VlcEntry] {
    static ENTRIES: OnceLock<Vec<VlcEntry>> = OnceLock::new();
    ENTRIES.get_or_init(|| {
        let mut entries = Vec::with_capacity(746);
        let mut canonical_code = 0_u32;
        for index in 0..VLC_LEN.len() {
            let len = VLC_LEN[index];
            let code = canonical_code >> (32 - len);
            canonical_code = canonical_code.wrapping_add(1_u32 << (32 - len));
            let level = i16::from(VLC_LEVEL[index]);
            if level == 0 {
                entries.push(VlcEntry {
                    code: u16::try_from(code).expect("15-bit DV VLC fits u16"),
                    len,
                    run: VLC_RUN[index],
                    level,
                });
            } else {
                entries.push(VlcEntry {
                    code: u16::try_from(code << 1).expect("16-bit DV VLC fits u16"),
                    len: len + 1,
                    run: VLC_RUN[index],
                    level,
                });
                entries.push(VlcEntry {
                    code: u16::try_from((code << 1) | 1).expect("16-bit DV VLC fits u16"),
                    len: len + 1,
                    run: VLC_RUN[index],
                    level: -level,
                });
            }
        }
        entries.sort_by_key(|entry| entry.len);
        entries
    })
}

pub(crate) fn vlc_for_encoding(run: usize, level: i32) -> Result<Vec<u8>> {
    let run = u8::try_from(run)
        .map_err(|_| Error::InvalidData("DV coefficient run exceeds 127".into()))?;
    let level = i16::try_from(level)
        .map_err(|_| Error::InvalidData("DV coefficient level exceeds i16".into()))?;
    if let Some(entry) = vlc_entries()
        .iter()
        .filter(|entry| entry.run == run && entry.level == level)
        .min_by_key(|entry| entry.len)
    {
        return Ok(vlc_bits(*entry));
    }
    if run == 0 || level == 0 {
        return Err(Error::InvalidData(format!(
            "DV has no VLC for run {run}, level {level}"
        )));
    }
    let run_entry = vlc_entries()
        .iter()
        .filter(|entry| entry.run == run - 1 && entry.level == 0)
        .min_by_key(|entry| entry.len)
        .ok_or_else(|| Error::InvalidData(format!("DV has no run extension for {run}")))?;
    let level_entry = vlc_entries()
        .iter()
        .filter(|entry| entry.run == 0 && entry.level == level)
        .min_by_key(|entry| entry.len)
        .ok_or_else(|| Error::InvalidData(format!("DV has no level code for {level}")))?;
    let mut bits = vlc_bits(*run_entry);
    bits.extend(vlc_bits(*level_entry));
    Ok(bits)
}

fn vlc_bits(entry: VlcEntry) -> Vec<u8> {
    (0..entry.len)
        .rev()
        .map(|shift| u8::from(u32::from(entry.code) & (1 << shift) != 0))
        .collect()
}

fn macroblock_coordinates(profile: DvProfile, sequence: usize, slot: usize) -> [(usize, usize); 5] {
    const OFF: [usize; 5] = [2, 6, 8, 0, 4];
    const SHUF3: [usize; 5] = [18, 9, 27, 0, 36];
    const L_START_SHUFFLED: [usize; 5] = [9, 4, 13, 0, 18];
    const SERPENT1: [usize; 27] = [
        0, 1, 2, 2, 1, 0, 0, 1, 2, 2, 1, 0, 0, 1, 2, 2, 1, 0, 0, 1, 2, 2, 1, 0, 0, 1, 2,
    ];
    const SERPENT2: [usize; 30] = [
        0, 1, 2, 3, 4, 5, 5, 4, 3, 2, 1, 0, 0, 1, 2, 3, 4, 5, 5, 4, 3, 2, 1, 0, 0, 1, 2, 3, 4, 5,
    ];
    std::array::from_fn(|macroblock| match profile.system {
        DvSystem::System625_50 => {
            let x = SHUF3[macroblock] + slot / 3;
            let y = SERPENT1[slot] + ((sequence + OFF[macroblock]) % profile.dif_sequences) * 3;
            (x * 2, y * 2)
        }
        DvSystem::System525_60 => {
            let row = (sequence + OFF[macroblock]) % profile.dif_sequences;
            let k = slot + usize::from(matches!(macroblock, 1 | 2)) * 3;
            let x = L_START_SHUFFLED[macroblock] + k / 6;
            let mut y = SERPENT2[k] + row * 6;
            if x > 21 {
                y = y * 2 - row * 6;
            }
            (x * 4, y)
        }
    })
}

pub(crate) fn macroblock_coordinates_for_encoding(
    profile: DvProfile,
    sequence: usize,
    slot: usize,
) -> [(usize, usize); 5] {
    macroblock_coordinates(profile, sequence, slot)
}

fn place_macroblock(
    profile: DvProfile,
    planes: &mut [Plane],
    blocks: &[DctBlock],
    mb_x: usize,
    mb_y: usize,
) -> Result<()> {
    let pixels: Vec<[u8; 64]> = blocks.iter().map(inverse_transform).collect();
    let x = mb_x * 8;
    let y = mb_y * 8;
    match profile.system {
        DvSystem::System625_50 => {
            put_block(&mut planes[0], x, y, &pixels[0], 8, 8)?;
            put_block(&mut planes[0], x + 8, y, &pixels[1], 8, 8)?;
            put_block(&mut planes[0], x, y + 8, &pixels[2], 8, 8)?;
            put_block(&mut planes[0], x + 8, y + 8, &pixels[3], 8, 8)?;
            put_block(&mut planes[2], x / 2, y / 2, &pixels[4], 8, 8)?;
            put_block(&mut planes[1], x / 2, y / 2, &pixels[5], 8, 8)?;
        }
        DvSystem::System525_60 if mb_x < 88 => {
            for (index, pixels) in pixels[..4].iter().enumerate() {
                put_block(&mut planes[0], x + index * 8, y, pixels, 8, 8)?;
            }
            put_block(&mut planes[2], x / 4, y, &pixels[4], 8, 8)?;
            put_block(&mut planes[1], x / 4, y, &pixels[5], 8, 8)?;
        }
        DvSystem::System525_60 => {
            put_block(&mut planes[0], x, y, &pixels[0], 8, 8)?;
            put_block(&mut planes[0], x + 8, y, &pixels[1], 8, 8)?;
            put_block(&mut planes[0], x, y + 8, &pixels[2], 8, 8)?;
            put_block(&mut planes[0], x + 8, y + 8, &pixels[3], 8, 8)?;
            put_split_chroma(&mut planes[2], x / 4, y, &pixels[4])?;
            put_split_chroma(&mut planes[1], x / 4, y, &pixels[5])?;
        }
    }
    Ok(())
}

fn put_block(
    plane: &mut Plane,
    x: usize,
    y: usize,
    pixels: &[u8; 64],
    width: usize,
    height: usize,
) -> Result<()> {
    if x + width > plane.width || y + height > plane.height {
        return Err(Error::InvalidData(format!(
            "DV macroblock placement ({x}, {y}) exceeds {}x{} plane",
            plane.width, plane.height
        )));
    }
    for row in 0..height {
        let destination = (y + row) * plane.stride + x;
        plane.data[destination..destination + width]
            .copy_from_slice(&pixels[row * 8..row * 8 + width]);
    }
    Ok(())
}

fn put_split_chroma(plane: &mut Plane, x: usize, y: usize, pixels: &[u8; 64]) -> Result<()> {
    for row in 0..8 {
        let top = (y + row) * plane.stride + x;
        let bottom = (y + 8 + row) * plane.stride + x;
        if bottom + 4 > plane.data.len() {
            return Err(Error::InvalidData(
                "split DV chroma placement exceeds plane".into(),
            ));
        }
        plane.data[top..top + 4].copy_from_slice(&pixels[row * 8..row * 8 + 4]);
        plane.data[bottom..bottom + 4].copy_from_slice(&pixels[row * 8 + 4..row * 8 + 8]);
    }
    Ok(())
}

fn inverse_transform(block: &DctBlock) -> [u8; 64] {
    if block.dct_mode {
        inverse_248(&block.coefficients)
    } else {
        inverse_88(&block.coefficients)
    }
}

#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss
)]
fn inverse_88(coefficients: &[i32; 64]) -> [u8; 64] {
    let mut output = [0_u8; 64];
    for y in 0..8 {
        for x in 0..8 {
            let mut sum = 0.0;
            for v in 0..8 {
                for u in 0..8 {
                    let cu = if u == 0 {
                        std::f64::consts::FRAC_1_SQRT_2
                    } else {
                        1.0
                    };
                    let cv = if v == 0 {
                        std::f64::consts::FRAC_1_SQRT_2
                    } else {
                        1.0
                    };
                    sum += cu
                        * cv
                        * f64::from(coefficients[v * 8 + u])
                        * ((2 * x + 1) as f64 * u as f64 * std::f64::consts::PI / 16.0).cos()
                        * ((2 * y + 1) as f64 * v as f64 * std::f64::consts::PI / 16.0).cos();
                }
            }
            output[y * 8 + x] = (sum / 4.0).round().clamp(0.0, 255.0) as u8;
        }
    }
    output
}

#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss
)]
fn inverse_248(coefficients: &[i32; 64]) -> [u8; 64] {
    let mut butterfly = [0.0_f64; 64];
    for pair in 0..4 {
        for u in 0..8 {
            let first = f64::from(coefficients[pair * 16 + u]);
            let second = f64::from(coefficients[pair * 16 + 8 + u]);
            butterfly[pair * 16 + u] = first + second;
            butterfly[pair * 16 + 8 + u] = first - second;
        }
    }
    let mut horizontal = [0.0_f64; 64];
    for row in 0..8 {
        for x in 0..8 {
            let mut sum = 0.0;
            for u in 0..8 {
                let cu = if u == 0 {
                    std::f64::consts::FRAC_1_SQRT_2
                } else {
                    1.0
                };
                sum += cu
                    * butterfly[row * 8 + u]
                    * ((2 * x + 1) as f64 * u as f64 * std::f64::consts::PI / 16.0).cos();
            }
            horizontal[row * 8 + x] = sum / 2.0;
        }
    }
    let mut output = [0_u8; 64];
    for field in 0..2 {
        for y in 0..4 {
            for x in 0..8 {
                let mut sum = 0.0;
                for v in 0..4 {
                    let cv = if v == 0 {
                        std::f64::consts::FRAC_1_SQRT_2
                    } else {
                        1.0
                    };
                    sum += cv
                        * horizontal[(v * 2 + field) * 8 + x]
                        * ((2 * y + 1) as f64 * v as f64 * std::f64::consts::PI / 8.0).cos();
                }
                output[(y * 2 + field) * 8 + x] = (sum / 2.0).round().clamp(0.0, 255.0) as u8;
            }
        }
    }
    output
}

fn bytes_to_bits(bytes: &[u8]) -> Vec<u8> {
    let mut bits = Vec::with_capacity(bytes.len() * 8);
    for &byte in bytes {
        for shift in (0..8).rev() {
            bits.push((byte >> shift) & 1);
        }
    }
    bits
}

fn bits_value(bits: &[u8]) -> u32 {
    bits.iter()
        .fold(0_u32, |value, &bit| value << 1 | u32::from(bit))
}

fn signed_bits(bits: &[u8]) -> i32 {
    let value = i32::try_from(bits_value(bits)).unwrap_or(0);
    if bits.first() == Some(&1) {
        value - (1_i32 << bits.len())
    } else {
        value
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_vlc_table_has_signed_entries_and_eob() {
        let entries = vlc_entries();
        assert!(entries.iter().any(|entry| entry.level < 0));
        assert!(entries.iter().any(|entry| entry.run == 127));
    }

    #[test]
    fn inverse_dc_block_is_constant() {
        let mut coefficients = [0; 64];
        coefficients[0] = 1_024;
        assert_eq!(inverse_88(&coefficients), [128; 64]);
        assert_eq!(inverse_248(&coefficients), [128; 64]);
    }

    #[test]
    fn coordinates_cover_expected_macroblock_count() {
        for profile in [DvProfile::DV25_525_60, DvProfile::DV25_625_50] {
            let mut coordinates = std::collections::HashSet::new();
            for sequence in 0..profile.dif_sequences {
                for slot in 0..27 {
                    coordinates.extend(macroblock_coordinates(profile, sequence, slot));
                }
            }
            assert_eq!(coordinates.len(), profile.dif_sequences * 27 * 5);
        }
    }
}
