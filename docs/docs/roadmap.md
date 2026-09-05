---
title: Roadmap
description: The next bounded milestones for editing, codecs, audio, MMFX, and project maturity.
---

# Roadmap

MMRecode grows through bounded, verifiable slices rather than attempting broad format support all at once. Priorities can move when a concrete media file or workflow reveals a more valuable boundary.

## Near-term direction

1. Validate native AAC-LC playback with representative real-world media, then add bounded buffering and seek preroll.
2. Add media fingerprints, relinking, and portable project collection.
3. Extend recursive preview/export composition from video and MMFX into audio and later transitions.
4. Extend the implemented MMFX intrinsic layout, typed parameters, exact animation, and scrolling slice with media slots, named styles, and richer timing controls.
5. Strengthen H.264 dependency analysis for arbitrary edit boundaries and extend the encoder with inter-frame tools.
6. Improve incremental MPEG-TS demuxing, streaming audio, buffering, and backpressure.

## Editing and delivery

The immediate editor work is dependable project handling and composition rather than adding a large effect catalog. Key milestones include hierarchy-projected timeline rows, dedicated trim adjustment modes, multi-clip audio, complete render-plan explanations, cancellation/progress, and recursive alpha-aware composition.

## MMFX progression

MMFX now has a typed Scene 0.4 foundation with public parameters, text, images, intrinsic row/column/overlay layout,
exact-frame keyframes, scrolling, and recursive placement-time preview/export. The next progression is:

1. richer scene objects, media slots, and named reusable styles;
2. animation delay/repetition and more timing controls;
3. a safe scalar Kernel IR;
4. transitions and neighborhood effects;
5. optimized CPU/GPU backends; and
6. a versioned third-party module and plugin boundary.

## Later breadth

AVI, MXF, MPEG Program Stream, fragmented MP4, wider codec profiles, hardware acceleration, stable bindings, and additional modern codecs are demand-driven. They should begin only with a real workflow, test media, and an acceptance criterion.

The detailed working backlog lives in [`todo.md`](https://github.com/markusmoenig/MMRecode/blob/main/todo.md).
