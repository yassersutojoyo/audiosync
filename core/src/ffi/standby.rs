//! The standby rule of a Denon or Marantz receiver (P262 to P264) through
//! the C interface: the host keeps one [`AscStandby`] per AirPlay speaker,
//! stores its two marks with the speaker's saved settings, and feeds it the
//! `standby` outcome of each speaker event and the `denon` answer of each
//! receiver event.
//!
//! The rules are in [`airplay_standby`](crate::policy::airplay_standby); this module only turns codes into
//! its types. For the host's control thread.

use audiosync_airplay::discovery::DenonBrand;

use super::guard;
use crate::policy::airplay_standby::{Cause, Outcome, Recognized, Record, Standby, StandbyMarks};

/// [`AscAirPlayReceiver::denon`](super::AscAirPlayReceiver): not a Denon or
/// Marantz AV receiver.
pub const DENON_NONE: i32 = 0;
/// One by its advertised manufacturer.
pub const DENON_MANUFACTURER: i32 = 1;
/// One by the address block of its device identifier.
pub const DENON_ADDRESS_BLOCK: i32 = 2;
/// For [`asc_standby_update`]'s `record`: no record of it in view.
pub const DENON_NOT_IN_VIEW: i32 = -1;

/// [`AscAirPlayEvent::standby`](super::AscAirPlayEvent): not a state or
/// error event.
pub const STANDBY_OUTCOME_NONE: i32 = 0;
/// It played.
pub const STANDBY_OUTCOME_PLAYED: i32 = 1;
/// It stopped announcing itself (P215).
pub const STANDBY_OUTCOME_UNANNOUNCED: i32 = 2;
/// Unreachable from a connect that got no answer.
pub const STANDBY_OUTCOME_NO_ANSWER: i32 = 3;
/// Connecting, or waiting for a retry: neither counts nor breaks.
pub const STANDBY_OUTCOME_BETWEEN: i32 = 4;
/// Any other outcome, which breaks the run.
pub const STANDBY_OUTCOME_BREAKS: i32 = 5;

/// [`AscStandbyShown::recognized`]: by its stored mark.
pub const STANDBY_BY_STORED_MARK: i32 = 3;
/// [`AscStandbyShown::cause`]: it is unannounced or not in view.
pub const STANDBY_CAUSE_UNANNOUNCED: i32 = 1;
/// [`AscStandbyShown::cause`]: its connects go unanswered.
pub const STANDBY_CAUSE_NO_ANSWER: i32 = 2;

/// One speaker's standby state. The host sets the two marks from its saved
/// settings before the first call, stores them again after each call that
/// may change them, and reads `hint` for the row; it never writes the rest.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct AscStandby {
    /// It was recognized as a Denon or Marantz receiver (stored, P263).
    pub recognized_mark: bool,
    /// The hint showed since it last played (stored, P263).
    pub hint_shown_mark: bool,
    /// The hint shows on its row now.
    pub hint: bool,
    /// Seconds the run of counted outcomes started at; negative for none.
    pub run_since: f64,
    /// The last counted outcome's `STANDBY_CAUSE_*`; 0 for none.
    pub cause: i32,
}

/// Why the hint showed, for the host's one debug log line (P263).
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct AscStandbyShown {
    /// `DENON_MANUFACTURER`, `DENON_ADDRESS_BLOCK` or
    /// `STANDBY_BY_STORED_MARK`.
    pub recognized: i32,
    /// `STANDBY_CAUSE_*`.
    pub cause: i32,
}

pub(crate) fn denon_code(brand: Option<DenonBrand>) -> i32 {
    match brand {
        None => DENON_NONE,
        Some(DenonBrand::Manufacturer) => DENON_MANUFACTURER,
        Some(DenonBrand::AddressBlock) => DENON_ADDRESS_BLOCK,
    }
}

pub(crate) fn standby_outcome_code(outcome: Outcome) -> i32 {
    match outcome {
        Outcome::Played => STANDBY_OUTCOME_PLAYED,
        Outcome::Unannounced => STANDBY_OUTCOME_UNANNOUNCED,
        Outcome::NoAnswer => STANDBY_OUTCOME_NO_ANSWER,
        Outcome::Between => STANDBY_OUTCOME_BETWEEN,
        Outcome::Breaks => STANDBY_OUTCOME_BREAKS,
    }
}

fn cause_code(cause: Option<Cause>) -> i32 {
    match cause {
        None => 0,
        Some(Cause::Unannounced) => STANDBY_CAUSE_UNANNOUNCED,
        Some(Cause::NoAnswer) => STANDBY_CAUSE_NO_ANSWER,
    }
}

impl AscStandby {
    fn split(&self) -> (Standby, StandbyMarks) {
        let standby = Standby {
            run_since: (self.run_since >= 0.0).then_some(self.run_since),
            cause: match self.cause {
                STANDBY_CAUSE_UNANNOUNCED => Some(Cause::Unannounced),
                STANDBY_CAUSE_NO_ANSWER => Some(Cause::NoAnswer),
                _ => None,
            },
            hint: self.hint,
        };
        let marks = StandbyMarks {
            recognized: self.recognized_mark,
            hint_shown: self.hint_shown_mark,
        };
        (standby, marks)
    }

    fn join(&mut self, standby: Standby, marks: StandbyMarks) {
        self.recognized_mark = marks.recognized;
        self.hint_shown_mark = marks.hint_shown;
        self.hint = standby.hint;
        self.run_since = standby.run_since.unwrap_or(-1.0);
        self.cause = cause_code(standby.cause);
    }
}

/// A fresh state with the stored marks.
#[unsafe(no_mangle)]
pub extern "C" fn asc_standby_new(recognized_mark: bool, hint_shown_mark: bool) -> AscStandby {
    AscStandby {
        recognized_mark,
        hint_shown_mark,
        hint: false,
        run_since: -1.0,
        cause: 0,
    }
}

/// Takes a speaker event's `standby` outcome at `now` (seconds on the host's
/// monotonic clock); `STANDBY_OUTCOME_NONE` and unknown codes change nothing.
///
/// # Safety
///
/// `standby` is null or points to a valid [`AscStandby`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn asc_standby_outcome(standby: *mut AscStandby, outcome: i32, now: f64) {
    guard((), || {
        // SAFETY: by the caller's contract null or valid.
        let Some(state) = (unsafe { standby.as_mut() }) else {
            return;
        };
        let outcome = match outcome {
            STANDBY_OUTCOME_PLAYED => Outcome::Played,
            STANDBY_OUTCOME_UNANNOUNCED => Outcome::Unannounced,
            STANDBY_OUTCOME_NO_ANSWER => Outcome::NoAnswer,
            STANDBY_OUTCOME_BETWEEN => Outcome::Between,
            STANDBY_OUTCOME_BREAKS => Outcome::Breaks,
            _ => return,
        };
        let (mut rule, mut marks) = state.split();
        rule.outcome(outcome, now, &mut marks);
        state.join(rule, marks);
    })
}

/// Brings the speaker up to date at `now`: `record` is its receiver's last
/// `denon` code, or `DENON_NOT_IN_VIEW`; `wanted` whether the host wants it
/// (on, alone or in a zone, while streaming runs). True when the hint shows
/// now, with `shown` (null allowed) filled for the log line.
///
/// # Safety
///
/// `standby` is null or points to a valid [`AscStandby`]; `shown` is null
/// or points to writable memory for one [`AscStandbyShown`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn asc_standby_update(
    standby: *mut AscStandby,
    record: i32,
    wanted: bool,
    now: f64,
    shown: *mut AscStandbyShown,
) -> bool {
    guard(false, || {
        // SAFETY: by the caller's contract null or valid.
        let Some(state) = (unsafe { standby.as_mut() }) else {
            return false;
        };
        let record = match record {
            DENON_NOT_IN_VIEW => Record::NotInView,
            DENON_MANUFACTURER => Record::Seen(Some(DenonBrand::Manufacturer)),
            DENON_ADDRESS_BLOCK => Record::Seen(Some(DenonBrand::AddressBlock)),
            _ => Record::Seen(None),
        };
        let (mut rule, mut marks) = state.split();
        let hint = rule.update(record, wanted, now, &mut marks);
        state.join(rule, marks);
        let Some(hint) = hint else {
            return false;
        };
        // SAFETY: by the caller's contract null or writable.
        if let Some(shown) = unsafe { shown.as_mut() } {
            *shown = AscStandbyShown {
                recognized: match hint.recognized {
                    Recognized::Manufacturer => DENON_MANUFACTURER,
                    Recognized::AddressBlock => DENON_ADDRESS_BLOCK,
                    Recognized::StoredMark => STANDBY_BY_STORED_MARK,
                },
                cause: cause_code(Some(hint.cause)),
            };
        }
        true
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ptr;

    // A run through the C interface: the marks the host stores come back
    // as it set them, and the hint shows once, 30 s into the run.
    // covers: P262, P263
    #[test]
    fn the_c_interface_runs_the_standby_rule_with_the_host_s_marks() {
        let mut state = asc_standby_new(false, false);
        let mut shown = AscStandbyShown {
            recognized: 0,
            cause: 0,
        };
        // SAFETY: valid pointers to locals.
        unsafe {
            assert!(!asc_standby_update(
                &mut state,
                DENON_MANUFACTURER,
                true,
                0.0,
                &mut shown
            ));
            asc_standby_outcome(&mut state, STANDBY_OUTCOME_PLAYED, 0.0);
            asc_standby_outcome(&mut state, STANDBY_OUTCOME_NO_ANSWER, 5.0);
            asc_standby_outcome(&mut state, STANDBY_OUTCOME_BETWEEN, 6.0);
            assert!(!asc_standby_update(
                &mut state,
                DENON_MANUFACTURER,
                true,
                34.9,
                &mut shown
            ));
            assert!(asc_standby_update(
                &mut state,
                DENON_MANUFACTURER,
                true,
                35.0,
                &mut shown
            ));
            assert!(!asc_standby_update(
                ptr::null_mut(),
                0,
                true,
                0.0,
                ptr::null_mut()
            ));
        }
        assert!(state.hint && state.recognized_mark && state.hint_shown_mark);
        assert_eq!(
            (shown.recognized, shown.cause),
            (DENON_MANUFACTURER, STANDBY_CAUSE_NO_ANSWER)
        );

        // A relaunch with the stored marks: recognized while not in view,
        // and the hint already shown, so it stays off.
        let mut relaunched = asc_standby_new(true, true);
        // SAFETY: valid pointers to locals.
        unsafe {
            asc_standby_update(
                &mut relaunched,
                DENON_NOT_IN_VIEW,
                true,
                0.0,
                ptr::null_mut(),
            );
            assert!(!asc_standby_update(
                &mut relaunched,
                DENON_NOT_IN_VIEW,
                true,
                40.0,
                ptr::null_mut()
            ));
        }
        assert!(!relaunched.hint);
    }
}
