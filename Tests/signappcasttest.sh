#!/bin/bash
# Checks scripts/sign_appcast.sh's choice between a release with Windows zips
# and a Mac-only one, without a Mac or a signing key: the script must stop
# before signing anything. A fake uname reports Darwin and SPARKLE_BIN points
# at an empty folder, so an accepted release stops at "generate_appcast not
# found" on any host. Usage: Tests/signappcasttest.sh (run by Tests/run-tests.sh).
# covers: S311
set -o errexit -o nounset -o pipefail
cd "$(dirname "$0")/.."
tmp=$(mktemp -d "${TMPDIR:-/tmp}/audiosync-signappcast-XXXXXX")
trap 'rm -rf "$tmp"' EXIT
failures=0

mkdir -p "$tmp/bin" "$tmp/sparkle" "$tmp/release"
printf '#!/bin/sh\necho Darwin\n' > "$tmp/bin/uname"
chmod +x "$tmp/bin/uname"
release=$tmp/release
mac_zip=$release/AudioSync-0.8.0.zip
x64_zip=AudioSync-0.8.0-windows-x64.zip
arm64_zip=AudioSync-0.8.0-windows-arm64.zip

# lay <zip names…>: the release folder holds the Mac zip and these.
lay() {
  rm -f "$release"/*.zip
  : > "$mac_zip"
  local name
  for name in "$@"; do : > "$release/$name"; done
}

# expect <name> <text the output must hold> [<text it must not hold>] -- <arguments…>
expect() {
  local name=$1 want=$2 refuse="" result=0
  shift 2
  if [ "$1" != "--" ]; then refuse=$1; shift; fi
  shift
  PATH="$tmp/bin:$PATH" SPARKLE_BIN="$tmp/sparkle" scripts/sign_appcast.sh "$@" > "$tmp/out" 2>&1 || result=$?
  if [ "$result" = 0 ]; then
    echo "  FAIL  $name: exit 0 (it must stop before signing)"
    failures=$((failures + 1))
  elif ! grep -qF -- "$want" "$tmp/out"; then
    echo "  FAIL  $name: no line with '$want'"
    sed 's/^/        /' "$tmp/out"
    failures=$((failures + 1))
  elif [ -n "$refuse" ] && grep -qF -- "$refuse" "$tmp/out"; then
    echo "  FAIL  $name: a line with '$refuse'"
    sed 's/^/        /' "$tmp/out"
    failures=$((failures + 1))
  else
    echo "  ok    $name"
  fi
}

lay
expect "no Windows zips and no --mac-only: refused, naming the x64 zip" "$x64_zip" -- 0.8.0 "$mac_zip"
expect "no Windows zips and no --mac-only: refused, naming the arm64 zip" "$arm64_zip" -- 0.8.0 "$mac_zip"
lay "$x64_zip"
expect "only the x64 zip: refused, naming the arm64 zip" "$arm64_zip" "generate_appcast not found" \
  -- 0.8.0 "$mac_zip"
lay "$arm64_zip"
expect "only the arm64 zip: refused, naming the x64 zip" "$x64_zip" "generate_appcast not found" \
  -- 0.8.0 "$mac_zip"
lay "$x64_zip" "$arm64_zip"
expect "both Windows zips: on to the signing tools" "generate_appcast not found" "windows" \
  -- 0.8.0 "$mac_zip"
lay
expect "--mac-only without Windows zips: on to the signing tools" "generate_appcast not found" "windows" \
  -- --mac-only 0.8.0 "$mac_zip"
lay "$x64_zip" "$arm64_zip"
expect "--mac-only with Windows zips next to it: refused, naming one" "$x64_zip" "generate_appcast not found" \
  -- --mac-only 0.8.0 "$mac_zip"
lay
expect "an unknown option: usage" "usage:" "generate_appcast not found" -- --mac 0.8.0 "$mac_zip"

if [ "$failures" = 0 ]; then
  echo "PASSED"
else
  echo "FAILED: $failures case(s)"
  exit 1
fi
