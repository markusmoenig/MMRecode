use mmrecode_core::{AudioFrame, AudioSampleFormat, Error, PixelFormat, Result, VideoFrame};

use crate::{
    DvProfile, DvSystem, Timecode,
    audio::shuffle,
    decode_video,
    decoder::{inverse_factor_for_encoding, macroblock_coordinates_for_encoding, vlc_for_encoding},
    parse_frame,
};

const BLOCK_BITS: [usize; 6] = [112, 112, 112, 112, 80, 80];
const ZIGZAG: [usize; 64] = [
    0, 1, 8, 16, 9, 2, 3, 10, 17, 24, 32, 25, 18, 11, 4, 5, 12, 19, 26, 33, 40, 48, 41, 34, 27, 20,
    13, 6, 7, 14, 21, 28, 35, 42, 49, 56, 57, 50, 43, 36, 29, 22, 15, 23, 30, 37, 44, 51, 58, 59,
    52, 45, 38, 31, 39, 46, 53, 60, 61, 54, 47, 55, 62, 63,
];

/// Encoded raw DV bytes and the encoder's decoded reconstruction.
#[derive(Clone, Debug)]
pub struct EncodedDv {
    /// One complete raw DV25 frame.
    pub data: Vec<u8>,
    /// Reconstruction obtained through the public decoder path.
    pub reconstructed: VideoFrame,
}

/// Optional media embedded while encoding one DV frame.
#[derive(Clone, Copy, Debug, Default)]
pub struct DvEncodeOptions<'a> {
    /// One audio frame aligned with the video frame.
    pub audio: Option<&'a AudioFrame>,
    /// SMPTE timecode written into the subcode packs.
    pub timecode: Option<Timecode>,
}

struct EncodedCell {
    dc: i32,
    class: usize,
    ac: Vec<u8>,
}

struct PackedCell {
    dc: i32,
    class: usize,
    storage: Vec<u8>,
    used: usize,
    ac: Vec<u8>,
    ac_offset: usize,
}

impl PackedCell {
    fn complete(&self) -> bool {
        self.ac_offset == self.ac.len()
    }
}

/// Encodes one uncompressed frame as a deterministic raw DV25 frame.
///
/// The profile is selected from the exact DV-native dimensions and pixel
/// format. The current reference encoder uses frame-DCT mode and searches the
/// finest quantization number that fits each five-macroblock video segment.
///
/// # Errors
///
/// Returns an error for a non-DV25 frame layout, malformed plane storage, or a
/// segment that cannot fit even at the coarsest standardized quantization.
pub fn encode_video(frame: &VideoFrame) -> Result<EncodedDv> {
    encode_frame(frame, DvEncodeOptions::default())
}

/// Encodes one DV frame with optional embedded audio and timecode.
///
/// # Errors
///
/// Returns an error when the video, audio, or timecode cannot be represented
/// by a supported DV25 profile.
pub fn encode_frame(frame: &VideoFrame, options: DvEncodeOptions<'_>) -> Result<EncodedDv> {
    let profile = profile_for_frame(frame)?;
    validate_frame(frame, profile)?;
    let mut data = initialize_dif_frame(profile);
    for sequence in 0..profile.dif_sequences {
        for slot in 0..27 {
            encode_segment(frame, profile, sequence, slot, &mut data)?;
        }
    }
    let parsed = parse_frame(&data)?;
    let reconstructed = decode_video(&parsed)?;
    if let Some(audio) = options.audio {
        embed_audio(profile, audio, &mut data)?;
    }
    if let Some(timecode) = options.timecode {
        embed_timecode(profile, timecode, &mut data)?;
    }
    Ok(EncodedDv {
        data,
        reconstructed,
    })
}

/// Encodes video together with one 16-bit stereo DV audio frame.
///
/// Audio samples are placed with the profile-specific DIF shuffle and an AAUX
/// source pack. The sample count must be within the standardized per-frame
/// range for its rate.
///
/// # Errors
///
/// Returns an error when video encoding fails or the audio rate, channel
/// layout, storage, or per-frame sample count cannot be represented by DV25.
pub fn encode_video_with_audio(frame: &VideoFrame, audio: &AudioFrame) -> Result<EncodedDv> {
    encode_frame(
        frame,
        DvEncodeOptions {
            audio: Some(audio),
            timecode: None,
        },
    )
}

fn embed_timecode(profile: DvProfile, timecode: Timecode, data: &mut [u8]) -> Result<()> {
    let frame_limit = match profile.system {
        DvSystem::System525_60 => 30,
        DvSystem::System625_50 => 25,
    };
    if timecode.hours >= 24
        || timecode.minutes >= 60
        || timecode.seconds >= 60
        || timecode.frames >= frame_limit
        || (timecode.drop_frame && profile.system == DvSystem::System625_50)
    {
        return Err(Error::InvalidData(format!(
            "timecode {timecode:?} is invalid for {:?}",
            profile.system
        )));
    }
    let pack = [
        0x13,
        bcd(timecode.frames) | u8::from(timecode.drop_frame) << 6,
        bcd(timecode.seconds) | 0x80,
        bcd(timecode.minutes) | 0x80,
        bcd(timecode.hours) | 0xc0,
    ];
    for sequence in 0..profile.dif_sequences {
        for block in 0..2 {
            let base = (sequence * 150 + 1 + block) * 80;
            for sync in 0..6 {
                let offset = base + 6 + sync * 8;
                data[offset..offset + 5].copy_from_slice(&pack);
            }
        }
    }
    Ok(())
}

const fn bcd(value: u8) -> u8 {
    ((value / 10) << 4) | (value % 10)
}

fn embed_audio(profile: DvProfile, audio: &AudioFrame, data: &mut [u8]) -> Result<()> {
    audio.validate()?;
    if audio.format != AudioSampleFormat::I16Interleaved || audio.channels != 2 {
        return Err(Error::Unsupported(
            "DV audio encoding currently requires interleaved 16-bit stereo".into(),
        ));
    }
    let rate_index = match audio.sample_rate {
        48_000 => 0,
        44_100 => 1,
        32_000 => 2,
        rate => return Err(Error::Unsupported(format!("DV audio sample rate {rate}"))),
    };
    let minimum = profile.audio_min_samples[rate_index];
    let delta = audio
        .samples_per_channel
        .checked_sub(minimum)
        .filter(|&value| value <= 63)
        .ok_or_else(|| {
            Error::InvalidData(format!(
                "DV audio frame has {} samples/channel; profile range begins at {minimum}",
                audio.samples_per_channel
            ))
        })?;

    for sequence in 0..profile.dif_sequences {
        for audio_block in 0..9 {
            let offset = (sequence * 150 + 6 + audio_block * 16) * 80;
            for byte in (8..80).step_by(2) {
                data[offset + byte] = 0x80;
                data[offset + byte + 1] = 0x00;
            }
        }
        let source_block = if sequence.is_multiple_of(2) { 3 } else { 0 };
        let offset = (sequence * 150 + 6 + source_block * 16) * 80 + 3;
        data[offset..offset + 5].copy_from_slice(&[
            0x50,
            0xc0 | u8::try_from(delta).unwrap_or(0),
            0x00,
            match profile.system {
                DvSystem::System525_60 => 0xc0,
                DvSystem::System625_50 => 0xe0,
            },
            0x80 | u8::try_from(rate_index * 8).unwrap_or(0),
        ]);
    }

    for sequence in 0..profile.dif_sequences {
        for audio_block in 0..9 {
            let offset = (sequence * 150 + 6 + audio_block * 16) * 80;
            for byte in (8..80).step_by(2) {
                let sample_index =
                    shuffle(profile, sequence, audio_block) + (byte - 8) / 2 * profile.audio_stride;
                if sample_index >= audio.samples.len() {
                    continue;
                }
                let encoded = audio.samples[sample_index].to_be_bytes();
                data[offset + byte] = encoded[0];
                data[offset + byte + 1] = encoded[1];
            }
        }
    }
    Ok(())
}

fn profile_for_frame(frame: &VideoFrame) -> Result<DvProfile> {
    match (frame.width, frame.height, frame.format) {
        (720, 480, PixelFormat::Yuv411p8) => Ok(DvProfile::DV25_525_60),
        (720, 576, PixelFormat::Yuv420p8) => Ok(DvProfile::DV25_625_50),
        _ => Err(Error::Unsupported(format!(
            "DV25 encoding requires 720x480 Yuv411p8 or 720x576 Yuv420p8, found {}x{} {:?}",
            frame.width, frame.height, frame.format
        ))),
    }
}

fn validate_frame(frame: &VideoFrame, profile: DvProfile) -> Result<()> {
    let chroma = match profile.system {
        DvSystem::System525_60 => (180, 480),
        DvSystem::System625_50 => (360, 288),
    };
    let expected = [(profile.width, profile.height), chroma, chroma];
    if frame.planes.len() != 3 {
        return Err(Error::InvalidData(
            "DV25 input requires three planes".into(),
        ));
    }
    for (index, (plane, &(width, height))) in frame.planes.iter().zip(&expected).enumerate() {
        let required = plane
            .stride
            .checked_mul(height)
            .ok_or_else(|| Error::InvalidData("DV plane layout overflow".into()))?;
        if plane.width != width
            || plane.height != height
            || plane.stride < width
            || plane.data.len() < required
        {
            return Err(Error::InvalidData(format!(
                "DV input plane {index} has an invalid layout"
            )));
        }
    }
    Ok(())
}

fn initialize_dif_frame(profile: DvProfile) -> Vec<u8> {
    let mut data = vec![0xff; profile.frame_size];
    for sequence in 0..profile.dif_sequences {
        for position in 0..150 {
            let offset = (sequence * 150 + position) * 80;
            let (section, number) = match position {
                0 => (0x1f, 0),
                1..=2 => (0x3f, position - 1),
                3..=5 => (0x56, position - 3),
                _ if (position - 6).is_multiple_of(16) => (0x76, (position - 6) / 16),
                _ => (0x96, (position - 7) - (position - 6) / 16),
            };
            data[offset] = section;
            data[offset + 1] = u8::try_from(sequence).unwrap_or(0) << 4 | 0x07;
            data[offset + 2] = u8::try_from(number).unwrap_or(0);
        }
        initialize_header(profile, sequence, &mut data);
        initialize_subcode(sequence, &mut data);
        initialize_vaux(profile, sequence, &mut data);
    }
    data
}

fn initialize_header(profile: DvProfile, sequence: usize, data: &mut [u8]) {
    let offset = sequence * 150 * 80;
    let (dsf, application) = match profile.system {
        DvSystem::System525_60 => (0x3f, 0xf9),
        DvSystem::System625_50 => (0xbf, 0xf8),
    };
    data[offset + 3] = dsf;
    data[offset + 4] = application;
    data[offset + 5..offset + 8].fill(application & 0x7f);
}

fn initialize_subcode(sequence: usize, data: &mut [u8]) {
    for block in 0..2 {
        let base = (sequence * 150 + 1 + block) * 80;
        for sync in 0..6 {
            let offset = base + 3 + sync * 8;
            data[offset] = 0x8f;
            data[offset + 1] = 0xf0 | u8::try_from(sync).unwrap_or(0);
            data[offset + 2] = 0xff;
        }
    }
}

fn initialize_vaux(profile: DvProfile, sequence: usize, data: &mut [u8]) {
    let video_source = match profile.system {
        DvSystem::System525_60 => [0x60, 0xff, 0xff, 0xc0, 0xff],
        DvSystem::System625_50 => [0x60, 0xff, 0xff, 0xe0, 0xff],
    };
    let packs = [
        video_source,
        [0x61, 0x3f, 0xc8, 0xfc, 0xff],
        [0x62, 0xff, 0xc1, 0x01, 0x70],
        [0x63, 0xff, 0x80, 0x80, 0xc0],
    ];
    for block in 0..3 {
        let base = (sequence * 150 + 3 + block) * 80;
        for start in [3, 48] {
            for (index, pack) in packs.iter().enumerate() {
                let offset = base + start + index * 5;
                data[offset..offset + 5].copy_from_slice(pack);
            }
        }
    }
}

fn encode_segment(
    frame: &VideoFrame,
    profile: DvProfile,
    sequence: usize,
    slot: usize,
    output: &mut [u8],
) -> Result<()> {
    let coordinates = macroblock_coordinates_for_encoding(profile, sequence, slot);
    let source: Vec<[i32; 64]> = coordinates
        .into_iter()
        .flat_map(|(x, y)| gather_macroblock(frame, profile, x, y))
        .map(|pixels| forward_dct(&pixels))
        .collect();

    let mut selected = None;
    for quantization in (0..=15).rev() {
        let cells: Vec<EncodedCell> = source
            .iter()
            .map(|block| quantize_block(block, quantization))
            .collect::<Result<_>>()?;
        let total_bits: usize = cells.iter().map(|cell| cell.ac.len()).sum();
        if total_bits <= 2_680 {
            selected = Some((quantization, cells));
            break;
        }
    }
    let (quantization, cells) = selected.ok_or_else(|| {
        Error::InvalidData(format!(
            "DV segment {slot} in sequence {sequence} cannot fit its coefficient budget"
        ))
    })?;
    let packed = pack_segment(&cells)?;
    for macroblock in 0..5 {
        let video_number = slot * 5 + macroblock;
        let group = video_number / 15;
        let within_group = video_number % 15;
        let block_index = sequence * 150 + 7 + group * 16 + within_group;
        let offset = block_index * 80;
        output[offset + 3] = u8::try_from(quantization).unwrap_or(0);
        let mut payload = Vec::with_capacity(608);
        for cell in &packed[macroblock * 6..macroblock * 6 + 6] {
            payload.extend(signed_to_bits(cell.dc, 9));
            payload.push(0);
            payload.extend(unsigned_to_bits(u32::try_from(cell.class).unwrap_or(0), 2));
            payload.extend_from_slice(&cell.storage);
        }
        output[offset + 4..offset + 80].copy_from_slice(&bits_to_bytes(&payload));
    }
    Ok(())
}

fn quantize_block(coefficients: &[i32; 64], quantization: usize) -> Result<EncodedCell> {
    let mut class = 2;
    let mut levels = quantized_levels(coefficients, quantization, class);
    if levels.iter().any(|&level| level.unsigned_abs() > 255) {
        class = 3;
        levels = quantized_levels(coefficients, quantization, class);
    }
    let mut ac = Vec::new();
    let mut previous = 0;
    for (position, &level) in levels.iter().enumerate().skip(1) {
        if level == 0 {
            continue;
        }
        if level.unsigned_abs() > 255 {
            return Err(Error::InvalidData(
                "DV coefficient exceeds encodable level after class selection".into(),
            ));
        }
        ac.extend(vlc_for_encoding(position - previous - 1, level)?);
        previous = position;
    }
    ac.extend(vlc_for_encoding(127, 0)?);
    let dc_delta = coefficients[0] - 1_024;
    let dc = if dc_delta >= 0 {
        (dc_delta + 2) / 4
    } else {
        (dc_delta - 2) / 4
    }
    .clamp(-256, 255);
    Ok(EncodedCell { dc, class, ac })
}

fn quantized_levels(coefficients: &[i32; 64], quantization: usize, class: usize) -> [i32; 64] {
    let mut levels = [0; 64];
    for position in 1..64 {
        let coefficient = coefficients[ZIGZAG[position]];
        let factor = inverse_factor_for_encoding(quantization, class, false, position);
        let magnitude = (i64::from(coefficient.unsigned_abs()) * 16_384 + i64::from(factor / 2))
            / i64::from(factor);
        levels[position] = i32::try_from(magnitude)
            .unwrap_or(i32::MAX)
            .copysign(coefficient);
    }
    levels
}

fn pack_segment(cells: &[EncodedCell]) -> Result<Vec<PackedCell>> {
    let mut packed = Vec::with_capacity(30);
    for (index, cell) in cells.iter().enumerate() {
        let capacity = BLOCK_BITS[index % 6] - 12;
        let used = capacity.min(cell.ac.len());
        let mut storage = vec![1; capacity];
        storage[..used].copy_from_slice(&cell.ac[..used]);
        packed.push(PackedCell {
            dc: cell.dc,
            class: cell.class,
            storage,
            used,
            ac: cell.ac.clone(),
            ac_offset: used,
        });
    }

    let mut segment_positions = Vec::new();
    for macroblock in 0..5 {
        let range = macroblock * 6..macroblock * 6 + 6;
        let pool = available_positions(&packed, range.clone());
        let unused = fill_pending(&mut packed, range.clone(), &pool);
        if packed[range.clone()].iter().all(PackedCell::complete) {
            segment_positions.extend_from_slice(&pool[pool.len() - unused..]);
        }
    }
    let all = 0..packed.len();
    let _unused = fill_pending(&mut packed, all.clone(), &segment_positions);
    if packed.iter().any(|cell| !cell.complete()) {
        return Err(Error::InvalidData("DV coefficient spill overflow".into()));
    }
    Ok(packed)
}

fn available_positions(cells: &[PackedCell], range: std::ops::Range<usize>) -> Vec<(usize, usize)> {
    let mut positions = Vec::new();
    for index in range {
        if cells[index].complete() {
            positions
                .extend((cells[index].used..cells[index].storage.len()).map(|bit| (index, bit)));
        }
    }
    positions
}

fn fill_pending(
    cells: &mut [PackedCell],
    pending_range: std::ops::Range<usize>,
    positions: &[(usize, usize)],
) -> usize {
    let mut position = 0;
    for pending in pending_range {
        while !cells[pending].complete() && position < positions.len() {
            let (cell, bit) = positions[position];
            cells[cell].storage[bit] = cells[pending].ac[cells[pending].ac_offset];
            cells[pending].ac_offset += 1;
            position += 1;
        }
    }
    positions.len() - position
}

fn gather_macroblock(
    frame: &VideoFrame,
    profile: DvProfile,
    mb_x: usize,
    mb_y: usize,
) -> [[u8; 64]; 6] {
    let x = mb_x * 8;
    let y = mb_y * 8;
    match profile.system {
        DvSystem::System625_50 => [
            read_block(&frame.planes[0], x, y),
            read_block(&frame.planes[0], x + 8, y),
            read_block(&frame.planes[0], x, y + 8),
            read_block(&frame.planes[0], x + 8, y + 8),
            read_block(&frame.planes[2], x / 2, y / 2),
            read_block(&frame.planes[1], x / 2, y / 2),
        ],
        DvSystem::System525_60 if mb_x < 88 => [
            read_block(&frame.planes[0], x, y),
            read_block(&frame.planes[0], x + 8, y),
            read_block(&frame.planes[0], x + 16, y),
            read_block(&frame.planes[0], x + 24, y),
            read_block(&frame.planes[2], x / 4, y),
            read_block(&frame.planes[1], x / 4, y),
        ],
        DvSystem::System525_60 => [
            read_block(&frame.planes[0], x, y),
            read_block(&frame.planes[0], x + 8, y),
            read_block(&frame.planes[0], x, y + 8),
            read_block(&frame.planes[0], x + 8, y + 8),
            read_split_chroma(&frame.planes[2], x / 4, y),
            read_split_chroma(&frame.planes[1], x / 4, y),
        ],
    }
}

fn read_block(plane: &mmrecode_core::Plane, x: usize, y: usize) -> [u8; 64] {
    std::array::from_fn(|index| plane.data[(y + index / 8) * plane.stride + x + index % 8])
}

fn read_split_chroma(plane: &mmrecode_core::Plane, x: usize, y: usize) -> [u8; 64] {
    std::array::from_fn(|index| {
        let row = index / 8;
        let column = index % 8;
        if column < 4 {
            plane.data[(y + row) * plane.stride + x + column]
        } else {
            plane.data[(y + 8 + row) * plane.stride + x + column - 4]
        }
    })
}

#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss
)]
fn forward_dct(pixels: &[u8; 64]) -> [i32; 64] {
    let mut coefficients = [0; 64];
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
            let mut sum = 0.0;
            for y in 0..8 {
                for x in 0..8 {
                    sum += f64::from(pixels[y * 8 + x])
                        * ((2 * x + 1) as f64 * u as f64 * std::f64::consts::PI / 16.0).cos()
                        * ((2 * y + 1) as f64 * v as f64 * std::f64::consts::PI / 16.0).cos();
                }
            }
            coefficients[v * 8 + u] = (cu * cv * sum / 4.0).round() as i32;
        }
    }
    coefficients
}

fn signed_to_bits(value: i32, count: usize) -> Vec<u8> {
    let mask = (1_u32 << count) - 1;
    unsigned_to_bits(value.cast_unsigned() & mask, count)
}

fn unsigned_to_bits(value: u32, count: usize) -> Vec<u8> {
    (0..count)
        .rev()
        .map(|shift| u8::from(value & (1 << shift) != 0))
        .collect()
}

fn bits_to_bytes(bits: &[u8]) -> Vec<u8> {
    bits.as_chunks::<8>()
        .0
        .iter()
        .map(|chunk| chunk.iter().fold(0, |byte, &bit| byte << 1 | bit))
        .collect()
}

trait CopySign {
    fn copysign(self, sign: i32) -> Self;
}

impl CopySign for i32 {
    fn copysign(self, sign: i32) -> Self {
        if sign < 0 { -self } else { self }
    }
}

#[cfg(test)]
mod tests {
    use crate::DvPackData;
    use mmrecode_core::{ColorDescription, FieldOrder, FrameTiming, Plane};

    use super::*;

    fn constant_frame(profile: DvProfile, value: u8) -> VideoFrame {
        let (chroma_width, chroma_height) = match profile.system {
            DvSystem::System525_60 => (180, 480),
            DvSystem::System625_50 => (360, 288),
        };
        VideoFrame {
            format: profile.pixel_format,
            width: profile.width,
            height: profile.height,
            planes: vec![
                Plane {
                    data: vec![value; profile.width * profile.height],
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
            ],
            timing: FrameTiming::default(),
            color: ColorDescription::default(),
            field_order: FieldOrder::BottomFirst,
        }
    }

    #[test]
    fn constant_frames_encode_deterministically_and_reconstruct() {
        for profile in [DvProfile::DV25_525_60, DvProfile::DV25_625_50] {
            let source = constant_frame(profile, 96);
            let first = encode_video(&source).unwrap();
            let second = encode_video(&source).unwrap();
            assert_eq!(first.data, second.data);
            assert_eq!(first.data.len(), profile.frame_size);
            assert_eq!(first.reconstructed.planes[0].data, source.planes[0].data);
        }
    }

    #[test]
    fn timecode_pack_round_trips() {
        let profile = DvProfile::DV25_525_60;
        let timecode = Timecode {
            hours: 12,
            minutes: 34,
            seconds: 56,
            frames: 12,
            drop_frame: true,
        };
        let mut data = initialize_dif_frame(profile);
        embed_timecode(profile, timecode, &mut data).unwrap();
        let parsed = parse_frame(&data).unwrap();
        assert!(
            parsed
                .packs()
                .iter()
                .any(|pack| matches!(pack.data, DvPackData::Timecode(value) if value == timecode))
        );
    }
}
