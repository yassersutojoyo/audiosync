#!/bin/bash
# Checks scripts/install.sh without the network: a fake curl on PATH answers
# from fixtures. Covers the version lookup (pretty and minified API answers,
# a given version, a bad one, an unreachable API) through --dry-run, and on a
# Mac a checksum mismatch, which must stop before /Applications changes.
# Usage: Tests/installtest.sh (run by Tests/run-tests.sh).
set -o errexit -o nounset -o pipefail
cd "$(dirname "$0")/.."
repo=$PWD
tmp=$(mktemp -d "${TMPDIR:-/tmp}/audiosync-install-test-XXXXXX")
trap 'rm -rf "$tmp"' EXIT
failures=0
pass() { echo "  PASS  $1"; }
flunk() { echo "  FAIL  $1"; failures=$((failures + 1)); }

# The fake curl prints $FIXTURES/api.json for the API, saves $FIXTURES/<file
# name> for a download given -o, and fails like curl -f when FAKE_CURL_FAIL is set.
mkdir -p "$tmp/bin" "$tmp/fixtures"
cat > "$tmp/bin/curl" <<'EOF'
#!/bin/bash
[ -z "${FAKE_CURL_FAIL:-}" ] || exit 22
out="" url=""
while [ $# -gt 0 ]; do
  case $1 in
    -o) out=$2; shift ;;
    -H) shift ;;
    -*) ;;
    *) url=$1 ;;
  esac
  shift
done
case $url in
  */releases/latest) cat "$FIXTURES/api.json" ;;
  *) cp "$FIXTURES/${url##*/}" "$out" ;;
esac
EOF
chmod +x "$tmp/bin/curl"
export FIXTURES="$tmp/fixtures"

run() {   # run <args…>: install.sh with the fake curl, stdout and stderr together
  PATH="$tmp/bin:$PATH" bash "$repo/scripts/install.sh" "$@" 2>&1
}

base=https://github.com/yassersutojoyo/audiosync/releases/download/v0.6.0
expected="version 0.6.0
zip $base/AudioSync-0.6.0.zip
sums $base/SHA256SUMS.txt
app /Applications/AudioSync.app"

check_dry() {   # check_dry <name> <args…>
  local name=$1 got; shift
  if got=$(run --dry-run "$@" | grep -v '^Looking up'); [ "$got" = "$expected" ]; then
    pass "$name"
  else
    flunk "$name: got '$got'"
  fi
}

check_dry "given version" 0.6.0
check_dry "given tag with a v" v0.6.0

cat > "$tmp/fixtures/api.json" <<'EOF'
{
  "url": "https://api.github.com/repos/yassersutojoyo/audiosync/releases/1",
  "author": {
    "login": "yassersutojoyo"
  },
  "tag_name": "v0.6.0",
  "name": "AudioSync 0.6.0",
  "body": "Mentions \"tag_name\": \"v9.9.9\" in the notes."
}
EOF
check_dry "latest, pretty JSON"

printf '%s' '{"url":"x","author":{"login":"y"},"tag_name":"v0.6.0","assets":[{"name":"AudioSync.dmg"}]}' \
  > "$tmp/fixtures/api.json"
check_dry "latest, minified JSON"

printf '%s' '{"message":"Not Found"}' > "$tmp/fixtures/api.json"
if run --dry-run >/dev/null; then flunk "API answer without a tag"; else pass "API answer without a tag"; fi

if FAKE_CURL_FAIL=1 run --dry-run >/dev/null; then flunk "API unreachable"; else pass "API unreachable"; fi

for bad in '1.0; rm -rf /' '../0.6.0' 'latest' $'0.6.0\nrm' ''; do
  if got=$(run --dry-run "$bad"); then
    flunk "rejects version '$bad'"
  elif printf '%s' "$got" | grep -Eq "isn't a version number|is empty"; then
    pass "rejects version '$bad'"
  else
    flunk "rejects version '$bad': got '$got'"
  fi
done

if [ "$(uname -s)" != Darwin ]; then
  if got=$(run 0.6.0); then
    flunk "refuses to install off macOS"
  elif printf '%s' "$got" | grep -q 'macOS only'; then
    pass "refuses to install off macOS"
  else
    flunk "refuses to install off macOS: got '$got'"
  fi
elif [ -w /Applications ]; then
  printf 'not a zip' > "$tmp/fixtures/AudioSync-0.6.0.zip"
  printf '%064d  AudioSync-0.6.0.zip\n' 0 > "$tmp/fixtures/SHA256SUMS.txt"
  before=$(ls -la /Applications)
  if got=$(run 0.6.0); then
    flunk "checksum mismatch stops the install"
  elif printf '%s' "$got" | grep -q 'Nothing was installed' && [ "$(ls -la /Applications)" = "$before" ]; then
    pass "checksum mismatch stops the install"
  else
    flunk "checksum mismatch stops the install: got '$got'"
  fi
else
  echo "  SKIP  checksum mismatch: /Applications isn't writable here"
fi

if [ "$failures" -gt 0 ]; then
  echo "FAILED: $failures"
  exit 1
fi
echo "PASSED"
