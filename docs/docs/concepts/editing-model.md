---
title: Hierarchical editing
description: How MMRecode models media, placement links, local timelines, and typed commands.
---

# Hierarchical editing

MMRecode treats the composition as a recursive media graph. The project is the root media timeline, and every media definition may own an ordered local timeline of child placements.

## Media and placements are different

A media definition has stable identity and contains the reusable content. A placement link puts that media into a parent timeline and carries instance-specific information:

- source in and out points;
- parent-local timeline range;
- human-readable alias;
- scale or presentation behavior; and
- eventually transforms, opacity, masks, and typed parameter overrides.

One media definition can appear in several places. Cycles are rejected, but reuse is valid.

## Paths identify context

A path such as `/Clip0/LowerThird` follows placement links. Entering a path changes the authoring context and the local time shown by the timeline:

```text
Film > ls
Clip0      |──────── video ────────|
Music      |~~~~~~~~ audio ~~~~~~~~~~~~~|

Film > cd Clip0
Film > Clip0 > ls
LowerThird |       [fx]       |
Grade      |==================|
```

At the project root, the timeline shows direct root placements. Inside `Clip0`, it shows `Clip0`’s children in clip-local time. Parent siblings do not remain permanently expanded.

The monitor does not follow `cd` implicitly. `monitor project` is the default and keeps a separate
root-timeline playhead, so editing `LowerThird` previews it over the underlying project frame.
`monitor local` explicitly isolates the selected object and its descendants; `monitor toggle`
switches between the scopes. The local timeline playhead maps through the selected placement to the
project playhead, so switching views does not jump in time or modify the project.

## One command model

Terminal input is parsed into typed, versionable editor commands. Scripts, future graphical tools, and natural-language frontends can produce the same operations:

```text
terminal command ─┐
script            ├─→ typed commands → media graph → render intent
future GUI        ┘
```

This makes sessions replayable and testable without making shell strings the internal editor API.

## Exact time

Authoritative media time is rational, never floating-point seconds. Editor timecode is compact and frame-oriented:

- `1:15` — one second and fifteen frames;
- `2:01:15` — two minutes, one second, and fifteen frames;
- `out -0:10` — move the out-point ten frames earlier.

The active media context supplies the exact frame rate. Project, placement, and source time domains remain distinct.

## Persistence

Projects are readable, versioned `.mmrecode` JSON documents. They retain resolved authoring settings, stable identifiers, the recursive graph, placement ranges, managed relative media paths, and explicit external links.

Media fingerprints, relinking, portable collection, recursive generated-content compilation, and full audio editing remain future slices.
