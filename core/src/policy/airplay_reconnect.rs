//! The host's side of an AirPlay speaker the sender retries: when the
//! reconnecting notice shows and clears (S222, S412), a speaker the sender
//! makes wait (P169, S285), a sender that couldn't start (S101), the PTP
//! ports notice (S358) and speakers across a sleep (S412). The sender's own
//! backoff is the airplay crate's (P142).
//!
//! Plain data in and out; the Mac host keeps its own copy in Swift for now.

use super::Seconds;

/// The retried error that brings up the reconnecting notice: the third,
/// about 3 s into the sender's backoff, so a blip its first retry heals
/// never shows (S222).
pub const NOTICE_AT_RETRIED_ERROR: u32 = 3;

/// Seconds between tries to start a sender that couldn't start (S101).
pub const SENDER_RETRY_SECONDS: Seconds = 10.0;

/// A speaker session's state as the sender reports it, as far as the
/// retry count cares.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionState {
    Playing,
    /// It left the network; the sender keeps it (P215).
    Unannounced,
    Other,
}

/// An error the sender reports for a speaker.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SpeakerError {
    /// The sender tries it again by itself, at a time it gave.
    pub retried: bool,
    /// The sender keeps it on and starts it once it lets it: too many
    /// AirPlay 2 speakers, or PTP in use on another network (P169, S285).
    pub waits: bool,
}

/// What the host does about a speaker's error.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ErrorReaction {
    /// It stays on while the sender retries; `notice` says whether the
    /// reconnecting notice shows now.
    Retry { notice: bool },
    /// It stays on, counts as on and shows why it waits.
    Wait,
    /// It is turned off with the error's notice.
    TurnOff,
}

/// A speaker's retried errors since it last played or went unannounced.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Retries {
    count: u32,
}

impl Retries {
    pub fn on_state(&mut self, state: SessionState) {
        if matches!(state, SessionState::Playing | SessionState::Unannounced) {
            self.count = 0;
        }
    }

    /// One reported while the host sleeps counts nothing (S412), and the
    /// notice shows once per loss, so a dismissed one stays dismissed until
    /// it plays again.
    pub fn on_error(&mut self, error: SpeakerError, asleep: bool) -> ErrorReaction {
        if error.retried {
            if asleep {
                return ErrorReaction::Retry { notice: false };
            }
            self.count = self.count.saturating_add(1);
            ErrorReaction::Retry {
                notice: self.count == NOTICE_AT_RETRIED_ERROR,
            }
        } else if error.waits {
            ErrorReaction::Wait
        } else {
            ErrorReaction::TurnOff
        }
    }
}

/// What ends a speaker's reconnecting or waiting notice (S222).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NoticeEnd {
    /// It plays: every notice on its row clears.
    Plays,
    /// The sender started it again after a wait.
    WaitOver,
    /// It left the network, which needs nothing from the user (P215).
    LeftNetwork,
    /// It left the sender, or its row went: nothing retries it.
    LeftSender,
}

/// Which of a speaker's notices clear.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NoticeClear {
    All,
    Reconnecting,
}

pub fn notice_clear(end: NoticeEnd) -> NoticeClear {
    match end {
        NoticeEnd::Plays => NoticeClear::All,
        NoticeEnd::WaitOver | NoticeEnd::LeftNetwork | NoticeEnd::LeftSender => {
            NoticeClear::Reconnecting
        }
    }
}

/// How a sender start was asked for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SenderStart {
    /// The first start, at launch.
    First,
    /// The user's Retry.
    Retry,
    /// The 10 s restart after one that couldn't start.
    Due,
}

/// The restart of a sender that couldn't start, [`SENDER_RETRY_SECONDS`]
/// later counted in the host's polls (S101).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SenderRestart {
    polls_left: Option<u32>,
}

impl SenderRestart {
    /// The start failed: it is tried again `polls` polls later. Returns
    /// whether the user is told; a restart that fails again isn't, so the
    /// notice never shows twice in a row.
    pub fn failed(&mut self, start: SenderStart, polls: u32) -> bool {
        self.polls_left = Some(polls);
        start != SenderStart::Due
    }

    /// A sender runs, or the host shuts down: nothing is due.
    pub fn cancel(&mut self) {
        self.polls_left = None;
    }

    /// Counts one poll; true when the restart is due.
    pub fn poll(&mut self) -> bool {
        let Some(polls) = self.polls_left.as_mut() else {
            return false;
        };
        *polls = polls.saturating_sub(1);
        *polls == 0
    }

    pub fn pending(&self) -> bool {
        self.polls_left.is_some()
    }
}

/// Whether the PTP ports notice goes: a speaker played since the clash
/// report and no row is off for PTP timing (S358).
pub fn ptp_notice_clears(played_since_report: bool, rows_off_for_timing: usize) -> bool {
    played_since_report && rows_off_for_timing == 0
}

/// Whether a speaker refused for PTP timing raises the ports notice: within
/// an episode, from a report to a bind, each speaker raises it once (S358).
pub fn ptp_notice_raised(in_episode: bool, first_refusal_in_episode: bool) -> bool {
    in_episode && first_refusal_in_episode
}

/// Whether speakers join the stream: asleep, they only leave, and the
/// wake's reconcile starts the ones held back (S412).
pub fn speakers_join(streaming: bool, asleep: bool) -> bool {
    streaming && !asleep
}

/// Whether a receiver's changed records wait for the wake: asleep, nothing
/// restarts; awake, a speaker the sender retries takes them at once (P142,
/// S412).
pub fn records_wait_for_wake(asleep: bool) -> bool {
    asleep
}

#[cfg(test)]
mod tests {
    use super::*;

    const RETRIED: SpeakerError = SpeakerError {
        retried: true,
        waits: false,
    };

    fn notices(retries: &mut Retries, errors: usize, asleep: bool) -> Vec<bool> {
        (0..errors)
            .map(|_| match retries.on_error(RETRIED, asleep) {
                ErrorReaction::Retry { notice } => notice,
                other => panic!("{other:?}"),
            })
            .collect()
    }

    #[test]
    fn the_reconnecting_notice_shows_once_at_the_third_retried_error() {
        // covers: S222
        let mut retries = Retries::default();
        assert_eq!(
            notices(&mut retries, 5, false),
            [false, false, true, false, false]
        );
    }

    #[test]
    fn playing_or_going_unannounced_starts_the_count_again() {
        // covers: S222
        for state in [SessionState::Playing, SessionState::Unannounced] {
            let mut retries = Retries::default();
            notices(&mut retries, 2, false);
            retries.on_state(state);
            assert_eq!(notices(&mut retries, 3, false), [false, false, true]);
        }
        let mut retries = Retries::default();
        notices(&mut retries, 2, false);
        retries.on_state(SessionState::Other);
        assert_eq!(notices(&mut retries, 1, false), [true]);
    }

    #[test]
    fn retried_errors_while_asleep_count_nothing() {
        // covers: S412
        let mut retries = Retries::default();
        notices(&mut retries, 1, false);
        assert_eq!(notices(&mut retries, 4, true), [false; 4]);
        assert_eq!(notices(&mut retries, 2, false), [false, true]);
    }

    #[test]
    fn a_speaker_the_sender_makes_wait_stays_on_with_its_reason() {
        // covers: S285, P169
        let mut retries = Retries::default();
        let waits = SpeakerError {
            retried: false,
            waits: true,
        };
        assert_eq!(retries.on_error(waits, false), ErrorReaction::Wait);
        assert_eq!(retries.on_error(waits, true), ErrorReaction::Wait);
        let off = SpeakerError {
            retried: false,
            waits: false,
        };
        assert_eq!(retries.on_error(off, false), ErrorReaction::TurnOff);
        assert_eq!(retries, Retries::default(), "neither counts");
    }

    #[test]
    fn playing_clears_every_notice_and_leaving_clears_the_reconnecting_one() {
        // covers: S222
        assert_eq!(notice_clear(NoticeEnd::Plays), NoticeClear::All);
        for end in [
            NoticeEnd::WaitOver,
            NoticeEnd::LeftNetwork,
            NoticeEnd::LeftSender,
        ] {
            assert_eq!(notice_clear(end), NoticeClear::Reconnecting);
        }
    }

    #[test]
    fn a_sender_that_couldnt_start_is_tried_every_10_s_and_told_once() {
        // covers: S101
        let polls = 20;
        let mut restart = SenderRestart::default();
        assert!(!restart.poll(), "nothing is due before a failure");
        assert!(restart.failed(SenderStart::First, polls));
        for _ in 1..polls {
            assert!(!restart.poll());
        }
        assert!(restart.poll());
        assert!(!restart.failed(SenderStart::Due, polls), "no second notice");
        assert!(restart.pending());
        assert!(restart.failed(SenderStart::Retry, polls), "Retry tells");
        restart.cancel();
        assert!(!restart.pending());
        assert!(!restart.poll());
    }

    #[test]
    fn the_ptp_notice_shows_until_a_speaker_plays_and_no_row_is_off_for_timing() {
        // covers: S358
        assert!(!ptp_notice_clears(false, 0));
        assert!(!ptp_notice_clears(true, 1));
        assert!(ptp_notice_clears(true, 0));
        assert!(ptp_notice_raised(true, true));
        assert!(!ptp_notice_raised(true, false), "once per speaker");
        assert!(!ptp_notice_raised(false, true), "no clash reported");
    }

    #[test]
    fn asleep_speakers_only_leave_and_new_records_wait_for_the_wake() {
        // covers: S412
        assert!(speakers_join(true, false));
        assert!(!speakers_join(true, true));
        assert!(!speakers_join(false, false));
        assert!(records_wait_for_wake(true));
        assert!(!records_wait_for_wake(false));
    }
}
