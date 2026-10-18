#!/bin/bash
# Benchmark baseline of the engine, the Swift one or the Rust core (--engine),
# completely silent and unattended. Runs the soak harness in two configurations,
# one after the other:
#   two_local        two local outputs at gain 0, plus the delay-accuracy test
#                    (150/500/2000/3000 ms on each output)
#   local_plus_fifo  one local output at gain 0 plus the FIFO path to a local
#                    reader standing in for the AirPlay engine
# Real AirPlay is recorded as "not available" (it needs a speaker on the network
# and makes sound). While each configuration runs, `ps` samples the harness's
# CPU % and RSS every second. Writes Tests/baseline/<engine>.json (machine,
# macOS, commit and every number), which Tests/soak.sh judges against, and the
# results table in docs/BENCHMARKS.md (both engines' columns, from their
# baselines). Keeps the Mac awake while it runs.
#
# Usage: Tests/bench.sh [--engine swift|rust] [--minutes N] [--out DIR] [--baseline-out FILE] [--no-doc] [--doc-only]
#   --engine NAME        swift (default) or rust: which implementation plays
#   --minutes N          minutes per configuration (default 60, so two hours in all)
#   --out DIR            working folder for logs (default: new audiosync-bench-* under $TMPDIR)
#   --baseline-out FILE  where the baseline goes (default Tests/baseline/<engine>.json)
#   --no-doc             leave docs/BENCHMARKS.md alone
#   --doc-only           only rewrite the table from the baselines in Tests/baseline/
set -euo pipefail
export LC_ALL=C
cd "$(dirname "$0")/.."

engine=swift
minutes=60
out=""
baseline_out=""
doc=docs/BENCHMARKS.md
doc_only=false
while [ $# -gt 0 ]; do
  case $1 in
    --engine)       engine=${2:?--engine needs a value}; shift 2 ;;
    --minutes)      minutes=${2:?--minutes needs a value}; shift 2 ;;
    --out)          out=${2:?--out needs a value}; shift 2 ;;
    --baseline-out) baseline_out=${2:?--baseline-out needs a value}; shift 2 ;;
    --no-doc)       doc=""; shift ;;
    --doc-only)     doc_only=true; shift ;;
    -h|--help)      sed -n '2,22p' "$0"; exit 0 ;;
    *) echo "unknown option: $1" >&2; exit 2 ;;
  esac
done
case $engine in swift|rust) ;; *) echo "--engine must be swift or rust" >&2; exit 2 ;; esac
case $minutes in ''|*[!0-9]*|0) echo "--minutes must be a positive whole number" >&2; exit 2 ;; esac
if [ -z "$baseline_out" ]; then baseline_out=Tests/baseline/$engine.json; fi
if ! command -v python3 >/dev/null 2>&1; then
  echo "python3 is needed to check and format the baseline (it comes with the Command Line Tools)" >&2
  exit 2
fi

# write_doc SWIFT_BASELINE RUST_BASELINE: the results table between the markers
# in the doc, one column per engine whose baseline file exists.
write_doc() {
  python3 -I - "$doc" "$1" "$2" <<'PY'
import json, os, sys
doc_path, paths = sys.argv[1], {"swift": sys.argv[2], "rust": sys.argv[3]}
names = {"swift": "Swift", "rust": "Rust"}
baselines = {e: json.load(open(p, encoding="utf-8")) for e, p in paths.items() if os.path.exists(p)}
if not baselines:
    sys.exit("no baseline to write the table from")
def g(x):
    return "%g" % x
def describe(e, b):
    a, f = b["configs"]["two_local"], b["configs"]["local_plus_fifo"]
    m = b["machine"]
    names_out = ", ".join(o["name"] for o in a["harness"]["config"]["outputs"])
    where = ("the working tree of %s (with uncommitted changes)" % b["commit"]) if b["uncommitted_changes"] else b["commit"]
    mins = b["minutes_per_config"]
    lines = [
        "%s: %d min per configuration, %s, on %s." % (names[e], mins, b["generated"][:10], where),
        "Machine: %s (%s), %s, %d GB, macOS %s (%s). Outputs: %s; local + FIFO used %s."
        % (m["model_name"], m["model_identifier"], m["chip"], m["memory_gb"], m["macos"], m["macos_build"],
           names_out, f["harness"]["config"]["outputs"][0]["name"]),
    ]
    if mins < 60:
        lines.append("This is a short smoke run, not the release baseline; record that with a plain "
                     "`Tests/bench.sh --engine %s`." % e)
    return lines
def column(b):
    a, f = b["configs"]["two_local"], b["configs"]["local_plus_fifo"]
    am, fm = a["harness"]["metrics"], f["harness"]["metrics"]
    acc = a["harness"]["delay_accuracy"]
    def errs(t):
        return ", ".join("%+.1f" % r["error_ms"] for r in acc if r["target_ms"] == t)
    return [
        " / ".join("(%s)" % errs(t) for t in (150, 500, 2000, 3000)),
        g(am["local_dropouts_per_h"]),
        g(am["local_starved_renders_per_h"]),
        g(am["local_drift_corrections_per_h"]),
        g(am["local_lag_err_ms_max_abs"]),
        "%s / %s" % (g(am["inter_output_drift_ms_mean"]), g(am["inter_output_drift_ms_max_abs"])),
        g(am["inter_output_drift_ms_change"]),
        "%s / %s" % (g(am["cpu_pct_mean"]), g(a["process"]["cpu_pct_mean"])),
        "%s / %s" % (g(am["footprint_mb_max"]), g(a["process"]["rss_mb_max"])),
        g(fm["fifo_underruns_per_h"]),
        g(fm["fifo_gaps_per_h"]),
        "%s / %s" % (g(fm["fifo_latency_ms_mean"]), g(fm["fifo_latency_ms_max"])),
        "%d, %d, %d" % (fm["fifo_pipe_full_waits"], fm["fifo_short_writes"], fm["fifo_backlog_skips"]),
        g(fm["local_dropouts_per_h"]),
        "%s / %s" % (g(fm["cpu_pct_mean"]), g(f["process"]["cpu_pct_mean"])),
        "not available",
    ]
labels = [
    "Delay error at 150 / 500 / 2000 / 3000 ms (ms, per output)",
    "Two local: dropouts per hour (worst output)",
    "Two local: starved renders per hour (worst output)",
    "Two local: drift corrections per hour (worst output)",
    "Two local: worst lag error, 10 s mean (ms)",
    "Two local: inter-output drift, mean / worst (ms)",
    "Two local: inter-output drift change, first to last window (ms)",
    "Two local: CPU % mean (harness / ps)",
    "Two local: memory footprint max / RSS max (MB)",
    "Local + FIFO: FIFO underruns per hour",
    "Local + FIFO: FIFO gaps per hour",
    "Local + FIFO: FIFO latency mean / max (ms)",
    "Local + FIFO: pipe-full waits, short writes, backlog skips",
    "Local + FIFO: local dropouts per hour",
    "Local + FIFO: CPU % mean (harness / ps)",
    "Real AirPlay",
]
cols = {e: column(b) if b else None for e, b in baselines.items()}
blank = [""] * len(labels)
rows = zip(labels, cols.get("swift") or blank, cols.get("rust") or blank)
def row(cells):
    return "|" + "|".join(" %s " % c if c else " " for c in cells) + "|"
table = ["| Metric | Swift | Rust |", "|---|---|---|"] + [row(r) for r in rows]
text = []
for e in ("swift", "rust"):
    if e in baselines:
        text += describe(e, baselines[e])
block = "\n\n".join(text) + "\n\n" + "\n".join(table) + "\n"
begin, end = "<!-- bench:results:begin -->", "<!-- bench:results:end -->"
doc = open(doc_path, encoding="utf-8").read()
if begin not in doc or end not in doc:
    sys.exit("%s has no %s ... %s markers" % (doc_path, begin, end))
head, rest = doc.split(begin, 1)
_, tail = rest.split(end, 1)
open(doc_path, "w", encoding="utf-8").write(head + begin + "\n" + block + end + tail)
print("▶ results table written to " + doc_path)
PY
}

# The other engine's column comes from its baseline in Tests/baseline/.
other_baseline() {
  if [ "$engine" = swift ]; then echo Tests/baseline/rust.json; else echo Tests/baseline/swift.json; fi
}

if [ "$doc_only" = true ]; then
  [ -n "$doc" ] || { echo "--doc-only and --no-doc together leave nothing to do" >&2; exit 2; }
  write_doc Tests/baseline/swift.json Tests/baseline/rust.json
  exit 0
fi

if [ -z "$out" ]; then out=$(mktemp -d "${TMPDIR:-/tmp}/audiosync-bench-XXXXXX"); fi
mkdir -p "$out"
bin="$out/soaktest"
echo "▶ building the Rust core and the harness"
if ! scripts/build_core.sh > "$out/core-build.log" 2>&1; then
  tail -n 20 "$out/core-build.log" >&2
  echo "the Rust core did not build" >&2
  exit 2
fi
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
  echo "▶ $name, $engine engine, $minutes min ($dir)"
  "$bin" --label "$name" --engine "$engine" --seconds "$per_config" --interval 10 --out "$dir" "$@" \
    > "$dir/harness.log" 2>&1 &
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
  printf '{\n"engine": %s,\n' "$(json_str "$engine")"
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
echo "▶ baseline written to $baseline_out ($engine engine, $minutes min per configuration, commit $commit)"

[ -n "$doc" ] || exit 0
if [ "$engine" = swift ]; then
  write_doc "$baseline_out" "$(other_baseline)"
else
  write_doc "$(other_baseline)" "$baseline_out"
fi
