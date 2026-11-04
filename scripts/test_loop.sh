#!/usr/bin/env bash
# Runs one test binary over and over in parallel processes, to catch a crash
# or a load flake that shows up once in hundreds of runs, and prints how often
# each test failed. On Windows a crashed cast or desktop test binary leaves a
# minidump in <crate>/target/crash-dumps (cast/tests/common/crash_dump.rs).
#
# usage: scripts/test_loop.sh [-b <busy loops>] <crate dir> <runs per process> <processes> <cargo test target>
#   e.g. scripts/test_loop.sh -b 16 airplay 50 8 --test ap2_samsung
# The busy loops run at the lowest priority and the copies at normal priority,
# so the copies compete with the load as the app would.
set -u
busy=0
while getopts b: opt; do
    case $opt in
        b) busy=$OPTARG ;;
        *) exit 2 ;;
    esac
done
shift $((OPTIND - 1))
[ $# -ge 4 ] || { sed -n '7,8p' "$0" >&2; exit 2; }
crate=$1 runs=$2 procs=$3
shift 3
root=$(cd "$(dirname "$0")/.." && pwd)
main=$(dirname "$(git -C "$root" rev-parse --path-format=absolute --git-common-dir)")
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$main/windows/target}"
cd "$root/$crate" || exit 1
build=$(cargo test "$@" --no-run 2>&1) || { echo "$build" >&2; exit 1; }
exe=$(echo "$build" | sed -n 's/.*Executable .*(\(.*\))$/\1/p' | tail -1)
[ -n "$exe" ] || { echo "$build" >&2; echo "cargo built no test binary for: $*" >&2; exit 1; }
log=target/test-loop.log
failed=target/test-loop.failed
passed=target/test-loop.passed
mkdir -p target
: > "$log"
: > "$failed"
: > "$passed"
rm -f target/test-loop-*-*
# A build in another worktree can replace the binary in the shared target dir
# mid-loop, so the loop runs a private copy.
bin=target/test-loop-$$-$(basename "$exe")
cp "$exe" "$bin" || exit 1
exe=$bin
loops=()
copies=()
# Windows keeps a running binary locked, so the copies end before it is removed.
trap 'kill "${loops[@]}" "${copies[@]}" 2> /dev/null; wait; rm -f "$bin"' EXIT
for _ in $(seq "$busy"); do
    nice -n 19 sh -c 'while :; do :; done' &
    loops+=("$!")
done
for p in $(seq "$procs"); do
    (
        out=target/test-loop-$p.out
        for r in $(seq "$runs"); do
            "$exe" > "$out" 2>&1 && { echo "$p" >> "$passed"; continue; }
            code=$?
            tests=$(sed -n 's/^test \(.*\) \.\.\. FAILED$/\1/p' "$out")
            echo "process $p run $r: exit $code ${tests:-(no test reported FAILED)}" | paste -sd ' ' - >> "$log"
            [ -z "$tests" ] || echo "$tests" >> "$failed"
            cp "$out" "target/test-loop-$p-$r.fail"
        done
        rm -f "$out"
    ) &
    copies+=("$!")
done
wait "${copies[@]}"
for p in $(seq "$procs"); do
    echo "process $p: $(grep -cx "$p" "$passed") passed, $(grep -c "^process $p run " "$log") failed"
done
echo "$(wc -l < "$log") of $(($(wc -l < "$log") + $(wc -l < "$passed"))) runs failed ($crate/$log)"
sort "$failed" | uniq -c | sort -rn
ls -l target/crash-dumps 2> /dev/null
[ ! -s "$log" ]
