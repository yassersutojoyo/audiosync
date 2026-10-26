#!/bin/bash
# Installs or updates AudioSync in /Applications from a GitHub release.
#   curl -fsSL https://raw.githubusercontent.com/yassersutojoyo/audiosync/main/scripts/install.sh | bash
#   curl -fsSL …/install.sh | bash -s -- 0.6.0      a given version instead of the latest
#   scripts/install.sh --dry-run [VERSION]           print what it would download, change nothing
# Checks the zip against the release's SHA256SUMS.txt before anything in
# /Applications changes, and never uses sudo.
set -o errexit -o nounset -o pipefail

REPO=yassersutojoyo/audiosync
API="https://api.github.com/repos/$REPO/releases/latest"
APPS=/Applications
APP_NAME=AudioSync.app
EXECUTABLE=AudioSync
MIN_MACOS=15

say() { printf '%s\n' "$*"; }
fail() { printf 'AudioSync install: %s\n' "$*" >&2; exit 1; }

usage() {
  say "Usage: install.sh [--dry-run] [VERSION]"
  say "Installs AudioSync VERSION (for example 0.6.0), or the latest release, into $APPS."
}

# Reads the latest release's JSON on stdin and prints its tag. Splitting on
# commas and braces (tr pads the newline out to all three) puts each key on
# its own line, pretty-printed or not.
tag_from_json() {
  tr ',{}' '\n' \
    | sed -n 's/^[[:space:]]*"tag_name"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' \
    | head -n 1
}

valid_version() {
  printf '%s' "$1" | grep -Eq '^[0-9]+(\.[0-9]+)*(-[0-9A-Za-z.]+)?$'
}

check_mac() {
  [ "$(uname -s)" = Darwin ] || fail "this installer is for macOS only. Windows and Linux builds are on https://github.com/$REPO/releases"
  local ver major
  ver=$(sw_vers -productVersion)
  major=${ver%%.*}
  [ "$major" -ge "$MIN_MACOS" ] 2>/dev/null || fail "AudioSync needs macOS $MIN_MACOS or later; this Mac runs $ver."
  local tool
  for tool in curl shasum ditto open pgrep pkill; do
    command -v "$tool" >/dev/null || fail "$tool is missing."
  done
  [ -w "$APPS" ] || fail "$USER can't write to $APPS. Run this from an administrator account, or install from AudioSync.dmg instead. This script never asks for sudo."
}

quit_running() {
  pgrep -x "$EXECUTABLE" >/dev/null || return 0
  say "Quitting the running AudioSync"
  # SIGTERM is a normal quit for AudioSync, and unlike an Apple event it needs no Automation permission.
  pkill -TERM -x "$EXECUTABLE" || true
  local _
  for _ in $(seq 1 100); do
    pgrep -x "$EXECUTABLE" >/dev/null || return 0
    sleep 0.1
  done
  fail "AudioSync is still running after 10 s. Quit it from its menu bar icon and run this again."
}

main() {
  local dry_run=0 version=""
  while [ $# -gt 0 ]; do
    case $1 in
      --dry-run) dry_run=1 ;;
      -h|--help) usage; exit 0 ;;
      -*) usage >&2; exit 2 ;;
      *) [ -z "$version" ] || { usage >&2; exit 2; }; version=$1 ;;
    esac
    shift
  done

  [ "$dry_run" = 1 ] || check_mac

  if [ -z "$version" ]; then
    say "Looking up the latest release"
    local json
    json=$(curl -fsSL -H 'Accept: application/vnd.github+json' "$API") \
      || fail "couldn't reach the GitHub API (it allows 60 lookups an hour per address). Name a version instead: bash -s -- 0.6.0"
    version=$(printf '%s' "$json" | tag_from_json)
    [ -n "$version" ] || fail "the GitHub API answer had no release tag."
  fi
  version=${version#v}
  valid_version "$version" || fail "'$version' isn't a version number like 0.6.0."

  local base="https://github.com/$REPO/releases/download/v$version"
  local zip="AudioSync-$version.zip"
  if [ "$dry_run" = 1 ]; then
    say "version $version"
    say "zip $base/$zip"
    say "sums $base/SHA256SUMS.txt"
    say "app $APPS/$APP_NAME"
    exit 0
  fi

  tmp=$(mktemp -d "${TMPDIR:-/tmp}/audiosync-install-XXXXXX")
  stage=""
  trap 'rm -rf "$tmp"; if [ -n "$stage" ]; then rm -rf "$stage"; fi' EXIT

  say "Downloading AudioSync $version"
  curl -fL --progress-bar -o "$tmp/$zip" "$base/$zip" || fail "couldn't download $base/$zip"
  curl -fsSL -o "$tmp/SHA256SUMS.txt" "$base/SHA256SUMS.txt" || fail "couldn't download $base/SHA256SUMS.txt"

  say "Checking the SHA-256 checksum"
  local expected actual
  expected=$(awk -v f="$zip" '$2 == f || $2 == "*" f { print $1; exit }' "$tmp/SHA256SUMS.txt")
  printf '%s' "$expected" | grep -Eq '^[0-9a-fA-F]{64}$' || fail "SHA256SUMS.txt lists no checksum for $zip."
  actual=$(shasum -a 256 "$tmp/$zip" | awk '{ print $1 }')
  [ "$actual" = "$expected" ] || fail "the checksum of $zip doesn't match SHA256SUMS.txt. Nothing was installed."

  # Unpacked next to the old app, so putting it in place is a rename on the same volume.
  stage=$(mktemp -d "$APPS/.AudioSync-install-XXXXXX")
  ditto -x -k "$tmp/$zip" "$stage" || fail "couldn't unpack $zip."
  [ -x "$stage/$APP_NAME/Contents/MacOS/$EXECUTABLE" ] || fail "$zip doesn't hold $APP_NAME."

  quit_running

  say "Installing to $APPS/$APP_NAME"
  local dest="$APPS/$APP_NAME" old="$stage.old"
  if [ -e "$dest" ]; then
    mv "$dest" "$old" || fail "couldn't move the old $dest aside. Nothing was changed."
  fi
  if ! mv "$stage/$APP_NAME" "$dest"; then
    if [ -e "$old" ]; then mv "$old" "$dest" || true; fi
    fail "couldn't put the new app in place. The old one was put back."
  fi
  if [ -e "$old" ]; then
    rm -rf "$old" || say "Couldn't delete the old copy at $old. Delete it by hand."
  fi

  # curl sets no quarantine attribute, so Gatekeeper doesn't stop this copy at first launch.
  say "Opening AudioSync"
  open "$dest"
  say "Done: AudioSync $version is in $APPS."
}

main "$@"
