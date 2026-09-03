# mmrecode-isobmff

`mmrecode-isobmff` is the container-owned MP4/QuickTime layer. Its first demuxing slice:

- walks bounded 32-bit, extended-size, and to-end boxes;
- discovers video, audio, and data tracks;
- expands `stts`, `ctts`, `stsc`, `stsz`, `stco`/`co64`, and `stss` sample tables;
- emits generic packets with exact DTS, PTS, duration, and sync flags;
- preserves codec tags and opaque `avcC`/sample-entry configuration;
- exposes pixel aspect, NCLX/NCLC colour, display rotation, and basic audio format metadata; and
- seeks to the closest preceding video sync sample.

Its first writing slice accepts one generic, already-timed H.264 video packet stream and emits a
non-fragmented MP4. It preserves opaque `avcC`, rotation, pixel aspect, colour declarations, sample
payloads, composition offsets, durations, and sync flags. Codec-aware code decides whether packets
are safe to copy; the muxer does not parse H.264.

The crate deliberately has no dependency on `mmrecode-h264`; parameter sets and NAL syntax belong
to the codec. Fragmented MP4, edit lists, multiple sample descriptions, incremental I/O, DRM,
richer metadata preservation, audio/multitrack muxing, interleaving, and files above 4 GiB are not
implemented yet.
