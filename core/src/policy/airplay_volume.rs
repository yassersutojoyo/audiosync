//! The host's side of an AirPlay speaker's own volume: how often it is read
//! back and which speakers are asked (S291, P121, P184), which reading is the
//! echo of a set (S271), when a held slider ignores readings (S271), the
//! level the sender keeps for the next session (S291, P119, P180) and the
//! explicit pause's timing (P213). The level mapping is [`crate::volume`]'s,
//! a speaker's remote commands [`super::remote`]'s.
//!
//! Plain data in and out; the Mac host reaches it through the Rust glue in
//! `ffi::airplay`, not through the C ABI.
//! Where the hosts still differ, a [`HostRules`] field names the difference
//! and each host's constant holds what it does today.

use std::time::Duration;

use audiosync_airplay::error::Warning;
use audiosync_airplay::rtsp::Method;
use audiosync_airplay::volume::Volume;

use super::remote::MUTED_DB;

/// How often playing speakers are asked for their own volume (P122, S291).
pub const VOLUME_READ_INTERVAL: Duration = Duration::from_secs(1);

/// How long after the host sets a volume a reading near it is the speaker's
/// echo, not a person's change (S271, S276).
pub const VOLUME_SETTLE: Duration = Duration::from_secs(3);

/// How long after a pause a reading may still predate the app's own pause;
/// later readings that don't show the app paused resume. Longer than the
/// receivers' buffer, so the flush it bought is never undone audibly (P213).
pub const STALE_READING: Duration = Duration::from_secs(3);

/// A sample louder than this, -60 dBFS, is sound: digital silence and
/// dither stay below it (P213).
pub const SOUND_LEVEL: f32 = 0.001;

/// How much sound the capture carries, in total, before a paused stream
/// resumes on its own (P213).
pub const SOUND_TO_RESUME: Duration = Duration::from_millis(200);

/// How long a slider that started editing ignores the speaker's readings.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HeldSlider {
    /// Until it is released.
    UntilRelease,
}

/// What one host does where the hosts still differ.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HostRules {
    pub held_slider: HeldSlider,
}

pub const MAC: HostRules = HostRules {
    held_slider: HeldSlider::UntilRelease,
};

/// The Windows and Linux app.
pub const DESKTOP: HostRules = HostRules {
    held_slider: HeldSlider::UntilRelease,
};

/// Whether a reading `since_set` after the host last set the volume may be
/// that set's echo (S271).
pub fn within_echo_window(since_set: Duration) -> bool {
    since_set < VOLUME_SETTLE
}

/// Whether a slider that is `dragging` still holds its row against the
/// speaker's readings (S271).
pub fn slider_held(rule: HeldSlider, dragging: bool) -> bool {
    match rule {
        HeldSlider::UntilRelease => dragging,
    }
}

/// Whether `warning` says the speaker won't answer volume reads this session
/// (P184).
pub fn refuses_reads(warning: &Warning) -> bool {
    matches!(
        warning,
        Warning::RequestFailed {
            method: Method::GetParameter,
            ..
        } | Warning::VolumeReadsStopped
    )
}

/// The level the sender keeps from a reading of `db` for the next session,
/// past the wire range its nearest (S291, P119, P180).
pub fn kept_volume(db: f64) -> Volume {
    if db <= MUTED_DB {
        Volume::Muted
    } else {
        Volume::from_db(db).unwrap_or(Volume::Muted)
    }
}

/// The volume reads of one added speaker or target.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Reads {
    /// A read was asked since the volume was last set.
    pub reading: bool,
    /// It turned a read down, or its AirPlay 2 reads stopped (P184): it
    /// isn't asked again this session.
    pub unreadable: bool,
}

impl Reads {
    /// Asks a speaker that is `playing` unless it scales its own samples
    /// (P121) or turned reads down; returns whether it asked.
    pub fn ask(&mut self, playing: bool, software_volume: bool) -> bool {
        if !playing || software_volume || self.unreadable {
            return false;
        }
        self.reading = true;
        true
    }

    /// Whether an answer arriving now counts: one was asked since the last
    /// set. The read is answered either way.
    pub fn answered(&mut self) -> bool {
        std::mem::take(&mut self.reading)
    }

    /// The volume was set or the slider was held or let go: an answer to a
    /// read asked before is old.
    pub fn set(&mut self) {
        self.reading = false;
    }

    /// It turned a read down (P184).
    pub fn refused(&mut self) {
        self.unreadable = true;
        self.reading = false;
    }

    /// Its session stopped playing: P184 holds for one session only, so the
    /// next one is asked again.
    pub fn session_stopped(&mut self) {
        self.unreadable = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_playing_speaker_is_asked_once_a_second_unless_it_cannot_answer() {
        // covers: S291
        assert_eq!(VOLUME_READ_INTERVAL, Duration::from_secs(1));
        let mut reads = Reads::default();
        assert!(!reads.ask(false, false), "stopped");
        assert!(!reads.ask(true, true), "scales its own samples");
        assert!(!reads.answered(), "nobody asked");
        assert!(reads.ask(true, false));
        assert!(reads.answered());
        assert!(!reads.answered(), "one answer per read");
        assert!(reads.ask(true, false));
        reads.set();
        assert!(!reads.answered(), "asked before the set");
    }

    #[test]
    fn a_speaker_that_refused_a_read_is_asked_again_next_session_only() {
        // covers: S271
        let refusals = [
            Warning::VolumeReadsStopped,
            Warning::RequestFailed {
                method: Method::GetParameter,
                status: 500,
            },
        ];
        for warning in refusals {
            assert!(refuses_reads(&warning), "{warning:?}");
            let mut reads = Reads::default();
            assert!(reads.ask(true, false));
            reads.refused();
            assert!(!reads.answered(), "{warning:?}");
            assert!(!reads.ask(true, false), "{warning:?}");
            reads.session_stopped();
            assert!(reads.ask(true, false), "{warning:?}");
        }
        let set_failed = Warning::RequestFailed {
            method: Method::SetParameter,
            status: 500,
        };
        assert!(!refuses_reads(&set_failed));
        assert!(!refuses_reads(&Warning::JackDisconnected));
    }

    #[test]
    fn a_reading_within_3_s_of_a_set_may_be_its_echo() {
        // covers: S271
        assert!(within_echo_window(Duration::ZERO));
        assert!(within_echo_window(Duration::from_millis(2999)));
        assert!(!within_echo_window(Duration::from_secs(3)));
    }

    #[test]
    fn a_held_slider_ignores_readings_by_each_hosts_rule() {
        // covers: S271
        assert_eq!(MAC.held_slider, HeldSlider::UntilRelease);
        assert_eq!(DESKTOP.held_slider, HeldSlider::UntilRelease);
        assert!(slider_held(MAC.held_slider, true));
        assert!(!slider_held(MAC.held_slider, false));
    }

    #[test]
    fn the_level_kept_for_the_next_session_is_the_reading_within_the_wire_range() {
        // covers: S291
        assert_eq!(kept_volume(MUTED_DB), Volume::Muted);
        assert_eq!(kept_volume(-200.0), Volume::Muted);
        assert_eq!(kept_volume(-12.0), Volume::from_db(-12.0).unwrap());
        assert_eq!(kept_volume(-45.0), Volume::from_db(-30.0).unwrap());
        assert_eq!(kept_volume(0.0), Volume::from_db(0.0).unwrap());
    }

    #[test]
    fn the_pause_waits_3_s_for_stale_readings_and_200_ms_of_sound() {
        assert_eq!(STALE_READING, Duration::from_secs(3));
        assert_eq!(SOUND_TO_RESUME, Duration::from_millis(200));
        assert_eq!(SOUND_LEVEL, 0.001);
    }
}
