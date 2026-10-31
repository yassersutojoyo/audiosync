#!/bin/bash
# Runs every weekend cell a loopback fake television can prove, and prints
# one line per cell: <cell> PASS|FAIL|MANUAL <measured value>. The volume,
# stale row and fake Samsung cells come from the core, Windows host and
# AirPlay 2 tests that hold them (each prints a CELL line once it passed),
# the Google Cast cells from the fake Cast device in cast/tests/fake_cells.rs;
# the rest from the airplay example fake_tv_cells.
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
windows=0
case "$(uname -s)" in
    MINGW* | MSYS* | CYGWIN*)
        windows=1
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
    "$core::the_s290_vector_cases_land_on_the_television=S290.row100,S290.tv80.z64,S290.row100.z50,S290.tv80.cap,DV.100.z63,DV.80-99.z63,DV.40.z0,S413.master" \
    "$core::a_withdrawn_receiver_loses_only_its_row_and_comes_back_when_announced=Stale.row"

# Calibrate All and zones live in the Windows host's speaker list, so these cells run there;
# elsewhere the desktop crate lacks the system libraries it links (fontconfig on Linux).
held=(
    "audio::airplay::tests::the_tv_keeps_a_level_held_at_zone_0_through_calibrate_all=S290.held.z0"
    "audio::airplay::tests::the_tv_lets_a_level_held_at_zone_0_go_once_the_zone_moves=S290.held.z0.release"
)
if [ "$windows" = 1 ]; then
    cd "$root/desktop"
    cells_from_tests "--bin audiosync" "${held[@]}"
else
    for spec in "${held[@]}"; do
        printf '%-16s %-7s %s\n' "${spec#*=}" MANUAL "PC: cargo test --bin audiosync ${spec%%=*} in desktop/"
    done
fi

cd "$root/airplay"
cells_from_tests "--test ap2_volume" \
    "row_20_reaches_the_samsung_as_minus_24_db_after_the_flush_answer_without_master_volume=AP2.row20" \
    "a_new_session_starts_the_samsung_at_the_level_it_holds_without_master_volume=AP2.row20.keep"
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
    "a_slider_level_the_device_rounds_onto_its_own_is_sent_once_and_not_chased=C12.slider36" \
    "a_sender_suspended_past_the_devices_ping_timeout_reconnects_and_plays_on=C13" \
    "the_level_set_stays_shown_after_a_reconnect_whose_first_status_has_no_level=C13.level" \
    "current_time_in_any_form_changes_nothing_but_the_diagnostics=C19" \
    "txt_keys_that_change_with_the_devices_state_keep_one_entry_of_the_same_kind=C16" \
    "a_tls_1_2_only_device_back_with_a_new_certificate_is_reconnected_and_plays_on=C10" \
    "a_group_that_answers_with_a_level_of_its_own_is_shown_as_it_says_and_never_chased=C3.e"

cd "$root/airplay"
if [ "$windows" = 1 ]; then
    # Every worktree links the same example exe, which can't be relinked while one runs it (LNK1104),
    # so this run starts its own copy.
    cargo build --quiet --locked --example fake_tv_cells
    copy="$CARGO_TARGET_DIR/debug/examples/fake_tv_cells-$$.exe"
    trap 'rm -f "$copy"' EXIT
    cp "$CARGO_TARGET_DIR/debug/examples/fake_tv_cells.exe" "$copy"
    "$copy" "$@" || failed=1
else
    cargo run --quiet --locked --example fake_tv_cells -- "$@" || failed=1
fi
exit "$failed"
