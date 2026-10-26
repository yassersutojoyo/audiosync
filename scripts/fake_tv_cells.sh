#!/bin/bash
# Runs every weekend cell a loopback fake television can prove, and prints
# one line per cell: <cell> PASS|FAIL|MANUAL <measured value>. The volume,
# stale row and fake Samsung cells come from the core and AirPlay 2 tests
# that hold them (each prints a CELL line once it passed), the Google Cast
# cells from the fake Cast device in cast/tests/fake_cells.rs; the rest from
# the airplay example fake_tv_cells.
# A cell that needs the real television prints MANUAL with what to measure.
# Exits 1 when a cell fails.
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

failed=0

# Runs the tests named in "<test>=<cell>,<cell>" specs with their CELL lines
# shown, then prints every expected cell in order: PASS with what its test
# measured, or FAIL with the first panic message when it never printed.
cells_from_tests() {
    local target=$1 spec id cells names=() ids=() out why line status=0
    shift
    for spec in "$@"; do
        names+=("${spec%%=*}")
        IFS=, read -r -a cells <<<"${spec#*=}"
        ids+=("${cells[@]}")
    done
    # The CELL lines and panics go to stderr; libtest's own lines are dropped.
    # shellcheck disable=SC2086 # $target is one or two cargo arguments.
    out=$(cargo test --quiet --locked $target -- --exact --nocapture --test-threads=1 \
        "${names[@]}" 2>&1 >/dev/null) || status=$?
    why=$(grep -A1 -m1 "panicked at" <<<"$out" | tail -n 1 || true)
    for id in "${ids[@]}"; do
        line=$(grep -m1 "^CELL $id " <<<"$out" || true)
        if [ -n "$line" ]; then
            printf '%-16s %-7s %s\n' "$id" PASS "${line#"CELL $id "}"
        else
            printf '%-16s %-7s %s\n' "$id" FAIL "${why:-its test did not pass: cargo test $target}"
            failed=1
        fi
    done
    # A panic after the last CELL line still fails the run.
    if [ "$status" -ne 0 ]; then
        echo "fake_tv_cells: cargo test $target exited $status" >&2
        failed=1
    fi
}

core=ffi::airplay::tests
cd "$root/core"
cells_from_tests --lib \
    "$core::k1_at_master_63_the_row_and_the_television_agree_both_ways=K1.m63.echo,K1.m63.tv20,K1.m63.row20" \
    "$core::k1_keep_a_row_at_40_comes_back_at_40_after_a_restart=K1.keep.m63" \
    "$core::the_s290_vector_cases_land_on_the_television=S290.row100,S290.tv80.m64,S290.row100.m50,S290.tv80.cap,DV.100.m63,DV.80-99.m63,DV.40.m0" \
    "$core::a_withdrawn_receiver_loses_only_its_row_and_comes_back_when_announced=Stale.row"

cd "$root/airplay"
cells_from_tests "--test ap2_volume" \
    "k1_row_20_reaches_the_samsung_as_minus_24_db_after_the_flush_answer=AP2.K1" \
    "k1_keep_a_new_session_starts_the_samsung_at_the_level_it_holds=AP2.K1.keep"
cells_from_tests "--test ap2_samsung" \
    "capture_switch_without_flush=W6.switch" \
    "w2_a_pause_reaches_the_samsung_as_a_flush_within_a_second=W2.flush" \
    "lo4_the_samsung_remote_play_pause_and_next_reach_the_host_once_each=LO4.dacp" \
    "progress_stays_on_the_track_time_across_a_pause_and_a_seek=Progress" \
    "l341_a_busy_or_refusing_samsung_reports_an_error_and_retries_on_the_back_off=L341" \
    "g9_the_latency_the_samsung_reports_reaches_the_host_as_r_s=G9"

cd "$root/cast"
cells_from_tests "--test fake_cells" \
    "a_device_that_rounds_levels_to_its_step_moves_one_step_per_press_and_shows_its_own_level=C12" \
    "mute_and_level_changed_on_the_device_reach_the_host_and_nothing_goes_back=C2.device-volume" \
    "a_sender_suspended_past_the_devices_ping_timeout_reconnects_and_plays_on=C13" \
    "current_time_in_any_form_changes_nothing_but_the_diagnostics=C19" \
    "txt_keys_that_change_with_the_devices_state_keep_one_entry_of_the_same_kind=C16" \
    "a_tls_1_2_only_device_back_with_a_new_certificate_is_reconnected_and_plays_on=C10" \
    "a_group_that_answers_with_a_level_of_its_own_is_shown_as_it_says_and_never_chased=C3.e"

cd "$root/airplay"
cargo run --quiet --locked --example fake_tv_cells -- "$@" || failed=1
exit "$failed"
