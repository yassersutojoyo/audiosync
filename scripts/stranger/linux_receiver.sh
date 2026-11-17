#!/usr/bin/env bash
# The test receiver for .github/workflows/stranger.yml: the airplay crate's
# stranger_rx example (its tests' fake AirPlay 1 receiver, PCM only) named
# "Stranger Receiver" on the runner's own address, advertised over mDNS,
# writing what it plays to stdout; heard.py records it with a clock stamp
# per chunk.
#
#   scripts/stranger/linux_receiver.sh start <work folder> <stranger_rx binary>
#   scripts/stranger/linux_receiver.sh stop <work folder>
#
# start leaves receiver.wav, receiver.stamps and receiver.log to fill; stop
# ends the receiver, so the recorder sees the end of its input and closes
# the WAV.
set -euo pipefail

here=$(cd "$(dirname "$0")" && pwd)
usage() { echo "usage: $0 start <work folder> <stranger_rx> | stop <work folder>" >&2; exit 2; }
[ $# -ge 2 ] || usage
mkdir -p "$2"
work=$(realpath "$2")

case "$1" in
  start)
    [ $# -eq 3 ] || usage
    binary=$(realpath "$3")
    # The address the default route leaves from: a LAN address, as a speaker has.
    address=$(ip -4 -o route get 192.0.2.1 | sed -n 's/.* src \([0-9.]*\).*/\1/p')
    echo "receiver: AudioSync's fake AirPlay 1 receiver (airplay/examples/stranger_rx.rs), PCM, on the runner's LAN address" \
      > "$work/receiver-env.txt"
    # shellcheck disable=SC2016 # $1 to $4 expand in the child shell.
    setsid bash -c '"$3" "$4" "Stranger Receiver" 2> "$1/receiver.log" \
      | python3 -I "$2/heard.py" record "$1/receiver.wav" "$1/receiver.stamps" --rate 44100' \
      _ "$work" "$here" "$binary" "$address" > /dev/null 2>&1 < /dev/null &
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
