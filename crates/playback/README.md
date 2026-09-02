# mmrecode-playback

Reusable playback timing for MMRecode applications.

The crate maps exact positive rational frame rates to `Duration`, selects frames for media
positions, and manages play, pause, seek, end, loop, wall-clock, and external-clock state.
Applications may synchronize it to rendered audio samples so audio remains the master clock.

Its first decode orchestration is `Mpeg2PlaybackSource`: construction indexes picture metadata and
dependencies without reconstructing pixels; requests run on a background worker from the closest
preceding clean random-access point and deliver only a bounded presentation window. The caller owns
the final frame-cache policy. The crate has no GUI, audio-device, or container dependency.

Current limits:

- fixed-rate video timelines only;
- MPEG-2 video only for indexed asynchronous decode;
- no incremental container packet or audio queues;
- no device output, resampling, or clock-drift estimation;
- no variable-frame-rate timestamp index yet.
