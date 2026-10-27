#!/bin/bash
# The two-speaker sync check of the spec row hardware_two_speaker_sync:
# starts two AirPlay 1 test receivers that write their PCM into FIFOs
# instead of a sound device, plays a click every 4 s to both through the
# airplay example rx_pair with a delay per speaker, and lets the example
# rx_onset time each click on each receiver's play timeline, on one clock.
# Prints rx_onset's pair, drift and PIPE lines, then rx_pair's stated lines
# and its SENDER line: PASS when, read back from the datagrams that left,
# each click's RTP timestamp on each receiver and the time the syncs sent to
# that receiver state for it differ between the receivers by the delay
# difference within one frame. SENDER proves what the sender put on the
# wire; it does not prove the receivers play on time. That is the PIPE line
# (to a few packets on a stdout receiver) and the real-device row. The check
# passes on SENDER PASS and drift PASS; PIPE is information only, and so is
# rx_pair's latency line per receiver: the stated play time's excess over L
# beside the Audio-Latency the receiver reported (P203).
#
#   scripts/two_speaker_sync_wsl.sh --receiver <binary> [--seconds <n>]
#       [--delays <ms>,<ms>] [--host <address>] [--ports <port>,<port>]
#
# Defaults: 30 s, delays 0,500, host 127.0.0.1,
# ports 5000,5100 (UDP from port + 1001). Linux only (WSL included). The
# receiver binary must take `-c <config> -o stdout` and honor sync packets,
# and avahi-daemon must run; the receivers run as the calling user on
# unprivileged ports. Files go to airplay/target/two_speaker_sync. Exits 0
# on PASS, 1 on FAIL or when a receiver didn't play, 2 on bad arguments.
set -euo pipefail

root=$(git rev-parse --show-toplevel)
cd "$root"

usage() {
    echo "usage: scripts/two_speaker_sync_wsl.sh --receiver <binary> [--seconds <n>] [--delays <ms>,<ms>] [--host <address>] [--ports <port>,<port>]" >&2
    exit 2
}

seconds=30
delays=0,500
receiver=
host=127.0.0.1
ports=5000,5100
while [ $# -gt 0 ]; do
    [ $# -ge 2 ] || usage
    case "$1" in
        --seconds) seconds=$2 ;;
        --delays) delays=$2 ;;
        --receiver) receiver=$2 ;;
        --host) host=$2 ;;
        --ports) ports=$2 ;;
        *) usage ;;
    esac
    shift 2
done
[ -n "$receiver" ] || usage
[[ $seconds =~ ^[0-9]+$ ]] || usage
[[ $delays =~ ^[0-9]+,[0-9]+$ ]] || usage
[[ $ports =~ ^[0-9]+,[0-9]+$ ]] || usage
delay_a=${delays%,*}
delay_b=${delays#*,}
port_a=${ports%,*}
port_b=${ports#*,}

if [ "$(uname -s)" != Linux ]; then
    echo "Linux only (WSL included): the receivers write into FIFOs" >&2
    exit 2
fi
if ! command -v "$receiver" >/dev/null 2>&1; then
    echo "no receiver binary '$receiver' (see --receiver)" >&2
    exit 2
fi
if ! pgrep -x avahi-daemon >/dev/null 2>&1; then
    echo "note: avahi-daemon isn't running, so the receivers may not start (sudo systemctl start avahi-daemon)" >&2
fi

# shellcheck source=scripts/cargo_path.sh
. scripts/cargo_path.sh
# A release build binds every address; a debug build would keep to loopback.
cargo build --release --quiet --manifest-path airplay/Cargo.toml --example rx_pair --example rx_onset
examples="${CARGO_TARGET_DIR:-airplay/target}/release/examples"

run=airplay/target/two_speaker_sync
rm -rf "$run"
mkdir -p "$run"

pids=()
cleanup() {
    local pid
    for pid in ${pids[@]+"${pids[@]}"}; do
        kill "$pid" 2>/dev/null || true
    done
}
trap cleanup EXIT

# ignore_volume_control keeps the click at full level, clear of dither.
receiver_config() {
    cat >"$run/$1.conf" <<EOF
general =
{
    name = "Sync check $1";
    port = $2;
    udp_port_base = $(($2 + 1001));
    udp_port_range = 10;
    ignore_volume_control = "yes";
};
EOF
}

max_delay=$((delay_a > delay_b ? delay_a : delay_b))
# Long enough for the last click, the receivers' buffer and the larger delay.
drain=$((3 + (max_delay + 999) / 1000))

mkfifo "$run/a.pcm" "$run/b.pcm"
"$examples/rx_onset" --seconds $((seconds + drain + 30)) --reads "$run" \
    --pcm "$run/a.pcm,delay=$delay_a" --pcm "$run/b.pcm,delay=$delay_b" >"$run/onsets.txt" &
reader=$!

receiver_config a "$port_a"
receiver_config b "$port_b"
"$receiver" -c "$run/a.conf" -o stdout -v >"$run/a.pcm" 2>"$run/a.log" &
pids+=($!)
"$receiver" -c "$run/b.conf" -o stdout -v >"$run/b.pcm" 2>"$run/b.log" &
pids+=($!)
sleep 2

played=0
"$examples/rx_pair" --seconds "$seconds" --click \
    --rx "$host:$port_a,ap1,delay=$delay_a" --rx "$host:$port_b,ap1,delay=$delay_b" \
    >"$run/stated.txt" || played=$?
sleep "$drain"

# Stopping the receivers closes the FIFOs, which ends the reader.
cleanup
pids=()
# The stdout backend has no output clock, so its write times start a whole
# number of packets off its play timeline: PIPE informs, rx_onset's exit doesn't gate.
wait "$reader" || true
cat "$run/onsets.txt" "$run/stated.txt"
if [ "$played" -ne 0 ] && [ "$played" -ne 3 ]; then
    echo "FAIL: a receiver didn't play (rx_pair exited $played); receiver logs: $run/a.log, $run/b.log"
    exit 1
fi
grep -q '^SENDER PASS' "$run/stated.txt" || exit 1
grep -Eq '^drift .* over [1-9][0-9]* clicks .* PASS$' "$run/onsets.txt" || exit 1
