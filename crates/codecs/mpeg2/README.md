# mmrecode-mpeg2

`mmrecode-mpeg2` is MMRecode's native safe-Rust MPEG-2 Video elementary-stream slice. It is an
end-to-end reference implementation and inspection surface, not yet a claim of complete H.262
conformance or production encoder performance.

## Implemented

- Byte-localized start-code scanning and typed sequence, sequence-display, quant-matrix, GOP,
  picture, picture-coding-extension, user-data, and slice parsing
- Main Profile 4:2:0 frame-picture reconstruction for progressive and interlaced sequences
- I, P, and B pictures; frame and field motion prediction within frame pictures; frame and field
  DCT organization; linear/non-linear quantization; alternate scan; MPEG-2 intra VLC; mismatch
  control; separate luma/chroma quant matrices
- Decode/presentation reordering, closed/open GOP reference graphs, clean/recovery random access,
  parameter fingerprints, and explainable smart-render damage propagation
- Deterministic Main Profile/Main Level closed-GOP encoder with I/P/B ordering, integer-pixel P
  motion search, bidirectional B prediction, slice rows, native reconstruction, and VBR delay
  signalling under the Main Level header bounds
- Shared Rust codec adapters, one-shot C ABI, CLI inspect/decode/encode/verify/plan commands, and a
  viewer macroblock/dependency inspection mode
- Permanent progressive, interlaced, closed-GOP, and open-GOP vectors with malformed-input tests
  and independent FFmpeg reconstruction checks for both decoder and native encoder output

## Deliberate current limits

- MPEG-1 Video, field pictures, and dual-prime prediction are rejected with explicit errors.
- 4:2:2 and 4:4:4 profiles, scalability extensions, and high-bit-depth extensions are not decoded.
- The encoder emits frame pictures only. B motion is currently zero-vector bidirectional rather
  than searched, and there is no production VBV scheduler or adaptive rate-control loop.
- Damaged-slice concealment, slice/thread parallelism, SIMD, hardware acceleration, transport
  streams, program streams, MXF, and other container mappings remain separate follow-on work.

These boundaries are intentional: supported streams are reconstructed and externally checked;
unsupported syntax is surfaced instead of being silently approximated.

## References and verification

The implementation follows ITU-T H.262 / ISO/IEC 13818-2 syntax and normative tables. FFmpeg's
independent MPEG-2 decoder and encoder are used as interoperability oracles in optional regression
tests when `ffmpeg` is installed. The repository vectors are small deterministic regression media,
not a normative conformance suite.
