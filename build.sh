#!/bin/bash
#
# Builds AudioSync.app from this checkout, signs it and installs it.
#
#   ./build.sh               build, sign, quit the running AudioSync, install
#   ./build.sh --no-install  build and sign only: nothing is quit or installed,
#                            and the app stays in .build/app/AudioSync.app
#
# Steps:
#   1) the Rust audio core (scripts/build_core.sh)
#   2) Sparkle, the update framework: the pinned archive (SHA-256 checked,
#      cached in ~/Library/Caches/<bundle id>), or SPARKLE_DIR. Without it
#      (offline, checksum mismatch) the app is built without update support.
#   3) the app binary (swift build -c release), linked with the core and Sparkle
#   4) a fresh AudioSync.app: Info.plist, the binary, the icon, the font, the
#      notices and license files, Sparkle.framework
#   5) signing, inside out: Sparkle's helpers, Sparkle, the app
#   6) quitting the running AudioSync and installing to /Applications, or
#      ~/Applications when /Applications isn't writable
#
# Signing: with CODESIGN_IDENTITY when it's set (a "Developer ID Application: …"
# name also gets the Hardened Runtime, a secure timestamp and
# AudioSync.entitlements); otherwise with the local identity make-local-cert.sh
# creates on the first build. Its keychain joins the keychain search list while
# signing and leaves it afterwards. If that identity exists but can't sign, the
# build stops before anything is quit or installed. Ad-hoc only with
# AUDIOSYNC_ADHOC=1, or when there's no local identity and none can be created:
# an ad-hoc signature changes with every build, so macOS stops honoring the
# System Audio Recording permission (capture goes silent).
#
# Environment:
#   AUDIOSYNC_INSTALL_DIR         install there instead of /Applications
#   AUDIOSYNC_ADHOC=1             sign ad-hoc
#   CODESIGN_IDENTITY             the signing identity, by its full name
#   ENTITLEMENTS                  entitlements for the app's signature
#   SPARKLE_DIR                   an already verified folder holding
#                                 Sparkle.framework and Sparkle's LICENSE
#
set -o errexit -o nounset -o pipefail

caller_dir=$PWD
cd -- "$(dirname -- "$0")"
ROOT=$(pwd)

# The Sparkle release the app ships, and the SHA-256 of its .tar.xz archive
# (pinned for supply-chain integrity). A new release needs a new checksum:
# shasum -a 256 Sparkle-<release>.tar.xz
SPARKLE_RELEASE=2.9.5
SPARKLE_ARCHIVE_SHA256=015336b601493e05c237964954bff6191370003d94edefe663724c88840d73cc
SPARKLE_URL="https://github.com/sparkle-project/Sparkle/releases/download/$SPARKLE_RELEASE/Sparkle-$SPARKLE_RELEASE.tar.xz"
LOCAL_IDENTITY="AudioSync Local Code Signing"
LOCAL_KEYCHAIN="$HOME/Library/Keychains/audiosync-signing.keychain-db"
LAUNCH_SERVICES_TOOL=/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister
CORE_OUT="$ROOT/core/target/universal-apple-darwin/release"
STAGE="$ROOT/.build/app"
APP="$STAGE/AudioSync.app"

die() { echo "❌ $*" >&2; exit 1; }
say() { printf '\n\033[1;36m▶ %s\033[0m\n' "$1"; }
warn() { echo "   ⚠️  $*" >&2; }
# absolute PATH: a path the caller gave, relative to the folder they ran this from.
absolute() {
  case "$1" in /*) printf '%s\n' "$1" ;; *) printf '%s\n' "$caller_dir/$1" ;; esac
}
sha256_of() { shasum -a 256 "$1" | awk '{ print $1 }'; }
plist_value() { plutil -extract "$1" raw -o - "$ROOT/Info.plist" 2>/dev/null; }

install=1
for arg in "$@"; do
  case "$arg" in
    --no-install) install=0 ;;
    -h|--help) awk 'NR > 2 && /^#/ { sub(/^# ?/, ""); print; next } NR > 2 { exit }' "$0"; exit 0 ;;
    *) die "unknown option $arg (./build.sh --help lists the options)" ;;
  esac
done

[ "$(uname -s)" = Darwin ] || die "AudioSync.app builds on macOS only"
command -v swift >/dev/null || die "swift not found: install the Xcode Command Line Tools (xcode-select --install)"

EXECUTABLE=$(plist_value CFBundleExecutable) || die "Info.plist has no CFBundleExecutable"
BUNDLE_ID=$(plist_value CFBundleIdentifier) || die "Info.plist has no CFBundleIdentifier"
MIN_MACOS=$(plist_value LSMinimumSystemVersion) || die "Info.plist has no LSMinimumSystemVersion"
VERSION=$(plist_value CFBundleShortVersionString) || VERSION="?"
# The app's macOS floor, for anything built here that honors it.
export MACOSX_DEPLOYMENT_TARGET=$MIN_MACOS

# ---------------------------------------------------------------------------
say "1/6  Rust audio core"
scripts/build_core.sh

# ---------------------------------------------------------------------------
say "2/6  Sparkle $SPARKLE_RELEASE"
sparkle_dir=""   # the folder holding Sparkle.framework, empty to build without it

# The caller's own Sparkle (SPARKLE_DIR), taken as it is.
use_given_sparkle() {
  local given
  given=$(absolute "$SPARKLE_DIR")
  case "$given" in */Sparkle.framework|*/Sparkle.framework/) given=$(dirname "$given") ;; esac
  [ -d "$given/Sparkle.framework" ] || die "SPARKLE_DIR=$SPARKLE_DIR holds no Sparkle.framework"
  [ -f "$given/LICENSE" ] || die "SPARKLE_DIR=$SPARKLE_DIR has no LICENSE next to Sparkle.framework (the app ships Sparkle's license)"
  echo "   Using $given (SPARKLE_DIR: no checksum check)"
  sparkle_dir=$given
}

# unpack_sparkle ARCHIVE: unpacked once per archive, so the framework's files
# keep their dates and an unchanged Sparkle doesn't make SwiftPM recompile.
unpack_sparkle() {
  local unpacked="$ROOT/.build/sparkle/$SPARKLE_RELEASE"
  if [ "$(cat "$unpacked/.sha256" 2>/dev/null)" != "$SPARKLE_ARCHIVE_SHA256" ]; then
    rm -rf "$unpacked"
    mkdir -p "$unpacked"
    tar -xf "$1" -C "$unpacked"
    echo "$SPARKLE_ARCHIVE_SHA256" > "$unpacked/.sha256"
  fi
  [ -d "$unpacked/Sparkle.framework" ] || die "the Sparkle archive has no Sparkle.framework ($unpacked)"
  [ -f "$unpacked/LICENSE" ] || die "the Sparkle archive has no LICENSE ($unpacked)"
  sparkle_dir=$unpacked
}

# The pinned archive, from the cache or downloaded and checked. A download
# that fails or doesn't match the checksum leaves sparkle_dir empty: the app
# is then built without update support.
fetch_sparkle() {
  local cache="$HOME/Library/Caches/$BUNDLE_ID"
  local archive="$cache/Sparkle-$SPARKLE_RELEASE.tar.xz"
  local partial="$archive.part" problem=""
  if [ -f "$archive" ] && [ "$(sha256_of "$archive")" = "$SPARKLE_ARCHIVE_SHA256" ]; then
    echo "   ✓ Using the cached archive ($archive)"
  else
    mkdir -p "$cache"
    rm -f "$partial"
    if ! curl -fsSL --retry 2 -o "$partial" "$SPARKLE_URL"; then
      problem="Couldn't download Sparkle"
    elif [ "$(sha256_of "$partial")" != "$SPARKLE_ARCHIVE_SHA256" ]; then
      problem="The Sparkle download doesn't match its pinned SHA-256"
    else
      mv -f "$partial" "$archive"
      echo "   ✓ Downloaded and checked $(basename "$archive")"
    fi
  fi
  if [ -n "$problem" ]; then
    rm -f "$partial"
    warn "$problem: building without update support"
  else
    unpack_sparkle "$archive"
  fi
}

if [ -n "${SPARKLE_DIR:-}" ]; then use_given_sparkle; else fetch_sparkle; fi

# ---------------------------------------------------------------------------
say "3/6  App (swift build -c release)"
swift_args=(-c release)
if [ -n "$sparkle_dir" ]; then
  # Sparkle's module map makes `import Sparkle` work and asks the linker for
  # the framework; both need to know where it is.
  swift_args+=(-Xswiftc "-F$sparkle_dir" -Xlinker "-F$sparkle_dir")
fi
bin_dir=$(swift build "${swift_args[@]}" --show-bin-path)
binary="$bin_dir/$EXECUTABLE"
# SwiftPM doesn't see the core library as an input: when build_core.sh
# replaced it, remove the linked binary so the build links it again.
if [ -f "$binary" ] && [ "$CORE_OUT/libaudiosync_core.a" -nt "$binary" ]; then
  rm -f "$binary"
fi
swift build "${swift_args[@]}"
[ -f "$binary" ] || die "swift build left no $binary"
nm -gU "$binary" | grep ' T _asc_version$' >/dev/null \
  || die "$binary doesn't contain the audio core (no _asc_version): run scripts/build_core.sh and build again"

# ---------------------------------------------------------------------------
say "4/6  Assembling AudioSync.app"
contents="$APP/Contents"
resources="$contents/Resources"
third_party="$resources/ThirdParty"
rm -rf -- "$APP"
mkdir -p "$contents/MacOS" "$third_party/licenses"
cp Info.plist "$contents/Info.plist"
printf 'APPL????' > "$contents/PkgInfo"
cp "$binary" "$contents/MacOS/$EXECUTABLE"
chmod 755 "$contents/MacOS/$EXECUTABLE"

icon=$(plist_value CFBundleIconFile) || icon=AppIcon
cp AppIcon.icns "$resources/${icon%.icns}.icns"
# The wordmark's typeface, registered through ATSApplicationFontsPath.
fonts=$(plist_value ATSApplicationFontsPath) || fonts=Fonts
mkdir -p "$resources/$fonts"
cp brand/fonts/Poppins-Medium.ttf "$resources/$fonts/"

# AudioSync's own license, the notices, and the license of everything the app
# carries.
cp LICENSE "$resources/LICENSE"
cp THIRD_PARTY_NOTICES.md "$third_party/"
mkdir -p "$third_party/licenses/poppins"
cp brand/fonts/OFL.txt "$third_party/licenses/poppins/OFL.txt"
[ -d "$CORE_OUT/licenses/rust" ] || die "$CORE_OUT/licenses/rust is missing: run scripts/build_core.sh"
cp -R "$CORE_OUT/licenses/rust" "$third_party/licenses/rust"

if [ -n "$sparkle_dir" ]; then
  mkdir -p "$contents/Frameworks" "$third_party/licenses/sparkle"
  ditto "$sparkle_dir/Sparkle.framework" "$contents/Frameworks/Sparkle.framework"
  cp "$sparkle_dir/LICENSE" "$third_party/licenses/sparkle/LICENSE"
  # Sparkle's install name is @rpath/…: the app finds it in Contents/Frameworks.
  if ! otool -l "$contents/MacOS/$EXECUTABLE" | grep 'path @executable_path/\.\./Frameworks ' >/dev/null; then
    # (It warns that the linker's signature becomes invalid; the app is signed below.)
    if ! out=$(install_name_tool -add_rpath @executable_path/../Frameworks "$contents/MacOS/$EXECUTABLE" 2>&1); then
      echo "$out" >&2
      die "Couldn't add the Frameworks rpath to $contents/MacOS/$EXECUTABLE"
    fi
  fi
  echo "   Sparkle.framework embedded"
else
  warn "No Sparkle: this build has no update support"
fi

# ---------------------------------------------------------------------------
say "5/6  Signing"
adhoc_warning() {   # adhoc_warning REASON
  echo >&2
  echo "   ⚠️  ⚠️  ⚠️  SIGNING AD-HOC: $1" >&2
  echo "   An ad-hoc signature changes with every build. macOS then stops honoring the" >&2
  echo "   System Audio Recording permission for this app: the toggle stays on, but" >&2
  echo "   capture is silent until you grant it again. Don't ship or keep this build." >&2
  echo >&2
}

# The local identity's keychain has to be in the user's keychain search list
# while codesign uses it. It's added for the signing and the list is put back
# the way it was afterwards, also when the build stops.
saved_keychains=()
keychains_changed=0
restore_keychains() {
  if [ "$keychains_changed" = 1 ]; then
    security list-keychains -d user -s ${saved_keychains[@]+"${saved_keychains[@]}"} \
      || warn "Couldn't restore the keychain search list (security list-keychains -d user shows it)"
    keychains_changed=0
  fi
}
add_local_keychain() {
  local line k
  while IFS= read -r line; do
    k=$(printf '%s\n' "$line" | sed -e 's/^[[:space:]]*"//' -e 's/"[[:space:]]*$//')
    if [ -n "$k" ]; then saved_keychains+=("$k"); fi
  done < <(security list-keychains -d user)
  for k in ${saved_keychains[@]+"${saved_keychains[@]}"}; do
    if [ "$k" = "$LOCAL_KEYCHAIN" ]; then return 0; fi
  done
  keychains_changed=1
  security list-keychains -d user -s "$LOCAL_KEYCHAIN" ${saved_keychains[@]+"${saved_keychains[@]}"}
}
trap restore_keychains EXIT
trap 'exit 130' INT TERM

identity=""     # what codesign signs with: a name, a SHA-1 hash, or - (ad-hoc)
keychain=""     # the keychain holding the local identity
hardened=0      # Developer ID: Hardened Runtime and a secure timestamp
entitlements=""
if [[ -n ${CODESIGN_IDENTITY:-} ]]; then
  identity=$CODESIGN_IDENTITY
  case "$identity" in
    "Developer ID Application"*) hardened=1 ;;
  esac
  label=$identity
elif [ "${AUDIOSYNC_ADHOC:-0}" = 1 ]; then
  identity=-
  label="ad-hoc (AUDIOSYNC_ADHOC=1)"
  adhoc_warning "you asked for it with AUDIOSYNC_ADHOC=1"
elif "$ROOT/make-local-cert.sh"; then
  local_hash=$(security find-identity -p codesigning "$LOCAL_KEYCHAIN" 2>/dev/null \
    | awk -v n="\"$LOCAL_IDENTITY\"" 'index($0, n) && !found { print $2; found = 1 }') || local_hash=""
  [ -n "$local_hash" ] || die "make-local-cert.sh finished, but $LOCAL_KEYCHAIN holds no \"$LOCAL_IDENTITY\". Nothing was quit or installed"
  identity=$local_hash
  keychain=$LOCAL_KEYCHAIN
  label="\"$LOCAL_IDENTITY\" ($local_hash)"
elif [ -e "$LOCAL_KEYCHAIN" ]; then
  # An identity that exists but can't be used must not turn into an ad-hoc
  # app that replaces the installed one and loses its permissions.
  die "The local signing identity in $LOCAL_KEYCHAIN can't be used (see make-local-cert.sh above). Nothing was quit or installed"
else
  identity=-
  label="ad-hoc (no local identity could be created)"
  adhoc_warning "make-local-cert.sh couldn't create the local identity (see above)"
fi
if [ -n "${ENTITLEMENTS:-}" ]; then
  entitlements=$(absolute "$ENTITLEMENTS")
  [ -f "$entitlements" ] || die "ENTITLEMENTS=$ENTITLEMENTS not found"
elif [ "$hardened" = 1 ]; then
  entitlements="$ROOT/AudioSync.entitlements"
fi

# sign [codesign options…] PATH
sign() {
  local args=(--force --sign "$identity") out
  if [ -n "$keychain" ]; then args+=(--keychain "$keychain"); fi
  if [ "$hardened" = 1 ]; then
    args+=(--options runtime --timestamp)
  else
    args+=(--timestamp=none)
  fi
  # codesign's own words reach the log only when it fails.
  out=$(codesign "${args[@]}" "$@" 2>&1) || { echo "$out" >&2; return 1; }
}

# Inside out: every piece of code is signed before the bundle that holds it.
sign_all() {
  local f version_dir app_args=()
  if [ -d "$contents/Frameworks/Sparkle.framework" ]; then
    version_dir=$(cd "$contents/Frameworks/Sparkle.framework/Versions/Current" && pwd -P)
    # Sparkle's own entitlements stay (its downloader service needs them).
    for f in "$version_dir"/XPCServices/*.xpc "$version_dir/Autoupdate" "$version_dir/Updater.app"; do
      if [ -e "$f" ]; then sign --preserve-metadata=entitlements "$f" || return 1; fi
    done
    sign "$contents/Frameworks/Sparkle.framework" || return 1
  fi
  if [ -n "$entitlements" ]; then app_args=(--entitlements "$entitlements"); fi
  sign ${app_args[@]+"${app_args[@]}"} "$APP" || return 1
}

# Extended attributes (Finder info, quarantine) aren't allowed in a signed bundle.
xattr -cr "$APP"
if [ -n "$keychain" ]; then add_local_keychain; fi
# No fallback: an app signed differently would lose its privacy permissions.
sign_all || die "Signing with $label failed. Nothing was quit or installed"
codesign --verify --deep --strict "$APP" || die "The signature of $APP doesn't verify. Nothing was quit or installed"
restore_keychains
echo "   Signed with $label"
if [ "$hardened" = 1 ]; then echo "   Hardened Runtime, secure timestamp, entitlements: $(basename "$entitlements")"; fi

# ---------------------------------------------------------------------------
if [ "$install" = 0 ]; then
  echo
  echo "✅ AudioSync $VERSION built: $APP (--no-install: nothing quit or installed)"
  exit 0
fi

say "6/6  Installing"
if [ -n "${AUDIOSYNC_INSTALL_DIR:-}" ]; then
  dest_dir=$(absolute "$AUDIOSYNC_INSTALL_DIR")
elif [ -w /Applications ]; then
  dest_dir=/Applications
else
  dest_dir="$HOME/Applications"
fi
mkdir -p "$dest_dir"
dest="$dest_dir/AudioSync.app"

# The app quits cleanly on SIGTERM.
quit_running() {
  pgrep -x "$EXECUTABLE" >/dev/null || return 0
  echo "   Quitting the running AudioSync…"
  pkill -TERM -x "$EXECUTABLE" || true
  for _ in $(seq 1 100); do
    pgrep -x "$EXECUTABLE" >/dev/null || return 0
    sleep 0.1
  done
  warn "AudioSync didn't quit within 10 s: stopping it"
  pkill -KILL -x "$EXECUTABLE" || true
  sleep 0.5
}
quit_running

# Copied next to the old app first, so the swap itself is a rename.
next="$dest_dir/.AudioSync.app.installing"
rm -rf "$next"
ditto "$APP" "$next"
rm -rf "$dest"
mv "$next" "$dest"
if [ -x "$LAUNCH_SERVICES_TOOL" ]; then "$LAUNCH_SERVICES_TOOL" -f "$dest" || true; fi

# remove_copy PATH: unregister and delete another AudioSync.app, so that
# `open -a AudioSync` (and Launch Services) only knows the installed one.
remove_copy() {
  local other=$1 id
  [ -d "$other" ] || return 0
  if [ "$other" -ef "$dest" ]; then return 0; fi
  id=$(plutil -extract CFBundleIdentifier raw -o - "$other/Contents/Info.plist" 2>/dev/null) || id=""
  [ "$id" = "$BUNDLE_ID" ] || return 0
  if [ -x "$LAUNCH_SERVICES_TOOL" ]; then "$LAUNCH_SERVICES_TOOL" -u "$other" >/dev/null 2>&1 || true; fi
  if rm -rf "$other" 2>/dev/null; then
    echo "   Removed another copy: $other"
  else
    warn "Couldn't remove $other: open -a AudioSync may start that copy"
  fi
}
case "$dest_dir" in
  /Applications) remove_copy "$HOME/Applications/AudioSync.app" ;;
  "$HOME/Applications") remove_copy /Applications/AudioSync.app ;;
esac
remove_copy "$APP"
rm -rf "$STAGE"
# A bundle an earlier build script left at the top of the checkout.
remove_copy "$ROOT/AudioSync.app"

echo
echo "✅ AudioSync $VERSION installed: $dest (signed with $label)"
echo "   Start it with: open \"$dest\""
