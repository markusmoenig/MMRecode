# MMRecode Architecture

## Status

This document describes the intended architecture and the initial scaffold. APIs are unstable
until at least one complete codec, container/test format, and verification path have exercised the
boundaries.

## Design goals

- Allow applications to select individual codecs and containers.
- Keep codecs independent of containers.
- Keep editing intent independent of encoding syntax.
- Preserve encoded packets without unnecessary decoding or copying.
- Represent exact media time without floating-point timestamps.
- Expose codec dependencies for future smart rendering.
- Make portable safe-Rust implementations the reference behavior.
- Make verification infrastructure reusable across codecs.
- Permit a stable C ABI later without designing the Rust API around C today.

## Non-goals for the initial scaffold

- A complete multimedia framework
- Dynamic runtime codec plugins
- Async I/O throughout the API
- Zero-copy buffers across every possible backend
- A stable public API or ABI
- GPU frame graphs
- A graphical editor
- A universal abstraction over every codec algorithm

## Dependency architecture

Dependencies flow downward only:

```text
                          mmrecode-cli
                               │
                    ┌──────────┴──────────┐
                    │                     │
              mmrecode-edit       mmrecode-quality
                    │
             mmrecode-render
                  ┌─┴───────────┐
                  │             │
             codec crates   container crates
                  │             │
                  └──────┬──────┘
                         │
                  mmrecode-core
                         ▲
                         │
                mmrecode-bitstream
```

The diagram shows conceptual layers; `mmrecode-bitstream` depends on `mmrecode-core` for common
errors, while codec crates depend on both. `mmrecode-core` itself has no MMRecode dependencies.

Forbidden dependencies include:

- `mmrecode-mjpeg` depending on `mmrecode-avi`
- `mmrecode-isobmff` depending on `mmrecode-h264`
- `mmrecode-core` depending on any implementation crate
- `mmrecode-edit` containing MPEG-2 or H.264 syntax rules
- `mmrecode-quality` being embedded in an encoder implementation

## Repository layout

The initial workspace contains only crates needed for the first vertical slice:

```text
crates/
├── core/                    mmrecode-core
├── bitstream/               mmrecode-bitstream
├── codecs/
│   └── mjpeg/               mmrecode-mjpeg
├── containers/
│   └── y4m/                 mmrecode-y4m
├── quality/                 mmrecode-quality
├── testkit/                 mmrecode-testkit
└── cli/                     mmrecode-cli; binary: mmrecode
```

Planned crates are added only when implementation begins:

```text
crates/
├── codecs/
│   ├── dv/                  mmrecode-dv
│   ├── mpeg2/               mmrecode-mpeg2
│   ├── h264/                mmrecode-h264
│   ├── hevc/                mmrecode-hevc
│   ├── av1/                 mmrecode-av1
│   └── vvc/                 mmrecode-vvc
├── containers/
│   ├── avi/                 mmrecode-avi
│   ├── isobmff/             mmrecode-isobmff
│   ├── mpegts/              mmrecode-mpegts
│   └── mxf/                 mmrecode-mxf
├── render/                  mmrecode-render
├── edit/                    mmrecode-edit
├── capi/                    mmrecode-capi
└── facade/                  package: mmrecode
```

An empty crate is not created merely to reserve a name. Planned boundaries live in this document
until they have working code and tests.

## `mmrecode-core`

`mmrecode-core` defines vocabulary and interfaces. It must remain small, unsurprising, and free of
codec algorithms or container syntax.

### Time

`Rational` and `Timestamp` represent exact media time:

```rust
pub struct Rational {
    numerator: i64,
    denominator: i64,
}

pub struct Timestamp {
    pub value: i64,
    pub time_base: Rational,
}
```

Floating-point seconds are acceptable for UI display but not as the authoritative representation
of PTS, DTS, frame duration, edit boundaries, or muxer timing.

Future time operations should include checked rescaling and explicit rounding policies. Silent
timestamp rounding is not acceptable at container or edit boundaries.

### Frames

`VideoFrame` owns or references uncompressed pixel planes and carries timing, field order, and
color interpretation.

The scaffold uses owned `Vec<u8>` planes to keep the first API understandable. A later buffer
abstraction may add reference-counted pools, aligned allocations, hardware surfaces, and borrowed
views. Such optimization must not leak backend-specific behavior into codec algorithms.

Audio frames will be added before container editing. Containers must already allow audio and data
streams even while the first codec is video-only.

### Packets

`Packet` is the primary boundary between containers and codecs:

```text
Demuxer → Packet → Decoder → VideoFrame
VideoFrame → Encoder → Packet → Muxer
```

A packet carries:

- Stream identity
- Encoded bytes
- PTS, DTS, and duration
- Key/corruption flags
- Opaque side data

The initial `Vec<u8>` payload favors clarity. Later zero-copy storage must retain the same semantic
boundary.

### Codec descriptors

`CodecDescriptor` contains an extensible codec identifier, optional container tag, media type, and
opaque configuration bytes.

Containers preserve configuration records without interpreting codec semantics. For example:

- ISO-BMFF may carry an `avcC` configuration record.
- AVI may carry a bitmap/video format block.
- MPEG-TS may derive codec identification from PMT descriptors.

The corresponding codec crate interprets the configuration. This prevents a container crate from
depending on every codec it can carry.

### Codec API

Encoder and decoder interfaces use explicit input and output queues:

```rust
pub trait Decoder {
    fn configure(&mut self, descriptor: &CodecDescriptor) -> Result<()>;
    fn send_packet(&mut self, packet: Packet) -> Result<()>;
    fn receive_frame(&mut self) -> Result<Option<VideoFrame>>;
    fn flush(&mut self) -> Result<()>;
}
```

This model handles frame reordering, codec delay, field pairing, packet fragmentation, and draining
more honestly than `decode(packet) -> frame`.

Codec-specific settings remain typed inside each codec crate. `VideoEncoderSettings.options` is a
temporary escape hatch during scaffolding, not the desired long-term public configuration API.

### Container API

Demuxers own their input and produce packets in container order. Muxers accept stream descriptors
and packets, then finalize indexes and trailing metadata.

The first interfaces are synchronous. Streaming muxers must eventually declare whether they need
seekable output. Async applications can initially adapt synchronous components at their boundary;
the entire codec API should not be made async merely because one data source is a network.

## `mmrecode-bitstream`

This crate provides low-level mechanics that are genuinely reusable:

- Most-significant-bit-first readers and writers
- Byte alignment
- VLC table construction and decoding
- MPEG-style start-code scanning
- Marker scanning
- Checked integer and length helpers

It must not contain MPEG-2 picture syntax, JPEG marker semantics, H.264 Exp-Golomb field meanings,
or container box definitions. Those belong to their format crates.

Bit-level errors should eventually report absolute byte/bit offsets and syntax context.

## Codec crates

Each codec crate owns:

- Codec syntax structures
- Parser and serializer
- Decoder
- Encoder
- Reconstruction path
- Codec-specific configuration
- Dependency analyzer
- Conformance rules
- Codec-local acceleration interfaces
- Unit and regression tests

Suggested internal layout:

```text
mmrecode-mpeg2/src/
├── syntax/
├── parser/
├── decoder/
├── encoder/
├── prediction/
├── transform/
├── quantization/
├── entropy/
├── motion/
├── rate_control/
├── dependency/
└── acceleration/
```

The exact module structure should follow the codec. A JPEG crate does not need artificial motion
or rate-control modules simply to resemble MPEG-2.

### Avoid premature algorithm abstraction

Codecs may share concepts without sharing implementations.

MPEG-2 and H.264 both use motion vectors, but their partitions, interpolation, vector predictors,
and reference rules differ. MPEG-2 and DV both use block transforms, but their exact transforms,
scans, mismatch behavior, and quantization rules differ.

Implement the clear codec-local version first. Extract shared code only after another codec proves
that the behavior and invariants are truly common. Shared acceleration dispatch, buffer layout,
SAD helpers, or test machinery may be reusable even when the full algorithm is not.

## Container crates

Use one crate per real container family:

- `mmrecode-avi`
- `mmrecode-isobmff` for shared MP4 and QuickTime/MOV machinery
- `mmrecode-mpegts`
- `mmrecode-mxf`
- `mmrecode-y4m` for the simple uncompressed test format

Muxing and demuxing normally belong in the same crate because they share syntax, descriptors, and
timestamp rules.

Container implementations own:

- Structural parsing and writing
- Stream discovery
- Sample/packet tables
- Interleaving
- Timestamp conversion
- Seeking and indexing
- Container metadata
- Preservation of unknown or opaque data where possible
- Mapping between container tags and extensible `CodecId` values

Container implementations do not own:

- JPEG entropy parsing
- MPEG-2 GOP semantics
- H.264 SPS/PPS interpretation
- Codec reconstruction
- Encoder decisions

## Dependency analysis and smart rendering

Every inter-frame codec can optionally implement `DependencyAnalyzer`. It converts codec-specific
reference behavior into `AccessUnitInfo`:

- Picture identifier
- Decode and presentation ordering
- Broad picture type
- Referenced pictures
- Random-access strength
- Parameter fingerprint relevant to splicing

A future `mmrecode-render` crate will construct an explicit plan:

```rust
pub enum RenderOperation {
    CopyPackets(PacketRange),
    RewriteTimestamps(PacketRange),
    Decode(FrameRange),
    ApplyEffects(FrameRange),
    BridgeEncode(FrameRange),
    FullEncode(FrameRange),
    Mux,
}
```

The generic planner propagates edit damage through the reference graph. Codec-specific adapters
determine whether a reconnection is valid and which parameters the bridge encoder must match.

`mmrecode-edit` is separate. It models user intent—sources, clips, tracks, ranges, effects, and
transitions—without deciding how encoded data is regenerated.

## Quality and verification

`mmrecode-quality` contains runtime-usable measurements such as:

- Exact plane/frame comparison
- PSNR
- SSIM
- Difference images and statistics
- Later perceptual metrics through optional components

`mmrecode-testkit` is development infrastructure used through `dev-dependencies`:

- Corpus manifests
- External decoder execution
- Frame hashing and comparison
- Mutation and truncation helpers
- Regression report generation
- Reference-stream discovery
- Determinism checks

Production codec users should not pull command runners, large corpora, or external-tool adapters
into their dependency graph.

## CLI and facade

The `mmrecode-cli` package produces one `mmrecode` binary with subcommands:

```text
encode
decode
inspect
verify
compare
benchmark
edit
```

The CLI is an integration client, not the owner of media logic. A behavior useful to another
application belongs in a library crate.

A future top-level `mmrecode` facade crate may re-export implementations behind opt-in features.
Direct dependencies remain supported so users can select only one codec or container.

No feature should enable every codec and container by default.

## Registration

Static Rust dependencies are sufficient initially. Applications instantiate the implementations
they use.

A registry may later map codec identifiers and container probes to constructors. Dynamic plugins
should be considered only after a stable C ABI exists; Rust trait-object ABI is not a plugin ABI.

## C ABI and bindings

`mmrecode-capi` is deferred until Rust APIs and ownership models have survived real codecs.

The C layer should:

- Use opaque handles
- Use explicit structure sizes and ABI versions
- Avoid exposing Rust enum layouts
- Return structured error codes and retrievable diagnostic text
- Permit caller-provided allocation strategies where required
- Remain separate from internal Rust traits

Swift, Kotlin, Python, and other bindings should build on the stable C ABI unless a language has a
strong reason to use a native Rust binding.

## Safety and acceleration

Workspace lints currently forbid unsafe Rust. This establishes a safe portable reference path.

When acceleration is introduced:

- Unsafe and assembly code live in narrowly scoped acceleration modules or crates.
- Every accelerated function has a safe reference implementation.
- Differential tests cover alignment, dimensions, edge extension, bit depth, and CPU features.
- Runtime dispatch never invokes unsupported instructions.
- Performance changes must not silently change normative reconstruction.

## Compatibility and versioning

The workspace begins at version `0.0.1` and is not publishable. APIs may change freely while the
first vertical slice is built.

Before publishing crates:

- Establish a minimum supported Rust version policy.
- Decide which crate APIs are public commitments.
- Add changelogs and release automation.
- Define encoded-output compatibility expectations.
- Define serialized encoder-setting versions.
- Audit dependency licenses and standard-essential patent considerations.

## License boundary

All current crates inherit `Apache-2.0` from the workspace. Third-party code must not be copied into
the repository merely because its functionality is useful. Every dependency and adapted algorithm
requires provenance and license review.

Apache-2.0's contributor patent grant covers only claims licensable by a contributor under the
license terms. It does not grant third-party patent-pool rights for standardized media formats.

## Architectural review questions

Before adding a module or crate, ask:

1. Is this a shared media concept or a format-specific rule?
2. Can the dependency point downward without forming a cycle?
3. Does another codec or container genuinely need this abstraction now?
4. Can the behavior be tested independently of the implementation that uses it?
5. Does this preserve timing, metadata, and encoded data needed for future smart rendering?
6. Would a user needing only one codec be forced to compile unrelated components?

If the answers are unclear, keep the implementation local until evidence establishes the correct
boundary.

