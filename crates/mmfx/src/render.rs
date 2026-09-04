//! Deterministic scalar CPU reference renderer for typed MMFX scenes.

use std::{borrow::Cow, collections::BTreeMap, fmt, sync::Arc};

use crate::{
    Color, Length, Node, NodeKind, Overflow, Scene, TextAlign, TextContent, TextLineHeight,
    TextWrap,
};
use parley::fontique::{Blob, Collection, CollectionOptions, SourceCache};
use parley::{
    Alignment, AlignmentOptions, FontContext, FontFamily, FontWeight, Layout, LayoutContext,
    LineHeight, PositionedLayoutItem, StyleProperty, TextWrapMode,
};
use swash::FontRef;
use swash::scale::image::Content as GlyphImageContent;
use swash::scale::{Render as GlyphRender, ScaleContext, Source as GlyphSource};
use zeno::{Command, Format, Mask, PathBuilder, Placement, Vector};

const MAX_PIXELS: usize = 100_000_000;
const CHANNEL_MAX: u32 = u16::MAX as u32;

trait ResolveLength {
    fn resolve_f64(self, containing: f64) -> f64;
}

impl ResolveLength for Length {
    fn resolve_f64(self, containing: f64) -> f64 {
        match self {
            Self::Pixels(value) => f64::from(value),
            Self::Percent(value) => containing * f64::from(value) / 100.0,
        }
    }
}

/// A rendering failure caused by an invalid or impractically large output.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RenderError {
    message: String,
}

impl fmt::Display for RenderError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for RenderError {}

/// Loaded binary resources supplied independently of the typed scene model.
#[derive(Clone, Debug, Default)]
pub struct RenderResources {
    fonts: BTreeMap<String, Arc<Vec<u8>>>,
}

impl RenderResources {
    /// Create an empty resource set.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Add or replace one explicitly named font file.
    pub fn add_font(&mut self, name: impl Into<String>, data: Vec<u8>) {
        self.fonts.insert(name.into(), Arc::new(data));
    }
}

/// A rendered frame stored internally as linear premultiplied RGBA.
#[derive(Clone, Debug)]
pub struct Surface {
    width: u32,
    height: u32,
    pixels: Vec<LinearPixel>,
}

impl Surface {
    fn transparent(width: u32, height: u32) -> Result<Self, RenderError> {
        let length = usize::try_from(width)
            .ok()
            .and_then(|width| {
                usize::try_from(height)
                    .ok()
                    .and_then(|height| width.checked_mul(height))
            })
            .filter(|length| *length <= MAX_PIXELS)
            .ok_or_else(|| RenderError {
                message: format!(
                    "MMFX output {width}x{height} is too large for the CPU reference renderer"
                ),
            })?;
        Ok(Self {
            width,
            height,
            pixels: vec![LinearPixel::TRANSPARENT; length],
        })
    }

    /// Frame width in pixels.
    #[must_use]
    pub const fn width(&self) -> u32 {
        self.width
    }

    /// Frame height in pixels.
    #[must_use]
    pub const fn height(&self) -> u32 {
        self.height
    }

    /// Convert the frame to tightly packed straight-alpha sRGBA8 bytes.
    #[must_use]
    pub fn to_rgba8(&self) -> Vec<u8> {
        let mut output = Vec::with_capacity(self.pixels.len() * 4);
        for pixel in &self.pixels {
            let alpha = pixel.alpha;
            if alpha == 0 {
                output.extend_from_slice(&[0, 0, 0, 0]);
                continue;
            }
            let unpremultiply = |channel: u16| {
                let linear =
                    (u32::from(channel) * CHANNEL_MAX + u32::from(alpha) / 2) / u32::from(alpha);
                linear_u16_to_srgb(linear.min(CHANNEL_MAX) as u16)
            };
            output.extend_from_slice(&[
                unpremultiply(pixel.red),
                unpremultiply(pixel.green),
                unpremultiply(pixel.blue),
                u8::try_from((u32::from(alpha) + 128) / 257).unwrap_or(u8::MAX),
            ]);
        }
        output
    }

    /// Return one output pixel as straight-alpha sRGBA8.
    #[must_use]
    pub fn pixel(&self, x: u32, y: u32) -> Option<[u8; 4]> {
        if x >= self.width || y >= self.height {
            return None;
        }
        let offset = (usize::try_from(y).ok()? * usize::try_from(self.width).ok()?
            + usize::try_from(x).ok()?)
            * 4;
        self.to_rgba8().get(offset..offset + 4)?.try_into().ok()
    }

    fn blend_surface(&mut self, source: &Self, opacity: u16, clip: Clip) {
        let height = i32::try_from(self.height).unwrap_or(i32::MAX);
        let width = i32::try_from(self.width).unwrap_or(i32::MAX);
        for y in clip.top.max(0)..clip.bottom.min(height) {
            for x in clip.left.max(0)..clip.right.min(width) {
                let index = self.index(x, y);
                self.pixels[index] = source.pixels[index].over(self.pixels[index], opacity);
            }
        }
    }

    fn index(&self, x: i32, y: i32) -> usize {
        usize::try_from(y).expect("clipped y") * usize::try_from(self.width).expect("width")
            + usize::try_from(x).expect("clipped x")
    }
}

/// Render a validated scene using the scalar CPU reference backend.
///
/// Blending occurs in linear light with premultiplied alpha. The returned
/// surface can be converted to conventional sRGBA8 with [`Surface::to_rgba8`].
///
/// # Errors
///
/// Returns an error when the output dimensions would exceed the CPU reference
/// renderer's guarded allocation limit.
pub fn render(scene: &Scene) -> Result<Surface, RenderError> {
    render_with_resources(scene, &RenderResources::new())
}

/// Render a validated scene with an explicit set of binary resources.
///
/// System fonts are never consulted. Every `@text` node must reference a font
/// declared by the scene and supplied under the same name in `resources`.
///
/// # Errors
///
/// Returns an error for missing or invalid fonts, mismatched declared family
/// names, or output dimensions beyond the guarded allocation limit.
pub fn render_with_resources(
    scene: &Scene,
    resources: &RenderResources,
) -> Result<Surface, RenderError> {
    let mut state = RenderState::new(scene, resources)?;
    let mut surface = Surface::transparent(scene.width, scene.height)?;
    let viewport = Bounds {
        x: 0.0,
        y: 0.0,
        width: f64::from(scene.width),
        height: f64::from(scene.height),
    };
    let clip = Clip {
        left: 0,
        top: 0,
        right: i32::try_from(scene.width).unwrap_or(i32::MAX),
        bottom: i32::try_from(scene.height).unwrap_or(i32::MAX),
    };
    fill_rounded(&mut surface, viewport, 0.0, scene.background, clip, &[]);
    let mut coverage_clips = Vec::new();
    for child in &scene.children {
        draw_node(
            &mut surface,
            child,
            viewport,
            clip,
            &mut coverage_clips,
            &mut state,
        )?;
    }
    Ok(surface)
}

struct RenderState {
    fonts: FontContext,
    layout: LayoutContext<Color>,
    scale: ScaleContext,
}

impl RenderState {
    fn new(scene: &Scene, resources: &RenderResources) -> Result<Self, RenderError> {
        let mut collection = Collection::new(CollectionOptions {
            shared: false,
            system_fonts: false,
        });
        for declared in &scene.fonts {
            let data = resources
                .fonts
                .get(&declared.name)
                .ok_or_else(|| RenderError {
                    message: format!(
                        "font '{}' was declared from '{}' but no bytes were supplied",
                        declared.name, declared.source
                    ),
                })?;
            let data: Arc<dyn AsRef<[u8]> + Send + Sync> = data.clone();
            let registered = collection.register_fonts(Blob::new(data), None);
            if registered.is_empty() {
                return Err(RenderError {
                    message: format!("font resource '{}' is not a valid font", declared.name),
                });
            }
        }
        for declared in &scene.fonts {
            if collection.family_id(&declared.name).is_none() {
                return Err(RenderError {
                    message: format!(
                        "declared font name '{}' does not match a family in '{}'",
                        declared.name, declared.source
                    ),
                });
            }
        }
        Ok(Self {
            fonts: FontContext {
                collection,
                source_cache: SourceCache::default(),
            },
            layout: LayoutContext::new(),
            scale: ScaleContext::new(),
        })
    }
}

fn draw_node(
    target: &mut Surface,
    node: &Node,
    parent: Bounds,
    inherited_clip: Clip,
    coverage_clips: &mut Vec<CoverageMask>,
    state: &mut RenderState,
) -> Result<(), RenderError> {
    let bounds = resolve_bounds(node, parent);
    let bounds_clip = Clip::from_bounds(bounds);
    let child_clip = if node.style.overflow == Overflow::Hidden {
        inherited_clip.intersect(bounds_clip)
    } else {
        inherited_clip
    };
    let mut layer = Surface::transparent(target.width, target.height)?;
    let radius = node
        .style
        .border_radius
        .resolve_f64(bounds.width.min(bounds.height))
        .max(0.0);
    fill_rounded(
        &mut layer,
        bounds,
        radius,
        node.style.background,
        inherited_clip,
        coverage_clips,
    );
    let adds_clip = node.style.overflow == Overflow::Hidden;
    if adds_clip {
        coverage_clips.push(rasterize_rounded_rect(bounds, radius));
    }
    if let NodeKind::Text(text) = &node.kind {
        draw_text(
            &mut layer,
            text,
            bounds,
            inherited_clip,
            coverage_clips,
            state,
        )?;
    }
    for child in &node.children {
        draw_node(&mut layer, child, bounds, child_clip, coverage_clips, state)?;
    }
    if adds_clip {
        coverage_clips.pop();
    }
    target.blend_surface(&layer, node.style.opacity, inherited_clip);
    Ok(())
}

fn resolve_bounds(node: &Node, parent: Bounds) -> Bounds {
    let width = node.style.width.resolve_f64(parent.width).max(0.0);
    let height = node.style.height.resolve_f64(parent.height).max(0.0);
    let x = if let Some(left) = node.style.left {
        parent.x + left.resolve_f64(parent.width)
    } else if let Some(right) = node.style.right {
        parent.x + parent.width - right.resolve_f64(parent.width) - width
    } else {
        parent.x
    };
    let y = if let Some(top) = node.style.top {
        parent.y + top.resolve_f64(parent.height)
    } else if let Some(bottom) = node.style.bottom {
        parent.y + parent.height - bottom.resolve_f64(parent.height) - height
    } else {
        parent.y
    };
    Bounds {
        x: x + node.style.transform.translate_x.resolve_f64(parent.width),
        y: y + node.style.transform.translate_y.resolve_f64(parent.height),
        width,
        height,
    }
}

fn fill_rounded(
    surface: &mut Surface,
    bounds: Bounds,
    radius: f64,
    color: Color,
    clip: Clip,
    inherited_coverage_masks: &[CoverageMask],
) {
    if color.alpha == 0 || bounds.width <= 0.0 || bounds.height <= 0.0 {
        return;
    }
    let shape = rasterize_rounded_rect(bounds, radius);
    paint_coverage_mask(surface, &shape, color, clip, inherited_coverage_masks);
}

fn paint_coverage_mask(
    surface: &mut Surface,
    shape: &CoverageMask,
    color: Color,
    clip: Clip,
    inherited_coverage_masks: &[CoverageMask],
) {
    let shape_clip = clip.intersect(shape.clip());
    let source = LinearPixel::from_color(color);
    let height = i32::try_from(surface.height).unwrap_or(i32::MAX);
    let width = i32::try_from(surface.width).unwrap_or(i32::MAX);
    for y in shape_clip.top.max(0)..shape_clip.bottom.min(height) {
        for x in shape_clip.left.max(0)..shape_clip.right.min(width) {
            let mut coverage = shape.coverage_at(x, y);
            for inherited in inherited_coverage_masks {
                coverage = multiply_coverage(coverage, inherited.coverage_at(x, y));
            }
            if coverage == 0 {
                continue;
            }
            let index = surface.index(x, y);
            surface.pixels[index] = source.over(surface.pixels[index], u16::from(coverage) * 257);
        }
    }
}

fn draw_text(
    surface: &mut Surface,
    text: &TextContent,
    bounds: Bounds,
    clip: Clip,
    coverage_masks: &[CoverageMask],
    state: &mut RenderState,
) -> Result<(), RenderError> {
    let mut builder = state
        .layout
        .ranged_builder(&mut state.fonts, &text.content, 1.0, false);
    builder.push_default(StyleProperty::FontFamily(FontFamily::Source(
        Cow::Borrowed(&text.font_family),
    )));
    builder.push_default(StyleProperty::FontSize(text.font_size));
    builder.push_default(StyleProperty::FontWeight(FontWeight::new(text.font_weight)));
    builder.push_default(StyleProperty::LineHeight(match text.line_height {
        TextLineHeight::Relative(value) => LineHeight::FontSizeRelative(value),
        TextLineHeight::Pixels(value) => LineHeight::Absolute(value),
    }));
    builder.push_default(StyleProperty::TextWrapMode(match text.wrap {
        TextWrap::Wrap => TextWrapMode::Wrap,
        TextWrap::NoWrap => TextWrapMode::NoWrap,
    }));
    builder.push_default(StyleProperty::Brush(text.color));
    let mut layout: Layout<Color> = builder.build(&text.content);
    layout.break_all_lines(Some(f64_to_f32(bounds.width)));
    layout.align(
        match text.align {
            TextAlign::Start => Alignment::Start,
            TextAlign::Center => Alignment::Center,
            TextAlign::End => Alignment::End,
        },
        AlignmentOptions::default(),
    );

    for line in layout.lines() {
        for item in line.items() {
            if let PositionedLayoutItem::GlyphRun(glyph_run) = item {
                draw_glyph_run(
                    surface,
                    &glyph_run,
                    bounds,
                    clip,
                    coverage_masks,
                    &mut state.scale,
                )?;
            }
        }
    }
    Ok(())
}

fn draw_glyph_run(
    surface: &mut Surface,
    glyph_run: &parley::GlyphRun<'_, Color>,
    bounds: Bounds,
    clip: Clip,
    coverage_masks: &[CoverageMask],
    scale_context: &mut ScaleContext,
) -> Result<(), RenderError> {
    let run = glyph_run.run();
    let font = run.font();
    let font_index = usize::try_from(font.index).map_err(|_| RenderError {
        message: "Parley selected a font face index too large for this platform".to_owned(),
    })?;
    let font_ref =
        FontRef::from_index(font.data.as_ref(), font_index).ok_or_else(|| RenderError {
            message: "Parley selected an invalid font face index".to_owned(),
        })?;
    let mut scaler = scale_context
        .builder(font_ref)
        .size(run.font_size())
        .hint(true)
        .normalized_coords(run.normalized_coords())
        .build();

    for glyph in glyph_run.positioned_glyphs() {
        let glyph_id = u16::try_from(glyph.id).map_err(|_| RenderError {
            message: format!("glyph id {} exceeds Swash's supported range", glyph.id),
        })?;
        let glyph_x = f64_to_f32(bounds.x) + glyph.x;
        let glyph_y = f64_to_f32(bounds.y) + glyph.y;
        let Some(image) = GlyphRender::new(&[GlyphSource::Outline])
            .format(Format::Alpha)
            .offset(Vector::new(glyph_x.fract(), glyph_y.fract()))
            .render(&mut scaler, glyph_id)
        else {
            continue;
        };
        if image.content != GlyphImageContent::Mask {
            continue;
        }
        let placement = Placement {
            left: floor_f32_to_i32(glyph_x).saturating_add(image.placement.left),
            top: floor_f32_to_i32(glyph_y).saturating_sub(image.placement.top),
            width: image.placement.width,
            height: image.placement.height,
        };
        paint_coverage_mask(
            surface,
            &CoverageMask {
                pixels: image.data,
                placement,
            },
            glyph_run.style().brush,
            clip,
            coverage_masks,
        );
    }
    Ok(())
}

fn rasterize_rounded_rect(bounds: Bounds, radius: f64) -> CoverageMask {
    let mut path = Vec::<Command>::new();
    let x = f64_to_f32(bounds.x);
    let y = f64_to_f32(bounds.y);
    let width = f64_to_f32(bounds.width.max(0.0));
    let height = f64_to_f32(bounds.height.max(0.0));
    let radius = f64_to_f32(
        radius
            .max(0.0)
            .min(bounds.width / 2.0)
            .min(bounds.height / 2.0),
    );
    path.add_round_rect([x, y], width, height, radius, radius);
    let (pixels, placement) = Mask::new(&path).render();
    CoverageMask { pixels, placement }
}

fn multiply_coverage(left: u8, right: u8) -> u8 {
    let product = u16::from(left) * u16::from(right) + 127;
    u8::try_from(product / 255).unwrap_or(u8::MAX)
}

#[allow(clippy::cast_possible_truncation)]
fn f64_to_f32(value: f64) -> f32 {
    value.clamp(f64::from(f32::MIN), f64::from(f32::MAX)) as f32
}

#[derive(Clone, Copy, Debug)]
struct Bounds {
    x: f64,
    y: f64,
    width: f64,
    height: f64,
}

#[derive(Clone, Debug)]
struct CoverageMask {
    pixels: Vec<u8>,
    placement: Placement,
}

impl CoverageMask {
    fn coverage_at(&self, x: i32, y: i32) -> u8 {
        let local_x = i64::from(x) - i64::from(self.placement.left);
        let local_y = i64::from(y) - i64::from(self.placement.top);
        if local_x < 0
            || local_y < 0
            || local_x >= i64::from(self.placement.width)
            || local_y >= i64::from(self.placement.height)
        {
            return 0;
        }
        let index = usize::try_from(local_y).expect("non-negative mask y")
            * usize::try_from(self.placement.width).expect("mask width")
            + usize::try_from(local_x).expect("non-negative mask x");
        self.pixels.get(index).copied().unwrap_or(0)
    }

    fn clip(&self) -> Clip {
        let right = i64::from(self.placement.left) + i64::from(self.placement.width);
        let bottom = i64::from(self.placement.top) + i64::from(self.placement.height);
        Clip {
            left: self.placement.left,
            top: self.placement.top,
            right: i32::try_from(right).unwrap_or(i32::MAX),
            bottom: i32::try_from(bottom).unwrap_or(i32::MAX),
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct Clip {
    left: i32,
    top: i32,
    right: i32,
    bottom: i32,
}

impl Clip {
    fn from_bounds(bounds: Bounds) -> Self {
        Self {
            left: floor_to_i32(bounds.x),
            top: floor_to_i32(bounds.y),
            right: ceil_to_i32(bounds.x + bounds.width),
            bottom: ceil_to_i32(bounds.y + bounds.height),
        }
    }

    fn intersect(self, other: Self) -> Self {
        Self {
            left: self.left.max(other.left),
            top: self.top.max(other.top),
            right: self.right.min(other.right),
            bottom: self.bottom.min(other.bottom),
        }
    }
}

fn floor_to_i32(value: f64) -> i32 {
    if value <= f64::from(i32::MIN) {
        i32::MIN
    } else if value >= f64::from(i32::MAX) {
        i32::MAX
    } else {
        #[allow(clippy::cast_possible_truncation)]
        {
            value.floor() as i32
        }
    }
}

fn floor_f32_to_i32(value: f32) -> i32 {
    floor_to_i32(f64::from(value))
}

fn ceil_to_i32(value: f64) -> i32 {
    if value <= f64::from(i32::MIN) {
        i32::MIN
    } else if value >= f64::from(i32::MAX) {
        i32::MAX
    } else {
        #[allow(clippy::cast_possible_truncation)]
        {
            value.ceil() as i32
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct LinearPixel {
    red: u16,
    green: u16,
    blue: u16,
    alpha: u16,
}

impl LinearPixel {
    const TRANSPARENT: Self = Self {
        red: 0,
        green: 0,
        blue: 0,
        alpha: 0,
    };

    fn from_color(color: Color) -> Self {
        let alpha = u16::from(color.alpha) * 257;
        Self {
            red: multiply_channel(srgb_to_linear_u16(color.red), alpha),
            green: multiply_channel(srgb_to_linear_u16(color.green), alpha),
            blue: multiply_channel(srgb_to_linear_u16(color.blue), alpha),
            alpha,
        }
    }

    fn over(self, destination: Self, opacity: u16) -> Self {
        let source_alpha = multiply_channel(self.alpha, opacity);
        let inverse = u16::MAX - source_alpha;
        Self {
            red: add_saturating(
                multiply_channel(self.red, opacity),
                multiply_channel(destination.red, inverse),
            ),
            green: add_saturating(
                multiply_channel(self.green, opacity),
                multiply_channel(destination.green, inverse),
            ),
            blue: add_saturating(
                multiply_channel(self.blue, opacity),
                multiply_channel(destination.blue, inverse),
            ),
            alpha: add_saturating(source_alpha, multiply_channel(destination.alpha, inverse)),
        }
    }
}

fn multiply_channel(left: u16, right: u16) -> u16 {
    let product = u32::from(left) * u32::from(right) + CHANNEL_MAX / 2;
    u16::try_from(product / CHANNEL_MAX).unwrap_or(u16::MAX)
}

fn add_saturating(left: u16, right: u16) -> u16 {
    left.saturating_add(right)
}

fn srgb_to_linear_u16(value: u8) -> u16 {
    let encoded = f64::from(value) / 255.0;
    let linear = if encoded <= 0.040_45 {
        encoded / 12.92
    } else {
        ((encoded + 0.055) / 1.055).powf(2.4)
    };
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    {
        (linear * f64::from(u16::MAX)).round() as u16
    }
}

fn linear_u16_to_srgb(value: u16) -> u8 {
    let linear = f64::from(value) / f64::from(u16::MAX);
    let encoded = if linear <= 0.003_130_8 {
        linear * 12.92
    } else {
        1.055 * linear.powf(1.0 / 2.4) - 0.055
    };
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    {
        (encoded * 255.0).round().clamp(0.0, 255.0) as u8
    }
}

#[cfg(test)]
mod tests {
    use crate::{RenderResources, parse_scene, render, render_with_resources};

    fn inter_resources() -> RenderResources {
        let mut resources = RenderResources::new();
        resources.add_font(
            "Inter",
            include_bytes!("../../../assets/fonts/Inter.ttf").to_vec(),
        );
        resources
    }

    #[test]
    fn resolves_percentages_bottom_anchor_and_translation() {
        let scene = parse_scene(
            "@scene x { width: 10px; height: 10px; background: #000; \
             @rect r { left: 20%; bottom: 1px; width: 4px; height: 3px; \
             transform: translate(1px, 0); background: #fff; } }",
        )
        .expect("valid scene");
        let surface = render(&scene).expect("rendered scene");
        assert_eq!(surface.pixel(2, 6), Some([0, 0, 0, 255]));
        assert_eq!(surface.pixel(3, 7), Some([255, 255, 255, 255]));
        assert_eq!(surface.pixel(6, 8), Some([255, 255, 255, 255]));
        assert_eq!(surface.pixel(7, 8), Some([0, 0, 0, 255]));
    }

    #[test]
    fn clips_children_to_hidden_group() {
        let scene = parse_scene(
            "@scene x { width: 5px; height: 3px; @group crop { left: 1px; width: 2px; \
             height: 3px; overflow: hidden; @rect wide { width: 5px; height: 3px; \
             background: #f00; } } }",
        )
        .expect("valid scene");
        let surface = render(&scene).expect("rendered scene");
        assert_eq!(surface.pixel(0, 1), Some([0, 0, 0, 0]));
        assert_eq!(surface.pixel(1, 1), Some([255, 0, 0, 255]));
        assert_eq!(surface.pixel(2, 1), Some([255, 0, 0, 255]));
        assert_eq!(surface.pixel(3, 1), Some([0, 0, 0, 0]));
    }

    #[test]
    fn rounded_hidden_group_clips_child_corners() {
        let scene = parse_scene(
            "@scene x { width: 8px; height: 8px; @group crop { width: 8px; height: 8px; \
             overflow: hidden; border-radius: 4px; @rect fill { background: #f00; } } }",
        )
        .expect("valid scene");
        let surface = render(&scene).expect("rendered scene");
        assert_eq!(surface.pixel(0, 0), Some([0, 0, 0, 0]));
        let edge = surface.pixel(3, 0).expect("edge pixel");
        assert_eq!(&edge[..3], &[255, 0, 0]);
        assert!((1..=254).contains(&edge[3]), "edge was {edge:?}");
    }

    #[test]
    fn composites_group_opacity_in_linear_light() {
        let scene = parse_scene(
            "@scene x { width: 1px; height: 1px; background: #000; \
             @group faded { opacity: 0.5; @rect r { background: #f00; } } }",
        )
        .expect("valid scene");
        let surface = render(&scene).expect("rendered scene");
        let pixel = surface.pixel(0, 0).expect("pixel");
        assert!((187..=188).contains(&pixel[0]), "pixel was {pixel:?}");
        assert_eq!(&pixel[1..], &[0, 0, 255]);
    }

    #[test]
    fn rounded_rectangle_leaves_corner_transparent() {
        let scene = parse_scene(
            "@scene x { width: 8px; height: 8px; @rect r { background: #fff; \
             border-radius: 4px; } }",
        )
        .expect("valid scene");
        let surface = render(&scene).expect("rendered scene");
        assert_eq!(surface.pixel(0, 0), Some([0, 0, 0, 0]));
        let edge = surface.pixel(3, 0).expect("edge pixel");
        assert_eq!(&edge[..3], &[255, 255, 255]);
        assert!((1..=254).contains(&edge[3]), "edge was {edge:?}");
        assert_eq!(surface.pixel(4, 4), Some([255, 255, 255, 255]));
    }

    #[test]
    fn fractional_rectangle_edges_receive_partial_coverage() {
        let scene = parse_scene(
            "@scene x { width: 5px; height: 2px; @rect r { left: 1.5px; top: 0; \
             width: 2px; height: 2px; background: #fff; } }",
        )
        .expect("valid scene");
        let surface = render(&scene).expect("rendered scene");
        assert_eq!(surface.pixel(0, 0), Some([0, 0, 0, 0]));
        let left = surface.pixel(1, 0).expect("left edge");
        let middle = surface.pixel(2, 0).expect("middle");
        let right = surface.pixel(3, 0).expect("right edge");
        assert!((120..=136).contains(&left[3]), "left edge was {left:?}");
        assert_eq!(middle, [255, 255, 255, 255]);
        assert!((120..=136).contains(&right[3]), "right edge was {right:?}");
        assert_eq!(surface.pixel(4, 0), Some([0, 0, 0, 0]));
    }

    #[test]
    fn text_requires_explicit_font_bytes() {
        let scene = parse_scene(
            r#"@scene x { width: 160px; height: 60px;
                @font Inter { src: "Inter.ttf"; }
                @text title { content: "MMFX"; font-family: Inter; font-size: 30px; }
            }"#,
        )
        .expect("valid scene");
        let error = render(&scene).expect_err("font bytes are required");
        assert!(error.to_string().contains("no bytes were supplied"));
    }

    #[test]
    fn shapes_and_antialiases_text_with_parley_and_swash() {
        let scene = parse_scene(
            r#"@scene x { width: 220px; height: 80px;
                @font Inter { src: "Inter.ttf"; }
                @text title {
                    left: 8.5px; top: 4px; width: 200px; height: 70px;
                    content: "MMRecode office"; font-family: Inter;
                    font-size: 31px; font-weight: 620; line-height: 1.2;
                    color: #f4f7f8;
                }
            }"#,
        )
        .expect("valid scene");
        let surface = render_with_resources(&scene, &inter_resources()).expect("rendered text");
        let rgba = surface.to_rgba8();
        let pixels = rgba.as_chunks::<4>().0;
        let painted = pixels.iter().filter(|pixel| pixel[3] != 0).count();
        let antialiased = pixels
            .iter()
            .filter(|pixel| (1..=254).contains(&pixel[3]))
            .count();
        assert!(painted > 1_000, "painted pixel count was {painted}");
        assert!(
            antialiased > 200,
            "antialiased pixel count was {antialiased}"
        );
    }

    #[test]
    fn wraps_text_to_the_typed_box_width() {
        let scene = parse_scene(
            r#"@scene x { width: 120px; height: 120px;
                @font Inter { src: "Inter.ttf"; }
                @text body {
                    width: 100px; height: 110px; content: "alpha beta gamma delta";
                    font-family: Inter; font-size: 22px; line-height: 1.2; color: #fff;
                }
            }"#,
        )
        .expect("valid scene");
        let surface = render_with_resources(&scene, &inter_resources()).expect("rendered text");
        let rgba = surface.to_rgba8();
        let painted_rows = (0..surface.height())
            .filter(|y| {
                let width = usize::try_from(surface.width()).expect("width");
                let y = usize::try_from(*y).expect("y");
                rgba[y * width * 4..(y + 1) * width * 4]
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .any(|pixel| pixel[3] != 0)
            })
            .count();
        assert!(painted_rows > 40, "painted row count was {painted_rows}");
    }
}
