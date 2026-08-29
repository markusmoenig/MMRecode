//! MPEG-2 Video elementary-stream start-code and header parser.

use std::ops::Range;

use mmrecode_bitstream::{BitReader, find_start_code_prefix};
use mmrecode_core::{Error, Result};

use crate::syntax::{
    ChromaFormat, ColourDescription, Extension, FrameRate, GroupHeader, PictureCodingExtension,
    PictureHeader, PictureStructure, PictureType, QuantMatrixExtension, SequenceDisplayExtension,
    SequenceExtension, SequenceHeader, SequenceParameters,
};

/// One byte-aligned MPEG start-code unit.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StartCodeUnit {
    /// Start-code byte following the `00 00 01` prefix.
    pub code: u8,
    /// Byte offset of the prefix.
    pub offset: usize,
    /// Payload range after the code byte and before the next prefix.
    pub payload_range: Range<usize>,
}

impl StartCodeUnit {
    /// Returns the payload from the original elementary stream.
    #[must_use]
    pub fn payload<'a>(&self, data: &'a [u8]) -> &'a [u8] {
        &data[self.payload_range.clone()]
    }
}

/// One MPEG-2 slice start and its immediately accessible header fields.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Slice {
    /// Raw slice start-code byte.
    pub start_code: u8,
    /// Resolved vertical macroblock-row position, one-based.
    pub vertical_position: u16,
    /// Five-bit quantizer scale code.
    pub quantiser_scale_code: u8,
    /// Byte offset of the slice start-code prefix.
    pub offset: usize,
    /// Raw slice payload range.
    pub payload_range: Range<usize>,
}

/// One coded MPEG-2 picture with the active sequence and GOP context.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Picture {
    /// Bytes beginning at the picture start code and ending before the next picture or preamble.
    pub source_range: Range<usize>,
    /// Base picture header.
    pub header: PictureHeader,
    /// MPEG-2 picture coding extension.
    pub coding_extension: PictureCodingExtension,
    /// Active sequence parameters.
    pub sequence: SequenceParameters,
    /// Most recent GOP header, when present.
    pub group: Option<GroupHeader>,
    /// Slices belonging to this picture.
    pub slices: Vec<Slice>,
    /// Number of user-data units attached to the picture.
    pub user_data_units: usize,
}

/// Parsed MPEG-2 Video elementary stream.
#[derive(Clone, Debug)]
pub struct Mpeg2Stream<'a> {
    data: &'a [u8],
    units: Vec<StartCodeUnit>,
    sequence_headers: Vec<SequenceHeader>,
    sequence_extensions: Vec<SequenceExtension>,
    sequence_display_extensions: Vec<SequenceDisplayExtension>,
    quant_matrix_extensions: Vec<QuantMatrixExtension>,
    groups: Vec<GroupHeader>,
    pictures: Vec<Picture>,
}

impl<'a> Mpeg2Stream<'a> {
    /// Original elementary-stream bytes.
    #[must_use]
    pub const fn data(&self) -> &'a [u8] {
        self.data
    }

    /// All byte-aligned start-code units in source order.
    #[must_use]
    pub fn units(&self) -> &[StartCodeUnit] {
        &self.units
    }

    /// Parsed sequence headers.
    #[must_use]
    pub fn sequence_headers(&self) -> &[SequenceHeader] {
        &self.sequence_headers
    }

    /// Parsed MPEG-2 sequence extensions.
    #[must_use]
    pub fn sequence_extensions(&self) -> &[SequenceExtension] {
        &self.sequence_extensions
    }

    /// Parsed MPEG-2 sequence display extensions.
    #[must_use]
    pub fn sequence_display_extensions(&self) -> &[SequenceDisplayExtension] {
        &self.sequence_display_extensions
    }

    /// Parsed MPEG-2 quant-matrix extensions in elementary-stream order.
    #[must_use]
    pub fn quant_matrix_extensions(&self) -> &[QuantMatrixExtension] {
        &self.quant_matrix_extensions
    }

    /// Parsed GOP headers.
    #[must_use]
    pub fn groups(&self) -> &[GroupHeader] {
        &self.groups
    }

    /// Parsed coded pictures in elementary-stream decode order.
    #[must_use]
    pub fn pictures(&self) -> &[Picture] {
        &self.pictures
    }
}

/// Scans byte-aligned MPEG start codes without interpreting their payloads.
///
/// # Errors
///
/// Returns an error when a prefix is truncated before its code byte or no start code is present.
pub fn scan_start_codes(data: &[u8]) -> Result<Vec<StartCodeUnit>> {
    let Some(mut offset) = find_start_code_prefix(data, 0) else {
        return Err(Error::InvalidData(
            "MPEG-2 elementary stream contains no start code".into(),
        ));
    };
    let mut units = Vec::new();
    loop {
        let code_offset = offset
            .checked_add(3)
            .ok_or_else(|| Error::InvalidData("MPEG start-code offset overflow".into()))?;
        let Some(&code) = data.get(code_offset) else {
            return Err(Error::InvalidData(format!(
                "truncated MPEG start code at byte {offset}"
            )));
        };
        let payload_start = code_offset + 1;
        let next = find_start_code_prefix(data, payload_start);
        let payload_end = next.unwrap_or(data.len());
        units.push(StartCodeUnit {
            code,
            offset,
            payload_range: payload_start..payload_end,
        });
        let Some(next_offset) = next else {
            break;
        };
        offset = next_offset;
    }
    Ok(units)
}

#[derive(Debug)]
struct PictureBuilder {
    start: usize,
    header: PictureHeader,
    coding_extension: Option<PictureCodingExtension>,
    sequence: SequenceParameters,
    group: Option<GroupHeader>,
    slices: Vec<Slice>,
    user_data_units: usize,
}

impl PictureBuilder {
    fn finish(self, end: usize) -> Result<Picture> {
        let coding_extension = self.coding_extension.ok_or_else(|| {
            Error::InvalidData(format!(
                "picture at byte {} has no picture coding extension",
                self.start
            ))
        })?;
        if self.slices.is_empty() {
            return Err(Error::InvalidData(format!(
                "picture at byte {} contains no slices",
                self.start
            )));
        }
        Ok(Picture {
            source_range: self.start..end,
            header: self.header,
            coding_extension,
            sequence: self.sequence,
            group: self.group,
            slices: self.slices,
            user_data_units: self.user_data_units,
        })
    }
}

/// Parses an MPEG-2 Video elementary stream and validates its structural header dependencies.
///
/// # Errors
///
/// Returns an error for malformed headers, missing marker bits, missing MPEG-2 extensions,
/// pictures without slices, or unsupported/reserved core syntax.
#[allow(clippy::too_many_lines)]
pub fn parse_stream(data: &[u8]) -> Result<Mpeg2Stream<'_>> {
    let units = scan_start_codes(data)?;
    let mut sequence_headers = Vec::new();
    let mut sequence_extensions = Vec::new();
    let mut sequence_display_extensions = Vec::new();
    let mut quant_matrix_extensions = Vec::new();
    let mut groups = Vec::new();
    let mut pictures = Vec::new();
    let mut current_header: Option<SequenceHeader> = None;
    let mut current_extension: Option<SequenceExtension> = None;
    let mut current_display: Option<SequenceDisplayExtension> = None;
    let mut current_quant_matrices = QuantMatrixExtension::default();
    let mut current_group: Option<GroupHeader> = None;
    let mut current_picture: Option<PictureBuilder> = None;

    for unit in &units {
        let payload = unit.payload(data);
        match unit.code {
            0xb3 => {
                finish_picture(&mut current_picture, unit.offset, &mut pictures)?;
                let header = parse_sequence_header(payload, unit.offset)?;
                sequence_headers.push(header.clone());
                current_header = Some(header);
                current_extension = None;
                current_display = None;
                current_quant_matrices = QuantMatrixExtension::default();
                current_group = None;
            }
            0xb5 => {
                let extension = parse_extension(payload, unit.offset)?;
                match extension {
                    Extension::Sequence(value) => {
                        if current_picture.is_some() {
                            return Err(Error::InvalidData(format!(
                                "sequence extension inside picture at byte {}",
                                unit.offset
                            )));
                        }
                        sequence_extensions.push(value.clone());
                        current_extension = Some(value);
                    }
                    Extension::PictureCoding(value) => {
                        let picture = current_picture.as_mut().ok_or_else(|| {
                            Error::InvalidData(format!(
                                "picture coding extension without picture at byte {}",
                                unit.offset
                            ))
                        })?;
                        if picture.coding_extension.replace(value).is_some() {
                            return Err(Error::InvalidData(format!(
                                "duplicate picture coding extension at byte {}",
                                unit.offset
                            )));
                        }
                    }
                    Extension::SequenceDisplay(value) => {
                        if current_picture.is_some() || current_header.is_none() {
                            return Err(Error::InvalidData(format!(
                                "sequence display extension outside a sequence header at byte {}",
                                unit.offset
                            )));
                        }
                        current_display = Some(value);
                        sequence_display_extensions.push(value);
                    }
                    Extension::QuantMatrix(value) => {
                        if current_header.is_none() {
                            return Err(Error::InvalidData(format!(
                                "quant-matrix extension without sequence at byte {}",
                                unit.offset
                            )));
                        }
                        if let Some(picture) = current_picture.as_mut() {
                            picture.sequence.apply_quant_matrix_extension(&value);
                        }
                        current_quant_matrices.merge(&value);
                        quant_matrix_extensions.push(*value);
                    }
                    Extension::Other { .. } => {}
                }
            }
            0xb8 => {
                finish_picture(&mut current_picture, unit.offset, &mut pictures)?;
                let group = parse_group_header(payload, unit.offset)?;
                groups.push(group);
                current_group = Some(group);
            }
            0x00 => {
                finish_picture(&mut current_picture, unit.offset, &mut pictures)?;
                let header = current_header.as_ref().ok_or_else(|| {
                    Error::InvalidData(format!(
                        "picture at byte {} precedes a sequence header",
                        unit.offset
                    ))
                })?;
                let extension = current_extension.as_ref().ok_or_else(|| {
                    Error::InvalidData(format!(
                        "picture at byte {} has no active MPEG-2 sequence extension",
                        unit.offset
                    ))
                })?;
                let mut sequence = SequenceParameters::resolve(header, extension)?;
                sequence.display = current_display;
                sequence.apply_quant_matrix_extension(&current_quant_matrices);
                current_picture = Some(PictureBuilder {
                    start: unit.offset,
                    header: parse_picture_header(payload, unit.offset)?,
                    coding_extension: None,
                    sequence,
                    group: current_group,
                    slices: Vec::new(),
                    user_data_units: 0,
                });
            }
            0x01..=0xaf => {
                let picture = current_picture.as_mut().ok_or_else(|| {
                    Error::InvalidData(format!(
                        "slice at byte {} appears outside a picture",
                        unit.offset
                    ))
                })?;
                picture
                    .slices
                    .push(parse_slice(unit, payload, picture.sequence.height)?);
            }
            0xb2 => {
                if let Some(picture) = current_picture.as_mut() {
                    picture.user_data_units += 1;
                }
            }
            0xb7 => {
                finish_picture(&mut current_picture, unit.offset, &mut pictures)?;
                current_header = None;
                current_extension = None;
                current_display = None;
                current_quant_matrices = QuantMatrixExtension::default();
                current_group = None;
            }
            0xb4 | 0xb6 | 0xb9..=0xff => {
                return Err(Error::InvalidData(format!(
                    "reserved MPEG-2 start code 0x{:02x} at byte {}",
                    unit.code, unit.offset
                )));
            }
            _ => {}
        }
    }
    finish_picture(&mut current_picture, data.len(), &mut pictures)?;
    if sequence_headers.is_empty() {
        return Err(Error::InvalidData(
            "MPEG-2 stream contains no sequence header".into(),
        ));
    }
    if pictures.is_empty() {
        return Err(Error::InvalidData(
            "MPEG-2 stream contains no coded pictures".into(),
        ));
    }
    Ok(Mpeg2Stream {
        data,
        units,
        sequence_headers,
        sequence_extensions,
        sequence_display_extensions,
        quant_matrix_extensions,
        groups,
        pictures,
    })
}

fn finish_picture(
    current: &mut Option<PictureBuilder>,
    end: usize,
    pictures: &mut Vec<Picture>,
) -> Result<()> {
    if let Some(builder) = current.take() {
        pictures.push(builder.finish(end)?);
    }
    Ok(())
}

fn parse_sequence_header(payload: &[u8], offset: usize) -> Result<SequenceHeader> {
    let mut bits = BitReader::new(payload);
    let horizontal_size_value = read_u16(&mut bits, 12, "horizontal_size_value", offset)?;
    let vertical_size_value = read_u16(&mut bits, 12, "vertical_size_value", offset)?;
    if horizontal_size_value == 0 || vertical_size_value == 0 {
        return Err(syntax_error(offset, "sequence dimensions must be non-zero"));
    }
    let aspect_ratio_information = read_u8(&mut bits, 4, "aspect_ratio_information", offset)?;
    let frame_rate_code = read_u8(&mut bits, 4, "frame_rate_code", offset)?;
    let frame_rate = FrameRate::from_code(frame_rate_code).ok_or_else(|| {
        syntax_error(
            offset,
            &format!("reserved frame_rate_code {frame_rate_code}"),
        )
    })?;
    let bit_rate_value = read_u32(&mut bits, 18, "bit_rate_value", offset)?;
    require_marker(&mut bits, "sequence bit-rate marker", offset)?;
    let vbv_buffer_size_value = read_u16(&mut bits, 10, "vbv_buffer_size_value", offset)?;
    let constrained_parameters_flag = read_bool(&mut bits, "constrained_parameters_flag", offset)?;
    let intra_quantizer_matrix = read_optional_matrix(&mut bits, "intra matrix", offset)?;
    let non_intra_quantizer_matrix = read_optional_matrix(&mut bits, "non-intra matrix", offset)?;
    Ok(SequenceHeader {
        horizontal_size_value,
        vertical_size_value,
        aspect_ratio_information,
        frame_rate,
        bit_rate_value,
        vbv_buffer_size_value,
        constrained_parameters_flag,
        intra_quantizer_matrix,
        non_intra_quantizer_matrix,
    })
}

fn read_optional_matrix(
    bits: &mut BitReader<'_>,
    name: &str,
    offset: usize,
) -> Result<Option<[u8; 64]>> {
    if !read_bool(bits, &format!("load_{name}"), offset)? {
        return Ok(None);
    }
    let mut matrix = [0_u8; 64];
    for value in &mut matrix {
        *value = read_u8(bits, 8, name, offset)?;
    }
    Ok(Some(matrix))
}

fn parse_extension(payload: &[u8], offset: usize) -> Result<Extension> {
    let mut bits = BitReader::new(payload);
    let identifier = read_u8(&mut bits, 4, "extension_start_code_identifier", offset)?;
    match identifier {
        1 => Ok(Extension::Sequence(parse_sequence_extension(
            &mut bits, offset,
        )?)),
        2 => Ok(Extension::SequenceDisplay(
            parse_sequence_display_extension(&mut bits, offset)?,
        )),
        3 => Ok(Extension::QuantMatrix(Box::new(
            parse_quant_matrix_extension(&mut bits, offset)?,
        ))),
        8 => Ok(Extension::PictureCoding(parse_picture_coding_extension(
            &mut bits, offset,
        )?)),
        _ => Ok(Extension::Other {
            identifier,
            data: payload.to_vec(),
        }),
    }
}

fn parse_sequence_display_extension(
    bits: &mut BitReader<'_>,
    offset: usize,
) -> Result<SequenceDisplayExtension> {
    let video_format = read_u8(bits, 3, "video_format", offset)?;
    let colour_description = if read_bool(bits, "colour_description", offset)? {
        Some(ColourDescription {
            colour_primaries: read_u8(bits, 8, "colour_primaries", offset)?,
            transfer_characteristics: read_u8(bits, 8, "transfer_characteristics", offset)?,
            matrix_coefficients: read_u8(bits, 8, "matrix_coefficients", offset)?,
        })
    } else {
        None
    };
    let display_horizontal_size = read_u16(bits, 14, "display_horizontal_size", offset)?;
    require_marker(bits, "sequence-display size marker", offset)?;
    let display_vertical_size = read_u16(bits, 14, "display_vertical_size", offset)?;
    if display_horizontal_size == 0 || display_vertical_size == 0 {
        return Err(syntax_error(offset, "sequence display dimensions are zero"));
    }
    Ok(SequenceDisplayExtension {
        video_format,
        colour_description,
        display_horizontal_size,
        display_vertical_size,
    })
}

fn parse_quant_matrix_extension(
    bits: &mut BitReader<'_>,
    offset: usize,
) -> Result<QuantMatrixExtension> {
    Ok(QuantMatrixExtension {
        intra_quantizer_matrix: read_optional_matrix(bits, "intra quantizer matrix", offset)?,
        non_intra_quantizer_matrix: read_optional_matrix(
            bits,
            "non-intra quantizer matrix",
            offset,
        )?,
        chroma_intra_quantizer_matrix: read_optional_matrix(
            bits,
            "chroma intra quantizer matrix",
            offset,
        )?,
        chroma_non_intra_quantizer_matrix: read_optional_matrix(
            bits,
            "chroma non-intra quantizer matrix",
            offset,
        )?,
    })
}

fn parse_sequence_extension(bits: &mut BitReader<'_>, offset: usize) -> Result<SequenceExtension> {
    let profile_and_level_indication = read_u8(bits, 8, "profile_and_level_indication", offset)?;
    let progressive_sequence = read_bool(bits, "progressive_sequence", offset)?;
    let chroma_format = ChromaFormat::from_code(read_u8(bits, 2, "chroma_format", offset)?);
    let horizontal_size_extension = read_u8(bits, 2, "horizontal_size_extension", offset)?;
    let vertical_size_extension = read_u8(bits, 2, "vertical_size_extension", offset)?;
    let bit_rate_extension = read_u16(bits, 12, "bit_rate_extension", offset)?;
    require_marker(bits, "sequence-extension bit-rate marker", offset)?;
    let vbv_buffer_size_extension = read_u8(bits, 8, "vbv_buffer_size_extension", offset)?;
    let low_delay = read_bool(bits, "low_delay", offset)?;
    let frame_rate_extension_n = read_u8(bits, 2, "frame_rate_extension_n", offset)?;
    let frame_rate_extension_d = read_u8(bits, 5, "frame_rate_extension_d", offset)?;
    Ok(SequenceExtension {
        profile_and_level_indication,
        progressive_sequence,
        chroma_format,
        horizontal_size_extension,
        vertical_size_extension,
        bit_rate_extension,
        vbv_buffer_size_extension,
        low_delay,
        frame_rate_extension_n,
        frame_rate_extension_d,
    })
}

fn parse_group_header(payload: &[u8], offset: usize) -> Result<GroupHeader> {
    let mut bits = BitReader::new(payload);
    let drop_frame_flag = read_bool(&mut bits, "drop_frame_flag", offset)?;
    let hours = read_u8(&mut bits, 5, "time_code_hours", offset)?;
    let minutes = read_u8(&mut bits, 6, "time_code_minutes", offset)?;
    let marker_bit = read_bool(&mut bits, "time_code marker", offset)?;
    let seconds = read_u8(&mut bits, 6, "time_code_seconds", offset)?;
    let pictures = read_u8(&mut bits, 6, "time_code_pictures", offset)?;
    let closed_gop = read_bool(&mut bits, "closed_gop", offset)?;
    let broken_link = read_bool(&mut bits, "broken_link", offset)?;
    if hours > 23 || minutes > 59 || seconds > 59 {
        return Err(syntax_error(offset, "invalid GOP timecode"));
    }
    Ok(GroupHeader {
        drop_frame_flag,
        hours,
        minutes,
        marker_bit,
        seconds,
        pictures,
        closed_gop,
        broken_link,
    })
}

fn parse_picture_header(payload: &[u8], offset: usize) -> Result<PictureHeader> {
    let mut bits = BitReader::new(payload);
    let temporal_reference = read_u16(&mut bits, 10, "temporal_reference", offset)?;
    let picture_coding_type =
        PictureType::from_code(read_u8(&mut bits, 3, "picture_coding_type", offset)?);
    if matches!(picture_coding_type, PictureType::Reserved(_)) {
        return Err(syntax_error(offset, "reserved picture coding type"));
    }
    let vbv_delay = read_u16(&mut bits, 16, "vbv_delay", offset)?;
    let mut full_pel_forward_vector = None;
    let mut forward_f_code = None;
    let mut full_pel_backward_vector = None;
    let mut backward_f_code = None;
    if matches!(picture_coding_type, PictureType::P | PictureType::B) {
        full_pel_forward_vector = Some(read_bool(&mut bits, "full_pel_forward_vector", offset)?);
        let value = read_u8(&mut bits, 3, "forward_f_code", offset)?;
        if value == 0 {
            return Err(syntax_error(offset, "forward_f_code is zero"));
        }
        forward_f_code = Some(value);
    }
    if picture_coding_type == PictureType::B {
        full_pel_backward_vector = Some(read_bool(&mut bits, "full_pel_backward_vector", offset)?);
        let value = read_u8(&mut bits, 3, "backward_f_code", offset)?;
        if value == 0 {
            return Err(syntax_error(offset, "backward_f_code is zero"));
        }
        backward_f_code = Some(value);
    }
    while bits.bits_remaining() > 0 && bits.peek_bits(1)? != 0 {
        bits.skip_bits(1)?;
        if bits.bits_remaining() < 8 {
            return Err(syntax_error(offset, "truncated extra picture information"));
        }
        bits.skip_bits(8)?;
    }
    if bits.bits_remaining() > 0 {
        bits.skip_bits(1)?;
    }
    Ok(PictureHeader {
        temporal_reference,
        picture_coding_type,
        vbv_delay,
        full_pel_forward_vector,
        forward_f_code,
        full_pel_backward_vector,
        backward_f_code,
    })
}

fn parse_picture_coding_extension(
    bits: &mut BitReader<'_>,
    offset: usize,
) -> Result<PictureCodingExtension> {
    let f_code = [
        [
            read_u8(bits, 4, "f_code[0][0]", offset)?,
            read_u8(bits, 4, "f_code[0][1]", offset)?,
        ],
        [
            read_u8(bits, 4, "f_code[1][0]", offset)?,
            read_u8(bits, 4, "f_code[1][1]", offset)?,
        ],
    ];
    let intra_dc_precision = read_u8(bits, 2, "intra_dc_precision", offset)?;
    let picture_structure =
        PictureStructure::from_code(read_u8(bits, 2, "picture_structure", offset)?);
    if picture_structure == PictureStructure::Reserved {
        return Err(syntax_error(offset, "reserved picture_structure"));
    }
    let top_field_first = read_bool(bits, "top_field_first", offset)?;
    let frame_pred_frame_dct = read_bool(bits, "frame_pred_frame_dct", offset)?;
    let concealment_motion_vectors = read_bool(bits, "concealment_motion_vectors", offset)?;
    let q_scale_type = read_bool(bits, "q_scale_type", offset)?;
    let intra_vlc_format = read_bool(bits, "intra_vlc_format", offset)?;
    let alternate_scan = read_bool(bits, "alternate_scan", offset)?;
    let repeat_first_field = read_bool(bits, "repeat_first_field", offset)?;
    let chroma_420_type = read_bool(bits, "chroma_420_type", offset)?;
    let progressive_frame = read_bool(bits, "progressive_frame", offset)?;
    if read_bool(bits, "composite_display_flag", offset)? {
        bits.skip_bits(1 + 3 + 1 + 7 + 8).map_err(|error| {
            syntax_error(offset, &format!("truncated composite display: {error}"))
        })?;
    }
    Ok(PictureCodingExtension {
        f_code,
        intra_dc_precision,
        picture_structure,
        top_field_first,
        frame_pred_frame_dct,
        concealment_motion_vectors,
        q_scale_type,
        intra_vlc_format,
        alternate_scan,
        repeat_first_field,
        chroma_420_type,
        progressive_frame,
    })
}

fn parse_slice(unit: &StartCodeUnit, payload: &[u8], vertical_size: usize) -> Result<Slice> {
    let mut bits = BitReader::new(payload);
    let vertical_extension = if vertical_size > 2_800 {
        read_u16(
            &mut bits,
            3,
            "slice_vertical_position_extension",
            unit.offset,
        )?
    } else {
        0
    };
    let vertical_position = (vertical_extension << 7) | u16::from(unit.code);
    let quantiser_scale_code = read_u8(&mut bits, 5, "quantiser_scale_code", unit.offset)?;
    if quantiser_scale_code == 0 {
        return Err(syntax_error(
            unit.offset,
            "slice quantiser_scale_code is zero",
        ));
    }
    if read_bool(&mut bits, "extra_bit_slice", unit.offset)? {
        if bits.bits_remaining() < 8 {
            return Err(syntax_error(
                unit.offset,
                "truncated extra slice information",
            ));
        }
        bits.skip_bits(8)?;
        while read_bool(&mut bits, "extra_bit_slice", unit.offset)? {
            if bits.bits_remaining() < 8 {
                return Err(syntax_error(
                    unit.offset,
                    "truncated extra slice information",
                ));
            }
            bits.skip_bits(8)?;
        }
    }
    Ok(Slice {
        start_code: unit.code,
        vertical_position,
        quantiser_scale_code,
        offset: unit.offset,
        payload_range: unit.payload_range.clone(),
    })
}

fn read_bool(bits: &mut BitReader<'_>, field: &str, offset: usize) -> Result<bool> {
    bits.read_bit()
        .map_err(|_| syntax_error(offset, &format!("truncated {field}")))
}

fn read_u8(bits: &mut BitReader<'_>, count: u8, field: &str, offset: usize) -> Result<u8> {
    bits.read_bits(count)
        .and_then(|value| {
            u8::try_from(value)
                .map_err(|_| Error::InvalidData(format!("{field} exceeds eight bits")))
        })
        .map_err(|_| syntax_error(offset, &format!("truncated {field}")))
}

fn read_u16(bits: &mut BitReader<'_>, count: u8, field: &str, offset: usize) -> Result<u16> {
    bits.read_bits(count)
        .and_then(|value| {
            u16::try_from(value)
                .map_err(|_| Error::InvalidData(format!("{field} exceeds sixteen bits")))
        })
        .map_err(|_| syntax_error(offset, &format!("truncated {field}")))
}

fn read_u32(bits: &mut BitReader<'_>, count: u8, field: &str, offset: usize) -> Result<u32> {
    bits.read_bits(count)
        .and_then(|value| {
            u32::try_from(value).map_err(|_| Error::InvalidData(format!("{field} exceeds 32 bits")))
        })
        .map_err(|_| syntax_error(offset, &format!("truncated {field}")))
}

fn require_marker(bits: &mut BitReader<'_>, field: &str, offset: usize) -> Result<()> {
    if !read_bool(bits, field, offset)? {
        return Err(syntax_error(offset, &format!("zero {field}")));
    }
    Ok(())
}

fn syntax_error(offset: usize, message: &str) -> Error {
    Error::InvalidData(format!("MPEG-2 syntax at byte {offset}: {message}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use mmrecode_bitstream::BitWriter;

    #[test]
    fn scans_units_and_preserves_ranges() {
        let data = [0, 0, 1, 0xb3, 1, 2, 0, 0, 1, 0x00, 3, 4];
        let units = scan_start_codes(&data).unwrap();
        assert_eq!(units.len(), 2);
        assert_eq!(units[0].code, 0xb3);
        assert_eq!(units[0].payload(&data), [1, 2]);
        assert_eq!(units[1].offset, 6);
        assert_eq!(units[1].payload(&data), [3, 4]);
    }

    #[test]
    fn rejects_stream_without_start_codes() {
        assert!(scan_start_codes(&[1, 2, 3]).is_err());
    }

    #[test]
    fn resolves_extended_sequence_parameters() {
        let header = SequenceHeader {
            horizontal_size_value: 720,
            vertical_size_value: 576,
            aspect_ratio_information: 3,
            frame_rate: FrameRate::Fps25,
            bit_rate_value: 10_000,
            vbv_buffer_size_value: 112,
            constrained_parameters_flag: false,
            intra_quantizer_matrix: None,
            non_intra_quantizer_matrix: None,
        };
        let extension = SequenceExtension {
            profile_and_level_indication: 0x48,
            progressive_sequence: false,
            chroma_format: ChromaFormat::Yuv420,
            horizontal_size_extension: 0,
            vertical_size_extension: 0,
            bit_rate_extension: 0,
            vbv_buffer_size_extension: 0,
            low_delay: false,
            frame_rate_extension_n: 0,
            frame_rate_extension_d: 0,
        };
        let resolved = SequenceParameters::resolve(&header, &extension).unwrap();
        assert_eq!(resolved.width, 720);
        assert_eq!(resolved.height, 576);
        assert_eq!(resolved.bit_rate, Some(4_000_000));
        assert_eq!(resolved.vbv_buffer_size_bits, 112 * 16_384);
        assert_eq!(resolved.intra_quantizer_matrix[0], 8);
        assert_eq!(resolved.non_intra_quantizer_matrix, [16; 64]);
        assert_eq!(resolved.chroma_intra_quantizer_matrix[0], 8);
        assert_eq!(resolved.chroma_non_intra_quantizer_matrix, [16; 64]);
    }

    #[test]
    fn parses_and_applies_separate_quant_matrices() {
        let mut bits = BitWriter::new();
        bits.write_bits(3, 4).unwrap();
        bits.write_bit(true).unwrap();
        for value in 1_u8..=64 {
            bits.write_bits(u64::from(value), 8).unwrap();
        }
        bits.write_bit(false).unwrap();
        bits.write_bit(true).unwrap();
        for value in (65_u8..=128).rev() {
            bits.write_bits(u64::from(value), 8).unwrap();
        }
        bits.write_bit(false).unwrap();
        bits.align_to_byte();
        let payload = bits.into_bytes();
        let Extension::QuantMatrix(extension) = parse_extension(&payload, 100).unwrap() else {
            panic!("expected quant-matrix extension");
        };
        assert_eq!(extension.intra_quantizer_matrix.unwrap()[0], 1);
        assert!(extension.non_intra_quantizer_matrix.is_none());
        assert_eq!(extension.chroma_intra_quantizer_matrix.unwrap()[0], 128);
        assert!(extension.chroma_non_intra_quantizer_matrix.is_none());

        let first_picture = PROGRESSIVE_TEST_VECTOR
            .windows(4)
            .position(|window| window == [0, 0, 1, 0])
            .unwrap();
        let mut extended = PROGRESSIVE_TEST_VECTOR[..first_picture].to_vec();
        extended.extend_from_slice(&[0, 0, 1, 0xb5]);
        extended.extend_from_slice(&payload);
        extended.extend_from_slice(&PROGRESSIVE_TEST_VECTOR[first_picture..]);
        let stream = parse_stream(&extended).unwrap();
        assert_eq!(stream.quant_matrix_extensions().len(), 1);
        let sequence = &stream.pictures()[0].sequence;
        assert_eq!(sequence.intra_quantizer_matrix[0], 1);
        assert_eq!(sequence.intra_quantizer_matrix[1], 2);
        assert_eq!(sequence.chroma_intra_quantizer_matrix[0], 128);
        assert_eq!(sequence.non_intra_quantizer_matrix, [16; 64]);
    }

    #[test]
    fn rejects_zero_mpeg2_bit_rate() {
        let stream = parse_stream(PROGRESSIVE_TEST_VECTOR).unwrap();
        let mut header = stream.sequence_headers()[0].clone();
        let extension = stream.sequence_extensions()[0].clone();
        header.bit_rate_value = 0;
        assert!(SequenceParameters::resolve(&header, &extension).is_err());
    }

    #[test]
    fn parses_sequence_display_colour_metadata() {
        let mut bits = BitWriter::new();
        bits.write_bits(2, 4).unwrap();
        bits.write_bits(5, 3).unwrap();
        bits.write_bit(true).unwrap();
        bits.write_bits(1, 8).unwrap();
        bits.write_bits(1, 8).unwrap();
        bits.write_bits(6, 8).unwrap();
        bits.write_bits(96, 14).unwrap();
        bits.write_bit(true).unwrap();
        bits.write_bits(64, 14).unwrap();
        bits.align_to_byte();
        let payload = bits.into_bytes();
        let Extension::SequenceDisplay(display) = parse_extension(&payload, 200).unwrap() else {
            panic!("expected sequence display extension");
        };
        assert_eq!(display.video_format, 5);
        assert_eq!(
            (
                display.display_horizontal_size,
                display.display_vertical_size
            ),
            (96, 64)
        );
        assert_eq!(display.colour_description.unwrap().matrix_coefficients, 6);
    }

    const PROGRESSIVE_TEST_VECTOR: &[u8] =
        include_bytes!("../../../../testdata/mpeg2/valid/main-ml-progressive-ibp.m2v");
}
