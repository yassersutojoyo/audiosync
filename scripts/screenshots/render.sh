#!/bin/bash
#
# Regenerates the app screenshots in docs/images from mock data: the main window,
# the menu bar panel and the equalizer, each in light and dark appearance. It draws
# the app's own SwiftUI views offscreen, so it needs no installed app, no engine, no
# permissions and no audio devices. Needs only Xcode's command line tools.
# Then composes social-preview.png (the link preview image) from the light main
# window and the brand files, with social-preview.swift.
#
# Control styling comes from the macOS that runs the script: re-render after a UI
# change, and on the macOS version the README describes.
#
# Usage: scripts/screenshots/render.sh [output-dir]
#        scripts/screenshots/render.sh --review [output-dir]
#   output-dir  where the PNGs go (default: docs/images)
#   --review    also renders states worth checking before a UI change lands
#               (streaming off with a notice, no speakers) as review-*.png. They
#               go to a new temporary directory unless output-dir is given, which
#               must be outside the repository. It skips the social preview.
#
set -euo pipefail

usage() {
  echo "usage: scripts/screenshots/render.sh [--review] [output-dir]" >&2
  exit 2
}

review=""
dest=""
for arg in "$@"; do
  case "$arg" in
    --review) review="--review" ;;
    -*) usage ;;
    *) [ -z "$dest" ] || usage; dest="$arg" ;;
  esac
done

# A relative output directory is relative to where the script was run from.
case "$dest" in
  "" | /*) ;;
  *) dest="$(pwd -P)/$dest" ;;
esac

cd "$(dirname "$0")/../.."
repo=$(pwd -P)

if [ -n "$review" ]; then
  if [ -z "$dest" ]; then
    dest=$(mktemp -d "${TMPDIR:-/tmp}/audiosync-review-XXXXXX")
  fi
  case "$dest/" in
    "$repo"/*) echo "--review writes review-*.png: pick an output directory outside the repository" >&2; exit 2 ;;
  esac
fi
dest="${dest:-docs/images}"

BUILD=$(mktemp -d "${TMPDIR:-/tmp}/audiosync-screenshots-XXXXXX")
trap 'rm -rf "$BUILD"' EXIT

# Every app source except main.swift, whose top-level code would launch the app.
srcs=()
for f in Sources/AudioSync/*.swift; do
  [ "$(basename "$f")" = main.swift ] || srcs+=("$f")
done
cp scripts/screenshots/render.swift "$BUILD/main.swift"
echo "▶ building the renderer"
swiftc -swift-version 5 "$BUILD/main.swift" "${srcs[@]}" -o "$BUILD/render"

"$BUILD/render" "$dest" brand/fonts/Poppins-Medium.ttf $review

if [ -z "$review" ]; then
  echo "▶ composing the social preview"
  swiftc -swift-version 5 scripts/screenshots/social-preview.swift -o "$BUILD/social-preview"
  "$BUILD/social-preview" "$dest/main-window-light.png" "$dest/social-preview.png"
fi
echo "✅ Screenshots rendered."
