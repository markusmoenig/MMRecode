# mmrecode-h264

`mmrecode-h264` is the codec-owned syntax and dependency layer for AVC. It currently provides:

- Annex-B and ISO/`avcC` length-prefixed NAL splitting and conversion;
- AVC decoder configuration record parsing;
- emulation-prevention removal and Exp-Golomb syntax reading;
- SPS, PPS, VUI, and leading slice-header parsing;
- container-timed access-unit indexing;
- IDR/reference classification and a conservative active-reference dependency set.

The crate does not parse MP4/MOV or encode H.264. Its native decoder foundation reconstructs
single-slice, frame-coded, 8-bit 4:2:0 IDR pictures containing `I_PCM`, CAVLC `Intra_16x16`, or
CAVLC `Intra_4x4` macroblocks. All Intra16, Intra4, and 8x8 chroma prediction modes operate across
the macroblock raster. Neighbor-context CAVLC parsing, DC/AC quantization, inverse transforms, and
the normative in-loop deblocking filter reconstruct nonzero luma and chroma residuals. The CAVLC
P-slice path retains one list-0 reference and
supports `P_Skip`, 16x16, 16x8, 8x16, and 8x8 sub-macroblock partitions down to 4x4. It includes
motion-vector prediction, quarter-sample luma and eighth-sample chroma interpolation, inter
residuals, explicit weighted prediction, mixed intra macroblocks, and motion/coefficient-derived
boundary strengths. Both Baseline and the High Profile subset using CAVLC and 4x4 transforms are
covered. The native CABAC arithmetic engine now handles context
initialization, decision, bypass, termination, and restart operations. Its native CABAC path covers
`I_PCM`, Intra16, Intra8, and Intra4 macroblocks plus P skip/inter partitions from 16x16 down to
4x4, motion, residuals, and QP changes. High Profile intra and inter macroblocks may select the 8x8
luma inverse transform, with matching transform-size/coefficient contexts and transform-aware
deblocking. Sequence and picture scaling lists are parsed with their AVC fallback rules and applied
to native intra/inter 4x4 and luma 8x8 inverse quantization. This is a
normative pixel path covering parameter activation, slice traversal, prediction, reference
retention, macroblock placement, filtering, cropping, colour metadata, and packet timing. B slices,
multiple-reference decoded-picture-buffer
management, reference-list modification, recovery points, fields, multiple slices, and complete
picture ordering remain explicit follow-on work.

The editor's first usable H.264 preview keeps pixel decoding behind `mmrecode-playback`'s bounded
request/event interface. Playback tries the native Rust decoder first and currently invokes an
optional installed FFmpeg process on a native-demuxed Annex-B GOP window only when a stream uses
reconstruction tools not implemented yet. This fallback does not change container, timing,
indexing, seeking, or editor decisions.
