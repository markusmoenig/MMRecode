//! Strict parser and semantic validation for the initial MMFX scene subset.

use std::collections::{BTreeMap, BTreeSet};

use crate::{
    AlignItems, AnimatedStyle, Animation, AnimationDuration, Color, Display, FontResource,
    ImageContent, JustifyContent, Keyframe, Keyframes, Length, Node, NodeKind, ObjectFit, Overflow,
    ParameterKind, ParameterValue, Position, Scene, SceneParameter, Scroll, ScrollDirection, Style,
    TextAlign, TextContent, TextLineHeight, TextWrap, TimingFunction, Transform,
};

/// Half-open byte range in MMFX source text.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct SourceSpan {
    /// First byte included by this span.
    pub start: usize,
    /// First byte after this span.
    pub end: usize,
}

impl SourceSpan {
    /// Resolve the start of this span to a one-based line and column.
    #[must_use]
    pub fn line_column(self, source: &str) -> (usize, usize) {
        let prefix = source.get(..self.start).unwrap_or(source);
        let line = prefix.bytes().filter(|byte| *byte == b'\n').count() + 1;
        let column = prefix
            .rsplit_once('\n')
            .map_or(prefix.chars().count() + 1, |(_, tail)| {
                tail.chars().count() + 1
            });
        (line, column)
    }
}

/// A source-located MMFX syntax or validation error.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Diagnostic {
    /// Human-readable description of the problem.
    pub message: String,
    /// Source range responsible for the problem.
    pub span: SourceSpan,
    /// Optional corrective hint.
    pub help: Option<String>,
}

impl Diagnostic {
    fn new(message: impl Into<String>, span: SourceSpan) -> Self {
        Self {
            message: message.into(),
            span,
            help: None,
        }
    }

    fn with_help(mut self, help: impl Into<String>) -> Self {
        self.help = Some(help.into());
        self
    }
}

/// Parse and validate one MMFX scene.
///
/// All diagnostics are returned together where recovery is possible, so an
/// editor can underline several independent mistakes in a single pass.
///
/// # Errors
///
/// Returns source-spanned diagnostics when the document has invalid syntax or
/// cannot be lowered to a fully typed scene.
pub fn parse_scene(source: &str) -> Result<Scene, Vec<Diagnostic>> {
    parse_scene_with_bindings(source, &BTreeMap::new())
}

/// Parse and validate a scene with host-provided parameter bindings.
///
/// Binding names omit the source-level `--` prefix. Values use command-friendly spelling: text is
/// accepted directly, while colors, lengths, numbers, booleans, and choices use their MMFX source
/// spelling.
///
/// # Errors
///
/// Returns source-spanned diagnostics for invalid source, unknown bindings, or values that do not
/// match their declared parameter type.
pub fn parse_scene_with_bindings(
    source: &str,
    bindings: &BTreeMap<String, String>,
) -> Result<Scene, Vec<Diagnostic>> {
    let mut parser = Parser::new(source);
    let raw = parser.parse_document();
    if !parser.diagnostics.is_empty() {
        return Err(parser.diagnostics);
    }

    if raw.is_empty() {
        return Err(vec![Diagnostic::new(
            "expected an @scene block",
            SourceSpan::default(),
        )]);
    }
    let mut diagnostics = Vec::new();
    let scene = lower_document(raw, bindings, &mut diagnostics);
    if !diagnostics.is_empty() {
        Err(diagnostics)
    } else if let Some(scene) = scene {
        Ok(scene)
    } else {
        Err(vec![Diagnostic::new(
            "scene could not be lowered",
            SourceSpan::default(),
        )])
    }
}

#[derive(Debug)]
struct RawBlock {
    kind: String,
    name: String,
    span: SourceSpan,
    properties: Vec<RawProperty>,
    children: Vec<Self>,
}

#[derive(Debug)]
struct RawProperty {
    name: String,
    name_span: SourceSpan,
    value: String,
    value_span: SourceSpan,
}

struct Parser<'a> {
    source: &'a str,
    cursor: usize,
    diagnostics: Vec<Diagnostic>,
}

impl<'a> Parser<'a> {
    fn new(source: &'a str) -> Self {
        Self {
            source,
            cursor: 0,
            diagnostics: Vec::new(),
        }
    }

    fn parse_document(&mut self) -> Vec<RawBlock> {
        let mut blocks = Vec::new();
        self.skip_trivia();
        while !self.at_end() {
            if let Some(block) = self.parse_block() {
                blocks.push(block);
            }
            self.skip_trivia();
        }
        blocks
    }

    fn parse_block(&mut self) -> Option<RawBlock> {
        let start = self.cursor;
        if !self.consume('@') {
            self.diagnostics.push(Diagnostic::new(
                "expected an object beginning with '@'",
                self.span_at_cursor(),
            ));
            self.recover_to_block_boundary();
            return None;
        }
        let kind = self.parse_ident("expected an object type after '@'")?;
        self.skip_trivia();
        let name = self.parse_ident("expected an object name")?;
        self.skip_trivia();
        if !self.expect('{', "expected '{' after the object name") {
            return None;
        }

        let mut properties = Vec::new();
        let mut children = Vec::new();
        loop {
            self.skip_trivia();
            if self.consume('}') {
                break;
            }
            if self.at_end() {
                self.diagnostics.push(Diagnostic::new(
                    "unterminated object; expected '}'",
                    SourceSpan {
                        start,
                        end: self.cursor,
                    },
                ));
                break;
            }
            if kind == "keyframes" && self.peek() != Some('@') {
                if let Some(child) = self.parse_keyframe_stop() {
                    children.push(child);
                }
            } else if self.peek() == Some('@') {
                if let Some(child) = self.parse_block() {
                    children.push(child);
                }
            } else if let Some(property) = self.parse_property() {
                properties.push(property);
            }
        }

        Some(RawBlock {
            kind,
            name,
            span: SourceSpan {
                start,
                end: self.cursor,
            },
            properties,
            children,
        })
    }

    fn parse_keyframe_stop(&mut self) -> Option<RawBlock> {
        let start = self.cursor;
        while self
            .peek()
            .is_some_and(|character| !character.is_whitespace() && character != '{')
        {
            self.bump();
        }
        if start == self.cursor {
            self.diagnostics.push(Diagnostic::new(
                "expected a keyframe selector such as from, 50%, or to",
                self.span_at_cursor(),
            ));
            self.recover_to_block_boundary();
            return None;
        }
        let name = self.source[start..self.cursor].to_owned();
        self.skip_trivia();
        if !self.expect('{', "expected '{' after the keyframe selector") {
            return None;
        }
        let mut properties = Vec::new();
        loop {
            self.skip_trivia();
            if self.consume('}') {
                break;
            }
            if self.at_end() {
                self.diagnostics.push(Diagnostic::new(
                    "unterminated keyframe stop; expected '}'",
                    SourceSpan {
                        start,
                        end: self.cursor,
                    },
                ));
                break;
            }
            if self.peek() == Some('@') {
                self.diagnostics.push(Diagnostic::new(
                    "keyframe stops cannot contain objects",
                    self.span_at_cursor(),
                ));
                let _ = self.parse_block();
            } else if let Some(property) = self.parse_property() {
                properties.push(property);
            }
        }
        Some(RawBlock {
            kind: "keyframe-stop".into(),
            name,
            span: SourceSpan {
                start,
                end: self.cursor,
            },
            properties,
            children: Vec::new(),
        })
    }

    fn parse_property(&mut self) -> Option<RawProperty> {
        let name_start = self.cursor;
        let name = self.parse_ident("expected a property name")?;
        let name_span = SourceSpan {
            start: name_start,
            end: self.cursor,
        };
        self.skip_trivia();
        if !self.expect(':', "expected ':' after the property name") {
            self.recover_to_property_boundary();
            return None;
        }
        self.skip_trivia();
        let raw_start = self.cursor;
        let mut parentheses = 0_u32;
        let mut quote = None;
        let mut escaped = false;
        while let Some(character) = self.peek() {
            if let Some(delimiter) = quote {
                self.bump();
                if escaped {
                    escaped = false;
                } else if character == '\\' {
                    escaped = true;
                } else if character == delimiter {
                    quote = None;
                }
                continue;
            }
            match character {
                '\'' | '"' => {
                    quote = Some(character);
                    self.bump();
                }
                '(' => {
                    parentheses += 1;
                    self.bump();
                }
                ')' => {
                    parentheses = parentheses.saturating_sub(1);
                    self.bump();
                }
                ';' if parentheses == 0 => break,
                '}' if parentheses == 0 => {
                    self.diagnostics.push(Diagnostic::new(
                        format!("property '{name}' must end with ';'"),
                        name_span,
                    ));
                    return None;
                }
                _ => self.bump(),
            }
        }
        let raw_end = self.cursor;
        if quote.is_some() {
            self.diagnostics.push(Diagnostic::new(
                format!("unterminated string in property '{name}'"),
                SourceSpan {
                    start: raw_start,
                    end: raw_end,
                },
            ));
        }
        if !self.consume(';') {
            self.diagnostics.push(Diagnostic::new(
                format!("property '{name}' must end with ';'"),
                name_span,
            ));
        }
        let raw = &self.source[raw_start..raw_end];
        let leading = raw.len() - raw.trim_start().len();
        let trimmed = raw.trim();
        let value_start = raw_start + leading;
        let value_span = SourceSpan {
            start: value_start,
            end: value_start + trimmed.len(),
        };
        if trimmed.is_empty() {
            self.diagnostics
                .push(Diagnostic::new("expected a property value", value_span));
        }
        Some(RawProperty {
            name,
            name_span,
            value: trimmed.to_owned(),
            value_span,
        })
    }

    fn parse_ident(&mut self, message: &str) -> Option<String> {
        self.skip_trivia();
        let start = self.cursor;
        while self.peek().is_some_and(is_ident_character) {
            self.bump();
        }
        if start == self.cursor {
            self.diagnostics
                .push(Diagnostic::new(message, self.span_at_cursor()));
            None
        } else {
            Some(self.source[start..self.cursor].to_owned())
        }
    }

    fn skip_trivia(&mut self) {
        loop {
            while self.peek().is_some_and(char::is_whitespace) {
                self.bump();
            }
            if self.source[self.cursor..].starts_with("/*") {
                let start = self.cursor;
                self.cursor += 2;
                if let Some(length) = self.source[self.cursor..].find("*/") {
                    self.cursor += length + 2;
                } else {
                    self.cursor = self.source.len();
                    self.diagnostics.push(Diagnostic::new(
                        "unterminated comment",
                        SourceSpan {
                            start,
                            end: self.cursor,
                        },
                    ));
                    return;
                }
            } else {
                return;
            }
        }
    }

    fn expect(&mut self, expected: char, message: &str) -> bool {
        if self.consume(expected) {
            true
        } else {
            self.diagnostics
                .push(Diagnostic::new(message, self.span_at_cursor()));
            false
        }
    }

    fn consume(&mut self, expected: char) -> bool {
        if self.peek() == Some(expected) {
            self.bump();
            true
        } else {
            false
        }
    }

    fn peek(&self) -> Option<char> {
        self.source[self.cursor..].chars().next()
    }

    fn bump(&mut self) {
        if let Some(character) = self.peek() {
            self.cursor += character.len_utf8();
        }
    }

    fn at_end(&self) -> bool {
        self.cursor == self.source.len()
    }

    fn span_at_cursor(&self) -> SourceSpan {
        SourceSpan {
            start: self.cursor,
            end: self.cursor + self.peek().map_or(0, char::len_utf8),
        }
    }

    fn recover_to_property_boundary(&mut self) {
        while let Some(character) = self.peek() {
            if matches!(character, ';' | '}') {
                if character == ';' {
                    self.bump();
                }
                return;
            }
            self.bump();
        }
    }

    fn recover_to_block_boundary(&mut self) {
        while let Some(character) = self.peek() {
            if matches!(character, '@' | '}') {
                return;
            }
            self.bump();
        }
    }
}

fn is_ident_character(character: char) -> bool {
    character.is_ascii_alphanumeric() || matches!(character, '-' | '_')
}

fn lower_document(
    raw: Vec<RawBlock>,
    bindings: &BTreeMap<String, String>,
    diagnostics: &mut Vec<Diagnostic>,
) -> Option<Scene> {
    let mut scene = None;
    let mut keyframe_blocks = Vec::new();
    let mut parameter_blocks = Vec::new();
    for block in raw {
        match block.kind.as_str() {
            "scene" if scene.is_none() => scene = Some(block),
            "scene" => diagnostics.push(Diagnostic::new(
                "only one top-level @scene block is allowed",
                block.span,
            )),
            "keyframes" => keyframe_blocks.push(block),
            "param" => parameter_blocks.push(block),
            _ => diagnostics.push(
                Diagnostic::new(
                    format!(
                        "top-level object must be @scene, @param, or @keyframes, not @{}",
                        block.kind
                    ),
                    block.span,
                )
                .with_help("wrap visual objects in an @scene name { ... } block"),
            ),
        }
    }
    let mut parameters = BTreeMap::new();
    for block in parameter_blocks {
        if let Some(parameter) = lower_parameter(block, bindings, diagnostics) {
            if parameters.contains_key(&parameter.name) {
                diagnostics.push(Diagnostic::new(
                    format!("duplicate @param --{}", parameter.name),
                    SourceSpan::default(),
                ));
            } else {
                parameters.insert(parameter.name.clone(), parameter);
            }
        }
    }
    for name in bindings.keys() {
        let name = name.strip_prefix("--").unwrap_or(name);
        if !parameters.contains_key(name) {
            diagnostics.push(
                Diagnostic::new(
                    format!("binding '--{name}' has no matching @param declaration"),
                    SourceSpan::default(),
                )
                .with_help(format!(
                    "declare @param --{name} or remove the stored binding"
                )),
            );
        }
    }
    let values = parameters
        .iter()
        .map(|(name, parameter)| (name.clone(), parameter_value_source(&parameter.value)))
        .collect::<BTreeMap<_, _>>();
    let Some(mut scene) = scene else {
        diagnostics.push(Diagnostic::new(
            "expected one top-level @scene block",
            SourceSpan::default(),
        ));
        return None;
    };
    substitute_block_variables(&mut scene, &values, diagnostics);
    for block in &mut keyframe_blocks {
        substitute_block_variables(block, &values, diagnostics);
    }

    let animations = keyframe_blocks
        .into_iter()
        .filter_map(|block| lower_keyframes(block, diagnostics))
        .collect::<Vec<_>>();
    let mut animation_names = BTreeSet::new();
    for animation in &animations {
        if !animation_names.insert(animation.name.clone()) {
            diagnostics.push(Diagnostic::new(
                format!("duplicate @keyframes definition '{}'", animation.name),
                SourceSpan::default(),
            ));
        }
    }
    lower_scene(
        scene,
        parameters.into_values().collect(),
        animations,
        &animation_names,
        diagnostics,
    )
}

fn lower_parameter(
    raw: RawBlock,
    bindings: &BTreeMap<String, String>,
    diagnostics: &mut Vec<Diagnostic>,
) -> Option<SceneParameter> {
    if !raw.children.is_empty() {
        diagnostics.push(Diagnostic::new("@param cannot contain blocks", raw.span));
    }
    let Some(name) = raw.name.strip_prefix("--").filter(|name| !name.is_empty()) else {
        diagnostics.push(
            Diagnostic::new("@param names must begin with '--'", raw.span)
                .with_help("example: @param --title { type: text; default: \"Title\"; }"),
        );
        return None;
    };
    let properties = collect_properties(raw.properties, diagnostics);
    reject_unknown(&properties, &["type", "default", "choices"], diagnostics);
    let Some(kind_property) = properties.get("type") else {
        diagnostics.push(Diagnostic::new(
            "@param requires a 'type' property",
            raw.span,
        ));
        return None;
    };
    let kind = match kind_property.value.as_str() {
        "text" => ParameterKind::Text,
        "color" => ParameterKind::Color,
        "length" => ParameterKind::Length,
        "number" => ParameterKind::Number,
        "boolean" => ParameterKind::Boolean,
        "choice" => ParameterKind::Choice,
        _ => {
            diagnostics.push(Diagnostic::new(
                "parameter type must be text, color, length, number, boolean, or choice",
                kind_property.value_span,
            ));
            return None;
        }
    };
    let choices = if kind == ParameterKind::Choice {
        let Some(property) = properties.get("choices") else {
            diagnostics.push(Diagnostic::new(
                "choice @param requires a 'choices' property",
                raw.span,
            ));
            return None;
        };
        let choices = parse_string(property, diagnostics)?;
        let choices = choices
            .split(',')
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
            .collect::<Vec<_>>();
        if choices.is_empty() {
            diagnostics.push(Diagnostic::new(
                "choice parameter requires at least one comma-separated choice",
                property.value_span,
            ));
            return None;
        }
        choices
    } else {
        if let Some(property) = properties.get("choices") {
            diagnostics.push(Diagnostic::new(
                "only choice parameters accept a 'choices' property",
                property.name_span,
            ));
        }
        Vec::new()
    };
    let Some(default_property) = properties.get("default") else {
        diagnostics.push(Diagnostic::new(
            "@param requires a 'default' property",
            raw.span,
        ));
        return None;
    };
    let default = parse_parameter_value(kind, default_property, &choices, false, diagnostics)?;
    let binding = bindings
        .get(name)
        .or_else(|| bindings.get(&format!("--{name}")));
    let value = binding.map_or_else(
        || Some(default.clone()),
        |value| {
            let property = RawProperty {
                name: format!("binding --{name}"),
                name_span: raw.span,
                value: value.clone(),
                value_span: raw.span,
            };
            parse_parameter_value(kind, &property, &choices, true, diagnostics)
        },
    )?;
    Some(SceneParameter {
        name: name.to_owned(),
        kind,
        default,
        value,
        choices,
    })
}

fn parse_parameter_value(
    kind: ParameterKind,
    property: &RawProperty,
    choices: &[String],
    host_binding: bool,
    diagnostics: &mut Vec<Diagnostic>,
) -> Option<ParameterValue> {
    match kind {
        ParameterKind::Text if host_binding => Some(ParameterValue::Text(property.value.clone())),
        ParameterKind::Text => parse_string(property, diagnostics).map(ParameterValue::Text),
        ParameterKind::Color => parse_color(property, diagnostics).map(ParameterValue::Color),
        ParameterKind::Length => {
            parse_box_length(property, diagnostics).map(ParameterValue::Length)
        }
        ParameterKind::Number => match property.value.parse::<f32>() {
            Ok(value) if value.is_finite() => Some(ParameterValue::Number(value)),
            _ => {
                diagnostics.push(Diagnostic::new(
                    "number parameter must be finite",
                    property.value_span,
                ));
                None
            }
        },
        ParameterKind::Boolean => match property.value.as_str() {
            "true" => Some(ParameterValue::Boolean(true)),
            "false" => Some(ParameterValue::Boolean(false)),
            _ => {
                diagnostics.push(Diagnostic::new(
                    "boolean parameter must be true or false",
                    property.value_span,
                ));
                None
            }
        },
        ParameterKind::Choice => {
            let value = if host_binding {
                property.value.clone()
            } else {
                parse_identifier_or_string(property, diagnostics)?
            };
            if choices.contains(&value) {
                Some(ParameterValue::Choice(value))
            } else {
                diagnostics.push(
                    Diagnostic::new(
                        format!("choice must be one of {}", choices.join(", ")),
                        property.value_span,
                    )
                    .with_help(format!("try {}", choices.join(" or "))),
                );
                None
            }
        }
    }
}

fn parameter_value_source(value: &ParameterValue) -> String {
    match value {
        ParameterValue::Text(value) => format!(
            "\"{}\"",
            value
                .replace('\\', "\\\\")
                .replace('"', "\\\"")
                .replace('\n', "\\n")
                .replace('\r', "\\r")
                .replace('\t', "\\t")
        ),
        ParameterValue::Color(value) => format!(
            "#{:02x}{:02x}{:02x}{:02x}",
            value.red, value.green, value.blue, value.alpha
        ),
        ParameterValue::Length(value) => ParameterValue::Length(*value).display(),
        ParameterValue::Number(value) => value.to_string(),
        ParameterValue::Boolean(value) => value.to_string(),
        ParameterValue::Choice(value) => value.clone(),
    }
}

fn substitute_block_variables(
    block: &mut RawBlock,
    values: &BTreeMap<String, String>,
    diagnostics: &mut Vec<Diagnostic>,
) {
    for property in &mut block.properties {
        let value = property.value.trim();
        if !value.contains("var(") {
            continue;
        }
        let Some(name) = value
            .strip_prefix("var(")
            .and_then(|value| value.strip_suffix(')'))
            .map(str::trim)
            .and_then(|name| name.strip_prefix("--"))
        else {
            diagnostics.push(
                Diagnostic::new(
                    "a typed var() reference must occupy the complete property value",
                    property.value_span,
                )
                .with_help("example: color: var(--accent);"),
            );
            continue;
        };
        if let Some(value) = values.get(name) {
            property.value.clone_from(value);
        } else {
            diagnostics.push(
                Diagnostic::new(
                    format!("unknown scene parameter '--{name}'"),
                    property.value_span,
                )
                .with_help(format!("declare @param --{name} before the scene")),
            );
        }
    }
    for child in &mut block.children {
        substitute_block_variables(child, values, diagnostics);
    }
}

fn lower_scene(
    raw: RawBlock,
    parameters: Vec<SceneParameter>,
    animations: Vec<Keyframes>,
    animation_names: &BTreeSet<String>,
    diagnostics: &mut Vec<Diagnostic>,
) -> Option<Scene> {
    let properties = collect_properties(raw.properties, diagnostics);
    let width = required_scene_dimension("width", &properties, raw.span, diagnostics);
    let height = required_scene_dimension("height", &properties, raw.span, diagnostics);
    let background = properties
        .get("background")
        .and_then(|property| parse_color(property, diagnostics))
        .unwrap_or(Color::TRANSPARENT);
    reject_unknown(&properties, &["width", "height", "background"], diagnostics);

    let mut fonts_with_spans = Vec::new();
    let mut node_blocks = Vec::new();
    for child in raw.children {
        if child.kind == "font" {
            if let Some(font) = lower_font(child, diagnostics) {
                fonts_with_spans.push(font);
            }
        } else {
            node_blocks.push(child);
        }
    }
    let mut font_names = BTreeMap::new();
    for (font, span) in &fonts_with_spans {
        if let Some(previous) = font_names.insert(font.name.clone(), *span) {
            diagnostics.push(
                Diagnostic::new(format!("duplicate font resource '{}'", font.name), *span)
                    .with_help(format!(
                        "the first declaration starts at byte {}",
                        previous.start
                    )),
            );
        }
    }
    let children = node_blocks
        .into_iter()
        .filter_map(|child| lower_node(child, &font_names, animation_names, diagnostics))
        .collect();
    let fonts = fonts_with_spans.into_iter().map(|(font, _)| font).collect();
    match (width, height) {
        (Some(width), Some(height)) => Some(Scene {
            name: raw.name,
            width,
            height,
            background,
            fonts,
            parameters,
            animations,
            children,
        }),
        _ => None,
    }
}

fn lower_font(
    raw: RawBlock,
    diagnostics: &mut Vec<Diagnostic>,
) -> Option<(FontResource, SourceSpan)> {
    if !raw.children.is_empty() {
        diagnostics.push(Diagnostic::new(
            "@font resources cannot contain objects",
            raw.span,
        ));
    }
    let properties = collect_properties(raw.properties, diagnostics);
    reject_unknown(&properties, &["src"], diagnostics);
    let Some(source_property) = properties.get("src") else {
        diagnostics.push(Diagnostic::new("@font requires a 'src' property", raw.span));
        return None;
    };
    let source = parse_string(source_property, diagnostics)?;
    Some((
        FontResource {
            name: raw.name,
            source,
        },
        raw.span,
    ))
}

fn lower_keyframes(raw: RawBlock, diagnostics: &mut Vec<Diagnostic>) -> Option<Keyframes> {
    if !raw.properties.is_empty() {
        diagnostics.push(Diagnostic::new(
            "@keyframes may contain only keyframe stops",
            raw.span,
        ));
    }
    let mut stops = raw
        .children
        .into_iter()
        .filter_map(|stop| {
            let offset = parse_keyframe_offset(&stop.name, stop.span, diagnostics)?;
            let properties = collect_properties(stop.properties, diagnostics);
            let allowed = [
                "left",
                "top",
                "width",
                "height",
                "background",
                "color",
                "opacity",
                "transform",
            ];
            reject_unknown(&properties, &allowed, diagnostics);
            let style = AnimatedStyle {
                left: properties
                    .get("left")
                    .and_then(|property| parse_signed_length(property, diagnostics)),
                top: properties
                    .get("top")
                    .and_then(|property| parse_signed_length(property, diagnostics)),
                width: properties
                    .get("width")
                    .and_then(|property| parse_box_length(property, diagnostics)),
                height: properties
                    .get("height")
                    .and_then(|property| parse_box_length(property, diagnostics)),
                background: properties
                    .get("background")
                    .and_then(|property| parse_color(property, diagnostics)),
                color: properties
                    .get("color")
                    .and_then(|property| parse_color(property, diagnostics)),
                opacity: properties
                    .get("opacity")
                    .and_then(|property| parse_opacity(property, diagnostics)),
                transform: properties
                    .get("transform")
                    .and_then(|property| parse_transform(property, diagnostics)),
            };
            Some(Keyframe { offset, style })
        })
        .collect::<Vec<_>>();
    stops.sort_by(|left, right| left.offset.total_cmp(&right.offset));
    if stops.len() < 2 {
        diagnostics.push(
            Diagnostic::new(
                format!("@keyframes {} requires at least two stops", raw.name),
                raw.span,
            )
            .with_help("add from { ... } and to { ... } stops"),
        );
        return None;
    }
    for pair in stops.windows(2) {
        if (pair[0].offset - pair[1].offset).abs() < f32::EPSILON {
            diagnostics.push(Diagnostic::new(
                format!(
                    "@keyframes {} contains duplicate {}% stops",
                    raw.name,
                    pair[0].offset * 100.0
                ),
                raw.span,
            ));
        }
    }
    Some(Keyframes {
        name: raw.name,
        stops,
    })
}

fn parse_keyframe_offset(
    value: &str,
    span: SourceSpan,
    diagnostics: &mut Vec<Diagnostic>,
) -> Option<f32> {
    match value {
        "from" => Some(0.0),
        "to" => Some(1.0),
        _ => {
            let Some(percent) = value.strip_suffix('%') else {
                diagnostics.push(
                    Diagnostic::new("keyframe selectors must be from, to, or a percentage", span)
                        .with_help("examples: from, 50%, or to"),
                );
                return None;
            };
            match percent.parse::<f32>() {
                Ok(value) if value.is_finite() && (0.0..=100.0).contains(&value) => {
                    Some(value / 100.0)
                }
                _ => {
                    diagnostics.push(Diagnostic::new(
                        "keyframe percentage must be from 0% through 100%",
                        span,
                    ));
                    None
                }
            }
        }
    }
}

fn lower_node(
    raw: RawBlock,
    font_names: &BTreeMap<String, SourceSpan>,
    animation_names: &BTreeSet<String>,
    diagnostics: &mut Vec<Diagnostic>,
) -> Option<Node> {
    let kind_name = raw.kind.as_str();
    if !matches!(kind_name, "group" | "rect" | "text" | "image") {
        diagnostics.push(
            Diagnostic::new(format!("unknown object type '@{}'", raw.kind), raw.span)
                .with_help("supported scene objects are @group, @rect, @text, and @image"),
        );
        return None;
    }
    if matches!(kind_name, "rect" | "text" | "image") && !raw.children.is_empty() {
        diagnostics.push(
            Diagnostic::new(
                format!("@{kind_name} objects cannot contain children"),
                raw.span,
            )
            .with_help("use an @group container when nesting objects"),
        );
    }
    let properties = collect_properties(raw.properties, diagnostics);
    let text_allowed = [
        "content",
        "font-family",
        "font-size",
        "font-weight",
        "line-height",
        "color",
        "text-align",
        "white-space",
    ];
    let mut allowed = COMMON_STYLE_PROPERTIES.to_vec();
    if kind_name == "text" {
        allowed.extend(text_allowed);
    } else if kind_name == "image" {
        allowed.extend(["src", "object-fit"]);
    }
    reject_unknown(&properties, &allowed, diagnostics);
    let style = lower_style(&properties, animation_names, diagnostics);
    let kind = match kind_name {
        "group" => NodeKind::Group,
        "rect" => NodeKind::Rect,
        "text" => NodeKind::Text(lower_text(&properties, font_names, raw.span, diagnostics)?),
        "image" => NodeKind::Image(lower_image(&properties, raw.span, diagnostics)?),
        other => {
            diagnostics.push(
                Diagnostic::new(format!("unknown object type '@{other}'"), raw.span)
                    .with_help("supported scene objects are @group, @rect, @text, and @image"),
            );
            return None;
        }
    };
    let children = raw
        .children
        .into_iter()
        .filter_map(|child| lower_node(child, font_names, animation_names, diagnostics))
        .collect();
    Some(Node {
        name: raw.name,
        kind,
        style,
        children,
    })
}

const COMMON_STYLE_PROPERTIES: &[&str] = &[
    "position",
    "display",
    "flex-direction",
    "left",
    "top",
    "right",
    "bottom",
    "width",
    "height",
    "min-width",
    "max-width",
    "min-height",
    "max-height",
    "padding",
    "gap",
    "align-items",
    "justify-content",
    "background",
    "opacity",
    "overflow",
    "border-radius",
    "transform",
    "animation",
    "mm-scroll-direction",
    "mm-scroll-range",
    "mm-scroll-duration",
];

fn collect_properties(
    properties: Vec<RawProperty>,
    diagnostics: &mut Vec<Diagnostic>,
) -> BTreeMap<String, RawProperty> {
    let mut collected = BTreeMap::new();
    for property in properties {
        if let Some(previous) = collected.get(&property.name) {
            let previous: &RawProperty = previous;
            diagnostics.push(
                Diagnostic::new(
                    format!("duplicate property '{}'", property.name),
                    property.name_span,
                )
                .with_help(format!(
                    "the first declaration starts at byte {}",
                    previous.name_span.start
                )),
            );
        } else {
            collected.insert(property.name.clone(), property);
        }
    }
    collected
}

#[allow(clippy::too_many_lines)]
fn lower_style(
    properties: &BTreeMap<String, RawProperty>,
    animation_names: &BTreeSet<String>,
    diagnostics: &mut Vec<Diagnostic>,
) -> Style {
    let mut style = Style {
        position: match properties
            .get("position")
            .map(|property| property.value.as_str())
        {
            None => Position::Flow,
            Some("absolute") => Position::Absolute,
            Some(_) => {
                let property = &properties["position"];
                diagnostics.push(Diagnostic::new(
                    "position must be 'absolute' when specified",
                    property.value_span,
                ));
                Position::Flow
            }
        },
        display: parse_display(properties, diagnostics),
        left: properties
            .get("left")
            .and_then(|property| parse_signed_length(property, diagnostics)),
        top: properties
            .get("top")
            .and_then(|property| parse_signed_length(property, diagnostics)),
        right: properties
            .get("right")
            .and_then(|property| parse_signed_length(property, diagnostics)),
        bottom: properties
            .get("bottom")
            .and_then(|property| parse_signed_length(property, diagnostics)),
        ..Style::default()
    };
    if properties.contains_key("left") && properties.contains_key("right") {
        diagnostics.push(Diagnostic::new(
            "the initial MMFX subset does not allow both 'left' and 'right'",
            properties["right"].name_span,
        ));
    }
    if properties.contains_key("top") && properties.contains_key("bottom") {
        diagnostics.push(Diagnostic::new(
            "the initial MMFX subset does not allow both 'top' and 'bottom'",
            properties["bottom"].name_span,
        ));
    }
    if let Some(property) = properties.get("width")
        && let Some(value) = parse_box_length(property, diagnostics)
    {
        style.width = value;
    }
    if let Some(property) = properties.get("height")
        && let Some(value) = parse_box_length(property, diagnostics)
    {
        style.height = value;
    }
    style.min_width = properties
        .get("min-width")
        .and_then(|property| parse_length(property, diagnostics));
    style.max_width = properties
        .get("max-width")
        .and_then(|property| parse_length(property, diagnostics));
    style.min_height = properties
        .get("min-height")
        .and_then(|property| parse_length(property, diagnostics));
    style.max_height = properties
        .get("max-height")
        .and_then(|property| parse_length(property, diagnostics));
    if let Some(property) = properties.get("padding")
        && let Some(value) = parse_length(property, diagnostics)
    {
        style.padding = value;
    }
    if let Some(property) = properties.get("gap")
        && let Some(value) = parse_length(property, diagnostics)
    {
        style.gap = value;
    }
    if let Some(property) = properties.get("align-items") {
        style.align_items = match property.value.as_str() {
            "start" | "flex-start" => AlignItems::Start,
            "center" => AlignItems::Center,
            "end" | "flex-end" => AlignItems::End,
            "stretch" => AlignItems::Stretch,
            _ => {
                diagnostics.push(Diagnostic::new(
                    "align-items must be start, center, end, or stretch",
                    property.value_span,
                ));
                AlignItems::Start
            }
        };
    }
    if let Some(property) = properties.get("justify-content") {
        style.justify_content = match property.value.as_str() {
            "start" | "flex-start" => JustifyContent::Start,
            "center" => JustifyContent::Center,
            "end" | "flex-end" => JustifyContent::End,
            "space-between" => JustifyContent::SpaceBetween,
            _ => {
                diagnostics.push(Diagnostic::new(
                    "justify-content must be start, center, end, or space-between",
                    property.value_span,
                ));
                JustifyContent::Start
            }
        };
    }
    if let Some(property) = properties.get("background")
        && let Some(value) = parse_color(property, diagnostics)
    {
        style.background = value;
    }
    if let Some(property) = properties.get("opacity")
        && let Some(value) = parse_opacity(property, diagnostics)
    {
        style.opacity = value;
    }
    if let Some(property) = properties.get("overflow") {
        style.overflow = match property.value.as_str() {
            "visible" => Overflow::Visible,
            "hidden" => Overflow::Hidden,
            _ => {
                diagnostics.push(
                    Diagnostic::new(
                        "overflow must be 'visible' or 'hidden'",
                        property.value_span,
                    )
                    .with_help("try overflow: hidden;"),
                );
                Overflow::Visible
            }
        };
    }
    if let Some(property) = properties.get("border-radius")
        && let Some(value) = parse_length(property, diagnostics)
    {
        style.border_radius = value;
    }
    if let Some(property) = properties.get("transform")
        && let Some(value) = parse_transform(property, diagnostics)
    {
        style.transform = value;
    }
    if let Some(property) = properties.get("animation") {
        style.animation = parse_animation(property, animation_names, diagnostics);
    }
    style.scroll = parse_scroll(properties, diagnostics);
    style
}

fn parse_display(
    properties: &BTreeMap<String, RawProperty>,
    diagnostics: &mut Vec<Diagnostic>,
) -> Display {
    let display = properties
        .get("display")
        .map(|property| property.value.as_str());
    let direction = properties
        .get("flex-direction")
        .map(|property| property.value.as_str());
    match (display, direction) {
        (None | Some("overlay"), None) => Display::Overlay,
        (Some("row"), None) | (Some("flex"), Some("row") | None) => Display::Row,
        (Some("column"), None) | (Some("flex"), Some("column")) => Display::Column,
        (None | Some("overlay" | "row" | "column"), Some(_)) => {
            let property = &properties["flex-direction"];
            diagnostics.push(Diagnostic::new(
                "flex-direction requires display: flex",
                property.value_span,
            ));
            Display::Overlay
        }
        (Some(_), _) => {
            let property = &properties["display"];
            diagnostics.push(Diagnostic::new(
                "display must be overlay, row, column, or flex",
                property.value_span,
            ));
            Display::Overlay
        }
    }
}

fn lower_text(
    properties: &BTreeMap<String, RawProperty>,
    font_names: &BTreeMap<String, SourceSpan>,
    fallback_span: SourceSpan,
    diagnostics: &mut Vec<Diagnostic>,
) -> Option<TextContent> {
    let Some(content_property) = properties.get("content") else {
        diagnostics.push(Diagnostic::new(
            "@text requires a 'content' property",
            fallback_span,
        ));
        return None;
    };
    let Some(family_property) = properties.get("font-family") else {
        diagnostics.push(Diagnostic::new(
            "@text requires a 'font-family' property",
            fallback_span,
        ));
        return None;
    };
    let content = parse_string(content_property, diagnostics)?;
    let font_family = parse_identifier_or_string(family_property, diagnostics)?;
    if !font_names.contains_key(&font_family) {
        diagnostics.push(
            Diagnostic::new(
                format!("font family '{font_family}' has not been declared"),
                family_property.value_span,
            )
            .with_help(format!(
                "add @font {font_family} {{ src: \"path/to/font.ttf\"; }} to the scene"
            )),
        );
    }
    let font_size = properties.get("font-size").map_or(Some(16.0), |property| {
        parse_pixel_length(property, "font-size", diagnostics)
    })?;
    let font_weight = properties
        .get("font-weight")
        .map_or(Some(400.0), |property| {
            parse_bounded_number(property, "font-weight", 1.0..=1000.0, diagnostics)
        })?;
    let line_height = properties
        .get("line-height")
        .map_or(Some(TextLineHeight::Relative(1.2)), |property| {
            parse_line_height(property, diagnostics)
        })?;
    let color = properties
        .get("color")
        .map_or(Some(Color::rgba(0, 0, 0, u8::MAX)), |property| {
            parse_color(property, diagnostics)
        })?;
    let align = match properties
        .get("text-align")
        .map(|property| property.value.as_str())
    {
        None | Some("start" | "left") => TextAlign::Start,
        Some("center") => TextAlign::Center,
        Some("end" | "right") => TextAlign::End,
        Some(_) => {
            let property = &properties["text-align"];
            diagnostics.push(Diagnostic::new(
                "text-align must be start, center, end, left, or right",
                property.value_span,
            ));
            TextAlign::Start
        }
    };
    let wrap = match properties
        .get("white-space")
        .map(|property| property.value.as_str())
    {
        None | Some("normal") => TextWrap::Wrap,
        Some("nowrap") => TextWrap::NoWrap,
        Some(_) => {
            let property = &properties["white-space"];
            diagnostics.push(Diagnostic::new(
                "white-space must be 'normal' or 'nowrap'",
                property.value_span,
            ));
            TextWrap::Wrap
        }
    };
    Some(TextContent {
        content,
        font_family,
        font_size,
        font_weight,
        line_height,
        color,
        align,
        wrap,
    })
}

fn lower_image(
    properties: &BTreeMap<String, RawProperty>,
    fallback_span: SourceSpan,
    diagnostics: &mut Vec<Diagnostic>,
) -> Option<ImageContent> {
    let Some(source_property) = properties.get("src") else {
        diagnostics.push(Diagnostic::new(
            "@image requires a 'src' property",
            fallback_span,
        ));
        return None;
    };
    let source = parse_string(source_property, diagnostics)?;
    let fit = match properties
        .get("object-fit")
        .map(|property| property.value.as_str())
    {
        None | Some("contain") => ObjectFit::Contain,
        Some("cover") => ObjectFit::Cover,
        Some("fill" | "stretch") => ObjectFit::Fill,
        Some(_) => {
            diagnostics.push(Diagnostic::new(
                "object-fit must be contain, cover, or fill",
                properties["object-fit"].value_span,
            ));
            ObjectFit::Contain
        }
    };
    Some(ImageContent { source, fit })
}

fn parse_animation(
    property: &RawProperty,
    animation_names: &BTreeSet<String>,
    diagnostics: &mut Vec<Diagnostic>,
) -> Option<Animation> {
    let parts = property.value.split_whitespace().collect::<Vec<_>>();
    if !(2..=3).contains(&parts.len()) {
        diagnostics.push(
            Diagnostic::new(
                "animation requires a name, duration, and optional timing function",
                property.value_span,
            )
            .with_help("example: animation: enter 12f ease-out;"),
        );
        return None;
    }
    let name = parts[0].to_owned();
    if !animation_names.contains(&name) {
        diagnostics.push(
            Diagnostic::new(
                format!("animation references unknown @keyframes '{name}'"),
                property.value_span,
            )
            .with_help(format!(
                "add @keyframes {name} {{ from {{ ... }} to {{ ... }} }}"
            )),
        );
    }
    let duration = parse_duration(parts[1], property.value_span, diagnostics)?;
    let timing = parts.get(2).map_or(Some(TimingFunction::Ease), |value| {
        parse_timing(value, property.value_span, diagnostics)
    })?;
    Some(Animation {
        name,
        duration,
        timing,
    })
}

fn parse_scroll(
    properties: &BTreeMap<String, RawProperty>,
    diagnostics: &mut Vec<Diagnostic>,
) -> Option<Scroll> {
    let direction = properties.get("mm-scroll-direction");
    let duration = properties.get("mm-scroll-duration");
    let range = properties.get("mm-scroll-range");
    if direction.is_none() && duration.is_none() && range.is_none() {
        return None;
    }
    if range.is_some_and(|property| property.value != "cover") {
        diagnostics.push(Diagnostic::new(
            "the initial mm-scroll-range must be 'cover'",
            range.expect("checked").value_span,
        ));
    }
    let Some(direction) = direction else {
        diagnostics.push(Diagnostic::new(
            "mm-scroll-direction is required when scrolling",
            range
                .or(duration)
                .map_or(SourceSpan::default(), |property| property.name_span),
        ));
        return None;
    };
    let direction = match direction.value.as_str() {
        "block-start" => ScrollDirection::BlockStart,
        "block-end" => ScrollDirection::BlockEnd,
        "inline-start" => ScrollDirection::InlineStart,
        "inline-end" => ScrollDirection::InlineEnd,
        _ => {
            diagnostics.push(Diagnostic::new(
                "mm-scroll-direction must be block-start, block-end, inline-start, or inline-end",
                direction.value_span,
            ));
            return None;
        }
    };
    let Some(duration) = duration else {
        diagnostics.push(Diagnostic::new(
            "mm-scroll-duration is required when scrolling",
            direction_span(properties),
        ));
        return None;
    };
    Some(Scroll {
        direction,
        duration: parse_duration(&duration.value, duration.value_span, diagnostics)?,
    })
}

fn direction_span(properties: &BTreeMap<String, RawProperty>) -> SourceSpan {
    properties
        .get("mm-scroll-direction")
        .map_or(SourceSpan::default(), |property| property.name_span)
}

fn parse_duration(
    value: &str,
    span: SourceSpan,
    diagnostics: &mut Vec<Diagnostic>,
) -> Option<AnimationDuration> {
    if value == "scene" {
        return Some(AnimationDuration::Scene);
    }
    let Some(frames) = value.strip_suffix('f') else {
        diagnostics.push(
            Diagnostic::new("animation durations must use frames or 'scene'", span)
                .with_help("examples: 12f or scene"),
        );
        return None;
    };
    match frames.parse::<u32>() {
        Ok(frames) if frames > 0 => Some(AnimationDuration::Frames(frames)),
        _ => {
            diagnostics.push(Diagnostic::new(
                "frame duration must be a positive whole number",
                span,
            ));
            None
        }
    }
}

fn parse_timing(
    value: &str,
    span: SourceSpan,
    diagnostics: &mut Vec<Diagnostic>,
) -> Option<TimingFunction> {
    match value {
        "linear" => Some(TimingFunction::Linear),
        "ease" => Some(TimingFunction::Ease),
        "ease-in" => Some(TimingFunction::EaseIn),
        "ease-out" => Some(TimingFunction::EaseOut),
        "ease-in-out" => Some(TimingFunction::EaseInOut),
        _ => {
            diagnostics.push(Diagnostic::new(
                "timing must be linear, ease, ease-in, ease-out, or ease-in-out",
                span,
            ));
            None
        }
    }
}

fn parse_string(property: &RawProperty, diagnostics: &mut Vec<Diagnostic>) -> Option<String> {
    let value = property.value.as_str();
    let Some(delimiter) = value
        .chars()
        .next()
        .filter(|character| matches!(character, '\'' | '"'))
    else {
        diagnostics.push(
            Diagnostic::new("expected a quoted string", property.value_span)
                .with_help("wrap the value in double quotes"),
        );
        return None;
    };
    if value.len() < 2 || !value.ends_with(delimiter) {
        diagnostics.push(Diagnostic::new(
            "unterminated quoted string",
            property.value_span,
        ));
        return None;
    }
    let mut output = String::new();
    let mut characters = value[delimiter.len_utf8()..value.len() - delimiter.len_utf8()].chars();
    while let Some(character) = characters.next() {
        if character == delimiter {
            diagnostics.push(Diagnostic::new(
                "unescaped quote inside string",
                property.value_span,
            ));
            return None;
        }
        if character != '\\' {
            output.push(character);
            continue;
        }
        let Some(escaped) = characters.next() else {
            diagnostics.push(Diagnostic::new(
                "string ends with an incomplete escape",
                property.value_span,
            ));
            return None;
        };
        match escaped {
            'n' => output.push('\n'),
            'r' => output.push('\r'),
            't' => output.push('\t'),
            '\\' => output.push('\\'),
            '\'' => output.push('\''),
            '"' => output.push('"'),
            _ => {
                diagnostics.push(Diagnostic::new(
                    format!("unsupported string escape '\\{escaped}'"),
                    property.value_span,
                ));
                return None;
            }
        }
    }
    Some(output)
}

fn parse_identifier_or_string(
    property: &RawProperty,
    diagnostics: &mut Vec<Diagnostic>,
) -> Option<String> {
    if property.value.starts_with(['\'', '"']) {
        return parse_string(property, diagnostics);
    }
    if !property.value.is_empty() && property.value.chars().all(is_ident_character) {
        Some(property.value.clone())
    } else {
        diagnostics.push(
            Diagnostic::new(
                "font-family must be an identifier or quoted family name",
                property.value_span,
            )
            .with_help("quote font family names containing spaces"),
        );
        None
    }
}

fn parse_pixel_length(
    property: &RawProperty,
    name: &str,
    diagnostics: &mut Vec<Diagnostic>,
) -> Option<f32> {
    match parse_length(property, diagnostics) {
        Some(Length::Pixels(value)) if value > 0.0 => Some(value),
        Some(_) => {
            diagnostics.push(Diagnostic::new(
                format!("{name} must be a positive px length"),
                property.value_span,
            ));
            None
        }
        None => None,
    }
}

fn parse_bounded_number(
    property: &RawProperty,
    name: &str,
    range: std::ops::RangeInclusive<f32>,
    diagnostics: &mut Vec<Diagnostic>,
) -> Option<f32> {
    match property.value.parse::<f32>() {
        Ok(value) if value.is_finite() && range.contains(&value) => Some(value),
        _ => {
            diagnostics.push(Diagnostic::new(
                format!(
                    "{name} must be a number from {} through {}",
                    range.start(),
                    range.end()
                ),
                property.value_span,
            ));
            None
        }
    }
}

fn parse_line_height(
    property: &RawProperty,
    diagnostics: &mut Vec<Diagnostic>,
) -> Option<TextLineHeight> {
    if property.value.ends_with("px") {
        return parse_pixel_length(property, "line-height", diagnostics)
            .map(TextLineHeight::Pixels);
    }
    parse_bounded_number(property, "line-height", 0.1..=20.0, diagnostics)
        .map(TextLineHeight::Relative)
}

fn required_scene_dimension(
    name: &str,
    properties: &BTreeMap<String, RawProperty>,
    fallback_span: SourceSpan,
    diagnostics: &mut Vec<Diagnostic>,
) -> Option<u32> {
    let Some(property) = properties.get(name) else {
        diagnostics.push(Diagnostic::new(
            format!("@scene requires a '{name}' property"),
            fallback_span,
        ));
        return None;
    };
    let Some(Length::Pixels(value)) = parse_length(property, diagnostics) else {
        diagnostics.push(Diagnostic::new(
            format!("scene {name} must use px units"),
            property.value_span,
        ));
        return None;
    };
    if value.fract() != 0.0 || !(1.0..=1_000_000.0).contains(&value) {
        diagnostics.push(Diagnostic::new(
            format!("scene {name} must be 1 through 1000000 whole pixels"),
            property.value_span,
        ));
        None
    } else {
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        Some(value as u32)
    }
}

fn parse_length(property: &RawProperty, diagnostics: &mut Vec<Diagnostic>) -> Option<Length> {
    parse_length_value(property, false, diagnostics)
}

fn parse_box_length(property: &RawProperty, diagnostics: &mut Vec<Diagnostic>) -> Option<Length> {
    if property.value.trim() == "auto" {
        Some(Length::Auto)
    } else {
        parse_length(property, diagnostics)
    }
}

fn parse_signed_length(
    property: &RawProperty,
    diagnostics: &mut Vec<Diagnostic>,
) -> Option<Length> {
    parse_length_value(property, true, diagnostics)
}

fn parse_length_value(
    property: &RawProperty,
    signed: bool,
    diagnostics: &mut Vec<Diagnostic>,
) -> Option<Length> {
    let value = property.value.trim();
    let (number, unit) = if let Some(number) = value.strip_suffix("px") {
        (number, "px")
    } else if let Some(number) = value.strip_suffix('%') {
        (number, "%")
    } else if value == "0" {
        (value, "px")
    } else {
        diagnostics.push(
            Diagnostic::new("lengths require px or % units", property.value_span)
                .with_help("examples: 24px or 50%"),
        );
        return None;
    };
    match number.trim().parse::<f32>() {
        Ok(number) if number.is_finite() && (signed || number >= 0.0) => Some(if unit == "px" {
            Length::Pixels(number)
        } else {
            Length::Percent(number)
        }),
        _ => {
            diagnostics.push(Diagnostic::new(
                if signed {
                    "length must be a finite number"
                } else {
                    "length must be a finite non-negative number"
                },
                property.value_span,
            ));
            None
        }
    }
}

fn parse_color(property: &RawProperty, diagnostics: &mut Vec<Diagnostic>) -> Option<Color> {
    let Some(hex) = property.value.strip_prefix('#') else {
        diagnostics.push(
            Diagnostic::new("colors must use hexadecimal notation", property.value_span)
                .with_help("examples: #2a3344 or #2a3344cc"),
        );
        return None;
    };
    let expanded = match hex.len() {
        3 | 4 => hex
            .chars()
            .flat_map(|character| [character, character])
            .collect::<String>(),
        6 | 8 => hex.to_owned(),
        _ => {
            diagnostics.push(Diagnostic::new(
                "colors must contain 3, 4, 6, or 8 hexadecimal digits",
                property.value_span,
            ));
            return None;
        }
    };
    let parse_channel = |start| u8::from_str_radix(&expanded[start..start + 2], 16);
    if let (Ok(red), Ok(green), Ok(blue), Ok(alpha)) = (
        parse_channel(0),
        parse_channel(2),
        parse_channel(4),
        if expanded.len() == 8 {
            parse_channel(6)
        } else {
            Ok(u8::MAX)
        },
    ) {
        Some(Color::rgba(red, green, blue, alpha))
    } else {
        diagnostics.push(Diagnostic::new(
            "color contains a non-hexadecimal digit",
            property.value_span,
        ));
        None
    }
}

fn parse_opacity(property: &RawProperty, diagnostics: &mut Vec<Diagnostic>) -> Option<u16> {
    match property.value.parse::<f32>() {
        Ok(value) if value.is_finite() && (0.0..=1.0).contains(&value) =>
        {
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            Some((value * f32::from(u16::MAX)).round() as u16)
        }
        _ => {
            diagnostics.push(Diagnostic::new(
                "opacity must be a number from 0 through 1",
                property.value_span,
            ));
            None
        }
    }
}

fn parse_transform(property: &RawProperty, diagnostics: &mut Vec<Diagnostic>) -> Option<Transform> {
    let mut transform = Transform::default();
    let mut remaining = property.value.trim();
    while !remaining.is_empty() {
        let Some(open) = remaining.find('(') else {
            return invalid_transform(property, diagnostics);
        };
        let name = remaining[..open].trim();
        let arguments_and_tail = &remaining[open + 1..];
        let Some(close) = arguments_and_tail.find(')') else {
            return invalid_transform(property, diagnostics);
        };
        let arguments = arguments_and_tail[..close].trim();
        remaining = arguments_and_tail[close + 1..].trim();
        match name {
            "translate" => {
                let Some((x, y)) = arguments.split_once(',') else {
                    diagnostics.push(Diagnostic::new(
                        "translate requires two comma-separated lengths",
                        property.value_span,
                    ));
                    return None;
                };
                transform.translate_x = parse_transform_length(x, property, diagnostics)?;
                transform.translate_y = parse_transform_length(y, property, diagnostics)?;
            }
            "translateX" => {
                transform.translate_x = parse_transform_length(arguments, property, diagnostics)?;
            }
            "translateY" => {
                transform.translate_y = parse_transform_length(arguments, property, diagnostics)?;
            }
            "scale" => {
                let (x, y) = arguments
                    .split_once(',')
                    .map_or((arguments, arguments), |(x, y)| (x, y));
                transform.scale_x = parse_scale(x, property, diagnostics)?;
                transform.scale_y = parse_scale(y, property, diagnostics)?;
            }
            "rotate" => {
                let Some(degrees) = arguments.strip_suffix("deg") else {
                    diagnostics.push(Diagnostic::new(
                        "rotate angles require deg units",
                        property.value_span,
                    ));
                    return None;
                };
                transform.rotate_degrees = match degrees.trim().parse::<f32>() {
                    Ok(value) if value.is_finite() => value,
                    _ => {
                        diagnostics.push(Diagnostic::new(
                            "rotate angle must be finite",
                            property.value_span,
                        ));
                        return None;
                    }
                };
            }
            _ => return invalid_transform(property, diagnostics),
        }
    }
    Some(transform)
}

fn parse_transform_length(
    value: &str,
    property: &RawProperty,
    diagnostics: &mut Vec<Diagnostic>,
) -> Option<Length> {
    parse_signed_length(
        &RawProperty {
            name: "transform".to_owned(),
            name_span: property.name_span,
            value: value.trim().to_owned(),
            value_span: property.value_span,
        },
        diagnostics,
    )
}

fn parse_scale(
    value: &str,
    property: &RawProperty,
    diagnostics: &mut Vec<Diagnostic>,
) -> Option<f32> {
    match value.trim().parse::<f32>() {
        Ok(value) if value.is_finite() && value >= 0.0 => Some(value),
        _ => {
            diagnostics.push(Diagnostic::new(
                "scale must be a finite non-negative number",
                property.value_span,
            ));
            None
        }
    }
}

fn invalid_transform(
    property: &RawProperty,
    diagnostics: &mut Vec<Diagnostic>,
) -> Option<Transform> {
    diagnostics.push(
        Diagnostic::new(
            "transform supports translate, translateX, translateY, scale, and rotate",
            property.value_span,
        )
        .with_help("example: transform: translateY(12px) scale(0.9) rotate(3deg);"),
    );
    None
}

fn reject_unknown(
    properties: &BTreeMap<String, RawProperty>,
    allowed: &[&str],
    diagnostics: &mut Vec<Diagnostic>,
) {
    for (name, property) in properties {
        if allowed.contains(&name.as_str()) {
            continue;
        }
        let mut diagnostic =
            Diagnostic::new(format!("unknown property '{name}'"), property.name_span);
        if let Some(suggestion) = closest_property(name, allowed) {
            diagnostic = diagnostic.with_help(format!("did you mean '{suggestion}'?"));
        }
        diagnostics.push(diagnostic);
    }
}

fn closest_property<'a>(name: &str, candidates: &'a [&str]) -> Option<&'a str> {
    candidates
        .iter()
        .map(|candidate| {
            let common_prefix = name
                .bytes()
                .zip(candidate.bytes())
                .take_while(|(left, right)| left == right)
                .count();
            (*candidate, edit_distance(name, candidate), common_prefix)
        })
        .min_by_key(|(_, distance, common_prefix)| (*distance, usize::MAX - common_prefix))
        .filter(|(_, distance, _)| *distance <= 3)
        .map(|(candidate, _, _)| candidate)
}

fn edit_distance(left: &str, right: &str) -> usize {
    let mut previous = (0..=right.len()).collect::<Vec<_>>();
    for (left_index, left_byte) in left.bytes().enumerate() {
        let mut current = vec![left_index + 1];
        for (right_index, right_byte) in right.bytes().enumerate() {
            current.push(if left_byte == right_byte {
                previous[right_index]
            } else {
                1 + previous[right_index]
                    .min(previous[right_index + 1])
                    .min(current[right_index])
            });
        }
        previous = current;
    }
    previous[right.len()]
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::{parse_scene, parse_scene_with_bindings};
    use crate::{
        AlignItems, AnimationDuration, Color, Display, Length, NodeKind, ObjectFit, Overflow,
        ParameterKind, ParameterValue, ScrollDirection, TextAlign, TextLineHeight, TextWrap,
        TimingFunction,
    };

    const SOURCE: &str = r"
        @scene title-card {
            width: 640px;
            height: 360px;
            background: #102030;

            @group card {
                position: absolute;
                display: overlay;
                left: 10%;
                bottom: 24px;
                width: 80%;
                height: 96px;
                overflow: hidden;
                transform: translate(2px, 0);

                @rect panel {
                    width: 100%;
                    height: 100%;
                    background: #e83a72cc;
                    border-radius: 12px;
                }
            }
        }
    ";

    #[test]
    fn parses_typed_nested_scene() {
        let scene = parse_scene(SOURCE).expect("valid scene");
        assert_eq!(scene.name, "title-card");
        assert_eq!((scene.width, scene.height), (640, 360));
        assert_eq!(scene.background, Color::rgba(0x10, 0x20, 0x30, 0xff));
        let group = &scene.children[0];
        assert_eq!(group.kind, NodeKind::Group);
        assert_eq!(group.style.left, Some(Length::Percent(10.0)));
        assert_eq!(group.style.overflow, Overflow::Hidden);
        assert_eq!(group.children[0].kind, NodeKind::Rect);
        assert_eq!(
            group.children[0].style.background,
            Color::rgba(0xe8, 0x3a, 0x72, 0xcc)
        );
    }

    #[test]
    fn parses_intrinsic_box_sizes_and_constraints() {
        let scene = parse_scene(
            "@scene x { width: 640px; height: 360px; @group stack { width: auto; \
             height: auto; min-width: 80px; max-width: 50%; min-height: 12px; } }",
        )
        .expect("valid intrinsic sizing");
        let style = &scene.children[0].style;
        assert_eq!(style.width, Length::Auto);
        assert_eq!(style.height, Length::Auto);
        assert_eq!(style.min_width, Some(Length::Pixels(80.0)));
        assert_eq!(style.max_width, Some(Length::Percent(50.0)));
        assert_eq!(style.min_height, Some(Length::Pixels(12.0)));
    }

    #[test]
    fn reports_unknown_property_with_source_and_suggestion() {
        let source = "@scene x { width: 10px; height: 10px; @rect r { widht: 5px; } }";
        let errors = parse_scene(source).expect_err("typo must fail validation");
        let error = errors
            .iter()
            .find(|error| error.message.contains("widht"))
            .expect("unknown property error");
        assert_eq!(error.span.line_column(source), (1, 49));
        assert_eq!(error.help.as_deref(), Some("did you mean 'width'?"));
    }

    #[test]
    fn accumulates_independent_validation_errors() {
        let source = "@scene bad { width: 20%; @rect r { opacity: 2; mystery: yes; } }";
        let errors = parse_scene(source).expect_err("invalid scene must fail");
        assert!(errors.len() >= 3, "errors were {errors:#?}");
        assert!(errors.iter().any(|error| error.message.contains("height")));
        assert!(errors.iter().any(|error| error.message.contains("opacity")));
        assert!(errors.iter().any(|error| error.message.contains("mystery")));
    }

    #[test]
    fn rejects_children_inside_rectangles() {
        let source = "@scene x { width: 10px; height: 10px; @rect r { @rect child {} } }";
        let errors = parse_scene(source).expect_err("rect nesting must fail");
        assert!(
            errors
                .iter()
                .any(|error| error.message.contains("cannot contain children"))
        );
    }

    #[test]
    fn parses_explicit_font_and_typed_text() {
        let source = r#"
            @scene x {
                width: 320px;
                height: 180px;
                @font Inter { src: "fonts/Inter.ttf"; }
                @text title {
                    content: "Cut; don't encode.";
                    font-family: Inter;
                    font-size: 32px;
                    font-weight: 650;
                    line-height: 1.25;
                    color: #f0f4f8;
                    text-align: center;
                    white-space: nowrap;
                }
            }
        "#;
        let scene = parse_scene(source).expect("valid text scene");
        assert_eq!(scene.fonts[0].name, "Inter");
        assert_eq!(scene.fonts[0].source, "fonts/Inter.ttf");
        let NodeKind::Text(text) = &scene.children[0].kind else {
            panic!("expected text node");
        };
        assert_eq!(text.content, "Cut; don't encode.");
        assert_eq!(text.font_family, "Inter");
        assert!((text.font_size - 32.0).abs() < f32::EPSILON);
        assert!((text.font_weight - 650.0).abs() < f32::EPSILON);
        assert_eq!(text.line_height, TextLineHeight::Relative(1.25));
        assert_eq!(text.color, Color::rgba(0xf0, 0xf4, 0xf8, 0xff));
        assert_eq!(text.align, TextAlign::Center);
        assert_eq!(text.wrap, TextWrap::NoWrap);
    }

    #[test]
    fn types_parameters_and_applies_host_bindings() {
        let source = r#"
            @param --title { type: text; default: "Default title"; }
            @param --accent { type: color; default: #f00; }
            @param --size { type: length; default: 24px; }
            @param --visible { type: boolean; default: true; }
            @param --align { type: choice; default: center; choices: "start, center, end"; }
            @scene x {
                width: 320px; height: 180px;
                @font Inter { src: "Inter.ttf"; }
                @text title {
                    width: auto; height: auto; content: var(--title);
                    font-family: Inter; font-size: var(--size); color: var(--accent);
                    text-align: var(--align);
                }
            }
        "#;
        let bindings = BTreeMap::from([
            ("title".into(), "Bound title".into()),
            ("accent".into(), "#42d6c7".into()),
            ("size".into(), "31px".into()),
            ("align".into(), "end".into()),
        ]);
        let scene = parse_scene_with_bindings(source, &bindings).expect("typed bindings");
        assert_eq!(scene.parameters.len(), 5);
        let title = scene
            .parameters
            .iter()
            .find(|parameter| parameter.name == "title")
            .unwrap();
        assert_eq!(title.kind, ParameterKind::Text);
        assert_eq!(title.value, ParameterValue::Text("Bound title".into()));
        let NodeKind::Text(text) = &scene.children[0].kind else {
            panic!("expected text");
        };
        assert_eq!(text.content, "Bound title");
        assert_eq!(text.font_size, 31.0);
        assert_eq!(text.color, Color::rgba(0x42, 0xd6, 0xc7, 0xff));
        assert_eq!(text.align, TextAlign::End);
    }

    #[test]
    fn rejects_unknown_and_mistyped_parameter_bindings() {
        let source = "@param --accent { type: color; default: #f00; } \
            @scene x { width: 1px; height: 1px; background: var(--accent); }";
        let errors = parse_scene_with_bindings(
            source,
            &BTreeMap::from([
                ("accent".into(), "blue".into()),
                ("missing".into(), "1".into()),
            ]),
        )
        .expect_err("invalid bindings");
        assert!(
            errors
                .iter()
                .any(|error| error.message.contains("hexadecimal"))
        );
        assert!(
            errors
                .iter()
                .any(|error| error.message.contains("no matching"))
        );
    }

    #[test]
    fn rejects_undeclared_text_font() {
        let source = r#"@scene x { width: 10px; height: 10px;
            @text t { content: "x"; font-family: Missing; }
        }"#;
        let errors = parse_scene(source).expect_err("font must be declared");
        assert!(
            errors
                .iter()
                .any(|error| error.message.contains("has not been declared"))
        );
    }

    #[test]
    fn parses_flow_images_keyframes_and_scrolling() {
        let source = r#"
            @scene motion {
                width: 320px;
                height: 180px;
                @group row {
                    display: flex;
                    flex-direction: row;
                    padding: 12px;
                    gap: 8px;
                    align-items: center;
                    justify-content: space-between;
                    animation: enter 12f ease-out;
                    @image logo {
                        width: 80px;
                        height: 60px;
                        src: "logo.png";
                        object-fit: contain;
                    }
                    @rect ticker {
                        width: 40px;
                        height: 20px;
                        mm-scroll-direction: inline-start;
                        mm-scroll-range: cover;
                        mm-scroll-duration: scene;
                    }
                }
            }
            @keyframes enter {
                from { opacity: 0; transform: translateY(20px) scale(0.9) rotate(-2deg); }
                50% { opacity: 0.8; }
                to { opacity: 1; transform: translateY(0) scale(1) rotate(0deg); }
            }
        "#;
        let scene = parse_scene(source).expect("Scene 0.2 syntax");
        let row = &scene.children[0];
        assert_eq!(row.style.display, Display::Row);
        assert_eq!(row.style.align_items, AlignItems::Center);
        let animation = row.style.animation.as_ref().expect("animation");
        assert_eq!(animation.duration, AnimationDuration::Frames(12));
        assert_eq!(animation.timing, TimingFunction::EaseOut);
        assert_eq!(scene.animations[0].stops.len(), 3);
        let NodeKind::Image(image) = &row.children[0].kind else {
            panic!("expected image");
        };
        assert_eq!(image.source, "logo.png");
        assert_eq!(image.fit, ObjectFit::Contain);
        assert_eq!(
            row.children[1].style.scroll.expect("scroll").direction,
            ScrollDirection::InlineStart
        );
        assert!(scene.is_animated());
        assert_eq!(scene.image_sources(), vec!["logo.png"]);
    }
}
