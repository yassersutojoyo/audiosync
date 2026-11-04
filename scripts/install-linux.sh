#!/bin/bash
# Installs or updates the AudioSync AppImage for the current user from a GitHub release.
#   curl -fsSL https://github.com/yassersutojoyo/audiosync/releases/latest/download/install-linux.sh | bash
#   curl -fsSL .../install-linux.sh | bash -s -- 0.9.0     a given version instead of the latest
#   scripts/install-linux.sh --dry-run [VERSION]          print what it would download, change nothing
# Checks the AppImage against the release's SHA256SUMS.txt before anything is
# installed, never uses sudo and writes only under ~/.local: the AppImage in
# bin, a menu entry in share/applications and its icon in share/icons.
set -o errexit -o nounset -o pipefail

REPO=yassersutojoyo/audiosync
# The release key's public half ("ssh-ed25519 AAAA..."). Empty until that key exists
# (RELEASE.md, "Signed checksums"); until then the install trusts the tag over HTTPS alone.
RELEASE_KEY=""
# The first version signed with that key; from it on a release without SHA256SUMS.txt.sig is refused.
KEY_SINCE=""
API="https://api.github.com/repos/$REPO/releases/latest"
ID=io.github.yassersutojoyo.audiosync
BIN="$HOME/.local/bin"
DATA="${XDG_DATA_HOME:-$HOME/.local/share}"
EXECUTABLE=audiosync

say() { printf '%s\n' "$*"; }
fail() { printf 'AudioSync install: %s\n' "$*" >&2; exit 1; }

usage() {
  say "Usage: install-linux.sh [--dry-run] [VERSION]"
  say "Installs AudioSync VERSION, or the latest release, as $BIN/$EXECUTABLE."
}

# Same reading as scripts/install.sh: each JSON key on its own line, pretty-printed or not.
tag_from_json() {
  tr ',{}' '\n' \
    | sed -n 's/^[[:space:]]*"tag_name"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' \
    | head -n 1
}

# Checks SHA256SUMS.txt against the SHA256SUMS.txt.sig on the same tag, when the tag has one.
check_signature() {   # check_signature <release URL> <SHA256SUMS.txt path> <version>
  [ -n "$RELEASE_KEY" ] || return 0
  local sig="$2.sig" code
  code=$(curl -sSL -o "$sig" -w '%{http_code}' "$1/SHA256SUMS.txt.sig") || fail "couldn't download $1/SHA256SUMS.txt.sig"
  case $code in
    200) ;;
    404)
      if [ -n "$KEY_SINCE" ] && version_at_least "$3" "$KEY_SINCE"; then
        fail "AudioSync $3 has no SHA256SUMS.txt.sig, though every release from $KEY_SINCE on is signed. Nothing was installed."
      fi
      return 0 ;;   # a release from before the key
    *) fail "couldn't download $1/SHA256SUMS.txt.sig (HTTP $code)" ;;
  esac
  command -v ssh-keygen >/dev/null || fail "ssh-keygen is missing, so the release's signature can't be checked. Nothing was installed."
  printf 'audiosync-release %s\n' "$RELEASE_KEY" > "$2.signers"
  ssh-keygen -Y verify -f "$2.signers" -I audiosync-release -n file -s "$sig" < "$2" >/dev/null 2>&1 \
    || fail "SHA256SUMS.txt doesn't carry the release key's signature. Nothing was installed."
}

# True when version $1 is $2 or later, field by field; a -suffix is ignored.
version_at_least() {
  local IFS=. i x y
  local -a a b
  read -r -a a <<< "${1%%-*}"
  read -r -a b <<< "${2%%-*}"
  for ((i = 0; i < ${#a[@]} || i < ${#b[@]}; i++)); do
    x=$((10#${a[i]:-0})) y=$((10#${b[i]:-0}))
    [ "$x" -eq "$y" ] || { [ "$x" -gt "$y" ]; return; }
  done
}

valid_version() {
  local re='^[0-9]+(\.[0-9]+)*(-[0-9A-Za-z.]+)?$'
  [[ $1 =~ $re ]]
}

machine_arch() {
  case $(uname -m) in
    x86_64|amd64) echo x86_64 ;;
    aarch64|arm64) echo aarch64 ;;
    *) fail "AudioSync runs on x86_64 and aarch64; this machine is $(uname -m)." ;;
  esac
}

check_linux() {
  [ "$(uname -s)" = Linux ] || fail "this installer is for Linux only. The Mac uses scripts/install.sh, Windows scripts/install.ps1."
  local tool
  for tool in curl sha256sum pgrep nohup; do
    command -v "$tool" >/dev/null || fail "$tool is missing."
  done
}

# This user's processes started from the installed AppImage, which sets
# APPIMAGE to its own path; a build run from elsewhere doesn't match.
installed_pids() {   # installed_pids <path>
  local pid
  for pid in $(pgrep -u "$(id -u)" -x "$EXECUTABLE"); do
    if tr '\0' '\n' < "/proc/$pid/environ" 2>/dev/null | grep -qxF "APPIMAGE=$1"; then
      echo "$pid"
    fi
  done
}

quit_running() {   # quit_running <path>
  local pids
  pids=$(installed_pids "$1")
  [ -n "$pids" ] || return 0
  say "Quitting the running AudioSync"
  # shellcheck disable=SC2086 # one PID per word
  kill -TERM $pids 2>/dev/null || true
  local _
  for _ in $(seq 1 100); do
    [ -n "$(installed_pids "$1")" ] || return 0
    sleep 0.1
  done
  fail "AudioSync is still running after 10 s. Quit it from its tray icon and run this again."
}

main() {
  local dry_run=0 version=""
  while [ $# -gt 0 ]; do
    case $1 in
      --dry-run) dry_run=1 ;;
      -h|--help) usage; exit 0 ;;
      -*) usage >&2; exit 2 ;;
      '') fail "the version argument is empty." ;;
      *) [ -z "$version" ] || { usage >&2; exit 2; }; version=$1 ;;
    esac
    shift
  done

  [ "$dry_run" = 1 ] || check_linux
  local arch
  arch=$(machine_arch)

  if [ -z "$version" ]; then
    say "Looking up the latest release"
    local json
    json=$(curl -fsSL -H 'Accept: application/vnd.github+json' "$API") \
      || fail "couldn't reach the GitHub API (it allows 60 lookups an hour per address). Name a version instead: bash -s -- 0.9.0"
    version=$(printf '%s' "$json" | tag_from_json)
    [ -n "$version" ] || fail "the GitHub API answer had no release tag."
  fi
  version=${version#v}
  valid_version "$version" || fail "'$version' isn't a version number like 0.9.0."

  local base="https://github.com/$REPO/releases/download/v$version"
  local appimage="AudioSync-$version-$arch.AppImage"
  local dest="$BIN/$EXECUTABLE"
  local desktop="$DATA/applications/$ID.desktop"
  local icon="$DATA/icons/hicolor/256x256/apps/$ID.png"
  if [ "$dry_run" = 1 ]; then
    say "version $version"
    say "appimage $base/$appimage"
    say "sums $base/SHA256SUMS.txt"
    say "app $dest"
    say "desktop $desktop"
    say "icon $icon"
    exit 0
  fi

  tmp=$(mktemp -d "${TMPDIR:-/tmp}/audiosync-install-XXXXXX")
  trap 'rm -rf "$tmp"' EXIT

  curl -fsSL -o "$tmp/SHA256SUMS.txt" "$base/SHA256SUMS.txt" || fail "couldn't download $base/SHA256SUMS.txt"
  check_signature "$base" "$tmp/SHA256SUMS.txt" "$version"
  local expected
  expected=$(awk -v f="$appimage" '$2 == f || $2 == "*" f { print $1; exit }' "$tmp/SHA256SUMS.txt")
  local hex='^[0-9a-fA-F]{64}$'
  [[ $expected =~ $hex ]] \
    || fail "AudioSync $version has no Linux AppImage for $arch ($appimage isn't in its SHA256SUMS.txt). Nothing was installed."

  say "Downloading AudioSync $version"
  curl -fL --progress-bar -o "$tmp/$appimage" "$base/$appimage" || fail "couldn't download $base/$appimage"
  say "Checking the SHA-256 checksum"
  local actual
  actual=$(sha256sum "$tmp/$appimage" | awk '{ print $1 }')
  [ "$actual" = "$expected" ] || fail "the checksum of $appimage doesn't match SHA256SUMS.txt. Nothing was installed."
  chmod 755 "$tmp/$appimage"

  # The AppImage's runtime unpacks its own icon, no FUSE needed.
  (cd "$tmp" && "./$appimage" --appimage-extract "usr/share/icons/hicolor/256x256/apps/$ID.png" >/dev/null) \
    || fail "couldn't take the icon out of $appimage. Nothing was installed."

  quit_running "$dest"

  say "Installing to $dest"
  mkdir -p "$BIN" "${desktop%/*}" "${icon%/*}"
  # A rename on the same file system, so the old AppImage is replaced in one step.
  cp "$tmp/$appimage" "$dest.new"
  mv -f "$dest.new" "$dest"
  cp "$tmp/squashfs-root/usr/share/icons/hicolor/256x256/apps/$ID.png" "$icon"
  # Exec quotes the path, since the desktop entry spec splits it on spaces.
  cat > "$desktop" <<EOF
[Desktop Entry]
Type=Application
Name=AudioSync
Comment=Play this computer's sound on AirPlay and Google Cast speakers and local outputs
Exec="$dest"
Icon=$ID
Categories=AudioVideo;Audio;
StartupWMClass=$ID
SingleMainWindow=true
Terminal=false
EOF

  say "Opening AudioSync"
  nohup "$dest" >/dev/null 2>&1 &
  say "Done: AudioSync $version is in $BIN, with an entry in your app menu."
}

main "$@"
