#!/usr/bin/env bash
# Packs a Linux build of the desktop app into an .rpm for Fedora 40 or newer
# (glibc 2.39), with the files of the .deb and no scriptlets: Fedora's file
# triggers refresh the menu and icon caches.
#
#   scripts/build_rpm.sh <audiosync> <THIRD-PARTY-NOTICES.txt> <version> <output>
#
# rpmbuild packs the files as they are. Requires names Fedora's packages for
# the libraries the binary links, as the .deb's Depends does, instead of the
# sonames rpm would generate on the build host. The build tree is made next to
# <output> and removed.
set -euo pipefail

if [ $# -ne 4 ]; then
  echo "usage: $0 <audiosync> <THIRD-PARTY-NOTICES.txt> <version> <output>" >&2
  exit 2
fi
binary=$1 notices=$2 version=$3 output=$4
repo=$(cd "$(dirname "$0")/.." && pwd)
linux="$repo/desktop/linux"
id=io.github.yassersutojoyo.audiosync
top=$(cd "$(dirname "$output")" && pwd)/$(basename "${output%.rpm}").rpmtree

rm -rf "$top"
mkdir -p "$top/SOURCES" "$top/SPECS"
install -m 755 "$binary" "$top/SOURCES/audiosync"
install -m 644 "$linux/$id.desktop" "$linux/$id.metainfo.xml" "$repo/LICENSE" "$top/SOURCES/"
install -m 644 "$repo/desktop/res/audiosync-256.png" "$top/SOURCES/$id.png"
install -m 644 "$notices" "$top/SOURCES/THIRD-PARTY-NOTICES.txt"

# The binary ships as built: no strip, no debuginfo package, no build-id links.
cat > "$top/SPECS/audiosync.spec" <<EOF
%global debug_package %{nil}
%global __os_install_post %{nil}
%global _build_id_links none

Name: audiosync
Version: $version
Release: 1
Summary: Play your computer's sound on speakers
License: GPL-3.0-only
URL: https://github.com/yassersutojoyo/audiosync
AutoReqProv: no
Requires: glibc >= 2.39
Requires: fontconfig
Requires: openssl-libs
Requires: pipewire-libs

%description
AudioSync plays this computer's sound on AirPlay and Google Cast speakers
and on the outputs connected to it at the same time, with a delay for each
speaker to line them up.

AudioSync's own code is GPL-3.0-or-later; it compiles in Slint under
GPL-3.0-only, so this binary as a whole is GPL-3.0-only.

%install
s=%{_topdir}/SOURCES
install -D -m 755 \$s/audiosync %{buildroot}/usr/bin/audiosync
install -D -m 644 \$s/$id.desktop %{buildroot}/usr/share/applications/$id.desktop
install -D -m 644 \$s/$id.metainfo.xml %{buildroot}/usr/share/metainfo/$id.metainfo.xml
install -D -m 644 \$s/$id.png %{buildroot}/usr/share/icons/hicolor/256x256/apps/$id.png
install -D -m 644 \$s/THIRD-PARTY-NOTICES.txt %{buildroot}/usr/share/doc/audiosync/THIRD-PARTY-NOTICES.txt
install -D -m 644 \$s/LICENSE %{buildroot}/usr/share/doc/audiosync/LICENSE

%files
/usr/bin/audiosync
/usr/share/applications/$id.desktop
/usr/share/metainfo/$id.metainfo.xml
/usr/share/icons/hicolor/256x256/apps/$id.png
%dir /usr/share/doc/audiosync
%doc /usr/share/doc/audiosync/THIRD-PARTY-NOTICES.txt
%license /usr/share/doc/audiosync/LICENSE
EOF

# The same commit builds the same .rpm: the build time and every file's time
# stamp are the commit's.
SOURCE_DATE_EPOCH=${SOURCE_DATE_EPOCH:-$(git -C "$repo" log -1 --format=%ct)}
export SOURCE_DATE_EPOCH
rpmbuild -bb --quiet --define "_topdir $top" --define "_buildhost reproducible" \
  --define "use_source_date_epoch_as_buildtime 1" --define "clamp_mtime_to_source_date_epoch 1" \
  "$top/SPECS/audiosync.spec"
mv "$top"/RPMS/*/audiosync-*.rpm "$output"
rm -rf "$top"
(cd "$(dirname "$output")" && sha256sum "$(basename "$output")" > "$(basename "$output").sha256")
echo "Wrote $output"
