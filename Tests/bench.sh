#!/bin/bash
# Benchmark baseline of the Swift engine, completely silent and unattended.
# Runs the soak harness in two configurations, one after the other:
#   two_local        two local outputs at gain 0, plus the delay-accuracy test
#                    (150/500/2000/3000 ms on each output)
#   local_plus_fifo  one local output at gain 0 plus the FIFO path to a local
#                    reader standing in for the AirPlay engine
# Real AirPlay is recorded as "not available" (it needs a speaker on the network
# and makes sound). While each configuration runs, `ps` samples the harness's
# CPU % and RSS every second. Writes Tests/baseline/swift.json (machine, macOS,
# commit and every number), which Tests/soak.sh judges against, and the Swift
# results table in docs/BENCHMARKS.md. Keeps the Mac awake while it runs.
#
# Usage: Tests/bench.sh [--minutes N] [--out DIR] [--baseline-out FILE] [--no-doc]
#   --minutes N          minutes per configuration (default 60, so two hours in all)
#   --out DIR            working folder for logs (default: new audiosync-bench-* under $TMPDIR)
#   --baseline-out FILE  where the baseline goes (default Tests/baseline/swift.json)
#   --no-doc             leave docs/BENCHMARKS.md alone
set -euo pipefail
export LC_ALL=C
cd "$(dirname "$0")/.."

minutes=60
out=""
baseline_out=Tests/baseline/swift.json
doc=docs/BENCHMARKS.md
while [ $# -gt 0 ]; do
  case $1 in
    --minutes)      minutes=${2:?--minutes needs a value}; shift 2 ;;
    --out)          out=${2:?--out needs a value}; shift 2 ;;
    --baseline-out) baseline_out=${2:?--baseline-out needs a value}; shift 2 ;;
    --no-doc)       doc=""; shift ;;
    -h|--help)      sed -n '2,19p' "$0"; exit 0 ;;
    *) echo "unknown option: $1" >&2; exit 2 ;;
  esac
done
case $minutes in ''|*[!0-9]*|0) echo "--minutes must be a positive whole number" >&2; exit 2 ;; esac
if ! command -v python3 >/dev/null 2>&1; then
  echo "python3 is needed to check and format the baseline (it comes with the Command Line Tools)" >&2
  exit 2
fi

if [ -z "$out" ]; then out=$(mktemp -d "${TMPDIR:-/tmp}/audiosync-bench-XXXXXX"); fi
mkdir -p "$out"
bin="$out/soaktest"
echo "▶ building the harness"
if ! Tests/build-harness.sh "$bin"; then echo "the harness did not compile" >&2; exit 2; fi
caffeinate -i -w $$ &   # no idle sleep while this script runs
per_config=$((minutes * 60))   # seconds per configuration

harness_pid=""
stop_harness() { if [ -n "$harness_pid" ]; then kill "$harness_pid" 2>/dev/null || true; fi; }
trap stop_harness EXIT
trap 'stop_harness; exit 130' INT TERM

# run_config NAME HARNESS_ARGS…: runs the harness, sampling it with ps every second.
run_config() {
  local name=$1; shift
  local dir="$out/$name"
  mkdir -p "$dir"
  echo "▶ $name, $minutes min ($dir)"
  "$bin" --label "$name" --seconds "$per_config" --interval 10 --out "$dir" "$@" > "$dir/harness.log" 2>&1 &
  harness_pid=$!
  local t=0 sample
  echo "t_s,cpu_pct,rss_kb" > "$dir/ps.csv"
  while kill -0 "$harness_pid" 2>/dev/null; do
    sample=$(ps -o %cpu=,rss= -p "$harness_pid" 2>/dev/null) || break
    # shellcheck disable=SC2086 # split "cpu rss" into two fields
    set -- $sample
    [ $# -eq 2 ] && echo "$t,$1,$2" >> "$dir/ps.csv"
    sleep 1
    t=$((t + 1))
  done
  local status=0
  wait "$harness_pid" || status=$?
  harness_pid=""
  # 3 = finished without a baseline to judge against, which is what bench wants.
  if { [ "$status" -ne 0 ] && [ "$status" -ne 3 ]; } || [ ! -f "$dir/result.json" ]; then
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
dirty=false
if [ -n "$(git status --porcelain --untracked-files=no)" ]; then dirty=true; fi

mkdir -p "$(dirname "$baseline_out")"
tmp="$out/baseline.json"
{
  printf '{\n"engine": "swift",\n'
  printf '"generated": %s,\n' "$(json_str "$(date -u +%Y-%m-%dT%H:%M:%SZ)")"
  printf '"minutes_per_config": %d,\n' "$minutes"
  printf '"commit": %s,\n"uncommitted_changes": %s,\n' "$(json_str "$commit")" "$dirty"
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
if ! python3 -I -m json.tool --indent 1 "$tmp" > "$tmp.pretty"; then
  echo "the baseline is not valid JSON; kept at $tmp, nothing written" >&2
  exit 1
fi
mv "$tmp.pretty" "$baseline_out"
echo "▶ baseline written to $baseline_out ($minutes min per configuration, commit $commit)"

[ -n "$doc" ] || exit 0
# Replace the Swift results between the markers in the doc with this run's numbers.
python3 -I - "$baseline_out" "$doc" <<'PY'
import json, sys
base_path, doc_path = sys.argv[1], sys.argv[2]
b = json.load(open(base_path, encoding="utf-8"))
a, f = b["configs"]["two_local"], b["configs"]["local_plus_fifo"]
am, fm = a["harness"]["metrics"], f["harness"]["metrics"]
acc = a["harness"]["delay_accuracy"]
m = b["machine"]
def errs(t):
    return ", ".join("%+.1f" % r["error_ms"] for r in acc if r["target_ms"] == t)
def g(x):
    return "%g" % x
names = ", ".join(o["name"] for o in a["harness"]["config"]["outputs"])
commit = b["commit"]
where = ("the working tree of %s (with uncommitted changes)" % commit) if b["uncommitted_changes"] else commit
mins = b["minutes_per_config"]
lines = [
    "Run: %d min per configuration, %s, on %s." % (mins, b["generated"][:10], where),
    "Machine: %s (%s), %s, %d GB, macOS %s (%s). Outputs: %s; local + FIFO used %s."
    % (m["model_name"], m["model_identifier"], m["chip"], m["memory_gb"], m["macos"], m["macos_build"],
       names, f["harness"]["config"]["outputs"][0]["name"]),
]
if mins < 60:
    lines.append("This is a short smoke run, not the release baseline; record that with a plain `Tests/bench.sh`.")
rows = [
    ("Delay error at 150 / 500 / 2000 / 3000 ms (ms, per output)",
     " / ".join("(%s)" % errs(t) for t in (150, 500, 2000, 3000))),
    ("Two local: dropouts per hour (worst output)", g(am["local_dropouts_per_h"])),
    ("Two local: starved renders per hour (worst output)", g(am["local_starved_renders_per_h"])),
    ("Two local: drift corrections per hour (worst output)", g(am["local_drift_corrections_per_h"])),
    ("Two local: worst lag error, 10 s mean (ms)", g(am["local_lag_err_ms_max_abs"])),
    ("Two local: inter-output drift, mean / worst (ms)",
     "%s / %s" % (g(am["inter_output_drift_ms_mean"]), g(am["inter_output_drift_ms_max_abs"]))),
    ("Two local: inter-output drift change, first to last window (ms)", g(am["inter_output_drift_ms_change"])),
    ("Two local: CPU % mean (harness / ps)", "%s / %s" % (g(am["cpu_pct_mean"]), g(a["process"]["cpu_pct_mean"]))),
    ("Two local: memory footprint max / RSS max (MB)",
     "%s / %s" % (g(am["footprint_mb_max"]), g(a["process"]["rss_mb_max"]))),
    ("Local + FIFO: FIFO underruns per hour", g(fm["fifo_underruns_per_h"])),
    ("Local + FIFO: FIFO gaps per hour", g(fm["fifo_gaps_per_h"])),
    ("Local + FIFO: FIFO latency mean / max (ms)",
     "%s / %s" % (g(fm["fifo_latency_ms_mean"]), g(fm["fifo_latency_ms_max"]))),
    ("Local + FIFO: pipe-full waits, short writes, backlog skips",
     "%d, %d, %d" % (fm["fifo_pipe_full_waits"], fm["fifo_short_writes"], fm["fifo_backlog_skips"])),
    ("Local + FIFO: local dropouts per hour", g(fm["local_dropouts_per_h"])),
    ("Local + FIFO: CPU % mean (harness / ps)", "%s / %s" % (g(fm["cpu_pct_mean"]), g(f["process"]["cpu_pct_mean"]))),
    ("Real AirPlay", "not available"),
]
table = ["| Metric | Swift | Rust |", "|---|---|---|"] + ["| %s | %s | |" % r for r in rows]
block = "\n\n".join(lines) + "\n\n" + "\n".join(table) + "\n"
begin, end = "<!-- bench:swift:begin -->", "<!-- bench:swift:end -->"
doc = open(doc_path, encoding="utf-8").read()
if begin not in doc or end not in doc:
    sys.exit("%s has no %s ... %s markers" % (doc_path, begin, end))
head, rest = doc.split(begin, 1)
_, tail = rest.split(end, 1)
open(doc_path, "w", encoding="utf-8").write(head + begin + "\n" + block + end + tail)
print("▶ results table written to " + doc_path)
PY
