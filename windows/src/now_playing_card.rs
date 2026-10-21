//! The Now Playing card in the main window, as on the Mac: the track of the
//! media session that counts for the chosen source, with its artwork, and
//! Previous, Play or Pause, and Next for that session.
//!
//! The sessions are Windows' system media transport controls, read through
//! [`MediaControls`] (`audio::wasapi::MediaCardReader` on Windows, a fake in
//! tests). They are read only while the main window shows, as the Mac reads
//! its players only while the window or a receiver needs them. With the
//! whole system as the source, the session that plays counts, else the first
//! paused one; with one app, that app's own; an input device has none, so
//! the card hides. Without a session that counts, the card hides too.
//!
//! The card keeps the last track while the window is hidden, so it shows at
//! once when the window comes back, and changes with the first new reading.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use crate::ui_model::AppState;

/// Which sessions count for the card, from the chosen source.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CardSource {
    /// The whole system: any app's session.
    AnyApp,
    /// One app, by its executable's file name: its own session only.
    App { executable: String },
}

impl CardSource {
    /// The card's source for the source the main window shows as chosen;
    /// `None` for an input device, which has no media session.
    pub fn of(state: &AppState) -> Option<CardSource> {
        let id = state.source_id.as_str();
        if id == "system" {
            return Some(CardSource::AnyApp);
        }
        // App sources are `app:<pid>:<executable>`.
        let executable = id.strip_prefix("app:")?.split_once(':')?.1;
        Some(CardSource::App {
            executable: executable.to_owned(),
        })
    }
}

/// A button of the card.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Transport {
    Previous,
    PlayPause,
    Next,
}

/// A track's artwork for the card: square, premultiplied RGBA.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CardArtwork {
    pub side: u32,
    pub rgba: Arc<[u8]>,
}

/// How many pixels a side the card's artwork has: its 56 px tile at 200 %.
pub const ARTWORK_SIDE: u32 = 112;

/// The size an image of `width` × `height` is scaled to so that it covers a
/// `side` × `side` square, the rest cropped evenly, as the Mac's tile fills.
pub fn cover_size(width: u32, height: u32, side: u32) -> (u32, u32) {
    let shorter = width.min(height).max(1);
    let scaled = |length: u32| {
        let length = (u64::from(length) * u64::from(side)).div_ceil(u64::from(shorter));
        u32::try_from(length).unwrap_or(u32::MAX).max(side)
    };
    (scaled(width), scaled(height))
}

/// Artwork kept for at most this many tracks.
const ARTWORK_CACHE: usize = 32;

/// The card's artwork, made once per track by key. A failed attempt isn't
/// kept, so the next reading tries again; only a track's first failure is
/// handed back to be logged.
#[derive(Default)]
pub struct ArtworkCache {
    made: HashMap<String, Option<CardArtwork>>,
    failed: HashSet<String>,
}

impl ArtworkCache {
    /// What was made for `key`, if an attempt worked.
    pub fn get(&self, key: &str) -> Option<Option<CardArtwork>> {
        self.made.get(key).cloned()
    }

    /// Takes an attempt for `key`: the artwork, and the error when it is
    /// the track's first.
    pub fn keep<E>(
        &mut self,
        key: &str,
        attempt: Result<Option<CardArtwork>, E>,
    ) -> (Option<CardArtwork>, Option<E>) {
        match attempt {
            Ok(artwork) => {
                if self.made.len() >= ARTWORK_CACHE {
                    self.made.clear();
                }
                self.failed.remove(key);
                self.made.insert(key.to_owned(), artwork.clone());
                (artwork, None)
            }
            Err(error) => {
                if self.failed.len() >= ARTWORK_CACHE {
                    self.failed.clear();
                }
                let first = self.failed.insert(key.to_owned());
                (None, first.then_some(error))
            }
        }
    }
}

/// What the card shows of a session.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CardTrack {
    /// The app's user model ID, as the session reports it.
    pub app_id: String,
    pub title: String,
    pub artist: String,
    pub playing: bool,
    pub artwork: Option<CardArtwork>,
}

impl CardTrack {
    /// The line under the title: the artist, or the app when the session
    /// names no artist, as on the Mac.
    pub fn subtitle(&self) -> String {
        if !self.artist.trim().is_empty() {
            return self.artist.clone();
        }
        app_name(&self.app_id)
    }
}

/// An app's name from its session's user model ID: the executable's name
/// without `.exe` (`Spotify.exe`), the last part of a path, or a packaged
/// app's own name after `!`.
pub fn app_name(app_id: &str) -> String {
    let last = app_id.rsplit(['\\', '/', '!']).next().unwrap_or(app_id);
    let stem = match last.len().checked_sub(4) {
        Some(cut) if last.is_char_boundary(cut) && last[cut..].eq_ignore_ascii_case(".exe") => {
            &last[..cut]
        }
        _ => last,
    };
    let mut chars = stem.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

/// Windows' media sessions, as the card reads and drives them.
pub trait MediaControls {
    /// Reads the sessions from now on for `source`, or stops (`None`). A
    /// reading made for an earlier source is dropped.
    fn follow(&mut self, source: Option<CardSource>);

    /// The newest reading since the last call, if one was made: the track
    /// that counts, or `None` when no session does.
    fn take_reading(&mut self) -> Option<Option<CardTrack>>;

    /// Sends `command` to the session that counts, then reads again soon.
    fn send(&mut self, command: Transport);
}

/// The card's state between the windows and the sessions.
#[derive(Default)]
pub struct NowPlayingCard {
    controls: Option<Box<dyn MediaControls>>,
    /// What the sessions are read for; `None` while they aren't.
    following: Option<CardSource>,
    track: Option<CardTrack>,
}

impl NowPlayingCard {
    pub fn set_controls(&mut self, controls: Box<dyn MediaControls>) {
        self.controls = Some(controls);
        self.following = None;
    }

    /// Follows the chosen `source` while the main window `shows`, and takes
    /// the newest reading. Returns whether the card changed.
    pub fn sync(&mut self, source: Option<CardSource>, shows: bool) -> bool {
        let Some(controls) = self.controls.as_mut() else {
            return false;
        };
        let following = if shows { source.clone() } else { None };
        if following != self.following {
            controls.follow(following.clone());
            self.following = following;
        }
        let mut next = self.track.clone();
        if source.is_none() {
            next = None;
        }
        if let Some(reading) = controls.take_reading() {
            next = reading;
        }
        if next == self.track {
            return false;
        }
        self.track = next;
        true
    }

    /// The track the card shows; `None` while the card is hidden.
    pub fn track(&self) -> Option<&CardTrack> {
        self.track.as_ref()
    }

    /// One of the card's buttons was used. Nothing happens while the card
    /// is hidden.
    pub fn send(&mut self, command: Transport) {
        if self.track.is_none() {
            return;
        }
        if let Some(controls) = self.controls.as_mut() {
            controls.send(command);
        }
    }
}

#[cfg(test)]
pub mod fake {
    use std::cell::RefCell;
    use std::rc::Rc;

    use super::*;

    #[derive(Default)]
    pub struct FakeState {
        pub following: Option<CardSource>,
        /// Every follow call, in order.
        pub follows: Vec<Option<CardSource>>,
        pub reading: Option<Option<CardTrack>>,
        pub sent: Vec<Transport>,
    }

    /// Media sessions the test sets; clones share them.
    #[derive(Clone, Default)]
    pub struct FakeControls(pub Rc<RefCell<FakeState>>);

    impl FakeControls {
        pub fn controls(&self) -> Box<dyn MediaControls> {
            Box::new(self.clone())
        }

        /// The next reading finds `track`.
        pub fn read(&self, track: Option<CardTrack>) {
            self.0.borrow_mut().reading = Some(track);
        }

        pub fn following(&self) -> Option<CardSource> {
            self.0.borrow().following.clone()
        }

        pub fn sent(&self) -> Vec<Transport> {
            self.0.borrow().sent.clone()
        }
    }

    impl MediaControls for FakeControls {
        fn follow(&mut self, source: Option<CardSource>) {
            let mut state = self.0.borrow_mut();
            state.follows.push(source.clone());
            state.following = source;
        }

        fn take_reading(&mut self) -> Option<Option<CardTrack>> {
            self.0.borrow_mut().reading.take()
        }

        fn send(&mut self, command: Transport) {
            self.0.borrow_mut().sent.push(command);
        }
    }

    pub fn track(app_id: &str, title: &str, artist: &str, playing: bool) -> CardTrack {
        CardTrack {
            app_id: app_id.into(),
            title: title.into(),
            artist: artist.into(),
            playing,
            artwork: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::fake::{FakeControls, track};
    use super::*;
    use crate::ui_model::{Backend, MockBackend};

    fn card(fake: &FakeControls) -> NowPlayingCard {
        let mut card = NowPlayingCard::default();
        card.set_controls(fake.controls());
        card
    }

    fn player() -> Option<CardSource> {
        Some(CardSource::App {
            executable: "player.exe".into(),
        })
    }

    #[test]
    fn the_cards_source_follows_the_chosen_source() {
        let mut state = MockBackend::new().snapshot();
        let mut source = |id: &str| {
            state.source_id = id.into();
            CardSource::of(&state)
        };
        assert_eq!(source("system"), Some(CardSource::AnyApp));
        assert_eq!(source("app:2000:player.exe"), player());
        assert_eq!(source("input:{0.0.1.00000000}.{mic}"), None);
    }

    #[test]
    fn the_card_shows_the_sessions_track_and_follows_its_changes() {
        let fake = FakeControls::default();
        let mut card = card(&fake);
        assert!(!card.sync(Some(CardSource::AnyApp), true));
        assert_eq!(fake.following(), Some(CardSource::AnyApp));
        assert_eq!(card.track(), None);

        fake.read(Some(track("Spotify.exe", "Song", "Band", true)));
        assert!(card.sync(Some(CardSource::AnyApp), true));
        let shown = card.track().expect("the card shows");
        assert_eq!(
            (shown.title.as_str(), shown.subtitle()),
            ("Song", "Band".into())
        );
        assert!(shown.playing);

        assert!(!card.sync(Some(CardSource::AnyApp), true), "no new reading");
        fake.read(Some(track("Spotify.exe", "Song", "Band", true)));
        assert!(!card.sync(Some(CardSource::AnyApp), true), "the same track");

        fake.read(Some(track("Spotify.exe", "Song", "Band", false)));
        assert!(card.sync(Some(CardSource::AnyApp), true));
        assert!(!card.track().unwrap().playing, "paused");
    }

    #[test]
    fn the_card_hides_without_a_session_that_counts() {
        let fake = FakeControls::default();
        let mut card = card(&fake);
        fake.read(Some(track("Spotify.exe", "Song", "Band", true)));
        card.sync(Some(CardSource::AnyApp), true);
        fake.read(None);
        assert!(card.sync(Some(CardSource::AnyApp), true));
        assert_eq!(card.track(), None);
    }

    #[test]
    fn an_input_device_hides_the_card_at_once_and_reads_nothing() {
        let fake = FakeControls::default();
        let mut card = card(&fake);
        fake.read(Some(track("Spotify.exe", "Song", "Band", true)));
        card.sync(Some(CardSource::AnyApp), true);
        assert!(card.sync(None, true));
        assert_eq!(card.track(), None);
        assert_eq!(fake.following(), None);
    }

    #[test]
    fn sessions_are_read_only_while_the_window_shows_and_the_track_stays() {
        let fake = FakeControls::default();
        let mut card = card(&fake);
        card.sync(player(), false);
        assert_eq!(fake.0.borrow().follows, [], "never read while hidden");
        card.sync(player(), true);
        fake.read(Some(track("Player.exe", "Song", "", true)));
        card.sync(player(), true);

        assert!(!card.sync(player(), false));
        assert_eq!(fake.following(), None);
        assert_eq!(card.track().map(|t| t.title.as_str()), Some("Song"));
        card.sync(player(), true);
        assert_eq!(fake.following(), player());
    }

    #[test]
    fn the_buttons_drive_the_session_only_while_the_card_shows() {
        let fake = FakeControls::default();
        let mut card = card(&fake);
        card.sync(Some(CardSource::AnyApp), true);
        card.send(Transport::PlayPause);
        assert_eq!(fake.sent(), [], "no card, no button");

        fake.read(Some(track("Spotify.exe", "Song", "Band", true)));
        card.sync(Some(CardSource::AnyApp), true);
        card.send(Transport::Previous);
        card.send(Transport::PlayPause);
        card.send(Transport::Next);
        assert_eq!(
            fake.sent(),
            [Transport::Previous, Transport::PlayPause, Transport::Next]
        );
    }

    #[test]
    fn artwork_is_scaled_to_cover_the_square_tile() {
        assert_eq!(cover_size(600, 600, 112), (112, 112));
        assert_eq!(cover_size(300, 150, 112), (224, 112));
        assert_eq!(cover_size(100, 333, 112), (112, 373));
        assert_eq!(cover_size(56, 56, 112), (112, 112), "scaled up");
        assert_eq!(cover_size(0, 10, 112), (112, 1120));
    }

    #[test]
    fn failed_artwork_is_tried_again_and_logged_once() {
        let art = CardArtwork {
            side: 1,
            rgba: Arc::from(&[1, 2, 3, 4][..]),
        };
        let mut cache = ArtworkCache::default();
        assert_eq!(cache.keep("a", Err("busy")), (None, Some("busy")));
        assert_eq!(cache.get("a"), None, "a failure isn't kept");
        assert_eq!(cache.keep("a", Err("busy")), (None, None), "logged once");
        assert_eq!(
            cache.keep::<&str>("a", Ok(Some(art.clone()))),
            (Some(art.clone()), None)
        );
        assert_eq!(cache.get("a"), Some(Some(art)));
        assert_eq!(cache.keep::<&str>("b", Ok(None)), (None, None));
        assert_eq!(cache.get("b"), Some(None), "an empty image is kept");
    }

    #[test]
    fn without_an_artist_the_app_names_the_track() {
        let named = |app_id: &str| track(app_id, "Song", " ", true).subtitle();
        assert_eq!(named("Spotify.exe"), "Spotify");
        assert_eq!(named("chrome"), "Chrome");
        assert_eq!(named(r"C:\Apps\vlc.EXE"), "Vlc");
        assert_eq!(
            named("SpotifyAB.SpotifyMusic_zpdnekdrzrea0!Spotify"),
            "Spotify"
        );
        assert_eq!(named(""), "");
    }
}
