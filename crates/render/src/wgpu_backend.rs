//! Optional wgpu execution of RGBA project composition graphs.

use std::sync::mpsc::{self, Receiver, Sender};

use image::RgbaImage;
use mmrecode_core::{Error, Result};

use crate::{
    CompositeOperator, CompositionBackend, CompositionGraph, CompositionPass, DeviceResourceCache,
    DeviceResourceCacheStats, FrameFormat, FrameHandle, FrameResourceKey, FrameResourceProvider,
    FrameResourceView,
};

const SHADER: &str = r"
struct VertexOutput {
    @builtin(position) position: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@group(0) @binding(0) var source_texture: texture_2d<f32>;
@group(0) @binding(1) var source_sampler: sampler;

@vertex
fn vertex_main(@builtin(vertex_index) index: u32) -> VertexOutput {
    var positions = array<vec2<f32>, 3>(
        vec2<f32>(-1.0, -1.0),
        vec2<f32>(3.0, -1.0),
        vec2<f32>(-1.0, 3.0),
    );
    var coordinates = array<vec2<f32>, 3>(
        vec2<f32>(0.0, 1.0),
        vec2<f32>(2.0, 1.0),
        vec2<f32>(0.0, -1.0),
    );
    var output: VertexOutput;
    output.position = vec4<f32>(positions[index], 0.0, 1.0);
    output.uv = coordinates[index];
    return output;
}

@fragment
fn fragment_main(input: VertexOutput) -> @location(0) vec4<f32> {
    return textureSample(source_texture, source_sampler, input.uv);
}
";

/// Existing wgpu target into which an RGBA composition graph is drawn.
///
/// The texture view must use `format`, cover `width × height`, and support
/// [`wgpu::TextureUsages::RENDER_ATTACHMENT`]. The first backend slice expects an opaque base;
/// transparent project targets remain on the scalar reference backend until premultiplied target
/// semantics are added to the outer graph.
#[derive(Clone, Copy, Debug)]
pub struct WgpuRgbaTarget<'a> {
    /// Render-attachment view.
    pub view: &'a wgpu::TextureView,
    /// Visible width.
    pub width: u32,
    /// Visible height.
    pub height: u32,
    /// View format selected when the backend was created.
    pub format: wgpu::TextureFormat,
}

/// One asynchronously completed wgpu preview readback.
#[derive(Debug)]
pub struct WgpuPreviewFrame {
    /// Monotonic identifier returned when this frame was submitted.
    pub submission: u64,
    /// Semantic identity of the graph target that produced the pixels.
    pub target: FrameResourceKey,
    /// Completed straight-alpha sRGBA8 monitor pixels.
    pub image: RgbaImage,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PreviewSlotState {
    Available,
    InFlight {
        submission: u64,
        target: FrameResourceKey,
    },
}

#[derive(Debug)]
struct PreviewSlot {
    texture: wgpu::Texture,
    view: wgpu::TextureView,
    readback: wgpu::Buffer,
    state: PreviewSlotState,
}

/// Asynchronous wgpu-to-CPU bridge for terminal and other CPU-pixel monitors.
///
/// The renderer owns a ring of output textures and padded readback buffers. [`Self::submit`] never
/// waits for the GPU: it returns `Ok(None)` when every slot is busy so an interactive host can drop
/// obsolete frames and retry only its newest playhead. [`Self::poll_latest`] returns the newest
/// completed submission and discards older completions from the same poll.
#[derive(Debug)]
pub struct WgpuPreviewRenderer {
    device: wgpu::Device,
    queue: wgpu::Queue,
    backend: WgpuCompositionBackend,
    width: u32,
    height: u32,
    padded_bytes_per_row: u32,
    slots: Vec<PreviewSlot>,
    completion_tx: Sender<(usize, std::result::Result<(), String>)>,
    completion_rx: Receiver<(usize, std::result::Result<(), String>)>,
    next_submission: u64,
}

#[derive(Debug)]
struct CachedTexture {
    _texture: wgpu::Texture,
    bind_group: wgpu::BindGroup,
}

#[derive(Debug)]
struct Draw {
    bind_group: wgpu::BindGroup,
    x: u32,
    y: u32,
    width: u32,
    height: u32,
    viewport: [f32; 4],
}

/// Wgpu executor for positioned RGBA8 source-over passes.
///
/// The backend accepts an existing device and queue so a native viewer can share the same wgpu
/// instance and surface. Source textures are uploaded once and retained by semantic
/// [`crate::FrameResourceKey`]. One command buffer is submitted per graph, with one render pass for
/// all active overlays.
#[derive(Debug)]
pub struct WgpuCompositionBackend {
    device: wgpu::Device,
    queue: wgpu::Queue,
    format: wgpu::TextureFormat,
    source_format: wgpu::TextureFormat,
    bind_group_layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    pipeline: wgpu::RenderPipeline,
    textures: DeviceResourceCache<CachedTexture>,
}

impl WgpuCompositionBackend {
    /// Create a backend sharing an existing wgpu device and queue.
    ///
    /// # Errors
    ///
    /// Returns an error unless `format` is an RGBA8/BGRA8 unorm or sRGB format, or cache
    /// configuration is invalid. Unorm targets preserve the current byte-domain CPU preview
    /// blend; sRGB targets use the graphics API's linear-light blend behavior.
    pub fn new(
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        format: wgpu::TextureFormat,
        texture_budget_bytes: usize,
    ) -> Result<Self> {
        if !matches!(
            format,
            wgpu::TextureFormat::Rgba8Unorm
                | wgpu::TextureFormat::Rgba8UnormSrgb
                | wgpu::TextureFormat::Bgra8Unorm
                | wgpu::TextureFormat::Bgra8UnormSrgb
        ) {
            return Err(Error::Unsupported(format!(
                "wgpu composition requires an RGBA8 or BGRA8 target, received {format:?}"
            )));
        }
        let source_format = if format.is_srgb() {
            wgpu::TextureFormat::Rgba8UnormSrgb
        } else {
            wgpu::TextureFormat::Rgba8Unorm
        };
        let bind_group_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("mmrecode composition source layout"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("mmrecode composition sampler"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("mmrecode composition shader"),
            source: wgpu::ShaderSource::Wgsl(SHADER.into()),
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("mmrecode composition pipeline layout"),
            bind_group_layouts: &[Some(&bind_group_layout)],
            immediate_size: 0,
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("mmrecode source-over pipeline"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vertex_main"),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                buffers: &[],
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fragment_main"),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: Some(wgpu::BlendState {
                        color: wgpu::BlendComponent {
                            src_factor: wgpu::BlendFactor::SrcAlpha,
                            dst_factor: wgpu::BlendFactor::OneMinusSrcAlpha,
                            operation: wgpu::BlendOperation::Add,
                        },
                        alpha: wgpu::BlendComponent {
                            src_factor: wgpu::BlendFactor::One,
                            dst_factor: wgpu::BlendFactor::OneMinusSrcAlpha,
                            operation: wgpu::BlendOperation::Add,
                        },
                    }),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview_mask: None,
            cache: None,
        });
        Ok(Self {
            device: device.clone(),
            queue: queue.clone(),
            format,
            source_format,
            bind_group_layout,
            sampler,
            pipeline,
            textures: DeviceResourceCache::new("wgpu", texture_budget_bytes)?,
        })
    }

    /// Backend texture-cache state.
    #[must_use]
    pub fn cache_stats(&self) -> DeviceResourceCacheStats {
        self.textures.stats()
    }

    /// Release uploaded textures idle for more than the given number of graph generations.
    pub fn release_idle(&mut self, maximum_idle_generations: u64) -> usize {
        self.textures.release_idle(maximum_idle_generations)
    }

    fn prepare_draw(
        &mut self,
        source_handle: &FrameHandle,
        resources: &dyn FrameResourceProvider,
        x: u32,
        y: u32,
        target: &WgpuRgbaTarget<'_>,
    ) -> Result<Draw> {
        let Some(FrameResourceView::Rgba8(source)) = resources.resource(source_handle) else {
            return Err(Error::InvalidState(format!(
                "wgpu composition source {:?} is not resident as RGBA8",
                source_handle.key
            )));
        };
        if source_handle.descriptor.format != FrameFormat::Rgba8
            || (source.width, source.height)
                != (
                    source_handle.descriptor.width,
                    source_handle.descriptor.height,
                )
            || source.stride < source.width as usize * 4
            || source.pixels.len() < source.stride.saturating_mul(source.height as usize)
            || x.saturating_add(source.width) > target.width
            || y.saturating_add(source.height) > target.height
        {
            return Err(Error::InvalidData(
                "wgpu RGBA source storage, descriptor, or placement is invalid".into(),
            ));
        }
        let estimated_bytes = (source.width as usize)
            .checked_mul(source.height as usize)
            .and_then(|pixels| pixels.checked_mul(4))
            .ok_or_else(|| Error::InvalidData("wgpu source texture size overflows".into()))?;
        let device = self.device.clone();
        let queue = self.queue.clone();
        let layout = self.bind_group_layout.clone();
        let sampler = self.sampler.clone();
        let source_format = self.source_format;
        let (texture, _) =
            self.textures
                .retain_with(source_handle, estimated_bytes, move || {
                    create_texture(&device, &queue, &layout, &sampler, source_format, source)
                })?;
        Ok(Draw {
            bind_group: texture.bind_group.clone(),
            x,
            y,
            width: source.width,
            height: source.height,
            viewport: [
                viewport_component(x)?,
                viewport_component(y)?,
                viewport_component(source.width)?,
                viewport_component(source.height)?,
            ],
        })
    }
}

impl WgpuPreviewRenderer {
    /// Create a fixed-size asynchronous preview ring on an existing wgpu device.
    ///
    /// Three slots normally allow one image to be displayed, one to be mapped, and one to be
    /// rendered without making the playback thread wait.
    ///
    /// # Errors
    ///
    /// Returns an error for zero dimensions, a zero slot count, row-size overflow, or invalid
    /// texture-cache configuration.
    pub fn new(
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        width: u32,
        height: u32,
        slot_count: usize,
        texture_budget_bytes: usize,
    ) -> Result<Self> {
        if width == 0 || height == 0 || slot_count == 0 {
            return Err(Error::InvalidData(
                "wgpu preview dimensions and slot count must be positive".into(),
            ));
        }
        let unpadded_bytes_per_row = width
            .checked_mul(4)
            .ok_or_else(|| Error::InvalidData("wgpu preview row size overflows".into()))?;
        let padded_bytes_per_row =
            align_to(unpadded_bytes_per_row, wgpu::COPY_BYTES_PER_ROW_ALIGNMENT)?;
        let readback_size = u64::from(padded_bytes_per_row)
            .checked_mul(u64::from(height))
            .ok_or_else(|| Error::InvalidData("wgpu preview buffer size overflows".into()))?;
        let format = wgpu::TextureFormat::Rgba8Unorm;
        let slots = (0..slot_count)
            .map(|_| {
                let texture = device.create_texture(&wgpu::TextureDescriptor {
                    label: Some("mmrecode preview ring target"),
                    size: wgpu::Extent3d {
                        width,
                        height,
                        depth_or_array_layers: 1,
                    },
                    mip_level_count: 1,
                    sample_count: 1,
                    dimension: wgpu::TextureDimension::D2,
                    format,
                    usage: wgpu::TextureUsages::RENDER_ATTACHMENT
                        | wgpu::TextureUsages::COPY_DST
                        | wgpu::TextureUsages::COPY_SRC,
                    view_formats: &[],
                });
                let view = texture.create_view(&wgpu::TextureViewDescriptor {
                    label: Some("mmrecode preview ring target view"),
                    ..Default::default()
                });
                let readback = device.create_buffer(&wgpu::BufferDescriptor {
                    label: Some("mmrecode preview ring readback"),
                    size: readback_size,
                    usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
                    mapped_at_creation: false,
                });
                PreviewSlot {
                    texture,
                    view,
                    readback,
                    state: PreviewSlotState::Available,
                }
            })
            .collect();
        let (completion_tx, completion_rx) = mpsc::channel();
        Ok(Self {
            device: device.clone(),
            queue: queue.clone(),
            backend: WgpuCompositionBackend::new(device, queue, format, texture_budget_bytes)?,
            width,
            height,
            padded_bytes_per_row,
            slots,
            completion_tx,
            completion_rx,
            next_submission: 1,
        })
    }

    /// Fixed output dimensions.
    #[must_use]
    pub const fn dimensions(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    /// Whether a submission can enter the ring without waiting.
    #[must_use]
    pub fn has_capacity(&self) -> bool {
        self.slots
            .iter()
            .any(|slot| slot.state == PreviewSlotState::Available)
    }

    /// Texture-cache state shared by every ring slot.
    #[must_use]
    pub fn cache_stats(&self) -> DeviceResourceCacheStats {
        self.backend.cache_stats()
    }

    /// Queue one base image plus its project composition without waiting for GPU completion.
    ///
    /// `Ok(Some(id))` means the work entered the ring. `Ok(None)` means all slots are busy; the
    /// caller should drop this request and retry its newest desired frame after polling.
    ///
    /// # Errors
    ///
    /// Returns an error when the base image or graph target does not match this renderer, when the
    /// graph contains unsupported operations, or when wgpu resource preparation fails.
    pub fn submit(
        &mut self,
        graph: &CompositionGraph,
        resources: &dyn FrameResourceProvider,
        base: &RgbaImage,
    ) -> Result<Option<u64>> {
        if base.dimensions() != self.dimensions()
            || graph.target().descriptor.width != self.width
            || graph.target().descriptor.height != self.height
        {
            return Err(Error::InvalidData(
                "wgpu preview base image and graph dimensions must match the ring".into(),
            ));
        }
        let Some(slot_index) = self
            .slots
            .iter()
            .position(|slot| slot.state == PreviewSlotState::Available)
        else {
            return Ok(None);
        };
        let submission = self.next_submission;
        self.next_submission = self.next_submission.saturating_add(1);
        let slot = &mut self.slots[slot_index];
        self.queue.write_texture(
            slot.texture.as_image_copy(),
            base.as_raw(),
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(self.width * 4),
                rows_per_image: Some(self.height),
            },
            slot.texture.size(),
        );
        let mut target = WgpuRgbaTarget {
            view: &slot.view,
            width: self.width,
            height: self.height,
            format: wgpu::TextureFormat::Rgba8Unorm,
        };
        self.backend.execute(graph, &mut target, resources)?;
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("mmrecode preview readback encoder"),
            });
        encoder.copy_texture_to_buffer(
            slot.texture.as_image_copy(),
            wgpu::TexelCopyBufferInfo {
                buffer: &slot.readback,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(self.padded_bytes_per_row),
                    rows_per_image: Some(self.height),
                },
            },
            slot.texture.size(),
        );
        self.queue.submit([encoder.finish()]);
        slot.state = PreviewSlotState::InFlight {
            submission,
            target: graph.target().key,
        };
        let completion = self.completion_tx.clone();
        slot.readback
            .slice(..)
            .map_async(wgpu::MapMode::Read, move |result| {
                let result = result.map_err(|error| error.to_string());
                let _ = completion.send((slot_index, result));
            });
        Ok(Some(submission))
    }

    /// Poll GPU progress and return only the newest completed frame.
    ///
    /// This call never waits. Older completed submissions are unmapped and released back to the
    /// ring but omitted from the result so a delayed preview cannot move the visible monitor
    /// backwards.
    ///
    /// # Errors
    ///
    /// Returns an error if device polling or an asynchronous buffer mapping fails.
    pub fn poll_latest(&mut self) -> Result<Option<WgpuPreviewFrame>> {
        self.device
            .poll(wgpu::PollType::Poll)
            .map_err(|error| Error::InvalidState(format!("cannot poll wgpu preview: {error}")))?;
        self.collect_completed()
    }

    fn collect_completed(&mut self) -> Result<Option<WgpuPreviewFrame>> {
        let mut latest = None;
        while let Ok((slot_index, result)) = self.completion_rx.try_recv() {
            let Some(slot) = self.slots.get_mut(slot_index) else {
                return Err(Error::InvalidState(
                    "wgpu preview completed an unknown ring slot".into(),
                ));
            };
            let PreviewSlotState::InFlight { submission, target } = slot.state else {
                return Err(Error::InvalidState(
                    "wgpu preview completed a slot that was not in flight".into(),
                ));
            };
            slot.state = PreviewSlotState::Available;
            result.map_err(|error| {
                Error::InvalidState(format!("cannot map wgpu preview output: {error}"))
            })?;
            let mapped = slot.readback.slice(..).get_mapped_range();
            let pixels =
                unpad_rgba_rows(&mapped, self.width, self.height, self.padded_bytes_per_row);
            drop(mapped);
            slot.readback.unmap();
            let pixels = pixels?;
            let image = RgbaImage::from_raw(self.width, self.height, pixels).ok_or_else(|| {
                Error::InvalidState("wgpu preview produced an invalid RGBA image".into())
            })?;
            if latest
                .as_ref()
                .is_none_or(|frame: &WgpuPreviewFrame| submission > frame.submission)
            {
                latest = Some(WgpuPreviewFrame {
                    submission,
                    target,
                    image,
                });
            }
        }
        Ok(latest)
    }
}

impl CompositionBackend<WgpuRgbaTarget<'_>> for WgpuCompositionBackend {
    fn name(&self) -> &'static str {
        "wgpu-rgba8"
    }

    fn execute(
        &mut self,
        graph: &CompositionGraph,
        target: &mut WgpuRgbaTarget<'_>,
        resources: &dyn FrameResourceProvider,
    ) -> Result<()> {
        if target.format != self.format
            || target.width != graph.target().descriptor.width
            || target.height != graph.target().descriptor.height
            || graph.target().descriptor.format != FrameFormat::Rgba8
        {
            return Err(Error::InvalidData(
                "wgpu target does not match the composition graph".into(),
            ));
        }
        self.textures.begin_frame();
        let mut draws = Vec::new();
        for pass in graph.passes() {
            match pass {
                CompositionPass::Composite {
                    source,
                    target: pass_target,
                    x,
                    y,
                    operator: CompositeOperator::SourceOver,
                } if pass_target.key == graph.target().key => {
                    draws.push(self.prepare_draw(source, resources, *x, *y, target)?);
                }
                CompositionPass::Deliver { source, .. } if source.key == graph.target().key => {}
                CompositionPass::Scale { .. } | CompositionPass::ColorConvert { .. } => {
                    return Err(Error::Unsupported(
                        "first wgpu backend slice supports RGBA composition passes only".into(),
                    ));
                }
                CompositionPass::Composite { .. } | CompositionPass::Deliver { .. } => {
                    return Err(Error::InvalidState(
                        "wgpu composition pass targets a different output frame".into(),
                    ));
                }
            }
        }
        if draws.is_empty() {
            return Ok(());
        }
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("mmrecode composition encoder"),
            });
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("mmrecode RGBA source-over composition"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: target.view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Load,
                        store: wgpu::StoreOp::Store,
                    },
                    depth_slice: None,
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            pass.set_pipeline(&self.pipeline);
            for draw in &draws {
                pass.set_viewport(
                    draw.viewport[0],
                    draw.viewport[1],
                    draw.viewport[2],
                    draw.viewport[3],
                    0.0,
                    1.0,
                );
                pass.set_scissor_rect(draw.x, draw.y, draw.width, draw.height);
                pass.set_bind_group(0, &draw.bind_group, &[]);
                pass.draw(0..3, 0..1);
            }
        }
        self.queue.submit([encoder.finish()]);
        Ok(())
    }
}

fn align_to(value: u32, alignment: u32) -> Result<u32> {
    let remainder = value % alignment;
    if remainder == 0 {
        return Ok(value);
    }
    value
        .checked_add(alignment - remainder)
        .ok_or_else(|| Error::InvalidData("wgpu row alignment overflows".into()))
}

fn unpad_rgba_rows(
    mapped: &[u8],
    width: u32,
    height: u32,
    padded_bytes_per_row: u32,
) -> Result<Vec<u8>> {
    let row_bytes = width
        .checked_mul(4)
        .ok_or_else(|| Error::InvalidData("wgpu RGBA row size overflows".into()))?;
    let output_len = usize::try_from(row_bytes)
        .ok()
        .and_then(|row| row.checked_mul(height as usize))
        .ok_or_else(|| Error::InvalidData("wgpu RGBA output size overflows".into()))?;
    let padded = usize::try_from(padded_bytes_per_row)
        .map_err(|error| Error::InvalidData(format!("wgpu row pitch is invalid: {error}")))?;
    let row = usize::try_from(row_bytes)
        .map_err(|error| Error::InvalidData(format!("wgpu row size is invalid: {error}")))?;
    if mapped.len() < padded.saturating_mul(height as usize) {
        return Err(Error::InvalidState(
            "mapped wgpu preview is shorter than its declared rows".into(),
        ));
    }
    let mut output = Vec::with_capacity(output_len);
    for source in mapped.chunks_exact(padded).take(height as usize) {
        output.extend_from_slice(&source[..row]);
    }
    Ok(output)
}

#[allow(clippy::cast_precision_loss)]
fn viewport_component(value: u32) -> Result<f32> {
    if value > 1 << f32::MANTISSA_DIGITS {
        return Err(Error::Unsupported(
            "wgpu viewport component exceeds exact f32 integer precision".into(),
        ));
    }
    Ok(value as f32)
}

fn create_texture(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    layout: &wgpu::BindGroupLayout,
    sampler: &wgpu::Sampler,
    format: wgpu::TextureFormat,
    source: crate::Rgba8ResourceView<'_>,
) -> Result<CachedTexture> {
    let texture = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("mmrecode cached RGBA source"),
        size: wgpu::Extent3d {
            width: source.width,
            height: source.height,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    });
    queue.write_texture(
        wgpu::TexelCopyTextureInfo {
            texture: &texture,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        source.pixels,
        wgpu::TexelCopyBufferLayout {
            offset: 0,
            bytes_per_row: Some(u32::try_from(source.stride).map_err(|error| {
                Error::InvalidData(format!("wgpu source stride is invalid: {error}"))
            })?),
            rows_per_image: Some(source.height),
        },
        wgpu::Extent3d {
            width: source.width,
            height: source.height,
            depth_or_array_layers: 1,
        },
    );
    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
    let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("mmrecode cached RGBA source bind group"),
        layout,
        entries: &[
            wgpu::BindGroupEntry {
                binding: 0,
                resource: wgpu::BindingResource::TextureView(&view),
            },
            wgpu::BindGroupEntry {
                binding: 1,
                resource: wgpu::BindingResource::Sampler(sampler),
            },
        ],
    });
    Ok(CachedTexture {
        _texture: texture,
        bind_group,
    })
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;

    use crate::{
        FrameDescriptor, FrameResidency, FrameResourceKey, FrameResourceNamespace,
        Rgba8ResourceView,
    };

    use super::*;

    struct Source {
        handle: FrameHandle,
        pixels: [u8; 16],
    }

    impl FrameResourceProvider for Source {
        fn resource(&self, handle: &FrameHandle) -> Option<FrameResourceView<'_>> {
            (handle.key == self.handle.key).then_some(FrameResourceView::Rgba8(Rgba8ResourceView {
                width: 2,
                height: 2,
                stride: 8,
                pixels: &self.pixels,
            }))
        }
    }

    #[test]
    fn composites_and_reuses_a_texture_when_an_adapter_is_available() {
        pollster::block_on(async {
            let instance = wgpu::Instance::default();
            let Ok(adapter) = instance
                .request_adapter(&wgpu::RequestAdapterOptions {
                    power_preference: wgpu::PowerPreference::LowPower,
                    force_fallback_adapter: false,
                    compatible_surface: None,
                })
                .await
            else {
                return;
            };
            let Ok((device, queue)) = adapter
                .request_device(&wgpu::DeviceDescriptor::default())
                .await
            else {
                return;
            };
            let source_handle = handle(FrameResourceNamespace::MmfxCanvas, 7, 2, 2);
            let target_handle = handle(FrameResourceNamespace::Transient, 0, 4, 2);
            let graph = CompositionGraph::new(
                target_handle.clone(),
                vec![
                    CompositionPass::Composite {
                        source: source_handle.clone(),
                        target: target_handle.clone(),
                        x: 1,
                        y: 0,
                        operator: CompositeOperator::SourceOver,
                    },
                    CompositionPass::Deliver {
                        source: target_handle,
                        delivery: crate::FrameDelivery::Preview,
                    },
                ],
            );
            let source = Source {
                handle: source_handle,
                pixels: [255, 0, 0, 128].repeat(4).try_into().unwrap(),
            };
            let texture = device.create_texture(&wgpu::TextureDescriptor {
                label: Some("mmrecode wgpu test target"),
                size: wgpu::Extent3d {
                    width: 4,
                    height: 2,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::Rgba8UnormSrgb,
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT
                    | wgpu::TextureUsages::COPY_DST
                    | wgpu::TextureUsages::COPY_SRC,
                view_formats: &[],
            });
            queue.write_texture(
                texture.as_image_copy(),
                &[0, 0, 0, 255].repeat(8),
                wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(16),
                    rows_per_image: Some(2),
                },
                texture.size(),
            );
            let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
            let mut target = WgpuRgbaTarget {
                view: &view,
                width: 4,
                height: 2,
                format: wgpu::TextureFormat::Rgba8UnormSrgb,
            };
            let mut backend = WgpuCompositionBackend::new(
                &device,
                &queue,
                wgpu::TextureFormat::Rgba8UnormSrgb,
                1 << 20,
            )
            .unwrap();
            backend.execute(&graph, &mut target, &source).unwrap();
            backend.execute(&graph, &mut target, &source).unwrap();
            assert_eq!(backend.cache_stats().insertions, 1);
            assert_eq!(backend.cache_stats().reuses, 1);

            let padded_bytes_per_row = wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
            let buffer = device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("mmrecode wgpu test readback"),
                size: u64::from(padded_bytes_per_row) * 2,
                usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
                mapped_at_creation: false,
            });
            let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("mmrecode wgpu test readback encoder"),
            });
            encoder.copy_texture_to_buffer(
                texture.as_image_copy(),
                wgpu::TexelCopyBufferInfo {
                    buffer: &buffer,
                    layout: wgpu::TexelCopyBufferLayout {
                        offset: 0,
                        bytes_per_row: Some(padded_bytes_per_row),
                        rows_per_image: Some(2),
                    },
                },
                texture.size(),
            );
            queue.submit([encoder.finish()]);
            let slice = buffer.slice(..);
            let (sender, receiver) = mpsc::channel();
            slice.map_async(wgpu::MapMode::Read, move |result| {
                let _ = sender.send(result);
            });
            device.poll(wgpu::PollType::wait_indefinitely()).unwrap();
            receiver.recv().unwrap().unwrap();
            let mapped = slice.get_mapped_range();
            assert_eq!(&mapped[..4], &[0, 0, 0, 255]);
            assert!(mapped[4] > 128);
            assert_eq!(&mapped[5..8], &[0, 0, 255]);
            drop(mapped);
            buffer.unmap();

            let mut preview = WgpuPreviewRenderer::new(&device, &queue, 4, 2, 2, 1 << 20).unwrap();
            let base = RgbaImage::from_pixel(4, 2, image::Rgba([0, 0, 0, 255]));
            assert_eq!(preview.submit(&graph, &source, &base).unwrap(), Some(1));
            assert_eq!(preview.submit(&graph, &source, &base).unwrap(), Some(2));
            device.poll(wgpu::PollType::wait_indefinitely()).unwrap();
            let completed = preview.poll_latest().unwrap().unwrap();
            assert_eq!(completed.submission, 2);
            assert_eq!(completed.target, graph.target().key);
            assert_eq!(completed.image.get_pixel(0, 0).0, [0, 0, 0, 255]);
            assert_eq!(completed.image.get_pixel(1, 0).0, [128, 0, 0, 255]);
            assert_eq!(preview.cache_stats().insertions, 1);
            assert_eq!(preview.cache_stats().reuses, 1);
        });
    }

    #[test]
    fn removes_wgpu_row_padding() {
        let mut mapped = vec![0_u8; 512];
        mapped[..8].copy_from_slice(&[1, 2, 3, 4, 5, 6, 7, 8]);
        mapped[256..264].copy_from_slice(&[9, 10, 11, 12, 13, 14, 15, 16]);
        assert_eq!(
            unpad_rgba_rows(&mapped, 2, 2, 256).unwrap(),
            (1_u8..=16).collect::<Vec<_>>()
        );
    }

    fn handle(
        namespace: FrameResourceNamespace,
        owner: u64,
        width: u32,
        height: u32,
    ) -> FrameHandle {
        FrameHandle {
            key: FrameResourceKey {
                namespace,
                owner,
                revision: 1,
                local_frame: -1,
                width,
                height,
                variant: 0,
            },
            descriptor: FrameDescriptor::rgba8(width, height),
            residency: FrameResidency::Cpu,
        }
    }
}
