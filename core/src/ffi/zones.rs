//! Zones through the C interface (S246, S248): a zone click's plan, and
//! which saved group members the one-time migration keeps.
//!
//! The rules are in [`zones`]; this module only turns C arrays and codes
//! into its types. For the host's control thread: it may allocate.

use std::ffi::{CStr, c_char};

use crate::policy::zones::{self, CastLeftOff, ClickAction, Member, MemberKind, Situation, Step};

/// [`MemberKind::Local`].
pub const ZONE_MEMBER_LOCAL: u32 = 0;
/// [`MemberKind::AirPlay`].
pub const ZONE_MEMBER_AIRPLAY: u32 = 1;
/// [`MemberKind::AirPlay2`].
pub const ZONE_MEMBER_AIRPLAY2: u32 = 2;
/// [`MemberKind::Cast`].
pub const ZONE_MEMBER_CAST: u32 = 3;

/// [`ClickAction::Nothing`].
pub const ZONE_CLICK_NOTHING: u32 = 0;
/// [`ClickAction::TurnOn`] while streaming is on.
pub const ZONE_CLICK_TURN_ON: u32 = 1;
/// [`ClickAction::TurnOn`] that starts streaming first.
pub const ZONE_CLICK_START: u32 = 2;
/// [`ClickAction::TurnOff`].
pub const ZONE_CLICK_TURN_OFF: u32 = 3;

/// [`Step::Absent`].
pub const ZONE_STEP_ABSENT: u32 = 0;
/// [`Step::NeedsAirPlay2`].
pub const ZONE_STEP_NEEDS_AIRPLAY2: u32 = 1;
/// [`Step::InTarget`].
pub const ZONE_STEP_IN_TARGET: u32 = 2;
/// [`Step::On`].
pub const ZONE_STEP_ON: u32 = 3;
/// [`Step::Waiting`].
pub const ZONE_STEP_WAITING: u32 = 4;
/// [`Step::Off`].
pub const ZONE_STEP_OFF: u32 = 5;
/// [`CastLeftOff::AnotherCastPlays`].
pub const ZONE_STEP_CAST_ANOTHER_PLAYS: u32 = 6;
/// [`CastLeftOff::OneCastAtATime`].
pub const ZONE_STEP_CAST_ONE_AT_A_TIME: u32 = 7;
/// [`CastLeftOff::Busy`].
pub const ZONE_STEP_CAST_BUSY: u32 = 8;

/// One zone member as the host sees it when the zone is clicked ([`Member`]).
#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AscZoneMember {
    pub kind: u32,
    pub listed: bool,
    pub on: bool,
    pub busy: bool,
    pub needs_airplay2: bool,
    pub in_target: bool,
}

fn member(member: &AscZoneMember) -> Member {
    let kind = match member.kind {
        ZONE_MEMBER_LOCAL => Some(MemberKind::Local),
        ZONE_MEMBER_AIRPLAY => Some(MemberKind::AirPlay),
        ZONE_MEMBER_AIRPLAY2 => Some(MemberKind::AirPlay2),
        ZONE_MEMBER_CAST => Some(MemberKind::Cast),
        _ => None,
    };
    Member {
        kind: kind.unwrap_or(MemberKind::Local),
        // A member of an unknown kind is never switched.
        listed: member.listed && kind.is_some(),
        on: member.on,
        busy: member.busy,
        needs_airplay2: member.needs_airplay2,
        in_target: member.in_target,
    }
}

fn step_code(step: Step) -> u32 {
    match step {
        Step::Absent => ZONE_STEP_ABSENT,
        Step::NeedsAirPlay2 => ZONE_STEP_NEEDS_AIRPLAY2,
        Step::InTarget => ZONE_STEP_IN_TARGET,
        Step::On => ZONE_STEP_ON,
        Step::Waiting => ZONE_STEP_WAITING,
        Step::Off => ZONE_STEP_OFF,
        Step::CastLeftOff(CastLeftOff::AnotherCastPlays) => ZONE_STEP_CAST_ANOTHER_PLAYS,
        Step::CastLeftOff(CastLeftOff::OneCastAtATime) => ZONE_STEP_CAST_ONE_AT_A_TIME,
        Step::CastLeftOff(CastLeftOff::Busy) => ZONE_STEP_CAST_BUSY,
    }
}

/// The plan of a click on a zone ([`zones::click_plan`], S246, S247): its
/// `ZONE_CLICK_` code, with one `ZONE_STEP_` code per member written into
/// `steps`, in zone order. A null array with a count above 0 plans nothing
/// and writes nothing, and a member of an unknown kind is absent.
///
/// # Safety
///
/// `members` and `steps` are null or point to `count` elements each, valid
/// for the call; `steps` may be written.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn asc_zone_click_plan(
    members: *const AscZoneMember,
    count: usize,
    streaming: bool,
    cast_on_elsewhere: bool,
    airplay2_playing_elsewhere: bool,
    steps: *mut u32,
) -> u32 {
    if count > 0 && (members.is_null() || steps.is_null()) {
        return ZONE_CLICK_NOTHING;
    }
    let members: Vec<Member> = if count == 0 {
        Vec::new()
    } else {
        // SAFETY: non-null, and by the caller's contract `count` elements.
        unsafe { std::slice::from_raw_parts(members, count) }
            .iter()
            .map(member)
            .collect()
    };
    let plan = zones::click_plan(
        &members,
        Situation {
            streaming,
            cast_on_elsewhere,
            airplay2_playing_elsewhere,
        },
    );
    if count > 0 {
        // SAFETY: non-null, and by the caller's contract `count` writable elements.
        let out = unsafe { std::slice::from_raw_parts_mut(steps, count) };
        for (slot, step) in out.iter_mut().zip(plan.steps) {
            *slot = step_code(step);
        }
    }
    match plan.action {
        ClickAction::Nothing => ZONE_CLICK_NOTHING,
        ClickAction::TurnOn {
            start_streaming: false,
        } => ZONE_CLICK_TURN_ON,
        ClickAction::TurnOn {
            start_streaming: true,
        } => ZONE_CLICK_START,
        ClickAction::TurnOff => ZONE_CLICK_TURN_OFF,
    }
}

/// The one-time migration's rule for members (S248): with every saved
/// group's member keys in `keys`, group after group in saved order, writes
/// into `keep` whether each stays in its group's zone, false for a key that
/// comes earlier in the list, and for a null or non-UTF-8 one. Does nothing
/// for a null array with a count above 0.
///
/// # Safety
///
/// `keys` and `keep` are null or point to `count` elements each, valid for
/// the call; each key is null or a NUL-terminated string that stays valid
/// and unchanged during the call; `keep` may be written.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn asc_zone_migration_keeps(
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
    let texts: Vec<Option<String>> = keys
        .iter()
        .map(|&key| {
            (!key.is_null())
                .then(|| {
                    // SAFETY: non-null, and by the caller's contract NUL-terminated.
                    unsafe { CStr::from_ptr(key) }
                        .to_str()
                        .ok()
                        .map(str::to_owned)
                })
                .flatten()
        })
        .collect();
    let members: Vec<String> = texts.iter().flatten().cloned().collect();
    let kept = zones::zones_from_groups([zones::SavedGroup {
        id: "",
        name: "",
        members: &members,
    }]);
    let mut kept = kept.list()[0].members.iter().peekable();
    for (slot, text) in keep.iter_mut().zip(&texts) {
        *slot = match text {
            Some(text) if kept.peek() == Some(&text) => {
                kept.next();
                true
            }
            _ => false,
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn member(kind: u32, on: bool) -> AscZoneMember {
        AscZoneMember {
            kind,
            listed: true,
            on,
            ..AscZoneMember::default()
        }
    }

    fn plan(members: &[AscZoneMember], streaming: bool, cast_elsewhere: bool) -> (u32, Vec<u32>) {
        let mut steps = vec![99; members.len()];
        // SAFETY: both arrays hold members.len() elements.
        let action = unsafe {
            asc_zone_click_plan(
                members.as_ptr(),
                members.len(),
                streaming,
                cast_elsewhere,
                false,
                steps.as_mut_ptr(),
            )
        };
        (action, steps)
    }

    // covers: S246, S247
    #[test]
    fn the_click_plan_crosses_the_boundary_as_the_core_makes_it() {
        let busy = AscZoneMember {
            busy: true,
            ..member(ZONE_MEMBER_CAST, false)
        };
        let in_pair = AscZoneMember {
            in_target: true,
            ..member(ZONE_MEMBER_AIRPLAY2, false)
        };
        let pair = AscZoneMember {
            needs_airplay2: true,
            ..member(ZONE_MEMBER_AIRPLAY2, false)
        };
        let hidden = AscZoneMember {
            listed: false,
            ..member(ZONE_MEMBER_LOCAL, false)
        };
        let members = [
            busy,
            member(ZONE_MEMBER_CAST, false),
            member(ZONE_MEMBER_CAST, false),
            member(ZONE_MEMBER_AIRPLAY2, false),
            member(ZONE_MEMBER_AIRPLAY2, false),
            member(ZONE_MEMBER_AIRPLAY, false),
            in_pair,
            pair,
            hidden,
        ];
        assert_eq!(
            plan(&members, false, false),
            (
                ZONE_CLICK_START,
                vec![
                    ZONE_STEP_CAST_ONE_AT_A_TIME,
                    ZONE_STEP_ON,
                    ZONE_STEP_CAST_ONE_AT_A_TIME,
                    ZONE_STEP_ON,
                    ZONE_STEP_WAITING,
                    ZONE_STEP_ON,
                    ZONE_STEP_IN_TARGET,
                    ZONE_STEP_NEEDS_AIRPLAY2,
                    ZONE_STEP_ABSENT,
                ]
            )
        );
        assert_eq!(
            plan(&[busy, member(ZONE_MEMBER_CAST, false)], true, true),
            (
                ZONE_CLICK_NOTHING,
                vec![ZONE_STEP_CAST_ANOTHER_PLAYS, ZONE_STEP_CAST_ANOTHER_PLAYS]
            )
        );
        assert_eq!(
            plan(&[busy, member(ZONE_MEMBER_LOCAL, false)], true, false),
            (ZONE_CLICK_TURN_ON, vec![ZONE_STEP_CAST_BUSY, ZONE_STEP_ON])
        );
        assert_eq!(
            plan(&[member(ZONE_MEMBER_LOCAL, true)], true, false),
            (ZONE_CLICK_TURN_OFF, vec![ZONE_STEP_OFF])
        );
    }

    // covers: S246
    #[test]
    fn bad_zone_arguments_never_switch() {
        assert_eq!(
            plan(
                &[member(7, false), member(ZONE_MEMBER_LOCAL, true)],
                true,
                false
            ),
            (ZONE_CLICK_TURN_OFF, vec![ZONE_STEP_ABSENT, ZONE_STEP_OFF])
        );
        assert_eq!(plan(&[member(7, false)], true, false).0, ZONE_CLICK_NOTHING);
        let mut steps = [99u32; 2];
        // SAFETY: a null member array is allowed; nothing is written.
        let action = unsafe {
            asc_zone_click_plan(std::ptr::null(), 2, true, false, false, steps.as_mut_ptr())
        };
        assert_eq!((action, steps), (ZONE_CLICK_NOTHING, [99, 99]));
        let one = [member(ZONE_MEMBER_LOCAL, false)];
        // SAFETY: a null step array is allowed.
        let action = unsafe {
            asc_zone_click_plan(one.as_ptr(), 1, true, false, false, std::ptr::null_mut())
        };
        assert_eq!(action, ZONE_CLICK_NOTHING);
        // SAFETY: nothing is read or written for a count of 0.
        let empty = unsafe {
            asc_zone_click_plan(
                std::ptr::null(),
                0,
                true,
                false,
                false,
                std::ptr::null_mut(),
            )
        };
        assert_eq!(empty, ZONE_CLICK_NOTHING);
    }

    // covers: S248
    #[test]
    fn the_migration_keeps_each_key_in_its_first_group() {
        let not_utf8 = c"\xff\xfe";
        let keys = [
            c"lc:usb".as_ptr(),
            c"ap:1".as_ptr(),
            c"ap:1".as_ptr(),
            std::ptr::null(),
            not_utf8.as_ptr(),
            c"lc:usb".as_ptr(),
            c"ap:2".as_ptr(),
        ];
        let mut keep = [true, false, true, true, true, true, false];
        // SAFETY: both arrays hold keys.len() elements; every key is null or a literal.
        unsafe { asc_zone_migration_keeps(keys.as_ptr(), keys.len(), keep.as_mut_ptr()) };
        assert_eq!(keep, [true, true, false, false, false, false, true]);
        let mut untouched = [true];
        // SAFETY: a null key array is allowed; nothing is written.
        unsafe { asc_zone_migration_keeps(std::ptr::null(), 1, untouched.as_mut_ptr()) };
        assert_eq!(untouched, [true]);
    }
}
