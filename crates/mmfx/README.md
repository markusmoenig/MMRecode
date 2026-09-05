# `mmrecode-mmfx`

`mmrecode-mmfx` is MMRecode's renderer-independent scene model, strict CSS-shaped parser, and
scalar CPU reference renderer. Source must parse and validate into typed values before any backend
can execute it. Static text is shaped and laid out with Parley, rasterized through Swash/Zeno, and
composited by the same linear-light backend as vector shapes.

The executable Scene 0.4 foundation supports:

- `@scene`, `@group`, `@rect`, and `@image` objects
- explicit `@font` resources and typed `@text` objects
- nested overlay, row, and column layout with absolute children, uniform padding/gap, alignment,
  and justification
- `px`, `%`, and content-measured `auto` sizes with min/max constraints
- typed `@param` declarations, strict `var(--name)` references, and host bindings
- hexadecimal sRGBA colors
- opacity and true group compositing
- image `contain`, `cover`, and `fill` fitting
- exact local-frame `@keyframes` with easing
- translation, scale, and rotation of complete node layers
- cover-style horizontal and vertical scrolling
- Zeno 256-level antialiased rectangle and rounded-rectangle coverage masks
- antialiased rectangular and rounded overflow clipping
- linear-light premultiplied RGBA compositing
- source-spanned diagnostics, duplicate detection, and unknown-property suggestions
- Unicode shaping, normal wrapping, no-wrap text, line height, alignment, variable font weight,
  and fractional glyph coverage
- deterministic font resolution with no implicit system-font fallback
- a bundled `builtin:inter` font for portable generated starter scenes

Render the checked-in example from the workspace root:

```console
cargo run -p mmrecode -- render-mmfx examples/mmfx/lower-third.mmfx output.png
cargo run -p mmrecode -- render-mmfx examples/mmfx/motion-layout.mmfx frame-23.png --frame 23 --frames 60
cargo run -p mmrecode -- render-mmfx examples/mmfx/rolling-credits.mmfx credits.png --frame 59 --frames 120
cargo run -p mmrecode -- render-mmfx examples/mmfx/parameterized-title.mmfx title.png --set "title=Launch Day"
```

Example source:

```css
@scene card {
    width: 1280px;
    height: 720px;
    background: #10151b;

    @font Inter {
        src: "../../assets/fonts/Inter.ttf";
    }

    @group lower-third {
        left: 6%;
        bottom: 8%;
        width: 58%;
        height: 150px;
        overflow: hidden;
        border-radius: 24px;
        background: #192433e8;

        @rect accent {
            width: 18px;
            height: 100%;
            background: #42d6c7;
        }

        @text title {
            left: 58px;
            top: 24px;
            width: 82%;
            height: 50px;
            content: "MMRecode";
            font-family: Inter;
            font-size: 38px;
            font-weight: 650;
            line-height: 1.1;
            color: #f4f7f8;
            white-space: nowrap;
        }
    }
}
```

Edit a scene directly in the full-screen terminal editor:

```text
add scene LowerThird 5:00 at 10:00
cd LowerThird
edit
scene load examples/mmfx/lower-third.mmfx
save as film.mmrecode
scene save as scenes/lower-third.mmfx
scene close
```

`add scene` creates an object in the current local timeline; its duration and placement belong to the
link, while its starter MMFX source belongs to the generated media definition. Focus that object
with `cd` and use contextual `edit` for the multiline editor and automatic worker preview. `fx edit`
remains an alias. Source edits are project edits and ordinary project `save` serializes them.
`scene load` embeds a copy of an external scene in the focused object, while `scene save as` extracts a
copy without changing ownership. Tab and Shift-Tab move pane focus, Ctrl-S saves the containing
project, Ctrl-Z/Ctrl-Y use project history, and Esc returns to the prompt. Parse/render errors report
source line and column while the monitor retains its last valid frame. Run `help`, `man edit`, or
`man scene` for the complete workflow. The `fx` namespace is reserved for future filters,
transitions, generators, and kernels; the old scene-oriented `add fx` commands remain aliases.

MMFX placements at the current hierarchy level are rendered at their timeline ranges even while
the source pane is open. FX-only projects use the same custom-pixel timeline as video projects and
show the composited project canvas in the monitor; when video is present, active MMFX scenes are
overlaid on its decoded preview frame. The same CPU project compositor renders recursively nested
FX layers into MPEG-2/TS export, including FX-only output. It maps parent time to exact source-local
animation time, prepares source and resources once, rasterizes static scenes once, and retains a
bounded cache of animated frame overlays.

Font fallback chains, color glyphs, text decorations, media slots, named reusable styles, richer timing,
compiler highlighting metadata, Kernel IR, tiled/SIMD CPU backends, and GPU preview are deliberately
later slices. See the
workspace [`mmfx-concept.md`](../../mmfx-concept.md) for the complete direction.
