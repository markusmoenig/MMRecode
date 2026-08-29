# DV test vectors

The `valid` directory contains one raw DV25 frame for each initial MMRecode
profile. They were generated independently by FFmpeg 9.0.1 from its `testsrc2`
and `sine` filters. Run `tools/generate-dv-test-vectors.sh` to regenerate them.

Expected SHA-256 checksums are recorded in `valid/SHA256SUMS` after generation.
The vectors are synthetic and contain no third-party audiovisual material.
