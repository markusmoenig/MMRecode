//! Uncompressed frame storage.

use crate::{ColorDescription, FieldOrder, PixelFormat, Timestamp};

/// Timing information associated with a decoded or source frame.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct FrameTiming {
    /// Presentation timestamp.
    pub pts: Option<Timestamp>,
    /// Frame duration.
    pub duration: Option<Timestamp>,
}

/// One pixel plane with explicit layout.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Plane {
    /// Contiguous plane storage.
    pub data: Vec<u8>,
    /// Bytes between corresponding samples on adjacent rows.
    pub stride: usize,
    /// Visible width in samples.
    pub width: usize,
    /// Visible height in samples.
    pub height: usize,
}

/// An uncompressed video frame.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VideoFrame {
    /// Pixel storage format.
    pub format: PixelFormat,
    /// Visible luma or packed-pixel width.
    pub width: usize,
    /// Visible luma or packed-pixel height.
    pub height: usize,
    /// Pixel planes in format-defined order.
    pub planes: Vec<Plane>,
    /// Presentation timing.
    pub timing: FrameTiming,
    /// Color interpretation.
    pub color: ColorDescription,
    /// Progressive or interlaced field order.
    pub field_order: FieldOrder,
}
