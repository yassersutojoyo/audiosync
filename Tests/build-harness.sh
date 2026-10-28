#!/bin/bash
# Compiles the soak/benchmark harness (Tests/soaktest.swift, with its judging
# rules in Tests/soakrules.swift) with the engine sources it exercises, linked
# with the Rust core that scripts/build_core.sh builds (run it first). Used by
# Tests/soak.sh, Tests/bench.sh and, compile only, by Tests/run-tests.sh.
# Usage: Tests/build-harness.sh OUTPUT_BINARY
set -o errexit -o nounset -o pipefail
cd "$(dirname "$0")/.."
dest=${1:?usage: Tests/build-harness.sh OUTPUT_BINARY}
core_lib=core/target/universal-apple-darwin/release
if [ ! -f "$core_lib/libaudiosync_core.a" ]; then
  echo "no $core_lib/libaudiosync_core.a: run scripts/build_core.sh first" >&2
  exit 1
fi
work=$(mktemp -d "${TMPDIR:-/tmp}/audiosync-harness-XXXXXX")
trap 'rm -rf "$work"' EXIT
cp Tests/soaktest.swift "$work/main.swift"
SRC=Sources/AudioSync
swiftc -O -swift-version 5 "$work/main.swift" Tests/soakrules.swift \
  "$SRC/EngineParts.swift" "$SRC/CoreEngine.swift" "$SRC/OutputRenderer.swift" "$SRC/OutputFades.swift" \
  "$SRC/OutputFormatWatch.swift" "$SRC/AudioDevices.swift" "$SRC/Diagnostics.swift" "$SRC/Equalizer.swift" \
  "$SRC/NativeAirPlayTap.swift" "$SRC/CastTap.swift" "$SRC/CalibrationTap.swift" \
  -I Sources/CAudioSyncCore -L "$core_lib" -laudiosync_core -framework Security -framework CoreFoundation \
  -o "$dest"
