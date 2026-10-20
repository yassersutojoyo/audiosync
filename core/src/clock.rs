//! The monotonic clock the hosts stamp the capture's blocks and the
//! outputs' callbacks with (S207).
//!
//! [`now_ns`] counts nanoseconds from the clock's origin, its first use in
//! the process. Both sides of a ring must stamp with it (or with times on
//! one other shared clock): a renderer compares the capture's stamps with
//! its own.

#![forbid(unsafe_code)]

use std::sync::OnceLock;
use std::time::Instant;

static ORIGIN: OnceLock<Instant> = OnceLock::new();

/// Nanoseconds since the clock's origin, on the system's monotonic clock
/// (`std::time::Instant`). The first call sets the origin; [`Ring::new`]
/// makes it, before any audio thread runs (S128), so on the audio threads
/// a call only loads the origin and reads the clock: it never allocates,
/// locks or blocks. Saturates after some 584 years.
///
/// [`Ring::new`]: crate::ring::Ring::new
pub fn now_ns() -> u64 {
    let origin = ORIGIN.get_or_init(Instant::now);
    u64::try_from(origin.elapsed().as_nanos()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    // covers: S207
    #[test]
    fn the_clock_never_goes_back() {
        let mut last = now_ns();
        for _ in 0..1000 {
            let now = now_ns();
            assert!(now >= last);
            last = now;
        }
        std::thread::sleep(std::time::Duration::from_millis(2));
        assert!(now_ns() >= last + 1_000_000);
    }
}
