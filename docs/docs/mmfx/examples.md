---
title: Examples
description: Runnable MMFX scenes and reference CPU-rendered output frames.
---

# MMFX examples

The checked-in [`motion-layout.mmfx`](https://github.com/markusmoenig/MMRecode/blob/feature/jpeg-inspect/examples/mmfx/motion-layout.mmfx)
uses an image resource, nested row/column layout, an entrance animation, and cover-style scrolling.
Render representative local frames from the repository root:

```console
cargo run -p mmrecode -- render-mmfx examples/mmfx/motion-layout.mmfx frame-000.png --frame 0 --frames 60
cargo run -p mmrecode -- render-mmfx examples/mmfx/motion-layout.mmfx frame-023.png --frame 23 --frames 60
cargo run -p mmrecode -- render-mmfx examples/mmfx/motion-layout.mmfx frame-059.png --frame 59 --frames 60
```

`--frame` is zero-based. `--frames` supplies the complete local scene duration, which is necessary
for `scene`-duration animation and scrolling. If omitted, the renderer uses the smallest duration
that contains the requested frame.

## Reference output

These PNGs are produced by the scalar CPU reference renderer, not hand-authored mockups.

### Frame 0 — entrance begins

![Motion layout at frame 0](/img/mmfx/motion-layout-000.png)

### Frame 23 — entrance complete

![Motion layout at frame 23](/img/mmfx/motion-layout-023.png)

### Frame 59 — ticker completes its cover traversal

![Motion layout at frame 59](/img/mmfx/motion-layout-059.png)

The smaller [`lower-third.mmfx`](https://github.com/markusmoenig/MMRecode/blob/feature/jpeg-inspect/examples/mmfx/lower-third.mmfx)
is a static text-and-shape example:

```console
cargo run -p mmrecode -- render-mmfx examples/mmfx/lower-third.mmfx lower-third.png
```

Inside the editor, `add scene`, `cd` into the generated object, and run `edit`. The source remains
embedded in the project; `scene load` imports an external example as an embedded copy and `scene save as`
extracts a reusable copy. Tab and Shift-Tab move input focus between source, timeline, inspector,
and command panes, so the timeline can be scrubbed while source stays open. Pointer movement never
changes keyboard focus; click a pane or use Tab to select it. The default `monitor project` view
shows the draft scene composited over decoded media at the current project playhead. Use
`monitor local` to isolate the current `cd` context and its descendants, or `monitor toggle` to
switch back and forth. Local generated content uses a checkerboard background to reveal
transparency. Switching scope preserves both mapped playheads and does not modify the project.

`scene` names declarative generated timeline content. The `fx` namespace is reserved for the later
filter, generator, transition, and kernel workflow. Legacy `add fx` and `fx load/save/close` remain
accepted so existing projects and scripts continue to work.
