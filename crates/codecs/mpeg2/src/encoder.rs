//! Deterministic MPEG-2 Main Profile 4:2:0 encoder reference path.

use mmrecode_bitstream::BitWriter;
use mmrecode_core::{Error, PixelFormat, Result, VideoFrame};

use crate::{
    FrameRate, PictureType, SequenceDisplayExtension, decode_stream,
    tables::{
        CODED_BLOCK_PATTERN, DC_CHROMA, DC_LUMA, DCT_COEFFICIENT_ZERO, DEFAULT_INTRA_MATRIX,
        DEFAULT_NON_INTRA_MATRIX, LEVEL, MACROBLOCK_ADDRESS_INCREMENT, MACROBLOCK_B, MACROBLOCK_P,
        MOTION_CODE, RUN, ZIGZAG,
    },
    transform::forward_dct,
};

const ESCAPE_SYMBOL: usize = 111;
const EOB_SYMBOL: usize = 112;
const F_CODE: u8 = 3;

/// Quantizer matrices used and signalled by the encoder, in natural coefficient order.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Mpeg2QuantMatrices {
    /// Luma intra matrix.
    pub intra: [u8; 64],
    /// Luma non-intra matrix.
    pub non_intra: [u8; 64],
    /// Chroma intra matrix.
    pub chroma_intra: [u8; 64],
    /// Chroma non-intra matrix.
    pub chroma_non_intra: [u8; 64],
}

impl Default for Mpeg2QuantMatrices {
    fn default() -> Self {
        let intra = std::array::from_fn(|index| {
            u8::try_from(DEFAULT_INTRA_MATRIX[index])
                .expect("default MPEG-2 intra matrix values fit u8")
        });
        let non_intra = std::array::from_fn(|index| {
            u8::try_from(DEFAULT_NON_INTRA_MATRIX[index])
                .expect("default MPEG-2 non-intra matrix values fit u8")
        });
        Self {
            intra,
            non_intra,
            chroma_intra: intra,
            chroma_non_intra: non_intra,
        }
    }
}

/// Sequence and GOP metadata controlled independently from picture coding tools.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Mpeg2SequenceSettings {
    /// MPEG aspect-ratio information code.
    pub aspect_ratio_information: u8,
    /// Declared upper-bound input bitrate in bits per second, in 400-bit/s units.
    pub bit_rate: u64,
    /// Declared VBV buffer size in bits, in 16,384-bit units.
    pub vbv_buffer_size_bits: u64,
    /// MPEG-2 profile-and-level indication.
    pub profile_and_level_indication: u8,
    /// Optional sequence display and colour metadata.
    pub display: Option<SequenceDisplayExtension>,
    /// Quantizer matrices used for coding and written into the sequence.
    pub quant_matrices: Mpeg2QuantMatrices,
    /// Absolute first-frame timecode origin for the encoded segment.
    pub timecode_start_frame: u64,
    /// Use SMPTE drop-frame numbering for 30000/1001 content.
    pub drop_frame_timecode: bool,
}

impl Default for Mpeg2SequenceSettings {
    fn default() -> Self {
        Self {
            aspect_ratio_information: 1,
            bit_rate: 15_000_000,
            vbv_buffer_size_bits: 112 * 16_384,
            profile_and_level_indication: 0x48,
            display: None,
            quant_matrices: Mpeg2QuantMatrices::default(),
            timecode_start_frame: 0,
            drop_frame_timecode: false,
        }
    }
}

/// Deterministic MPEG-2 encoder settings.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Mpeg2EncodeOptions {
    /// Nominal frame rate written into the sequence header.
    pub frame_rate: FrameRate,
    /// Maximum pictures per closed GOP.
    pub gop_size: usize,
    /// B pictures between successive I/P references.
    pub b_frames: usize,
    /// Linear quantizer-scale code from 1 through 31.
    pub quantiser_scale_code: u8,
    /// Integer-pixel P-picture motion-search radius.
    pub motion_search_range: usize,
    /// Emit progressive sequence/picture flags when true.
    pub progressive: bool,
    /// Top-field-first flag for interlaced frame pictures.
    pub top_field_first: bool,
    /// Sequence/display, matrix, rate-signalling, and GOP timecode settings.
    pub sequence: Mpeg2SequenceSettings,
}

impl Default for Mpeg2EncodeOptions {
    fn default() -> Self {
        Self {
            frame_rate: FrameRate::Fps25,
            gop_size: 12,
            b_frames: 2,
            quantiser_scale_code: 8,
            motion_search_range: 4,
            progressive: true,
            top_field_first: false,
            sequence: Mpeg2SequenceSettings::default(),
        }
    }
}

/// Encoded elementary stream and its normative native reconstruction.
#[derive(Clone, Debug)]
pub struct EncodedMpeg2 {
    /// Raw MPEG-2 Video elementary stream.
    pub data: Vec<u8>,
    /// Decoder reconstruction in presentation order.
    pub reconstructed: Vec<VideoFrame>,
    /// Coded picture types in elementary-stream decode order.
    pub picture_types: Vec<PictureType>,
}

/// Encodes a complete sequence of 8-bit planar 4:2:0 frames.
///
/// The reference encoder uses closed GOPs, I/P/B reordering, integer-pixel P-picture motion
/// estimation, zero-vector bidirectional B prediction, one slice per macroblock row, and VBR
/// (`vbv_delay = 0xffff`) operation bounded by the Main Profile/Main Level maximum bit rate.
/// Reconstruction is produced by the native decoder.
///
/// # Errors
///
/// Returns an error for empty or inconsistent input, invalid options, entropy overflow, or an
/// internally undecodable generated stream.
pub fn encode_stream(frames: &[VideoFrame], options: Mpeg2EncodeOptions) -> Result<EncodedMpeg2> {
    validate_input(frames, &options)?;
    let mut data = Vec::new();
    write_sequence_header(&mut data, frames[0].width, frames[0].height, &options)?;
    write_sequence_extension(&mut data, &options)?;
    if let Some(display) = options.sequence.display {
        write_sequence_display_extension(&mut data, display)?;
    }
    write_chroma_quant_matrix_extension(&mut data, options.sequence.quant_matrices)?;
    let mut picture_types = Vec::with_capacity(frames.len());

    for gop_start in (0..frames.len()).step_by(options.gop_size) {
        let gop_end = (gop_start + options.gop_size).min(frames.len());
        write_group_header(&mut data, gop_start, &options)?;
        encode_and_append_picture(
            &mut data,
            &frames[gop_start],
            None,
            None,
            PictureType::I,
            0,
            &options,
        )?;
        picture_types.push(PictureType::I);
        let mut previous_reference_index = gop_start;
        let mut previous_reconstruction = decode_last_reference(&data)?;
        let reference_distance = options.b_frames + 1;

        while previous_reference_index + 1 < gop_end {
            let next_reference_index =
                (previous_reference_index + reference_distance).min(gop_end - 1);
            encode_and_append_picture(
                &mut data,
                &frames[next_reference_index],
                Some(&previous_reconstruction),
                None,
                PictureType::P,
                next_reference_index - gop_start,
                &options,
            )?;
            picture_types.push(PictureType::P);
            let next_reconstruction = decode_last_reference(&data)?;
            for (display_index, frame) in frames
                .iter()
                .enumerate()
                .take(next_reference_index)
                .skip(previous_reference_index + 1)
            {
                encode_and_append_picture(
                    &mut data,
                    frame,
                    Some(&previous_reconstruction),
                    Some(&next_reconstruction),
                    PictureType::B,
                    display_index - gop_start,
                    &options,
                )?;
                picture_types.push(PictureType::B);
            }
            previous_reference_index = next_reference_index;
            previous_reconstruction = next_reconstruction;
        }
    }
    append_start_code(&mut data, 0xb7);
    let reconstructed = decode_stream(&data)?
        .into_iter()
        .map(|picture| picture.frame)
        .collect();
    Ok(EncodedMpeg2 {
        data,
        reconstructed,
        picture_types,
    })
}

fn validate_input(frames: &[VideoFrame], options: &Mpeg2EncodeOptions) -> Result<()> {
    let first = frames
        .first()
        .ok_or_else(|| Error::InvalidData("cannot encode an empty MPEG-2 sequence".into()))?;
    if first.width == 0 || first.height == 0 || first.width > 720 || first.height > 576 {
        return Err(Error::Unsupported(
            "MPEG-2 Main Profile/Main Level encoder dimensions must be at most 720x576".into(),
        ));
    }
    if first.format != PixelFormat::Yuv420p8 || first.width % 2 != 0 || first.height % 2 != 0 {
        return Err(Error::Unsupported(
            "MPEG-2 reference encoder requires even-sized Yuv420p8 frames".into(),
        ));
    }
    if options.gop_size == 0 || options.gop_size > 1_024 || options.b_frames >= options.gop_size {
        return Err(Error::InvalidData("invalid MPEG-2 GOP settings".into()));
    }
    if !(1..=31).contains(&options.quantiser_scale_code) || options.motion_search_range > 7 {
        return Err(Error::InvalidData(
            "MPEG-2 quantizer code must be 1..=31 and search range at most 7".into(),
        ));
    }
    validate_sequence_settings(options)?;
    if matches!(
        options.frame_rate,
        FrameRate::Fps50 | FrameRate::Fps59_94 | FrameRate::Fps60
    ) {
        return Err(Error::Unsupported(
            "MPEG-2 Main Level encoder supports frame rates through 30 fps".into(),
        ));
    }
    let rate = options.frame_rate.rational();
    let samples_per_second = u64::try_from(first.width)
        .ok()
        .and_then(|width| {
            u64::try_from(first.height)
                .ok()
                .and_then(|height| width.checked_mul(height))
        })
        .and_then(|samples| {
            u64::try_from(rate.numerator())
                .ok()
                .and_then(|numerator| samples.checked_mul(numerator))
        })
        .and_then(|rate_samples| {
            u64::try_from(rate.denominator())
                .ok()
                .map(|denominator| rate_samples / denominator)
        })
        .ok_or_else(|| Error::Unsupported("MPEG-2 sample rate overflows".into()))?;
    if samples_per_second > 10_368_000 {
        return Err(Error::Unsupported(format!(
            "MPEG-2 Main Level sample rate {samples_per_second} exceeds 10368000 samples/s"
        )));
    }
    for (index, frame) in frames.iter().enumerate() {
        if (frame.width, frame.height, frame.format) != (first.width, first.height, first.format) {
            return Err(Error::InvalidData(format!(
                "MPEG-2 input frame {index} does not match the sequence format"
            )));
        }
        validate_planes(frame)?;
    }
    Ok(())
}

fn validate_sequence_settings(options: &Mpeg2EncodeOptions) -> Result<()> {
    let sequence = options.sequence;
    if !(1..=4).contains(&sequence.aspect_ratio_information) {
        return Err(Error::Unsupported(
            "MPEG-2 encoder supports aspect-ratio information codes 1 through 4".into(),
        ));
    }
    if sequence.profile_and_level_indication != 0x48 {
        return Err(Error::Unsupported(
            "MPEG-2 reference encoder only emits Main Profile at Main Level (0x48)".into(),
        ));
    }
    if sequence.bit_rate == 0 || !sequence.bit_rate.is_multiple_of(400) {
        return Err(Error::InvalidData(
            "MPEG-2 declared bitrate must be a non-zero multiple of 400 bit/s".into(),
        ));
    }
    let bit_rate_value = sequence.bit_rate / 400;
    if bit_rate_value >= (1_u64 << 30) {
        return Err(Error::InvalidData(
            "MPEG-2 declared bitrate exceeds its 30-bit syntax field".into(),
        ));
    }
    if sequence.vbv_buffer_size_bits == 0
        || !sequence.vbv_buffer_size_bits.is_multiple_of(16_384)
        || sequence.vbv_buffer_size_bits / 16_384 >= (1_u64 << 18)
    {
        return Err(Error::InvalidData(
            "MPEG-2 VBV buffer size must fit the non-zero 18-bit syntax in 16,384-bit units".into(),
        ));
    }
    if sequence.drop_frame_timecode && options.frame_rate != FrameRate::Fps29_97 {
        return Err(Error::Unsupported(
            "drop-frame GOP timecode requires 30000/1001 fps".into(),
        ));
    }
    if let Some(display) = sequence.display
        && (display.video_format > 7
            || display.display_horizontal_size == 0
            || display.display_horizontal_size >= (1 << 14)
            || display.display_vertical_size == 0
            || display.display_vertical_size >= (1 << 14))
    {
        return Err(Error::InvalidData(
            "invalid MPEG-2 sequence display metadata".into(),
        ));
    }
    for matrix in [
        &sequence.quant_matrices.intra,
        &sequence.quant_matrices.non_intra,
        &sequence.quant_matrices.chroma_intra,
        &sequence.quant_matrices.chroma_non_intra,
    ] {
        if matrix.contains(&0) {
            return Err(Error::InvalidData(
                "MPEG-2 quantizer matrices cannot contain zero".into(),
            ));
        }
    }
    Ok(())
}

fn validate_planes(frame: &VideoFrame) -> Result<()> {
    let expected = [
        (frame.width, frame.height),
        (frame.width / 2, frame.height / 2),
        (frame.width / 2, frame.height / 2),
    ];
    if frame.planes.len() != 3 {
        return Err(Error::InvalidData(
            "Yuv420p8 frame must have three planes".into(),
        ));
    }
    for (index, (plane, &(width, height))) in frame.planes.iter().zip(&expected).enumerate() {
        if plane.width != width
            || plane.height != height
            || plane.stride < width
            || plane.data.len() < plane.stride * height
        {
            return Err(Error::InvalidData(format!(
                "invalid MPEG-2 input plane {index} layout"
            )));
        }
    }
    Ok(())
}

fn write_sequence_header(
    data: &mut Vec<u8>,
    width: usize,
    height: usize,
    options: &Mpeg2EncodeOptions,
) -> Result<()> {
    append_start_code(data, 0xb3);
    let mut bits = BitWriter::new();
    bits.write_bits(u64::try_from(width).map_err(integer_error)?, 12)?;
    bits.write_bits(u64::try_from(height).map_err(integer_error)?, 12)?;
    bits.write_bits(u64::from(options.sequence.aspect_ratio_information), 4)?;
    bits.write_bits(u64::from(options.frame_rate.code()), 4)?;
    let bit_rate_value = options.sequence.bit_rate / 400;
    bits.write_bits(bit_rate_value & ((1 << 18) - 1), 18)?;
    bits.write_bit(true)?;
    let vbv_value = options.sequence.vbv_buffer_size_bits / 16_384;
    bits.write_bits(vbv_value & ((1 << 10) - 1), 10)?;
    bits.write_bit(false)?;
    write_matrix(
        &mut bits,
        &options.sequence.quant_matrices.intra,
        &default_intra_matrix_array(),
    )?;
    write_matrix(
        &mut bits,
        &options.sequence.quant_matrices.non_intra,
        &default_non_intra_matrix_array(),
    )?;
    bits.align_to_byte();
    data.extend(bits.into_bytes());
    Ok(())
}

fn write_sequence_extension(data: &mut Vec<u8>, options: &Mpeg2EncodeOptions) -> Result<()> {
    append_start_code(data, 0xb5);
    let mut bits = BitWriter::new();
    bits.write_bits(1, 4)?;
    bits.write_bits(u64::from(options.sequence.profile_and_level_indication), 8)?;
    bits.write_bit(options.progressive)?;
    bits.write_bits(1, 2)?;
    bits.write_bits(0, 2)?;
    bits.write_bits(0, 2)?;
    let bit_rate_value = options.sequence.bit_rate / 400;
    bits.write_bits(bit_rate_value >> 18, 12)?;
    bits.write_bit(true)?;
    let vbv_value = options.sequence.vbv_buffer_size_bits / 16_384;
    bits.write_bits(vbv_value >> 10, 8)?;
    bits.write_bit(options.b_frames == 0)?;
    bits.write_bits(0, 2)?;
    bits.write_bits(0, 5)?;
    bits.align_to_byte();
    data.extend(bits.into_bytes());
    Ok(())
}

fn write_sequence_display_extension(
    data: &mut Vec<u8>,
    display: SequenceDisplayExtension,
) -> Result<()> {
    append_start_code(data, 0xb5);
    let mut bits = BitWriter::new();
    bits.write_bits(2, 4)?;
    bits.write_bits(u64::from(display.video_format), 3)?;
    bits.write_bit(display.colour_description.is_some())?;
    if let Some(colour) = display.colour_description {
        bits.write_bits(u64::from(colour.colour_primaries), 8)?;
        bits.write_bits(u64::from(colour.transfer_characteristics), 8)?;
        bits.write_bits(u64::from(colour.matrix_coefficients), 8)?;
    }
    bits.write_bits(u64::from(display.display_horizontal_size), 14)?;
    bits.write_bit(true)?;
    bits.write_bits(u64::from(display.display_vertical_size), 14)?;
    bits.align_to_byte();
    data.extend(bits.into_bytes());
    Ok(())
}

fn write_chroma_quant_matrix_extension(
    data: &mut Vec<u8>,
    matrices: Mpeg2QuantMatrices,
) -> Result<()> {
    let write_chroma_intra = matrices.chroma_intra != matrices.intra;
    let write_chroma_non_intra = matrices.chroma_non_intra != matrices.non_intra;
    if !write_chroma_intra && !write_chroma_non_intra {
        return Ok(());
    }
    append_start_code(data, 0xb5);
    let mut bits = BitWriter::new();
    bits.write_bits(3, 4)?;
    bits.write_bit(false)?;
    bits.write_bit(false)?;
    write_optional_matrix(&mut bits, write_chroma_intra, &matrices.chroma_intra)?;
    write_optional_matrix(
        &mut bits,
        write_chroma_non_intra,
        &matrices.chroma_non_intra,
    )?;
    bits.align_to_byte();
    data.extend(bits.into_bytes());
    Ok(())
}

fn write_matrix(bits: &mut BitWriter, matrix: &[u8; 64], default: &[u8; 64]) -> Result<()> {
    let present = matrix != default;
    write_optional_matrix(bits, present, matrix)
}

fn write_optional_matrix(bits: &mut BitWriter, present: bool, matrix: &[u8; 64]) -> Result<()> {
    bits.write_bit(present)?;
    if present {
        for &position in &ZIGZAG {
            bits.write_bits(u64::from(matrix[position]), 8)?;
        }
    }
    Ok(())
}

fn default_intra_matrix_array() -> [u8; 64] {
    std::array::from_fn(|index| {
        u8::try_from(DEFAULT_INTRA_MATRIX[index])
            .expect("default MPEG-2 intra matrix values fit u8")
    })
}

fn default_non_intra_matrix_array() -> [u8; 64] {
    std::array::from_fn(|index| {
        u8::try_from(DEFAULT_NON_INTRA_MATRIX[index])
            .expect("default MPEG-2 non-intra matrix values fit u8")
    })
}

fn write_group_header(
    data: &mut Vec<u8>,
    frame_index: usize,
    options: &Mpeg2EncodeOptions,
) -> Result<()> {
    append_start_code(data, 0xb8);
    let absolute_frame = options
        .sequence
        .timecode_start_frame
        .checked_add(u64::try_from(frame_index).map_err(integer_error)?)
        .ok_or_else(|| Error::InvalidData("MPEG-2 GOP timecode frame count overflows".into()))?;
    let (hours, minutes, seconds, pictures) = gop_timecode(absolute_frame, options)?;
    let mut bits = BitWriter::new();
    bits.write_bit(options.sequence.drop_frame_timecode)?;
    bits.write_bits(u64::from(hours), 5)?;
    bits.write_bits(u64::from(minutes), 6)?;
    bits.write_bit(true)?;
    bits.write_bits(u64::from(seconds), 6)?;
    bits.write_bits(u64::from(pictures), 6)?;
    bits.write_bit(true)?;
    bits.write_bit(false)?;
    bits.align_to_byte();
    data.extend(bits.into_bytes());
    Ok(())
}

fn gop_timecode(frame: u64, options: &Mpeg2EncodeOptions) -> Result<(u8, u8, u8, u8)> {
    let nominal_fps = match options.frame_rate {
        FrameRate::Fps23_976 | FrameRate::Fps24 => 24_u64,
        FrameRate::Fps25 => 25,
        FrameRate::Fps29_97 | FrameRate::Fps30 => 30,
        FrameRate::Fps50 => 50,
        FrameRate::Fps59_94 | FrameRate::Fps60 => 60,
    };
    let numbered_frame = if options.sequence.drop_frame_timecode {
        let ten_minute_blocks = frame / 17_982;
        let remainder = frame % 17_982;
        let dropped = ten_minute_blocks
            .checked_mul(18)
            .and_then(|value| {
                value.checked_add(if remainder >= 2 {
                    2 * ((remainder - 2) / 1_798)
                } else {
                    0
                })
            })
            .ok_or_else(|| Error::InvalidData("drop-frame timecode overflows".into()))?;
        frame
            .checked_add(dropped)
            .ok_or_else(|| Error::InvalidData("drop-frame timecode overflows".into()))?
    } else {
        frame
    };
    let total_seconds = numbered_frame / nominal_fps;
    Ok((
        u8::try_from((total_seconds / 3_600) % 24).map_err(integer_error)?,
        u8::try_from((total_seconds / 60) % 60).map_err(integer_error)?,
        u8::try_from(total_seconds % 60).map_err(integer_error)?,
        u8::try_from(numbered_frame % nominal_fps).map_err(integer_error)?,
    ))
}

#[allow(clippy::too_many_arguments)]
fn encode_and_append_picture(
    data: &mut Vec<u8>,
    source: &VideoFrame,
    previous: Option<&VideoFrame>,
    next: Option<&VideoFrame>,
    picture_type: PictureType,
    temporal_reference: usize,
    options: &Mpeg2EncodeOptions,
) -> Result<()> {
    write_picture_header(data, picture_type, temporal_reference)?;
    write_picture_coding_extension(data, options)?;
    let mb_width = source.width.div_ceil(16);
    let mb_height = source.height.div_ceil(16);
    for mb_y in 0..mb_height {
        append_start_code(
            data,
            u8::try_from(mb_y + 1)
                .map_err(|_| Error::Unsupported("MPEG-2 slice row exceeds one byte".into()))?,
        );
        let mut bits = BitWriter::new();
        bits.write_bits(u64::from(options.quantiser_scale_code), 5)?;
        bits.write_bit(false)?;
        write_vlc(&mut bits, MACROBLOCK_ADDRESS_INCREMENT[0])?;
        let mut dc_predictor = [128_i32; 3];
        let mut motion_predictor = [0_i32; 2];
        for mb_x in 0..mb_width {
            encode_macroblock(
                &mut bits,
                source,
                previous,
                next,
                mb_x,
                mb_y,
                picture_type,
                options,
                &mut dc_predictor,
                &mut motion_predictor,
            )?;
            if mb_x + 1 < mb_width {
                write_vlc(&mut bits, MACROBLOCK_ADDRESS_INCREMENT[0])?;
            }
        }
        bits.align_to_byte();
        data.extend(bits.into_bytes());
    }
    Ok(())
}

fn write_picture_header(
    data: &mut Vec<u8>,
    picture_type: PictureType,
    temporal_reference: usize,
) -> Result<()> {
    append_start_code(data, 0x00);
    let type_code = match picture_type {
        PictureType::I => 1,
        PictureType::P => 2,
        PictureType::B => 3,
        PictureType::D | PictureType::Reserved(_) => {
            return Err(Error::Unsupported("cannot encode MPEG-1 D pictures".into()));
        }
    };
    let mut bits = BitWriter::new();
    bits.write_bits(
        u64::try_from(temporal_reference).map_err(integer_error)?,
        10,
    )?;
    bits.write_bits(type_code, 3)?;
    bits.write_bits(0xffff, 16)?;
    if matches!(picture_type, PictureType::P | PictureType::B) {
        bits.write_bit(false)?;
        bits.write_bits(u64::from(F_CODE), 3)?;
    }
    if picture_type == PictureType::B {
        bits.write_bit(false)?;
        bits.write_bits(u64::from(F_CODE), 3)?;
    }
    bits.write_bit(false)?;
    bits.align_to_byte();
    data.extend(bits.into_bytes());
    Ok(())
}

fn write_picture_coding_extension(data: &mut Vec<u8>, options: &Mpeg2EncodeOptions) -> Result<()> {
    append_start_code(data, 0xb5);
    let mut bits = BitWriter::new();
    bits.write_bits(8, 4)?;
    for _ in 0..4 {
        bits.write_bits(u64::from(F_CODE), 4)?;
    }
    bits.write_bits(0, 2)?;
    bits.write_bits(3, 2)?;
    bits.write_bit(options.top_field_first)?;
    bits.write_bit(true)?;
    bits.write_bit(false)?;
    bits.write_bit(false)?;
    bits.write_bit(false)?;
    bits.write_bit(false)?;
    bits.write_bit(false)?;
    bits.write_bit(options.progressive)?;
    bits.write_bit(options.progressive)?;
    bits.write_bit(false)?;
    bits.align_to_byte();
    data.extend(bits.into_bytes());
    Ok(())
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn encode_macroblock(
    bits: &mut BitWriter,
    source: &VideoFrame,
    previous: Option<&VideoFrame>,
    next: Option<&VideoFrame>,
    mb_x: usize,
    mb_y: usize,
    picture_type: PictureType,
    options: &Mpeg2EncodeOptions,
    dc_predictor: &mut [i32; 3],
    motion_predictor: &mut [i32; 2],
) -> Result<()> {
    let qscale = i32::from(options.quantiser_scale_code) * 2;
    if picture_type == PictureType::I {
        bits.write_bit(true)?;
        for block in 0..6 {
            let coefficients = forward_dct(&read_intra_block(source, mb_x, mb_y, block));
            let matrix = if block < 4 {
                &options.sequence.quant_matrices.intra
            } else {
                &options.sequence.quant_matrices.chroma_intra
            };
            write_intra_block(
                bits,
                block,
                &quantize_intra(&coefficients, qscale, matrix),
                dc_predictor,
            )?;
        }
        return Ok(());
    }

    let (forward_motion, backward_motion) = match picture_type {
        PictureType::P => {
            let reference = previous.ok_or_else(|| {
                Error::InvalidState("P picture encoder lacks previous reconstruction".into())
            })?;
            (
                Some(search_motion(
                    source,
                    reference,
                    mb_x,
                    mb_y,
                    options.motion_search_range,
                )),
                None,
            )
        }
        PictureType::B => (Some([0, 0]), Some([0, 0])),
        _ => unreachable!("intra picture returned above"),
    };
    let mut quantized_blocks = [[0_i16; 64]; 6];
    let mut cbp = 0_u8;
    for (block, quantized) in quantized_blocks.iter_mut().enumerate() {
        let residual = read_residual_block(
            source,
            previous,
            next,
            mb_x,
            mb_y,
            block,
            forward_motion,
            backward_motion,
        )?;
        let matrix = if block < 4 {
            &options.sequence.quant_matrices.non_intra
        } else {
            &options.sequence.quant_matrices.chroma_non_intra
        };
        *quantized = quantize_non_intra(&forward_dct(&residual), qscale, matrix);
        if quantized.iter().any(|&value| value != 0) {
            cbp |= 1 << (5 - block);
        }
    }

    match picture_type {
        PictureType::P => {
            write_sparse_vlc(bits, &MACROBLOCK_P, if cbp == 0 { 0x04 } else { 0x06 })?;
            write_motion_vector(
                bits,
                forward_motion.expect("P picture has forward motion"),
                motion_predictor,
            )?;
        }
        PictureType::B => {
            write_sparse_vlc(bits, &MACROBLOCK_B, if cbp == 0 { 0x0c } else { 0x0e })?;
            write_motion_vector(
                bits,
                forward_motion.expect("B picture has forward motion"),
                &mut [0; 2],
            )?;
            write_motion_vector(
                bits,
                backward_motion.expect("B picture has backward motion"),
                &mut [0; 2],
            )?;
        }
        _ => unreachable!("intra picture returned above"),
    }
    if cbp != 0 {
        write_vlc(bits, CODED_BLOCK_PATTERN[usize::from(cbp)])?;
        for (block, quantized) in quantized_blocks.iter().enumerate() {
            if cbp & (1 << (5 - block)) != 0 {
                write_non_intra_block(bits, quantized)?;
            }
        }
    }
    Ok(())
}

fn quantize_intra(coefficients: &[i32; 64], qscale: i32, matrix: &[u8; 64]) -> [i16; 64] {
    let mut output = [0_i16; 64];
    output[0] = i16::try_from((coefficients[0] + 4) / 8).unwrap_or(255);
    for position in 1..64 {
        let divisor = qscale * i32::from(matrix[position]);
        let value = divide_round(coefficients[position] * 16, divisor).clamp(-2_047, 2_047);
        output[position] = i16::try_from(value).unwrap_or_default();
    }
    output
}

fn quantize_non_intra(coefficients: &[i32; 64], qscale: i32, matrix: &[u8; 64]) -> [i16; 64] {
    let mut output = [0_i16; 64];
    for position in 0..64 {
        let divisor = qscale * i32::from(matrix[position]);
        let value = divide_round(coefficients[position] * 16, divisor).clamp(-2_047, 2_047);
        output[position] = i16::try_from(value).unwrap_or_default();
    }
    output
}

fn divide_round(value: i32, divisor: i32) -> i32 {
    if value < 0 {
        -((-value + divisor / 2) / divisor)
    } else {
        (value + divisor / 2) / divisor
    }
}

fn write_intra_block(
    bits: &mut BitWriter,
    block: usize,
    coefficients: &[i16; 64],
    dc_predictor: &mut [i32; 3],
) -> Result<()> {
    let component = if block < 4 { 0 } else { block - 3 };
    let dc = i32::from(coefficients[0]);
    let differential = dc - dc_predictor[component];
    dc_predictor[component] = dc;
    write_dc(bits, differential, component == 0)?;
    write_ac_coefficients(bits, coefficients, 1)?;
    write_vlc(bits, DCT_COEFFICIENT_ZERO[EOB_SYMBOL])
}

fn write_non_intra_block(bits: &mut BitWriter, coefficients: &[i16; 64]) -> Result<()> {
    let mut start = 0;
    if coefficients[0].unsigned_abs() == 1 {
        bits.write_bit(true)?;
        bits.write_bit(coefficients[0] < 0)?;
        start = 1;
    }
    write_ac_coefficients(bits, coefficients, start)?;
    write_vlc(bits, DCT_COEFFICIENT_ZERO[EOB_SYMBOL])
}

fn write_ac_coefficients(
    bits: &mut BitWriter,
    coefficients: &[i16; 64],
    start: usize,
) -> Result<()> {
    let mut run = 0_usize;
    for &position in &ZIGZAG[start..] {
        let level = coefficients[position];
        if level == 0 {
            run += 1;
        } else {
            write_run_level(bits, run, level)?;
            run = 0;
        }
    }
    Ok(())
}

fn write_run_level(bits: &mut BitWriter, run: usize, level: i16) -> Result<()> {
    let magnitude = level.unsigned_abs();
    if let Some(symbol) = (0..ESCAPE_SYMBOL).find(|&index| {
        usize::from(RUN[index]) == run && u16::try_from(LEVEL[index]).ok() == Some(magnitude)
    }) {
        write_vlc(bits, DCT_COEFFICIENT_ZERO[symbol])?;
        bits.write_bit(level < 0)?;
        return Ok(());
    }
    if run > 63 || level == 0 {
        return Err(Error::InvalidData(
            "MPEG-2 escaped coefficient is out of range".into(),
        ));
    }
    write_vlc(bits, DCT_COEFFICIENT_ZERO[ESCAPE_SYMBOL])?;
    bits.write_bits(u64::try_from(run).map_err(integer_error)?, 6)?;
    bits.write_bits(
        u64::try_from(i32::from(level) & 0x0fff).map_err(integer_error)?,
        12,
    )
}

fn write_dc(bits: &mut BitWriter, differential: i32, luma: bool) -> Result<()> {
    let magnitude = differential.unsigned_abs();
    let size = if magnitude == 0 {
        0
    } else {
        u8::try_from(32 - magnitude.leading_zeros()).map_err(integer_error)?
    };
    if size > 11 {
        return Err(Error::InvalidData(
            "MPEG-2 DC differential is too large".into(),
        ));
    }
    let code = if luma {
        DC_LUMA[usize::from(size)]
    } else {
        DC_CHROMA[usize::from(size)]
    };
    write_vlc(bits, code)?;
    if size != 0 {
        let value = if differential < 0 {
            differential + (1_i32 << size) - 1
        } else {
            differential
        };
        bits.write_bits(u64::try_from(value).map_err(integer_error)?, size)?;
    }
    Ok(())
}

fn search_motion(
    source: &VideoFrame,
    reference: &VideoFrame,
    mb_x: usize,
    mb_y: usize,
    range: usize,
) -> [i32; 2] {
    let mut best = [0, 0];
    let mut best_sad = u64::MAX;
    let range = i32::try_from(range).unwrap_or(0);
    let block_x = i32::try_from(mb_x * 16).unwrap_or(i32::MAX);
    let block_y = i32::try_from(mb_y * 16).unwrap_or(i32::MAX);
    let coded_width = i32::try_from(reference.width.div_ceil(16) * 16).unwrap_or(i32::MAX);
    let coded_height = i32::try_from(reference.height.div_ceil(16) * 16).unwrap_or(i32::MAX);
    for dy in -range..=range {
        for dx in -range..=range {
            if block_x + dx < 0
                || block_y + dy < 0
                || block_x + dx + 15 >= coded_width
                || block_y + dy + 15 >= coded_height
            {
                continue;
            }
            let mut sad = 0_u64;
            for row in 0..16 {
                for column in 0..16 {
                    let x = mb_x * 16 + column;
                    let y = mb_y * 16 + row;
                    let source_sample = frame_sample(source, 0, x, y, [0, 0]);
                    let reference_sample = frame_sample(reference, 0, x, y, [dx * 2, dy * 2]);
                    sad += u64::from(source_sample.abs_diff(reference_sample));
                }
            }
            if sad < best_sad {
                best_sad = sad;
                best = [dx * 2, dy * 2];
            }
        }
    }
    best
}

fn write_motion_vector(
    bits: &mut BitWriter,
    vector: [i32; 2],
    predictor: &mut [i32; 2],
) -> Result<()> {
    for component in 0..2 {
        write_motion_component(bits, vector[component], predictor[component], F_CODE)?;
        predictor[component] = vector[component];
    }
    Ok(())
}

fn write_motion_component(
    bits: &mut BitWriter,
    value: i32,
    predictor: i32,
    f_code: u8,
) -> Result<()> {
    let shift = f_code - 1;
    let modulus = 1_i32 << (5 + shift);
    let mut delta = (value - predictor).rem_euclid(modulus);
    if delta >= modulus / 2 {
        delta -= modulus;
    }
    if delta == 0 {
        return write_vlc(bits, MOTION_CODE[0]);
    }
    let magnitude = delta.unsigned_abs();
    let code = usize::try_from(((magnitude - 1) >> shift) + 1).map_err(integer_error)?;
    if code >= MOTION_CODE.len() {
        return Err(Error::InvalidData(
            "MPEG-2 motion delta exceeds f_code".into(),
        ));
    }
    write_vlc(bits, MOTION_CODE[code])?;
    bits.write_bit(delta < 0)?;
    if shift != 0 {
        bits.write_bits(u64::from((magnitude - 1) & ((1_u32 << shift) - 1)), shift)?;
    }
    Ok(())
}

fn read_intra_block(frame: &VideoFrame, mb_x: usize, mb_y: usize, block: usize) -> [i16; 64] {
    let (plane, block_x, block_y) = block_location(mb_x, mb_y, block);
    std::array::from_fn(|index| {
        i16::from(frame_sample(
            frame,
            plane,
            block_x + index % 8,
            block_y + index / 8,
            [0, 0],
        ))
    })
}

#[allow(clippy::too_many_arguments)]
fn read_residual_block(
    source: &VideoFrame,
    previous: Option<&VideoFrame>,
    next: Option<&VideoFrame>,
    mb_x: usize,
    mb_y: usize,
    block: usize,
    forward_motion: Option<[i32; 2]>,
    backward_motion: Option<[i32; 2]>,
) -> Result<[i16; 64]> {
    let (plane, block_x, block_y) = block_location(mb_x, mb_y, block);
    let forward_motion = forward_motion.map(|vector| scale_motion_for_plane(vector, plane));
    let backward_motion = backward_motion.map(|vector| scale_motion_for_plane(vector, plane));
    let mut residual = [0_i16; 64];
    for (index, value) in residual.iter_mut().enumerate() {
        let x = block_x + index % 8;
        let y = block_y + index / 8;
        let source_sample = frame_sample(source, plane, x, y, [0, 0]);
        let forward = forward_motion
            .zip(previous)
            .map(|(motion, frame)| frame_sample(frame, plane, x, y, motion));
        let backward = backward_motion
            .zip(next)
            .map(|(motion, frame)| frame_sample(frame, plane, x, y, motion));
        let prediction = match (forward, backward) {
            (Some(a), Some(b)) => {
                u8::try_from((u16::from(a) + u16::from(b)).div_ceil(2)).unwrap_or(255)
            }
            (Some(sample), None) | (None, Some(sample)) => sample,
            (None, None) => {
                return Err(Error::InvalidState(
                    "predicted picture has no reference".into(),
                ));
            }
        };
        *value = i16::from(source_sample) - i16::from(prediction);
    }
    Ok(residual)
}

fn block_location(mb_x: usize, mb_y: usize, block: usize) -> (usize, usize, usize) {
    match block {
        0 => (0, mb_x * 16, mb_y * 16),
        1 => (0, mb_x * 16 + 8, mb_y * 16),
        2 => (0, mb_x * 16, mb_y * 16 + 8),
        3 => (0, mb_x * 16 + 8, mb_y * 16 + 8),
        4 => (1, mb_x * 8, mb_y * 8),
        5 => (2, mb_x * 8, mb_y * 8),
        _ => unreachable!("MPEG-2 4:2:0 has six blocks per macroblock"),
    }
}

fn scale_motion_for_plane(vector: [i32; 2], plane: usize) -> [i32; 2] {
    if plane == 0 {
        vector
    } else {
        [vector[0] / 2, vector[1] / 2]
    }
}

fn frame_sample(
    frame: &VideoFrame,
    plane_index: usize,
    x: usize,
    y: usize,
    motion: [i32; 2],
) -> u8 {
    let plane = &frame.planes[plane_index];
    let base_x = i64::try_from(x).unwrap_or(i64::MAX) + i64::from(motion[0].div_euclid(2));
    let base_y = i64::try_from(y).unwrap_or(i64::MAX) + i64::from(motion[1].div_euclid(2));
    let sample = |sample_x: i64, sample_y: i64| {
        let sample_x = sample_x.clamp(
            0,
            i64::try_from(plane.width.saturating_sub(1)).unwrap_or(i64::MAX),
        );
        let sample_y = sample_y.clamp(
            0,
            i64::try_from(plane.height.saturating_sub(1)).unwrap_or(i64::MAX),
        );
        plane.data[usize::try_from(sample_y).unwrap_or(0) * plane.stride
            + usize::try_from(sample_x).unwrap_or(0)]
    };
    let a = u16::from(sample(base_x, base_y));
    match (motion[0].rem_euclid(2) != 0, motion[1].rem_euclid(2) != 0) {
        (false, false) => u8::try_from(a).unwrap_or(255),
        (true, false) => {
            u8::try_from((a + u16::from(sample(base_x + 1, base_y))).div_ceil(2)).unwrap_or(255)
        }
        (false, true) => {
            u8::try_from((a + u16::from(sample(base_x, base_y + 1))).div_ceil(2)).unwrap_or(255)
        }
        (true, true) => {
            let sum = a
                + u16::from(sample(base_x + 1, base_y))
                + u16::from(sample(base_x, base_y + 1))
                + u16::from(sample(base_x + 1, base_y + 1));
            u8::try_from((sum + 2) / 4).unwrap_or(255)
        }
    }
}

fn decode_last_reference(data: &[u8]) -> Result<VideoFrame> {
    decode_stream(data)?
        .into_iter()
        .filter(|picture| matches!(picture.picture_type, PictureType::I | PictureType::P))
        .max_by_key(|picture| picture.decode_order)
        .map(|picture| picture.frame)
        .ok_or_else(|| Error::InvalidState("encoder produced no MPEG-2 reference picture".into()))
}

fn write_vlc(bits: &mut BitWriter, (code, length): (u16, u8)) -> Result<()> {
    bits.write_bits(u64::from(code), length)
}

fn write_sparse_vlc(bits: &mut BitWriter, table: &[(u16, u8, u16)], symbol: u16) -> Result<()> {
    let &(code, length, _) = table
        .iter()
        .find(|entry| entry.2 == symbol)
        .ok_or_else(|| Error::InvalidState("missing standard macroblock VLC".into()))?;
    write_vlc(bits, (code, length))
}

fn append_start_code(data: &mut Vec<u8>, code: u8) {
    data.extend_from_slice(&[0, 0, 1, code]);
}

fn integer_error<T: std::fmt::Display>(error: T) -> Error {
    Error::InvalidData(format!("integer conversion failed: {error}"))
}
