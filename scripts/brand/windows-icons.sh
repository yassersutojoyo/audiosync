#!/bin/bash
#
# Generates the Windows app's icons from the brand files, with no dependencies
# beyond Xcode's command line tools and Pillow:
#   desktop/res/audiosync.ico, audiosync-tile-256.png and the MSIX logos
#                                     the app icon, from scripts/brand/app_tile.py
#   desktop/res/audiosync-256.png     the white plate at 256 px (receiver artwork,
#                                     the Linux tray, the installer's pictures)
#   desktop/res/tray-{light,dark}.ico           the tray icon for a light or dark taskbar
#   desktop/res/tray-{light,dark}-off.ico       the same, dimmed while streaming is off
#   desktop/res/tray-*-notice.ico               each with a badge while a notice is up
# The plate is brand/audiosync-icon.svg cropped to its tile; the tray icons are
# brand/audiosync-mark.svg in the token colors (scripts/brand/ico.py).
#
# Usage: scripts/brand/windows-icons.sh
#
set -o errexit -o nounset -o pipefail
cd "$(dirname "$0")/../.."

work=$(mktemp -d "${TMPDIR:-/tmp}/audiosync-windows-icons-XXXXXX")
trap 'rm -rf "$work"' EXIT
out=desktop/res

python3 -I scripts/brand/ico.py svgs "$work"
swiftc -O scripts/brand/render-svg.swift -o "$work/render-svg"

# render <name> <size>...: renders $work/<name>.svg at each size and packs the ICO.
render() {
  local name="$1"
  shift
  local pngs=()
  for size in "$@"; do
    "$work/render-svg" "$work/$name.svg" "$work/$name-$size.png" "$size"
    pngs+=("$work/$name-$size.png")
  done
  python3 -I scripts/brand/ico.py pack "$out/$name.ico" "${pngs[@]}"
}

# Tray icons: 16 px at 100% scaling up to 48 px at 300%.
for name in tray-light tray-dark tray-light-off tray-dark-off; do
  render "$name" 16 20 24 32 40 48
  render "$name-notice" 16 20 24 32 40 48
done

"$work/render-svg" "$work/app.svg" "$out/audiosync-256.png" 256
python3 -I scripts/brand/app_tile.py

echo "✅ Windows icons written to $out/"
