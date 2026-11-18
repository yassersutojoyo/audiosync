#!/usr/bin/env bash
# Calibration on a clean Linux runner through a digital loop, run by
# .github/workflows/calibrate-loop.yml after the package is installed. Null
# sinks stand in for the PC's own speakers, a desk speaker, a headset and a
# far speaker; every one but the far speaker is linked into a virtual
# microphone, the desk speaker starting 60 ms off, and the fake AirPlay
# receiver (stranger_rx, in a network namespace of its own) plays what it
# receives into the same microphone. linux_calibrate.py then drives single
# speaker calibration and Calibrate All over AT-SPI and photographs each state.
# A digital loop proves the flow and the numbers, not a room's acoustics.
#
#   scripts/stranger/linux_calibrate.sh <work folder> <sni_watcher_stub> <stranger_rx>
set -euo pipefail

here=$(cd "$(dirname "$0")" && pwd)

if [ "${1:-}" != --inside ]; then
  [ $# -eq 3 ] || { echo "usage: $0 <work folder> <sni_watcher_stub> <stranger_rx>" >&2; exit 2; }
  mkdir -p "$1"
  exec dbus-run-session -- "$0" --inside "$@"
fi
shift
work=$(realpath "$1") stub=$2 receiver=$(realpath "$3")

export XDG_RUNTIME_DIR="$work/xdg-runtime" TMPDIR="$work/tmp" XDG_STATE_HOME="$work/state" \
  XDG_CONFIG_HOME="$work/config" XDG_DATA_HOME="$work/data" XDG_CACHE_HOME="$work/cache" DISPLAY=:73
mkdir -p "$XDG_RUNTIME_DIR" "$TMPDIR" "$XDG_STATE_HOME" "$XDG_CONFIG_HOME" "$XDG_DATA_HOME/audiosync" "$XDG_CACHE_HOME"
chmod 700 "$XDG_RUNTIME_DIR"
: > "$work/not-set-up.txt"
port=18732
pids=()
trap 'kill "${pids[@]}" 2>/dev/null; sudo pkill -f "examples/stranger_rx " 2>/dev/null; wait 2>/dev/null || true' EXIT

{
  echo "loop: digital (null sinks and a virtual microphone in PipeWire), not acoustic"
  echo "os: $(sed -n 's/^PRETTY_NAME=//p' /etc/os-release | tr -d '"')"
  echo "pipewire: $(pipewire --version | sed -n 's/.*libpipewire //p' | tail -1)"
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

# A device.id makes a null sink read as hardware rather than a virtual output;
# the form factor makes "Speakers" the reference and "Headset" a worn output.
node() {
  pw-cli create-node adapter "{ factory.name=support.null-audio-sink node.name=$1 node.description=\"$2\" media.class=$3 object.linger=true audio.position=[$4] $5 }" > /dev/null
}
node stranger-speakers Speakers Audio/Sink 'FL FR' 'device.id=9001 device.form-factor=internal'
node stranger-desk 'Desk Speaker' Audio/Sink 'FL FR' 'device.id=9002'
node stranger-far 'Far Speaker' Audio/Sink 'FL FR' 'device.id=9003'
node stranger-headset Headset Audio/Sink 'FL FR' 'device.id=9004 device.form-factor=headset'
node stranger-system 'System Mix' Audio/Sink 'FL FR' ''
node stranger-ear Ear Audio/Sink 'FL FR' ''
node stranger-mic Microphone Audio/Source/Virtual MONO ''
sleep 1
pactl set-default-sink stranger-system
pactl set-default-source stranger-mic

ear() {
  for channel in FL FR; do
    pw-link "$1:monitor_$channel" "stranger-ear:playback_$channel" || echo "not linked: $1 $channel" | tee -a "$work/loop.txt"
  done
}
ear stranger-speakers
ear stranger-headset
ear stranger-desk
pw-link stranger-ear:monitor_FL stranger-mic:input_MONO || echo "not linked: ear to microphone" | tee -a "$work/loop.txt"
pw-link stranger-ear:monitor_FR stranger-mic:input_MONO || true
# Run 44324336345 put the desk speaker behind pw-loopback --delay 0.04: its
# latency moved about 10 ms between runs, so the desk starts off by a fixed
# saved +60 ms instead and calibration has to bring it back to 0.
echo "desk speaker: linked straight, saved delay +60 ms before the first calibration" | tee -a "$work/loop.txt"
echo "far speaker: not linked, so the microphone never hears it" | tee -a "$work/loop.txt"

# The fake AirPlay receiver on its own address, as a speaker on the LAN; what
# it plays at its sync times goes into the ear.
sudo ip netns add stranger-rx
sudo ip link add stranger-rx0 type veth peer name stranger-rx1 netns stranger-rx
sudo ip addr add 198.51.100.1/24 dev stranger-rx0
sudo ip link set stranger-rx0 up
sudo ip -n stranger-rx addr add 198.51.100.2/24 dev stranger-rx1
sudo ip -n stranger-rx link set stranger-rx1 up
sudo ip -n stranger-rx link set lo up
# shellcheck disable=SC2016 # $1 to $3 expand in the child shell.
setsid bash -c 'sudo ip netns exec stranger-rx sudo -u "$3" -- "$1" 198.51.100.2 "Stranger Receiver" 2> "$2/receiver.log" \
  | pacat --playback --raw --format=s16le --rate=44100 --channels=2 --latency-msec=20 -d stranger-ear' \
  _ "$receiver" "$work" "$(id -un)" > "$work/receiver-pipe.out" 2>&1 < /dev/null &
pids+=($!)
sleep 2
pw-link -l > "$work/links.txt"

"$stub" > "$work/tray-host.out" 2>&1 &
pids+=($!)
dbus-send --session --print-reply --dest=org.a11y.Bus /org/a11y/bus org.freedesktop.DBus.Properties.Set \
  string:org.a11y.Status string:IsEnabled variant:boolean:true > /dev/null || echo "accessibility bus: not switched on"
printf '{"version":3,"appearance":"light","outputs":{"stranger-desk":{"enabled":false,"volume":60,"delay_ms":60}}}\n' \
  > "$XDG_DATA_HOME/audiosync/settings.json"

AUDIOSYNC_DEBUG=1 AUDIOSYNC_WEB_REMOTE=1 AUDIOSYNC_WEB_REMOTE_PORT=$port \
  SLINT_BACKEND=winit-software setsid audiosync > "$work/app.out" 2>&1 &
app=$!
sleep 5
log=
for _ in $(seq 1 30); do
  log=$(find "$XDG_STATE_HOME" -name '*debug.log' -print -quit)
  [ -z "$log" ] || break
  sleep 1
done
status=0
python3 -I "$here/linux_calibrate.py" "$work" "$app" "$log" "$port" || status=1
pw-link -l > "$work/links-after.txt"
kill -TERM -- "-$app" 2>/dev/null || true
sleep 3
kill -KILL -- "-$app" 2>/dev/null || true
[ -z "$log" ] || cp "$log" "$work/audiosync-debug.log"
cp "$XDG_DATA_HOME/audiosync/settings.json" "$work/settings-after.json" 2>/dev/null || true
python3 -I "$here/linux_calibrate.py" --check "$work" || status=1
exit "$status"
