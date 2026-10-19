#!/bin/bash
#
# Builds the Mac app icon (AppIcon.icns) from one square PNG, normally the 1024 px
# icon-source.png that scripts/brand/build-brand.sh renders from brand/audiosync-icon.svg.
# Uses only sips and iconutil, which ship with macOS.
#
# Usage: ./makeicon.sh [source.png] [output.icns]
#   source.png    defaults to icon-source.png (square, at least 1024 px)
#   output.icns   defaults to AppIcon.icns in the repository root
#
set -o errexit -o nounset -o pipefail
root="$(cd "$(dirname "$0")" && pwd)"
src="${1:-$root/icon-source.png}"
out="${2:-$root/AppIcon.icns}"

[ -f "$src" ] || { echo "makeicon.sh: no such file: $src" >&2; exit 1; }
# One sips call reports both sides.
read -r width height < <(sips -g pixelWidth -g pixelHeight "$src" |
  awk '$1 == "pixelWidth:" { w = $2 } $1 == "pixelHeight:" { h = $2 } END { print w, h }')
if [ "$width" != "$height" ] || [ "$width" -lt 1024 ]; then
  echo "makeicon.sh: $src is ${width}x${height}; it must be square and at least 1024 px" >&2
  exit 1
fi

work=$(mktemp -d "${TMPDIR:-/tmp}/audiosync-appicon-XXXXXX")
trap 'rm -rf "$work"' EXIT
# iconutil only accepts a folder whose name ends in .iconset.
set_dir="$work/AppIcon.iconset"
mkdir "$set_dir"

# scaled <pixels> <file>: the source at <pixels> square; copied unchanged at full size.
scaled() {
  if [ "$1" -eq "$width" ]; then
    cp "$src" "$2"
  else
    sips --resampleHeightWidth "$1" "$1" "$src" --out "$2" >/dev/null
  fi
}

# The iconset layout iconutil expects: every point size at 1x and 2x.
for points in 16 32 128 256 512; do
  scaled "$points" "$set_dir/icon_${points}x${points}.png"
  scaled $((points * 2)) "$set_dir/icon_${points}x${points}@2x.png"
done

iconutil --convert icns --output "$out" "$set_dir"
echo "Wrote $out"
