#!/bin/bash
# The several-receiver check of P212 on open receivers: three AirPlay 2 test
# receivers, each in a network namespace of its own with its own nqptp,
# D-Bus and avahi-daemon (private /run/dbus, /run/avahi-daemon and /dev/shm),
# on a bridge inside a namespace of its own with TEST-NET-1 addresses, so
# nothing listens on the host's network. The airplay example rx_pair, in a
# fourth namespace, plays a click every 4 s to two of them, turns the third
# on mid-play (--join) and off again (--leave); rx_onset times each click in
# each receiver's PCM, read from FIFOs on one clock. Checks, each printed as
# PASS or FAIL:
#   setpeers  every SETPEERS a receiver logged was answered 2xx; the first
#             two got an update naming the third and, after it, one naming
#             the other of the two without the third (P209, P210); the
#             third's list named the first two; the sender warned about none
#   p206      the sender logged no GrandmasterOutranked warning
#   gap       the first two's PCM never paused for more than --gap-ms, the
#             join and the leave included, until the sender stops (100 ms
#             by default; the pipe's own pace gave up to 46 ms)
#   onsets    every click's onsets on all receivers that heard it lie
#             within --limit-ms of each other, each timed from the reads in
#             the second before it, once the receiver has played 20 s
#             (rx_onset's SPREAD line); the first two are judged across the
#             join and the leave
#   settling  the same over every click, a receiver's first 20 s included,
#             within --settle-ms (rx_onset's SETTLING line)
# The onset limit defaults to 10 ms, calibration's error: the largest
# uncertainty at which a calibration row still applies (S236), so a user
# couldn't tell or correct receivers closer than that. It also clears the
# instrument's own error: a pipe receiver has no output clock and its
# writes slip by a whole packet (352 frames, 8 ms) now and then. The settling
# limit defaults to 100 ms: a joiner's PTP helper lines up over its first
# seconds while the WSL clock it reads steps back about 0.6 s every 32 s,
# which left a joiner up to 48 ms off soon after it began (16 WSL runs,
# 8 Nov 2026); twice that is a fault, not the helper lining up. Settles the
# protocol side only; Apple's receivers stay a check on real devices (G20).
#
#   scripts/several_ap2_receivers_wsl.sh --examples <dir> --receiver <binary>
#       [--seconds <n>] [--join <s>] [--leave <s>] [--limit-ms <ms>]
#       [--settle-ms <ms>] [--gap-ms <ms>]
#
# <dir> holds release builds of the examples rx_pair and rx_onset. The
# receiver is an AirPlay 2 test receiver built with its stdout backend that
# takes `-c <config> -o stdout` and its PTP helper nqptp. Defaults: 94 s,
# join at 30 s, leave at 74 s. Root only (namespaces); from Windows run it with
# wsl.exe -u root. Logs go to airplay/target/several_ap2_receivers. Removes
# every namespace and process it started, also on failure. Exits 0 on PASS,
# 1 on FAIL, 2 on bad arguments.
# covers: P212
set -euo pipefail

root=$(git rev-parse --show-toplevel 2>/dev/null || dirname "$(dirname "$(realpath "$0")")")
cd "$root"

usage() {
    echo "usage: scripts/several_ap2_receivers_wsl.sh --examples <dir> --receiver <binary> [--seconds <n>] [--join <s>] [--leave <s>] [--limit-ms <ms>] [--settle-ms <ms>] [--gap-ms <ms>]" >&2
    exit 2
}

examples=
receiver=
seconds=94
join=30
leave=74
limit_ms=10
settle_ms=100
gap_ms=100
while [ $# -gt 0 ]; do
    [ $# -ge 2 ] || usage
    case "$1" in
        --examples) examples=$2 ;;
        --receiver) receiver=$2 ;;
        --seconds) seconds=$2 ;;
        --join) join=$2 ;;
        --leave) leave=$2 ;;
        --limit-ms) limit_ms=$2 ;;
        --settle-ms) settle_ms=$2 ;;
        --gap-ms) gap_ms=$2 ;;
        *) usage ;;
    esac
    shift 2
done
[ -n "$examples" ] && [ -n "$receiver" ] || usage
for number in "$seconds" "$join" "$leave" "$limit_ms" "$settle_ms" "$gap_ms"; do
    [[ $number =~ ^[0-9]+$ ]] || usage
done
[ "$join" -lt "$leave" ] && [ "$leave" -lt "$seconds" ] || usage
if [ "$(id -u)" -ne 0 ]; then
    echo "root only: the receivers run in network namespaces" >&2
    exit 2
fi
for tool in "$receiver" nqptp dbus-daemon avahi-daemon unshare ip "$examples/rx_pair" "$examples/rx_onset"; do
    command -v "$tool" >/dev/null 2>&1 || {
        echo "missing: $tool" >&2
        exit 2
    }
done

names=(a b c)
run=airplay/target/several_ap2_receivers
rm -rf "$run"
mkdir -p "$run"
fifos=
reader=

teardown() {
    local ns pids
    for ns in ap2s-a ap2s-b ap2s-c ap2s-tx ap2s-hub; do
        ip netns list | grep -qw "$ns" || continue
        pids=$(ip netns pids "$ns")
        # shellcheck disable=SC2086
        [ -z "$pids" ] || kill $pids 2>/dev/null || true
        sleep 0.5
        pids=$(ip netns pids "$ns")
        # shellcheck disable=SC2086
        [ -z "$pids" ] || kill -9 $pids 2>/dev/null || true
        ip netns del "$ns"
    done
    [ -z "$reader" ] || kill "$reader" 2>/dev/null || true
    [ -z "$fifos" ] || rm -rf "$fifos"
}
trap teardown EXIT
teardown
fifos=$(mktemp -d)

ip netns add ap2s-hub
ip -n ap2s-hub link add br0 type bridge
ip -n ap2s-hub link set br0 up

# attach <namespace> <port name> <address>
attach() {
    ip netns add "$1"
    ip -n ap2s-hub link add "$2" type veth peer name eth0 netns "$1"
    ip -n ap2s-hub link set "$2" master br0 up
    ip -n "$1" addr add "$3/24" dev eth0
    ip -n "$1" link set eth0 up
    ip -n "$1" link set lo up
}

attach ap2s-tx v-tx 192.0.2.1
for index in 0 1 2; do
    name=${names[$index]}
    attach "ap2s-$name" "v-$name" "192.0.2.1$((index + 1))"
    cat >"$run/$name.conf" <<EOF
general =
{
    name = "Several check $name";
    port = 7000;
    ignore_volume_control = "yes";
};
stdout =
{
    output_rate = 44100;
    output_format = "S16_LE";
    output_channels = 2;
};
EOF
    cat >"$run/avahi-$name.conf" <<EOF
[server]
host-name=several-check-$name
use-ipv6=no
allow-interfaces=eth0
EOF
    mkfifo "$fifos/$name.pcm"
done

"$examples/rx_onset" --seconds $((seconds + 60)) --reads "$run" \
    --pcm "$fifos/a.pcm" --pcm "$fifos/b.pcm" --pcm "$fifos/c.pcm" >"$run/onsets.txt" 2>"$run/onsets.log" &
reader=$!

# The receiver advertises itself, so it needs an avahi-daemon of its own on
# the namespace's interface; the host's would publish the host's address.
for name in "${names[@]}"; do
    # The inner shell expands its own arguments.
    # shellcheck disable=SC2016
    ip netns exec "ap2s-$name" unshare -m --propagation private bash -c '
        mount -t tmpfs tmpfs /run/dbus
        mount -t tmpfs tmpfs /run/avahi-daemon
        mount -t tmpfs tmpfs /dev/shm
        dbus-daemon --system --fork
        avahi-daemon -D --no-chroot -f "$1"
        nqptp >"$2" 2>&1 &
        exec "$3" -c "$4" -o stdout -vvv --statistics >"$5" 2>"$6"
    ' _ "$run/avahi-$name.conf" "$run/nqptp-$name.log" "$receiver" "$run/$name.conf" \
        "$fifos/$name.pcm" "$run/$name.log" &
done
sleep 3

played=0
ip netns exec ap2s-tx "$examples/rx_pair" --seconds "$seconds" --join "$join" --leave "$leave" --click --verbose \
    --rx 192.0.2.11:7000 --rx 192.0.2.12:7000 --rx 192.0.2.13:7000 \
    >"$run/stated.txt" 2>"$run/sender.log" || played=$?
sleep 5

# Ending the receivers closes the FIFOs, which ends the reader.
for name in "${names[@]}"; do
    pkill -f -- "-c $run/$name.conf" || true
done
wait "$reader" || true
reader=

fail=0
verdict() {
    if [ "$2" -eq 0 ]; then
        echo "PASS $1: $3"
    else
        echo "FAIL $1: $3"
        fail=1
    fi
}

grep '^rx ' "$run/stated.txt" || true
# Exit 3 is rx_pair's SENDER check, which reads AirPlay 1 timing traces only.
ok=$(grep -c '^rx .* OK ' "$run/stated.txt" || true)
verdict played $((played != 0 && played != 3 || ok != 3)) "$ok of 3 receivers played (rx_pair exited $played)"

# One line per SETPEERS the receiver logged: its answer code, then the
# addresses its body named, each followed by a space.
setpeers() {
    awk '/SETPEERS request/ { body = 1; list = ""; next }
        body && /<string>/ { gsub(/.*<string>|<\/string>.*/, ""); list = list " " $0; next }
        body && /<\/plist>/ { body = 0; answer = 1; next }
        answer && /Response Code:/ { code = $NF; sub(/\.$/, "", code); print code list " "; answer = 0 }' "$1"
}
# named <lines> <address>...: one of the lines names every address.
named() {
    local lines=$1 address
    shift
    for address in "$@"; do
        lines=$(grep -F " $address " <<<"$lines" || true)
    done
    [ -n "$lines" ]
}
summary=
peers_fail=0
for name in "${names[@]}"; do
    setpeers "$run/$name.log" >"$run/setpeers-$name.txt"
    lists=$(cat "$run/setpeers-$name.txt")
    answered=$(grep -c '^2' <<<"$lists" || true)
    count=$(grep -c . <<<"$lists" || true)
    summary="$summary $name=$answered/$count"
    [ "$count" -gt 0 ] && [ "$answered" -eq "$count" ] || peers_fail=1
done
# The update after the third's RECORD names it, the one after its leave
# names the other of the two without it.
for pair in a:192.0.2.12 b:192.0.2.11; do
    lists=$(cat "$run/setpeers-${pair%%:*}.txt")
    after=$(awk '/ 192\.0\.2\.13( |$)/ { found = 1; next } found' <<<"$lists")
    named "$lists" 192.0.2.13 || peers_fail=1
    grep -v ' 192\.0\.2\.13\( \|$\)' <<<"$after" | grep -Eq " ${pair#*:}( |$)" || peers_fail=1
done
named "$(cat "$run/setpeers-c.txt")" 192.0.2.11 192.0.2.12 || peers_fail=1
warnings=$(grep -c 'warning' "$run/sender.log" || true)
verdict setpeers $((peers_fail || warnings)) "SETPEERS answered 2xx per receiver:$summary; the first two named the third, then dropped it; sender warnings $warnings"

outranked=$(grep -c 'GrandmasterOutranked' "$run/sender.log" || true)
verdict p206 "$outranked" "$outranked GrandmasterOutranked lines"

gaps=
gap_fail=0
for index in 1 2; do
    # reads-N.txt: frames read so far and arrival minus those frames at
    # 44.1 kHz. The last 3 s are the sender stopping, not part of the check.
    read -r worst at < <(awk 'NR == FNR { end = $2 + $1 / 44100; next }
        { t = $2 + $1 / 44100; if (t > end - 3) exit; if (FNR > 1 && t - last > worst) { worst = t - last; at = t }; last = t }
        END { printf "%.0f %.1f\n", worst * 1000, at }' "$run/reads-$index.txt" "$run/reads-$index.txt" 2>/dev/null || echo none)
    gaps="$gaps ${names[$((index - 1))]}=${worst}ms at ${at}s"
    [[ $worst =~ ^[0-9]+$ ]] && [ "$worst" -le "$gap_ms" ] || gap_fail=1
done
verdict gap "$gap_fail" "longest pause between reads:$gaps (limit $gap_ms ms)"

# rx_onset's SPREAD line times each click from the second before it on
# every stream; its pair and PIPE lines don't line a joiner's clicks up.
read -r worst_ms clicks three <<<"$(awk '$1 == "SPREAD" { print $2, $5, $7 }' "$run/onsets.txt")"
within=$(awk -v worst="${worst_ms:-none}" -v limit="$limit_ms" 'BEGIN { print (worst != "none" && worst + 0 <= limit) }')
verdict onsets $((within == 0 || ${three:-0} == 0)) "largest spread ${worst_ms:-none} ms over ${clicks:-0} clicks, ${three:-0} heard on all three (limit $limit_ms ms)"

settling=$(awk '$1 == "SETTLING" { print $2 }' "$run/onsets.txt")
within=$(awk -v worst="${settling:-none}" -v limit="$settle_ms" 'BEGIN { print (worst != "none" && worst + 0 <= limit) }')
verdict settling $((within == 0)) "largest spread ${settling:-none} ms over every click, a receiver's first 20 s included (limit $settle_ms ms)"

exit "$fail"
