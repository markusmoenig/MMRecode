//! MPEG audio elementary-stream framing and timing.
//!
//! The initial slice parses MPEG-1 Audio Layer II frames for transport pass-through. It does not
//! decode or encode audio samples.

use std::ops::Range;

use mmrecode_core::{Error, Result};

const LAYER2_BIT_RATES_KBIT: [u16; 16] = [
    0, 32, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320, 384, 0,
];

/// MPEG-1 Audio Layer II channel mode.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ChannelMode {
    /// Two independent channels.
    Stereo,
    /// Stereo with some jointly coded subbands.
    JointStereo,
    /// Two unrelated mono channels.
    DualChannel,
    /// One mono channel.
    Mono,
}

/// Parsed MPEG-1 Audio Layer II frame header.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Layer2Header {
    /// Coded bitrate in bits per second.
    pub bit_rate: u32,
    /// Audio sample rate in hertz.
    pub sample_rate: u32,
    /// Number of coded channels.
    pub channels: u8,
    /// Channel-mode field.
    pub channel_mode: ChannelMode,
    /// Whether a 16-bit CRC follows the four-byte header.
    pub has_crc: bool,
    /// Complete frame size in bytes.
    pub frame_length: usize,
    /// PCM samples represented by this frame per channel.
    pub samples_per_frame: u16,
}

/// One byte-localized MPEG-1 Audio Layer II frame.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Layer2Frame {
    /// Zero-based frame number.
    pub index: usize,
    /// Byte range in the original elementary stream.
    pub source_range: Range<usize>,
    /// Parsed header fields.
    pub header: Layer2Header,
}

impl Layer2Frame {
    /// Returns this complete frame from its original elementary stream.
    #[must_use]
    pub fn data<'a>(&self, stream: &'a [u8]) -> &'a [u8] {
        &stream[self.source_range.clone()]
    }
}

/// Parses a complete MPEG-1 Audio Layer II elementary stream.
///
/// # Errors
///
/// Returns an error for unsupported MPEG versions or layers, invalid/reserved header fields,
/// changing sample rate/channel layout, or incomplete frames and trailing bytes.
pub fn parse_layer2_stream(data: &[u8]) -> Result<Vec<Layer2Frame>> {
    if data.is_empty() {
        return Err(Error::InvalidData(
            "empty MPEG audio elementary stream".into(),
        ));
    }
    let mut frames = Vec::new();
    let mut offset = 0_usize;
    let mut format = None;
    while offset < data.len() {
        if data.len() - offset < 4 {
            return Err(Error::InvalidData(format!(
                "truncated MPEG audio header at byte {offset}"
            )));
        }
        let header = parse_layer2_header(&data[offset..offset + 4], offset)?;
        let current_format = (header.sample_rate, header.channels);
        if format.is_some_and(|expected| expected != current_format) {
            return Err(Error::Unsupported(format!(
                "MPEG audio format changes at frame {}",
                frames.len()
            )));
        }
        format = Some(current_format);
        let end = offset.checked_add(header.frame_length).ok_or_else(|| {
            Error::InvalidData("MPEG audio frame length overflows address space".into())
        })?;
        if end > data.len() {
            return Err(Error::InvalidData(format!(
                "truncated MPEG audio frame {} at byte {offset}: needs {} bytes, has {}",
                frames.len(),
                header.frame_length,
                data.len() - offset
            )));
        }
        frames.push(Layer2Frame {
            index: frames.len(),
            source_range: offset..end,
            header,
        });
        offset = end;
    }
    Ok(frames)
}

fn parse_layer2_header(data: &[u8], offset: usize) -> Result<Layer2Header> {
    let bits = u32::from_be_bytes([data[0], data[1], data[2], data[3]]);
    if bits >> 21 != 0x7ff {
        return Err(Error::InvalidData(format!(
            "MPEG audio sync word missing at byte {offset}"
        )));
    }
    let version = (bits >> 19) & 0x03;
    if version != 0x03 {
        return Err(Error::Unsupported(format!(
            "MPEG audio version field {version:02b} at byte {offset}; only MPEG-1 is supported"
        )));
    }
    let layer = (bits >> 17) & 0x03;
    if layer != 0x02 {
        return Err(Error::Unsupported(format!(
            "MPEG audio layer field {layer:02b} at byte {offset}; only Layer II is supported"
        )));
    }
    let bitrate_index =
        usize::try_from((bits >> 12) & 0x0f).expect("four-bit bitrate index fits usize");
    let bit_rate = u32::from(LAYER2_BIT_RATES_KBIT[bitrate_index]) * 1_000;
    if bit_rate == 0 {
        return Err(Error::Unsupported(format!(
            "free-format or reserved MPEG Layer II bitrate at byte {offset}"
        )));
    }
    let sample_rate = match (bits >> 10) & 0x03 {
        0 => 44_100,
        1 => 48_000,
        2 => 32_000,
        _ => {
            return Err(Error::InvalidData(format!(
                "reserved MPEG audio sample rate at byte {offset}"
            )));
        }
    };
    if bits & 0x03 == 0x02 {
        return Err(Error::InvalidData(format!(
            "reserved MPEG audio emphasis at byte {offset}"
        )));
    }
    let channel_mode = match (bits >> 6) & 0x03 {
        0 => ChannelMode::Stereo,
        1 => ChannelMode::JointStereo,
        2 => ChannelMode::DualChannel,
        _ => ChannelMode::Mono,
    };
    let channels = if channel_mode == ChannelMode::Mono {
        1
    } else {
        2
    };
    let padding = usize::from(bits & (1 << 9) != 0);
    let frame_length = usize::try_from(144_u64 * u64::from(bit_rate) / u64::from(sample_rate))
        .map_err(|_| Error::InvalidData("MPEG audio frame length does not fit usize".into()))?
        + padding;
    Ok(Layer2Header {
        bit_rate,
        sample_rate,
        channels,
        channel_mode,
        has_crc: bits & (1 << 16) == 0,
        frame_length,
        samples_per_frame: 1_152,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const MP2: &[u8] =
        include_bytes!("../../../../testdata/mpegaudio/valid/sine-48k-stereo-192k.mp2");

    #[test]
    fn parses_permanent_layer2_vector() {
        let frames = parse_layer2_stream(MP2).unwrap();
        assert_eq!(frames.len(), 20);
        assert_eq!(frames[0].header.sample_rate, 48_000);
        assert_eq!(frames[0].header.bit_rate, 192_000);
        assert_eq!(frames[0].header.channels, 2);
        assert!(frames.iter().all(|frame| frame.header.frame_length == 576));
        assert_eq!(frames.last().unwrap().source_range.end, MP2.len());
    }

    #[test]
    fn rejects_truncation_and_wrong_layer() {
        assert!(parse_layer2_stream(&MP2[..MP2.len() - 1]).is_err());
        let mut layer3 = MP2.to_vec();
        layer3[1] |= 0x02;
        assert!(parse_layer2_stream(&layer3).is_err());

        let mut free_format = MP2.to_vec();
        free_format[2] &= 0x0f;
        assert!(parse_layer2_stream(&free_format).is_err());

        let mut reserved_rate = MP2.to_vec();
        reserved_rate[2] = (reserved_rate[2] & 0xf3) | 0x0c;
        assert!(parse_layer2_stream(&reserved_rate).is_err());

        for length in 0..4 {
            assert!(parse_layer2_stream(&MP2[..length]).is_err());
        }
    }
}
