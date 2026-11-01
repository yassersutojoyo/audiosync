//! The host's zone on a Cast device's volume (C104, C105): the arithmetic
//! both apps share, the Windows app directly and the Mac app through the
//! core. The master scales the audio instead, never the device's volume.

use audiosync_cast::status::{
    NO_ZONE_LEVEL, answers_level, device_level, own_level, row_own_level, start_own_level,
};

const NO_ZONE: f64 = NO_ZONE_LEVEL;

// covers: C105
#[test]
fn a_session_starts_from_the_own_level_kept_whatever_the_device_reports() {
    assert_eq!(
        start_own_level(NO_ZONE, Some(0.8), 0.1),
        0.8,
        "quieter device"
    );
    assert_eq!(
        start_own_level(NO_ZONE, Some(0.7), 0.8),
        0.7,
        "louder device"
    );
    assert_eq!(start_own_level(NO_ZONE, None, 0.2), 0.2, "none kept yet");
    assert_eq!(start_own_level(NO_ZONE, Some(1.25), 0.3), 1.25, "above 1");
    assert_eq!(start_own_level(NO_ZONE, Some(f64::NAN), 0.3), 0.3);
}

// covers: C105
#[test]
fn a_zoned_session_without_an_own_level_divides_by_the_zone() {
    assert!(
        (start_own_level(0.5, None, 0.2) - 0.4).abs() < 1e-9,
        "0.2 ÷ 0.5"
    );
    assert_eq!(
        start_own_level(0.0, None, 0.3),
        0.3,
        "zone 0: the level itself"
    );
    assert_eq!(start_own_level(0.5, Some(0.6), 0.1), 0.6, "one kept stays");
}

fn close(a: f64, b: f64) -> bool {
    (a - b).abs() < 1e-9
}

// covers: C104
#[test]
fn the_cast_level_is_zone_times_own() {
    assert!(close(device_level(0.5, 0.8), 0.4));
    assert_eq!(device_level(NO_ZONE, 0.8), 0.8);
    assert_eq!(device_level(0.0, 0.8), 0.0, "zone 0 sends level 0");
    assert_eq!(device_level(2.0, 0.8), 0.8, "a zone above 1 counts as 1");
    assert_eq!(device_level(0.8, 1.25), 1.0, "own above 1");
    assert_eq!(device_level(NO_ZONE, 2.0), 1.0, "at most 1 (C67)");
    assert_eq!(device_level(0.5, -1.0), 0.0);
    for bad in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
        assert_eq!(device_level(NO_ZONE, bad), 0.0, "own {bad}");
        assert_eq!(device_level(bad, 0.8), 0.0, "zone {bad}");
    }
}

// covers: C104
#[test]
fn a_cast_report_moves_own_to_level_over_the_zone() {
    assert!(close(own_level(0.2, 0.5, 0.9), 0.4), "0.2 ÷ 0.5");
    assert_eq!(own_level(0.3, NO_ZONE, 0.9), 0.3);
    assert!(close(own_level(0.6, 0.5, 0.9), 1.2), "unclamped above 1");
    assert_eq!(own_level(0.2, 0.0, 0.7), 0.7, "zone 0 leaves it");
    for bad in [f64::NAN, f64::INFINITY, -1.0] {
        assert_eq!(own_level(0.2, bad, 0.7), 0.7, "zone {bad}");
    }
    assert_eq!(own_level(f64::NAN, 0.5, 0.7), 0.7);
}

#[test]
fn a_row_move_stores_the_row_over_the_zone() {
    assert!(close(row_own_level(30.0, 0.5).unwrap(), 0.6));
    assert!(close(row_own_level(100.0, 0.8).unwrap(), 1.25));
    assert_eq!(row_own_level(40.0, NO_ZONE), Some(0.4));
    assert_eq!(row_own_level(50.0, 0.0), None, "zone 0 sends nothing");
    assert_eq!(row_own_level(f64::NAN, 0.5), None);
}

#[test]
fn a_reported_level_answers_a_sent_one_within_half_a_percent() {
    assert!(answers_level(0.4, 0.4));
    assert!(
        answers_level(0.4, 0.400_000_005_960_464_5),
        "a 32-bit float"
    );
    assert!(answers_level(0.4, 0.404));
    assert!(!answers_level(0.4, 0.41));
    assert!(!answers_level(0.4, f64::NAN));
}
