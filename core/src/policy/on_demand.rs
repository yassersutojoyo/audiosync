//! Speakers on demand: lasting sound in the captured source (S481), yielding
//! an AirPlay or Cast speaker another sender took (S482), and releasing them
//! all when nothing plays (S483).
//!
//! The host feeds the peak of each 100 ms window of the source, after the
//! source mix and before the equalizer and volume, and the senders' reports;
//! it carries out the [`Action`]s and shows [`Status`]. Local outputs never
//! go through here.

use std::collections::BTreeMap;

use super::Seconds;

/// The length of one window the host measures (S481).
pub const WINDOW: Seconds = 0.1;

/// A window is audible when its largest sample magnitude exceeds this,
/// -60 dBFS (S481).
pub const AUDIBLE_PEAK: f32 = 0.001;

/// Audible windows of the last [`SPAN_WINDOWS`] that make lasting sound,
/// 5.0 s of 6.0 s (S481).
pub const LASTING_WINDOWS: u32 = 50;

/// The windows lasting sound looks back over (S481).
pub const SPAN_WINDOWS: u32 = 60;

/// Seconds with no audible window that end Playing (S481).
pub const PLAYING_HOLD: Seconds = 10.0;

/// Silence before sound that makes it fresh for a waiting speaker (S482).
pub const FRESH_SILENCE: Seconds = 2.0;

/// The idle time a new release setting starts with, in minutes (S483).
pub const DEFAULT_RELEASE_MINUTES: u32 = 15;

/// Where a speaker the user switched on is (S482, S483).
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Status {
    /// Its sender runs it, or retries it as usual.
    Active,
    /// The receiver closed it; the sender's evidence window runs (P249,
    /// C141). `playing` is the policy's state at the close.
    Closed { playing: bool },
    /// Yielded to another sender since `since`: back on fresh lasting sound
    /// or the user's switch.
    Waiting { since: Seconds },
    /// Let go after the idle time: back on lasting sound, the user's switch
    /// or the setting turned off.
    Released,
}

/// What the host does for a speaker.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action<K> {
    /// Start a new session, as when the user switches it on.
    Start(K),
    /// End its session as a deselect does, keeping it switched on (P250,
    /// C142).
    Release(K),
}

/// The policy's state, one per host.
#[derive(Clone, Debug)]
pub struct OnDemand<K> {
    /// One bit per window, the newest lowest.
    windows: u64,
    playing: bool,
    last_audible: Option<Seconds>,
    /// The first window of the current run of silent windows.
    quiet_since: Option<Seconds>,
    /// The first audible window after a silence of at least
    /// [`FRESH_SILENCE`], and when that silence began.
    stretch: Option<(Seconds, Seconds)>,
    /// When the idle clock started: the last audible window of the last
    /// Playing stretch, or the policy's start (S483).
    idle_from: Seconds,
    release_minutes: Option<u32>,
    released: bool,
    speakers: BTreeMap<K, Status>,
}

impl<K: Ord + Clone> OnDemand<K> {
    pub fn new(now: Seconds) -> OnDemand<K> {
        OnDemand {
            windows: 0,
            playing: false,
            last_audible: None,
            quiet_since: Some(now),
            stretch: None,
            idle_from: now,
            release_minutes: None,
            released: false,
            speakers: BTreeMap::new(),
        }
    }

    /// Playing (S481).
    pub fn playing(&self) -> bool {
        self.playing
    }

    pub fn status(&self, key: &K) -> Option<Status> {
        self.speakers.get(key).copied()
    }

    fn lasting(&self) -> bool {
        let span = self.windows & ((1u64 << SPAN_WINDOWS) - 1);
        span.count_ones() >= LASTING_WINDOWS
    }

    /// The user switched the speaker on or picked it again: the host starts
    /// it itself (S482, S483).
    pub fn switched_on(&mut self, key: K) {
        self.speakers.insert(key, Status::Active);
    }

    /// The speakers it holds, in key order.
    pub fn switched_on_keys(&self) -> Vec<K> {
        self.speakers.keys().cloned().collect()
    }

    pub fn switched_off(&mut self, key: &K) {
        self.speakers.remove(key);
    }

    /// The sender reports the receiver closed the connection (P249, C141).
    pub fn closed_by_receiver(&mut self, key: &K) {
        let playing = self.playing;
        if let Some(status) = self.speakers.get_mut(key) {
            *status = Status::Closed { playing };
        }
    }

    /// The sender reports another sender has it (P249, C141).
    pub fn taken(&mut self, key: &K, now: Seconds) {
        if let Some(status) = self.speakers.get_mut(key) {
            *status = Status::Waiting { since: now };
        }
    }

    /// The evidence window ended with no evidence: closed while Quiet it
    /// waits, while Playing the host lets the sender retry (S482).
    pub fn no_evidence(&mut self, key: &K, now: Seconds) -> Option<Action<K>> {
        let status = self.speakers.get_mut(key)?;
        match *status {
            Status::Closed { playing: false } => {
                *status = Status::Waiting { since: now };
                None
            }
            Status::Closed { playing: true } => {
                *status = Status::Active;
                Some(Action::Start(key.clone()))
            }
            _ => None,
        }
    }

    /// Turns the idle release on with `minutes`, or off with `None` (S483).
    /// Off brings released speakers back at once.
    pub fn set_release(&mut self, minutes: Option<u32>, now: Seconds) -> Vec<Action<K>> {
        self.release_minutes = minutes;
        if minutes.is_none() && self.released {
            return self.come_back(|status| matches!(status, Status::Released));
        }
        self.check_idle(now)
    }

    /// One window of the source: its largest sample magnitude, measured
    /// at `now` (S481).
    pub fn window(&mut self, peak: f32, now: Seconds) -> Vec<Action<K>> {
        let audible = peak > AUDIBLE_PEAK;
        self.windows = (self.windows << 1) | u64::from(audible);
        if audible {
            if let Some(quiet) = self.quiet_since.take()
                && (self.stretch.is_none() || now - quiet >= FRESH_SILENCE)
            {
                self.stretch = Some((now, quiet));
            }
            self.last_audible = Some(now);
        } else if self.quiet_since.is_none() {
            self.quiet_since = Some(now);
        }
        let lasting = self.lasting();
        if lasting {
            self.playing = true;
        } else if self.playing && self.last_audible.is_none_or(|at| now - at >= PLAYING_HOLD) {
            self.playing = false;
        }
        if self.playing && audible {
            self.idle_from = now;
        }
        let mut actions = Vec::new();
        if lasting {
            let stretch = self.stretch;
            let fresh = move |status: &Status| match (*status, stretch) {
                (Status::Released, _) => true,
                (Status::Waiting { since }, Some((onset, quiet))) => {
                    onset - quiet.max(since) >= FRESH_SILENCE
                }
                _ => false,
            };
            actions = self.come_back(fresh);
            self.released = false;
        }
        actions.extend(self.check_idle(now));
        actions
    }

    fn come_back(&mut self, back: impl Fn(&Status) -> bool) -> Vec<Action<K>> {
        let mut actions = Vec::new();
        for (key, status) in &mut self.speakers {
            if back(status) {
                *status = Status::Active;
                actions.push(Action::Start(key.clone()));
            }
        }
        if !self
            .speakers
            .values()
            .any(|s| matches!(s, Status::Released))
        {
            self.released = false;
        }
        actions
    }

    fn check_idle(&mut self, now: Seconds) -> Vec<Action<K>> {
        let Some(minutes) = self.release_minutes else {
            return Vec::new();
        };
        if self.playing || self.released || now - self.idle_from < f64::from(minutes) * 60.0 {
            return Vec::new();
        }
        self.released = true;
        let mut actions = Vec::new();
        for (key, status) in &mut self.speakers {
            if !matches!(status, Status::Released) {
                *status = Status::Released;
                actions.push(Action::Release(key.clone()));
            }
        }
        actions
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LOUD: f32 = 0.1;
    const QUIET: f32 = 0.0005;

    /// Feeds `seconds` of windows at `peak` from `*now`.
    fn feed(
        policy: &mut OnDemand<&'static str>,
        now: &mut Seconds,
        seconds: Seconds,
        peak: f32,
    ) -> Vec<Action<&'static str>> {
        let mut actions = Vec::new();
        let count = (seconds / WINDOW).round() as u32;
        for _ in 0..count {
            *now += WINDOW;
            actions.extend(policy.window(peak, *now));
        }
        actions
    }

    // covers: S481
    #[test]
    fn five_seconds_of_six_make_lasting_sound_and_ten_quiet_seconds_end_it() {
        let mut now = 0.0;
        let mut policy = OnDemand::<&str>::new(now);
        feed(&mut policy, &mut now, 4.9, LOUD);
        assert!(!policy.playing());
        feed(&mut policy, &mut now, 0.1, LOUD);
        assert!(policy.playing());
        feed(&mut policy, &mut now, 9.9, QUIET);
        assert!(policy.playing());
        feed(&mut policy, &mut now, 0.1, 0.0);
        assert!(!policy.playing());
        // Gaps of 1 s in every 6 s still add up.
        let mut policy = OnDemand::<&str>::new(0.0);
        let mut now = 0.0;
        for _ in 0..2 {
            feed(&mut policy, &mut now, 2.5, LOUD);
            feed(&mut policy, &mut now, 0.5, 0.0);
        }
        feed(&mut policy, &mut now, 0.1, LOUD);
        assert!(policy.playing());
    }

    // A 2.11 s notification, the longest Windows ships, and one every few
    // seconds never make lasting sound.
    // covers: S481
    #[test]
    fn notifications_never_make_lasting_sound() {
        let mut now = 0.0;
        let mut policy = OnDemand::<&str>::new(now);
        for _ in 0..20 {
            feed(&mut policy, &mut now, 2.2, LOUD);
            feed(&mut policy, &mut now, 1.5, 0.0);
            assert!(!policy.playing());
        }
    }

    // covers: S482
    #[test]
    fn a_taken_speaker_waits_for_fresh_lasting_sound() {
        let mut now = 0.0;
        let mut policy = OnDemand::new(now);
        policy.switched_on("tv");
        policy.switched_on("kitchen");
        // T13: sound runs through the takeover with gaps under 2 s.
        feed(&mut policy, &mut now, 8.0, LOUD);
        policy.taken(&"tv", now);
        for _ in 0..10 {
            assert_eq!(feed(&mut policy, &mut now, 5.0, LOUD), []);
            assert_eq!(feed(&mut policy, &mut now, 1.5, 0.0), []);
        }
        assert!(matches!(policy.status(&"tv"), Some(Status::Waiting { .. })));
        assert_eq!(policy.status(&"kitchen"), Some(Status::Active));
        // T11: a notification after silence brings nothing back.
        feed(&mut policy, &mut now, 3.0, 0.0);
        assert_eq!(feed(&mut policy, &mut now, 2.1, LOUD), []);
        // T12: 3 s of silence, then 5 s of sound: one start.
        feed(&mut policy, &mut now, 3.0, 0.0);
        assert_eq!(feed(&mut policy, &mut now, 4.9, LOUD), []);
        assert_eq!(
            feed(&mut policy, &mut now, 0.1, LOUD),
            [Action::Start("tv")]
        );
        assert_eq!(policy.status(&"tv"), Some(Status::Active));
        assert_eq!(feed(&mut policy, &mut now, 30.0, LOUD), []);
    }

    // A silence that began before the takeover counts only from it.
    // covers: S482
    #[test]
    fn only_silence_after_the_takeover_makes_sound_fresh() {
        let mut now = 0.0;
        let mut policy = OnDemand::new(now);
        policy.switched_on("tv");
        feed(&mut policy, &mut now, 30.0, 0.0);
        policy.taken(&"tv", now);
        feed(&mut policy, &mut now, 1.0, 0.0);
        assert_eq!(feed(&mut policy, &mut now, 10.0, LOUD), []);
        let mut policy = OnDemand::new(0.0);
        let mut now = 0.0;
        policy.switched_on("tv");
        feed(&mut policy, &mut now, 30.0, 0.0);
        policy.taken(&"tv", now);
        feed(&mut policy, &mut now, 2.0, 0.0);
        assert_eq!(
            feed(&mut policy, &mut now, 5.0, LOUD),
            [Action::Start("tv")]
        );
    }

    // T4q, T4p: no evidence after a close while Quiet waits; while Playing
    // the sender retries.
    // covers: S482
    #[test]
    fn a_close_with_no_evidence_waits_when_quiet_and_retries_when_playing() {
        let mut now = 0.0;
        let mut policy = OnDemand::new(now);
        policy.switched_on("tv");
        policy.closed_by_receiver(&"tv");
        feed(&mut policy, &mut now, 10.0, 0.0);
        assert_eq!(policy.no_evidence(&"tv", now), None);
        assert!(matches!(policy.status(&"tv"), Some(Status::Waiting { .. })));
        feed(&mut policy, &mut now, 2.0, 0.0);
        feed(&mut policy, &mut now, 6.0, LOUD);
        assert_eq!(policy.status(&"tv"), Some(Status::Active));
        policy.closed_by_receiver(&"tv");
        assert_eq!(policy.status(&"tv"), Some(Status::Closed { playing: true }));
        feed(&mut policy, &mut now, 10.0, LOUD);
        assert_eq!(policy.no_evidence(&"tv", now), Some(Action::Start("tv")));
        policy.taken(&"tv", now);
        policy.switched_on("tv");
        assert_eq!(policy.status(&"tv"), Some(Status::Active));
    }

    // T14, T15: blips don't reset the idle clock; the release comes on time,
    // a blip brings nothing back, lasting sound brings every speaker back.
    // covers: S483
    #[test]
    fn speakers_are_released_after_the_idle_time_and_come_back_together() {
        let mut now = 0.0;
        let mut policy = OnDemand::new(now);
        policy.switched_on("tv");
        policy.switched_on("kitchen");
        assert_eq!(policy.set_release(Some(DEFAULT_RELEASE_MINUTES), now), []);
        feed(&mut policy, &mut now, 10.0, LOUD);
        let last_sound = now;
        let mut released = Vec::new();
        while released.is_empty() {
            released = feed(&mut policy, &mut now, 58.0, 0.0);
            released.extend(feed(&mut policy, &mut now, 2.0, LOUD));
        }
        assert_eq!(
            released,
            [Action::Release("kitchen"), Action::Release("tv")]
        );
        assert!(
            now - last_sound >= 900.0 && now - last_sound < 960.0,
            "{}",
            now - last_sound
        );
        assert_eq!(policy.status(&"tv"), Some(Status::Released));
        assert_eq!(feed(&mut policy, &mut now, 2.0, LOUD), []);
        feed(&mut policy, &mut now, 1.0, 0.0);
        assert_eq!(
            feed(&mut policy, &mut now, 5.0, LOUD),
            [Action::Start("kitchen"), Action::Start("tv")]
        );
        assert_eq!(feed(&mut policy, &mut now, 100.0, 0.0), []);
    }

    // Off by default; turning it off brings released speakers back at once.
    // covers: S483
    #[test]
    fn the_release_is_off_by_default_and_turning_it_off_restarts_released_speakers() {
        let mut now = 0.0;
        let mut policy = OnDemand::new(now);
        policy.switched_on("tv");
        assert_eq!(feed(&mut policy, &mut now, 3600.0, 0.0), []);
        assert_eq!(policy.set_release(Some(1), now), [Action::Release("tv")]);
        assert_eq!(policy.set_release(None, now), [Action::Start("tv")]);
        assert_eq!(policy.status(&"tv"), Some(Status::Active));
    }
}
