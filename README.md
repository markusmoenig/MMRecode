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
- `mmrecode-playback`: exact fixed-rate timelines and audio-clock synchronization
- `mmrecode-quality`: objective frame-comparison utilities
- `mmrecode-testkit`: reusable verification support for codec crates
- `mmrecode-capi`: experimental C ABI with an owned-buffer boundary
- `mmrecode-viewer`: native visual inspection and synchronized playback tool
- `mmrecode-cli`: the `mmrecode` command-line application

## Status

The constrained Motion JPEG vertical slice is implemented end to end: multi-frame Y4M input and
output, baseline syntax inspection, reference decoding and encoding, internal encoder
reconstruction, deterministic regression vectors, frame-quality reports, and independent FFmpeg
checks. APIs remain intentionally unstable while coverage and conformance are expanded.

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
cargo run -p mmrecode-cli -- inspect testdata/jpeg/valid/baseline-420.jpg
cargo run -p mmrecode-cli -- inspect testdata/dv/valid/dv25-525-60-one-frame.dv
cargo run -p mmrecode-cli -- \
  inspect testdata/mpeg2/valid/main-ml-progressive-ibp.m2v
cargo run -p mmrecode-cli -- \
  plan-mpeg2 testdata/mpeg2/valid/main-ml-progressive-open-gop.m2v 9 10
cargo run -p mmrecode-cli -- \
  decode testdata/mpeg2/valid/main-ml-progressive-ibp.m2v /tmp/mmrecode-mpeg2.y4m
cargo run -p mmrecode-cli -- \
  encode-mpeg2 /tmp/mmrecode-mpeg2.y4m /tmp/mmrecode-roundtrip.m2v 8
cargo run -p mmrecode-cli -- \
  mux-mpegts testdata/mpeg2/valid/main-ml-progressive-ibp.m2v /tmp/mmrecode.ts \
  testdata/mpegaudio/valid/sine-48k-stereo-192k.mp2
cargo run -p mmrecode-cli -- inspect /tmp/mmrecode.ts
cargo run -p mmrecode-cli -- demux-mpegts /tmp/mmrecode.ts /tmp/mmrecode-extracted.m2v
cargo run -p mmrecode-cli -- extract-mpegts-audio /tmp/mmrecode.ts /tmp/mmrecode-audio.mp2
cargo run -p mmrecode-cli -- extract-dv-audio \
  testdata/dv/valid/dv25-525-60-one-frame.dv /tmp/mmrecode-dv.s16le
cargo run -p mmrecode-cli -- \
  decode testdata/dv/valid/dv25-525-60-one-frame.dv /tmp/mmrecode-dv.y4m
cargo run -p mmrecode-cli -- encode-dv /tmp/mmrecode-dv.y4m /tmp/mmrecode-roundtrip.dv
cargo run -p mmrecode-cli -- \
  decode testdata/jpeg/valid/baseline-420.jpg /tmp/mmrecode-frame.y4m
cargo run -p mmrecode-cli -- \
  encode testdata/y4m/valid/two-frame-420.y4m /tmp/mmrecode.mjpg 85
cargo run -p mmrecode-cli -- \
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
which is appropriate for codec vectors; bounded streaming queues are future work for long programs.

The workspace minimum supported Rust version is 1.92. `mmrecode-viewer` pins `eframe` 0.35 because
the following release raised its MSRV beyond 1.92. Viewer audio output uses Rodio, and temporary
pure-Rust MP2 sample decoding uses Symphonia behind Rodio's `symphonia-mp2` feature. No FFmpeg
library or executable is used during playback.

## License

Licensed under the Apache License, Version 2.0. Codec patent licensing, where applicable, is
separate from the copyright license for this source code.
