//! When to try a speaker again after its session ended with an error
//! (P142).
//!
//! While the host still wants a speaker, a failed session is followed by a
//! new one after 1, 2, 4, 8 and 16 s and then every 30 s, each a complete
//! new session with fresh identifiers. Kinds that need the user
//! ([`ErrorKind::needs_user`]: a missing or wrong password, pairing, an
//! unsupported receiver) stop the retries. [`Backoff`] is the sans-I/O
//! schedule; the sender's driver keeps one per speaker.

use std::time::{Duration, Instant};

use crate::error::ErrorKind;

/// The waits before the first retries after a loss (P142).
pub const RETRY_DELAYS: [Duration; 5] = [
    Duration::from_secs(1),
    Duration::from_secs(2),
    Duration::from_secs(4),
    Duration::from_secs(8),
    Duration::from_secs(16),
];

/// The wait between retries once [`RETRY_DELAYS`] are used up (P142).
pub const RETRY_INTERVAL: Duration = Duration::from_secs(30);

/// The longest back-off a receiver's pair-verify answer can impose before
/// a retry (P154).
pub const MAX_BACK_OFF: Duration = Duration::from_secs(600);

/// The wait before retry number `attempt`, counted from 0 since the last
/// time the speaker played (P142).
pub fn retry_delay(attempt: u32) -> Duration {
    usize::try_from(attempt)
        .ok()
        .and_then(|index| RETRY_DELAYS.get(index).copied())
        .unwrap_or(RETRY_INTERVAL)
}

/// One speaker's retry schedule.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Backoff {
    /// Failed sessions since the speaker last played.
    failures: u32,
    /// What one second of [`RETRY_DELAYS`] and [`RETRY_INTERVAL`] lasts.
    unit: Duration,
}

impl Default for Backoff {
    fn default() -> Backoff {
        Backoff::new()
    }
}

impl Backoff {
    pub fn new() -> Backoff {
        Backoff::with_unit(Duration::from_secs(1))
    }

    /// The schedule with each of its seconds lasting `unit`; a test picks a
    /// short one to see several retries quickly.
    pub fn with_unit(unit: Duration) -> Backoff {
        Backoff { failures: 0, unit }
    }

    fn delay(&self) -> Duration {
        retry_delay(self.failures).mul_f64(self.unit.as_secs_f64())
    }

    /// A session ended with `kind` at `now`: the time the next session
    /// starts, or `None` when the kind needs the user and the retries stop
    /// (P142).
    pub fn on_failure(&mut self, kind: ErrorKind, now: Instant) -> Option<Instant> {
        if kind.needs_user() {
            return None;
        }
        let wait = self.delay();
        self.failures = self.failures.saturating_add(1);
        Some(now + wait)
    }

    /// A pair-verify refused for the receiver's state (P154) at `now`: the
    /// next session starts on the usual schedule, but no sooner than
    /// `wait`, which is capped at [`MAX_BACK_OFF`].
    pub fn on_receiver_state(&mut self, wait: Duration, now: Instant) -> Instant {
        let at = now + self.delay().max(wait.min(MAX_BACK_OFF));
        self.failures = self.failures.saturating_add(1);
        at
    }

    /// A session reached playing: the next loss starts the schedule again
    /// at 1 s.
    pub fn on_playing(&mut self) {
        self.failures = 0;
    }

    /// Failed sessions since the speaker last played.
    pub fn failures(&self) -> u32 {
        self.failures
    }
}
