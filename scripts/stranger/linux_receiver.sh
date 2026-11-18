#!/usr/bin/env bash
# The test receiver for .github/workflows/stranger.yml: the airplay crate's
# stranger_rx example (its tests' fake AirPlay 1 receiver, PCM only) named
# "Stranger Receiver" in a network namespace of its own, advertised over mDNS,
# writing what it plays to stdout; heard.py records it with a clock stamp
# per chunk.
#
#   scripts/stranger/linux_receiver.sh start <work folder> <stranger_rx binary> [--latency-not-added]
#   scripts/stranger/linux_receiver.sh stop <work folder>
#
# start leaves receiver.wav, receiver.stamps and receiver.log to fill; stop
# ends the receiver, so the recorder sees the end of its input and closes
# the WAV.
set -euo pipefail

here=$(cd "$(dirname "$0")" && pwd)
usage() { echo "usage: $0 start <work folder> <stranger_rx> [--latency-not-added] | stop <work folder>" >&2; exit 2; }
[ $# -ge 2 ] || usage
mkdir -p "$2"
work=$(realpath "$2")

case "$1" in
  start)
    [ $# -eq 3 ] || { [ $# -eq 4 ] && [ "$4" = --latency-not-added ]; } || usage
    binary=$(realpath "$3")
    # A network namespace of its own behind a veth link, as a speaker on the
    # LAN: the app never lists a service at this computer's own address (P234).
    sudo ip netns add stranger-rx
    sudo ip link add stranger-rx0 type veth peer name stranger-rx1 netns stranger-rx
    sudo ip addr add 198.51.100.1/24 dev stranger-rx0
    sudo ip link set stranger-rx0 up
    sudo ip -n stranger-rx addr add 198.51.100.2/24 dev stranger-rx1
    sudo ip -n stranger-rx link set stranger-rx1 up
    sudo ip -n stranger-rx link set lo up
    address=198.51.100.2
    echo "receiver: AudioSync's fake AirPlay 1 receiver (airplay/examples/stranger_rx.rs), PCM, in a network namespace on a veth link" \
      > "$work/receiver-env.txt"
    # shellcheck disable=SC2016 # $1 to $6 expand in the child shell.
    setsid bash -c 'sudo ip netns exec stranger-rx sudo -u "$5" -- "$3" "$4" "Stranger Receiver" ${6:+"$6"} 2> "$1/receiver.log" \
      | python3 -I "$2/heard.py" record "$1/receiver.wav" "$1/receiver.stamps" --rate 44100' \
      _ "$work" "$here" "$binary" "$address" "$(id -un)" "${4:-}" > /dev/null 2>&1 < /dev/null &
    sleep 2
    pgrep -f "$binary $address" > /dev/null \
      || { echo "the receiver did not start:" >&2; cat "$work/receiver.log" >&2; exit 1; }
    cat "$work/receiver.log"
    ;;
  stop)
    pkill -f "examples/stranger_rx " || true
    for _ in $(seq 1 10); do
      pgrep -f "heard.py record $work/receiver.wav" > /dev/null || break
      sleep 1
    done
    echo "--- receiver log"
    cat "$work/receiver.log"
    ;;
  *)
    usage
    ;;
esac
