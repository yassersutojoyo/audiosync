//! Resume Playback After Sleep (S465): what a sleep notes of the player that
//! plays, and whether the wake sends that player play.

use std::time::{Duration, Instant};

use super::now_playing::MediaSession;

/// A wake's Wi-Fi rejoin and AirPlay reconnect take seconds; past 30 s a play
/// would surprise rather than resume.
pub const GIVE_UP_AFTER: Duration = Duration::from_secs(30);

/// The player and song playing when the PC went to sleep.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Noted {
    pub app_id: String,
    pub title: String,
    pub artist: String,
    pub album: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    Resume,
    Wait,
    Skip(&'static str),
}

/// A noted player from the sleep until the wake's verdict.
#[derive(Clone, Debug)]
pub struct Pending {
    pub noted: Noted,
    pub woke_at: Option<Instant>,
    pub back_at: Option<Instant>,
    pub user_acted: bool,
}

/// What a sleep notes: the session read last, when it plays while streaming
/// runs and the switch is on.
pub fn noted(enabled: bool, streaming: bool, reading: Option<&MediaSession>) -> Option<Noted> {
    let session = reading.filter(|session| enabled && streaming && session.playing)?;
    Some(Noted {
        app_id: session.app_id.clone(),
        title: session.title.clone(),
        artist: session.artist.clone(),
        album: session.album.clone(),
    })
}

/// `after`: the reading taken once the outputs were back, `None` until one
/// came; `Some(None)` when it found no session.
pub fn decide(
    enabled: bool,
    noted: Option<&Noted>,
    streaming: bool,
    user_acted: bool,
    outputs_back: bool,
    after: Option<Option<&MediaSession>>,
    waited: Duration,
) -> Verdict {
    if !enabled {
        return Verdict::Skip("the switch is off");
    }
    let Some(noted) = noted else {
        return Verdict::Skip("nothing was noted at sleep");
    };
    if user_acted {
        return Verdict::Skip("the user acted or the player played since the wake");
    }
    if !streaming {
        return Verdict::Skip("streaming is off");
    }
    let Some(after) = after.filter(|_| outputs_back) else {
        return if waited < GIVE_UP_AFTER {
            Verdict::Wait
        } else {
            Verdict::Skip("the outputs or a reading did not come within 30 s")
        };
    };
    let Some(after) = after else {
        return Verdict::Skip("the player quit or no session is read");
    };
    if after.app_id != noted.app_id {
        return Verdict::Skip("another player is read");
    }
    if (&after.title, &after.artist, &after.album) != (&noted.title, &noted.artist, &noted.album) {
        return Verdict::Skip("another song is read");
    }
    if after.playing {
        return Verdict::Skip("the player already plays");
    }
    Verdict::Resume
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::now_playing::fake::session;

    fn spotify(playing: bool) -> MediaSession {
        session("Spotify.exe", "Song", playing)
    }

    fn noted_spotify() -> Noted {
        noted(true, true, Some(&spotify(true))).expect("a playing session is noted")
    }

    fn verdict(
        enabled: bool,
        user_acted: bool,
        outputs_back: bool,
        after: Option<Option<&MediaSession>>,
        waited: u64,
    ) -> Verdict {
        decide(
            enabled,
            Some(&noted_spotify()),
            true,
            user_acted,
            outputs_back,
            after,
            Duration::from_secs(waited),
        )
    }

    // covers: S465
    #[test]
    fn a_sleep_notes_only_a_playing_session_while_streaming_with_the_switch_on() {
        assert_eq!(
            noted_spotify(),
            Noted {
                app_id: "Spotify.exe".to_owned(),
                title: "Song".to_owned(),
                artist: "Song artist".to_owned(),
                album: "Song album".to_owned(),
            }
        );
        assert_eq!(noted(false, true, Some(&spotify(true))), None);
        assert_eq!(noted(true, false, Some(&spotify(true))), None);
        assert_eq!(noted(true, true, Some(&spotify(false))), None);
        assert_eq!(noted(true, true, None), None);
    }

    // covers: S465
    #[test]
    fn the_wake_sends_play_once_the_outputs_are_back_and_the_song_reads_paused() {
        let paused = spotify(false);
        assert_eq!(
            verdict(true, false, true, Some(Some(&paused)), 5),
            Verdict::Resume
        );
    }

    // covers: S465
    #[test]
    fn the_wake_waits_for_the_outputs_and_a_reading_after_them() {
        let paused = spotify(false);
        assert_eq!(verdict(true, false, false, None, 5), Verdict::Wait);
        assert_eq!(
            verdict(true, false, false, Some(Some(&paused)), 5),
            Verdict::Wait
        );
        assert_eq!(verdict(true, false, true, None, 29), Verdict::Wait);
    }

    // covers: S465
    #[test]
    fn nothing_is_sent_30_seconds_after_the_wake() {
        assert!(matches!(
            verdict(true, false, false, None, 30),
            Verdict::Skip(_)
        ));
        assert!(matches!(
            verdict(true, false, true, None, 31),
            Verdict::Skip(_)
        ));
    }

    // covers: S465
    #[test]
    fn nothing_is_sent_with_the_switch_off_or_after_the_user_acted() {
        let paused = spotify(false);
        assert!(matches!(
            verdict(false, false, true, Some(Some(&paused)), 5),
            Verdict::Skip(_)
        ));
        assert!(matches!(
            verdict(true, true, true, Some(Some(&paused)), 5),
            Verdict::Skip(_)
        ));
    }

    // covers: S465
    #[test]
    fn nothing_is_sent_for_another_song_another_player_a_playing_one_or_none() {
        let other_song = session("Spotify.exe", "Other", false);
        let other_player = session("Music.UI.exe", "Song", false);
        let playing = spotify(true);
        for after in [Some(&other_song), Some(&other_player), Some(&playing), None] {
            assert!(
                matches!(verdict(true, false, true, Some(after), 5), Verdict::Skip(_)),
                "{after:?}"
            );
        }
    }

    // covers: S465
    #[test]
    fn nothing_is_sent_with_nothing_noted_or_streaming_off() {
        let paused = spotify(false);
        let wait = Duration::from_secs(5);
        assert!(matches!(
            decide(true, None, true, false, true, Some(Some(&paused)), wait),
            Verdict::Skip(_)
        ));
        assert!(matches!(
            decide(
                true,
                Some(&noted_spotify()),
                false,
                false,
                true,
                Some(Some(&paused)),
                wait
            ),
            Verdict::Skip(_)
        ));
    }
}
