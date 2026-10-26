//! When the AirPlay speakers take an explicit pause (P213): a play-pause the
//! user sent to the source app, from a speaker's remote or the app's own
//! button, with the app known to play, or the app itself seen going from
//! playing to paused. The direction of a play-pause comes from the
//! now-playing reading, since it reaches the app as a toggle; with no
//! reading the speakers keep playing. Nothing here looks at the audio:
//! silence never pauses.

use std::time::{Duration, Instant};

/// How long after a pause a reading may still predate the app's own pause;
/// later readings that don't show the app paused resume. Longer than the
/// receivers' buffer, so the flush it bought is never undone audibly.
pub const STALE_READING: Duration = Duration::from_secs(3);

/// A sample louder than this, -60 dBFS, is sound: digital silence and
/// dither stay below it.
pub const SOUND_LEVEL: f32 = 0.001;

/// How much sound the capture carries, in total, before a paused stream
/// resumes on its own.
pub const SOUND_TO_RESUME: Duration = Duration::from_millis(200);

/// The stereo frames of interleaved `block` with a sample louder than
/// [`SOUND_LEVEL`]. Real-time safe.
pub fn sound_frames(block: &[f32]) -> u64 {
    block
        .as_chunks::<2>()
        .0
        .iter()
        .filter(|[left, right]| left.abs() > SOUND_LEVEL || right.abs() > SOUND_LEVEL)
        .count() as u64
}

/// What the sender gets.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    Nothing,
    Pause,
    Resume,
}

/// The app the pause was taken for, and when.
#[derive(Clone, Debug, PartialEq)]
struct Paused {
    app: Option<String>,
    at: Instant,
    /// The capture's sound total once [`STALE_READING`] had passed.
    heard_from: Option<f64>,
}

#[derive(Debug, Default)]
pub struct ExplicitPause {
    paused: Option<Paused>,
    /// The previous reading, so only a change the readings show counts.
    last: Option<(String, bool)>,
}

impl ExplicitPause {
    #[cfg(test)]
    pub fn is_paused(&self) -> bool {
        self.paused.is_some()
    }

    /// A play-pause goes to the app the now-playing control follows, which
    /// the latest reading shows as `playback` (its app and whether it
    /// plays; `None` without a reading). A paused stream resumes whatever
    /// the reading says, so a toggle never leaves the speakers silent.
    pub fn toggled(&mut self, playback: Option<(&str, bool)>, now: Instant) -> Step {
        if self.paused.take().is_some() {
            return Step::Resume;
        }
        match playback {
            Some((app, true)) => {
                self.paused = Some(Paused {
                    app: Some(app.to_owned()),
                    at: now,
                    heard_from: None,
                });
                Step::Pause
            }
            _ => Step::Nothing,
        }
    }

    /// A now-playing reading taken at `read_at`. The app seen going from
    /// playing to paused pauses a stream the speakers play (`streaming`);
    /// seen going from paused to playing, it resumes one at once. A paused
    /// stream also resumes once a reading taken [`STALE_READING`] after the
    /// pause shows anything but the same app paused: the app plays, another
    /// one counts, or none.
    pub fn read(
        &mut self,
        playback: Option<(&str, bool)>,
        read_at: Instant,
        streaming: bool,
    ) -> Step {
        let previous = self.last.take();
        self.last = playback.map(|(app, playing)| (app.to_owned(), playing));
        let turned = match (previous, playback) {
            (Some((was_app, was_playing)), Some((app, playing)))
                if was_app == app && was_playing != playing =>
            {
                Some(playing)
            }
            _ => None,
        };
        let Some(paused) = &self.paused else {
            if turned == Some(false) && streaming {
                self.paused = Some(Paused {
                    app: playback.map(|(app, _)| app.to_owned()),
                    at: read_at,
                    heard_from: None,
                });
                return Step::Pause;
            }
            return Step::Nothing;
        };
        if turned == Some(true) {
            self.paused = None;
            return Step::Resume;
        }
        if read_at.saturating_duration_since(paused.at) < STALE_READING {
            return Step::Nothing;
        }
        match playback {
            Some((app, false)) if paused.app.as_deref() == Some(app) => Step::Nothing,
            _ => {
                self.paused = None;
                Step::Resume
            }
        }
    }

    /// The source changed or the stream stopped: a paused stream resumes,
    /// so the next audio plays.
    pub fn clear(&mut self) -> Step {
        if self.paused.take().is_some() {
            Step::Resume
        } else {
            Step::Nothing
        }
    }

    /// The capture carried `sound` seconds of sound so far, a running total.
    /// A paused stream resumes once [`SOUND_TO_RESUME`] of it came after
    /// [`STALE_READING`] past the pause: another app plays, whatever the
    /// reading says. Sound before that may be the paused app's last audio.
    pub fn heard(&mut self, sound: f64, now: Instant) -> Step {
        let Some(paused) = self.paused.as_mut() else {
            return Step::Nothing;
        };
        if now.saturating_duration_since(paused.at) < STALE_READING {
            return Step::Nothing;
        }
        let from = *paused.heard_from.get_or_insert(sound);
        if sound - from >= SOUND_TO_RESUME.as_secs_f64() {
            self.paused = None;
            Step::Resume
        } else {
            Step::Nothing
        }
    }

    /// A new sender, which starts unpaused.
    pub fn forget(&mut self) {
        self.paused = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SPOTIFY: &str = "Spotify.exe";
    const CHROME: &str = "chrome.exe";

    fn paused_at(t0: Instant) -> ExplicitPause {
        let mut pause = ExplicitPause::default();
        assert_eq!(pause.toggled(Some((SPOTIFY, true)), t0), Step::Pause);
        pause
    }

    // covers: P213
    #[test]
    fn a_toggle_while_the_app_plays_pauses() {
        let t0 = Instant::now();
        let pause = paused_at(t0);
        assert!(pause.is_paused());
    }

    // covers: P213
    #[test]
    fn a_toggle_while_paused_resumes_whatever_the_reading_says() {
        let t0 = Instant::now();
        for reading in [Some((SPOTIFY, false)), Some((SPOTIFY, true)), None] {
            let mut pause = paused_at(t0);
            assert_eq!(pause.toggled(reading, t0), Step::Resume, "{reading:?}");
            assert!(!pause.is_paused());
        }
    }

    // covers: P213
    #[test]
    fn a_toggle_with_the_app_paused_or_unknown_does_nothing() {
        let t0 = Instant::now();
        let mut pause = ExplicitPause::default();
        assert_eq!(pause.toggled(Some((SPOTIFY, false)), t0), Step::Nothing);
        assert_eq!(pause.toggled(None, t0), Step::Nothing);
        assert!(!pause.is_paused());
    }

    // covers: P213
    #[test]
    fn the_app_going_from_playing_to_paused_pauses_once() {
        let t0 = Instant::now();
        let mut pause = ExplicitPause::default();
        assert_eq!(pause.read(Some((SPOTIFY, true)), t0, true), Step::Nothing);
        assert_eq!(pause.read(Some((SPOTIFY, false)), t0, true), Step::Pause);
        assert!(pause.is_paused());
        let later = t0 + STALE_READING * 2;
        assert_eq!(
            pause.read(Some((SPOTIFY, false)), later, true),
            Step::Nothing
        );
        assert!(pause.is_paused());
    }

    // covers: P213
    #[test]
    fn the_app_going_from_paused_to_playing_resumes_at_once() {
        let t0 = Instant::now();
        let mut pause = ExplicitPause::default();
        pause.read(Some((SPOTIFY, true)), t0, true);
        assert_eq!(pause.read(Some((SPOTIFY, false)), t0, true), Step::Pause);
        let soon = t0 + Duration::from_millis(500);
        assert_eq!(pause.read(Some((SPOTIFY, true)), soon, true), Step::Resume);
        assert!(!pause.is_paused());
    }

    // covers: P213
    #[test]
    fn a_first_reading_or_one_after_an_unknown_state_is_no_transition() {
        let t0 = Instant::now();
        let mut pause = ExplicitPause::default();
        assert_eq!(pause.read(Some((SPOTIFY, false)), t0, true), Step::Nothing);
        pause.read(Some((SPOTIFY, true)), t0, true);
        assert_eq!(pause.read(None, t0, true), Step::Nothing, "the app quit");
        assert_eq!(pause.read(Some((SPOTIFY, false)), t0, true), Step::Nothing);
        assert!(!pause.is_paused());
    }

    // covers: P213
    #[test]
    fn a_player_switch_follows_the_active_player() {
        let t0 = Instant::now();
        let mut pause = ExplicitPause::default();
        pause.read(Some((SPOTIFY, true)), t0, true);
        assert_eq!(
            pause.read(Some((CHROME, false)), t0, true),
            Step::Nothing,
            "another app counts, already paused"
        );
        pause.read(Some((CHROME, true)), t0, true);
        assert_eq!(pause.read(Some((CHROME, false)), t0, true), Step::Pause);
    }

    // covers: P213
    #[test]
    fn a_transition_while_nothing_streams_never_pauses() {
        let t0 = Instant::now();
        let mut pause = ExplicitPause::default();
        pause.read(Some((SPOTIFY, true)), t0, false);
        assert_eq!(pause.read(Some((SPOTIFY, false)), t0, false), Step::Nothing);
        assert!(!pause.is_paused());
    }

    // covers: P213
    #[test]
    fn sound_still_resumes_a_pause_the_app_took() {
        let t0 = Instant::now();
        let mut pause = ExplicitPause::default();
        pause.read(Some((SPOTIFY, true)), t0, true);
        assert_eq!(pause.read(Some((SPOTIFY, false)), t0, true), Step::Pause);
        let later = t0 + STALE_READING;
        assert_eq!(pause.heard(1.0, later), Step::Nothing);
        assert_eq!(pause.heard(1.25, later), Step::Resume);
    }

    // covers: P213
    #[test]
    fn the_app_shown_paused_keeps_the_pause() {
        let t0 = Instant::now();
        let mut pause = paused_at(t0);
        assert_eq!(
            pause.read(Some((SPOTIFY, false)), t0 + STALE_READING * 4, true),
            Step::Nothing
        );
        assert!(pause.is_paused());
    }

    // covers: P213
    #[test]
    fn a_stale_reading_right_after_the_pause_keeps_it() {
        let t0 = Instant::now();
        let mut pause = paused_at(t0);
        let soon = t0 + STALE_READING - Duration::from_millis(1);
        assert_eq!(pause.read(Some((SPOTIFY, true)), soon, true), Step::Nothing);
        assert_eq!(pause.read(None, soon, true), Step::Nothing);
        assert!(pause.is_paused());
    }

    // covers: P213
    #[test]
    fn the_app_playing_again_resumes() {
        let t0 = Instant::now();
        let mut pause = paused_at(t0);
        assert_eq!(
            pause.read(Some((SPOTIFY, true)), t0 + STALE_READING, true),
            Step::Resume
        );
        assert!(!pause.is_paused());
    }

    // covers: P213
    #[test]
    fn another_app_or_no_reading_resumes() {
        let t0 = Instant::now();
        let later = t0 + STALE_READING;
        let mut pause = paused_at(t0);
        assert_eq!(
            pause.read(Some(("chrome.exe", false)), later, true),
            Step::Resume
        );
        let mut pause = paused_at(t0);
        assert_eq!(pause.read(None, later, true), Step::Resume);
    }

    // covers: P213
    #[test]
    fn only_samples_above_minus_60_dbfs_are_sound() {
        assert_eq!(sound_frames(&[0.0; 8]), 0, "digital silence");
        assert_eq!(sound_frames(&[3e-5, -3e-5, 2e-5, -1e-5]), 0, "dither");
        assert_eq!(sound_frames(&[0.002, 0.0, 0.0, -0.002, 0.0, 0.0]), 2);
        assert_eq!(sound_frames(&[0.5, 0.5, -0.5, -0.5]), 2);
    }

    // covers: P213
    #[test]
    fn sound_resumes_once_200_ms_of_it_came_after_the_stale_reading_time() {
        let t0 = Instant::now();
        let mut pause = paused_at(t0);
        assert_eq!(pause.heard(1.0, t0 + Duration::from_secs(1)), Step::Nothing);
        let later = t0 + STALE_READING;
        assert_eq!(
            pause.heard(1.0, later),
            Step::Nothing,
            "the app's last audio"
        );
        assert_eq!(pause.heard(1.15, later), Step::Nothing);
        assert_eq!(pause.heard(1.25, later), Step::Resume);
        assert!(!pause.is_paused());
    }

    // covers: P213
    #[test]
    fn sound_never_pauses() {
        let t0 = Instant::now();
        let mut pause = ExplicitPause::default();
        assert_eq!(pause.heard(0.0, t0), Step::Nothing);
        assert_eq!(pause.heard(60.0, t0 + STALE_READING * 4), Step::Nothing);
        assert!(!pause.is_paused());
    }

    // covers: P213
    #[test]
    fn clearing_resumes_only_a_paused_stream() {
        let t0 = Instant::now();
        let mut pause = paused_at(t0);
        assert_eq!(pause.clear(), Step::Resume);
        assert_eq!(pause.clear(), Step::Nothing);
        let mut pause = paused_at(t0);
        pause.forget();
        assert!(!pause.is_paused());
    }
}
