//! MMFX scene language and renderer-independent scene model.
//!
//! Source text is parsed and validated into a typed [`Scene`] before it can be
//! rendered. This deliberately keeps the artist-facing syntax independent of
//! the scalar CPU reference renderer and future GPU backends.

mod parser;
mod render;
mod scene;

pub use parser::{Diagnostic, SourceSpan, parse_scene, parse_scene_with_bindings};
pub use render::{
    PreparedScene, RenderError, RenderResources, SceneTime, Surface, prepare_scene, render,
    render_frame_with_resources, render_with_resources,
};
pub use scene::{
    AlignItems, AnimatedStyle, Animation, AnimationDuration, Color, Display, FontResource,
    ImageContent, JustifyContent, Keyframe, Keyframes, Length, Node, NodeKind, ObjectFit, Overflow,
    ParameterKind, ParameterValue, Position, Scene, SceneParameter, Scroll, ScrollDirection, Style,
    TextAlign, TextContent, TextLineHeight, TextWrap, TimingFunction, Transform,
};
