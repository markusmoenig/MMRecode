# MMRecode TODO

This document is the central index of work that remains across MMRecode. Detailed descriptions of
the current architecture and implemented slices live in [`concept.md`](concept.md),
[`design.md`](design.md), and the individual crate READMEs.

A codec or container marked **slice complete** has a useful, tested vertical slice; it does not mean
that every profile, operating mode, or production optimization in the format is implemented.
Checkboxes are deliberately grouped into near-term work and optional later breadth so that the
roadmap does not turn every possible feature into an immediate commitment.

## Suggested next milestones

1. Add media fingerprints, relinking, and collect/portable-copy behavior to the new versioned
   project document, then extend timeline export recursively into nested media/effect content.
2. Add dedicated interactive `in`/`out` adjustment modes while retaining canonical typed commands
   underneath them.
3. Extend edit delivery to multi-clip audio selection, boundary policy, and MPEG-TS output.
4. Define the typed scene/object boundary and CPU-reference MMFX IR before implementing effects or
   third-party plugins.
5. Extend the new indexed MPEG-2 preview path with incremental TS demux, streaming audio, buffering,
   and backpressure.
6. Add a native MPEG-1 Layer II decoder when audio must move from pass-through/viewer support into
   the reusable codec layer.
7. Extend the native H.264 decoder foundation from raw IDR macroblocks through intra prediction,
   residual decoding, inter prediction, reference management, and deblocking; then extend the new
   clean-GOP remuxer into dependency-aware arbitrary edit boundaries.

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
- [ ] Evaluate a pixel-rendered 24-bit timeline layer for thumbnails, waveforms, curves, and dense
  colored media regions while retaining terminal-native text and controls.
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

**Status:** Architectural direction only. Final effects are CPU-authoritative; GPU execution is an
optional backend. Plugins exchange versioned semantic values rather than internal Rust objects.

- [ ] Define typed scene objects for text, paths, rectangles, images, groups, transforms, layout,
  timing, and animation without tying them to a renderer.
- [ ] Define the safe, bounded MMFX language and typed portable IR, including color, sampling,
  coordinate, edge, precision, and time semantics.
- [ ] Implement a scalar CPU reference backend with deterministic golden tests.
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

**Status:** The H.264 syntax/indexing, MP4/MOV editor import/playback, and video-only clean-GOP
remux slices are implemented.
Pixel reconstruction attempts the native CAVLC and CABAC decoder first and
currently uses an optional bounded FFmpeg process fallback for other reconstruction tools. Native
in-loop deblocking and single-reference CAVLC P slices are also implemented, including skip,
16x16, 16x8, 8x16, and sub-macroblock partitions with fractional-sample motion compensation,
explicit weighted prediction, and inter residuals. The same path accepts the High Profile subset using CAVLC, implicit flat
scaling, and 4x4 transforms. MMRecode itself owns demuxing, timestamps, NAL conversion, SPS/PPS/VUI/slice
parsing, dependency indexing, and seek-window selection. HEVC, AV1, and VVC have not been started.

- [x] **H.264/AVC foundation:** Annex-B and length-prefixed NAL handling, `avcC`, SPS/PPS/VUI and
  leading slice headers, container-timed access-unit indexing, IDR/reference classification, and a
  conservative active-reference dependency index.
- [x] Import, inspect, seek, scrub, and play ordinary non-fragmented H.264 MP4/MOV media in the
  terminal editor, including `project match` from SPS/VUI and container audio/display metadata.
- [ ] Replace conservative dependency sets with complete reference-list modification, decoded
  reference picture marking/MMCO, frame-num gap, field, recovery-point, and POC semantics.
- [x] Add the native Rust decoder foundation behind the playback interface: activate `avcC`
  parameter sets, traverse one frame-coded IDR I-slice, reconstruct 8-bit 4:2:0 `I_PCM` and CAVLC
  `Intra_16x16` and `Intra_4x4` macroblocks with all luma/chroma predictors, neighbor-context coefficient parsing,
  nonzero DC/AC quantization and inverse transforms, normative intra-picture deblocking and slice
  offsets, crop the coded canvas, preserve timing/colour metadata, retain one decoded reference,
  reconstruct single-reference CAVLC P slices with skip, 16x16, 16x8, 8x16, and sub-macroblock
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
- [ ] Complete native H.264 reconstruction with `Intra_8x8`, scaling matrices, remaining CABAC tools,
  B slices, multiple-reference decoded-picture-buffer/reference-list semantics, fields, multi-slice
  filtering rules, recovery points, and complete picture ordering; retain system acceleration only
  as an optional backend.
- [x] Add explainable video-only clean-GOP MP4 remuxing: require IDR/sync boundaries, verify a
  contiguous dependency-closed decode range, preserve encoded sample bytes and display metadata,
  rebuild exact timing/sample tables, and reject rather than round an unsafe cut.
- [ ] Extend H.264 planning to arbitrary edit boundaries after complete reference semantics exist.
  Treat a production encoder as a later, separate decision.
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

**Status:** Current code favors clear deterministic reference implementations. Optimization is
intentionally isolated from correctness.

- [ ] Profile representative workloads before choosing optimization targets.
- [ ] Add benchmark coverage for parsing, transforms, motion compensation, color conversion,
  demux/mux, and end-to-end decode/render.
- [ ] Add runtime-dispatched SIMD behind bit-exact scalar fallbacks.
- [ ] Add frame/slice/task parallelism with deterministic output and bounded memory.
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
