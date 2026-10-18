#!/bin/bash
# Compiles the soak/benchmark harness (Tests/soaktest.swift) with the engine
# sources it exercises. Used by Tests/soak.sh, Tests/bench.sh and, compile only,
# by Tests/run-tests.sh. Usage: Tests/build-harness.sh OUTPUT_BINARY
set -euo pipefail
cd "$(dirname "$0")/.."
dest=${1:?usage: Tests/build-harness.sh OUTPUT_BINARY}
work=$(mktemp -d "${TMPDIR:-/tmp}/audiosync-harness-XXXXXX")
trap 'rm -rf "$work"' EXIT
cp Tests/soaktest.swift "$work/main.swift"
SRC=Sources/AudioSync
swiftc -O -swift-version 5 "$work/main.swift" \
  "$SRC/OutputRenderer.swift" "$SRC/RingBuffer.swift" "$SRC/AudioDevices.swift" \
  "$SRC/Diagnostics.swift" "$SRC/Volume.swift" "$SRC/FifoFeed.swift" \
  -o "$dest"
