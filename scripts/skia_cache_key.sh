#!/usr/bin/env bash
# Prints the Actions cache key of the Skia archive for a Windows target: the
# skia-bindings version and the features the app enables, which fix the
# binaries key skia-bindings downloads by. Reads Cargo.lock only; builds nothing.
# Usage: scripts/skia_cache_key.sh <target triple>
set -euo pipefail

target=${1:?usage: skia_cache_key.sh <target triple>}
manifest="$(dirname "$0")/../desktop/Cargo.toml"
tree=$(cargo tree --locked --manifest-path "$manifest" --target "$target" -e features -i skia-bindings)
version=$(grep -o 'skia-bindings v[0-9][0-9.]*' <<<"$tree" | head -n 1 | cut -d v -f 2)
features=$(grep -o 'skia-bindings feature "[a-z0-9-]*"' <<<"$tree" | cut -d '"' -f 2 | sort -u | paste -sd - -)
if [ -z "$version" ] || [ -z "$features" ]; then
    echo "skia_cache_key.sh: no skia-bindings in the $target build" >&2
    exit 1
fi
# The packing script and the Rust toolchain shape the archive too.
packer=$(sha256sum "$(dirname "$0")/skia_archive.py" | cut -c 1-12)
rust=$(rustc --version | cut -d ' ' -f 2)
echo "skia-$target-$version-$features-static-rust$rust-pack$packer"
