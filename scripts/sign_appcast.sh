#!/bin/bash
#
# Signs a release zip for Sparkle and writes the appcast.xml that goes on the
# GitHub release next to it (RELEASE.md). The app's SUFeedURL points at
# releases/latest/download/appcast.xml, so each release carries its own.
#
#   scripts/sign_appcast.sh VERSION [ZIP]
#
# ZIP defaults to AudioSync-VERSION.zip in the repository root, and
# appcast.xml lands there too. Sparkle's generate_appcast signs the zip with
# the EdDSA private key in this Mac's login keychain (made once with Sparkle's
# generate_keys); the key never leaves the keychain.
#
# It runs generate_appcast as a copy named "AudioSync Release Signing", so the
# keychain prompt should name AudioSync (not yet confirmed on a Mac). The copy
# keeps Sparkle's signature; docs/DEVELOPMENT.md ("Signing") says why, and why
# to click Allow rather than Always Allow.
#
# When AudioSync-VERSION-windows-x64.zip and -arm64.zip sit next to ZIP, each
# gets its own item, signed by Sparkle's sign_update (run under the same name)
# with the same key, and scripts/check_appcast.py checks the result.
# covers: S311
#
# Environment:
#   SPARKLE_BIN   the folder holding generate_appcast, when it isn't in the
#                 Sparkle package that swift build fetched into .build/artifacts
#
set -o errexit -o nounset -o pipefail

caller_dir=$PWD
cd -- "$(dirname -- "$0")/.."
ROOT=$(pwd)
REPO_URL=https://github.com/yassersutojoyo/audiosync

die() { echo "❌ $*" >&2; exit 1; }
absolute() {
  case "$1" in /*) printf '%s\n' "$1" ;; *) printf '%s\n' "$caller_dir/$1" ;; esac
}

usage="usage: scripts/sign_appcast.sh VERSION [ZIP]"
case "${1:-}" in -h|--help) echo "$usage"; exit 0 ;; esac
[ $# -ge 1 ] && [ $# -le 2 ] || die "$usage"
[ "$(uname -s)" = Darwin ] || die "Sparkle's tools run on macOS only"
version=$1
if [ $# = 2 ]; then zip=$(absolute "$2"); else zip="$ROOT/AudioSync-$version.zip"; fi
[ -f "$zip" ] || die "$zip not found (RELEASE.md makes it with ditto)"

if [ -n "${SPARKLE_BIN:-}" ]; then
  tool="$(absolute "$SPARKLE_BIN")/generate_appcast"
else
  tool=$(find "$ROOT/.build/artifacts" -type f -path '*/bin/generate_appcast' 2>/dev/null | head -n 1) || tool=""
fi
[ -n "$tool" ] && [ -x "$tool" ] \
  || die "generate_appcast not found: run swift build first, or set SPARKLE_BIN to the bin folder of the Sparkle release"
sign_tool="$(dirname -- "$tool")/sign_update"
[ -x "$sign_tool" ] || die "sign_update not found next to $tool"
python3 -I -c 'pass' 2>/dev/null || die "python3 not found: install the Command Line Tools"

# Both Windows zips or neither: Windows users of the missing arch would
# otherwise never see this release.
zip_dir=$(dirname -- "$zip")
x64_zip="$zip_dir/AudioSync-$version-windows-x64.zip"
arm64_zip="$zip_dir/AudioSync-$version-windows-arm64.zip"
python3 -I scripts/check_appcast.py --strays "$zip_dir" "$version" \
  || die "a Windows zip next to $zip has another name (RELEASE.md names them)"
windows=no
if [ -f "$x64_zip" ] && [ -f "$arm64_zip" ]; then
  windows=yes
elif [ -f "$x64_zip" ] || [ -f "$arm64_zip" ]; then
  die "only one Windows zip next to $zip: attach both, or neither"
fi

mkdir -p "$ROOT/.build"
work=$(mktemp -d "$ROOT/.build/appcast.XXXXXX")
trap 'rm -rf -- "$work"' EXIT

# A build without the public key or the release flag has updates off, and
# users who install it would never get the next update: refuse it here.
ditto -x -k "$zip" "$work/app"
plist=$(find "$work/app" -maxdepth 3 -path '*.app/Contents/Info.plist' | head -n 1)
[ -n "$plist" ] || die "$zip holds no app bundle"
zip_version=$(plutil -extract CFBundleShortVersionString raw -o - "$plist")
[ "$zip_version" = "$version" ] || die "$zip holds version $zip_version, not $version"
key=$(plutil -extract SUPublicEDKey raw -o - "$plist" 2>/dev/null) || key=""
[ -n "${key//[[:space:]]/}" ] \
  || die "the app in $zip has no SUPublicEDKey, so it would never check for updates: put the public key from generate_keys in Info.plist and build again"
flag=$(plutil -extract AudioSyncReleaseBuild raw -o - "$plist" 2>/dev/null) || flag=""
[ "$flag" = true ] \
  || die "the app in $zip isn't a release build, so it would never check for updates: build it with ./build.sh --release"

mkdir "$work/archives" "$work/bin"
cp "$zip" "$work/archives/"
signer="$work/bin/AudioSync Release Signing"
cp -p "$tool" "$signer"
"$signer" --download-url-prefix "$REPO_URL/releases/download/v$version/" "$work/archives"
[ -f "$work/archives/appcast.xml" ] || die "generate_appcast wrote no appcast.xml"
grep -q 'sparkle:edSignature=' "$work/archives/appcast.xml" || die "appcast.xml carries no EdDSA signature"

if [ "$windows" = yes ]; then
  mkdir "$work/bin/sign"
  zip_signer="$work/bin/sign/AudioSync Release Signing"
  cp -p "$sign_tool" "$zip_signer"
  signature_of() {
    local signed
    signed=$("$zip_signer" "$1") || die "sign_update failed on $1"
    printf '%s\n' "$signed" | sed -n 's/.*sparkle:edSignature="\([^"]*\)".*/\1/p'
  }
  x64_signature=$(signature_of "$x64_zip")
  arm64_signature=$(signature_of "$arm64_zip")
  python3 -I scripts/check_appcast.py "$work/archives/appcast.xml" "$version" "$zip" "$x64_zip" "$arm64_zip" \
    --add "$x64_zip" "$x64_signature" --add "$arm64_zip" "$arm64_signature" \
    || die "appcast.xml with the Windows items fails the S311 checks above"
  signed_for="$(basename "$zip") and both Windows zips"
else
  python3 -I scripts/check_appcast.py "$work/archives/appcast.xml" "$version" "$zip" \
    || die "appcast.xml fails the S311 checks above"
  signed_for="$(basename "$zip") only (no Windows zips next to it)"
fi
cp "$work/archives/appcast.xml" "$ROOT/appcast.xml"

echo "✅ $ROOT/appcast.xml signed for $signed_for: attach it to the v$version release next to the zips"
