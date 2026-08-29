#!/bin/sh
# Regenerate the small independent raw-DV interoperability vectors.
# Requires an FFmpeg build with the dvvideo encoder.
set -eu

repository=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
destination="$repository/testdata/dv/valid"
mkdir -p "$destination"

ffmpeg -hide_banner -loglevel error -y \
    -f lavfi -i "testsrc2=size=720x480:rate=30000/1001:duration=0.034" \
    -f lavfi -i "sine=frequency=997:sample_rate=48000:duration=0.034" \
    -map 0:v -map 1:a -c:v dvvideo -pix_fmt yuv411p \
    -c:a pcm_s16le -ar 48000 -ac 2 -f dv \
    "$destination/dv25-525-60-one-frame.dv"

ffmpeg -hide_banner -loglevel error -y \
    -f lavfi -i "testsrc2=size=720x576:rate=25:duration=0.04" \
    -f lavfi -i "sine=frequency=997:sample_rate=48000:duration=0.04" \
    -map 0:v -map 1:a -c:v dvvideo -pix_fmt yuv420p \
    -c:a pcm_s16le -ar 48000 -ac 2 -f dv \
    "$destination/dv25-625-50-one-frame.dv"

(
    cd "$destination"
    shasum -a 256 ./*.dv > SHA256SUMS
    cat SHA256SUMS
)
