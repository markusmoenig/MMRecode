//! MMFX scene language and renderer-independent scene model.
//!
//! Source text is parsed and validated into a typed [`Scene`] before it can be
//! rendered. This deliberately keeps the artist-facing syntax independent of
//! the scalar CPU reference renderer and future GPU backends.

mod parser;
mod render;
mod scene;

pub use parser::{Diagnostic, SourceSpan, parse_scene};
pub use render::{RenderError, RenderResources, Surface, render, render_with_resources};
pub use scene::{
    Color, FontResource, Length, Node, NodeKind, Overflow, Scene, Style, TextAlign, TextContent,
    TextLineHeight, TextWrap, Transform,
};
