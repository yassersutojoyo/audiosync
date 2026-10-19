#!/bin/bash
# Builds and runs AudioSync's standalone tests (no XCTest needed, so it works with
# just the Command Line Tools). Usage: Tests/run-tests.sh [--hardware]
#   --hardware  also runs the engine test, which opens real output devices at
#               zero volume (silent) to check render flow and delay end to end,
#               once on the Swift engine and once on the Rust core.
# Also runs shellcheck on the scripts in scripts/shellcheck-files.txt (it fails
# if shellcheck isn't installed: brew install shellcheck) and the private-reference
# check, scripts/privacy_check.py. The core parity suite, the soak harness
# and the engine test link the Rust core, so this builds it first with
# scripts/build_core.sh (which needs a Rust toolchain).
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

echo "▶ private references (scripts/privacy_check.py)"
{ python3 -I scripts/privacy_check.py --self-test && python3 -I scripts/privacy_check.py; } || status=1

echo "▶ side effects in debug logging"
# DBG.log takes an autoclosure that only runs with AUDIOSYNC_DEBUG=1, so a
# call inside its message never happens in a normal run.
if grep -nE 'DBG\.log\(.*\b(perform|send|write|open|close)\(' "$SRC"/*.swift; then
  echo "FAIL: a call inside DBG.log(...) only runs when debugging is on"
  status=1
else
  echo "PASSED"
fi

echo "▶ AirPlay volume requests"
# The AirPlay engine's master volume is its loudest selected speaker's
# volume, so a master write (the player volume, or a 'pvol' metadata item)
# sets a lone speaker to it. AudioSync sends per-speaker volumes only, and
# only AirPlayVolume (AirPlayReconcile.swift) builds a speaker's volume or
# switch-on body, so every switch-on carries the speaker's own volume.
requests_ok=1
if grep -nE 'api/player/volume|code: *"pvol"|70766f6c' "$SRC"/*.swift; then
  echo "FAIL: AudioSync writes the AirPlay engine's master volume"
  requests_ok=0
fi
# A body key in an escaped, raw or dictionary literal ("{\"volume\": 5}",
# #"{"selected": true}"#, ["volume": 5]), or the engine's bulk switch
# endpoints, which switch speakers on without a volume.
if grep -nE '"(selected|volume)\\?" *:|outputs/set|/toggle' "$SRC"/*.swift | grep -v "^$SRC/AirPlayReconcile.swift:"; then
  echo "FAIL: a speaker's volume or switch-on body is built outside AirPlayVolume"
  requests_ok=0
fi
# OwnToneClient.select sends every request of AirPlayVolume.selectRequests in
# order: a loop over all but the last (the volume), then the last one (the
# selection), whose answer it waits for. Without the loop the speaker is
# switched on without its volume first, at the engine's own idea of it.
# Line and block comments are dropped first, and the loop must come before the
# first use of the last request.
select_body=$(awk '/static func select\(/ { f = 1 } f { sub(/\/\/.*/, ""); print } f && /^    }$/ { exit }' \
  "$SRC/OwnToneClient.swift" | tr '\n' ' ' | sed -E 's:/\*([^*]|\*+[^*/])*\*+/::g')
select_var=$(sed -nE 's/.*let +([A-Za-z_][A-Za-z0-9_]*) *= *AirPlayVolume\.selectRequests\(.*/\1/p' <<< "$select_body")
select_head=${select_body%%"$select_var".last*}
if [ -z "$select_var" ] \
  || ! grep -qE "for +[A-Za-z_][A-Za-z0-9_]* +in +$select_var\.dropLast\(\) *\{[^}]*[^A-Za-z0-9_.](enqueue|perform)\(" <<< "$select_head" \
  || ! grep -qE "[^A-Za-z0-9_.]$select_var\.last([^A-Za-z0-9_]|\$)" <<< "$select_body"; then
  echo "FAIL: OwnToneClient.select no longer sends every request of AirPlayVolume.selectRequests"
  requests_ok=0
fi
if [ "$requests_ok" = 1 ]; then echo "PASSED"; else status=1; fi

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
