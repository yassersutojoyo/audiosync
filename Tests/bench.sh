#!/bin/bash
# Benchmark baseline of the Swift engine, completely silent and unattended.
# Runs the soak harness in two configurations, half the time each:
#   two_local        two local outputs at gain 0, plus the delay-accuracy test
#                    (150/500/2000/3000 ms on each output)
#   local_plus_fifo  one local output at gain 0 plus the FIFO path to a local
#                    reader standing in for the AirPlay engine
# Real AirPlay is recorded as "not available" (it needs a speaker on the network
# and makes sound). While each configuration runs, `ps` samples the harness's
# CPU % and RSS every second. Writes Tests/baseline/swift.json (machine, macOS,
# commit and every number), which Tests/soak.sh judges against. See
# docs/BENCHMARKS.md. Keeps the Mac awake while it runs.
#
# Usage: Tests/bench.sh [--minutes N] [--out DIR] [--baseline-out FILE]
#   --minutes N          total duration, split over the two configurations (default 60)
#   --out DIR            working folder for logs (default: new audiosync-bench-* under $TMPDIR)
#   --baseline-out FILE  where the baseline goes (default Tests/baseline/swift.json)
set -euo pipefail
cd "$(dirname "$0")/.."

minutes=60
out=""
baseline_out=Tests/baseline/swift.json
while [ $# -gt 0 ]; do
  case $1 in
    --minutes)      minutes=${2:?--minutes needs a value}; shift 2 ;;
    --out)          out=${2:?--out needs a value}; shift 2 ;;
    --baseline-out) baseline_out=${2:?--baseline-out needs a value}; shift 2 ;;
    -h|--help)      sed -n '2,19p' "$0"; exit 0 ;;
    *) echo "unknown option: $1" >&2; exit 2 ;;
  esac
done
case $minutes in ''|*[!0-9]*|0|1) echo "--minutes must be a whole number of at least 2" >&2; exit 2 ;; esac

if [ -z "$out" ]; then out=$(mktemp -d "${TMPDIR:-/tmp}/audiosync-bench-XXXXXX"); fi
mkdir -p "$out"
bin="$out/soaktest"
echo "▶ building the harness"
Tests/build-harness.sh "$bin"
caffeinate -i -w $$ &   # no idle sleep while this script runs
per_config=$((minutes * 30))   # seconds per configuration

# run_config NAME HARNESS_ARGS…: runs the harness, sampling it with ps every second.
run_config() {
  local name=$1; shift
  local dir="$out/$name"
  mkdir -p "$dir"
  echo "▶ $name, $((per_config / 60)) min ($dir)"
  "$bin" --label "$name" --seconds "$per_config" --interval 10 --out "$dir" "$@" > "$dir/harness.log" 2>&1 &
  local pid=$! t=0 sample
  echo "t_s,cpu_pct,rss_kb" > "$dir/ps.csv"
  while kill -0 "$pid" 2>/dev/null; do
    sample=$(ps -o %cpu=,rss= -p "$pid" 2>/dev/null) || break
    # shellcheck disable=SC2086 # split "cpu rss" into two fields
    set -- $sample
    [ $# -eq 2 ] && echo "$t,$1,$2" >> "$dir/ps.csv"
    sleep 1
    t=$((t + 1))
  done
  local status=0
  wait "$pid" || status=$?
  if [ "$status" -ne 0 ] || [ ! -f "$dir/result.json" ]; then
    echo "$name failed (exit $status); see $dir/harness.log" >&2
    tail -n 20 "$dir/harness.log" >&2
    exit 1
  fi
  sed -n '/^AudioSync soak harness/,$p' "$dir/harness.log"
}

# ps_summary FILE: JSON object with the mean and max of the ps samples.
ps_summary() {
  awk -F, 'NR > 1 { n++; c += $2; r += $3; if ($2 > cm) cm = $2; if ($3 > rm) rm = $3 }
    END { if (n == 0) { print "{\"samples\": 0}"; exit }
          printf "{\"samples\": %d, \"cpu_pct_mean\": %.2f, \"cpu_pct_max\": %.1f, \"rss_mb_mean\": %.1f, \"rss_mb_max\": %.1f}",
                 n, c / n, cm, r / n / 1024, rm / 1024 }' "$1"
}

# json_str TEXT: TEXT as a JSON string (quotes, backslashes and control characters dropped).
json_str() { printf '"%s"' "$(printf '%s' "$1" | tr -d '"\134[:cntrl:]')"; }

run_config two_local --outputs 2 --no-fifo --delay-accuracy
run_config local_plus_fifo --outputs 1 --fifo

hw=$(system_profiler SPHardwareDataType 2>/dev/null || true)
model_name=$(printf '%s\n' "$hw" | awk -F': ' '/Model Name/ { print $2; exit }')
chip=$(printf '%s\n' "$hw" | awk -F': ' '/Chip/ { print $2; exit }')
[ -n "$chip" ] || chip=$(sysctl -n machdep.cpu.brand_string)
memory_gb=$(( $(sysctl -n hw.memsize) / 1073741824 ))
commit=$(git rev-parse --short HEAD)
if [ -n "$(git status --porcelain --untracked-files=no)" ]; then commit="$commit+dirty"; fi

mkdir -p "$(dirname "$baseline_out")"
tmp="$out/baseline.json"
{
  printf '{\n"engine": "swift",\n'
  printf '"generated": %s,\n' "$(json_str "$(date -u +%Y-%m-%dT%H:%M:%SZ)")"
  printf '"minutes": %d,\n' "$minutes"
  printf '"commit": %s,\n' "$(json_str "$commit")"
  printf '"machine": {"model_name": %s, "model_identifier": %s, "chip": %s, "memory_gb": %d, ' \
    "$(json_str "$model_name")" "$(json_str "$(sysctl -n hw.model)")" "$(json_str "$chip")" "$memory_gb"
  printf '"macos": %s, "macos_build": %s},\n' \
    "$(json_str "$(sw_vers -productVersion)")" "$(json_str "$(sw_vers -buildVersion)")"
  printf '"configs": {\n'
  printf '"two_local": {"harness": %s, "process": %s},\n' \
    "$(cat "$out/two_local/result.json")" "$(ps_summary "$out/two_local/ps.csv")"
  printf '"local_plus_fifo": {"harness": %s, "process": %s},\n' \
    "$(cat "$out/local_plus_fifo/result.json")" "$(ps_summary "$out/local_plus_fifo/ps.csv")"
  printf '"airplay": {"status": "not available", "reason": %s}\n' \
    "$(json_str "real AirPlay needs a speaker on the network and makes sound; the FIFO path to a local reader stands in for the engine")"
  printf '}\n}\n'
} > "$tmp"
if command -v python3 >/dev/null 2>&1 && python3 -I -m json.tool --indent 1 "$tmp" > "$tmp.pretty" 2>/dev/null; then
  mv "$tmp.pretty" "$baseline_out"
else
  cp "$tmp" "$baseline_out"
fi
echo "▶ baseline written to $baseline_out ($minutes min, commit $commit)"
