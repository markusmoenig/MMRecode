# MMRecode

MMRecode is an experimental, professional media-codec and editing ecosystem written in Rust.
It begins with independently coded production formats and grows toward inter-frame codecs,
container support, verification, and minimal-recompression editing.

The project is currently a scaffold. Its purpose and intended scope are described in
[`concept.md`](concept.md); crate boundaries and dependency rules are described in
[`design.md`](design.md).

## Initial workspace

- `mmrecode-core`: shared media types and codec/container interfaces
- `mmrecode-bitstream`: bit-level readers, writers, VLC support, and start-code utilities
- `mmrecode-mjpeg`: the first codec implementation
- `mmrecode-y4m`: simple uncompressed test input and output
- `mmrecode-quality`: objective frame-comparison utilities
- `mmrecode-testkit`: reusable verification support for codec crates
- `mmrecode-cli`: the `mmrecode` command-line application

## Status

No codec is implemented yet. APIs are intentionally unstable while the first complete vertical
slice is built.

## License

Licensed under the Apache License, Version 2.0. Codec patent licensing, where applicable, is
separate from the copyright license for this source code.

