# MMRecode Test Data

This directory contains small, permanent regression vectors shared by codec,
container, CLI, and verification tests.

## Policy

- Commit only small vectors with known provenance and redistribution rights.
- Record every committed vector in `manifests/` with its SHA-256 digest.
- Keep malformed streams: each one represents behavior that must not regress.
- Do not silently replace a vector. Update its manifest entry and explain why.
- Keep large or third-party corpora outside Git. A future corpus manifest may
  identify those by URL, local path, license, size, and digest.
- A file under `valid/` must be independently decodable. Files under `parser/`
  exercise marker syntax and are not necessarily complete decodable images.

## JPEG corpus

```text
jpeg/
├── valid/       Independently decodable baseline JPEG images
├── encoded/     Deterministic output from the MMRecode encoder
├── parser/      Synthetic marker and entropy-boundary cases
└── invalid/     Intentionally malformed input
y4m/
└── valid/       Source frames for encoder and quality regressions
```

The valid images are generated from FFmpeg's built-in filters, so they do not
contain third-party artwork. `jpeg/valid/unknown-app-marker.jpg` is derived from
`baseline-420.jpg` by inserting an opaque APP2 marker after SOI; this does not
alter the encoded image.

Regenerate the corpus from the repository root with:

```sh
rustc testdata/tools/generate_jpeg_vectors.rs \
  -o /tmp/mmrecode-generate-jpeg-vectors
/tmp/mmrecode-generate-jpeg-vectors
```

Regeneration requires FFmpeg. Encoder output can change between FFmpeg
versions, so review binary and manifest digest changes rather than assuming
they are mechanical.
