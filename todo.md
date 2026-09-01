# MMRecode TODO

This document is the central index of work that remains across MMRecode. Detailed descriptions of
the current architecture and implemented slices live in [`concept.md`](concept.md),
[`design.md`](design.md), and the individual crate READMEs.

A codec or container marked **slice complete** has a useful, tested vertical slice; it does not mean
that every profile, operating mode, or production optimization in the format is implemented.
Checkboxes are deliberately grouped into near-term work and optional later breadth so that the
roadmap does not turn every possible feature into an immediate commitment.

## Suggested next milestones

1. Build the first `mmrecode-edit` and `mmrecode-render` vertical slice around independent-frame
   MJPEG/DV cuts, packet copying, selective re-encoding, timestamp rewriting, and muxing.
2. Extend that executor to MPEG-2 GOP-aware bridge encoding using the dependency plans already
   produced by `mmrecode-mpeg2`.
3. Add bounded streaming, indexing, seeking, and decode queues to playback and the viewer.
4. Add AVI as the first additional production container for MJPEG and DV workflows.
5. Add a native MPEG-1 Layer II decoder when audio must move from pass-through/viewer support into
   the reusable codec layer.
6. Start H.264 only after the edit/render interfaces have been exercised by the existing codecs.

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
command plans damage propagation; it does not yet execute an edited render.

- [ ] Execute bridge plans through `mmrecode-render`: copy unaffected pictures, decode the damaged
  region, bridge-encode it, rewrite timestamps, and mux the result.
- [ ] Match source sequence/GOP parameters at bridge boundaries and validate splice continuity.
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
deterministic A/V muxing.

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
models sources, streams, tracks, clips, exact ranges, effects, transitions, and output intent.
`mmrecode-render` plans and executes packet-aligned, independent-frame DV and MJPEG
cut/concatenate paths with payload and side-data preservation plus exact timestamp rewriting. It
does not yet drive a muxer directly or regenerate changed frames.

- [x] Create `mmrecode-edit` with sources, tracks, clips, ranges, transitions, effects, and output
  intent without codec-specific syntax.
- [x] Create `mmrecode-render` with explicit operations such as `CopyPackets`,
  `RewriteTimestamps`, `Decode`, `ApplyEffects`, `BridgeEncode`, `FullEncode`, and `Mux`.
- [x] Implement the first independent-frame cut/concatenate path with DV.
- [ ] Drive a selected container muxer directly instead of returning container-ready packets.
- [x] Add an MJPEG dependency analyzer and connect the same independent-frame path.
- [ ] Implement MPEG-2 GOP-aware cuts and bridge encoding using `DependencyAnalyzer` output.
- [ ] Define exact edit-boundary rules for video frames, audio samples, preroll, and A/V sync.
- [x] Verify codec-parameter compatibility and preserve packet flags and side data in the initial
  packet-copy path.
- [ ] Make every render plan explainable: copied/reencoded ranges, causes, dependencies, expected
  quality loss, and byte/time estimates.
- [ ] Add deterministic output, cancellation, progress reporting, and recoverable failure handling.
- [ ] Add transitions/effects only after the copy/reencode boundary machinery is verified.
- [ ] Expose stable editing/rendering entry points through the C API after the Rust model settles.

## Playback engine

**Status:** Exact fixed-rate timelines, play/pause/stop/seek/step/loop behavior, and wall/external
audio clocks exist. Codec-vector playback is functional; long media is still predecoded by the
viewer.

- [ ] Add timestamp-indexed variable-frame-rate timelines.
- [ ] Add bounded packet, decode, audio, and presentation queues with backpressure.
- [ ] Add keyframe-aware seeking, decoder preroll, and discontinuity flushing.
- [ ] Add reusable audio resampling, channel conversion, device-format negotiation, and clock
  latency/drift compensation.
- [ ] Define buffering and underflow behavior for files and live inputs.
- [ ] Add playback-rate support only after clock and resampling behavior is stable.

## Native viewer

**Status:** JPEG/MJPEG, DV, MPEG-2 ES, MPEG-TS, and Y4M inspection/playback work, including
synchronized TS/DV audio, plane and pixel inspection, JPEG markers, DV DIF maps, and MPEG-2
macroblock/dependency information.

- [ ] Move long-file operation from complete predecode to the playback engine's bounded queues.
- [ ] Add timeline navigation by timestamp, keyframe, GOP, PES, error, and marker.
- [ ] Add slice-boundary and motion-vector overlays.
- [ ] Add audio waveform, meters, channel selection, and device selection where useful.
- [ ] Replace or supplement CPU RGB conversion with GPU planar presentation and a controlled color
  pipeline; add HDR only with formats that need it.
- [ ] Allow the user to choose the assumed frame rate for raw MJPEG.
- [ ] Use the native MPEG Audio decoder once it exists, removing the viewer-only MP2 decode path.
- [ ] Package a distributable macOS application after the interfaces stabilize.

## Additional containers

**Status:** MPEG-TS and Y4M are the only container/file-format slices. Each item below should be its
own crate and bounded vertical slice.

- [ ] **AVI:** prioritize OpenDML indexing, MJPEG, DV, PCM, and metadata preservation.
- [ ] **QuickTime/MOV and ISO BMFF/MP4:** start with sample tables, timestamps, seeking, and the
  codecs actually implemented by MMRecode; keep MOV-specific behavior explicit.
- [ ] **MPEG Program Stream/VOB:** add PES, SCR, navigation/private-stream handling, and MPEG-2/MP2
  mappings as required by archive/DVD workflows.
- [ ] **MXF:** begin with one operational pattern and concrete DV/MPEG-2 archive samples rather than
  attempting all of SMPTE MXF at once.
- [ ] Add container-independent indexing and metadata-preservation tests shared by these crates.

## Future video codecs

**Status:** No H.264, HEVC, AV1, or VVC crate has been started. These should not block proving the
editing architecture with the current codecs.

- [ ] **H.264/AVC:** begin with Annex B/length-prefixed parsing, parameter sets, access units,
  reference dependencies, random access, and smart-render planning; then add native decode. Treat a
  production encoder as a separate decision.
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
planning, and MPEG-TS mux/demux commands exist. `edit` and `benchmark` remain planned.

- [ ] Add `edit`/`render-plan`/`render` commands with dry-run explanations.
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
