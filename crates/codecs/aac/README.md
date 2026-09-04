# MMRecode AAC

This crate owns MPEG-4 AAC syntax needed by containers and decoder backends. The current slice
parses `AudioSpecificConfig`, resolves AAC-LC rate/channel/frame metadata, and wraps raw MP4 access
units in ADTS for interoperability and decoder-oracle testing.

Spectral reconstruction, SBR/PS, encoding, and AAC muxing remain later milestones.
