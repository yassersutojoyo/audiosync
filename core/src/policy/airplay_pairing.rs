//! The host's side of pairing an AirPlay speaker: who may pair or be asked
//! for a code (S222, S223), what a refusal asks for (P135, P154, S222), the
//! speakers that ask for a code every time (P61, S223), which group member
//! is asked for its password (P176) and the pairings file's notices (S224).
//! The pairing exchange itself is the airplay crate's.
//!
//! Plain data in and out; the Mac host keeps its own copy in Swift for now.
//! Where the hosts still differ, a [`HostRules`] field names the difference
//! and each host's constant holds what it does today.

/// What a PIN must look like before it goes to the Apple TV.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PinFormat {
    /// Exactly the four ASCII digits an Apple TV shows; anything else asks
    /// again, marked wrong.
    FourDigits,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PinCheck {
    Send,
    Refuse { wrong: bool },
}

pub fn check_pin(pin: &str, format: PinFormat) -> PinCheck {
    let digits = !pin.is_empty() && pin.bytes().all(|byte| byte.is_ascii_digit());
    match format {
        PinFormat::FourDigits if digits && pin.len() == 4 => PinCheck::Send,
        PinFormat::FourDigits => PinCheck::Refuse { wrong: true },
    }
}

/// A speaker whose PIN pairing is open when the sender stops by itself.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OpenPairingAtSenderStop {
    /// It goes off with the needs-a-PIN notice.
    TurnOff,
    /// It stays on, silent until Retry; one asking for a code every time
    /// still goes off, as its PIN session ended with the stream.
    StayOn,
}

/// Whether the speaker goes off with the needs-a-PIN notice.
pub fn off_at_sender_stop(rule: OpenPairingAtSenderStop, every_time: bool) -> bool {
    every_time || rule == OpenPairingAtSenderStop::TurnOff
}

/// When the user hears that the pairings file couldn't be read at launch
/// (S224). Both hosts keep a copy of the file aside before writing over it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UnreadablePairingsNotice {
    /// At launch: the paired speakers ask for a code again.
    AtLaunch,
    /// Once the copy is kept aside, naming it.
    OnceKeptAside,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PairingsMoment {
    Launch,
    KeptAside,
}

/// Whether the notice shows at `moment`; each moment comes once a run.
pub fn tell_unreadable_pairings(rule: UnreadablePairingsNotice, moment: PairingsMoment) -> bool {
    matches!(
        (rule, moment),
        (UnreadablePairingsNotice::AtLaunch, PairingsMoment::Launch)
            | (
                UnreadablePairingsNotice::OnceKeptAside,
                PairingsMoment::KeptAside
            )
    )
}

/// Which members of a stereo pair or home theater the user's switch-on
/// asks for their passwords (P176).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PasswordMembers {
    /// Every member that needs one, the prompts queued at once.
    AllUpFront,
}

/// What one host does where the hosts still differ.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HostRules {
    pub pin_format: PinFormat,
    pub open_pairing_at_sender_stop: OpenPairingAtSenderStop,
    pub unreadable_pairings_notice: UnreadablePairingsNotice,
    pub password_members: PasswordMembers,
    pub refused_reappears: RefusedReappears,
}

pub const MAC: HostRules = HostRules {
    pin_format: PinFormat::FourDigits,
    open_pairing_at_sender_stop: OpenPairingAtSenderStop::TurnOff,
    unreadable_pairings_notice: UnreadablePairingsNotice::AtLaunch,
    password_members: PasswordMembers::AllUpFront,
    refused_reappears: RefusedReappears::NoticeAgain,
};

/// The Windows and Linux app.
pub const DESKTOP: HostRules = HostRules {
    pin_format: PinFormat::FourDigits,
    open_pairing_at_sender_stop: OpenPairingAtSenderStop::TurnOff,
    unreadable_pairings_notice: UnreadablePairingsNotice::OnceKeptAside,
    password_members: PasswordMembers::AllUpFront,
    refused_reappears: RefusedReappears::Quiet,
};

/// What a speaker that is on but hasn't joined would ask the user for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Asks {
    Nothing,
    Pin,
    Password { typed: bool },
}

/// A speaker that is on but hasn't joined, while streaming.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Unjoined {
    pub asks: Asks,
    /// The user switched it on, or paired it, in this stream.
    pub users_own: bool,
    pub prompt_open: bool,
    /// A stereo pair or home theater it belongs to holds it.
    pub held_by_group: bool,
}

/// Whether it goes off with its notice: only the user's own switch-on
/// pairs or asks, so one restored or switched on by auto-connect that would
/// ask stays silent left on (S222, S223).
pub fn off_uninvited(speaker: Unjoined) -> bool {
    let asks = match speaker.asks {
        Asks::Nothing => false,
        Asks::Pin => true,
        Asks::Password { typed } => !typed,
    };
    asks && !speaker.users_own && !speaker.prompt_open && !speaker.held_by_group
}

/// A speaker refused its session for a pairing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PinRefusal {
    /// The user's own switch-on is under way.
    pub in_attempt: bool,
    /// It paired once already within that attempt.
    pub paired_in_attempt: bool,
    pub joined: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AfterPinRefusal {
    /// Its stored pairing turned down, it pairs again (P154).
    PairAgain,
    /// It refused the pairing made this attempt too: it still needs a code.
    StillNeedsCode,
    NeedsPin,
}

pub fn pin_refused(refusal: PinRefusal) -> AfterPinRefusal {
    if refusal.paired_in_attempt {
        AfterPinRefusal::StillNeedsCode
    } else if refusal.in_attempt && refusal.joined {
        AfterPinRefusal::PairAgain
    } else {
        AfterPinRefusal::NeedsPin
    }
}

/// Whether a speaker refused before any pairing step, so its notice names
/// no code (S222): it asked for a pairing over AirPlay 2 outside the user's
/// attempt while its listing asked for nothing and it holds no credentials.
pub fn refused_unasked(over_airplay2: bool, in_attempt: bool, listing_asks_nothing: bool) -> bool {
    over_airplay2 && !in_attempt && listing_asks_nothing
}

/// Whether a refused password is asked for again: within the user's
/// attempt, while the speaker is in the sender (P135).
pub fn password_asked_again(in_attempt: bool, joined: bool) -> bool {
    in_attempt && joined
}

/// Whether the password prompt shows as wrong: a password typed this run
/// was turned down, or the speaker said so.
pub fn password_marked_wrong(typed: bool, said_wrong: bool) -> bool {
    typed || said_wrong
}

/// Why a speaker went off, as far as switching it on again cares.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OffReason {
    NotAccepted,
    PairingNotYet,
    StillNeedsCode,
    NeedsPin,
    Other,
}

/// Whether a speaker that went off stays off for auto-connect and presets
/// until the user turns it on (S349): it refused, or it needs a PIN the
/// user's click didn't ask for.
pub fn stays_refused(reason: OffReason, on_by_hand: bool) -> bool {
    match reason {
        OffReason::NotAccepted | OffReason::PairingNotYet | OffReason::StillNeedsCode => true,
        OffReason::NeedsPin => !on_by_hand,
        OffReason::Other => false,
    }
}

/// A speaker that appeared in the list.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Appeared {
    pub auto_connect: bool,
    pub hidden: bool,
    pub refused: bool,
    pub every_time: bool,
}

/// Whether auto-connect switches it on: a refused one, or one asking for a
/// code every time, never (P61, S349).
pub fn auto_connects(speaker: Appeared) -> bool {
    speaker.auto_connect && !speaker.hidden && !speaker.refused && !speaker.every_time
}

/// What auto-connect does with a speaker it turned off for a code earlier
/// this run (S349) once it leaves the network and appears again.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RefusedReappears {
    /// Switched on and off again with why: the notice comes each time.
    NoticeAgain,
    /// It stays off with no notice for the rest of the run.
    Quiet,
}

/// Whether a refusal kept this run still holds a speaker that appears again,
/// so auto-connect passes it over in silence; `asks_code`: it went off
/// because it asks for a PIN or a password (S349).
pub fn refusal_holds(rule: RefusedReappears, asks_code: bool) -> bool {
    rule == RefusedReappears::Quiet || !asks_code
}

/// A listed speaker that asks for a code every time.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EveryTimeListed {
    pub on: bool,
    pub playing: bool,
    /// The user's click in this stream switched it on.
    pub on_by_hand: bool,
    /// It appeared in this listing.
    pub new: bool,
    pub auto_connect: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EveryTimeAction {
    Untouched,
    /// Auto-connect leaves it off.
    StaysOff,
    /// It goes off quietly, also when it was on from the last run (S223).
    TurnOff,
}

pub fn every_time_listed(speaker: EveryTimeListed) -> EveryTimeAction {
    if speaker.playing || speaker.on_by_hand {
        EveryTimeAction::Untouched
    } else if speaker.on {
        EveryTimeAction::TurnOff
    } else if speaker.new && speaker.auto_connect {
        EveryTimeAction::StaysOff
    } else {
        EveryTimeAction::Untouched
    }
}

/// Whether a speaker goes off with the needs-a-PIN notice as a stream ends:
/// the PIN session a click bought ends with it, unless its pairing or
/// prompt is under way (S223).
pub fn off_at_stream_end(on: bool, every_time: bool, prompt_open: bool) -> bool {
    on && every_time && !prompt_open
}

/// The members asked for their passwords when the user switches a stereo
/// pair or home theater on, by their places in `needs_password`.
pub fn members_to_ask(
    rule: PasswordMembers,
    needs_password: impl IntoIterator<Item = bool>,
) -> Vec<usize> {
    let needing = needs_password
        .into_iter()
        .enumerate()
        .filter_map(|(index, needs)| needs.then_some(index));
    match rule {
        PasswordMembers::AllUpFront => needing.collect(),
    }
}

/// Whether writes of the pairings file fail, so the user is told once when
/// they start failing and once when one works again (S224).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PairingsWrites {
    unsaved: bool,
}

impl PairingsWrites {
    /// Returns the change to tell: whether the file is saved now.
    pub fn written(&mut self, saved: bool) -> Option<bool> {
        (saved == self.unsaved).then(|| {
            self.unsaved = !saved;
            saved
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pin_of_four_digits_is_sent_and_anything_else_asks_again_marked_wrong() {
        assert_eq!(check_pin("4821", PinFormat::FourDigits), PinCheck::Send);
        for pin in ["", "482", "48210", "48a1", "４８２１"] {
            assert_eq!(
                check_pin(pin, PinFormat::FourDigits),
                PinCheck::Refuse { wrong: true },
                "{pin}"
            );
        }
    }

    #[test]
    fn an_open_pin_pairing_at_a_sender_stop_goes_off_or_stays_on_by_rule() {
        // covers: S223
        assert!(off_at_sender_stop(OpenPairingAtSenderStop::TurnOff, false));
        assert!(off_at_sender_stop(OpenPairingAtSenderStop::TurnOff, true));
        assert!(!off_at_sender_stop(OpenPairingAtSenderStop::StayOn, false));
        assert!(off_at_sender_stop(OpenPairingAtSenderStop::StayOn, true));
    }

    #[test]
    fn the_mac_turns_an_open_pin_pairing_off_at_a_sender_stop_as_desktop_does() {
        // covers: S223
        assert_eq!(
            MAC.open_pairing_at_sender_stop,
            DESKTOP.open_pairing_at_sender_stop
        );
    }

    #[test]
    fn an_unreadable_pairings_file_is_told_at_launch_or_once_kept_aside_by_rule() {
        // covers: S224
        use PairingsMoment::{KeptAside, Launch};
        use UnreadablePairingsNotice::{AtLaunch, OnceKeptAside};
        assert!(tell_unreadable_pairings(AtLaunch, Launch));
        assert!(!tell_unreadable_pairings(AtLaunch, KeptAside));
        assert!(!tell_unreadable_pairings(OnceKeptAside, Launch));
        assert!(tell_unreadable_pairings(OnceKeptAside, KeptAside));
    }

    #[test]
    fn each_host_keeps_what_it_does_today() {
        assert_eq!(DESKTOP.pin_format, PinFormat::FourDigits);
        assert_eq!(
            DESKTOP.open_pairing_at_sender_stop,
            OpenPairingAtSenderStop::TurnOff
        );
        assert_eq!(
            DESKTOP.unreadable_pairings_notice,
            UnreadablePairingsNotice::OnceKeptAside
        );
        assert_eq!(MAC.pin_format, PinFormat::FourDigits);
        assert_eq!(
            MAC.unreadable_pairings_notice,
            UnreadablePairingsNotice::AtLaunch
        );
        assert_eq!(DESKTOP.password_members, PasswordMembers::AllUpFront);
        assert_eq!(MAC.password_members, PasswordMembers::AllUpFront);
    }

    const ASKS_PIN: Unjoined = Unjoined {
        asks: Asks::Pin,
        users_own: false,
        prompt_open: false,
        held_by_group: false,
    };

    #[test]
    fn only_the_users_own_switch_on_pairs_or_asks() {
        // covers: S222, S223
        assert!(off_uninvited(ASKS_PIN));
        assert!(off_uninvited(Unjoined {
            asks: Asks::Password { typed: false },
            ..ASKS_PIN
        }));
        for kept_on in [
            Unjoined {
                users_own: true,
                ..ASKS_PIN
            },
            Unjoined {
                prompt_open: true,
                ..ASKS_PIN
            },
            Unjoined {
                held_by_group: true,
                ..ASKS_PIN
            },
            Unjoined {
                asks: Asks::Password { typed: true },
                ..ASKS_PIN
            },
            Unjoined {
                asks: Asks::Nothing,
                ..ASKS_PIN
            },
        ] {
            assert!(!off_uninvited(kept_on), "{kept_on:?}");
        }
    }

    #[test]
    fn a_turned_down_pairing_pairs_again_once_then_still_needs_a_code() {
        // covers: P154
        let refusal = PinRefusal {
            in_attempt: true,
            paired_in_attempt: false,
            joined: true,
        };
        assert_eq!(pin_refused(refusal), AfterPinRefusal::PairAgain);
        assert_eq!(
            pin_refused(PinRefusal {
                paired_in_attempt: true,
                ..refusal
            }),
            AfterPinRefusal::StillNeedsCode
        );
        for refusal in [
            PinRefusal {
                in_attempt: false,
                ..refusal
            },
            PinRefusal {
                joined: false,
                ..refusal
            },
        ] {
            assert_eq!(pin_refused(refusal), AfterPinRefusal::NeedsPin);
        }
    }

    #[test]
    fn a_refusal_before_any_pairing_step_names_no_code() {
        // covers: S222
        assert!(refused_unasked(true, false, true));
        assert!(!refused_unasked(false, false, true));
        assert!(!refused_unasked(true, true, true));
        assert!(!refused_unasked(true, false, false));
    }

    #[test]
    fn a_refused_password_is_asked_again_within_the_attempt_marked_wrong() {
        // covers: P135
        assert!(password_asked_again(true, true));
        assert!(!password_asked_again(false, true));
        assert!(!password_asked_again(true, false));
        assert!(password_marked_wrong(true, false));
        assert!(password_marked_wrong(false, true));
        assert!(!password_marked_wrong(false, false));
    }

    #[test]
    fn a_code_refusal_reappearing_posts_again_on_the_mac_and_stays_quiet_on_desktop() {
        // covers: S349
        assert!(!refusal_holds(MAC.refused_reappears, true));
        assert!(refusal_holds(MAC.refused_reappears, false));
        assert!(refusal_holds(DESKTOP.refused_reappears, true));
        assert!(refusal_holds(DESKTOP.refused_reappears, false));
    }

    #[test]
    fn a_refused_speaker_stays_off_for_auto_connect_until_the_user_turns_it_on() {
        // covers: S349
        for reason in [
            OffReason::NotAccepted,
            OffReason::PairingNotYet,
            OffReason::StillNeedsCode,
        ] {
            assert!(stays_refused(reason, true));
        }
        assert!(stays_refused(OffReason::NeedsPin, false));
        assert!(!stays_refused(OffReason::NeedsPin, true));
        assert!(!stays_refused(OffReason::Other, false));
        let appeared = Appeared {
            auto_connect: true,
            hidden: false,
            refused: false,
            every_time: false,
        };
        assert!(auto_connects(appeared));
        for skipped in [
            Appeared {
                refused: true,
                ..appeared
            },
            Appeared {
                every_time: true,
                ..appeared
            },
            Appeared {
                hidden: true,
                ..appeared
            },
            Appeared {
                auto_connect: false,
                ..appeared
            },
        ] {
            assert!(!auto_connects(skipped), "{skipped:?}");
        }
    }

    #[test]
    fn a_code_every_time_speaker_never_auto_connects_and_goes_off_quietly_when_listed() {
        // covers: P61, S223
        let listed = EveryTimeListed {
            on: true,
            playing: false,
            on_by_hand: false,
            new: true,
            auto_connect: true,
        };
        assert_eq!(every_time_listed(listed), EveryTimeAction::TurnOff);
        let off = EveryTimeListed {
            on: false,
            ..listed
        };
        assert_eq!(every_time_listed(off), EveryTimeAction::StaysOff);
        for untouched in [
            EveryTimeListed {
                playing: true,
                ..listed
            },
            EveryTimeListed {
                on_by_hand: true,
                ..listed
            },
            EveryTimeListed { new: false, ..off },
            EveryTimeListed {
                auto_connect: false,
                ..off
            },
        ] {
            assert_eq!(
                every_time_listed(untouched),
                EveryTimeAction::Untouched,
                "{untouched:?}"
            );
        }
    }

    #[test]
    fn a_code_every_time_speaker_goes_off_at_the_stream_end_unless_it_is_asking() {
        // covers: S223
        assert!(off_at_stream_end(true, true, false));
        assert!(!off_at_stream_end(true, true, true));
        assert!(!off_at_stream_end(true, false, false));
        assert!(!off_at_stream_end(false, true, false));
    }

    #[test]
    fn a_group_asks_every_member_needing_a_password() {
        // covers: P176
        use PasswordMembers::AllUpFront;
        assert_eq!(members_to_ask(AllUpFront, [false, true, true]), [1, 2]);
        assert!(members_to_ask(AllUpFront, [false, false]).is_empty());
    }

    #[test]
    fn failing_pairings_writes_are_told_once_and_the_next_good_write_clears_it() {
        // covers: S224
        let mut writes = PairingsWrites::default();
        assert_eq!(writes.written(true), None);
        assert_eq!(writes.written(false), Some(false));
        assert_eq!(writes.written(false), None);
        assert_eq!(writes.written(true), Some(true));
        assert_eq!(writes.written(true), None);
    }
}
