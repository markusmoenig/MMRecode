# mmrecode-terminal-shm

Internal POSIX shared-memory transport for MMRecode's experimental local Kitty graphics path.

The terminal editor forbids unsafe Rust. Linux and macOS require a memory mapping to fill a POSIX
shared-memory object, so this crate isolates that operation behind a small safe API and tests the
create, map, verify, and unlink lifecycle. Automatic use waits for terminal capability negotiation;
the editor's compatible temporary-file transport remains the default. This crate contains no
terminal UI, codec, or rendering policy.
