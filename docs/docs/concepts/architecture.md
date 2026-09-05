---
title: Architecture
description: Crate boundaries, dependency direction, packet and frame interfaces, and smart rendering.
---

# Architecture

MMRecode is a cross-platform media layer. The command-driven editor and native viewer sit at the top; codecs, containers, playback, editing, rendering, quality analysis, and MMFX remain reusable components underneath.

The system is modular by behavior, not only by Cargo package. Codecs do not know about containers, containers transport packets without implementing codecs, and editing intent contains no MPEG-2 or H.264 syntax rules.

## Dependency direction

```text
                         applications
                    mmrecode / viewer
                              │
                 edit · playback · quality
                              │
                           render
                       ┌──────┴──────┐
                       │             │
                    codecs       containers
                       └──────┬──────┘
                              │
                            core
                              ▲
                         bitstream
```

Implementation crates remain selectable. An application using only DV decoding should not need MP4, H.264, the editor, or the viewer.

The application layer can be replaced, embedded, or extended without changing codec or container behavior.

## Core boundaries

`mmrecode-core` provides exact time, frames, packets, stream descriptors, errors, codec interfaces, container interfaces, and dependency vocabulary. It contains no format algorithms.

`Packet` is the encoded boundary between demuxers, codecs, render execution, and muxers:

```text
Demuxer → Packet → Decoder → VideoFrame
VideoFrame → Encoder → Packet → Muxer
```

Packets retain stream identity, encoded bytes, PTS/DTS, duration, flags, and opaque side data so unchanged media can travel through an edit without unnecessary reconstruction.

## Render planning

The codec-independent renderer consumes edit intent plus codec dependency analysis. Its operation vocabulary includes:

- `CopyPackets`
- `RewriteTimestamps`
- `Decode`
- `ApplyEffects`
- `BridgeEncode`
- `FullEncode`
- `Mux`

The first inter-frame implementation uses MPEG-2 reference graphs to propagate cut or effect damage through dependent pictures, include unchanged decoder preroll, and return to packet copying at the next safe point.

The H.264 path includes native decoding, deterministic Baseline encoding, and dependency-closed GOP remuxing. Arbitrary-boundary smart rendering and inter-frame encoder tools remain future work.

## Safe reference paths

Portable safe Rust defines reference behavior. Unsafe pointer work is confined to the experimental C boundary. Future SIMD, GPU, hardware, and assembly paths must stay behind narrow interfaces and retain differential tests against the portable implementation.

## API status

The project is young and its library APIs and experimental C ABI may still change while more formats and editing operations exercise these boundaries. End users install the command-line application with `cargo install mmrecode`; API stability is a separate future commitment.
