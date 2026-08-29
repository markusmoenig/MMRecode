# MMRecode Concept

## Purpose

MMRecode is an experimental, professional-quality media-codec and editing ecosystem written in
Rust.

The project asks a practical engineering question:

> How much trustworthy, professional codec and media infrastructure can one experienced codec
> architect build today when AI coding agents perform a large part of the implementation work?

MMRecode is initially a professional-interest and open-source project, not a company. It does not
need an immediate commercial wedge or a commitment to reproduce an entire commercial codec SDK.
Useful support, integration, long-term maintenance, analysis, or archival products may emerge
later, but monetization is an option rather than a condition for beginning.

## Perspective

Professional codec software is not defined only by compression efficiency or by whether a test
file plays. It must remain compatible, diagnosable, reproducible, maintainable, and supportable
over long operational lifetimes.

Broadcast and archival installations are deliberately conservative. Working systems may remain
in production for decades. Consequently, older production formats such as Motion JPEG, DV, and
MPEG-2 remain relevant even when newer distribution codecs exist.

MMRecode starts with these durable formats. This creates a technically progressive path and
produces useful components before the project reaches the complexity of AVC, HEVC, AV1, or VVC.

## Codec progression

The intended progression is:

```text
Motion JPEG
    ↓
DV family
    ↓
MPEG-2 Video
    ↓
H.264 / AVC
    ↓
HEVC, AV1, VVC, and other formats as interest justifies
```

Each step introduces another class of engineering problem.

### Motion JPEG

Motion JPEG establishes:

- Bit-level parsing and writing
- DCT, quantization, and entropy coding
- Raw frame and pixel-format representations
- Color conversion and sampling conventions
- Frame- and field-based variants
- Independent frame verification
- Container integration
- Selective copying and frame-local re-encoding

### DV

DV adds:

- Fixed-size compressed frames
- DIF sequences and block organization
- PAL, NTSC, and professional DV-family variants
- Chroma-layout differences
- Embedded and shuffled audio
- Timecode and recording metadata
- Error detection, concealment, and damaged-media behavior
- Archival and camera-file workflows

### MPEG-2 Video

MPEG-2 introduces inter-frame system behavior:

- I, P, and B pictures
- Motion estimation and compensation
- Decode-order versus presentation-order handling
- Open and closed GOPs
- Reference-picture dependencies
- VBV conformance
- Frame and field prediction
- Smart rendering through bridge GOPs

### H.264 and later codecs

H.264 tests whether the architecture survives substantially more complex prediction, transforms,
entropy coding, reference-picture management, random access, and conformance rules. It should be
attempted after the shared substrate has proved useful with earlier codecs, not used to design a
large theoretical framework in advance.

## Core principles

### Rust first

Implement codec and media logic in safe Rust wherever practical. Unsafe code, architecture-
specific SIMD, GPU integration, and assembly must be isolated behind narrow acceleration
interfaces and tested against portable reference paths.

Rust is not itself the product. The benefits sought are explicit ownership, memory safety,
predictable deployment, strong types, portable libraries, and maintainability over long product
lifetimes.

### Modular rather than monolithic

MMRecode is not intended to become another FFmpeg-style monolith.

Codecs, containers, quality analysis, editing, and tools remain independently usable components.
An application needing only DV decoding should not acquire MP4, H.264, an editing timeline, or a
command-line application.

The modular boundary is behavioral, not merely a collection of Cargo packages:

- Codecs do not know about containers.
- Containers transport encoded packets without implementing codecs.
- Editing describes user intent independently of codec syntax.
- Rendering converts edits into copy, rewrite, decode, effect, and encode operations.
- Quality and verification utilities are not hidden inside one encoder.

### Verification is a product property

Correctness cannot be inferred from successful playback.

Every implemented feature should acquire automated, independently checkable evidence. Depending
on the component, this includes:

- Syntax and profile conformance
- Decoder interoperability
- Pixel-exact reconstruction checks
- Independent reference-decoder comparison
- Encoder internal reconstruction versus external decoding
- Round-trip container checks
- Timestamp and ordering verification
- Fuzzing and mutation testing
- Pathological and damaged streams
- PSNR, SSIM, and later perceptual metrics
- Bitrate and rate-distortion regression tests
- Performance and allocation regression tests
- Permanent regression coverage for every discovered failure

The verification infrastructure should eventually be user-facing:

```text
mmrecode inspect input.m2v
mmrecode verify input.mov
mmrecode compare reference.y4m candidate.y4m
mmrecode benchmark corpus.toml
```

### Verification-friendly AI development

AI coding agents may implement modules, tests, optimizations, documentation, and mechanical
integration. Human engineering remains responsible for:

- Architecture and module boundaries
- Interpretation of standards
- Algorithm and quality decisions
- Test independence
- Performance acceptance
- Security and correctness review
- Deciding whether generated code is maintainable

Modules should have narrow responsibilities and strong contracts so generated work can be tested
without trusting the agent that produced it.

Productivity is measured in validated functionality per human engineering hour, not generated
lines of code.

### Professional lifecycle

Professional compatibility includes behavior across releases. MMRecode should develop practices
for:

- Deterministic encoding modes
- Serialized, versioned configurations
- Explicit API and output-behavior changes
- Reproducible builds and test corpora
- Long-lived regression vectors
- Diagnostic errors with byte, picture, and syntax context
- Compatibility modes where operationally justified
- Stable ABI distributions only after the Rust APIs mature

## Containers and media workflows

Container support is part of the ecosystem but remains separate from codec implementation.

Likely formats include:

- YUV4MPEG2 for early uncompressed tests
- AVI for Motion JPEG and DV workflows
- ISO Base Media File Format for MP4 and QuickTime/MOV
- MPEG-2 Transport Stream for broadcast and delivery
- MXF for professional production and archives
- Raw codec elementary streams where applicable

Muxers and demuxers must preserve timestamps, opaque codec configuration, side data, metadata,
and unknown data when possible. A container must not decode codec syntax merely to move packets.

## Editing and minimal recompression

MMRecode may grow into a modular editing and rendering substrate rather than a desktop editor.

The central idea is a codec-aware render planner:

```text
timeline or edit request
          ↓
codec dependency analysis
          ↓
affected-region propagation
          ↓
render plan
   ├── copy encoded packets
   ├── rewrite timestamps or headers
   ├── bridge-encode a boundary region
   ├── render an affected frame range
   └── fully encode when required
```

Motion JPEG and DV provide simple independent-frame cases: unchanged frames can generally be
copied while modified frames are re-encoded. MPEG-2 adds GOP dependencies and bridge encoding.
H.264 and later codecs add more complex reference graphs and random-access semantics.

The planner should be explainable. A future command might report:

```text
Pictures 0–738: copied unchanged
Pictures 739–766: bridge-encoded
Reason: retained picture 742 depends on discarded reference picture 735
Pictures 767–2140: copied unchanged
Copied encoded payload: 98.4%
```

Editing is intentionally not part of the initial implementation milestone. The codec and packet
APIs must preserve the timing and dependency information needed to add it later without a rewrite.

## Initial experiment

The project should earn continued investment through bounded vertical slices.

### First vertical slice: Motion JPEG

Implement only enough infrastructure to demonstrate a trustworthy end-to-end path:

1. Shared media time, packet, frame, and codec interfaces
2. Bit reader, bit writer, VLC support, and marker parsing
3. Y4M input and output
4. Baseline JPEG/Motion-JPEG parser
5. Constrained decoder
6. Constrained encoder with internal reconstruction
7. Independent decode comparison
8. Frame-level quality and regression reports
9. Basic `encode`, `decode`, `inspect`, and `verify` commands

Do not initially build a GUI, C ABI, dynamic plugin system, broad container suite, GPU path, or
elaborate rate-control system.

### Continuation criteria

Continue when:

- AI-assisted implementation remains understandable and reviewable.
- Failures can be reduced to permanent regression tests.
- External decoders confirm output correctness.
- New formats extend rather than destabilize the shared architecture.
- The work remains professionally and intellectually enjoyable.

Reconsider when:

- Most time is spent finding subtle errors in plausible generated code.
- Shared abstractions become dominated by codec-specific exceptions.
- Test independence cannot be maintained.
- The repository accumulates large amounts of code that cannot be confidently supported.

## Possible later applications

Without requiring a new company, useful later outputs may include:

- Open codec libraries
- Supported and reproducibly built SDK distributions
- Long-term-support branches
- Archive ingest, verification, repair, and migration tools
- Frame-accurate and minimal-recompression editing
- Codec and bitstream inspection
- Encoder comparison and rate-distortion analysis
- Hardware codec abstraction
- Customer-specific interoperability work
- C ABI and language bindings
- Certification and conformance assistance

## Licensing

MMRecode initially uses the Apache License, Version 2.0.

Apache-2.0 is permissive enough for proprietary broadcast, archival, and media products to embed
the libraries. It also contains an explicit patent grant from contributors for patent claims they
can license that are necessarily infringed by their contributions.

This source-code license does **not** grant licenses to third-party standard-essential patents.
Patent licensing for JPEG-family formats, DV, MPEG-2, AVC, HEVC, VVC, audio codecs, or container
technologies must be evaluated separately for each use, territory, and distribution model.

The project name and related trademarks are also separate from the source-code license.

