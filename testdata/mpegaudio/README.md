# MPEG audio regression vectors

`valid/sine-48k-stereo-192k.mp2` is a 0.48-second, 48 kHz stereo MPEG-1 Audio Layer II elementary
stream generated from FFmpeg's synthetic sine source. It contains no third-party material.

Run `tools/generate-mpegts-test-vectors.sh` to regenerate it. The vector exercises frame boundary,
bitrate, sample-rate, channel-mode, and exact audio timestamp calculations; it is not a normative
conformance stream.
