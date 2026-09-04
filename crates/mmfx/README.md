# `mmrecode-mmfx`

`mmrecode-mmfx` is MMRecode's renderer-independent scene model, strict CSS-shaped parser, and
scalar CPU reference renderer. Source must parse and validate into typed values before any backend
can execute it. Static text is shaped and laid out with Parley, rasterized through Swash/Zeno, and
composited by the same linear-light backend as vector shapes.

The executable foundation supports:

- `@scene`, `@group`, and `@rect` objects
- explicit `@font` resources and typed `@text` objects
- nested overlay layout with absolute anchors
- `px` and `%` lengths
- hexadecimal sRGBA colors
- opacity and true group compositing
- `translate(x, y)`
- Zeno 256-level antialiased rectangle and rounded-rectangle coverage masks
- antialiased rectangular and rounded overflow clipping
- linear-light premultiplied RGBA compositing
- source-spanned diagnostics, duplicate detection, and unknown-property suggestions
- Unicode shaping, normal wrapping, no-wrap text, line height, alignment, variable font weight,
  and fractional glyph coverage
- deterministic font resolution with no implicit system-font fallback

Render the checked-in example from the workspace root:

```console
cargo run -p mmrecode -- render-mmfx examples/mmfx/lower-third.mmfx output.png
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
add fx LowerThird 5:00 at 10:00
cd LowerThird
fx edit
fx load examples/mmfx/lower-third.mmfx
save as film.mmrecode
fx save as scenes/lower-third.mmfx
fx close
```

`add fx` creates an object in the current local timeline; its duration and placement belong to the
link, while its starter MMFX source belongs to the generated media definition. Enter that object
with `cd` and use `fx edit` for the multiline editor and automatic worker preview. Source edits are
project edits and ordinary project `save` serializes them. `fx load` embeds a copy of an external
scene in the focused object, while `fx save as` extracts a copy without changing ownership. Tab and
Shift-Tab move pane focus, Ctrl-S saves the containing project, Ctrl-Z/Ctrl-Y use project history,
and Esc returns to the prompt. Parse/render errors report source line and column while the monitor
retains its last valid frame. Run `help` or `man fx` for the complete workflow.

Font fallback chains, color glyphs, text decorations, images/media slots, row/column layout,
animation, timeline compositing/export, compiler highlighting metadata, Kernel IR, optimized CPU
backends, and GPU preview are deliberately later slices. See the
workspace [`mmfx-concept.md`](../../mmfx-concept.md) for the complete direction.
