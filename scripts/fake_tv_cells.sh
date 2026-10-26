#!/bin/bash
# Runs the dry-run cells the AirPlay sender can prove against the fake
# television (the airplay example fake_tv_cells) on loopback only, and prints
# one line per cell: <cell> PASS|FAIL|MANUAL <measured value>. Exits 1 when a
# cell fails.
#
#   scripts/fake_tv_cells.sh [--pause-minutes <n>]    D4 pauses 11 min unless told less
#
# On Windows it builds into the main checkout's windows/target, as
# scripts/pc_test.sh does, so worktrees don't each fill the disk.
set -euo pipefail

root=$(git rev-parse --show-toplevel)
cd "$root"

# A debug build binds the sender's sockets to loopback unless this is set.
unset AUDIOSYNC_LAN_TESTS
case "$(uname -s)" in
    MINGW* | MSYS* | CYGWIN*)
        main=$(dirname "$(git rev-parse --path-format=absolute --git-common-dir)")
        export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$main/windows/target}"
        export CARGO_BUILD_JOBS="${CARGO_BUILD_JOBS:-6}"
        export CARGO_INCREMENTAL=0
        git ls-files -z desktop core airplay cast remote | xargs -0 touch
        ;;
esac

cd airplay
exec cargo run --quiet --locked --example fake_tv_cells -- "$@"
