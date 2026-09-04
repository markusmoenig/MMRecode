# mmrecode-h264

`mmrecode-h264` is the codec-owned syntax and dependency layer for AVC. It currently provides:

- Annex-B and ISO/`avcC` length-prefixed NAL splitting and conversion;
- AVC decoder configuration record parsing;
- emulation-prevention removal and Exp-Golomb syntax reading;
- SPS, PPS, VUI, and leading slice-header parsing;
- container-timed access-unit indexing;
- IDR/reference classification and a conservative active-reference dependency set.

The crate does not parse MP4/MOV or encode H.264. Its native decoder foundation reconstructs
frame-coded, 8-bit 4:2:0 IDR pictures containing `I_PCM`, CAVLC `Intra_16x16`, or
CAVLC `Intra_4x4` macroblocks. All Intra16, Intra4, and 8x8 chroma prediction modes operate across
the macroblock raster. Neighbor-context CAVLC parsing, DC/AC quantization, inverse transforms, and
the normative in-loop deblocking filter reconstruct nonzero luma and chroma residuals. The CAVLC
P-slice path retains a bounded short-term decoded-picture buffer and supports default-list
reference indices for `P_Skip`, 16x16, 16x8, 8x16, and 8x8 sub-macroblock partitions down to 4x4.
It includes motion-vector prediction, quarter-sample luma and eighth-sample chroma interpolation,
inter residuals, single-reference explicit weighted prediction, mixed intra macroblocks, and motion/coefficient-derived
boundary strengths. Both Baseline and the High Profile subset using CAVLC and 4x4 transforms are
covered. The native CABAC arithmetic engine now handles context
initialization, decision, bypass, termination, and restart operations. Its native CABAC path covers
`I_PCM`, Intra16, Intra8, and Intra4 macroblocks plus P skip/inter partitions from 16x16 down to
4x4, default-list multiple-reference selection, short-term list-0 reordering, motion, residuals,
and QP changes. Its frame-picture DPB applies sliding-window and adaptive MMCO marking, supports
short- and long-term list-0 modification, and accepts IDR/current-picture long-term assignment.
POC type-0 tracking also drives default B reference lists, with native CAVLC reconstruction for
16x16 list-0, list-1, unweighted bidirectional, spatial-direct, and skipped macroblocks, plus all
explicit 16x8/8x16 L0/L1/Bi combinations and all `B_8x8` subtypes down to 4x4. Spatial and
temporal direct prediction cover whole, skipped, and `B_Direct_8x8` macroblocks, including both
SPS direct-inference granularities and POC-scaled colocated motion. Explicit weight tables and
implicit POC-distance weights apply across CAVLC B prediction modes.
High Profile intra and inter macroblocks may select the 8x8 luma inverse transform,
with matching transform-size/coefficient contexts and transform-aware
deblocking. Sequence and picture scaling lists are parsed with their AVC fallback rules and applied
to native intra/inter 4x4 and luma 8x8 inverse quantization. This is a
normative pixel path covering parameter activation, slice traversal, prediction, reference
retention, macroblock placement, filtering, cropping, colour metadata, and packet timing. B-picture
deblocking compares both reference lists by picture identity. CABAC B slices reconstruct skip and
direct macroblocks, every explicit 16x16/16x8/8x16 and `B_8x8` prediction form, embedded intra
macroblocks, temporal direct, implicit weighting, and High Profile 8x8 residuals. Recovery-point
SEI metadata is parsed and attached to indexed access units without misclassifying it as an IDR,
and native non-IDR I pictures can establish an intra-only entry without prior reference state.
CAVLC and CABAC multi-slice I/P/B frame pictures restart slice-local entropy, intra-neighbor,
motion-predictor, and coded-block context state and retain per-slice deblocking offsets plus
`disable_deblocking_filter_idc` boundary behavior. The playback index validates
`recovery_frame_cnt` against the active `MaxFrameNum` and natively starts matured non-IDR intra or
cyclic intra-refresh P recovery windows. Inter recovery uses bounded neutral unavailable
short-term references and is verified at the signalled target against uninterrupted FFmpeg
output. Frame-coded pictures under an interlaced SPS are native and pixel-checked against x264;
field-picture POC is derived for all three modes. Complementary IDR intra, reference
P, and explicit bipredictive B fields are reconstructed through a field DPB with POC-ordered,
parity-aware default and modified reference lists plus adaptive MMCO marking, paired, and woven
with byte-exact FFmpeg verification. Multi-slice field pictures retain slice-local reconstruction
and deblocking state on field-height canvases. The native CAVLC MBAFF foundation maps pair scan
order into raster macroblocks for I/P/B pictures, reconstructs frame-coded pairs, and interleaves
field-coded `I_PCM` pairs. Field-coded Intra4, Intra8, Intra16, spatial B-direct, every `P_L0` shape, all
explicit B 16x16/16x8/8x16 combinations, and all `B_8x8` submacroblock shapes are native.
Mixed-pair CAVLC neighbors, frame/field motion-vector and reference-index conversion, field
coefficient scans, and cross-parity 4:2:0 chroma adjustment are pixel-checked on moving x264 MBAFF
GOPs and forced all-shape vectors. Field-coded temporal-direct B prediction, mixed-mode deblocking,
CABAC MBAFF, and multi-slice MBAFF remain explicit follow-on work.

The editor's first usable H.264 preview keeps pixel decoding behind `mmrecode-playback`'s bounded
request/event and decode-executor interfaces. Access-unit-sized jobs publish frames incrementally,
retain native decoder/DPB state across sequential requests, and cancel stale scrub work between
access units. A fixed-size pool executes those jobs on native hosts; the baseline WebAssembly
backend advances them cooperatively when playback is polled. Progressive non-reference B pictures
use cheap independent decoder forks whose reference planes and motion metadata are immutable
shared allocations. Native hosts can reconstruct those pictures concurrently while the
authoritative session advances through reference pictures; WebAssembly executes the same jobs
serially. Playback tries
the native Rust decoder first and currently invokes an
optional installed FFmpeg process on a native-demuxed Annex-B GOP window only when a stream uses
reconstruction tools not implemented yet. This fallback does not change container, timing,
indexing, seeking, or editor decisions.
