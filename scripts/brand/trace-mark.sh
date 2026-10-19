#!/bin/bash
#
# Re-traces the logo mark (brand/audiosync-mark.svg) from its raster reference
# (brand/source/mark-reference.png) with potrace. Only needed when the reference
# changes; everything else is generated from the SVG by scripts/brand/build-brand.sh.
#
# Requires potrace (brew install potrace).
# Usage: scripts/brand/trace-mark.sh
#
set -o errexit -o nounset -o pipefail
cd "$(dirname "$0")/../.."

command -v potrace >/dev/null || { echo "❌ potrace not found: brew install potrace"; exit 1; }

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

# 4x upsample with a light blur (sigma 3) removes compression noise along the edges.
swift scripts/brand/prepare-bitmap.swift brand/source/mark-reference.png "$WORK/mark.pgm" 4 3
# -a 1.0: default corner threshold; -O 1: merge curves within 1 px; -t 20: drop specks.
potrace "$WORK/mark.pgm" -b svg -k 0.5 -t 20 -a 1.0 -O 1 --flat -o "$WORK/potrace.svg"
python3 -I scripts/brand/tidy-svg.py "$WORK/potrace.svg" brand/audiosync-mark.svg

echo "✅ Traced brand/audiosync-mark.svg — run scripts/brand/build-brand.sh to regenerate the icons."
