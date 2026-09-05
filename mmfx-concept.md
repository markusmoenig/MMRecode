# MMFX Concept

## Purpose

MMFX is MMRecode's portable scene, layout, animation, transition, and visual-effect system. It is
intended to cover two different authoring needs without confusing their execution models:

- Artists describe text, rectangles, images, media, groups, layout, styling, and animation through
  a strict CSS-shaped scene language.
- Effect authors implement custom generators, filters, and transitions through a safe bounded
  kernel language.

Both languages belong to one module system and share typed parameters, resources, time, color,
diagnostics, packaging, and versioning. They compile to separate typed intermediate
representations which meet in MMRecode's render graph.

This document records the intended direction. Syntax examples illustrate semantics and remain
subject to adjustment while the parser, typed scene model, and reference renderer are built.

## Documentation contract

Every executable MMFX syntax, semantic, editor-integration, or rendering change must update the
public reference under `docs/docs/mmfx/` in the same change. Visual features should also add or
update a checked-in scene under `examples/mmfx/` and CPU-reference output under
`docs/static/img/mmfx/`. Documentation examples are runnable acceptance material, not speculative
syntax; future-only designs remain in this concept document and must be labeled as such.

## Core decisions

1. **Scene authoring is CSS-shaped rather than a general-purpose programming language.** Layout,
   styling, typography, and ordinary animation are declarative tasks. Familiar CSS property names
   and behavior are retained wherever their semantics fit deterministic video rendering.
2. **MMFX is not a web browser.** The initial profile excludes the global cascade, selector
   specificity, a DOM, scripting, floats, browser compatibility behavior, and silent recovery from
   unknown properties.
3. **Custom pixel work uses a separate kernel language.** Scene authors can use built-in effects
   without reading or writing kernels. Kernel code is available when a genuinely new image
   operation is required.
4. **CPU execution defines correctness.** Scene evaluation, layout, text shaping, and scalar
   reference rendering run on the CPU. Tiled/SIMD CPU and WGSL/wgpu implementations are optional
   accelerators which consume the same typed IR and are tested against the reference.
5. **Preview and final rendering share semantics.** Preview may explicitly select proxies, reduced
   precision, or a GPU backend, but it does not acquire a separate scene or effect language.
6. **Time, coordinates, color, sampling, alpha, precision, and edge behavior are explicit.** They
   must not be inherited accidentally from a display API or codec pixel format.
7. **The editor supports direct source editing.** Scene and kernel source can replace the contextual
   inspector while the monitor displays either the incrementally compiled real project composition
   or an explicitly selected isolated local context.

## System shape

```text
                       MMFX module
                            │
                    parse and type-check
                     ┌──────┴──────┐
                     │             │
                  Scene IR      Kernel IR
                     │             │
            layout and animation   │
                     │             │
                 display list      │
                     └──────┬──────┘
                            │
       decoded media ──> render graph
                            │
          ┌─────────────────┼─────────────────┐
          │                 │                 │
    scalar CPU         tiled/SIMD CPU      WGSL/wgpu
    reference             optimized         optional
          │                 │                 │
          └─────────────────┴─────────────────┘
                            │
                 composited project frame
```

The scene language does not execute on the GPU. It resolves a semantic object tree into geometry,
glyph runs, image placements, clips, masks, and effect passes. Rasterization, compositing, and
kernels may then use any compatible backend.

## Current executable foundation

The first implementation slices now live in the renderer-independent `mmrecode-mmfx` crate. A
`.mmfx` file is parsed into typed `Scene`, `Group`, `Rect`, `Font`, `Text`, and `Image` values before rendering. The parser
rejects unknown and duplicate declarations, reports byte spans plus line/column locations, and
suggests likely property names. Invalid source never reaches a renderer.

The scalar CPU backend supports an explicit pixel canvas, nested overlay/row/column groups,
absolute left/top/right/bottom anchors, padding, gaps, alignment and justification, `px` and `%`
lengths, translation/scale/rotation, decoded images with contain/cover/fill, sRGBA hexadecimal colors,
opacity, overflow clipping, and rounded rectangles. It composites in linear premultiplied RGBA and
implements true group opacity. Rounded hidden groups also clip their descendants to the rounded
shape. Rectangle, rounded-rectangle, and clip coverage is rasterized through pinned Zeno 0.3.3
with 256-level antialiasing; fractional edges feed coverage directly into the linear compositor.
Static Unicode text uses Parley 0.9 for shaping, bidirectional analysis, wrapping, line metrics, and
alignment, then Swash 0.2.10 with Zeno coverage for hinted glyph rasterization. Exact-frame named
keyframes and cover-style scrolling evaluate in the scene object's source-local time. Scene 0.4
also measures `auto` text, image, and flow-group boxes before placement, applies min/max constraints,
uses the resulting content extent for cover scrolling, and adds typed `@param` declarations with
strict `var(--name)` references and persisted host bindings. Font files are
explicit module resources; the CPU render context disables system fonts so the same project cannot
silently select different export fonts on another machine. The CLI proof is:

```text
mmrecode render-mmfx examples/mmfx/motion-layout.mmfx output.png --frame 23 --frames 60
```

Generated scene objects now own their source inside the project, appear as ordinary hierarchical
timeline placements, and can be edited directly with automatic worker previews and last-valid-frame
retention. A reusable incremental CPU project compositor serves both the monitor and direct-root
MPEG-2/TS export, including FX-only projects and recursively nested placements. It caches parsing,
resources, prepared scenes, static rasterization, placement scaling, transparent bounds,
terminal-size variants, and limited/full-range YUV values. Animated overlays are evaluated lazily
at exact local frame time and kept in a bounded cache. Repeated-frame work performs no parsing,
font/image loading, scene allocation, or image resizing and touches only active prepared pixels.
Decoders remain outside the compositor so interactive hosts can discard obsolete seeks while
export hosts decode sequentially.

This is intentionally narrower than the target profile below. It does not yet define fallback font
chains, color glyphs, text decorations, media slots, named reusable styles, richer timing controls, or
Kernel IR. The current direct YUV delivery blend is the optimized SDR path; the
high-precision linear project-frame path, tiled/SIMD execution, and differential tests remain
required before treating it as the final color pipeline. Those features should extend the typed
boundary rather than bypass it.

## MMFX scene CSS

### Language profile

MMFX Scene is a strict, versioned CSS profile. A module uses nested MMFX rules to create semantic
objects and ordinary CSS-shaped declarations to style and lay them out.

Top-level and nested object rules initially include:

- `@scene`
- `@group`
- `@rect`
- `@text`
- `@image`
- `@media`

The first layout and presentation profile should include:

- Flex-style row and column layout
- Overlay/stack layout
- Absolute positioning
- Width, height, min/max size, and intrinsic size
- Padding, margin, and gap
- Alignment and justification
- Anchors and transform origin
- Two-dimensional transforms
- Opacity
- Overflow clipping
- Background colors and gradients
- Borders and rounded corners
- Image/media `object-fit` behavior
- Font family, weight, size, line height, alignment, and wrapping
- Named styles or mixins without a global selector cascade
- Standard keyframe concepts, easing, delay, iteration count, and fill mode

The first profile deliberately omits:

- General selectors and specificity
- A global style cascade
- Browser DOM concepts
- Floats and browser page-flow behavior
- Media queries
- JavaScript or arbitrary procedural scene code
- Silent acceptance of unknown or misspelled properties

Unknown declarations are diagnostics by default. A misspelling such as `opactiy` should report a
likely correction rather than quietly changing the rendered result.

### Target-profile example scene

```css
@scene lower-third {
    position: absolute;
    left: 64px;
    bottom: 64px;

    display: flex;
    flex-direction: row;
    align-items: stretch;
    gap: 18px;
    padding: 20px;

    opacity: 1;
    animation: enter 12f ease-out;

    @rect accent {
        width: 8px;
        background: var(--accent);
    }

    @group labels {
        display: flex;
        flex-direction: column;
        gap: 5px;

        @text name {
            content: var(--name);
            font: asset("Inter-Bold.ttf");
            font-size: 48px;
        }

        @text role {
            content: var(--role);
            font: asset("Inter-Regular.ttf");
            font-size: 25px;
        }
    }
}

@keyframes enter {
    from {
        opacity: 0;
        transform: translateY(24px);
    }
    to {
        opacity: 1;
        transform: translateY(0);
    }
}
```

Nested object rules define the semantic scene tree. CSS declarations do not construct or mutate a
browser DOM.

### Parameters and reusable scenes

Scene 0.4 exposes initial typed parameters as
`@param --name { type: <kind>; default: <value>; }` and consumes them through a complete-property
`var(--name)` reference. The executable kinds are text, color, length, number, boolean, and an
enumerated choice with a comma-separated `choices` declaration. The broader module schema should
eventually add:

- Integer and bounded scalar values
- Angle and duration values
- Rich text
- Gradients
- Image/media resource references

The codec-independent project record stores canonical binding input alongside the embedded scene;
the MMFX compiler immediately validates and lowers every value to the typed `SceneParameter` model
before preview or export. Terminal commands, future graphical controls, plugins, and source code
therefore operate against one compiler-owned schema without making the edit graph depend on a
rendering backend. A future public interchange API may serialize the typed values directly.

General functions, loops, and mutable state are not part of the initial scene language. Repeated or
data-driven content can initially be produced by composition plugins which emit typed Scene IR.
Restrained component or repetition constructs should be added only after representative scenes
show that they are necessary.

## MMFX extensions to CSS

MMFX-specific properties use the `mm-` prefix. This separates transferable CSS knowledge from
video-specific behavior. MMFX object rules use explicit at-rules such as `@scene` and `@text`.

### Exact media time

MMFX adds an exact frame unit:

```css
animation-duration: 12f;
animation-delay: 3f;
```

Frame values resolve against the containing media's exact time base. Seconds may be used when an
actual duration is intended, but authoritative internal time remains rational. A `scene` keyword
may select the complete local duration:

```css
animation-duration: scene;
```

Evaluation exposes distinct project, scene-local, placement-local, and source-media time domains.
Conversions between them are explicit so moving or trimming a placement cannot silently alter an
animation's intended clock.

### Media slots

A scene does not open arbitrary files or inspect editor internals. `@media` and `@image` objects bind
declared slots or managed resources supplied through the project graph:

```css
@media foreground {
    mm-source: slot("main");
    width: 100%;
    height: 100%;
    object-fit: cover;
}
```

Slots preserve MMRecode's recursive media abstraction and allow the same scene definition to be
reused with different child media.

### Scrolling content

Scrolling is a layout-time motion behavior that applies to any visual box, not a special text
renderer. This supports rolling credits, tickers, logo carousels, and other moving groups.

```css
@scene credits {
    overflow: clip;

    @text body {
        content: var(--credits);
        width: 70%;
        margin-inline: auto;
        font-size: 42px;
        line-height: 1.4;
        text-align: center;

        mm-scroll-direction: block-start;
        mm-scroll-range: cover;
        mm-scroll-duration: scene;
        animation-timing-function: linear;
    }
}
```

Initial scrolling declarations are:

- `mm-scroll-direction`: `block-start`, `block-end`, `inline-start`, or `inline-end`
- `mm-scroll-range`: initially `cover`
- `mm-scroll-duration`: an exact duration or `scene`
- `mm-scroll-speed`: a length per second, mutually exclusive with duration
- `mm-scroll-delay`: an exact duration
- `mm-scroll-iteration-count`: a positive count or `infinite`

Logical directions respect text writing direction. For `cover`, layout is resolved first; the box
starts immediately beyond the relevant clipping edge and ends after it has passed completely beyond
the opposite edge. Travel therefore includes both the resolved viewport extent and content extent.
Duration and speed cannot both be specified. Standard easing and fill-mode semantics apply.

Scrolling lowers to ordinary transforms and animation in Scene IR. It does not require a kernel.

### Effects and transitions

Built-in filters and transitions are usable from scene CSS without exposing kernel code. Their
eventual surface syntax should remain property-oriented and introspectable by the editor. Each
effect invocation resolves to a typed module entry point plus typed parameter bindings.

A custom kernel is required only when a module introduces a new image operation. Effect ranges and
transition progress remain owned by the edit timeline; kernels receive already-resolved inputs and
progress values.

Potential future extensions include title/action safe-area anchors, path-following motion, and
explicit temporal sampling. They should not be added before a concrete scene requires them.

## Kernel language

The kernel language is a small pure language, not general Rust, C, WGSL, or a host scripting API.
Its semantics must map consistently to scalar CPU, SIMD CPU, and WGSL/wgpu.

Exported entry-point categories are:

- Generator: no image input, produces an image
- Filter: one or more image inputs, produces an image
- Transition: two primary image inputs plus timeline-supplied progress
- Internal kernel/pass: implementation detail used by an exported operation

Kernel types distinguish pixel coordinates, normalized texture coordinates, lengths, vectors,
matrices, colors, alpha representation, image format, sampler state, and time. Sampling always has
defined filtering and edge behavior.

The initial safety model includes:

- No pointers or unsafe memory access
- No filesystem, network, process, or unrestricted host calls
- No recursion
- No unbounded loops
- No dynamic allocation inside a pixel kernel
- No mutable global state
- Deterministic pseudo-random values only from explicit seeds and coordinates
- Statically validated resource access and output dimensions
- Explicit upper bounds for parameter-dependent work

Neighborhood effects declare their maximum spatial radius so tiled execution can provide correct
halos. A later temporal-effect model must similarly declare frames required before and after the
current output frame because those dependencies affect decoding, caching, and render planning.

Raw WGSL may be considered later as an explicitly nonportable escape hatch, but it is not part of
the normative MMFX language and cannot define CPU behavior.

## Scene IR, Kernel IR, and render graph

MMFX uses two typed IR domains rather than prematurely lowering every operation to pixels.

Scene IR retains semantic values such as:

- Text runs and font-resource references
- Paths, rectangles, images, media slots, and groups
- Layout constraints and intrinsic sizes
- Transforms, opacity, clips, masks, and blend modes
- Typed parameters and animation tracks
- Exact local timing

At a requested time, scene evaluation performs layout and shaping and emits a display list. The
display list and decoded media frames lower into a render graph containing rasterization,
off-screen surfaces, masks, compositing, color conversion, and kernel passes.

Kernel IR represents typed scalar/vector expressions, sampling, bounded control flow, and pass
interfaces. It should be backend-neutral rather than using WGSL, SPIR-V, or a CPU instruction set as
its semantic definition.

Source remains authoritative. Compiled IR is a cache keyed by source hash, imported-resource hashes,
compiler version, target settings, and relevant project color/precision policy. A stable serialized
IR format is unnecessary until a plugin or deployment boundary proves that it is needed.

## Color, alpha, and precision

General scene composition should use an explicit linear-light, premultiplied-alpha working surface.
Codec YUV is converted at the render boundary and converted back only for delivery. Text and vector
coverage must not inherit the blending behavior of a terminal, GUI toolkit, or codec pixel format.

The first scalar reference backend should use a clearly specified precision and define:

- Color-space conversion
- Transfer functions
- Premultiplication and unpremultiplication
- Clamp and out-of-range behavior
- NaN and infinity behavior
- Rounding
- Permitted or forbidden fused operations
- Deterministic approximations for mathematical built-ins where required

Integer/fixed operations may be bit-exact. Floating-point kernels need a documented reproducibility
class and comparison tolerance unless their math is explicitly defined to be bit-identical across
supported targets.

## CPU text and vector implementation candidates

MMFX owns the normative text, layout, coverage, and compositing semantics, but it should reuse
focused Rust libraries rather than implement OpenType shaping and path rasterization from scratch.
The initial CPU reference path should evaluate the following stack:

- [Parley](https://github.com/linebender/parley) for rich paragraph layout, font selection and
  fallback, bidi resolution, line breaking, alignment, inline boxes, and positioned glyph runs.
- [Swash](https://github.com/dfrg/swash) for font introspection, scalable and color glyph sources,
  variation coordinates, hinting, outline scaling, fractional positioning, and glyph rendering.
- [Zeno](https://docs.rs/zeno/latest/zeno/) for high-quality CPU path coverage, including 256-level
  antialiasing, non-zero/even-odd fills, strokes, joins, caps, and stable dashing.

The intended data flow is:

```text
MMFX rich text
      ↓
Parley paragraph layout
      ↓
positioned glyph runs
      ↓
Swash glyph scaling/rendering ──> Zeno-backed alpha masks
                                          │
MMFX paths and rectangles ──────> Zeno alpha masks
                                          │
                                          ↓
                         MMFX linear premultiplied compositor
```

Swash is intentionally low-level and lists full text layout and composition as non-goals. It is a
strong glyph engine, not by itself the complete MMFX text system. Parley fills the paragraph-layout
layer; MMFX still owns CSS property resolution, box layout, animation, clipping, color policy,
resource identity, and final composition.

These crates are implementation candidates rather than MMFX's public semantic contract. They must
pass MMRecode's independent conformance corpus before becoming the reference path, and project
files must not serialize their private Rust types.

### Final-output antialiasing policy

High-quality output depends on policy as much as the rasterizer:

- Final video uses grayscale coverage masks, not display-specific LCD RGB subpixel masks. The
  eventual playback display, scaling, rotation, and chroma subsampling are unknown.
- Coverage is converted to the working precision and composited in linear light with premultiplied
  alpha. An 8-bit coverage mask must not force the working surface itself to 8-bit precision.
- Fractional glyph placement is retained. Hinting, vertical quantization, and fractional-offset
  quantization are explicit typography settings and cache-key inputs.
- Animated, rotated, or scaled text should be rasterized from transformed outlines at the required
  output scale where practical rather than repeatedly transforming a low-resolution cached mask.
- Optional supersampling is available for demanding transforms, very small type, and reference
  validation; it must use a specified reconstruction filter.
- Glyph caches are keyed by font content hash, face index, glyph identifier, size, variation
  coordinates, hinting policy, transform/scale class, fractional offset, mask format, and relevant
  renderer version.
- Color glyph layers and embedded bitmaps are converted through MMFX's color/alpha policy rather
  than composited with library or display defaults.

Final deterministic rendering uses project-managed or embedded font files with recorded hashes and
an explicit fallback chain. System font discovery may assist interactive authoring, but a system
font selected for final output must be resolved into a durable project resource or reported as a
nonportable dependency.

The first evaluation should compare Zeno/Swash output against independently rendered reference
images across small and large text, fractional placement, complex scripts, variable fonts, color
glyphs, strokes, transforms, and scrolling motion. Particular attention should be paid to mask
precision, edge stability during animation, and output consistency across CPU architectures.

## Project and editor integration

The current recursive `MediaProject` is the authoring foundation. It already provides reusable
media definitions, placement links, local timelines, stable identities, and cycle rejection. MMFX
requires the next project-document version to add typed generated content and placement overrides.

A generated media definition should be able to contain either a typed scene document or an MMFX
module instance. A module instance records:

- A managed/embedded module reference
- Content hash and language version
- Exported scene, effect, transition, or generator entry point
- Typed parameter bindings
- Declared resource bindings

Visual placement links eventually add transform, anchor, opacity, blend mode, crop/mask, and typed
instance overrides alongside the existing timing and scale behavior.

The flattened render intent replaces string-valued effect and transition parameters with typed
module references and bindings. Any active visual effect forces decoding and regeneration over its
affected range. Declared temporal dependencies can extend that range and its decode prerequisites.

### Direct source editing

The terminal editor can switch its contextual inspector among inspection, scene source, kernel
source, parameters, and diagnostics while retaining the monitor, timeline, result area, and command
prompt. The source pane and hierarchy path are authoring context only: `cd` never changes monitor
scope implicitly. `monitor project` is the default and renders from the project root, decoding the
underlying media at the project playhead and compositing the edited scene where its placement is
active. `monitor local` explicitly isolates the current context and its descendants on a
transparency-revealing checkerboard when no local video is present; `monitor toggle` switches the
two modes. The hierarchy timeline continues to use local time, and its playhead maps exactly to and
from the separate project playhead across placement ranges and rational time bases. Monitor scope is
editor-session state, not project content. A later full-screen source view can use the same editor
buffer and preview state.

The live compilation loop is:

```text
source edit
    ↓
short debounce
    ↓
parse, type-check, and compile on a worker
    ├── success: atomically install a new draft generation,
    │            invalidate affected preview frames, and redraw
    └── failure: retain the last valid preview and publish
                 source-spanned diagnostics
```

Temporary syntax errors must not blank the monitor. Diagnostics include source span, severity,
message, related locations, and suggested corrections where available.

Source edits participate directly in project undo/redo alongside module references and parameter
bindings. Draft preview does not require saving, but ordinary project save persists the current
source even when it contains a temporary diagnostic. Opening the user's external editor remains a
useful optional escape hatch.

The first executable integration implements this loop on hierarchical timeline objects. `add scene`
creates generated media with embedded starter Scene source and a placement in the current local
timeline. `cd` selects that object and contextual `edit` opens its source in the right-hand pane
(`scene edit` is an alias). Every source edit updates the project object and project history;
ordinary project `save` serializes the source.
`scene load` replaces it with an embedded copy of an external module while retaining that module's
directory as the relative-resource base, and `scene save as` only extracts a copy. Compilation is
debounced and coalesced on a worker. Source-spanned failures are shown without replacing the last
successful monitor frame. `help`, `man edit`, and `man scene` are the normative interactive discovery
surface.

The command vocabulary distinguishes content from processing. `scene` creates and manages a
declarative, duration-bearing timeline object. `fx` is reserved for applying filters, generators,
transitions, and their optional kernel implementations to timeline objects or boundaries. Legacy
`add fx` and `fx load/save/close` remain compatibility aliases for early projects and scripts.

In project-monitor mode, MMFX placements recursively nested below the root participate in preview
regardless of the current `cd` path. In local-monitor mode, only the current media definition and its
descendants participate. FX-only projects retain the same custom-pixel timeline renderer used with
decoded media. The project monitor renders a black project canvas beneath active scenes; the local
monitor uses a checkerboard when there is no local video. With a decoded video preview, active
scenes are alpha-composited over the underlying frame in the selected scope. The same flattened time
mapping drives the local hierarchy playhead, monitor, and MPEG-2/TS export, including ancestor trims
and different local frame rates. Static scene/resource pixels remain cached across hierarchy and
monitor-scope changes.
Generated starter scenes contain a named text element and use the bundled deterministic
`builtin:inter` resource.

## Modules and plugins

An MMFX module manifest eventually declares:

- Language and module version
- Exported entry points and their categories
- Typed parameters, defaults, bounds, and editor hints
- Required resource slots
- Spatial and temporal dependencies
- Required precision and backend capabilities
- Determinism/reproducibility claim
- Imported module and asset hashes

Built-in modules may be implemented in Rust but expose the same semantic entry-point and parameter
model. Third-party modules cross a versioned protocol boundary through validated MMFX source/IR,
sandboxed WASM/WASI, or an external process; they do not receive mutable access to editor internals.

Composition generators such as a Markdown-to-video plugin produce typed scene objects and media
placements. They need not manufacture CSS source, although generated objects should remain
inspectable and editable through the same semantic model.

### Built-in Markdown composition generator

MMRecode should include a Markdown composition generator because it makes substantial amounts of
structured text easy to author while preserving headings, paragraphs, code, media, and diagrams as
semantic project content. It is called a renderer from the user's perspective, but architecturally
it is an authoring-time generator upstream of MMFX Scene:

```text
Markdown source
      ↓
built-in Markdown plugin
      ↓
document AST
      ↓
typed composition fragment
├── media definitions and placement links
├── scene documents
├── text/image/media objects
├── named semantic style roles
├── source mappings
├── managed resource requests
└── diagnostics and dependencies
      ↓
ordinary Scene IR and render graph
```

The Markdown plugin does not rasterize text or bypass MMFX color and composition. Representative
lowerings include:

- Headings and paragraphs to rich text objects
- Images to managed image or media objects
- Video/audio references to media placements where explicitly supported
- Code blocks to rich text with semantic syntax spans
- Rules and simple tables to rectangles, groups, and text
- Diagrams to paths/text or a nested semantic-object generator
- Sections to nested scenes, slides, chapters, or timeline placements according to an explicit
  generation profile
- A complete document to a scrolling group using `mm-scroll-*` when rolling-document output is
  selected

Markdown does not define presentation time. The invocation therefore supplies an explicit profile
such as one scrolling scene, one scene per top-level heading, one slide per section, or externally
provided durations. The generated timeline is inspectable before it is committed.

Generated nodes reference named style roles such as `markdown.heading-1`, `markdown.paragraph`, and
`markdown.code`. An MMFX CSS theme defines those roles without requiring a general selector
cascade. Restyling the theme therefore changes the complete document while its semantic structure
remains intact.

The generator supports two explicit ownership modes:

- **Linked:** the Markdown source and plugin invocation remain authoritative. Source edits
  regenerate the derived composition, and source mappings let selection navigate back to the
  corresponding Markdown range. User changes are retained only through documented overrides.
- **Materialized:** generation runs once and the result becomes ordinary independently editable
  MMRecode media and scene objects. It no longer follows the Markdown source.

A linked result must never drift into an ambiguous half-generated state. Detaching/materializing is
an explicit undoable operation. The project records plugin identity and version, input and resource
hashes, options, source mappings, and the last successful typed output snapshot. If the plugin is
missing later, MMRecode can continue inspecting and rendering that snapshot while reporting that
regeneration is unavailable. A newer plugin version never silently regenerates an existing project.

The built-in implementation may call internal Rust traits directly, but it obeys the same logical
input/output and determinism contract intended for third-party composition generators. It receives
declared source/resources and produces typed values; it does not receive unrestricted mutable
editor state.

## Verification

MMFX correctness needs evidence at every lowering boundary:

- Parser and diagnostic tests with exact source spans
- Type-checking tests for units, colors, time domains, images, and parameters
- Scene validation and serialization round trips
- Golden layout geometry independent of rasterization
- Text shaping and font-resource golden tests
- Scalar CPU pixel goldens
- Tile-size and halo tests, including awkward dimensions and large radii
- Differential scalar/SIMD/GPU comparisons under the declared precision contract
- Alpha, clipping, blend, gradient, sampling, and edge-mode vectors
- Transition endpoint tests: progress zero and one reproduce the required inputs
- Scroll tests covering intrinsic content size, viewport size, writing direction, speed, duration,
  clipping, and exact first/last positions
- Render-plan tests showing which ranges require copy, decode, effects, and re-encoding
- Markdown generator tests for AST lowering, semantic style roles, source mappings, linked
  regeneration, materialization, version pinning, and last-good snapshot fallback
- Resource-limit and malicious-source tests

Every discovered discrepancy becomes a permanent regression vector. Preview quality reductions are
tested as explicit modes rather than accepted as undocumented differences.

## Initial implementation slices

### Slice 1: typed scene boundary

- [x] Add renderer-independent scene values, object types, units, transforms, bounded layout,
  animation, and validation.
- [x] Add typed generated-content records to the project model and its versioned serialization.
- [x] Add typed placement presentation properties needed by the first composition.

### Slice 2: scalar scene renderer

- [x] Introduce a linear premultiplied working surface.
- [x] Render groups, rectangles, decoded images, opacity, transforms, and clipping.
- [x] Implement row, column, overlay, and absolute layout.
- [x] Recursively compile generated MMFX media into preview and MPEG-2/TS project rendering.
- [ ] Add live media slots to the render graph.

### Slice 3: text and scrolling

- [x] Pin Parley/Swash for text, reusing the pinned Zeno coverage backend behind
  MMFX-owned text and path interfaces.
- [x] Add controlled font resources, shaping, wrapping, glyph coverage, and compositing.
- [ ] Add deterministic fallback font chains and color glyph policy.
- [x] Implement exact local animation time and keyframes.
- [x] Implement the initial `mm-scroll-*` cover profile.
- [x] Prove titles, subtitles, and a horizontal ticker in checked-in examples and output frames.
- [x] Add a complete rolling-credits example driven by intrinsic text and group sizing.

### Slice 4: CSS source and live editor

- [x] Parse and type-check the strict scene profile into the typed scene model.
- [x] Surface source-spanned diagnostics in the editor.
- [x] Embed a multiline code editor in the inspector area for internal and file-backed sources.
- [x] Debounce/coalesce compilation and retain the last-good preview.
- [x] Add an initial MMFX-aware syntax highlighter to the terminal source editor and highlighted website examples.
- [x] Add typed public scene parameters, persisted bindings, editor commands, completion, and cache invalidation.
- [ ] Add compiler-driven semantic metadata and targeted frame-cache invalidation.

### Slice 5: built-in Markdown generator

- Parse Markdown into a retained document AST with exact source mappings.
- Lower headings, paragraphs, images, code, rules, and sections to typed composition fragments.
- Add named MMFX CSS theme roles and explicit scrolling/section/slide timing profiles.
- Implement linked regeneration, explicit materialization, version pinning, and last-good snapshots.

### Slice 6: kernel reference path

- Define typed Kernel IR and the safe source subset.
- Implement the scalar CPU interpreter/compiler.
- Prove pointwise color effects and deterministic generators.
- Connect typed effects to `ApplyEffects` and explainable full-render planning.

### Slice 7: transitions and neighborhoods

- Implement dissolve and wipe transitions.
- Implement a separable blur to prove multipass scheduling, bounded sampling, and tile halos.
- Add spatial/temporal capability declarations to module metadata and planning.

### Slice 8: acceleration and plugin boundary

- Add tiled and SIMD CPU backends with differential testing.
- Add optional WGSL/wgpu preview lowering from Kernel IR and compatible display-list operations.
- Define the module manifest, caching, capability, and third-party protocol boundary.

## First end-to-end proof

The first compelling project should:

1. Import video through the existing editor.
2. Add an MMFX lower third containing a rectangle and shaped text.
3. Edit the CSS source beside the moving monitor and see a last-good live preview.
4. Animate its position and opacity using exact frame durations.
5. Add rolling credits using `mm-scroll-*`.
6. Apply one scalar CPU color effect.
7. Transition between two clips with a wipe.
8. Export through the CPU-authoritative render path.
9. Explain which timeline intervals were packet-preserved and which required full rendering.
10. Generate an equivalent styled title/credits composition from linked Markdown and materialize a
    copy for direct scene editing.

This slice exercises the authoring graph, persistence, CSS profile, typography, layout, time,
effects, transitions, color, planning, preview, and final rendering without depending on a specific
new codec implementation.

## Deferred decisions

The following should remain explicit design questions until implementation evidence is available:

- Named-style syntax and broader parameter editor metadata
- Whether restrained component/repetition syntax belongs in Scene CSS
- The first normative working-surface precision
- Font packaging, fallback, and substitution policy
- Whether Parley, Swash, and Zeno pass the normative text/vector corpus unchanged or require narrow
  adapters, fixes, or a separately maintained reference path
- Whether float math is cross-platform bit-exact or tolerance-class deterministic
- The point at which compiled IR requires durable serialization
- Whether a raw backend-specific shader escape hatch is ever justified
- Which terminal editor widget best matches MMRecode's key and diagnostic model
