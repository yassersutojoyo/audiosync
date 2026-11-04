#!/usr/bin/env bash
# Packs a Linux build of the desktop app into a .deb for the machine it runs on
# (amd64 or arm64), with the same files as the AppImage and no maintainer
# scripts: dpkg's triggers refresh the menu and icon caches.
#
#   scripts/build_deb.sh <audiosync> <THIRD-PARTY-NOTICES.txt> <version> <output>
#
# Libraries come from the system, as for the AppImage; Depends names the ones
# the binary links. The package root is built next to <output> and removed.
set -euo pipefail

if [ $# -ne 4 ]; then
  echo "usage: $0 <audiosync> <THIRD-PARTY-NOTICES.txt> <version> <output>" >&2
  exit 2
fi
binary=$1 notices=$2 version=$3 output=$4
repo=$(cd "$(dirname "$0")/.." && pwd)
linux="$repo/desktop/linux"
id=io.github.yassersutojoyo.audiosync
root="${output%.deb}.debroot"

rm -rf "$root"
mkdir -p "$root/DEBIAN" "$root/usr/bin" "$root/usr/share/applications" \
  "$root/usr/share/icons/hicolor/256x256/apps" "$root/usr/share/metainfo" \
  "$root/usr/share/doc/audiosync"
install -m 755 "$binary" "$root/usr/bin/audiosync"
install -m 644 "$linux/$id.desktop" "$root/usr/share/applications/$id.desktop"
install -m 644 "$linux/$id.metainfo.xml" "$root/usr/share/metainfo/$id.metainfo.xml"
install -m 644 "$repo/desktop/res/audiosync-256.png" "$root/usr/share/icons/hicolor/256x256/apps/$id.png"
install -m 644 "$notices" "$root/usr/share/doc/audiosync/THIRD-PARTY-NOTICES.txt"
{
  echo "Format: https://www.debian.org/doc/packaging-manuals/copyright-format/1.0/"
  echo "Upstream-Name: AudioSync"
  echo "Source: https://github.com/yassersutojoyo/audiosync"
  echo
  echo "Files: *"
  echo "Copyright: yassersutojoyo"
  echo "License: GPL-3.0-or-later"
  echo " The full text is in /usr/share/common-licenses/GPL-3. Third-party code"
  echo " compiled into audiosync is listed in THIRD-PARTY-NOTICES.txt here."
} > "$root/usr/share/doc/audiosync/copyright"
chmod 644 "$root/usr/share/doc/audiosync/copyright"

cat > "$root/DEBIAN/control" <<EOF
Package: audiosync
Version: $version
Architecture: $(dpkg --print-architecture)
Maintainer: yassersutojoyo <https://github.com/yassersutojoyo/audiosync/issues>
Installed-Size: $(du -sk --exclude=DEBIAN "$root" | cut -f1)
Depends: libc6 (>= 2.39), libfontconfig1, libssl3t64 | libssl3, libpipewire-0.3-0t64 | libpipewire-0.3-0
Section: sound
Priority: optional
Homepage: https://github.com/yassersutojoyo/audiosync
Description: Play your computer's sound on speakers
 AudioSync plays this computer's sound on AirPlay and Google Cast speakers
 and on the outputs connected to it at the same time, with a delay for each
 speaker to line them up.
EOF

dpkg-deb --root-owner-group --build "$root" "$output"
rm -rf "$root"
(cd "$(dirname "$output")" && sha256sum "$(basename "$output")" > "$(basename "$output").sha256")
echo "Wrote $output"
