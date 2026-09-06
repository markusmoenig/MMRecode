# MMRecode AAC

This crate owns MPEG-4 AAC syntax needed by containers and codec backends. It parses
`AudioSpecificConfig`, resolves AAC-LC rate/channel/frame metadata, wraps raw MP4 access units in
ADTS, and natively reconstructs the common mono/stereo AAC-LC subset.

`AacLcEncoder` implements the shared PCM-to-packet interface for standard-rate mono/stereo input.
Its first deterministic nonzero mode uses 1024-sample sine long windows, native MDCT analysis,
uniform scalefactors, zero/escape section selection, escape-codebook Huffman emission, and
packet-budget-driven quantization. It signals a 1024-sample priming requirement and emits a tail
packet on flush. A psychoacoustic model, band-specific scalefactors and nonzero codebook choices,
and short-window transient decisions remain later quality stages rather than hidden external
fallbacks.
