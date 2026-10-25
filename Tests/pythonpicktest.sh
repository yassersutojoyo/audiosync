#!/bin/bash
# Checks that Tests/run-tests.sh runs its Python checks with a Python that
# works: Windows puts a Microsoft Store stub named python3 on PATH, which only
# prints "Python was not found", and the privacy and text checks must not pass
# or vanish behind it. Usage: Tests/pythonpicktest.sh (run by Tests/run-tests.sh).
set -o errexit -o nounset -o pipefail
cd "$(dirname "$0")/.."
tmp=$(mktemp -d "${TMPDIR:-/tmp}/audiosync-python-XXXXXX")
trap 'rm -rf "$tmp"' EXIT
failures=0

real=
for candidate in python3 python; do
  if [ "$("$candidate" -I -c 'print(6 * 7)' 2>/dev/null)" = 42 ]; then
    real=$(command -v "$candidate")
    break
  fi
done
if [ -z "$real" ]; then
  echo "  FAIL  no working python3 or python on PATH to run the checks with"
  exit 1
fi

# The Store stub; it exits 0 here, the worst case, so only its output tells.
stub='#!/bin/sh\necho "Python was not found; run without arguments to install from the Microsoft Store" >&2\n'
mkdir -p "$tmp/fallback" "$tmp/none"
for dir in fallback none; do
  printf '%b' "$stub" > "$tmp/$dir/python3"
  chmod +x "$tmp/$dir/python3"
done
# shellcheck disable=SC2016 # "$@" is the shim's own, expanded when it runs
printf '#!/bin/sh\nexec "%s" "$@"\n' "$real" > "$tmp/fallback/python"
printf '%b' "$stub" > "$tmp/none/python"
chmod +x "$tmp/fallback/python" "$tmp/none/python"

suite() {   # suite <stub dir>: the Python checks of Tests/run-tests.sh only
  AUDIOSYNC_TESTS_ONLY=python PATH="$1:$PATH" Tests/run-tests.sh > "$tmp/out" 2>&1
}

if suite "$tmp/fallback" && grep -q '^private references: none in' "$tmp/out" \
    && grep -q '^self-test: [0-9]* cases, 0 failure' "$tmp/out"; then
  echo "  PASS  a python3 stub falls back to python"
else
  echo "  FAIL  a python3 stub falls back to python; the suite printed:"
  sed 's/^/        /' "$tmp/out"
  failures=$((failures + 1))
fi

if ! suite "$tmp/none" && grep -q 'FAIL.*Python' "$tmp/out" && ! grep -q '^private references: none' "$tmp/out"; then
  echo "  PASS  no working Python fails the suite"
else
  echo "  FAIL  no working Python fails the suite; the suite printed:"
  sed 's/^/        /' "$tmp/out"
  failures=$((failures + 1))
fi

[ "$failures" = 0 ]
