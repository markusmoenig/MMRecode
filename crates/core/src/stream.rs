//! Stream and codec descriptions shared by codecs and containers.

use crate::Rational;

/// Stable stream identifier within a demuxer or muxer.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct StreamId(pub u32);

/// A four-byte container codec or handler tag.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct FourCc(pub [u8; 4]);

/// Extensible codec identifier.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct CodecId(String);

impl CodecId {
    /// Creates a namespaced codec identifier such as `video/mjpeg`.
    #[must_use]
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// Returns the identifier text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// High-level kind of media carried by a stream.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
#[non_exhaustive]
pub enum MediaType {
    /// Video samples.
    Video,
    /// Audio samples.
    Audio,
    /// Timed or untimed metadata.
    Data,
}

/// Opaque codec configuration conveyed by a container or encoder.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CodecDescriptor {
    /// Codec identifier.
    pub codec_id: CodecId,
    /// Container-specific codec tag, when one exists.
    pub codec_tag: Option<FourCc>,
    /// Media kind.
    pub media_type: MediaType,
    /// Opaque configuration record interpreted by the codec implementation.
    pub configuration: Vec<u8>,
}

/// Description of one stream in a container.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StreamDescriptor {
    /// Stream identifier.
    pub id: StreamId,
    /// Codec and configuration.
    pub codec: CodecDescriptor,
    /// Timestamp unit used by packets from this stream.
    pub time_base: Rational,
}
