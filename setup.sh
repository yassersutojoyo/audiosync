#!/bin/bash
#
# AudioSync — one-time setup on a new Mac: checks the tools, then builds the
# app, signs it and installs it to /Applications (./build.sh). AirPlay goes
# through AudioSync's own sender, which is part of the Rust core the app links
# (core/ and airplay/), so there is no separate engine to build or bundle.
#
# Requirements: Xcode Command Line Tools (swift, clang), a Rust toolchain
# (https://rustup.rs), python3, macOS 15.0+ (Sequoia).
#
set -o errexit -o nounset -o pipefail

# step TITLE: a bold cyan heading for the next step.
step() { printf '\n\033[1;36m▶ %s\033[0m\n' "$1"; }

command -v swift >/dev/null || { echo "Xcode Command Line Tools are required: xcode-select --install"; exit 1; }
# cargo may be in ~/.cargo/bin or next to Homebrew's keg-only rustup, off PATH.
# shellcheck source=scripts/cargo_path.sh disable=SC1091
source "$(dirname -- "$0")/scripts/cargo_path.sh"
command -v cargo >/dev/null || { echo "A Rust toolchain is required: https://rustup.rs"; exit 1; }

step "Building and installing AudioSync"
cd -- "$(dirname -- "$0")"
./build.sh

cat <<'NEXT_STEPS'

✅ Setup complete.

  1) Click the AudioSync icon in the menu bar.
  2) Pick an audio source and click a speaker to start. HomePods and other AirPlay
     speakers appear under "Connected Speakers".
     For a HomePod, Home app → Home Settings → Speakers & TV → Allow Speaker & TV Access must be
     "Anyone on the Same Network" (recommended) or "Everyone".
  3) The first time, allow "System Audio Recording" when macOS asks.

NEXT_STEPS
