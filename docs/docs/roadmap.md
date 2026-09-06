---
title: Roadmap
description: The next bounded milestones for editing, codecs, audio, MMFX, and project maturity.
---

# Roadmap

MMRecode grows through bounded, verifiable slices rather than attempting broad format support all at once. Priorities can move when a concrete media file or workflow reveals a more valuable boundary.

## Near-term direction

1. Meet the realtime preview gate: a responsive project clock, latest-frame-wins decode and
   conversion queues, measured terminal delivery, GPU YUV conversion/scaling, and a native GPU
   monitor fallback if terminal transport cannot sustain ordinary 1080p playback.
2. Improve the connected MPEG-TS Layer II and MP4/MOV AAC timeline path with band-adaptive
   psychoacoustic quantization, short-window transient coding, and bounded streaming PCM buffers.
3. Validate native AAC-LC playback with representative real-world media, then add bounded buffering and seek preroll.
4. Add media fingerprints, relinking, and portable project collection.
5. Extend project-clock preview from sequential/topmost video and MMFX into simultaneous video
   blending, placement-aware audio mixing, and later transitions.
6. Extend the implemented MMFX intrinsic layout, typed parameters, exact animation, and scrolling slice with media slots, named styles, and richer timing controls.
7. Strengthen H.264 dependency analysis for arbitrary edit boundaries and production-oriented rate/distortion tuning.
8. Improve incremental MPEG-TS demuxing, streaming audio, buffering, and backpressure.

## Realtime preview gate

Realtime preview is a product requirement. The project clock and keyboard input must remain
responsive even when decoding or display misses its deadline; visual work drops obsolete frames
instead of slowing project time. The current terminal editor caches its timeline base raster,
defers thumbnail refresh while playing, converts preview frames on a latest-request-wins worker,
and bounds terminal proxies to 960×540 for Kitty or 800×450 for fallback protocols. The monitor
reports measured view/decode frame rates plus smoothed conversion and terminal-send latency. Kitty
shared-memory plumbing exists behind an explicit experimental opt-in, but automatic selection must
wait for terminal capability negotiation and acknowledgement handling.

Acceptance targets for representative 1080p media are 30 fps monitor presentation, under 50 ms
input latency, and an indexed scrub response within roughly 100–150 ms. Full-resolution export is
unaffected by preview proxies. GPU color conversion/scaling and direct native monitor delivery are
the next steps. If Kitty transport cannot meet the presentation target, the terminal remains the
editor while an optional native wgpu surface becomes the realtime monitor.

## Editing and delivery

The immediate editor work is dependable project handling and composition rather than adding a large effect catalog. Key milestones include hierarchy-projected timeline rows, dedicated trim adjustment modes, multi-clip audio, complete render-plan explanations, cancellation/progress, and recursive alpha-aware composition.

## MMFX progression

MMFX now has a typed Scene 0.4 foundation with public parameters, text, images, intrinsic
row/column/overlay layout, exact-frame keyframes, scrolling, embedded or auto-synchronized linked
source, recursive placement-time preview/export, a backend-neutral scene display-list/render-graph,
and residency-aware project composition graphs with stable MMFX resource keys plus a public
resource-provider/backend execution boundary. Decoded YUV fit/fill/stretch/native conformance now
uses the same graph, with explicit high-quality or preview sampling. A generic bounded device
resource cache now provides stable-key reuse, deterministic eviction, current-graph protection,
and explicit idle release without exposing a graphics API. An optional wgpu backend now proves
cached RGBA project composition on a shared device/queue. The terminal application enables this by
default, uses a non-blocking three-slot readback ring, and preserves an explicit CPU-only build plus
automatic runtime fallback. The next progression is:

1. connect the wgpu compositor directly to a native monitor surface without CPU readback;
2. implement GPU scale/color-conversion passes and backend-owned delivery;
3. richer scene objects, media slots, and named reusable styles;
4. animation delay/repetition and more timing controls;
5. a safe scalar Kernel IR;
6. transitions and neighborhood effects;
7. tiled/SIMD CPU execution and differential CPU/GPU testing; and
8. a versioned third-party module and plugin boundary.

## Later breadth

AVI, MXF, MPEG Program Stream, fragmented MP4, wider codec profiles, hardware acceleration, stable bindings, and additional modern codecs are demand-driven. They should begin only with a real workflow, test media, and an acceptance criterion.

The detailed working backlog lives in [`todo.md`](https://github.com/markusmoenig/MMRecode/blob/main/todo.md).
