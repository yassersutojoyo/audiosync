#!/bin/bash
#
# Builds the site's icons from one square PNG, normally the 1024 px icon-source.png
# that scripts/brand/build-brand.sh renders from brand/audiosync-icon.svg:
#   favicon.ico                   32 px, the browser tab fallback
#   favicon-16x16.png, favicon-32x32.png, favicon.png (48 px)
#   apple-touch-icon.png          180 px, the iOS home screen
#   mstile-150x150.png            150 px, Windows tiles
#   icon-192.png, icon-512.png    the web app manifest's icons
#   icon.png                      the source itself, full size
# Uses only sips, which ships with macOS.
#
# Usage: ./make-web-icons.sh [source.png] [output-dir]
#   source.png    defaults to icon-source.png (square, at least 512 px)
#   output-dir    defaults to site/ in the repository root
#
set -euo pipefail
root="$(cd "$(dirname "$0")" && pwd)"
src="${1:-$root/icon-source.png}"
dest="${2:-$root/site}"

[ -f "$src" ] || { echo "make-web-icons.sh: no such file: $src" >&2; exit 1; }
# One sips call reports both sides.
read -r width height < <(sips -g pixelWidth -g pixelHeight "$src" |
  awk '$1 == "pixelWidth:" { w = $2 } $1 == "pixelHeight:" { h = $2 } END { print w, h }')
if [ "$width" != "$height" ] || [ "$width" -lt 512 ]; then
  echo "make-web-icons.sh: $src is ${width}x${height}; it must be square and at least 512 px" >&2
  exit 1
fi
mkdir -p "$dest"

# png <pixels> <name>: the source scaled to <pixels> square, written to <dest>/<name>.
png() {
  sips --resampleHeightWidth "$1" "$1" "$src" --out "$dest/$2" >/dev/null
}

png 16 favicon-16x16.png
png 32 favicon-32x32.png
png 48 favicon.png
png 150 mstile-150x150.png
png 180 apple-touch-icon.png
png 192 icon-192.png
png 512 icon-512.png
cp "$src" "$dest/icon.png"
sips --setProperty format ico "$dest/favicon-32x32.png" --out "$dest/favicon.ico" >/dev/null

echo "Wrote the site icons to $dest"
