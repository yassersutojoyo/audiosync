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
# The prebuilt standard library in cargo's archive also carries LLVM bitcode
# (__LLVM sections). Some Apple linkers keep it in ld -r output, and an older
# Xcode's nm and ld can't read bitcode from a newer LLVM, so it's removed before
# the prelink with the toolchain's own objcopy, which shares rustc's LLVM.
#
# That code includes parts of the Rust standard library, so licenses/rust/ next
# to the library receives its copyright notices and license texts from the
# toolchain that built it, and, in crates.txt, those of the crates compiled in
# with the core (scripts/core_notices.py); build.sh ships them in the app.
#
# AUDIOSYNC_CORE_PROBE=1 (CI only) builds the same library with the core's
# ffi-panic-probe feature into universal-apple-darwin/probe/ instead, exporting
# asc_airplay_panic_probe besides the header's functions, and skips the
# notices: Tests/panic-probe.c links it to check that a panic inside an
# asc_airplay_* call stops at the C boundary in this build (S205). The app's
# library is never built that way.
#
# Needs a Rust toolchain (https://rustup.rs) with the standard library for both
# macOS targets, and python3; with rustup, a missing target is added here.
# Usage: scripts/build_core.sh
set -o errexit -o nounset -o pipefail
cd "$(dirname "$0")/.."
root=$(pwd)

arm=aarch64-apple-darwin
intel=x86_64-apple-darwin
lib=libaudiosync_core.a
header="$root/core/include/audiosync_core.h"
target_dir="$root/core/target"
out="$target_dir/universal-apple-darwin/release"

# The panic probe's build (see above): its own folder, one more export.
probe=${AUDIOSYNC_CORE_PROBE:-0}
features=()
built=""
if [ "$probe" = 1 ]; then
  out="$target_dir/universal-apple-darwin/probe"
  features=(--features ffi-panic-probe)
  built=", with the panic probe"
elif [ "$probe" != 0 ]; then
  echo "❌ AUDIOSYNC_CORE_PROBE must be 0 or 1, not \"$probe\"" >&2
  exit 1
fi

# The app's LSMinimumSystemVersion: the core must not need a newer macOS.
export MACOSX_DEPLOYMENT_TARGET=15.0

# cargo may be in ~/.cargo/bin or next to Homebrew's keg-only rustup, off PATH.
# shellcheck source=scripts/cargo_path.sh disable=SC1091
source "$root/scripts/cargo_path.sh"
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
# The toolchain's objcopy: rust-objcopy, which ships with rustc, or llvm-objcopy
# from rustup's llvm-tools. It loads rustc's LLVM library from the toolchain's
# lib/, which prelink puts on its library path the way rustc does.
tools="$sysroot/lib/rustlib/$(rustc -vV | sed -n 's/^host: //p')/bin"
objcopy="$tools/rust-objcopy"
[ -x "$objcopy" ] || objcopy="$tools/llvm-objcopy"
if [ ! -x "$objcopy" ]; then
  echo "❌ This Rust toolchain has no rust-objcopy ($tools)." >&2
  echo "   Update it (rustup update stable) or add one: rustup component add llvm-tools" >&2
  exit 1
fi

echo "▶ Rust core (release, $arm and $intel$built)…"
# The array's ${…+…} form: bash 3.2, macOS's, calls an empty one unbound.
cargo build --release --lib --locked ${features[@]+"${features[@]}"} \
  --target "$arm" --target "$intel" --target-dir "$target_dir"

mkdir -p "$out"
work=$(mktemp -d "$target_dir/.universal.XXXXXX")
trap 'rm -rf "$work"' EXIT

# The header's functions as Mach-O symbol names. ld fails if one is missing.
grep -o -E 'asc_[a-z0-9_]+[(]' "$header" | sed -e 's/[(]$//' -e 's/^/_/' | sort -u > "$work/exports" || true
if [ ! -s "$work/exports" ]; then
  echo "❌ No asc_* functions found in $header" >&2
  exit 1
fi
if [ "$probe" = 1 ]; then
  echo _asc_airplay_panic_probe >> "$work/exports"
  sort -u -o "$work/exports" "$work/exports"
fi

sdk=$(xcrun --sdk macosx --show-sdk-version)
# -u makes ld load every archive member the header's functions need, whatever a
# linker makes of the export list, which then hides everything else.
roots=()
while IFS= read -r symbol; do
  roots+=(-u "$symbol")
done < "$work/exports"
prelink() {   # prelink ARCH RUST_TARGET
  DYLD_FALLBACK_LIBRARY_PATH="$sysroot/lib" "$objcopy" \
    --remove-section=__LLVM,__bitcode --remove-section=__LLVM,__cmdline \
    "$target_dir/$2/release/$lib" "$work/$1.a"
  # -S leaves out the debug map, which would record this run's temporary paths.
  xcrun ld -r -S -arch "$1" -platform_version macos "$MACOSX_DEPLOYMENT_TARGET" "$sdk" \
    "${roots[@]}" -exported_symbols_list "$work/exports" -o "$work/$1.o" "$work/$1.a"
  xcrun otool -l "$work/$1.o" > "$work/$1.load"
  if grep -q 'segname __LLVM' "$work/$1.load"; then
    echo "❌ The $1 object still carries LLVM bitcode (an __LLVM section)." >&2
    exit 1
  fi
}
prelink arm64 "$arm"
prelink x86_64 "$intel"
# Zero member dates, so an unchanged core gives a byte-identical library.
ZERO_AR_DATE=1 xcrun libtool -static -o "$work/$lib" "$work/arm64.o" "$work/x86_64.o"

for arch in arm64 x86_64; do
  xcrun nm -gU --no-llvm-bc -arch "$arch" "$work/$lib" | awk 'NF == 3 { print $3 }' | sort -u > "$work/globals"
  if ! cmp -s "$work/globals" "$work/exports"; then
    echo "❌ The $arch slice exports more or less than the header declares:" >&2
    diff "$work/exports" "$work/globals" >&2 || true
    exit 1
  fi
done

# An unchanged library keeps its timestamp, so build.sh doesn't relink for
# nothing; a changed one is newer than the app, and build.sh then removes the
# linked binary so every SwiftPM build system relinks it.
if ! cmp -s "$work/$lib" "$out/$lib"; then
  mv -f "$work/$lib" "$out/$lib"
fi
if [ "$probe" = 1 ]; then
  echo "   ${out#"$root"/}/$lib ($(xcrun lipo -archs "$out/$lib")$built)"
  exit 0
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
# The crates compiled into the library (AudioSync's AirPlay sender and the
# crates it uses): each one's license and notice texts, from core/Cargo.lock.
# Fails when a crate's license isn't on core/deny.toml's list.
python3 -I "$root/scripts/core_notices.py" --output "$licenses/crates.txt" "$arm" "$intel"

echo "   ${out#"$root"/}/$lib ($(xcrun lipo -archs "$out/$lib"))"
