#!/bin/bash
# Builds and runs AudioSync's standalone tests (no XCTest needed, so it works with
# just the Command Line Tools). Usage: Tests/run-tests.sh [--hardware]
#   --hardware  also runs the engine test, which opens real output devices at
#               zero volume (silent) to check render flow and delay end to end.
# Also runs shellcheck on the scripts in scripts/shellcheck-files.txt, and fails
# if shellcheck isn't installed (brew install shellcheck).
set -euo pipefail
cd "$(dirname "$0")/.."
SRC=Sources/AudioSync
OUT=$(mktemp -d "${TMPDIR:-/tmp}/audiosync-tests-XXXXXX")
trap 'rm -rf "$OUT"' EXIT

build() {   # build <name> <test file> <source files…>
  local name=$1 test=$2; shift 2
  mkdir -p "$OUT/$name"
  cp "$test" "$OUT/$name/main.swift"
  local srcs=()
  for f in "$@"; do srcs+=("$SRC/$f"); done
  swiftc -O -swift-version 5 "$OUT/$name/main.swift" "${srcs[@]}" -o "$OUT/$name/run"
}

status=0
echo "▶ shellcheck"
scripts/shellcheck.sh || status=1

echo "▶ ring buffer"
build ring Tests/ringtest.swift RingBuffer.swift
"$OUT/ring/run" > "$OUT/ring.log" || status=1
grep -E "FAIL|PASSED" "$OUT/ring.log" || true

echo "▶ logic"
build logic Tests/logictest.swift Volume.swift AudioSource.swift SpeakerGroups.swift \
  Equalizer.swift FifoFeed.swift RingBuffer.swift Diagnostics.swift AirPlayPairing.swift AirPlayReconcile.swift EngineLifecycle.swift Notices.swift SpeakerState.swift CaptureCheck.swift \
  AudioDevices.swift OwnToneOutputs.swift AirPlayEngineConfig.swift LocalNetworkProbe.swift SpeakerLayout.swift Appearance.swift
"$OUT/logic/run" || status=1

echo "▶ fifo"
build fifo Tests/fifotest.swift FifoFeed.swift
"$OUT/fifo/run" || status=1

if [ "${1:-}" = "--hardware" ]; then
  echo "▶ engine (real output devices, silent)"
  build engine Tests/enginetest.swift OutputRenderer.swift RingBuffer.swift AudioDevices.swift \
    Diagnostics.swift Volume.swift
  "$OUT/engine/run" || status=1
fi

exit $status
