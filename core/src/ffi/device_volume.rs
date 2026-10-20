//! An output device's own volume and mute through the C interface (S192 to
//! S202): what a reading means, what the one-click fix writes and when a new
//! reading is worth a log line, for the host's control thread.
//!
//! The rules are in [`device_volume`]; this module only turns the host's
//! plain values into a [`DeviceVolume`] and the answers back into C values.
//! Nothing here allocates, so a call may run on any thread, but no host
//! calls it from an audio thread: the readings come from Core Audio's or
//! Windows' property calls, which belong on the control thread (S193, S194).

use std::ffi::{CStr, c_char};

use crate::policy::device_volume::{self, Decision, DeviceVolume};

/// [`Decision::Unknown`]: neither part is known, the row shows nothing.
pub const DEVICE_VOLUME_UNKNOWN: u32 = 0;
/// [`Decision::Fine`]: not muted and not low, the row shows nothing.
pub const DEVICE_VOLUME_FINE: u32 = 1;
/// [`Decision::Low`]: the volume is 25 % or less.
pub const DEVICE_VOLUME_LOW: u32 = 2;
/// [`Decision::Muted`]: muted at a volume that isn't low or isn't known.
pub const DEVICE_VOLUME_MUTED: u32 = 3;
/// [`Decision::MutedAndLow`]: muted, and 25 % or less too.
pub const DEVICE_VOLUME_MUTED_AND_LOW: u32 = 4;

/// One reading of an output device's own volume and mute, as the host read
/// it: a part it couldn't read, or can't set, has its `has_` flag false and
/// its value ignored.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct AscDeviceVolume {
    pub has_volume: bool,
    pub volume: f64,
    pub has_mute: bool,
    pub muted: bool,
}

/// What a reading means for the row and what one click on its fix writes.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct AscDeviceVolumeDecision {
    pub decision: u32,
    pub shows_hint: bool,
    pub has_percent: bool,
    pub percent: u32,
    pub fix_sets_volume: bool,
    pub fix_volume: f64,
    pub fix_unmutes: bool,
}

impl From<AscDeviceVolume> for DeviceVolume {
    fn from(reading: AscDeviceVolume) -> Self {
        DeviceVolume {
            volume: reading.has_volume.then_some(reading.volume),
            muted: reading.has_mute.then_some(reading.muted),
        }
    }
}

/// A decision as its C value.
fn code(decision: Decision) -> u32 {
    match decision {
        Decision::Unknown => DEVICE_VOLUME_UNKNOWN,
        Decision::Fine => DEVICE_VOLUME_FINE,
        Decision::Low => DEVICE_VOLUME_LOW,
        Decision::Muted => DEVICE_VOLUME_MUTED,
        Decision::MutedAndLow => DEVICE_VOLUME_MUTED_AND_LOW,
    }
}

/// The decision a C value names, or `None` for a value no decision has.
fn decision(code: u32) -> Option<Decision> {
    match code {
        DEVICE_VOLUME_UNKNOWN => Some(Decision::Unknown),
        DEVICE_VOLUME_FINE => Some(Decision::Fine),
        DEVICE_VOLUME_LOW => Some(Decision::Low),
        DEVICE_VOLUME_MUTED => Some(Decision::Muted),
        DEVICE_VOLUME_MUTED_AND_LOW => Some(Decision::MutedAndLow),
        _ => None,
    }
}

/// What `reading` means and what its fix writes ([`DeviceVolume::decision`],
/// [`DeviceVolume::percent`], [`DeviceVolume::fix`]): the decision (S196),
/// whether the row shows a hint, the whole percent when the volume is known
/// and finite, and the fix: 0.75 only for a low volume, the unmute only for
/// a muted device (S197).
#[unsafe(no_mangle)]
pub extern "C" fn asc_device_volume_decide(reading: AscDeviceVolume) -> AscDeviceVolumeDecision {
    let reading = DeviceVolume::from(reading);
    let decision = reading.decision();
    let percent = reading.percent();
    let fix = reading.fix();
    AscDeviceVolumeDecision {
        decision: code(decision),
        shows_hint: decision.shows_hint(),
        has_percent: percent.is_some(),
        percent: percent.unwrap_or(0),
        fix_sets_volume: fix.volume.is_some(),
        fix_volume: fix.volume.unwrap_or(0.0),
        fix_unmutes: fix.unmute,
    }
}

/// Whether a reading whose decision is `now` gets a log line after one whose
/// decision was `previous` ([`device_volume::decision_change`], S201): only
/// when the two differ. The first reading (`has_previous` false) has a line
/// of its own, so it is no change here; a value no decision has counts as no
/// decision, so a bad `previous` gives false like a first reading, and a bad
/// `now` gives false.
#[unsafe(no_mangle)]
pub extern "C" fn asc_device_volume_decision_changed(
    has_previous: bool,
    previous: u32,
    now: u32,
) -> bool {
    let Some(now) = decision(now) else {
        return false;
    };
    let previous = if has_previous {
        decision(previous)
    } else {
        None
    };
    device_volume::decision_change(previous, now).is_some()
}

/// A decision's name in the debug log, "unknown", "fine", "low", "muted" or
/// "muted and low", as a static C string; null for a value no decision has
/// (S201).
#[unsafe(no_mangle)]
pub extern "C" fn asc_device_volume_decision_name(decision_code: u32) -> *const c_char {
    let name: &CStr = match decision(decision_code) {
        Some(Decision::Unknown) => c"unknown",
        Some(Decision::Fine) => c"fine",
        Some(Decision::Low) => c"low",
        Some(Decision::Muted) => c"muted",
        Some(Decision::MutedAndLow) => c"muted and low",
        None => return std::ptr::null(),
    };
    name.as_ptr()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(volume: Option<f64>, muted: Option<bool>) -> AscDeviceVolume {
        AscDeviceVolume {
            has_volume: volume.is_some(),
            volume: volume.unwrap_or(f64::NAN),
            has_mute: muted.is_some(),
            muted: muted.unwrap_or(true),
        }
    }

    // covers: S196, S197
    #[test]
    fn the_decision_crosses_the_boundary_as_the_core_makes_it() {
        // The quiet MacBook speakers at 13 %.
        let low = asc_device_volume_decide(at(Some(0.13), Some(false)));
        assert_eq!(
            low,
            AscDeviceVolumeDecision {
                decision: DEVICE_VOLUME_LOW,
                shows_hint: true,
                has_percent: true,
                percent: 13,
                fix_sets_volume: true,
                fix_volume: device_volume::FIX_VOLUME,
                fix_unmutes: false,
            }
        );
        let edge = asc_device_volume_decide(at(Some(0.25), Some(false)));
        assert_eq!(edge.decision, DEVICE_VOLUME_LOW);
        assert!(edge.shows_hint && edge.fix_sets_volume && !edge.fix_unmutes);
        assert_eq!(edge.percent, 25);
        let fine = asc_device_volume_decide(at(Some(0.26), Some(false)));
        assert_eq!(fine.decision, DEVICE_VOLUME_FINE);
        assert!(!fine.shows_hint && !fine.fix_sets_volume && !fine.fix_unmutes);
        assert_eq!(fine.percent, 26);
        let muted = asc_device_volume_decide(at(Some(0.9), Some(true)));
        assert_eq!(muted.decision, DEVICE_VOLUME_MUTED);
        assert!(muted.shows_hint && muted.fix_unmutes && !muted.fix_sets_volume);
        let both = asc_device_volume_decide(at(Some(0.0), Some(true)));
        assert_eq!(both.decision, DEVICE_VOLUME_MUTED_AND_LOW);
        assert!(both.fix_unmutes && both.fix_sets_volume && both.fix_volume == 0.75);
        assert_eq!(both.percent, 0);
    }

    // covers: S196
    #[test]
    fn a_part_without_its_flag_is_unknown_whatever_its_value() {
        let unknown = asc_device_volume_decide(at(None, None));
        assert_eq!(unknown.decision, DEVICE_VOLUME_UNKNOWN);
        assert!(!unknown.shows_hint && !unknown.has_percent);
        assert!(!unknown.fix_sets_volume && !unknown.fix_unmutes);
        // A mute that isn't known never mutes, even with `muted` set.
        let volume_only = asc_device_volume_decide(at(Some(0.5), None));
        assert_eq!(volume_only.decision, DEVICE_VOLUME_FINE);
        let not_finite = asc_device_volume_decide(at(Some(f64::INFINITY), Some(false)));
        assert_eq!(not_finite.decision, DEVICE_VOLUME_FINE);
        assert!(!not_finite.has_percent && !not_finite.fix_sets_volume);
        let mute_only = asc_device_volume_decide(at(None, Some(true)));
        assert_eq!(mute_only.decision, DEVICE_VOLUME_MUTED);
        assert!(!mute_only.has_percent && mute_only.fix_unmutes && !mute_only.fix_sets_volume);
    }

    // covers: S201
    #[test]
    fn a_log_line_only_when_the_decision_changes() {
        assert!(!asc_device_volume_decision_changed(
            false,
            0,
            DEVICE_VOLUME_LOW
        ));
        assert!(!asc_device_volume_decision_changed(
            true,
            DEVICE_VOLUME_FINE,
            DEVICE_VOLUME_FINE
        ));
        assert!(asc_device_volume_decision_changed(
            true,
            DEVICE_VOLUME_FINE,
            DEVICE_VOLUME_LOW
        ));
        assert!(asc_device_volume_decision_changed(
            true,
            DEVICE_VOLUME_MUTED,
            DEVICE_VOLUME_MUTED_AND_LOW
        ));
        assert!(!asc_device_volume_decision_changed(
            true,
            99,
            DEVICE_VOLUME_LOW
        ));
        assert!(!asc_device_volume_decision_changed(
            true,
            DEVICE_VOLUME_LOW,
            99
        ));
    }

    // covers: S201
    #[test]
    fn every_decision_has_the_name_the_core_logs() {
        let all = [
            Decision::Unknown,
            Decision::Fine,
            Decision::Low,
            Decision::Muted,
            Decision::MutedAndLow,
        ];
        for decision in all {
            let name = asc_device_volume_decision_name(code(decision));
            assert!(!name.is_null());
            // SAFETY: a non-null name is a static NUL-terminated string.
            let name = unsafe { CStr::from_ptr(name) };
            assert_eq!(name.to_str(), Ok(decision.to_string().as_str()));
            assert_eq!(super::decision(code(decision)), Some(decision));
        }
        assert!(asc_device_volume_decision_name(5).is_null());
    }
}
