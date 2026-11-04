#!/bin/bash
# Checks that build.sh signs with the Hardened Runtime and AudioSync.entitlements
# whatever the identity, not only a Developer ID one: without the runtime any
# process of the same user can start AudioSync with DYLD_INSERT_LIBRARIES and use
# its microphone and audio recording permission. Runs build.sh's own sign()
# and entitlements choice against a fake codesign, so it needs no Mac.
# Usage: Tests/hardenedruntimetest.sh (run by Tests/run-tests.sh).
set -o errexit -o nounset -o pipefail
cd "$(dirname "$0")/.."
failures=0

sign_fn=$(awk '/^sign\(\) \{$/ { f = 1 } f { print } f && /^}$/ { exit }' build.sh)
entitlements_choice=$(awk '/^if \[ -n "\$\{ENTITLEMENTS:-\}" \]; then$/ { f = 1 } f { print } f && /^fi$/ { exit }' build.sh)

# codesign_args <identity> <keychain>: the arguments sign() hands codesign, on one line.
# shellcheck disable=SC2034,SC2329 # build.sh's sign(), run through eval, reads and calls these
codesign_args() (
  set +o nounset
  identity=$1
  keychain=$2
  # sign() shows codesign's output only when codesign fails.
  codesign() { echo "$*"; return 1; }
  eval "$sign_fn"
  sign /fake/AudioSync.app 2>&1 || true
)

# shellcheck disable=SC2034 # read by the evaluated build.sh lines
default_entitlements() (
  set +o nounset
  unset ENTITLEMENTS
  ROOT=/repo
  entitlements=""
  eval "$entitlements_choice"
  echo "$entitlements"
)

check() {   # check <name> <command...>
  local name=$1
  shift
  if "$@"; then
    echo "  ok    $name"
  else
    echo "  FAIL  $name"
    failures=$((failures + 1))
  fi
}
holds() { [[ " $1 " == *" $2 "* ]]; }

if [ -z "$sign_fn" ] || [ -z "$entitlements_choice" ]; then
  echo "  FAIL  build.sh's sign() or its ENTITLEMENTS choice wasn't found"
  exit 1
fi
local_args=$(codesign_args 0123456789ABCDEF0123456789ABCDEF01234567 /keychains/local)
adhoc_args=$(codesign_args - "")
check "local identity: Hardened Runtime" holds "$local_args" "--options runtime"
check "local identity: no secure timestamp" holds "$local_args" "--timestamp=none"
check "ad-hoc: Hardened Runtime" holds "$adhoc_args" "--options runtime"
check "no ENTITLEMENTS: AudioSync.entitlements" [ "$(default_entitlements)" = /repo/AudioSync.entitlements ]

if [ "$failures" -gt 0 ]; then
  echo "FAIL: $failures check(s)"
  exit 1
fi
echo "PASSED"
