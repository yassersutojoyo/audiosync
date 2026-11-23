//! The host's side of AirPlay rows and groups: which receiver gets a row
//! (S222, P176), when a stereo pair or home theater group holds its member
//! (P176), how a target reads from its members (P176, P181), why an Apple TV
//! and its home theater group never play at once (P177, P229), and how many
//! receivers one announced address lists.
//!
//! Plain data in and out; the Mac host keeps its own copy in Swift for now.
//! Where the hosts still differ, a [`HostRules`] field names the difference
//! and each host's constant holds what it does today.

/// The most receivers the Windows and Linux app lists under one announced
/// address, a bridge that fronts many speakers from one host included. The
/// announcer picks that address, so this bounds one address, not one host.
pub const MAX_LISTED_PER_ADDRESS: usize = 64;

/// What one host does where the hosts still differ.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HostRules {
    /// The most receivers listed under one announced address, or no limit.
    pub listed_per_address: Option<usize>,
}

pub const MAC: HostRules = HostRules {
    listed_per_address: None,
};

/// The Windows and Linux app.
pub const DESKTOP: HostRules = HostRules {
    listed_per_address: Some(MAX_LISTED_PER_ADDRESS),
};

/// Whether a receiver not listed yet, at an address that lists `listed_at`
/// already, is left out.
pub fn address_full(rules: HostRules, listed_at: usize) -> bool {
    rules
        .listed_per_address
        .is_some_and(|limit| listed_at >= limit)
}

/// Whether a receiver gets a row: a target keeps its row with the reason it
/// can't play; a single receiver that can't play has none (S222, P176).
pub fn has_row(is_target: bool, cant_play: bool) -> bool {
    is_target || !cant_play
}

/// Whether a speaker plays only as part of its target: the sender holds it
/// for one, or it is off and its listing names a target while targets are
/// listed at all (P176). One left on by itself before its listing named a
/// target still plays alone.
pub fn held_by_target(
    in_added_target: bool,
    targets_listed: bool,
    listing_names_target: bool,
    on: bool,
) -> bool {
    in_added_target || (targets_listed && listing_names_target && !on)
}

/// How a target reads from its members in view (P176, P181).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TargetReads {
    /// A member asks for a password, which is asked for by name first.
    AsksPassword,
    /// A member can play.
    CanPlay,
    /// None can: it reads as its first member, or as able with none in view.
    FirstMember,
}

pub fn target_reads(any_asks_password: bool, any_can_play: bool) -> TargetReads {
    if any_asks_password {
        TargetReads::AsksPassword
    } else if any_can_play {
        TargetReads::CanPlay
    } else {
        TargetReads::FirstMember
    }
}

/// Why a speaker or target can't be switched on next to the other side of
/// its home theater group (P177, P229).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HomeTheaterConflict {
    /// It is a home theater group whose Apple TV is on and plays or can.
    AppleTvPlays,
    /// It is the Apple TV of a home theater group that is on and plays or can.
    GroupPlays,
}

pub fn home_theater_conflict(
    its_apple_tv_on: bool,
    its_group_on: bool,
) -> Option<HomeTheaterConflict> {
    if its_apple_tv_on {
        Some(HomeTheaterConflict::AppleTvPlays)
    } else if its_group_on {
        Some(HomeTheaterConflict::GroupPlays)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_desktop_app_stops_listing_an_address_at_64() {
        assert!(!address_full(DESKTOP, 63));
        assert!(address_full(DESKTOP, 64));
        assert!(!address_full(MAC, 10_000));
    }

    #[test]
    fn a_single_receiver_that_cant_play_has_no_row_and_a_target_keeps_its() {
        // covers: S222, P176
        assert!(has_row(false, false));
        assert!(!has_row(false, true));
        assert!(has_row(true, true));
    }

    #[test]
    fn a_member_is_held_by_its_target_unless_it_was_left_on_by_itself() {
        // covers: P176
        assert!(held_by_target(true, false, false, true));
        assert!(held_by_target(false, true, true, false));
        assert!(!held_by_target(false, true, true, true));
        assert!(!held_by_target(false, false, true, false));
        assert!(!held_by_target(false, true, false, false));
    }

    #[test]
    fn a_target_asks_a_password_first_then_plays_when_a_member_can() {
        // covers: P176, P181
        assert_eq!(target_reads(true, true), TargetReads::AsksPassword);
        assert_eq!(target_reads(false, true), TargetReads::CanPlay);
        assert_eq!(target_reads(false, false), TargetReads::FirstMember);
    }

    #[test]
    fn an_apple_tv_and_its_home_theater_group_never_play_at_once() {
        // covers: P177, P229
        assert_eq!(
            home_theater_conflict(true, false),
            Some(HomeTheaterConflict::AppleTvPlays)
        );
        assert_eq!(
            home_theater_conflict(false, true),
            Some(HomeTheaterConflict::GroupPlays)
        );
        assert_eq!(home_theater_conflict(false, false), None);
    }
}
