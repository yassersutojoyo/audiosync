#!/bin/bash
# Runs the Rust crates' checks the way CI does (cargo fmt --check, clippy with
# warnings as errors, cargo test) on a Windows desktop. Without
# AUDIOSYNC_LAN_TESTS (which only CI sets) every test socket stays on
# loopback, so Windows Firewall asks nothing about the new test programs.
#
#   scripts/pc_test.sh [crate ...]    crates: desktop core airplay remote cast
#
# All crates build into one shared target folder, the main checkout's
# windows/target (or CARGO_TARGET_DIR when set), without incremental caches,
# so worktrees don't each fill the disk. Sources are touched first, since a
# fresh worktree's files can be older than that folder's builds of another
# checkout. The airplay example fake_tv is built but never run here.
set -euo pipefail

root=$(git rev-parse --show-toplevel)
main=$(dirname "$(git rev-parse --path-format=absolute --git-common-dir)")
cd "$root"

crates=("$@")
if [ ${#crates[@]} -eq 0 ]; then
    crates=(desktop core airplay remote cast)
fi
for crate in "${crates[@]}"; do
    case "$crate" in
        desktop | core | airplay | remote | cast) ;;
        *)
            echo "unknown crate: $crate (desktop, core, airplay, remote or cast)" >&2
            exit 2
            ;;
    esac
done

unset AUDIOSYNC_LAN_TESTS
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$main/windows/target}"
export CARGO_BUILD_JOBS="${CARGO_BUILD_JOBS:-6}"
export CARGO_INCREMENTAL=0

# All five, since the crates build each other by path in the shared target.
git ls-files -z desktop core airplay cast remote | xargs -0 touch

failed=()
for crate in "${crates[@]}"; do
    echo "== $crate"
    # Chained, since errexit is off inside a tested subshell.
    if ! (
        cd "$crate" &&
            cargo fmt --check &&
            cargo clippy --locked --all-targets -- -D warnings &&
            cargo test --locked
    ); then
        failed+=("$crate")
    fi
done

if [ ${#failed[@]} -gt 0 ]; then
    echo "failed: ${failed[*]}" >&2
    exit 1
fi
echo "all passed: ${crates[*]}"
