//! Speaker presets through the C interface (S251 to S253): which stored
//! members a preset keeps, its levels as stored, a recall's plan and the
//! current-preset check.
//!
//! The rules are in [`presets`]; this module only turns C arrays and codes
//! into its types. A volume that is NaN stands for none. For the host's
//! control thread: it may allocate.

use std::ffi::{CStr, c_char};

use crate::eq::BAND_COUNT;
use crate::policy::presets::{
    self, LeftOff, Levels, MemberNow, RecallAction, RecallMember, RecallOther, RecallStep, ZoneNow,
};
use crate::policy::zones::{MemberKind, zone_volume};

use super::zones::{
    ZONE_MEMBER_AIRPLAY, ZONE_MEMBER_AIRPLAY2, ZONE_MEMBER_CAST, ZONE_MEMBER_LOCAL,
};

/// [`RecallAction::Nothing`].
pub const PRESET_RECALL_NOTHING: u32 = 0;
/// [`RecallAction::Apply`] while streaming is on.
pub const PRESET_RECALL_APPLY: u32 = 1;
/// [`RecallAction::Apply`] that starts streaming first.
pub const PRESET_RECALL_START: u32 = 2;

/// [`RecallStep::On`].
pub const PRESET_STEP_ON: u32 = 0;
/// [`RecallStep::Waiting`].
pub const PRESET_STEP_WAITING: u32 = 1;
/// [`LeftOff::NotFound`].
pub const PRESET_STEP_NOT_FOUND: u32 = 2;
/// [`LeftOff::Hidden`].
pub const PRESET_STEP_HIDDEN: u32 = 3;
/// [`LeftOff::NeedsAirPlay2`].
pub const PRESET_STEP_NEEDS_AIRPLAY2: u32 = 4;
/// [`LeftOff::InTarget`].
pub const PRESET_STEP_IN_TARGET: u32 = 5;
/// [`LeftOff::Busy`].
pub const PRESET_STEP_BUSY: u32 = 6;
/// [`RecallStep::Ignored`].
pub const PRESET_STEP_IGNORED: u32 = 7;

/// One stored member as the host sees it at recall ([`RecallMember`]).
#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AscPresetMember {
    pub kind: u32,
    pub found: bool,
    pub hidden: bool,
    pub on: bool,
    pub busy: bool,
    pub needs_airplay2: bool,
    pub in_target: bool,
    pub fixed_volume: bool,
    pub has_volume: bool,
}

/// A speaker outside the preset at recall ([`RecallOther`]).
#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AscPresetOther {
    pub on: bool,
    pub reference: bool,
}

/// One stored member as the current-preset check sees it ([`MemberNow`]).
#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct AscPresetMemberNow {
    pub on: bool,
    pub stored_volume: f64,
    pub volume: f64,
}

/// One stored zone volume as the current-preset check sees it ([`ZoneNow`]).
#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct AscPresetZoneNow {
    pub stored: f64,
    pub volume: f64,
}

fn optional(value: f64) -> Option<f64> {
    (!value.is_nan()).then_some(value)
}

fn recall_member(member: &AscPresetMember) -> RecallMember {
    let kind = match member.kind {
        ZONE_MEMBER_LOCAL => Some(MemberKind::Local),
        ZONE_MEMBER_AIRPLAY => Some(MemberKind::AirPlay),
        ZONE_MEMBER_AIRPLAY2 => Some(MemberKind::AirPlay2),
        ZONE_MEMBER_CAST => Some(MemberKind::Cast),
        _ => None,
    };
    RecallMember {
        kind: kind.unwrap_or(MemberKind::Local),
        // A member of an unknown kind is never switched.
        found: member.found && kind.is_some(),
        hidden: member.hidden,
        on: member.on,
        busy: member.busy,
        needs_airplay2: member.needs_airplay2,
        in_target: member.in_target,
        fixed_volume: member.fixed_volume,
        has_volume: member.has_volume,
    }
}

fn step_code(step: RecallStep) -> u32 {
    match step {
        RecallStep::On => PRESET_STEP_ON,
        RecallStep::Waiting => PRESET_STEP_WAITING,
        RecallStep::LeftOff(LeftOff::NotFound) => PRESET_STEP_NOT_FOUND,
        RecallStep::LeftOff(LeftOff::Hidden) => PRESET_STEP_HIDDEN,
        RecallStep::LeftOff(LeftOff::NeedsAirPlay2) => PRESET_STEP_NEEDS_AIRPLAY2,
        RecallStep::LeftOff(LeftOff::InTarget) => PRESET_STEP_IN_TARGET,
        RecallStep::LeftOff(LeftOff::Busy) => PRESET_STEP_BUSY,
        RecallStep::Ignored => PRESET_STEP_IGNORED,
    }
}

/// `count` elements at `pointer`, or none for a count of 0.
///
/// # Safety
///
/// `pointer` is non-null and points to `count` elements, unless `count` is 0.
unsafe fn elements<'a, T>(pointer: *const T, count: usize) -> &'a [T] {
    if count == 0 {
        &[]
    } else {
        // SAFETY: by the caller's contract, non-null with `count` elements.
        unsafe { std::slice::from_raw_parts(pointer, count) }
    }
}

/// Which stored member keys a preset keeps ([`presets::member_keeps`],
/// S251): writes into `keep` false for a key that isn't a speaker key with
/// an ID, one earlier in the list, a Cast key after the first, and a null or
/// non-UTF-8 one. Does nothing for a null array with a count above 0.
///
/// # Safety
///
/// `keys` and `keep` are null or point to `count` elements each, valid for
/// the call; each key is null or a NUL-terminated string that stays valid
/// and unchanged during the call; `keep` may be written.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn asc_preset_member_keeps(
    keys: *const *const c_char,
    count: usize,
    keep: *mut bool,
) {
    if count == 0 || keys.is_null() || keep.is_null() {
        return;
    }
    // SAFETY: non-null, and by the caller's contract `count` elements each.
    let (keys, keep) = unsafe {
        (
            std::slice::from_raw_parts(keys, count),
            std::slice::from_raw_parts_mut(keep, count),
        )
    };
    let texts = keys.iter().map(|&key| {
        if key.is_null() {
            return None;
        }
        // SAFETY: non-null, and by the caller's contract NUL-terminated.
        let text = unsafe { CStr::from_ptr(key) };
        text.to_str().ok()
    });
    for (slot, kept) in keep.iter_mut().zip(presets::member_keeps(texts)) {
        *slot = kept;
    }
}

/// A preset's levels as stored (S251), in place: each member volume clamped
/// to 0 to 100 (NaN, none, stays; infinity becomes none), each zone volume
/// as a zone's, the master clamped (the default when it isn't a number) and
/// the ten gains on the controls' steps (S23). A null array is skipped.
///
/// # Safety
///
/// `volumes` and `zone_volumes` are null or point to `count` and
/// `zone_count` elements; `master` is null or points to one; `gains_db` is
/// null or points to 10; all writable and valid for the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn asc_preset_levels(
    volumes: *mut f64,
    count: usize,
    zone_volumes: *mut f64,
    zone_count: usize,
    master: *mut f64,
    gains_db: *mut f64,
) {
    if !volumes.is_null() && count > 0 {
        // SAFETY: non-null, and by the caller's contract `count` writable elements.
        for volume in unsafe { std::slice::from_raw_parts_mut(volumes, count) } {
            *volume = presets::member_volume(optional(*volume)).unwrap_or(f64::NAN);
        }
    }
    if !zone_volumes.is_null() && zone_count > 0 {
        // SAFETY: non-null, and by the caller's contract `zone_count` writable elements.
        for volume in unsafe { std::slice::from_raw_parts_mut(zone_volumes, zone_count) } {
            *volume = zone_volume(*volume);
        }
    }
    if !master.is_null() {
        // SAFETY: non-null, and by the caller's contract one writable element.
        unsafe { *master = presets::master_volume(*master) };
    }
    if !gains_db.is_null() {
        // SAFETY: non-null, and by the caller's contract ten writable elements.
        for gain in unsafe { std::slice::from_raw_parts_mut(gains_db, BAND_COUNT) } {
            *gain = crate::eq::quantize_gain_db(*gain);
        }
    }
}

/// The plan of a preset recall ([`presets::recall_plan`], S252): its
/// `PRESET_RECALL_` code, with one `PRESET_STEP_` code and one set-volume
/// flag per stored member, in preset order, and one switch-off flag per
/// speaker outside the preset. Null arrays with a count above 0 plan nothing
/// and write nothing, and a member of an unknown kind is not found.
///
/// # Safety
///
/// `members`, `steps` and `set_volume` are null or point to `count`
/// elements each, `others` and `switch_off` to `other_count`, valid for the
/// call; the output arrays may be written.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn asc_preset_recall_plan(
    members: *const AscPresetMember,
    count: usize,
    others: *const AscPresetOther,
    other_count: usize,
    streaming: bool,
    steps: *mut u32,
    set_volume: *mut bool,
    switch_off: *mut bool,
) -> u32 {
    if count > 0 && (members.is_null() || steps.is_null() || set_volume.is_null())
        || other_count > 0 && (others.is_null() || switch_off.is_null())
    {
        return PRESET_RECALL_NOTHING;
    }
    // SAFETY: checked above, and by the caller's contract the counts hold.
    let (members, others) = unsafe { (elements(members, count), elements(others, other_count)) };
    let members: Vec<RecallMember> = members.iter().map(recall_member).collect();
    let others: Vec<RecallOther> = others
        .iter()
        .map(|other| RecallOther {
            on: other.on,
            reference: other.reference,
        })
        .collect();
    let plan = presets::recall_plan(&members, &others, streaming);
    if count > 0 {
        // SAFETY: non-null, and by the caller's contract `count` writable elements each.
        let (out_steps, out_volume) = unsafe {
            (
                std::slice::from_raw_parts_mut(steps, count),
                std::slice::from_raw_parts_mut(set_volume, count),
            )
        };
        for ((slot, flag), (step, set)) in out_steps
            .iter_mut()
            .zip(out_volume.iter_mut())
            .zip(plan.steps.iter().zip(&plan.set_volume))
        {
            *slot = step_code(*step);
            *flag = *set;
        }
    }
    if other_count > 0 {
        // SAFETY: non-null, and by the caller's contract `other_count` writable elements.
        let out = unsafe { std::slice::from_raw_parts_mut(switch_off, other_count) };
        for (slot, off) in out.iter_mut().zip(&plan.switch_off) {
            *slot = *off;
        }
    }
    match plan.action {
        RecallAction::Nothing => PRESET_RECALL_NOTHING,
        RecallAction::Apply {
            start_streaming: false,
        } => PRESET_RECALL_APPLY,
        RecallAction::Apply {
            start_streaming: true,
        } => PRESET_RECALL_START,
    }
}

/// Whether the host plays a preset exactly ([`presets::matches`], S253):
/// `on_now` speakers are on, the reference output left out; NaN stands for
/// no own volume and, in a zone's `volume`, a deleted zone. False for a null
/// array with a count above 0, or null gains.
///
/// # Safety
///
/// `members` and `zones` are null or point to `count` and `zone_count`
/// elements; `stored_gains_db` and `gains_db` are null or point to 10; all
/// valid for the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn asc_preset_matches(
    members: *const AscPresetMemberNow,
    count: usize,
    on_now: usize,
    zones: *const AscPresetZoneNow,
    zone_count: usize,
    stored_master: f64,
    stored_gains_db: *const f64,
    master: f64,
    gains_db: *const f64,
) -> bool {
    if count > 0 && members.is_null()
        || zone_count > 0 && zones.is_null()
        || stored_gains_db.is_null()
        || gains_db.is_null()
    {
        return false;
    }
    // SAFETY: checked above, and by the caller's contract the counts hold.
    let (members, zones, stored_gains, gains) = unsafe {
        (
            elements(members, count),
            elements(zones, zone_count),
            elements(stored_gains_db, BAND_COUNT),
            elements(gains_db, BAND_COUNT),
        )
    };
    let members: Vec<MemberNow> = members
        .iter()
        .map(|member| MemberNow {
            on: member.on,
            stored_volume: optional(member.stored_volume),
            volume: optional(member.volume),
        })
        .collect();
    let zones: Vec<ZoneNow> = zones
        .iter()
        .map(|zone| ZoneNow {
            stored: zone.stored,
            volume: optional(zone.volume),
        })
        .collect();
    let ten = |slice: &[f64]| {
        let mut out = [0.0; BAND_COUNT];
        out.copy_from_slice(slice);
        out
    };
    presets::matches(
        &members,
        on_now,
        &zones,
        Levels {
            master: stored_master,
            gains_db: ten(stored_gains),
        },
        Levels {
            master,
            gains_db: ten(gains),
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn member(kind: u32) -> AscPresetMember {
        AscPresetMember {
            kind,
            found: true,
            has_volume: true,
            ..AscPresetMember::default()
        }
    }

    fn plan(
        members: &[AscPresetMember],
        others: &[AscPresetOther],
        streaming: bool,
    ) -> (u32, Vec<u32>, Vec<bool>, Vec<bool>) {
        let mut steps = vec![99; members.len()];
        let mut set = vec![false; members.len()];
        let mut off = vec![false; others.len()];
        // SAFETY: each array holds the count passed with it.
        let action = unsafe {
            asc_preset_recall_plan(
                members.as_ptr(),
                members.len(),
                others.as_ptr(),
                others.len(),
                streaming,
                steps.as_mut_ptr(),
                set.as_mut_ptr(),
                off.as_mut_ptr(),
            )
        };
        (action, steps, set, off)
    }

    // covers: S252
    #[test]
    fn the_recall_plan_crosses_the_boundary_as_the_core_makes_it() {
        let fixed_cast = AscPresetMember {
            fixed_volume: true,
            ..member(ZONE_MEMBER_CAST)
        };
        let members = [
            member(ZONE_MEMBER_LOCAL),
            fixed_cast,
            member(ZONE_MEMBER_AIRPLAY2),
            member(ZONE_MEMBER_AIRPLAY2),
            AscPresetMember {
                hidden: true,
                ..member(ZONE_MEMBER_AIRPLAY)
            },
            AscPresetMember {
                found: false,
                ..member(ZONE_MEMBER_AIRPLAY)
            },
            AscPresetMember {
                needs_airplay2: true,
                found: false,
                ..member(ZONE_MEMBER_AIRPLAY2)
            },
            AscPresetMember {
                in_target: true,
                ..member(ZONE_MEMBER_AIRPLAY2)
            },
            member(ZONE_MEMBER_CAST),
            member(9),
        ];
        let others = [
            AscPresetOther {
                on: true,
                reference: false,
            },
            AscPresetOther {
                on: true,
                reference: true,
            },
            AscPresetOther::default(),
        ];
        assert_eq!(
            plan(&members, &others, false),
            (
                PRESET_RECALL_START,
                vec![
                    PRESET_STEP_ON,
                    PRESET_STEP_ON,
                    PRESET_STEP_ON,
                    PRESET_STEP_WAITING,
                    PRESET_STEP_HIDDEN,
                    PRESET_STEP_NOT_FOUND,
                    PRESET_STEP_NEEDS_AIRPLAY2,
                    PRESET_STEP_IN_TARGET,
                    PRESET_STEP_IGNORED,
                    PRESET_STEP_NOT_FOUND,
                ],
                vec![
                    true, false, true, true, false, false, false, false, false, false
                ],
                vec![true, false, false],
            )
        );
        let busy = AscPresetMember {
            busy: true,
            ..member(ZONE_MEMBER_CAST)
        };
        assert_eq!(
            plan(&[busy], &others, true),
            (
                PRESET_RECALL_NOTHING,
                vec![PRESET_STEP_BUSY],
                vec![false],
                vec![false, false, false]
            )
        );
        assert_eq!(
            plan(&[member(ZONE_MEMBER_LOCAL)], &[], true).0,
            PRESET_RECALL_APPLY
        );
    }

    // covers: S252
    #[test]
    fn bad_recall_arguments_never_switch() {
        let mut steps = [99u32; 1];
        let mut set = [true; 1];
        // SAFETY: a null member array is allowed; nothing is written.
        let action = unsafe {
            asc_preset_recall_plan(
                std::ptr::null(),
                1,
                std::ptr::null(),
                0,
                false,
                steps.as_mut_ptr(),
                set.as_mut_ptr(),
                std::ptr::null_mut(),
            )
        };
        assert_eq!((action, steps, set), (PRESET_RECALL_NOTHING, [99], [true]));
        // SAFETY: nothing is read or written for counts of 0.
        let empty = unsafe {
            asc_preset_recall_plan(
                std::ptr::null(),
                0,
                std::ptr::null(),
                0,
                true,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        assert_eq!(empty, PRESET_RECALL_NOTHING);
    }

    // covers: S251
    #[test]
    fn member_keys_and_levels_are_kept_as_a_preset_stores_them() {
        let not_utf8 = c"\xff\xfe";
        let keys = [
            c"lc:usb".as_ptr(),
            c"cc:den".as_ptr(),
            c"lc:usb".as_ptr(),
            c"cc:attic".as_ptr(),
            std::ptr::null(),
            not_utf8.as_ptr(),
            c"bt:x".as_ptr(),
            c"ap:".as_ptr(),
            c"ap:native:A1".as_ptr(),
        ];
        let mut keep = [false; 9];
        // SAFETY: both arrays hold keys.len() elements; every key is null or a literal.
        unsafe { asc_preset_member_keeps(keys.as_ptr(), keys.len(), keep.as_mut_ptr()) };
        assert_eq!(
            keep,
            [true, true, false, false, false, false, false, false, true]
        );

        let mut volumes = [140.0, f64::NAN, -3.0, f64::INFINITY, 42.5];
        let mut zones = [f64::NAN, 120.0, 30.0];
        let mut master = f64::INFINITY;
        let mut gains = [0.3, 0.2, -13.0, 12.6, f64::NAN, 1.0, 2.0, 3.0, 4.0, 5.0];
        // SAFETY: every array holds the count passed with it, gains ten.
        unsafe {
            asc_preset_levels(
                volumes.as_mut_ptr(),
                volumes.len(),
                zones.as_mut_ptr(),
                zones.len(),
                &mut master,
                gains.as_mut_ptr(),
            );
        }
        assert_eq!(volumes[0], 100.0);
        assert!(volumes[1].is_nan() && volumes[3].is_nan());
        assert_eq!((volumes[2], volumes[4]), (0.0, 42.5));
        assert_eq!(zones, [100.0, 100.0, 30.0]);
        assert_eq!(master, 100.0);
        assert_eq!(gains, [0.5, 0.0, -12.0, 12.0, 0.0, 1.0, 2.0, 3.0, 4.0, 5.0]);
    }

    // covers: S253
    #[test]
    fn the_current_check_crosses_the_boundary_as_the_core_makes_it() {
        let gains = [1.0; 10];
        let other_gains = [0.0; 10];
        let members = [
            AscPresetMemberNow {
                on: true,
                stored_volume: 40.0,
                volume: 40.0,
            },
            AscPresetMemberNow {
                on: true,
                stored_volume: f64::NAN,
                volume: 15.0,
            },
        ];
        let zones = [
            AscPresetZoneNow {
                stored: 70.0,
                volume: 70.0,
            },
            AscPresetZoneNow {
                stored: 20.0,
                volume: f64::NAN,
            },
        ];
        let matches = |on_now: usize, master: f64, now: &[f64; 10]| {
            // SAFETY: the arrays hold their counts, gains ten each.
            unsafe {
                asc_preset_matches(
                    members.as_ptr(),
                    members.len(),
                    on_now,
                    zones.as_ptr(),
                    zones.len(),
                    80.0,
                    gains.as_ptr(),
                    master,
                    now.as_ptr(),
                )
            }
        };
        assert!(matches(2, 80.0, &gains));
        assert!(!matches(3, 80.0, &gains), "a third speaker on");
        assert!(!matches(2, 81.0, &gains));
        assert!(!matches(2, 80.0, &other_gains));
        // SAFETY: null gains answer false without reading.
        let null = unsafe {
            asc_preset_matches(
                std::ptr::null(),
                0,
                0,
                std::ptr::null(),
                0,
                80.0,
                std::ptr::null(),
                80.0,
                gains.as_ptr(),
            )
        };
        assert!(!null);
    }
}
