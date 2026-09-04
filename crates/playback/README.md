# mmrecode-playback

Reusable playback timing for MMRecode applications.

The crate maps exact positive rational frame rates to `Duration`, selects frames for media
positions, and manages play, pause, seek, end, loop, wall-clock, and external-clock state.
Applications may synchronize it to rendered audio samples so audio remains the master clock.

Its indexed decode sources cover MPEG-2 elementary video, H.264 in ISO-BMFF, and AAC in ISO-BMFF.
MPEG-2 and H.264 requests reconstruct bounded presentation windows. AAC indexes exact access-unit
timing and currently reconstructs a complete short track to PCM through the shared executor. The
caller owns video frame-cache and audio-device policy; this crate has no GUI or device dependency.

Current limits:

- no incremental container packet or audio queues;
- no device output, resampling, or clock-drift estimation;
- AAC-LC PCM currently uses an optional native FFmpeg bridge; the Rust spectral decoder and browser
  output are not implemented yet.
