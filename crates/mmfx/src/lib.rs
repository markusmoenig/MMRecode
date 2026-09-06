//! MMFX scene language and renderer-independent scene model.
//!
//! Source text is parsed and validated into a typed [`Scene`] before it can be
//! rendered. This deliberately keeps the artist-facing syntax independent of
//! the scalar CPU reference renderer and future GPU backends.

mod graph;
mod parser;
mod render;
mod scene;

pub use graph::{
    AlphaMask, DisplayCommand, DisplayList, DrawCommand, LayerTransform, PixelRect,
    ReferencePrecision, RenderBackend, RenderGraph, RenderPass, SceneRect, SurfaceAlphaMode,
    SurfaceDescriptor, SurfaceId, WorkingColorSpace,
};
pub use parser::{Diagnostic, SourceSpan, parse_scene, parse_scene_with_bindings};
pub use render::{
    ImageResourceView, PreparedScene, RenderError, RenderResources, ScalarCpuBackend, SceneTime,
    Surface, prepare_scene, render, render_frame_with_resources, render_with_resources,
};
pub use scene::{
    AlignItems, AnimatedStyle, Animation, AnimationDuration, Color, Display, FontResource,
    ImageContent, JustifyContent, Keyframe, Keyframes, Length, Node, NodeKind, ObjectFit, Overflow,
    ParameterKind, ParameterValue, Position, Scene, SceneParameter, Scroll, ScrollDirection, Style,
    TextAlign, TextContent, TextLineHeight, TextWrap, TimingFunction, Transform,
};
