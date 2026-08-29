use mmrecode_core::{Error, PixelFormat, Rational, Result};

use crate::DIF_BLOCKS_PER_SEQUENCE;

/// The base television system of a DV25 frame.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum DvSystem {
    /// 525-line, approximately 59.94-field/s system.
    System525_60,
    /// 625-line, 50-field/s system.
    System625_50,
}

/// A supported fixed-rate DV25 profile.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct DvProfile {
    /// Television system.
    pub system: DvSystem,
    /// Encoded bytes per frame.
    pub frame_size: usize,
    /// DIF sequences per frame.
    pub dif_sequences: usize,
    /// Visible picture width.
    pub width: usize,
    /// Visible picture height.
    pub height: usize,
    /// Native decoded pixel format.
    pub pixel_format: PixelFormat,
    /// Audio shuffle stride.
    pub audio_stride: usize,
    /// Minimum samples per frame for 48, 44.1, and 32 kHz audio.
    pub audio_min_samples: [usize; 3],
}

impl DvProfile {
    /// Consumer DV25 for the 525/60 system.
    pub const DV25_525_60: Self = Self {
        system: DvSystem::System525_60,
        frame_size: 120_000,
        dif_sequences: 10,
        width: 720,
        height: 480,
        pixel_format: PixelFormat::Yuv411p8,
        audio_stride: 90,
        audio_min_samples: [1_580, 1_452, 1_053],
    };

    /// Consumer DV25 for the 625/50 system.
    pub const DV25_625_50: Self = Self {
        system: DvSystem::System625_50,
        frame_size: 144_000,
        dif_sequences: 12,
        width: 720,
        height: 576,
        pixel_format: PixelFormat::Yuv420p8,
        audio_stride: 108,
        audio_min_samples: [1_896, 1_742, 1_264],
    };

    /// Exact frame rate in frames per second.
    ///
    /// # Panics
    ///
    /// This function does not panic; both standardized constant ratios have
    /// non-zero positive denominators.
    #[must_use]
    pub fn frame_rate(self) -> Rational {
        match self.system {
            DvSystem::System525_60 => Rational::new(30_000, 1_001).expect("constant is valid"),
            DvSystem::System625_50 => Rational::new(25, 1).expect("constant is valid"),
        }
    }

    /// Number of DIF blocks in one frame.
    #[must_use]
    pub const fn block_count(self) -> usize {
        self.dif_sequences * DIF_BLOCKS_PER_SEQUENCE
    }
}

/// Detects the supported DV25 profile from a complete raw frame.
///
/// # Errors
///
/// Returns an error for a truncated header, a non-DV header, an unsupported
/// profile, or a length that disagrees with the header DSF flag.
pub fn detect_profile(data: &[u8]) -> Result<DvProfile> {
    let profile = detect_profile_prefix(data)?;
    if data.len() != profile.frame_size {
        return Err(Error::InvalidData(format!(
            "DV header identifies a {}-byte frame, found {} bytes",
            profile.frame_size,
            data.len()
        )));
    }
    Ok(profile)
}

/// Detects a supported DV25 profile from the first four bytes of a raw frame.
///
/// Unlike [`detect_profile`], this function accepts a prefix or a multi-frame
/// stream and does not validate the complete frame length.
///
/// # Errors
///
/// Returns an error for a truncated prefix or a non-DV header.
pub fn detect_profile_prefix(data: &[u8]) -> Result<DvProfile> {
    if data.len() < 4 {
        return Err(Error::InvalidData(format!(
            "DV profile detection needs 4 header bytes, found {}",
            data.len()
        )));
    }
    if data[0] >> 5 != 0 {
        return Err(Error::InvalidData("first DIF block is not a header".into()));
    }
    let profile = if data[3] & 0x80 == 0 {
        DvProfile::DV25_525_60
    } else {
        DvProfile::DV25_625_50
    };
    Ok(profile)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_both_systems() {
        let mut ntsc = vec![0xff; 120_000];
        ntsc[..4].copy_from_slice(&[0x1f, 0x07, 0x00, 0x3f]);
        assert_eq!(detect_profile(&ntsc).unwrap(), DvProfile::DV25_525_60);

        let mut pal = vec![0xff; 144_000];
        pal[..4].copy_from_slice(&[0x1f, 0x07, 0x00, 0xbf]);
        assert_eq!(detect_profile(&pal).unwrap(), DvProfile::DV25_625_50);
    }

    #[test]
    fn rejects_dsf_length_disagreement() {
        let mut data = vec![0xff; 120_000];
        data[..4].copy_from_slice(&[0x1f, 0x07, 0x00, 0xbf]);
        assert!(detect_profile(&data).is_err());
    }
}
