---
title: Scene language
description: The executable MMFX Scene 0.4 syntax for typed parameters, intrinsic layout, text, animation, and scrolling.
---

# MMFX Scene 0.4

MMFX Scene is a strict, CSS-shaped composition language. It deliberately has no DOM, selectors,
global cascade, JavaScript, or silent recovery. Unknown and duplicate declarations are errors with
source locations.

This page documents executable behavior only. Proposed syntax stays in the MMFX concept document
until it has parser, renderer, and test coverage; visual additions also ship with runnable examples
and CPU-reference output frames.

One module contains exactly one `@scene` and may contain `@param` and `@keyframes` blocks. The scene may contain
`@group`, `@rect`, `@text`, and `@image` objects. Only groups may contain children, and later
siblings paint over earlier siblings.

## Typed parameters

Reusable scenes declare public inputs before `@scene`. A reference occupies one complete property
value, so the receiving property determines and validates its final MMFX type.

```css
@param --title {
    type: text;
    default: "Default title";
}

@param --accent {
    type: color;
    default: #42d6c7;
}

@param --alignment {
    type: choice;
    default: start;
    choices: "start, center, end";
}

@scene reusable-title {
    width: 960px;
    height: 540px;

    @font Inter {
        src: "builtin:inter";
    }

    @text title {
        width: auto;
        height: auto;
        content: var(--title);
        color: var(--accent);
        text-align: var(--alignment);
        font-family: Inter;
    }
}
```

Parameter types are `text`, `color`, `length`, `number`, `boolean`, and `choice`. Text defaults are
quoted; colors use hexadecimal notation; lengths use `px`, `%`, or `auto`; numbers must be finite;
booleans are `true` or `false`; choices require a quoted comma-separated `choices` list. Unknown,
duplicate, and mistyped bindings are errors. Embedded expressions such as
`translateX(var(--offset))` are deliberately not accepted yet; write `left: var(--offset)` so one
typed value maps to one typed property.

In the editor, use `scene params`, `scene set <name> <value>`, and `scene reset [name]`. Quote text
containing spaces. Bindings participate in undo/redo, project persistence, preview, export, and
scene cache signatures. `scene save as` extracts reusable source with its declared defaults rather
than baking project bindings into the file. The standalone renderer accepts repeated
`--set name=value` options.

## Embedded and linked source

MMRecode provides two explicit external-file workflows:

```text
scene load titles/lower-third.mmfx
scene link titles/lower-third.mmfx
scene reload
scene unlink
```

`scene load` is a one-time import. It copies the file into the focused scene object, retains the
file's directory as the base for relative fonts and images, and then treats the source as embedded
project content.

Like loading a different source, linking starts from the new module's declared parameter defaults
and clears bindings from the previous source. `scene link` keeps the canonical external path and a
last-valid source snapshot in the project.
While the full-screen editor is running, it polls only linked files and debounces write bursts.
After a changed file parses and type-checks with the scene's current parameter bindings, the new
snapshot atomically replaces the old one and invalidates only that scene's render cache. A missing,
partially written, or invalid file leaves the cached scene active and reports the problem in the
editor. `scene reload` forces the same validation immediately. `scene unlink` embeds the current
snapshot and stops watching.

Linked source is read-only in MMRecode's internal code pane to prevent simultaneous internal and
external writers. Edit it in the external editor, or unlink it before using `edit`. Project `save`
persists the external path, cached snapshot, resource base, and parameter bindings, so preview and
export can continue from the cache when the external file is unavailable. Linked paths are explicit
absolute links; moving a project does not silently retarget them.

## Canvas and resources

```css
@scene card {
    width: 1280px;
    height: 720px;
    background: #10151b;

    @font Inter {
        src: "builtin:inter";
    }

    @image logo {
        width: 180px;
        height: 120px;
        src: "logo.png";
        object-fit: contain;
    }
}
```

Scene dimensions are positive whole-pixel `px` values. Colors accept `#rgb`, `#rgba`, `#rrggbb`,
or `#rrggbbaa`. Font and image paths are resolved relative to the MMFX module's resource base;
absolute paths are rejected by the MMRecode host. `builtin:inter` is the portable built-in font.
Image fitting is `contain`, `cover`, or `fill`.

## Boxes and layout

Every object has a box. Lengths use `px` or `%`, with zero allowed without a unit. Object `width`
and `height` additionally accept `auto`, which measures text, source image dimensions, or a group's
flow children before placement. The shared
properties currently are:

| Purpose | Properties and values |
| --- | --- |
| Placement | `position: absolute`, `left`, `top`, `right`, `bottom`, `width`, `height`, `min-width`, `max-width`, `min-height`, `max-height` |
| Child layout | `display` accepts `overlay`, `row`, `column`, or `flex`; `flex-direction` accepts `row` or `column` |
| Flow spacing | `padding`, `gap`, `align-items` (`start`, `center`, `end`, `stretch`), `justify-content` (`start`, `center`, `end`, `space-between`) |
| Paint | `background`, `opacity: 0..1`, `border-radius`, `overflow` (`visible` or `hidden`) |
| Geometry | `transform: translate(...) translateX(...) translateY(...) scale(...) rotate(...deg)` |

Children participate in their parent's row or column flow by default. An absolute child is removed
from that flow and uses its inset properties. Overlay places children in the same containing box.
This bounded profile has uniform padding and gap. In a row or column, automatic text boxes use
Parley's shaped line metrics, automatic images use their source dimensions and preserve aspect
ratio when only one axis is declared, and automatic groups enclose their flow children plus padding
and gaps. Percentages inside an automatic parent resolve against the available containing box; this
avoids circular browser-style layout. Min/max constraints are then applied to the measured result.
The profile does not yet have margins, wrapping flex rows, or a browser box model.

## Text

`@text` requires `content` and `font-family`. The family must name an earlier `@font` resource.
Supported text properties are `font-size` in pixels, numeric `font-weight`, pixel or unitless
`line-height`, `color`, `text-align: start|center|end`, and `white-space: normal|nowrap`.

Text is shaped with Parley and rasterized through Swash/Zeno on the deterministic CPU reference
path. System-font fallback is intentionally disabled, so a project cannot silently change fonts on
another machine.

## Exact-frame animation

```css
@group card {
    animation: enter 12f ease-out;
}

@keyframes enter {
    from { opacity: 0; transform: translateY(24px) scale(0.96); }
    70%  { opacity: 1; transform: translateY(-3px) scale(1.01); }
    to   { opacity: 1; transform: translateY(0) scale(1); }
}
```

The animation shorthand is `name duration [timing]`. Duration is a positive exact frame count such
as `12f`, or `scene` for the complete local duration of the scene object. Timing may be `linear`,
`ease`, `ease-in`, `ease-out`, or `ease-in-out`. Animations currently play once from the first
local source frame and retain their final value.

Keyframe selectors are `from`, `to`, or percentages. Animatable properties are `left`, `top`,
`width`, `height`, `background`, text `color`, `opacity`, and `transform`. Use consistent units for
a property across stops; mixed `px` and `%` values currently switch discretely at the midpoint.

## Cover scrolling

```css
@text ticker {
    width: 900px;
    height: 48px;
    content: "A deterministic scrolling title";
    font-family: Inter;
    font-size: 26px;
    white-space: nowrap;
    mm-scroll-direction: inline-start;
    mm-scroll-range: cover;
    mm-scroll-duration: scene;
}
```

`mm-scroll-direction` accepts `inline-start`, `inline-end`, `block-start`, or `block-end`.
`mm-scroll-range: cover` moves the entire object from beyond one edge of its containing box to
beyond the opposite edge. Layout and intrinsic measurement happen first, so an automatic-height
column can be used directly for rolling credits. Duration uses the same exact `Nf` or `scene`
syntax as animation.

## Time and caching

Animation evaluates in the generated scene object's source-local frame domain. Moving a placement
changes where it appears in its parent; trimming its source range changes which local animation
frames are visible. The same mapping is used by timeline preview and export, including nested
placements with different exact rational time bases.

Static scenes are rasterized once. Animated scenes are parsed and prepared once, then rendered
frames are kept in a bounded cache. Fonts and images are loaded once per source/canvas revision;
timeline scrubbing does not reparse source or resize resources for a cached frame.

Current limits include no media slots, gradients, paths, borders, margins, animation
delay/repetition, named reusable styles, fallback fonts, color glyphs, Kernel IR, or GPU backend.
