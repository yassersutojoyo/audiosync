#!/bin/bash
# Checks scripts/shellcheck.sh on a made-up repository in a temp dir: every
# listed script (globs included) goes to shellcheck, a finding fails the run,
# and so do a shellcheck other than the pinned version and no shellcheck.
# Usage: Tests/shellchecktest.sh (run by Tests/run-tests.sh).
# covers: S156
set -o errexit -o nounset -o pipefail
cd "$(dirname "$0")/.."
repo=$PWD
pinned=$(sed -n 's/^VERSION=//p' scripts/shellcheck.sh)
tmp=$(mktemp -d "${TMPDIR:-/tmp}/audiosync-shellcheck-XXXXXX")
trap 'rm -rf "$tmp"' EXIT
failures=0
pass() { echo "  PASS  $1"; }
fail() { echo "  FAIL  $1"; failures=$((failures + 1)); }

# The runs get a PATH of only a tools dir, so no shellcheck can answer but a
# stub. Dropping whole PATH folders that hold one would also drop /usr/bin on
# Linux, and with it the dirname and sed that shellcheck.sh needs. Wrappers
# exec by absolute path, so Windows tools still find their DLLs beside them.
bare_path=$tmp/tools
mkdir -p "$bare_path"
for tool in dirname sed git grep sort tr cat; do
  if ! real=$(command -v "$tool"); then echo "FAILED: $tool not found on PATH"; exit 1; fi
  printf '#!/bin/sh\nexec "%s" "$@"\n' "$real" > "$bare_path/$tool"
  chmod +x "$bare_path/$tool"
done

# Outside any git repository (GIT_CEILING_DIRECTORIES in run), so the
# tracked-file rule has nothing to compare.
fixture=$tmp/repo
mkdir -p "$fixture/scripts" "$fixture/more"
cp "$repo/scripts/shellcheck.sh" "$fixture/scripts/"
printf 'good.sh\nmore/*.sh\n' > "$fixture/scripts/shellcheck-files.txt"
printf '#!/bin/bash\necho "one"\n' > "$fixture/good.sh"
printf '#!/bin/bash\necho "two"\n' > "$fixture/more/a.sh"
printf '#!/bin/bash\necho "three"\n' > "$fixture/more/b.sh"

# A stub shellcheck: reports $STUB_VERSION, logs the files it is given and
# finds a problem in any file that contains STUB-FINDING.
mkdir -p "$tmp/stub"
cat > "$tmp/stub/shellcheck" <<'EOF'
#!/bin/sh
if [ "$1" = --version ]; then printf 'ShellCheck\nversion: %s\n' "$STUB_VERSION"; exit 0; fi
printf '%s\n' "$@" > "$STUB_LOG"
for f in "$@"; do
  if grep -q STUB-FINDING "$f"; then echo "$f: finding"; exit 1; fi
done
EOF
chmod +x "$tmp/stub/shellcheck"

run() {   # run <PATH> [<stub version>]; sets out and rc
  rc=0
  out=$(GIT_CEILING_DIRECTORIES=$tmp STUB_VERSION=${2:-} STUB_LOG="$tmp/stub.log" PATH=$1 "$BASH" "$fixture/scripts/shellcheck.sh" 2>&1) || rc=$?
}

run "$tmp/stub:$bare_path" "$pinned"
if [ "$rc" = 0 ] && [ "$out" = "shellcheck $pinned passed (3 scripts)" ] \
    && [ "$(sort "$tmp/stub.log" | tr '\n' ' ')" = "good.sh more/a.sh more/b.sh " ]; then
  pass "every listed script, globs included, is checked and counted"
else
  fail "listed scripts: rc $rc, output '$out', checked: $(tr '\n' ' ' < "$tmp/stub.log" 2>/dev/null)"
fi

echo 'echo STUB-FINDING' >> "$fixture/more/b.sh"
run "$tmp/stub:$bare_path" "$pinned"
if [ "$rc" != 0 ] && [[ $out != *passed* ]]; then
  pass "a finding in a listed script fails the run"
else
  fail "a finding passed: rc $rc, output '$out'"
fi
printf '#!/bin/bash\necho "three"\n' > "$fixture/more/b.sh"

run "$tmp/stub:$bare_path" 0.9.0
if [ "$rc" != 0 ] && [[ $out == *"0.9.0 found"* ]]; then
  pass "a shellcheck other than $pinned fails the run"
else
  fail "another version: rc $rc, output '$out'"
fi

run "$bare_path"
if [ "$rc" != 0 ] && [[ $out == *"shellcheck is not installed"* ]]; then
  pass "a missing shellcheck fails the run"
else
  fail "no shellcheck: rc $rc, output '$out'"
fi

# The real pinned shellcheck on a planted bad script (SC2086, unquoted $1).
if ! command -v shellcheck >/dev/null 2>&1 \
    || [ "$(shellcheck --version | sed -n 's/^version: //p')" != "$pinned" ]; then
  echo "  SKIP  real shellcheck finding (shellcheck $pinned isn't installed here; CI runs it)"
else
  run "$PATH"
  if [ "$rc" = 0 ]; then
    # shellcheck disable=SC2016 # the planted script must keep $1 unquoted
    printf '#!/bin/bash\necho $1\n' > "$fixture/more/b.sh"
    run "$PATH"
    if [ "$rc" != 0 ] && [[ $out == *SC2086* ]] && [[ $out != *passed* ]]; then
      pass "real shellcheck $pinned: a planted finding fails the run"
    else
      fail "real shellcheck on a planted finding: rc $rc, output '$out'"
    fi
  else
    fail "real shellcheck on clean scripts: rc $rc, output '$out'"
  fi
fi

if [ "$failures" = 0 ]; then echo "PASSED"; else echo "FAILED ($failures)"; exit 1; fi
