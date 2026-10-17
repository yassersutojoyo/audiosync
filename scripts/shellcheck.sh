#!/bin/bash
# Runs shellcheck on every script listed in scripts/shellcheck-files.txt. Fails
# when shellcheck is missing or isn't the pinned version, when a listed path
# doesn't exist, or when a tracked *.sh file is missing from the list. CI and
# Tests/run-tests.sh both run this script with the same shellcheck version, so a
# local test run catches what the CI job catches (versions differ in what they
# flag: 0.9 reports SC2015 cases that 0.11 accepts).
#
#   scripts/shellcheck.sh                 check the listed scripts
#   scripts/shellcheck.sh --install DIR   download the pinned release into DIR
#                                         (checksum-verified; used by CI)
set -euo pipefail
cd "$(dirname "$0")/.."
LIST=scripts/shellcheck-files.txt
VERSION=0.11.0

if [ "${1:-}" = "--install" ]; then
  dir=${2:?usage: scripts/shellcheck.sh --install DIR}
  case "$(uname -s).$(uname -m)" in
    Linux.x86_64)   platform=linux.x86_64   sum=8c3be12b05d5c177a04c29e3c78ce89ac86f1595681cab149b65b97c4e227198 ;;
    Linux.aarch64)  platform=linux.aarch64  sum=12b331c1d2db6b9eb13cfca64306b1b157a86eb69db83023e261eaa7e7c14588 ;;
    Darwin.arm64)   platform=darwin.aarch64 sum=56affdd8de5527894dca6dc3d7e0a99a873b0f004d7aabc30ae407d3f48b0a79 ;;
    Darwin.x86_64)  platform=darwin.x86_64  sum=3c89db4edcab7cf1c27bff178882e0f6f27f7afdf54e859fa041fca10febe4c6 ;;
    *) echo "no pinned shellcheck build for $(uname -s) $(uname -m)" >&2; exit 1 ;;
  esac
  archive="shellcheck-v$VERSION.$platform.tar.xz"
  mkdir -p "$dir"
  curl -fsSL -o "$dir/$archive" "https://github.com/koalaman/shellcheck/releases/download/v$VERSION/$archive"
  echo "$sum  $dir/$archive" | shasum -a 256 -c - >/dev/null
  tar -xJf "$dir/$archive" -C "$dir"
  mv "$dir/shellcheck-v$VERSION/shellcheck" "$dir/shellcheck"
  rm -rf "${dir:?}/$archive" "${dir:?}/shellcheck-v$VERSION"
  "$dir/shellcheck" --version | sed -n 2p
  exit 0
fi

if ! command -v shellcheck >/dev/null 2>&1; then
  echo "shellcheck is not installed (macOS: brew install shellcheck; it must be $VERSION)" >&2
  exit 1
fi
found_version=$(shellcheck --version | sed -n 's/^version: //p')
if [ "$found_version" != "$VERSION" ]; then
  echo "shellcheck $found_version found, but CI uses $VERSION; install $VERSION or change VERSION and the checksums in $0" >&2
  exit 1
fi

files=()
while IFS= read -r pattern; do
  case $pattern in ''|'#'*) continue ;; esac
  found=0
  for f in $pattern; do   # unquoted on purpose: the list may hold globs
    [ -e "$f" ] || continue
    files+=("$f")
    found=1
  done
  if [ "$found" = 0 ]; then
    echo "$LIST: nothing matches '$pattern'" >&2
    exit 1
  fi
done < "$LIST"

if git rev-parse --is-inside-work-tree >/dev/null 2>&1; then
  missing=0
  while IFS= read -r tracked; do
    listed=0
    for f in "${files[@]}"; do
      if [ "$f" = "$tracked" ]; then listed=1; break; fi
    done
    if [ "$listed" = 0 ]; then
      echo "$tracked is not in $LIST" >&2
      missing=1
    fi
  done < <(git ls-files '*.sh')
  [ "$missing" = 0 ] || exit 1
fi

shellcheck "${files[@]}"
echo "shellcheck $VERSION passed (${#files[@]} scripts)"
