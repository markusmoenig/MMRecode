//! Uncompressed audio frame storage.

use crate::FrameTiming;

/// The in-memory representation of one decoded audio sample.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
#[non_exhaustive]
pub enum AudioSampleFormat {
    /// Signed 16-bit interleaved integer samples in native byte order.
    I16Interleaved,
}

/// An owned block of uncompressed audio.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AudioFrame {
    /// Sample storage format.
    pub format: AudioSampleFormat,
    /// Samples per second for each channel.
    pub sample_rate: u32,
    /// Number of interleaved channels.
    pub channels: u16,
    /// Number of samples per channel.
    pub samples_per_channel: usize,
    /// Interleaved sample data.
    pub samples: Vec<i16>,
    /// Presentation timing.
    pub timing: FrameTiming,
}

impl AudioFrame {
    /// Validates that the sample buffer agrees with the declared layout.
    ///
    /// # Errors
    ///
    /// Returns an error when the channel count is zero or the sample count and storage disagree.
    pub fn validate(&self) -> crate::Result<()> {
        if self.channels == 0 {
            return Err(crate::Error::InvalidData(
                "an audio frame must contain at least one channel".into(),
            ));
        }
        let expected = self
            .samples_per_channel
            .checked_mul(usize::from(self.channels))
            .ok_or_else(|| crate::Error::InvalidData("audio sample count overflow".into()))?;
        if self.samples.len() != expected {
            return Err(crate::Error::InvalidData(format!(
                "audio frame declares {expected} samples but stores {}",
                self.samples.len()
            )));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_interleaved_layout() {
        let frame = AudioFrame {
            format: AudioSampleFormat::I16Interleaved,
            sample_rate: 48_000,
            channels: 2,
            samples_per_channel: 2,
            samples: vec![1, -1, 2, -2],
            timing: FrameTiming::default(),
        };
        assert!(frame.validate().is_ok());
    }

    #[test]
    fn rejects_mismatched_storage() {
        let frame = AudioFrame {
            format: AudioSampleFormat::I16Interleaved,
            sample_rate: 48_000,
            channels: 2,
            samples_per_channel: 2,
            samples: vec![1, -1],
            timing: FrameTiming::default(),
        };
        assert!(frame.validate().is_err());
    }
}
