# MMRecode

MMRecode is an experimental, professional media-codec and editing ecosystem written in Rust.
It begins with independently coded production formats and grows toward inter-frame codecs,
container support, verification, and minimal-recompression editing.

The project is building its first complete vertical slice. Its purpose and intended scope are described in
[`concept.md`](concept.md); crate boundaries and dependency rules are described in
[`design.md`](design.md).

## Initial workspace

- `mmrecode-core`: shared media types and codec/container interfaces
- `mmrecode-bitstream`: bit-level readers, writers, VLC support, and start-code utilities
- `mmrecode-mjpeg`: the first codec implementation
- `mmrecode-y4m`: simple uncompressed test input and output
- `mmrecode-quality`: objective frame-comparison utilities
- `mmrecode-testkit`: reusable verification support for codec crates
- `mmrecode-capi`: experimental C ABI with an owned-buffer boundary
- `mmrecode-viewer`: native visual frame and JPEG-structure inspection tool
- `mmrecode-cli`: the `mmrecode` command-line application

## Status

The constrained Motion JPEG vertical slice is implemented end to end: multi-frame Y4M input and
output, baseline syntax inspection, reference decoding and encoding, internal encoder
reconstruction, deterministic regression vectors, frame-quality reports, and independent FFmpeg
checks. APIs remain intentionally unstable while coverage and conformance are expanded.

The current codec subset is eight-bit baseline sequential JPEG with a single interleaved scan.
Progressive and multi-scan JPEG, unusual component layouts, CMYK conversion, optimized Huffman
tables, and production-speed integer/SIMD transforms remain future work.

Small permanent media vectors live in [`testdata/`](testdata/README.md), with provenance,
licensing, sizes, and SHA-256 digests recorded in corpus manifests.

## Try it

```sh
cargo run -p mmrecode-cli -- inspect testdata/jpeg/valid/baseline-420.jpg
cargo run -p mmrecode-cli -- \
  decode testdata/jpeg/valid/baseline-420.jpg /tmp/mmrecode-frame.y4m
cargo run -p mmrecode-cli -- \
  encode testdata/y4m/valid/two-frame-420.y4m /tmp/mmrecode.mjpg 85
cargo run -p mmrecode-cli -- \
  verify /tmp/mmrecode.mjpg testdata/y4m/valid/two-frame-420.y4m
```

The experimental C API currently exposes one-shot baseline MJPEG decode and encode. Its checked-in
header is [`crates/capi/include/mmrecode.h`](crates/capi/include/mmrecode.h). Run its compiled C
smoke test with:

```sh
sh crates/capi/tests/run-smoke.sh
```

The C ABI has explicit version and structure-size checks, library-owned output buffers with matching
free functions, thread-local diagnostics, and panic containment. It is usable for integration
experiments but is not yet a compatibility promise.

## Visual inspection

Launch the native viewer with a JPEG, concatenated raw MJPEG stream, or Y4M file:

```sh
cargo run -p mmrecode-viewer -- testdata/jpeg/valid/baseline-420.jpg
```

Files can also be dropped onto the window or opened by entering a path. The viewer provides frame
navigation, fit/manual zoom, nearest-neighbor display, individual Y/Cb/Cr planes, pixel values,
8×8 block overlays, frame and plane metadata, and a collapsible JPEG marker/scan inspector. The
initial CPU display conversion uses BT.601 coefficients; raw plane views remain unconverted so they
can reveal decoder and sampling problems.

The workspace minimum supported Rust version is 1.92. `mmrecode-viewer` pins `eframe` 0.35 because
the following release raised its MSRV beyond 1.92.

## License

Licensed under the Apache License, Version 2.0. Codec patent licensing, where applicable, is
separate from the copyright license for this source code.
