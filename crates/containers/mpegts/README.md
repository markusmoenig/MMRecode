# mmrecode-mpegts

`mmrecode-mpegts` is MMRecode's safe-Rust MPEG-2 Transport Stream container slice. It keeps
H.222.0 systems syntax out of codec crates and exchanges encoded data through the shared
`Packet`, `Demuxer`, and `Muxer` interfaces.

## Implemented

- Strict 188-byte transport-packet parsing with byte ranges, PID/header fields, adaptation flags,
  continuity counters, random-access indicators, and 90 kHz PCR values
- Pointer-aware PSI section reassembly, PAT/PMT discovery, MPEG-2 CRC-32 validation, descriptors,
  multiple-program discovery, and common stream-type-to-`CodecId` mapping
- PES reassembly with packet-length bounds, MPEG-2 optional-header validation, PTS/DTS extraction,
  and MPEG-2 Video elementary-stream extraction
- Deterministic single-program MPEG-2 Video and optional MPEG-1 Audio Layer II muxing with
  timestamp-ordered interleaving, PAT/PMT repetition, PES packetization, PTS/DTS, PCR,
  random-access signalling, adaptation stuffing, and per-PID continuity
- Shared Rust traits, one-shot C functions, CLI inspect/mux/demux/decode/verify commands, and
  viewer container/program inspection
- An independently generated FFmpeg vector, malformed sync/continuity/CRC/truncation tests,
  byte-exact native round trips, and external FFmpeg decoding of native output

## Deliberate current limits

- The muxer emits one program containing one MPEG-2 Video stream and optionally one MPEG-1 Audio
  Layer II stream. Other audio codecs, subtitles, metadata, and multiple programs are not emitted.
- Demuxing discovers multiple programs and common stream types. MPEG Layer II is framed and
  extractable but not decoded to PCM by MMRecode yet.
- Only 188-byte packets are accepted. 192-byte M2TS and 204-byte error-protected packets are not.
- Scrambling, conditional access, DVB/ATSC service tables, splice metadata, live CBR null-packet
  scheduling, network jitter recovery, and timestamp-wrap-aware seeking remain follow-on work.
- Program Stream, VOB, MXF, and other container families belong in separate crates.

The implementation follows ITU-T H.222.0 / ISO/IEC 13818-1 systems syntax. Regression vectors are
interoperability checks, not a normative conformance suite.
