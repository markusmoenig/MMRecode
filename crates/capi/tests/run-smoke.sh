#!/bin/sh
set -eu

repo_root=$(CDPATH= cd -- "$(dirname -- "$0")/../../.." && pwd)
output="$repo_root/target/mmrecode-capi-smoke"

cargo build --manifest-path "$repo_root/Cargo.toml" -p mmrecode-capi
cc -std=c11 -Wall -Wextra -Werror \
    -I "$repo_root/crates/capi/include" \
    "$repo_root/crates/capi/tests/smoke.c" \
    -L "$repo_root/target/debug" -lmmrecode_capi \
    -o "$output"

case "$(uname -s)" in
    Darwin)
        env DYLD_LIBRARY_PATH="$repo_root/target/debug" \
            "$output" "$repo_root/testdata/jpeg/valid/baseline-420.jpg"
        ;;
    *)
        env LD_LIBRARY_PATH="$repo_root/target/debug" \
            "$output" "$repo_root/testdata/jpeg/valid/baseline-420.jpg"
        ;;
esac
