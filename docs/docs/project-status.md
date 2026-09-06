---
title: Project status
description: Implemented MMRecode slices, current limits, and maturity expectations.
---

# Project status

MMRecode is under active development. Its codec, editor, playback, rendering, and MMFX foundations are implemented and growing, while public library and ABI compatibility are still evolving.

## Implemented slices

| Area | Current foundation |
| --- | --- |
| Motion JPEG | Baseline sequential 8-bit JPEG decode/encode, inspection, verification, and independent-frame packet copying |
| DV | Raw DV25 525/60 and 625/50 parsing, video decode/encode, embedded audio, timecode, and validation |
| MPEG-2 Video | Main Profile 4:2:0 frame-picture parsing, I/P/B reconstruction, constrained encoding, dependency analysis, and bridge rendering |
| H.264/AVC | Native Baseline/Main/High reconstruction, deterministic CAVLC/CABAC intra/P/B encoding, indexed playback, and clean-GOP MP4 remuxing |
| Audio | Native mono/stereo AAC-LC reconstruction, deterministic long-window AAC-LC encoding, sample-domain placement/mix/resampling, plus MPEG-1 Layer II framing and pass-through |
| Containers | Y4M, 188-byte MPEG-TS demux/mux, non-fragmented ISO-BMFF/QuickTime sample-table demuxing, and interleaved H.264/AAC Fast Start MP4 writing |
| Editing | Recursive media graph, typed commands, exact time, undo/redo, versioned projects, terminal workspace, MPEG-2/TS export, and YouTube H.264/AAC MP4 export with imported timeline audio |
| MMFX | Strict typed scenes and public parameters, intrinsic row/column/overlay layout, images, Unicode text, exact-frame animation and scrolling, embedded or auto-synchronized linked source, highlighted live editing, backend-neutral scene and project composition graphs, residency-aware frame handles, stable resource keys, bounded device-resource retention policy, public resource-provider/backend execution contracts, graph-backed decoded-video conformance, scalar CPU graph execution, optional cached wgpu RGBA project composition with asynchronous terminal readback, and recursive timeline preview/export composition |
| Integration | Main CLI, native viewer, quality utilities, project-clock editor playback across sequential clips, gaps, and MMFX scenes, cached timeline raster with independently refreshed playhead and deferred in-playback thumbnail refresh, latest-request-wins asynchronous preview conversion, bounded terminal proxies, compatible Kitty delivery with experimental shared-memory plumbing, live stage timing, and an experimental one-shot C API |

## Important limits

- The application is installed with `cargo install mmrecode`, while its library APIs remain intentionally unstable.
- The H.264 encoder includes GOP construction, quarter-pixel P/B motion search, multiple references,
  adaptive rate control, and complete CAVLC/CABAC intra, P, and B picture coding.
- YouTube 1080p/2160p upload presets produce native High/CABAC, two-B-frame, BT.709 Fast Start MP4
  with synchronized AAC-LC at 48 kHz stereo and a 384 kbps target. MPEG-TS Layer II and MP4/MOV
  AAC carried beside H.264 are decoded, source-trimmed, timeline-mixed/resampled, and re-encoded;
  sources without audio contribute silence. Source-specific demux/decode is hidden behind a common
  timed-PCM ingestion boundary, so the timeline and delivery encoder do not depend on input format.
- Uncommon AAC modes such as 960-sample frames, multichannel layouts, and HE-AAC still use the optional fallback.
- MPEG Layer II framing and Rust/Symphonia PCM reconstruction are available; encoding is not.
- Final export recursively composites nested generated MMFX content and attached audio from video
  placements; broader generated media, transitions, and dedicated audio-only composition remain
  incomplete.
- Dedicated audio-only media nodes, automation, fades, and interactive waveform editing are not
  implemented yet.
- Overlapping video preview currently selects the topmost opaque placement; simultaneous decoded
  video blending is not implemented yet.
- MMFX does not yet have media slots, named reusable styles, richer animation controls, Kernel IR,
  transitions, an accelerated scene-graph backend, GPU scale/color conversion, or direct native
  monitor delivery. The default terminal build uses the wgpu project compositor for positioned
  RGBA overlays through an asynchronous readback path, with automatic CPU fallback. Codec seeking,
  frame decoding, thumbnail generation, scene evaluation, and terminal image transfer remain on
  the CPU, so the current wgpu path does not make clip-only scrubbing or thumbnails faster.
- Incremental streaming, broad metadata preservation, fuzzing, performance baselines, packaging, and compatibility policies remain future work.

## Verification approach

The repository includes deterministic test vectors, parser damage cases, frame and plane comparisons, encoder reconstruction tests, and optional external FFmpeg interoperability checks. These are strong regression tools, but they are not a complete normative conformance suite.

## Source of truth

This page is a readable snapshot, not a generated compatibility matrix. For the most detailed live status, consult the root [`README.md`](https://github.com/markusmoenig/MMRecode/blob/main/README.md), crate READMEs, and [`todo.md`](https://github.com/markusmoenig/MMRecode/blob/main/todo.md).
