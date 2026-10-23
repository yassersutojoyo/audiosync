//! The host's master on a Cast device's volume (C104, C105): the arithmetic
//! both apps share, the Windows app directly and the Mac app through the
//! core.

use audiosync_cast::status::{answers_level, device_level, own_level, start_own_level};

// covers: C105
#[test]
fn a_session_starts_from_the_own_level_kept_whatever_the_device_reports() {
    assert_eq!(start_own_level(1.0, Some(0.8), 0.1), 0.8, "quieter device");
    assert_eq!(start_own_level(0.5, Some(0.7), 0.8), 0.7, "louder device");
    assert!(
        (start_own_level(0.5, None, 0.2) - 0.4).abs() < 1e-9,
        "none kept yet: the reported level by the master"
    );
    assert_eq!(start_own_level(0.0, None, 0.7), 0.7, "none kept, master 0");
    assert_eq!(start_own_level(0.5, None, 0.8), 1.0);
    assert_eq!(start_own_level(1.0, Some(f64::NAN), 0.3), 0.3);
}

fn close(a: f64, b: f64) -> bool {
    (a - b).abs() < 1e-9
}

// covers: C104
#[test]
fn the_level_sent_is_the_master_times_the_own_level() {
    assert_eq!(device_level(0.0, 0.8), 0.0, "master 0 sends level 0");
    assert!(close(device_level(0.5, 0.8), 0.4));
    assert_eq!(device_level(1.0, 0.8), 0.8);
    assert_eq!(device_level(1.5, 2.0), 1.0, "clamped (C67)");
    assert_eq!(device_level(0.5, -1.0), 0.0);
    for bad in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
        assert_eq!(device_level(bad, 0.8), 0.0, "master {bad}");
        assert_eq!(device_level(0.8, bad), 0.0, "own {bad}");
    }
}

#[test]
fn a_level_from_elsewhere_moves_the_own_level_by_the_master() {
    assert!(close(own_level(0.2, 0.5, 0.9), 0.4));
    assert!(close(own_level(0.3, 1.0, 0.9), 0.3));
    assert_eq!(
        own_level(0.8, 0.5, 0.9),
        1.0,
        "louder than the master allows"
    );
    assert_eq!(own_level(0.2, 0.0, 0.7), 0.7, "master 0 leaves it");
    for bad in [f64::NAN, f64::INFINITY, -1.0] {
        assert_eq!(own_level(0.2, bad, 0.7), 0.7, "master {bad}");
    }
    assert_eq!(own_level(f64::NAN, 0.5, 0.7), 0.7);
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
