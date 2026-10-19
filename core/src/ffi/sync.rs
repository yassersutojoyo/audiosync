//! The AirPlay sync rule and the delay helpers through the C interface (S3,
//! S77 to S80, S124): pure functions for the control thread.
//!
//! The rule itself is [`delay::airplay_chosen`] and [`delay::effective_delay_ms`];
//! this module only reads the host's C strings. It may allocate (the two ID
//! lists), so it never runs on an audio thread.

use std::ffi::{CStr, c_char};

use super::host_slice;
use crate::policy::delay;
use crate::reader;

/// The inputs of the sync rule: the AirPlay output IDs the user enabled, the
/// ones the engine lists now, and the previous decision.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct AscSyncInput {
    pub enabled_ids: *const *const c_char,
    pub enabled_count: usize,
    pub listed_ids: *const *const c_char,
    pub listed_count: usize,
    pub was_chosen: bool,
}

/// The sync rule's decision.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AscSyncDecision {
    pub airplay_chosen: bool,
    pub flipped: bool,
}

/// The IDs of a C array of C strings. A null array or entry, and an entry
/// that isn't UTF-8, count as no ID.
///
/// # Safety
///
/// `ids` is null or points to `count` pointers, each null or pointing to a
/// NUL-terminated string, all unchanged during `'a`.
unsafe fn ids<'a>(ids: *const *const c_char, count: usize) -> Vec<&'a str> {
    // SAFETY: by the caller's contract the array is null or holds `count`
    // readable pointers.
    let Some(entries) = (unsafe { host_slice(ids, count) }) else {
        return Vec::new();
    };
    entries
        .iter()
        .filter(|entry| !entry.is_null())
        // SAFETY: by the caller's contract each non-null entry points to a
        // NUL-terminated string that stays unchanged during 'a.
        .filter_map(|&entry| unsafe { CStr::from_ptr(entry) }.to_str().ok())
        .collect()
}

/// Decides whether an AirPlay speaker is chosen (S79) and whether that
/// flipped since `was_chosen` (S80). A null input gives all false.
///
/// # Safety
///
/// `input` is null or points to an [`AscSyncInput`] whose arrays follow the
/// header's rules, all unchanged during the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn asc_sync_decide(input: *const AscSyncInput) -> AscSyncDecision {
    // SAFETY: by the caller's contract the input is null or readable.
    let Some(input) = (unsafe { input.as_ref() }) else {
        return AscSyncDecision::default();
    };
    // SAFETY: by the caller's contract both arrays follow `ids`' rules.
    let (enabled, listed) = unsafe {
        (
            ids(input.enabled_ids, input.enabled_count),
            ids(input.listed_ids, input.listed_count),
        )
    };
    let chosen = delay::airplay_chosen(&enabled, &listed);
    AscSyncDecision {
        airplay_chosen: chosen,
        flipped: chosen != input.was_chosen,
    }
}

/// [`delay::effective_delay_ms`] (S78).
#[unsafe(no_mangle)]
pub extern "C" fn asc_sync_effective_delay_ms(configured_ms: f64, airplay_chosen: bool) -> f64 {
    delay::effective_delay_ms(configured_ms, airplay_chosen)
}

/// [`delay::sanitize_delay_ms`] (S77, S124): false, writing nothing, for a
/// non-finite delay or a null `sanitized_ms`.
///
/// # Safety
///
/// `sanitized_ms` is null or points to an aligned, writable double that
/// nothing else touches during the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn asc_delay_sanitize_ms(delay_ms: f64, sanitized_ms: *mut f64) -> bool {
    let Some(sanitized) = delay::sanitize_delay_ms(delay_ms) else {
        return false;
    };
    if sanitized_ms.is_null() {
        return false;
    }
    // SAFETY: non-null, and by the caller's contract aligned, writable and
    // not touched by anyone else.
    unsafe { sanitized_ms.write(sanitized) };
    true
}

/// [`reader::delay_ms_to_frames`] (S3): false, writing nothing, for a
/// non-finite delay or a null `frames`.
///
/// # Safety
///
/// `frames` is null or points to an aligned, writable `int64_t` that nothing
/// else touches during the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn asc_delay_ms_to_frames(
    delay_ms: f64,
    rate: u32,
    frames: *mut i64,
) -> bool {
    let Some(value) = reader::delay_ms_to_frames(delay_ms, rate) else {
        return false;
    };
    if frames.is_null() {
        return false;
    }
    // SAFETY: non-null, and by the caller's contract aligned, writable and
    // not touched by anyone else.
    unsafe { frames.write(value) };
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::delay::SyncRule;
    use std::ffi::CString;
    use std::ptr;

    /// Runs the C rule over owned strings.
    fn decide(enabled: &[&str], listed: &[&str], was_chosen: bool) -> AscSyncDecision {
        let enabled: Vec<CString> = enabled
            .iter()
            .map(|id| CString::new(*id).unwrap())
            .collect();
        let listed: Vec<CString> = listed.iter().map(|id| CString::new(*id).unwrap()).collect();
        let enabled_ptrs: Vec<*const c_char> = enabled.iter().map(|id| id.as_ptr()).collect();
        let listed_ptrs: Vec<*const c_char> = listed.iter().map(|id| id.as_ptr()).collect();
        let input = AscSyncInput {
            enabled_ids: enabled_ptrs.as_ptr(),
            enabled_count: enabled_ptrs.len(),
            listed_ids: listed_ptrs.as_ptr(),
            listed_count: listed_ptrs.len(),
            was_chosen,
        };
        // SAFETY: every pointer refers to a live NUL-terminated string.
        unsafe { asc_sync_decide(&input) }
    }

    // covers: S79
    #[test]
    fn a_speaker_counts_only_when_enabled_and_listed() {
        assert!(decide(&["40"], &["12", "40"], false).airplay_chosen);
        assert!(
            !decide(&["40"], &["12"], false).airplay_chosen,
            "not listed today"
        );
        assert!(
            !decide(&[], &["12", "40"], false).airplay_chosen,
            "listed, not chosen"
        );
        assert!(!decide(&[], &[], false).airplay_chosen);
        assert!(
            !decide(&["4"], &["40"], false).airplay_chosen,
            "whole IDs only"
        );
    }

    // covers: S80, S81
    #[test]
    fn the_rule_reports_flips_like_the_rust_one() {
        let steps: [(&[&str], &[&str]); 6] = [
            (&["1"], &[]),
            (&["1"], &["1", "2"]),
            (&["1", "2"], &["1", "2"]),
            // An unreachable engine leaves the listed set as it was: no flip.
            (&["1", "2"], &["1", "2"]),
            (&["2"], &["1"]),
            (&["1"], &["1"]),
        ];
        let mut rule = SyncRule::default();
        let mut was_chosen = false;
        for (enabled, listed) in steps {
            let flipped = rule.update(enabled, listed);
            let decision = decide(enabled, listed, was_chosen);
            assert_eq!(
                decision,
                AscSyncDecision {
                    airplay_chosen: rule.chosen(),
                    flipped,
                }
            );
            was_chosen = decision.airplay_chosen;
        }
    }

    // covers: S78
    #[test]
    fn effective_delays_follow_the_rule() {
        assert_eq!(asc_sync_effective_delay_ms(1820.0, true), 1820.0);
        assert_eq!(asc_sync_effective_delay_ms(1820.0, false), 0.0);
        assert_eq!(asc_sync_effective_delay_ms(0.0, true), 0.0);
    }

    #[test]
    fn null_arrays_entries_and_non_utf8_ids_count_as_none() {
        let id = CString::new("7").unwrap();
        let bad = CString::new(vec![0xffu8, 0xfe]).unwrap();
        let entries = [ptr::null(), bad.as_ptr(), id.as_ptr()];
        let both_bad = [bad.as_ptr()];
        let input = AscSyncInput {
            enabled_ids: entries.as_ptr(),
            enabled_count: 3,
            listed_ids: both_bad.as_ptr(),
            listed_count: 1,
            was_chosen: true,
        };
        // SAFETY: every pointer is null or refers to a live string.
        let decision = unsafe { asc_sync_decide(&input) };
        assert_eq!(
            decision,
            AscSyncDecision {
                airplay_chosen: false,
                flipped: true,
            }
        );
        let input = AscSyncInput {
            listed_ids: ptr::null(),
            listed_count: 5,
            ..input
        };
        // SAFETY: as above; a null array is allowed with any count.
        assert!(!unsafe { asc_sync_decide(&input) }.airplay_chosen);
        let input = AscSyncInput {
            listed_ids: entries.as_ptr(),
            listed_count: 3,
            ..input
        };
        // SAFETY: as above.
        assert!(unsafe { asc_sync_decide(&input) }.airplay_chosen);
        // SAFETY: a null input is allowed.
        let none = unsafe { asc_sync_decide(ptr::null()) };
        assert_eq!(none, AscSyncDecision::default());
    }

    // covers: S77, S124
    #[test]
    fn delay_inputs_are_sanitized() {
        let mut out = -1.0;
        // SAFETY: `out` is a live double; null is allowed.
        unsafe {
            assert!(asc_delay_sanitize_ms(-5.0, &mut out));
            assert_eq!(out, 0.0);
            assert!(asc_delay_sanitize_ms(25_000.0, &mut out));
            assert_eq!(out, 20_000.0);
            assert!(asc_delay_sanitize_ms(1820.5, &mut out));
            assert_eq!(out, 1820.5);
            assert!(!asc_delay_sanitize_ms(f64::NAN, &mut out));
            assert!(!asc_delay_sanitize_ms(f64::INFINITY, &mut out));
            assert_eq!(out, 1820.5, "an ignored input writes nothing");
            assert!(!asc_delay_sanitize_ms(100.0, ptr::null_mut()));
        }
    }

    // covers: S172, S176
    #[test]
    fn hold_backs_through_the_c_interface_match_the_rust_ones() {
        let outputs = [
            AscLocalOutput {
                offset_ms: 900.0,
                is_reference: true,
            },
            AscLocalOutput {
                offset_ms: 150.0,
                is_reference: false,
            },
            AscLocalOutput {
                offset_ms: -250.0,
                is_reference: false,
            },
        ];
        let mut held = [f64::NAN; 3];
        // SAFETY: both arrays hold three entries; null is allowed with a
        // count of 0.
        unsafe {
            assert!(asc_local_hold_backs_ms(
                outputs.as_ptr(),
                3,
                false,
                2000.0,
                held.as_mut_ptr()
            ));
            assert_eq!(held, [250.0, 400.0, 0.0], "only local outputs");
            assert!(asc_local_hold_backs_ms(
                outputs.as_ptr(),
                3,
                true,
                2000.0,
                held.as_mut_ptr()
            ));
            assert_eq!(held, [2000.0, 2150.0, 1750.0], "L on top");
            assert!(asc_local_hold_backs_ms(
                outputs[1..].as_ptr(),
                1,
                false,
                0.0,
                held.as_mut_ptr()
            ));
            assert_eq!(held[0], 0.0, "a lone output plays at once");
            assert!(asc_local_hold_backs_ms(
                ptr::null(),
                0,
                true,
                2000.0,
                ptr::null_mut()
            ));
            held = [-1.0; 3];
            assert!(!asc_local_hold_backs_ms(
                ptr::null(),
                3,
                false,
                0.0,
                held.as_mut_ptr()
            ));
            assert!(!asc_local_hold_backs_ms(
                outputs.as_ptr(),
                3,
                false,
                0.0,
                ptr::null_mut()
            ));
            assert_eq!(held, [-1.0; 3], "a refused call writes nothing");
        }
    }

    // covers: S175, S178
    #[test]
    fn offsets_and_airplay_settings_are_sanitized() {
        let mut out = 7.0;
        // SAFETY: `out` is a live double; null is allowed.
        unsafe {
            assert!(asc_local_offset_sanitize_ms(-2500.0, &mut out));
            assert_eq!(out, -2000.0);
            assert!(asc_local_offset_sanitize_ms(-250.0, &mut out));
            assert_eq!(out, -250.0);
            assert!(!asc_local_offset_sanitize_ms(f64::NAN, &mut out));
            assert_eq!(out, -250.0, "an ignored input writes nothing");
            assert!(!asc_local_offset_sanitize_ms(1.0, ptr::null_mut()));
            assert!(asc_airplay_delay_sanitize_ms(-5.0, &mut out));
            assert_eq!(out, 0.0);
            assert!(asc_airplay_delay_sanitize_ms(2300.0, &mut out));
            assert_eq!(out, 2300.0);
            assert!(!asc_airplay_delay_sanitize_ms(f64::INFINITY, &mut out));
            assert!(asc_airplay_adjustment_for_ms(2000.0, 1750.0, &mut out));
            assert_eq!(out, 250.0);
            assert!(asc_airplay_adjustment_for_ms(-9000.0, 1750.0, &mut out));
            assert_eq!(out, -2000.0);
            assert!(!asc_airplay_adjustment_for_ms(f64::NAN, 1750.0, &mut out));
            assert!(!asc_airplay_adjustment_for_ms(1.0, 1.0, ptr::null_mut()));
        }
        assert_eq!(asc_airplay_native_latency_ms(1750.0, 250.0), 2000.0);
        assert_eq!(asc_airplay_native_latency_ms(1750.0, -2000.0), 0.0);
        assert_eq!(asc_airplay_native_latency_ms(19_000.0, 5000.0), 20_000.0);
    }

    // covers: S179
    #[test]
    fn the_mac_migration_through_the_c_interface_matches_the_rust_one() {
        // R 250, Bluetooth 0, USB 400, an untuned listed output, D0 2000.
        let entries = [
            AscStoredDelay {
                is_reference: true,
                has_stored_delay: true,
                stored_delay_ms: 250.0,
            },
            AscStoredDelay {
                is_reference: false,
                has_stored_delay: true,
                stored_delay_ms: 0.0,
            },
            AscStoredDelay {
                is_reference: false,
                has_stored_delay: true,
                stored_delay_ms: 400.0,
            },
            AscStoredDelay {
                is_reference: false,
                has_stored_delay: false,
                stored_delay_ms: 0.0,
            },
        ];
        let mut offsets = [AscMigratedOffset::default(); 4];
        let mut result = AscDelayMigration::default();
        // SAFETY: both arrays hold four entries; `result` is live.
        let done = unsafe {
            asc_local_delay_migrate(
                entries.as_ptr(),
                4,
                false,
                0.0,
                offsets.as_mut_ptr(),
                &mut result,
            )
        };
        assert!(done);
        assert_eq!(
            result,
            AscDelayMigration {
                airplay_delay_ms: 2000.0,
                native_adjustment_ms: 0.0,
            }
        );
        let stored: Vec<Option<f64>> = offsets
            .iter()
            .map(|offset| offset.store.then_some(offset.offset_ms))
            .collect();
        assert_eq!(stored, [None, Some(-250.0), Some(150.0), Some(0.0)]);
        // The legacy delay counts when there is one: D0 = 600 keeps the
        // untuned output with the reference.
        // SAFETY: as above.
        unsafe {
            assert!(asc_local_delay_migrate(
                entries.as_ptr(),
                4,
                true,
                600.0,
                offsets.as_mut_ptr(),
                &mut result,
            ));
        }
        assert_eq!(offsets[3].offset_ms, 350.0);
        // SAFETY: null arrays are refused unless the count is 0; a null
        // result is refused.
        unsafe {
            assert!(asc_local_delay_migrate(
                ptr::null(),
                0,
                false,
                0.0,
                ptr::null_mut(),
                &mut result
            ));
            assert_eq!(result.airplay_delay_ms, 2000.0);
            assert!(!asc_local_delay_migrate(
                ptr::null(),
                4,
                false,
                0.0,
                offsets.as_mut_ptr(),
                &mut result
            ));
            assert!(!asc_local_delay_migrate(
                entries.as_ptr(),
                4,
                false,
                0.0,
                offsets.as_mut_ptr(),
                ptr::null_mut()
            ));
        }
    }

    // covers: S3
    #[test]
    fn delays_convert_to_frames_at_the_capture_rate() {
        let mut frames = -1i64;
        // SAFETY: `frames` is a live integer; null is allowed.
        unsafe {
            assert!(asc_delay_ms_to_frames(2000.0, 48_000, &mut frames));
            assert_eq!(frames, 96_000);
            assert!(asc_delay_ms_to_frames(1820.0, 48_000, &mut frames));
            assert_eq!(frames, 87_360);
            assert!(asc_delay_ms_to_frames(10.0, 44_100, &mut frames));
            assert_eq!(frames, 441);
            assert!(asc_delay_ms_to_frames(-3.0, 48_000, &mut frames));
            assert_eq!(frames, 0);
            assert!(!asc_delay_ms_to_frames(f64::NAN, 48_000, &mut frames));
            assert_eq!(frames, 0, "an ignored input writes nothing");
            assert!(!asc_delay_ms_to_frames(500.0, 48_000, ptr::null_mut()));
        }
    }
}
