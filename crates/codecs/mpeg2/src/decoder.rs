//! Portable MPEG-2 Main Profile 4:2:0 reconstruction.

use mmrecode_bitstream::BitReader;
use mmrecode_core::{
    ColorDescription, ColorRange, Error, FieldOrder, FrameTiming, PixelFormat, Plane, Rational,
    Result, Timestamp, VideoFrame,
};

use crate::{
    ChromaFormat, Mpeg2Stream, Picture, PictureStructure, PictureType, analyze_dependencies,
    parse_stream,
    tables::{
        ALTERNATE_SCAN, LEVEL, NON_LINEAR_QUANTISER_SCALE, RUN, ZIGZAG, coded_block_pattern_table,
        dc_chroma_table, dc_luma_table, dct_one_table, dct_zero_table, macroblock_address_table,
        macroblock_b_table, macroblock_p_table, motion_code_table,
    },
    transform::inverse_dct,
};

const MB_INTRA: u16 = 0x01;
const MB_PATTERN: u16 = 0x02;
const MB_FORWARD: u16 = 0x04;
const MB_BACKWARD: u16 = 0x08;
const MB_QUANT: u16 = 0x10;
const ESCAPE_SYMBOL: u16 = 111;
const EOB_SYMBOL: u16 = 112;

/// Broad macroblock coding class exposed for inspection overlays.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MacroblockCoding {
    /// Intra-coded macroblock.
    Intra,
    /// Predicted macroblock with residual data.
    Predicted,
    /// Skipped macroblock using implicit prediction.
    Skipped,
}

/// Motion-prediction organization for one macroblock.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MotionType {
    /// No motion prediction (intra).
    None,
    /// One frame vector for the complete macroblock.
    Frame,
    /// Two field vectors for a frame picture.
    Field,
    /// Dual-prime prediction.
    DualPrime,
}

/// Inspectable macroblock reconstruction metadata.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MacroblockInfo {
    /// Raster address in the coded macroblock grid.
    pub address: usize,
    /// Horizontal macroblock coordinate.
    pub x: usize,
    /// Vertical macroblock coordinate.
    pub y: usize,
    /// Intra, predicted, or skipped coding.
    pub coding: MacroblockCoding,
    /// Quantizer scale after linear/non-linear mapping.
    pub quantiser_scale: u8,
    /// Six-bit coded-block pattern for 4:2:0 video.
    pub coded_block_pattern: u8,
    /// Forward frame motion vector in half-luma-sample units, when present.
    pub forward_motion: Option<[i32; 2]>,
    /// Backward frame motion vector in half-luma-sample units, when present.
    pub backward_motion: Option<[i32; 2]>,
    /// Prediction organization.
    pub motion_type: MotionType,
    /// Whether field DCT organization was signalled.
    pub interlaced_dct: bool,
}

/// Decoded picture with syntax metadata retained for inspection and smart rendering.
#[derive(Clone, Debug)]
pub struct DecodedMpeg2Picture {
    /// Reconstructed visible frame.
    pub frame: VideoFrame,
    /// Coded picture type.
    pub picture_type: PictureType,
    /// Elementary-stream decode order.
    pub decode_order: i64,
    /// Display/presentation order derived from temporal references.
    pub presentation_order: i64,
    /// Reconstructed macroblock metadata.
    pub macroblocks: Vec<MacroblockInfo>,
}

#[derive(Clone, Debug)]
struct FrameBuffer {
    width: usize,
    height: usize,
    stride_y: usize,
    stride_c: usize,
    y: Vec<u8>,
    cb: Vec<u8>,
    cr: Vec<u8>,
}

impl FrameBuffer {
    fn new(width: usize, height: usize) -> Result<Self> {
        let stride_y = width.div_ceil(16) * 16;
        let coded_height = height.div_ceil(16) * 16;
        let stride_c = stride_y / 2;
        let chroma_height = coded_height / 2;
        let luma_len = stride_y
            .checked_mul(coded_height)
            .ok_or_else(|| Error::InvalidData("MPEG-2 luma allocation overflows".into()))?;
        let chroma_len = stride_c
            .checked_mul(chroma_height)
            .ok_or_else(|| Error::InvalidData("MPEG-2 chroma allocation overflows".into()))?;
        Ok(Self {
            width,
            height,
            stride_y,
            stride_c,
            y: vec![0; luma_len],
            cb: vec![0; chroma_len],
            cr: vec![0; chroma_len],
        })
    }

    fn to_video_frame(&self, picture: &Picture, presentation_order: i64) -> Result<VideoFrame> {
        let chroma_width = self.width.div_ceil(2);
        let chroma_height = self.height.div_ceil(2);
        let time_base = Rational::new(
            picture.sequence.frame_rate.denominator(),
            picture.sequence.frame_rate.numerator(),
        )?;
        Ok(VideoFrame {
            format: PixelFormat::Yuv420p8,
            width: self.width,
            height: self.height,
            planes: vec![
                Plane {
                    data: crop_plane(&self.y, self.stride_y, self.width, self.height),
                    stride: self.width,
                    width: self.width,
                    height: self.height,
                },
                Plane {
                    data: crop_plane(&self.cb, self.stride_c, chroma_width, chroma_height),
                    stride: chroma_width,
                    width: chroma_width,
                    height: chroma_height,
                },
                Plane {
                    data: crop_plane(&self.cr, self.stride_c, chroma_width, chroma_height),
                    stride: chroma_width,
                    width: chroma_width,
                    height: chroma_height,
                },
            ],
            timing: FrameTiming {
                pts: Some(Timestamp {
                    value: presentation_order,
                    time_base,
                }),
                duration: Some(Timestamp {
                    value: 1,
                    time_base,
                }),
            },
            color: sequence_color_description(&picture.sequence),
            field_order: if picture.coding_extension.progressive_frame {
                FieldOrder::Progressive
            } else if picture.coding_extension.top_field_first {
                FieldOrder::TopFirst
            } else {
                FieldOrder::BottomFirst
            },
        })
    }
}

fn sequence_color_description(sequence: &crate::SequenceParameters) -> ColorDescription {
    let colour = sequence
        .display
        .and_then(|display| display.colour_description);
    ColorDescription {
        range: ColorRange::Limited,
        primaries: colour.map(|value| colour_primaries_name(value.colour_primaries)),
        transfer: colour.map(|value| transfer_name(value.transfer_characteristics)),
        matrix: colour.map(|value| matrix_name(value.matrix_coefficients)),
    }
}

fn colour_primaries_name(code: u8) -> String {
    match code {
        1 => "BT.709".into(),
        4 => "BT.470 System M".into(),
        5 => "BT.470 System B/G".into(),
        6 => "SMPTE 170M".into(),
        7 => "SMPTE 240M".into(),
        8 => "Generic film".into(),
        _ => format!("MPEG-2 colour-primaries code {code}"),
    }
}

fn transfer_name(code: u8) -> String {
    match code {
        1 => "BT.709".into(),
        4 => "Gamma 2.2".into(),
        5 => "Gamma 2.8".into(),
        6 => "SMPTE 170M".into(),
        7 => "SMPTE 240M".into(),
        8 => "Linear".into(),
        _ => format!("MPEG-2 transfer-characteristics code {code}"),
    }
}

fn matrix_name(code: u8) -> String {
    match code {
        1 => "BT.709".into(),
        4 => "FCC".into(),
        5 => "BT.470 System B/G".into(),
        6 => "SMPTE 170M".into(),
        7 => "SMPTE 240M".into(),
        _ => format!("MPEG-2 matrix-coefficients code {code}"),
    }
}

#[derive(Clone, Copy, Debug, Default)]
struct MotionPrediction {
    forward: Option<[i32; 2]>,
    backward: Option<[i32; 2]>,
    forward_fields: Option<[FieldMotion; 2]>,
    backward_fields: Option<[FieldMotion; 2]>,
    motion_type: Option<MotionType>,
}

#[derive(Clone, Copy, Debug, Default)]
struct FieldMotion {
    vector: [i32; 2],
    field_select: usize,
}

/// Picture-at-a-time MPEG-2 reconstruction state.
///
/// The decoder retains only the two reference pictures needed by MPEG-2 prediction. Callers can
/// therefore drive reconstruction from an indexed elementary stream without retaining every
/// decoded frame. Start a fresh instance at a clean intra-coded random-access picture when
/// seeking.
#[derive(Clone, Debug, Default)]
pub struct Mpeg2PictureDecoder {
    older_reference: Option<FrameBuffer>,
    newer_reference: Option<FrameBuffer>,
}

impl Mpeg2PictureDecoder {
    /// Reconstructs one parsed picture in elementary-stream decode order.
    ///
    /// `decode_order` and `presentation_order` come from dependency analysis. The caller must feed
    /// pictures in decode order, beginning with an independently decodable I picture.
    ///
    /// # Errors
    ///
    /// Returns an error for unsupported picture organization, malformed coded data, or missing
    /// prediction references.
    pub fn decode_picture(
        &mut self,
        data: &[u8],
        picture: &Picture,
        decode_order: i64,
        presentation_order: i64,
    ) -> Result<DecodedMpeg2Picture> {
        if picture.sequence.chroma_format != ChromaFormat::Yuv420 {
            return Err(Error::Unsupported(
                "MPEG-2 decoder currently supports Main Profile 4:2:0 video".into(),
            ));
        }
        if picture.coding_extension.picture_structure != PictureStructure::Frame {
            return Err(Error::Unsupported(
                "MPEG-2 field pictures are not yet reconstructed".into(),
            ));
        }
        let mut buffer = FrameBuffer::new(picture.sequence.width, picture.sequence.height)?;
        let macroblocks = decode_picture_data(
            data,
            picture,
            &mut buffer,
            self.older_reference.as_ref(),
            self.newer_reference.as_ref(),
        )?;
        let frame = buffer.to_video_frame(picture, presentation_order)?;
        let decoded = DecodedMpeg2Picture {
            frame,
            picture_type: picture.header.picture_coding_type,
            decode_order,
            presentation_order,
            macroblocks,
        };
        if matches!(
            picture.header.picture_coding_type,
            PictureType::I | PictureType::P
        ) {
            self.older_reference = self.newer_reference.take();
            self.newer_reference = Some(buffer);
        }
        Ok(decoded)
    }
}

/// Decodes every picture in a Main Profile 4:2:0 MPEG-2 elementary stream.
///
/// Returned pictures are in presentation order even though reconstruction occurs in coded decode
/// order. The portable path supports frame pictures with frame or field prediction; field-picture
/// structures and dual-prime prediction report a precise unsupported error rather than misdecoding.
///
/// # Errors
///
/// Returns an error for malformed VLC/transform syntax, missing references, unsupported chroma or
/// picture structures, duplicate macroblocks, or incomplete prediction modes.
pub fn decode_stream(data: &[u8]) -> Result<Vec<DecodedMpeg2Picture>> {
    let stream = parse_stream(data)?;
    decode_parsed_stream(&stream)
}

fn decode_parsed_stream(stream: &Mpeg2Stream<'_>) -> Result<Vec<DecodedMpeg2Picture>> {
    let dependencies = analyze_dependencies(stream)?;
    let mut decoder = Mpeg2PictureDecoder::default();
    let mut pictures = Vec::with_capacity(stream.pictures().len());

    for (picture, access) in stream.pictures().iter().zip(&dependencies) {
        pictures.push(decoder.decode_picture(
            stream.data(),
            picture,
            access.decode_order,
            access.presentation_order,
        )?);
    }
    pictures.sort_by_key(|picture| picture.presentation_order);
    Ok(pictures)
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn decode_picture_data(
    data: &[u8],
    picture: &Picture,
    destination: &mut FrameBuffer,
    older_reference: Option<&FrameBuffer>,
    newer_reference: Option<&FrameBuffer>,
) -> Result<Vec<MacroblockInfo>> {
    let mb_width = picture.sequence.width.div_ceil(16);
    let mb_height = picture.sequence.height.div_ceil(16);
    let mut decoded_map = vec![false; mb_width * mb_height];
    let mut macroblocks = Vec::with_capacity(decoded_map.len());

    for slice in &picture.slices {
        let mut bits = BitReader::new(&data[slice.payload_range.clone()]);
        if picture.sequence.height > 2_800 {
            bits.skip_bits(3)?;
        }
        let mut qscale = decode_quantiser_scale(
            read_bits_u8(&mut bits, 5, "slice quantiser_scale_code")?,
            picture.coding_extension.q_scale_type,
        )?;
        skip_extra_information(&mut bits, "slice")?;
        let slice_row = usize::from(slice.vertical_position)
            .checked_sub(1)
            .ok_or_else(|| Error::InvalidData("zero MPEG-2 slice row".into()))?;
        let slice_base = slice_row
            .checked_mul(mb_width)
            .ok_or_else(|| Error::InvalidData("MPEG-2 slice address overflows".into()))?;
        let first_increment = decode_macroblock_increment(&mut bits)?;
        let mut address = slice_base
            .checked_add(first_increment)
            .and_then(|value| value.checked_sub(1))
            .ok_or_else(|| Error::InvalidData("MPEG-2 macroblock address overflows".into()))?;

        let mut dc_predictor = [128_i32 << picture.coding_extension.intra_dc_precision; 3];
        let mut motion_predictor = [[[0_i32; 2]; 2]; 2];
        let mut previous_b_motion = MotionPrediction::default();
        loop {
            if address >= decoded_map.len() {
                return Err(Error::InvalidData(format!(
                    "macroblock address {address} exceeds {mb_width}x{mb_height} picture"
                )));
            }
            if decoded_map[address] {
                return Err(Error::InvalidData(format!(
                    "duplicate MPEG-2 macroblock address {address}"
                )));
            }
            let mb_x = address % mb_width;
            let mb_y = address / mb_width;
            let mb_type = decode_macroblock_type(&mut bits, picture.header.picture_coding_type)?;
            let intra = mb_type & MB_INTRA != 0;
            let mut interlaced_dct = false;
            let mut motion = MotionPrediction::default();

            if intra {
                if !picture.coding_extension.frame_pred_frame_dct {
                    interlaced_dct = bits.read_bit()?;
                }
                if mb_type & MB_QUANT != 0 {
                    qscale = decode_quantiser_scale(
                        read_bits_u8(&mut bits, 5, "macroblock_quant")?,
                        picture.coding_extension.q_scale_type,
                    )?;
                }
                motion_predictor = [[[0; 2]; 2]; 2];
                for block in 0..6 {
                    let coefficients =
                        decode_intra_block(&mut bits, block, &mut dc_predictor, qscale, picture)?;
                    put_block(
                        destination,
                        mb_x,
                        mb_y,
                        block,
                        &coefficients,
                        true,
                        interlaced_dct,
                    );
                }
            } else {
                dc_predictor = [128_i32 << picture.coding_extension.intra_dc_precision; 3];
                let has_motion = mb_type & (MB_FORWARD | MB_BACKWARD) != 0;
                if has_motion {
                    let motion_type = if picture.coding_extension.frame_pred_frame_dct {
                        MotionType::Frame
                    } else {
                        match read_bits_u8(&mut bits, 2, "frame_motion_type")? {
                            1 => MotionType::Field,
                            2 => MotionType::Frame,
                            3 => MotionType::DualPrime,
                            _ => {
                                return Err(Error::InvalidData(
                                    "zero MPEG-2 frame_motion_type".into(),
                                ));
                            }
                        }
                    };
                    if motion_type == MotionType::DualPrime {
                        return Err(Error::Unsupported(format!(
                            "MPEG-2 {motion_type:?} prediction at macroblock {address}"
                        )));
                    }
                    motion.motion_type = Some(motion_type);
                } else if picture.header.picture_coding_type == PictureType::P
                    && mb_type & MB_PATTERN != 0
                {
                    motion.motion_type = Some(MotionType::Frame);
                    motion.forward = Some([0, 0]);
                    motion_predictor[0] = [[0; 2]; 2];
                }
                if !picture.coding_extension.frame_pred_frame_dct && mb_type & MB_PATTERN != 0 {
                    interlaced_dct = bits.read_bit()?;
                }
                if mb_type & MB_QUANT != 0 {
                    qscale = decode_quantiser_scale(
                        read_bits_u8(&mut bits, 5, "macroblock_quant")?,
                        picture.coding_extension.q_scale_type,
                    )?;
                }
                if mb_type & MB_FORWARD != 0 {
                    if motion.motion_type == Some(MotionType::Field) {
                        let fields = decode_field_motion(
                            &mut bits,
                            &mut motion_predictor[0],
                            picture.coding_extension.f_code[0],
                        )?;
                        motion.forward = Some(fields[0].vector);
                        motion.forward_fields = Some(fields);
                    } else {
                        let vector = decode_frame_motion(
                            &mut bits,
                            &mut motion_predictor[0],
                            picture.coding_extension.f_code[0],
                        )?;
                        motion.forward = Some(vector);
                    }
                }
                if mb_type & MB_BACKWARD != 0 {
                    if motion.motion_type == Some(MotionType::Field) {
                        let fields = decode_field_motion(
                            &mut bits,
                            &mut motion_predictor[1],
                            picture.coding_extension.f_code[1],
                        )?;
                        motion.backward = Some(fields[0].vector);
                        motion.backward_fields = Some(fields);
                    } else {
                        let vector = decode_frame_motion(
                            &mut bits,
                            &mut motion_predictor[1],
                            picture.coding_extension.f_code[1],
                        )?;
                        motion.backward = Some(vector);
                    }
                }
                predict_macroblock(
                    destination,
                    mb_x,
                    mb_y,
                    motion,
                    older_reference,
                    newer_reference,
                    picture.header.picture_coding_type,
                )?;
                let cbp = if mb_type & MB_PATTERN != 0 {
                    u8::try_from(coded_block_pattern_table().decode(&mut bits)?)
                        .map_err(|_| Error::InvalidData("coded block pattern exceeds u8".into()))?
                } else {
                    0
                };
                for block in 0..6 {
                    if cbp & (1 << (5 - block)) != 0 {
                        let coefficients =
                            decode_non_intra_block(&mut bits, block, qscale, picture)?;
                        put_block(
                            destination,
                            mb_x,
                            mb_y,
                            block,
                            &coefficients,
                            false,
                            interlaced_dct,
                        );
                    }
                }
                previous_b_motion = motion;
                decoded_map[address] = true;
                macroblocks.push(MacroblockInfo {
                    address,
                    x: mb_x,
                    y: mb_y,
                    coding: MacroblockCoding::Predicted,
                    quantiser_scale: u8::try_from(qscale).unwrap_or(u8::MAX),
                    coded_block_pattern: cbp,
                    forward_motion: motion.forward,
                    backward_motion: motion.backward,
                    motion_type: motion.motion_type.unwrap_or(MotionType::Frame),
                    interlaced_dct,
                });
                if remaining_bits_are_zero(&bits) {
                    break;
                }
                let increment = decode_macroblock_increment(&mut bits)?;
                reconstruct_skips(
                    destination,
                    &mut decoded_map,
                    &mut macroblocks,
                    address,
                    increment,
                    mb_width,
                    picture,
                    older_reference,
                    newer_reference,
                    &mut motion_predictor,
                    previous_b_motion,
                    qscale,
                    &mut dc_predictor,
                )?;
                address += increment;
                continue;
            }

            decoded_map[address] = true;
            macroblocks.push(MacroblockInfo {
                address,
                x: mb_x,
                y: mb_y,
                coding: MacroblockCoding::Intra,
                quantiser_scale: u8::try_from(qscale).unwrap_or(u8::MAX),
                coded_block_pattern: 0x3f,
                forward_motion: None,
                backward_motion: None,
                motion_type: MotionType::None,
                interlaced_dct,
            });
            if remaining_bits_are_zero(&bits) {
                break;
            }
            let increment = decode_macroblock_increment(&mut bits)?;
            reconstruct_skips(
                destination,
                &mut decoded_map,
                &mut macroblocks,
                address,
                increment,
                mb_width,
                picture,
                older_reference,
                newer_reference,
                &mut motion_predictor,
                previous_b_motion,
                qscale,
                &mut dc_predictor,
            )?;
            address += increment;
        }
    }
    if decoded_map.iter().any(|decoded| !decoded) {
        return Err(Error::InvalidData(format!(
            "MPEG-2 picture contains {} undecoded macroblocks",
            decoded_map.iter().filter(|decoded| !**decoded).count()
        )));
    }
    macroblocks.sort_by_key(|info| info.address);
    Ok(macroblocks)
}

#[allow(clippy::too_many_arguments)]
fn reconstruct_skips(
    destination: &mut FrameBuffer,
    decoded_map: &mut [bool],
    macroblocks: &mut Vec<MacroblockInfo>,
    current_address: usize,
    increment: usize,
    mb_width: usize,
    picture: &Picture,
    older_reference: Option<&FrameBuffer>,
    newer_reference: Option<&FrameBuffer>,
    motion_predictor: &mut [[[i32; 2]; 2]; 2],
    previous_b_motion: MotionPrediction,
    qscale: i32,
    dc_predictor: &mut [i32; 3],
) -> Result<()> {
    for address in current_address + 1..current_address + increment {
        if address >= decoded_map.len() || decoded_map[address] {
            return Err(Error::InvalidData(
                "invalid skipped macroblock address".into(),
            ));
        }
        let motion = match picture.header.picture_coding_type {
            PictureType::P => {
                *motion_predictor = [[[0; 2]; 2]; 2];
                MotionPrediction {
                    forward: Some([0, 0]),
                    backward: None,
                    forward_fields: None,
                    backward_fields: None,
                    motion_type: Some(MotionType::Frame),
                }
            }
            PictureType::B => previous_b_motion,
            _ => {
                return Err(Error::InvalidData(
                    "skipped macroblock in intra picture".into(),
                ));
            }
        };
        *dc_predictor = [128_i32 << picture.coding_extension.intra_dc_precision; 3];
        if motion.forward.is_none() && motion.backward.is_none() {
            return Err(Error::InvalidData(
                "skipped B macroblock has no previous prediction mode".into(),
            ));
        }
        let x = address % mb_width;
        let y = address / mb_width;
        predict_macroblock(
            destination,
            x,
            y,
            motion,
            older_reference,
            newer_reference,
            picture.header.picture_coding_type,
        )?;
        decoded_map[address] = true;
        macroblocks.push(MacroblockInfo {
            address,
            x,
            y,
            coding: MacroblockCoding::Skipped,
            quantiser_scale: u8::try_from(qscale).unwrap_or(u8::MAX),
            coded_block_pattern: 0,
            forward_motion: motion.forward,
            backward_motion: motion.backward,
            motion_type: MotionType::Frame,
            interlaced_dct: false,
        });
    }
    Ok(())
}

fn decode_macroblock_type(bits: &mut BitReader<'_>, picture_type: PictureType) -> Result<u16> {
    match picture_type {
        PictureType::I => {
            let first = bits.read_bit()?;
            if first {
                Ok(MB_INTRA)
            } else if bits.read_bit()? {
                Ok(MB_INTRA | MB_QUANT)
            } else {
                Err(Error::InvalidData(
                    "invalid I-picture macroblock type".into(),
                ))
            }
        }
        PictureType::P => macroblock_p_table().decode(bits),
        PictureType::B => macroblock_b_table().decode(bits),
        PictureType::D | PictureType::Reserved(_) => Err(Error::Unsupported(
            "MPEG-1 D picture macroblocks are unsupported".into(),
        )),
    }
}

fn decode_macroblock_increment(bits: &mut BitReader<'_>) -> Result<usize> {
    let mut increment = 0_usize;
    loop {
        let symbol = macroblock_address_table().decode(bits)?;
        match symbol {
            0..=32 => return Ok(increment + usize::from(symbol) + 1),
            33 => {
                increment = increment.checked_add(33).ok_or_else(|| {
                    Error::InvalidData("macroblock address increment overflow".into())
                })?;
            }
            34 => {}
            35 => return Err(Error::InvalidData("unexpected slice-end code".into())),
            _ => return Err(Error::InvalidData("invalid macroblock increment".into())),
        }
    }
}

fn decode_quantiser_scale(code: u8, non_linear: bool) -> Result<i32> {
    if code == 0 {
        return Err(Error::InvalidData(
            "zero MPEG-2 quantiser_scale_code".into(),
        ));
    }
    Ok(if non_linear {
        NON_LINEAR_QUANTISER_SCALE[usize::from(code)]
    } else {
        i32::from(code) * 2
    })
}

fn decode_intra_block(
    bits: &mut BitReader<'_>,
    block: usize,
    dc_predictor: &mut [i32; 3],
    qscale: i32,
    picture: &Picture,
) -> Result<[i32; 64]> {
    let component = if block < 4 { 0 } else { block - 3 };
    let size = if component == 0 {
        dc_luma_table().decode(bits)?
    } else {
        dc_chroma_table().decode(bits)?
    };
    let differential = receive_signed(
        bits,
        u8::try_from(size)
            .map_err(|_| Error::InvalidData("MPEG-2 DC size exceeds eight bits".into()))?,
    )?;
    dc_predictor[component] += differential;
    let mut coefficients = [0_i32; 64];
    coefficients[0] = dc_predictor[component] << (3 - picture.coding_extension.intra_dc_precision);
    let mut mismatch = coefficients[0] ^ 1;
    let scan = if picture.coding_extension.alternate_scan {
        &ALTERNATE_SCAN
    } else {
        &ZIGZAG
    };
    let table = if picture.coding_extension.intra_vlc_format {
        dct_one_table()
    } else {
        dct_zero_table()
    };
    let matrix = if component == 0 {
        &picture.sequence.intra_quantizer_matrix
    } else {
        &picture.sequence.chroma_intra_quantizer_matrix
    };
    let mut scan_position = 0_usize;
    loop {
        let symbol = table.decode(bits)?;
        if symbol == EOB_SYMBOL {
            break;
        }
        let (run, signed_level) = decode_run_level(bits, symbol)?;
        scan_position = scan_position
            .checked_add(run + 1)
            .ok_or_else(|| Error::InvalidData("intra coefficient index overflow".into()))?;
        if scan_position >= 64 {
            return Err(Error::InvalidData("intra coefficient exceeds block".into()));
        }
        let position = scan[scan_position];
        let magnitude = signed_level.abs() * qscale * i32::from(matrix[position]) / 16;
        let value = magnitude.copysign(signed_level).clamp(-2_048, 2_047);
        coefficients[position] = value;
        mismatch ^= value;
    }
    coefficients[63] ^= mismatch & 1;
    Ok(coefficients)
}

fn decode_non_intra_block(
    bits: &mut BitReader<'_>,
    block: usize,
    qscale: i32,
    picture: &Picture,
) -> Result<[i32; 64]> {
    let scan = if picture.coding_extension.alternate_scan {
        &ALTERNATE_SCAN
    } else {
        &ZIGZAG
    };
    let mut coefficients = [0_i32; 64];
    let matrix = if block < 4 {
        &picture.sequence.non_intra_quantizer_matrix
    } else {
        &picture.sequence.chroma_non_intra_quantizer_matrix
    };
    let mut mismatch = 1_i32;
    let mut scan_position: isize = -1;
    if bits.peek_bits(1)? == 1 {
        bits.skip_bits(1)?;
        let sign = bits.read_bit()?;
        let mut value = 3 * qscale * i32::from(matrix[0]) / 32;
        if sign {
            value = -value;
        }
        coefficients[0] = value;
        mismatch ^= value;
        scan_position = 0;
    }
    loop {
        let symbol = dct_zero_table().decode(bits)?;
        if symbol == EOB_SYMBOL {
            break;
        }
        let (run, signed_level) = decode_run_level(bits, symbol)?;
        scan_position += isize::try_from(run + 1)
            .map_err(|_| Error::InvalidData("coefficient run exceeds isize".into()))?;
        if !(0..64).contains(&scan_position) {
            return Err(Error::InvalidData(
                "non-intra coefficient exceeds block".into(),
            ));
        }
        let position = scan[usize::try_from(scan_position)
            .map_err(|_| Error::InvalidData("negative coefficient position".into()))?];
        let magnitude = (2 * signed_level.abs() + 1) * qscale * i32::from(matrix[position]) / 32;
        let value = magnitude.copysign(signed_level).clamp(-2_048, 2_047);
        coefficients[position] = value;
        mismatch ^= value;
    }
    coefficients[63] ^= mismatch & 1;
    Ok(coefficients)
}

fn decode_run_level(bits: &mut BitReader<'_>, symbol: u16) -> Result<(usize, i32)> {
    if symbol == ESCAPE_SYMBOL {
        let run = usize::from(read_bits_u8(bits, 6, "escape run")?);
        let raw = i32::try_from(bits.read_bits(12)?)
            .map_err(|_| Error::InvalidData("escaped coefficient exceeds i32".into()))?;
        let level = if raw & 0x800 != 0 { raw - 0x1000 } else { raw };
        if level == 0 {
            return Err(Error::InvalidData("zero escaped MPEG-2 coefficient".into()));
        }
        return Ok((run, level));
    }
    let index = usize::from(symbol);
    let run = usize::from(RUN[index]);
    let level = i32::from(LEVEL[index]);
    Ok((run, if bits.read_bit()? { -level } else { level }))
}

fn receive_signed(bits: &mut BitReader<'_>, size: u8) -> Result<i32> {
    if size == 0 {
        return Ok(0);
    }
    let value = i32::try_from(bits.read_bits(size)?)
        .map_err(|_| Error::InvalidData("DC differential exceeds i32".into()))?;
    let threshold = 1_i32 << (size - 1);
    Ok(if value < threshold {
        value + 1 - (1_i32 << size)
    } else {
        value
    })
}

fn decode_frame_motion(
    bits: &mut BitReader<'_>,
    predictor: &mut [[i32; 2]; 2],
    f_code: [u8; 2],
) -> Result<[i32; 2]> {
    let x = decode_motion_component(bits, f_code[0], predictor[0][0])?;
    let y = decode_motion_component(bits, f_code[1], predictor[0][1])?;
    predictor[0] = [x, y];
    predictor[1] = [x, y];
    Ok([x, y])
}

fn decode_field_motion(
    bits: &mut BitReader<'_>,
    predictor: &mut [[i32; 2]; 2],
    f_code: [u8; 2],
) -> Result<[FieldMotion; 2]> {
    let mut fields = [FieldMotion::default(); 2];
    for field in 0..2 {
        let field_select = usize::from(bits.read_bit()?);
        let x = decode_motion_component(bits, f_code[0], predictor[field][0])?;
        let y = decode_motion_component(bits, f_code[1], predictor[field][1] >> 1)?;
        predictor[field] = [x, y * 2];
        fields[field] = FieldMotion {
            vector: [x, y],
            field_select,
        };
    }
    Ok(fields)
}

fn decode_motion_component(bits: &mut BitReader<'_>, f_code: u8, predictor: i32) -> Result<i32> {
    if f_code == 0 || f_code > 7 {
        return Err(Error::InvalidData(format!(
            "invalid MPEG-2 f_code {f_code}"
        )));
    }
    let code = i32::from(motion_code_table().decode(bits)?);
    if code == 0 {
        return Ok(predictor);
    }
    let negative = bits.read_bit()?;
    let shift = f_code - 1;
    let residual = if shift == 0 {
        0
    } else {
        i32::try_from(bits.read_bits(shift)?)
            .map_err(|_| Error::InvalidData("motion residual exceeds i32".into()))?
    };
    let mut delta = ((code - 1) << shift) + residual + 1;
    if negative {
        delta = -delta;
    }
    let value = predictor + delta;
    let bits = 5 + shift;
    let modulus = 1_i32 << bits;
    let sign = 1_i32 << (bits - 1);
    let wrapped = value & (modulus - 1);
    Ok(if wrapped & sign != 0 {
        wrapped - modulus
    } else {
        wrapped
    })
}

fn predict_macroblock(
    destination: &mut FrameBuffer,
    mb_x: usize,
    mb_y: usize,
    motion: MotionPrediction,
    older_reference: Option<&FrameBuffer>,
    newer_reference: Option<&FrameBuffer>,
    picture_type: PictureType,
) -> Result<()> {
    let (forward_reference, backward_reference) = match picture_type {
        PictureType::P => (newer_reference, None),
        PictureType::B => (older_reference, newer_reference),
        _ => (None, None),
    };
    if motion.forward.is_some() && forward_reference.is_none() {
        return Err(Error::InvalidData(
            "missing forward MPEG-2 reference".into(),
        ));
    }
    if motion.backward.is_some() && backward_reference.is_none() {
        return Err(Error::InvalidData(
            "missing backward MPEG-2 reference".into(),
        ));
    }
    if motion.motion_type == Some(MotionType::Field) {
        predict_field_plane(
            &mut destination.y,
            destination.stride_y,
            mb_x * 16,
            mb_y * 16,
            16,
            16,
            motion.forward_fields,
            motion.backward_fields,
            forward_reference.map(|frame| (&frame.y[..], frame.stride_y)),
            backward_reference.map(|frame| (&frame.y[..], frame.stride_y)),
        );
        predict_field_plane(
            &mut destination.cb,
            destination.stride_c,
            mb_x * 8,
            mb_y * 8,
            8,
            8,
            motion.forward_fields.map(chroma_field_motion),
            motion.backward_fields.map(chroma_field_motion),
            forward_reference.map(|frame| (&frame.cb[..], frame.stride_c)),
            backward_reference.map(|frame| (&frame.cb[..], frame.stride_c)),
        );
        predict_field_plane(
            &mut destination.cr,
            destination.stride_c,
            mb_x * 8,
            mb_y * 8,
            8,
            8,
            motion.forward_fields.map(chroma_field_motion),
            motion.backward_fields.map(chroma_field_motion),
            forward_reference.map(|frame| (&frame.cr[..], frame.stride_c)),
            backward_reference.map(|frame| (&frame.cr[..], frame.stride_c)),
        );
    } else {
        predict_plane(
            &mut destination.y,
            destination.stride_y,
            mb_x * 16,
            mb_y * 16,
            16,
            16,
            motion.forward,
            motion.backward,
            forward_reference.map(|frame| (&frame.y[..], frame.stride_y)),
            backward_reference.map(|frame| (&frame.y[..], frame.stride_y)),
        );
        let chroma_forward = motion.forward.map(chroma_motion);
        let chroma_backward = motion.backward.map(chroma_motion);
        predict_plane(
            &mut destination.cb,
            destination.stride_c,
            mb_x * 8,
            mb_y * 8,
            8,
            8,
            chroma_forward,
            chroma_backward,
            forward_reference.map(|frame| (&frame.cb[..], frame.stride_c)),
            backward_reference.map(|frame| (&frame.cb[..], frame.stride_c)),
        );
        predict_plane(
            &mut destination.cr,
            destination.stride_c,
            mb_x * 8,
            mb_y * 8,
            8,
            8,
            chroma_forward,
            chroma_backward,
            forward_reference.map(|frame| (&frame.cr[..], frame.stride_c)),
            backward_reference.map(|frame| (&frame.cr[..], frame.stride_c)),
        );
    }
    Ok(())
}

#[allow(
    clippy::cast_possible_truncation,
    clippy::many_single_char_names,
    clippy::too_many_arguments
)]
fn predict_field_plane(
    destination: &mut [u8],
    destination_stride: usize,
    x: usize,
    y: usize,
    width: usize,
    height: usize,
    forward_motion: Option<[FieldMotion; 2]>,
    backward_motion: Option<[FieldMotion; 2]>,
    forward: Option<(&[u8], usize)>,
    backward: Option<(&[u8], usize)>,
) {
    for field in 0..2 {
        for field_row in 0..height / 2 {
            let physical_y = y + field + field_row * 2;
            for column in 0..width {
                let forward_sample = forward_motion.zip(forward).map(|(motions, reference)| {
                    halfpel_field_sample(
                        reference.0,
                        reference.1,
                        x + column,
                        physical_y,
                        motions[field],
                    )
                });
                let backward_sample = backward_motion.zip(backward).map(|(motions, reference)| {
                    halfpel_field_sample(
                        reference.0,
                        reference.1,
                        x + column,
                        physical_y,
                        motions[field],
                    )
                });
                let sample = match (forward_sample, backward_sample) {
                    (Some(a), Some(b)) => (u16::from(a) + u16::from(b)).div_ceil(2) as u8,
                    (Some(value), None) | (None, Some(value)) => value,
                    (None, None) => 0,
                };
                destination[physical_y * destination_stride + x + column] = sample;
            }
        }
    }
}

#[allow(clippy::cast_possible_truncation, clippy::many_single_char_names)]
fn halfpel_field_sample(
    reference: &[u8],
    stride: usize,
    x: usize,
    destination_y: usize,
    motion: FieldMotion,
) -> u8 {
    let base_x = i64::try_from(x).unwrap_or(i64::MAX) + i64::from(motion.vector[0].div_euclid(2));
    let destination_field_y = destination_y / 2;
    let base_field_y = i64::try_from(destination_field_y).unwrap_or(i64::MAX)
        + i64::from(motion.vector[1].div_euclid(2));
    let fractional_x = motion.vector[0].rem_euclid(2) != 0;
    let fractional_y = motion.vector[1].rem_euclid(2) != 0;
    let sample = |sample_x: i64, field_y: i64| {
        let physical_y = field_y * 2 + i64::try_from(motion.field_select).unwrap_or(0);
        clamped_field_sample(reference, stride, sample_x, physical_y, motion.field_select)
    };
    let a = u16::from(sample(base_x, base_field_y));
    match (fractional_x, fractional_y) {
        (false, false) => a as u8,
        (true, false) => (a + u16::from(sample(base_x + 1, base_field_y))).div_ceil(2) as u8,
        (false, true) => (a + u16::from(sample(base_x, base_field_y + 1))).div_ceil(2) as u8,
        (true, true) => {
            let b = u16::from(sample(base_x + 1, base_field_y));
            let c = u16::from(sample(base_x, base_field_y + 1));
            let d = u16::from(sample(base_x + 1, base_field_y + 1));
            ((a + b + c + d + 2) / 4) as u8
        }
    }
}

fn clamped_field_sample(
    reference: &[u8],
    stride: usize,
    x: i64,
    y: i64,
    field_select: usize,
) -> u8 {
    let height = reference.len() / stride;
    let max_field_row = height.saturating_sub(1 + field_select) / 2;
    let field_row = (y - i64::try_from(field_select).unwrap_or(0)).div_euclid(2);
    let clamped_field_row = field_row.clamp(0, i64::try_from(max_field_row).unwrap_or(i64::MAX));
    let physical_y = usize::try_from(clamped_field_row).unwrap_or(0) * 2 + field_select;
    let clamped_x = x.clamp(
        0,
        i64::try_from(stride.saturating_sub(1)).unwrap_or(i64::MAX),
    );
    reference[physical_y * stride + usize::try_from(clamped_x).unwrap_or(0)]
}

#[allow(clippy::cast_possible_truncation, clippy::too_many_arguments)]
fn predict_plane(
    destination: &mut [u8],
    destination_stride: usize,
    x: usize,
    y: usize,
    width: usize,
    height: usize,
    forward_motion: Option<[i32; 2]>,
    backward_motion: Option<[i32; 2]>,
    forward: Option<(&[u8], usize)>,
    backward: Option<(&[u8], usize)>,
) {
    for row in 0..height {
        for column in 0..width {
            let forward_sample = forward_motion.zip(forward).map(|(motion, reference)| {
                halfpel_sample(reference.0, reference.1, x + column, y + row, motion)
            });
            let backward_sample = backward_motion.zip(backward).map(|(motion, reference)| {
                halfpel_sample(reference.0, reference.1, x + column, y + row, motion)
            });
            let sample = match (forward_sample, backward_sample) {
                (Some(a), Some(b)) => (u16::from(a) + u16::from(b)).div_ceil(2) as u8,
                (Some(value), None) | (None, Some(value)) => value,
                (None, None) => 0,
            };
            destination[(y + row) * destination_stride + x + column] = sample;
        }
    }
}

#[allow(clippy::cast_possible_truncation, clippy::many_single_char_names)]
fn halfpel_sample(reference: &[u8], stride: usize, x: usize, y: usize, motion: [i32; 2]) -> u8 {
    let base_x = i64::try_from(x).unwrap_or(i64::MAX) + i64::from(motion[0].div_euclid(2));
    let base_y = i64::try_from(y).unwrap_or(i64::MAX) + i64::from(motion[1].div_euclid(2));
    let fractional_x = motion[0].rem_euclid(2) != 0;
    let fractional_y = motion[1].rem_euclid(2) != 0;
    let a = u16::from(clamped_sample(reference, stride, base_x, base_y));
    match (fractional_x, fractional_y) {
        (false, false) => a as u8,
        (true, false) => {
            let b = u16::from(clamped_sample(reference, stride, base_x + 1, base_y));
            (a + b).div_ceil(2) as u8
        }
        (false, true) => {
            let c = u16::from(clamped_sample(reference, stride, base_x, base_y + 1));
            (a + c).div_ceil(2) as u8
        }
        (true, true) => {
            let b = u16::from(clamped_sample(reference, stride, base_x + 1, base_y));
            let c = u16::from(clamped_sample(reference, stride, base_x, base_y + 1));
            let d = u16::from(clamped_sample(reference, stride, base_x + 1, base_y + 1));
            ((a + b + c + d + 2) / 4) as u8
        }
    }
}

fn clamped_sample(reference: &[u8], stride: usize, x: i64, y: i64) -> u8 {
    let height = reference.len() / stride;
    let clamped_x = x.clamp(
        0,
        i64::try_from(stride.saturating_sub(1)).unwrap_or(i64::MAX),
    );
    let clamped_y = y.clamp(
        0,
        i64::try_from(height.saturating_sub(1)).unwrap_or(i64::MAX),
    );
    reference
        [usize::try_from(clamped_y).unwrap_or(0) * stride + usize::try_from(clamped_x).unwrap_or(0)]
}

fn chroma_motion(vector: [i32; 2]) -> [i32; 2] {
    [vector[0] / 2, vector[1] / 2]
}

fn chroma_field_motion(fields: [FieldMotion; 2]) -> [FieldMotion; 2] {
    fields.map(|field| FieldMotion {
        vector: chroma_motion(field.vector),
        field_select: field.field_select,
    })
}

fn put_block(
    destination: &mut FrameBuffer,
    mb_x: usize,
    mb_y: usize,
    block: usize,
    coefficients: &[i32; 64],
    intra: bool,
    interlaced_dct: bool,
) {
    let samples = inverse_dct(coefficients);
    let (plane, stride, x, y, line_step) = match block {
        0 => (
            &mut destination.y,
            destination.stride_y,
            mb_x * 16,
            mb_y * 16,
            if interlaced_dct { 2 } else { 1 },
        ),
        1 => (
            &mut destination.y,
            destination.stride_y,
            mb_x * 16 + 8,
            mb_y * 16,
            if interlaced_dct { 2 } else { 1 },
        ),
        2 => (
            &mut destination.y,
            destination.stride_y,
            mb_x * 16,
            mb_y * 16 + if interlaced_dct { 1 } else { 8 },
            if interlaced_dct { 2 } else { 1 },
        ),
        3 => (
            &mut destination.y,
            destination.stride_y,
            mb_x * 16 + 8,
            mb_y * 16 + if interlaced_dct { 1 } else { 8 },
            if interlaced_dct { 2 } else { 1 },
        ),
        4 => (
            &mut destination.cb,
            destination.stride_c,
            mb_x * 8,
            mb_y * 8,
            1,
        ),
        5 => (
            &mut destination.cr,
            destination.stride_c,
            mb_x * 8,
            mb_y * 8,
            1,
        ),
        _ => return,
    };
    for row in 0..8 {
        for column in 0..8 {
            let index = (y + row * line_step) * stride + x + column;
            let residual = i32::from(samples[row * 8 + column]);
            let value = if intra {
                residual
            } else {
                i32::from(plane[index]) + residual
            };
            plane[index] = u8::try_from(value.clamp(0, 255)).unwrap_or_default();
        }
    }
}

fn crop_plane(data: &[u8], stride: usize, width: usize, height: usize) -> Vec<u8> {
    let mut output = Vec::with_capacity(width * height);
    for row in data.chunks_exact(stride).take(height) {
        output.extend_from_slice(&row[..width]);
    }
    output
}

fn skip_extra_information(bits: &mut BitReader<'_>, context: &str) -> Result<()> {
    while bits.read_bit()? {
        if bits.bits_remaining() < 8 {
            return Err(Error::InvalidData(format!(
                "truncated MPEG-2 extra information in {context}"
            )));
        }
        bits.skip_bits(8)?;
    }
    Ok(())
}

fn remaining_bits_are_zero(bits: &BitReader<'_>) -> bool {
    let mut copy = bits.clone();
    while copy.bits_remaining() > 0 {
        if copy.read_bit().ok() == Some(true) {
            return false;
        }
    }
    true
}

fn read_bits_u8(bits: &mut BitReader<'_>, count: u8, field: &str) -> Result<u8> {
    let value = bits.read_bits(count)?;
    u8::try_from(value).map_err(|_| Error::InvalidData(format!("{field} exceeds u8")))
}

trait CopySign {
    fn copysign(self, sign: Self) -> Self;
}

impl CopySign for i32 {
    fn copysign(self, sign: Self) -> Self {
        if sign < 0 { -self } else { self }
    }
}
