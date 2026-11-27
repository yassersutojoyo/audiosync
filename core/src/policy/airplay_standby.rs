//! A Denon or Marantz receiver asleep off the network with its Network
//! Control at `Off In Standby`, and the hint that names the setting (P262 to
//! P264 of docs/AIRPLAY_SPEC.md). Recognizing the receiver is the sender's
//! (P261); storing the two marks and showing the hint are the host's.
//!
//! Plain data in and out, the same rule for every host.

use audiosync_airplay::discovery::DenonBrand;
use audiosync_airplay::error::{ErrorKind, SessionError, UnreachableCause};
use audiosync_airplay::sender::SpeakerState;

use super::Seconds;

/// How long every outcome must have been unannounced or unanswered (P262).
pub const ASLEEP_AFTER: Seconds = 30.0;

/// A speaker's outcome as P262 sorts it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    Played,
    /// Unannounced (P215), or no record of it in view.
    Unannounced,
    /// Unreachable from a connect that got no answer, or found the host or
    /// the network unreachable.
    NoAnswer,
    /// Connecting, or the wait for a retry: neither counts nor breaks.
    Between,
    /// Anything else, which breaks the run: a refused connect, a Timeout or
    /// a later error, Busy, Refused and the rest.
    Breaks,
}

impl Outcome {
    pub fn of_state(state: SpeakerState) -> Outcome {
        match state {
            SpeakerState::Playing => Outcome::Played,
            SpeakerState::Unannounced => Outcome::Unannounced,
            SpeakerState::Connecting | SpeakerState::Retrying { .. } | SpeakerState::Stopped => {
                Outcome::Between
            }
            SpeakerState::Failed => Outcome::Breaks,
        }
    }

    pub fn of_error(error: &SessionError) -> Outcome {
        if error.kind == ErrorKind::Unreachable
            && error.unreachable == Some(UnreachableCause::NoAnswer)
        {
            Outcome::NoAnswer
        } else {
            Outcome::Breaks
        }
    }
}

/// Why the hint showed, for the host's debug log line (P263).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Recognized {
    Manufacturer,
    AddressBlock,
    StoredMark,
}

/// The counted outcome that made it asleep (P263).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cause {
    Unannounced,
    NoAnswer,
}

/// The hint just showed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HintShown {
    pub recognized: Recognized,
    pub cause: Cause,
}

/// What the host knows of the speaker's records now.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Record {
    /// No record of it in view.
    NotInView,
    /// In view, with P261's answer for its records.
    Seen(Option<DenonBrand>),
}

/// The two marks the host stores with the speaker's saved settings, keyed by
/// its device identifier (P263).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StandbyMarks {
    /// It was recognized under P261.
    pub recognized: bool,
    /// The hint showed since it last played.
    pub hint_shown: bool,
}

/// One speaker's run of counted outcomes and its hint.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Standby {
    pub(crate) run_since: Option<Seconds>,
    pub(crate) cause: Option<Cause>,
    pub(crate) hint: bool,
}

impl Standby {
    /// Whether the hint shows on its row now.
    pub fn hint(&self) -> bool {
        self.hint
    }

    /// Takes an outcome the sender reported at `now`.
    pub fn outcome(&mut self, outcome: Outcome, now: Seconds, marks: &mut StandbyMarks) {
        match outcome {
            Outcome::Played => {
                self.run_since = None;
                self.hint = false;
                marks.hint_shown = false;
            }
            Outcome::Unannounced => self.count(Cause::Unannounced, now),
            Outcome::NoAnswer => self.count(Cause::NoAnswer, now),
            Outcome::Between => {}
            Outcome::Breaks => self.run_since = None,
        }
    }

    fn count(&mut self, cause: Cause, now: Seconds) {
        self.run_since.get_or_insert(now);
        self.cause = Some(cause);
    }

    /// Brings the speaker up to date at `now`; returns the hint when it
    /// shows now, for the host's one log line.
    pub fn update(
        &mut self,
        record: Record,
        wanted: bool,
        now: Seconds,
        marks: &mut StandbyMarks,
    ) -> Option<HintShown> {
        let recognized = match record {
            Record::Seen(brand) => {
                marks.recognized = brand.is_some();
                brand.map(|brand| match brand {
                    DenonBrand::Manufacturer => Recognized::Manufacturer,
                    DenonBrand::AddressBlock => Recognized::AddressBlock,
                })
            }
            Record::NotInView => marks.recognized.then_some(Recognized::StoredMark),
        };
        if !wanted {
            self.run_since = None;
            self.hint = false;
            return None;
        }
        if record == Record::NotInView {
            self.count(Cause::Unannounced, now);
        }
        let Some(recognized) = recognized else {
            self.hint = false;
            return None;
        };
        let asleep = self
            .run_since
            .is_some_and(|since| now - since >= ASLEEP_AFTER);
        if !asleep || self.hint || marks.hint_shown {
            return None;
        }
        self.hint = true;
        marks.hint_shown = true;
        Some(HintShown {
            recognized,
            cause: self.cause.unwrap_or(Cause::Unannounced),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use audiosync_airplay::error::UnreachableCause;
    use std::time::Instant;

    const DENON: Record = Record::Seen(Some(DenonBrand::Manufacturer));

    fn unreachable(cause: Option<UnreachableCause>) -> SessionError {
        SessionError {
            unreachable: cause,
            ..SessionError::new(ErrorKind::Unreachable)
        }
    }

    /// A speaker that played at 0 s and is wanted, with its marks.
    fn playing() -> (Standby, StandbyMarks) {
        let mut standby = Standby::default();
        let mut marks = StandbyMarks::default();
        standby.update(DENON, true, 0.0, &mut marks);
        standby.outcome(Outcome::Played, 0.0, &mut marks);
        (standby, marks)
    }

    // covers: P262
    #[test]
    fn the_outcomes_sort_as_p262_says() {
        assert_eq!(Outcome::of_state(SpeakerState::Playing), Outcome::Played);
        assert_eq!(
            Outcome::of_state(SpeakerState::Unannounced),
            Outcome::Unannounced
        );
        assert_eq!(
            Outcome::of_state(SpeakerState::Connecting),
            Outcome::Between
        );
        assert_eq!(
            Outcome::of_state(SpeakerState::Retrying { at: Instant::now() }),
            Outcome::Between
        );
        assert_eq!(Outcome::of_state(SpeakerState::Failed), Outcome::Breaks);
        assert_eq!(
            Outcome::of_error(&unreachable(Some(UnreachableCause::NoAnswer))),
            Outcome::NoAnswer
        );
        assert_eq!(
            Outcome::of_error(&unreachable(Some(UnreachableCause::Refused))),
            Outcome::Breaks
        );
        assert_eq!(Outcome::of_error(&unreachable(None)), Outcome::Breaks);
        for kind in [
            ErrorKind::Timeout,
            ErrorKind::Lost,
            ErrorKind::Busy,
            ErrorKind::Refused,
            ErrorKind::PasswordRequired,
            ErrorKind::PairingRequired,
        ] {
            assert_eq!(
                Outcome::of_error(&SessionError::new(kind)),
                Outcome::Breaks,
                "{kind:?}"
            );
        }
    }

    // covers: P262, P263
    #[test]
    fn thirty_seconds_of_counted_outcomes_show_the_hint_once_on_both_sides_of_the_edge() {
        for counted in [Outcome::Unannounced, Outcome::NoAnswer] {
            let (mut standby, mut marks) = playing();
            standby.outcome(counted, 10.0, &mut marks);
            standby.outcome(Outcome::Between, 15.0, &mut marks);
            standby.outcome(counted, 20.0, &mut marks);
            assert_eq!(standby.update(DENON, true, 39.9, &mut marks), None);
            assert!(!standby.hint());
            let shown = standby.update(DENON, true, 40.0, &mut marks);
            let cause = if counted == Outcome::NoAnswer {
                Cause::NoAnswer
            } else {
                Cause::Unannounced
            };
            assert_eq!(
                shown,
                Some(HintShown {
                    recognized: Recognized::Manufacturer,
                    cause
                })
            );
            assert!(standby.hint() && marks.hint_shown && marks.recognized);
            assert_eq!(standby.update(DENON, true, 41.0, &mut marks), None);
            assert!(standby.hint(), "it stays");
        }
    }

    // covers: P262
    #[test]
    fn an_outcome_that_breaks_the_run_starts_it_again_at_the_next_counted_one() {
        let (mut standby, mut marks) = playing();
        standby.outcome(Outcome::NoAnswer, 10.0, &mut marks);
        standby.outcome(Outcome::Breaks, 25.0, &mut marks);
        assert_eq!(standby.update(DENON, true, 45.0, &mut marks), None);
        standby.outcome(Outcome::NoAnswer, 30.0, &mut marks);
        assert_eq!(standby.update(DENON, true, 59.9, &mut marks), None);
        assert!(standby.update(DENON, true, 60.0, &mut marks).is_some());

        let (mut standby, mut marks) = playing();
        standby.outcome(Outcome::Unannounced, 10.0, &mut marks);
        standby.outcome(Outcome::Played, 20.0, &mut marks);
        assert_eq!(standby.update(DENON, true, 50.0, &mut marks), None);
    }

    // covers: P262
    #[test]
    fn only_a_recognized_receiver_the_host_wants_is_asleep() {
        let other = Record::Seen(None);
        let mut standby = Standby::default();
        let mut marks = StandbyMarks::default();
        standby.outcome(Outcome::NoAnswer, 0.0, &mut marks);
        assert_eq!(standby.update(other, true, 40.0, &mut marks), None);
        assert!(!marks.recognized);

        let (mut standby, mut marks) = playing();
        standby.outcome(Outcome::NoAnswer, 0.0, &mut marks);
        assert_eq!(standby.update(DENON, false, 20.0, &mut marks), None);
        standby.update(DENON, true, 25.0, &mut marks);
        assert_eq!(standby.update(DENON, true, 40.0, &mut marks), None);
        standby.outcome(Outcome::NoAnswer, 30.0, &mut marks);
        assert!(standby.update(DENON, true, 60.0, &mut marks).is_some());
    }

    // covers: P262, P263
    #[test]
    fn the_stored_mark_recognizes_a_speaker_not_in_view_whose_absence_counts() {
        let mut marks = StandbyMarks {
            recognized: true,
            hint_shown: false,
        };
        let mut standby = Standby::default();
        assert_eq!(
            standby.update(Record::NotInView, true, 5.0, &mut marks),
            None
        );
        assert_eq!(
            standby.update(Record::NotInView, true, 35.0, &mut marks),
            Some(HintShown {
                recognized: Recognized::StoredMark,
                cause: Cause::Unannounced
            })
        );

        let mut marks = StandbyMarks::default();
        let mut standby = Standby::default();
        standby.update(Record::NotInView, true, 5.0, &mut marks);
        assert_eq!(
            standby.update(Record::NotInView, true, 35.0, &mut marks),
            None
        );
    }

    // covers: P263, P264
    #[test]
    fn the_hint_clears_on_play_off_or_a_record_that_is_no_receiver_and_shows_once() {
        let asleep = |standby: &mut Standby, marks: &mut StandbyMarks, from: Seconds| {
            standby.outcome(Outcome::NoAnswer, from, marks);
            standby.update(DENON, true, from + ASLEEP_AFTER, marks)
        };
        let (mut standby, mut marks) = playing();
        assert!(asleep(&mut standby, &mut marks, 10.0).is_some());

        // Off: it clears, the mark stays, so on again shows no hint.
        standby.update(DENON, false, 50.0, &mut marks);
        assert!(!standby.hint() && marks.hint_shown);
        standby.update(DENON, true, 51.0, &mut marks);
        assert_eq!(asleep(&mut standby, &mut marks, 52.0), None);
        assert!(!standby.hint());

        // A relaunch with the stored marks shows none either.
        let mut relaunched = Standby::default();
        relaunched.update(Record::NotInView, true, 0.0, &mut marks);
        assert_eq!(
            relaunched.update(Record::NotInView, true, 40.0, &mut marks),
            None
        );

        // Playing clears both, so a later sleep shows it again.
        standby.outcome(Outcome::Played, 100.0, &mut marks);
        assert!(!standby.hint() && !marks.hint_shown);
        assert!(asleep(&mut standby, &mut marks, 110.0).is_some());

        // A record that is no receiver clears it and the recognized mark.
        assert_eq!(
            standby.update(Record::Seen(None), true, 150.0, &mut marks),
            None
        );
        assert!(!standby.hint() && !marks.recognized && marks.hint_shown);
    }
}
