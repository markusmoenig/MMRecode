//! Encoded packet exchange between containers and codecs.

use crate::{StreamId, Timestamp};

/// Flags describing an encoded packet.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq)]
pub struct PacketFlags(u32);

impl PacketFlags {
    /// The packet is independently decodable according to its container.
    pub const KEY: Self = Self(1 << 0);
    /// The packet is known to contain damaged data.
    pub const CORRUPT: Self = Self(1 << 1);

    /// Creates an empty flag set.
    #[must_use]
    pub const fn empty() -> Self {
        Self(0)
    }

    /// Returns true if every flag in `other` is present.
    #[must_use]
    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    /// Inserts flags into this set.
    pub fn insert(&mut self, other: Self) {
        self.0 |= other.0;
    }
}

/// Container- or codec-specific side data associated with a packet.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PacketSideData {
    /// Stable side-data kind name.
    pub kind: String,
    /// Opaque side-data payload.
    pub data: Vec<u8>,
}

/// One encoded packet exchanged between demuxers, codecs, and muxers.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Packet {
    /// Stream to which the packet belongs.
    pub stream_id: StreamId,
    /// Encoded bytes.
    pub data: Vec<u8>,
    /// Presentation timestamp.
    pub pts: Option<Timestamp>,
    /// Decode timestamp.
    pub dts: Option<Timestamp>,
    /// Packet duration.
    pub duration: Option<Timestamp>,
    /// Packet flags.
    pub flags: PacketFlags,
    /// Additional metadata that must travel with the packet.
    pub side_data: Vec<PacketSideData>,
}
