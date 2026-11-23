//! Apps listening to a microphone while the whole system streams (S485),
//! through the C interface: the host passes each poll's facts and reads back
//! the apps left out. The Mac has no silent endpoint an app can stay bound
//! to, so nothing is ever reported on it and no app ever stays.
//!
//! The rules are in [`CallListeners`]; this module only turns C strings into
//! its types. For the host's control thread: it may allocate.

use std::ffi::{CStr, CString, c_char};
use std::ptr;

use super::{guard, host_slice};
use crate::policy::call_listeners::{CallListeners, Facts, ListenerAction};

/// The rule's state across a stream, behind an opaque C pointer.
pub struct AscCallListeners {
    rule: CallListeners,
    left_out: Vec<CString>,
}

/// A rule with no app left out.
#[unsafe(no_mangle)]
pub extern "C" fn asc_call_listeners_create() -> *mut AscCallListeners {
    guard(ptr::null_mut(), || {
        Box::into_raw(Box::new(AscCallListeners {
            rule: CallListeners::default(),
            left_out: Vec::new(),
        }))
    })
}

/// Frees a rule.
///
/// # Safety
///
/// `rule` is null or a live handle no other call uses now or later.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn asc_call_listeners_destroy(rule: *mut AscCallListeners) {
    guard((), || {
        if !rule.is_null() {
            // SAFETY: a handle from Box::into_raw that nothing uses any more.
            drop(unsafe { Box::from_raw(rule) });
        }
    })
}

/// One poll at `now` seconds ([`CallListeners::update`]): `whole_system`
/// whether the whole-system source streams, `listening` the `count` apps
/// with an active microphone stream. True when the apps left out changed. A
/// null rule, a null array with a nonzero count, or a null or non-UTF-8 entry
/// changes nothing.
///
/// # Safety
///
/// `rule` is null or a live handle no other call uses during this one;
/// `listening` is null or holds `count` pointers, each to a NUL-terminated
/// string, all unchanged during the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn asc_call_listeners_update(
    rule: *mut AscCallListeners,
    whole_system: bool,
    listening: *const *const c_char,
    count: usize,
    now: f64,
) -> bool {
    guard(false, || {
        if rule.is_null() {
            return false;
        }
        // SAFETY: by the caller's contract null or `count` pointers nothing
        // writes during the call.
        let Some(pointers) = (unsafe { host_slice(listening, count) }) else {
            return false;
        };
        let mut apps = Vec::with_capacity(pointers.len());
        for &pointer in pointers {
            if pointer.is_null() {
                return false;
            }
            // SAFETY: non-null, and by the caller's contract NUL-terminated.
            let Ok(app) = unsafe { CStr::from_ptr(pointer) }.to_str() else {
                return false;
            };
            apps.push(app.to_owned());
        }
        // SAFETY: a live handle that only this call uses, by the caller's contract.
        let rule = unsafe { &mut *rule };
        let facts = Facts {
            whole_system,
            listening: &apps,
            on_silent: &[],
        };
        let changed = rule
            .rule
            .update(facts, now)
            .iter()
            .any(|action| !matches!(action, ListenerAction::Stayed(_)));
        if changed {
            rule.left_out = rule
                .rule
                .left_out()
                .filter_map(|app| CString::new(app).ok())
                .collect();
        }
        changed
    })
}

/// The number of apps left out; 0 for a null rule.
///
/// # Safety
///
/// `rule` is null or a live handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn asc_call_listeners_left_out_count(rule: *const AscCallListeners) -> usize {
    guard(0, || {
        if rule.is_null() {
            return 0;
        }
        // SAFETY: a live handle, by the caller's contract.
        unsafe { &*rule }.left_out.len()
    })
}

/// The app left out at `index`, valid until the next update or the destroy;
/// null for a null rule or an index past the count.
///
/// # Safety
///
/// `rule` is null or a live handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn asc_call_listeners_left_out(
    rule: *const AscCallListeners,
    index: usize,
) -> *const c_char {
    guard(ptr::null(), || {
        if rule.is_null() {
            return ptr::null();
        }
        // SAFETY: a live handle, by the caller's contract.
        unsafe { &*rule }
            .left_out
            .get(index)
            .map_or(ptr::null(), |app| app.as_ptr())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Rule(*mut AscCallListeners);

    impl Drop for Rule {
        fn drop(&mut self) {
            // SAFETY: a handle from create that nothing else uses.
            unsafe { asc_call_listeners_destroy(self.0) }
        }
    }

    impl Rule {
        fn new() -> Self {
            Self(asc_call_listeners_create())
        }

        fn poll(&self, whole_system: bool, listening: &[&CStr], now: f64) -> bool {
            let pointers: Vec<*const c_char> = listening.iter().map(|app| app.as_ptr()).collect();
            // SAFETY: a live handle and live C strings.
            unsafe {
                asc_call_listeners_update(
                    self.0,
                    whole_system,
                    pointers.as_ptr(),
                    pointers.len(),
                    now,
                )
            }
        }

        fn left_out(&self) -> Vec<String> {
            // SAFETY: a live handle; each pointer is read before the next update.
            unsafe {
                (0..asc_call_listeners_left_out_count(self.0))
                    .map(|index| {
                        CStr::from_ptr(asc_call_listeners_left_out(self.0, index))
                            .to_str()
                            .unwrap()
                            .to_owned()
                    })
                    .collect()
            }
        }
    }

    // covers: S485
    #[test]
    fn a_call_is_left_out_at_once_and_rejoins_after_the_hold() {
        let rule = Rule::new();
        assert!(!rule.poll(true, &[], 0.0));
        assert!(rule.poll(true, &[c"412"], 1.0));
        assert_eq!(rule.left_out(), ["412"]);
        assert!(!rule.poll(true, &[c"412"], 2.0));
        assert!(!rule.poll(true, &[], 4.5));
        assert_eq!(rule.left_out(), ["412"]);
        assert!(rule.poll(true, &[], 5.0));
        assert!(rule.left_out().is_empty());
    }

    // covers: S485
    #[test]
    fn every_app_rejoins_when_the_whole_system_stops_streaming() {
        let rule = Rule::new();
        assert!(rule.poll(true, &[c"412", c"977"], 0.0));
        assert_eq!(rule.left_out(), ["412", "977"]);
        assert!(rule.poll(false, &[c"412"], 0.5));
        assert!(rule.left_out().is_empty());
        assert!(!rule.poll(false, &[c"412"], 1.0));
    }

    // covers: S485
    #[test]
    fn bad_arguments_change_nothing() {
        let rule = Rule::new();
        let not_utf8 = c"\xff\xfe";
        assert!(!rule.poll(true, &[not_utf8], 0.0));
        let with_null = [c"412".as_ptr(), ptr::null()];
        // SAFETY: a live handle; a null entry is allowed.
        assert!(!unsafe { asc_call_listeners_update(rule.0, true, with_null.as_ptr(), 2, 0.0) });
        // SAFETY: a null array with a nonzero count and a null rule are allowed.
        unsafe {
            assert!(!asc_call_listeners_update(
                rule.0,
                true,
                ptr::null(),
                1,
                0.0
            ));
            assert!(!asc_call_listeners_update(
                ptr::null_mut(),
                true,
                with_null.as_ptr(),
                1,
                0.0
            ));
            assert_eq!(asc_call_listeners_left_out_count(ptr::null()), 0);
            assert!(asc_call_listeners_left_out(rule.0, 0).is_null());
        }
        assert!(rule.left_out().is_empty());
    }
}
