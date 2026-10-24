//! A test entry into the AirPlay sender's C interface, built only with the
//! `ffi-panic-probe` feature, for CI (S205): `scripts/build_core.sh` with
//! `AUDIOSYNC_CORE_PROBE=1` builds the library the app links, prelinked the
//! same way, with this entry added, and `Tests/panic-probe.c` calls it to
//! prove that a panic inside an `asc_airplay_*` call still stops at the
//! boundary in that build. It is not in `include/audiosync_core.h`, and
//! the app's library never has it.

#![warn(clippy::undocumented_unsafe_blocks)]

use crate::ffi::AscAirPlay;
use crate::ffi::airplay::live;

/// What the probe's panic says; the failure's detail must never hold it.
pub const PROBE_MESSAGE: &str = "the panic probe's own message (a test)";

/// Panics inside the guard of every call on the sender behind `airplay`.
/// Returns false, the guard's failure value, and leaves the sender dead, so
/// its next event is `AIRPLAY_EVENT_FAILED` with where the panic happened;
/// false for a null or dead handle too. True would mean the guard let a
/// call through without the panic.
///
/// # Safety
///
/// `airplay` is null or a live handle, called under the threading contract
/// of every call on it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn asc_airplay_panic_probe(airplay: *mut AscAirPlay) -> bool {
    // SAFETY: by the caller's contract null or a live handle that no other
    // call uses now.
    unsafe { live(airplay, false, |_| panic!("{PROBE_MESSAGE}")) }
}

#[cfg(test)]
mod tests {
    use std::ffi::CStr;
    use std::ptr;

    use super::*;
    use crate::ffi::{
        AIRPLAY_EVENT_FAILED, AscAirPlayEvent, asc_airplay_create, asc_airplay_destroy,
        asc_airplay_next_event,
    };

    // covers: S205
    #[test]
    fn the_probe_panics_inside_the_guard_and_the_sender_fails_saying_where() {
        let mut airplay = asc_airplay_create();
        assert!(!airplay.is_null());
        // SAFETY: a live handle, used by this thread only.
        assert!(!unsafe { asc_airplay_panic_probe(airplay) });
        let mut event = AscAirPlayEvent {
            kind: 0,
            id: ptr::null(),
            state: 0,
            error: 0,
            unsupported: 0,
            retry_ms: 0,
            latency_ms: 0.0,
            receiver_latency_frames: 0,
            restarted: false,
            detail: ptr::null(),
            volume: 0.0,
            warning: 0,
            remote: 0,
        };
        // SAFETY: as above; a local out struct, whose strings stay valid
        // until the next call on the handle.
        unsafe {
            assert!(asc_airplay_next_event(airplay, &mut event));
            assert_eq!(event.kind, AIRPLAY_EVENT_FAILED);
            let detail = CStr::from_ptr(event.detail).to_str().unwrap();
            assert!(
                detail.starts_with("panic at src/panic_probe.rs:"),
                "{detail}"
            );
            assert!(!detail.contains("own message"), "{detail}");
            assert!(!asc_airplay_panic_probe(airplay), "a dead handle");
            asc_airplay_destroy(&mut airplay);
        }
        assert!(airplay.is_null());
    }
}
