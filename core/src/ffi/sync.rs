//! The local delay model and the delay helpers through the C interface (S3,
//! S124, S172 to S179, S328): functions for the control thread.
//!
//! The rules themselves are in [`delay`]: [`delay::hold_backs`] and
//! [`delay::migrate_mac_delays`] for the local delay model; this module only
//! reads the host's C arrays. It may
//! allocate (the outputs taking part), so it never runs on an audio thread.

use std::ffi::{CStr, CString, c_char};
use std::time::Duration;

use super::{guard, host_slice, host_slice_mut};
use crate::policy::delay;
use crate::reader;

/// [`delay::sanitize_delay_ms`] (S124, S178): false, writing nothing, for a
/// non-finite delay or a null `sanitized_ms`.
///
/// # Safety
///
/// `sanitized_ms` is null or points to an aligned, writable double that
/// nothing else touches during the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn asc_delay_sanitize_ms(delay_ms: f64, sanitized_ms: *mut f64) -> bool {
    guard(false, || {
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
    })
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
    guard(false, || {
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
    })
}

/// A local output taking part, for the hold-backs (S172, S176).
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct AscLocalOutput {
    pub offset_ms: f64,
    pub is_reference: bool,
}

/// One UID the Mac's migration looks at (S179).
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct AscStoredDelay {
    pub is_reference: bool,
    pub has_stored_delay: bool,
    pub stored_delay_ms: f64,
}

/// The offset the Mac's migration stores for one UID, if any (S179).
#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct AscMigratedOffset {
    pub store: bool,
    pub offset_ms: f64,
}

/// The AirPlay settings the Mac's migration stores (S178, S179).
#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct AscDelayMigration {
    pub airplay_delay_ms: f64,
    pub native_adjustment_ms: f64,
}

/// Writes a sanitized value through `out`: false, writing nothing, for an
/// ignored value or a null `out`.
///
/// # Safety
///
/// `out` is null or points to an aligned, writable double that nothing else
/// touches during the call.
unsafe fn write_sanitized(value: Option<f64>, out: *mut f64) -> bool {
    let Some(value) = value else {
        return false;
    };
    if out.is_null() {
        return false;
    }
    // SAFETY: non-null, and by the caller's contract aligned, writable and
    // not touched by anyone else.
    unsafe { out.write(value) };
    true
}

/// [`delay::hold_backs`] (S176) of the `count` outputs taking part, in the
/// same order, into `hold_backs_ms`; `airplay_latency_ms` counts only when
/// `airplay_chosen`. False, writing nothing, for a null array with a nonzero
/// count.
///
/// # Safety
///
/// Each array is null or holds `count` aligned elements; `outputs` stays
/// unchanged and nothing else touches `hold_backs_ms` during the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn asc_local_hold_backs_ms(
    outputs: *const AscLocalOutput,
    count: usize,
    airplay_chosen: bool,
    airplay_latency_ms: f64,
    hold_backs_ms: *mut f64,
) -> bool {
    guard(false, || {
        // SAFETY: by the caller's contract each array is null or holds `count`
        // elements that only this call touches.
        let (Some(outputs), Some(held)) = (unsafe {
            (
                host_slice(outputs, count),
                host_slice_mut(hold_backs_ms, count),
            )
        }) else {
            return false;
        };
        let taking_part: Vec<delay::Participant> = outputs
            .iter()
            .map(|output| delay::Participant {
                id: "",
                offset_ms: output.offset_ms,
                is_reference: output.is_reference,
            })
            .collect();
        let latency = airplay_chosen.then_some(airplay_latency_ms);
        for (out, (_, value)) in held
            .iter_mut()
            .zip(delay::hold_backs(&taking_part, latency))
        {
            *out = value;
        }
        true
    })
}

/// [`delay::sanitize_offset_ms`] (S175): false, writing nothing, for a
/// non-finite offset or a null `sanitized_ms`.
///
/// # Safety
///
/// `sanitized_ms` is null or points to an aligned, writable double that
/// nothing else touches during the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn asc_local_offset_sanitize_ms(
    offset_ms: f64,
    sanitized_ms: *mut f64,
) -> bool {
    guard(false, || {
        // SAFETY: by the caller's contract.
        unsafe { write_sanitized(delay::sanitize_offset_ms(offset_ms), sanitized_ms) }
    })
}

/// [`delay::bluetooth_seed_offset_ms`] (S455) into `seed_ms`: false,
/// writing nothing, when the output gets no seed or `seed_ms` is null.
///
/// # Safety
///
/// `seed_ms` is null or points to an aligned, writable double that nothing
/// else touches during the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn asc_local_bluetooth_seed_ms(
    bluetooth: bool,
    is_reference: bool,
    known: bool,
    has_offset: bool,
    latency_ms: f64,
    reference_latency_ms: f64,
    reference_bluetooth: bool,
    seed_ms: *mut f64,
) -> bool {
    guard(false, || {
        let output = delay::ListedOutput {
            bluetooth,
            is_reference,
            known,
            has_offset,
            latency_ms,
        };
        let seed =
            delay::bluetooth_seed_offset_ms(output, reference_latency_ms, reference_bluetooth);
        // SAFETY: by the caller's contract.
        unsafe { write_sanitized(seed, seed_ms) }
    })
}

/// [`delay::sanitize_airplay_delay_ms`] (S178): false, writing nothing, for
/// a non-finite delay or a null `sanitized_ms`.
///
/// # Safety
///
/// `sanitized_ms` is null or points to an aligned, writable double that
/// nothing else touches during the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn asc_local_airplay_delay_sanitize_ms(
    delay_ms: f64,
    sanitized_ms: *mut f64,
) -> bool {
    guard(false, || {
        // SAFETY: by the caller's contract.
        unsafe { write_sanitized(delay::sanitize_airplay_delay_ms(delay_ms), sanitized_ms) }
    })
}

/// [`delay::native_airplay_latency_ms`] (S178).
#[unsafe(no_mangle)]
pub extern "C" fn asc_local_native_latency_ms(sender_latency_ms: f64, adjustment_ms: f64) -> f64 {
    guard(0.0, || {
        delay::native_airplay_latency_ms(sender_latency_ms, adjustment_ms)
    })
}

/// [`delay::native_airplay_adjustment_ms`] (S178, S181): false, writing
/// nothing, for a non-finite value or a null `adjustment_ms`.
///
/// # Safety
///
/// `adjustment_ms` is null or points to an aligned, writable double that
/// nothing else touches during the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn asc_local_native_adjustment_ms(
    shown_ms: f64,
    sender_latency_ms: f64,
    adjustment_ms: *mut f64,
) -> bool {
    guard(false, || {
        // SAFETY: by the caller's contract.
        unsafe {
            write_sanitized(
                delay::native_airplay_adjustment_ms(shown_ms, sender_latency_ms),
                adjustment_ms,
            )
        }
    })
}

/// [`delay::migrate_mac_delays`] (S179) of the `count` entries, writing one
/// offset per entry, in the same order, and the AirPlay settings. The
/// legacy delay counts only when `has_legacy_delay`. False, writing
/// nothing, for a null array with a nonzero count or a null `result`.
///
/// # Safety
///
/// Each array is null or holds `count` aligned elements, and `result` is
/// null or points to an aligned, writable [`AscDelayMigration`]; `entries`
/// stays unchanged and nothing else touches `offsets` or `result` during
/// the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn asc_local_delay_migrate(
    entries: *const AscStoredDelay,
    count: usize,
    has_legacy_delay: bool,
    legacy_delay_ms: f64,
    offsets: *mut AscMigratedOffset,
    result: *mut AscDelayMigration,
) -> bool {
    guard(false, || {
        if result.is_null() {
            return false;
        }
        // SAFETY: by the caller's contract each array is null or holds `count`
        // elements that only this call touches.
        let (Some(entries), Some(offsets)) =
            (unsafe { (host_slice(entries, count), host_slice_mut(offsets, count)) })
        else {
            return false;
        };
        let stored: Vec<delay::StoredDelay> = entries
            .iter()
            .map(|entry| delay::StoredDelay {
                is_reference: entry.is_reference,
                stored_delay_ms: entry.has_stored_delay.then_some(entry.stored_delay_ms),
            })
            .collect();
        let migration =
            delay::migrate_mac_delays(&stored, has_legacy_delay.then_some(legacy_delay_ms));
        for (out, offset) in offsets.iter_mut().zip(&migration.offsets_ms) {
            *out = AscMigratedOffset {
                store: offset.is_some(),
                offset_ms: offset.unwrap_or(0.0),
            };
        }
        // SAFETY: non-null, and by the caller's contract aligned, writable and
        // not touched by anyone else.
        unsafe {
            result.write(AscDelayMigration {
                airplay_delay_ms: migration.airplay_delay_ms,
                native_adjustment_ms: migration.native_adjustment_ms,
            });
        }
        true
    })
}

/// [`delay::BriefLosses`] (S328) behind an opaque C pointer, with the held
/// outputs' IDs as C strings for [`asc_brief_losses_held_id`].
pub struct AscBriefLosses {
    losses: delay::BriefLosses,
    held: Vec<CString>,
}

impl AscBriefLosses {
    fn refresh_held(&mut self) {
        self.held = self
            .losses
            .held()
            .filter_map(|id| CString::new(id).ok())
            .collect();
    }
}

/// # Safety
///
/// `id` is null or points to a NUL-terminated string that stays valid and
/// unchanged during `'a`.
unsafe fn host_id<'a>(id: *const c_char) -> Option<&'a str> {
    if id.is_null() {
        return None;
    }
    // SAFETY: non-null, and by the caller's contract NUL-terminated.
    unsafe { CStr::from_ptr(id) }.to_str().ok()
}

/// The `count` IDs at `ids`, null and non-UTF-8 ones left out; `None` for a
/// null array with a nonzero count.
///
/// # Safety
///
/// `ids` is null or holds `count` aligned elements, each null or a
/// NUL-terminated string, all valid and unchanged during `'a`.
unsafe fn host_ids<'a>(ids: *const *const c_char, count: usize) -> Option<Vec<&'a str>> {
    // SAFETY: by the caller's contract.
    let ids = unsafe { host_slice(ids, count) }?;
    // SAFETY: by the caller's contract each is null or NUL-terminated.
    Some(
        ids.iter()
            .filter_map(|&id| unsafe { host_id(id) })
            .collect(),
    )
}

/// The host's uptime in seconds; a negative or non-finite one counts as 0.
fn host_time(seconds: f64) -> Duration {
    Duration::try_from_secs_f64(seconds).unwrap_or_default()
}

/// Creates the brief-loss hold (S328), holding nothing.
#[unsafe(no_mangle)]
pub extern "C" fn asc_brief_losses_create() -> *mut AscBriefLosses {
    Box::into_raw(Box::new(AscBriefLosses {
        losses: delay::BriefLosses::default(),
        held: Vec::new(),
    }))
}

/// Destroys a brief-loss hold.
///
/// # Safety
///
/// `losses` is null or a pointer returned by [`asc_brief_losses_create`]
/// that was not destroyed yet, and no other call on it runs now or later.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn asc_brief_losses_destroy(losses: *mut AscBriefLosses) {
    guard((), || {
        if losses.is_null() {
            return;
        }
        // SAFETY: the pointer came from Box::into_raw in asc_brief_losses_create,
        // and by the caller's contract it is destroyed once, with no call in
        // progress.
        drop(unsafe { Box::from_raw(losses) });
    })
}

/// [`delay::BriefLosses::note_listing`]: the `listed_count` IDs listed at
/// `now_s`, the `enabled_count` switched on, and the reference among them
/// (null for none). Does nothing for a null handle or a null array with a
/// nonzero count.
///
/// # Safety
///
/// `losses` is null or a live handle that nothing else uses during the
/// call; each array is null or holds its count of aligned elements, and each
/// ID and `reference` is null or a NUL-terminated string, all valid and
/// unchanged during the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn asc_brief_losses_note_listing(
    losses: *mut AscBriefLosses,
    listed: *const *const c_char,
    listed_count: usize,
    enabled: *const *const c_char,
    enabled_count: usize,
    reference: *const c_char,
    now_s: f64,
) {
    guard((), || {
        // SAFETY: by the caller's contract.
        let (Some(losses), Some(listed), Some(enabled), reference) = (unsafe {
            (
                losses.as_mut(),
                host_ids(listed, listed_count),
                host_ids(enabled, enabled_count),
                host_id(reference),
            )
        }) else {
            return;
        };
        losses.losses.note_listing(
            listed,
            |id| enabled.contains(&id),
            reference,
            host_time(now_s),
        );
        losses.refresh_held();
    })
}

/// [`delay::BriefLosses::switched_off`]: whether `id` was held; false for a
/// null handle or ID.
///
/// # Safety
///
/// `losses` is null or a live handle that nothing else uses during the
/// call; `id` is null or a NUL-terminated string, valid and unchanged
/// during the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn asc_brief_losses_switched_off(
    losses: *mut AscBriefLosses,
    id: *const c_char,
) -> bool {
    guard(false, || {
        // SAFETY: by the caller's contract.
        let (Some(losses), Some(id)) = (unsafe { (losses.as_mut(), host_id(id)) }) else {
            return false;
        };
        let was_held = losses.losses.switched_off(id);
        losses.refresh_held();
        was_held
    })
}

/// [`delay::BriefLosses::expire`] at `now_s`: whether any hold ended; false
/// for a null handle.
///
/// # Safety
///
/// `losses` is null or a live handle that nothing else uses during the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn asc_brief_losses_expire(losses: *mut AscBriefLosses, now_s: f64) -> bool {
    guard(false, || {
        // SAFETY: by the caller's contract.
        let Some(losses) = (unsafe { losses.as_mut() }) else {
            return false;
        };
        let ended = losses.losses.expire(host_time(now_s));
        losses.refresh_held();
        ended
    })
}

/// [`delay::BriefLosses::restart`] (S20); nothing for a null handle.
///
/// # Safety
///
/// `losses` is null or a live handle that nothing else uses during the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn asc_brief_losses_restart(losses: *mut AscBriefLosses) {
    guard((), || {
        // SAFETY: by the caller's contract.
        if let Some(losses) = unsafe { losses.as_mut() } {
            losses.losses.restart();
            losses.refresh_held();
        }
    })
}

/// How many outputs are held; 0 for a null handle.
///
/// # Safety
///
/// `losses` is null or a live handle that nothing changes during the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn asc_brief_losses_held_count(losses: *const AscBriefLosses) -> usize {
    guard(0, || {
        // SAFETY: by the caller's contract.
        unsafe { losses.as_ref() }.map_or(0, |losses| losses.held.len())
    })
}

/// The ID of held output `index`, sorted by ID; null past the end or for a
/// null handle. It stays valid until the next call that changes the handle.
///
/// # Safety
///
/// `losses` is null or a live handle that nothing changes during the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn asc_brief_losses_held_id(
    losses: *const AscBriefLosses,
    index: usize,
) -> *const c_char {
    guard(std::ptr::null(), || {
        // SAFETY: by the caller's contract.
        unsafe { losses.as_ref() }
            .and_then(|losses| losses.held.get(index))
            .map_or(std::ptr::null(), |id| id.as_ptr())
    })
}

/// [`delay::BriefLosses::next_expiry`] into `at_s`, in the host's uptime
/// seconds: false, writing nothing, when nothing is held or for a null
/// pointer.
///
/// # Safety
///
/// `losses` is null or a live handle that nothing changes during the call;
/// `at_s` is null or points to an aligned, writable double that nothing
/// else touches during the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn asc_brief_losses_next_expiry_s(
    losses: *const AscBriefLosses,
    at_s: *mut f64,
) -> bool {
    guard(false, || {
        // SAFETY: by the caller's contract.
        let next = unsafe { losses.as_ref() }.and_then(|losses| losses.losses.next_expiry());
        // SAFETY: by the caller's contract.
        unsafe { write_sanitized(next.map(|at| at.as_secs_f64()), at_s) }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ptr;

    // covers: S124, S178
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
            assert!(asc_local_airplay_delay_sanitize_ms(-5.0, &mut out));
            assert_eq!(out, 0.0);
            assert!(asc_local_airplay_delay_sanitize_ms(2300.0, &mut out));
            assert_eq!(out, 2300.0);
            assert!(!asc_local_airplay_delay_sanitize_ms(
                f64::INFINITY,
                &mut out
            ));
            assert!(asc_local_native_adjustment_ms(2000.0, 1750.0, &mut out));
            assert_eq!(out, 250.0);
            assert!(asc_local_native_adjustment_ms(-9000.0, 1750.0, &mut out));
            assert_eq!(out, -2000.0);
            assert!(!asc_local_native_adjustment_ms(f64::NAN, 1750.0, &mut out));
            assert!(!asc_local_native_adjustment_ms(1.0, 1.0, ptr::null_mut()));
        }
        assert_eq!(asc_local_native_latency_ms(1750.0, 250.0), 2000.0);
        assert_eq!(asc_local_native_latency_ms(1750.0, -2000.0), 0.0);
        assert_eq!(asc_local_native_latency_ms(19_000.0, 5000.0), 20_000.0);
    }

    // covers: S455
    #[test]
    fn a_bluetooth_seed_through_the_c_interface() {
        let mut out = 7.0;
        // SAFETY: `out` is a live double; null is allowed.
        unsafe {
            assert!(asc_local_bluetooth_seed_ms(
                true, false, false, false, 160.5, 24.5, false, &mut out
            ));
            assert_eq!(out, -216.0);
            assert!(!asc_local_bluetooth_seed_ms(
                true, false, true, false, 160.5, 24.5, false, &mut out
            ));
            assert!(!asc_local_bluetooth_seed_ms(
                true, false, false, true, 160.5, 24.5, false, &mut out
            ));
            assert_eq!(out, -216.0, "no seed writes nothing");
            assert!(!asc_local_bluetooth_seed_ms(
                true,
                false,
                false,
                false,
                160.5,
                24.5,
                false,
                ptr::null_mut()
            ));
        }
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

    /// A brief-loss hold driven as the Mac drives it.
    struct Hold(*mut AscBriefLosses);

    impl Hold {
        fn new() -> Self {
            Self(asc_brief_losses_create())
        }

        fn note(&self, listed: &[&CStr], enabled: &[&CStr], reference: Option<&CStr>, now_s: f64) {
            let listed: Vec<*const c_char> = listed.iter().map(|id| id.as_ptr()).collect();
            let enabled: Vec<*const c_char> = enabled.iter().map(|id| id.as_ptr()).collect();
            let reference = reference.map_or(ptr::null(), CStr::as_ptr);
            // SAFETY: a live handle; the arrays and strings outlive the call.
            unsafe {
                asc_brief_losses_note_listing(
                    self.0,
                    listed.as_ptr(),
                    listed.len(),
                    enabled.as_ptr(),
                    enabled.len(),
                    reference,
                    now_s,
                );
            }
        }

        fn held(&self) -> Vec<String> {
            // SAFETY: a live handle; each ID is read before the next change.
            unsafe {
                (0..asc_brief_losses_held_count(self.0))
                    .map(|index| {
                        CStr::from_ptr(asc_brief_losses_held_id(self.0, index))
                            .to_str()
                            .unwrap()
                            .to_owned()
                    })
                    .collect()
            }
        }

        fn next_expiry(&self) -> Option<f64> {
            let mut at = -1.0;
            // SAFETY: a live handle and a live double.
            unsafe { asc_brief_losses_next_expiry_s(self.0, &mut at) }.then_some(at)
        }

        fn expire(&self, now_s: f64) -> bool {
            // SAFETY: a live handle.
            unsafe { asc_brief_losses_expire(self.0, now_s) }
        }

        fn switched_off(&self, id: &CStr) -> bool {
            // SAFETY: a live handle and a live string.
            unsafe { asc_brief_losses_switched_off(self.0, id.as_ptr()) }
        }
    }

    impl Drop for Hold {
        fn drop(&mut self) {
            // SAFETY: created by asc_brief_losses_create, destroyed once.
            unsafe { asc_brief_losses_destroy(self.0) };
        }
    }

    const R: &CStr = c"built-in";
    const USB: &CStr = c"usb";
    const BT: &CStr = c"bluetooth";
    const ON: [&CStr; 3] = [R, USB, BT];

    /// Bluetooth listed at 0 s and gone from the list at `lost_at` s.
    fn flapped(lost_at: f64) -> Hold {
        let hold = Hold::new();
        hold.note(&ON, &ON, Some(R), 0.0);
        hold.note(&[R, USB], &ON, Some(R), lost_at);
        hold
    }

    // covers: S328
    #[test]
    fn the_brief_loss_hold_crosses_the_boundary_as_the_core_keeps_it() {
        let flap = flapped(5.0);
        assert_eq!(flap.held(), ["bluetooth"]);
        flap.note(&ON, &ON, Some(R), 6.0);
        assert!(flap.held().is_empty(), "listed again within 10 s");

        let flap = flapped(5.0);
        assert!(!flap.expire(14.9));
        assert_eq!(flap.next_expiry(), Some(15.0));
        assert!(flap.expire(15.0), "still lost at 10 s");
        assert!(!flap.expire(16.0), "it ends once");
        assert_eq!(flap.next_expiry(), None);
        assert!(flap.held().is_empty());

        let flap = flapped(5.0);
        assert!(flap.switched_off(BT));
        assert!(!flap.switched_off(USB), "a listed output wasn't held");
        assert!(flap.held().is_empty());

        let flap = flapped(5.0);
        // SAFETY: a live handle.
        unsafe { asc_brief_losses_restart(flap.0) };
        assert!(flap.held().is_empty(), "a session restart holds nothing");

        let lost_reference = Hold::new();
        lost_reference.note(&ON, &ON, Some(R), 0.0);
        lost_reference.note(&[USB, BT], &ON, None, 5.0);
        assert!(lost_reference.held().is_empty());

        let off_first = Hold::new();
        off_first.note(&ON, &ON, Some(R), 0.0);
        off_first.note(&[R, USB], &[R, USB], Some(R), 5.0);
        assert!(off_first.held().is_empty());

        let flap = flapped(5.0);
        assert!(flap.switched_off(BT));
        flap.note(&[R, USB], &ON, Some(R), 6.0);
        assert!(
            flap.held().is_empty(),
            "switched off and on again while still missing isn't held again"
        );
    }

    // covers: S328
    #[test]
    fn the_brief_loss_hold_ignores_bad_arguments() {
        let flap = flapped(5.0);
        // SAFETY: null handles and arrays are allowed; the strings are live.
        unsafe {
            asc_brief_losses_note_listing(flap.0, ptr::null(), 2, ptr::null(), 0, ptr::null(), 6.0);
            assert_eq!(
                flap.held(),
                ["bluetooth"],
                "a null array with a count changes nothing"
            );
            asc_brief_losses_note_listing(
                ptr::null_mut(),
                ptr::null(),
                0,
                ptr::null(),
                0,
                ptr::null(),
                0.0,
            );
            assert!(!asc_brief_losses_switched_off(flap.0, ptr::null()));
            assert!(!asc_brief_losses_expire(ptr::null_mut(), 20.0));
            asc_brief_losses_restart(ptr::null_mut());
            assert_eq!(asc_brief_losses_held_count(ptr::null()), 0);
            assert!(asc_brief_losses_held_id(flap.0, 1).is_null());
            assert!(!asc_brief_losses_next_expiry_s(flap.0, ptr::null_mut()));
            asc_brief_losses_destroy(ptr::null_mut());
        }
        assert!(
            !flap.expire(f64::NAN),
            "a time that isn't finite counts as 0"
        );
    }
}
