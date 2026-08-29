//! Typed MPEG-2 Video header syntax.

use mmrecode_core::{Error, Rational, Result};

use crate::tables::{DEFAULT_INTRA_MATRIX, DEFAULT_NON_INTRA_MATRIX, ZIGZAG};

/// MPEG-2 chroma sampling format.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ChromaFormat {
    /// 4:2:0 sampling.
    Yuv420,
    /// 4:2:2 sampling.
    Yuv422,
    /// 4:4:4 sampling.
    Yuv444,
    /// Reserved syntax value.
    Reserved,
}

impl ChromaFormat {
    pub(crate) const fn from_code(code: u8) -> Self {
        match code {
            1 => Self::Yuv420,
            2 => Self::Yuv422,
            3 => Self::Yuv444,
            _ => Self::Reserved,
        }
    }
}

/// Standard MPEG frame-rate code.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum FrameRate {
    /// 24000/1001 frames per second.
    Fps23_976,
    /// 24 frames per second.
    Fps24,
    /// 25 frames per second.
    Fps25,
    /// 30000/1001 frames per second.
    Fps29_97,
    /// 30 frames per second.
    Fps30,
    /// 50 frames per second.
    Fps50,
    /// 60000/1001 frames per second.
    Fps59_94,
    /// 60 frames per second.
    Fps60,
}

impl FrameRate {
    /// Converts an MPEG frame-rate code into a named rate.
    #[must_use]
    pub const fn from_code(code: u8) -> Option<Self> {
        match code {
            1 => Some(Self::Fps23_976),
            2 => Some(Self::Fps24),
            3 => Some(Self::Fps25),
            4 => Some(Self::Fps29_97),
            5 => Some(Self::Fps30),
            6 => Some(Self::Fps50),
            7 => Some(Self::Fps59_94),
            8 => Some(Self::Fps60),
            _ => None,
        }
    }

    /// Returns the four-bit MPEG frame-rate code.
    #[must_use]
    pub const fn code(self) -> u8 {
        match self {
            Self::Fps23_976 => 1,
            Self::Fps24 => 2,
            Self::Fps25 => 3,
            Self::Fps29_97 => 4,
            Self::Fps30 => 5,
            Self::Fps50 => 6,
            Self::Fps59_94 => 7,
            Self::Fps60 => 8,
        }
    }

    /// Returns the exact nominal frame rate.
    ///
    /// # Panics
    ///
    /// This function cannot panic because every standard rate has a non-zero denominator.
    #[must_use]
    pub fn rational(self) -> Rational {
        match self {
            Self::Fps23_976 => Rational::new(24_000, 1_001),
            Self::Fps24 => Rational::new(24, 1),
            Self::Fps25 => Rational::new(25, 1),
            Self::Fps29_97 => Rational::new(30_000, 1_001),
            Self::Fps30 => Rational::new(30, 1),
            Self::Fps50 => Rational::new(50, 1),
            Self::Fps59_94 => Rational::new(60_000, 1_001),
            Self::Fps60 => Rational::new(60, 1),
        }
        .expect("standard MPEG frame-rate rationals have non-zero denominators")
    }
}

/// Sequence header preceding one or more GOPs.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SequenceHeader {
    /// Low twelve bits of the coded width.
    pub horizontal_size_value: u16,
    /// Low twelve bits of the coded height.
    pub vertical_size_value: u16,
    /// Sample-aspect/display-aspect code.
    pub aspect_ratio_information: u8,
    /// Base frame rate.
    pub frame_rate: FrameRate,
    /// Low 18 bits of the MPEG-2 maximum bit rate in units of 400 bit/s.
    pub bit_rate_value: u32,
    /// Base VBV buffer size in units of 16,384 bits.
    pub vbv_buffer_size_value: u16,
    /// MPEG-1 constrained-parameters flag.
    pub constrained_parameters_flag: bool,
    /// Intra quantizer matrix in transmitted (zig-zag scan) order, if present.
    pub intra_quantizer_matrix: Option<[u8; 64]>,
    /// Non-intra quantizer matrix in transmitted (zig-zag scan) order, if present.
    pub non_intra_quantizer_matrix: Option<[u8; 64]>,
}

/// MPEG-2 sequence extension.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SequenceExtension {
    /// Profile and level indication byte.
    pub profile_and_level_indication: u8,
    /// Whether the coded sequence contains progressive frames only.
    pub progressive_sequence: bool,
    /// Chroma sampling format.
    pub chroma_format: ChromaFormat,
    /// Two high bits of coded width.
    pub horizontal_size_extension: u8,
    /// Two high bits of coded height.
    pub vertical_size_extension: u8,
    /// Twelve high bits of the bit-rate value.
    pub bit_rate_extension: u16,
    /// Eight high bits of the VBV buffer size.
    pub vbv_buffer_size_extension: u8,
    /// Whether the sequence has no B-picture reordering delay.
    pub low_delay: bool,
    /// Frame-rate extension numerator field.
    pub frame_rate_extension_n: u8,
    /// Frame-rate extension denominator field.
    pub frame_rate_extension_d: u8,
}

/// Optional colour-description fields in a sequence display extension.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ColourDescription {
    /// ISO/IEC colour-primaries code.
    pub colour_primaries: u8,
    /// ISO/IEC transfer-characteristics code.
    pub transfer_characteristics: u8,
    /// ISO/IEC matrix-coefficients code.
    pub matrix_coefficients: u8,
}

/// MPEG-2 sequence display extension (extension identifier 2).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SequenceDisplayExtension {
    /// Three-bit source video-format code.
    pub video_format: u8,
    /// Optional explicitly signalled colour characteristics.
    pub colour_description: Option<ColourDescription>,
    /// Intended display width in samples.
    pub display_horizontal_size: u16,
    /// Intended display height in lines.
    pub display_vertical_size: u16,
}

/// Resolved parameters from a sequence header and its MPEG-2 extension.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SequenceParameters {
    /// Visible/coded horizontal size.
    pub width: usize,
    /// Visible/coded vertical size.
    pub height: usize,
    /// Chroma sampling format.
    pub chroma_format: ChromaFormat,
    /// Progressive-only sequence flag.
    pub progressive_sequence: bool,
    /// Exact extended frame rate.
    pub frame_rate: Rational,
    /// Maximum VBV input bit rate in bits per second.
    ///
    /// The optional representation is retained for forward compatibility with MPEG-1 syntax;
    /// parsed MPEG-2 streams always contain `Some` because a zero value is forbidden and MPEG-2
    /// does not use the MPEG-1 all-ones variable-rate sentinel.
    pub bit_rate: Option<u64>,
    /// VBV buffer size in bits.
    pub vbv_buffer_size_bits: u64,
    /// Profile and level byte.
    pub profile_and_level_indication: u8,
    /// Active sequence display and colour metadata, when signalled.
    pub display: Option<SequenceDisplayExtension>,
    /// Active intra quantizer matrix in natural coefficient order.
    pub intra_quantizer_matrix: [u8; 64],
    /// Active non-intra quantizer matrix in natural coefficient order.
    pub non_intra_quantizer_matrix: [u8; 64],
    /// Active chroma intra matrix in natural coefficient order.
    pub chroma_intra_quantizer_matrix: [u8; 64],
    /// Active chroma non-intra matrix in natural coefficient order.
    pub chroma_non_intra_quantizer_matrix: [u8; 64],
}

impl SequenceParameters {
    /// Resolves sequence header and extension fields and validates reserved values.
    ///
    /// # Errors
    ///
    /// Returns an error for reserved chroma syntax or arithmetic overflow.
    pub fn resolve(header: &SequenceHeader, extension: &SequenceExtension) -> Result<Self> {
        if extension.chroma_format == ChromaFormat::Reserved {
            return Err(Error::InvalidData("reserved MPEG-2 chroma format".into()));
        }
        let width = usize::from(header.horizontal_size_value)
            | (usize::from(extension.horizontal_size_extension) << 12);
        let height = usize::from(header.vertical_size_value)
            | (usize::from(extension.vertical_size_extension) << 12);
        if width == 0 || height == 0 {
            return Err(Error::InvalidData(
                "MPEG-2 sequence dimensions are zero".into(),
            ));
        }
        let base = header.frame_rate.rational();
        let rate_numerator = base
            .numerator()
            .checked_mul(i64::from(extension.frame_rate_extension_n) + 1)
            .ok_or_else(|| Error::InvalidData("MPEG-2 frame rate overflows".into()))?;
        let rate_denominator = base
            .denominator()
            .checked_mul(i64::from(extension.frame_rate_extension_d) + 1)
            .ok_or_else(|| Error::InvalidData("MPEG-2 frame rate overflows".into()))?;
        let bit_rate_value =
            u64::from(header.bit_rate_value) | (u64::from(extension.bit_rate_extension) << 18);
        if bit_rate_value == 0 {
            return Err(Error::InvalidData(
                "zero MPEG-2 bit_rate value is forbidden".into(),
            ));
        }
        let bit_rate = Some(bit_rate_value * 400);
        let vbv_value = u64::from(header.vbv_buffer_size_value)
            | (u64::from(extension.vbv_buffer_size_extension) << 10);
        let intra_quantizer_matrix = resolve_matrix(
            header.intra_quantizer_matrix.as_ref(),
            &DEFAULT_INTRA_MATRIX,
        );
        let non_intra_quantizer_matrix = resolve_matrix(
            header.non_intra_quantizer_matrix.as_ref(),
            &DEFAULT_NON_INTRA_MATRIX,
        );
        Ok(Self {
            width,
            height,
            chroma_format: extension.chroma_format,
            progressive_sequence: extension.progressive_sequence,
            frame_rate: Rational::new(rate_numerator, rate_denominator)?,
            bit_rate,
            vbv_buffer_size_bits: vbv_value * 16_384,
            profile_and_level_indication: extension.profile_and_level_indication,
            display: None,
            intra_quantizer_matrix,
            non_intra_quantizer_matrix,
            chroma_intra_quantizer_matrix: intra_quantizer_matrix,
            chroma_non_intra_quantizer_matrix: non_intra_quantizer_matrix,
        })
    }

    pub(crate) fn apply_quant_matrix_extension(&mut self, extension: &QuantMatrixExtension) {
        if let Some(matrix) = extension.intra_quantizer_matrix.as_ref() {
            self.intra_quantizer_matrix = resolve_matrix(Some(matrix), &DEFAULT_INTRA_MATRIX);
        }
        if let Some(matrix) = extension.non_intra_quantizer_matrix.as_ref() {
            self.non_intra_quantizer_matrix =
                resolve_matrix(Some(matrix), &DEFAULT_NON_INTRA_MATRIX);
        }
        if let Some(matrix) = extension.chroma_intra_quantizer_matrix.as_ref() {
            self.chroma_intra_quantizer_matrix =
                resolve_matrix(Some(matrix), &DEFAULT_INTRA_MATRIX);
        }
        if let Some(matrix) = extension.chroma_non_intra_quantizer_matrix.as_ref() {
            self.chroma_non_intra_quantizer_matrix =
                resolve_matrix(Some(matrix), &DEFAULT_NON_INTRA_MATRIX);
        }
    }
}

fn resolve_matrix(transmitted: Option<&[u8; 64]>, default: &[i32; 64]) -> [u8; 64] {
    if let Some(transmitted) = transmitted {
        let mut natural = [0_u8; 64];
        for (scan_index, &value) in transmitted.iter().enumerate() {
            natural[ZIGZAG[scan_index]] = value;
        }
        natural
    } else {
        std::array::from_fn(|index| {
            u8::try_from(default[index]).expect("default MPEG quantizer values fit u8")
        })
    }
}

/// GOP timecode and splice flags.
#[allow(clippy::struct_excessive_bools)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GroupHeader {
    /// Drop-frame timecode flag.
    pub drop_frame_flag: bool,
    /// Timecode hours.
    pub hours: u8,
    /// Timecode minutes.
    pub minutes: u8,
    /// Marker bit between minutes and seconds.
    pub marker_bit: bool,
    /// Timecode seconds.
    pub seconds: u8,
    /// Timecode picture count.
    pub pictures: u8,
    /// Closed GOP flag.
    pub closed_gop: bool,
    /// Broken-link flag.
    pub broken_link: bool,
}

/// MPEG picture coding type.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum PictureType {
    /// Intra-coded picture.
    I,
    /// Forward-predicted picture.
    P,
    /// Bidirectionally predicted picture.
    B,
    /// D picture (MPEG-1 only).
    D,
    /// Reserved picture coding value.
    Reserved(u8),
}

impl PictureType {
    pub(crate) const fn from_code(code: u8) -> Self {
        match code {
            1 => Self::I,
            2 => Self::P,
            3 => Self::B,
            4 => Self::D,
            other => Self::Reserved(other),
        }
    }
}

/// Base picture header.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PictureHeader {
    /// Ten-bit display-order counter within the GOP.
    pub temporal_reference: u16,
    /// Picture coding type.
    pub picture_coding_type: PictureType,
    /// VBV delay, with `0xffff` denoting unspecified variable-rate timing.
    pub vbv_delay: u16,
    /// MPEG-1 full-pel forward vector flag, if syntactically present.
    pub full_pel_forward_vector: Option<bool>,
    /// MPEG-1 forward f-code, if syntactically present.
    pub forward_f_code: Option<u8>,
    /// MPEG-1 full-pel backward vector flag, if syntactically present.
    pub full_pel_backward_vector: Option<bool>,
    /// MPEG-1 backward f-code, if syntactically present.
    pub backward_f_code: Option<u8>,
}

/// Picture structure signalled by the picture coding extension.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum PictureStructure {
    /// Top field picture.
    TopField,
    /// Bottom field picture.
    BottomField,
    /// Complete frame picture.
    Frame,
    /// Reserved value.
    Reserved,
}

impl PictureStructure {
    pub(crate) const fn from_code(code: u8) -> Self {
        match code {
            1 => Self::TopField,
            2 => Self::BottomField,
            3 => Self::Frame,
            _ => Self::Reserved,
        }
    }
}

/// MPEG-2 picture coding extension.
#[allow(clippy::struct_excessive_bools)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PictureCodingExtension {
    /// Forward/backward horizontal/vertical motion-vector f-codes.
    pub f_code: [[u8; 2]; 2],
    /// Intra DC precision code.
    pub intra_dc_precision: u8,
    /// Field or frame picture structure.
    pub picture_structure: PictureStructure,
    /// Top-field-first display flag.
    pub top_field_first: bool,
    /// Frame prediction/frame DCT shortcut flag.
    pub frame_pred_frame_dct: bool,
    /// Concealment motion-vector flag.
    pub concealment_motion_vectors: bool,
    /// Non-linear quantizer-scale flag.
    pub q_scale_type: bool,
    /// Alternate intra VLC table flag.
    pub intra_vlc_format: bool,
    /// Alternate vertical scan flag.
    pub alternate_scan: bool,
    /// Repeat-first-field display flag.
    pub repeat_first_field: bool,
    /// 4:2:0 chroma type flag.
    pub chroma_420_type: bool,
    /// Whether the current picture represents a progressive frame.
    pub progressive_frame: bool,
}

/// MPEG-2 quant-matrix extension (extension identifier 3).
///
/// Loaded matrices are retained in transmitted scan order. Missing fields leave the corresponding
/// active matrix unchanged.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct QuantMatrixExtension {
    /// Replacement luma intra matrix, when loaded.
    pub intra_quantizer_matrix: Option<[u8; 64]>,
    /// Replacement luma non-intra matrix, when loaded.
    pub non_intra_quantizer_matrix: Option<[u8; 64]>,
    /// Replacement chroma intra matrix, when loaded.
    pub chroma_intra_quantizer_matrix: Option<[u8; 64]>,
    /// Replacement chroma non-intra matrix, when loaded.
    pub chroma_non_intra_quantizer_matrix: Option<[u8; 64]>,
}

impl QuantMatrixExtension {
    pub(crate) fn merge(&mut self, newer: &Self) {
        if newer.intra_quantizer_matrix.is_some() {
            self.intra_quantizer_matrix = newer.intra_quantizer_matrix;
        }
        if newer.non_intra_quantizer_matrix.is_some() {
            self.non_intra_quantizer_matrix = newer.non_intra_quantizer_matrix;
        }
        if newer.chroma_intra_quantizer_matrix.is_some() {
            self.chroma_intra_quantizer_matrix = newer.chroma_intra_quantizer_matrix;
        }
        if newer.chroma_non_intra_quantizer_matrix.is_some() {
            self.chroma_non_intra_quantizer_matrix = newer.chroma_non_intra_quantizer_matrix;
        }
    }
}

/// Typed extension attached to a sequence or picture.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Extension {
    /// Sequence extension (identifier 1).
    Sequence(SequenceExtension),
    /// Sequence display extension (identifier 2).
    SequenceDisplay(SequenceDisplayExtension),
    /// Picture coding extension (identifier 8).
    PictureCoding(PictureCodingExtension),
    /// Quant-matrix extension (identifier 3).
    QuantMatrix(Box<QuantMatrixExtension>),
    /// Extension syntax retained but not yet interpreted.
    Other {
        /// Four-bit extension identifier.
        identifier: u8,
        /// Complete extension payload, including identifier bits.
        data: Vec<u8>,
    },
}
