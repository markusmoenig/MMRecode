# mmrecode-render

`mmrecode-render` converts codec-independent edit intent into explicit, explainable media
operations.

The initial vertical slice plans and executes packet-copy-only cuts and concatenation for one video
track of independently coded access units. Real DV and MJPEG integration vectors exercise the same
generic path. It:

- requires clean, reference-free access units;
- requires clip boundaries to match complete packets;
- verifies codec and parameter compatibility across copied clips;
- requires source and timeline durations to map exactly;
- preserves encoded payloads, flags, and packet side data;
- rewrites PTS, DTS, duration, and output stream identifiers; and
- emits a mux operation marker and container-ready packets.

The operation vocabulary already reserves decode, effect, bridge-encode, full-encode, and mux
steps, but the initial executor rejects plans containing regeneration. MPEG-2 bridge execution,
audio-boundary policy, effects, transitions, progress/cancellation, and direct muxer driving remain
future slices.
