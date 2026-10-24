#!/usr/bin/env bash
# The Linux tray smoke test, run by .github/workflows/linux.yml: the AppImage
# under Xvfb on a session bus of its own, once with a stand-in StatusNotifier
# watcher (the icon must register and the log say "tray icon: shown") and once
# without (window-only: "tray icon: not shown"). Either way the app must still
# run after 20 s with no error or warning in its debug log.
#
#   Tests/linux-tray-smoke.sh <AppImage> <sni_watcher_stub> [work folder]
#
# The AppImage must be a mock-backend build: AUDIOSYNC_PREVIEW_ZONES shows
# example speakers, so the test needs no audio server. Needs Xvfb and
# dbus-run-session.
set -euo pipefail

if [ "${1:-}" = "--case" ]; then
  # One case, inside dbus-run-session: $2 tray|window, $3 its folder, $4 the
  # AppImage, $5 the watcher stub, $6 the X display.
  name=$2 dir=$3 appimage=$4 stub=$5 display=$6
  export TMPDIR="$dir/tmp" XDG_STATE_HOME="$dir/state" XDG_CONFIG_HOME="$dir/config" \
    XDG_DATA_HOME="$dir/data" XDG_CACHE_HOME="$dir/cache" DISPLAY="$display" \
    AUDIOSYNC_DEBUG=1 AUDIOSYNC_PREVIEW_ZONES=1 SLINT_BACKEND=winit-software \
    APPIMAGE_EXTRACT_AND_RUN=1
  mkdir -p "$TMPDIR" "$XDG_STATE_HOME" "$XDG_CONFIG_HOME" "$XDG_DATA_HOME" "$XDG_CACHE_HOME"
  stub_pid=
  if [ "$name" = tray ]; then
    "$stub" > "$dir/watcher.out" 2>&1 &
    stub_pid=$!
    sleep 1
  fi
  # A process group of its own: the AppImage runtime forks the app.
  setsid "$appimage" > "$dir/app.out" 2>&1 &
  app_pid=$!
  if [ "$name" = tray ]; then expected="tray icon: shown"; else expected="tray icon: not shown"; fi
  log=
  for _ in $(seq 1 15); do
    sleep 1
    log=$(find "$dir" -name '*debug.log' -print -quit)
    if [ -n "$log" ] && grep -q "$expected" "$log" \
      && { [ "$name" != tray ] || grep -q '^registered ' "$dir/watcher.out"; }; then
      break
    fi
  done
  sleep 5
  status=0
  if ! kill -0 "$app_pid" 2>/dev/null; then
    echo "[$name] the app stopped before 20 s" >&2
    status=1
  fi
  if [ -z "$log" ] || ! grep -q "$expected" "$log"; then
    echo "[$name] the log lacks \"$expected\"" >&2
    status=1
  fi
  if [ "$name" = tray ] && ! grep -q '^registered ' "$dir/watcher.out"; then
    echo "[$name] no item registered with the watcher" >&2
    status=1
  fi
  if [ -n "$log" ] && grep -i -E 'error|warning|panic|cannot run|app id: not set' "$log" >&2; then
    echo "[$name] the log has the errors or warnings above" >&2
    status=1
  fi
  kill -- "-$app_pid" 2>/dev/null || true
  if [ -n "$stub_pid" ]; then kill "$stub_pid" 2>/dev/null || true; fi
  wait 2>/dev/null || true
  echo "--- [$name] debug log"
  if [ -n "$log" ]; then cat "$log"; fi
  echo "--- [$name] output"
  cat "$dir/app.out"
  if [ -f "$dir/watcher.out" ]; then echo "--- [$name] watcher"; cat "$dir/watcher.out"; fi
  exit "$status"
fi

if [ $# -lt 2 ]; then
  echo "usage: $0 <AppImage> <sni_watcher_stub> [work folder]" >&2
  exit 2
fi
appimage=$(realpath "$1")
stub=$(realpath "$2")
work=$(realpath -m "${3:-.build/tray-smoke}")
rm -rf "$work"
mkdir -p "$work"

Xvfb :71 -screen 0 1280x800x24 -nolisten tcp > "$work/xvfb.out" 2>&1 &
xvfb_pid=$!
trap 'kill "$xvfb_pid" 2>/dev/null || true' EXIT
sleep 1

status=0
for name in tray window; do
  mkdir -p "$work/$name"
  if dbus-run-session -- "$0" --case "$name" "$work/$name" "$appimage" "$stub" :71; then
    echo "[$name] passed"
  else
    echo "[$name] FAILED" >&2
    status=1
  fi
done
exit "$status"
