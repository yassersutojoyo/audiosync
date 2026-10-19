#!/bin/bash
# Builds and runs AudioSync's standalone tests (no XCTest needed, so it works with
# just the Command Line Tools). Usage: Tests/run-tests.sh [--hardware]
#   --hardware  also runs the engine test, which opens real output devices at
#               zero volume (silent) to check render flow and delay end to end,
#               and the pipe start test, which runs a private AirPlay engine (the
#               bundled one, or AUDIOSYNC_PIPESTART_ENGINE) with only its fifo
#               output in .build/scratch-pipestart and times how late audio
#               leaves the pipe after streaming pauses, stops and starts again.
# Also runs shellcheck on the scripts in scripts/shellcheck-files.txt (it fails
# if shellcheck isn't installed: brew install shellcheck) and the private-reference
# check, scripts/privacy_check.py. The logic and FIFO suites, the soak harness
# and the engine test link the Rust core, so this builds it first with
# scripts/build_core.sh (which needs a Rust toolchain).
set -euo pipefail
cd "$(dirname "$0")/.."
SRC=Sources/AudioSync
OUT=$(mktemp -d "${TMPDIR:-/tmp}/audiosync-tests-XXXXXX")
trap 'rm -rf "$OUT"' EXIT

# The core's C interface and the static library scripts/build_core.sh makes.
CORE=(-I Sources/CAudioSyncCore -L core/target/universal-apple-darwin/release -laudiosync_core)
# The engine's parts on the core, with what they need to compile.
ENGINE_PARTS=(EngineParts.swift CoreEngine.swift OutputRenderer.swift Equalizer.swift Diagnostics.swift
  NativeAirPlayTap.swift)

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

echo "▶ streaming off stops the pipe writer before the engine"
# stopEngine must call engine.stop(), which returns once the pipe writer has
# closed the pipe, before it asks the AirPlay engine to stop. The other way
# round, audio still coming in makes the engine start the pipe again by itself
# (pipe_autostart), and that stray start plays the next session late (EC13).
if awk '
  /func stopEngine\(/ { inside = 1 }
  inside {
    line = $0
    sub(/\/\/.*/, "", line)
    if (!writer && line ~ /engine\.stop\(\)/) writer = NR
    if (!player && line ~ /OwnToneClient\.stopPlayback\(\)/) player = NR
    depth += gsub(/[{]/, "{", line) - gsub(/[}]/, "}", line)
    if (depth > 0) opened = 1
    if (opened && depth == 0) inside = 0
  }
  END { exit !(writer && player && writer < player) }
' "$SRC/AppDelegate+Engine.swift"; then
  echo "PASSED"
else
  echo "FAIL: stopEngine in AppDelegate+Engine.swift must call engine.stop() (the pipe writer)" \
    "before OwnToneClient.stopPlayback() (the engine's stop request)"
  status=1
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

echo "▶ capture IOProc stays real-time safe"
# The capture IOProc runs CaptureSink (ProcessTapCapture.swift) and the
# CaptureChannels it builds (CaptureSupport.swift). Neither may log, lock,
# dispatch or build strings; the debug log is written from the main thread only.
# Comment lines are dropped first; an empty extract means the check lost track.
capture_rt=$(awk '/^    final class CaptureSink/ { f = 1 } f { print } f && /^    }$/ { exit }' \
    "$SRC/ProcessTapCapture.swift"
  awk '/^struct CaptureChannels/ { f = 1 } f { print } f && /^}$/ { exit }' "$SRC/CaptureSupport.swift")
capture_rt=$(grep -vE '^[[:space:]]*//' <<< "$capture_rt" || true)
if ! grep -q 'func consume' <<< "$capture_rt" || ! grep -q 'func copy' <<< "$capture_rt"; then
  echo "FAIL: the capture IOProc's code (CaptureSink, CaptureChannels) wasn't found"
  status=1
elif grep -nE 'DBG|print\(|NSLog|os_log|Logger|DispatchQueue|Lock|Mutex|pthread_|String\(|"' <<< "$capture_rt"; then
  echo "FAIL: the capture IOProc logs, locks, dispatches or builds strings"
  status=1
else
  echo "PASSED"
fi

echo "▶ Rust core (scripts/build_core.sh)"
if scripts/build_core.sh > "$OUT/core.log" 2>&1; then
  echo "▶ logic"
  # The local delay model (LocalDelay.swift) and the device volume hint
  # (DeviceVolume.swift) ask the core, and the texts (Localization.swift)
  # name the capture's errors, which bring the engine's parts along: the
  # logic suite links the core too.
  build logic Tests/logictest.swift PipeMetadata.swift ReceiverInfo.swift NowPlaying.swift Volume.swift AudioSource.swift SpeakerGroups.swift \
    AirPlayPairing.swift AirPlayReconcile.swift EngineLifecycle.swift Notices.swift SpeakerState.swift CaptureCheck.swift CaptureSupport.swift \
    AudioDevices.swift OwnToneOutputs.swift OwnTonePlayer.swift AirPlayEngineConfig.swift LocalNetworkProbe.swift SpeakerLayout.swift Appearance.swift \
    AirPlayImpl.swift NativeAirPlayModel.swift WindowRules.swift AppSettings.swift AppLifecycle.swift LocalDelay.swift DeviceVolume.swift \
    Localization.swift ProcessTapCapture.swift "${ENGINE_PARTS[@]}" -- "${CORE[@]}"
  "$OUT/logic/run" || status=1
  echo "▶ fifo and the core's app-side parts"
  build fifo Tests/fifotest.swift "${ENGINE_PARTS[@]}" PipeMetadata.swift -- "${CORE[@]}"
  "$OUT/fifo/run" || status=1
else
  tail -n 20 "$OUT/core.log"
  echo "❌ the Rust core did not build, so the suites that link it (logic, fifo) can't run"
  status=1
fi

echo "▶ soak rules (the harness's memory check on recorded series)"
build soakrules Tests/soakrulestest.swift -- Tests/soakrules.swift
"$OUT/soakrules/run" || status=1

echo "▶ soak harness (compile only; Tests/soak.sh and Tests/bench.sh run it)"
Tests/build-harness.sh "$OUT/soaktest" || status=1

if [ "${1:-}" = "--hardware" ]; then
  echo "▶ engine (real output devices, silent)"
  build engine Tests/enginetest.swift "${ENGINE_PARTS[@]}" AudioDevices.swift -- "${CORE[@]}"
  "$OUT/engine/run" || status=1
  echo "▶ pipe start latency (a private engine and its fifo output, no audio device)"
  build pipestart Tests/pipestart.swift "${ENGINE_PARTS[@]}" AirPlayEngineConfig.swift OwnTonePlayer.swift -- "${CORE[@]}"
  "$OUT/pipestart/run" "$PWD/.build/scratch-pipestart" || status=1
fi

exit $status
