#!/bin/bash
# Builds audiosync-core (core/, Rust) as one universal static library for Apple
# silicon and Intel Macs:
#
#   core/target/universal-apple-darwin/release/libaudiosync_core.a
#
# Package.swift links it into the AudioSync executable, so build.sh and CI run
# this before `swift build`. Cargo rebuilds only what changed, and the library
# is replaced only when its content changes.
#
# The app sees only the functions core/include/audiosync_core.h declares.
# Cargo's static library also exports the Rust standard library's own symbols,
# and the app's link would take some of them in place of the system's: every
# Swift availability check would call Rust's ___isPlatformVersionAtLeast
# instead of compiler-rt's. So each slice is prelinked (ld -r) into one object
# that keeps only the header's functions global and holds just the code they
# need.
#
# That code includes parts of the Rust standard library, so licenses/rust/ next
# to the library receives its copyright notices and license texts from the
# toolchain that built it; build.sh ships them in the app.
#
# Needs a Rust toolchain (https://rustup.rs) with the standard library for both
# macOS targets; with rustup, a missing target is added here.
# Usage: scripts/build_core.sh
set -euo pipefail
cd "$(dirname "$0")/.."
root=$(pwd)

arm=aarch64-apple-darwin
intel=x86_64-apple-darwin
lib=libaudiosync_core.a
header="$root/core/include/audiosync_core.h"
target_dir="$root/core/target"
out="$target_dir/universal-apple-darwin/release"

# The app's LSMinimumSystemVersion: the core must not need a newer macOS.
export MACOSX_DEPLOYMENT_TARGET=15.0

# rustup's installer puts cargo in ~/.cargo/bin and Homebrew's rustup keeps it
# next to the rustup binary; neither has to be on the PATH of the build's shell.
rustup_dir() {
  local rustup
  rustup=$(command -v rustup 2>/dev/null) || return 0
  dirname "$(realpath "$rustup")"
}
if ! command -v cargo >/dev/null 2>&1; then
  for dir in "${CARGO_HOME:-$HOME/.cargo}/bin" "$(rustup_dir)"; do
    if [ -n "$dir" ] && [ -x "$dir/cargo" ]; then
      PATH="$dir:$PATH"
      break
    fi
  done
fi
if ! command -v cargo >/dev/null 2>&1; then
  echo "❌ cargo not found: the audio core (core/) needs a Rust toolchain." >&2
  echo "   Install it with rustup (https://rustup.rs), then run the build again." >&2
  exit 1
fi

# From core/, so a toolchain override set for the repository applies.
cd "$root/core"
sysroot=$(rustc --print sysroot)
for target in "$arm" "$intel"; do
  if [ ! -d "$sysroot/lib/rustlib/$target/lib" ] && command -v rustup >/dev/null 2>&1; then
    echo "▶ Adding the Rust standard library for $target (rustup)…"
    rustup target add "$target"
  fi
  if [ ! -d "$sysroot/lib/rustlib/$target/lib" ]; then
    echo "❌ The Rust standard library for $target is missing ($sysroot)." >&2
    echo "   With rustup: rustup target add $arm $intel" >&2
    exit 1
  fi
done

echo "▶ Rust core (release, $arm and $intel)…"
cargo build --release --lib --locked --target "$arm" --target "$intel" --target-dir "$target_dir"

mkdir -p "$out"
work=$(mktemp -d "$target_dir/.universal.XXXXXX")
trap 'rm -rf "$work"' EXIT

# The header's functions as Mach-O symbol names. ld fails if one is missing.
grep -o -E 'asc_[a-z0-9_]+[(]' "$header" | sed -e 's/[(]$//' -e 's/^/_/' | sort -u > "$work/exports" || true
if [ ! -s "$work/exports" ]; then
  echo "❌ No asc_* functions found in $header" >&2
  exit 1
fi

sdk=$(xcrun --sdk macosx --show-sdk-version)
prelink() {   # prelink ARCH RUST_TARGET
  xcrun ld -r -arch "$1" -platform_version macos "$MACOSX_DEPLOYMENT_TARGET" "$sdk" \
    -exported_symbols_list "$work/exports" -o "$work/$1.o" "$target_dir/$2/release/$lib"
}
prelink arm64 "$arm"
prelink x86_64 "$intel"
# Zero member dates, so an unchanged core gives a byte-identical library.
ZERO_AR_DATE=1 xcrun libtool -static -o "$work/$lib" "$work/arm64.o" "$work/x86_64.o"

for arch in arm64 x86_64; do
  xcrun nm -gU -arch "$arch" "$work/$lib" | awk 'NF == 3 { print $3 }' | sort -u > "$work/globals"
  if ! cmp -s "$work/globals" "$work/exports"; then
    echo "❌ The $arch slice exports more or less than the header declares:" >&2
    diff "$work/exports" "$work/globals" >&2 || true
    exit 1
  fi
done

# An unchanged library keeps its timestamp, so build.sh doesn't relink for nothing.
if ! cmp -s "$work/$lib" "$out/$lib"; then
  mv -f "$work/$lib" "$out/$lib"
fi

licenses="$out/licenses/rust"
rm -rf "$licenses"
mkdir -p "$licenses"
for notice in COPYRIGHT-library.html licenses/MIT.txt licenses/Apache-2.0.txt; do
  if [ -f "$sysroot/share/doc/rust/$notice" ]; then
    cp "$sysroot/share/doc/rust/$notice" "$licenses/"
  else
    echo "   ⚠️  $sysroot/share/doc/rust/$notice not found: install Rust with rustup before distributing" >&2
  fi
done
rustc --version > "$licenses/rustc-version.txt"

echo "   ${out#"$root"/}/$lib ($(xcrun lipo -archs "$out/$lib"))"
