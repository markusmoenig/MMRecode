//! Typed representations of JPEG marker segments.

/// A JPEG marker code.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Marker {
    /// Temporary marker (TEM).
    Temporary,
    /// Baseline sequential DCT frame header (SOF0).
    StartOfFrameBaseline,
    /// Huffman table definitions (DHT).
    DefineHuffmanTables,
    /// Restart marker RST0 through RST7.
    Restart(u8),
    /// Start of image (SOI).
    StartOfImage,
    /// End of image (EOI).
    EndOfImage,
    /// Start of scan (SOS).
    StartOfScan,
    /// Quantization table definitions (DQT).
    DefineQuantizationTables,
    /// Number of lines definition (DNL).
    DefineNumberOfLines,
    /// Restart interval definition (DRI).
    DefineRestartInterval,
    /// Application-specific marker APP0 through APP15.
    Application(u8),
    /// Comment marker (COM).
    Comment,
    /// A recognized marker prefix with otherwise unclassified code.
    Other(u8),
}

impl Marker {
    pub(crate) const fn from_code(code: u8) -> Self {
        match code {
            0x01 => Self::Temporary,
            0xc0 => Self::StartOfFrameBaseline,
            0xc4 => Self::DefineHuffmanTables,
            0xd0..=0xd7 => Self::Restart(code - 0xd0),
            0xd8 => Self::StartOfImage,
            0xd9 => Self::EndOfImage,
            0xda => Self::StartOfScan,
            0xdb => Self::DefineQuantizationTables,
            0xdc => Self::DefineNumberOfLines,
            0xdd => Self::DefineRestartInterval,
            0xe0..=0xef => Self::Application(code - 0xe0),
            0xfe => Self::Comment,
            _ => Self::Other(code),
        }
    }

    /// Returns the marker's byte following the `0xff` prefix.
    #[must_use]
    pub const fn code(self) -> u8 {
        match self {
            Self::Temporary => 0x01,
            Self::StartOfFrameBaseline => 0xc0,
            Self::DefineHuffmanTables => 0xc4,
            Self::Restart(number) => 0xd0 + number,
            Self::StartOfImage => 0xd8,
            Self::EndOfImage => 0xd9,
            Self::StartOfScan => 0xda,
            Self::DefineQuantizationTables => 0xdb,
            Self::DefineNumberOfLines => 0xdc,
            Self::DefineRestartInterval => 0xdd,
            Self::Application(number) => 0xe0 + number,
            Self::Comment => 0xfe,
            Self::Other(code) => code,
        }
    }

    /// Returns the conventional short marker name.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Temporary => "TEM",
            Self::StartOfFrameBaseline => "SOF0",
            Self::DefineHuffmanTables => "DHT",
            Self::Restart(0) => "RST0",
            Self::Restart(1) => "RST1",
            Self::Restart(2) => "RST2",
            Self::Restart(3) => "RST3",
            Self::Restart(4) => "RST4",
            Self::Restart(5) => "RST5",
            Self::Restart(6) => "RST6",
            Self::Restart(7) => "RST7",
            Self::Restart(_) => "RST?",
            Self::StartOfImage => "SOI",
            Self::EndOfImage => "EOI",
            Self::StartOfScan => "SOS",
            Self::DefineQuantizationTables => "DQT",
            Self::DefineNumberOfLines => "DNL",
            Self::DefineRestartInterval => "DRI",
            Self::Application(_) => "APP",
            Self::Comment => "COM",
            Self::Other(_) => "UNKNOWN",
        }
    }

    pub(crate) const fn has_length(self) -> bool {
        !matches!(
            self,
            Self::Temporary | Self::Restart(_) | Self::StartOfImage | Self::EndOfImage
        )
    }
}

/// Storage precision of a quantization table entry.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum QuantizationPrecision {
    /// Eight-bit entries.
    EightBit,
    /// Sixteen-bit entries.
    SixteenBit,
}

/// One JPEG quantization table in encoded zigzag order.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct QuantizationTable {
    /// Destination table identifier, from 0 through 3.
    pub id: u8,
    /// Entry precision.
    pub precision: QuantizationPrecision,
    /// The 64 table entries in stream (zigzag) order.
    pub values_in_zigzag_order: [u16; 64],
}

/// The coefficient class addressed by a Huffman table.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HuffmanTableClass {
    /// DC coefficient differences.
    Dc,
    /// AC coefficients.
    Ac,
}

/// One canonical JPEG Huffman table definition.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HuffmanTable {
    /// Whether this table encodes DC or AC values.
    pub class: HuffmanTableClass,
    /// Destination table identifier, from 0 through 3.
    pub id: u8,
    /// Number of codes for bit lengths 1 through 16.
    pub code_counts: [u8; 16],
    /// Symbols ordered by increasing code length.
    pub symbols: Vec<u8>,
}

/// One image component declared in a frame header.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FrameComponent {
    /// Component identifier from the bitstream.
    pub id: u8,
    /// Horizontal sampling factor.
    pub horizontal_sampling: u8,
    /// Vertical sampling factor.
    pub vertical_sampling: u8,
    /// Quantization table selector.
    pub quantization_table: u8,
}

/// Parsed baseline frame dimensions and component layout.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FrameHeader {
    /// Sample precision in bits.
    pub sample_precision: u8,
    /// Image width in pixels.
    pub width: u16,
    /// Image height in pixels, or zero when supplied later by DNL.
    pub height: u16,
    /// Components in stream order.
    pub components: Vec<FrameComponent>,
}

/// One component participating in an entropy-coded scan.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ScanComponent {
    /// Component selector matching a frame component identifier.
    pub selector: u8,
    /// DC Huffman table selector.
    pub dc_table: u8,
    /// AC Huffman table selector.
    pub ac_table: u8,
}

/// Parsed start-of-scan header.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ScanHeader {
    /// Components included in this scan.
    pub components: Vec<ScanComponent>,
    /// Start of spectral selection.
    pub spectral_start: u8,
    /// End of spectral selection.
    pub spectral_end: u8,
    /// Successive approximation high bit position.
    pub successive_approximation_high: u8,
    /// Successive approximation low bit position.
    pub successive_approximation_low: u8,
}

/// Parsed JFIF data carried by an APP0 segment.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct JfifHeader {
    /// JFIF major version.
    pub version_major: u8,
    /// JFIF minor version.
    pub version_minor: u8,
    /// Density unit code from the JFIF header.
    pub density_units: u8,
    /// Horizontal pixel density.
    pub density_x: u16,
    /// Vertical pixel density.
    pub density_y: u16,
    /// Thumbnail width in pixels.
    pub thumbnail_width: u8,
    /// Thumbnail height in pixels.
    pub thumbnail_height: u8,
    /// Packed RGB thumbnail bytes.
    pub thumbnail_rgb: Vec<u8>,
}

/// Opaque application-specific payload.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ApplicationSegment {
    /// APP marker number from 0 through 15.
    pub number: u8,
    /// Segment payload, excluding marker and length field.
    pub data: Vec<u8>,
}

/// Typed data decoded from a marker segment.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SegmentData {
    /// A marker with no payload.
    Empty,
    /// Baseline frame header.
    Frame(FrameHeader),
    /// One or more quantization table definitions.
    QuantizationTables(Vec<QuantizationTable>),
    /// One or more Huffman table definitions.
    HuffmanTables(Vec<HuffmanTable>),
    /// Restart interval measured in minimum coded units.
    RestartInterval(u16),
    /// Scan header.
    Scan(ScanHeader),
    /// JFIF APP0 header.
    Jfif(JfifHeader),
    /// Opaque APP segment.
    Application(ApplicationSegment),
    /// Comment bytes preserved without assuming a character encoding.
    Comment(Vec<u8>),
    /// Payload of an otherwise unclassified marker.
    Unknown(Vec<u8>),
}

/// One top-level JPEG marker and its decoded payload.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct JpegSegment {
    /// Absolute byte offset of the marker prefix.
    pub offset: usize,
    /// Marker kind.
    pub marker: Marker,
    /// Absolute byte offset of the payload, when present.
    pub payload_offset: Option<usize>,
    /// Payload size, excluding the two-byte length field.
    pub payload_length: usize,
    /// Parsed or preserved payload data.
    pub data: SegmentData,
}

/// A restart marker found inside entropy-coded data.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RestartMarker {
    /// Absolute offset of the marker prefix.
    pub offset: usize,
    /// Restart marker number from 0 through 7.
    pub number: u8,
}

/// Location and restart markers of one entropy-coded scan.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EntropyScan {
    /// Absolute offset of the first entropy-coded byte.
    pub data_offset: usize,
    /// Number of source bytes up to, but not including, the next structural marker.
    pub data_length: usize,
    /// Restart markers encountered in the entropy-coded data.
    pub restart_markers: Vec<RestartMarker>,
}

/// Fully indexed structure of one JPEG image.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct JpegImage {
    /// Marker segments in source order. Restart markers remain attached to scans.
    pub segments: Vec<JpegSegment>,
    /// Entropy-coded regions following SOS markers.
    pub entropy_scans: Vec<EntropyScan>,
    /// Bytes following EOI, preserved for inspection.
    pub trailing_data: Vec<u8>,
}

impl JpegImage {
    /// Returns the first baseline frame header, if present.
    #[must_use]
    pub fn frame_header(&self) -> Option<&FrameHeader> {
        self.segments
            .iter()
            .find_map(|segment| match &segment.data {
                SegmentData::Frame(header) => Some(header),
                _ => None,
            })
    }

    /// Returns the first parsed JFIF APP0 header, if present.
    #[must_use]
    pub fn jfif_header(&self) -> Option<&JfifHeader> {
        self.segments
            .iter()
            .find_map(|segment| match &segment.data {
                SegmentData::Jfif(header) => Some(header),
                _ => None,
            })
    }
}
