#!/usr/bin/env bash
# Packs an x64 release build of the Windows app into an unsigned MSIX for the
# Microsoft Store, from desktop/msix/ (manifest and logos):
#
#   scripts/make_msix.sh <audiosync.exe> <THIRD-PARTY-NOTICES.txt>
#
# Writes dist/msix/AudioSync-<version>-x64.msix (dist/ is ignored) and leaves the
# staged layout in dist/msix/stage. Runs under Git Bash; MakeAppx comes from
# $MAKEAPPX or the newest Windows 10/11 SDK installed.
set -euo pipefail

if [ $# -ne 2 ]; then
  echo "usage: $0 <audiosync.exe> <THIRD-PARTY-NOTICES.txt>" >&2
  exit 2
fi
exe=$1 notices=$2
repo=$(cd "$(dirname "$0")/.." && pwd)

version=$(sed -n 's/^version = "\([0-9]*\.[0-9]*\.[0-9]*\)"$/\1/p' "$repo/desktop/Cargo.toml" | head -n 1)
[ -n "$version" ] || { echo "no X.Y.Z version in desktop/Cargo.toml" >&2; exit 1; }

makeappx=${MAKEAPPX:-$(find "/c/Program Files (x86)/Windows Kits/10/bin" -ipath '*/x64/makeappx.exe' 2>/dev/null | sort -V | tail -n 1)}
[ -n "$makeappx" ] || { echo "makeappx.exe not found; install a Windows 10/11 SDK" >&2; exit 1; }

out="$repo/dist/msix"
stage="$out/stage"
package="$out/AudioSync-$version-x64.msix"
rm -rf "$stage" "$package"
mkdir -p "$stage/Assets"

sed "s/@VERSION@/$version.0/" "$repo/desktop/msix/AppxManifest.xml" > "$stage/AppxManifest.xml"
cp "$repo"/desktop/msix/Assets/*.png "$stage/Assets/"
cp "$exe" "$stage/audiosync.exe"
cp "$notices" "$stage/THIRD-PARTY-NOTICES.txt"

# MSYS would rewrite MakeAppx's /d and /p switches as drive paths.
MSYS2_ARG_CONV_EXCL='*' "$makeappx" pack /o /d "$(cygpath -w "$stage")" /p "$(cygpath -w "$package")"
echo "Unsigned package: $package"
