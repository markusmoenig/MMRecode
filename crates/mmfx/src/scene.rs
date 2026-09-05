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
    /// Named animation definitions referenced by scene nodes.
    pub animations: Vec<Keyframes>,
    /// Top-level nodes, in paint order.
    pub children: Vec<Node>,
}

impl Scene {
    /// Returns every image source referenced by the scene, in paint order.
    #[must_use]
    pub fn image_sources(&self) -> Vec<&str> {
        fn collect<'a>(nodes: &'a [Node], sources: &mut Vec<&'a str>) {
            for node in nodes {
                if let NodeKind::Image(image) = &node.kind {
                    sources.push(&image.source);
                }
                collect(&node.children, sources);
            }
        }

        let mut sources = Vec::new();
        collect(&self.children, &mut sources);
        sources
    }

    /// Returns whether any node changes with scene-local time.
    #[must_use]
    pub fn is_animated(&self) -> bool {
        fn animated(nodes: &[Node]) -> bool {
            nodes.iter().any(|node| {
                node.style.animation.is_some()
                    || node.style.scroll.is_some()
                    || animated(&node.children)
            })
        }
        animated(&self.children)
    }
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
    /// A decoded image resource supplied by the host.
    Image(ImageContent),
}

/// Typed properties specific to an image object.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ImageContent {
    /// Module-relative resource source.
    pub source: String,
    /// Mapping of the image pixels into the resolved object box.
    pub fit: ObjectFit,
}

/// Image fitting policy inside an image object's box.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ObjectFit {
    /// Preserve aspect ratio and fit completely inside the box.
    #[default]
    Contain,
    /// Preserve aspect ratio and cover the box, clipping excess pixels.
    Cover,
    /// Stretch independently in both dimensions.
    Fill,
}

/// An explicitly declared module-relative or built-in font resource.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FontResource {
    /// Family name used by `font-family` declarations.
    pub name: String,
    /// Module-relative source path or stable built-in resource name.
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
    /// Whether this node participates in parent flow or is positioned independently.
    pub position: Position,
    /// How this node arranges its children.
    pub display: Display,
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
    /// Optional lower bound for the resolved object width.
    pub min_width: Option<Length>,
    /// Optional upper bound for the resolved object width.
    pub max_width: Option<Length>,
    /// Optional lower bound for the resolved object height.
    pub min_height: Option<Length>,
    /// Optional upper bound for the resolved object height.
    pub max_height: Option<Length>,
    /// Uniform inset applied to child layout.
    pub padding: Length,
    /// Space inserted between flow children.
    pub gap: Length,
    /// Cross-axis alignment of flow children.
    pub align_items: AlignItems,
    /// Main-axis distribution of flow children.
    pub justify_content: JustifyContent,
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
    /// Optional named keyframe animation.
    pub animation: Option<Animation>,
    /// Optional cover-style timeline scroll.
    pub scroll: Option<Scroll>,
}

impl Default for Style {
    fn default() -> Self {
        Self {
            position: Position::Flow,
            display: Display::Overlay,
            left: None,
            top: None,
            right: None,
            bottom: None,
            width: Length::Percent(100.0),
            height: Length::Percent(100.0),
            min_width: None,
            max_width: None,
            min_height: None,
            max_height: None,
            padding: Length::Pixels(0.0),
            gap: Length::Pixels(0.0),
            align_items: AlignItems::Start,
            justify_content: JustifyContent::Start,
            background: Color::TRANSPARENT,
            opacity: u16::MAX,
            overflow: Overflow::Visible,
            border_radius: Length::Pixels(0.0),
            transform: Transform::default(),
            animation: None,
            scroll: None,
        }
    }
}

/// Participation in the parent's child layout.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum Position {
    /// Participate in row or column flow.
    #[default]
    Flow,
    /// Position independently using inset properties.
    Absolute,
}

/// Child layout mode.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum Display {
    /// Paint children in the same containing box.
    #[default]
    Overlay,
    /// Lay out non-absolute children from left to right.
    Row,
    /// Lay out non-absolute children from top to bottom.
    Column,
}

/// Cross-axis flow alignment.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum AlignItems {
    /// Align children to the cross-axis start.
    #[default]
    Start,
    /// Center children on the cross axis.
    Center,
    /// Align children to the cross-axis end.
    End,
    /// Expand children across the available cross axis.
    Stretch,
}

/// Main-axis flow distribution.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum JustifyContent {
    /// Pack children at the main-axis start.
    #[default]
    Start,
    /// Center the packed child sequence.
    Center,
    /// Pack children at the main-axis end.
    End,
    /// Distribute remaining space between children.
    SpaceBetween,
}

/// A CSS-like length resolved relative to the containing object.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Length {
    /// Size the box from its text, image, or child layout.
    Auto,
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
            Self::Auto => containing,
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
    /// Horizontal scale around the object center.
    pub scale_x: f32,
    /// Vertical scale around the object center.
    pub scale_y: f32,
    /// Clockwise rotation around the object center, in degrees.
    pub rotate_degrees: f32,
}

impl Default for Transform {
    fn default() -> Self {
        Self {
            translate_x: Length::Pixels(0.0),
            translate_y: Length::Pixels(0.0),
            scale_x: 1.0,
            scale_y: 1.0,
            rotate_degrees: 0.0,
        }
    }
}

/// A named keyframe animation attached to a node.
#[derive(Clone, Debug, PartialEq)]
pub struct Animation {
    /// Referenced `@keyframes` name.
    pub name: String,
    /// Exact duration in local frames, or the complete containing scene duration.
    pub duration: AnimationDuration,
    /// Timing curve applied between stops.
    pub timing: TimingFunction,
}

/// Duration of a scene animation.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum AnimationDuration {
    /// Exact number of frames in the containing media's time base.
    Frames(u32),
    /// Complete local duration of the containing FX media.
    Scene,
}

/// Supported deterministic timing curves.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum TimingFunction {
    /// Constant-rate interpolation.
    Linear,
    /// Symmetric smooth acceleration and deceleration.
    #[default]
    Ease,
    /// Accelerate from rest.
    EaseIn,
    /// Decelerate to rest.
    EaseOut,
    /// Smoothly accelerate and decelerate.
    EaseInOut,
}

/// One named set of ordered animation stops.
#[derive(Clone, Debug, PartialEq)]
pub struct Keyframes {
    /// Name referenced by `animation` declarations.
    pub name: String,
    /// Ordered keyframe stops.
    pub stops: Vec<Keyframe>,
}

/// One keyframe stop at an offset from zero through one.
#[derive(Clone, Debug, PartialEq)]
pub struct Keyframe {
    /// Normalized position within the animation.
    pub offset: f32,
    /// Properties overridden at this stop.
    pub style: AnimatedStyle,
}

/// Properties which may be interpolated by the initial CPU evaluator.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct AnimatedStyle {
    /// Animated horizontal inset.
    pub left: Option<Length>,
    /// Animated vertical inset.
    pub top: Option<Length>,
    /// Animated width.
    pub width: Option<Length>,
    /// Animated height.
    pub height: Option<Length>,
    /// Animated box background.
    pub background: Option<Color>,
    /// Animated text color.
    pub color: Option<Color>,
    /// Animated node opacity.
    pub opacity: Option<u16>,
    /// Animated two-dimensional transform.
    pub transform: Option<Transform>,
}

/// Cover-style scrolling lowered to a timeline-dependent translation.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Scroll {
    /// Direction in which content travels.
    pub direction: ScrollDirection,
    /// Exact local duration.
    pub duration: AnimationDuration,
}

/// Logical scroll direction.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ScrollDirection {
    /// Move toward the top edge.
    BlockStart,
    /// Move toward the bottom edge.
    BlockEnd,
    /// Move toward the left edge.
    InlineStart,
    /// Move toward the right edge.
    InlineEnd,
}
