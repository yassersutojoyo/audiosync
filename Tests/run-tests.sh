#!/bin/bash
# Builds and runs AudioSync's standalone tests (no XCTest needed, so it works with
# just the Command Line Tools). Usage: Tests/run-tests.sh [--hardware]
#   --hardware  also runs the engine test, which opens real output devices at
#               zero volume (silent) to check render flow and delay end to end,
#               once on the Swift engine and once on the Rust core.
# Also runs shellcheck on the scripts in scripts/shellcheck-files.txt, and fails
# if shellcheck isn't installed (brew install shellcheck). The core parity suite,
# the soak harness and the engine test link the Rust core, so this builds it
# first with scripts/build_core.sh (which needs a Rust toolchain).
set -euo pipefail
cd "$(dirname "$0")/.."
SRC=Sources/AudioSync
OUT=$(mktemp -d "${TMPDIR:-/tmp}/audiosync-tests-XXXXXX")
trap 'rm -rf "$OUT"' EXIT

# The core's C interface and the static library scripts/build_core.sh makes.
CORE=(-I Sources/CAudioSyncCore -L core/target/universal-apple-darwin/release -laudiosync_core)
# The engine parts of both implementations, with what they need to compile.
ENGINE_PARTS=(EngineImpl.swift EngineParts.swift CoreEngine.swift OutputRenderer.swift RingBuffer.swift
  Equalizer.swift FifoFeed.swift Volume.swift SpeakerState.swift AirPlayPairing.swift Diagnostics.swift)

build() {   # build <name> <test file> <source files…> [-- <more swiftc arguments…>]
  local name=$1 test=$2; shift 2
  mkdir -p "$OUT/$name"
  cp "$test" "$OUT/$name/main.swift"
  local srcs=() extra=()
  while [ $# -gt 0 ] && [ "$1" != "--" ]; do srcs+=("$SRC/$1"); shift; done
  if [ $# -gt 0 ]; then shift; extra=("$@"); fi
  swiftc -O -swift-version 5 "$OUT/$name/main.swift" "${srcs[@]}" ${extra[@]+"${extra[@]}"} -o "$OUT/$name/run"
}

status=0
echo "▶ shellcheck"
scripts/shellcheck.sh || status=1

echo "▶ side effects in debug logging"
# DBG.log takes an autoclosure that only runs with AUDIOSYNC_DEBUG=1, so a
# call inside its message never happens in a normal run.
if grep -nE 'DBG\.log\(.*\b(perform|send|write|open|close)\(' "$SRC"/*.swift; then
  echo "FAIL: a call inside DBG.log(...) only runs when debugging is on"
  status=1
else
  echo "PASSED"
fi

echo "▶ no AirPlay master volume"
# The AirPlay engine's master volume is its loudest selected speaker's
# volume, and a master write sets a lone speaker to it: AudioSync sends
# per-speaker volumes only (AirPlayVolume in AirPlayReconcile.swift).
if grep -n 'api/player/volume' "$SRC"/*.swift; then
  echo "FAIL: AudioSync writes the AirPlay engine's master volume"
  status=1
else
  echo "PASSED"
fi

echo "▶ ring buffer"
build ring Tests/ringtest.swift RingBuffer.swift
"$OUT/ring/run" > "$OUT/ring.log" || status=1
grep -E "FAIL|PASSED" "$OUT/ring.log" || true

echo "▶ logic"
build logic Tests/logictest.swift PipeMetadata.swift ReceiverInfo.swift NowPlaying.swift Volume.swift AudioSource.swift SpeakerGroups.swift \
  Equalizer.swift FifoFeed.swift RingBuffer.swift Diagnostics.swift AirPlayPairing.swift AirPlayReconcile.swift EngineLifecycle.swift Notices.swift SpeakerState.swift CaptureCheck.swift \
  AudioDevices.swift OwnToneOutputs.swift OwnTonePlayer.swift AirPlayEngineConfig.swift LocalNetworkProbe.swift SpeakerLayout.swift Appearance.swift \
  EngineImpl.swift
"$OUT/logic/run" || status=1

echo "▶ fifo"
build fifo Tests/fifotest.swift FifoFeed.swift PipeMetadata.swift Diagnostics.swift
"$OUT/fifo/run" || status=1

echo "▶ Rust core (scripts/build_core.sh)"
if scripts/build_core.sh > "$OUT/core.log" 2>&1; then
  echo "▶ core parity (the Swift suites' checks on both engines' parts)"
  build parity Tests/paritytest.swift "${ENGINE_PARTS[@]}" -- "${CORE[@]}"
  "$OUT/parity/run" > "$OUT/parity.log" || status=1
  grep -E "FAIL|KNOWN|PASSED|^core " "$OUT/parity.log" || true
else
  tail -n 20 "$OUT/core.log"
  echo "❌ the Rust core did not build, so the suites that link it can't run"
  status=1
fi

echo "▶ soak harness (compile only; Tests/soak.sh and Tests/bench.sh run it)"
Tests/build-harness.sh "$OUT/soaktest" || status=1

if [ "${1:-}" = "--hardware" ]; then
  echo "▶ engine (real output devices, silent), on each engine"
  build engine Tests/enginetest.swift "${ENGINE_PARTS[@]}" AudioDevices.swift -- "${CORE[@]}"
  for engine in swift rust; do
    AUDIOSYNC_ENGINE=$engine "$OUT/engine/run" || status=1
  done
fi

exit $status
