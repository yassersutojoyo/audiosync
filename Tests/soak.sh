#!/bin/bash
# Soak test of the engine, completely silent and unattended: plays a known
# signal through two local outputs at gain 0 (a virtual output first, then the
# built-in one; never AirPlay, never an input device) and through the FIFO path
# to a local reader standing in for the AirPlay engine. Logs a record every 10 s
# and ends with a summary judged against the engine's baseline,
# Tests/baseline/<engine>.json (see docs/BENCHMARKS.md). Keeps the Mac awake
# while it runs.
#
# Usage: Tests/soak.sh [--engine swift|rust] [--minutes N] [--out DIR] [--baseline FILE] [--device NAME]...
#   --engine NAME    swift (default) or rust: which implementation plays
#   --minutes N      measured duration (default 60)
#   --out DIR        where windows.jsonl, windows.csv, result.json and summary.txt
#                    go (default: a new audiosync-soak-* folder under $TMPDIR)
#   --baseline FILE  baseline to judge against (default Tests/baseline/<engine>.json)
#   --device NAME    use this output instead of the default pick (repeat for two)
# Exit status: 0 PASS, 1 FAIL, 2 setup error (the core or the harness doesn't
# build, too few usable outputs, a baseline without a needed metric), 3 no
# baseline file (numbers only; run Tests/bench.sh first).
set -euo pipefail
export LC_ALL=C
cd "$(dirname "$0")/.."

engine=swift
minutes=60
out=""
baseline=""
devices=()
while [ $# -gt 0 ]; do
  case $1 in
    --engine)   engine=${2:?--engine needs a value}; shift 2 ;;
    --minutes)  minutes=${2:?--minutes needs a value}; shift 2 ;;
    --out)      out=${2:?--out needs a value}; shift 2 ;;
    --baseline) baseline=${2:?--baseline needs a value}; shift 2 ;;
    --device)   devices+=(--device "${2:?--device needs a value}"); shift 2 ;;
    -h|--help)  sed -n '2,19p' "$0"; exit 0 ;;
    *) echo "unknown option: $1" >&2; exit 2 ;;
  esac
done
case $engine in swift|rust) ;; *) echo "--engine must be swift or rust" >&2; exit 2 ;; esac
case $minutes in ''|*[!0-9]*|0) echo "--minutes must be a positive whole number" >&2; exit 2 ;; esac
if [ -z "$baseline" ]; then baseline=Tests/baseline/$engine.json; fi

if [ -z "$out" ]; then out=$(mktemp -d "${TMPDIR:-/tmp}/audiosync-soak-XXXXXX"); fi
mkdir -p "$out"
bin="$out/soaktest"
echo "▶ building the Rust core and the harness"
if ! scripts/build_core.sh > "$out/core-build.log" 2>&1; then
  tail -n 20 "$out/core-build.log" >&2
  echo "the Rust core did not build" >&2
  exit 2
fi
if ! Tests/build-harness.sh "$bin"; then echo "the harness did not compile" >&2; exit 2; fi

args=(--label soak --engine "$engine" --seconds $((minutes * 60)) --interval 10 --outputs 2 --fifo --out "$out")
if [ -f "$baseline" ]; then
  args+=(--baseline "$baseline")
else
  echo "no baseline at $baseline: numbers only, exit status 3 (run Tests/bench.sh --engine $engine first)" >&2
fi

caffeinate -i -w $$ &   # no idle sleep while this script runs
echo "▶ soak, $engine engine, $minutes min, output in $out"
"$bin" "${args[@]}" ${devices[@]+"${devices[@]}"} &
harness_pid=$!
trap 'kill "$harness_pid" 2>/dev/null || true' EXIT
trap 'kill "$harness_pid" 2>/dev/null || true; exit 130' INT TERM
status=0
wait "$harness_pid" || status=$?
echo "results: $out/summary.txt, $out/result.json, $out/windows.jsonl"
exit $status
