//! MPEG-2 Transport Stream syntax, demuxing, and deterministic muxing.
//!
//! The initial slice supports 188-byte transport packets, PAT/PMT program discovery,
//! PES reassembly with PTS/DTS, PCR inspection, and a single-program MPEG-2 Video muxer.

mod crc;
mod demux;
mod mux;
mod syntax;

pub use demux::{MpegTsDemuxer, TransportStream, demux_transport_stream};
pub use mux::{MpegTsMuxConfig, MpegTsMuxer, mux_mpeg2_video};
pub use syntax::{
    ElementaryStreamInfo, PatProgram, ProgramAssociationTable, ProgramMapTable, TransportPacket,
    TransportPacketHeader, TransportStreamIssue,
};

/// MPEG-2 Transport Stream packet size in bytes.
pub const TS_PACKET_SIZE: usize = 188;

/// MPEG-2 Systems 90 kHz timestamp time base denominator.
pub const SYSTEM_CLOCK_FREQUENCY: i64 = 90_000;
