//! Pixel formats and color metadata.

/// A raw pixel format understood by the shared frame representation.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
#[non_exhaustive]
pub enum PixelFormat {
    /// Planar 8-bit 4:2:0 YCbCr.
    Yuv420p8,
    /// Planar 8-bit 4:2:2 YCbCr.
    Yuv422p8,
    /// Planar 8-bit 4:4:4 YCbCr.
    Yuv444p8,
    /// Packed 8-bit RGB.
    Rgb24,
}

/// Whether component values use full or studio range.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
#[non_exhaustive]
pub enum ColorRange {
    /// Range has not been signalled.
    Unspecified,
    /// Full-range component values.
    Full,
    /// Studio or limited-range component values.
    Limited,
}

/// The ordering and interpretation of fields in an image.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
#[non_exhaustive]
pub enum FieldOrder {
    /// Progressive frame.
    Progressive,
    /// Top field is displayed first.
    TopFirst,
    /// Bottom field is displayed first.
    BottomFirst,
    /// Field order has not been determined.
    Unspecified,
}

/// Color characteristics attached to a video frame or stream.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ColorDescription {
    /// Component range.
    pub range: ColorRange,
    /// Named or standardized color-primary identifier.
    pub primaries: Option<String>,
    /// Named or standardized transfer-function identifier.
    pub transfer: Option<String>,
    /// Named or standardized matrix-coefficient identifier.
    pub matrix: Option<String>,
}

impl Default for ColorDescription {
    fn default() -> Self {
        Self {
            range: ColorRange::Unspecified,
            primaries: None,
            transfer: None,
            matrix: None,
        }
    }
}
