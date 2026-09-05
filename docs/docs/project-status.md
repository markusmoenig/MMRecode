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
| H.264/AVC | Native Baseline/Main/High reconstruction, deterministic Baseline encoding, indexed playback, and clean-GOP MP4 remuxing |
| Audio | Native mono/stereo AAC-LC reconstruction plus MPEG-1 Layer II framing and pass-through |
| Containers | Y4M, 188-byte MPEG-TS demux/mux, and non-fragmented ISO-BMFF/QuickTime sample-table demuxing |
| Editing | Recursive media graph, typed commands, exact time, undo/redo, versioned projects, terminal workspace, and initial MPEG-2 export |
| MMFX | Strict typed scenes, intrinsic row/column/overlay layout, images, Unicode text, exact-frame animation and scrolling, highlighted live editing, scalar CPU rendering, and recursive timeline preview/export composition |
| Integration | Main CLI, native viewer, quality utilities, playback scheduling, and an experimental one-shot C API |

## Important limits

- The application is installed with `cargo install mmrecode`, while its library APIs remain intentionally unstable.
- The H.264 encoder does not yet provide inter prediction, GOP construction, motion search, or adaptive rate control.
- Uncommon AAC modes such as 960-sample frames, multichannel layouts, and HE-AAC still use the optional fallback.
- MPEG Layer II is framed for transport but not decoded by the reusable codec crate.
- Final export recursively composites nested generated MMFX content; broader generated media, transitions, and audio composition remain incomplete.
- Multi-clip audio editing, mixing, resampling, and sample-domain boundary handling are incomplete.
- MMFX does not yet have media slots, reusable parameters/styles, richer animation controls, Kernel IR, transitions, or optimized CPU/GPU backends.
- Incremental streaming, broad metadata preservation, fuzzing, performance baselines, packaging, and compatibility policies remain future work.

## Verification approach

The repository includes deterministic test vectors, parser damage cases, frame and plane comparisons, encoder reconstruction tests, and optional external FFmpeg interoperability checks. These are strong regression tools, but they are not a complete normative conformance suite.

## Source of truth

This page is a readable snapshot, not a generated compatibility matrix. For the most detailed live status, consult the root [`README.md`](https://github.com/markusmoenig/MMRecode/blob/feature/jpeg-inspect/README.md), crate READMEs, and [`todo.md`](https://github.com/markusmoenig/MMRecode/blob/feature/jpeg-inspect/todo.md).
