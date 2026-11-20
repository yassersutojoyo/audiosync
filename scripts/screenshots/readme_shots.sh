#!/bin/bash
#
# Renders the README's Windows and Linux screenshots (main window and tray
# panel, light and dark) into docs/images with the readme_shots test in
# desktop/src/view.rs, playing the song below with docs/images/now-playing-cover.png.
# Runs on the Windows PC from Git Bash: the Windows shots build here, the Linux
# ones in WSL (Ubuntu-24.04, with the Rust toolchain). Needs Python with Pillow.
#
# Usage: scripts/screenshots/readme_shots.sh [windows|linux]   (default: both)
#
set -o errexit -o nounset -o pipefail

SONG='Sexy Magic|CA7RIEL & Paco Amoroso, PinkPantheress, Fred again.. & Étienne de Crécy'
repo="$(cd "$(dirname "$0")/../.." && pwd -P)"
work="$repo/.build/readme-shots"
mkdir -p "$work"

# The Windows build has no image decoder, so the test takes the cover as raw RGBA.
python -I -c '
import sys
from PIL import Image
cover = Image.open(sys.argv[1]).convert("RGBA").resize((640, 640), Image.LANCZOS)
open(sys.argv[2], "wb").write(cover.tobytes())
' "$(cygpath -w "$repo/docs/images/now-playing-cover.png")" "$(cygpath -w "$work/cover.rgba")"

for os in ${1:-windows linux}; do
  case "$os" in
    windows)
      (cd "$repo/desktop" && AUDIOSYNC_SHOT_DIR="$(cygpath -w "$work")" AUDIOSYNC_SHOT_TRACK="$SONG" \
        AUDIOSYNC_SHOT_COVER="$(cygpath -w "$work/cover.rgba")" \
        cargo test --bin audiosync readme_shots -- --ignored) ;;
    linux)
      # Git Bash's /c/... is WSL's /mnt/c/...
      wsl -d Ubuntu-24.04 -- bash -lc "cd $(printf %q "/mnt$repo/desktop") && \
        AUDIOSYNC_SHOT_DIR=$(printf %q "/mnt$work") AUDIOSYNC_SHOT_TRACK=$(printf %q "$SONG") \
        AUDIOSYNC_SHOT_COVER=$(printf %q "/mnt$work/cover.rgba") \
        CARGO_TARGET_DIR=\$HOME/audiosync-target-linux \
        cargo test --bin audiosync readme_shots -- --ignored" ;;
    *) echo "usage: $0 [windows|linux]" >&2; exit 2 ;;
  esac
  for name in main-window tray-panel; do
    for look in light dark; do
      python -I -c 'import sys; from PIL import Image; Image.open(sys.argv[1]).save(sys.argv[2], optimize=True)' \
        "$(cygpath -w "$work/$name-$os-$look.png")" "$(cygpath -w "$repo/docs/images/$name-$os-$look.png")"
    done
  done
done
