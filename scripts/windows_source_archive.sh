#!/bin/bash
# Builds the source archive a release with the Windows preview attaches
# (RELEASE.md, "Licensing"): the repository tree at one commit plus the crates
# windows/ compiles, vendored, so the archive builds with no network.
#
#   scripts/windows_source_archive.sh <tag-or-commit>
#
# Writes dist/AudioSync-<version>-windows-source.tar.gz and prints its SHA-256.
# <version> is the tag without its leading "v", or the short commit hash when
# the argument isn't a tag. The archive's windows/.cargo/config.toml is the
# [source] snippet `cargo vendor` prints, so `cargo build --release --offline`
# in windows/ uses vendor/ instead of crates.io.
set -euo pipefail

if [ $# -ne 1 ]; then
  echo "usage: $0 <tag-or-commit>" >&2
  exit 2
fi
ref=$1

root=$(git rev-parse --show-toplevel)
# shellcheck source=scripts/cargo_path.sh
. "$root/scripts/cargo_path.sh"
command -v cargo >/dev/null 2>&1 || { echo "cargo not found" >&2; exit 1; }

commit=$(git -C "$root" rev-parse --verify --quiet "$ref^{commit}") \
  || { echo "not a commit: $ref" >&2; exit 1; }
if git -C "$root" rev-parse --verify --quiet "refs/tags/$ref" >/dev/null; then
  version=${ref#v}
else
  version=$(git -C "$root" rev-parse --short "$commit")
fi

name="AudioSync-$version-windows-source"
out_dir="$root/dist"
work="$out_dir/$name.work"
archive="$out_dir/$name.tar.gz"
mkdir -p "$out_dir"
rm -rf "$work" "$archive"
trap 'rm -rf "$work"' EXIT
mkdir -p "$work"

git -C "$root" archive --format=tar --prefix="$name/" "$commit" | tar -x -C "$work"

tree="$work/$name"
mkdir -p "$tree/windows/.cargo"
# --locked keeps the versions in the commit's Cargo.lock, the ones its CI built.
(cd "$tree/windows" && cargo vendor --locked --versioned-dirs vendor) \
  > "$tree/windows/.cargo/config.toml"
grep -q '^\[source.vendored-sources\]' "$tree/windows/.cargo/config.toml" \
  || { echo "cargo vendor printed no [source] snippet" >&2; exit 1; }

# Relative names: GNU tar reads "C:" in an absolute Windows path as a remote host.
(cd "$out_dir" && tar -czf "$name.tar.gz" -C "$name.work" "$name")

if command -v sha256sum >/dev/null 2>&1; then
  (cd "$out_dir" && sha256sum "$name.tar.gz")
else
  (cd "$out_dir" && shasum -a 256 "$name.tar.gz")
fi
