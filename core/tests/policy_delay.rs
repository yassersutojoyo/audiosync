//! Local output delays and whether an AirPlay speaker is chosen (S79, S124, S178, S179).

use audiosync_core::policy::delay::{
    DEFAULT_DELAY_MS, MAX_DELAY_MS, airplay_chosen, initial_delay_ms, sanitize_delay_ms,
};
use proptest::prelude::*;

// covers: S179
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

// covers: S124, S178
#[test]
fn non_finite_delay_input_is_ignored_and_other_input_clamped() {
    assert_eq!(sanitize_delay_ms(f64::NAN), None);
    assert_eq!(sanitize_delay_ms(f64::INFINITY), None);
    assert_eq!(sanitize_delay_ms(f64::NEG_INFINITY), None);
    assert_eq!(sanitize_delay_ms(-10.0), Some(0.0));
    assert_eq!(sanitize_delay_ms(25_000.0), Some(20_000.0));
    assert_eq!(sanitize_delay_ms(1820.0), Some(1820.0));
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

proptest! {
    // covers: S124, S178
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
