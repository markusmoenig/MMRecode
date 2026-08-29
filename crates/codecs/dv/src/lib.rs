//! Raw DV25 DIF parsing, metadata inspection, and embedded-audio extraction.
//!
//! A raw DV frame is a fixed-size sequence of 80-byte DIF blocks. This crate
//! keeps that physical organization visible so diagnostics and future
//! smart-recode operations can identify exact damaged or changed regions.

mod audio;
mod codec;
mod decoder;
mod dif;
mod encoder;
mod packs;
mod profile;
mod tables;

pub use audio::{DvAudioError, extract_audio};
pub use codec::{CODEC_NAME, DvDecoder, DvDependencyAnalyzer, DvEncoder};
pub use decoder::{
    ConcealedVideoSegment, DecodedDvVideo, DvVideoDecodeOptions, decode_video,
    decode_video_with_options,
};
pub use dif::{
    DIF_BLOCK_SIZE, DIF_BLOCKS_PER_SEQUENCE, DifBlock, DifBlockId, DifSection, DvFrame, DvIssue,
    DvIssueKind, parse_frame,
};
pub use encoder::{
    DvEncodeOptions, EncodedDv, encode_frame, encode_video, encode_video_with_audio,
};
pub use packs::{AudioQuantization, AudioSource, DvPack, DvPackData, Timecode};
pub use profile::{DvProfile, DvSystem, detect_profile, detect_profile_prefix};
