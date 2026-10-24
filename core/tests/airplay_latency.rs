//! The AirPlay latency rule through the C interface (AirPlay spec P200 to
//! P202): the session's Lf from the speakers' reported latencies, and the
//! sender's latency setter.

use std::ptr;
use std::thread;
use std::time::{Duration, Instant};

use audiosync_airplay::latency::session_latency_frames;
use audiosync_core::ffi::airplay::{
    asc_airplay_create, asc_airplay_destroy, asc_airplay_latency_ms,
    asc_airplay_session_latency_frames, asc_airplay_set_latency,
};

fn c_rule(reported_us: &[i64]) -> u32 {
    // SAFETY: a slice of `len` values.
    unsafe { asc_airplay_session_latency_frames(reported_us.as_ptr(), reported_us.len()) }
}

#[test]
fn the_c_session_latency_matches_the_crate() {
    // SAFETY: a null array is allowed.
    assert_eq!(
        unsafe { asc_airplay_session_latency_frames(ptr::null(), 3) },
        77_175
    );
    assert_eq!(c_rule(&[]), 77_175);
    assert_eq!(c_rule(&[-1, -400_000]), 77_175);
    let cases: [&[i64]; 5] = [
        &[250_000, 400_000],
        &[1_300_000],
        &[200_000, 1_300_000],
        &[1_600_000],
        &[0, -5, 1_300_000],
    ];
    for reported in cases {
        let crate_value =
            session_latency_frames(reported.iter().map(|&us| u64::try_from(us).unwrap_or(0)));
        assert_eq!(c_rule(reported), crate_value, "{reported:?}");
    }
    assert_eq!(c_rule(&[1_300_000]), 79_380);
    assert_eq!(c_rule(&[1_600_000]), 88_200);
}

/// Waits up to 5 s for the sender's L to read `ms`.
fn latency_becomes(handle: *mut audiosync_core::ffi::airplay::AscAirPlay, ms: f64) -> bool {
    let until = Instant::now() + Duration::from_secs(5);
    while Instant::now() < until {
        // SAFETY: a live handle, used by this thread only.
        if (unsafe { asc_airplay_latency_ms(handle) } - ms).abs() < 0.01 {
            return true;
        }
        thread::sleep(Duration::from_millis(10));
    }
    false
}

#[test]
fn set_latency_clamps_and_ignores_a_null_handle() {
    // SAFETY: a null handle does nothing.
    unsafe { asc_airplay_set_latency(ptr::null_mut(), 44_100) };
    let mut handle = asc_airplay_create();
    assert!(!handle.is_null());
    assert!(latency_becomes(handle, 1750.0), "1.75 s by default");
    // With no speaker sounding the value applies at once (P201).
    for (frames, ms) in [(5_000, 250.0), (200_000, 2000.0), (44_100, 1000.0)] {
        // SAFETY: a live handle, used by this thread only.
        unsafe { asc_airplay_set_latency(handle, frames) };
        assert!(latency_becomes(handle, ms), "{frames} frames");
    }
    // SAFETY: destroyed once, by this thread.
    unsafe { asc_airplay_destroy(&mut handle) };
}
