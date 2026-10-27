#!/bin/bash
#
# Package the installed AudioSync.app into a disk image for a GitHub release
# (see RELEASE.md).
#
# Usage:  ./make-dmg.sh [--app PATH | PATH] [--out FILE] [--plain]
#
#   --app PATH  the app bundle to package; a lone PATH without --app means the
#               same (default: the AudioSync.app that build.sh installed: in
#               AUDIOSYNC_INSTALL_DIR when that is set, else in /Applications,
#               else in ~/Applications)
#   --out FILE  the image to write, ending in .dmg (default: AudioSync.dmg in
#               the repository root); an existing file there is replaced
#   --plain     skip the Finder window layout described below
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
# Window layout: the volume opens in a Finder window whose background
# (brand/dmg, drawn by scripts/brand/dmg_background.py) shows an arrow from the
# app to Applications and the one-time Open Anyway steps; the window size and
# icon positions come from brand/dmg/layout.json, so they match the art. The
# script writes a read-write image, has Finder lay out the window through
# AppleScript (which stores it in the volume's .DS_Store), then compresses it.
# Finder asks once to let the terminal control it. When the art is missing,
# another volume named AudioSync is mounted, or Finder can't be scripted, the
# image is made without the layout and the script says so at the end.
#
# CODESIGN_IDENTITY (optional): sign the image itself with this identity too,
# for a notarized release; a "Developer ID Application" identity also gets a
# secure timestamp, which notarization requires.
#
# macOS only (hdiutil, ditto, codesign, tiffutil, plutil, osascript).
#
set -o errexit -o nounset -o pipefail

ROOT="$(cd "$(dirname "$0")" && pwd)"
REPO_URL="https://github.com/yassersutojoyo/audiosync"
VOLUME_NAME="AudioSync"
ART="$ROOT/brand/dmg"
WORK=""
STAGE=""
MOUNT_POINT=""
MOUNTED=0
LAYOUT_MOUNT=""
LAYOUT_NOTE=""

die() { echo "❌ $*" >&2; exit 1; }

# Detach a mounted image. Finder or Spotlight can hold a fresh mount for a few
# seconds, so a busy volume gets plain retries first: -force pulls it from under
# them, and macOS then shows "Disk Not Ejected Properly".
detach() {
  local try=1
  until hdiutil detach -quiet "$1" 2>/dev/null; do
    if [ "$try" -ge 5 ]; then
      echo "⚠️  $1 is still busy after $try tries; detaching it with -force" >&2
      hdiutil detach -force -quiet "$1"
      return
    fi
    try=$((try + 1))
    sleep 2
  done
}

# Undo whatever is still in place when the script ends, after a failure too:
# the layout and check mounts, and the work folder with the staging folder and
# the read-write image.
cleanup() {
  if [ -n "$LAYOUT_MOUNT" ]; then
    detach "$LAYOUT_MOUNT" || true
  fi
  if [ "$MOUNTED" = 1 ]; then
    detach "$MOUNT_POINT" || true
  fi
  if [ -n "$MOUNT_POINT" ]; then
    rmdir "$MOUNT_POINT" 2>/dev/null || true
  fi
  if [ -n "$WORK" ]; then
    rm -rf "$WORK"
  fi
}
trap cleanup EXIT
# Ctrl-C or a kill still detaches what is mounted, through the EXIT trap.
trap 'exit 130' INT
trap 'exit 143' TERM
trap 'exit 129' HUP

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

# Write an image from the staging folder; the arguments after the image go to
# hdiutil create (the format and its options). hdiutil create now and then
# fails with "Resource busy" while the system still holds an earlier image; a
# short wait and another try get past it.
create_image() {
  local image="$1" attempt created=1
  shift
  for attempt in 1 2 3; do
    if hdiutil create -quiet -volname "$VOLUME_NAME" -srcfolder "$STAGE" -fs HFS+ "$@" -ov "$image"; then
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

# A value from brand/dmg/layout.json, by key path (icons.app.x).
layout_value() {
  plutil -extract "$1" raw -o - "$ART/layout.json"
}

# Put the window background into the staging folder as one HiDPI TIFF that
# holds the 1x and 2x art, so Finder picks by screen. On failure LAYOUT_NOTE
# says why.
stage_background() {
  local file
  for file in background.png background@2x.png layout.json; do
    if [ ! -f "$ART/$file" ]; then
      LAYOUT_NOTE="brand/dmg/$file is missing"
      return 1
    fi
  done
  mkdir -p "$STAGE/.background"
  if ! tiffutil -cathidpicheck "$ART/background.png" "$ART/background@2x.png" \
       -out "$STAGE/.background/background.tiff" >/dev/null 2>&1; then
    LAYOUT_NOTE="tiffutil could not combine the background images"
    return 1
  fi
}

# Have Finder lay out the mounted volume's window: icon view with no toolbar or
# status bar, the background, the icon size and the icon positions. The bounds
# get 28 points more than the art for the title bar; where Finder leaves that
# room below the art instead, it matches the art's white edge.
finder_layout() {
  local bundle="$1"
  osascript - "$VOLUME_NAME" "$(layout_value window.width)" "$(layout_value window.height)" \
    "$(layout_value iconSize)" "$(layout_value labelSize)" \
    "$bundle" "$(layout_value icons.app.x)" "$(layout_value icons.app.y)" \
    Applications "$(layout_value icons.applications.x)" "$(layout_value icons.applications.y)" \
    README.md "$(layout_value icons.readme.x)" "$(layout_value icons.readme.y)" <<'APPLESCRIPT'
on run argv
  set volumeName to item 1 of argv
  set windowWidth to (item 2 of argv) as integer
  set windowHeight to (item 3 of argv) as integer
  tell application "Finder"
    with timeout of 60 seconds
      tell disk volumeName
        open
        set current view of container window to icon view
        set toolbar visible of container window to false
        set statusbar visible of container window to false
        set bounds of container window to {200, 120, 200 + windowWidth, 120 + windowHeight + 28}
        set viewOptions to icon view options of container window
        set arrangement of viewOptions to not arranged
        set icon size of viewOptions to (item 4 of argv) as integer
        set text size of viewOptions to (item 5 of argv) as integer
        set background picture of viewOptions to file ".background:background.tiff"
        repeat with i from 6 to (count of argv) by 3
          set itemName to item i of argv
          set position of item itemName to {(item (i + 1) of argv) as integer, (item (i + 2) of argv) as integer}
        end repeat
        close
        open
        update without registering applications
        delay 2
        close
      end tell
    end timeout
  end tell
end run
APPLESCRIPT
}

# Write the image with the Finder window layout: a read-write image, mounted
# where Finder sees it and laid out, then compressed into IMAGE. That mount
# can't take -nobrowse: Finder only scripts volumes it shows. On failure
# LAYOUT_NOTE says why; a mount left behind stays in LAYOUT_MOUNT.
lay_out_image() {
  local image="$1" bundle="$2" rw="$WORK/layout.dmg" size attached
  if [ -e "/Volumes/$VOLUME_NAME" ]; then
    LAYOUT_NOTE="another volume named $VOLUME_NAME is mounted; eject it and run again"
    return 1
  fi
  # Room for the files plus what Finder and the file system add while mounted.
  size=$(( $(du -sm "$STAGE" | cut -f1) + 20 ))
  if ! create_image "$rw" -format UDRW -size "${size}m"; then
    LAYOUT_NOTE="hdiutil could not create the read-write image"
    return 1
  fi
  if ! attached="$(hdiutil attach -readwrite -noverify -noautoopen "$rw")"; then
    LAYOUT_NOTE="the read-write image does not mount"
    return 1
  fi
  LAYOUT_MOUNT="$(printf '%s\n' "$attached" | awk -F '\t' '$NF ~ /^\/Volumes\// { sub(/ +$/, "", $NF); print $NF }')"
  if [ -z "$LAYOUT_MOUNT" ]; then
    # No volume path to detach by, so detach the whole disk from the first line.
    detach "$(printf '%s\n' "$attached" | awk 'NR == 1 { print $1 }')" || true
    LAYOUT_NOTE="the read-write image mounted at an unknown place"
    return 1
  fi
  if [ "$LAYOUT_MOUNT" != "/Volumes/$VOLUME_NAME" ]; then
    LAYOUT_NOTE="the read-write image mounted at $LAYOUT_MOUNT"
    return 1
  fi
  if ! finder_layout "$bundle" >/dev/null; then
    LAYOUT_NOTE="Finder could not lay out the window; allow the terminal to control Finder in System Settings > Privacy & Security > Automation"
    return 1
  fi
  if [ ! -f "$LAYOUT_MOUNT/.DS_Store" ]; then
    LAYOUT_NOTE="Finder saved no layout (.DS_Store)"
    return 1
  fi
  rm -rf "$LAYOUT_MOUNT/.fseventsd"
  sync
  if ! detach "$LAYOUT_MOUNT"; then
    LAYOUT_NOTE="could not detach $LAYOUT_MOUNT"
    return 1
  fi
  LAYOUT_MOUNT=""
  if ! hdiutil convert -quiet "$rw" -format UDZO -imagekey zlib-level=9 -ov -o "$image"; then
    LAYOUT_NOTE="hdiutil could not compress the laid-out image"
    return 1
  fi
  rm -f "$rw"
}

# Mount the finished image read-only and check what a user will find in it.
check_image() {
  local image="$1" bundle="$2" laid_out="$3" link
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
  if [ "$laid_out" = 1 ]; then
    [ -s "$MOUNT_POINT/.background/background.tiff" ] || die "the window background is missing from the image"
    [ -s "$MOUNT_POINT/.DS_Store" ] || die "the window layout (.DS_Store) is missing from the image"
  fi
  detach "$MOUNT_POINT" || die "could not detach $MOUNT_POINT"
  MOUNTED=0
  rmdir "$MOUNT_POINT"
  MOUNT_POINT=""
}

main() {
  local app="" out="$ROOT/AudioSync.dmg" plain=0
  while [ $# -gt 0 ]; do
    case "$1" in
      --app) app="${2:?--app needs a path}"; shift 2 ;;
      --out) out="${2:?--out needs a file}"; shift 2 ;;
      --plain) plain=1; shift ;;
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
  WORK="$(mktemp -d "${TMPDIR:-/tmp}/audiosync-dmg.XXXXXX")"
  STAGE="$WORK/stage"
  mkdir "$STAGE"
  ditto "$app" "$STAGE/$bundle"
  ln -s /Applications "$STAGE/Applications"
  readme_for_image "$ROOT/README.md" > "$STAGE/README.md"
  if grep -nE "]\(${RELATIVE}[) ]|(src|href)=\"${RELATIVE}\"" "$STAGE/README.md" >&2; then
    echo "⚠️  README.md in the image still has the relative links above; they won't open from the image" >&2
  fi

  local laid_out=0
  if [ "$plain" = 1 ]; then
    LAYOUT_NOTE="--plain"
  elif stage_background && lay_out_image "$out" "$bundle"; then
    laid_out=1
  else
    if [ -n "$LAYOUT_MOUNT" ]; then
      detach "$LAYOUT_MOUNT" || true
      LAYOUT_MOUNT=""
    fi
    rm -rf "$STAGE/.background"
    echo "⚠️  Making the image without the Finder window layout: $LAYOUT_NOTE" >&2
  fi
  if [ "$laid_out" = 0 ]; then
    create_image "$out" -format UDZO -imagekey zlib-level=9 || die "hdiutil could not create $out"
  fi
  rm -rf "$WORK"
  WORK=""
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
  check_image "$out" "$bundle" "$laid_out"

  echo "✅ $out ($(du -h "$out" | cut -f1 | tr -d ' '))"
  echo "   SHA-256 $(shasum -a 256 "$out" | cut -d' ' -f1)"
  if [ "$laid_out" = 1 ]; then
    echo "   Finder window: background and icon positions from brand/dmg"
  else
    echo "⚠️  Finder window: plain, no background ($LAYOUT_NOTE)" >&2
  fi
  echo "Reminder: the release must also ship THIRD_PARTY_NOTICES.md; RELEASE.md has the steps."
}

main "$@"
