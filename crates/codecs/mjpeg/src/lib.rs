//! Motion JPEG parsing, decoding, encoding, and dependency analysis.
//!
//! Motion JPEG is the bootstrap codec for `MMRecode`. JPEG syntax is kept
//! separate from container-specific Motion JPEG conventions.

mod codec;
mod decoder;
mod encoder;
mod entropy;
mod parser;
mod syntax;
mod tables;
mod transform;

pub use codec::{CODEC_NAME, MjpegDecoder, MjpegEncoder};
pub use decoder::{DecodedComponent, DecodedJpeg, JpegColorModel, decode_jpeg};
pub use encoder::{EncodedJpeg, JpegEncodeOptions, encode_jpeg};
pub use parser::parse_jpeg;
pub use syntax::{
    ApplicationSegment, EntropyScan, FrameComponent, FrameHeader, HuffmanTable, HuffmanTableClass,
    JfifHeader, JpegImage, JpegSegment, Marker, QuantizationPrecision, QuantizationTable,
    RestartMarker, ScanComponent, ScanHeader, SegmentData,
};
