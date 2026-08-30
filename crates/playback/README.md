# mmrecode-playback

Reusable playback timing for MMRecode applications.

The crate maps exact positive rational frame rates to `Duration`, selects frames for media
positions, and manages play, pause, seek, end, loop, wall-clock, and external-clock state. It has no
GUI, audio-device, codec, or container dependency. Applications may synchronize it to rendered
audio samples so audio remains the master clock.

Current limits:

- fixed-rate video timelines only;
- no packet scheduling or decode queues;
- no device output, resampling, or clock-drift estimation;
- no variable-frame-rate timestamp index yet.
