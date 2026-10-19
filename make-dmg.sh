#!/bin/bash
#
# Package the installed AudioSync.app into a disk image for a GitHub release
# (see RELEASE.md).
#
# Usage:  ./make-dmg.sh [--app PATH | PATH] [--out FILE]
#
#   --app PATH  the app bundle to package; a lone PATH without --app means the
#               same (default: the AudioSync.app that build.sh installed: in
#               AUDIOSYNC_INSTALL_DIR when that is set, else in /Applications,
#               else in ~/Applications)
#   --out FILE  the image to write, ending in .dmg (default: AudioSync.dmg in
#               the repository root); an existing file there is replaced
#
# The image is compressed and read-only (UDZO). It opens as a volume named
# "AudioSync" that holds the app, copied with its signature untouched, a link
# to /Applications, so installing is one drag, and README.md, so the install
# steps (including getting past Gatekeeper) are right there. That README is the
# repository's, with every relative link and image turned into an absolute
# GitHub URL, since the image holds no other repository files. The app must
# carry a valid code signature. After writing, the script mounts the image and
# checks that the app inside still verifies, the link points at /Applications
# and the README is there.
#
# CODESIGN_IDENTITY (optional): sign the image itself with this identity too,
# for a notarized release; a "Developer ID Application" identity also gets a
# secure timestamp, which notarization requires.
#
# macOS only (hdiutil, ditto, codesign).
#
set -o errexit -o nounset -o pipefail

ROOT="$(cd "$(dirname "$0")" && pwd)"
REPO_URL="https://github.com/yassersutojoyo/audiosync"
VOLUME_NAME="AudioSync"
STAGE=""
MOUNT_POINT=""
MOUNTED=0

die() { echo "❌ $*" >&2; exit 1; }

# Detach a mounted image. Spotlight or Finder can hold a fresh mount for a
# moment, so a failed detach is tried once more with -force.
detach() {
  hdiutil detach -quiet "$1" 2>/dev/null || { sleep 2; hdiutil detach -force -quiet "$1"; }
}

# Undo whatever is still in place when the script ends, after a failure too:
# the check mount and the staging folder.
cleanup() {
  if [ "$MOUNTED" = 1 ]; then
    detach "$MOUNT_POINT" || true
  fi
  if [ -n "$MOUNT_POINT" ]; then
    rmdir "$MOUNT_POINT" 2>/dev/null || true
  fi
  if [ -n "$STAGE" ]; then
    rm -rf "$STAGE"
  fi
}
trap cleanup EXIT

# A relative link target in README.md, as a regular expression (ERE): an
# optional "./" or "/" (\1), the path (\2) and an optional #anchor (\3).
# Absolute URLs never match (their scheme ends in a colon) and neither do
# in-page #anchors.
RELATIVE='(\.?/)?([A-Za-z0-9_.][^):#"<> ]*)(#[^)"<> ]*)?'

# Print the repository's README for the image: every relative Markdown link
# and image, every relative src or href in its HTML, and every relative
# reference definition becomes an absolute GitHub URL (raw/main for images,
# blob/main for everything else), since the image holds no other repository
# files. Images are rewritten first, so the link rule never sees them.
readme_for_image() {
  local blob="$REPO_URL/blob/main" raw="$REPO_URL/raw/main"
  sed -E \
    -e "s|!\[([^]]*)]\(${RELATIVE}([) ])|![\1]($raw/\3\4\5|g" \
    -e "s|]\(${RELATIVE}([) ])|]($blob/\2\3\4|g" \
    -e "s|src=\"${RELATIVE}\"|src=\"$raw/\2\3\"|g" \
    -e "s|href=\"${RELATIVE}\"|href=\"$blob/\2\3\"|g" \
    -e "s|^(\[[^]]+]:[[:space:]]*)${RELATIVE}[[:space:]]*\$|\1$blob/\3\4|" \
    "$1"
}

# The AudioSync.app build.sh installed, looked up where it installs; the
# status is nonzero when none of those places holds one.
installed_app() {
  local place found=1
  for place in "${AUDIOSYNC_INSTALL_DIR:-}" /Applications "$HOME/Applications"; do
    if [ -n "$place" ] && [ -d "$place/AudioSync.app" ]; then
      echo "$place/AudioSync.app"
      found=0
      break
    fi
  done
  return "$found"
}

# Write the image from the staging folder. hdiutil create now and then fails
# with "Resource busy" while the system still holds an earlier image; a short
# wait and another try get past it.
create_image() {
  local image="$1" attempt created=1
  for attempt in 1 2 3; do
    if hdiutil create -quiet -volname "$VOLUME_NAME" -srcfolder "$STAGE" -fs HFS+ \
         -format UDZO -imagekey zlib-level=9 -ov "$image"; then
      created=0
      break
    fi
    if [ "$attempt" != 3 ]; then
      echo "hdiutil create failed (attempt $attempt), trying again" >&2
      sleep 5
    fi
  done
  return "$created"
}

# Mount the finished image read-only and check what a user will find in it.
check_image() {
  local image="$1" bundle="$2" link
  MOUNT_POINT="$(mktemp -d "${TMPDIR:-/tmp}/audiosync-dmg-check.XXXXXX")"
  hdiutil attach -quiet -nobrowse -readonly -noautoopen -mountpoint "$MOUNT_POINT" "$image" \
    || die "the new image does not mount"
  MOUNTED=1
  [ -d "$MOUNT_POINT/$bundle" ] || die "$bundle is missing from the image"
  codesign --verify "$MOUNT_POINT/$bundle" || die "$bundle in the image fails signature verification"
  [ -L "$MOUNT_POINT/Applications" ] || die "the Applications link is missing from the image"
  link="$(readlink "$MOUNT_POINT/Applications")"
  [ "$link" = /Applications ] || die "the Applications link points at $link"
  [ -s "$MOUNT_POINT/README.md" ] || die "README.md is missing from the image"
  detach "$MOUNT_POINT" || die "could not detach $MOUNT_POINT"
  MOUNTED=0
  rmdir "$MOUNT_POINT"
  MOUNT_POINT=""
}

main() {
  local app="" out="$ROOT/AudioSync.dmg"
  while [ $# -gt 0 ]; do
    case "$1" in
      --app) app="${2:?--app needs a path}"; shift 2 ;;
      --out) out="${2:?--out needs a file}"; shift 2 ;;
      -h|--help) awk 'NR == 1 { next } /^#/ { print; next } { exit }' "$0"; exit 0 ;;
      -*) die "unknown option $1 (see --help)" ;;
      *)
        [ -z "$app" ] || die "more than one app given: $app and $1"
        app="$1"; shift ;;
    esac
  done
  [ "$(uname -s)" = Darwin ] || die "make-dmg.sh runs on macOS only"

  if [ -z "$app" ]; then
    app="$(installed_app)" || die "no installed AudioSync.app; run ./build.sh first, or pass --app PATH"
  fi
  app="${app%/}"
  [ -f "$app/Contents/Info.plist" ] || die "$app is not an app bundle"
  case "$out" in
    *.dmg) ;;
    *) die "--out must end in .dmg: $out" ;;
  esac
  mkdir -p "$(dirname "$out")"

  local bundle version signature
  bundle="$(basename "$app")"
  version="$(/usr/libexec/PlistBuddy -c 'Print :CFBundleShortVersionString' "$app/Contents/Info.plist" 2>/dev/null)" \
    || version="(no version)"
  codesign --verify "$app" || die "$app has no valid code signature; build it with ./build.sh"
  signature="$(codesign -dv "$app" 2>&1 || true)"
  case "$signature" in
    *Signature=adhoc*) echo "⚠️  $bundle is only ad-hoc signed: fine for a test, not for a release (RELEASE.md)" >&2 ;;
  esac

  echo "▶ Packaging $bundle $version from $app"
  STAGE="$(mktemp -d "${TMPDIR:-/tmp}/audiosync-dmg.XXXXXX")"
  ditto "$app" "$STAGE/$bundle"
  ln -s /Applications "$STAGE/Applications"
  readme_for_image "$ROOT/README.md" > "$STAGE/README.md"
  if grep -nE "]\(${RELATIVE}[) ]|(src|href)=\"${RELATIVE}\"" "$STAGE/README.md" >&2; then
    echo "⚠️  README.md in the image still has the relative links above; they won't open from the image" >&2
  fi
  create_image "$out" || die "hdiutil could not create $out"
  rm -rf "$STAGE"
  STAGE=""

  if [[ -n ${CODESIGN_IDENTITY:-} ]]; then
    local sign=(--force --sign "$CODESIGN_IDENTITY")
    case "$CODESIGN_IDENTITY" in
      "Developer ID Application"*) sign+=(--timestamp) ;;
    esac
    codesign "${sign[@]}" "$out" || die "could not sign $out with $CODESIGN_IDENTITY"
    codesign --verify "$out" || die "the signature on $out does not verify"
  fi

  hdiutil verify -quiet "$out" || die "$out fails hdiutil verify"
  check_image "$out" "$bundle"

  echo "✅ $out ($(du -h "$out" | cut -f1 | tr -d ' '))"
  echo "   SHA-256 $(shasum -a 256 "$out" | cut -d' ' -f1)"
  echo "Reminder: the release must also ship THIRD_PARTY_NOTICES.md; RELEASE.md has the steps."
}

main "$@"
