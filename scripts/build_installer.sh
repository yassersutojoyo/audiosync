#!/usr/bin/env bash
# Builds the Windows installer from a folder holding audiosync.exe and
# THIRD-PARTY-NOTICES.txt (the zip's two files), with Inno Setup 6's ISCC.
#   scripts/build_installer.sh x64 desktop/target/x86_64-pc-windows-msvc/release [out-dir]
# Writes AudioSync-Setup.exe (x64) or AudioSync-Setup-arm64.exe into out-dir,
# which defaults to the source folder.
set -euo pipefail

if [ $# -lt 2 ] || { [ "$1" != x64 ] && [ "$1" != arm64 ]; }; then
  echo "usage: $0 x64|arm64 <folder with audiosync.exe> [out-dir]" >&2
  exit 2
fi
arch=$1
src=$(cd "$2" && pwd)
out=${3:-$src}
mkdir -p "$out"
out=$(cd "$out" && pwd)
for file in audiosync.exe THIRD-PARTY-NOTICES.txt; do
  [ -f "$src/$file" ] || { echo "build_installer: $src has no $file" >&2; exit 1; }
done

iscc=$(command -v ISCC.exe || command -v iscc || true)
if [ -z "$iscc" ]; then
  for dir in "${LOCALAPPDATA:-}/Programs/Inno Setup 6" "C:/Program Files (x86)/Inno Setup 6" "C:/Program Files/Inno Setup 6"; do
    if [ -f "$dir/ISCC.exe" ]; then iscc="$dir/ISCC.exe"; break; fi
  done
fi
[ -n "$iscc" ] || { echo "build_installer: Inno Setup 6 (ISCC.exe) not found" >&2; exit 1; }

iss="$(cd "$(dirname "$0")/.." && pwd)/desktop/installer/audiosync.iss"
winpath() { if command -v cygpath >/dev/null; then cygpath -w "$1"; else printf '%s' "$1"; fi; }
# Git Bash would otherwise rewrite the /D and /O switches as paths.
MSYS2_ARG_CONV_EXCL="*" "$iscc" /Q "/DArch=$arch" "/DSourceDir=$(winpath "$src")" "/O$(winpath "$out")" "$(winpath "$iss")"

name=AudioSync-Setup.exe
[ "$arch" = arm64 ] && name=AudioSync-Setup-arm64.exe
[ -f "$out/$name" ] || { echo "build_installer: ISCC wrote no $name" >&2; exit 1; }
echo "$out/$name"
