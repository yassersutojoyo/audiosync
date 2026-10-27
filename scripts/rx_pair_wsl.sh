#!/bin/bash
# Plays one stream for a while to test receivers that are already running,
# given as a host:port list, through the airplay example rx_pair, and prints
# its one summary line per receiver. Starts no receiver itself. A protocol
# check only: it never tells whether the receivers sound in step.
#
#   scripts/rx_pair_wsl.sh [--seconds <n>] [--tone] [--verbose] <host:port>[,ap1|ap2]...
#
# Meant for Linux (WSL included). An AirPlay 2 receiver needs an IP address
# of its own, and the sender binds the PTP ports 319 and 320, which takes
# root or a lowered net.ipv4.ip_unprivileged_port_start. Exits as rx_pair
# does: 1 when a receiver didn't play or reported an error.
set -euo pipefail

root=$(git rev-parse --show-toplevel)
cd "$root"

usage() {
    echo "usage: scripts/rx_pair_wsl.sh [--seconds <n>] [--tone] [--verbose] <host:port>[,ap1|ap2]..." >&2
    exit 2
}

args=()
receivers=0
while [ $# -gt 0 ]; do
    case "$1" in
        --seconds)
            [ $# -ge 2 ] || usage
            args+=(--seconds "$2")
            shift 2
            ;;
        --tone | --verbose)
            args+=("$1")
            shift
            ;;
        -*)
            usage
            ;;
        *)
            args+=(--rx "$1")
            receivers=$((receivers + 1))
            shift
            ;;
    esac
done
[ "$receivers" -gt 0 ] || usage

start=$(cat /proc/sys/net/ipv4/ip_unprivileged_port_start 2>/dev/null || echo 1024)
if [ "$(id -u)" -ne 0 ] && [ "$start" -gt 319 ]; then
    echo "note: not root and ports below $start are privileged; AirPlay 2 receivers may get no PTP" >&2
fi

# A release build binds every address; a debug build would keep to loopback.
exec cargo run --release --quiet --manifest-path airplay/Cargo.toml --example rx_pair -- "${args[@]}"
