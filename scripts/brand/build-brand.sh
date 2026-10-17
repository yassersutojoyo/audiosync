#!/bin/bash
#
# Regenerates every brand-derived file from brand/audiosync-mark.svg and the bundled
# Poppins font: the icon and wordmark SVGs, the SwiftUI mark, icon-source.png,
# AppIcon.icns and the site's favicons. No dependencies beyond Xcode's command line tools.
#
# Usage: scripts/brand/build-brand.sh
#
set -euo pipefail
cd "$(dirname "$0")/../.."

python3 -I scripts/brand/compose.py
swift scripts/brand/wordmark.swift
swift scripts/brand/render-svg.swift brand/audiosync-icon.svg icon-source.png 1024 --icon-shadow
./makeicon.sh icon-source.png
./make-web-icons.sh icon-source.png
cp brand/audiosync-wordmark.svg site/wordmark.svg

echo "✅ Brand files regenerated."
