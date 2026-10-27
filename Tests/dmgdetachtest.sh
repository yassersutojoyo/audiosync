#!/bin/bash
# Checks how make-dmg.sh detaches an image, without a Mac: a fake hdiutil that
# stays busy for a set number of plain detaches. A volume Finder or Spotlight
# still holds must get plain retries before -force, since a forced detach makes
# macOS show "Disk Not Ejected Properly". Usage: Tests/dmgdetachtest.sh (run by
# Tests/run-tests.sh).
set -o errexit -o nounset -o pipefail
cd "$(dirname "$0")/.."
tmp=$(mktemp -d "${TMPDIR:-/tmp}/audiosync-dmgdetach-XXXXXX")
trap 'rm -rf "$tmp"' EXIT
failures=0

definition=$(awk '/^detach\(\) \{/, /^\}/' make-dmg.sh)
if [ -z "$definition" ]; then
  echo "  FAIL  no detach() function in make-dmg.sh"
  exit 1
fi
eval "$definition"
# The real wait would only slow the test; the log shows where it happened.
sleep() { echo "sleep $*" >> "$tmp/calls"; }

mkdir -p "$tmp/bin"
cat > "$tmp/bin/hdiutil" <<'FAKE'
#!/bin/bash
echo "hdiutil $*" >> "$FAKE_DIR/calls"
case " $* " in
  *" -force "*) exit "$FAKE_FORCE_STATUS" ;;
esac
count=$(cat "$FAKE_DIR/busy")
if [ "$count" -gt 0 ]; then
  echo $((count - 1)) > "$FAKE_DIR/busy"
  echo "hdiutil: couldn't unmount \"disk9\" - Resource busy" >&2
  exit 16
fi
exit 0
FAKE
chmod +x "$tmp/bin/hdiutil"
export FAKE_DIR="$tmp"

# run <busy plain detaches> <-force exit status>: detach /Volumes/AudioSync and
# leave its exit status in $result, its stderr in $tmp/err and the calls in $tmp/calls.
run() {
  echo "$1" > "$tmp/busy"
  : > "$tmp/calls"
  result=0
  FAKE_FORCE_STATUS=$2 PATH="$tmp/bin:$PATH" detach /Volumes/AudioSync 2> "$tmp/err" || result=$?
}

expect() {   # expect <name> <exit status> <plain detaches> <forced detaches> <warns: 0|1>
  local plain forced warned=0
  plain=$(grep -c '^hdiutil detach -quiet ' "$tmp/calls" || true)
  forced=$(grep -c '^hdiutil detach -force ' "$tmp/calls" || true)
  if grep -q -- '-force' "$tmp/err"; then warned=1; fi
  if [ "$result" = "$2" ] && [ "$plain" = "$3" ] && [ "$forced" = "$4" ] && [ "$warned" = "$5" ]; then
    echo "  ok    $1"
  else
    echo "  FAIL  $1: exit $result, $plain plain, $forced forced, warned $warned (want $2, $3, $4, $5)"
    sed 's/^/        /' "$tmp/calls" "$tmp/err"
    failures=$((failures + 1))
  fi
}

run 0 0
expect "a free volume detaches on the first plain try" 0 1 0 0

run 2 0
expect "a volume busy twice detaches on the third plain try, never forced" 0 3 0 0
if [ "$(grep -c '^sleep ' "$tmp/calls")" != 2 ]; then
  echo "  FAIL  the two busy tries were not each followed by a wait"
  failures=$((failures + 1))
fi

run 99 0
expect "a volume busy on every plain try is forced last, with a warning" 0 5 1 1

run 99 1
expect "a failed forced detach fails the call" 1 5 1 1

if [ "$failures" = 0 ]; then
  echo "PASSED"
else
  echo "FAIL: $failures make-dmg.sh detach case(s)"
  exit 1
fi
