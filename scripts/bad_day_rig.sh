#!/bin/bash
# Replays a bad Wi-Fi day against the AirPlay sender of one or more builds
# and prints what each build did. For every profile (jitter with 1% loss,
# jitter with 3% loss, jitter with burst loss) each build streams a tone
# through the example rx_pair to its own shairport-sync (AirPlay 1) side by
# side, through one tc netem queue on lo that carries only the UDP sent to
# the receivers' ports: clean, impaired, clean again. The receivers' resend
# requests go to the sender's ports and are never touched.
#
#   scripts/bad_day_rig.sh <label>=<git ref>...    build each ref, run, print the table
#   scripts/bad_day_rig.sh --table <label>...      print the table again; a label with no
#                                                  results shows as pending
#
# Linux only (WSL included); needs root for tc, so on the PC run it through
# "wsl -u root -e bash scripts/bad_day_rig.sh ...". Cargo runs as the owner
# of the checkout. Every build
# gets this checkout's rx_pair example (it needs --every and
# --latency-frames). Sources, the cargo target and results stay under
# airplay/target/scratch-badday. Phase lengths in seconds: BAD_DAY_CLEAN_S,
# BAD_DAY_BAD_S, BAD_DAY_AFTER_S (60, 240, 120); Lf: BAD_DAY_LF (15435
# frames, what the apps pick for shairport-sync); BAD_DAY_PROFILES picks
# profiles by name.
set -euo pipefail

root=$(cd "$(dirname "$0")/.." && pwd)
work=$root/airplay/target/scratch-badday
results=$work/results
clean_s=${BAD_DAY_CLEAN_S:-60}
bad_s=${BAD_DAY_BAD_S:-240}
after_s=${BAD_DAY_AFTER_S:-120}
lf=${BAD_DAY_LF:-15435}

base_delay="delay 85ms 65ms distribution pareto"
spike_delay="delay 300ms"
spike_every=20
read -r -a profiles <<<"${BAD_DAY_PROFILES:-jitter-loss1 jitter-loss3 jitter-burst}"
declare -A profile_loss=(
    [jitter-loss1]="loss random 1%"
    [jitter-loss3]="loss random 3%"
    [jitter-burst]="loss gemodel 1% 30%"
)

usage() {
    echo "usage: scripts/bad_day_rig.sh <label>=<git ref>... | --table <label>..." >&2
    exit 2
}

table() {
    python3 - "$results" "$clean_s" "$bad_s" "${profiles[@]}" -- "$@" <<'EOF'
import os, re, sys
args = sys.argv[1:]
results, clean_s, bad_s = args[0], float(args[1]), float(args[2])
split = args.index("--")
profiles, labels = args[3:split], args[split + 1:]
packet_s = 352 / 44100

def stats(path):
    rows = []
    for line in open(path, errors="replace"):
        v = [x.strip() for x in line.split(",")]
        if len(v) == 11 and v[0].isdigit():
            rows.append(dict(t=int(v[0]) * packet_s, missing=int(v[1]), late=int(v[2]),
                             too_late=int(v[3]), requests=int(v[4])))
    return rows

def at(rows, t, key):
    before = [r for r in rows if r["t"] <= t]
    return before[-1][key] if before else 0

def cell(profile, label):
    rx = os.path.join(results, f"{profile}.{label}.rx.log")
    tx = os.path.join(results, f"{profile}.{label}.tx.out")
    if not (os.path.exists(rx) and os.path.exists(tx)):
        return f"pending ({label} not run)"
    rows = stats(rx)
    if not rows:
        return "no receiver statistics"
    on, off = clean_s, clean_s + bad_s
    minutes = bad_s / 60
    missing = at(rows, off, "missing") - at(rows, on, "missing")
    too_late = at(rows, off, "too_late") - at(rows, on, "too_late")
    requests = at(rows, off, "requests") - at(rows, on, "requests")
    steps, last = [], None
    for line in open(tx, errors="replace"):
        m = re.match(r"every t_s=([\d.]+) .* latency_ms=([\d.]+)", line)
        if m:
            t, ms = float(m.group(1)), float(m.group(2))
            if ms != last:
                steps.append(f"{ms:.0f}" if last is None else f"{ms:.0f} at {t - on:+.0f} s")
                last = ms
    lf = " to ".join(steps) if steps else "no reading"
    bad_after = [r for r in rows if r["t"] > off + 8]
    dirty, prev = [], at(rows, off + 8, "missing") + at(rows, off + 8, "too_late")
    for r in bad_after:
        now = r["missing"] + r["too_late"]
        if now != prev:
            dirty.append(r["t"])
        prev = now
    clean = "at once" if not dirty else f"after {max(dirty) - off:.0f} s"
    return (f"played without {missing} ({missing / minutes:.1f}/min), too late {too_late}; "
            f"resend requests {requests / minutes:.0f}/min; Lf ms {lf}; clean {clean}")

print("| Profile | " + " | ".join(labels) + " |")
print("|---|" + "---|" * len(labels))
for p in profiles:
    print(f"| {p} | " + " | ".join(cell(p, l) for l in labels) + " |")
EOF
}

if [ "${1:-}" = "--table" ]; then
    shift
    [ $# -gt 0 ] || usage
    table "$@"
    exit 0
fi
[ $# -gt 0 ] || usage
[ "$(id -u)" -eq 0 ] || {
    echo "needs root for tc" >&2
    exit 2
}
if tc qdisc show dev lo | grep -qv noqueue; then
    echo "lo already has a qdisc; another rig may be running" >&2
    exit 1
fi

owner=$(stat -c %U "$root")
git_dir=$root/.git
if [ -f "$git_dir" ]; then
    # A worktree made on Windows names its git dir as C:/..., which WSL reads as /mnt/c/...
    git_dir=$(sed -E 's|^gitdir: ||; s|^([A-Za-z]):|/mnt/\L\1|' "$git_dir")
fi
mkdir -p "$work/bin" "$results"
labels=()
for pair in "$@"; do
    label=${pair%%=*}
    ref=${pair#*=}
    [ -n "$label" ] && [ "$label" != "$pair" ] || usage
    labels+=("$label")
    src=$work/src-$label
    rm -rf "$src"
    mkdir -p "$src"
    git -c safe.directory='*' --git-dir="$git_dir" archive "$ref" airplay | tar -x -C "$src" --strip-components=1
    rm -rf "$src/examples/rx_pair" "$src/examples/rx_pair.rs"
    cp -r "$root/airplay/examples/rx_pair" "$root/airplay/examples/rx_pair.rs" "$src/examples/"
    # git archive stamps files with the commit time, and every build shares one target, so
    # cargo took an older ref as fresh and the copy below was the previous build's binary.
    find "$src" -exec touch {} +
    chown -R "$owner" "$src"
    runuser -l "$owner" -c "cd '$src' && CARGO_TARGET_DIR='$work/target' cargo build --release --locked --quiet --example rx_pair"
    cp "$work/target/release/examples/rx_pair" "$work/bin/rx_pair-$label"
    echo "built $label from $(git -c safe.directory='*' --git-dir="$git_dir" rev-parse --short "$ref^{commit}"), md5 $(md5sum <"$work/bin/rx_pair-$label" | cut -c1-12)"
done

pids=()
cleanup() {
    for pid in "${pids[@]}"; do
        kill "$pid" 2>/dev/null || true
    done
    tc qdisc del dev lo root 2>/dev/null || true
}
trap cleanup EXIT

netem() {
    tc qdisc change dev lo parent 1:4 handle 40: netem limit 10000 "$@"
}

tc qdisc add dev lo root handle 1: prio bands 4 priomap 1 2 2 2 1 2 0 0 1 1 1 1 1 1 1 1
tc qdisc add dev lo parent 1:4 handle 40: netem limit 10000
for i in "${!labels[@]}"; do
    # Mask 0xfff0 takes the 16 ports from the receiver's UDP base.
    tc filter add dev lo parent 1: protocol ip prio 1 u32 \
        match ip protocol 17 0xff match ip dport $((6800 + 16 * i)) 0xfff0 flowid 1:4
done

for profile in "${profiles[@]}"; do
    read -r -a loss <<<"${profile_loss[$profile]}"
    read -r -a base <<<"$base_delay"
    read -r -a spike <<<"$spike_delay"
    pids=()
    for i in "${!labels[@]}"; do
        label=${labels[$i]}
        conf=$results/$profile.$label.conf
        cat >"$conf" <<EOF
general = { name = "Bad day $i"; port = $((5800 + i)); udp_port_base = $((6800 + 16 * i)); udp_port_range = 10;
    ignore_volume_control = "yes"; audio_backend_buffer_desired_length_in_seconds = 0.05; };
stdout = { audio_backend_buffer_desired_length_in_seconds = 0.05; };
diagnostics = { statistics = "yes"; };
EOF
        shairport-sync -c "$conf" -u -o stdout >/dev/null 2>"$results/$profile.$label.rx.log" &
        pids+=($!)
    done
    sleep 2
    senders=()
    for i in "${!labels[@]}"; do
        label=${labels[$i]}
        "$work/bin/rx_pair-$label" --rx "127.0.0.1:$((5800 + i)),ap1" --tone --every 10 \
            --latency-frames "$lf" --seconds $((clean_s + bad_s + after_s)) \
            >"$results/$profile.$label.tx.out" 2>"$results/$profile.$label.tx.err" &
        senders+=($!)
    done
    pids+=("${senders[@]}")
    sleep "$clean_s"
    netem "${base[@]}" "${loss[@]}"
    (
        while :; do
            sleep $((spike_every - 1)).7
            netem "${spike[@]}" "${loss[@]}"
            sleep 0.3
            netem "${base[@]}" "${loss[@]}"
        done
    ) &
    spikes=$!
    sleep "$bad_s"
    kill "$spikes"
    wait "$spikes" 2>/dev/null || true
    netem
    for pid in "${senders[@]}"; do
        wait "$pid" || true
    done
    for pid in "${pids[@]}"; do
        kill "$pid" 2>/dev/null || true
    done
    sleep 2
    echo "ran $profile"
done

table "${labels[@]}"
