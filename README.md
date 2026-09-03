# MMRecode

MMRecode is an experimental, professional media-codec and editing ecosystem written in Rust.
It begins with independently coded production formats and grows toward inter-frame codecs,
container support, verification, and minimal-recompression editing.

The project is growing through bounded, complete vertical slices. Its purpose and intended scope are described in
[`concept.md`](concept.md); crate boundaries and dependency rules are described in
[`design.md`](design.md); and remaining work across codecs, containers, editing, playback, APIs,
and release engineering is tracked in [`todo.md`](todo.md).

## Initial workspace

- `mmrecode-core`: shared media types and codec/container interfaces
- `mmrecode-bitstream`: bit-level readers, writers, VLC support, and start-code utilities
- `mmrecode-mjpeg`: the first codec implementation
- `mmrecode-mpegaudio`: MPEG-1 Audio Layer II framing and timing for pass-through
- `mmrecode-dv`: raw DV25 DIF parsing, validation, metadata, and embedded audio
- `mmrecode-mpeg2`: MPEG-2 Video parsing, I/P/B reconstruction/encoding, and dependency planning
- `mmrecode-mpegts`: 188-byte MPEG-2 Transport Stream demuxing and deterministic muxing
- `mmrecode-y4m`: simple uncompressed test input and output
- `mmrecode-edit`: recursive linked-media authoring, typed editor commands, and flattened render intent
- `mmrecode-render`: explicit render planning, minimal-recompression execution, and optional
  MPEG-TS delivery
- `mmrecode-playback`: exact timelines, audio-clock synchronization, and indexed MPEG-2 preview
- `mmrecode-quality`: objective frame-comparison utilities
- `mmrecode-testkit`: reusable verification support for codec crates
- `mmrecode-capi`: experimental C ABI with an owned-buffer boundary
- `mmrecode-viewer`: native visual inspection and synchronized playback tool
- `mmrecode`: the main terminal editor and codec-tool application

## Status

The constrained Motion JPEG vertical slice is implemented end to end: multi-frame Y4M input and
output, baseline syntax inspection, reference decoding and encoding, internal encoder
reconstruction, deterministic regression vectors, frame-quality reports, and independent FFmpeg
checks. APIs remain intentionally unstable while coverage and conformance are expanded.

The first codec-independent editing slice is implemented in `mmrecode-edit` and
`mmrecode-render`. The project and every media node now expose an ordered local timeline of linked
child media without artificial track/folder levels. Stable media and placement identities support
recursive paths, aliases, reuse, cycle rejection, exact local source/timeline ranges, and typed
project lifecycle, `import`/`cd`/`ls`/`add`/`in`/`out` commands with undo/redo. `mmrecode edit` and
`mmrecode edit <script>`
share one parser and session. The existing flattened intent validates sources, streams, tracks,
clips, time ranges, effects, transitions, and output intent, then plans and executes packet-aligned
cuts and concatenation for independently coded video. The executor preserves encoded payloads and side data while rebasing PTS/DTS and
stream identifiers; real DV and MJPEG integration tests prove reordered output without
re-encoding. The generic inter-frame planner now consumes decode-order reference graphs, propagates
frame-aligned changes, identifies unchanged decode preroll, and reserves exact copy and bridge-
encode packet slots. Its decisions match the MPEG-2 codec-local smart-render planner on real I/P/B
test data. It now selects frame-accurate ranges across multiple compatible sources, regenerates
references damaged by a cut, and returns to byte-preserving copy at the next safe point. With the
opt-in `mmrecode-render/mpeg2` feature, the native executor accepts replacement frames, encodes
closed bridge GOPs, preserves unaffected packet payloads, and validates the final splice with both
MMRecode and FFmpeg. The opt-in `mpegts` feature adds a dry-runnable direct mux
path for those packets plus optional MPEG-1 Layer II audio. It reports copied/regenerated work and
uses an explicit exact/contained/cover complete-audio-frame policy; the resulting A/V transport is
validated by native demux/decode and FFmpeg. Bridge encoding now preserves aspect, display/colour,
profile/level, and all four quantizer matrices; recomputes closed-GOP timecode from the source
origin; and reports deliberate bitrate, VBV-buffer, and picture-delay rewrites. Versioned project
persistence and the first one-root-placement graph-to-render compiler are implemented; recursive
composition compilation, sample-level audio editing, multi-clip audio, transitions, and production
VBV continuity remain subsequent slices. Existing long-form render commands are development
harnesses; the intended editor surface is a shared typed command language for script and
interactive terminal modes.

The second codec slice is implemented in `mmrecode-dv`. It recognizes
525/60 and 625/50 DV25, indexes and validates every 80-byte DIF block, retains subcode/VAUX/AAUX
packs with typed timecode and audio-source metadata, extracts 16-bit linear or 12-bit nonlinear
embedded audio, reconstructs native 4:1:1/4:2:0 video, and deterministically encodes video,
16-bit stereo audio, and timecode. Both standard vectors pass independent FFmpeg decoding; decoded
samples differ by at most one and extracted PCM is byte-identical for both systems. Raw DV25 is
also connected to the codec/dependency APIs, CLI, native viewer, and experimental C ABI.

The third vertical slice is implemented in `mmrecode-mpeg2`. It parses MPEG-2 Video elementary
streams into typed sequence/display/quant-matrix, GOP, picture, extension, and slice structures;
reconstructs progressive and interlaced Main Profile 4:2:0 frame pictures in presentation order;
and deterministically encodes constrained Main Profile/Main Level closed-GOP I/P/B streams. Open
GOP dependencies, recovery points, and bridge-encode propagation are exposed through an
explainable smart-render plan. Progressive, interlaced, open-GOP, malformed-input, nonzero-motion,
and native-encoder vectors are checked against FFmpeg. Current limits—including field pictures,
dual-prime prediction, chroma profiles, and production VBV rate control—are explicit in
[`crates/codecs/mpeg2/README.md`](crates/codecs/mpeg2/README.md).

The H.264 vertical slice owns ISO-BMFF sample parsing, AVC syntax, timing, dependency indexing,
editor import/seek/playback, `project match`, and clean-GOP video remuxing. Its native Rust decoder
currently reconstructs CAVLC I/P pictures for the documented single-reference 4x4-transform
subset, plus CABAC `I_PCM`, Intra16, Intra8, and Intra4 IDRs. CABAC P pictures support skip, 16x16,
16x8, 8x16, and 8x8 partitions down to 4x4, mixed Intra4/Intra16/PCM macroblocks, motion,
luma/chroma residuals, QP changes, resolved SPS/PPS scaling matrices, 4x4/8x8 luma transforms,
filtering, and QP-zero transform bypass for lossless Intra4 and inter residuals. Unsupported B
slices and fuller reference semantics remain on the explicit optional playback fallback path; no
H.264 encoder has been started.

The first container slice is implemented in `mmrecode-mpegts`. It validates 188-byte transport
packets, continuity, PAT/PMT PSI and CRCs; discovers programs and streams; reassembles MPEG-2 PES
with PTS/DTS and PCR timing; and deterministically muxes timed MPEG-2 Video with optional MPEG-1
Audio Layer II. Native A/V output is accepted by FFmpeg, while independent FFmpeg vectors exercise
demuxing. The current audio-codec, M2TS, service-table, scrambling, CBR, and seeking boundaries are documented in
[`crates/containers/mpegts/README.md`](crates/containers/mpegts/README.md).

The current codec subset is eight-bit baseline sequential JPEG with a single interleaved scan.
Progressive and multi-scan JPEG, unusual component layouts, CMYK conversion, optimized Huffman
tables, and production-speed integer/SIMD transforms remain future work.

Small permanent media vectors live in [`testdata/`](testdata/README.md), with provenance,
licensing, sizes, and SHA-256 digests recorded in corpus manifests.

## Try it

```sh
cargo run -p mmrecode -- inspect testdata/jpeg/valid/baseline-420.jpg
cargo run -p mmrecode -- inspect testdata/dv/valid/dv25-525-60-one-frame.dv
cargo run -p mmrecode -- \
  inspect testdata/mpeg2/valid/main-ml-progressive-ibp.m2v
cargo run -p mmrecode -- \
  plan-mpeg2 testdata/mpeg2/valid/main-ml-progressive-open-gop.m2v 9 10
cargo run -p mmrecode -- \
  decode testdata/mpeg2/valid/main-ml-progressive-ibp.m2v /tmp/mmrecode-mpeg2.y4m
cargo run -p mmrecode -- \
  render-plan testdata/mpeg2/valid/main-ml-progressive-ibp.m2v \
  --replace 3 /tmp/replacement.y4m \
  --audio testdata/mpegaudio/valid/sine-48k-stereo-192k.mp2 --audio-end exact
cargo run -p mmrecode -- \
  render testdata/mpeg2/valid/main-ml-progressive-ibp.m2v /tmp/mmrecode-render.ts \
  --replace 3 /tmp/replacement.y4m \
  --audio testdata/mpegaudio/valid/sine-48k-stereo-192k.mp2 --audio-end exact
cargo run -p mmrecode -- \
  encode-mpeg2 /tmp/mmrecode-mpeg2.y4m /tmp/mmrecode-roundtrip.m2v 8
cargo run -p mmrecode -- \
  mux-mpegts testdata/mpeg2/valid/main-ml-progressive-ibp.m2v /tmp/mmrecode.ts \
  testdata/mpegaudio/valid/sine-48k-stereo-192k.mp2
cargo run -p mmrecode -- inspect /tmp/mmrecode.ts
cargo run -p mmrecode -- demux-mpegts /tmp/mmrecode.ts /tmp/mmrecode-extracted.m2v
cargo run -p mmrecode -- extract-mpegts-audio /tmp/mmrecode.ts /tmp/mmrecode-audio.mp2
cargo run -p mmrecode -- extract-dv-audio \
  testdata/dv/valid/dv25-525-60-one-frame.dv /tmp/mmrecode-dv.s16le
cargo run -p mmrecode -- \
  decode testdata/dv/valid/dv25-525-60-one-frame.dv /tmp/mmrecode-dv.y4m
cargo run -p mmrecode -- encode-dv /tmp/mmrecode-dv.y4m /tmp/mmrecode-roundtrip.dv
cargo run -p mmrecode -- \
  decode testdata/jpeg/valid/baseline-420.jpg /tmp/mmrecode-frame.y4m
cargo run -p mmrecode -- \
  encode testdata/y4m/valid/two-frame-420.y4m /tmp/mmrecode.mjpg 85
cargo run -p mmrecode -- \
  verify /tmp/mmrecode.mjpg testdata/y4m/valid/two-frame-420.y4m
```

The experimental C API currently exposes one-shot baseline MJPEG, raw DV25, complete MPEG-2
elementary-stream decode/encode, and MPEG-TS video/audio mux/demux. Its checked-in
header is [`crates/capi/include/mmrecode.h`](crates/capi/include/mmrecode.h). Run its compiled C
smoke test with:

```sh
sh crates/capi/tests/run-smoke.sh
```

The C ABI has explicit version and structure-size checks, library-owned output buffers with matching
free functions, thread-local diagnostics, and panic containment. It is usable for integration
experiments but is not yet a compatibility promise.

## Terminal preview and first editor loop

The CLI has a real terminal-graphics preview for MPEG-2 Video elementary streams and MPEG-TS:

```sh
cargo run --release -p mmrecode -- preview projects/output.ts
```

It queries the active terminal and selects Kitty graphics, Sixel, iTerm2 images, or a portable
24-bit Unicode half-block renderer. Kitty-compatible terminals include Kitty and Ghostty; the
native Kitty path uses local RGB transfer and two client-switched image slots so playback does not
erase the previous image while preparing the next one. The fallback still provides a recognizable
full-colour moving preview in ordinary true-colour terminals. Space
plays or pauses, Left/Right step, Home/End seek, `l` toggles looping, and `q` quits. MPEG-2 decoding
and fallback terminal image resize/encoding run on separate workers, with bounded decoded-frame
caching and buffering.

The same backend now powers the first real interactive editor shell. Start it without a long
argument list (or spell out `edit`); the complete full-screen workspace appears even before media
is loaded:

```text
$ cargo run --release -p mmrecode
# then type in the full-screen prompt:
Untitled > import projects/output.ts as Clip0
Untitled > save as MyFilm
```

The empty workspace already contains the monitor, contextual help/inspector, graphical timeline,
result area, and prompt. `import` probes the MPEG-2 ES/TS source, adds a real external media node,
enters its placement, and fills that workspace in place. The compact monitor sits beside a
hierarchy-aware inspector: at the root it shows project metadata; inside video it shows placement
timing, source origin, dimensions, chroma, scan mode, frame rate, bit rate, profile, and current
picture type. `help` restores the quick command overview, `man <command>` shows detailed help, and
`info`, `info project`, `info video`, `info audio`, or `info source` select contextual metadata.
A successful `in` or `out` temporarily focuses the panel on that boundary and exposes `left
<time>` / `right <time>` follow-ups. The lower timeline now includes a time ruler, retained and
trimmed regions, I-picture landmarks, and the playhead. Type canonical editor commands directly
below it; for example, `in +0:10`, `out -0:10`, `undo`, and `redo`. Time is compact frame
timecode counted from the right: `1:15` means one second and fifteen frames, `2:01:15` means two
minutes, one second, and fifteen frames, and leading zero fields are omitted. A successful `in` or `out`
immediately changes the playable range without unexpectedly moving an already valid playhead.
Up/Down recalls command history and restores an unfinished draft after the newest entry. History
is persisted across interactive MMRecode launches in the operating system's conventional
application-state directory. Tab completes command names, `man`/`info` topics, project settings,
project/export presets, hierarchy aliases after `cd`, and filesystem paths after `open`, `import`,
`save as`, or `export`; paths containing spaces are quoted automatically.
Click or drag anywhere in the timeline to scrub, or use Left/Right for one frame,
Shift-Left/Right for ten frames, and Page Up/Page Down for roughly one second. Ctrl-Space toggles
playback; playing while parked at the out-point restarts from the in-point. Home/End seeks to the
edit boundaries, and Ctrl-Z/Ctrl-Y undo or redo. Ctrl-Q leaves a clean editor; unsaved projects
must first be saved or explicitly closed with `quit --discard`.

`mmrecode edit <script>` uses the same typed commands; relative media paths resolve from the script
directory and it does not launch a terminal UI. `new`, project `open`, `save`/`save as`, media
`import`, project presets/settings, `export plan`, and `export` share the same host requests as the
interactive UI. Project files are readable, versioned JSON with project-relative managed origins
and explicit external links. `save as MyFilm` writes `MyFilm.mmrecode`; the extension is always
appended when omitted, and the initial Untitled project adopts `MyFilm` as its name. Export always
renders the complete project-root timeline regardless of the current `cd` context. The MPEG-2/TS
slice renders all root MPEG-2 placements, trims, positions, gaps, rate differences, and scale
modes. A single fully compatible placement may use packet-preserving smart rendering internally;
other supported timelines are decoded, composited in project order, re-encoded, and muxed.
Relinking/collection, nested generated/effect composition, alpha, audio, and dedicated one-key
adjustment modes remain subsequent slices.

Project frame rate remains editable after media is placed:

```text
project match
project set rate 25
project set rate 30000/1001 conform time
project set rate 24 conform frames
```

After import, `project match` adopts the focused media's video canvas, exact frame rate, pixel
aspect, scan mode, and working color in one undoable operation. Supported container audio also
supplies sample rate and channel count; media without supported audio leaves project audio settings
unchanged. Root placement times are conformed to preserve presentation time.

`conform time` is the default and preserves presentation time by rescaling direct root placement
boundaries to the nearest frame at the new rate. `conform frames` preserves their integer frame
numbers, intentionally changing presentation time. Both are undoable; source in/out ranges and
nested media time bases remain untouched. The `mpeg2-ts` exporter packet-copies compatible input
and automatically switches to full rendering for a source-rate or canvas mismatch. `scale fit`
(the default), `scale fill`, `scale stretch`, and `scale native` define placement sizing. The
current CPU render path is progressive YUV 4:2:0, supports MPEG frame rates through 60 fps and even
canvases through 1920x1152, and does not yet mix audio. `man project`, `man scale`, and `man export`
document the complete behavior and current boundaries.

## Visual inspection

Launch the native viewer with MPEG-TS, MPEG-2 Video, raw DV, JPEG/MJPEG, or Y4M:

```sh
cargo run -p mmrecode-viewer -- testdata/jpeg/valid/baseline-420.jpg
cargo run -p mmrecode-viewer -- testdata/dv/valid/dv25-625-50-one-frame.dv
cargo run -p mmrecode-viewer -- \
  testdata/mpeg2/valid/main-ml-progressive-open-gop.m2v
cargo run -p mmrecode-viewer -- testdata/mpegts/valid/single-program-mpeg2-mp2.ts
```

Files can also be dropped onto the window or opened by entering a path. Space or the toolbar starts
and pauses playback; the player also supports stop, seeking, frame stepping, looping, and volume.
MPEG-TS MPEG Layer II and complete raw-DV audio are decoded to PCM before playback, and the rendered
audio position is the master clock for video. MPEG-2 Video, Y4M, and raw Motion JPEG animate without
audio; Y4M's declared frame rate is honored, while raw Motion JPEG defaults to an explicitly marked
25 fps because it has no container timeline.

The viewer also provides fit/manual zoom, nearest-neighbor display, individual Y/Cb/Cr planes, pixel values,
8×8 block overlays, frame and plane metadata, and a collapsible JPEG marker/scan inspector. The
initial CPU display conversion uses BT.601 coefficients; raw plane views remain unconverted so they
can reveal decoder and sampling problems.

For raw DV, the viewer displays decoded pixels and can switch to a color-coded physical DIF map
with frame profile, timecode, embedded-audio layout, metadata-pack count, and byte-localized
structural issues.

For MPEG-2, frames are presented in display order while the inspector retains decode order,
temporal reference, I/P/B type, byte range, random-access strength, references, slices, profile,
VBV, field flags, and colour metadata. A macroblock-map view distinguishes intra, predicted,
skipped, B-picture, and field-predicted regions.

For MPEG-TS, the same decoded picture and macroblock views are augmented with transport-packet,
PAT/PMT, program, PID, stream-type, PES, PCR, and MPEG Layer II audio summaries. First video and
audio PTS values establish the playback alignment. The viewer currently predecodes complete media,
except for MPEG-2 video: MPEG-2 ES/TS opening now builds a lightweight presentation/GOP index,
decodes requested pictures on a worker from the closest clean random-access point, and retains at
most 36 decoded pictures and macroblock maps. MPEG Layer II audio and the older DV/MJPEG/Y4M paths
remain eager; incremental transport demux and audio queues are still future work for long programs.
Starting playback waits for 12 contiguous preview frames; a file underflow pauses both clocks and
resumes automatically after replenishing that buffer instead of repeatedly superseding decode work.

The workspace minimum supported Rust version is 1.92. `mmrecode-viewer` pins `eframe` 0.35 because
the following release raised its MSRV beyond 1.92. Viewer audio output uses Rodio, and temporary
pure-Rust MP2 sample decoding uses Symphonia behind Rodio's `symphonia-mp2` feature. No FFmpeg
library or executable is used during playback.

## License

Licensed under the Apache License, Version 2.0. Codec patent licensing, where applicable, is
separate from the copyright license for this source code.
