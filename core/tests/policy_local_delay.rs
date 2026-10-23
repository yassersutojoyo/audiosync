//! The local delay model (S172 to S179): offsets relative to a reference
//! output, hold-backs from the outputs taking part, re-timing, the AirPlay
//! latency L and the Mac's one-time migration of its stored delays.
//!
//! Every example uses the built-in speakers as the reference R, a USB
//! speaker at +150 ms, a Bluetooth speaker at −250 ms and, with an AirPlay
//! speaker chosen, L = 2000 ms (S176), unless the test says otherwise.

use audiosync_core::policy::delay::{
    DEFAULT_AIRPLAY_DELAY_MS, DelayMigration, MAX_HOLD_BACK_MS, MAX_OFFSET_MS, MIN_OFFSET_MS,
    Participant, Retimer, StoredDelay, airplay_chosen, hold_backs, migrate_mac_delays,
    native_airplay_adjustment_ms, native_airplay_latency_ms, offset_ms, sanitize_airplay_delay_ms,
    sanitize_offset_ms,
};
use proptest::prelude::*;

const L: f64 = 2000.0;

fn r() -> Participant<'static> {
    Participant {
        id: "builtin",
        offset_ms: 0.0,
        is_reference: true,
    }
}

fn usb() -> Participant<'static> {
    Participant {
        id: "usb",
        offset_ms: 150.0,
        is_reference: false,
    }
}

fn bluetooth() -> Participant<'static> {
    Participant {
        id: "bluetooth",
        offset_ms: -250.0,
        is_reference: false,
    }
}

fn output(id: &'static str, offset_ms: f64) -> Participant<'static> {
    Participant {
        id,
        offset_ms,
        is_reference: false,
    }
}

/// The hold-backs of `taking_part`, sorted by ID.
fn held(taking_part: &[Participant<'static>], airplay: Option<f64>) -> Vec<(&'static str, f64)> {
    let mut held = hold_backs(taking_part, airplay);
    held.sort_by(|a, b| a.0.cmp(b.0));
    held
}

// covers: S176
#[test]
fn only_local_outputs_a_speakers_offset_is_applied() {
    // The reported bug: with no AirPlay speaker chosen, the USB speaker
    // played at 0 like the built-in speakers and its delay did nothing.
    assert_eq!(
        held(&[r(), usb()], None),
        [("builtin", 0.0), ("usb", 150.0)]
    );
}

// covers: S172, S176
#[test]
fn a_late_bluetooth_speaker_makes_the_reference_wait() {
    assert_eq!(
        held(&[r(), bluetooth()], None),
        [("bluetooth", 0.0), ("builtin", 250.0)]
    );
    // The reference's offset is 0 whatever value is stored for it.
    let stored = Participant {
        offset_ms: 900.0,
        ..r()
    };
    assert_eq!(
        held(&[stored, bluetooth()], None),
        [("bluetooth", 0.0), ("builtin", 250.0)]
    );
}

// covers: S176
#[test]
fn the_smallest_offset_taking_part_plays_at_once_and_no_hold_back_is_negative() {
    assert_eq!(
        held(&[r(), usb(), bluetooth()], None),
        [("bluetooth", 0.0), ("builtin", 250.0), ("usb", 400.0)]
    );
    // Adding the same number to every offset changes nothing.
    let shifted = held(&[output("a", 900.0), output("b", 1150.0)], None);
    assert_eq!(shifted, [("a", 0.0), ("b", 250.0)]);
    let shifted = held(&[output("a", -2000.0), output("b", -1750.0)], None);
    assert_eq!(shifted, [("a", 0.0), ("b", 250.0)]);
}

// covers: S172, S176
#[test]
fn a_switched_off_reference_doesnt_take_part() {
    assert_eq!(
        held(&[usb(), bluetooth()], None),
        [("bluetooth", 0.0), ("usb", 400.0)]
    );
}

// covers: S176
#[test]
fn a_lone_output_plays_at_once_whatever_its_offset() {
    assert_eq!(held(&[bluetooth()], None), [("bluetooth", 0.0)]);
    assert_eq!(held(&[usb()], None), [("usb", 0.0)]);
    assert_eq!(held(&[output("tv", 20_000.0)], None), [("tv", 0.0)]);
    assert_eq!(held(&[r()], None), [("builtin", 0.0)]);
    assert!(hold_backs(&[], None).is_empty());
    assert!(hold_backs(&[], Some(L)).is_empty());
}

// covers: S176
#[test]
fn an_airplay_speaker_chosen_adds_l_on_top() {
    assert_eq!(
        held(&[r(), usb(), bluetooth()], Some(L)),
        [("bluetooth", 1750.0), ("builtin", 2000.0), ("usb", 2150.0)]
    );
    assert_eq!(held(&[bluetooth()], Some(L)), [("bluetooth", 1750.0)]);
    assert_eq!(held(&[r()], Some(L)), [("builtin", 2000.0)]);
}

// covers: S176
#[test]
fn an_output_later_than_airplay_sets_the_base() {
    // L = 1000 ms and a speaker 1500 ms late: it sets m = −1500, the local
    // outputs still line up with each other, and the AirPlay speakers lead.
    let late = output("far", -1500.0);
    assert_eq!(
        held(&[r(), usb(), late], Some(1000.0)),
        [("builtin", 1500.0), ("far", 0.0), ("usb", 1650.0)]
    );
}

// covers: S176
#[test]
fn hold_backs_are_capped_at_20000_ms() {
    assert_eq!(MAX_HOLD_BACK_MS, 20_000.0);
    assert_eq!(
        held(&[r(), output("tv", 20_000.0)], Some(L)),
        [("builtin", 2000.0), ("tv", 20_000.0)]
    );
    assert_eq!(
        held(&[output("late", -2000.0), output("tv", 20_000.0)], None),
        [("late", 0.0), ("tv", 20_000.0)]
    );
    assert_eq!(
        held(&[r(), output("tv", 19_000.0)], Some(20_000.0)),
        [("builtin", 20_000.0), ("tv", 20_000.0)]
    );
}

// covers: S175
#[test]
fn offsets_are_clamped_to_minus_2000_to_20000_and_non_finite_input_is_ignored() {
    assert_eq!((MIN_OFFSET_MS, MAX_OFFSET_MS), (-2000.0, 20_000.0));
    assert_eq!(sanitize_offset_ms(-250.0), Some(-250.0));
    assert_eq!(sanitize_offset_ms(150.0), Some(150.0));
    assert_eq!(sanitize_offset_ms(-2500.0), Some(-2000.0));
    assert_eq!(sanitize_offset_ms(25_000.0), Some(20_000.0));
    assert_eq!(sanitize_offset_ms(f64::NAN), None);
    assert_eq!(sanitize_offset_ms(f64::INFINITY), None);
    assert_eq!(sanitize_offset_ms(f64::NEG_INFINITY), None);
    // An offset out of range that reaches the hold-backs counts clamped,
    // and one that isn't finite counts as never set.
    assert_eq!(
        held(&[r(), output("x", -9000.0)], None),
        [("builtin", 2000.0), ("x", 0.0)]
    );
    assert_eq!(
        held(&[r(), output("x", f64::NAN), usb()], None),
        [("builtin", 0.0), ("usb", 150.0), ("x", 0.0)]
    );
}

// covers: S175
#[test]
fn an_output_never_set_has_offset_0() {
    assert_eq!(offset_ms(None), 0.0);
    assert_eq!(offset_ms(Some(-250.0)), -250.0);
    assert_eq!(offset_ms(Some(-3000.0)), -2000.0);
    assert_eq!(offset_ms(Some(f64::NAN)), 0.0);
}

// covers: S177
#[test]
fn a_joining_output_with_a_lower_offset_retimes_the_others() {
    let mut retimer = Retimer::default();
    assert!(
        retimer.update(&[r(), usb()], None).is_empty(),
        "outputs being attached start at their hold-back"
    );
    assert_eq!(retimer.hold_back_ms("usb"), Some(150.0));
    // The Bluetooth speaker is switched on: the other two wait 250 ms more.
    assert_eq!(
        retimer.update(&[r(), usb(), bluetooth()], None),
        [("builtin".to_owned(), 250.0), ("usb".to_owned(), 400.0)]
    );
    assert_eq!(retimer.hold_back_ms("bluetooth"), Some(0.0));
}

// covers: S177
#[test]
fn a_leaving_output_retimes_the_others_back() {
    let mut retimer = Retimer::default();
    retimer.update(&[r(), usb(), bluetooth()], None);
    assert_eq!(
        retimer.update(&[r(), usb()], None),
        [("builtin".to_owned(), 0.0), ("usb".to_owned(), 150.0)]
    );
    assert_eq!(retimer.hold_back_ms("bluetooth"), None);
}

// covers: S177
#[test]
fn a_late_speaker_switched_on_and_off_three_times_retimes_each_other_output_six_times() {
    let toggles = |airplay: Option<f64>| {
        let mut retimer = Retimer::default();
        retimer.update(&[r(), usb()], airplay);
        let mut calls = Vec::new();
        for _ in 0..3 {
            calls.extend(retimer.update(&[r(), usb(), bluetooth()], airplay));
            calls.extend(retimer.update(&[r(), usb()], airplay));
        }
        calls
    };
    let without_airplay = toggles(None);
    for id in ["builtin", "usb"] {
        let count = without_airplay.iter().filter(|(c, _)| c == id).count();
        assert_eq!(count, 6, "{id}: the speaker sets the base each time");
    }
    // With an AirPlay speaker chosen, −L is the base: nothing moves.
    assert!(toggles(Some(L)).is_empty());
}

// covers: S177
#[test]
fn only_outputs_whose_hold_back_changed_are_retimed() {
    let mut retimer = Retimer::default();
    retimer.update(&[r(), usb(), bluetooth()], None);
    assert!(
        retimer.update(&[r(), usb(), bluetooth()], None).is_empty(),
        "nothing changed: no call, no resync"
    );
    let usb_later = output("usb", 200.0);
    assert_eq!(
        retimer.update(&[r(), usb_later, bluetooth()], None),
        [("usb".to_owned(), 450.0)]
    );
    // The Bluetooth speaker's offset to −300: its own hold-back stays 0 and
    // the other two move 50 ms later.
    let later_bluetooth = output("bluetooth", -300.0);
    assert_eq!(
        retimer.update(&[r(), usb_later, later_bluetooth], None),
        [("builtin".to_owned(), 300.0), ("usb".to_owned(), 500.0)]
    );
    // Another output becomes the reference: the hold-backs follow it.
    let usb_as_reference = Participant {
        is_reference: true,
        ..usb_later
    };
    let builtin_not_reference = output("builtin", 0.0);
    assert_eq!(
        retimer.update(
            &[builtin_not_reference, usb_as_reference, later_bluetooth],
            None
        ),
        [("usb".to_owned(), 300.0)]
    );
}

// covers: S177
#[test]
fn an_airplay_flip_or_a_new_l_retimes_every_output_taking_part() {
    let mut retimer = Retimer::default();
    let all = [r(), usb(), bluetooth()];
    retimer.update(&all, None);
    assert_eq!(
        retimer.update(&all, Some(L)),
        [
            ("bluetooth".to_owned(), 1750.0),
            ("builtin".to_owned(), 2000.0),
            ("usb".to_owned(), 2150.0)
        ]
    );
    assert_eq!(
        retimer.update(&all, Some(1500.0)),
        [
            ("bluetooth".to_owned(), 1250.0),
            ("builtin".to_owned(), 1500.0),
            ("usb".to_owned(), 1650.0)
        ]
    );
    assert_eq!(
        retimer.update(&all, None),
        [
            ("bluetooth".to_owned(), 0.0),
            ("builtin".to_owned(), 250.0),
            ("usb".to_owned(), 400.0)
        ]
    );
}

// covers: S81, S177
#[test]
fn a_listed_set_kept_through_failed_polls_retimes_nothing() {
    // A poll that can't read the AirPlay speakers keeps the listed set as it
    // was (S81), so the AirPlay condition and every hold-back stay put.
    let mut retimer = Retimer::default();
    let all = [r(), usb(), bluetooth()];
    let listed = ["7".to_owned()];
    let latency = |listed: &[String]| airplay_chosen(&["7"], listed).then_some(L);
    retimer.update(&all, latency(&listed));
    assert_eq!(retimer.hold_back_ms("builtin"), Some(2000.0));
    for _ in 0..3 {
        assert!(
            retimer.update(&all, latency(&listed)).is_empty(),
            "no re-timing while the speakers can't be read"
        );
    }
}

// covers: S178
#[test]
fn the_native_senders_l_is_its_latency_plus_the_adjustment_clamped() {
    assert_eq!(native_airplay_latency_ms(1750.0, 0.0), 1750.0);
    assert_eq!(native_airplay_latency_ms(1750.0, 250.0), 2000.0);
    assert_eq!(native_airplay_latency_ms(1750.0, -2000.0), 0.0);
    assert_eq!(native_airplay_latency_ms(19_000.0, 5000.0), 20_000.0);
    // An adjustment out of range counts clamped, a non-finite one as 0.
    assert_eq!(native_airplay_latency_ms(1750.0, -9000.0), 0.0);
    assert_eq!(native_airplay_latency_ms(1750.0, f64::NAN), 1750.0);
}

// covers: S178
#[test]
fn an_entered_airplay_delay_stores_the_difference_to_the_senders_latency() {
    // The field shows L = 1750 + A; entering 2000 stores A = 250.
    assert_eq!(native_airplay_adjustment_ms(2000.0, 1750.0), Some(250.0));
    // A is relative: with a new L_s of 1500 the field shows 1750.
    assert_eq!(native_airplay_latency_ms(1500.0, 250.0), 1750.0);
    assert_eq!(native_airplay_adjustment_ms(0.0, 1750.0), Some(-1750.0));
    assert_eq!(native_airplay_adjustment_ms(-5000.0, 1750.0), Some(-2000.0));
    assert_eq!(native_airplay_adjustment_ms(90_000.0, 0.0), Some(20_000.0));
    assert_eq!(native_airplay_adjustment_ms(f64::NAN, 1750.0), None);
    assert_eq!(native_airplay_adjustment_ms(2000.0, f64::NAN), None);
}

// covers: S178
#[test]
fn the_bundled_engines_airplay_delay_is_0_to_20000_ms_and_defaults_to_2000() {
    assert_eq!(DEFAULT_AIRPLAY_DELAY_MS, 2000.0);
    assert_eq!(sanitize_airplay_delay_ms(2300.0), Some(2300.0));
    assert_eq!(sanitize_airplay_delay_ms(-5.0), Some(0.0));
    assert_eq!(sanitize_airplay_delay_ms(25_000.0), Some(20_000.0));
    assert_eq!(sanitize_airplay_delay_ms(f64::NAN), None);
}

/// A stored entry: the reference, or another UID, with or without a stored
/// delay.
fn stored(is_reference: bool, delay_ms: Option<f64>) -> StoredDelay {
    StoredDelay {
        is_reference,
        stored_delay_ms: delay_ms,
    }
}

/// The hold-backs of the migrated outputs, in the order given, all taking
/// part; the reference first in every example.
fn migrated_hold_backs(migration: &DelayMigration, airplay: bool) -> Vec<f64> {
    let ids = ["r", "a", "b", "c"];
    let taking_part: Vec<Participant> = migration
        .offsets_ms
        .iter()
        .enumerate()
        .map(|(index, offset)| Participant {
            id: ids[index],
            offset_ms: offset_ms(*offset),
            is_reference: index == 0,
        })
        .collect();
    hold_backs(&taking_part, airplay.then_some(migration.airplay_delay_ms))
        .into_iter()
        .map(|(_, held)| held)
        .collect()
}

// covers: S179
#[test]
fn migration_a_setup_tuned_for_airplay_sounds_the_same_with_airplay() {
    // R 2000, USB 2100, Bluetooth 1800, D0 2000.
    let migration = migrate_mac_delays(
        &[
            stored(true, Some(2000.0)),
            stored(false, Some(2100.0)),
            stored(false, Some(1800.0)),
        ],
        None,
    );
    assert_eq!(migration.airplay_delay_ms, 2000.0);
    assert_eq!(migration.offsets_ms, [None, Some(100.0), Some(-200.0)]);
    assert_eq!(
        migrated_hold_backs(&migration, true),
        [2000.0, 2100.0, 1800.0]
    );
    assert_eq!(migrated_hold_backs(&migration, false), [200.0, 300.0, 0.0]);
}

// covers: S179
#[test]
fn migration_the_references_old_delay_becomes_the_airplay_delay() {
    // R 2500, Bluetooth 2250.
    let migration = migrate_mac_delays(
        &[stored(true, Some(2500.0)), stored(false, Some(2250.0))],
        None,
    );
    assert_eq!(migration.airplay_delay_ms, 2500.0);
    assert_eq!(migration.offsets_ms, [None, Some(-250.0)]);
    assert_eq!(migrated_hold_backs(&migration, true), [2500.0, 2250.0]);
    assert_eq!(migrated_hold_backs(&migration, false), [250.0, 0.0]);
}

// covers: S179
#[test]
fn migration_r_without_a_stored_delay_uses_d0() {
    // R none, HDMI TV 2400, D0 2000.
    let migration = migrate_mac_delays(&[stored(true, None), stored(false, Some(2400.0))], None);
    assert_eq!(migration.airplay_delay_ms, 2000.0);
    assert_eq!(migration.offsets_ms, [None, Some(400.0)]);
    assert_eq!(migrated_hold_backs(&migration, true), [2000.0, 2400.0]);
    assert_eq!(migrated_hold_backs(&migration, false), [0.0, 400.0]);
    // No reference at all: D_R is D0 too.
    let migration = migrate_mac_delays(&[stored(false, Some(2400.0))], Some(2000.0));
    assert_eq!(migration.airplay_delay_ms, 2000.0);
    assert_eq!(migration.offsets_ms, [Some(400.0)]);
}

// covers: S179
#[test]
fn migration_an_untuned_output_that_played_at_d0_sounds_the_same_with_airplay() {
    // R 2300, USB without a stored delay (listed), D0 2000.
    let migration = migrate_mac_delays(&[stored(true, Some(2300.0)), stored(false, None)], None);
    assert_eq!(migration.airplay_delay_ms, 2300.0);
    assert_eq!(migration.offsets_ms, [None, Some(-300.0)]);
    assert_eq!(migrated_hold_backs(&migration, true), [2300.0, 2000.0]);
    assert_eq!(migrated_hold_backs(&migration, false), [300.0, 0.0]);
}

// covers: S179
#[test]
fn migration_with_a_legacy_delay_below_1000_ms_untuned_outputs_keep_playing_together() {
    // R none, USB none (listed), the legacy single delay 0: D0 = 0.
    let migration = migrate_mac_delays(&[stored(true, None), stored(false, None)], Some(0.0));
    assert_eq!(migration.airplay_delay_ms, DEFAULT_AIRPLAY_DELAY_MS);
    assert_eq!(migration.offsets_ms, [None, Some(0.0)]);
    assert_eq!(migrated_hold_backs(&migration, true), [2000.0, 2000.0]);
    assert_eq!(migrated_hold_backs(&migration, false), [0.0, 0.0]);
    // D0 of 600 with R stored at 600: the untuned output stays with R.
    let migration = migrate_mac_delays(
        &[stored(true, Some(600.0)), stored(false, None)],
        Some(600.0),
    );
    assert_eq!(migration.airplay_delay_ms, 2000.0);
    assert_eq!(migration.offsets_ms, [None, Some(0.0)]);
}

// covers: S179
#[test]
fn migration_the_native_senders_adjustment_starts_at_0() {
    for entries in [
        vec![stored(true, Some(2500.0))],
        vec![stored(true, Some(250.0)), stored(false, Some(0.0))],
        vec![],
    ] {
        assert_eq!(migrate_mac_delays(&entries, None).native_adjustment_ms, 0.0);
    }
}

// covers: S179
#[test]
fn migration_small_values_keep_their_alignment_and_take_the_default_airplay_delay() {
    // R 250, Bluetooth 0, USB 400, D0 2000.
    let migration = migrate_mac_delays(
        &[
            stored(true, Some(250.0)),
            stored(false, Some(0.0)),
            stored(false, Some(400.0)),
        ],
        None,
    );
    assert_eq!(migration.airplay_delay_ms, 2000.0);
    assert_eq!(migration.offsets_ms, [None, Some(-250.0), Some(150.0)]);
    assert_eq!(
        migrated_hold_backs(&migration, true),
        [2000.0, 1750.0, 2150.0]
    );
    assert_eq!(migrated_hold_backs(&migration, false), [250.0, 0.0, 400.0]);
}

// covers: S179
#[test]
fn migration_an_offset_below_minus_2000_ms_is_clamped() {
    // R 2500, USB 0.
    let migration = migrate_mac_delays(
        &[stored(true, Some(2500.0)), stored(false, Some(0.0))],
        None,
    );
    assert_eq!(migration.airplay_delay_ms, 2500.0);
    assert_eq!(migration.offsets_ms, [None, Some(-2000.0)]);
    assert_eq!(migrated_hold_backs(&migration, true), [2500.0, 500.0]);
    assert_eq!(migrated_hold_backs(&migration, false), [2000.0, 0.0]);
}

// covers: S179
#[test]
fn migration_reads_stored_delays_as_s77_kept_them() {
    // A stored delay out of range counts clamped to 0 to 20 000, one that
    // isn't finite as none; the legacy delay the same way.
    let migration = migrate_mac_delays(
        &[
            stored(true, Some(25_000.0)),
            stored(false, Some(f64::NAN)),
            stored(false, Some(-40.0)),
        ],
        Some(f64::INFINITY),
    );
    assert_eq!(migration.airplay_delay_ms, 20_000.0);
    assert_eq!(
        migration.offsets_ms,
        [None, Some(-2000.0), Some(-2000.0)],
        "D0 is 2000 with the legacy delay ignored; −18 000 and −20 000 clamp"
    );
}

proptest! {
    // covers: S175
    #[test]
    fn sanitized_offsets_are_finite_and_in_range(ms in proptest::num::f64::ANY) {
        match sanitize_offset_ms(ms) {
            Some(clean) => {
                prop_assert!(ms.is_finite());
                prop_assert!((MIN_OFFSET_MS..=MAX_OFFSET_MS).contains(&clean));
                if (MIN_OFFSET_MS..=MAX_OFFSET_MS).contains(&ms) {
                    prop_assert_eq!(clean, ms);
                }
            }
            None => prop_assert!(!ms.is_finite()),
        }
    }

    // covers: S176
    #[test]
    fn hold_backs_are_in_range_and_keep_the_offsets_apart(
        offsets in proptest::collection::vec(-2000.0f64..=20_000.0, 1..6),
        latency in proptest::option::of(0.0f64..=20_000.0),
    ) {
        let ids = ["a", "b", "c", "d", "e"];
        let taking_part: Vec<Participant> = offsets
            .iter()
            .enumerate()
            .map(|(index, &offset_ms)| Participant { id: ids[index], offset_ms, is_reference: false })
            .collect();
        let held = hold_backs(&taking_part, latency);
        prop_assert_eq!(held.len(), offsets.len());
        let base = offsets
            .iter()
            .copied()
            .chain(latency.map(|l| -l))
            .fold(f64::INFINITY, f64::min);
        for ((_, h), offset) in held.iter().zip(&offsets) {
            prop_assert!((0.0..=MAX_HOLD_BACK_MS).contains(h));
            prop_assert_eq!(*h, (offset - base).min(MAX_HOLD_BACK_MS));
        }
        if latency.is_none() {
            prop_assert!(held.iter().any(|(_, h)| *h == 0.0), "the earliest plays at once");
        }
    }

    // covers: S176
    #[test]
    fn without_airplay_adding_the_same_number_to_every_offset_changes_nothing(
        offsets in proptest::collection::vec(-2000i32..=9000, 1..6),
        shift in 0i32..=11_000,
    ) {
        let ids = ["a", "b", "c", "d", "e"];
        let taking_part = |shift: i32| -> Vec<Participant> {
            offsets
                .iter()
                .enumerate()
                .map(|(index, &offset)| output(ids[index], f64::from(offset + shift)))
                .collect()
        };
        prop_assert_eq!(hold_backs(&taking_part(shift), None), hold_backs(&taking_part(0), None));
    }
}

// covers: S179
#[test]
fn migration_takes_a_reference_delay_of_1000_ms_as_tuned_for_airplay() {
    let at = |reference_ms: f64| {
        migrate_mac_delays(
            &[
                stored(true, Some(reference_ms)),
                stored(false, Some(1200.0)),
            ],
            None,
        )
    };
    let tuned = at(1000.0);
    assert_eq!(tuned.airplay_delay_ms, 1000.0);
    assert_eq!(tuned.offsets_ms, [None, Some(200.0)]);
    let untuned = at(999.0);
    assert_eq!(untuned.airplay_delay_ms, DEFAULT_AIRPLAY_DELAY_MS);
    assert_eq!(untuned.offsets_ms, [None, Some(201.0)]);
    // D0 at 1000 ms counts as tuned too: an output without a stored delay
    // keeps its D0 sound against the new AirPlay delay.
    let migration = migrate_mac_delays(&[stored(true, None), stored(false, None)], Some(1000.0));
    assert_eq!(migration.airplay_delay_ms, 1000.0);
    assert_eq!(migration.offsets_ms, [None, Some(0.0)]);
}

// covers: S179
#[test]
fn migration_counts_only_the_first_entry_marked_as_the_reference() {
    let migration = migrate_mac_delays(
        &[
            stored(false, Some(2400.0)),
            stored(true, Some(2500.0)),
            stored(true, Some(1500.0)),
        ],
        None,
    );
    assert_eq!(migration.airplay_delay_ms, 2500.0);
    assert_eq!(migration.offsets_ms, [Some(-100.0), None, Some(-1000.0)]);
}
