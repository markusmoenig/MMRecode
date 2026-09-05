---
sidebar_position: 2
title: Get started
description: Install MMRecode and try its codec tools, preview, and terminal editor.
---

# Get started

MMRecode is distributed as a Cargo application. You need a working Rust toolchain with Cargo available.

## Install MMRecode

Install the current release from crates.io:

```bash
cargo install mmrecode
```

After installation, the `mmrecode` command is available from your Cargo binary directory.

```bash
mmrecode --help
```

## Choose a terminal

MMRecode is a full-screen terminal editor, not a command that launches a separate GUI. For the richest moving-video preview, use [Kitty](https://sw.kovidgoyal.net/kitty/) or Ghostty; both support the Kitty graphics protocol used by MMRecode's direct RGB preview path.

MMRecode detects terminal capabilities automatically:

- **Kitty and Ghostty:** direct Kitty graphics.
- **iTerm2:** native inline images.
- **Sixel-capable terminals:** Sixel image output.
- **Other true-color terminals:** portable 24-bit Unicode half-block rendering.

The fallback keeps the editor usable in terminals without an image protocol, although native image protocols provide the clearest and most efficient preview.

## Inspect a media file

The `inspect` command recognizes the format and reports typed structure:

```bash
mmrecode inspect input.jpg
mmrecode inspect input.m2v
```

## Decode MPEG-2 Video

Decode a checked-in elementary stream to YUV4MPEG2:

```bash
mmrecode decode input.m2v output.y4m
```

## Open the terminal editor

Run the application without a subcommand:

```bash
mmrecode
```

The editor opens with an Untitled project. Its command prompt and visual workspace are available before media is imported.

```text
Untitled > import projects/output.ts as Clip0
Untitled > save as MyFilm
```

Use `help` for the concise vocabulary and `man <command>` for detailed command help.

## Preview supported media

```bash
mmrecode preview input.ts
```

Terminal preview selects Kitty graphics, Sixel, iTerm2 images, or a portable 24-bit half-block renderer according to terminal capability.

## Render an MMFX scene

```bash
mmrecode render-mmfx lower-third.mmfx output.png
mmrecode render-mmfx motion-layout.mmfx frame-23.png --frame 23 --frames 60
```

The command renders through the typed parser, deterministic font/image resource handling, text
shaping, layout, exact local-frame animation, vector coverage, and linear-light CPU compositor.
See the [Scene language](./mmfx/scene-language) reference and [rendered examples](./mmfx/examples).

## Current expectations

MMRecode is under active development and its capabilities continue to grow quickly. See [Project status](./project-status) for current format coverage. Contributors can find the source and development instructions in the [GitHub repository](https://github.com/markusmoenig/MMRecode).
