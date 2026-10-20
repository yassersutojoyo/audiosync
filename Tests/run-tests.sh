#!/bin/bash
# Builds and runs AudioSync's standalone tests (no XCTest needed, so it works with
# just the Command Line Tools). Usage: Tests/run-tests.sh [--hardware]
#   --hardware  also runs the engine test, which opens real output devices at
#               zero volume (silent) to check render flow and delay end to end.
# Also runs shellcheck on the scripts in scripts/shellcheck-files.txt (it fails
# if shellcheck isn't installed: brew install shellcheck) and the private-reference
# check, scripts/privacy_check.py. The logic and core suites, the soak harness
# and the engine test link the Rust core, so this builds it first with
# scripts/build_core.sh (which needs a Rust toolchain).
set -o errexit -o nounset -o pipefail
cd "$(dirname "$0")/.."
SRC=Sources/AudioSync
OUT=$(mktemp -d "${TMPDIR:-/tmp}/audiosync-tests-XXXXXX")
# A run passes only by reaching the last line. macOS's bash 3.2 can end a
# run that set -u stopped (an unbound variable) with status 0 once an EXIT
# trap has run, which once let CI pass with suites left unrun, so any exit
# before the end fails.
finished=0
# shellcheck disable=SC2329 # runs from the EXIT trap below, not by name
cleanup() {
  local rc=$?
  rm -rf "$OUT"
  if [ "$finished" != 1 ] && [ "$rc" = 0 ]; then
    echo "❌ Tests/run-tests.sh stopped before its last suite" >&2
    rc=1
  fi
  exit "$rc"
}
trap cleanup EXIT

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
  swiftc -O -swift-version 5 "$OUT/$name/main.swift" ${srcs[@]+"${srcs[@]}"} ${extra[@]+"${extra[@]}"} \
    -o "$OUT/$name/run"
}

status=0
echo "▶ shellcheck"
scripts/shellcheck.sh || status=1

echo "▶ private references (scripts/privacy_check.py)"
# --verify-names checks scripts/forbidden-names.txt against the local plain
# list (info/forbidden-names in the git directory) and skips without one.
{ python3 -I scripts/privacy_check.py --self-test && python3 -I scripts/privacy_check.py \
  && python3 -I scripts/privacy_check.py --verify-names; } || status=1

echo "▶ side effects in debug logging"
# DBG.log takes an autoclosure that only runs with AUDIOSYNC_DEBUG=1, so a
# call inside its message never happens in a normal run.
if grep -nE 'DBG\.log\(.*\b(perform|send|write|open|close)\(' "$SRC"/*.swift; then
  echo "FAIL: a call inside DBG.log(...) only runs when debugging is on"
  status=1
else
  echo "PASSED"
fi

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
  # name the capture's and the engine's start errors, which bring the
  # engine's parts along (SyncEngine.swift with its StartError): the
  # logic suite links the core too.
  build logic Tests/logictest.swift TrackMetadata.swift ReceiverInfo.swift NowPlaying.swift Volume.swift AudioSource.swift SpeakerGroups.swift \
    AirPlayPairing.swift Notices.swift SpeakerState.swift CaptureCheck.swift CaptureSupport.swift \
    AudioDevices.swift LocalNetworkProbe.swift SpeakerLayout.swift Appearance.swift \
    NativeReceiver.swift NativeAirPlayModel.swift WindowRules.swift AppSettings.swift AppLifecycle.swift LocalDelay.swift DeviceVolume.swift \
    Localization.swift ProcessTapCapture.swift SyncEngine.swift "${ENGINE_PARTS[@]}" -- "${CORE[@]}"
  "$OUT/logic/run" || status=1
  echo "▶ core (the app's wrappers of the core)"
  build core Tests/coretest.swift "${ENGINE_PARTS[@]}" -- "${CORE[@]}"
  "$OUT/core/run" || status=1
else
  tail -n 20 "$OUT/core.log"
  echo "❌ the Rust core did not build, so the suites that link it (logic, core) can't run"
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
fi

finished=1
exit $status
