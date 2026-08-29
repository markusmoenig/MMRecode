//! Codec-independent picture dependency descriptions.

use crate::{Packet, Result};

/// Stable picture identifier assigned by a codec analyzer.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct PictureId(pub u64);

/// Broad coding role of a picture.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
#[non_exhaustive]
pub enum PictureKind {
    /// Intra-coded picture.
    Intra,
    /// Forward-predicted picture.
    Predicted,
    /// Bidirectionally predicted picture.
    Bidirectional,
    /// Codec-specific picture type not represented above.
    Other,
}

/// Strength of a picture as an entry point into a stream.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
#[non_exhaustive]
pub enum RandomAccessKind {
    /// Not a random-access picture.
    None,
    /// Entry requires codec-specific leading or recovery pictures.
    Recovery,
    /// Entry is independently decodable without earlier pictures.
    Clean,
}

/// Fingerprint of codec parameters relevant to splice compatibility.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct ParameterFingerprint(pub u64);

/// Codec-independent information about one coded access unit.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AccessUnitInfo {
    /// Analyzer-assigned picture identifier.
    pub picture_id: PictureId,
    /// Broad coding role.
    pub picture_kind: PictureKind,
    /// Position in decoding order.
    pub decode_order: i64,
    /// Position in presentation order.
    pub presentation_order: i64,
    /// Pictures required to reconstruct this picture.
    pub references: Vec<PictureId>,
    /// Random-access properties.
    pub random_access: RandomAccessKind,
    /// Parameters that must remain compatible across a copied splice.
    pub parameters: ParameterFingerprint,
}

/// Codec-specific parser that exposes a generic reference graph.
pub trait DependencyAnalyzer {
    /// Analyzes one encoded packet or access unit.
    ///
    /// # Errors
    ///
    /// Returns an error when syntax is malformed or the analyzer lacks required prior state.
    fn analyze_access_unit(&mut self, packet: &Packet) -> Result<AccessUnitInfo>;
}
