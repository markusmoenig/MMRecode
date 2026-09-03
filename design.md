# MMRecode Architecture

## Status

This document describes the intended architecture and the implemented Motion JPEG, DV25,
MPEG-2 Video, MPEG-2 Transport Stream, and first H.264-in-ISO-BMFF vertical slices. APIs remain
unstable while more containers and editing operations exercise the boundaries.

## Design goals

- Allow applications to select individual codecs and containers.
- Keep codecs independent of containers.
- Keep editing intent independent of encoding syntax.
- Preserve encoded packets without unnecessary decoding or copying.
- Represent exact media time without floating-point timestamps.
- Expose codec dependencies for future smart rendering.
- Make portable safe-Rust implementations the reference behavior.
- Make verification infrastructure reusable across codecs.
- Permit a stable C ABI later without designing the Rust API around C today.

## Non-goals for the initial scaffold

- A complete multimedia framework
- Dynamic runtime codec plugins
- Async I/O throughout the API
- Zero-copy buffers across every possible backend
- A stable public API or ABI
- GPU frame graphs
- A graphical editor
- A universal abstraction over every codec algorithm

## Dependency architecture

Dependencies flow downward only:

```text
                            mmrecode
                               │
                    ┌──────────┴──────────┐
                    │                     │
              mmrecode-edit       mmrecode-quality
                    │
             mmrecode-render
                  ┌─┴───────────┐
                  │             │
             codec crates   container crates
                  │             │
                  └──────┬──────┘
                         │
                  mmrecode-core
                         ▲
                         │
                mmrecode-bitstream
```

The diagram shows conceptual layers; `mmrecode-bitstream` depends on `mmrecode-core` for common
errors, while codec crates depend on both. `mmrecode-core` itself has no MMRecode dependencies.
`mmrecode-viewer`, like the CLI, is an application at the top of the graph and may consume codecs,
containers, playback, and quality tools without becoming a dependency of any codec or container
crate.

Forbidden dependencies include:

- `mmrecode-mjpeg` depending on `mmrecode-avi`
- `mmrecode-isobmff` depending on `mmrecode-h264`
- `mmrecode-core` depending on any implementation crate
- `mmrecode-edit` containing MPEG-2 or H.264 syntax rules
- `mmrecode-quality` being embedded in an encoder implementation

## Repository layout

The current workspace contains the crates exercised by implemented vertical slices:

```text
crates/
├── apps/
│   └── viewer/               mmrecode-viewer; native inspection application
├── core/                    mmrecode-core
├── bitstream/               mmrecode-bitstream
├── codecs/
│   ├── mjpeg/               mmrecode-mjpeg
│   ├── mpegaudio/           mmrecode-mpegaudio; Layer II framing/timing
│   ├── dv/                  mmrecode-dv; raw-DV25 slice
│   ├── mpeg2/               mmrecode-mpeg2; MPEG-2 Video elementary-stream slice
│   └── h264/                mmrecode-h264; AVC syntax/dependency slice
├── containers/
│   ├── isobmff/             mmrecode-isobmff; MP4/MOV sample-table demuxer
│   ├── mpegts/              mmrecode-mpegts; H.222.0 transport slice
│   └── y4m/                 mmrecode-y4m
├── playback/                mmrecode-playback; timeline/clock policy
├── render/                  mmrecode-render; dependency-aware render planning/execution
├── edit/                    mmrecode-edit; authoring graph, commands, and project documents
├── quality/                 mmrecode-quality
├── testkit/                 mmrecode-testkit
├── capi/                    mmrecode-capi; experimental C boundary
└── cli/                     mmrecode; main terminal application
```

Planned crates are added only when implementation begins:

```text
crates/
├── codecs/
│   ├── hevc/                mmrecode-hevc
│   ├── av1/                 mmrecode-av1
│   └── vvc/                 mmrecode-vvc
├── containers/
│   ├── avi/                 mmrecode-avi
│   └── mxf/                 mmrecode-mxf
└── facade/                  optional future package: mmrecode-sdk
```

An empty crate is not created merely to reserve a name. Planned boundaries live in this document
until they have working code and tests.

## `mmrecode-core`

`mmrecode-core` defines vocabulary and interfaces. It must remain small, unsurprising, and free of
codec algorithms or container syntax.

### Time

`Rational` and `Timestamp` represent exact media time:

```rust
pub struct Rational {
    numerator: i64,
    denominator: i64,
}

pub struct Timestamp {
    pub value: i64,
    pub time_base: Rational,
}
```

Floating-point seconds are acceptable for UI display but not as the authoritative representation
of PTS, DTS, frame duration, edit boundaries, or muxer timing.

Future time operations should include checked rescaling and explicit rounding policies. Silent
timestamp rounding is not acceptable at container or edit boundaries.

### Frames

`VideoFrame` owns or references uncompressed pixel planes and carries timing, field order, and
color interpretation.

The scaffold uses owned `Vec<u8>` planes to keep the first API understandable. A later buffer
abstraction may add reference-counted pools, aligned allocations, hardware surfaces, and borrowed
views. Such optimization must not leak backend-specific behavior into codec algorithms.

`AudioFrame` uses owned interleaved sample storage for the same clarity-first reason as
`VideoFrame`. DV is the first consumer because its audio is embedded and physically shuffled among
DIF blocks. Containers must allow audio and data streams without depending on the DV crate.

### Packets

`Packet` is the primary boundary between containers and codecs:

```text
Demuxer → Packet → Decoder → VideoFrame
VideoFrame → Encoder → Packet → Muxer
```

A packet carries:

- Stream identity
- Encoded bytes
- PTS, DTS, and duration
- Key/corruption flags
- Opaque side data

The initial `Vec<u8>` payload favors clarity. Later zero-copy storage must retain the same semantic
boundary.

### Codec descriptors

`CodecDescriptor` contains an extensible codec identifier, optional container tag, media type, and
opaque configuration bytes.

Containers preserve configuration records without interpreting codec semantics. For example:

- ISO-BMFF may carry an `avcC` configuration record.
- AVI may carry a bitmap/video format block.
- MPEG-TS may derive codec identification from PMT descriptors.

The corresponding codec crate interprets the configuration. This prevents a container crate from
depending on every codec it can carry.

### Codec API

Encoder and decoder interfaces use explicit input and output queues:

```rust
pub trait Decoder {
    fn configure(&mut self, descriptor: &CodecDescriptor) -> Result<()>;
    fn send_packet(&mut self, packet: Packet) -> Result<()>;
    fn receive_frame(&mut self) -> Result<Option<VideoFrame>>;
    fn flush(&mut self) -> Result<()>;
}
```

This model handles frame reordering, codec delay, field pairing, packet fragmentation, and draining
more honestly than `decode(packet) -> frame`.

Codec-specific settings remain typed inside each codec crate. `VideoEncoderSettings.options` is a
temporary escape hatch during scaffolding, not the desired long-term public configuration API.

### Container API

Demuxers own their input and produce packets in container order. Muxers accept stream descriptors
and packets, then finalize indexes and trailing metadata.

The first interfaces are synchronous. Streaming muxers must eventually declare whether they need
seekable output. Async applications can initially adapt synchronous components at their boundary;
the entire codec API should not be made async merely because one data source is a network.

## `mmrecode-bitstream`

This crate provides low-level mechanics that are genuinely reusable:

- Most-significant-bit-first readers and writers
- Byte alignment
- VLC table construction and decoding
- MPEG-style start-code scanning
- Marker scanning
- Checked integer and length helpers

It must not contain MPEG-2 picture syntax, JPEG marker semantics, H.264 Exp-Golomb field meanings,
or container box definitions. Those belong to their format crates.

Bit-level errors should eventually report absolute byte/bit offsets and syntax context.

## Codec crates

Each codec crate owns:

- Codec syntax structures
- Parser and serializer
- Decoder
- Encoder
- Reconstruction path
- Codec-specific configuration
- Dependency analyzer
- Conformance rules
- Codec-local acceleration interfaces
- Unit and regression tests

Suggested internal layout:

```text
mmrecode-mpeg2/src/
├── syntax/
├── parser/
├── decoder/
├── encoder/
├── prediction/
├── transform/
├── quantization/
├── entropy/
├── motion/
├── rate_control/
├── dependency/
└── acceleration/
```

The exact module structure should follow the codec. A JPEG crate does not need artificial motion
or rate-control modules simply to resemble MPEG-2.

### Implemented MPEG-2 slice

`mmrecode-mpeg2` keeps MPEG syntax and reconstruction codec-local while exposing generic
`AccessUnitInfo` records to the rest of the workspace. Its current public surface includes typed
sequence/display/quant-matrix, GOP, picture, extension, and slice parsing; presentation/decode
ordering; open/closed-GOP references; portable Main Profile 4:2:0 frame-picture reconstruction;
deterministic Main Profile/Main Level encoding; and an explainable affected-picture smart-render
plan.

The smart-render plan is intentionally not the future generic `mmrecode-render` crate. It proves
the codec-specific propagation rule—including B pictures that precede a changed future reference
in display order and leading B pictures that cross an open GOP—without prematurely defining mux,
timestamp, multi-source timeline, or effect operations.

Current decoder exclusions are field pictures, dual-prime prediction, non-4:2:0 profiles,
scalability extensions, and damaged-slice concealment. The encoder emits frame pictures, closed
GOPs, zero-vector B prediction, and VBR delay signalling; adaptive rate control and a normative VBV
scheduler remain follow-on work. These limits are explicit API errors and documented in the crate
README rather than silent approximations.

Encoder sequence settings separate coding tools from splice metadata. They control aspect ratio,
display/colour description, profile/level, four natural-order quantizer matrices, declared
bitrate/VBV size, and GOP timecode origin. Custom matrices affect coefficient quantization and are
also written into sequence/quant-matrix syntax. Drop-frame timecode is computed for 30000/1001
content rather than treated as a punctuation flag.

### Implemented H.264 syntax and playback slice

`mmrecode-h264` owns Annex-B and ISO/`avcC` length-prefixed NAL framing, decoder configuration
records, emulation-prevention removal, Exp-Golomb syntax, SPS/PPS/VUI interpretation, leading slice
headers, picture classification, and a conservative active-reference index. ISO-BMFF timestamps
remain authoritative: the syntax index pairs each coded sample with its exact DTS, PTS, duration,
IDR status, and decode-order dependencies without teaching the container about AVC.

`mmrecode-playback` adapts that index to the existing bounded request/event interface. It selects a
preceding sync sample and attempts native Rust reconstruction first. The first native pixel slice
handles single-slice, frame-coded, 8-bit 4:2:0 IDR pictures containing `I_PCM`, CAVLC
`Intra_16x16`, or CAVLC `Intra_4x4` macroblocks. Its complete Intra16, Intra4, and chroma
prediction modes work across the macroblock raster, with neighbor-context CAVLC DC/AC parsing,
quantization, inverse transforms, in-loop deblocking, coded-canvas placement, and display cropping.
Its CAVLC P-slice path retains one list-0 reference and reconstructs skip, 16x16, 16x8, 8x16, and
8x8 sub-macroblock partitions down to 4x4. Motion-vector prediction, quarter-sample luma and
eighth-sample chroma interpolation, explicit weighted prediction, inter residuals, mixed intra
macroblocks, and inter-picture boundary strengths are native. Baseline and High Profile streams
using CAVLC, implicit flat scaling, and 4x4 transforms share this path. For unsupported
reconstruction tools, playback constructs only the
required Annex-B window and sends it to an optional installed FFmpeg fallback. FFmpeg does not
demux the file, define timestamps, choose seek points, or perform edit planning.
The CABAC layer supplies its own context-state initialization, binary arithmetic decision, bypass,
termination, and restart processes. CABAC `I_PCM`, Intra16, and Intra4 IDRs use that path end to
end, including prediction syntax, coded-block-pattern and neighboring-block context derivation,
luma/chroma DC and AC coefficients, quantization, inverse transforms, filtering, and exact
x264/FFmpeg interoperability coverage. CABAC P slices additionally reconstruct skipped, 16x16,
16x8, 8x16, and 8x8-partitioned inter macroblocks down to 4x4, plus mixed Intra4/Intra16/PCM
macroblocks, with context-coded motion-vector differences, luma/chroma residuals, QP deltas, and
inter-picture filtering. The High Profile QP-zero bypass path directly reconstructs lossless
Intra4 and inter residual samples, including horizontal/vertical chroma residual DPCM. B slices and
8x8 transform syntax still return `Unsupported` for fallback.
Complete decoded-reference-picture marking and reference-list modification remain necessary before
the index is strong enough for arbitrary-boundary H.264 smart rendering. The first render adapter
already uses the conservative graph for a stricter operation: it accepts only complete contiguous
GOPs bounded by MP4 sync samples that contain IDR pictures, reports why the range is copy-safe, and
copies every encoded sample unchanged. No H.264 encoder has been started.

### Avoid premature algorithm abstraction

Codecs may share concepts without sharing implementations.

MPEG-2 and H.264 both use motion vectors, but their partitions, interpolation, vector predictors,
and reference rules differ. MPEG-2 and DV both use block transforms, but their exact transforms,
scans, mismatch behavior, and quantization rules differ.

Implement the clear codec-local version first. Extract shared code only after another codec proves
that the behavior and invariants are truly common. Shared acceleration dispatch, buffer layout,
SAD helpers, or test machinery may be reusable even when the full algorithm is not.

## Container crates

Use one crate per real container family:

- `mmrecode-avi`
- `mmrecode-isobmff` for shared MP4 and QuickTime/MOV machinery
- `mmrecode-mpegts`
- `mmrecode-mxf`
- `mmrecode-y4m` for the simple uncompressed test format

Muxing and demuxing normally belong in the same crate because they share syntax, descriptors, and
timestamp rules.

Container implementations own:

- Structural parsing and writing
- Stream discovery
- Sample/packet tables
- Interleaving
- Timestamp conversion
- Seeking and indexing
- Container metadata
- Preservation of unknown or opaque data where possible
- Mapping between container tags and extensible `CodecId` values

Container implementations do not own:

- JPEG entropy parsing
- MPEG-2 GOP semantics
- H.264 SPS/PPS interpretation
- Codec reconstruction

### Implemented ISO-BMFF/QuickTime slice

`mmrecode-isobmff` reads non-fragmented MP4/MOV box hierarchies and expands `stsd`, `stts`, `ctts`,
`stsc`, `stsz`, `stco`/`co64`, and `stss` into generic timed packets. It preserves `avcC` as opaque
codec configuration and exposes container-level `pasp`, `colr`, audio sample-entry, dimension, and
track-matrix rotation metadata. Seeking selects the closest preceding video sync sample. The crate
does not depend on H.264 and never parses parameter sets or slice syntax.

The crate also has a minimal single-video-track MP4 writer for clean packet-copy workflows. It
rebuilds decode/composition-time, size, chunk, and sync tables while preserving opaque `avcC`,
rotation, pixel aspect, colour metadata, and sample bytes. The render adapter—not the writer—proves
that a requested H.264 range is independently decodable. The current slice deliberately excludes
fragmented movies, edit lists, multiple active sample descriptions, incremental I/O, DRM,
audio/multitrack muxing, and files above 4 GiB.

### Implemented MPEG-2 Transport Stream slice

`mmrecode-mpegts` implements strict 188-byte packet parsing, adaptation/PCR fields, per-PID
continuity, pointer-aware PAT/PMT section reassembly with CRC validation, program and stream
discovery, and PES reassembly with PTS/DTS. Its muxer accepts generic MPEG-2 Video packets and
MPEG-1 Audio Layer II packets and emits a deterministic single-program stream with repeated PSI,
PCR, random-access flags, exact 90 kHz timestamp rescaling, and A/V interleaving. The CLI supplies
video picture dependencies and audio frame timing from codec crates; the container crate never
parses either codec's elementary syntax.

The slice deliberately does not claim live broadcast-system coverage: other audio codecs,
multi-program muxing, 192/204-byte variants, DVB/ATSC service information, scrambling, CBR
null-packet scheduling, jitter recovery, indexing, and timestamp-wrap-aware seeking remain future
work. Program Stream and MXF are separate container families rather than modes of this crate.
- Encoder decisions

## Dependency analysis and smart rendering

Every inter-frame codec can optionally implement `DependencyAnalyzer`. It converts codec-specific
reference behavior into `AccessUnitInfo`:

- Picture identifier
- Decode and presentation ordering
- Broad picture type
- Referenced pictures
- Random-access strength
- Parameter fingerprint relevant to splicing

`mmrecode-render` constructs an explicit plan using this operation vocabulary:

```rust
pub enum RenderOperation {
    CopyPackets(PacketRange),
    RewriteTimestamps(PacketRange),
    Decode(FrameRange),
    ApplyEffects(FrameRange),
    BridgeEncode(FrameRange),
    FullEncode(FrameRange),
    Mux,
}
```

The independent-frame slice accepts one gap-free video track of clean, reference-free access units.
It verifies packet-aligned clip boundaries, exact duration mapping, codec and parameter
compatibility, then emits and executes `CopyPackets`, `RewriteTimestamps`, and `Mux` operations.
Encoded payloads, flags, and opaque side data are preserved. The executor returns container-ready
packets. Direct delivery remains an optional adapter so the generic planner does not depend on all
containers.

`mmrecode-edit` separates the recursive authoring model from the flattened render intent. The
authoring project is a linked media graph: the project root and every media node expose an ordered
local timeline of child placements. A placement link owns its alias, parent-local timeline range,
child source range, and eventually transform/override data. Paths traverse links and establish a
local editing context; they are not filesystem ownership. Stable media and link identifiers allow
one media definition to be placed more than once while keeping instance edits unambiguous. Cycles
are forbidden in the composition graph.

The existing `EditSequence` remains the renderer-facing, codec-independent intent model. A later
compiler flattens the recursive graph into sources, typed streams, tracks, clips, exact ranges,
effects, transitions, and output intent without making codecs or containers understand authoring
navigation. This also lets terminal commands, scripts, a GUI, and AI operate on the same typed
authoring commands while preview and final render consume compiled graph state.

The generic inter-frame planner consumes decode-ordered `AccessUnitInfo` graphs for frame-aligned
ranges from one or more compatible sources. Each encode operation carries the exact source packet
indices represented by its output slots. References crossing a cut boundary force regeneration;
damage then propagates through retained dependents until copying is safe again. The planner also
maps exact timeline changes into directly edited pictures, identifies unchanged reference preroll,
and reserves output packet slots for copied and regenerated runs. A real MPEG-2 I/P/B integration
test proves both codec-local damage propagation and a multi-source boundary splice. The packet
executor can losslessly process unchanged MPEG-2 regions. An optional MPEG-2 adapter executes regeneration:
it supplies decoded source pictures and explicit replacements to the native encoder, creates a
closed GOP for each affected run, fills reserved decode-order packet slots, and validates the
resulting elementary stream through parse, dependency analysis, native reconstruction, and FFmpeg.
The feature boundary keeps the generic renderer independent of MPEG-2 unless selected. Exact
display/colour metadata, aspect ratio, profile/level, and luma/chroma matrices are now preserved by
bridge runs. GOP timecodes are recomputed from the source origin. `Mpeg2SpliceReport` classifies
each field as preserved, absent, recomputed, or rewritten. The reference encoder rewrites
incompatible source bitrate/VBV declarations to its explicit Main-Level bounds and uses
`vbv_delay = 0xffff`; production buffer continuity remains follow-on work.

The optional `mpegts` render feature is the first direct-delivery adapter. It validates a gap-free
MPEG-2 presentation timeline, frames optional Layer II without decoding it, and creates an
inspectable A/V packet schedule before muxing. Complete audio frames use an explicit `Exact`,
`Contained`, or `Cover` end policy with rational timestamp comparisons. Executing the plan then
registers the selected streams and feeds that exact schedule to `MpegTsMuxer`; planning itself emits
no container bytes. This keeps edit-boundary policy in rendering and H.222.0 syntax in the
container crate.

## Quality and verification

`mmrecode-quality` contains runtime-usable measurements such as:

- Exact plane/frame comparison
- PSNR
- SSIM
- Difference images and statistics
- Later perceptual metrics through optional components

`mmrecode-testkit` is development infrastructure used through `dev-dependencies`:

- Corpus manifests
- External decoder execution
- Frame hashing and comparison
- Mutation and truncation helpers
- Regression report generation
- Reference-stream discovery
- Determinism checks

Production codec users should not pull command runners, large corpora, or external-tool adapters
into their dependency graph.

## CLI and facade

The `mmrecode` package produces the main `mmrecode` application. With no arguments it enters the
interactive editor; explicit subcommands expose development and codec tooling:

```text
encode
decode
inspect
verify
compare
benchmark
edit
```

The CLI is an integration client, not the owner of media logic. A behavior useful to another
application belongs in a library crate.

A future `mmrecode-sdk` facade crate may re-export implementations behind opt-in features.
Direct dependencies remain supported so users can select only one codec or container.

No feature should enable every codec and container by default.

## Visual inspection application

`mmrecode-viewer` is a development and codec-analysis application, not a media-library layer or the
future editing UI. It directly consumes codec/container crates and never becomes their dependency.

Its first implementation uses `eframe`/`egui` with the `wgpu` renderer. Frames are converted to an
RGBA inspection texture on the CPU, while raw component planes remain separately viewable. This
keeps the initial implementation easy to validate. A later GPU presentation module may upload
planar textures and apply matrix, range, transfer, chroma-siting, and HDR transforms in shaders.
That display transform remains non-normative and separate from decoder reconstruction.

Codec-specific overlays—JPEG blocks, DV DIF maps, and MPEG-2 macroblock/dependency maps today;
later motion-vector arrows and slice overlays—read public inspection structures rather than placing
GUI concerns inside codec implementations. Reusable presentation machinery should be extracted to
a library crate only when the editor or another application actually needs it.

The first reusable presentation boundary is now `mmrecode-playback`. It owns exact fixed-frame-rate
timeline mapping and play/pause/seek/loop clock state, but it knows nothing about GUI frameworks,
audio devices, or containers. The viewer supplies either a monotonic wall clock or rendered audio
position. Audio is the master clock when present; video selects the corresponding display frame
and may skip frames rather than allowing A/V drift.

The first codec-specific orchestration in this layer is the indexed MPEG-2 playback source. It
parses presentation/dependency metadata without reconstructing pixels, moves owned elementary bytes
to a worker, begins each request at the closest preceding clean random-access picture, and emits a
small requested presentation window. New seek generations supersede stale work between pictures.
The codec exposes picture-at-a-time reconstruction state that retains only MPEG-2 reference frames;
the viewer currently retains at most 36 decoded display frames and matching macroblock maps. This
same source is intended for editor preview rather than being hidden in the GUI application.
The viewer starts after 12 contiguous frames are ready. If it reaches an unavailable frame it
freezes the media clock, pauses audio, requests the next non-overlapping window, and resumes from
the underflow position after preroll. Automatic refills never replace an unfinished covered request;
only an explicit seek creates a superseding generation.

The first terminal frontend consumes that same playback source through both
`mmrecode preview <media-file>` and the full-screen loop entered by `mmrecode edit`. Terminal UI
and graphics protocol dependencies belong only to the CLI crate. Capability probing selects Kitty,
Sixel, iTerm2, or 24-bit Unicode half-block output;
codec, container, playback, edit, and render crates remain terminal-agnostic. MPEG-2 reconstruction
and fallback terminal-specific resize/encoding use separate workers. Direct Kitty output transfers
local RGB frames through temporary files and alternates two image slots: it uploads and places the
next frame before deleting the previous placement. This uses the widely implemented baseline Kitty
graphics operations rather than optional terminal animation.
The UI thread owns input, the playback clock, a bounded display cache, and protocol state. In edit
mode it creates the full-screen shell even with no playback source, parses commands through
`mmrecode-edit`, and applies them to one `EditorSession`. Filesystem resolution and MPEG-2 probing
stay in the CLI host:
the library command produces an `ImportRequested` value, and the host returns typed `ImportedMedia`
metadata. Project authoring settings remain authoritative; imported durations are conformed to the
nearest parent frame rather than changing the project rate. Placement source ranges then bound
playback and are refreshed after trims or undo/redo. The terminal frontend maps
keyboard and mouse timeline scrubbing to the same bounded playback controller; canonical trims
preserve the playhead when it remains valid, while transport started at the out-point restarts at
the in-point. This is a reusable frontend boundary, not another editor or render graph.

The editor formats native positions as compact non-drop frame timecode and uses the same parser in
scripts and interactive input. Fields are interpreted from the right (`S:FF`, `M:SS:FF`, then
`H:MM:SS:FF`) and leading unused fields are omitted. The selected media time base supplies the
nominal frame-field width and limit, while the resolved edit remains an exact integer native-frame
position. Fractional rates therefore do not pass through floating-point seconds.

One command history serves the full-screen prompt and persists through the platform-specific
application state directory. Up/Down navigate entries, consecutive duplicates are collapsed, and
moving past the newest entry restores the user's unsubmitted draft. The prompt handles Tab itself
because a standalone readline implementation cannot also own the terminal event loop: completion
currently covers commands, manual/info topics, project settings, project/export presets, local
hierarchy aliases, and quoted filesystem paths. Canonical vocabulary is exported by `mmrecode-edit`
instead of duplicated in the frontend; tests require commands to keep manual entries and settings
and presets to appear in their manuals. Page Up/Page Down retain one-second timeline scrubbing so
history navigation has conventional shell semantics. Non-terminal stdin retains a line-oriented
adapter for scripts and integration pipelines; it is not the primary interactive UI.

`mmrecode-edit` also owns the versioned project-document schema and resolved authoring settings,
but not filesystem policy or codec probing. The JSON document carries a format marker and version,
stable media/link identifiers, project settings, relative managed origins, explicit external
origins, and placement ranges. Saves use a same-directory temporary file and rename. CLI host
requests distinguish `new`, project `open`, `save`, media `import`, and `export`; replacement and
quit operations protect dirty sessions unless `--discard` is explicit. Export compilation always
starts at the project root, independently of the current navigation path. The initial MPEG-2 slice
walks every root placement in composition order and renders its trim and project position, filling
gaps with project black. A single placement covering the timeline with matching rate, canvas, and
scan settings lowers to `EditSequence` and can use the GOP-aware packet path. Other progressive
root timelines decode required pictures and reference dependencies, conform by project-frame
timestamps, map YUV 4:2:0 planes into the project canvas with CPU Lanczos scaling, encode bounded
closed-GOP chunks, and mux the regenerated MPEG-2 stream as MPEG-TS. Nested generated/effect media
and alpha-aware composition remain the next compiler layer.
Unsupported delivery presets report
their missing codec/container slice instead of invoking an opaque external transcoder.

`project match` is also an explicit host request because the terminal-independent edit crate does
not probe codecs or containers. The host resolves the focused media origin, derives MPEG-2 canvas,
exact rate, pixel aspect, scan, and color, optionally reads MPEG audio rate/channel layout, then
submits the complete settings snapshot to `EditorSession` as one undoable time-preserving mutation.

The host normalizes every save target to the `.mmrecode` extension. Save As prepares a snapshot
before writing; if the current project still has the default `Untitled` name, the snapshot adopts
the normalized file stem. Only after the atomic write succeeds does the session adopt that name and
path. Undo/redo snapshots receive the same canonical project name so a later content undo cannot
unexpectedly revert the prompt to `Untitled`.

Project-rate changes are explicit root-timeline conformance operations. The default policy rescales
direct root placement boundaries to the new time base with nearest-ties-away rounding and returns a
report of affected placements and rounded boundaries. The alternate policy retains integer frame
numbers, intentionally changing presentation time. Both policies leave source ranges and nested
media time bases unchanged, recompute root duration, participate in session undo/redo, and persist
as ordinary exact placement ranges in the project document.

The panel beside the monitor is a derived context inspector. It resolves the current `MediaPath`
on every draw and presents the project root or selected media/placement without storing a second
selection model. It also owns presentation of contextual help: `Help` and `Man` outputs replace the
panel body, explicit `InfoTopic` commands select metadata sections, and successful trim commands
select an in-point or out-point focus with temporary left/right adjustment aliases. Kind-specific
sections are appended by the frontend from available typed media metadata; the first video section
reads the bounded MPEG-2 playback index rather than decoding an extra frame. The timeline is a
derived terminal visualization of the same playback range and index, with a ruler, edit boundaries,
playhead, and I-picture landmarks; clicking it maps back to a bounded source-frame seek.

Device output and temporary MP2-to-PCM decoding remain application-local. `mmrecode-viewer` uses
Rodio with its pure-Rust Symphonia MP2 backend and predecodes audio for short inspection media. This does not
turn Symphonia into MMRecode's normative MPEG audio implementation: `mmrecode-mpegaudio` still owns
validated Layer II framing, and a native sample decoder remains a separate codec milestone. Long
programs will require incremental transport demux and bounded audio queues; MPEG-2 video no longer
uses the viewer's former whole-file pixel model.

## Registration

Static Rust dependencies are sufficient initially. Applications instantiate the implementations
they use.

A registry may later map codec identifiers and container probes to constructors. Dynamic plugins
must not use Rust trait objects as a binary ABI.

Extensibility is broader than runtime codec loading. The future plugin model distinguishes media
importers, composition generators, semantic object renderers, MMFX modules, codecs/containers,
exporters, and command extensions. Built-ins can implement Rust traits directly. Third-party
plugins cross a versioned protocol boundary through sandboxed WASM/WASI or an external process,
with a stable C ABI reserved for native integrations that truly require it.

Every external plugin has a manifest declaring its API version, plugin kind, input/output media
types, capabilities, and determinism claim. Plugins exchange typed edit commands, scene objects,
packets, frames, or documented document trees; they never receive the editor's private Rust data
structures. A Markdown composition plugin should be an early proof because it exercises structured
authoring without requiring frame-level access.

## Effect execution

The planned MMFX source language compiles into a typed backend-neutral IR. Its normative execution
path is a safe scalar CPU interpreter or compiler. Multithreaded tiled and SIMD CPU backends are
tested against that reference and are the default candidates for high-quality final rendering.
Typography, vector coverage, large-radius filters, color conversion, and blending therefore remain
under MMRecode's explicit quality and precision policy.

WGSL/wgpu is an optional backend for responsive preview and compatible accelerated renders. It
consumes the same IR rather than defining separate effect behavior. Backend choice, preview proxies,
and any quality reduction are explicit render settings.

## C ABI and bindings

`mmrecode-capi` began experimentally after the first end-to-end MJPEG slice and now exercises
baseline JPEG, raw DV25, complete MPEG-2 elementary-stream decode/encode, and MPEG-TS video/audio
mux/demux ownership.
It exposes planar frame views, owned output buffers, version queries, structure-size validation,
and thread-local diagnostics without declaring the ABI stable. Stateful streaming handles remain a
later design step rather than wrappers around unproven lifecycle assumptions.

The C layer should:

- Use opaque handles for stateful streaming objects
- Use explicit structure sizes and ABI versions
- Avoid exposing Rust enum layouts
- Return structured error codes and retrievable diagnostic text
- Permit caller-provided allocation strategies where required
- Remain separate from internal Rust traits

All allocations crossing the initial ABI have one clearly named library free function. Every
exported operation catches Rust panics before they can unwind into C. Raw-pointer access and other
necessary unsafe code are isolated in `mmrecode-capi`; the remaining workspace continues to forbid
unsafe Rust.

Swift, Kotlin, Python, and other bindings should build on the stable C ABI unless a language has a
strong reason to use a native Rust binding.

## Safety and acceleration

Workspace lints forbid unsafe Rust outside the narrowly scoped C boundary crate. This establishes a
safe portable reference path while permitting the unavoidable pointer operations at the ABI edge.

When acceleration is introduced:

- Unsafe and assembly code live in narrowly scoped acceleration modules or crates.
- Every accelerated function has a safe reference implementation.
- Differential tests cover alignment, dimensions, edge extension, bit depth, and CPU features.
- Runtime dispatch never invokes unsupported instructions.
- Performance changes must not silently change normative reconstruction.

## Compatibility and versioning

The workspace begins at version `0.0.1`, uses Rust 1.92, and is not publishable. APIs may change
freely while the first vertical slice is built.

Before publishing crates:

- Establish a minimum supported Rust version policy.
- Decide which crate APIs are public commitments.
- Add changelogs and release automation.
- Define encoded-output compatibility expectations.
- Define serialized encoder-setting versions.
- Audit dependency licenses and standard-essential patent considerations.

## License boundary

All current crates inherit `Apache-2.0` from the workspace. Third-party code must not be copied into
the repository merely because its functionality is useful. Every dependency and adapted algorithm
requires provenance and license review.

The viewer's Rodio dependency is MIT/Apache-2.0; Rodio's selected Symphonia MP2 backend is MPL-2.0.
It is confined to the application dependency graph, and no Symphonia source is copied or modified
inside MMRecode. Binary distribution must retain the applicable third-party notices.

Apache-2.0's contributor patent grant covers only claims licensable by a contributor under the
license terms. It does not grant third-party patent-pool rights for standardized media formats.

## Architectural review questions

Before adding a module or crate, ask:

1. Is this a shared media concept or a format-specific rule?
2. Can the dependency point downward without forming a cycle?
3. Does another codec or container genuinely need this abstraction now?
4. Can the behavior be tested independently of the implementation that uses it?
5. Does this preserve timing, metadata, and encoded data needed for future smart rendering?
6. Would a user needing only one codec be forced to compile unrelated components?

If the answers are unclear, keep the implementation local until evidence establishes the correct
boundary.
