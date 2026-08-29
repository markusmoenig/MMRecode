//! MPEG-2 reference-picture and presentation-order analysis.

use std::{collections::BTreeSet, ops::Range};

use mmrecode_core::{
    AccessUnitInfo, DependencyAnalyzer, Error, Packet, ParameterFingerprint, PictureId,
    PictureKind, RandomAccessKind, Result,
};

use crate::{Mpeg2Stream, Picture, PictureType, SequenceParameters, parse_stream};

/// How one coded MPEG-2 picture participates in a smart-render operation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SmartRenderDisposition {
    /// The original coded picture can be copied unchanged.
    Copy,
    /// The picture itself was edited and must be encoded.
    EncodeEdited,
    /// The picture depends directly or transitively on a picture that must be encoded.
    BridgeEncode {
        /// Already affected references that caused propagation into this picture.
        affected_references: Vec<PictureId>,
    },
}

/// One picture decision in an MPEG-2 smart-render plan.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SmartRenderPicture {
    /// Stable picture identifier in elementary-stream decode order.
    pub picture_id: PictureId,
    /// Elementary-stream byte range containing this picture.
    pub source_range: Range<usize>,
    /// Decode position.
    pub decode_order: i64,
    /// Presentation position.
    pub presentation_order: i64,
    /// Copy or encode decision with an explainable propagation reason.
    pub disposition: SmartRenderDisposition,
}

/// Explainable affected-picture plan for one edited presentation interval.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SmartRenderPlan {
    /// Requested half-open edited interval in presentation order.
    pub edited_presentation_range: Range<i64>,
    /// Per-picture decisions in elementary-stream decode order.
    pub pictures: Vec<SmartRenderPicture>,
    /// Coalesced presentation intervals that require native/bridge encoding.
    pub encode_presentation_ranges: Vec<Range<i64>>,
}

/// Stateful MPEG-2 picture dependency analyzer.
#[derive(Clone, Debug, Default)]
pub struct Mpeg2DependencyAnalyzer {
    next_picture: u64,
    gop_presentation_base: i64,
    current_gop_max_temporal_reference: i64,
    seen_picture: bool,
    older_reference: Option<PictureId>,
    newer_reference: Option<PictureId>,
}

impl Mpeg2DependencyAnalyzer {
    /// Analyzes one already parsed picture in elementary-stream decode order.
    ///
    /// # Errors
    ///
    /// Returns an error when a predicted picture lacks the reference pictures required for
    /// reconstruction, or when picture counters overflow.
    pub fn analyze_picture(&mut self, picture: &Picture) -> Result<AccessUnitInfo> {
        let picture_id = PictureId(self.next_picture);
        let decode_order = i64::try_from(self.next_picture)
            .map_err(|_| Error::InvalidState("MPEG-2 decode order exceeds i64".into()))?;

        if picture.header.picture_coding_type == PictureType::I
            && self.seen_picture
            && i64::from(picture.header.temporal_reference)
                <= self.current_gop_max_temporal_reference
        {
            self.gop_presentation_base = self
                .gop_presentation_base
                .checked_add(self.current_gop_max_temporal_reference + 1)
                .ok_or_else(|| Error::InvalidState("MPEG-2 presentation order overflow".into()))?;
            self.current_gop_max_temporal_reference = -1;
        }
        let temporal_reference = i64::from(picture.header.temporal_reference);
        self.current_gop_max_temporal_reference = self
            .current_gop_max_temporal_reference
            .max(temporal_reference);
        let presentation_order = self
            .gop_presentation_base
            .checked_add(temporal_reference)
            .ok_or_else(|| Error::InvalidState("MPEG-2 presentation order overflow".into()))?;

        let (picture_kind, references, random_access) = match picture.header.picture_coding_type {
            PictureType::I => {
                let access = if picture.group.is_some_and(|group| group.closed_gop)
                    || (!self.seen_picture && picture.header.temporal_reference == 0)
                {
                    RandomAccessKind::Clean
                } else {
                    RandomAccessKind::Recovery
                };
                (PictureKind::Intra, Vec::new(), access)
            }
            PictureType::P => {
                let reference = self.newer_reference.ok_or_else(|| {
                    Error::InvalidData(format!(
                        "P picture {picture_id:?} has no preceding I/P reference"
                    ))
                })?;
                (
                    PictureKind::Predicted,
                    vec![reference],
                    RandomAccessKind::None,
                )
            }
            PictureType::B => {
                let older = self.older_reference.ok_or_else(|| {
                    Error::InvalidData(format!("B picture {picture_id:?} has no past reference"))
                })?;
                let newer = self.newer_reference.ok_or_else(|| {
                    Error::InvalidData(format!("B picture {picture_id:?} has no future reference"))
                })?;
                (
                    PictureKind::Bidirectional,
                    vec![older, newer],
                    RandomAccessKind::None,
                )
            }
            PictureType::D | PictureType::Reserved(_) => {
                return Err(Error::Unsupported(
                    "MPEG-1 D pictures are outside the MPEG-2 slice".into(),
                ));
            }
        };

        if matches!(
            picture.header.picture_coding_type,
            PictureType::I | PictureType::P
        ) {
            self.older_reference = self.newer_reference;
            self.newer_reference = Some(picture_id);
        }
        self.next_picture = self
            .next_picture
            .checked_add(1)
            .ok_or_else(|| Error::InvalidState("MPEG-2 picture identifier overflow".into()))?;
        self.seen_picture = true;
        Ok(AccessUnitInfo {
            picture_id,
            picture_kind,
            decode_order,
            presentation_order,
            references,
            random_access,
            parameters: parameter_fingerprint(&picture.sequence),
        })
    }
}

impl DependencyAnalyzer for Mpeg2DependencyAnalyzer {
    fn analyze_access_unit(&mut self, packet: &Packet) -> Result<AccessUnitInfo> {
        let stream = parse_stream(&packet.data)?;
        if stream.pictures().len() != 1 {
            return Err(Error::InvalidData(format!(
                "MPEG-2 access-unit packet contains {} pictures; expected one",
                stream.pictures().len()
            )));
        }
        self.analyze_picture(&stream.pictures()[0])
    }
}

/// Analyzes every picture in a parsed elementary stream.
///
/// # Errors
///
/// Returns an error when the stream's prediction graph is incomplete or inconsistent.
pub fn analyze_dependencies(stream: &Mpeg2Stream<'_>) -> Result<Vec<AccessUnitInfo>> {
    let mut analyzer = Mpeg2DependencyAnalyzer {
        current_gop_max_temporal_reference: -1,
        ..Mpeg2DependencyAnalyzer::default()
    };
    stream
        .pictures()
        .iter()
        .map(|picture| analyzer.analyze_picture(picture))
        .collect()
}

/// Propagates an edited presentation interval through the MPEG-2 reference graph.
///
/// A modified I/P picture affects every later coded picture that references it, including B
/// pictures that can precede the reference in presentation order. Propagation naturally stops at
/// an independently coded picture whose reference graph no longer reaches the edit. The result is
/// deliberately codec-local; a future generic renderer can turn these decisions into packet-copy,
/// decode, encode, timestamp, and mux operations.
///
/// # Errors
///
/// Returns an error for an empty/out-of-stream edit interval or an invalid dependency graph.
pub fn plan_smart_render(
    stream: &Mpeg2Stream<'_>,
    edited_presentation_range: Range<i64>,
) -> Result<SmartRenderPlan> {
    if edited_presentation_range.start >= edited_presentation_range.end {
        return Err(Error::InvalidData(
            "MPEG-2 smart-render edit range is empty".into(),
        ));
    }
    let dependencies = analyze_dependencies(stream)?;
    let stream_start = dependencies
        .iter()
        .map(|picture| picture.presentation_order)
        .min()
        .ok_or_else(|| Error::InvalidData("MPEG-2 stream contains no pictures".into()))?;
    let stream_end = dependencies
        .iter()
        .map(|picture| picture.presentation_order)
        .max()
        .and_then(|last| last.checked_add(1))
        .ok_or_else(|| Error::InvalidData("MPEG-2 presentation range overflows".into()))?;
    if edited_presentation_range.start < stream_start || edited_presentation_range.end > stream_end
    {
        return Err(Error::InvalidData(format!(
            "MPEG-2 edit range {edited_presentation_range:?} is outside stream range {stream_start}..{stream_end}"
        )));
    }

    let mut affected = BTreeSet::new();
    let mut pictures = Vec::with_capacity(dependencies.len());
    for (picture, access) in stream.pictures().iter().zip(&dependencies) {
        let directly_edited = edited_presentation_range.contains(&access.presentation_order);
        let affected_references: Vec<_> = access
            .references
            .iter()
            .copied()
            .filter(|reference| affected.contains(reference))
            .collect();
        let disposition = if directly_edited {
            SmartRenderDisposition::EncodeEdited
        } else if affected_references.is_empty() {
            SmartRenderDisposition::Copy
        } else {
            SmartRenderDisposition::BridgeEncode {
                affected_references,
            }
        };
        if disposition != SmartRenderDisposition::Copy {
            affected.insert(access.picture_id);
        }
        pictures.push(SmartRenderPicture {
            picture_id: access.picture_id,
            source_range: picture.source_range.clone(),
            decode_order: access.decode_order,
            presentation_order: access.presentation_order,
            disposition,
        });
    }

    let mut affected_presentation: Vec<_> = pictures
        .iter()
        .filter(|picture| picture.disposition != SmartRenderDisposition::Copy)
        .map(|picture| picture.presentation_order)
        .collect();
    affected_presentation.sort_unstable();
    let encode_presentation_ranges = coalesce_positions(&affected_presentation)?;
    Ok(SmartRenderPlan {
        edited_presentation_range,
        pictures,
        encode_presentation_ranges,
    })
}

fn coalesce_positions(positions: &[i64]) -> Result<Vec<Range<i64>>> {
    let mut ranges: Vec<Range<i64>> = Vec::new();
    for &position in positions {
        let end = position
            .checked_add(1)
            .ok_or_else(|| Error::InvalidData("MPEG-2 presentation range overflows".into()))?;
        if let Some(previous) = ranges.last_mut()
            && previous.end == position
        {
            previous.end = end;
        } else {
            ranges.push(position..end);
        }
    }
    Ok(ranges)
}

fn parameter_fingerprint(sequence: &SequenceParameters) -> ParameterFingerprint {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for value in [
        sequence.width as u64,
        sequence.height as u64,
        sequence.chroma_format as u64,
        u64::from(sequence.progressive_sequence),
        sequence.frame_rate.numerator().cast_unsigned(),
        sequence.frame_rate.denominator().cast_unsigned(),
        sequence.bit_rate.unwrap_or(u64::MAX),
        sequence.vbv_buffer_size_bits,
        u64::from(sequence.profile_and_level_indication),
        u64::from(sequence.display.is_some()),
    ] {
        for byte in value.to_le_bytes() {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }
    if let Some(display) = sequence.display {
        for value in [
            u64::from(display.video_format),
            u64::from(display.display_horizontal_size),
            u64::from(display.display_vertical_size),
            u64::from(display.colour_description.is_some()),
        ] {
            for byte in value.to_le_bytes() {
                hash ^= u64::from(byte);
                hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
            }
        }
        if let Some(colour) = display.colour_description {
            for byte in [
                colour.colour_primaries,
                colour.transfer_characteristics,
                colour.matrix_coefficients,
            ] {
                hash ^= u64::from(byte);
                hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
            }
        }
    }
    for byte in sequence
        .intra_quantizer_matrix
        .iter()
        .chain(&sequence.non_intra_quantizer_matrix)
        .chain(&sequence.chroma_intra_quantizer_matrix)
        .chain(&sequence.chroma_non_intra_quantizer_matrix)
    {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    ParameterFingerprint(hash)
}

#[cfg(test)]
mod tests {
    use super::*;

    const PROGRESSIVE: &[u8] =
        include_bytes!("../../../../testdata/mpeg2/valid/main-ml-progressive-ibp.m2v");

    #[test]
    fn exposes_decode_presentation_and_reference_order() {
        let stream = parse_stream(PROGRESSIVE).unwrap();
        let access = analyze_dependencies(&stream).unwrap();
        assert_eq!(access.len(), 12);
        assert_eq!(access[0].picture_kind, PictureKind::Intra);
        assert!(access[0].references.is_empty());
        assert_eq!(access[1].picture_kind, PictureKind::Predicted);
        assert_eq!(access[1].references, [PictureId(0)]);
        assert_eq!(access[2].picture_kind, PictureKind::Bidirectional);
        assert_eq!(access[2].references, [PictureId(0), PictureId(1)]);
        assert_eq!(access[0].presentation_order, 0);
        assert_eq!(access[1].presentation_order, 3);
        assert_eq!(access[2].presentation_order, 1);
        assert_eq!(access[3].presentation_order, 2);
        assert_eq!(access[0].random_access, RandomAccessKind::Clean);

        let mut presentation: Vec<_> = access.iter().map(|info| info.presentation_order).collect();
        presentation.sort_unstable();
        assert_eq!(presentation, (0..12).collect::<Vec<_>>());
    }

    #[test]
    fn smart_render_propagates_until_the_next_closed_gop() {
        let stream = parse_stream(PROGRESSIVE).unwrap();
        let plan = plan_smart_render(&stream, 0..1).unwrap();
        assert_eq!(plan.encode_presentation_ranges.len(), 1);
        assert_eq!(plan.encode_presentation_ranges[0], 0..4);
        let encoded: Vec<_> = plan
            .pictures
            .iter()
            .filter(|picture| picture.disposition != SmartRenderDisposition::Copy)
            .map(|picture| picture.presentation_order)
            .collect();
        let mut encoded_sorted = encoded;
        encoded_sorted.sort_unstable();
        assert_eq!(encoded_sorted, [0, 1, 2, 3]);
        assert!(plan.pictures.iter().any(|picture| matches!(
            picture.disposition,
            SmartRenderDisposition::BridgeEncode { .. }
        )));
        assert!(
            plan.pictures
                .iter()
                .filter(|picture| picture.presentation_order >= 4)
                .all(|picture| picture.disposition == SmartRenderDisposition::Copy)
        );
    }

    #[test]
    fn smart_render_marks_bidirectional_dependents_before_a_changed_p_reference() {
        let stream = parse_stream(PROGRESSIVE).unwrap();
        let plan = plan_smart_render(&stream, 3..4).unwrap();
        assert_eq!(plan.encode_presentation_ranges.len(), 1);
        assert_eq!(plan.encode_presentation_ranges[0], 1..4);
    }
}
