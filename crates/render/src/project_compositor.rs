//! Cached CPU composition of generated MMFX objects in a project timeline.
//!
//! Parsing, font/image loading, and scene preparation happen only when a source or canvas changes.
//! Static scenes are rasterized once; animated frames are evaluated lazily at exact placement-local
//! time and retained in a bounded prepared-overlay cache. Repeated frames only look up active
//! layers and blend their prepared pixels into the caller's frame.

use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    fmt,
    hash::{DefaultHasher, Hash as _, Hasher},
    ops::Range,
};

use image::{RgbaImage, imageops::FilterType};
#[cfg(test)]
use mmrecode_core::ColorRange;
use mmrecode_core::{
    Error, PixelFormat, Rational, Result, Timestamp, TimestampRounding, VideoFrame,
};
use mmrecode_edit::{MediaId, MediaProject, MmfxSource, VisualScaleMode};
use mmrecode_mmfx::{PreparedScene, RenderResources, Scene, SceneTime};

use crate::{
    CompositeOperator, CompositionBackend, CompositionGraph, CompositionPass,
    CpuCompositionBackend, FrameDelivery, FrameDescriptor, FrameFormat, FrameHandle,
    FrameResidency, FrameResourceKey, FrameResourceNamespace, FrameResourceProvider,
    FrameResourceView, Rgba8ResourceView, Yuv420AlphaResourceView,
};

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
        let graph = self.composition_graph_rgba8(
            frame,
            0,
            base.width(),
            base.height(),
            FrameDelivery::Preview,
        )?;
        self.execute_rgba8_graph(&graph, base)
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
        let graph = self.composition_graph_rgba8(
            frame,
            0,
            base.width(),
            base.height(),
            FrameDelivery::Preview,
        )?;
        self.execute_rgba8_graph(&graph, base)
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
        let width = u32::try_from(base.width)
            .map_err(|error| Error::InvalidData(format!("frame width is invalid: {error}")))?;
        let height = u32::try_from(base.height)
            .map_err(|error| Error::InvalidData(format!("frame height is invalid: {error}")))?;
        let graph = self.composition_graph_yuv420(
            frame,
            first_order,
            width,
            height,
            base.color.clone(),
            FrameDelivery::Encoder,
        )?;
        self.execute_yuv420_graph(&graph, base)
    }

    /// Build the exact MMFX composition schedule for an sRGBA8 project or preview target.
    ///
    /// The returned handles contain stable semantic keys and explicit residency. The optional wgpu
    /// backend retains uploaded textures by key and executes the same pass order as the CPU path.
    ///
    /// # Errors
    ///
    /// Returns an error if an active animated scene frame cannot be rendered or scaled.
    pub fn composition_graph_rgba8(
        &mut self,
        frame: i64,
        first_order: usize,
        width: u32,
        height: u32,
        delivery: FrameDelivery,
    ) -> Result<CompositionGraph> {
        let descriptor = FrameDescriptor::rgba8(width, height);
        self.build_composition_graph(frame, first_order, &descriptor, delivery)
    }

    /// Build the exact MMFX conversion/composition schedule for a YUV 4:2:0 target.
    ///
    /// # Errors
    ///
    /// Returns an error if an active animated scene frame cannot be rendered.
    pub fn composition_graph_yuv420(
        &mut self,
        frame: i64,
        first_order: usize,
        width: u32,
        height: u32,
        color: mmrecode_core::ColorDescription,
        delivery: FrameDelivery,
    ) -> Result<CompositionGraph> {
        let descriptor = FrameDescriptor::yuv420p8(width, height, color);
        self.build_composition_graph(frame, first_order, &descriptor, delivery)
    }

    #[allow(clippy::too_many_lines)]
    fn build_composition_graph(
        &mut self,
        frame: i64,
        first_order: usize,
        target_descriptor: &FrameDescriptor,
        delivery: FrameDelivery,
    ) -> Result<CompositionGraph> {
        if target_descriptor.width == 0 || target_descriptor.height == 0 {
            return Err(Error::InvalidData(
                "composition graph target dimensions must be positive".into(),
            ));
        }
        let target = FrameHandle {
            key: FrameResourceKey {
                namespace: FrameResourceNamespace::DecodedVideo,
                owner: 0,
                revision: self.revision,
                local_frame: frame,
                width: target_descriptor.width,
                height: target_descriptor.height,
                variant: format_variant(target_descriptor.format),
            },
            descriptor: target_descriptor.clone(),
            residency: FrameResidency::Cpu,
        };
        let preview = (target_descriptor.width, target_descriptor.height) != self.canvas;
        let canvas = self.canvas;
        let mut passes = Vec::new();
        for (asset_key, local_frame, source_revision) in self.active_assets(frame, first_order) {
            let frame_key = self
                .assets
                .get(&asset_key)
                .map_or(-1, |asset| if asset.animated { local_frame } else { -1 });
            let namespace = if preview {
                let scaled_key = (
                    asset_key,
                    frame_key,
                    target_descriptor.width,
                    target_descriptor.height,
                );
                if !self.scaled_assets.contains_key(&scaled_key) {
                    let Some(source_canvas) = self
                        .overlay_for(asset_key, local_frame)?
                        .map(|source| source.to_canvas(canvas))
                    else {
                        continue;
                    };
                    let scaled = image::imageops::resize(
                        &source_canvas,
                        target_descriptor.width,
                        target_descriptor.height,
                        FilterType::Triangle,
                    );
                    let Some(prepared) = PreparedOverlay::from_canvas(&scaled) else {
                        continue;
                    };
                    self.scaled_assets.insert(scaled_key, prepared);
                }
                self.touch_scaled_asset(scaled_key);
                FrameResourceNamespace::MmfxPreview
            } else {
                if self.overlay_for(asset_key, local_frame)?.is_none() {
                    continue;
                }
                FrameResourceNamespace::MmfxCanvas
            };
            let key = FrameResourceKey {
                namespace,
                owner: asset_key.media_id.0,
                revision: source_revision,
                local_frame: frame_key,
                width: target_descriptor.width,
                height: target_descriptor.height,
                variant: u32::from(asset_key.scale_mode),
            };
            let Some((x, y, width, height)) = self.overlay_geometry(key) else {
                continue;
            };
            let source = FrameHandle {
                key,
                descriptor: FrameDescriptor::rgba8(width, height),
                residency: FrameResidency::Cpu,
            };
            let source = if target_descriptor.format == FrameFormat::Yuv420p8 {
                let converted = FrameHandle {
                    key: FrameResourceKey {
                        namespace: FrameResourceNamespace::ColorConversion,
                        ..key
                    },
                    descriptor: FrameDescriptor {
                        format: FrameFormat::Yuv420p8Alpha,
                        color: target_descriptor.color.clone(),
                        ..source.descriptor.clone()
                    },
                    residency: FrameResidency::Cpu,
                };
                passes.push(CompositionPass::ColorConvert {
                    source,
                    target: converted.clone(),
                });
                converted
            } else {
                source
            };
            passes.push(CompositionPass::Composite {
                source,
                target: target.clone(),
                x,
                y,
                operator: CompositeOperator::SourceOver,
            });
        }
        passes.push(CompositionPass::Deliver {
            source: target.clone(),
            delivery,
        });
        Ok(CompositionGraph::new(target, passes))
    }

    fn execute_rgba8_graph(&self, graph: &CompositionGraph, base: &mut RgbaImage) -> Result<()> {
        CpuCompositionBackend.execute(graph, base, self)
    }

    fn execute_yuv420_graph(&self, graph: &CompositionGraph, base: &mut VideoFrame) -> Result<()> {
        CpuCompositionBackend.execute(graph, base, self)
    }

    fn overlay_geometry(&self, key: FrameResourceKey) -> Option<(u32, u32, u32, u32)> {
        self.overlay_by_resource(key)
            .map(|overlay| (overlay.x, overlay.y, overlay.width, overlay.height))
    }

    fn overlay_by_resource(&self, mut key: FrameResourceKey) -> Option<&PreparedOverlay> {
        if key.namespace == FrameResourceNamespace::ColorConversion {
            key.namespace = if (key.width, key.height) == self.canvas {
                FrameResourceNamespace::MmfxCanvas
            } else {
                FrameResourceNamespace::MmfxPreview
            };
        }
        let asset_key = AssetKey {
            media_id: MediaId(key.owner),
            scale_mode: u8::try_from(key.variant).ok()?,
        };
        if self.assets.get(&asset_key)?.good_signature != Some(key.revision) {
            return None;
        }
        match key.namespace {
            FrameResourceNamespace::MmfxPreview => {
                self.scaled_assets
                    .get(&(asset_key, key.local_frame, key.width, key.height))
            }
            FrameResourceNamespace::MmfxCanvas => {
                let asset = self.assets.get(&asset_key)?;
                if asset.animated {
                    asset
                        .frame_overlays
                        .get(&key.local_frame)
                        .and_then(Option::as_ref)
                } else {
                    asset.static_overlay.as_ref()
                }
            }
            _ => None,
        }
    }

    fn active_assets(&self, frame: i64, first_order: usize) -> Vec<(AssetKey, i64, u64)> {
        self.layers
            .iter()
            .filter(|layer| {
                layer.composition_order >= first_order && layer.timeline.contains(&frame)
            })
            .filter_map(|layer| {
                Some((
                    layer.asset,
                    layer.source_frame(frame)?,
                    layer.source_signature,
                ))
            })
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

impl FrameResourceProvider for ProjectCompositor {
    fn resource(&self, handle: &FrameHandle) -> Option<FrameResourceView<'_>> {
        let overlay = self.overlay_by_resource(handle.key)?;
        match handle.descriptor.format {
            FrameFormat::Rgba8 => Some(FrameResourceView::Rgba8(Rgba8ResourceView {
                width: overlay.width,
                height: overlay.height,
                stride: overlay.width as usize * 4,
                pixels: &overlay.rgba,
            })),
            FrameFormat::Yuv420p8Alpha => {
                Some(FrameResourceView::Yuv420p8Alpha(Yuv420AlphaResourceView {
                    width: overlay.width,
                    height: overlay.height,
                    rgba: &overlay.rgba,
                    y_limited: &overlay.y_limited,
                    y_full: &overlay.y_full,
                    u_limited: &overlay.u_limited,
                    v_limited: &overlay.v_limited,
                    u_full: &overlay.u_full,
                    v_full: &overlay.v_full,
                    chroma_alpha: &overlay.chroma_alpha,
                }))
            }
            FrameFormat::Yuv420p8 => None,
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
    let mut hasher = StableResourceHasher::new();
    stable_hash_bytes(&mut hasher, source.source.as_bytes());
    if let Some(path) = &source.resource_base {
        hasher.write(&[1]);
        stable_hash_bytes(&mut hasher, path.to_string_lossy().as_bytes());
    } else {
        hasher.write(&[0]);
    }
    hasher.write(
        &u64::try_from(source.parameter_bindings.len())
            .unwrap_or(u64::MAX)
            .to_le_bytes(),
    );
    for (name, value) in &source.parameter_bindings {
        stable_hash_bytes(&mut hasher, name.as_bytes());
        stable_hash_bytes(&mut hasher, value.as_bytes());
    }
    hasher.write(&canvas.0.to_le_bytes());
    hasher.write(&canvas.1.to_le_bytes());
    hasher.write(&[scale_mode]);
    hasher.finish()
}

fn stable_hash_bytes(hasher: &mut StableResourceHasher, bytes: &[u8]) {
    hasher.write(&u64::try_from(bytes.len()).unwrap_or(u64::MAX).to_le_bytes());
    hasher.write(bytes);
}

struct StableResourceHasher(u64);

impl StableResourceHasher {
    const fn new() -> Self {
        Self(0xcbf2_9ce4_8422_2325)
    }
}

impl Hasher for StableResourceHasher {
    fn finish(&self) -> u64 {
        self.0
    }

    fn write(&mut self, bytes: &[u8]) {
        for byte in bytes {
            self.0 ^= u64::from(*byte);
            self.0 = self.0.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }
}

fn mode_key(mode: VisualScaleMode) -> u8 {
    match mode {
        VisualScaleMode::Fill => 1,
        VisualScaleMode::Stretch => 2,
        VisualScaleMode::Native => 3,
        _ => 0,
    }
}

const fn format_variant(format: FrameFormat) -> u32 {
    match format {
        FrameFormat::Rgba8 => 1,
        FrameFormat::Yuv420p8 => 2,
        FrameFormat::Yuv420p8Alpha => 3,
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
    let scene =
        mmrecode_mmfx::parse_scene_with_bindings(&source.source, &source.parameter_bindings)
            .map_err(|diagnostics| {
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
                    linked_path: None,
                    parameter_bindings: BTreeMap::new(),
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
                    linked_path: None,
                    parameter_bindings: BTreeMap::new(),
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
    fn composition_graph_exposes_stable_preview_and_delivery_resources() {
        let project = project_with_fx(red_scene());
        let mut compositor = ProjectCompositor::new();
        compositor.synchronize(&project, project.root_id(), |_, _, _| {
            Ok(RenderResources::new())
        });

        let first = compositor
            .composition_graph_rgba8(1, 0, 4, 4, FrameDelivery::Preview)
            .unwrap();
        let second = compositor
            .composition_graph_rgba8(1, 0, 4, 4, FrameDelivery::Preview)
            .unwrap();
        assert_eq!(first, second);
        assert_eq!(first.passes().len(), 2);
        let CompositionPass::Composite { source, target, .. } = &first.passes()[0] else {
            panic!("first pass should composite the active MMFX layer");
        };
        assert_eq!(source.key.namespace, FrameResourceNamespace::MmfxCanvas);
        assert_eq!(source.residency, FrameResidency::Cpu);
        assert_eq!(target, first.target());
        let FrameResourceView::Rgba8(view) = compositor.resource(source).unwrap() else {
            panic!("canvas resource should expose RGBA pixels");
        };
        assert_eq!((view.width, view.height, view.stride), (4, 4, 16));
        assert_eq!(view.pixels.len(), 64);
        assert!(matches!(
            first.passes()[1],
            CompositionPass::Deliver {
                delivery: FrameDelivery::Preview,
                ..
            }
        ));

        let yuv = compositor
            .composition_graph_yuv420(
                1,
                0,
                4,
                4,
                ColorDescription {
                    range: ColorRange::Limited,
                    ..ColorDescription::default()
                },
                FrameDelivery::Encoder,
            )
            .unwrap();
        assert!(matches!(
            yuv.passes()[0],
            CompositionPass::ColorConvert { .. }
        ));
        let CompositionPass::ColorConvert {
            target: converted, ..
        } = &yuv.passes()[0]
        else {
            unreachable!();
        };
        let FrameResourceView::Yuv420p8Alpha(view) = compositor.resource(converted).unwrap() else {
            panic!("converted resource should expose YUV and alpha planes");
        };
        assert_eq!((view.width, view.height), (4, 4));
        assert_eq!(view.y_limited.len(), 16);
        assert_eq!(view.chroma_alpha.len(), 4);
        assert!(matches!(yuv.passes()[1], CompositionPass::Composite { .. }));
        assert!(matches!(
            yuv.passes()[2],
            CompositionPass::Deliver {
                delivery: FrameDelivery::Encoder,
                ..
            }
        ));
    }

    #[test]
    fn parameter_binding_change_recompiles_only_the_affected_asset() {
        let source = "@param --accent { type: color; default: #ff0000; } \
            @scene overlay { width: 4px; height: 4px; background: var(--accent); }";
        let mut project = project_with_fx(source);
        let media_id = project
            .media_nodes()
            .find(|media| media.kind.is_mmfx_scene())
            .unwrap()
            .id;
        let mut compositor = ProjectCompositor::new();
        let first = compositor.synchronize(&project, project.root_id(), |_, _, _| {
            Ok(RenderResources::new())
        });
        assert_eq!(first.compiled_assets, 1);
        let mut red = RgbaImage::new(4, 4);
        compositor.composite_rgba8(1, &mut red).unwrap();
        assert_eq!(red.get_pixel(0, 0).0, [255, 0, 0, 255]);

        let mut payload = project.media(media_id).unwrap().mmfx.clone().unwrap();
        payload
            .parameter_bindings
            .insert("accent".into(), "#00ff00".into());
        project.set_mmfx_source(media_id, payload).unwrap();

        let changed = compositor.synchronize(&project, project.root_id(), |_, _, _| {
            Ok(RenderResources::new())
        });
        assert!(changed.changed);
        assert_eq!(changed.compiled_assets, 1);
        let mut green = RgbaImage::new(4, 4);
        compositor.composite_rgba8(1, &mut green).unwrap();
        assert_eq!(green.get_pixel(0, 0).0, [0, 255, 0, 255]);
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
                    linked_path: None,
                    parameter_bindings: BTreeMap::new(),
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
