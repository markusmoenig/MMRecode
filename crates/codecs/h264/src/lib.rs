//! H.264/AVC elementary-stream coding, syntax, and dependency inspection.
//!
//! Containers remain responsible for sample timing and framing. Pixel reconstruction and the
//! deterministic `I_PCM`, Intra16, Intra4, multiple-reference P, and bounded reordered B encoding
//! stay behind the shared codec interfaces.

mod cabac;
mod cavlc;
mod deblock;
mod decoder;
mod encoder;
mod nal;
mod syntax;

pub use decoder::H264Decoder;
pub use encoder::H264Encoder;

pub use nal::{
    AvcDecoderConfigurationRecord, NalUnit, NalUnitHeader, NalUnitType, annex_b_nal_units,
    length_prefixed_nal_units, nal_units_to_annex_b, remove_emulation_prevention,
};
pub use syntax::{
    AspectRatio, H264AccessUnit, H264StreamIndex, H264StreamIndexer, HrdParameters, HrdSeiTiming,
    PictureOrderCountType, PictureTiming, PictureType, PictureUnit, Pps, RecoveryPoint,
    ScalingMatrices, SliceHeader, Sps, VuiParameters, parse_hrd_sei, parse_pps,
    parse_recovery_point_sei, parse_slice_header, parse_sps,
};

/// Canonical `MMRecode` codec identifier for H.264/AVC.
pub const CODEC_NAME: &str = "video/h264";
