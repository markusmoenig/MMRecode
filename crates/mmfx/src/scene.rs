//! Typed scene representation produced by the MMFX parser.

/// A fully validated MMFX scene.
#[derive(Clone, Debug, PartialEq)]
pub struct Scene {
    /// Author-facing scene name.
    pub name: String,
    /// Output width in pixels.
    pub width: u32,
    /// Output height in pixels.
    pub height: u32,
    /// Color painted before scene nodes.
    pub background: Color,
    /// Explicit font resources required by text objects.
    pub fonts: Vec<FontResource>,
    /// Top-level nodes, in paint order.
    pub children: Vec<Node>,
}

/// A node in the typed MMFX scene tree.
#[derive(Clone, Debug, PartialEq)]
pub struct Node {
    /// Author-facing object name.
    pub name: String,
    /// Semantic object type.
    pub kind: NodeKind,
    /// Layout and paint properties.
    pub style: Style,
    /// Child nodes, in paint order.
    pub children: Vec<Self>,
}

/// Supported scene node types in the initial MMFX subset.
#[derive(Clone, Debug, PartialEq)]
pub enum NodeKind {
    /// A transparent overlay container.
    Group,
    /// A painted rectangular shape.
    Rect,
    /// Shaped and laid-out text.
    Text(TextContent),
}

/// An explicitly declared font file.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FontResource {
    /// Family name used by `font-family` declarations.
    pub name: String,
    /// Module-relative source path.
    pub source: String,
}

/// Typed properties specific to a text object.
#[derive(Clone, Debug, PartialEq)]
pub struct TextContent {
    /// Unicode text to shape and render.
    pub content: String,
    /// Explicitly declared font family.
    pub font_family: String,
    /// Font size in output pixels.
    pub font_size: f32,
    /// CSS-compatible numeric font weight.
    pub font_weight: f32,
    /// Line-height policy.
    pub line_height: TextLineHeight,
    /// Glyph color.
    pub color: Color,
    /// Horizontal paragraph alignment.
    pub align: TextAlign,
    /// Line-wrapping policy.
    pub wrap: TextWrap,
}

/// CSS-shaped text line height.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum TextLineHeight {
    /// Unitless multiple of the font size.
    Relative(f32),
    /// Absolute output pixels.
    Pixels(f32),
}

/// Horizontal paragraph alignment.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum TextAlign {
    /// Align to the logical line start.
    #[default]
    Start,
    /// Center each line in the text box.
    Center,
    /// Align to the logical line end.
    End,
}

/// Text wrapping behavior.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum TextWrap {
    /// Wrap at normal Unicode line-breaking opportunities.
    #[default]
    Wrap,
    /// Keep text on explicit lines only.
    NoWrap,
}

/// Layout and paint properties shared by scene nodes.
#[derive(Clone, Debug, PartialEq)]
pub struct Style {
    /// Distance from the parent left edge.
    pub left: Option<Length>,
    /// Distance from the parent top edge.
    pub top: Option<Length>,
    /// Distance from the parent right edge.
    pub right: Option<Length>,
    /// Distance from the parent bottom edge.
    pub bottom: Option<Length>,
    /// Object width. Defaults to 100 percent.
    pub width: Length,
    /// Object height. Defaults to 100 percent.
    pub height: Length,
    /// Fill color. Only rectangles paint it in the initial subset.
    pub background: Color,
    /// Object opacity represented as an integer unit interval.
    pub opacity: u16,
    /// Child clipping behavior.
    pub overflow: Overflow,
    /// Corner radius.
    pub border_radius: Length,
    /// Geometric transform applied after layout.
    pub transform: Transform,
}

impl Default for Style {
    fn default() -> Self {
        Self {
            left: None,
            top: None,
            right: None,
            bottom: None,
            width: Length::Percent(100.0),
            height: Length::Percent(100.0),
            background: Color::TRANSPARENT,
            opacity: u16::MAX,
            overflow: Overflow::Visible,
            border_radius: Length::Pixels(0.0),
            transform: Transform::default(),
        }
    }
}

/// A CSS-like length resolved relative to the containing object.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Length {
    /// Device-independent output pixels.
    Pixels(f32),
    /// Percentage of the corresponding containing dimension.
    Percent(f32),
}

impl Length {
    /// Resolve this length against a containing dimension.
    #[must_use]
    pub fn resolve(self, containing: f32) -> f32 {
        match self {
            Self::Pixels(value) => value,
            Self::Percent(value) => containing * value / 100.0,
        }
    }
}

/// An eight-bit straight-alpha sRGB color.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Color {
    /// Red channel.
    pub red: u8,
    /// Green channel.
    pub green: u8,
    /// Blue channel.
    pub blue: u8,
    /// Alpha channel.
    pub alpha: u8,
}

impl Color {
    /// Fully transparent black.
    pub const TRANSPARENT: Self = Self::rgba(0, 0, 0, 0);

    /// Construct an sRGB color from channel values.
    #[must_use]
    pub const fn rgba(red: u8, green: u8, blue: u8, alpha: u8) -> Self {
        Self {
            red,
            green,
            blue,
            alpha,
        }
    }
}

impl Default for Color {
    fn default() -> Self {
        Self::rgba(0, 0, 0, u8::MAX)
    }
}

/// Child clipping mode.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum Overflow {
    /// Children may paint outside the object bounds.
    #[default]
    Visible,
    /// Children are clipped to the object bounds.
    Hidden,
}

/// The transform subset supported by the reference implementation.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Transform {
    /// Horizontal translation.
    pub translate_x: Length,
    /// Vertical translation.
    pub translate_y: Length,
}

impl Default for Transform {
    fn default() -> Self {
        Self {
            translate_x: Length::Pixels(0.0),
            translate_y: Length::Pixels(0.0),
        }
    }
}
