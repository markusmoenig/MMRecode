//! H.264/AVC elementary-stream syntax and dependency inspection.
//!
//! This crate deliberately stops at syntax and picture relationships. Containers remain
//! responsible for sample timing and framing, and pixel reconstruction stays behind the shared
//! decoder interface.

mod cabac;
mod cavlc;
mod deblock;
mod decoder;
mod nal;
mod syntax;

pub use decoder::H264Decoder;

pub use nal::{
    AvcDecoderConfigurationRecord, NalUnit, NalUnitHeader, NalUnitType, annex_b_nal_units,
    length_prefixed_nal_units, nal_units_to_annex_b, remove_emulation_prevention,
};
pub use syntax::{
    AspectRatio, H264AccessUnit, H264StreamIndex, H264StreamIndexer, PictureOrderCountType,
    PictureTiming, PictureType, PictureUnit, Pps, SliceHeader, Sps, VuiParameters, parse_pps,
    parse_slice_header, parse_sps,
};

/// Canonical `MMRecode` codec identifier for H.264/AVC.
pub const CODEC_NAME: &str = "video/h264";
