#!/usr/bin/env bash
# The installed app's screens on a clean Linux runner, run by
# .github/workflows/stranger.yml after the package is installed: headless
# PipeWire with null sinks as outputs, Xvfb, a tray host (stalonetray with
# snixembed, which draws StatusNotifier items as tray icons, or else the
# stand-in watcher), the accessibility bus switched on, then per theme a
# launch of the installed audiosync and linux_shots.py driving one screen
# group over AT-SPI. Pictures and not-set-up.txt land in the work folder.
# The nowplaying group adds a stand-in MPRIS player (mpris_player.py) and
# fails when the Now Playing card lacks Shuffle or Repeat.
#
#   scripts/stranger/linux_shots.sh <work folder> <sni_watcher_stub> <main|delay|tray|calibrate|notices|nowplaying>
set -euo pipefail

here=$(cd "$(dirname "$0")" && pwd)

if [ "${1:-}" != --inside ]; then
  [ $# -eq 3 ] || { echo "usage: $0 <work folder> <sni_watcher_stub> <group>" >&2; exit 2; }
  mkdir -p "$1"
  exec dbus-run-session -- "$0" --inside "$@"
fi
shift
work=$(realpath "$1") stub=$2 group=$3

export XDG_RUNTIME_DIR="$work/xdg-runtime" TMPDIR="$work/tmp" XDG_STATE_HOME="$work/state" \
  XDG_CONFIG_HOME="$work/config" XDG_DATA_HOME="$work/data" XDG_CACHE_HOME="$work/cache" DISPLAY=:73
mkdir -p "$XDG_RUNTIME_DIR" "$TMPDIR" "$XDG_STATE_HOME" "$XDG_CONFIG_HOME" "$XDG_DATA_HOME/audiosync" "$XDG_CACHE_HOME"
chmod 700 "$XDG_RUNTIME_DIR"
: > "$work/not-set-up.txt"
pids=()
trap 'kill "${pids[@]}" 2>/dev/null; wait 2>/dev/null || true' EXIT

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

sink() {
  pw-cli create-node adapter "{ factory.name=support.null-audio-sink node.name=$1 node.description=\"$2\" media.class=$3 object.linger=true audio.position=[$4] }" > /dev/null
}
sink stranger-speakers Speakers Audio/Sink 'FL FR'
virtual=false
if [ "$group" = delay ] || [ "$group" = calibrate ]; then
  # Null sinks count as virtual outputs, which the app lists once Show Virtual Devices is on.
  sink stranger-desk 'Desk Speaker' Audio/Sink 'FL FR'
  sink stranger-headset Headset Audio/Sink 'FL FR'
  sink stranger-mic Microphone Audio/Source/Virtual MONO
  virtual=true
fi
sleep 1
pactl set-default-sink stranger-speakers
[ "$virtual" = false ] || pactl set-default-source stranger-mic

if command -v snixembed > /dev/null && command -v stalonetray > /dev/null; then
  stalonetray --geometry 1x1+1240+0 --icon-size 32 --kludges force_icons_size > "$work/tray-host.out" 2>&1 &
  pids+=($!)
  sleep 1
  snixembed > "$work/snixembed.out" 2>&1 &
  pids+=($!)
  export TRAY_ICON_AT="1256 16"
  echo "tray host: stalonetray with snixembed"
else
  "$stub" > "$work/tray-host.out" 2>&1 &
  pids+=($!)
  echo "tray host: the stand-in watcher (no icon drawn; the panel opens through Activate)"
fi
sleep 1

if [ "$group" = nowplaying ]; then
  # A patterned test cover: a two-colour gradient over a checkerboard.
  convert -size 300x300 gradient:'#2f6f8f-#f0c040' -size 300x300 pattern:checkerboard \
    -compose multiply -composite "$work/cover.png"
  python3 -I "$here/mpris_player.py" "$work/cover.png" > "$work/player.out" 2>&1 &
  pids+=($!)
  sleep 1
fi

dbus-send --session --print-reply --dest=org.a11y.Bus /org/a11y/bus org.freedesktop.DBus.Properties.Set \
  string:org.a11y.Status string:IsEnabled variant:boolean:true > /dev/null || echo "accessibility bus: not switched on"

failed=0
for theme in terang gelap; do
  appearance=light
  [ "$theme" = terang ] || appearance=dark
  # Settings > Appearance as the user would leave it; everything else at its default.
  printf '{"version":3,"appearance":"%s","show_virtual_devices":%s}\n' "$appearance" "$virtual" \
    > "$XDG_DATA_HOME/audiosync/settings.json"
  # Software rendering: Xvfb has no GPU.
  AUDIOSYNC_DEBUG=1 SLINT_BACKEND=winit-software setsid audiosync > "$work/app-$theme.out" 2>&1 &
  app=$!
  sleep 5
  python3 -I "$here/linux_shots.py" "$work" "$group" "$theme" "$app" || { echo "driver failed for $theme"; failed=1; }
  kill -TERM -- "-$app" 2>/dev/null || true
  sleep 3
  kill -KILL -- "-$app" 2>/dev/null || true
  log=$(find "$XDG_STATE_HOME" -name '*debug.log' -print -quit)
  [ -z "$log" ] || mv "$log" "$work/audiosync-debug-$theme.log"
done

shots=$(find "$work" -maxdepth 1 -name '*.png' | wc -l)
echo "$shots pictures, $(wc -l < "$work/not-set-up.txt") states not reached"
# A state the runner can't set up is recorded, not failed; an app with no window fails.
[ "$failed" -eq 0 ] && ! grep -q 'no window' "$work/not-set-up.txt"
