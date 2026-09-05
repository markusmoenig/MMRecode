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

The operation vocabulary includes decode, effect, bridge-encode, full-encode, and mux steps. The
inter-frame planner accepts frame-aligned ranges from multiple compatible analyzed sources plus
localized changed intervals. It:

- consumes codec-independent decode and presentation order plus reference-picture identifiers;
- propagates changed pictures through dependent access units;
- regenerates pictures whose references cross the beginning or end of a selected source range;
- separates directly edited, bridge-encoded, and reusable pictures;
- includes unchanged reference pictures needed as decoder preroll;
- reserves exact output packet slots for copied and regenerated runs; and
- reports copied, decoded, and encoded picture counts with a human-readable reason.

A real MPEG-2 I/P/B vector verifies that these generic decisions match the codec-local smart-render
plan. An unchanged MPEG-2 elementary stream also passes losslessly through the generic packet
executor, while a two-source vector covers arbitrary start/end cuts and concatenation.

The optional `mpeg2` Cargo feature adds the native MPEG-2 bridge adapter without making MPEG-2 a
default dependency of the generic renderer. It accepts compositor-produced replacement frames,
decodes required source references, regenerates each affected run as a closed GOP, fills the
reserved packet slots, and preserves unaffected packet payloads and side data. The completed splice
is reparsed, dependency-checked, decoded natively, and conditionally decoded by FFmpeg in integration
tests. The executor handles frame-aligned ranges from multiple compatible fixed-rate sources. A
cut that imports references from outside its selected range is regenerated; dependency damage is
propagated until the executor can resume byte-preserving packet copy. Exact source packet mappings
and continuous output PTS/DTS remain visible in the generic plan. It emits a fresh sequence header
for each regenerated run and matches reconstruction-critical dimensions, chroma,
frame rate, progressive mode, profile/level, aspect ratio, sequence-display/colour metadata, and
luma/chroma quantizer matrices. The matrices are used by the encoder as well as signalled. GOP
timecodes retain the source origin and are recomputed for each closed bridge GOP.
For a multi-source plan, the first packet source establishes regenerated metadata and the bridge
timecode origin; GOP headers in copied regions remain byte-preserved and may therefore retain each
source's original timecode labels.

`execute_mpeg2_plan_with_report` exposes the splice contract. Source bitrate and VBV-buffer fields
are preserved only when they already match the reference encoder's declared Main-Level settings;
otherwise the generated headers use those settings and report `Rewritten`. Picture `vbv_delay`
uses the explicit `0xffff` VBR value. This is header honesty, not a claim of production VBV
continuity or rate control.

The optional `mpegts` feature (which includes `mpeg2`) adds the first direct delivery path. It turns
the executed MPEG-2 packets and optional complete MPEG-1 Layer II stream into a validated dry-run
plan, with copied/regenerated packet counts, elementary byte estimates, exact stream ends, and A/V
end delta. Audio boundaries are never rounded silently: callers choose `Exact`, `Contained`, or
`Cover` complete-frame behavior. Execution drives `MpegTsMuxer` with the same inspected packet
schedule. A permanent 480 ms A/V vector verifies the middle-of-GOP bridge through native demux and
decode plus FFmpeg. A separate multi-source vector cuts both ends inside GOPs, bridges the damaged
boundaries, resumes packet copy between them, concatenates a second source, and verifies the
resulting 14-frame timeline natively and with FFmpeg. Production VBV continuity, transitions,
multi-clip audio, and progress/cancellation remain future slices.

The optional `h264` feature adds a deliberately narrower lossless path. It indexes AVC samples in
an MP4/MOV, accepts only half-open presentation ranges whose start and end are matching container
sync samples containing IDR pictures, verifies that the selected pictures form complete contiguous
decode-order GOPs with no external dependencies, and remuxes their encoded bytes unchanged into a
video-only MP4. It reports presentation and decode ranges, GOP count, copied bytes, and zero encoded
frames. Cuts inside a GOP are rejected explicitly; they are not rounded. This is clean-GOP remuxing,
not yet general H.264 smart rendering.

The optional `mmfx` feature adds the reusable CPU project compositor used by both the terminal
monitor and full MPEG-2/TS rendering. The compositor owns no decoder: callers provide decoded base
frames, keeping latest-request-wins scrubbing and sequential export independent of scene work. It
incrementally caches MMFX parsing, host-supplied font/image resources, prepared scenes, static
rasterization, placement scaling, transparent bounds, preview-size variants, and limited/full-range
YUV conversions. Animated scenes evaluate lazily at exact placement-local frames and keep a bounded
overlay cache. Repeated-frame export blends only active prepared pixels directly into planar Yuv420p8 rather than
round-tripping the base frame through RGBA. Invalid edits retain the last valid cached pixels for
interactive preview, while export treats diagnostics as fatal. A shared recursive timeline
projection flattens nested placement paths in stable depth-first composition order and maps every
output frame through exact source/timeline transforms. Ancestor trims clip descendants correctly,
including across differing frame rates. The editor keys compositor synchronization by project
revision and hierarchy context, so an unchanged graph is not re-flattened in the playback loop.
The MPEG-2/TS full renderer uses the same projection for nested video and MMFX objects; MMFX assets
remain cached by reusable media identity and scale variant.

The `mmrecode` binary exposes the earlier one-clip path as `render-plan` and `render`. Those
argument-heavy commands are development and integration-test harnesses, not the intended editor
interface. The user-facing direction is one typed command model shared by script files and an
interactive terminal session.
