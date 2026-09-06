# mmrecode-mpegaudio

`mmrecode-mpegaudio` provides strict MPEG-1 Audio Layer II elementary-stream framing for transport
and editing workflows. It identifies frame boundaries and exposes bitrate, sample rate, channel
mode, CRC presence, byte ranges, and the fixed 1152-sample duration. Complete mono/stereo streams
can also be reconstructed to interleaved signed-16 PCM through Rodio's Rust Symphonia backend.

There is no Layer II encoder, psychoacoustic model, or concealment path yet; resampling and channel
mixing remain codec-independent render operations. MPEG-2/2.5 audio, Layer I, Layer III, and
free-format streams are rejected explicitly.
