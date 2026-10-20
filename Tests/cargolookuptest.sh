#!/bin/bash
# Checks scripts/cargo_path.sh, the cargo lookup that setup.sh and
# scripts/build_core.sh share: cargo is found when it isn't on PATH but sits
# next to Homebrew's keg-only rustup or in ~/.cargo/bin, and both scripts use
# the shared lookup. Usage: Tests/cargolookuptest.sh (run by Tests/run-tests.sh).
set -o errexit -o nounset -o pipefail
cd "$(dirname "$0")/.."
repo=$PWD
tmp=$(mktemp -d "${TMPDIR:-/tmp}/audiosync-cargo-XXXXXX")
trap 'rm -rf "$tmp"' EXIT
failures=0
check() {   # check <name> <expected path> <PATH> <HOME>
  local name=$1 expected=$2 path=$3 home=$4 found
  found=$(env -i HOME="$home" PATH="$path" /bin/bash -c \
    'source "$1/scripts/cargo_path.sh" && command -v cargo' _ "$repo" 2>&1) || true
  if [ "$found" = "$expected" ]; then
    echo "  PASS  $name"
  else
    echo "  FAIL  $name: expected $expected, got '$found'"
    failures=$((failures + 1))
  fi
}

# A keg-only rustup: only a link to it is on PATH; cargo sits next to the real file.
mkdir -p "$tmp/keg/bin" "$tmp/linked" "$tmp/home1"
printf '#!/bin/sh\n' > "$tmp/keg/bin/rustup"
printf '#!/bin/sh\n' > "$tmp/keg/bin/cargo"
chmod +x "$tmp/keg/bin/rustup" "$tmp/keg/bin/cargo"
ln -s "$tmp/keg/bin/rustup" "$tmp/linked/rustup"
check "cargo next to a linked rustup" "$tmp/keg/bin/cargo" "$tmp/linked:/usr/bin:/bin" "$tmp/home1"

# rustup's installer: cargo in ~/.cargo/bin, nothing on PATH.
mkdir -p "$tmp/home2/.cargo/bin"
printf '#!/bin/sh\n' > "$tmp/home2/.cargo/bin/cargo"
chmod +x "$tmp/home2/.cargo/bin/cargo"
check "cargo in ~/.cargo/bin" "$tmp/home2/.cargo/bin/cargo" "/usr/bin:/bin" "$tmp/home2"

for script in setup.sh scripts/build_core.sh; do
  if grep -q 'cargo_path.sh' "$script"; then
    echo "  PASS  $script uses scripts/cargo_path.sh"
  else
    echo "  FAIL  $script doesn't use scripts/cargo_path.sh"
    failures=$((failures + 1))
  fi
done

[ "$failures" = 0 ]
