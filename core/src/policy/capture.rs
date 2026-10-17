//! Whether captured audio flows (S17 to S19).
//!
//! A process tap delivers nothing until its source plays, so "no frames yet"
//! is normal (S17). Only a source known to have been playing for a while
//! without any captured frame points at a real problem.

use super::Seconds;
use super::source::AudioSource;

/// How long a source must be known to play, without captured frames, before
/// capture counts as not working (S18).
pub const PLAYING_GRACE: Seconds = 6.0;

/// The first check runs this long after a session starts (S18).
pub const FIRST_CHECK_AFTER: Seconds = 3.0;

/// Later checks run this often until audio flows or the session ends (S18).
pub const CHECK_EVERY: Seconds = 2.0;

/// The capture verdict (S18).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CaptureVerdict {
    /// Frames reached the ring in this session.
    Flowing,
    /// Nothing yet, which is normal while the source is silent.
    Waiting,
    /// The source has been playing for the grace period, yet nothing arrived.
    NotWorking,
}

/// The verdict for this session's captured frame count (which restarts at 0
/// with every session, S17) and how long the source is known to have been
/// playing (`None`: paused or unknown, which never yields `NotWorking`) (S18).
pub fn capture_verdict(frames_captured: u64, playing_for: Option<Seconds>) -> CaptureVerdict {
    if frames_captured > 0 {
        CaptureVerdict::Flowing
    } else if playing_for.is_some_and(|playing| playing >= PLAYING_GRACE) {
        CaptureVerdict::NotWorking
    } else {
        CaptureVerdict::Waiting
    }
}

/// One live Now Playing reading of a player (S19).
#[derive(Clone, Debug, PartialEq)]
pub struct PlayerReading {
    /// The player's bundle ID.
    pub bundle_id: String,
    /// When it started playing, or `None` while it doesn't play.
    pub playing_since: Option<Seconds>,
}

/// How long `source` is known to have been playing at `now` (S19): for the
/// system source any playing player counts, for an app source only that app,
/// and an input source is never known to play.
pub fn playing_for(
    source: &AudioSource,
    readings: &[PlayerReading],
    now: Seconds,
) -> Option<Seconds> {
    let counts = |reading: &&PlayerReading| match source {
        AudioSource::System => true,
        AudioSource::App { bundle_id } => reading.bundle_id == *bundle_id,
        AudioSource::Input { .. } => false,
    };
    readings
        .iter()
        .filter(counts)
        .filter_map(|reading| reading.playing_since)
        .map(|since| (now - since).max(0.0))
        .reduce(f64::max)
}

/// The host's capture check schedule (S18): 3 s after a start, then every 2 s
/// until audio flows or the session ends, reporting a verdict only when it
/// changes.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CaptureCheck {
    next_at: Option<Seconds>,
    last: Option<CaptureVerdict>,
}

impl CaptureCheck {
    /// The check of a session that started at `now`.
    pub fn started(now: Seconds) -> CaptureCheck {
        CaptureCheck {
            next_at: Some(now + FIRST_CHECK_AFTER),
            last: None,
        }
    }

    /// When the next check is due, or `None` once audio flows or the session ended.
    pub fn next_check_at(&self) -> Option<Seconds> {
        self.next_at
    }

    /// Runs the check if it is due. Returns the verdict when it differs from the
    /// last one reported (the host posts or clears its notice), else `None`.
    pub fn check(
        &mut self,
        now: Seconds,
        frames_captured: u64,
        playing_for: Option<Seconds>,
    ) -> Option<CaptureVerdict> {
        let due = self.next_at?;
        if now < due {
            return None;
        }
        let verdict = capture_verdict(frames_captured, playing_for);
        self.next_at = match verdict {
            CaptureVerdict::Flowing => None,
            _ => Some(now + CHECK_EVERY),
        };
        if self.last == Some(verdict) {
            None
        } else {
            self.last = Some(verdict);
            Some(verdict)
        }
    }

    /// The session ended: no more checks.
    pub fn session_ended(&mut self) {
        self.next_at = None;
    }
}
