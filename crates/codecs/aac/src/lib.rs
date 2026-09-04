//! MPEG-4 AAC configuration and transport framing.
//!
//! This first decoder slice owns `AudioSpecificConfig` parsing and raw-access-unit framing. AAC-LC
//! spectral reconstruction will live behind the audio decoder interface added by later slices.

use mmrecode_bitstream::BitReader;
use mmrecode_core::{Error, Result};

const SAMPLE_RATES: [u32; 13] = [
    96_000, 88_200, 64_000, 48_000, 44_100, 32_000, 24_000, 22_050, 16_000, 12_000, 11_025, 8_000,
    7_350,
];

/// Parsed MPEG-4 `AudioSpecificConfig` for a General Audio object type.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AudioSpecificConfig {
    /// Object type carried by the core coder. AAC-LC is type 2.
    pub audio_object_type: u8,
    /// Core sample rate after resolving indexed or explicit frequency syntax.
    pub sample_rate: u32,
    /// MPEG-4 channel-configuration code.
    pub channel_configuration: u8,
    /// Resolved output channel count for standard configurations.
    pub channels: u8,
    /// PCM samples represented by each raw AAC access unit.
    pub samples_per_frame: u16,
    /// Whether an explicit SBR extension was signalled.
    pub sbr_present: bool,
    /// Whether an explicit Parametric Stereo extension was signalled.
    pub ps_present: bool,
    /// Extension/output sample rate when SBR is explicit.
    pub extension_sample_rate: Option<u32>,
}

impl AudioSpecificConfig {
    /// Parses an MPEG-4 `AudioSpecificConfig` byte string.
    ///
    /// The first slice accepts AAC-LC and recognizes explicit HE-AAC signalling so callers can
    /// report it accurately. Program-config elements and non-General-Audio object types remain
    /// unsupported.
    ///
    /// # Errors
    ///
    /// Returns an error for truncated, reserved, or unsupported configurations.
    pub fn parse(data: &[u8]) -> Result<Self> {
        if data.is_empty() {
            return Err(Error::InvalidData("empty AAC AudioSpecificConfig".into()));
        }
        let mut reader = BitReader::new(data);
        let signalled_type = read_object_type(&mut reader)?;
        let sample_rate = read_sample_rate(&mut reader)?;
        let channel_configuration = u8::try_from(reader.read_bits(4)?)
            .map_err(|_| Error::InvalidData("AAC channel configuration overflows".into()))?;
        let mut audio_object_type = signalled_type;
        let mut sbr_present = false;
        let mut ps_present = false;
        let mut extension_sample_rate = None;
        if matches!(signalled_type, 5 | 29) {
            sbr_present = true;
            ps_present = signalled_type == 29;
            extension_sample_rate = Some(read_sample_rate(&mut reader)?);
            audio_object_type = read_object_type(&mut reader)?;
            if audio_object_type == 22 {
                let _extension_channel_configuration = reader.read_bits(4)?;
            }
        }
        if audio_object_type != 2 {
            return Err(Error::Unsupported(format!(
                "AAC audio object type {audio_object_type}; only AAC-LC (2) is implemented"
            )));
        }
        if channel_configuration == 0 {
            return Err(Error::Unsupported(
                "AAC program_config_element channel layouts are not implemented".into(),
            ));
        }
        let channels = channel_count(channel_configuration).ok_or_else(|| {
            Error::Unsupported(format!(
                "AAC channel configuration {channel_configuration} is not implemented"
            ))
        })?;
        let frame_length_flag = reader.read_bit()?;
        if reader.read_bit()? {
            reader.skip_bits(14)?;
        }
        let _extension_flag = reader.read_bit()?;
        Ok(Self {
            audio_object_type,
            sample_rate,
            channel_configuration,
            channels,
            samples_per_frame: if frame_length_flag { 960 } else { 1_024 },
            sbr_present,
            ps_present,
            extension_sample_rate,
        })
    }

    /// Builds a seven-byte ADTS header for one raw AAC-LC access unit.
    ///
    /// # Errors
    ///
    /// Returns an error when the configuration cannot be represented by ADTS or the frame is too
    /// large for its 13-bit length field.
    pub fn adts_header(&self, payload_length: usize) -> Result<[u8; 7]> {
        if self.audio_object_type != 2 || self.sbr_present || self.ps_present {
            return Err(Error::Unsupported(
                "ADTS bridge currently supports plain AAC-LC only".into(),
            ));
        }
        let frequency_index = SAMPLE_RATES
            .iter()
            .position(|rate| *rate == self.sample_rate)
            .ok_or_else(|| {
                Error::Unsupported(format!(
                    "AAC sample rate {} cannot be represented in ADTS",
                    self.sample_rate
                ))
            })?;
        let frame_length = payload_length
            .checked_add(7)
            .filter(|length| *length <= 0x1fff)
            .ok_or_else(|| Error::InvalidData("AAC access unit is too large for ADTS".into()))?;
        let channel = usize::from(self.channel_configuration);
        let profile = 1_usize; // AAC-LC object type minus one.
        let byte_2 = u8::try_from((profile << 6) | (frequency_index << 2) | (channel >> 2))
            .map_err(|_| Error::InvalidData("AAC ADTS header byte overflows".into()))?;
        let byte_3 = u8::try_from(((channel & 3) << 6) | (frame_length >> 11))
            .map_err(|_| Error::InvalidData("AAC ADTS header byte overflows".into()))?;
        let byte_4 = u8::try_from((frame_length >> 3) & 0xff)
            .map_err(|_| Error::InvalidData("AAC ADTS header byte overflows".into()))?;
        let byte_5 = u8::try_from(((frame_length & 7) << 5) | 0x1f)
            .map_err(|_| Error::InvalidData("AAC ADTS header byte overflows".into()))?;
        Ok([0xff, 0xf1, byte_2, byte_3, byte_4, byte_5, 0xfc])
    }
}

fn read_object_type(reader: &mut BitReader<'_>) -> Result<u8> {
    let value = u8::try_from(reader.read_bits(5)?).expect("five-bit object type fits u8");
    if value == 31 {
        let extension = u8::try_from(reader.read_bits(6)?).expect("six-bit extension fits u8");
        32_u8
            .checked_add(extension)
            .ok_or_else(|| Error::InvalidData("AAC extended audio object type overflows".into()))
    } else if value == 0 {
        Err(Error::InvalidData(
            "AAC audio object type zero is reserved".into(),
        ))
    } else {
        Ok(value)
    }
}

fn read_sample_rate(reader: &mut BitReader<'_>) -> Result<u32> {
    let index = usize::try_from(reader.read_bits(4)?).expect("four-bit rate index fits usize");
    if index == 15 {
        let explicit =
            u32::try_from(reader.read_bits(24)?).expect("24-bit explicit sample rate fits u32");
        return (explicit > 0)
            .then_some(explicit)
            .ok_or_else(|| Error::InvalidData("AAC explicit sample rate is zero".into()));
    }
    SAMPLE_RATES
        .get(index)
        .copied()
        .ok_or_else(|| Error::InvalidData(format!("reserved AAC sample-rate index {index}")))
}

const fn channel_count(configuration: u8) -> Option<u8> {
    match configuration {
        1 => Some(1),
        2 => Some(2),
        3 => Some(3),
        4 => Some(4),
        5 => Some(5),
        6 => Some(6),
        7 => Some(8),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_common_aac_lc_configuration() {
        let config = AudioSpecificConfig::parse(&[0x12, 0x10]).unwrap();
        assert_eq!(config.audio_object_type, 2);
        assert_eq!(config.sample_rate, 44_100);
        assert_eq!(config.channels, 2);
        assert_eq!(config.samples_per_frame, 1_024);
        assert!(!config.sbr_present);
    }

    #[test]
    fn creates_adts_header_for_raw_access_unit() {
        let config = AudioSpecificConfig::parse(&[0x11, 0x90]).unwrap();
        assert_eq!(config.sample_rate, 48_000);
        assert_eq!(config.channels, 2);
        assert_eq!(
            config.adts_header(100).unwrap(),
            [0xff, 0xf1, 0x4c, 0x80, 0x0d, 0x7f, 0xfc]
        );
    }

    #[test]
    fn rejects_truncated_and_unsupported_configurations() {
        assert!(AudioSpecificConfig::parse(&[]).is_err());
        assert!(AudioSpecificConfig::parse(&[0x12]).is_err());
        assert!(AudioSpecificConfig::parse(&[0x0a, 0x10]).is_err());
        assert!(AudioSpecificConfig::parse(&[0x12, 0x00]).is_err());
    }
}
