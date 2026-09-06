# mmrecode-isobmff

`mmrecode-isobmff` is the container-owned MP4/QuickTime layer. Its first demuxing slice:

- walks bounded 32-bit, extended-size, and to-end boxes;
- discovers video, audio, and data tracks;
- expands `stts`, `ctts`, `stsc`, `stsz`, `stco`/`co64`, and `stss` sample tables;
- emits generic packets with exact DTS, PTS, duration, and sync flags;
- preserves codec tags and opaque video configuration, and unwraps AAC `esds` to decoder-specific
  `AudioSpecificConfig` bytes;
- exposes pixel aspect, NCLX/NCLC colour, display rotation, and basic audio format metadata;
- applies a rate-1.0 media edit with an optional leading empty edit for presentation timing; and
- seeks to the closest preceding video sync sample.

Its writing slice accepts complete, already-timed H.264 video and AAC audio packet streams and
emits a non-fragmented Fast Start MP4 with `moov` before `mdat`. Samples are physically
interleaved by decode time, each track keeps its exact media clock, and AAC configuration is wrapped
in `esds`. A single rate-1 media edit can trim leading codec priming without removing its encoded
preroll sample. The writer preserves opaque `avcC`, rotation, pixel aspect, colour declarations,
sample payloads, composition offsets, durations, and sync flags. Codec-aware code decides whether
packets are safe to copy; the muxer does not parse H.264 or AAC access units.

The crate deliberately has no dependency on codec crates; parameter sets, AAC configuration fields,
and coded access-unit syntax remain codec concerns. Fragmented MP4, arbitrary multi-segment or
non-unit-rate edit lists, multiple sample descriptions, incremental I/O, DRM, richer metadata
preservation, general codecs/multitrack layouts, and files above 4 GiB are not implemented yet.
