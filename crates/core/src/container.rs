//! Container demuxing and muxing interfaces.

use crate::{Packet, Result, StreamDescriptor, StreamId, Timestamp};

/// Result of a container seek operation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SeekResult {
    /// Requested presentation time.
    pub requested: Timestamp,
    /// Earliest presentation time from which packets will be returned.
    pub actual: Timestamp,
}

/// An encoded media source.
pub trait Demuxer {
    /// Returns all discovered streams.
    fn streams(&self) -> &[StreamDescriptor];

    /// Reads the next encoded packet in container order.
    ///
    /// # Errors
    ///
    /// Returns an error when the container is malformed or the source cannot be read.
    fn read_packet(&mut self) -> Result<Option<Packet>>;

    /// Seeks to a container-supported position.
    ///
    /// # Errors
    ///
    /// Returns an error when seeking is unsupported or the source cannot seek.
    fn seek(&mut self, target: Timestamp) -> Result<SeekResult>;
}

/// An encoded media destination.
pub trait Muxer {
    /// Registers a stream and returns its output identifier.
    ///
    /// # Errors
    ///
    /// Returns an error when the descriptor cannot be represented by the container.
    fn add_stream(&mut self, descriptor: StreamDescriptor) -> Result<StreamId>;

    /// Writes one packet.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid ordering, incompatible packets, or destination I/O failures.
    fn write_packet(&mut self, packet: Packet) -> Result<()>;

    /// Writes indexes and other trailing metadata.
    ///
    /// # Errors
    ///
    /// Returns an error when final metadata cannot be generated or written.
    fn finalize(&mut self) -> Result<()>;
}
