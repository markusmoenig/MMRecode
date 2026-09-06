//! Backend-neutral paint recording and render scheduling for MMFX scenes.
//!
//! Scene evaluation produces a [`DisplayList`]. Lowering that list allocates logical surfaces and
//! emits an ordered [`RenderGraph`]. Backends execute the graph without needing the original scene
//! syntax or layout implementation.

use std::sync::Arc;

use crate::{Color, RenderError, RenderResources};

/// Working color space required by graph operations.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum WorkingColorSpace {
    /// Linear-light sRGB primaries and transfer inversion.
    LinearSrgb,
}

/// Alpha representation required by graph surfaces.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum SurfaceAlphaMode {
    /// Color channels are multiplied by alpha.
    Premultiplied,
}

/// Normative scalar precision against which accelerated output is compared.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum ReferencePrecision {
    /// Unsigned normalized 16-bit color and alpha channels.
    Unorm16,
}

/// Semantic requirements shared by every logical surface in the current graph.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SurfaceDescriptor {
    /// Surface width.
    pub width: u32,
    /// Surface height.
    pub height: u32,
    /// Required working color space.
    pub color_space: WorkingColorSpace,
    /// Required alpha representation.
    pub alpha_mode: SurfaceAlphaMode,
    /// Precision of the scalar reference result.
    pub reference_precision: ReferencePrecision,
}

/// A resolved floating-point rectangle in output pixel coordinates.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct SceneRect {
    /// Left edge.
    pub x: f64,
    /// Top edge.
    pub y: f64,
    /// Width.
    pub width: f64,
    /// Height.
    pub height: f64,
}

/// An integer scissor rectangle using half-open right and bottom edges.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PixelRect {
    /// Inclusive left edge.
    pub left: i32,
    /// Inclusive top edge.
    pub top: i32,
    /// Exclusive right edge.
    pub right: i32,
    /// Exclusive bottom edge.
    pub bottom: i32,
}

impl PixelRect {
    pub(crate) fn intersect(self, other: Self) -> Self {
        Self {
            left: self.left.max(other.left),
            top: self.top.max(other.top),
            right: self.right.min(other.right),
            bottom: self.bottom.min(other.bottom),
        }
    }
}

/// A reusable 8-bit coverage mask positioned on the output canvas.
///
/// Text shaping and the scalar reference rasterizer may prepare these masks on the CPU. A GPU
/// backend can upload them as single-channel textures while preserving identical coverage.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AlphaMask {
    /// Left edge of the mask in output pixels.
    pub left: i32,
    /// Top edge of the mask in output pixels.
    pub top: i32,
    /// Mask width.
    pub width: u32,
    /// Mask height.
    pub height: u32,
    /// Tightly packed row-major coverage values.
    pub pixels: Arc<[u8]>,
}

impl AlphaMask {
    pub(crate) fn coverage_at(&self, x: i32, y: i32) -> u8 {
        let local_x = i64::from(x) - i64::from(self.left);
        let local_y = i64::from(y) - i64::from(self.top);
        if local_x < 0
            || local_y < 0
            || local_x >= i64::from(self.width)
            || local_y >= i64::from(self.height)
        {
            return 0;
        }
        let index = usize::try_from(local_y).expect("non-negative mask y")
            * usize::try_from(self.width).expect("mask width")
            + usize::try_from(local_x).expect("non-negative mask x");
        self.pixels.get(index).copied().unwrap_or(0)
    }

    pub(crate) fn clip(&self) -> PixelRect {
        let right = i64::from(self.left) + i64::from(self.width);
        let bottom = i64::from(self.top) + i64::from(self.height);
        PixelRect {
            left: self.left,
            top: self.top,
            right: i32::try_from(right).unwrap_or(i32::MAX),
            bottom: i32::try_from(bottom).unwrap_or(i32::MAX),
        }
    }
}

/// A resolved transform applied to one isolated layer.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LayerTransform {
    /// Horizontal scale around the resolved layer bounds.
    pub scale_x: f32,
    /// Vertical scale around the resolved layer bounds.
    pub scale_y: f32,
    /// Clockwise rotation in degrees.
    pub rotate_degrees: f32,
}

impl LayerTransform {
    /// Identity transform.
    pub const IDENTITY: Self = Self {
        scale_x: 1.0,
        scale_y: 1.0,
        rotate_degrees: 0.0,
    };

    /// Returns whether the transform changes its input.
    #[must_use]
    pub fn is_identity(self) -> bool {
        (self.scale_x - 1.0).abs() <= f32::EPSILON
            && (self.scale_y - 1.0).abs() <= f32::EPSILON
            && self.rotate_degrees.abs() <= f32::EPSILON
    }
}

/// One backend-neutral primitive draw operation.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub enum DrawCommand {
    /// Rasterize and fill a rounded rectangle.
    FillRoundedRect {
        /// Resolved shape bounds.
        bounds: SceneRect,
        /// Corner radius in output pixels.
        radius: f64,
        /// Straight-alpha sRGB source color.
        color: Color,
        /// Integer scissor inherited from scene overflow.
        clip: PixelRect,
        /// Fractional ancestor clips multiplied into coverage.
        coverage_clips: Vec<AlphaMask>,
    },
    /// Sample a named decoded image into a resolved rectangle.
    DrawImage {
        /// Resource name in [`RenderResources`].
        source: String,
        /// Resolved destination rectangle after object-fit.
        destination: SceneRect,
        /// Integer scissor including the image object's box.
        clip: PixelRect,
        /// Fractional ancestor clips multiplied into coverage.
        coverage_clips: Vec<AlphaMask>,
    },
    /// Paint a shaped glyph or other prepared coverage mask.
    PaintMask {
        /// Positioned alpha coverage.
        mask: AlphaMask,
        /// Straight-alpha sRGB source color.
        color: Color,
        /// Integer scissor inherited from scene overflow.
        clip: PixelRect,
        /// Fractional ancestor clips multiplied into coverage.
        coverage_clips: Vec<AlphaMask>,
    },
}

/// One semantic operation recorded by scene evaluation.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub enum DisplayCommand {
    /// A primitive draw in the current target.
    Draw(DrawCommand),
    /// An isolated group which is transformed and composited as one unit.
    Layer {
        /// Bounds defining the transform origin.
        bounds: SceneRect,
        /// Layer opacity in the full `u16` range.
        opacity: u16,
        /// Resolved post-layout transform.
        transform: LayerTransform,
        /// Parent scissor used while compositing the isolated layer.
        clip: PixelRect,
        /// Paint operations inside the isolated layer.
        commands: Vec<DisplayCommand>,
    },
}

/// Resolved paint operations for one scene-local frame.
#[derive(Clone, Debug, PartialEq)]
pub struct DisplayList {
    width: u32,
    height: u32,
    commands: Vec<DisplayCommand>,
}

impl DisplayList {
    pub(crate) fn new(width: u32, height: u32, commands: Vec<DisplayCommand>) -> Self {
        Self {
            width,
            height,
            commands,
        }
    }

    /// Output width.
    #[must_use]
    pub const fn width(&self) -> u32 {
        self.width
    }

    /// Output height.
    #[must_use]
    pub const fn height(&self) -> u32 {
        self.height
    }

    /// Recorded semantic operations in paint order.
    #[must_use]
    pub fn commands(&self) -> &[DisplayCommand] {
        &self.commands
    }

    /// Lower semantic paint operations into explicit logical surfaces and passes.
    #[must_use]
    pub fn lower(&self) -> RenderGraph {
        GraphBuilder::new(self.width, self.height).lower(&self.commands)
    }
}

/// Logical surface identifier local to one render graph.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct SurfaceId(pub u32);

/// One ordered render-graph pass.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub enum RenderPass {
    /// Draw one or more consecutive primitives into a logical surface.
    Draw {
        /// Destination surface.
        target: SurfaceId,
        /// Ordered primitive operations.
        commands: Vec<DrawCommand>,
    },
    /// Transform one isolated surface into another.
    Transform {
        /// Untransformed source surface.
        source: SurfaceId,
        /// Transformed destination surface.
        target: SurfaceId,
        /// Bounds defining the transform origin.
        bounds: SceneRect,
        /// Resolved transform.
        transform: LayerTransform,
    },
    /// Composite a source surface over a destination surface.
    Composite {
        /// Source surface.
        source: SurfaceId,
        /// Destination surface.
        target: SurfaceId,
        /// Layer opacity in the full `u16` range.
        opacity: u16,
        /// Destination scissor.
        clip: PixelRect,
    },
}

/// Explicit backend-neutral schedule for one rendered frame.
#[derive(Clone, Debug, PartialEq)]
pub struct RenderGraph {
    surface: SurfaceDescriptor,
    surface_count: u32,
    output: SurfaceId,
    passes: Vec<RenderPass>,
}

impl RenderGraph {
    /// Output width.
    #[must_use]
    pub const fn width(&self) -> u32 {
        self.surface.width
    }

    /// Output height.
    #[must_use]
    pub const fn height(&self) -> u32 {
        self.surface.height
    }

    /// Color, alpha, dimensions, and reference-precision contract for logical surfaces.
    ///
    /// Every logical surface begins transparent. Backends may use a different physical format only
    /// when their selected quality policy permits it; conformance is measured against this
    /// descriptor's reference precision.
    #[must_use]
    pub const fn surface_descriptor(&self) -> SurfaceDescriptor {
        self.surface
    }

    /// Number of transient logical surfaces required by this graph.
    #[must_use]
    pub const fn surface_count(&self) -> u32 {
        self.surface_count
    }

    /// Surface containing the completed frame.
    #[must_use]
    pub const fn output(&self) -> SurfaceId {
        self.output
    }

    /// Ordered passes. Dependencies are represented by surface references and pass order.
    #[must_use]
    pub fn passes(&self) -> &[RenderPass] {
        &self.passes
    }
}

/// Execution boundary implemented by scalar CPU and future accelerated renderers.
pub trait RenderBackend {
    /// Backend-specific completed frame or texture handle.
    type Output;

    /// Stable diagnostic name.
    fn name(&self) -> &'static str;

    /// Execute a previously evaluated graph with its immutable binary resources.
    ///
    /// # Errors
    ///
    /// Returns a backend-specific rendering error.
    fn execute(
        &mut self,
        graph: &RenderGraph,
        resources: &RenderResources,
    ) -> Result<Self::Output, RenderError>;
}

struct GraphBuilder {
    width: u32,
    height: u32,
    next_surface: u32,
    passes: Vec<RenderPass>,
}

impl GraphBuilder {
    fn new(width: u32, height: u32) -> Self {
        Self {
            width,
            height,
            next_surface: 1,
            passes: Vec::new(),
        }
    }

    fn lower(mut self, commands: &[DisplayCommand]) -> RenderGraph {
        self.lower_commands(SurfaceId(0), commands);
        RenderGraph {
            surface: SurfaceDescriptor {
                width: self.width,
                height: self.height,
                color_space: WorkingColorSpace::LinearSrgb,
                alpha_mode: SurfaceAlphaMode::Premultiplied,
                reference_precision: ReferencePrecision::Unorm16,
            },
            surface_count: self.next_surface,
            output: SurfaceId(0),
            passes: self.passes,
        }
    }

    fn allocate_surface(&mut self) -> SurfaceId {
        let id = SurfaceId(self.next_surface);
        self.next_surface = self.next_surface.saturating_add(1);
        id
    }

    fn lower_commands(&mut self, target: SurfaceId, commands: &[DisplayCommand]) {
        let mut draws = Vec::new();
        for command in commands {
            match command {
                DisplayCommand::Draw(draw) => draws.push(draw.clone()),
                DisplayCommand::Layer {
                    bounds,
                    opacity,
                    transform,
                    clip,
                    commands,
                } => {
                    self.flush_draws(target, &mut draws);
                    let layer = self.allocate_surface();
                    self.lower_commands(layer, commands);
                    let source = if transform.is_identity() {
                        layer
                    } else {
                        let transformed = self.allocate_surface();
                        self.passes.push(RenderPass::Transform {
                            source: layer,
                            target: transformed,
                            bounds: *bounds,
                            transform: *transform,
                        });
                        transformed
                    };
                    self.passes.push(RenderPass::Composite {
                        source,
                        target,
                        opacity: *opacity,
                        clip: *clip,
                    });
                }
            }
        }
        self.flush_draws(target, &mut draws);
    }

    fn flush_draws(&mut self, target: SurfaceId, draws: &mut Vec<DrawCommand>) {
        if !draws.is_empty() {
            self.passes.push(RenderPass::Draw {
                target,
                commands: std::mem::take(draws),
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        DisplayCommand, DisplayList, LayerTransform, PixelRect, ReferencePrecision, RenderPass,
        SceneRect, SurfaceAlphaMode, WorkingColorSpace,
    };

    #[test]
    fn lowering_makes_layer_dependencies_explicit() {
        let clip = PixelRect {
            left: 0,
            top: 0,
            right: 1920,
            bottom: 1080,
        };
        let list = DisplayList::new(
            1920,
            1080,
            vec![DisplayCommand::Layer {
                bounds: SceneRect {
                    x: 10.0,
                    y: 20.0,
                    width: 100.0,
                    height: 50.0,
                },
                opacity: u16::MAX,
                transform: LayerTransform {
                    scale_x: 2.0,
                    scale_y: 1.0,
                    rotate_degrees: 0.0,
                },
                clip,
                commands: Vec::new(),
            }],
        );
        let graph = list.lower();
        assert_eq!(graph.surface_count(), 3);
        assert_eq!(
            graph.surface_descriptor().color_space,
            WorkingColorSpace::LinearSrgb
        );
        assert_eq!(
            graph.surface_descriptor().alpha_mode,
            SurfaceAlphaMode::Premultiplied
        );
        assert_eq!(
            graph.surface_descriptor().reference_precision,
            ReferencePrecision::Unorm16
        );
        assert!(matches!(graph.passes()[0], RenderPass::Transform { .. }));
        assert!(matches!(graph.passes()[1], RenderPass::Composite { .. }));
    }
}
