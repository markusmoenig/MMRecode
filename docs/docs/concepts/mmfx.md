---
title: MMFX
description: MMRecode's portable scene, typography, animation, transition, and visual-effect direction.
---

# MMFX

MMFX is MMRecode’s portable scene, layout, animation, transition, and visual-effect system. It separates two authoring needs:

- artists describe text, rectangles, images, media, groups, layout, styling, and animation with a strict CSS-shaped scene language;
- effect authors will implement custom generators, filters, and transitions with a separate safe, bounded kernel language.

## It looks like CSS, but it is not a browser

MMFX keeps familiar property names where their behavior fits deterministic video rendering. It does not include a DOM, JavaScript, selector specificity, browser compatibility recovery, or a global cascade.

Unknown declarations are errors. A misspelling should produce a source-spanned diagnostic and a useful suggestion rather than silently changing the render.

```css
@scene lower-third {
    width: 1280px;
    height: 720px;

    @font Inter {
        src: "../../assets/fonts/Inter.ttf";
    }

    @group card {
        left: 6%;
        bottom: 8%;
        width: 58%;
        height: 150px;
        overflow: hidden;
        border-radius: 24px;
        background: #192433e8;

        @text title {
            content: "MMRecode";
            font-family: Inter;
            font-size: 38px;
            color: #f4f7f8;
        }
    }
}
```

## CPU execution defines correctness

Scene evaluation, layout, text shaping, and the scalar reference renderer run on the CPU. Tiled/SIMD CPU execution and WGSL/wgpu preview are future accelerators consuming the same typed intermediate representation; they do not define separate effect semantics.

General composition uses linear-light, premultiplied alpha. Codec YUV is converted at the render boundary and converted back only for delivery.

## Current executable foundation

The `mmrecode-mmfx` crate currently provides:

- typed `Scene`, `Group`, `Rect`, `Font`, `Text`, and `Image` objects;
- strict, source-spanned parsing and validation;
- nested overlay, row, and column layout with absolute children, padding, gap, alignment, and justification;
- pixel and percentage lengths;
- colors, opacity, clipping, and rounded corners;
- image `contain`, `cover`, and `fill` fitting;
- exact local-frame keyframes, easing, translation, scaling, rotation, and cover-style scrolling;
- Parley text shaping and layout;
- Swash/Zeno glyph coverage and antialiasing;
- explicit font resources with no silent system-font fallback; and
- a linear-premultiplied scalar CPU renderer.

The terminal editor creates declarative content with `add scene`, edits its embedded source, debounces compilation on a worker, lets the animated timeline remain scrubbable while source is open, retains the last valid preview, and serializes source with the project. The default `monitor project` view composites the draft scene over underlying media at the project playhead. `monitor local` explicitly isolates the current hierarchy context and its descendants, while `monitor toggle` switches views without moving either mapped playhead. Merely using `cd` never changes monitor scope. The `fx` namespace is reserved for the future filter/transition/kernel workflow; legacy `add fx` remains compatible. The same exact placement-time evaluation is used for nested timeline preview and MPEG-2/TS project export.

## What comes next

Media slots, intrinsic sizing, fallback fonts, richer animation controls, Kernel IR, transitions, optimized CPU/GPU backends, and the plugin boundary are not implemented yet.

See the executable [Scene 0.2 reference](../mmfx/scene-language.md) and [rendered examples](../mmfx/examples.md).

See the complete design record in [`mmfx-concept.md`](https://github.com/markusmoenig/MMRecode/blob/feature/jpeg-inspect/mmfx-concept.md).
