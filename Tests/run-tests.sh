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
CORE=(-I Sources/CAudioSyncCore -L core/target/universal-apple-darwin/release -laudiosync_core
  -framework Security -framework CoreFoundation)
# The engine's parts on the core, with what they need to compile.
ENGINE_PARTS=(EngineParts.swift CoreEngine.swift OutputRenderer.swift OutputFades.swift Equalizer.swift Diagnostics.swift
  NativeAirPlayTap.swift CastTap.swift CalibrationTap.swift)

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

echo "▶ no em dashes in public docs (scripts/text_check.py)"
{ python3 -I scripts/text_check.py --self-test && python3 -I scripts/text_check.py; } || status=1

echo "▶ side effects in debug logging"
# DebugTrace.log takes an autoclosure that only runs with AUDIOSYNC_DEBUG=1, so a
# call inside its message never happens in a normal run.
if grep -nE 'DebugTrace\.log\(.*\b(perform|send|write|open|close)\(' "$SRC"/*.swift; then
  echo "FAIL: a call inside DebugTrace.log(...) only runs when debugging is on"
  status=1
else
  echo "PASSED"
fi

echo "▶ no SwiftUI macros the Command Line Tools can't expand"
# The Command Line Tools toolchain ships without SwiftUI's macro plugins, so a
# plain @State (a macro in the macOS 27 SDK) builds in CI's Xcode but not with
# ./build.sh on a Mac that has only the Command Line Tools. Keep such state in
# an ObservableObject held by @StateObject or @ObservedObject instead.
if grep -HnE '@State([^[:alnum:]_]|$)' "$SRC"/*.swift | grep -vE '^[^:]+:[0-9]+:[[:space:]]*//'; then
  echo "FAIL: plain @State needs SwiftUI's macro plugin, which the Command Line Tools lack"
  status=1
else
  echo "PASSED"
fi

echo "▶ capture IOProc stays real-time safe"
# The capture IOProc runs CaptureSink (ProcessTapCapture.swift), the
# CaptureChannels it builds (CaptureSupport.swift) and, during a calibration,
# CalibrationTap, which CalibrationFill's thread also writes through while the
# capture is quiet; the calibration's microphone IOProc runs MicrophoneSink. None
# may log, lock, dispatch or build strings; the debug log is written from the
# main thread only.
# Comment lines are dropped first; an empty extract means the check lost track.
capture_rt=$(awk '/^    final class CaptureSink/ { f = 1 } f { print } f && /^    }$/ { exit }' \
    "$SRC/ProcessTapCapture.swift"
  awk '/^struct CaptureChannels/ { f = 1 } f { print } f && /^}$/ { exit }' "$SRC/CaptureSupport.swift"
  awk '/^enum CalibrationTap/ { f = 1 } f { print } f && /^}$/ { exit }' "$SRC/CalibrationTap.swift"
  awk '/MARK: Fill thread/ { f = 1 } f { print } f && /^    }$/ { exit }' "$SRC/CalibrationTap.swift"
  awk '/^    func deliver/ { f = 1 } /^    \/\/\/ A local output reading/ { exit } f { print }' "$SRC/EngineParts.swift"
  awk '/^final class BlockGate/ { f = 1 } f { print } f && /^}$/ { exit }' "$SRC/EngineParts.swift"
  awk '/^private final class MicrophoneSink/ { f = 1 } f { print } f && /^}$/ { exit }' \
    "$SRC/CalibrationMicrophone.swift"
  awk '/AudioDeviceCreateIOProcIDWithBlock/ { f = 1 } f { print } f && /^        }$/ { exit }' \
    "$SRC/CalibrationMicrophone.swift")
capture_rt=$(grep -vE '^[[:space:]]*//' <<< "$capture_rt" || true)
# What runs on the IO threads themselves, without the sinks' setup, must not
# allocate either.
io_only=$(awk '/^        func consume/ { f = 1 } f { print } f && /^        }$/ { exit }' "$SRC/ProcessTapCapture.swift"
  awk '/^    func consume/ { f = 1 } f { print } f && /^    }$/ { exit }' "$SRC/CalibrationMicrophone.swift"
  awk '/MARK: Capture thread/ { f = 1 } /MARK: Main thread/ { exit } f { print }' "$SRC/CalibrationTap.swift"
  awk '/MARK: Fill thread/ { f = 1 } f { print } f && /^    }$/ { exit }' "$SRC/CalibrationTap.swift"
  awk '/^    func deliver/ { f = 1 } /^    \/\/\/ A local output reading/ { exit } f { print }' "$SRC/EngineParts.swift"
  awk '/^final class BlockGate/ { f = 1 } f { print } f && /^}$/ { exit }' "$SRC/EngineParts.swift"
  awk '/^struct CaptureChannels/ { f = 1 } f { print } f && /^}$/ { exit }' "$SRC/CaptureSupport.swift"
  awk '/AudioDeviceCreateIOProcIDWithBlock/ { f = 1 } f { print } f && /^        }$/ { exit }' \
    "$SRC/CalibrationMicrophone.swift")
io_only=$(grep -vE '^[[:space:]]*//' <<< "$io_only" || true)
if ! grep -q 'func copy' <<< "$capture_rt" || ! grep -q 'func addSignal' <<< "$capture_rt" \
    || [ "$(grep -c 'func consume' <<< "$capture_rt")" -lt 2 ] \
    || ! grep -q 'sink.consume(input, at:' <<< "$capture_rt" \
    || [ "$(grep -c 'func consume' <<< "$io_only")" -lt 2 ] || ! grep -q 'func addSignal' <<< "$io_only" \
    || ! grep -q 'func fill' <<< "$io_only" || ! grep -q 'private func write' <<< "$io_only" \
    || ! grep -q 'func enter' <<< "$io_only" || ! grep -q 'private func run' <<< "$io_only"; then
  echo "FAIL: the IOProcs' code (CaptureSink, CaptureChannels, CalibrationTap, MicrophoneSink) wasn't found"
  status=1
elif grep -nE 'DebugTrace|print\(|NSLog|os_log|Logger|DispatchQueue|Lock|Mutex|pthread_|String\(|"' <<< "$capture_rt"; then
  echo "FAIL: the capture IOProc logs, locks, dispatches or builds strings"
  status=1
elif grep -nE 'Array\(|\.allocate\(|\[Float\]\(|\.append\(' <<< "$io_only"; then
  echo "FAIL: an IOProc allocates"
  status=1
else
  echo "PASSED"
fi

echo "▶ cargo lookup (Tests/cargolookuptest.sh)"
Tests/cargolookuptest.sh || status=1

echo "▶ Rust core (scripts/build_core.sh)"
if scripts/build_core.sh > "$OUT/core.log" 2>&1; then
  echo "▶ logic"
  # The local delay model (LocalDelay.swift) and the device volume hint
  # (DeviceVolume.swift) ask the core, and the texts (Localization.swift)
  # name the capture's and the engine's start errors, which bring the
  # engine's parts along (SyncEngine.swift with StreamEngine's StartError): the
  # logic suite links the core too.
  build logic Tests/logictest.swift TrackMetadata.swift ReceiverInfo.swift NowPlaying.swift Volume.swift AudioSource.swift SpeakerGroups.swift \
    AirPlayPairing.swift Notices.swift SpeakerState.swift CaptureCheck.swift CaptureSupport.swift \
    AudioDevices.swift LocalNetworkProbe.swift SpeakerLayout.swift Appearance.swift \
    NativeReceiver.swift NativeAirPlayModel.swift CastModel.swift CastSender.swift WindowRules.swift AppSettings.swift SpeakerNames.swift AppLifecycle.swift LocalDelay.swift DeviceVolume.swift \
    Localization.swift WebRemoteModel.swift CalibrationFlow.swift CalibrateAllFlow.swift GuideArtwork.swift GuideLayout.swift ProcessTapCapture.swift SyncEngine.swift "${ENGINE_PARTS[@]}" -- "${CORE[@]}"
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
