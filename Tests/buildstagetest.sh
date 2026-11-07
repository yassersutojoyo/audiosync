#!/bin/bash
# Checks that build.sh refuses an AUDIOSYNC_INSTALL_DIR in its own staging
# folder: the install step deletes that folder, and the app installed into it,
# after copying. A fake uname stops build.sh at its macOS check otherwise, so
# nothing is built, quit or installed on any host. build.sh runs from a copy in
# a scratch folder, so the test decides whether its staging folder exists.
# Usage: Tests/buildstagetest.sh (run by Tests/run-tests.sh).
set -o errexit -o nounset -o pipefail
cd "$(dirname "$0")/.."
tmp=$(mktemp -d "${TMPDIR:-/tmp}/audiosync-build-stage-XXXXXX")
trap 'rm -rf "$tmp"' EXIT
root=$(cd "$tmp" && pwd -P)/root
mkdir -p "$root/.build"
cp build.sh "$root/build.sh"
printf '#!/bin/sh\necho NotDarwin\n' > "$tmp/uname"
chmod +x "$tmp/uname"
failures=0

build() {   # build <install dir> [option]: what build.sh --release prints, run from its root
  (cd "$root" && AUDIOSYNC_INSTALL_DIR=$1 PATH="$tmp:$PATH" bash build.sh --release ${2:+"$2"} 2>&1 || true)
}
refused() { [[ $(build "$@") == *"staging folder"* ]]; }
allowed() { [[ $(build "$@") == *"macOS only"* ]]; }

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

stage_cases() {   # stage_cases <state>
  check "$1: the staging folder: refused" refused "$root/.build/app"
  check "$1: relative with a trailing slash: refused" refused ".build/app/"
  check "$1: two trailing slashes: refused" refused ".build/app//"
  check "$1: through a missing folder and ..: refused" refused ".build/x/../app"
  check "$1: a folder inside the staging folder: refused" refused "$root/.build/app/inner"
  check "$1: a folder next to the staging folder: allowed" allowed "$root/.build/apps"
  check "$1: the staging folder with --no-install: allowed" allowed "$root/.build/app" --no-install
}
stage_cases "no staging folder"
mkdir "$root/.build/app"
stage_cases "staging folder present"

ln -s "$root/.build/app" "$tmp/link" 2>/dev/null || true
if [ -L "$tmp/link" ]; then
  check "a symlink to the staging folder: refused" refused "$tmp/link"
else
  echo "  skip  a symlink to the staging folder: this host can't make symlinks"
fi

if [ "$failures" -gt 0 ]; then
  echo "FAIL: $failures check(s)"
  exit 1
fi
echo "PASSED"
