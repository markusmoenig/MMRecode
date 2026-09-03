use std::ops::Range;

use mmrecode_core::{Error, Result};

/// H.264 NAL unit type from the one-byte NAL header.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
#[non_exhaustive]
pub enum NalUnitType {
    /// Non-IDR coded slice.
    CodedSlice,
    /// Coded slice data partition A.
    DataPartitionA,
    /// Coded slice data partition B.
    DataPartitionB,
    /// Coded slice data partition C.
    DataPartitionC,
    /// IDR coded slice.
    IdrSlice,
    /// Supplemental enhancement information.
    Sei,
    /// Sequence parameter set.
    Sps,
    /// Picture parameter set.
    Pps,
    /// Access-unit delimiter.
    AccessUnitDelimiter,
    /// End of sequence.
    EndOfSequence,
    /// End of stream.
    EndOfStream,
    /// Filler data.
    Filler,
    /// Sequence parameter set extension.
    SpsExtension,
    /// Prefix NAL unit.
    Prefix,
    /// Subset sequence parameter set.
    SubsetSps,
    /// Other specified or reserved type.
    Other(u8),
}

impl NalUnitType {
    /// Maps the five-bit syntax value to a typed NAL kind.
    #[must_use]
    pub const fn from_value(value: u8) -> Self {
        match value {
            1 => Self::CodedSlice,
            2 => Self::DataPartitionA,
            3 => Self::DataPartitionB,
            4 => Self::DataPartitionC,
            5 => Self::IdrSlice,
            6 => Self::Sei,
            7 => Self::Sps,
            8 => Self::Pps,
            9 => Self::AccessUnitDelimiter,
            10 => Self::EndOfSequence,
            11 => Self::EndOfStream,
            12 => Self::Filler,
            13 => Self::SpsExtension,
            14 => Self::Prefix,
            15 => Self::SubsetSps,
            other => Self::Other(other),
        }
    }

    /// Returns the five-bit syntax value.
    #[must_use]
    pub const fn value(self) -> u8 {
        match self {
            Self::CodedSlice => 1,
            Self::DataPartitionA => 2,
            Self::DataPartitionB => 3,
            Self::DataPartitionC => 4,
            Self::IdrSlice => 5,
            Self::Sei => 6,
            Self::Sps => 7,
            Self::Pps => 8,
            Self::AccessUnitDelimiter => 9,
            Self::EndOfSequence => 10,
            Self::EndOfStream => 11,
            Self::Filler => 12,
            Self::SpsExtension => 13,
            Self::Prefix => 14,
            Self::SubsetSps => 15,
            Self::Other(value) => value,
        }
    }
}

/// Parsed NAL header.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct NalUnitHeader {
    /// Reference priority from `nal_ref_idc`.
    pub reference_idc: u8,
    /// NAL unit payload kind.
    pub unit_type: NalUnitType,
}

impl NalUnitHeader {
    /// Parses the first byte of a NAL unit.
    ///
    /// # Errors
    ///
    /// Returns an error when `forbidden_zero_bit` is set.
    pub fn parse(byte: u8) -> Result<Self> {
        if byte & 0x80 != 0 {
            return Err(Error::InvalidData("H.264 forbidden_zero_bit is set".into()));
        }
        Ok(Self {
            reference_idc: (byte >> 5) & 0x03,
            unit_type: NalUnitType::from_value(byte & 0x1f),
        })
    }
}

/// One NAL unit without its Annex-B start code or `avcC` length field.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NalUnit<'a> {
    /// Byte range in the supplied representation.
    pub source_range: Range<usize>,
    /// Parsed header.
    pub header: NalUnitHeader,
    /// Full NAL bytes including the one-byte header.
    pub data: &'a [u8],
}

/// Splits Annex-B data into NAL units.
///
/// # Errors
///
/// Returns an error for a missing start code, empty payload, or invalid NAL header.
pub fn annex_b_nal_units(data: &[u8]) -> Result<Vec<NalUnit<'_>>> {
    let starts = find_start_codes(data);
    if starts.is_empty() {
        return Err(Error::InvalidData(
            "H.264 Annex-B stream has no start code".into(),
        ));
    }
    let mut units = Vec::with_capacity(starts.len());
    for (index, &(offset, prefix)) in starts.iter().enumerate() {
        let start = offset + prefix;
        let mut end = starts.get(index + 1).map_or(data.len(), |next| next.0);
        while end > start && data[end - 1] == 0 {
            end -= 1;
        }
        if start == end {
            continue;
        }
        units.push(NalUnit {
            source_range: start..end,
            header: NalUnitHeader::parse(data[start])?,
            data: &data[start..end],
        });
    }
    if units.is_empty() {
        return Err(Error::InvalidData(
            "H.264 Annex-B stream has no NAL payload".into(),
        ));
    }
    Ok(units)
}

fn find_start_codes(data: &[u8]) -> Vec<(usize, usize)> {
    let mut starts = Vec::new();
    let mut index = 0;
    while index + 3 <= data.len() {
        if data[index..].starts_with(&[0, 0, 1]) {
            starts.push((index, 3));
            index += 3;
        } else if data[index..].starts_with(&[0, 0, 0, 1]) {
            starts.push((index, 4));
            index += 4;
        } else {
            index += 1;
        }
    }
    starts
}

/// Splits an ISO-BMFF/`avcC` length-prefixed sample into NAL units.
///
/// # Errors
///
/// Returns an error for an invalid length width, truncation, empty NAL, or invalid header.
pub fn length_prefixed_nal_units(data: &[u8], length_size: u8) -> Result<Vec<NalUnit<'_>>> {
    if !(1..=4).contains(&length_size) {
        return Err(Error::InvalidData(
            "H.264 NAL length size must be 1 through 4".into(),
        ));
    }
    let mut units = Vec::new();
    let mut offset = 0;
    while offset < data.len() {
        let length_end = offset + usize::from(length_size);
        let length_bytes = data
            .get(offset..length_end)
            .ok_or_else(|| Error::InvalidData("truncated H.264 length-prefixed NAL size".into()))?;
        let mut length = 0_usize;
        for byte in length_bytes {
            length = length
                .checked_mul(256)
                .and_then(|value| value.checked_add(usize::from(*byte)))
                .ok_or_else(|| Error::InvalidData("H.264 NAL length overflows".into()))?;
        }
        if length == 0 {
            return Err(Error::InvalidData("zero-length H.264 NAL unit".into()));
        }
        let start = length_end;
        let end = start
            .checked_add(length)
            .filter(|end| *end <= data.len())
            .ok_or_else(|| Error::InvalidData("truncated H.264 NAL unit".into()))?;
        units.push(NalUnit {
            source_range: start..end,
            header: NalUnitHeader::parse(data[start])?,
            data: &data[start..end],
        });
        offset = end;
    }
    Ok(units)
}

/// Converts length-prefixed NAL units into four-byte-start-code Annex-B form.
///
/// # Errors
///
/// Returns an error when the input sample is malformed or the output size overflows.
pub fn nal_units_to_annex_b(data: &[u8], length_size: u8) -> Result<Vec<u8>> {
    let units = length_prefixed_nal_units(data, length_size)?;
    let capacity = units.iter().try_fold(0_usize, |size, unit| {
        size.checked_add(4 + unit.data.len())
            .ok_or_else(|| Error::InvalidData("Annex-B output size overflows".into()))
    })?;
    let mut output = Vec::with_capacity(capacity);
    for unit in units {
        output.extend_from_slice(&[0, 0, 0, 1]);
        output.extend_from_slice(unit.data);
    }
    Ok(output)
}

/// Removes H.264 emulation-prevention bytes from an EBSP payload.
#[must_use]
pub fn remove_emulation_prevention(data: &[u8]) -> Vec<u8> {
    let mut output = Vec::with_capacity(data.len());
    let mut zero_count = 0_u8;
    for &byte in data {
        if zero_count >= 2 && byte == 0x03 {
            zero_count = 0;
            continue;
        }
        output.push(byte);
        zero_count = if byte == 0 {
            zero_count.saturating_add(1)
        } else {
            0
        };
    }
    output
}

/// ISO/IEC 14496-15 AVC decoder configuration record (`avcC`).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AvcDecoderConfigurationRecord {
    /// AVC profile indication copied from the active SPS.
    pub profile_indication: u8,
    /// Profile compatibility flags.
    pub profile_compatibility: u8,
    /// AVC level indication.
    pub level_indication: u8,
    /// Number of bytes in each sample NAL length field.
    pub length_size: u8,
    /// Sequence parameter-set NAL units, including NAL headers.
    pub sequence_parameter_sets: Vec<Vec<u8>>,
    /// Picture parameter-set NAL units, including NAL headers.
    pub picture_parameter_sets: Vec<Vec<u8>>,
}

impl AvcDecoderConfigurationRecord {
    /// Parses an AVC decoder configuration record.
    ///
    /// # Errors
    ///
    /// Returns an error when the record or a parameter-set array is malformed.
    pub fn parse(data: &[u8]) -> Result<Self> {
        if data.len() < 7 || data[0] != 1 {
            return Err(Error::InvalidData(
                "invalid or truncated avcC record".into(),
            ));
        }
        let length_size = (data[4] & 0x03) + 1;
        let sps_count = usize::from(data[5] & 0x1f);
        let mut offset = 6;
        let sequence_parameter_sets = read_parameter_sets(data, &mut offset, sps_count, "SPS")?;
        let pps_count = usize::from(*data.get(offset).ok_or_else(|| {
            Error::InvalidData("truncated avcC picture-parameter-set count".into())
        })?);
        offset += 1;
        let picture_parameter_sets = read_parameter_sets(data, &mut offset, pps_count, "PPS")?;
        Ok(Self {
            profile_indication: data[1],
            profile_compatibility: data[2],
            level_indication: data[3],
            length_size,
            sequence_parameter_sets,
            picture_parameter_sets,
        })
    }

    /// Produces Annex-B parameter-set bytes suitable for a byte-stream decoder.
    #[must_use]
    pub fn parameter_sets_annex_b(&self) -> Vec<u8> {
        let mut output = Vec::new();
        for parameter_set in self
            .sequence_parameter_sets
            .iter()
            .chain(&self.picture_parameter_sets)
        {
            output.extend_from_slice(&[0, 0, 0, 1]);
            output.extend_from_slice(parameter_set);
        }
        output
    }
}

fn read_parameter_sets(
    data: &[u8],
    offset: &mut usize,
    count: usize,
    name: &str,
) -> Result<Vec<Vec<u8>>> {
    let mut sets = Vec::with_capacity(count);
    for _ in 0..count {
        let header = data
            .get(*offset..*offset + 2)
            .ok_or_else(|| Error::InvalidData(format!("truncated avcC {name} length")))?;
        let length = usize::from(u16::from_be_bytes([header[0], header[1]]));
        *offset += 2;
        let end = offset
            .checked_add(length)
            .filter(|end| *end <= data.len())
            .ok_or_else(|| Error::InvalidData(format!("truncated avcC {name}")))?;
        if length == 0 {
            return Err(Error::InvalidData(format!("empty avcC {name}")));
        }
        sets.push(data[*offset..end].to_vec());
        *offset = end;
    }
    Ok(sets)
}

#[cfg(test)]
mod tests {
    use super::{
        AvcDecoderConfigurationRecord, NalUnitType, annex_b_nal_units, length_prefixed_nal_units,
        nal_units_to_annex_b, remove_emulation_prevention,
    };

    #[test]
    fn recognizes_three_and_four_byte_annex_b_prefixes() {
        let bytes = [0, 0, 0, 1, 0x67, 1, 2, 0, 0, 1, 0x68, 3];
        let units = annex_b_nal_units(&bytes).unwrap();
        assert_eq!(units.len(), 2);
        assert_eq!(units[0].header.unit_type, NalUnitType::Sps);
        assert_eq!(units[1].header.unit_type, NalUnitType::Pps);
    }

    #[test]
    fn converts_length_prefixes_and_rejects_truncation() {
        let bytes = [0, 2, 0x65, 0x88, 0, 1, 0x06];
        let units = length_prefixed_nal_units(&bytes, 2).unwrap();
        assert_eq!(units.len(), 2);
        assert_eq!(units[0].header.unit_type, NalUnitType::IdrSlice);
        assert_eq!(nal_units_to_annex_b(&bytes, 2).unwrap()[..4], [0, 0, 0, 1]);
        assert!(length_prefixed_nal_units(&bytes[..6], 2).is_err());
    }

    #[test]
    fn parses_avcc_and_builds_parameter_set_preamble() {
        let bytes = [1, 100, 0, 40, 0xff, 0xe1, 0, 2, 0x67, 0x64, 1, 0, 1, 0x68];
        let record = AvcDecoderConfigurationRecord::parse(&bytes).unwrap();
        assert_eq!(record.length_size, 4);
        assert_eq!(record.sequence_parameter_sets, vec![vec![0x67, 0x64]]);
        assert_eq!(record.picture_parameter_sets, vec![vec![0x68]]);
        assert_eq!(&record.parameter_sets_annex_b()[..4], &[0, 0, 0, 1]);
    }

    #[test]
    fn removes_only_emulation_prevention_bytes() {
        assert_eq!(
            remove_emulation_prevention(&[0, 0, 3, 1, 0, 0, 3, 3, 4]),
            [0, 0, 1, 0, 0, 3, 4]
        );
    }
}
