# Eldiron patron credits export fixture

This project embeds `examples/mmfx/eldiron-patrons.mmfx` and plans a YouTube-ready 1080p30
H.264/AAC MP4 export. The source's fixed `300f` scroll duration automatically changes the
temporary one-second scene placement in `build.mmrs` to ten seconds.

Rebuild the saved project and render its export from the repository root:

```console
cargo run --release -p mmrecode -- edit projects/eldiron-patrons/build.mmrs
```

The generated artifacts are:

- `eldiron-patrons.mmrecode` — readable project with the MMFX source embedded.
- `eldiron-patrons-youtube.mp4` — ten-second 1920x1080 H.264 High/AAC-LC integration export.

On September 6, 2026, the optimized native encoder completed all 300 frames in 52 seconds on the
development Mac (about 0.19x real-time). Container, timing, H.264, BT.709, and AAC metadata pass
inspection. This test also exposes a known encoder defect: the IDR frame is clean, but subsequent
inter-predicted frames contain strong previous-position ghosts around the scrolling text. Treat
the MP4 as an integration and performance fixture until inter-frame reconstruction is corrected.

The static alternative is `examples/mmfx/eldiron-patrons-static.mmfx`; it has no intrinsic
duration and retains the duration chosen when it is added to a timeline.
