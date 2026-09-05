# MMRecode TODO

This document is the central index of work that remains across MMRecode. Detailed descriptions of
the current architecture and implemented slices live in [`concept.md`](concept.md),
[`design.md`](design.md), [`mmfx-concept.md`](mmfx-concept.md), and the individual crate READMEs.

A codec or container marked **slice complete** has a useful, tested vertical slice; it does not mean
that every profile, operating mode, or production optimization in the format is implemented.
Checkboxes are deliberately grouped into near-term work and optional later breadth so that the
roadmap does not turn every possible feature into an immediate commitment.

## Suggested next milestones

**Current codec path:** Validate representative iPhone/YouTube AAC-LC files, then add bounded audio
buffering/seek preroll. Uncommon 960-sample, multichannel, and HE-AAC variants remain explicit
fallback cases rather than native implementation targets. Strengthen H.264 arbitrary-cut dependency
planning before starting a native edit-boundary encoder.

1. Add media fingerprints, relinking, and collect/portable-copy behavior to the new versioned
   project document, then extend recursive timeline export beyond MPEG-2/MMFX into audio and later
   effect kinds.
2. Add dedicated interactive `in`/`out` adjustment modes while retaining canonical typed commands
   underneath them.
3. Extend edit delivery to multi-clip audio selection, boundary policy, and MPEG-TS output.
4. Extend the implemented MMFX Scene 0.2 image/layout/animation slice with media slots, intrinsic
   sizing, parameters, and richer timing before custom kernels or third-party plugins.
5. Extend the new indexed MPEG-2 preview path with incremental TS demux, streaming audio, buffering,
   and backpressure.
6. Add a native MPEG-1 Layer II decoder when audio must move from pass-through/viewer support into
   the reusable codec layer.
7. Extend the existing native H.264 reconstruction/conformance coverage and the clean-GOP remuxer
   into dependency-aware arbitrary edit boundaries; see the H.264 section for remaining breadth.

## Shared core and bitstream

**Status:** Foundational media, packet, stream, time, dependency, codec, container, bit reader/writer,
VLC, and start-code types exist.

- [x] Add checked timestamp rescaling with explicit rounding and overflow policies.
- [ ] Design reusable frame/audio buffer ownership: borrowed views, reference counting, pooling,
  alignment, and eventual hardware-surface handles.
- [ ] Exercise metadata and unknown side-data preservation through demux, edit, and remux paths.
- [ ] Define versioned, serializable encoder and render settings.
- [ ] Add property and fuzz tests for bit readers/writers, VLC tables, start-code scanning, and
  malformed parser input.
- [ ] Add registries and a high-level facade only when more codec/container implementations make
  static selection cumbersome.

## Motion JPEG

**Status:** Constrained slice complete for 8-bit baseline sequential JPEG with one interleaved scan,
including native decode/encode, reconstruction, CLI, C API, viewer, quality checks, and reference
vectors.

- [ ] Add progressive JPEG decoding.
- [ ] Add multi-scan sequential decoding.
- [ ] Support wider component layouts and color conversion, including CMYK/YCCK where appropriate.
- [ ] Generate optimized Huffman tables in the encoder.
- [ ] Improve restart-marker recovery and damaged-frame reporting/concealment.
- [ ] Support field-based/interlaced Motion JPEG conventions used by capture hardware.
- [ ] Add integer/SIMD transform and color-conversion paths after profiling the reference path.
- [x] Connect lossless per-frame packet copying and concatenation to `mmrecode-render`.
- [ ] Add selective MJPEG frame re-encoding when an edit changes decoded pixels.
- [ ] Add AVI and QuickTime/MOV mappings in their container crates.

## DV

**Status:** DV25 525/60 and 625/50 slice complete for raw DIF parsing, video decode/encode, 16-bit and
12-bit audio extraction, 16-bit stereo audio encode, timecode, API, CLI, C API, viewer, and FFmpeg
interoperability vectors.

- [ ] Implement adaptive 2-4-8 DCT-mode selection in the encoder.
- [ ] Preserve and round-trip more VAUX, AAUX, subcode, and camera metadata packs.
- [ ] Add explicit damaged-block concealment policies in addition to structural reporting.
- [ ] Cover locked/unlocked audio and additional valid audio/channel arrangements.
- [ ] Add DVCPRO25, DVCPRO50, and DVCPRO HD only when a concrete workflow requires them.
- [ ] Add threaded/SIMD hot paths after profiling.
- [x] Connect lossless frame-copy cuts and concatenation to `mmrecode-render`.
- [ ] Add selective DV frame re-encoding when an edit changes decoded pixels.
- [ ] Add AVI and QuickTime/MOV wrapping in their container crates.

## MPEG-2 Video

**Status:** Constrained Main Profile 4:2:0 progressive/interlaced frame-picture slice complete for
typed parsing, I/P/B reconstruction, deterministic encoding, open/closed GOP dependency analysis,
smart-render planning, API, CLI, C API, viewer, and FFmpeg vectors. The existing `plan-mpeg2`
CLI command only plans damage propagation; the optional Rust renderer now executes frame-accurate
cuts and concatenation across compatible MPEG-2 sources. The current long-form render CLI remains
a testing harness rather than the intended editor interface.

- [x] Translate decode-order dependencies and frame-aligned changes into generic render operations,
  including copied output slots, affected-picture propagation, and unchanged reference preroll.
- [x] Execute bridge plans through the optional native renderer adapter: copy unaffected pictures,
  decode reference preroll, apply replacement frames, bridge-encode the affected region, rewrite
  timestamps, and validate the elementary-stream splice.
- [x] Preserve aspect, display/colour metadata, profile/level, and all luma/chroma quantizer
  matrices at bridge boundaries; recompute GOP timecode from the source origin; deliberately
  preserve or report rewrites of bitrate, VBV-buffer, and picture-delay signalling.
- [x] Select frame-aligned source ranges across compatible MPEG-2 inputs, regenerate dependency-
  damaged cut boundaries, resume packet copying at safe points, and produce a continuous output
  timeline.
- [ ] Implement field-picture decoding and encoding.
- [ ] Implement dual-prime prediction.
- [ ] Add native encoder motion search for B pictures; the current encoder uses zero-vector
  bidirectional prediction.
- [ ] Add a production VBV scheduler and adaptive rate control.
- [ ] Add damaged-slice recovery and concealment.
- [ ] Add MPEG-1 Video syntax where legacy workflows require it.
- [ ] Add 4:2:2 Profile and other chroma/profile/level combinations as demand justifies.
- [ ] Consider scalability modes and other uncommon extensions only with real test streams and a
  concrete use case.
- [ ] Add slice/frame parallelism, SIMD, and optional hardware acceleration behind the normative
  reference path.
- [ ] Add Program Stream/VOB, MXF, and additional transport/container mappings in container crates.

## MPEG Audio

**Status:** MPEG-1 Audio Layer II framing, validation, timing, and pass-through are implemented.
Viewer playback currently uses Symphonia locally; the reusable codec crate does not yet decode to
PCM.

- [ ] Implement a native MPEG-1 Layer II PCM decoder.
- [ ] Validate protected-frame CRCs and define damaged-frame concealment behavior.
- [ ] Add native audio decode conformance and quality vectors independent of viewer playback.
- [ ] Add an encoder and psychoacoustic model only when native audio creation is required.
- [ ] Add reusable resampling, channel-layout conversion, and mixing outside the codec parser.
- [ ] Add MPEG-2/2.5 audio extensions when required.
- [ ] Treat Layer I, Layer III, and free-format support as optional separate slices.

## AAC

**Status:** The common iPhone/YouTube AAC-LC playback path and native nonzero spectral/synthesis
subset are implemented; this is deliberately not a general AAC decoder. ISO-BMFF unwraps `esds` into
decoder-specific bytes; the AAC crate validates `AudioSpecificConfig` and resolves object type,
sample rate, channels, and frame length and can frame raw MP4 access units as ADTS. Playback indexes
exact sample timing and
schedules complete-track PCM reconstruction through the shared executor. Playback tries our Rust
decoder first and optionally restarts unsupported tracks through FFmpeg. Completion events and
terminal preview identify the actual backend, and native-only policy prevents hidden fallback.
The actual iPad acceptance file's **silent** 44.1 kHz stereo track now decodes entirely in Rust;
its HEVC video remains a separate codec slice. Native Huffman decoding, inverse quantization,
PNS, M/S and intensity stereo, pulse reconstruction, TNS, and sine/KBD synthesis now reconstruct
nonzero mono/stereo audio. Uncommon 960-sample, multichannel, and HE-AAC files intentionally require
fallback under the iPhone/YouTube-focused scope.

- [x] Parse AAC-LC `AudioSpecificConfig`, standard channel configurations, explicit rates, and
  raw-access-unit ADTS headers.
- [x] Extract decoder-specific AAC bytes from ISO-BMFF `esds` descriptors and validate them at the
  codec/playback boundary.
- [x] Index AAC samples, timing, start offset, duration, and encoded sizes from ISO-BMFF tables.
- [x] Apply the common optional-empty-plus-single-media edit list and trim decoded AAC priming and
  padding to the edited presentation interval.
- [x] Schedule PCM decode through `DecodeExecutor` and synchronize terminal H.264 playback to the
  audio device clock across play, pause, seek, underflow, and loop operations.
- [x] Implement native mono/stereo raw-data-block, ICS/window grouping, ZERO_HCB section, M/S mask,
  bounded fill/data-element parsing, and zero-spectrum PCM behind `AudioDecoder` (1024 samples).
- [x] Bound codec output to one pending frame, preserve packet timing, and require reset after
  failed reconstruction so missing overlap history cannot be silently ignored.
- [x] Verify the native subset with independent Apple packets, FFmpeg-generated mono/stereo silence,
  native-only cooperative playback, malformed input, and unsupported-feature regressions.
- [x] Recognize ASC sync-extension SBR/PS and reject it from plain-LC native/ADTS paths; reject
  960-sample frames in the ADTS bridge instead of incorrectly signalling 1024 samples.
- [x] Drain external PCM concurrently with compressed-input feeding to avoid pipe-capacity deadlock.
- [x] Implement native Rust AAC-LC noiseless coding, inverse quantization, stereo tools, IMDCT,
  overlap/add, and conformance vectors behind `AudioDecoder`.
  - [x] Add scalefactor/spectral Huffman tables, all eleven books, escape bounds, band offsets,
    grouped-short deinterleaving, inverse quantization, and M/S stereo.
  - [x] Add long/short sine/KBD synthesis, start/stop transitions, and persistent overlap with
    nonzero reference PCM tests (13 standard mono rates, stereo M/S/independent spectra).
  - [x] Verify native-only nonzero MP4 playback, presentation trimming, and overlap/reset behavior.
  - [x] Add PNS, intensity stereo, pulse, TNS, and bounded gain-control syntax, with isolated and
    combined independent PCM comparisons.
  - [x] Replace the scalar O(N²) transform with an audited radix-2 FFT-backed transform.
  - [ ] Validate a retained corpus of representative iPhone recordings and YouTube AAC-LC MP4s.
- [ ] Replace eager complete-track PCM with bounded packet/PCM queues and sample-accurate seek
  preroll for long media.
- [ ] Add browser audio-device output while preserving cooperative baseline WebAssembly decode.
- [ ] Add implicit/explicit SBR and Parametric Stereo only if the retained target corpus requires it.
- [ ] Add AAC encoding and ISO-BMFF audio muxing after decode and edit-boundary behavior stabilize.

## MPEG-2 Transport Stream

**Status:** Single-program 188-byte TS slice complete for PAT/PMT discovery, continuity and CRC
validation, PES reassembly, PTS/DTS/PCR timing, MPEG-2 Video demux, optional Layer II audio, and
deterministic A/V muxing. The optional renderer adapter now directly delivers smart-rendered MPEG-2
plus complete Layer II frames with an inspectable A/V boundary report.

- [ ] Add incremental streaming demux and mux APIs instead of requiring complete buffers.
- [ ] Build timestamp/keyframe indexes for seeking and handle 33-bit timestamp wrap explicitly.
- [ ] Improve discontinuity, packet-loss, jitter, and partial-PES recovery.
- [ ] Preserve unknown descriptors, PSI sections, and streams during remux where possible.
- [ ] Add muxing for multiple programs and richer PMT descriptors.
- [ ] Add more stream types as their codec slices arrive.
- [ ] Add live constant-bit-rate muxing, null-packet insertion, and disciplined PCR cadence.
- [ ] Add 192-byte M2TS and 204-byte protected transport-packet modes.
- [ ] Add relevant DVB/ATSC service tables and SCTE-35 splice metadata as separate bounded slices.
- [ ] Consider scrambling/conditional-access metadata only when there is a legitimate integration
  requirement; descrambling is outside the default scope.

## YUV4MPEG2

**Status:** The simple Y4M reader/writer supports the current uncompressed test and CLI workflows.

- [ ] Preserve and emit frame rate, aspect ratio, interlace, color, and extension tags consistently.
- [ ] Support additional chroma tags and higher bit depths needed by future codecs.
- [ ] Map per-frame tags and timing into shared frame metadata.
- [ ] Add incremental reader/writer APIs for long streams.

## Editing and minimal-recompression rendering

**Status:** The shared dependency vocabulary and MPEG-2 damage planner exist. `mmrecode-edit` now
models a recursive linked-media authoring graph plus the flattened sources/tracks/clips render
intent. Its first typed session navigates placement links and runs identical concise commands from
scripts or the interactive prompt.
`mmrecode-render` plans and executes packet-aligned, independent-frame DV and MJPEG
cut/concatenate paths with payload and side-data preservation plus exact timestamp rewriting. Its
generic inter-frame planner now maps MPEG-2 reference graphs and changed frame ranges into copy,
decode, effect, and bridge/full-encode operations with decode-preroll accounting. It selects exact
ranges across compatible sources and regenerates dependencies crossing either cut boundary. The
optional MPEG-2 adapter executes those fixed-rate bridge plans and validates the splice. Its optional
MPEG-TS delivery adapter drives the muxer directly and applies an explicit exact/contained/cover
policy to complete Layer II frames. Broader sample-domain audio editing remains unimplemented.

- [x] Create `mmrecode-edit` with sources, tracks, clips, ranges, transitions, effects, and output
  intent without codec-specific syntax.
- [x] Add the recursive media authoring graph: stable reusable media and placement-link IDs, local
  source/timeline ranges, ordered children, contextual paths, and cycle rejection.
- [x] Add the first shared typed command/session layer and `mmrecode edit [script]` frontend with
  `pwd`, `ls`, `info`, `cd`, `add`, `in`, `out`, undo, and redo.
- [x] Add a host-resolved typed `import` request and probe real MPEG-2 ES/TS and H.264 MP4/MOV sources into an
  undoable external media placement with its native frame time base.
- [x] Persist a readable versioned project snapshot with resolved authoring settings, stable IDs,
  project-relative managed media, explicit external links, atomic save, and validated load.
- [x] Add `new`, project `open`, `save`/`save as`, `import`, project presets/settings, dirty-state
  protection, and contextual project inspection to the shared command/host lifecycle.
- [x] Start with a usable default Untitled project; append `.mmrecode` on save and derive its name
  from the first Save As target without requiring a preliminary `new` command.
- [x] Allow undoable project-rate changes after media placement with explicit time-preserving and
  frame-number-preserving root-timeline conformance; report nearest-frame rounding without
  rewriting source ranges.
- [x] Add `project match` to atomically adopt the focused media's probed video and available audio
  format, with completion, contextual discovery, persistent settings, undo, and canonical help.
- [ ] Add media fingerprints, relinking, and collect/portable-copy behavior.
- [x] Compile one root MPEG-2 source placement into `EditSequence`, expose a dry-run export plan,
  and execute the existing smart-renderer plus MPEG-TS delivery through `export`.
- [x] Automatically full-render a mismatched progressive MPEG-2 placement: timestamp-based frame
  rate conformance, persisted fit/fill/stretch/native sizing, CPU Lanczos YUV 4:2:0 scaling,
  bounded Main@Main/Main@High encoding, MPEG-TS delivery, and explainable `export plan` output.
- [x] Render every root MPEG-2 placement independently of the current navigation context,
  including sequential cuts, trims, project positions, black gaps, and opaque composition order.
- [ ] Compile nested media/effect content recursively into the flattened render intent, including
  alpha-aware composition rather than the current opaque-video ordering.
- [x] Prove capability-selected Kitty, Sixel, iTerm2, and 24-bit half-block terminal preview with
  real asynchronous MPEG-2 ES/TS and H.264 MP4/MOV playback, stepping, seeking, looping, and bounded buffering.
- [x] Add a double-buffered direct Kitty playback path for flicker-free local terminal
  video in Kitty-compatible terminals such as Ghostty.
- [x] Make `mmrecode` / `mmrecode edit` a full-screen editor shell even with no source loaded, then
  populate its monitor in place when `import` resolves media.
- [x] Add a compact monitor/context/timeline layout with a time ruler, visible trim range,
  playhead, MPEG-2 I-picture landmarks, and bounded mouse/keyboard scrubbing.
- [x] Use compact frame timecode consistently in editor commands, listings, information, scrub
  feedback, and the terminal timeline while retaining legacy raw-frame script input.
- [x] Add full-screen prompt history with Up/Down navigation, duplicate suppression, and
  restoration of an unsubmitted draft.
- [x] Persist interactive history in the platform application-state directory and add contextual
  Tab completion for commands, help/info topics, hierarchy aliases, and quoted project/media paths.
- [x] Replace the fixed selection/transport panel with a hierarchy-aware inspector for project,
  placement, video settings, and focused in/out command context.
- [x] Turn the inspector into contextual discovery with startup/general `help`, detailed
  `man <command>`, explicit project/video/audio/source `info`, and focused left/right trim aliases.
- [x] Share canonical command/setting/preset vocabulary with prompt completion and add regression
  tests that require every command and setting to remain covered by interactive documentation.
- [ ] Add dedicated interactive adjustment keymaps/modes that emit the same canonical typed trim
  commands, with visible mode, boundary, delta, and commit/cancel state.
- [ ] Add an edit/full-screen-monitor view toggle over the same playback state and frame cache.
- [x] Add a pixel-rendered 24-bit timeline layer for thumbnails, codec landmarks, smart-render
  state, and dense colored media regions while retaining terminal-native text and controls.
- [x] Project the current hierarchy level into separate ordered object rows, label the timeline
  with its media-path breadcrumb and local time domain, and add a synthetic `self/source` context
  row. `cd` should replace the editable rows with the entered object's local children; a future
  explicit overview may show ancestors or flattened descendants.
- [x] Create `mmrecode-render` with explicit operations such as `CopyPackets`,
  `RewriteTimestamps`, `Decode`, `ApplyEffects`, `BridgeEncode`, `FullEncode`, and `Mux`.
- [x] Implement the first independent-frame cut/concatenate path with DV.
- [x] Drive the MPEG-TS muxer directly for the first MPEG-2 Video plus optional Layer II delivery
  path while retaining a separately inspectable dry-run plan.
- [ ] Generalize direct delivery across other selected containers and stream combinations.
- [x] Add an MJPEG dependency analyzer and connect the same independent-frame path.
- [x] Plan MPEG-2 GOP-aware regeneration through generic operations using `DependencyAnalyzer`
  output, including dependency propagation and decode preroll.
- [x] Execute MPEG-2 bridge encoding for one complete fixed-rate source and splice regenerated
  packets into byte-preserved copied output with native and FFmpeg validation.
- [x] Extend MPEG-2 planning/execution to frame-accurate ranges from multiple compatible sources,
  including non-contiguous decode-order selection at cut ends and continuous output timestamps.
- [x] Define the first exact A/V end policy for frame-aligned MPEG-2 and complete Layer II frames:
  reject, contain, or cover a fractional audio-frame boundary without silent rounding.
- [ ] Extend edit-boundary rules to decoded audio samples, audio preroll, fades/mixes, and general
  multi-track A/V sync.
- [x] Verify codec-parameter compatibility and preserve packet flags and side data in the initial
  packet-copy path.
- [ ] Make every render plan explainable: copied/reencoded ranges, causes, dependencies, expected
  quality loss, and byte/time estimates.
- [ ] Add deterministic output, cancellation, progress reporting, and recoverable failure handling.
- [ ] Add transitions/effects only after the copy/reencode boundary machinery is verified.
- [ ] Expose stable editing/rendering entry points through the C API after the Rust model settles.

## Effects, compositing, and authoring plugins

**Status:** The first executable `.mmfx` foundation parses typed `Scene`/`Group`/`Rect` objects and
renders nested absolute/overlay layouts through a tested linear-premultiplied scalar CPU backend.
Final effects remain CPU-authoritative; GPU execution is an optional backend. Plugins exchange
versioned semantic values rather than internal Rust objects.

- [x] Add strict, source-spanned `.mmfx` parsing with unknown-property rejection and suggestions.
- [x] Add typed scenes, groups, rectangles, px/% lengths, anchors, translation, color, opacity,
  clipping, and rounded corners without renderer types in the scene model.
- [x] Add a scalar linear-premultiplied RGBA reference renderer and a `render-mmfx` PNG CLI proof.
- [x] Use pinned Zeno coverage masks for antialiased rectangles, rounded corners, fractional
  placement, and nested rounded clipping.
- [x] Add explicit module-relative `@font` resources and typed static `@text` with Parley shaping
  and wrapping plus Swash/Zeno glyph coverage; disable implicit system fonts for final rendering.
- [x] Make generated scene media own embedded `.mmfx` source, create/place it through `add scene`, edit
  it in hierarchical `cd` context, serialize it with the project, load external source as an embedded
  copy with a retained resource base, extract it with `scene save as`, and provide multiline editing,
  project undo/redo, debounced worker preview, diagnostics, last-good retention, and complete help.
- [x] Use the custom-pixel timeline for FX-only projects, provide a text-bearing starter scene with
  a bundled deterministic font, and composite active direct MMFX placements into timeline preview.
- [x] Add typed decoded images, contain/cover/fill fitting, row/column layout, exact-frame
  keyframes, scale/rotation, and cover-style scrolling.
- [ ] Extend text with fallback chains, color glyphs, decorations, and intrinsic sizing; add media
  slots, parameters, animation delay/repetition, and richer timing controls.
- [x] Move direct MMFX preview/export into a reusable incremental CPU project compositor with
  cached scene/resource rasterization, cached preview scaling, transparent bounds, and preconverted
  in-place Yuv420p8 blending; export direct root FX layers and FX-only projects through MPEG-2/TS.
- [x] Extend the shared compositor from direct hierarchy levels to recursive nested composition,
  with exact per-frame path mapping, ancestor-trim clipping, shared video/FX composition order,
  cached synchronization by project revision/context, and recursive MPEG-2/TS export.

- [x] Define typed scene objects for text, rectangles, images, groups, transforms, bounded layout,
  exact timing, and keyframe animation without tying them to a renderer.
- [ ] Add paths, media slots, intrinsic sizing, parameters, and reusable styles to Scene IR.
- [ ] Define the safe, bounded MMFX language and typed portable IR, including color, sampling,
  coordinate, edge, precision, and time semantics.
- [x] Implement the first scalar CPU reference backend with deterministic parser, layout,
  compositing, clipping, and pixel tests.
- [ ] Add tiled multithreaded and SIMD CPU execution with correct halos for large-radius effects and
  differential testing against the reference backend.
- [ ] Add controlled text shaping, font fallback, vector rasterization, high-quality antialiasing,
  and linear-light/high-precision compositing.
- [ ] Add optional WGSL/wgpu preview execution from the same IR; keep backend and preview-quality
  choices explicit.
- [ ] Define a versioned plugin manifest, capability model, typed protocol, diagnostics, lifecycle,
  caching, and determinism contract.
- [ ] Support built-in Rust plugins plus sandboxed WASM/WASI and external-process plugins without
  treating Rust trait objects as a stable binary ABI.
- [ ] Use a Markdown composition generator as the first semantic authoring-plugin proof: headings,
  media, code, diagrams, and timing should become editable scene/timeline objects.

## Playback engine

**Status:** Exact fixed-rate timelines, play/pause/stop/seek/step/loop behavior, and wall/external
audio clocks exist. MPEG-2 now has a lightweight presentation/dependency index, picture-at-a-time
worker decoding from clean random-access points, seek generations, and a bounded viewer frame
cache. Other codecs and audio still use eager paths.

- [ ] Add timestamp-indexed variable-frame-rate timelines.
- [x] Add indexed, bounded, asynchronous MPEG-2 picture decode and presentation caching.
- [x] Add clean-random-access MPEG-2 seeking with decoder preroll and stale-request cancellation.
- [x] Add MPEG-2 file preroll plus clock-safe automatic pause/resume on decode underflow.
- [ ] Add bounded container-packet and audio queues with explicit backpressure.
- [ ] Add discontinuity flushing and recovery behavior for damaged/live inputs.
- [ ] Add reusable audio resampling, channel conversion, device-format negotiation, and clock
  latency/drift compensation.
- [ ] Generalize buffering and underflow policy from MPEG-2 files to other codecs and live inputs.
- [ ] Add playback-rate support only after clock and resampling behavior is stable.

## Native viewer

**Status:** JPEG/MJPEG, DV, MPEG-2 ES, MPEG-TS, and Y4M inspection/playback work, including
synchronized TS/DV audio, plane and pixel inspection, JPEG markers, DV DIF maps, and MPEG-2
macroblock/dependency information. MPEG-2 video uses background, on-demand decode and a 36-frame
cache; other media and audio remain eager.

- [x] Move MPEG-2 video from complete predecode to indexed playback requests and a bounded cache.
- [ ] Move TS demux, MP2 audio, DV, MJPEG, and Y4M long-file paths to incremental bounded queues.
- [ ] Add timeline navigation by timestamp, keyframe, GOP, PES, error, and marker.
- [ ] Add slice-boundary and motion-vector overlays.
- [ ] Add audio waveform, meters, channel selection, and device selection where useful.
- [ ] Replace or supplement CPU RGB conversion with GPU planar presentation and a controlled color
  pipeline; add HDR only with formats that need it.
- [ ] Allow the user to choose the assumed frame rate for raw MJPEG.
- [ ] Use the native MPEG Audio decoder once it exists, removing the viewer-only MP2 decode path.
- [ ] Package a distributable macOS application after the interfaces stabilize.

## Additional containers

**Status:** MPEG-TS, Y4M, and a first ISO-BMFF/QuickTime sample-table demuxer plus single-video-track
MP4 writer exist. Each remaining item below should be its own crate and bounded vertical slice.

- [ ] **AVI:** prioritize OpenDML indexing, MJPEG, DV, PCM, and metadata preservation.
- [x] **QuickTime/MOV and ISO BMFF/MP4:** read non-fragmented sample tables, DTS/PTS and composition
  offsets, sync samples, chunk offsets, `avcC`, `pasp`, `colr`, track rotation, basic AAC sample-entry
  metadata, generic packets, and keyframe-aligned seeking for H.264 editor import/playback.
- [ ] Add fragmented MP4, edit lists, multiple sample descriptions, richer metadata preservation,
  incremental I/O, and audio/multitrack muxing as separate ISO-BMFF slices.
- [ ] **MPEG Program Stream/VOB:** add PES, SCR, navigation/private-stream handling, and MPEG-2/MP2
  mappings as required by archive/DVD workflows.
- [ ] **MXF:** begin with one operational pattern and concrete DV/MPEG-2 archive samples rather than
  attempting all of SMPTE MXF at once.
- [ ] Add container-independent indexing and metadata-preservation tests shared by these crates.

## H.264 and future video codecs

**Status:** The H.264 syntax/indexing, MP4/MOV editor import/playback, video-only clean-GOP remux,
and first deterministic encoder foundation are implemented. The encoder currently emits
Baseline-profile, all-IDR lossless `I_PCM` pictures plus transform-coded Intra16 and Intra4
pictures with CAVLC residuals and configurable picture/macroblock QP. Its bounded inter mode retains
up to four references,
emits every P partition down to 4x4 with quarter-pixel motion and P-skip, and optionally reorders up
to three non-reference B pictures between anchors. B16x16, B16x8, B8x16, and all thirteen B8x8
subtypes select list-0/list-1/bi/direct motion independently; nonzero spatial direct, colocated-zero
handling, temporal direct with POC-scaled colocated motion, and B-skip are also encoded. Adaptive
frame-level target-bitrate control now adjusts QP across all compressed picture types from packet
size, frame duration, and a bounded virtual buffer. Optional activity-based macroblock AQ also
redistributes each picture target between quiet and textured regions with normative QP-delta state.
Opt-in single-CPB NAL HRD/VBV signalling and scheduling writes VUI plus buffering/picture-timing
SEI and rejects access units that violate the declared buffer. Broader coding tools remain
follow-on work. Encoder profile selection now advertises Baseline for I/P-only streams and Main for
B-picture streams instead of placing unsupported B syntax under a Baseline SPS. High Profile
Intra8 output adds filtered 8x8 prediction, transform/quantization, and CAVLC residuals. Annex A
level selection covers Levels 1 through 6.2 using structural, cadence, DPB, bitrate, and CPB limits.
Pixel reconstruction attempts the native CAVLC and CABAC decoder first and
currently uses an optional bounded FFmpeg process fallback for other reconstruction tools. Native
in-loop deblocking and default-list multiple-reference CAVLC P slices are also implemented, including skip,
16x16, 16x8, 8x16, and sub-macroblock partitions with fractional-sample motion compensation,
explicit weighted prediction, and inter residuals. The same path accepts the High Profile subset using CAVLC and
4x4 transforms. MMRecode itself owns demuxing, timestamps, NAL conversion, SPS/PPS/VUI/slice
parsing, dependency indexing, and seek-window selection. HEVC, AV1, and VVC have not been started.

- [x] Make native preview interactive: publish decoded frames immediately, preserve decoder/DPB
  state across sequential refills, coalesce queued seeks, and interrupt stale work between access
  units. Remove whole-chroma-plane copies from progressive prediction, add integer-motion fast
  paths, and cache deblocking boundary strengths. The development acceptance sample reaches its
  first 3456x2234 frame in about 0.16 seconds and an equivalent 1080p/48 fps encode decodes faster
  than real time; the original near-4K stream still requires SIMD or parallelism for real-time play.
- [x] Add a shared bounded decode-executor API. Native builds use a fixed-size process-wide worker
  pool; baseline WebAssembly uses the same job interface with cooperative polling. Move H.264
  playback to access-unit-sized jobs with generation cancellation and caller-supplied executors.
- [x] Add the first dependency-safe H.264 frame parallelism: share immutable DPB reference planes
  and motion metadata, fork progressive non-reference B pictures onto the executor, skip unneeded
  non-reference preroll, and keep the main reference-picture session authoritative. Verify forked
  pixels against sequential native decoding and a real x264/FFmpeg GOP. The 3456x2234 acceptance
  window improved from roughly 14 fps to 25 fps for 24 frames.

- [x] **H.264/AVC foundation:** Annex-B and length-prefixed NAL handling, `avcC`, SPS/PPS/VUI and
  leading slice headers, container-timed access-unit indexing, IDR/reference classification, and a
  conservative active-reference dependency index.
- [x] Import, inspect, seek, scrub, and play ordinary non-fragmented H.264 MP4/MOV media in the
  terminal editor, including `project match` from SPS/VUI and container audio/display metadata.
- [ ] Replace conservative dependency sets by teaching the syntax index complete reference-list
  modification, decoded-picture marking/MMCO, frame-num gap, field, recovery-point, and POC
  semantics.
- [x] Add the native Rust decoder foundation behind the playback interface: activate `avcC`
  parameter sets, traverse one frame-coded IDR I-slice, reconstruct 8-bit 4:2:0 `I_PCM` and CAVLC
  `Intra_16x16` and `Intra_4x4` macroblocks with all luma/chroma predictors, neighbor-context coefficient parsing,
  nonzero DC/AC quantization and inverse transforms, normative intra-picture deblocking and slice
  offsets, crop the coded canvas, preserve timing/colour metadata, retain one decoded reference,
  reconstruct CAVLC P slices with skip, 16x16, 16x8, 8x16, and sub-macroblock
  partitions down to 4x4, quarter-sample luma/eighth-sample chroma motion compensation, inter
  residuals, explicit weighted prediction, mixed intra macroblocks, and inter-picture boundary
  strengths, plus High Profile CAVLC/4x4 streams, and try it before
  fallback. Verify flat, residual, Intra4, multicolour, deblocked, fractional-motion, partitioned-P,
  and multi-frame x264 vectors byte-for-byte against independent FFmpeg reconstruction.
- [x] Establish the native CABAC arithmetic layer and integrate real x264 CABAC `I_PCM`, Intra16,
  and Intra4 IDRs:
  initialize adaptive contexts from slice QP, decode regular/bypass/termination bins, cross the
  byte-aligned PCM region, restart arithmetic decoding, derive neighboring luma/chroma DC/AC
  coded-block and coded-block-pattern contexts, reconstruct quantized residuals and prediction
  modes, and verify exact filtered pixels against FFmpeg.
- [x] Extend CABAC into P slices with all three `cabac_init_idc` tables, skip, 16x16, 16x8, 8x16,
  and 8x8 partitions down to 4x4, mixed Intra4/Intra16/PCM macroblocks, context-coded motion-vector
  differences, luma/chroma residuals, QP deltas, and inter-picture filtering; verify skipped,
  motion-only, residual, partitioned, and sustained mixed-macroblock x264 GOPs byte-for-byte.
- [x] Preserve and apply the High Profile QP-zero transform-bypass SPS flag for lossless Intra4 and
  inter luma/chroma residuals, including horizontal/vertical residual DPCM across chroma sub-blocks;
  verify mixed CABAC PCM/Intra4 pictures and a lossless P GOP byte-for-byte against FFmpeg.
- [x] Add CABAC `Intra_8x8` prediction and 8x8 luma inverse transforms for intra and inter
  macroblocks, including transform-size contexts, coefficient contexts, transform-aware deblocking,
  and sustained High Profile I/P GOP comparison byte-for-byte against FFmpeg.
- [x] Parse and resolve SPS/PPS scaling lists, apply them to native intra/inter 4x4 and luma 8x8
  inverse quantization, and capture the second chroma QP offset for component-correct deblocking;
  verify a sustained non-flat JVT-matrix CABAC I/P GOP byte-for-byte against FFmpeg.
- [x] Replace the single retained picture with a bounded sliding short-term DPB and decode default
  list-0 `ref_idx_l0` syntax for CAVLC P macroblock and sub-macroblock partitions, including
  unavailable-neighbour motion-predictor substitution; verify a textured 12-frame x264 `ref=2` GOP
  with older-picture selection and deblocking byte-for-byte against FFmpeg.
- [x] Decode CABAC default-list `ref_idx_l0` syntax with neighboring-reference contexts for 16x16,
  16x8, 8x16, and 8x8 sub-macroblock partitions; verify an alternating 12-frame x264 `ref=2` GOP
  that selects the older picture for every inter block byte-for-byte against FFmpeg.
- [x] Apply P-slice short-term list-0 modification with normative picture-number prediction,
  wraparound, insertion, and duplicate removal; verify a reordered skipped picture end to end
  against an independently decoded handcrafted stream.
- [x] Implement frame-picture decoded-reference marking: sliding-window eviction, all adaptive
  MMCO operations, maximum long-term index management, reset, short-to-long conversion, current/IDR
  long-term assignment, and long-term list-0 modification; verify explicit long-term reconstruction
  against FFmpeg and every state transition directly.
- [x] Establish frame-coded B reconstruction with POC type-0 tracking, default list-0/list-1
  construction, separate list motion state, and CAVLC 16x16 L0, L1, and unweighted bidirectional
  prediction; verify all three prediction modes against an independently decoded handcrafted GOP.
- [x] Add CAVLC spatial-direct `B_Direct_16x16` and `B_Skip`, including neighboring-reference
  inference, separate list motion predictors, and the co-located zero rule; verify handcrafted
  direct/skip pictures and a reordered x264 B-frame GOP byte-for-byte against FFmpeg.
- [x] Decode all 18 explicit CAVLC B 16x8/8x16 macroblock types, preserving H.264's list-grouped
  reference-index and motion-difference syntax and partition-specific motion predictors; verify
  every L0/L1/Bi combination and both partition orientations against FFmpeg.
- [x] Decode all twelve explicit CAVLC `B_8x8` sub-macroblock types from 8x8 through 4x4, retain
  list-not-used neighbors as available with `refIdx = -1`, and verify every subtype, mixed
  quadrants, nonzero 4x4 motion, and a sustained moving x264 B GOP against FFmpeg.
- [x] Decode spatial and temporal direct CAVLC B prediction for whole, skipped, and `B_8x8`
  macroblocks; retain colocated reference identity, scale temporal motion by POC distance, honor
  both `direct_8x8_inference_flag` granularities, and verify handcrafted and x264 GOPs against
  FFmpeg.
- [x] Apply explicit list weight tables and implicit POC-distance weighting to CAVLC B
  single-list, bidirectional, and direct partitions; verify asymmetric luma/chroma weights and a
  real weighted x264 GOP byte-for-byte against FFmpeg.
- [x] Apply in-loop deblocking to B pictures with two-list reference-identity and swapped-pair
  boundary-strength comparisons; verify a moving x264 B GOP byte-for-byte against FFmpeg.
- [x] Decode CABAC B slices with context-coded skip, macroblock and sub-macroblock types,
  reference indices, and motion differences; reconstruct direct/skip, all explicit
  16x16/16x8/8x16 and `B_8x8` forms, embedded intra macroblocks, temporal direct, implicit
  weighting, High Profile 8x8 transforms, and in-loop filtering. Verify partitioned, intra-mixed,
  temporal-direct, weighted, and deblocked x264 GOPs byte-for-byte against FFmpeg.
- [x] Correct Intra8x8 above-right sample availability for the top-right partition so filtered
  future references feed weighted/direct B prediction exactly; verify the broader High Profile
  `testsrc2` CABAC B stress GOP byte-for-byte against FFmpeg with in-loop deblocking enabled.
- [x] Retain type-1 POC cycle parameters and derive all three frame-picture POC modes, including
  deltas, non-reference offsets, frame-number wrap, B-list ordering, and MMCO 5 resets; verify custom
  type-1 B and type-2 reference sequences byte-for-byte against FFmpeg.
- [x] Parse recovery-point SEI payloads and attach their countdown, exact-match, broken-link, and
  changing-slice-group semantics to the indexed access unit without treating recovery as IDR.
- [x] Decode reference and non-reference non-IDR I pictures through the native CAVLC/CABAC intra
  path, including entry without prior DPB state; verify a non-IDR `I_PCM` picture against FFmpeg.
- [x] Reconstruct multi-slice CAVLC and CABAC I/P/B frame pictures with slice-local entropy,
  intra/motion prediction, and coded-block context availability, deferred full-picture filtering,
  per-slice offsets, and normative `disable_deblocking_filter_idc` cross-slice behavior; verify
  deblocked, non-row-aligned x264 slices byte-for-byte against FFmpeg.
- [x] Carry recovery-point SEI into the playback index, validate its count against the active
  `MaxFrameNum`, resolve the target reference picture using modulo `frame_num` and output order,
  and start native windows at matured non-IDR intra or cyclic intra-refresh P recovery points;
  synthesize bounded unavailable short-term references and verify a real x264 target byte-for-byte.
- [x] Decode frame pictures signalled under an interlaced SPS, consume `field_pic_flag`, preserve
  non-progressive output metadata, and derive type-0/1/2 POC for frame/top/bottom structures;
  verify a real x264 fake-interlaced I/P sequence byte-for-byte against FFmpeg.
- [x] Reconstruct complementary single-slice IDR intra, reference P, and explicit bipredictive B
  fields on field-height canvases; retain a field DPB, build POC-ordered frame groups and
  parity-alternating lists, apply field `PicNum` list modification and all adaptive MMCO
  transitions, pair by frame identity, weave luma/chroma rows, derive output field order from POC,
  and verify I_PCM, P-skip, modified P/B lists, adaptive marking, and B-bi pixels byte-for-byte
  against FFmpeg; reject an incomplete pair at drain.
- [x] Extend complementary IDR I, reference P, and explicit B reconstruction to multi-slice field
  pictures with field-sized macroblock ranges, slice-local entropy/prediction state, per-slice
  deblocking behavior, field reference lists, and complementary weaving; verify I/P/B output
  byte-for-byte against FFmpeg.
- [x] Establish native CAVLC MBAFF frame reconstruction: translate macroblock-pair scan order to
  raster storage, decode frame-coded I/P/B pairs, interleave field-coded `I_PCM` luma/chroma rows,
  retain the resulting frame as a reference, and verify a mixed-pair IDR/P/B sequence byte-for-byte
  against FFmpeg.
- [x] Add the first field-coded CAVLC MBAFF prediction paths: reconstruct Intra16, `P_L0`, and
  B-direct macroblocks on parity-specific field planes, place their residuals into interleaved frame
  rows, and verify a real four-frame x264 MBAFF GOP byte-for-byte against FFmpeg.
- [x] Complete the field-coded CAVLC MBAFF `P_L0` family: parse all 16x16, 16x8, 8x16, `P_8x8`,
  and `P_8x8ref0` macroblock/submacroblock partition shapes; derive mixed frame/field CAVLC
  neighbors; apply field scans, field-aware motion prediction and reference scaling, and
  cross-parity 4:2:0 chroma adjustment; verify a moving six-frame x264 MBAFF GOP byte-for-byte
  against FFmpeg.
- [x] Reconstruct field-coded CAVLC MBAFF Intra4 macroblocks with mixed-pair prediction-mode
  neighbors, field-plane sample prediction, field coefficient scans, residual placement, and
  chroma prediction; verify both a forced field-coded vector and textured x264 MBAFF frames
  byte-for-byte against FFmpeg.
- [x] Complete explicit field-coded CAVLC MBAFF B prediction: implement all 16x16, 16x8, 8x16,
  and `B_8x8` list-0/list-1/bi shapes plus spatial direct, convert motion/reference candidates at
  mixed frame/field edges, and verify forced all-shape vectors and a residual-bearing moving x264
  GOP byte-for-byte against FFmpeg.
- [x] Reconstruct CAVLC Intra8 macroblocks, including four-subblock coefficient interleaving,
  8x8 field scanning, parity-plane prediction, and stepped residual placement; verify a forced
  High Profile x264 MBAFF vector byte-for-byte against FFmpeg.
- [ ] Complete native H.264 reconstruction with fields and their picture-order semantics; retain
  system acceleration only as an optional backend. Remaining MBAFF work includes field-coded
  temporal-direct B prediction, CABAC pair flags/contexts, deblocking across mixed pair modes, and
  multi-slice pair availability.
- [x] Add explainable video-only clean-GOP MP4 remuxing: require IDR/sync boundaries, verify a
  contiguous dependency-closed decode range, preserve encoded sample bytes and display metadata,
  rebuild exact timing/sample tables, and reject rather than round an unsafe cut.
- [x] Start the native H.264 encoder with a deterministic progressive 8-bit 4:2:0 Baseline path:
  serialize SPS/PPS and `avcC`, emit cropped all-IDR `I_PCM` access units with emulation prevention,
  preserve packet timing, expose exact reconstruction, and verify byte-determinism plus native
  decoder pixel round trips.
- [x] Add the first compressed Intra16 mode with reconstructed-neighbor DC/horizontal/vertical
  prediction decisions, luma DC Hadamard plus 4x4 luma/chroma DC/AC transforms and quantization,
  neighbor-derived CAVLC coefficient serialization, normative reconstruction, and independent
  FFmpeg pixel verification.
- [x] Add Intra4 macroblocks with all nine luma prediction modes, reconstructed-neighbor predicted
  mode derivation, coded-block-pattern mapping, neighbor-derived CAVLC contexts, complete
  luma/chroma residuals, constant QP 0 through 51, and native/FFmpeg pixel verification.
- [x] Add the first reference-P encoder path: configurable IP GOPs, periodic Intra4 IDRs, one
  reconstructed short-term reference, P16x16 integer-pixel motion search, predicted vector
  differences, full luma/chroma CAVLC residuals, P-skip runs, frame-number wrap, cropped-canvas
  handling, and native/FFmpeg sequence verification.
- [x] Add adaptive P16x16/P16x8/P8x16 decisions with partition-specific searches, normative
  neighbor predictors, chroma partition prediction, deterministic rate-aware tie-breaking, and
  opt-in mean-luma scene-cut IDRs; force both split shapes through native and FFmpeg verification.
- [x] Add P8x8 macroblocks with independently selected 8x8, 8x4, 4x8, and 4x4 subpartitions;
  refine integer candidates to quarter-pixel luma motion with normative six-tap filtering and
  eighth-sample chroma prediction, and force every subtype plus fractional motion through native
  and FFmpeg verification.
- [x] Add a bounded multiple-reference/B encoder milestone: retain up to four short-term pictures,
  select older P references with matching reference-index motion prediction, and optionally reorder
  one to three non-reference B pictures between anchors. Search list-0/list-1 motion at quarter-pixel
  precision, select uni/bi prediction, preserve presentation PTS with decode-order DTS, drain
  pending pictures as P at flush/GOP boundaries, and verify native plus FFmpeg reconstruction.
- [x] Extend B coding through every B16x16, B16x8, and B8x16 list-direction combination with
  partition-specific motion prediction/search and normative unavailable-list history; force both
  split geometries and a three-B reorder through native plus FFmpeg reconstruction.
- [x] Add every B8x8 sub-macroblock partition down to 4x4, including `B_Direct_8x8`, plus
  macroblock-level spatial-direct and B-skip decisions. Retain future-anchor motion metadata for
  per-8x8 colocated-zero overrides; force mixed direct/explicit subtypes, nonzero direct motion,
  direct residuals, and a multi-macroblock skip run through native plus FFmpeg reconstruction.
- [x] Add picture-wide temporal-direct encoder decisions with colocated-reference identity,
  unwrapped POC-distance scaling, reference-availability fallback, direct residual/B-skip syntax,
  and native plus FFmpeg verification of a nonzero-motion direct picture.
- [x] Add deterministic frame-level target-bitrate control with duration-aware budgets, a bounded
  eight-frame virtual buffer, QP adaptation across IDR/P/B pictures, reconfiguration reset, and
  native plus FFmpeg verification that lower targets reduce size at the expected quality cost.
- [x] Add opt-in activity-based macroblock AQ across Intra16, Intra4, P, and B pictures with
  modulo-52 QP deltas, correct skipped/zero-residual state carry, and native plus FFmpeg pixel
  verification of mixed quiet and textured regions.
- [x] Add opt-in single-entry NAL HRD/VBV scheduling with scaled SPS rate/CPB syntax, 24-bit
  buffering-period and picture-timing SEI delays, duration-aware removal clocks, reordered B output
  delays, shared rate-controller capacity, and explicit CPB violation errors.
- [x] Add `profile=auto|baseline|main|high`, automatically promote B-picture streams to Main and
  Intra8 streams to High Profile, reject incompatible forced profiles, and keep SPS plus `avcC`
  profile declarations identical.
- [x] Add Annex A level negotiation across Levels 1 through 6.2, including Level 1b signalling,
  exact frame-size/rate and dimension checks, DPB/bitrate/CPB constraints, explicit-level
  diagnostics, actual-frame-duration enforcement, and matching SPS/`avcC` declarations.
- [x] Start High Profile encoding with all nine filtered Intra8 prediction directions, normative
  8x8 integer transforms and flat-matrix quantization, CAVLC coefficient interleaving, High SPS/PPS
  syntax, adaptive inter 4x4/8x8 transform decisions for eligible P/B macroblocks, matching native
  CAVLC reconstruction, and FFmpeg reconstruction checks.
- [x] Add High Profile QP-zero transform bypass for lossless Intra4 and inter P/B coding, including
  directional luma/chroma residual DPCM, automatic High Profile selection, incompatible-setting
  validation, exact native reconstruction, and FFmpeg verification.
- [x] Add `scaling_matrix=flat|jvt`, signal the standard High Profile SPS fallback matrices, and
  apply them consistently to Intra16/Intra4/Intra8, chroma, and adaptive P/B 4x4/8x8 quantization
  plus local reconstruction with native and FFmpeg verification.
- [x] Add the CABAC arithmetic encoder core and a complete `entropy=cabac` lossless `I_PCM` IDR
  path with PPS signalling, Main/High profile negotiation, byte alignment, PCM restart, native
  reconstruction, and FFmpeg verification.
- [x] Extend CABAC emission to compressed Intra16 IDRs, including macroblock/chroma modes,
  modulo-52 QP deltas, contextual luma/chroma coded-block flags, significance/last maps, reverse
  coefficient levels and signs, AQ, scaling matrices, and native plus FFmpeg verification.
- [ ] Extend CABAC emission to compressed Intra4/Intra8 and P/B macroblocks.
- [ ] Extend H.264 planning to arbitrary edit boundaries after complete reference semantics exist.
  Treat a production-quality encoder as a later, separate decision.
- [ ] **HEVC:** consider only after the H.264 interfaces expose what the shared model must represent.
- [ ] **AV1:** evaluate as a separate modern-codec slice when it supports a real workflow.
- [ ] **VVC:** defer until ecosystem demand and patent/licensing requirements justify the cost.

## C API and language bindings

**Status:** Experimental versioned one-shot APIs exist for MJPEG, DV, MPEG-2 Video, and MPEG-TS,
with owned buffers, diagnostics, structure-size checks, panic containment, and a compiled C smoke
test. ABI stability is not yet promised.

- [ ] Add opaque stateful handles for demuxers, decoders, encoders, renderers, and muxers.
- [ ] Add streaming packet/frame APIs, seeking, flushing, and end-of-stream semantics.
- [ ] Define caller allocation, callbacks, cancellation, progress, and thread-safety rules.
- [ ] Freeze ABI compatibility and deprecation policy only after real external integration.
- [ ] Generate or maintain C++ convenience wrappers after the C ABI is stable.
- [ ] Add Swift, Python, .NET, or other bindings only in response to an actual application need.

## CLI and high-level Rust API

**Status:** Inspection, decode/encode, verification/comparison, DV audio extraction, MPEG-2
planning, MPEG-TS mux/demux, bounded one-frame `render-plan`/`render`, and the interactive/scripted
terminal editor exist. `benchmark` remains planned.

- [x] Add bounded MPEG-2/Layer II `render-plan` and `render` commands with replacement-frame input,
  dry-run explanation, and explicit audio-end policy.
- [x] Add the interactive/scripted `edit` command over a shared typed command model.
- [ ] Add corpus benchmark commands and machine-readable results.
- [ ] Add optional structured JSON output for inspection, verification, and render plans.
- [ ] Add stdin/stdout and incremental operation where formats permit it.
- [ ] Add consistent progress, verbosity, and diagnostic controls.
- [ ] Add a top-level `mmrecode` facade crate with opt-in codec/container features when the public
  Rust APIs are ready for application use.

## Quality, conformance, and testing

**Status:** Exact frame comparison, MSE, PSNR, deterministic checked-in vectors, hashes/provenance,
malformed samples, and independent FFmpeg interoperability checks exist. `mmrecode-testkit` is
still minimal.

- [ ] Add SSIM and useful per-plane/aggregate reporting.
- [ ] Add difference images, heat maps, histograms, and encoder rate-distortion reports.
- [ ] Expand `mmrecode-testkit` with corpus manifests, external-tool runners, hashing, structured
  reports, determinism checks, and reusable malformed-input assertions.
- [ ] Add continuous fuzzing and mutation/truncation corpora for every parser and C entry point.
- [ ] Add long-duration, discontinuity, damaged-media, and memory-bound tests in addition to small
  permanent vectors.
- [ ] Add normative conformance streams where their licenses allow redistribution; record exact
  provenance and expected results.
- [ ] Cross-check with more than one independent implementation for high-risk syntax.
- [ ] Add performance benchmarks and regression thresholds without mixing them into conformance
  correctness tests.

## Performance and hardware acceleration

**Status:** Current code favors clear deterministic reference implementations. The first profiled
H.264 scalar optimizations are bit-exact and isolated from correctness; broader codecs and
workloads still need measured baselines.

- [ ] Profile representative workloads before choosing optimization targets.
- [ ] Add benchmark coverage for parsing, transforms, motion compensation, color conversion,
  demux/mux, and end-to-end decode/render.
- [ ] Add runtime-dispatched SIMD behind bit-exact scalar fallbacks.
- [ ] Extend dependency-aware parallelism beyond non-reference B pictures to safe reference-picture
  pipelines, independent multi-slice work, and measured wavefront/SIMD kernels while retaining
  deterministic output and bounded memory. Keep cooperative WebAssembly as the scalar fallback.
- [ ] Design hardware decode/encode and GPU surface interop only after buffer ownership is ready.
- [ ] Keep hardware paths optional and retain software reference paths for verification.

## Packaging, compatibility, and release engineering

**Status:** The workspace uses Rust 1.92 and Apache-2.0; the API and C ABI remain experimental.

- [ ] Define MSRV update, semantic-versioning, compatibility, and deprecation policies.
- [ ] Add changelogs, release automation, reproducible builds, and signed release artifacts.
- [ ] Package Rust crates plus C headers and static/dynamic libraries for supported platforms.
- [ ] Add CI coverage for supported macOS, Linux, and Windows targets as portability work begins.
- [ ] Publish third-party notices and keep viewer-only dependencies out of SDK artifacts.
- [ ] Document codec patent/licensing considerations separately from the Apache-2.0 source license.
- [ ] Add end-user API examples, integration guides, and format-support matrices before declaring a
  stable SDK release.

## Example applications and possible products

**Status:** The native viewer is the first application. These are possible demonstrations and
professional tools, not prerequisites for codec correctness.

- [ ] Build an archive ingest/inspect/verify/repair or migration tool around real customer media.
- [ ] Build a small desktop editor that demonstrates copy-versus-reencode decisions visibly.
- [ ] Build a web editor example using WebAssembly once the facade, incremental I/O, and render
  APIs are stable; use proxy media and server/native rendering where browser constraints require it.
- [ ] Consider conformance/reporting products, an SDK/LTS offering, and paid interoperability work
  only after the underlying slices solve a concrete professional need.

## Backlog maintenance

- [ ] Move completed items into the relevant crate README or release notes instead of letting this
  file become a historical changelog.
- [ ] Add a concrete test and acceptance criterion whenever an item is selected for implementation.
- [ ] Keep optional breadth demand-driven: a real file, workflow, or integration should justify each
  new profile, container mode, or codec.
