# mmrecode-edit

`mmrecode-edit` describes editing intent without making codec, dependency, or muxing decisions.

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

Current limits are intentional: there is no serialization format, speed mapping, nested sequence,
automation curve, or detailed effect schema yet. These should be added only when the first render
paths establish their requirements.
