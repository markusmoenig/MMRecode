# mmrecode-mjpeg

`mmrecode-mjpeg` implements the current Motion JPEG vertical slice.

Implemented:

- typed baseline JPEG marker, table, frame, scan, entropy, restart, application, and comment
  parsing;
- native 8-bit baseline sequential decoding and deterministic encoding;
- grayscale plus planar 4:2:0, 4:2:2, and 4:4:4 shared-frame mappings;
- internal encoder reconstruction and quality/reference checks;
- stateful shared codec interfaces;
- clean independent-picture dependency analysis with splice-parameter fingerprints; and
- lossless packet-aligned cut/concatenation through `mmrecode-render`.

The dependency fingerprint describes decoded stream compatibility—dimensions, precision,
component identifiers and sampling, plus relevant color interpretation—not per-picture
quantization or Huffman tables. Motion JPEG pictures may legitimately carry different tables and
quality levels while remaining independently decodable and splice-compatible.

Remaining codec work includes progressive and multi-scan JPEG, unusual component/color layouts,
optimized Huffman generation, stronger damaged-frame recovery, field-based conventions, and
profiled integer/SIMD acceleration. Selective re-encoding of visually changed frames belongs to
the next render-execution slice; AVI and QuickTime/MOV mapping belong to container crates.
