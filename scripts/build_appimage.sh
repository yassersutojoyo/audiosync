#!/usr/bin/env bash
# Packs a Linux build of the desktop app into an x86_64 AppImage.
#
#   scripts/build_appimage.sh <audiosync> <THIRD-PARTY-NOTICES.txt> <appimagetool> <runtime> <output>
#
# <appimagetool> and <runtime> are the pinned appimagetool and type2-runtime
# files (.github/workflows/linux.yml downloads and checks them); the runtime
# is passed in, so appimagetool downloads nothing. The AppDir is built next to
# <output> and left there for the checks that follow.
set -euo pipefail

if [ $# -ne 5 ]; then
  echo "usage: $0 <audiosync> <THIRD-PARTY-NOTICES.txt> <appimagetool> <runtime> <output>" >&2
  exit 2
fi
binary=$1 notices=$2 tool=$3 runtime=$4 output=$5
repo=$(cd "$(dirname "$0")/.." && pwd)
linux="$repo/desktop/linux"
id=io.github.yassersutojoyo.audiosync
appdir="${output%.AppImage}.AppDir"

rm -rf "$appdir"
mkdir -p "$appdir/usr/bin" "$appdir/usr/share/applications" \
  "$appdir/usr/share/icons/hicolor/256x256/apps" "$appdir/usr/share/metainfo" \
  "$appdir/usr/share/doc/audiosync"
install -m 755 "$binary" "$appdir/usr/bin/audiosync"
install -m 755 "$linux/AppRun" "$appdir/AppRun"
install -m 644 "$linux/$id.desktop" "$appdir/$id.desktop"
install -m 644 "$linux/$id.desktop" "$appdir/usr/share/applications/$id.desktop"
install -m 644 "$linux/$id.metainfo.xml" "$appdir/usr/share/metainfo/$id.metainfo.xml"
install -m 644 "$repo/desktop/res/audiosync-256.png" "$appdir/$id.png"
install -m 644 "$repo/desktop/res/audiosync-256.png" "$appdir/usr/share/icons/hicolor/256x256/apps/$id.png"
ln -s "$id.png" "$appdir/.DirIcon"
install -m 644 "$notices" "$appdir/usr/share/doc/audiosync/THIRD-PARTY-NOTICES.txt"
install -m 644 "$repo/LICENSE" "$appdir/usr/share/doc/audiosync/LICENSE"
install -m 644 "$linux/RUNTIME-NOTICES.txt" "$appdir/usr/share/doc/audiosync/RUNTIME-NOTICES.txt"

# appimagetool is an AppImage itself; extracting it needs no FUSE on the runner.
APPIMAGE_EXTRACT_AND_RUN=1 ARCH=x86_64 "$tool" --runtime-file "$runtime" "$appdir" "$output"
(cd "$(dirname "$output")" && sha256sum "$(basename "$output")" > "$(basename "$output").sha256")
echo "Wrote $output"
