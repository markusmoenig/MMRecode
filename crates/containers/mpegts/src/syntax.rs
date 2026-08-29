use std::ops::Range;

/// Header common to every 188-byte transport packet.
#[allow(clippy::struct_excessive_bools)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TransportPacketHeader {
    /// Packet corruption indication supplied by the transport layer.
    pub transport_error_indicator: bool,
    /// Payload starts a PSI section or PES packet.
    pub payload_unit_start_indicator: bool,
    /// Transport priority flag.
    pub transport_priority: bool,
    /// Thirteen-bit packet identifier.
    pub pid: u16,
    /// Two-bit scrambling control value.
    pub scrambling_control: u8,
    /// Whether the packet contains an adaptation field.
    pub has_adaptation_field: bool,
    /// Whether the packet contains payload bytes.
    pub has_payload: bool,
    /// Four-bit continuity counter.
    pub continuity_counter: u8,
}

/// Parsed MPEG-2 transport packet with byte-localized payload.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TransportPacket {
    /// Packet index within the stream.
    pub index: usize,
    /// Absolute source byte range.
    pub source_range: Range<usize>,
    /// Parsed packet header.
    pub header: TransportPacketHeader,
    /// Adaptation-field discontinuity flag.
    pub discontinuity_indicator: bool,
    /// Adaptation-field random-access flag.
    pub random_access_indicator: bool,
    /// Program clock reference base in 90 kHz ticks, when present.
    pub pcr: Option<u64>,
    /// Absolute source range containing payload bytes.
    pub payload_range: Option<Range<usize>>,
}

/// One program entry in a Program Association Table.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PatProgram {
    /// Program number. Zero identifies a network PID rather than a PMT.
    pub program_number: u16,
    /// PMT or network PID.
    pub pid: u16,
}

/// Parsed Program Association Table.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProgramAssociationTable {
    /// Transport-stream identifier.
    pub transport_stream_id: u16,
    /// Five-bit table version.
    pub version: u8,
    /// Whether the table is currently applicable.
    pub current_next: bool,
    /// Program mappings carried in this section.
    pub programs: Vec<PatProgram>,
}

/// One elementary stream declared by a PMT.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ElementaryStreamInfo {
    /// H.222.0 stream type.
    pub stream_type: u8,
    /// Elementary-stream PID.
    pub elementary_pid: u16,
    /// Opaque ES descriptor loop.
    pub descriptors: Vec<u8>,
}

/// Parsed Program Map Table.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProgramMapTable {
    /// Program number selected by the PAT.
    pub program_number: u16,
    /// PID on which this PMT was carried.
    pub pid: u16,
    /// Five-bit table version.
    pub version: u8,
    /// Whether the table is currently applicable.
    pub current_next: bool,
    /// PID carrying the program clock reference.
    pub pcr_pid: u16,
    /// Opaque program descriptor loop.
    pub descriptors: Vec<u8>,
    /// Declared elementary streams.
    pub streams: Vec<ElementaryStreamInfo>,
}

/// Non-fatal structural issue retained during transport inspection.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TransportStreamIssue {
    /// Packet associated with the issue, when localized.
    pub packet_index: Option<usize>,
    /// Human-readable diagnostic.
    pub message: String,
}
