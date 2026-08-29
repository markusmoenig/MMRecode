use std::ops::Range;

use mmrecode_core::{Error, Result};

use crate::{DvPack, DvProfile, detect_profile, packs::collect_packs};

/// Size of every DV DIF block in bytes.
pub const DIF_BLOCK_SIZE: usize = 80;
/// Number of DIF blocks in one DV sequence.
pub const DIF_BLOCKS_PER_SEQUENCE: usize = 150;

/// Physical section encoded by a DIF block.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum DifSection {
    /// One sequence header.
    Header,
    /// Subcode, including timecode packs.
    Subcode,
    /// Video auxiliary packs.
    Vaux,
    /// One embedded-audio block.
    Audio,
    /// One compressed-video block.
    Video,
    /// Reserved section identifier.
    Reserved(u8),
}

impl DifSection {
    const fn from_id(value: u8) -> Self {
        match value >> 5 {
            0 => Self::Header,
            1 => Self::Subcode,
            2 => Self::Vaux,
            3 => Self::Audio,
            4 => Self::Video,
            other => Self::Reserved(other),
        }
    }
}

/// The three-byte identifier at the start of a DIF block.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct DifBlockId {
    /// Physical DIF section.
    pub section: DifSection,
    /// DIF sequence number within the frame.
    pub sequence: u8,
    /// DIF channel bit carried in the identifier.
    pub channel: u8,
    /// Section-local block number.
    pub block_number: u8,
}

impl DifBlockId {
    fn parse(bytes: &[u8]) -> Self {
        Self {
            section: DifSection::from_id(bytes[0]),
            sequence: bytes[1] >> 4,
            channel: (bytes[1] >> 3) & 1,
            block_number: bytes[2],
        }
    }
}

/// An indexed DIF block in a parsed frame.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DifBlock {
    /// Decoded block identifier.
    pub id: DifBlockId,
    /// Byte range in the original frame.
    pub range: Range<usize>,
}

/// Category of a structural DV issue.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DvIssueKind {
    /// A block appeared in the wrong physical section position.
    UnexpectedSection {
        /// Required section at this position.
        expected: DifSection,
        /// Section found in the identifier.
        actual: DifSection,
    },
    /// DIF sequence number disagrees with physical placement.
    UnexpectedSequence {
        /// Required sequence number.
        expected: u8,
        /// Sequence number found in the identifier.
        actual: u8,
    },
    /// Section-local DIF block number disagrees with physical placement.
    UnexpectedBlockNumber {
        /// Required block number.
        expected: u8,
        /// Block number found in the identifier.
        actual: u8,
    },
}

/// One localized structural issue discovered while indexing a frame.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DvIssue {
    /// Absolute byte offset of the affected DIF block.
    pub offset: usize,
    /// Structural problem.
    pub kind: DvIssueKind,
}

/// A borrowed, indexed raw DV25 frame.
#[derive(Clone, Debug)]
pub struct DvFrame<'a> {
    data: &'a [u8],
    profile: DvProfile,
    blocks: Vec<DifBlock>,
    packs: Vec<DvPack>,
    issues: Vec<DvIssue>,
}

impl<'a> DvFrame<'a> {
    /// Encoded frame bytes.
    #[must_use]
    pub const fn data(&self) -> &'a [u8] {
        self.data
    }

    /// Detected DV25 profile.
    #[must_use]
    pub const fn profile(&self) -> DvProfile {
        self.profile
    }

    /// DIF blocks in physical frame order.
    #[must_use]
    pub fn blocks(&self) -> &[DifBlock] {
        &self.blocks
    }

    /// All discovered subcode, VAUX, and AAUX packs.
    #[must_use]
    pub fn packs(&self) -> &[DvPack] {
        &self.packs
    }

    /// Localized structural issues. A damaged frame remains inspectable.
    #[must_use]
    pub fn issues(&self) -> &[DvIssue] {
        &self.issues
    }

    /// Returns a block's complete 80 encoded bytes.
    #[must_use]
    pub fn block_bytes(&self, block: &DifBlock) -> &'a [u8] {
        &self.data[block.range.clone()]
    }

    /// Requires a canonical, issue-free DIF layout.
    ///
    /// # Errors
    ///
    /// Returns the first structural issue with its absolute byte offset.
    pub fn validate_strict(&self) -> Result<()> {
        if let Some(issue) = self.issues.first() {
            return Err(Error::InvalidData(format!(
                "invalid DIF block at byte {}: {:?}",
                issue.offset, issue.kind
            )));
        }
        Ok(())
    }
}

/// Parses and indexes one complete raw DV25 frame.
///
/// Structural DIF identifier damage is recorded in [`DvFrame::issues`] rather
/// than making the rest of the frame uninspectable.
///
/// # Errors
///
/// Returns an error when profile detection or the fixed frame length fails.
pub fn parse_frame(data: &[u8]) -> Result<DvFrame<'_>> {
    let profile = detect_profile(data)?;
    let mut blocks = Vec::with_capacity(profile.block_count());
    let mut issues = Vec::new();
    for index in 0..profile.block_count() {
        let offset = index * DIF_BLOCK_SIZE;
        let bytes = &data[offset..offset + DIF_BLOCK_SIZE];
        let id = DifBlockId::parse(bytes);
        let physical_sequence = index / DIF_BLOCKS_PER_SEQUENCE;
        let sequence_position = index % DIF_BLOCKS_PER_SEQUENCE;
        let (expected_section, expected_number) = expected_id(sequence_position);
        if id.section != expected_section {
            issues.push(DvIssue {
                offset,
                kind: DvIssueKind::UnexpectedSection {
                    expected: expected_section,
                    actual: id.section,
                },
            });
        }
        if usize::from(id.sequence) != physical_sequence {
            issues.push(DvIssue {
                offset,
                kind: DvIssueKind::UnexpectedSequence {
                    expected: u8::try_from(physical_sequence).unwrap_or(u8::MAX),
                    actual: id.sequence,
                },
            });
        }
        if id.block_number != expected_number {
            issues.push(DvIssue {
                offset,
                kind: DvIssueKind::UnexpectedBlockNumber {
                    expected: expected_number,
                    actual: id.block_number,
                },
            });
        }
        blocks.push(DifBlock {
            id,
            range: offset..offset + DIF_BLOCK_SIZE,
        });
    }
    let packs = collect_packs(data, &blocks);
    Ok(DvFrame {
        data,
        profile,
        blocks,
        packs,
        issues,
    })
}

#[allow(clippy::cast_possible_truncation)]
const fn expected_id(position: usize) -> (DifSection, u8) {
    match position {
        0 => (DifSection::Header, 0),
        1..=2 => (DifSection::Subcode, (position - 1) as u8),
        3..=5 => (DifSection::Vaux, (position - 3) as u8),
        _ => {
            let av_position = position - 6;
            let within = av_position % 16;
            if within == 0 {
                (DifSection::Audio, (av_position / 16) as u8)
            } else {
                (
                    DifSection::Video,
                    (av_position - av_position / 16 - 1) as u8,
                )
            }
        }
    }
}

#[cfg(test)]
pub(crate) fn synthetic_frame(profile: DvProfile) -> Vec<u8> {
    let mut data = vec![0xff; profile.frame_size];
    for index in 0..profile.block_count() {
        let offset = index * DIF_BLOCK_SIZE;
        let sequence = index / DIF_BLOCKS_PER_SEQUENCE;
        let position = index % DIF_BLOCKS_PER_SEQUENCE;
        let (section, number) = expected_id(position);
        let section_bits = match section {
            DifSection::Header => 0x00,
            DifSection::Subcode => 0x20,
            DifSection::Vaux => 0x40,
            DifSection::Audio => 0x60,
            DifSection::Video => 0x80,
            DifSection::Reserved(_) => unreachable!(),
        };
        data[offset] = section_bits | if position <= 2 { 0x1f } else { 0x16 };
        data[offset + 1] = u8::try_from(sequence).unwrap() << 4 | 0x07;
        data[offset + 2] = number;
    }
    data[3] = match profile.system {
        crate::DvSystem::System525_60 => 0x3f,
        crate::DvSystem::System625_50 => 0xbf,
    };
    data
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn indexes_every_block_in_both_profiles() {
        for profile in [DvProfile::DV25_525_60, DvProfile::DV25_625_50] {
            let data = synthetic_frame(profile);
            let frame = parse_frame(&data).unwrap();
            assert_eq!(frame.blocks().len(), profile.block_count());
            assert!(frame.issues().is_empty());
            frame.validate_strict().unwrap();
        }
    }

    #[test]
    fn reports_localized_identifier_damage() {
        let mut data = synthetic_frame(DvProfile::DV25_525_60);
        data[7 * DIF_BLOCK_SIZE] = 0x76;
        let frame = parse_frame(&data).unwrap();
        assert_eq!(frame.issues()[0].offset, 7 * DIF_BLOCK_SIZE);
        assert!(matches!(
            frame.issues()[0].kind,
            DvIssueKind::UnexpectedSection { .. }
        ));
        assert!(frame.validate_strict().is_err());
    }
}
