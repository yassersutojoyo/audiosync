#!/usr/bin/env bash
# Runs one test binary over and over in parallel processes, to catch a crash
# that shows up once in hundreds of runs. On Windows a crashed cast or desktop
# test binary leaves a minidump in <crate>/target/crash-dumps
# (cast/tests/common/crash_dump.rs).
#
# usage: scripts/test_loop.sh <crate dir> <runs per process> <processes> <cargo test target>
#   e.g. scripts/test_loop.sh cast 50 8 --test playback
set -u
[ $# -ge 4 ] || { sed -n '7,8p' "$0" >&2; exit 2; }
crate=$1 runs=$2 procs=$3
shift 3
root=$(cd "$(dirname "$0")/.." && pwd)
cd "$root/$crate" || exit 1
exe=$(cargo test "$@" --no-run 2>&1 | sed -n 's/.*Executable .*(\(.*\))$/\1/p' | tail -1)
[ -n "$exe" ] || { echo "cargo built no test binary for: $*" >&2; exit 1; }
log=target/test-loop.log
mkdir -p target
: > "$log"
for p in $(seq "$procs"); do
    (
        for r in $(seq "$runs"); do
            "$exe" -q > /dev/null 2>&1 || echo "process $p run $r: exit $?" >> "$log"
        done
    ) &
done
wait
echo "$(wc -l < "$log") of $((runs * procs)) runs failed ($crate/$log)"
ls -l target/crash-dumps 2> /dev/null
[ ! -s "$log" ]
