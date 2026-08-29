//! Native MPEG-2 Video elementary-stream parsing, dependency analysis, decoding, and encoding.
//!
//! The crate keeps sequence, GOP, picture, extension, and slice structure visible so callers can
//! inspect broadcast streams and plan smart-render boundaries without depending on a container.

mod codec;
mod decoder;
mod dependency;
mod encoder;
mod parser;
mod syntax;
mod tables;
mod transform;

pub use decoder::{
    DecodedMpeg2Picture, MacroblockCoding, MacroblockInfo, MotionType, decode_stream,
};
pub use dependency::{
    Mpeg2DependencyAnalyzer, SmartRenderDisposition, SmartRenderPicture, SmartRenderPlan,
    analyze_dependencies, plan_smart_render,
};
pub use encoder::{EncodedMpeg2, Mpeg2EncodeOptions, encode_stream};
pub use parser::{Mpeg2Stream, Picture, Slice, StartCodeUnit, parse_stream, scan_start_codes};
pub use syntax::{
    ChromaFormat, ColourDescription, Extension, FrameRate, GroupHeader, PictureCodingExtension,
    PictureHeader, PictureStructure, PictureType, QuantMatrixExtension, SequenceDisplayExtension,
    SequenceExtension, SequenceHeader, SequenceParameters,
};

/// Canonical `MMRecode` codec identifier for MPEG-2 Video.
pub const CODEC_NAME: &str = "video/mpeg2";
pub use codec::{Mpeg2Decoder, Mpeg2Encoder};
