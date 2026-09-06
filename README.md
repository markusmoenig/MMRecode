# MMRecode

**CROSS-PLATFORM MEDIA LAYER & EDITOR FOR ALL PLATFORMS**

MMRecode is a cross-platform media layer written in Rust. It provides native codecs, containers,
exact timing, playback, editing, render planning, and MMFX as independently usable components.

The terminal-native, command-driven editor is the flagship application built on top. Every media
object can have its own local timeline, while exports preserve encoded media wherever an edit
leaves it unchanged.

By Markus Moenig, founder of MainConcept.com and former CTO of DivX.

## Install

Install the application from crates.io:

```bash
cargo install mmrecode
```

Then open the editor directly in your terminal:

```bash
mmrecode
```

Kitty and Ghostty use the direct Kitty graphics path. MMRecode also selects Sixel or iTerm2 images
when available and falls back to portable 24-bit Unicode rendering in other true-color terminals.
Kitty-protocol sessions use the broadly compatible temporary-file transport by default. POSIX
shared-memory delivery remains an experimental opt-in while capability negotiation is developed.

## The editor runs in your terminal

The full-screen workspace contains the monitor, timeline, inspector, contextual help, and command
prompt. Navigate compositions with familiar commands, edit without changing tools, and preview
sequential clips, black gaps, and animated MMFX scenes from one project clock in the same workspace.
The preview clock and input path stay independent of slow rendering: frame conversion is
asynchronous and latest-frame-wins, terminal proxies are bounded, and live stage timings expose
decode, conversion, and delivery bottlenecks without changing full-resolution export.

```text
Untitled > import projects/output.ts as Clip0
Untitled > cd Clip0
Untitled/Clip0 > in +0:10
Untitled/Clip0 > out -0:10
Untitled/Clip0 > save as MyFilm
```

The same typed editing operations drive the interactive terminal, scripts, automation, project
files, preview, and final rendering.

## Lossless recoding

MMRecode plans exports from actual codec dependencies instead of treating every edit as a complete
transcode.

- Unchanged encoded packets are copied and retimed.
- Only dependencies damaged by a cut are regenerated.
- Titles and effects render only their affected frame ranges.

The render plan makes copied, bridge-encoded, and fully rendered ranges explicit.

## Hierarchical editing

A composition is a hierarchy of media and placement links rather than one permanently expanded
stack of tracks. A clip may contain a title, mask, effect, or another complete composition.

Entering a path such as `Film > Clip0 > Title` changes the visible timeline to that object's local
time. Media remains reusable while every placement retains its own timing and overrides.

## MMFX

MMFX is MMRecode's portable scene and visual-effects system for titles, typography, graphics,
layout, animation, transitions, and compositing. Its strict CSS-shaped language compiles to typed
scene data, then to an explicit display list and render graph. The scalar CPU reference backend and
future accelerated backends therefore share semantics without depending on a browser or a
particular GPU API.

## A reusable media layer

The editor is one application built on a reusable foundation. Codec, container, playback, editing,
rendering, quality, and MMFX crates can be used independently through shared packet, frame, stream,
exact-time, and dependency interfaces.

Codecs remain separate from containers, and neither contains editor policy. Applications can embed
or replace the frontend without changing codec or container behavior.

## Codecs and containers

Current codec work includes Motion JPEG, DV, MPEG-2 Video, H.264/AVC, AAC, and MPEG Audio. Container
support includes MPEG transport streams, MP4/QuickTime, and YUV4MPEG2.

See the [project status](https://mmrecode.com/docs/project-status) for detailed format coverage and
current boundaries.

## Documentation

Documentation is available at [mmrecode.com](https://mmrecode.com). Start with the
[editing model](https://mmrecode.com/docs/concepts/editing-model) or the
[installation guide](https://mmrecode.com/docs/get-started).

## License

MMRecode is open source under the Apache License 2.0, including its contributor patent grant. It
can be used in open and proprietary media products. The source license does not replace any
third-party patent licenses that may apply to standardized media formats.

Copyright © Markus Moenig.

## Contact

[nubby.leaving0w@icloud.com](mailto:nubby.leaving0w@icloud.com)
