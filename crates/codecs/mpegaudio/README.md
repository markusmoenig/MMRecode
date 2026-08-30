# mmrecode-mpegaudio

`mmrecode-mpegaudio` currently provides strict MPEG-1 Audio Layer II elementary-stream framing for
transport and editing workflows. It identifies frame boundaries and exposes bitrate, sample rate,
channel mode, CRC presence, byte ranges, and the fixed 1152-sample duration.

The initial scope is intentionally pass-through only: there is no PCM decoder, encoder,
psychoacoustic model, resampler, channel mixer, or concealment path yet. MPEG-2/2.5 audio,
Layer I, Layer III, and free-format streams are rejected explicitly.
