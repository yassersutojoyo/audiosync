//! What AirPlay receivers with a screen, such as a TV, show while AudioSync
//! streams to them: the playing track's title, artist, album and artwork,
//! read from the media sessions Windows lists (the system media transport
//! controls), as the Mac app reads its players.
//!
//! A [`TrackReader`] reads the sessions every [`POLL_INTERVAL`] on a thread
//! of its own, only while AirPlay receivers play; [`TrackInfo`] turns its
//! readings into what the sender is given, at the backend's polls, and
//! gives it only what changed. The sender keeps that and sends it to every
//! speaker that starts, until the last receiver stops: then it is cleared,
//! so a receiver that starts later never shows a track from before. The
//! rules, as on the Mac:
//!
//! - with the whole system as the source, the track of a session that
//!   plays; with one app as the source, that app's session only, paused or
//!   playing ([`pick`], [`decide`]);
//! - with no such track, AudioSync's own labels ([`ReceiverLabels`], worded
//!   in the UI's strings), the source's name as the artist, and its icon:
//!   never blank fields, since a TV may then show no text at all;
//! - a track without artwork waits up to [`ARTWORK_WAIT`] for it, then gets
//!   the icon. The Mac waits for Spotify only; Windows can hand any
//!   session's thumbnail a moment after its title;
//! - right after reading starts, nothing goes out until the first reading,
//!   for at most [`READING_WAIT`], so a track from before never does;
//! - artwork goes out as a JPEG of at most [`ARTWORK_LIMIT`] bytes and
//!   [`ARTWORK_SIDE`] pixels a side ([`fit_jpeg`]).
//!
//! Reading never runs on the UI thread or an audio thread: a session that
//! hangs holds up the reader's thread only, and a failed reading counts as
//! no track.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use audiosync_airplay::metadata::TrackInfo as TrackFields;
use audiosync_airplay::sender::NowPlaying;

use crate::debug_log::DebugLog;

/// How often the sessions are read while AirPlay receivers play.
pub const POLL_INTERVAL: Duration = Duration::from_secs(2);
/// How long a track waits for its artwork before the icon takes its place.
pub const ARTWORK_WAIT: Duration = Duration::from_secs(5);
/// How long track info waits for the first reading once reading starts.
pub const READING_WAIT: Duration = Duration::from_secs(3);
/// At most this many bytes of JPEG per image: a cover that every receiver
/// takes at once, well under the 512 KiB the sender accepts (P125).
pub const ARTWORK_LIMIT: usize = 6060;
/// At most this many pixels a side, as large as a receiver shows a cover.
pub const ARTWORK_SIDE: u32 = 600;
const SMALLEST_SIDE: u32 = 64;
const JPEG_QUALITIES: [f32; 3] = [0.65, 0.5, 0.4];

/// What receivers show while no track counts, worded by the UI
/// (`receiver-*` and `source-system` in ui/strings.slint).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReceiverLabels {
    pub title: String,
    pub album: String,
    /// The artist with the whole system as the source.
    pub whole_system: String,
    /// The artist when the source has no name to show.
    pub unnamed_source: String,
}

/// The labels, handed to track info before the UI's strings exist and set
/// once they do, before any receiver can play. Without them receivers get
/// nothing.
#[derive(Clone, Default)]
pub struct SharedLabels(Rc<RefCell<Option<ReceiverLabels>>>);

impl SharedLabels {
    pub fn set(&self, labels: ReceiverLabels) {
        *self.0.borrow_mut() = Some(labels);
    }

    fn get(&self) -> Option<ReceiverLabels> {
        self.0.borrow().clone()
    }
}

/// A media session as Windows lists it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MediaSession {
    /// The app's user model ID, as the session reports it.
    pub app_id: String,
    pub title: String,
    pub artist: String,
    pub album: String,
    pub playing: bool,
}

/// Which sessions count, from the capture source.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Follow {
    /// The whole system: any app's.
    AnyApp,
    /// One app, by its executable's file name: its own only.
    App { executable: String },
}

impl Follow {
    fn admits(&self, app_id: &str) -> bool {
        match self {
            Follow::AnyApp => true,
            Follow::App { executable } => app_matches(app_id, executable),
        }
    }
}

/// Whether the session app ID `app_id` is the app that runs `executable`.
/// A desktop app's ID is mostly its executable's name, with or without
/// `.exe` (`Spotify.exe`, `Chrome`), or a path to it; a packaged app's ends
/// in `!` and the app's own name (`…!Spotify`). An app whose ID is none of
/// these, such as a hash, never matches, so its track never shows while
/// that app alone is captured: AudioSync's labels show instead.
pub fn app_matches(app_id: &str, executable: &str) -> bool {
    let stem = executable_stem(executable);
    if stem.is_empty() {
        return false;
    }
    let id = app_id.to_lowercase();
    let last = id.rsplit(['\\', '/', '!']).next().unwrap_or(&id);
    executable_stem(last) == stem
}

fn executable_stem(name: &str) -> String {
    let name = name.to_lowercase();
    match name.strip_suffix(".exe") {
        Some(stem) => stem.to_owned(),
        None => name,
    }
}

/// The session whose track counts for `follow`: the first that plays, else
/// the first paused one. Sessions with a blank title, and those of other
/// apps when one app is followed, never count.
pub fn pick(sessions: &[MediaSession], follow: &Follow) -> Option<usize> {
    let mut paused = None;
    for (index, session) in sessions.iter().enumerate() {
        if session.title.trim().is_empty() || !follow.admits(&session.app_id) {
            continue;
        }
        if session.playing {
            return Some(index);
        }
        paused.get_or_insert(index);
    }
    paused
}

/// A track read from a media session.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Track {
    pub session: MediaSession,
    /// Its artwork, already fitted ([`fit_jpeg`]); `None` while the session
    /// has none, or it couldn't be fitted.
    pub artwork: Option<Arc<[u8]>>,
}

/// One reading of the sessions: the track that counts, if any.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Reading {
    pub track: Option<Track>,
}

/// Reads the media sessions.
pub trait TrackReader {
    /// Reads every [`POLL_INTERVAL`] from now on, for `follow`, or stops
    /// reading (`None`). A reading made for an earlier follow is dropped.
    fn follow(&mut self, follow: Option<Follow>);

    /// The newest reading since the last call, if one was made.
    fn take_reading(&mut self) -> Option<Reading>;

    /// AudioSync's icon, fitted as artwork is; `None` until it is made, or
    /// when it can't be.
    fn icon(&mut self) -> Option<Arc<[u8]>>;
}

/// What the source looks like to track info.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TrackSource {
    pub follow: Follow,
    /// The source's name as the source list shows it, if there is one; the
    /// whole system's comes from the labels.
    pub name: Option<String>,
}

/// The artwork receivers should show.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Artwork {
    Track(Arc<[u8]>),
    /// AudioSync's icon: no track counts.
    Icon,
    /// The track has no artwork yet: none until [`ARTWORK_WAIT`] is over,
    /// then the icon.
    Wait,
}

/// What receivers should show.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Shown {
    pub fields: TrackFields,
    pub artwork: Artwork,
}

/// What receivers should show for `track`, the newest reading's, with
/// `source` captured. A paused track counts only when its app is the
/// source: with the whole system as the source, what plays may be another
/// app. A track's fields go out as its app gave them, blank artist or album
/// included.
pub fn decide(track: Option<&Track>, source: &TrackSource, labels: &ReceiverLabels) -> Shown {
    if let Some(track) = track
        && (track.session.playing || matches!(source.follow, Follow::App { .. }))
    {
        let session = &track.session;
        return Shown {
            fields: TrackFields {
                title: session.title.clone(),
                artist: session.artist.clone(),
                album: session.album.clone(),
            },
            artwork: track
                .artwork
                .as_ref()
                .map_or(Artwork::Wait, |jpeg| Artwork::Track(jpeg.clone())),
        };
    }
    let artist = source
        .name
        .as_deref()
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .unwrap_or(match source.follow {
            Follow::AnyApp => &labels.whole_system,
            Follow::App { .. } => &labels.unnamed_source,
        });
    Shown {
        fields: TrackFields {
            title: labels.title.clone(),
            artist: artist.to_owned(),
            album: labels.album.clone(),
        },
        artwork: Artwork::Icon,
    }
}

/// An image of `width` × `height` pixels as a JPEG of at most
/// [`ARTWORK_LIMIT`] bytes, as large and sharp as fits: the longest side up
/// to [`ARTWORK_SIDE`], scaled down until it fits. `encode` makes the JPEG
/// at a size and quality (0 to 1), or fails. `None` when not even the
/// smallest fits, or `encode` failed.
pub fn fit_jpeg(
    width: u32,
    height: u32,
    mut encode: impl FnMut(u32, u32, f32) -> Option<Vec<u8>>,
) -> Option<Vec<u8>> {
    if width == 0 || height == 0 {
        return None;
    }
    let longest = width.max(height);
    let mut side = longest.min(ARTWORK_SIDE);
    loop {
        let (scaled_width, scaled_height) =
            (scale(width, side, longest), scale(height, side, longest));
        let mut smallest = usize::MAX;
        for quality in JPEG_QUALITIES {
            let jpeg = encode(scaled_width, scaled_height, quality)?;
            if jpeg.len() <= ARTWORK_LIMIT {
                return Some(jpeg);
            }
            smallest = smallest.min(jpeg.len());
        }
        if side <= SMALLEST_SIDE {
            return None;
        }
        // The size grows about with the pixel count: aim at a side that
        // should fit, but always step down.
        let aim = f64::from(side) * (ARTWORK_LIMIT as f64 / smallest as f64).sqrt();
        side = (aim as u32).min(side * 85 / 100).max(SMALLEST_SIDE);
    }
}

fn scale(length: u32, side: u32, longest: u32) -> u32 {
    let scaled = u64::from(length) * u64::from(side) / u64::from(longest);
    u32::try_from(scaled).unwrap_or(u32::MAX).max(1)
}

/// Track info for the sender: follows the source while AirPlay receivers
/// play, and says what the sender should be given when that changes. See
/// the module documentation.
#[derive(Default)]
pub struct TrackInfo {
    reader: Option<Box<dyn TrackReader>>,
    /// What the reader follows; `None` while it doesn't read.
    following: Option<Follow>,
    /// When the reader started following.
    since: Option<Instant>,
    /// The newest reading since then, if one came.
    reading: Option<Reading>,
    /// The fields receivers should show, and since when.
    fields: Option<(TrackFields, Instant)>,
    /// What the sender was given last.
    sent: Option<NowPlaying>,
    labels: SharedLabels,
    log: DebugLog,
}

impl TrackInfo {
    /// Reads through `reader` from now on.
    pub fn set_reader(&mut self, reader: Box<dyn TrackReader>) {
        self.reader = Some(reader);
        self.following = None;
    }

    /// Sends what happens to `log`.
    pub fn set_debug_log(&mut self, log: DebugLog) {
        self.log = log;
    }

    /// Words what receivers show while no track counts.
    pub fn set_labels(&mut self, labels: SharedLabels) {
        self.labels = labels;
    }

    /// At each backend poll: `source` is what is captured while AirPlay
    /// receivers play, `None` while none do. Returns what the sender should
    /// be given, when it differs from what it was given last: nothing at
    /// all once the last receiver stops, and everything again when one
    /// plays again. `None` with no reader.
    pub fn sync(&mut self, source: Option<&TrackSource>, now: Instant) -> Option<NowPlaying> {
        let reader = self.reader.as_mut()?;
        let follow = source.map(|source| source.follow.clone());
        if follow != self.following {
            reader.follow(follow.clone());
            let stopped = follow.is_none();
            self.following = follow;
            self.since = Some(now);
            self.reading = None;
            if stopped {
                self.fields = None;
                if self.sent.take().is_some() {
                    self.log
                        .log(|| "AirPlay now playing: cleared, no receiver plays".to_owned());
                    return Some(NowPlaying::default());
                }
            }
        }
        let source = source?;
        let labels = self.labels.get()?;
        if let Some(reading) = reader.take_reading() {
            self.reading = Some(reading);
        }
        let since = self.since.unwrap_or(now);
        if self.reading.is_none() && now.saturating_duration_since(since) < READING_WAIT {
            return None;
        }
        let track = self
            .reading
            .as_ref()
            .and_then(|reading| reading.track.as_ref());
        let shown = decide(track, source, &labels);
        let fields_since = match &self.fields {
            Some((fields, since)) if *fields == shown.fields => *since,
            _ => {
                self.fields = Some((shown.fields.clone(), now));
                now
            }
        };
        let artwork = match shown.artwork {
            Artwork::Track(jpeg) => Some(jpeg),
            Artwork::Icon => reader.icon(),
            Artwork::Wait if now.saturating_duration_since(fields_since) >= ARTWORK_WAIT => {
                reader.icon()
            }
            Artwork::Wait => None,
        };
        let artwork = artwork.map(|jpeg| jpeg.to_vec());
        let next = NowPlaying {
            track: Some(shown.fields),
            artwork,
            progress: None,
        };
        if self.sent.as_ref() == Some(&next) {
            return None;
        }
        self.log.log(|| describe(&next, track));
        self.sent = Some(next.clone());
        Some(next)
    }
}

fn describe(now_playing: &NowPlaying, track: Option<&Track>) -> String {
    let fields = now_playing.track.as_ref();
    let field = |pick: fn(&TrackFields) -> &str| fields.map_or("", pick).to_owned();
    let from = track.map_or_else(
        || "AudioSync's labels".to_owned(),
        |track| format!("the session of {}", track.session.app_id),
    );
    let artwork = match &now_playing.artwork {
        Some(jpeg) => format!("{} bytes of artwork", jpeg.len()),
        None => "no artwork".to_owned(),
    };
    format!(
        "AirPlay now playing: \"{}\" by \"{}\" on \"{}\", from {from}, {artwork}",
        field(|fields| &fields.title),
        field(|fields| &fields.artist),
        field(|fields| &fields.album),
    )
}

#[cfg(test)]
pub mod fake {
    use std::cell::RefCell;
    use std::rc::Rc;
    use std::sync::Arc;

    use super::{Follow, MediaSession, Reading, ReceiverLabels, SharedLabels, Track, TrackReader};

    /// Labels unlike the UI's, so a test sees they come from the strings.
    pub fn labels() -> ReceiverLabels {
        ReceiverLabels {
            title: "Title label".to_owned(),
            album: "Album label".to_owned(),
            whole_system: "Whole system label".to_owned(),
            unnamed_source: "Unnamed label".to_owned(),
        }
    }

    pub fn shared_labels() -> SharedLabels {
        let shared = SharedLabels::default();
        shared.set(labels());
        shared
    }

    #[derive(Default)]
    struct FakeState {
        follow: Option<Follow>,
        follows: Vec<Option<Follow>>,
        reading: Option<Reading>,
        icon: Option<Arc<[u8]>>,
    }

    /// A reader whose readings a test makes.
    #[derive(Clone, Default)]
    pub struct FakeReader(Rc<RefCell<FakeState>>);

    impl FakeReader {
        pub fn with_icon(icon: &[u8]) -> FakeReader {
            let reader = FakeReader::default();
            reader.0.borrow_mut().icon = Some(icon.into());
            reader
        }

        pub fn reader(&self) -> Box<dyn TrackReader> {
            Box::new(self.clone())
        }

        /// A reading of `track`, or of no session.
        pub fn read(&self, track: Option<Track>) {
            self.0.borrow_mut().reading = Some(Reading { track });
        }

        pub fn following(&self) -> Option<Follow> {
            self.0.borrow().follow.clone()
        }

        /// Every follow change so far, and forgets them.
        pub fn take_follows(&self) -> Vec<Option<Follow>> {
            std::mem::take(&mut self.0.borrow_mut().follows)
        }
    }

    impl TrackReader for FakeReader {
        fn follow(&mut self, follow: Option<Follow>) {
            let mut state = self.0.borrow_mut();
            state.follow = follow.clone();
            state.follows.push(follow);
            state.reading = None;
        }

        fn take_reading(&mut self) -> Option<Reading> {
            self.0.borrow_mut().reading.take()
        }

        fn icon(&mut self) -> Option<Arc<[u8]>> {
            self.0.borrow().icon.clone()
        }
    }

    pub fn session(app_id: &str, title: &str, playing: bool) -> MediaSession {
        MediaSession {
            app_id: app_id.to_owned(),
            title: title.to_owned(),
            artist: format!("{title} artist"),
            album: format!("{title} album"),
            playing,
        }
    }

    pub fn track(app_id: &str, title: &str, playing: bool, artwork: Option<&[u8]>) -> Track {
        Track {
            session: session(app_id, title, playing),
            artwork: artwork.map(Into::into),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::fake::{FakeReader, labels, session, shared_labels, track};
    use super::*;

    const ICON: &[u8] = b"icon jpeg";
    const COVER: &[u8] = b"cover jpeg";

    fn whole_system() -> TrackSource {
        TrackSource {
            follow: Follow::AnyApp,
            name: None,
        }
    }

    fn app(executable: &str, name: &str) -> TrackSource {
        TrackSource {
            follow: Follow::App {
                executable: executable.to_owned(),
            },
            name: Some(name.to_owned()),
        }
    }

    fn fields(title: &str, artist: &str, album: &str) -> Option<TrackFields> {
        Some(TrackFields {
            title: title.to_owned(),
            artist: artist.to_owned(),
            album: album.to_owned(),
        })
    }

    fn track_info(reader: &FakeReader) -> TrackInfo {
        let mut info = TrackInfo::default();
        info.set_reader(reader.reader());
        info.set_labels(shared_labels());
        info
    }

    #[test]
    fn app_ids_match_their_executable_by_name_path_or_package_app() {
        assert!(app_matches("Spotify.exe", "spotify.exe"));
        assert!(app_matches("Chrome", "chrome.exe"));
        assert!(app_matches("MSEdge", "msedge.exe"));
        assert!(app_matches(
            "SpotifyAB.SpotifyMusic_zpdnekdrzrea0!Spotify",
            "Spotify.exe"
        ));
        assert!(app_matches(r"C:\Program Files\VLC\vlc.exe", "vlc.exe"));
        assert!(!app_matches("308046B0AF4A39CB", "firefox.exe"));
        assert!(!app_matches("Chrome", "chromium.exe"));
        assert!(!app_matches("Chrome", ""));
    }

    #[test]
    fn a_playing_session_wins_over_a_paused_one_listed_first() {
        let sessions = [
            session("Spotify.exe", "Paused", false),
            session("Chrome", "Playing", true),
        ];
        assert_eq!(pick(&sessions, &Follow::AnyApp), Some(1));
    }

    #[test]
    fn with_nothing_playing_the_first_paused_session_counts() {
        let sessions = [
            session("Spotify.exe", "First", false),
            session("Chrome", "Second", false),
        ];
        assert_eq!(pick(&sessions, &Follow::AnyApp), Some(0));
    }

    #[test]
    fn an_app_source_only_follows_its_own_session() {
        let sessions = [
            session("Chrome", "Video", true),
            session("Spotify.exe", "Song", false),
        ];
        let spotify = Follow::App {
            executable: "spotify.exe".to_owned(),
        };
        assert_eq!(pick(&sessions, &spotify), Some(1));
        let vlc = Follow::App {
            executable: "vlc.exe".to_owned(),
        };
        assert_eq!(pick(&sessions, &vlc), None);
    }

    #[test]
    fn sessions_without_a_title_never_count() {
        let sessions = [session("Chrome", " ", true)];
        assert_eq!(pick(&sessions, &Follow::AnyApp), None);
        assert_eq!(pick(&[], &Follow::AnyApp), None);
    }

    #[test]
    fn a_paused_track_counts_only_when_its_app_is_the_source() {
        let paused = track("Spotify.exe", "Song", false, Some(COVER));
        let shown = decide(Some(&paused), &whole_system(), &labels());
        assert_eq!(
            Some(shown.fields),
            fields("Title label", "Whole system label", "Album label")
        );
        assert_eq!(shown.artwork, Artwork::Icon);

        let shown = decide(Some(&paused), &app("spotify.exe", "Spotify"), &labels());
        assert_eq!(
            Some(shown.fields),
            fields("Song", "Song artist", "Song album")
        );
        assert_eq!(shown.artwork, Artwork::Track(COVER.into()));
    }

    #[test]
    fn with_no_track_the_source_name_stands_in_for_the_artist() {
        let shown = decide(None, &app("player.exe", "Player"), &labels());
        assert_eq!(
            Some(shown.fields),
            fields("Title label", "Player", "Album label")
        );
        // An input device that isn't listed, as the controller gives it.
        let unnamed = TrackSource {
            follow: Follow::App {
                executable: String::new(),
            },
            name: Some("  ".to_owned()),
        };
        assert_eq!(
            Some(decide(None, &unnamed, &labels()).fields),
            fields("Title label", "Unnamed label", "Album label")
        );
    }

    #[test]
    fn a_track_reaches_the_sender_with_its_artwork_once_read() {
        let reader = FakeReader::with_icon(ICON);
        let mut info = track_info(&reader);
        let start = Instant::now();
        assert_eq!(
            info.sync(Some(&whole_system()), start),
            None,
            "no reading yet"
        );
        assert_eq!(reader.following(), Some(Follow::AnyApp));

        reader.read(Some(track("Spotify.exe", "Song", true, Some(COVER))));
        let sent = info.sync(Some(&whole_system()), start).expect("it is sent");
        assert_eq!(sent.track, fields("Song", "Song artist", "Song album"));
        assert_eq!(sent.artwork.as_deref(), Some(COVER));
        assert_eq!(sent.progress, None);
    }

    #[test]
    fn an_unchanged_track_is_not_sent_again() {
        let reader = FakeReader::with_icon(ICON);
        let mut info = track_info(&reader);
        let start = Instant::now();
        info.sync(Some(&whole_system()), start);
        reader.read(Some(track("Spotify.exe", "Song", true, Some(COVER))));
        assert!(info.sync(Some(&whole_system()), start).is_some());

        reader.read(Some(track("Spotify.exe", "Song", true, Some(COVER))));
        let later = start + POLL_INTERVAL;
        assert_eq!(info.sync(Some(&whole_system()), later), None);
        assert_eq!(info.sync(Some(&whole_system()), later + ARTWORK_WAIT), None);
    }

    #[test]
    fn a_new_track_is_sent_as_it_changes() {
        let reader = FakeReader::with_icon(ICON);
        let mut info = track_info(&reader);
        let start = Instant::now();
        info.sync(Some(&whole_system()), start);
        reader.read(Some(track("Spotify.exe", "First", true, Some(COVER))));
        info.sync(Some(&whole_system()), start);

        reader.read(Some(track("Spotify.exe", "Second", true, Some(b"second"))));
        let sent = info
            .sync(Some(&whole_system()), start + POLL_INTERVAL)
            .expect("the new track is sent");
        assert_eq!(
            sent.track,
            fields("Second", "Second artist", "Second album")
        );
        assert_eq!(sent.artwork.as_deref(), Some(&b"second"[..]));
    }

    #[test]
    fn with_no_session_receivers_get_audiosyncs_labels_and_icon() {
        let reader = FakeReader::with_icon(ICON);
        let mut info = track_info(&reader);
        let start = Instant::now();
        info.sync(Some(&whole_system()), start);
        reader.read(None);
        let sent = info
            .sync(Some(&whole_system()), start)
            .expect("labels go out");
        assert_eq!(
            sent.track,
            fields("Title label", "Whole system label", "Album label")
        );
        assert_eq!(sent.artwork.as_deref(), Some(ICON));
    }

    #[test]
    fn with_no_reading_at_all_the_labels_go_out_after_the_reading_wait() {
        let reader = FakeReader::with_icon(ICON);
        let mut info = track_info(&reader);
        let start = Instant::now();
        assert_eq!(info.sync(Some(&whole_system()), start), None);
        let just_before = start + READING_WAIT - Duration::from_millis(1);
        assert_eq!(info.sync(Some(&whole_system()), just_before), None);
        let sent = info
            .sync(Some(&whole_system()), start + READING_WAIT)
            .expect("labels go out");
        assert_eq!(
            sent.track,
            fields("Title label", "Whole system label", "Album label")
        );
    }

    #[test]
    fn missing_artwork_waits_then_the_icon_takes_its_place() {
        let reader = FakeReader::with_icon(ICON);
        let mut info = track_info(&reader);
        let start = Instant::now();
        info.sync(Some(&whole_system()), start);
        reader.read(Some(track("Chrome", "Video", true, None)));
        let sent = info
            .sync(Some(&whole_system()), start)
            .expect("the fields go");
        assert_eq!(sent.track, fields("Video", "Video artist", "Video album"));
        assert_eq!(sent.artwork, None, "no artwork while it may still come");

        reader.read(Some(track("Chrome", "Video", true, None)));
        let waiting = start + ARTWORK_WAIT - Duration::from_millis(1);
        assert_eq!(info.sync(Some(&whole_system()), waiting), None);
        let sent = info
            .sync(Some(&whole_system()), start + ARTWORK_WAIT)
            .expect("the icon goes");
        assert_eq!(sent.artwork.as_deref(), Some(ICON));
    }

    #[test]
    fn artwork_that_arrives_during_the_wait_goes_out() {
        let reader = FakeReader::with_icon(ICON);
        let mut info = track_info(&reader);
        let start = Instant::now();
        info.sync(Some(&whole_system()), start);
        reader.read(Some(track("Chrome", "Video", true, None)));
        info.sync(Some(&whole_system()), start);
        reader.read(Some(track("Chrome", "Video", true, Some(COVER))));
        let sent = info
            .sync(Some(&whole_system()), start + POLL_INTERVAL)
            .expect("the cover goes");
        assert_eq!(sent.artwork.as_deref(), Some(COVER));
    }

    #[test]
    fn without_an_icon_no_artwork_goes_in_its_place() {
        let reader = FakeReader::default();
        let mut info = track_info(&reader);
        let start = Instant::now();
        info.sync(Some(&whole_system()), start);
        reader.read(None);
        let sent = info
            .sync(Some(&whole_system()), start)
            .expect("labels go out");
        assert_eq!(sent.artwork, None);
    }

    #[test]
    fn reading_follows_the_source_and_stops_with_the_receivers() {
        let reader = FakeReader::with_icon(ICON);
        let mut info = track_info(&reader);
        let start = Instant::now();
        assert_eq!(info.sync(None, start), None);
        assert!(reader.take_follows().is_empty(), "nothing to read yet");

        info.sync(Some(&whole_system()), start);
        info.sync(Some(&whole_system()), start);
        let spotify = app("spotify.exe", "Spotify");
        info.sync(Some(&spotify), start);
        info.sync(None, start);
        assert_eq!(
            reader.take_follows(),
            [Some(Follow::AnyApp), Some(spotify.follow.clone()), None]
        );
    }

    #[test]
    fn a_new_source_waits_for_its_own_reading() {
        let reader = FakeReader::with_icon(ICON);
        let mut info = track_info(&reader);
        let start = Instant::now();
        info.sync(Some(&whole_system()), start);
        reader.read(Some(track("Chrome", "Video", true, Some(COVER))));
        assert!(info.sync(Some(&whole_system()), start).is_some());

        let spotify = app("spotify.exe", "Spotify");
        let later = start + POLL_INTERVAL;
        assert_eq!(
            info.sync(Some(&spotify), later),
            None,
            "the old reading is dropped"
        );
        reader.read(Some(track("Spotify.exe", "Song", false, Some(COVER))));
        let sent = info
            .sync(Some(&spotify), later)
            .expect("Spotify's track goes");
        assert_eq!(sent.track, fields("Song", "Song artist", "Song album"));
    }

    #[test]
    fn the_sender_is_cleared_when_the_last_receiver_stops() {
        let reader = FakeReader::with_icon(ICON);
        let mut info = track_info(&reader);
        let start = Instant::now();
        info.sync(Some(&whole_system()), start);
        reader.read(Some(track("Spotify.exe", "Song", true, Some(COVER))));
        assert!(info.sync(Some(&whole_system()), start).is_some());

        assert_eq!(info.sync(None, start), Some(NowPlaying::default()));
        assert_eq!(info.sync(None, start), None, "cleared once");

        let later = start + POLL_INTERVAL;
        info.sync(Some(&whole_system()), later);
        reader.read(Some(track("Spotify.exe", "Song", true, Some(COVER))));
        let sent = info
            .sync(Some(&whole_system()), later)
            .expect("the same track goes again");
        assert_eq!(sent.track, fields("Song", "Song artist", "Song album"));
    }

    #[test]
    fn a_restart_on_a_track_without_artwork_waits_for_it_again() {
        let reader = FakeReader::with_icon(ICON);
        let mut info = track_info(&reader);
        let start = Instant::now();
        info.sync(Some(&whole_system()), start);
        reader.read(Some(track("Chrome", "Video", true, None)));
        info.sync(Some(&whole_system()), start);
        info.sync(None, start + ARTWORK_WAIT);

        let restart = start + ARTWORK_WAIT * 2;
        info.sync(Some(&whole_system()), restart);
        reader.read(Some(track("Chrome", "Video", true, None)));
        let sent = info
            .sync(Some(&whole_system()), restart)
            .expect("the fields go again");
        assert_eq!(sent.artwork, None, "the wait starts over");
        let sent = info
            .sync(Some(&whole_system()), restart + ARTWORK_WAIT)
            .expect("then the icon");
        assert_eq!(sent.artwork.as_deref(), Some(ICON));
    }

    #[test]
    fn no_reader_means_nothing_for_the_sender() {
        let mut info = TrackInfo::default();
        assert_eq!(info.sync(Some(&whole_system()), Instant::now()), None);
    }

    #[test]
    fn the_labels_come_from_the_ui_and_nothing_goes_out_before_them() {
        let reader = FakeReader::with_icon(b"icon");
        let mut info = TrackInfo::default();
        info.set_reader(reader.reader());
        let shared = SharedLabels::default();
        info.set_labels(shared.clone());
        let start = Instant::now();
        info.sync(Some(&whole_system()), start);
        reader.read(None);
        assert_eq!(info.sync(Some(&whole_system()), start), None);
        shared.set(labels());
        let sent = info.sync(Some(&whole_system()), start).expect("labels");
        assert_eq!(
            sent.track,
            fields("Title label", "Whole system label", "Album label")
        );
    }

    /// A fake JPEG encoder whose output grows with the pixel count and
    /// shrinks with the quality; records each try.
    fn encoder(tries: &mut Vec<(u32, u32, f32)>) -> impl FnMut(u32, u32, f32) -> Option<Vec<u8>> {
        move |width, height, quality| {
            tries.push((width, height, quality));
            let bytes = f64::from(width * height) * f64::from(quality) * 0.1;
            Some(vec![0; bytes as usize])
        }
    }

    #[test]
    fn small_artwork_fits_at_once_at_its_own_size() {
        let mut tries = Vec::new();
        let jpeg = fit_jpeg(200, 100, encoder(&mut tries)).expect("it fits");
        assert_eq!(tries, [(200, 100, 0.65)]);
        assert!(jpeg.len() <= ARTWORK_LIMIT);
    }

    #[test]
    fn large_artwork_is_capped_then_scaled_down_until_it_fits() {
        let mut tries = Vec::new();
        let jpeg = fit_jpeg(3000, 1500, encoder(&mut tries)).expect("it fits");
        assert!(jpeg.len() <= ARTWORK_LIMIT);
        assert_eq!(tries[0], (600, 300, 0.65), "the longest side starts at 600");
        assert_eq!(
            &tries[..3].iter().map(|t| t.2).collect::<Vec<_>>(),
            &JPEG_QUALITIES
        );
        let (width, height, _) = *tries.last().unwrap();
        assert!(width < 600 && width == height * 2, "smaller, same shape");
    }

    #[test]
    fn artwork_that_never_fits_or_fails_to_encode_is_none() {
        assert_eq!(
            fit_jpeg(500, 500, |_, _, _| Some(vec![0; ARTWORK_LIMIT + 1])),
            None
        );
        assert_eq!(fit_jpeg(500, 500, |_, _, _| None), None);
        assert_eq!(fit_jpeg(0, 500, |_, _, _| Some(Vec::new())), None);
    }
}
