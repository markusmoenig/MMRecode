//! Cached CPU composition of generated MMFX objects in a project timeline.
//!
//! Parsing, font/image loading, and scene preparation happen only when a source or canvas changes.
//! Static scenes are rasterized once; animated frames are evaluated lazily at exact placement-local
//! time and retained in a bounded prepared-overlay cache. Repeated frames only look up active
//! layers and blend their prepared pixels into the caller's frame.

use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    fmt,
    hash::{DefaultHasher, Hash as _, Hasher as _},
    ops::Range,
};

use image::{RgbaImage, imageops::FilterType};
use mmrecode_core::{
    ColorRange, Error, PixelFormat, Rational, Result, Timestamp, TimestampRounding, VideoFrame,
};
use mmrecode_edit::{MediaId, MediaProject, MmfxSource, VisualScaleMode};
use mmrecode_mmfx::{PreparedScene, RenderResources, Scene, SceneTime};

const FRAME_CACHE_LIMIT: usize = 16;
const SCALED_FRAME_CACHE_LIMIT: usize = 32;

/// One MMFX source diagnostic produced while refreshing a project compositor.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProjectCompositorDiagnostic {
    /// Generated media definition which could not be compiled.
    pub media_id: MediaId,
    /// Source-spanned parse, resource, or render error.
    pub message: String,
}

/// Work performed by one incremental compositor refresh.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ProjectCompositorSync {
    /// Whether visible layer pixels or placement ranges changed.
    pub changed: bool,
    /// Number of scene/scale variants newly rasterized during this refresh.
    pub compiled_assets: usize,
    /// Number of unchanged scene/scale variants reused without rasterization.
    pub reused_assets: usize,
    /// Invalid assets. A previous valid image remains active when dimensions still match.
    pub diagnostics: Vec<ProjectCompositorDiagnostic>,
}

/// Incremental, CPU-authoritative compositor for one recursively flattened hierarchy context.
///
/// The object deliberately owns no decoder. A playback or export caller supplies the decoded base
/// frame, which lets UI seeks use latest-request-wins decoding and lets export decode sequentially.
/// MMFX scenes are cached by source, scale mode, and canvas dimensions. Static scenes own one
/// overlay; animated scenes reuse their prepared scene and keep a bounded set of local-frame
/// overlays. Both preview and export therefore use the same source-time evaluation.
#[derive(Default)]
pub struct ProjectCompositor {
    canvas: (u32, u32),
    context: Option<MediaId>,
    assets: BTreeMap<AssetKey, CachedAsset>,
    scaled_assets: BTreeMap<ScaledAssetKey, PreparedOverlay>,
    scaled_asset_order: VecDeque<ScaledAssetKey>,
    layers: Vec<Layer>,
    revision: u64,
}

impl fmt::Debug for ProjectCompositor {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProjectCompositor")
            .field("canvas", &self.canvas)
            .field("context", &self.context)
            .field("cached_assets", &self.assets.len())
            .field("scaled_assets", &self.scaled_assets.len())
            .field("scaled_asset_order", &self.scaled_asset_order)
            .field("layers", &self.layers)
            .field("revision", &self.revision)
            .finish()
    }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct AssetKey {
    media_id: MediaId,
    scale_mode: u8,
}

type ScaledAssetKey = (AssetKey, i64, u32, u32);

struct CachedAsset {
    attempted_signature: u64,
    good_signature: Option<u64>,
    canvas: (u32, u32),
    prepared: Option<PreparedScene>,
    static_overlay: Option<PreparedOverlay>,
    frame_overlays: BTreeMap<i64, Option<PreparedOverlay>>,
    frame_order: VecDeque<i64>,
    animated: bool,
    frame_count: u64,
    scale_mode: VisualScaleMode,
    error: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct Layer {
    asset: AssetKey,
    timeline: Range<i64>,
    source_signature: u64,
    composition_order: usize,
    mapping: Vec<TimeMappingStep>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct TimeMappingStep {
    parent_time_base: Rational,
    child_time_base: Rational,
    timeline_start: i64,
    timeline_end: i64,
    source_start: i64,
    source_end: i64,
}

impl Layer {
    fn source_frame(&self, context_frame: i64) -> Option<i64> {
        if !self.timeline.contains(&context_frame) {
            return None;
        }
        let mut frame = context_frame;
        for step in &self.mapping {
            if frame < step.timeline_start || frame >= step.timeline_end {
                return None;
            }
            let offset = frame.checked_sub(step.timeline_start)?;
            let source_offset = Timestamp {
                value: offset,
                time_base: step.parent_time_base,
            }
            .rescale(step.child_time_base, TimestampRounding::Floor)
            .ok()?
            .value;
            frame = step.source_start.checked_add(source_offset)?;
            if frame < step.source_start || frame >= step.source_end {
                return None;
            }
        }
        Some(frame)
    }
}

#[derive(Debug)]
struct PreparedOverlay {
    x: u32,
    y: u32,
    width: u32,
    height: u32,
    rgba: Vec<u8>,
    y_limited: Vec<u8>,
    y_full: Vec<u8>,
    u_limited: Vec<u8>,
    v_limited: Vec<u8>,
    u_full: Vec<u8>,
    v_full: Vec<u8>,
    chroma_alpha: Vec<u8>,
}

impl ProjectCompositor {
    /// Creates an empty compositor. The first [`Self::synchronize`] call establishes its canvas.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Incrementally synchronizes cached MMFX assets and recursive placements below `context`.
    ///
    /// If `context` is itself an MMFX scene, it is painted first for its entire local duration;
    /// direct scene children then paint in project composition order. Resource loading is supplied by
    /// the host so project-relative files and portable built-ins remain policy decisions outside
    /// the codec-independent render crate.
    #[allow(clippy::too_many_lines)]
    pub fn synchronize<F>(
        &mut self,
        project: &MediaProject,
        context: MediaId,
        mut load_resources: F,
    ) -> ProjectCompositorSync
    where
        F: FnMut(MediaId, &MmfxSource, &Scene) -> std::result::Result<RenderResources, String>,
    {
        let canvas = (project.settings().width, project.settings().height);
        let mut sync = ProjectCompositorSync::default();
        let requested = match requested_layers(project, context) {
            Ok(requested) => requested,
            Err(error) => {
                sync.diagnostics.push(ProjectCompositorDiagnostic {
                    media_id: context,
                    message: error.to_string(),
                });
                Vec::new()
            }
        };
        let live_media = project
            .media_nodes()
            .filter(|media| media.kind.is_mmfx_scene())
            .map(|media| media.id)
            .collect::<BTreeSet<_>>();
        let previous_asset_count = self.assets.len();
        self.assets
            .retain(|key, _| live_media.contains(&key.media_id));
        sync.changed |= previous_asset_count != self.assets.len();

        for request in &requested {
            let Some(media) = project.media(request.asset.media_id) else {
                continue;
            };
            let Some(source) = media.mmfx.as_ref() else {
                sync.diagnostics.push(ProjectCompositorDiagnostic {
                    media_id: media.id,
                    message: "generated MMFX scene has no embedded source".into(),
                });
                continue;
            };
            let signature = source_signature(source, canvas, request.asset.scale_mode);
            let cached = self.assets.get(&request.asset);
            if cached.is_some_and(|asset| {
                asset.attempted_signature == signature && asset.canvas == canvas
            }) {
                sync.reused_assets += 1;
                if let Some(message) = cached.and_then(|asset| asset.error.clone()) {
                    sync.diagnostics.push(ProjectCompositorDiagnostic {
                        media_id: media.id,
                        message,
                    });
                }
                continue;
            }

            let frame_count = u64::try_from(media.duration.value.max(1)).unwrap_or(u64::MAX);
            let result = compile_asset(
                media.id,
                source,
                canvas,
                mode_from_key(request.asset.scale_mode),
                frame_count,
                &mut load_resources,
            );
            let previous = self.assets.remove(&request.asset);
            match result {
                Ok(compiled) => {
                    self.assets.insert(
                        request.asset,
                        CachedAsset {
                            attempted_signature: signature,
                            good_signature: Some(signature),
                            canvas,
                            prepared: Some(compiled.prepared),
                            static_overlay: compiled.static_overlay,
                            frame_overlays: compiled.frame_overlays,
                            frame_order: compiled.frame_order,
                            animated: compiled.animated,
                            frame_count,
                            scale_mode: mode_from_key(request.asset.scale_mode),
                            error: None,
                        },
                    );
                    sync.compiled_assets += 1;
                    sync.changed = true;
                }
                Err(message) => {
                    let retained = previous.filter(|asset| asset.canvas == canvas);
                    if let Some(mut retained) = retained {
                        retained.attempted_signature = signature;
                        retained.error = Some(message.clone());
                        self.assets.insert(request.asset, retained);
                    } else {
                        self.assets.insert(
                            request.asset,
                            CachedAsset {
                                attempted_signature: signature,
                                good_signature: None,
                                canvas,
                                prepared: None,
                                static_overlay: None,
                                frame_overlays: BTreeMap::new(),
                                frame_order: VecDeque::new(),
                                animated: false,
                                frame_count,
                                scale_mode: mode_from_key(request.asset.scale_mode),
                                error: Some(message.clone()),
                            },
                        );
                    }
                    sync.diagnostics.push(ProjectCompositorDiagnostic {
                        media_id: media.id,
                        message,
                    });
                    sync.changed = true;
                }
            }
        }

        let layers = requested
            .into_iter()
            .filter_map(|request| {
                let cached = self.assets.get(&request.asset)?;
                Some(Layer {
                    asset: request.asset,
                    timeline: request.timeline,
                    source_signature: cached.good_signature?,
                    composition_order: request.composition_order,
                    mapping: request.mapping,
                })
            })
            .collect::<Vec<_>>();
        if self.canvas != canvas || self.context != Some(context) || self.layers != layers {
            self.canvas = canvas;
            self.context = Some(context);
            self.layers = layers;
            sync.changed = true;
        }
        if sync.changed {
            self.scaled_assets.clear();
            self.scaled_asset_order.clear();
            self.revision = self.revision.wrapping_add(1).max(1);
        }
        sync
    }

    /// Returns the project canvas for which cached pixels were prepared.
    #[must_use]
    pub const fn canvas(&self) -> (u32, u32) {
        self.canvas
    }

    /// Monotonic cache/layout revision, useful for invalidating UI image protocols.
    #[must_use]
    pub const fn revision(&self) -> u64 {
        self.revision
    }

    /// Returns whether this hierarchy level contains any successfully compiled MMFX layers.
    #[must_use]
    pub fn has_layers(&self) -> bool {
        !self.layers.is_empty()
    }

    /// Returns whether at least one successfully compiled MMFX layer is active at `frame`.
    #[must_use]
    pub fn has_active_layers(&self, frame: i64) -> bool {
        self.layers
            .iter()
            .any(|layer| layer.timeline.contains(&frame))
    }

    /// Returns a stable-in-process signature for the active layer stack at `frame`.
    #[must_use]
    pub fn active_signature(&self, frame: i64) -> u64 {
        let mut hasher = DefaultHasher::new();
        self.canvas.hash(&mut hasher);
        for layer in self
            .layers
            .iter()
            .filter(|layer| layer.timeline.contains(&frame))
        {
            layer.asset.media_id.hash(&mut hasher);
            layer.asset.scale_mode.hash(&mut hasher);
            layer.source_signature.hash(&mut hasher);
            layer.composition_order.hash(&mut hasher);
            if self
                .assets
                .get(&layer.asset)
                .is_some_and(|asset| asset.animated)
            {
                layer.source_frame(frame).hash(&mut hasher);
            }
        }
        hasher.finish()
    }

    /// Blends active cached layers into an opaque or transparent sRGBA8 canvas in place.
    ///
    /// # Errors
    ///
    /// Returns an error if `base` does not match the synchronized project canvas.
    pub fn composite_rgba8(&mut self, frame: i64, base: &mut RgbaImage) -> Result<()> {
        if base.dimensions() != self.canvas {
            return Err(Error::InvalidData(format!(
                "RGBA composition canvas is {}x{}, expected {}x{}",
                base.width(),
                base.height(),
                self.canvas.0,
                self.canvas.1
            )));
        }
        for (asset, local_frame) in self.active_assets(frame, 0) {
            if let Some(overlay) = self.overlay_for(asset, local_frame)? {
                overlay.blend_rgba(base);
            }
        }
        Ok(())
    }

    /// Blends active layers into an arbitrary preview-sized sRGBA8 image in place.
    ///
    /// Each layer is resized at most once per requested preview size and compositor revision. This
    /// keeps terminal resizing out of the frame-by-frame playback and scrubbing hot path.
    ///
    /// # Errors
    ///
    /// Returns an error if an animated scene frame cannot be evaluated or prepared.
    pub fn composite_rgba8_preview(&mut self, frame: i64, base: &mut RgbaImage) -> Result<()> {
        if base.dimensions() == self.canvas {
            return self.composite_rgba8(frame, base);
        }
        let target = base.dimensions();
        let canvas = self.canvas;
        for (asset_key, local_frame) in self.active_assets(frame, 0) {
            let frame_key = self
                .assets
                .get(&asset_key)
                .map_or(-1, |asset| if asset.animated { local_frame } else { -1 });
            let scaled_key = (asset_key, frame_key, target.0, target.1);
            if !self.scaled_assets.contains_key(&scaled_key) {
                let Some(source_canvas) = self
                    .overlay_for(asset_key, local_frame)?
                    .map(|source| source.to_canvas(canvas))
                else {
                    continue;
                };
                let scaled = image::imageops::resize(
                    &source_canvas,
                    target.0,
                    target.1,
                    FilterType::Triangle,
                );
                let Some(prepared) = PreparedOverlay::from_canvas(&scaled) else {
                    continue;
                };
                self.scaled_assets.insert(scaled_key, prepared);
            }
            self.touch_scaled_asset(scaled_key);
            if let Some(overlay) = self.scaled_assets.get(&scaled_key) {
                overlay.blend_rgba(base);
            }
        }
        Ok(())
    }

    /// Blends active cached layers directly into a planar 4:2:0 frame in place.
    ///
    /// RGB-to-YUV conversion and 4:2:0 alpha reduction are precomputed during synchronization,
    /// avoiding a full-frame RGB round trip for every exported frame.
    ///
    /// # Errors
    ///
    /// Returns an error for a malformed, unsupported, or differently sized base frame.
    pub fn composite_yuv420(&mut self, frame: i64, base: &mut VideoFrame) -> Result<()> {
        self.composite_yuv420_from(frame, base, 0)
    }

    /// Blends active cached layers whose project composition order is at least `first_order`.
    ///
    /// An opaque-video renderer uses this to skip FX layers painted below the selected topmost
    /// video while retaining FX layers painted above it.
    ///
    /// # Errors
    ///
    /// Returns an error for a malformed, unsupported, or differently sized base frame.
    pub fn composite_yuv420_from(
        &mut self,
        frame: i64,
        base: &mut VideoFrame,
        first_order: usize,
    ) -> Result<()> {
        validate_yuv_canvas(base, self.canvas)?;
        for (asset, local_frame) in self.active_assets(frame, first_order) {
            if let Some(overlay) = self.overlay_for(asset, local_frame)? {
                overlay.blend_yuv420(base);
            }
        }
        Ok(())
    }

    fn active_assets(&self, frame: i64, first_order: usize) -> Vec<(AssetKey, i64)> {
        self.layers
            .iter()
            .filter(|layer| {
                layer.composition_order >= first_order && layer.timeline.contains(&frame)
            })
            .filter_map(|layer| Some((layer.asset, layer.source_frame(frame)?)))
            .collect()
    }

    fn overlay_for(
        &mut self,
        asset_key: AssetKey,
        local_frame: i64,
    ) -> Result<Option<&PreparedOverlay>> {
        let Some(asset) = self.assets.get_mut(&asset_key) else {
            return Ok(None);
        };
        if !asset.animated {
            return Ok(asset.static_overlay.as_ref());
        }
        let frame = local_frame.clamp(0, i64::try_from(asset.frame_count - 1).unwrap_or(i64::MAX));
        if !asset.frame_overlays.contains_key(&frame) {
            let Some(prepared) = asset.prepared.as_mut() else {
                return Ok(None);
            };
            let surface = prepared
                .render_frame(SceneTime::new(
                    u64::try_from(frame).unwrap_or(0),
                    asset.frame_count,
                ))
                .map_err(|error| Error::InvalidData(error.to_string()))?;
            let source = RgbaImage::from_raw(surface.width(), surface.height(), surface.to_rgba8())
                .ok_or_else(|| {
                    Error::InvalidState("MMFX renderer returned invalid pixels".into())
                })?;
            let canvas = scale_rgba_to_canvas(&source, asset.canvas, asset.scale_mode);
            asset
                .frame_overlays
                .insert(frame, PreparedOverlay::from_canvas(&canvas));
        }
        if let Some(position) = asset.frame_order.iter().position(|cached| *cached == frame) {
            asset.frame_order.remove(position);
        }
        asset.frame_order.push_back(frame);
        while asset.frame_order.len() > FRAME_CACHE_LIMIT {
            if let Some(oldest) = asset.frame_order.pop_front() {
                asset.frame_overlays.remove(&oldest);
            }
        }
        Ok(asset.frame_overlays.get(&frame).and_then(Option::as_ref))
    }

    fn touch_scaled_asset(&mut self, key: ScaledAssetKey) {
        if let Some(position) = self
            .scaled_asset_order
            .iter()
            .position(|cached| *cached == key)
        {
            self.scaled_asset_order.remove(position);
        }
        self.scaled_asset_order.push_back(key);
        while self.scaled_asset_order.len() > SCALED_FRAME_CACHE_LIMIT {
            if let Some(oldest) = self.scaled_asset_order.pop_front() {
                self.scaled_assets.remove(&oldest);
            }
        }
    }
}

#[derive(Clone, Debug)]
struct RequestedLayer {
    asset: AssetKey,
    timeline: Range<i64>,
    composition_order: usize,
    mapping: Vec<TimeMappingStep>,
}

fn requested_layers(project: &MediaProject, context: MediaId) -> Result<Vec<RequestedLayer>> {
    crate::flatten_project_timeline(project, context)?
        .into_iter()
        .filter(|placement| {
            project
                .media(placement.media_id)
                .is_some_and(|media| media.kind.is_mmfx_scene())
        })
        .map(|placement| {
            let mapping = time_mapping(project, context, &placement.link_path)?;
            Ok(RequestedLayer {
                asset: AssetKey {
                    media_id: placement.media_id,
                    scale_mode: mode_key(placement.scale_mode),
                },
                timeline: placement.timeline_range,
                composition_order: placement.composition_order,
                mapping,
            })
        })
        .collect()
}

fn time_mapping(
    project: &MediaProject,
    context: MediaId,
    links: &[mmrecode_edit::MediaLinkId],
) -> Result<Vec<TimeMappingStep>> {
    let mut parent_id = context;
    let mut mapping = Vec::with_capacity(links.len());
    for link_id in links {
        let parent = project
            .media(parent_id)
            .ok_or_else(|| Error::InvalidState("MMFX time mapping lost its parent".into()))?;
        let link = project
            .link(*link_id)
            .ok_or_else(|| Error::InvalidState("MMFX time mapping lost a placement".into()))?;
        let child = project
            .media(link.media_id)
            .ok_or_else(|| Error::InvalidState("MMFX time mapping lost its child".into()))?;
        mapping.push(TimeMappingStep {
            parent_time_base: parent.time_base,
            child_time_base: child.time_base,
            timeline_start: link.timeline_range.start.value,
            timeline_end: link.timeline_range.end.value,
            source_start: link.source_range.start.value,
            source_end: link.source_range.end.value,
        });
        parent_id = child.id;
    }
    Ok(mapping)
}

fn source_signature(source: &MmfxSource, canvas: (u32, u32), scale_mode: u8) -> u64 {
    let mut hasher = DefaultHasher::new();
    source.source.hash(&mut hasher);
    source.resource_base.hash(&mut hasher);
    canvas.hash(&mut hasher);
    scale_mode.hash(&mut hasher);
    hasher.finish()
}

fn mode_key(mode: VisualScaleMode) -> u8 {
    match mode {
        VisualScaleMode::Fill => 1,
        VisualScaleMode::Stretch => 2,
        VisualScaleMode::Native => 3,
        _ => 0,
    }
}

fn mode_from_key(key: u8) -> VisualScaleMode {
    match key {
        1 => VisualScaleMode::Fill,
        2 => VisualScaleMode::Stretch,
        3 => VisualScaleMode::Native,
        _ => VisualScaleMode::Fit,
    }
}

struct CompiledAsset {
    prepared: PreparedScene,
    static_overlay: Option<PreparedOverlay>,
    frame_overlays: BTreeMap<i64, Option<PreparedOverlay>>,
    frame_order: VecDeque<i64>,
    animated: bool,
}

fn compile_asset<F>(
    media_id: MediaId,
    source: &MmfxSource,
    canvas: (u32, u32),
    scale_mode: VisualScaleMode,
    frame_count: u64,
    load_resources: &mut F,
) -> std::result::Result<CompiledAsset, String>
where
    F: FnMut(MediaId, &MmfxSource, &Scene) -> std::result::Result<RenderResources, String>,
{
    let scene = mmrecode_mmfx::parse_scene(&source.source).map_err(|diagnostics| {
        diagnostics
            .into_iter()
            .map(|diagnostic| {
                let (line, column) = diagnostic.span.line_column(&source.source);
                diagnostic.help.map_or_else(
                    || format!("{line}:{column}: {}", diagnostic.message),
                    |help| format!("{line}:{column}: {} — {help}", diagnostic.message),
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    })?;
    let resources = load_resources(media_id, source, &scene)?;
    let animated = scene.is_animated();
    let mut prepared =
        mmrecode_mmfx::prepare_scene(&scene, &resources).map_err(|error| error.to_string())?;
    let surface = prepared
        .render_frame(SceneTime::new(0, frame_count))
        .map_err(|error| error.to_string())?;
    let source = RgbaImage::from_raw(surface.width(), surface.height(), surface.to_rgba8())
        .ok_or_else(|| "MMFX renderer returned an invalid image buffer".to_owned())?;
    let canvas = scale_rgba_to_canvas(&source, canvas, scale_mode);
    let overlay = PreparedOverlay::from_canvas(&canvas);
    if animated {
        Ok(CompiledAsset {
            prepared,
            static_overlay: None,
            frame_overlays: BTreeMap::from([(0, overlay)]),
            frame_order: VecDeque::from([0]),
            animated,
        })
    } else {
        Ok(CompiledAsset {
            prepared,
            static_overlay: overlay,
            frame_overlays: BTreeMap::new(),
            frame_order: VecDeque::new(),
            animated,
        })
    }
}

fn scale_rgba_to_canvas(
    source: &RgbaImage,
    canvas: (u32, u32),
    mode: VisualScaleMode,
) -> RgbaImage {
    let (scaled_width, scaled_height) =
        scaled_dimensions(source.width(), source.height(), canvas.0, canvas.1, mode);
    let scaled = if (source.width(), source.height()) == (scaled_width, scaled_height) {
        source.clone()
    } else {
        image::imageops::resize(source, scaled_width, scaled_height, FilterType::Lanczos3)
    };
    let mut output = RgbaImage::new(canvas.0, canvas.1);
    let x = (i64::from(canvas.0) - i64::from(scaled_width)) / 2;
    let y = (i64::from(canvas.1) - i64::from(scaled_height)) / 2;
    image::imageops::overlay(&mut output, &scaled, x, y);
    output
}

fn scaled_dimensions(
    source_width: u32,
    source_height: u32,
    canvas_width: u32,
    canvas_height: u32,
    mode: VisualScaleMode,
) -> (u32, u32) {
    if mode == VisualScaleMode::Stretch {
        return (canvas_width, canvas_height);
    }
    if mode == VisualScaleMode::Native {
        return (source_width, source_height);
    }
    let width_limited = u128::from(canvas_width) * u128::from(source_height)
        <= u128::from(canvas_height) * u128::from(source_width);
    let use_width = if mode == VisualScaleMode::Fill {
        !width_limited
    } else {
        width_limited
    };
    if use_width {
        (
            canvas_width,
            ((u128::from(source_height) * u128::from(canvas_width) + u128::from(source_width) / 2)
                / u128::from(source_width))
            .try_into()
            .unwrap_or(u32::MAX),
        )
    } else {
        (
            ((u128::from(source_width) * u128::from(canvas_height)
                + u128::from(source_height) / 2)
                / u128::from(source_height))
            .try_into()
            .unwrap_or(u32::MAX),
            canvas_height,
        )
    }
}

impl PreparedOverlay {
    fn from_canvas(canvas: &RgbaImage) -> Option<Self> {
        let (width, height) = canvas.dimensions();
        let mut left = width;
        let mut top = height;
        let mut right = 0;
        let mut bottom = 0;
        for (x, y, pixel) in canvas.enumerate_pixels() {
            if pixel.0[3] != 0 {
                left = left.min(x);
                top = top.min(y);
                right = right.max(x + 1);
                bottom = bottom.max(y + 1);
            }
        }
        if left >= right || top >= bottom {
            return None;
        }
        left &= !1;
        top &= !1;
        right = right.saturating_add(1) & !1;
        bottom = bottom.saturating_add(1) & !1;
        right = right.min(width);
        bottom = bottom.min(height);
        let crop_width = right - left;
        let crop_height = bottom - top;
        let mut rgba = Vec::with_capacity((crop_width * crop_height * 4) as usize);
        for y in top..bottom {
            let start = ((y * width + left) * 4) as usize;
            let end = start + (crop_width * 4) as usize;
            rgba.extend_from_slice(&canvas.as_raw()[start..end]);
        }
        let pixel_count = (crop_width * crop_height) as usize;
        let chroma_count = (crop_width.div_ceil(2) * crop_height.div_ceil(2)) as usize;
        let mut prepared = Self {
            x: left,
            y: top,
            width: crop_width,
            height: crop_height,
            rgba,
            y_limited: Vec::with_capacity(pixel_count),
            y_full: Vec::with_capacity(pixel_count),
            u_limited: Vec::with_capacity(chroma_count),
            v_limited: Vec::with_capacity(chroma_count),
            u_full: Vec::with_capacity(chroma_count),
            v_full: Vec::with_capacity(chroma_count),
            chroma_alpha: Vec::with_capacity(chroma_count),
        };
        prepared.prepare_yuv();
        Some(prepared)
    }

    fn prepare_yuv(&mut self) {
        let (pixels, remainder) = self.rgba.as_chunks::<4>();
        debug_assert!(remainder.is_empty());
        for pixel in pixels {
            let (limited, full) = rgb_to_yuv(pixel[0], pixel[1], pixel[2]);
            self.y_limited.push(limited[0]);
            self.y_full.push(full[0]);
        }
        let width = self.width as usize;
        let height = self.height as usize;
        for y in (0..height).step_by(2) {
            for x in (0..width).step_by(2) {
                let mut alpha_sum = 0_u32;
                let mut limited_u = 0_u32;
                let mut limited_v = 0_u32;
                let mut full_u = 0_u32;
                let mut full_v = 0_u32;
                for dy in 0..2.min(height - y) {
                    for dx in 0..2.min(width - x) {
                        let offset = ((y + dy) * width + x + dx) * 4;
                        let pixel = &self.rgba[offset..offset + 4];
                        let alpha = u32::from(pixel[3]);
                        let (limited, full) = rgb_to_yuv(pixel[0], pixel[1], pixel[2]);
                        alpha_sum += alpha;
                        limited_u += u32::from(limited[1]) * alpha;
                        limited_v += u32::from(limited[2]) * alpha;
                        full_u += u32::from(full[1]) * alpha;
                        full_v += u32::from(full[2]) * alpha;
                    }
                }
                let sample_count = u32::try_from(2.min(height - y) * 2.min(width - x))
                    .expect("chroma sample count");
                let alpha = u8::try_from((alpha_sum + sample_count / 2) / sample_count)
                    .expect("u8 alpha values average");
                self.chroma_alpha.push(alpha);
                let weighted = |sum: u32| {
                    let value = (sum + alpha_sum / 2).checked_div(alpha_sum).unwrap_or(128);
                    u8::try_from(value).expect("weighted average of u8 channels")
                };
                self.u_limited.push(weighted(limited_u));
                self.v_limited.push(weighted(limited_v));
                self.u_full.push(weighted(full_u));
                self.v_full.push(weighted(full_v));
            }
        }
    }

    fn to_canvas(&self, canvas: (u32, u32)) -> RgbaImage {
        let mut image = RgbaImage::new(canvas.0, canvas.1);
        let canvas_width = canvas.0 as usize;
        let x = self.x as usize;
        let y = self.y as usize;
        let width = self.width as usize;
        let height = self.height as usize;
        let destination = image.as_mut();
        for row in 0..height {
            let source_start = row * width * 4;
            let destination_start = ((y + row) * canvas_width + x) * 4;
            destination[destination_start..destination_start + width * 4]
                .copy_from_slice(&self.rgba[source_start..source_start + width * 4]);
        }
        image
    }

    fn blend_rgba(&self, base: &mut RgbaImage) {
        let base_width = base.width() as usize;
        let x = self.x as usize;
        let y = self.y as usize;
        let width = self.width as usize;
        let height = self.height as usize;
        let destination_pixels = base.as_mut();
        for row in 0..height {
            let source_start = row * width * 4;
            let destination_start = ((y + row) * base_width + x) * 4;
            for column in 0..width {
                let source = source_start + column * 4;
                let destination = destination_start + column * 4;
                blend_rgba_pixel(
                    &mut destination_pixels[destination..destination + 4],
                    &self.rgba[source..source + 4],
                );
            }
        }
    }

    fn blend_yuv420(&self, base: &mut VideoFrame) {
        let limited = base.color.range == ColorRange::Limited;
        let (y_plane, chroma) = base.planes.split_at_mut(1);
        let (u_plane, v_plane) = chroma.split_at_mut(1);
        let y_plane = &mut y_plane[0];
        let u_plane = &mut u_plane[0];
        let v_plane = &mut v_plane[0];
        let x = self.x as usize;
        let y = self.y as usize;
        let width = self.width as usize;
        let height = self.height as usize;
        let source_y = if limited {
            &self.y_limited
        } else {
            &self.y_full
        };
        for row in 0..height {
            let source_start = row * width;
            let destination_start = (y + row) * y_plane.stride + x;
            for column in 0..width {
                let source = source_start + column;
                let alpha = self.rgba[source * 4 + 3];
                y_plane.data[destination_start + column] = blend_channel(
                    y_plane.data[destination_start + column],
                    source_y[source],
                    alpha,
                );
            }
        }
        let chroma_width = width / 2;
        let chroma_height = height / 2;
        let (source_u, source_v) = if limited {
            (&self.u_limited, &self.v_limited)
        } else {
            (&self.u_full, &self.v_full)
        };
        for row in 0..chroma_height {
            let source_start = row * chroma_width;
            let destination_u = (y / 2 + row) * u_plane.stride + x / 2;
            let destination_v = (y / 2 + row) * v_plane.stride + x / 2;
            for column in 0..chroma_width {
                let source = source_start + column;
                let alpha = self.chroma_alpha[source];
                u_plane.data[destination_u + column] = blend_channel(
                    u_plane.data[destination_u + column],
                    source_u[source],
                    alpha,
                );
                v_plane.data[destination_v + column] = blend_channel(
                    v_plane.data[destination_v + column],
                    source_v[source],
                    alpha,
                );
            }
        }
    }
}

fn blend_rgba_pixel(destination: &mut [u8], source: &[u8]) {
    let source_alpha = u32::from(source[3]);
    if source_alpha == 0 {
        return;
    }
    if source_alpha == 255 || destination[3] == 0 {
        destination.copy_from_slice(source);
        return;
    }
    let destination_alpha = u32::from(destination[3]);
    let inverse = 255 - source_alpha;
    let output_alpha = source_alpha + (destination_alpha * inverse + 127) / 255;
    for channel in 0..3 {
        let premultiplied = u32::from(source[channel]) * source_alpha
            + (u32::from(destination[channel]) * destination_alpha * inverse + 127) / 255;
        destination[channel] = u8::try_from((premultiplied + output_alpha / 2) / output_alpha)
            .expect("unpremultiplied u8 channel");
    }
    destination[3] = u8::try_from(output_alpha).expect("source-over u8 alpha");
}

fn blend_channel(destination: u8, source: u8, alpha: u8) -> u8 {
    if alpha == 0 {
        destination
    } else if alpha == 255 {
        source
    } else {
        let alpha = u32::from(alpha);
        u8::try_from(
            (u32::from(source) * alpha + u32::from(destination) * (255 - alpha) + 127) / 255,
        )
        .expect("blended u8 channel")
    }
}

fn rgb_to_yuv(red: u8, green: u8, blue: u8) -> ([u8; 3], [u8; 3]) {
    let red = i32::from(red);
    let green = i32::from(green);
    let blue = i32::from(blue);
    let limited_y = 16 + ((47 * red + 157 * green + 16 * blue + 128) >> 8);
    let limited_u = 128 + ((-26 * red - 87 * green + 112 * blue + 128) >> 8);
    let limited_v = 128 + ((112 * red - 102 * green - 10 * blue + 128) >> 8);
    let full_y = (54 * red + 183 * green + 18 * blue + 128) >> 8;
    let full_u = 128 + ((-29 * red - 99 * green + 128 * blue + 128) >> 8);
    let full_v = 128 + ((128 * red - 116 * green - 12 * blue + 128) >> 8);
    (
        [
            u8::try_from(limited_y.clamp(16, 235)).expect("clamped limited luma"),
            u8::try_from(limited_u.clamp(16, 240)).expect("clamped limited chroma"),
            u8::try_from(limited_v.clamp(16, 240)).expect("clamped limited chroma"),
        ],
        [
            u8::try_from(full_y.clamp(0, 255)).expect("clamped full luma"),
            u8::try_from(full_u.clamp(0, 255)).expect("clamped full chroma"),
            u8::try_from(full_v.clamp(0, 255)).expect("clamped full chroma"),
        ],
    )
}

fn validate_yuv_canvas(frame: &VideoFrame, canvas: (u32, u32)) -> Result<()> {
    let expected = (
        usize::try_from(canvas.0)
            .map_err(|error| Error::InvalidData(format!("canvas width is invalid: {error}")))?,
        usize::try_from(canvas.1)
            .map_err(|error| Error::InvalidData(format!("canvas height is invalid: {error}")))?,
    );
    if frame.format != PixelFormat::Yuv420p8
        || (frame.width, frame.height) != expected
        || frame.planes.len() != 3
        || !frame.width.is_multiple_of(2)
        || !frame.height.is_multiple_of(2)
    {
        return Err(Error::Unsupported(
            "project composition requires a canvas-sized Yuv420p8 frame".into(),
        ));
    }
    for (index, plane) in frame.planes.iter().enumerate() {
        let divisor = if index == 0 { 1 } else { 2 };
        if plane.width != frame.width / divisor
            || plane.height != frame.height / divisor
            || plane.stride < plane.width
            || plane.data.len() < plane.stride.saturating_mul(plane.height)
        {
            return Err(Error::InvalidData(format!(
                "project composition received malformed Yuv420p8 plane {index}"
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use mmrecode_core::{ColorDescription, FieldOrder, FrameTiming, Timestamp};
    use mmrecode_edit::{MediaKind, MediaOrigin, ProjectSettings, TimeRange};

    use super::*;

    fn project_with_fx(source: &str) -> MediaProject {
        let settings = ProjectSettings {
            width: 4,
            height: 4,
            ..ProjectSettings::default()
        };
        let mut project = MediaProject::with_settings("test", settings).unwrap();
        let time_base = project.settings().time_base().unwrap();
        let media_id = project
            .create_media(
                "overlay",
                MediaKind::new("fx").unwrap(),
                time_base,
                4,
                MediaOrigin::Generated,
            )
            .unwrap();
        project
            .set_mmfx_source(
                media_id,
                MmfxSource {
                    source: source.into(),
                    resource_base: None,
                },
            )
            .unwrap();
        project
            .link_media(
                project.root_id(),
                media_id,
                "overlay",
                TimeRange::new(
                    Timestamp {
                        value: 0,
                        time_base,
                    },
                    Timestamp {
                        value: 4,
                        time_base,
                    },
                )
                .unwrap(),
                TimeRange::new(
                    Timestamp {
                        value: 1,
                        time_base,
                    },
                    Timestamp {
                        value: 5,
                        time_base,
                    },
                )
                .unwrap(),
            )
            .unwrap();
        project
    }

    fn red_scene() -> &'static str {
        "@scene overlay { width: 4px; height: 4px; background: #ff000080; }"
    }

    fn project_with_nested_fx() -> (MediaProject, MediaId) {
        let settings = ProjectSettings {
            width: 4,
            height: 4,
            ..ProjectSettings::default()
        };
        let mut project = MediaProject::with_settings("nested", settings).unwrap();
        let time_base = project.settings().time_base().unwrap();
        let video = project
            .create_media(
                "clip",
                MediaKind::new("video/mpeg2").unwrap(),
                time_base,
                10,
                MediaOrigin::Generated,
            )
            .unwrap();
        project
            .link_media(
                project.root_id(),
                video,
                "Clip",
                TimeRange::new(
                    Timestamp {
                        value: 0,
                        time_base,
                    },
                    Timestamp {
                        value: 10,
                        time_base,
                    },
                )
                .unwrap(),
                TimeRange::new(
                    Timestamp {
                        value: 5,
                        time_base,
                    },
                    Timestamp {
                        value: 15,
                        time_base,
                    },
                )
                .unwrap(),
            )
            .unwrap();
        let fx = project
            .create_media(
                "lower",
                MediaKind::new("fx").unwrap(),
                time_base,
                4,
                MediaOrigin::Generated,
            )
            .unwrap();
        project
            .set_mmfx_source(
                fx,
                MmfxSource {
                    source: red_scene().into(),
                    resource_base: None,
                },
            )
            .unwrap();
        project
            .link_media(
                video,
                fx,
                "Lower",
                TimeRange::new(
                    Timestamp {
                        value: 0,
                        time_base,
                    },
                    Timestamp {
                        value: 4,
                        time_base,
                    },
                )
                .unwrap(),
                TimeRange::new(
                    Timestamp {
                        value: 2,
                        time_base,
                    },
                    Timestamp {
                        value: 6,
                        time_base,
                    },
                )
                .unwrap(),
            )
            .unwrap();
        (project, video)
    }

    #[test]
    fn synchronizes_once_and_reuses_prepared_pixels() {
        let project = project_with_fx(red_scene());
        let mut compositor = ProjectCompositor::new();
        let first = compositor.synchronize(&project, project.root_id(), |_, _, _| {
            Ok(RenderResources::new())
        });
        assert!(first.changed);
        assert_eq!(first.compiled_assets, 1);
        assert!(first.diagnostics.is_empty());
        let second = compositor.synchronize(&project, project.root_id(), |_, _, _| {
            panic!("unchanged asset must not reload resources")
        });
        assert!(!second.changed);
        assert_eq!(second.reused_assets, 1);
    }

    #[test]
    fn composites_only_inside_the_placement_range() {
        let project = project_with_fx(red_scene());
        let mut compositor = ProjectCompositor::new();
        compositor.synchronize(&project, project.root_id(), |_, _, _| {
            Ok(RenderResources::new())
        });
        let mut before = RgbaImage::from_pixel(4, 4, image::Rgba([0, 0, 0, 255]));
        compositor.composite_rgba8(0, &mut before).unwrap();
        assert_eq!(before.get_pixel(0, 0).0, [0, 0, 0, 255]);
        let mut active = RgbaImage::from_pixel(4, 4, image::Rgba([0, 0, 0, 255]));
        compositor.composite_rgba8(1, &mut active).unwrap();
        assert!(active.get_pixel(0, 0).0[0] >= 127);
        assert_eq!(active.get_pixel(0, 0).0[3], 255);
    }

    #[test]
    fn evaluates_animated_assets_in_placement_local_time() {
        let source = "@scene overlay { width: 4px; height: 4px; \
            @rect fill { background: #f00; animation: appear 4f linear; } } \
            @keyframes appear { from { opacity: 0; } to { opacity: 1; } }";
        let project = project_with_fx(source);
        let mut compositor = ProjectCompositor::new();
        let sync = compositor.synchronize(&project, project.root_id(), |_, _, _| {
            Ok(RenderResources::new())
        });
        assert_eq!(sync.compiled_assets, 1);
        let mut first = RgbaImage::new(4, 4);
        compositor.composite_rgba8(1, &mut first).unwrap();
        assert_eq!(first.get_pixel(0, 0).0, [0, 0, 0, 0]);
        let mut middle = RgbaImage::new(4, 4);
        compositor.composite_rgba8(2, &mut middle).unwrap();
        assert!((84..=86).contains(&middle.get_pixel(0, 0).0[3]));
        let mut last = RgbaImage::new(4, 4);
        compositor.composite_rgba8(4, &mut last).unwrap();
        assert_eq!(last.get_pixel(0, 0).0, [255, 0, 0, 255]);
        let asset = compositor.assets.values().next().unwrap();
        assert!(asset.animated);
        assert_eq!(asset.frame_overlays.len(), 3);
    }

    #[test]
    fn animated_frame_cache_is_bounded_and_reuses_recent_scrubs() {
        let source = "@scene overlay { width: 4px; height: 4px; \
            @rect fill { background: #f00; animation: appear scene linear; } } \
            @keyframes appear { from { opacity: 0; } to { opacity: 1; } }";
        let project = project_with_fx(source);
        let mut compositor = ProjectCompositor::new();
        compositor.synchronize(&project, project.root_id(), |_, _, _| {
            Ok(RenderResources::new())
        });
        let key = *compositor.assets.keys().next().unwrap();
        compositor.assets.get_mut(&key).unwrap().frame_count = 64;
        for frame in 0..32 {
            compositor.overlay_for(key, frame).unwrap();
        }
        let asset = compositor.assets.get(&key).unwrap();
        assert_eq!(asset.frame_overlays.len(), FRAME_CACHE_LIMIT);
        assert!(!asset.frame_overlays.contains_key(&0));
        assert!(asset.frame_overlays.contains_key(&31));

        compositor.overlay_for(key, 0).unwrap();
        let asset = compositor.assets.get(&key).unwrap();
        assert_eq!(asset.frame_overlays.len(), FRAME_CACHE_LIMIT);
        assert!(asset.frame_overlays.contains_key(&0));
        assert_eq!(asset.frame_order.back(), Some(&0));
    }

    #[test]
    fn recursively_composites_fx_in_root_and_entered_media_time() {
        let (project, video) = project_with_nested_fx();
        let mut compositor = ProjectCompositor::new();
        let root_sync = compositor.synchronize(&project, project.root_id(), |_, _, _| {
            Ok(RenderResources::new())
        });
        assert_eq!(root_sync.compiled_assets, 1);
        assert!(!compositor.has_active_layers(6));
        assert!(compositor.has_active_layers(7));
        assert!(compositor.has_active_layers(10));
        assert!(!compositor.has_active_layers(11));

        let entered_sync = compositor.synchronize(&project, video, |_, _, _| {
            panic!("entering a parent must reuse its compiled descendant FX")
        });
        assert_eq!(entered_sync.reused_assets, 1);
        assert!(!compositor.has_active_layers(1));
        assert!(compositor.has_active_layers(2));
        assert!(compositor.has_active_layers(5));
        assert!(!compositor.has_active_layers(6));
    }

    #[test]
    fn composites_preconverted_pixels_directly_into_yuv420() {
        let project = project_with_fx(red_scene());
        let mut compositor = ProjectCompositor::new();
        compositor.synchronize(&project, project.root_id(), |_, _, _| {
            Ok(RenderResources::new())
        });
        let mut frame = VideoFrame {
            format: PixelFormat::Yuv420p8,
            width: 4,
            height: 4,
            planes: vec![
                mmrecode_core::Plane {
                    data: vec![16; 16],
                    stride: 4,
                    width: 4,
                    height: 4,
                },
                mmrecode_core::Plane {
                    data: vec![128; 4],
                    stride: 2,
                    width: 2,
                    height: 2,
                },
                mmrecode_core::Plane {
                    data: vec![128; 4],
                    stride: 2,
                    width: 2,
                    height: 2,
                },
            ],
            timing: FrameTiming::default(),
            color: ColorDescription {
                range: ColorRange::Limited,
                ..ColorDescription::default()
            },
            field_order: FieldOrder::Progressive,
        };
        compositor.composite_yuv420(1, &mut frame).unwrap();
        assert!(frame.planes[0].data[0] > 16);
        assert!(frame.planes[2].data[0] > 128);

        let mut covered = frame.clone();
        covered.planes[0].data.fill(16);
        covered.planes[1].data.fill(128);
        covered.planes[2].data.fill(128);
        compositor
            .composite_yuv420_from(1, &mut covered, 1)
            .unwrap();
        assert_eq!(covered.planes[0].data[0], 16);
    }

    #[test]
    fn retains_last_good_pixels_after_an_invalid_edit() {
        let mut project = project_with_fx(red_scene());
        let media_id = project
            .media_nodes()
            .find(|media| media.kind.as_str() == "fx")
            .unwrap()
            .id;
        let mut compositor = ProjectCompositor::new();
        compositor.synchronize(&project, project.root_id(), |_, _, _| {
            Ok(RenderResources::new())
        });
        project
            .set_mmfx_source(
                media_id,
                MmfxSource {
                    source: "not mmfx".into(),
                    resource_base: None,
                },
            )
            .unwrap();
        let sync = compositor.synchronize(&project, project.root_id(), |_, _, _| {
            Ok(RenderResources::new())
        });
        assert_eq!(sync.diagnostics.len(), 1);
        assert!(compositor.has_active_layers(1));
    }
}
