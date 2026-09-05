# mmrecode-h264

`mmrecode-h264` is the codec-owned syntax, reconstruction, dependency, and first encoder layer for
AVC. It currently provides:

- Annex-B and ISO/`avcC` length-prefixed NAL splitting and conversion;
- AVC decoder configuration record parsing;
- emulation-prevention removal and Exp-Golomb syntax reading;
- SPS, PPS, VUI, and leading slice-header parsing;
- container-timed access-unit indexing;
- IDR/reference classification and a conservative active-reference dependency set;
- a deterministic Baseline-profile encoder foundation that emits lossless, all-IDR `I_PCM`
  pictures through the shared stateful encoder API, including `avcC` configuration, cropping,
  emulation prevention, packet timing, and exact native round-trip reconstruction;
- an optional `mode=intra16` compressed path with reconstructed-neighbor DC/horizontal/vertical
  mode decisions, quantized luma DC Hadamard plus 4x4 luma/chroma DC/AC coefficients, general CAVLC
  residual serialization, and a normative reconstruction matching independent FFmpeg decoding;
- a `mode=intra4` path that selects among all nine luma prediction modes block by block, derives
  predicted-mode and CAVLC contexts from reconstructed neighbors, and writes full luma/chroma
  residuals. Both compressed modes accept a `qp=0..51` option;
- a stateful `mode=inter` path with periodic Intra4 IDRs and the complete P
  partition tree: adaptive P16x16, P16x8, P8x16, and P8x8 macroblocks whose subpartitions reach
  8x4, 4x8, and 4x4. Deterministic integer searches receive quarter-pixel luma refinement with
  matched eighth-sample chroma prediction. The path also writes predicted motion-vector
  differences, full CAVLC residuals and P-skip runs, retains up to four reconstructed references,
  and exposes `gop_size`, `search_range`, `max_refs`, and opt-in `scene_cut_threshold`;
- an optional `b_frames=1..3` reorder path using type-0 picture order, B16x16/B16x8/B8x16
  list-0/list-1/bi motion combinations, all thirteen `B_8x8` sub-macroblock types, spatial and
  temporal direct prediction, B-skip decisions, CAVLC residuals, presentation PTS, decode-order
  DTS, flush-safe delayed-frame draining, and automatic Main Profile signalling;
- deterministic frame-level bitrate control for every compressed picture mode. The generic
  `bitrate` setting drives a bounded eight-frame virtual buffer and adjusts QP from the configured
  starting value using each packet's size and declared frame duration;
- optional `aq_strength=1..12` macroblock adaptive quantization. Mean absolute luma activity lowers
  QP in quiet regions and raises it in textured regions relative to the picture QP; zero is the
  default and disables AQ;
- opt-in single-CPB NAL HRD/VBV signalling through `vbv_buffer_ms=1..60000` when `bitrate` is set.
  The SPS carries VUI timing and scaled HRD rate/size values; buffering-period and picture-timing
  SEI carry 24-bit removal/output delays, including reordered B-picture output timing.

The crate does not parse MP4/MOV. Its native decoder foundation reconstructs
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

The encoder foundation accepts progressive, even-sized `Yuv420p8` frames, pads the coded canvas to
macroblock boundaries with edge samples, crops it back to the visible dimensions in the SPS, and
disables deblocking. Its default `I_PCM` mode is an exact normative reference; `intra16` and
`intra4` provide deterministic transform-coded all-IDR compression with configurable QP.
The `inter` mode retains `max_refs=1..4` reconstructed short-term pictures and applies the complete
P partition tree down to 4x4 with partition-specific integer search and quarter-pixel refinement,
or P-skip when the predicted list-0 block needs no residual. `b_frames=1..3` delays that many
display-order pictures, emits the next P anchor first, then emits each non-reference B picture.
B16x16, B16x8, and B8x16 modes independently select list-0, list-1, or bidirectional prediction per
partition. `B_8x8` adds every L0/L1/Bi shape from 8x8 through 4x4 plus mixed direct submacroblocks.
Spatial direct derives nonzero list motion from neighboring blocks and applies the normative
per-8x8 colocated-zero override; zero-residual macroblocks fold into B-skip runs. This activates
type-0 picture order and keeps PTS in presentation order while assigning DTS in decode order; flush
encodes unmatched delayed pictures as P. B mode retains at least two references even when
`max_refs=1`. `scene_cut_threshold=1..255` inserts an IDR when mean luma difference from the latest
reconstruction reaches the threshold; zero disables this check. `b_direct=spatial|temporal`
selects the picture-wide direct derivation, with spatial as the default. Temporal mode retains the
future anchor's colocated reference identity and unwrapped picture order to scale both list vectors.
Supplying `VideoEncoderSettings::bitrate` in `intra16`, `intra4`, or `inter` mode enables reactive
frame-level rate control; `qp` is then its initial value. A positive frame duration sets that
picture's bit budget, otherwise one configured time-base tick is used. `aq_strength=0..12` then
redistributes the resulting picture QP across macroblocks according to relative luma activity.
Macroblock QP deltas advance only where AVC syntax permits, including across skipped and
zero-residual inter blocks. `vbv_buffer_ms` makes the virtual capacity explicit and activates a
single-entry VBR NAL HRD model. Access-unit arrivals and removals are checked against that CPB;
units which cannot fit or be removed at the declared cadence are rejected instead of silently
violating the signal. Bitrate control and AQ with fixed-size lossless `I_PCM` are rejected.
`profile=auto|baseline|main` defaults to Baseline for I/P-only streams and Main when B pictures are
enabled. Explicit Baseline+B configurations are rejected, and the `avcC` profile/compatibility
bytes are copied from the encoded SPS. `level=auto|1|1b|1.1..6.2` checks the Annex A frame-size,
macroblock-rate, decoded-picture-buffer, target-bitrate, and optional CPB limits; automatic mode
selects the lowest conforming level and `avcC` mirrors the SPS level byte. One `time_base` tick is
the default frame interval used during configuration. Containers whose timestamp clock is finer
than one frame should set `frame_duration_ticks` to the nominal positive frame duration. Submitted
frames are checked again using their actual duration, so a faster variable-rate frame cannot
silently violate the declared level. Without a target bitrate, level selection covers structural
and cadence limits but cannot promise a bound for fixed-QP output size.

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
