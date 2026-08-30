#!/bin/sh
set -eu

repository=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
output="$repository/testdata/mpegts/valid"
audio_output="$repository/testdata/mpegaudio/valid"
mkdir -p "$output" "$audio_output"

ffmpeg -hide_banner -loglevel error -y \
  -f lavfi -i "testsrc2=size=96x64:rate=25" \
  -frames:v 12 -an -c:v mpeg2video -pix_fmt yuv420p \
  -g 12 -bf 2 -b:v 500k -threads 1 \
  -mpegts_transport_stream_id 1 -mpegts_service_id 1 \
  -mpegts_pmt_start_pid 4096 -mpegts_start_pid 256 \
  -f mpegts "$output/single-program-mpeg2.ts"

ffmpeg -hide_banner -loglevel error -y \
  -f lavfi -i "sine=frequency=1000:sample_rate=48000:duration=0.48" \
  -vn -c:a mp2 -b:a 192k -ar 48000 -ac 2 -threads 1 \
  -f mp2 "$audio_output/sine-48k-stereo-192k.mp2"

ffmpeg -hide_banner -loglevel error -y \
  -f lavfi -i "testsrc2=size=96x64:rate=25:duration=0.48" \
  -f lavfi -i "sine=frequency=1000:sample_rate=48000:duration=0.48" \
  -map 0:v:0 -map 1:a:0 -c:v mpeg2video -pix_fmt yuv420p \
  -g 12 -bf 2 -b:v 500k -c:a mp2 -b:a 192k -ar 48000 -ac 2 -threads 1 \
  -mpegts_transport_stream_id 1 -mpegts_service_id 1 \
  -mpegts_pmt_start_pid 4096 -mpegts_start_pid 256 \
  -shortest -f mpegts "$output/single-program-mpeg2-mp2.ts"

(cd "$output" && shasum -a 256 single-program-mpeg2.ts single-program-mpeg2-mp2.ts > SHA256SUMS)
(cd "$audio_output" && shasum -a 256 sine-48k-stereo-192k.mp2 > SHA256SUMS)
