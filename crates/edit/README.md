# mmrecode-edit

`mmrecode-edit` owns both the recursive authoring graph and the flattened editing intent without
making codec, dependency, or muxing decisions.

The authoring graph has no artificial track/folder hierarchy. The project and every media node own
an ordered local timeline of child placement links. Video, audio, text, images, effects, masks,
generators, and compositions use the same extensible media-kind boundary. A link carries stable
identity, a human alias, child source range, and parent-local timeline range. Contextual paths such
as `/Clip0/Title` traverse those links; cycles are rejected, while reusable media definitions may
have multiple acyclic placements.

The first typed command/session slice implements `pwd`, `ls`, `info`, `cd`, `add`, `in`, `out`,
`undo`, and `redo`. `mmrecode edit` and `mmrecode edit <script>` use the same parser and
`EditorSession`. Mutations return an explicit changed event so an interactive frontend can refresh
preview without making preview a script-side edit semantic.

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

The older `EditSequence` structures remain the flattened renderer-facing target. Compiling the
recursive graph into that target is subsequent work.

Current limits are intentional: there is not yet a persisted project format, media import/relink,
graph-to-render compiler, terminal image preview, speed mapping, automation curve, or detailed
effect schema.
