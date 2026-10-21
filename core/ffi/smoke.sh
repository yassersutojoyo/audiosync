#!/bin/bash
# Builds audiosync-core as a static library and runs a small Swift program that
# calls it through include/audiosync_core.h: the version, the volume curve, the
# presets, one equalizer pass, a ring with a delay reader (exact delays, no
# frame played twice), the delay helpers, the local delay model, and the
# AirPlay and Google Cast senders' handles without discovery. Nothing plays; no
# audio device or network is touched.
#
# macOS only (needs swiftc from Xcode or the Command Line Tools).
# Usage: core/ffi/smoke.sh
set -o errexit -o nounset -o pipefail

ffi_dir="$(cd "$(dirname "$0")" && pwd)"
core_dir="$(cd "$ffi_dir/.." && pwd)"
target_dir="${CARGO_TARGET_DIR:-$core_dir/target}"

cargo build --release --lib --manifest-path "$core_dir/Cargo.toml" --target-dir "$target_dir"

work_dir="$(mktemp -d "${TMPDIR:-/tmp}/audiosync-core-smoke.XXXXXX")"
trap 'rm -rf "$work_dir"' EXIT

swiftc -O \
    -I "$ffi_dir/swift-smoke" \
    -L "$target_dir/release" \
    "$ffi_dir/swift-smoke/main.swift" \
    -o "$work_dir/swift-smoke"

"$work_dir/swift-smoke"
