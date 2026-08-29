#!/bin/sh
set -eu

repo_root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
output_dir="$repo_root/testdata/mpeg2/valid"
mkdir -p "$output_dir"

common_args="-hide_banner -loglevel error -threads 1 -fflags +bitexact -flags +bitexact"

# Main Profile/Main Level, progressive sequence, closed GOPs, I/P/B pictures.
# shellcheck disable=SC2086
ffmpeg $common_args \
    -f lavfi -i "testsrc2=size=96x64:rate=25:duration=0.48" \
    -an -c:v mpeg2video -profile:v main -level:v main -pix_fmt yuv420p \
    -g 6 -bf 2 -sc_threshold 1000000000 -flags +bitexact+cgop \
    -b:v 500k -maxrate 500k -bufsize 1835008 \
    -f mpeg2video -y "$output_dir/main-ml-progressive-ibp.m2v"

# Main Profile/Main Level, interlaced frame pictures with field DCT/motion syntax.
# shellcheck disable=SC2086
ffmpeg $common_args \
    -f lavfi -i "testsrc2=size=96x64:rate=25:duration=0.48" \
    -an -c:v mpeg2video -profile:v main -level:v main -pix_fmt yuv420p \
    -g 6 -bf 2 -sc_threshold 1000000000 -flags +bitexact+cgop+ilme+ildct -field_order tt \
    -b:v 500k -maxrate 500k -bufsize 1835008 \
    -f mpeg2video -y "$output_dir/main-ml-interlaced-ibp.m2v"

# Main Profile/Main Level, multiple open GOPs with leading B pictures that depend on the prior GOP.
# shellcheck disable=SC2086
ffmpeg $common_args \
    -f lavfi -i "testsrc2=size=96x64:rate=25:duration=0.96" \
    -an -c:v mpeg2video -profile:v main -level:v main -pix_fmt yuv420p \
    -g 12 -bf 2 -sc_threshold 1000000000 -flags +bitexact \
    -b:v 500k -maxrate 500k -bufsize 1835008 \
    -f mpeg2video -y "$output_dir/main-ml-progressive-open-gop.m2v"

(
    cd "$output_dir"
    shasum -a 256 \
        main-ml-progressive-ibp.m2v \
        main-ml-interlaced-ibp.m2v \
        main-ml-progressive-open-gop.m2v > SHA256SUMS
)
