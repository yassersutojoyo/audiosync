#!/usr/bin/env bash
# One stranger's session on Linux, run by .github/workflows/stranger.yml after
# the package is installed and a receiver is up (linux_receiver.sh): headless
# PipeWire, WirePlumber and its PulseAudio server with a null sink as the
# default output (the stand-in PC speaker), Xvfb with a StatusNotifier watcher
# as the tray host, the installed audiosync with AUDIOSYNC_DEBUG=1, the
# receiver switched on through the web remote, a click train played to the
# default output while that output's monitor is recorded, then quit,
# uninstall, and the default output checked against the one found.
#
#   scripts/stranger/linux_session.sh <work folder> <sni_watcher_stub> <uninstall command...>
#
# Writes sink.wav and sink.stamps into the work folder for heard.py check,
# and env.txt naming the environment. Exits 1 when the tray icon doesn't
# register, the app stops or panics, the speaker can't be switched on, or
# the default output changed.
set -euo pipefail

here=$(cd "$(dirname "$0")" && pwd)

if [ "${1:-}" != --inside ]; then
  [ $# -ge 3 ] || { echo "usage: $0 <work folder> <sni_watcher_stub> <uninstall command...>" >&2; exit 2; }
  mkdir -p "$1"
  exec dbus-run-session -- "$0" --inside "$@"
fi
shift
work=$(realpath "$1") stub=$2
shift 2

export XDG_RUNTIME_DIR="$work/xdg-runtime" TMPDIR="$work/tmp" XDG_STATE_HOME="$work/state" \
  XDG_CONFIG_HOME="$work/config" XDG_DATA_HOME="$work/data" XDG_CACHE_HOME="$work/cache"
mkdir -p "$XDG_RUNTIME_DIR" "$TMPDIR" "$XDG_STATE_HOME" "$XDG_CONFIG_HOME" "$XDG_DATA_HOME" "$XDG_CACHE_HOME"
chmod 700 "$XDG_RUNTIME_DIR"
port=18731
pids=()
trap 'kill "${pids[@]}" 2>/dev/null; wait 2>/dev/null || true' EXIT

{
  echo "os: $(sed -n 's/^PRETTY_NAME=//p' /etc/os-release | tr -d '"')"
  echo "kernel: $(uname -r)"
  echo "pipewire: $(pipewire --version | sed -n 's/.*libpipewire //p' | tail -1)"
  echo "wireplumber: $(wireplumber --version | sed -n 's/.*libwireplumber //p' | tail -1)"
  echo "audiosync: $(command -v audiosync)"
} | tee "$work/env.txt"

Xvfb :73 -screen 0 1280x800x24 -nolisten tcp > "$work/xvfb.out" 2>&1 &
pids+=($!)
pipewire > "$work/pipewire.out" 2>&1 &
pids+=($!)
sleep 1
wireplumber > "$work/wireplumber.out" 2>&1 &
pids+=($!)
sleep 2
pipewire-pulse > "$work/pipewire-pulse.out" 2>&1 &
pids+=($!)
sleep 1
pw-cli create-node adapter "{ factory.name=support.null-audio-sink node.name=stranger-pc node.description=\"Stranger PC\" media.class=Audio/Sink object.linger=true audio.position=[FL FR] }" > /dev/null
sleep 1
pactl set-default-sink stranger-pc
before=$(pactl get-default-sink)
echo "default output before: $before"

"$stub" > "$work/watcher.out" 2>&1 &
pids+=($!)
sleep 1

# Software rendering: Xvfb has no GPU.
DISPLAY=:73 AUDIOSYNC_DEBUG=1 AUDIOSYNC_WEB_REMOTE=1 AUDIOSYNC_WEB_REMOTE_PORT=$port \
  SLINT_BACKEND=winit-software setsid audiosync > "$work/app.out" 2>&1 &
app=$!
pids+=("$app")

status=0
log=
for _ in $(seq 1 30); do
  sleep 1
  log=$(find "$TMPDIR" -name '*debug.log' -print -quit)
  if [ -n "$log" ] && grep -q 'tray icon: shown' "$log" && grep -q '^registered ' "$work/watcher.out"; then break; fi
done
if [ -z "$log" ] || ! grep -q 'tray icon: shown' "$log" || ! grep -q '^registered ' "$work/watcher.out"; then
  echo "FAIL: the tray icon did not register with the watcher" >&2
  status=1
else
  echo "tray: icon shown and registered"
fi

if [ -n "$log" ] && python3 -I "$here/remote.py" "$log" "$port" "Stranger Receiver"; then
  python3 -I "$here/heard.py" clicks "$work/clicks.wav" --seconds 45 --rate 48000
  parecord --raw --format=s16le --rate=48000 --channels=2 --latency-msec=20 -d stranger-pc.monitor \
    | python3 -I "$here/heard.py" record "$work/sink.wav" "$work/sink.stamps" --rate 48000 &
  recorder=$!
  # The AirPlay session needs a moment after the switch before audio flows.
  sleep 4
  pw-play --latency 1024 "$work/clicks.wav"
  sleep 6
  pkill -f 'parecord --raw --format=s16le --rate=48000' || true
  wait "$recorder" || true
else
  echo "FAIL: the receiver could not be switched on through the web remote" >&2
  status=1
fi

if kill -0 "$app" 2>/dev/null; then
  echo "app: still running after the stream"
else
  echo "FAIL: the app stopped during the session" >&2
  status=1
fi
kill -TERM -- "-$app" 2>/dev/null || true
sleep 3
kill -KILL -- "-$app" 2>/dev/null || true

if [ -n "$log" ]; then
  if grep -i 'panic' "$log" >&2; then
    echo "FAIL: the debug log has a panic" >&2
    status=1
  fi
  echo "--- error and warning lines in the debug log (information)"
  grep -i -E 'error|warning' "$log" || echo "(none)"
fi

echo "uninstall: $*"
"$@"
if command -v audiosync > /dev/null; then
  echo "FAIL: audiosync is still on PATH after the uninstall" >&2
  status=1
fi
after=$(pactl get-default-sink)
echo "default output after uninstall: $after"
if [ "$after" != "$before" ]; then
  echo "FAIL: the default output changed from $before to $after" >&2
  status=1
fi
exit "$status"
