# MMRecode Concept

## Purpose

MMRecode is an experimental, professional-quality media-codec and editing ecosystem written in
Rust.

The project asks a practical engineering question:

> How much trustworthy, professional codec and media infrastructure can one experienced codec
> architect build today when AI coding agents perform a large part of the implementation work?

MMRecode is initially a professional-interest and open-source project, not a company. It does not
need an immediate commercial wedge or a commitment to reproduce an entire commercial codec SDK.
Useful support, integration, long-term maintenance, analysis, or archival products may emerge
later, but monetization is an option rather than a condition for beginning.

## Perspective

Professional codec software is not defined only by compression efficiency or by whether a test
file plays. It must remain compatible, diagnosable, reproducible, maintainable, and supportable
over long operational lifetimes.

Broadcast and archival installations are deliberately conservative. Working systems may remain
in production for decades. Consequently, older production formats such as Motion JPEG, DV, and
MPEG-2 remain relevant even when newer distribution codecs exist.

MMRecode starts with these durable formats. This creates a technically progressive path and
produces useful components before the project reaches the complexity of AVC, HEVC, AV1, or VVC.

## Codec progression

The intended progression is:

```text
Motion JPEG
    ↓
DV family
    ↓
MPEG-2 Video
    ↓
H.264 / AVC
    ↓
HEVC, AV1, VVC, and other formats as interest justifies
```

Each step introduces another class of engineering problem.

### Motion JPEG

Motion JPEG establishes:

- Bit-level parsing and writing
- DCT, quantization, and entropy coding
- Raw frame and pixel-format representations
- Color conversion and sampling conventions
- Frame- and field-based variants
- Independent frame verification
- Container integration
- Selective copying and frame-local re-encoding

### DV

DV adds:

- Fixed-size compressed frames
- DIF sequences and block organization
- PAL, NTSC, and professional DV-family variants
- Chroma-layout differences
- Embedded and shuffled audio
- Timecode and recording metadata
- Error detection, concealment, and damaged-media behavior
- Archival and camera-file workflows

### MPEG-2 Video

MPEG-2 introduces inter-frame system behavior:

- I, P, and B pictures
- Motion estimation and compensation
- Decode-order versus presentation-order handling
- Open and closed GOPs
- Reference-picture dependencies
- VBV conformance
- Frame and field prediction
- Smart rendering through bridge GOPs

### H.264 and later codecs

H.264 tests whether the architecture survives substantially more complex prediction, transforms,
entropy coding, reference-picture management, random access, and conformance rules. It should be
attempted after the shared substrate has proved useful with earlier codecs, not used to design a
large theoretical framework in advance.

## Core principles

### Rust first

Implement codec and media logic in safe Rust wherever practical. Unsafe code, architecture-
specific SIMD, GPU integration, and assembly must be isolated behind narrow acceleration
interfaces and tested against portable reference paths.

Rust is not itself the product. The benefits sought are explicit ownership, memory safety,
predictable deployment, strong types, portable libraries, and maintainability over long product
lifetimes.

### Modular rather than monolithic

MMRecode is not intended to become another FFmpeg-style monolith.

Codecs, containers, quality analysis, editing, and tools remain independently usable components.
An application needing only DV decoding should not acquire MP4, H.264, an editing timeline, or a
command-line application.

The modular boundary is behavioral, not merely a collection of Cargo packages:

- Codecs do not know about containers.
- Containers transport encoded packets without implementing codecs.
- Editing describes user intent independently of codec syntax.
- Rendering converts edits into copy, rewrite, decode, effect, and encode operations.
- Quality and verification utilities are not hidden inside one encoder.

### Verification is a product property

Correctness cannot be inferred from successful playback.

Every implemented feature should acquire automated, independently checkable evidence. Depending
on the component, this includes:

- Syntax and profile conformance
- Decoder interoperability
- Pixel-exact reconstruction checks
- Independent reference-decoder comparison
- Encoder internal reconstruction versus external decoding
- Round-trip container checks
- Timestamp and ordering verification
- Fuzzing and mutation testing
- Pathological and damaged streams
- PSNR, SSIM, and later perceptual metrics
- Bitrate and rate-distortion regression tests
- Performance and allocation regression tests
- Permanent regression coverage for every discovered failure

The verification infrastructure should eventually be user-facing:

```text
mmrecode inspect input.m2v
mmrecode verify input.mov
mmrecode compare reference.y4m candidate.y4m
mmrecode benchmark corpus.toml
```

### Verification-friendly AI development

AI coding agents may implement modules, tests, optimizations, documentation, and mechanical
integration. Human engineering remains responsible for:

- Architecture and module boundaries
- Interpretation of standards
- Algorithm and quality decisions
- Test independence
- Performance acceptance
- Security and correctness review
- Deciding whether generated code is maintainable

Modules should have narrow responsibilities and strong contracts so generated work can be tested
without trusting the agent that produced it.

Productivity is measured in validated functionality per human engineering hour, not generated
lines of code.

### Professional lifecycle

Professional compatibility includes behavior across releases. MMRecode should develop practices
for:

- Deterministic encoding modes
- Serialized, versioned configurations
- Explicit API and output-behavior changes
- Reproducible builds and test corpora
- Long-lived regression vectors
- Diagnostic errors with byte, picture, and syntax context
- Compatibility modes where operationally justified
- Stable ABI distributions only after the Rust APIs mature

## Containers and media workflows

Container support is part of the ecosystem but remains separate from codec implementation.

Likely formats include:

- YUV4MPEG2 for early uncompressed tests
- AVI for Motion JPEG and DV workflows
- ISO Base Media File Format for MP4 and QuickTime/MOV
- MPEG-2 Transport Stream for broadcast and delivery
- MXF for professional production and archives
- Raw codec elementary streams where applicable

Muxers and demuxers must preserve timestamps, opaque codec configuration, side data, metadata,
and unknown data when possible. A container must not decode codec syntax merely to move packets.

## Editing and minimal recompression

MMRecode may grow into a modular editing and rendering substrate rather than a desktop editor.

The central idea is a codec-aware render planner:

```text
timeline or edit request
          ↓
codec dependency analysis
          ↓
affected-region propagation
          ↓
render plan
   ├── copy encoded packets
   ├── rewrite timestamps or headers
   ├── bridge-encode a boundary region
   ├── render an affected frame range
   └── fully encode when required
```

Motion JPEG and DV provide simple independent-frame cases: unchanged frames can generally be
copied while modified frames are re-encoded. MPEG-2 adds GOP dependencies and bridge encoding.
H.264 and later codecs add more complex reference graphs and random-access semantics.

The planner should be explainable. A future command might report:

```text
Pictures 0–738: copied unchanged
Pictures 739–766: bridge-encoded
Reason: retained picture 742 depends on discarded reference picture 735
Pictures 767–2140: copied unchanged
Copied encoded payload: 98.4%
```

Editing was intentionally excluded from the first codec milestone. The initial codec-independent
edit model and render planner now establish the boundary: edit descriptions express intent, codec
analyzers expose dependencies, and render planning decides what can be copied and what must be
reconstructed. Broader editing behavior should grow on this boundary rather than leaking timeline
semantics into codecs or containers.

The first end-to-end delivery path now applies that separation to MPEG-2 Video plus optional MPEG-1
Layer II audio in MPEG-TS. Container planning is inspectable before bytes are written, and a caller
must explicitly choose whether a fractional audio-frame boundary is rejected, ends inside the
video, or covers it with one final complete frame. This is a deliberately narrow policy proof; it
does not replace future sample-domain audio editing.

## Terminal-first editor

The first serious MMRecode editor should be a terminal-first, command-driven media editor with an
integrated visual and audio preview. It is not merely a collection of transcoding commands. The
project is the root media timeline, and linked media recursively establish further local timelines.
The hierarchy is therefore the media composition itself, not a filesystem of bins, conventional
video/audio tracks, or artificial folders such as `Main`, `Background`, or `Titles`.
The same concise commands must also run from a project/script file. Long FFmpeg-style option lists
may exist for diagnostics and automated integration tests, but they are not the primary authoring
interface.

Every media kind participates in the same recursive abstraction: source video, audio, still images,
text, shapes, generators, masks, effects, and compound compositions. A media link places a child
inside its parent's local time and carries instance-specific source range, timeline range,
transform, and overrides. The underlying media has stable identity and may be linked more than
once. A navigable path identifies one placement context rather than claiming filesystem ownership.

The interaction model resembles a small shell over this linked media graph:

```text
mmrecode edit

pwd
/

ls
Clip0     |---------- video ----------|
Music     |~~~~~~~~~~~~ audio ~~~~~~~~~~~~~|
EndTitle  |                            [text]|

cd Clip0
pwd
/Clip0

ls
Title       |      [text]       |
ColorGrade  |===================|
Mask        |       [mask]      |

info
in +12f
play around cursor
```

`cd` follows media-placement links. `ls` visualizes the child media in the current media's local
timeline rather than listing folders. The prompt is a breadcrumb through link aliases, for example
`Film > Clip0 > Title`. Commands without an explicit target operate in this current local time and
composition context. Fully qualified paths make scripts deterministic.

A user can move recursively into any media object, inspect it, modify it, and return to its parent:

```text
cd Clip0
text add "Hallo" as Title at 12:12 for 4s
cd Title
rect add as Background left=5% bottom=6% width=40% height=12% \
    fill=#101018e8 radius=12
fade in 8f
```

There is no separate track object required by the authoring abstraction: ordered child links are
the local timeline and composition order. Render compilation may derive typed audio/video/scene
lanes internally without imposing them on the user. Source-media registration and relinking can
still have a separate browser mode, but it is not the primary editing hierarchy.

Familiar navigation commands such as `ls`, `cd`, `pwd`, `tree`, and `info` should be complemented by
media commands such as `add`, `in`, `out`, `play`, `seek`, `split`, `ripple`, `keyframe`, `render`,
and `explain`. Mutating commands participate in undoable transactions so a multi-command edit can
be committed or reverted as one operation. Human-friendly aliases may be accepted interactively,
while command history and saved projects retain stable media and link identifiers.

Each navigation step establishes local presentation time. Child timing is relative to its parent;
by default children follow the parent's presentation time when that placement moves. Source-time
mapping remains explicit so trims cannot silently change whether an attachment follows presentation
or original source time. Composition cycles are invalid even though reusable media may have several
acyclic placements.

Time notation must be explicit and frame-accurate. Depending on context, the editor may accept
timecode (`00:00:12:12`), seconds plus frames (`12s+12f`), absolute frames (`312f`), decimal media
time (`12.480s`), and relative expressions (`+8f` or `start+12f`). The resolved time base and any
rounding must remain visible rather than becoming an implicit user-interface decision.

### One command model, multiple frontends

The terminal language must not become the editor's internal API. Commands are parsed into a typed,
versionable edit-command model which operates on the same edit sequence used by rendering:

```text
terminal command ─┐
natural language ─┼──> typed edit commands ──> linked media graph ──> render intent/planner
graphical timeline┘
```

This permits a future graphical timeline, scripting API, and natural-language assistant to be
different frontends for identical operations. The graphical editor should emit typed commands
directly rather than constructing shell strings. Natural-language requests should likewise compile
to inspectable commands and present ambiguous or destructive interpretations before applying them.
A session can therefore be replayed, diffed, automated, tested, or shared without depending on the
frontend that created it.

The first implemented command slice establishes this boundary in `mmrecode-edit`. `MediaProject`
stores stable media definitions and timed placement links; `MediaPath` traverses placement context;
and `EditorSession` applies typed navigation, add, trim, undo, and redo commands. Both
`mmrecode edit` and `mmrecode edit <script>` call the same parser/session implementation. Project
persistence, source import, graph-to-render compilation, and terminal preview remain explicit next
slices rather than hidden behavior in this prototype.

### Preview and render transparency

The terminal editor should provide real moving-picture and synchronized-audio preview. Where
available, native terminal image protocols can deliver full-color frames efficiently; a detached
preview window and simpler textual fallbacks keep the editor useful across terminals. Preview is a
consumer of the same media graph, effects, clock, and color rules as final rendering, although it
may deliberately select lower-resolution or proxy processing for responsiveness.

Codec-aware behavior should be visible during editing. Commands such as `explain` should report
which areas will be copied, timestamp-rewritten, bridge-encoded, or fully rendered and why. This
makes minimal recompression an understandable property of an edit rather than a surprising export
optimization.

The initial editor scope should remain focused:

- Frame-accurate cuts, trims, splits, ordering, and ripple operations
- Text, rectangles, images, groups, anchors, alignment, transforms, and opacity
- Effect ranges, fades, and keyframes
- Audio levels, fades, playback, and synchronization
- Terminal preview and optional detached visual preview
- Undo, redo, transactions, history, diffing, and project persistence
- Render-plan explanation and flattening through the modular renderer

More elaborate compositing, collaborative workflows, interactive-video authoring, and a graphical
timeline can follow after this command model proves expressive and pleasant in real editing work.
Interactive or dynamic content may remain live in MMRecode projects and be flattened to conventional
codec-independent video for services such as YouTube.

### CPU-authoritative effects and typography

Final rendering should prioritize reproducible quality over meeting a real-time frame deadline. A
future MMFX language should compile to a typed portable effect IR with a scalar CPU reference
backend. A tiled, multithreaded and SIMD CPU backend can optimize that same behavior while retaining
differential tests against the reference. Large filter radii need explicit tile halos rather than
quietly changing the algorithm at tile boundaries.

Text shaping, layout, vector rasterization, antialiasing, and compositing should have controlled CPU
implementations suitable for high-resolution typography and large-radius effects. Final
compositing should use explicit color spaces and sufficient precision rather than inheriting a
display API's blending behavior.

An optional WGSL/wgpu backend remains valuable for interactive preview and effects that map well to
the GPU. It must consume the same IR and must not define MMFX semantics. Preview may use proxies or
reduced quality; final CPU rendering never has to sacrifice quality to meet a presentation clock.

### Modular authoring plugins

Plugins extend more than codecs and pixel effects. Useful plugin categories include media importers,
composition generators, semantic scene objects, effect modules, codecs, containers, exporters, and
terminal-command extensions. A Markdown-to-video plugin is a representative composition generator:

```text
Markdown source
      ↓
document AST
      ↓
typed edit commands and semantic scene objects
      ↓
ordinary MMRecode preview, editing, and rendering
```

Headings should remain editable text objects, embedded media should become clips, and diagrams or
code blocks should remain structured wherever practical. YouTube delivery flattens the final
composition, but the project remains semantic and editable.

Built-in plugins may use internal Rust traits. Third-party boundaries should use a versioned
manifest and durable data protocol rather than Rust trait-object ABI. Portable sandboxed WASM/WASI
and language-independent external processes are preferred extension mechanisms. Manifests declare
plugin kind, accepted and produced media types, API version, required capabilities, and whether the
plugin claims deterministic output. Plugins produce typed commands, scene nodes, packets, frames,
or other defined values; they do not receive unrestricted mutable access to editor internals.

## Initial experiment

The project should earn continued investment through bounded vertical slices.

### First vertical slice: Motion JPEG

Implement only enough infrastructure to demonstrate a trustworthy end-to-end path:

1. Shared media time, packet, frame, and codec interfaces
2. Bit reader, bit writer, VLC support, and marker parsing
3. Y4M input and output
4. Baseline JPEG/Motion-JPEG parser
5. Constrained decoder
6. Constrained encoder with internal reconstruction
7. Independent decode comparison
8. Frame-level quality and regression reports
9. Basic `encode`, `decode`, `inspect`, and `verify` commands

Do not initially build a production editing GUI, dynamic plugin system, broad container suite,
GPU codec path, or elaborate rate-control system. A narrow visual inspection application is useful
development infrastructure rather than an attempt to build the editor. Keep the experimental C
surface narrow until multiple codecs have exercised its ownership and streaming model.

The first constrained implementation now exercises all nine items with eight-bit baseline
sequential JPEG, planar grayscale and YCbCr sampling, multi-frame raw Motion JPEG streams, and Y4M
test input/output. Its deliberately narrow limits are documented in the repository README; wider
JPEG conformance and performance optimization remain follow-on work rather than hidden assumptions.
An experimental one-shot C boundary now exposes that slice for early integration testing without
promising long-term ABI stability.

The native `mmrecode-viewer` application provides direct visual inspection of decoded frames, raw
component planes, pixel samples, block boundaries, and JPEG structure. It remains above the codec
and container libraries in the dependency graph so UI choices cannot shape normative media APIs.
It now also provides fixed-rate animation and synchronized audio playback. A reusable playback
crate maps exact rational frame rates to media time and accepts the rendered audio position as the
master clock; device handling and temporary third-party MP2 sample decoding remain viewer-local.

### Subsequent vertical slices

The DV25 slice now covers both 525/60 4:1:1 and 625/50 4:2:0 systems: DIF structure, typed packs,
timecode, embedded 16-bit and nonlinear 12-bit audio, video reconstruction, deterministic video and
audio encoding, damage reporting, CLI/viewer/C integration, and independent FFmpeg comparison.

The first MPEG-2 Video slice now covers typed elementary-stream structure, sequence display and
quant matrices, progressive and interlaced Main Profile 4:2:0 frame pictures, I/P/B reconstruction,
deterministic constrained Main Profile/Main Level encoding, open/closed GOP references, clean and
recovery entry points, and explainable bridge-encode propagation. The first generic inter-frame
render planner now consumes that dependency data, reproduces MPEG-2 damage propagation, identifies
decode preroll, and reserves copy and regeneration packet ranges. Its optional MPEG-2 executor now
regenerates a changed region as a closed GOP, preserves unaffected payloads, and validates the
splice. Bridge headers now preserve aspect, display/colour metadata, profile/level, and all luma and
chroma quantizer matrices. GOP timecode is recomputed from the source origin. Bitrate, VBV-buffer,
and picture-delay signalling are preserved only when honest for the reference encoder, otherwise
rewritten and reported. The inter-frame planner and MPEG-2 executor now also
accept frame-aligned ranges from multiple compatible sources: reference-damaged cut boundaries are
regenerated, safe regions return to byte-preserving packet copy, and packet timestamps form one
continuous timeline. Multi-clip audio, field pictures, dual-prime prediction, and production VBV
control remain explicit subsequent work.
The first direct delivery adapter schedules the resulting packets with optional Layer II audio and
drives the MPEG-TS muxer from the same explainable dry-run plan.

The first container slice now covers 188-byte MPEG-2 Transport Stream structure, PAT/PMT program
discovery, PES and 90 kHz timestamp reconstruction, and deterministic single-program MPEG-2 Video
plus optional MPEG-1 Audio Layer II muxing. Audio frames and video pictures are interleaved by exact
timestamps. This proves the intended `Packet` boundary in both directions without coupling H.222.0
systems syntax to either codec. Native MPEG audio sample decoding/encoding, broadcast service tables, live CBR
output, M2TS, seeking, and other container families remain explicit later work.

### Continuation criteria

Continue when:

- AI-assisted implementation remains understandable and reviewable.
- Failures can be reduced to permanent regression tests.
- External decoders confirm output correctness.
- New formats extend rather than destabilize the shared architecture.
- The work remains professionally and intellectually enjoyable.

Reconsider when:

- Most time is spent finding subtle errors in plausible generated code.
- Shared abstractions become dominated by codec-specific exceptions.
- Test independence cannot be maintained.
- The repository accumulates large amounts of code that cannot be confidently supported.

## Possible later applications

Without requiring a new company, useful later outputs may include:

- Open codec libraries
- Supported and reproducibly built SDK distributions
- Long-term-support branches
- Archive ingest, verification, repair, and migration tools
- Frame-accurate and minimal-recompression editing
- Codec and bitstream inspection
- Encoder comparison and rate-distortion analysis
- Hardware codec abstraction
- Customer-specific interoperability work
- C ABI and language bindings
- Certification and conformance assistance

## Licensing

MMRecode initially uses the Apache License, Version 2.0.

Apache-2.0 is permissive enough for proprietary broadcast, archival, and media products to embed
the libraries. It also contains an explicit patent grant from contributors for patent claims they
can license that are necessarily infringed by their contributions.

This source-code license does **not** grant licenses to third-party standard-essential patents.
Patent licensing for JPEG-family formats, DV, MPEG-2, AVC, HEVC, VVC, audio codecs, or container
technologies must be evaluated separately for each use, territory, and distribution model.

The project name and related trademarks are also separate from the source-code license.
