# MPEG-2 Transport Stream regression vectors

`valid/single-program-mpeg2.ts` is a deterministic 188-byte-packet transport stream containing
one program and one MPEG-2 Video elementary stream. FFmpeg 9.0.1 generated it from `testsrc2`; it
contains no third-party audiovisual material.

Run `tools/generate-mpegts-test-vectors.sh` from the repository root to regenerate the vector.
`SHA256SUMS` makes accidental changes visible. This is interoperability regression material, not
a normative H.222.0 conformance stream.
