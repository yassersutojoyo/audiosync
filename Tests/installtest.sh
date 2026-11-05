#!/bin/bash
# Checks scripts/install.sh without the network: a fake curl on PATH answers
# from fixtures. Covers the version lookup (pretty and minified API answers,
# a given version, a bad one, an unreachable API) through --dry-run, and on a
# Mac a checksum mismatch, which must stop before /Applications changes.
# Also runs scripts/install-linux.sh end to end in a throwaway HOME, and,
# where PowerShell exists, parses scripts/install.ps1 and checks its -DryRun.
# Usage: Tests/installtest.sh (run by Tests/run-tests.sh).
set -o errexit -o nounset -o pipefail
cd "$(dirname "$0")/.."
repo=$PWD
tmp=$(mktemp -d "${TMPDIR:-/tmp}/audiosync-install-test-XXXXXX")
trap 'rm -rf "$tmp"' EXIT
failures=0
pass() { echo "  PASS  $1"; }
flunk() { echo "  FAIL  $1"; failures=$((failures + 1)); }

# The fake curl prints $FIXTURES/api.json for the API, saves $FIXTURES/<file
# name> for a download given -o, and fails like curl -f when FAKE_CURL_FAIL is set.
# Given -w, it answers a missing file with a 404 status, as curl without -f does.
mkdir -p "$tmp/bin" "$tmp/fixtures"
cat > "$tmp/bin/curl" <<'EOF'
#!/bin/bash
[ -z "${FAKE_CURL_FAIL:-}" ] || exit 22
out="" url="" status=""
while [ $# -gt 0 ]; do
  case $1 in
    -o) out=$2; shift ;;
    -H) shift ;;
    -w) status=1; shift ;;
    -*) ;;
    *) url=$1 ;;
  esac
  shift
done
case $url in
  */releases/latest) cat "$FIXTURES/api.json" ;;
  *) if [ -n "$status" ] && [ ! -e "$FIXTURES/${url##*/}" ]; then printf 404; exit 0; fi
     cp "$FIXTURES/${url##*/}" "$out" && { [ -z "$status" ] || printf 200; } ;;
esac
EOF
chmod +x "$tmp/bin/curl"
export FIXTURES="$tmp/fixtures"

run() {   # run <args…>: install.sh with the fake curl, stdout and stderr together
  PATH="$tmp/bin:$PATH" bash "$repo/scripts/install.sh" "$@" 2>&1
}

base=https://github.com/yassersutojoyo/audiosync/releases/download/v0.6.0
expected="version 0.6.0
zip $base/AudioSync-0.6.0.zip
sums $base/SHA256SUMS.txt
app /Applications/AudioSync.app"

check_dry() {   # check_dry <name> <args…>
  local name=$1 got; shift
  if got=$(run --dry-run "$@" | grep -v '^Looking up'); [ "$got" = "$expected" ]; then
    pass "$name"
  else
    flunk "$name: got '$got'"
  fi
}

check_dry "given version" 0.6.0
check_dry "given tag with a v" v0.6.0

cat > "$tmp/fixtures/api.json" <<'EOF'
{
  "url": "https://api.github.com/repos/yassersutojoyo/audiosync/releases/1",
  "author": {
    "login": "yassersutojoyo"
  },
  "tag_name": "v0.6.0",
  "name": "AudioSync 0.6.0",
  "body": "Mentions \"tag_name\": \"v9.9.9\" in the notes."
}
EOF
check_dry "latest, pretty JSON"

printf '%s' '{"url":"x","author":{"login":"y"},"tag_name":"v0.6.0","assets":[{"name":"AudioSync.dmg"}]}' \
  > "$tmp/fixtures/api.json"
check_dry "latest, minified JSON"

printf '%s' '{"message":"Not Found"}' > "$tmp/fixtures/api.json"
if run --dry-run >/dev/null; then flunk "API answer without a tag"; else pass "API answer without a tag"; fi

if FAKE_CURL_FAIL=1 run --dry-run >/dev/null; then flunk "API unreachable"; else pass "API unreachable"; fi

for bad in '1.0; rm -rf /' '../0.6.0' 'latest' $'0.6.0\nrm' ''; do
  if got=$(run --dry-run "$bad"); then
    flunk "rejects version '$bad'"
  elif printf '%s' "$got" | grep -Eq "isn't a version number|is empty"; then
    pass "rejects version '$bad'"
  else
    flunk "rejects version '$bad': got '$got'"
  fi
done

if [ "$(uname -s)" != Darwin ]; then
  if got=$(run 0.6.0); then
    flunk "refuses to install off macOS"
  elif printf '%s' "$got" | grep -q 'macOS only'; then
    pass "refuses to install off macOS"
  else
    flunk "refuses to install off macOS: got '$got'"
  fi
elif [ -w /Applications ]; then
  printf 'not a zip' > "$tmp/fixtures/AudioSync-0.6.0.zip"
  printf '%064d  AudioSync-0.6.0.zip\n' 0 > "$tmp/fixtures/SHA256SUMS.txt"
  before=$(ls -la /Applications)
  if got=$(run 0.6.0); then
    flunk "checksum mismatch stops the install"
  elif printf '%s' "$got" | grep -q 'Nothing was installed' && [ "$(ls -la /Applications)" = "$before" ]; then
    pass "checksum mismatch stops the install"
  else
    flunk "checksum mismatch stops the install: got '$got'"
  fi
else
  echo "  SKIP  checksum mismatch: /Applications isn't writable here"
fi

# scripts/install-linux.sh, on any host: a fake uname says Linux, fake
# pgrep lists $FAKE_PIDS only when asked for this user's audiosync processes,
# and HOME is a fresh folder per run.
mkdir -p "$tmp/linuxbin"
cat > "$tmp/linuxbin/uname" <<'EOF'
#!/bin/bash
case $1 in -m) echo "${FAKE_ARCH:-x86_64}" ;; *) echo "${FAKE_OS:-Linux}" ;; esac
EOF
cat > "$tmp/linuxbin/pgrep" <<'EOF'
#!/bin/bash
[ "$*" = "-u $(id -u) -x audiosync" ] && [ -n "${FAKE_PIDS:-}" ] || exit 1
printf '%s\n' $FAKE_PIDS
EOF
if ! command -v sha256sum >/dev/null; then
  printf '#!/bin/bash\nshasum -a 256 "$@"\n' > "$tmp/linuxbin/sha256sum"
fi
chmod +x "$tmp/linuxbin"/*
# The fake AppImage writes its icon for --appimage-extract, and marks itself launched otherwise.
appimage=AudioSync-0.9.0-x86_64.AppImage
cat > "$tmp/fixtures/$appimage" <<'EOF'
#!/bin/bash
if [ "${1:-}" = --appimage-extract ]; then mkdir -p "squashfs-root/${2%/*}"; printf icon > "squashfs-root/$2"; exit 0; fi
touch "$FIXTURES/launched"
EOF

new_home() { home=$(mktemp -d "$tmp/home-XXXXXX"); }
run_linux() {   # run_linux <args…>: install-linux.sh with HOME=$home
  HOME=$home XDG_DATA_HOME="" PATH="$tmp/linuxbin:$tmp/bin:$PATH" \
    bash "${LINUX_SCRIPT:-$repo/scripts/install-linux.sh}" "$@" 2>&1
}

lbase=https://github.com/yassersutojoyo/audiosync/releases/download/v0.9.0
printf '%s' '{"tag_name":"v0.9.0"}' > "$tmp/fixtures/api.json"
new_home
for arch in x86_64 aarch64; do
  want="version 0.9.0
appimage $lbase/AudioSync-0.9.0-$arch.AppImage
sums $lbase/SHA256SUMS.txt"
  if got=$(FAKE_ARCH=$arch run_linux --dry-run | grep -v '^Looking up' | head -n 3); [ "$got" = "$want" ]; then
    pass "linux: latest $arch AppImage"
  else
    flunk "linux: latest $arch AppImage: got '$got'"
  fi
done

expect_linux_fail() {   # expect_linux_fail <name> <message> <args…>
  local name=$1 message=$2 got; shift 2
  new_home
  if got=$(run_linux "$@"); then
    flunk "$name"
  elif printf '%s' "$got" | grep -q "$message" && [ -z "$(ls -A "$home")" ]; then
    pass "$name"
  else
    flunk "$name: got '$got'"
  fi
}
FAKE_ARCH=riscv64 expect_linux_fail "linux: refuses another arch" "x86_64 and aarch64" 0.9.0
FAKE_OS=Darwin expect_linux_fail "linux: refuses another OS" "Linux only" 0.9.0
expect_linux_fail "linux: rejects a bad version" "isn't a version number" '0.9.0; rm -rf /'
printf '%064d  AudioSync-0.9.0.zip\n' 0 > "$tmp/fixtures/SHA256SUMS.txt"
expect_linux_fail "linux: a release without the AppImage" "has no Linux AppImage" 0.9.0
printf '%064d  %s\n' 0 "$appimage" > "$tmp/fixtures/SHA256SUMS.txt"
expect_linux_fail "linux: checksum mismatch installs nothing" "Nothing was installed" 0.9.0

install_ok() {   # install_ok <name>: an install into $home, checked file by file
  local name=$1 got
  (cd "$tmp/fixtures" && sha256sum "$appimage") > "$tmp/fixtures/SHA256SUMS.txt"
  rm -f "$tmp/fixtures/launched"
  local apps="$home/.local/share/applications/io.github.yassersutojoyo.audiosync.desktop"
  if ! got=$(run_linux 0.9.0); then
    flunk "$name: got '$got'"
  elif ! cmp -s "$tmp/fixtures/$appimage" "$home/.local/bin/audiosync" || [ ! -x "$home/.local/bin/audiosync" ]; then
    flunk "$name: the AppImage isn't in ~/.local/bin"
  elif ! grep -qx "Exec=\"$home/.local/bin/audiosync\"" "$apps"; then
    flunk "$name: the desktop entry doesn't run it"
  elif [ ! -s "$home/.local/share/icons/hicolor/256x256/apps/io.github.yassersutojoyo.audiosync.png" ]; then
    flunk "$name: no icon"
  else
    local _
    for _ in $(seq 1 50); do [ -e "$tmp/fixtures/launched" ] && break; sleep 0.1; done
    if [ -e "$tmp/fixtures/launched" ]; then pass "$name"; else flunk "$name: not launched"; fi
  fi
}
install_ok "linux: installs, adds a menu entry and an icon, opens it"
printf 'echo newer\n' >> "$tmp/fixtures/$appimage"
mkdir -p "$home/.local/bin" && printf 'old' > "$home/.local/bin/audiosync"
install_ok "linux: a second run replaces the old AppImage"

# Two stand-in processes: one started from the installed AppImage, one from elsewhere.
if [ -r "/proc/$$/environ" ]; then
  APPIMAGE="$home/.local/bin/audiosync" sleep 30 &
  ours=$!
  APPIMAGE=/opt/dev/audiosync sleep 30 &
  other=$!
  FAKE_PIDS="$ours $other" install_ok "linux: a rerun installs while a copy runs"
  # A killed child stays a zombie until reaped, with an empty environ.
  if tr '\0' '\n' 2>/dev/null < "/proc/$ours/environ" | grep -q '^APPIMAGE='; then
    flunk "linux: a rerun quits the installed copy"
  else
    pass "linux: a rerun quits the installed copy"
  fi
  if kill "$other" 2>/dev/null; then
    pass "linux: a rerun leaves a copy from elsewhere running"
  else
    flunk "linux: a rerun leaves a copy from elsewhere running"
  fi
else
  echo "  SKIP  linux: running copies (no /proc here)"
fi

# A stand-in key and KEY_SINCE (no real key): a release from KEY_SINCE on without
# SHA256SUMS.txt.sig is refused, an older one installs on its checksum.
sed -e 's/^RELEASE_KEY=""$/RELEASE_KEY="ssh-ed25519 STUB"/' -e 's/^KEY_SINCE=""$/KEY_SINCE="0.9.0"/' \
  "$repo/scripts/install-linux.sh" > "$tmp/install-linux-keyed.sh"
LINUX_SCRIPT=$tmp/install-linux-keyed.sh expect_linux_fail "linux: a release from KEY_SINCE on needs its signature" \
  "has no SHA256SUMS.txt.sig" 0.9.0
new_home
sed -i.bak 's/^KEY_SINCE="0.9.0"$/KEY_SINCE="0.10.0"/' "$tmp/install-linux-keyed.sh"
LINUX_SCRIPT=$tmp/install-linux-keyed.sh install_ok "linux: a release before KEY_SINCE installs without a signature"

# scripts/install.ps1: a parse check and the asset it picks, wherever PowerShell is.
ps=$(command -v pwsh || command -v powershell || true)
if [ -z "$ps" ]; then
  echo "  SKIP  install.ps1: no PowerShell here"
else
  # shellcheck disable=SC2016 # PowerShell variables, not the shell's
  if errors=$("$ps" -NoProfile -Command '$e = $null; [void][System.Management.Automation.Language.Parser]::ParseFile((Resolve-Path scripts/install.ps1).Path, [ref]$null, [ref]$e); $e.Count') \
      && [ "$(printf '%s' "$errors" | tr -d '\r')" = 0 ]; then
    pass "install.ps1 parses"
  else
    flunk "install.ps1 parses: got '$errors'"
  fi
  wbase=https://github.com/yassersutojoyo/audiosync/releases/download/v0.8.0
  case $(uname -s) in
    MINGW*|MSYS*|CYGWIN*) pairs="native:" ;;   # the registry names the arch; the variable can't stand in
    *) pairs="AMD64:x64 ARM64:arm64" ;;
  esac
  for pair in $pairs; do
    got=$(PROCESSOR_ARCHITECTURE=${pair%%:*} LOCALAPPDATA=$tmp/local APPDATA=$tmp/roaming \
      "$ps" -NoProfile -File scripts/install.ps1 -DryRun -Version v0.8.0 2>&1 | tr -d '\r' | sed -n 's/^zip //p') || true
    case $got in
      "$wbase/AudioSync-0.8.0-windows-${pair#*:}"*.zip) pass "install.ps1 picks $got" ;;
      *) flunk "install.ps1 for ${pair%%:*}: got '$got'" ;;
    esac
  done
  if got=$(PROCESSOR_ARCHITECTURE=AMD64 LOCALAPPDATA=$tmp/local APPDATA=$tmp/roaming \
      "$ps" -NoProfile -File scripts/install.ps1 -DryRun -Version '1; x' 2>&1) \
      || ! printf '%s' "$got" | grep -q "isn't a version number"; then
    flunk "install.ps1 rejects a bad version: got '$got'"
  else
    pass "install.ps1 rejects a bad version"
  fi
fi

# A one-liner served from a branch runs whatever was last pushed there; a release tag pins it.
# CHANGELOG.md keeps the old URL in a released entry.
if found=$(git -C "$repo" grep -n -E 'raw\.githubusercontent\.com/[^ ]*/(main|master)/scripts/install' -- . ':!CHANGELOG.md'); then
  flunk "the install one-liners fetch from a release, not a branch: $found"
else
  pass "the install one-liners fetch from a release, not a branch"
fi

if [ "$failures" -gt 0 ]; then
  echo "FAILED: $failures"
  exit 1
fi
echo "PASSED"
