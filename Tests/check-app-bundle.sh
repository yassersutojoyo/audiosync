#!/bin/bash
# Checks an AudioSync.app that ./build.sh made: the bundle layout, Info.plist,
# the license files, the Rust core and Sparkle in the binary, every signature,
# and that nothing else is bundled: AirPlay runs in the app itself, so there
# are no helper programs and no loose libraries. CI runs it on the app
# `./build.sh --no-install` leaves in .build/app; it works on an installed copy
# too.
#
# Usage: Tests/check-app-bundle.sh [--launch] [--signer NAME] PATH/TO/AudioSync.app
#   --launch       also starts the app with AUDIOSYNC_DEBUG=1 for a few seconds
#                  and checks that it keeps running, writes its debug log and
#                  quits on SIGTERM. Quit AudioSync first: a second copy hands
#                  over to the running one and exits.
#   --signer NAME  every signature must be NAME's: the certificate's name, as
#                  in "AudioSync Local Code Signing", or adhoc
set -o errexit -o nounset -o pipefail
cd "$(dirname "$0")/.."

usage() {
  echo "usage: Tests/check-app-bundle.sh [--launch] [--signer NAME] PATH/TO/AudioSync.app" >&2
  exit 2
}
launch=0
want_signer=""
app=""
while [ $# -gt 0 ]; do
  case "$1" in
    --launch) launch=1 ;;
    --signer) [ $# -gt 1 ] || usage; want_signer=$2; shift ;;
    -*) usage ;;
    *) app=${1%/} ;;
  esac
  shift
done
if [ -z "$app" ] || [ ! -d "$app" ]; then usage; fi

contents="$app/Contents"
resources="$contents/Resources"
third_party="$resources/ThirdParty"
failures=0
pass() { echo "  ✓ $1"; }
fail() { echo "  ✗ $1"; failures=$((failures + 1)); }
expect() {   # expect DESCRIPTION COMMAND…: passes when the command succeeds
  local what=$1
  shift
  if "$@" >/dev/null 2>&1; then pass "$what"; else fail "$what"; fi
}
plist() { plutil -extract "$1" raw -o - "$contents/Info.plist" 2>/dev/null; }
# version_le A B: version A (like 15.0) is at most B
version_le() {
  awk -v a="$1" -v b="$2" 'BEGIN {
    n = split(a, x, "."); m = split(b, y, "."); if (m > n) n = m
    for (i = 1; i <= n; i++) { if (x[i] + 0 < y[i] + 0) exit 0; if (x[i] + 0 > y[i] + 0) exit 1 }
    exit 0 }'
}
# minos_at_most FILE VERSION: every slice of FILE targets VERSION or older
minos_at_most() {
  local found=0 v
  for v in $(vtool -show-build "$1" 2>/dev/null | awk '$1 == "minos" { print $2 }'); do
    found=1
    version_le "$v" "$2" || return 1
  done
  [ "$found" = 1 ]
}
has_rpath() { otool -l "$1" | grep "path $2 " >/dev/null; }
# The signer codesign reports: the leaf certificate's name, or "adhoc"
# (codesign lists the certificates from verbosity 2 on).
signer() { codesign -dvv "$1" 2>&1 | awk -F= '/^(Authority|Signature)=/ && !found { print $2; found = 1 }'; }

exe=$(plist CFBundleExecutable) || exe=AudioSync
min=$(plist LSMinimumSystemVersion) || min=15.0
bin="$contents/MacOS/$exe"

echo "▶ Info.plist"
expect "valid property list" plutil -lint "$contents/Info.plist"
expect "the repository's Info.plist, unchanged" cmp -s Info.plist "$contents/Info.plist"
for key in CFBundleExecutable CFBundleIdentifier CFBundleShortVersionString CFBundleVersion \
  LSMinimumSystemVersion CFBundleIconFile ATSApplicationFontsPath \
  NSAudioCaptureUsageDescription NSMicrophoneUsageDescription NSLocalNetworkUsageDescription \
  NSAppleEventsUsageDescription; do
  if [ -n "$(plist "$key" || true)" ]; then pass "$key"; else fail "$key is missing"; fi
done
expect "CFBundleIdentifier is io.github.yassersutojoyo.audiosync" \
  test "$(plist CFBundleIdentifier || true)" = io.github.yassersutojoyo.audiosync

echo "▶ Layout"
expect "Contents/MacOS/$exe is executable" test -x "$bin"
expect "Contents/PkgInfo" test -f "$contents/PkgInfo"
icon=$(plist CFBundleIconFile) || icon=AppIcon
expect "the icon (Resources/${icon%.icns}.icns) is AppIcon.icns" cmp -s AppIcon.icns "$resources/${icon%.icns}.icns"
fonts=$(plist ATSApplicationFontsPath) || fonts=Fonts
expect "Poppins Medium in Resources/$fonts" cmp -s brand/fonts/Poppins-Medium.ttf "$resources/$fonts/Poppins-Medium.ttf"
expect "AudioSync's LICENSE" cmp -s LICENSE "$resources/LICENSE"
expect "ThirdParty/THIRD_PARTY_NOTICES.md" cmp -s THIRD_PARTY_NOTICES.md "$third_party/THIRD_PARTY_NOTICES.md"
expect "ThirdParty/licenses/poppins/OFL.txt" cmp -s brand/fonts/OFL.txt "$third_party/licenses/poppins/OFL.txt"
for notice in COPYRIGHT-library.html MIT.txt Apache-2.0.txt rustc-version.txt crates.txt; do
  expect "ThirdParty/licenses/rust/$notice" test -s "$third_party/licenses/rust/$notice"
done
# The crates the core links include the native AirPlay sender's; mdns-sd
# (its discovery) stands for them.
expect "crates.txt covers the AirPlay sender's crates (mdns-sd)" \
  grep -q '^mdns-sd ' "$third_party/licenses/rust/crates.txt"
expect "Frameworks/Sparkle.framework" test -f "$contents/Frameworks/Sparkle.framework/Versions/Current/Sparkle"
expect "ThirdParty/licenses/sparkle/LICENSE" test -s "$third_party/licenses/sparkle/LICENSE"

echo "▶ App binary"
links_core() { nm -gU "$bin" | grep ' T _asc_version$' >/dev/null; }
links_sparkle() { otool -L "$bin" | grep '@rpath/Sparkle.framework/' >/dev/null; }
expect "contains the Rust audio core (_asc_version)" links_core
expect "links Sparkle.framework" links_sparkle
expect "finds it through the rpath @executable_path/../Frameworks" has_rpath "$bin" @executable_path/../Frameworks
expect "targets macOS $min or older (LSMinimumSystemVersion)" minos_at_most "$bin" "$min"

echo "▶ Signatures"
expect "codesign --verify --deep --strict" codesign --verify --deep --strict "$app"
app_signer=$(signer "$app")
if [ -n "$app_signer" ]; then pass "app signed: $app_signer"; else fail "the app has no signature"; fi
if [ -n "$want_signer" ]; then
  expect "signed by $want_signer" test "$app_signer" = "$want_signer"
fi
macho=0
while IFS= read -r -d '' f; do
  case "$(file -b "$f")" in *Mach-O*) ;; *) continue ;; esac
  macho=$((macho + 1))
  s=$(signer "$f")
  if [ "$s" != "$app_signer" ] || ! codesign --verify --strict "$f" >/dev/null 2>&1; then
    fail "${f#"$app"/}: signed by '${s:-nobody}', the app by '$app_signer'"
  fi
done < <(find "$app" -type f -print0)
pass "checked $macho Mach-O files"
case "$app_signer" in
  "Developer ID Application"*)
    runtime() { codesign -dv "$app" 2>&1 | grep 'flags=0x[0-9a-f]*(runtime)' >/dev/null; }
    expect "Hardened Runtime (Developer ID)" runtime ;;
esac

echo "▶ Nothing besides the app and Sparkle"
# AirPlay goes through the sender inside the app binary (the Rust core), so a
# helper program, a loose library or an engine's version list would be a
# leftover of an older build.
expect "no Contents/Helpers" test ! -e "$contents/Helpers"
only_sparkle() {
  local f
  for f in "$contents/Frameworks"/*; do
    [ -e "$f" ] || continue
    [ "$(basename "$f")" = Sparkle.framework ] || { echo "$f" >&2; return 1; }
  done
}
expect "Frameworks holds only Sparkle.framework" only_sparkle
expect "no ThirdParty/versions.txt" test ! -e "$third_party/versions.txt"

if [ "$launch" = 1 ]; then
  echo "▶ Launch (AUDIOSYNC_DEBUG=1)"
  log=/tmp/audiosync-debug.log
  if pgrep -x "$exe" >/dev/null; then
    fail "AudioSync is already running: quit it first"
  else
    rm -f "$log"
    AUDIOSYNC_DEBUG=1 "$bin" >/dev/null 2>&1 &
    pid=$!
    sleep 8
    expect "still running after 8 s" kill -0 "$pid"
    expect "the debug log names the app" grep -q 'debug log: AudioSync' "$log"
    kill -TERM "$pid" 2>/dev/null || true
    for _ in $(seq 1 100); do
      kill -0 "$pid" 2>/dev/null || break
      sleep 0.1
    done
    if kill -0 "$pid" 2>/dev/null; then
      fail "still running 10 s after SIGTERM"
      kill -KILL "$pid" 2>/dev/null || true
    else
      pass "quit on SIGTERM"
    fi
    wait "$pid" 2>/dev/null || true
    if [ -f "$log" ]; then
      echo "  debug log (first 40 lines):"
      sed -n '1,40p' "$log" | sed 's/^/    /'
    fi
  fi
fi

if [ "$failures" -gt 0 ]; then
  echo "FAIL: $failures check(s) failed"
  exit 1
fi
echo "PASSED"
