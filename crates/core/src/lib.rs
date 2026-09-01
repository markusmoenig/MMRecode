//! Shared vocabulary and interfaces for the `MMRecode` ecosystem.
//!
//! This crate deliberately contains no codec or container implementation. It defines the types
//! that allow demuxers, codecs, renderers, and muxers to exchange data without depending on one
//! another.

mod audio;
mod codec;
mod container;
mod dependency;
mod error;
mod frame;
mod packet;
mod pixel;
mod stream;
mod time;

pub use audio::{AudioFrame, AudioSampleFormat};
pub use codec::{Decoder, Encoder, VideoEncoderSettings};
pub use container::{Demuxer, Muxer, SeekResult};
pub use dependency::{
    AccessUnitInfo, DependencyAnalyzer, ParameterFingerprint, PictureId, PictureKind,
    RandomAccessKind,
};
pub use error::{Error, Result};
pub use frame::{FrameTiming, Plane, VideoFrame};
pub use packet::{Packet, PacketFlags, PacketSideData};
pub use pixel::{ColorDescription, ColorRange, FieldOrder, PixelFormat};
pub use stream::{CodecDescriptor, CodecId, FourCc, MediaType, StreamDescriptor, StreamId};
pub use time::{Rational, Timestamp, TimestampRounding};
