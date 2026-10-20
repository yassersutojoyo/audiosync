#!/bin/bash
# Sourced by setup.sh and scripts/build_core.sh: puts cargo on PATH when it
# isn't there yet. rustup's installer puts cargo in ~/.cargo/bin, and
# Homebrew's rustup is keg-only, with cargo next to the real rustup file and
# only a link to rustup on PATH; neither has to be on the PATH of the shell
# that runs the scripts. Leaves PATH alone when cargo is found or missing.

# cargo_rustup_dir: the folder of the real rustup file, if rustup is on PATH.
cargo_rustup_dir() {
  local rustup
  rustup=$(command -v rustup 2>/dev/null) || return 0
  dirname "$(realpath "$rustup")"
}

if ! command -v cargo >/dev/null 2>&1; then
  for cargo_dir in "${CARGO_HOME:-${HOME:-}/.cargo}/bin" "$(cargo_rustup_dir)"; do
    if [ -n "$cargo_dir" ] && [ -x "$cargo_dir/cargo" ]; then
      PATH="$cargo_dir:$PATH"
      break
    fi
  done
  unset cargo_dir
fi
