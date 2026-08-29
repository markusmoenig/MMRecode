#!/bin/sh
set -eu

repository=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
output="$repository/testdata/mpegts/valid"
mkdir -p "$output"

ffmpeg -hide_banner -loglevel error -y \
  -f lavfi -i "testsrc2=size=96x64:rate=25" \
  -frames:v 12 -an -c:v mpeg2video -pix_fmt yuv420p \
  -g 12 -bf 2 -b:v 500k -threads 1 \
  -mpegts_transport_stream_id 1 -mpegts_service_id 1 \
  -mpegts_pmt_start_pid 4096 -mpegts_start_pid 256 \
  -f mpegts "$output/single-program-mpeg2.ts"

(cd "$output" && shasum -a 256 single-program-mpeg2.ts > SHA256SUMS)
