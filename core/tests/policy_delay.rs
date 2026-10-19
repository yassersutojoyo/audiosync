//! Local output delays and the AirPlay sync rule (S8, S77 to S81, S124).

use audiosync_core::policy::delay::{
    DEFAULT_DELAY_MS, MAX_DELAY_MS, SyncRule, airplay_chosen, effective_delay_ms, initial_delay_ms,
    sanitize_delay_ms,
};
use proptest::prelude::*;

// covers: S77
#[test]
fn a_configured_delay_is_0_to_20000_ms_and_defaults_to_2000() {
    assert_eq!(DEFAULT_DELAY_MS, 2000.0);
    assert_eq!(MAX_DELAY_MS, 20_000.0);
    assert_eq!(initial_delay_ms(None), 2000.0);
    assert_eq!(initial_delay_ms(Some(1500.0)), 1500.0);
    assert_eq!(
        initial_delay_ms(Some(90_000.0)),
        20_000.0,
        "a legacy delay is clamped"
    );
    assert_eq!(initial_delay_ms(Some(-3.0)), 0.0);
    assert_eq!(initial_delay_ms(Some(f64::NAN)), 2000.0);
}

// covers: S77, S124
#[test]
fn non_finite_delay_input_is_ignored_and_other_input_clamped() {
    assert_eq!(sanitize_delay_ms(f64::NAN), None);
    assert_eq!(sanitize_delay_ms(f64::INFINITY), None);
    assert_eq!(sanitize_delay_ms(f64::NEG_INFINITY), None);
    assert_eq!(sanitize_delay_ms(-10.0), Some(0.0));
    assert_eq!(sanitize_delay_ms(25_000.0), Some(20_000.0));
    assert_eq!(sanitize_delay_ms(1820.0), Some(1820.0));
}

// covers: S8, S78
#[test]
fn delay_airplay_chosen_gives_the_configured_delay() {
    assert_eq!(effective_delay_ms(1820.0, true), 1820.0);
}

// covers: S8, S78
#[test]
fn delay_no_airplay_plays_at_once_and_zero_stays_zero() {
    assert_eq!(effective_delay_ms(1820.0, false), 0.0);
    assert_eq!(effective_delay_ms(0.0, true), 0.0);
    assert_eq!(effective_delay_ms(0.0, false), 0.0);
    // The configured value is kept either way.
    let mut rule = SyncRule::default();
    let configured = 2500.0;
    assert_eq!(rule.effective_delay_ms(configured), 0.0);
    rule.update(&["7"], &["7"]);
    assert_eq!(rule.effective_delay_ms(configured), 2500.0);
}

// covers: S79
#[test]
fn only_a_chosen_and_listed_airplay_speaker_counts() {
    let none: [&str; 0] = [];
    assert!(
        !airplay_chosen(&["7"], &["3"]),
        "a chosen speaker not listed today doesn't count"
    );
    assert!(
        airplay_chosen(&["7", "9"], &["3", "9"]),
        "chosen and listed counts"
    );
    assert!(
        !airplay_chosen(&none, &["3", "9"]),
        "listed but not chosen doesn't count"
    );
    assert!(!airplay_chosen(&["7"], &none));
}

// covers: S80
#[test]
fn every_running_output_is_retimed_when_the_condition_flips() {
    let mut rule = SyncRule::default();
    let outputs = [("lc:usb", 2000.0), ("lc:hdmi", 1500.0)];
    assert!(!rule.update(&["7"], &["3"]), "nothing chosen yet: no flip");
    assert!(
        rule.update(&["7"], &["7", "3"]),
        "a chosen speaker appears: flip"
    );
    assert_eq!(
        rule.retime(&outputs),
        [("lc:usb", 2000.0), ("lc:hdmi", 1500.0)]
    );
    assert!(
        !rule.update(&["7", "3"], &["7", "3"]),
        "still chosen: no flip"
    );
    assert!(
        rule.update(&["3"], &["7"]),
        "the enabled set changed: flip to 0"
    );
    assert_eq!(rule.retime(&outputs), [("lc:usb", 0.0), ("lc:hdmi", 0.0)]);
}

// covers: S81
#[test]
fn a_listed_set_kept_through_failed_polls_leaves_the_local_delays_alone() {
    // A poll that can't read the AirPlay speakers keeps the listed set as it
    // was (S81): the rule sees the same set again, so nothing flips.
    let mut rule = SyncRule::default();
    let listed = ["7".to_owned()];
    assert!(rule.update(&["7"], &listed));
    for _ in 0..3 {
        assert!(
            !rule.update(&["7"], &listed),
            "no flap while the speakers can't be read"
        );
        assert_eq!(rule.effective_delay_ms(2000.0), 2000.0);
    }
}

proptest! {
    // covers: S77, S124
    #[test]
    fn sanitized_delays_are_finite_and_in_range(ms in proptest::num::f64::ANY) {
        match sanitize_delay_ms(ms) {
            Some(clean) => {
                prop_assert!(ms.is_finite());
                prop_assert!((0.0..=MAX_DELAY_MS).contains(&clean));
                if (0.0..=MAX_DELAY_MS).contains(&ms) {
                    prop_assert_eq!(clean, ms);
                }
            }
            None => prop_assert!(!ms.is_finite()),
        }
    }
}
