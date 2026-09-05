//! Deterministic scalar CPU reference renderer for typed MMFX scenes.

use std::{borrow::Cow, collections::BTreeMap, fmt, sync::Arc};

use crate::{
    AlignItems, AnimationDuration, Color, Display, ImageContent, JustifyContent, Keyframe, Length,
    Node, NodeKind, ObjectFit, Overflow, Position, Scene, ScrollDirection, Style, TextAlign,
    TextContent, TextLineHeight, TextWrap, TimingFunction, Transform,
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
            Self::Auto => containing,
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
    images: BTreeMap<String, ImageResource>,
}

#[derive(Clone, Debug)]
struct ImageResource {
    width: u32,
    height: u32,
    rgba: Arc<Vec<u8>>,
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

    /// Add or replace one decoded, tightly packed straight-alpha sRGBA8 image.
    ///
    /// # Errors
    ///
    /// Returns an error when dimensions overflow or the byte count is not `width * height * 4`.
    pub fn add_image(
        &mut self,
        source: impl Into<String>,
        width: u32,
        height: u32,
        rgba: Vec<u8>,
    ) -> Result<(), RenderError> {
        let expected = usize::try_from(width)
            .ok()
            .and_then(|width| {
                usize::try_from(height)
                    .ok()
                    .and_then(|height| width.checked_mul(height))
            })
            .and_then(|pixels| pixels.checked_mul(4))
            .ok_or_else(|| RenderError {
                message: "MMFX image dimensions overflow the host address space".into(),
            })?;
        if width == 0 || height == 0 || rgba.len() != expected {
            return Err(RenderError {
                message: format!(
                    "MMFX image resource must be non-empty {width}x{height} sRGBA8 ({expected} bytes, received {})",
                    rgba.len()
                ),
            });
        }
        self.images.insert(
            source.into(),
            ImageResource {
                width,
                height,
                rgba: Arc::new(rgba),
            },
        );
        Ok(())
    }
}

/// Scene-local time supplied to animation and scrolling evaluation.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct SceneTime {
    /// Zero-based local frame.
    pub frame: u64,
    /// Total local frame count, used by `scene` durations.
    pub frame_count: u64,
}

impl SceneTime {
    /// Construct a clamped scene-local time.
    #[must_use]
    pub const fn new(frame: u64, frame_count: u64) -> Self {
        let frame_count = if frame_count == 0 { 1 } else { frame_count };
        Self {
            frame: if frame >= frame_count {
                frame_count - 1
            } else {
                frame
            },
            frame_count,
        }
    }
}

impl Default for SceneTime {
    fn default() -> Self {
        Self::new(0, 1)
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
    render_frame_with_resources(scene, resources, SceneTime::default())
}

/// Render one scene-local animation frame with explicit resources.
///
/// This convenience entry point prepares resources for one call. Playback and export should use
/// [`prepare_scene`] so font registration and other invariant setup happen only once.
///
/// # Errors
///
/// Returns an error for invalid resources or an impractically large output.
pub fn render_frame_with_resources(
    scene: &Scene,
    resources: &RenderResources,
    time: SceneTime,
) -> Result<Surface, RenderError> {
    prepare_scene(scene, resources)?.render_frame(time)
}

/// Compile invariant renderer state for repeated frame evaluation.
///
/// # Errors
///
/// Returns an error when declared fonts or images are missing or invalid.
pub fn prepare_scene(
    scene: &Scene,
    resources: &RenderResources,
) -> Result<PreparedScene, RenderError> {
    Ok(PreparedScene {
        scene: scene.clone(),
        state: RenderState::new(scene, resources)?,
        resources: resources.clone(),
    })
}

/// A parsed scene with registered fonts and decoded images ready for repeated evaluation.
pub struct PreparedScene {
    scene: Scene,
    state: RenderState,
    resources: RenderResources,
}

impl PreparedScene {
    /// Access the immutable typed scene.
    #[must_use]
    pub const fn scene(&self) -> &Scene {
        &self.scene
    }

    /// Evaluate and render one local frame without reparsing or re-registering resources.
    ///
    /// # Errors
    ///
    /// Returns an error for an impractically large output or invalid prepared resources.
    pub fn render_frame(&mut self, time: SceneTime) -> Result<Surface, RenderError> {
        let scene = &self.scene;
        let mut surface = Surface::transparent(scene.width, scene.height)?;
        render_scene(scene, &self.resources, &mut self.state, time, &mut surface)?;
        Ok(surface)
    }
}

fn render_scene(
    scene: &Scene,
    resources: &RenderResources,
    state: &mut RenderState,
    time: SceneTime,
    surface: &mut Surface,
) -> Result<(), RenderError> {
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
    fill_rounded(surface, viewport, 0.0, scene.background, clip, &[]);
    let mut coverage_clips = Vec::new();
    for child in &scene.children {
        draw_node(
            surface,
            child,
            viewport,
            None,
            clip,
            &mut coverage_clips,
            state,
            resources,
            scene,
            time,
        )?;
    }
    Ok(())
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
        for source in scene.image_sources() {
            if !resources.images.contains_key(source) {
                return Err(RenderError {
                    message: format!("image '{source}' was referenced but no pixels were supplied"),
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

struct EvaluatedNode {
    style: Style,
    text_color: Option<Color>,
}

fn evaluate_node(node: &Node, scene: &Scene, time: SceneTime) -> EvaluatedNode {
    let mut style = node.style.clone();
    let mut text_color = match &node.kind {
        NodeKind::Text(text) => Some(text.color),
        _ => None,
    };
    let Some(animation) = &node.style.animation else {
        return EvaluatedNode { style, text_color };
    };
    let Some(keyframes) = scene
        .animations
        .iter()
        .find(|keyframes| keyframes.name == animation.name)
    else {
        return EvaluatedNode { style, text_color };
    };
    let progress = duration_progress(animation.duration, time);
    let (start, end) = surrounding_stops(&keyframes.stops, progress);
    let span = (end.offset - start.offset).max(f32::EPSILON);
    let segment = ((progress - start.offset) / span).clamp(0.0, 1.0);
    let amount = apply_timing(animation.timing, segment);

    style.left = lerp_optional_length(
        start.style.left.or(style.left),
        end.style.left.or(style.left),
        amount,
    );
    style.top = lerp_optional_length(
        start.style.top.or(style.top),
        end.style.top.or(style.top),
        amount,
    );
    style.width = lerp_length(
        start.style.width.unwrap_or(style.width),
        end.style.width.unwrap_or(style.width),
        amount,
    );
    style.height = lerp_length(
        start.style.height.unwrap_or(style.height),
        end.style.height.unwrap_or(style.height),
        amount,
    );
    style.background = lerp_color(
        start.style.background.unwrap_or(style.background),
        end.style.background.unwrap_or(style.background),
        amount,
    );
    style.opacity = lerp_u16(
        start.style.opacity.unwrap_or(style.opacity),
        end.style.opacity.unwrap_or(style.opacity),
        amount,
    );
    style.transform = lerp_transform(
        start.style.transform.unwrap_or(style.transform),
        end.style.transform.unwrap_or(style.transform),
        amount,
    );
    if let Some(base) = text_color {
        text_color = Some(lerp_color(
            start.style.color.unwrap_or(base),
            end.style.color.unwrap_or(base),
            amount,
        ));
    }
    EvaluatedNode { style, text_color }
}

fn surrounding_stops(stops: &[Keyframe], progress: f32) -> (&Keyframe, &Keyframe) {
    let first = stops.first().expect("validated keyframes have stops");
    let last = stops.last().expect("validated keyframes have stops");
    if progress <= first.offset {
        return (first, first);
    }
    for pair in stops.windows(2) {
        if progress <= pair[1].offset {
            return (&pair[0], &pair[1]);
        }
    }
    (last, last)
}

#[allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)]
fn duration_progress(duration: AnimationDuration, time: SceneTime) -> f32 {
    let last = match duration {
        AnimationDuration::Frames(frames) => u64::from(frames.saturating_sub(1).max(1)),
        AnimationDuration::Scene => time.frame_count.saturating_sub(1).max(1),
    };
    (time.frame.min(last) as f64 / last as f64) as f32
}

fn apply_timing(timing: TimingFunction, value: f32) -> f32 {
    let value = value.clamp(0.0, 1.0);
    match timing {
        TimingFunction::Linear => value,
        TimingFunction::Ease | TimingFunction::EaseInOut => value * value * (3.0 - 2.0 * value),
        TimingFunction::EaseIn => value * value,
        TimingFunction::EaseOut => 1.0 - (1.0 - value) * (1.0 - value),
    }
}

fn lerp_optional_length(start: Option<Length>, end: Option<Length>, amount: f32) -> Option<Length> {
    match (start, end) {
        (Some(start), Some(end)) => Some(lerp_length(start, end, amount)),
        (Some(value), None) | (None, Some(value)) => Some(value),
        (None, None) => None,
    }
}

fn lerp_length(start: Length, end: Length, amount: f32) -> Length {
    match (start, end) {
        (Length::Pixels(start), Length::Pixels(end)) => {
            Length::Pixels(start + (end - start) * amount)
        }
        (Length::Percent(start), Length::Percent(end)) => {
            Length::Percent(start + (end - start) * amount)
        }
        (start, end) => {
            if amount < 0.5 {
                start
            } else {
                end
            }
        }
    }
}

fn lerp_transform(start: Transform, end: Transform, amount: f32) -> Transform {
    Transform {
        translate_x: lerp_length(start.translate_x, end.translate_x, amount),
        translate_y: lerp_length(start.translate_y, end.translate_y, amount),
        scale_x: start.scale_x + (end.scale_x - start.scale_x) * amount,
        scale_y: start.scale_y + (end.scale_y - start.scale_y) * amount,
        rotate_degrees: start.rotate_degrees + (end.rotate_degrees - start.rotate_degrees) * amount,
    }
}

fn lerp_color(start: Color, end: Color, amount: f32) -> Color {
    let channel = |start: u8, end: u8| {
        let value = f32::from(start) + (f32::from(end) - f32::from(start)) * amount;
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        {
            value.round().clamp(0.0, 255.0) as u8
        }
    };
    Color::rgba(
        channel(start.red, end.red),
        channel(start.green, end.green),
        channel(start.blue, end.blue),
        channel(start.alpha, end.alpha),
    )
}

fn lerp_u16(start: u16, end: u16, amount: f32) -> u16 {
    let value = f32::from(start) + (f32::from(end) - f32::from(start)) * amount;
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    {
        value.round().clamp(0.0, f32::from(u16::MAX)) as u16
    }
}

fn lerp(start: f64, end: f64, amount: f64) -> f64 {
    start + (end - start) * amount.clamp(0.0, 1.0)
}

#[allow(clippy::too_many_arguments)]
fn draw_node(
    target: &mut Surface,
    node: &Node,
    parent: Bounds,
    assigned: Option<Bounds>,
    inherited_clip: Clip,
    coverage_clips: &mut Vec<CoverageMask>,
    state: &mut RenderState,
    resources: &RenderResources,
    scene: &Scene,
    time: SceneTime,
) -> Result<(), RenderError> {
    let evaluated = evaluate_node(node, scene, time);
    let intrinsic = if assigned.is_none() {
        Some(measure_node(node, parent, state, resources, scene, time)?)
    } else {
        None
    };
    let bounds = resolve_bounds(&evaluated.style, parent, assigned, intrinsic, time);
    let bounds_clip = Clip::from_bounds(bounds);
    let child_clip = if evaluated.style.overflow == Overflow::Hidden {
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
        evaluated.style.background,
        inherited_clip,
        coverage_clips,
    );
    let adds_clip = evaluated.style.overflow == Overflow::Hidden;
    if adds_clip {
        coverage_clips.push(rasterize_rounded_rect(bounds, radius));
    }
    match &node.kind {
        NodeKind::Text(text) => draw_text(
            &mut layer,
            text,
            evaluated.text_color.unwrap_or(text.color),
            bounds,
            inherited_clip,
            coverage_clips,
            state,
        )?,
        NodeKind::Image(image) => draw_image(
            &mut layer,
            image,
            bounds,
            inherited_clip,
            coverage_clips,
            resources,
        )?,
        NodeKind::Group | NodeKind::Rect => {}
    }
    draw_children(
        &mut layer,
        &node.children,
        bounds,
        child_clip,
        coverage_clips,
        state,
        resources,
        scene,
        time,
        &evaluated.style,
    )?;
    if adds_clip {
        coverage_clips.pop();
    }
    if (evaluated.style.transform.scale_x - 1.0).abs() > f32::EPSILON
        || (evaluated.style.transform.scale_y - 1.0).abs() > f32::EPSILON
        || evaluated.style.transform.rotate_degrees.abs() > f32::EPSILON
    {
        layer = transform_surface(&layer, bounds, evaluated.style.transform)?;
    }
    target.blend_surface(&layer, evaluated.style.opacity, inherited_clip);
    Ok(())
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn draw_children(
    layer: &mut Surface,
    children: &[Node],
    parent: Bounds,
    clip: Clip,
    coverage_clips: &mut Vec<CoverageMask>,
    state: &mut RenderState,
    resources: &RenderResources,
    scene: &Scene,
    time: SceneTime,
    parent_style: &Style,
) -> Result<(), RenderError> {
    let padding = parent_style.padding.resolve_f64(parent.width).max(0.0);
    let content = Bounds {
        x: parent.x + padding,
        y: parent.y + padding,
        width: (parent.width - padding * 2.0).max(0.0),
        height: (parent.height - padding * 2.0).max(0.0),
    };
    if parent_style.display == Display::Overlay {
        for child in children {
            draw_node(
                layer,
                child,
                content,
                None,
                clip,
                coverage_clips,
                state,
                resources,
                scene,
                time,
            )?;
        }
        return Ok(());
    }

    let flow = children
        .iter()
        .filter(|child| evaluate_node(child, scene, time).style.position == Position::Flow)
        .collect::<Vec<_>>();
    let main_extent = if parent_style.display == Display::Row {
        content.width
    } else {
        content.height
    };
    let base_gap = parent_style.gap.resolve_f64(main_extent).max(0.0);
    let measured = flow
        .iter()
        .map(|child| measure_node(child, content, state, resources, scene, time))
        .collect::<Result<Vec<_>, _>>()?;
    let main_sizes = measured
        .iter()
        .map(|size| {
            if parent_style.display == Display::Row {
                size.width
            } else {
                size.height
            }
        })
        .collect::<Vec<_>>();
    let gaps = flow.len().saturating_sub(1);
    let gaps_f64 = f64::from(u32::try_from(gaps).unwrap_or(u32::MAX));
    let packed = main_sizes.iter().sum::<f64>() + base_gap * gaps_f64;
    let free = (main_extent - packed).max(0.0);
    let (mut cursor, gap) = match parent_style.justify_content {
        JustifyContent::SpaceBetween if gaps > 0 => (
            0.0,
            base_gap + free / f64::from(u32::try_from(gaps).unwrap_or(u32::MAX)),
        ),
        JustifyContent::Center => (free / 2.0, base_gap),
        JustifyContent::End => (free, base_gap),
        JustifyContent::Start | JustifyContent::SpaceBetween => (0.0, base_gap),
    };
    for ((child, main_size), measured) in flow.iter().zip(main_sizes).zip(measured) {
        let natural_cross = if parent_style.display == Display::Row {
            measured.height
        } else {
            measured.width
        };
        let cross_extent = if parent_style.align_items == AlignItems::Stretch {
            if parent_style.display == Display::Row {
                content.height
            } else {
                content.width
            }
        } else {
            natural_cross
        };
        let available_cross = if parent_style.display == Display::Row {
            content.height
        } else {
            content.width
        };
        let cross = match parent_style.align_items {
            AlignItems::Start | AlignItems::Stretch => 0.0,
            AlignItems::Center => (available_cross - cross_extent) / 2.0,
            AlignItems::End => available_cross - cross_extent,
        }
        .max(0.0);
        let assigned = if parent_style.display == Display::Row {
            Bounds {
                x: content.x + cursor,
                y: content.y + cross,
                width: main_size,
                height: cross_extent,
            }
        } else {
            Bounds {
                x: content.x + cross,
                y: content.y + cursor,
                width: cross_extent,
                height: main_size,
            }
        };
        draw_node(
            layer,
            child,
            content,
            Some(assigned),
            clip,
            coverage_clips,
            state,
            resources,
            scene,
            time,
        )?;
        cursor += main_size + gap;
    }
    for child in children {
        if evaluate_node(child, scene, time).style.position == Position::Absolute {
            draw_node(
                layer,
                child,
                content,
                None,
                clip,
                coverage_clips,
                state,
                resources,
                scene,
                time,
            )?;
        }
    }
    Ok(())
}

fn resolve_bounds(
    style: &Style,
    parent: Bounds,
    assigned: Option<Bounds>,
    intrinsic: Option<IntrinsicSize>,
    time: SceneTime,
) -> Bounds {
    let mut bounds = assigned.unwrap_or_else(|| {
        let intrinsic = intrinsic.unwrap_or_default();
        let width = resolve_box_axis(
            style.width,
            intrinsic.width,
            parent.width,
            style.min_width,
            style.max_width,
        );
        let height = resolve_box_axis(
            style.height,
            intrinsic.height,
            parent.height,
            style.min_height,
            style.max_height,
        );
        let x = if let Some(left) = style.left {
            parent.x + left.resolve_f64(parent.width)
        } else if let Some(right) = style.right {
            parent.x + parent.width - right.resolve_f64(parent.width) - width
        } else {
            parent.x
        };
        let y = if let Some(top) = style.top {
            parent.y + top.resolve_f64(parent.height)
        } else if let Some(bottom) = style.bottom {
            parent.y + parent.height - bottom.resolve_f64(parent.height) - height
        } else {
            parent.y
        };
        Bounds {
            x,
            y,
            width,
            height,
        }
    });
    bounds.x += style.transform.translate_x.resolve_f64(parent.width);
    bounds.y += style.transform.translate_y.resolve_f64(parent.height);
    if let Some(scroll) = style.scroll {
        let progress = duration_progress(scroll.duration, time);
        apply_scroll(&mut bounds, parent, scroll.direction, f64::from(progress));
    }
    bounds
}

#[derive(Clone, Copy, Debug, Default)]
struct IntrinsicSize {
    width: f64,
    height: f64,
}

fn resolve_box_axis(
    declared: Length,
    intrinsic: f64,
    containing: f64,
    minimum: Option<Length>,
    maximum: Option<Length>,
) -> f64 {
    let mut value = match declared {
        Length::Auto => intrinsic,
        length => length.resolve_f64(containing),
    }
    .max(0.0);
    if let Some(minimum) = minimum {
        value = value.max(minimum.resolve_f64(containing));
    }
    if let Some(maximum) = maximum {
        value = value.min(maximum.resolve_f64(containing).max(0.0));
    }
    value
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn measure_node(
    node: &Node,
    parent: Bounds,
    state: &mut RenderState,
    resources: &RenderResources,
    scene: &Scene,
    time: SceneTime,
) -> Result<IntrinsicSize, RenderError> {
    let evaluated = evaluate_node(node, scene, time);
    let style = &evaluated.style;
    let declared_width = (!matches!(style.width, Length::Auto))
        .then(|| style.width.resolve_f64(parent.width).max(0.0));
    let declared_height = (!matches!(style.height, Length::Auto))
        .then(|| style.height.resolve_f64(parent.height).max(0.0));
    if let (Some(width), Some(height)) = (declared_width, declared_height) {
        return Ok(IntrinsicSize {
            width: resolve_box_axis(
                style.width,
                width,
                parent.width,
                style.min_width,
                style.max_width,
            ),
            height: resolve_box_axis(
                style.height,
                height,
                parent.height,
                style.min_height,
                style.max_height,
            ),
        });
    }
    let mut available_width = declared_width.unwrap_or(parent.width).max(0.0);
    let mut available_height = declared_height.unwrap_or(parent.height).max(0.0);
    if let Some(maximum) = style.max_width {
        available_width = available_width.min(maximum.resolve_f64(parent.width).max(0.0));
    }
    if let Some(maximum) = style.max_height {
        available_height = available_height.min(maximum.resolve_f64(parent.height).max(0.0));
    }
    let padding = style.padding.resolve_f64(available_width).max(0.0);
    let inner = Bounds {
        x: 0.0,
        y: 0.0,
        width: (available_width - padding * 2.0).max(0.0),
        height: (available_height - padding * 2.0).max(0.0),
    };

    let natural = match &node.kind {
        NodeKind::Text(text) => {
            let wrap_width = match (style.width, text.wrap) {
                (Length::Auto, TextWrap::NoWrap) => None,
                _ => Some(inner.width),
            };
            let layout = build_text_layout(
                text,
                evaluated.text_color.unwrap_or(text.color),
                wrap_width,
                state,
            );
            IntrinsicSize {
                width: f64::from(layout.full_width()),
                height: f64::from(layout.height()),
            }
        }
        NodeKind::Image(image) => {
            let resource = resources
                .images
                .get(&image.source)
                .ok_or_else(|| RenderError {
                    message: format!("image '{}' has no prepared pixels", image.source),
                })?;
            let aspect = f64::from(resource.width) / f64::from(resource.height);
            match (declared_width, declared_height) {
                (Some(width), None) => IntrinsicSize {
                    width,
                    height: width / aspect,
                },
                (None, Some(height)) => IntrinsicSize {
                    width: height * aspect,
                    height,
                },
                _ => IntrinsicSize {
                    width: f64::from(resource.width),
                    height: f64::from(resource.height),
                },
            }
        }
        NodeKind::Group | NodeKind::Rect => {
            let flow = node
                .children
                .iter()
                .filter(|child| evaluate_node(child, scene, time).style.position == Position::Flow)
                .collect::<Vec<_>>();
            let child_sizes = flow
                .iter()
                .map(|child| measure_node(child, inner, state, resources, scene, time))
                .collect::<Result<Vec<_>, _>>()?;
            let gap_extent = if style.display == Display::Row {
                inner.width
            } else {
                inner.height
            };
            let gap = style.gap.resolve_f64(gap_extent).max(0.0);
            let gap_count =
                f64::from(u32::try_from(flow.len().saturating_sub(1)).unwrap_or(u32::MAX));
            let content = match style.display {
                Display::Row => IntrinsicSize {
                    width: child_sizes.iter().map(|size| size.width).sum::<f64>() + gap * gap_count,
                    height: child_sizes
                        .iter()
                        .map(|size| size.height)
                        .fold(0.0, f64::max),
                },
                Display::Column => IntrinsicSize {
                    width: child_sizes
                        .iter()
                        .map(|size| size.width)
                        .fold(0.0, f64::max),
                    height: child_sizes.iter().map(|size| size.height).sum::<f64>()
                        + gap * gap_count,
                },
                Display::Overlay => IntrinsicSize {
                    width: child_sizes
                        .iter()
                        .map(|size| size.width)
                        .fold(0.0, f64::max),
                    height: child_sizes
                        .iter()
                        .map(|size| size.height)
                        .fold(0.0, f64::max),
                },
            };
            IntrinsicSize {
                width: content.width + padding * 2.0,
                height: content.height + padding * 2.0,
            }
        }
    };

    Ok(IntrinsicSize {
        width: resolve_box_axis(
            style.width,
            natural.width,
            parent.width,
            style.min_width,
            style.max_width,
        ),
        height: resolve_box_axis(
            style.height,
            natural.height,
            parent.height,
            style.min_height,
            style.max_height,
        ),
    })
}

fn apply_scroll(bounds: &mut Bounds, parent: Bounds, direction: ScrollDirection, progress: f64) {
    match direction {
        ScrollDirection::BlockStart => {
            bounds.y += lerp(parent.height, -bounds.height, progress);
        }
        ScrollDirection::BlockEnd => {
            bounds.y += lerp(-bounds.height, parent.height, progress);
        }
        ScrollDirection::InlineStart => {
            bounds.x += lerp(parent.width, -bounds.width, progress);
        }
        ScrollDirection::InlineEnd => {
            bounds.x += lerp(-bounds.width, parent.width, progress);
        }
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

#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn draw_image(
    surface: &mut Surface,
    image: &ImageContent,
    bounds: Bounds,
    clip: Clip,
    coverage_masks: &[CoverageMask],
    resources: &RenderResources,
) -> Result<(), RenderError> {
    let resource = resources
        .images
        .get(&image.source)
        .ok_or_else(|| RenderError {
            message: format!("image '{}' has no prepared pixels", image.source),
        })?;
    if bounds.width <= 0.0 || bounds.height <= 0.0 {
        return Ok(());
    }
    let source_aspect = f64::from(resource.width) / f64::from(resource.height);
    let box_aspect = bounds.width / bounds.height;
    let width_limited = source_aspect >= box_aspect;
    let use_width = match image.fit {
        ObjectFit::Fill => None,
        ObjectFit::Contain => Some(width_limited),
        ObjectFit::Cover => Some(!width_limited),
    };
    let (width, height) = match use_width {
        None => (bounds.width, bounds.height),
        Some(true) => (bounds.width, bounds.width / source_aspect),
        Some(false) => (bounds.height * source_aspect, bounds.height),
    };
    let destination = Bounds {
        x: bounds.x + (bounds.width - width) / 2.0,
        y: bounds.y + (bounds.height - height) / 2.0,
        width,
        height,
    };
    let paint_clip = clip
        .intersect(Clip::from_bounds(destination))
        .intersect(Clip::from_bounds(bounds));
    let surface_width = i32::try_from(surface.width).unwrap_or(i32::MAX);
    let surface_height = i32::try_from(surface.height).unwrap_or(i32::MAX);
    for y in paint_clip.top.max(0)..paint_clip.bottom.min(surface_height) {
        for x in paint_clip.left.max(0)..paint_clip.right.min(surface_width) {
            let u = (f64::from(x) + 0.5 - destination.x) / destination.width;
            let v = (f64::from(y) + 0.5 - destination.y) / destination.height;
            if !(0.0..1.0).contains(&u) || !(0.0..1.0).contains(&v) {
                continue;
            }
            let source_x = (u * f64::from(resource.width)).floor() as u32;
            let source_y = (v * f64::from(resource.height)).floor() as u32;
            let offset = (source_y as usize * resource.width as usize + source_x as usize) * 4;
            let rgba = &resource.rgba[offset..offset + 4];
            let mut opacity = u16::MAX;
            for coverage in coverage_masks {
                opacity = multiply_channel(opacity, u16::from(coverage.coverage_at(x, y)) * 257);
            }
            if opacity == 0 {
                continue;
            }
            let source = LinearPixel::from_rgba(rgba);
            let index = surface.index(x, y);
            surface.pixels[index] = source.over(surface.pixels[index], opacity);
        }
    }
    Ok(())
}

fn transform_surface(
    source: &Surface,
    bounds: Bounds,
    transform: Transform,
) -> Result<Surface, RenderError> {
    let mut output = Surface::transparent(source.width, source.height)?;
    if transform.scale_x <= f32::EPSILON || transform.scale_y <= f32::EPSILON {
        return Ok(output);
    }
    let scale_x = f64::from(transform.scale_x);
    let scale_y = f64::from(transform.scale_y);
    let radians = f64::from(transform.rotate_degrees).to_radians();
    let (sin, cos) = radians.sin_cos();
    let center_x = bounds.x + bounds.width / 2.0;
    let center_y = bounds.y + bounds.height / 2.0;
    let corners = [
        (bounds.x, bounds.y),
        (bounds.x + bounds.width, bounds.y),
        (bounds.x, bounds.y + bounds.height),
        (bounds.x + bounds.width, bounds.y + bounds.height),
    ];
    let transformed = corners.map(|(x, y)| {
        let x = (x - center_x) * scale_x;
        let y = (y - center_y) * scale_y;
        (center_x + x * cos - y * sin, center_y + x * sin + y * cos)
    });
    let left = transformed
        .iter()
        .map(|point| point.0)
        .fold(f64::INFINITY, f64::min);
    let right = transformed
        .iter()
        .map(|point| point.0)
        .fold(f64::NEG_INFINITY, f64::max);
    let top = transformed
        .iter()
        .map(|point| point.1)
        .fold(f64::INFINITY, f64::min);
    let bottom = transformed
        .iter()
        .map(|point| point.1)
        .fold(f64::NEG_INFINITY, f64::max);
    let destination = Clip::from_bounds(Bounds {
        x: left,
        y: top,
        width: right - left,
        height: bottom - top,
    });
    let width = i32::try_from(output.width).unwrap_or(i32::MAX);
    let height = i32::try_from(output.height).unwrap_or(i32::MAX);
    for y in destination.top.max(0)..destination.bottom.min(height) {
        for x in destination.left.max(0)..destination.right.min(width) {
            let destination_x = f64::from(x) + 0.5 - center_x;
            let destination_y = f64::from(y) + 0.5 - center_y;
            let source_x = (destination_x * cos + destination_y * sin) / scale_x + center_x;
            let source_y = (-destination_x * sin + destination_y * cos) / scale_y + center_y;
            let pixel = sample_surface(source, source_x - 0.5, source_y - 0.5);
            let index = output.index(x, y);
            output.pixels[index] = pixel;
        }
    }
    Ok(output)
}

fn sample_surface(surface: &Surface, x: f64, y: f64) -> LinearPixel {
    let x0 = floor_to_i32(x);
    let y0 = floor_to_i32(y);
    let x1 = x0.saturating_add(1);
    let y1 = y0.saturating_add(1);
    let fx = (x - f64::from(x0)).clamp(0.0, 1.0);
    let fy = (y - f64::from(y0)).clamp(0.0, 1.0);
    let sample = |x, y| {
        if x < 0
            || y < 0
            || x >= i32::try_from(surface.width).unwrap_or(i32::MAX)
            || y >= i32::try_from(surface.height).unwrap_or(i32::MAX)
        {
            LinearPixel::TRANSPARENT
        } else {
            surface.pixels[surface.index(x, y)]
        }
    };
    let top = LinearPixel::lerp(sample(x0, y0), sample(x1, y0), fx);
    let bottom = LinearPixel::lerp(sample(x0, y1), sample(x1, y1), fx);
    LinearPixel::lerp(top, bottom, fy)
}

fn draw_text(
    surface: &mut Surface,
    text: &TextContent,
    color: Color,
    bounds: Bounds,
    clip: Clip,
    coverage_masks: &[CoverageMask],
    state: &mut RenderState,
) -> Result<(), RenderError> {
    let layout = build_text_layout(text, color, Some(bounds.width), state);

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

fn build_text_layout(
    text: &TextContent,
    color: Color,
    width: Option<f64>,
    state: &mut RenderState,
) -> Layout<Color> {
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
    builder.push_default(StyleProperty::Brush(color));
    let mut layout: Layout<Color> = builder.build(&text.content);
    layout.break_all_lines(width.map(f64_to_f32));
    layout.align(
        match text.align {
            TextAlign::Start => Alignment::Start,
            TextAlign::Center => Alignment::Center,
            TextAlign::End => Alignment::End,
        },
        AlignmentOptions::default(),
    );
    layout
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

    fn from_rgba(rgba: &[u8]) -> Self {
        Self::from_color(Color::rgba(rgba[0], rgba[1], rgba[2], rgba[3]))
    }

    fn lerp(start: Self, end: Self, amount: f64) -> Self {
        let channel = |start: u16, end: u16| {
            let value = f64::from(start) + (f64::from(end) - f64::from(start)) * amount;
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            {
                value.round().clamp(0.0, f64::from(u16::MAX)) as u16
            }
        };
        Self {
            red: channel(start.red, end.red),
            green: channel(start.green, end.green),
            blue: channel(start.blue, end.blue),
            alpha: channel(start.alpha, end.alpha),
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
    use super::{Bounds, RenderState, measure_node};
    use crate::{
        RenderResources, SceneTime, parse_scene, prepare_scene, render, render_with_resources,
    };

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

    #[test]
    fn intrinsic_column_uses_measured_text_and_gap() {
        let scene = parse_scene(
            r#"@scene x { width: 240px; height: 180px;
                @font Inter { src: "Inter.ttf"; }
                @group stack { display: column; width: 160px; height: auto; gap: 8px;
                    @text a { width: 100%; height: auto; content: "alpha beta gamma";
                        font-family: Inter; font-size: 24px; line-height: 1.2; color: #fff; }
                    @text b { width: auto; max-width: 100px; height: auto;
                        content: "delta epsilon zeta"; font-family: Inter;
                        font-size: 20px; line-height: 1.2; color: #fff; }
                }
            }"#,
        )
        .expect("valid intrinsic scene");
        let resources = inter_resources();
        let mut state = RenderState::new(&scene, &resources).expect("font state");
        let measured = measure_node(
            &scene.children[0],
            Bounds {
                x: 0.0,
                y: 0.0,
                width: 240.0,
                height: 180.0,
            },
            &mut state,
            &resources,
            &scene,
            SceneTime::default(),
        )
        .expect("measure scene");
        assert_eq!(measured.width, 160.0);
        assert!(measured.height > 70.0, "height was {}", measured.height);
    }

    #[test]
    fn lays_out_row_children_with_gap_and_center_justification() {
        let scene = parse_scene(
            "@scene x { width: 20px; height: 10px; @group row { display: row; \
             justify-content: center; gap: 2px; @rect a { width: 4px; height: 10px; \
             background: #f00; } @rect b { width: 4px; height: 10px; background: #0f0; } } }",
        )
        .expect("valid row");
        let surface = render(&scene).expect("render row");
        assert_eq!(surface.pixel(4, 5), Some([0, 0, 0, 0]));
        assert_eq!(surface.pixel(5, 5), Some([255, 0, 0, 255]));
        assert_eq!(surface.pixel(9, 5), Some([0, 0, 0, 0]));
        assert_eq!(surface.pixel(11, 5), Some([0, 255, 0, 255]));
        assert_eq!(surface.pixel(15, 5), Some([0, 0, 0, 0]));
    }

    #[test]
    fn renders_contained_image_resources() {
        let scene = parse_scene(
            r#"@scene x { width: 4px; height: 4px;
                @image logo { width: 4px; height: 4px; src: "logo.png"; object-fit: contain; }
            }"#,
        )
        .expect("valid image");
        let mut resources = RenderResources::new();
        resources
            .add_image("logo.png", 2, 1, vec![255, 0, 0, 255, 0, 0, 255, 255])
            .unwrap();
        let surface = render_with_resources(&scene, &resources).expect("render image");
        assert_eq!(surface.pixel(0, 0), Some([0, 0, 0, 0]));
        assert_eq!(surface.pixel(0, 1), Some([255, 0, 0, 255]));
        assert_eq!(surface.pixel(3, 2), Some([0, 0, 255, 255]));
        assert_eq!(surface.pixel(0, 3), Some([0, 0, 0, 0]));
    }

    #[test]
    fn prepared_scene_evaluates_keyframes_without_reparsing() {
        let scene = parse_scene(
            "@scene x { width: 16px; height: 4px; @rect card { width: 4px; height: 4px; \
             background: #f00; animation: move 3f linear; } } \
             @keyframes move { from { opacity: 1; transform: translateX(0); background: #f00; } \
             to { opacity: 0.5; transform: translateX(8px); background: #00f; } }",
        )
        .expect("valid animation");
        let mut prepared = prepare_scene(&scene, &RenderResources::new()).expect("prepared scene");
        let first = prepared.render_frame(SceneTime::new(0, 3)).unwrap();
        let middle = prepared.render_frame(SceneTime::new(1, 3)).unwrap();
        let last = prepared.render_frame(SceneTime::new(2, 3)).unwrap();
        assert_eq!(first.pixel(0, 1), Some([255, 0, 0, 255]));
        assert_eq!(middle.pixel(6, 1).expect("middle pixel")[3], 191);
        assert_eq!(last.pixel(8, 1), Some([0, 0, 255, 128]));
        assert_eq!(last.pixel(0, 1), Some([0, 0, 0, 0]));
    }

    #[test]
    fn scroll_cover_uses_complete_scene_duration() {
        let scene = parse_scene(
            "@scene x { width: 10px; height: 10px; @group crop { overflow: hidden; \
             @rect roll { width: 2px; height: 2px; background: #fff; \
             mm-scroll-direction: block-start; mm-scroll-duration: scene; } } }",
        )
        .expect("valid scroll");
        let mut prepared = prepare_scene(&scene, &RenderResources::new()).unwrap();
        let first = prepared.render_frame(SceneTime::new(0, 3)).unwrap();
        let middle = prepared.render_frame(SceneTime::new(1, 3)).unwrap();
        let last = prepared.render_frame(SceneTime::new(2, 3)).unwrap();
        assert_eq!(first.pixel(0, 9), Some([0, 0, 0, 0]));
        assert_eq!(middle.pixel(0, 4), Some([255, 255, 255, 255]));
        assert_eq!(last.pixel(0, 0), Some([0, 0, 0, 0]));
    }

    #[test]
    fn scale_and_rotation_transform_the_complete_node_layer() {
        let scene = parse_scene(
            "@scene x { width: 12px; height: 12px; @rect card { left: 4px; top: 4px; \
             width: 4px; height: 4px; background: #fff; transform: scale(1.5) rotate(45deg); } }",
        )
        .expect("valid transform");
        let surface = render(&scene).expect("transformed scene");
        assert_eq!(surface.pixel(6, 6), Some([255, 255, 255, 255]));
        assert_ne!(surface.pixel(3, 3), Some([0, 0, 0, 0]));
    }
}
