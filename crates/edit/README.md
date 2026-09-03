# mmrecode-edit

`mmrecode-edit` owns both the recursive authoring graph and the flattened editing intent without
making codec, dependency, or muxing decisions.

The authoring graph has no artificial track/folder hierarchy. The project and every media node own
an ordered local timeline of child placement links. Video, audio, text, images, effects, masks,
generators, and compositions use the same extensible media-kind boundary. A link carries stable
identity, a human alias, child source range, and parent-local timeline range. Contextual paths such
as `/Clip0/Title` traverse those links; cycles are rejected, while reusable media definitions may
have multiple acyclic placements.

The typed command/session slice implements `new`, project `open`, `save`, media `import`, project
settings/presets, placement `scale`, export requests, `pwd`, `ls`, contextual and focused `info`, `cd`, `add`, `in`,
`out`, `undo`, `redo`, `help`, and `man`. `import` is deliberately a typed host request: the CLI
resolves and probes the locator, then supplies validated `ImportedMedia` metadata to the
terminal-agnostic session. Project settings remain authoritative when source rates differ.
Changing the project frame rate is an undoable root-timeline operation: presentation time is
preserved by default with explicit nearest-frame accounting, while a frame-number-preserving policy
is available when that is the intended edit. Neither policy rewrites source ranges or nested media
time bases. `project match` is a typed host request: the frontend probes the focused source and
returns a complete video/audio settings snapshot, which the session applies atomically with the
same time-preserving conformance and undo semantics.
`mmrecode edit` and `mmrecode edit <script>` use the same parser and `EditorSession`. Mutations
return an explicit changed event so an interactive frontend can refresh preview without making
preview a script-side edit semantic. The CLI's MPEG-2 ES/TS and H.264 MP4/MOV integrations use that event to
update the full-screen terminal preview range immediately after trims and undo/redo. Interactive
frontends may expose contextual aliases such as `left <time>` after an in/out operation, but expand
them into canonical typed trim commands before applying them.

The editor exports canonical command, manual-topic, project-setting, and delivery-preset
vocabularies for frontends. The full-screen completion system consumes those lists, and regression
tests require every command to retain a manual entry and every setting/preset to remain present in
interactive help.

Editor positions use compact non-drop timecode with the frame field on the right. For example,
`out 1:15` selects one second and fifteen frames, `out -0:10` removes ten frames relative to the
current out-point, and `2:01:15` includes minutes. Leading unused fields are omitted in command
output, listings, and the terminal UI. Values resolve against the current media's native frame rate;
the early raw-frame spelling remains accepted only for script compatibility.

The initial slice provides:

- exact source and sequence time ranges;
- sources and their stream descriptors;
- typed source, track, clip, and transition identifiers;
- media tracks and clips;
- codec-independent effects and transitions;
- an output intent; and
- structural validation of references, media types, time bases, and ranges.

The crate deliberately does not decide which packets can be copied or which frames must be
decoded. Those decisions belong to `mmrecode-render` and codec dependency analyzers.

The older `EditSequence` structures remain the flattened renderer-facing target. The CLI export
host always starts from the project root rather than the current `MediaPath`. Its MPEG-2 slice
renders every root placement, trim, position, and gap; a single compatible placement can still use
the packet path as an internal optimization. Progressive multi-placement, source-rate, or canvas
work selects a full-render path with persisted fit/fill/stretch/native sizing. Recursive nested
generated/effect content and alpha-aware composition remain subsequent work.

Project files are readable versioned JSON with resolved authoring settings, stable identifiers,
placement ranges, project-relative managed paths, and explicit external paths. Saves are atomic and
sessions track clean/modified state. The CLI host appends `.mmrecode` to save targets and makes the
first Save As file stem the initial Untitled project's name; the session adopts this canonicalized
snapshot only after the write succeeds. Current limits remain intentional: the host importer accepts
MPEG-2 ES/TS and non-fragmented H.264 MP4/MOV video, while fingerprints, relinking/collection, recursive composition preview and
export, speed mapping, automation curves, and detailed effect schemas are not implemented yet.
