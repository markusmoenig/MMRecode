use crate::{DifBlock, DifSection};

/// DV audio quantization identified by an AAUX source pack.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum AudioQuantization {
    /// 16-bit linear PCM.
    Linear16,
    /// 12-bit nonlinear PCM packed as two channels in three bytes.
    Nonlinear12,
    /// A reserved quantization value.
    Reserved(u8),
}

/// Parsed AAUX audio-source information.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct AudioSource {
    /// Samples in this frame in addition to the profile minimum.
    pub sample_count_delta: u8,
    /// Samples per second.
    pub sample_rate: Option<u32>,
    /// Audio quantization.
    pub quantization: AudioQuantization,
    /// Raw five-bit audio source type.
    pub source_type: u8,
    /// Number of signalled channels when the source type is known.
    pub channels: Option<u8>,
    /// Whether audio is locked to video timing.
    pub locked: bool,
}

/// SMPTE timecode carried in a DV subcode pack.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct Timecode {
    /// Hour field.
    pub hours: u8,
    /// Minute field.
    pub minutes: u8,
    /// Second field.
    pub seconds: u8,
    /// Frame field.
    pub frames: u8,
    /// Drop-frame flag. It is meaningful only for 525/60 material.
    pub drop_frame: bool,
}

/// Interpreted contents of a five-byte DV pack.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum DvPackData {
    /// Pack type `0x13`.
    Timecode(Timecode),
    /// Pack type `0x50`.
    AudioSource(AudioSource),
    /// A pack retained without a typed interpretation.
    Raw,
}

/// A five-byte metadata pack located inside a DIF block.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct DvPack {
    /// Absolute byte offset in the raw frame.
    pub offset: usize,
    /// DIF section containing the pack.
    pub section: DifSection,
    /// Raw pack bytes, including the pack-type byte.
    pub bytes: [u8; 5],
    /// Typed interpretation where available.
    pub data: DvPackData,
}

pub(crate) fn collect_packs(data: &[u8], blocks: &[DifBlock]) -> Vec<DvPack> {
    let mut packs = Vec::new();
    for block in blocks {
        let offsets: &[usize] = match block.id.section {
            DifSection::Subcode => &[6, 14, 22, 30, 38, 46],
            DifSection::Vaux => &[3, 8, 13, 18, 23, 28, 33, 38, 43, 48, 53, 58, 63, 68, 73],
            DifSection::Audio => &[3],
            _ => &[],
        };
        for &relative in offsets {
            let offset = block.range.start + relative;
            let bytes: [u8; 5] = data[offset..offset + 5]
                .try_into()
                .expect("pack is inside an indexed DIF block");
            if bytes[0] == 0xff {
                continue;
            }
            packs.push(DvPack {
                offset,
                section: block.id.section,
                bytes,
                data: parse_pack(bytes),
            });
        }
    }
    packs
}

fn parse_pack(bytes: [u8; 5]) -> DvPackData {
    match bytes[0] {
        0x13 => parse_timecode(bytes).map_or(DvPackData::Raw, DvPackData::Timecode),
        0x50 => DvPackData::AudioSource(parse_audio_source(bytes)),
        _ => DvPackData::Raw,
    }
}

fn parse_timecode(bytes: [u8; 5]) -> Option<Timecode> {
    let frames = bcd(bytes[1], 0x30)?;
    let seconds = bcd(bytes[2], 0x70)?;
    let minutes = bcd(bytes[3], 0x70)?;
    let hours = bcd(bytes[4], 0x30)?;
    (seconds < 60 && minutes < 60 && hours < 24).then_some(Timecode {
        hours,
        minutes,
        seconds,
        frames,
        drop_frame: bytes[1] & 0x40 != 0,
    })
}

fn bcd(value: u8, tens_mask: u8) -> Option<u8> {
    let units = value & 0x0f;
    let tens = (value & tens_mask) >> 4;
    (units < 10).then_some(tens * 10 + units)
}

const fn parse_audio_source(bytes: [u8; 5]) -> AudioSource {
    let rate_index = (bytes[4] >> 3) & 0x07;
    let quantization_index = bytes[4] & 0x07;
    let source_type = bytes[3] & 0x1f;
    AudioSource {
        sample_count_delta: bytes[1] & 0x3f,
        sample_rate: match rate_index {
            0 => Some(48_000),
            1 => Some(44_100),
            2 => Some(32_000),
            _ => None,
        },
        quantization: match quantization_index {
            0 => AudioQuantization::Linear16,
            1 => AudioQuantization::Nonlinear12,
            other => AudioQuantization::Reserved(other),
        },
        source_type,
        channels: match source_type {
            0 => Some(2),
            2 => Some(4),
            3 => Some(8),
            _ => None,
        },
        locked: bytes[1] & 0x80 == 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_timecode_bcd_and_drop_frame() {
        assert_eq!(
            parse_pack([0x13, 0x52, 0x56, 0x34, 0x12]),
            DvPackData::Timecode(Timecode {
                hours: 12,
                minutes: 34,
                seconds: 56,
                frames: 12,
                drop_frame: true,
            })
        );
    }

    #[test]
    fn parses_audio_source() {
        let DvPackData::AudioSource(source) = parse_pack([0x50, 0x05, 0x00, 0x00, 0x00]) else {
            panic!("expected audio source");
        };
        assert_eq!(source.sample_count_delta, 5);
        assert_eq!(source.sample_rate, Some(48_000));
        assert_eq!(source.quantization, AudioQuantization::Linear16);
        assert_eq!(source.channels, Some(2));
    }
}
