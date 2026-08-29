# `mmrecode-dv`

This crate is the native Rust raw-DV implementation. The first supported profiles are 25 Mb/s
consumer DV for the 525/60 and 625/50 systems. AVI-DV, QuickTime DV, DVCPRO50, and DVCPRO HD are
separate later extensions rather than hidden assumptions in the DV25 parser.

Implemented now:

- Fixed-size profile detection
- Complete 80-byte DIF block index and canonical layout validation
- Inspectable damaged frames with byte-localized issues
- Raw subcode, VAUX, and AAUX pack retention
- Typed SMPTE timecode and AAUX audio-source packs
- 16-bit linear and 12-bit nonlinear audio extraction and unshuffle
- Native DCT/VLC video reconstruction with three-level coefficient spill
- Deterministic DV25 video encoding with internal reconstruction
- 16-bit stereo audio shuffle/encoding and SMPTE timecode writing
- Streaming codec interfaces and independent-frame dependency analysis
- CLI, native viewer, and experimental C ABI integration
- Independent 525/60 and 625/50 regression vectors

The current encoder deliberately uses frame-DCT mode. Adaptive 2-4-8 transform selection, DVCPRO
profiles, and AVI/QuickTime wrapping are follow-on extensions rather than missing parts of the raw
DV25 baseline.

Primary format references used for this implementation:

- SMPTE ST 396, 25 Mb/s DIF sequence transmission order:
  <https://pub.smpte.org/pub/st396/st0396-2003.pdf>
- IETF RFC 3189, DV RTP payload and DIF organization:
  <https://datatracker.ietf.org/doc/rfc3189/>
- FFmpeg's official DV implementation as an independent interoperability oracle:
  <https://ffmpeg.org/doxygen/8.0/libavformat_2dv_8c_source.html>

The implementation is original Apache-2.0 Rust code. FFmpeg is used to generate and independently
decode synthetic regression vectors; no FFmpeg library is linked or required at runtime.
