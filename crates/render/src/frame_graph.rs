//! Backend-neutral frame handles and project composition scheduling.

use image::RgbaImage;
use mmrecode_core::{ColorDescription, ColorRange, Error, PixelFormat, Result, VideoFrame};
use mmrecode_edit::VisualScaleMode;

/// Stable semantic namespace for a cached frame resource.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
#[non_exhaustive]
pub enum FrameResourceNamespace {
    /// A decoded or caller-supplied project frame.
    DecodedVideo,
    /// A rendered MMFX layer at project-canvas resolution.
    MmfxCanvas,
    /// A rendered MMFX layer scaled for interactive preview.
    MmfxPreview,
    /// A backend-cached color conversion of another resource.
    ColorConversion,
    /// A transient render target.
    Transient,
}

/// Stable identity used to retain CPU buffers or GPU textures across frames.
///
/// The fields describe semantic identity rather than an address or backend object. A device backend
/// maps this key to its own texture allocation and may safely discard/recreate that allocation.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct FrameResourceKey {
    /// Resource category.
    pub namespace: FrameResourceNamespace,
    /// Stable project media identifier, or zero for caller-owned targets.
    pub owner: u64,
    /// Deterministic content revision.
    pub revision: u64,
    /// Source-local frame, or `-1` for a time-invariant resource.
    pub local_frame: i64,
    /// Width of this resource variant.
    pub width: u32,
    /// Height of this resource variant.
    pub height: u32,
    /// Backend-independent variant discriminator such as a placement scale mode.
    pub variant: u32,
}

/// Pixel organization required by a frame handle.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum FrameFormat {
    /// Tightly packed straight-alpha sRGBA8.
    Rgba8,
    /// Opaque planar YUV 4:2:0 with eight-bit samples.
    Yuv420p8,
    /// Planar YUV 4:2:0 plus retained alpha used while compositing an overlay.
    Yuv420p8Alpha,
}

/// Alpha interpretation attached to a frame resource.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum FrameAlphaMode {
    /// Every pixel is opaque.
    Opaque,
    /// Color channels are stored independently from straight alpha.
    Straight,
}

/// Complete semantic description of a project frame resource.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FrameDescriptor {
    /// Visible width.
    pub width: u32,
    /// Visible height.
    pub height: u32,
    /// Pixel organization.
    pub format: FrameFormat,
    /// Color interpretation. RGBA resources use explicit sRGB names.
    pub color: ColorDescription,
    /// Alpha interpretation.
    pub alpha_mode: FrameAlphaMode,
}

impl FrameDescriptor {
    /// Descriptor for tightly packed straight-alpha sRGBA8.
    #[must_use]
    pub fn rgba8(width: u32, height: u32) -> Self {
        Self {
            width,
            height,
            format: FrameFormat::Rgba8,
            color: ColorDescription {
                range: ColorRange::Full,
                primaries: Some("bt709".into()),
                transfer: Some("srgb".into()),
                matrix: Some("rgb".into()),
            },
            alpha_mode: FrameAlphaMode::Straight,
        }
    }

    /// Descriptor for an opaque project-sized YUV 4:2:0 frame.
    #[must_use]
    pub fn yuv420p8(width: u32, height: u32, color: ColorDescription) -> Self {
        Self {
            width,
            height,
            format: FrameFormat::Yuv420p8,
            color,
            alpha_mode: FrameAlphaMode::Opaque,
        }
    }
}

/// Current physical location of a frame resource.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum FrameResidency {
    /// Host-accessible memory.
    Cpu,
    /// Opaque resource owned by a named execution backend.
    Device {
        /// Backend identity, for example `wgpu-vulkan`.
        backend: String,
    },
}

/// Backend-independent reference to decoded, generated, intermediate, or output pixels.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FrameHandle {
    /// Stable semantic resource identity.
    pub key: FrameResourceKey,
    /// Pixel and color requirements.
    pub descriptor: FrameDescriptor,
    /// Current physical location.
    pub residency: FrameResidency,
}

/// Borrowed straight-alpha sRGBA8 pixels supplied to a composition backend.
#[derive(Clone, Copy, Debug)]
pub struct Rgba8ResourceView<'a> {
    /// Visible width.
    pub width: u32,
    /// Visible height.
    pub height: u32,
    /// Bytes between rows.
    pub stride: usize,
    /// Pixel bytes.
    pub pixels: &'a [u8],
}

/// Borrowed decoded planar YUV 4:2:0 frame supplied to a composition backend.
#[derive(Clone, Copy, Debug)]
pub struct Yuv420ResourceView<'a> {
    /// Frame storage, timing, field order, and color interpretation.
    pub frame: &'a VideoFrame,
}

/// Borrowed, preconverted YUV 4:2:0 overlay with retained alpha.
///
/// Both component ranges are retained because the destination range is a property of the project
/// target rather than the authored MMFX resource.
#[derive(Clone, Copy, Debug)]
pub struct Yuv420AlphaResourceView<'a> {
    /// Visible width. Current direct composition requires an even value.
    pub width: u32,
    /// Visible height. Current direct composition requires an even value.
    pub height: u32,
    /// Straight alpha interleaved in source RGBA pixels.
    pub rgba: &'a [u8],
    /// Limited-range luma.
    pub y_limited: &'a [u8],
    /// Full-range luma.
    pub y_full: &'a [u8],
    /// Limited-range subsampled chroma.
    pub u_limited: &'a [u8],
    /// Limited-range subsampled chroma.
    pub v_limited: &'a [u8],
    /// Full-range subsampled chroma.
    pub u_full: &'a [u8],
    /// Full-range subsampled chroma.
    pub v_full: &'a [u8],
    /// Alpha averaged over each 2×2 chroma sample.
    pub chroma_alpha: &'a [u8],
}

/// Host-accessible data associated with one semantic frame handle.
#[derive(Clone, Copy, Debug)]
#[non_exhaustive]
pub enum FrameResourceView<'a> {
    /// Straight-alpha sRGBA8 pixels.
    Rgba8(Rgba8ResourceView<'a>),
    /// Decoded opaque planar YUV 4:2:0 pixels.
    Yuv420p8(Yuv420ResourceView<'a>),
    /// Preconverted YUV 4:2:0 overlay and retained alpha.
    Yuv420p8Alpha(Yuv420AlphaResourceView<'a>),
}

/// Resolves semantic frame handles without exposing compositor cache internals.
pub trait FrameResourceProvider {
    /// Borrow host-accessible data for `handle`, if available.
    ///
    /// Device-resident targets and transient resources do not need to have a CPU view. Backends may
    /// retain an uploaded resource by [`FrameResourceKey`] after this borrow ends.
    fn resource(&self, handle: &FrameHandle) -> Option<FrameResourceView<'_>>;
}

/// Final consumer selected for a composition graph.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum FrameDelivery {
    /// Interactive terminal or native monitor preview.
    Preview,
    /// Encoder input for final delivery.
    Encoder,
}

/// Sampling filter selected for a scale pass.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum FrameScaleFilter {
    /// High-quality project conformance filter used by final rendering.
    Lanczos3,
    /// Lower-cost bilinear triangle filter used by reduced-size previews.
    Triangle,
}

/// Porter-Duff operation selected for a composite pass.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum CompositeOperator {
    /// Paint the foreground over the existing target.
    SourceOver,
}

/// One explicit project-frame operation.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum CompositionPass {
    /// Scale or conform an input into a target resource.
    Scale {
        /// Source frame.
        source: FrameHandle,
        /// Scaled result.
        target: FrameHandle,
        /// Aspect/crop placement policy.
        mode: VisualScaleMode,
        /// Sampling filter.
        filter: FrameScaleFilter,
    },
    /// Convert pixel organization or color interpretation.
    ColorConvert {
        /// Source frame.
        source: FrameHandle,
        /// Converted result.
        target: FrameHandle,
    },
    /// Composite one positioned foreground over an existing target.
    Composite {
        /// Foreground frame or overlay.
        source: FrameHandle,
        /// In-place composition target.
        target: FrameHandle,
        /// Horizontal target offset.
        x: u32,
        /// Vertical target offset.
        y: u32,
        /// Blend operation.
        operator: CompositeOperator,
    },
    /// Hand the completed resource to its consumer.
    Deliver {
        /// Completed frame.
        source: FrameHandle,
        /// Selected consumer.
        delivery: FrameDelivery,
    },
}

/// Inspectable schedule for one project frame.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CompositionGraph {
    target: FrameHandle,
    passes: Vec<CompositionPass>,
}

/// Executes a project composition graph against a concrete target representation.
pub trait CompositionBackend<Target: ?Sized> {
    /// Stable diagnostic name.
    fn name(&self) -> &'static str;

    /// Resolve resources and execute graph passes in order.
    ///
    /// # Errors
    ///
    /// Returns an error when a resource is unavailable or a pass cannot operate on `Target`.
    fn execute(
        &mut self,
        graph: &CompositionGraph,
        target: &mut Target,
        resources: &dyn FrameResourceProvider,
    ) -> Result<()>;
}

/// CPU implementation for terminal RGBA and direct encoder YUV composition.
#[derive(Clone, Copy, Debug, Default)]
pub struct CpuCompositionBackend;

impl CompositionBackend<RgbaImage> for CpuCompositionBackend {
    fn name(&self) -> &'static str {
        "cpu-rgba8"
    }

    fn execute(
        &mut self,
        graph: &CompositionGraph,
        target: &mut RgbaImage,
        resources: &dyn FrameResourceProvider,
    ) -> Result<()> {
        validate_rgba_target(graph, target)?;
        for pass in graph.passes() {
            match pass {
                CompositionPass::Composite {
                    source,
                    target: pass_target,
                    x,
                    y,
                    operator: CompositeOperator::SourceOver,
                } if pass_target.key == graph.target.key => {
                    let FrameResourceView::Rgba8(source) = resolve(resources, source)? else {
                        return Err(Error::InvalidState(
                            "RGBA composition source has incompatible storage".into(),
                        ));
                    };
                    blend_rgba8(target, source, *x, *y)?;
                }
                CompositionPass::Deliver { source, .. } if source.key == graph.target.key => {}
                CompositionPass::ColorConvert { .. } => {
                    return Err(Error::InvalidState(
                        "RGBA composition unexpectedly requested color conversion".into(),
                    ));
                }
                CompositionPass::Scale { .. } => {
                    return Err(Error::Unsupported(
                        "CPU composition graph scaling is not connected yet".into(),
                    ));
                }
                CompositionPass::Composite { .. } | CompositionPass::Deliver { .. } => {
                    return Err(Error::InvalidState(
                        "composition pass targets a different output frame".into(),
                    ));
                }
            }
        }
        Ok(())
    }
}

impl CompositionBackend<VideoFrame> for CpuCompositionBackend {
    fn name(&self) -> &'static str {
        "cpu-yuv420p8"
    }

    fn execute(
        &mut self,
        graph: &CompositionGraph,
        target: &mut VideoFrame,
        resources: &dyn FrameResourceProvider,
    ) -> Result<()> {
        let begins_with_scale = matches!(
            graph.passes().first(),
            Some(CompositionPass::Scale {
                target: pass_target,
                ..
            }) if pass_target.key == graph.target.key
        );
        if !begins_with_scale {
            validate_yuv_target(graph, target)?;
        }
        for pass in graph.passes() {
            match pass {
                CompositionPass::ColorConvert { target, .. } => {
                    if !matches!(
                        resolve(resources, target)?,
                        FrameResourceView::Yuv420p8Alpha(_)
                    ) {
                        return Err(Error::InvalidState(
                            "YUV conversion cache has incompatible storage".into(),
                        ));
                    }
                }
                CompositionPass::Composite {
                    source,
                    target: pass_target,
                    x,
                    y,
                    operator: CompositeOperator::SourceOver,
                } if pass_target.key == graph.target.key => {
                    let FrameResourceView::Yuv420p8Alpha(source) = resolve(resources, source)?
                    else {
                        return Err(Error::InvalidState(
                            "YUV composition source has incompatible storage".into(),
                        ));
                    };
                    blend_yuv420(target, source, *x, *y)?;
                }
                CompositionPass::Deliver { source, .. } if source.key == graph.target.key => {}
                CompositionPass::Scale {
                    source,
                    target: pass_target,
                    mode,
                    filter,
                } if pass_target.key == graph.target.key => {
                    let FrameResourceView::Yuv420p8(source) = resolve(resources, source)? else {
                        return Err(Error::InvalidState(
                            "YUV scale source has incompatible storage".into(),
                        ));
                    };
                    *target = crate::compositor::scale_yuv420_to_canvas_cpu(
                        source.frame,
                        usize::try_from(pass_target.descriptor.width).map_err(|error| {
                            Error::InvalidData(format!("scale target width is invalid: {error}"))
                        })?,
                        usize::try_from(pass_target.descriptor.height).map_err(|error| {
                            Error::InvalidData(format!("scale target height is invalid: {error}"))
                        })?,
                        *mode,
                        *filter,
                    )?;
                    validate_yuv_target(graph, target)?;
                }
                CompositionPass::Scale { .. }
                | CompositionPass::Composite { .. }
                | CompositionPass::Deliver { .. } => {
                    return Err(Error::InvalidState(
                        "composition pass targets a different output frame".into(),
                    ));
                }
            }
        }
        Ok(())
    }
}

fn resolve<'a>(
    resources: &'a dyn FrameResourceProvider,
    handle: &FrameHandle,
) -> Result<FrameResourceView<'a>> {
    resources.resource(handle).ok_or_else(|| {
        Error::InvalidState(format!(
            "composition resource {:?} is not resident",
            handle.key
        ))
    })
}

fn validate_rgba_target(graph: &CompositionGraph, target: &RgbaImage) -> Result<()> {
    let descriptor = &graph.target.descriptor;
    if descriptor.format != FrameFormat::Rgba8
        || descriptor.alpha_mode != FrameAlphaMode::Straight
        || (descriptor.width, descriptor.height) != target.dimensions()
    {
        return Err(Error::InvalidData(
            "RGBA composition target does not match its frame handle".into(),
        ));
    }
    Ok(())
}

fn validate_yuv_target(graph: &CompositionGraph, target: &VideoFrame) -> Result<()> {
    let descriptor = &graph.target.descriptor;
    let dimensions = (
        usize::try_from(descriptor.width)
            .map_err(|error| Error::InvalidData(format!("frame width is invalid: {error}")))?,
        usize::try_from(descriptor.height)
            .map_err(|error| Error::InvalidData(format!("frame height is invalid: {error}")))?,
    );
    if descriptor.format != FrameFormat::Yuv420p8
        || descriptor.alpha_mode != FrameAlphaMode::Opaque
        || !target.width.is_multiple_of(2)
        || !target.height.is_multiple_of(2)
        || target.format != PixelFormat::Yuv420p8
        || (target.width, target.height) != dimensions
        || target.color != descriptor.color
        || target.planes.len() != 3
    {
        return Err(Error::InvalidData(
            "YUV composition target does not match its frame handle".into(),
        ));
    }
    for (index, plane) in target.planes.iter().enumerate() {
        let divisor = if index == 0 { 1 } else { 2 };
        if plane.width != target.width / divisor
            || plane.height != target.height / divisor
            || plane.stride < plane.width
            || plane.data.len() < plane.stride.saturating_mul(plane.height)
        {
            return Err(Error::InvalidData(format!(
                "YUV composition target has malformed plane {index}"
            )));
        }
    }
    Ok(())
}

fn blend_rgba8(
    target: &mut RgbaImage,
    source: Rgba8ResourceView<'_>,
    x: u32,
    y: u32,
) -> Result<()> {
    let width = usize::try_from(source.width)
        .map_err(|error| Error::InvalidData(format!("overlay width is invalid: {error}")))?;
    let height = usize::try_from(source.height)
        .map_err(|error| Error::InvalidData(format!("overlay height is invalid: {error}")))?;
    let x = usize::try_from(x)
        .map_err(|error| Error::InvalidData(format!("overlay x is invalid: {error}")))?;
    let y = usize::try_from(y)
        .map_err(|error| Error::InvalidData(format!("overlay y is invalid: {error}")))?;
    let target_width = usize::try_from(target.width())
        .map_err(|error| Error::InvalidData(format!("target width is invalid: {error}")))?;
    let target_height = usize::try_from(target.height())
        .map_err(|error| Error::InvalidData(format!("target height is invalid: {error}")))?;
    if source.stride < width.saturating_mul(4)
        || source.pixels.len() < source.stride.saturating_mul(height)
        || x.saturating_add(width) > target_width
        || y.saturating_add(height) > target_height
    {
        return Err(Error::InvalidData(
            "RGBA overlay storage or placement is invalid".into(),
        ));
    }
    let destination = target.as_mut();
    for row in 0..height {
        let source_row = row * source.stride;
        let target_row = ((y + row) * target_width + x) * 4;
        for column in 0..width {
            let source_offset = source_row + column * 4;
            let target_offset = target_row + column * 4;
            blend_rgba_pixel(
                &mut destination[target_offset..target_offset + 4],
                &source.pixels[source_offset..source_offset + 4],
            );
        }
    }
    Ok(())
}

#[allow(clippy::too_many_lines)]
fn blend_yuv420(
    target: &mut VideoFrame,
    source: Yuv420AlphaResourceView<'_>,
    x: u32,
    y: u32,
) -> Result<()> {
    let width = usize::try_from(source.width)
        .map_err(|error| Error::InvalidData(format!("overlay width is invalid: {error}")))?;
    let height = usize::try_from(source.height)
        .map_err(|error| Error::InvalidData(format!("overlay height is invalid: {error}")))?;
    let x = usize::try_from(x)
        .map_err(|error| Error::InvalidData(format!("overlay x is invalid: {error}")))?;
    let y = usize::try_from(y)
        .map_err(|error| Error::InvalidData(format!("overlay y is invalid: {error}")))?;
    let pixels = width.checked_mul(height).ok_or_else(|| {
        Error::InvalidData("YUV overlay dimensions overflow the host address space".into())
    })?;
    let chroma = width.checked_div(2).unwrap_or(0).saturating_mul(height / 2);
    if !width.is_multiple_of(2)
        || !height.is_multiple_of(2)
        || !x.is_multiple_of(2)
        || !y.is_multiple_of(2)
        || x.saturating_add(width) > target.width
        || y.saturating_add(height) > target.height
        || source.rgba.len() < pixels.saturating_mul(4)
        || source.y_limited.len() < pixels
        || source.y_full.len() < pixels
        || source.u_limited.len() < chroma
        || source.v_limited.len() < chroma
        || source.u_full.len() < chroma
        || source.v_full.len() < chroma
        || source.chroma_alpha.len() < chroma
    {
        return Err(Error::InvalidData(
            "YUV overlay storage or placement is invalid".into(),
        ));
    }
    let limited = target.color.range == ColorRange::Limited;
    let (y_plane, chroma_planes) = target.planes.split_at_mut(1);
    let (u_plane, v_plane) = chroma_planes.split_at_mut(1);
    let y_plane = &mut y_plane[0];
    let u_plane = &mut u_plane[0];
    let v_plane = &mut v_plane[0];
    let source_y = if limited {
        source.y_limited
    } else {
        source.y_full
    };
    for row in 0..height {
        let source_start = row * width;
        let destination_start = (y + row) * y_plane.stride + x;
        for column in 0..width {
            let source_offset = source_start + column;
            let alpha = source.rgba[source_offset * 4 + 3];
            y_plane.data[destination_start + column] = blend_channel(
                y_plane.data[destination_start + column],
                source_y[source_offset],
                alpha,
            );
        }
    }
    let chroma_width = width / 2;
    let chroma_height = height / 2;
    let (source_u, source_v) = if limited {
        (source.u_limited, source.v_limited)
    } else {
        (source.u_full, source.v_full)
    };
    for row in 0..chroma_height {
        let source_start = row * chroma_width;
        let destination_u = (y / 2 + row) * u_plane.stride + x / 2;
        let destination_v = (y / 2 + row) * v_plane.stride + x / 2;
        for column in 0..chroma_width {
            let source_offset = source_start + column;
            let alpha = source.chroma_alpha[source_offset];
            u_plane.data[destination_u + column] = blend_channel(
                u_plane.data[destination_u + column],
                source_u[source_offset],
                alpha,
            );
            v_plane.data[destination_v + column] = blend_channel(
                v_plane.data[destination_v + column],
                source_v[source_offset],
                alpha,
            );
        }
    }
    Ok(())
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

impl CompositionGraph {
    pub(crate) fn new(target: FrameHandle, passes: Vec<CompositionPass>) -> Self {
        Self { target, passes }
    }

    /// Build a decoded-video conformance graph ending in preview or encoder delivery.
    ///
    /// The caller owns resource identity. Supplying stable source and target keys lets a device
    /// backend retain uploaded or scaled textures across frames.
    ///
    /// # Errors
    ///
    /// Returns an error unless both handles describe positive, even-sized opaque YUV 4:2:0 frames
    /// with the same color interpretation.
    pub fn scale_yuv420(
        source: FrameHandle,
        target: FrameHandle,
        mode: VisualScaleMode,
        filter: FrameScaleFilter,
        delivery: FrameDelivery,
    ) -> Result<Self> {
        for (name, handle) in [("source", &source), ("target", &target)] {
            let descriptor = &handle.descriptor;
            if descriptor.format != FrameFormat::Yuv420p8
                || descriptor.alpha_mode != FrameAlphaMode::Opaque
                || descriptor.width == 0
                || descriptor.height == 0
                || !descriptor.width.is_multiple_of(2)
                || !descriptor.height.is_multiple_of(2)
            {
                return Err(Error::InvalidData(format!(
                    "decoded-video scale {name} must be positive even-sized opaque YUV 4:2:0"
                )));
            }
        }
        if source.descriptor.color != target.descriptor.color {
            return Err(Error::InvalidData(
                "decoded-video scale requires matching source and target color descriptions".into(),
            ));
        }
        Ok(Self {
            passes: vec![
                CompositionPass::Scale {
                    source,
                    target: target.clone(),
                    mode,
                    filter,
                },
                CompositionPass::Deliver {
                    source: target.clone(),
                    delivery,
                },
            ],
            target,
        })
    }

    /// In-place frame containing the completed composition.
    #[must_use]
    pub const fn target(&self) -> &FrameHandle {
        &self.target
    }

    /// Ordered project-frame operations.
    #[must_use]
    pub fn passes(&self) -> &[CompositionPass] {
        &self.passes
    }
}

#[cfg(test)]
mod tests {
    use mmrecode_core::{FieldOrder, FrameTiming, Plane};

    use super::*;

    struct TestProvider {
        key: FrameResourceKey,
        frame: VideoFrame,
    }

    impl FrameResourceProvider for TestProvider {
        fn resource(&self, handle: &FrameHandle) -> Option<FrameResourceView<'_>> {
            (handle.key == self.key).then_some(FrameResourceView::Yuv420p8(Yuv420ResourceView {
                frame: &self.frame,
            }))
        }
    }

    fn yuv_frame(width: usize, height: usize) -> VideoFrame {
        VideoFrame {
            format: PixelFormat::Yuv420p8,
            width,
            height,
            planes: vec![
                Plane {
                    data: vec![200; width * height],
                    stride: width,
                    width,
                    height,
                },
                Plane {
                    data: vec![100; width * height / 4],
                    stride: width / 2,
                    width: width / 2,
                    height: height / 2,
                },
                Plane {
                    data: vec![150; width * height / 4],
                    stride: width / 2,
                    width: width / 2,
                    height: height / 2,
                },
            ],
            timing: FrameTiming::default(),
            color: ColorDescription {
                range: ColorRange::Limited,
                ..ColorDescription::default()
            },
            field_order: FieldOrder::Progressive,
        }
    }

    fn handle(
        namespace: FrameResourceNamespace,
        width: u32,
        height: u32,
        color: ColorDescription,
    ) -> FrameHandle {
        FrameHandle {
            key: FrameResourceKey {
                namespace,
                owner: 7,
                revision: 11,
                local_frame: 3,
                width,
                height,
                variant: 0,
            },
            descriptor: FrameDescriptor::yuv420p8(width, height, color),
            residency: FrameResidency::Cpu,
        }
    }

    #[test]
    fn resource_identity_is_independent_of_residency() {
        let key = FrameResourceKey {
            namespace: FrameResourceNamespace::MmfxCanvas,
            owner: 7,
            revision: 11,
            local_frame: 3,
            width: 1920,
            height: 1080,
            variant: 0,
        };
        let cpu = FrameHandle {
            key,
            descriptor: FrameDescriptor::rgba8(1920, 1080),
            residency: FrameResidency::Cpu,
        };
        let device = FrameHandle {
            residency: FrameResidency::Device {
                backend: "wgpu-vulkan".into(),
            },
            ..cpu.clone()
        };
        assert_eq!(cpu.key, device.key);
        assert_eq!(cpu.descriptor, device.descriptor);
        assert_ne!(cpu.residency, device.residency);
    }

    #[test]
    fn cpu_backend_executes_decoded_scale_graph_from_resource_handle() {
        let frame = yuv_frame(4, 4);
        let source = handle(
            FrameResourceNamespace::DecodedVideo,
            4,
            4,
            frame.color.clone(),
        );
        let target = handle(FrameResourceNamespace::Transient, 8, 4, frame.color.clone());
        let graph = CompositionGraph::scale_yuv420(
            source.clone(),
            target,
            VisualScaleMode::Fit,
            FrameScaleFilter::Triangle,
            FrameDelivery::Preview,
        )
        .unwrap();
        assert!(matches!(
            graph.passes(),
            [
                CompositionPass::Scale {
                    filter: FrameScaleFilter::Triangle,
                    ..
                },
                CompositionPass::Deliver {
                    delivery: FrameDelivery::Preview,
                    ..
                }
            ]
        ));
        let provider = TestProvider {
            key: source.key,
            frame,
        };
        let mut output = yuv_frame(2, 2);
        CpuCompositionBackend
            .execute(&graph, &mut output, &provider)
            .unwrap();
        assert_eq!((output.width, output.height), (8, 4));
        assert_eq!(&output.planes[0].data[..2], &[16, 16]);
        assert_eq!(&output.planes[0].data[2..6], &[200; 4]);
    }
}
