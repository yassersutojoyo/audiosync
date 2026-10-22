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
//!   [`ARTWORK_SIDE`] pixels a side ([`fit_jpeg`]);
//! - a playing track whose session reports its length gets its progress
//!   ([`Timeline`]); a live stream, a paused track and AudioSync's labels
//!   get none. The receiver counts on by itself, so progress goes out again
//!   only when the track changes or the position jumps by more than
//!   [`PROGRESS_SLACK`], as a seek does.
//!
//! Reading never runs on the UI thread or an audio thread: a session that
//! hangs holds up the reader's thread only, and a failed reading counts as
//! no track.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use audiosync_airplay::metadata::TrackInfo as TrackFields;
use audiosync_airplay::sender::{NowPlaying, Progress};
use audiosync_cast::media::Metadata;

use super::cast::CastTrack;
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
/// How far a track's position may stray from where the progress sent last
/// puts it by now before progress goes out again.
pub const PROGRESS_SLACK: Duration = Duration::from_secs(1);
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
    /// Where the track stands; `None` when its length is unknown.
    pub timeline: Option<Timeline>,
}

/// A session's timeline as Windows reports it, all in 100 ns units: the
/// track's start and end, the position, and the time that position was
/// current, counted from 1601 in UTC (0 when the app gives none).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RawTimeline {
    pub start: i64,
    pub end: i64,
    pub position: i64,
    pub updated: i64,
}

/// Where a track stands: `elapsed` into it at `at`, and its length.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Timeline {
    pub elapsed: Duration,
    pub length: Duration,
    pub at: Instant,
    /// What it was read from, which tells a timeline left from the previous
    /// track apart from a new one.
    pub raw: RawTimeline,
}

const TICK_NANOS: u64 = 100;
/// 1601 to 1970, in 100 ns units.
const UNIX_EPOCH_TICKS: i64 = 116_444_736_000_000_000;

/// `time` in the units and from the epoch of [`RawTimeline::updated`].
pub fn wall_ticks(time: SystemTime) -> i64 {
    let since = |duration: Duration| i64::try_from(duration.as_nanos() / 100).unwrap_or(i64::MAX);
    match time.duration_since(UNIX_EPOCH) {
        Ok(after) => UNIX_EPOCH_TICKS.saturating_add(since(after)),
        Err(before) => UNIX_EPOCH_TICKS.saturating_sub(since(before.duration())),
    }
}

fn ticks(count: i64) -> Duration {
    Duration::from_nanos(u64::try_from(count).unwrap_or(0).saturating_mul(TICK_NANOS))
}

impl Timeline {
    /// `raw` as read at `read_at`, when the wall clock stood at `read_wall`
    /// ([`wall_ticks`]). `None` for a track without a length (a live
    /// stream), or a position that can't be placed in time. Without an
    /// update time the position counts as current when read.
    pub fn read(raw: RawTimeline, read_wall: i64, read_at: Instant) -> Option<Timeline> {
        let length = raw
            .end
            .checked_sub(raw.start)
            .filter(|length| *length > 0)?;
        let elapsed = raw.position.saturating_sub(raw.start).clamp(0, length);
        let age = if raw.updated > 0 {
            ticks(read_wall.saturating_sub(raw.updated))
        } else {
            Duration::ZERO
        };
        Some(Timeline {
            elapsed: ticks(elapsed),
            length: ticks(length),
            at: read_at.checked_sub(age)?,
            raw,
        })
    }

    /// The progress at `now` of a track that plays on; `None` once past its
    /// end, where a timeline the app stopped updating would put it.
    pub fn progress_at(&self, now: Instant) -> Option<Progress> {
        let elapsed = self.elapsed + now.saturating_duration_since(self.at);
        (elapsed <= self.length).then_some(Progress {
            elapsed,
            length: self.length,
        })
    }
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
    if let Some(track) = track.filter(|track| counts(track, &source.follow)) {
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

fn counts(track: &Track, follow: &Follow) -> bool {
    track.session.playing || matches!(follow, Follow::App { .. })
}

/// What a Cast device should show for `track`: its own fields and artwork
/// only, nothing in place of what it lacks (Cast spec C53), and nothing at
/// all when no track counts.
pub fn cast_track(track: Option<&Track>, follow: &Follow) -> CastTrack {
    let Some(track) = track.filter(|track| counts(track, follow)) else {
        return CastTrack::default();
    };
    let field = |text: &str| {
        let text = text.trim();
        (!text.is_empty()).then(|| text.to_owned())
    };
    let session = &track.session;
    CastTrack {
        metadata: Metadata {
            title: field(&session.title),
            artist: field(&session.artist),
            album_name: field(&session.album),
            images: Vec::new(),
        },
        artwork: track.artwork.clone(),
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
    /// What the sender was given last, and when.
    sent: Option<(NowPlaying, Instant)>,
    timelines: TimelineGuard,
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

    /// Whether a reader was set.
    pub fn reads(&self) -> bool {
        self.reader.is_some()
    }

    /// What a Cast device should show, from the reading [`TrackInfo::sync`]
    /// took last: `None` while nothing is followed, and right after reading
    /// starts until the first reading, for at most [`READING_WAIT`].
    pub fn cast_track(&self, now: Instant) -> Option<CastTrack> {
        let follow = self.following.as_ref()?;
        let since = self.since.unwrap_or(now);
        if self.reading.is_none() && now.saturating_duration_since(since) < READING_WAIT {
            return None;
        }
        let track = self
            .reading
            .as_ref()
            .and_then(|reading| reading.track.as_ref());
        Some(cast_track(track, follow))
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
            self.timelines = TimelineGuard::default();
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
        let progress = self.timelines.progress(track, &shown.fields, now);
        let next = NowPlaying {
            track: Some(shown.fields),
            artwork,
            progress,
        };
        if let Some((sent, at)) = &self.sent
            && sent.track == next.track
            && sent.artwork == next.artwork
            && progress_holds(sent.progress, *at, next.progress, now)
        {
            return None;
        }
        self.log.log(|| describe(&next, track));
        self.sent = Some((next.clone(), now));
        Some(next)
    }
}

/// Keeps progress from a timeline an app may still report for a moment
/// after the track changed, which is the previous track's.
#[derive(Default)]
struct TimelineGuard {
    /// The fields shown last, and the timeline read with them.
    timed: Option<(TrackFields, Option<RawTimeline>)>,
    /// The previous track's timeline, while the reading still has it.
    stale: Option<RawTimeline>,
}

impl TimelineGuard {
    /// The progress at `now` of `track`, shown as `fields`: only while it
    /// plays, and never from the timeline the previous track left.
    fn progress(
        &mut self,
        track: Option<&Track>,
        fields: &TrackFields,
        now: Instant,
    ) -> Option<Progress> {
        let timeline = track
            .filter(|track| track.session.playing)
            .and_then(|track| track.timeline);
        let raw = timeline.map(|timeline| timeline.raw);
        if self
            .timed
            .as_ref()
            .is_some_and(|(timed, _)| timed != fields)
        {
            self.stale = self.timed.take().and_then(|(_, raw)| raw);
        }
        if raw.is_some() && raw == self.stale {
            return None;
        }
        if raw.is_some() {
            self.stale = None;
        }
        self.timed = Some((fields.clone(), raw));
        timeline?.progress_at(now)
    }
}

/// Whether `sent` at `sent_at` still says what `next` says at `now`: both
/// none, or the same length with `sent` carried to `now` within
/// [`PROGRESS_SLACK`] of `next`.
fn progress_holds(
    sent: Option<Progress>,
    sent_at: Instant,
    next: Option<Progress>,
    now: Instant,
) -> bool {
    match (sent, next) {
        (None, None) => true,
        (Some(sent), Some(next)) => {
            let carried = sent.elapsed + now.saturating_duration_since(sent_at);
            sent.length == next.length && carried.abs_diff(next.elapsed) <= PROGRESS_SLACK
        }
        _ => false,
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
    let progress = match &now_playing.progress {
        Some(progress) => format!(
            "{:.1} s of {:.1} s",
            progress.elapsed.as_secs_f64(),
            progress.length.as_secs_f64()
        ),
        None => "no progress".to_owned(),
    };
    format!(
        "AirPlay now playing: \"{}\" by \"{}\" on \"{}\", from {from}, {artwork}, {progress}",
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
            timeline: None,
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
    fn a_cast_device_gets_the_track_and_its_cover_once_read() {
        let reader = FakeReader::with_icon(ICON);
        let mut info = track_info(&reader);
        let start = Instant::now();
        assert_eq!(info.cast_track(start), None, "nothing is followed");
        info.sync(Some(&whole_system()), start);
        assert_eq!(info.cast_track(start), None, "no reading yet");

        let mut song = track("Spotify.exe", "Song", true, Some(COVER));
        song.session.album = "  ".to_owned();
        reader.read(Some(song));
        info.sync(Some(&whole_system()), start);
        let shown = info.cast_track(start).expect("read");
        assert_eq!(shown.metadata.title.as_deref(), Some("Song"));
        assert_eq!(shown.metadata.artist.as_deref(), Some("Song artist"));
        assert_eq!(shown.metadata.album_name, None, "a blank field is left out");
        assert!(shown.metadata.images.is_empty());
        assert_eq!(shown.artwork.as_deref(), Some(COVER));

        info.sync(None, start);
        assert_eq!(info.cast_track(start), None, "reading stopped");
    }

    #[test]
    fn a_cast_device_gets_no_labels_or_icon_without_a_track() {
        let reader = FakeReader::with_icon(ICON);
        let mut info = track_info(&reader);
        let start = Instant::now();
        info.sync(Some(&whole_system()), start);
        reader.read(Some(track("Spotify.exe", "Paused", false, Some(COVER))));
        info.sync(Some(&whole_system()), start);
        assert_eq!(info.cast_track(start), Some(CastTrack::default()));

        let mut other = track_info(&FakeReader::with_icon(ICON));
        other.sync(Some(&whole_system()), start);
        let later = start + READING_WAIT;
        other.sync(Some(&whole_system()), later);
        assert_eq!(other.cast_track(later), Some(CastTrack::default()));
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

    const SECOND: i64 = 10_000_000;

    fn raw(start: i64, end: i64, position: i64, updated: i64) -> RawTimeline {
        RawTimeline {
            start: start * SECOND,
            end: end * SECOND,
            position: position * SECOND,
            updated: updated * SECOND,
        }
    }

    fn secs(count: u64) -> Duration {
        Duration::from_secs(count)
    }

    /// A track `elapsed` seconds into `length` at `at`.
    fn timed(title: &str, playing: bool, elapsed: i64, length: i64, at: Instant) -> Track {
        Track {
            timeline: Timeline::read(raw(0, length, elapsed, 0), 0, at),
            ..track("Spotify.exe", title, playing, Some(COVER))
        }
    }

    fn progress(elapsed: u64, length: u64) -> Option<Progress> {
        Some(Progress {
            elapsed: secs(elapsed),
            length: secs(length),
        })
    }

    #[test]
    fn a_timeline_gives_the_elapsed_time_and_length_as_of_its_update() {
        let read_at = Instant::now() + secs(60);
        let wall = 13_000_000_000;
        let timeline = Timeline::read(raw(10, 210, 60, wall - 3), wall * SECOND, read_at)
            .expect("the length is known");
        assert_eq!((timeline.elapsed, timeline.length), (secs(50), secs(200)));
        assert_eq!(timeline.at, read_at - secs(3));
        assert_eq!(timeline.progress_at(read_at), progress(53, 200));
        assert_eq!(
            timeline.progress_at(read_at + secs(147)),
            progress(200, 200)
        );
        assert_eq!(
            timeline.progress_at(read_at + secs(148)),
            None,
            "past the end the timeline is out of date"
        );
    }

    #[test]
    fn without_an_update_time_the_position_is_current_when_read() {
        let read_at = Instant::now();
        let timeline = Timeline::read(raw(0, 100, 30, 0), 13_000_000_000 * SECOND, read_at);
        assert_eq!(timeline.map(|timeline| timeline.at), Some(read_at));
        let ahead = Timeline::read(raw(0, 100, 30, 101), 100 * SECOND, read_at);
        assert_eq!(
            ahead.map(|timeline| timeline.at),
            Some(read_at),
            "an update time ahead of the clock counts as now"
        );
    }

    #[test]
    fn an_unknown_or_zero_length_gives_no_timeline() {
        let now = Instant::now();
        assert_eq!(Timeline::read(raw(0, 0, 0, 0), 0, now), None, "live");
        assert_eq!(Timeline::read(raw(5, 5, 5, 0), 0, now), None);
        assert_eq!(Timeline::read(raw(10, 5, 7, 0), 0, now), None);
    }

    #[test]
    fn a_position_outside_the_track_is_held_to_its_bounds() {
        let now = Instant::now();
        let before = Timeline::read(raw(10, 110, 4, 0), 0, now).unwrap();
        assert_eq!(before.elapsed, Duration::ZERO);
        let after = Timeline::read(raw(10, 110, 500, 0), 0, now).unwrap();
        assert_eq!(after.elapsed, secs(100));
    }

    #[test]
    fn wall_ticks_count_from_1601() {
        assert_eq!(wall_ticks(UNIX_EPOCH), UNIX_EPOCH_TICKS);
        assert_eq!(
            wall_ticks(UNIX_EPOCH + secs(2)),
            UNIX_EPOCH_TICKS + 2 * SECOND
        );
    }

    #[test]
    fn a_playing_track_with_a_length_goes_out_with_its_progress() {
        let reader = FakeReader::with_icon(ICON);
        let mut info = track_info(&reader);
        let start = Instant::now();
        info.sync(Some(&whole_system()), start);
        reader.read(Some(timed("Song", true, 30, 180, start)));
        let sent = info
            .sync(Some(&whole_system()), start + secs(1))
            .expect("it is sent");
        assert_eq!(sent.progress, progress(31, 180));
        assert_eq!(sent.track, fields("Song", "Song artist", "Song album"));
    }

    #[test]
    fn progress_that_runs_on_as_sent_is_not_sent_again() {
        let reader = FakeReader::with_icon(ICON);
        let mut info = track_info(&reader);
        let start = Instant::now();
        info.sync(Some(&whole_system()), start);
        reader.read(Some(timed("Song", true, 30, 180, start)));
        assert!(info.sync(Some(&whole_system()), start).is_some());

        let later = start + POLL_INTERVAL;
        reader.read(Some(timed("Song", true, 32, 180, later)));
        assert_eq!(info.sync(Some(&whole_system()), later), None);
        let drifted = later + POLL_INTERVAL;
        reader.read(Some(timed("Song", true, 35, 180, drifted)));
        assert_eq!(
            info.sync(Some(&whole_system()), drifted),
            None,
            "a second off is within the slack"
        );
    }

    #[test]
    fn a_seek_sends_the_new_position() {
        let reader = FakeReader::with_icon(ICON);
        let mut info = track_info(&reader);
        let start = Instant::now();
        info.sync(Some(&whole_system()), start);
        reader.read(Some(timed("Song", true, 30, 180, start)));
        info.sync(Some(&whole_system()), start);

        let later = start + POLL_INTERVAL;
        reader.read(Some(timed("Song", true, 120, 180, later)));
        let sent = info
            .sync(Some(&whole_system()), later)
            .expect("the seek is sent");
        assert_eq!(sent.progress, progress(120, 180));
    }

    #[test]
    fn a_live_or_paused_track_or_the_labels_go_out_without_progress() {
        let reader = FakeReader::with_icon(ICON);
        let mut info = track_info(&reader);
        let start = Instant::now();
        let source = app("Spotify.exe", "Spotify");
        info.sync(Some(&source), start);
        reader.read(Some(track("Spotify.exe", "Live", true, Some(COVER))));
        let live = info.sync(Some(&source), start).expect("it is sent");
        assert_eq!(live.progress, None);

        reader.read(Some(timed("Song", false, 30, 180, start)));
        let paused = info.sync(Some(&source), start).expect("it is sent");
        assert_eq!(paused.progress, None);

        reader.read(Some(timed("Song", true, 30, 180, start)));
        let resumed = info.sync(Some(&source), start).expect("it is sent");
        assert_eq!(resumed.progress, progress(30, 180));

        reader.read(None);
        let labels = info.sync(Some(&source), start).expect("it is sent");
        assert_eq!(labels.progress, None);
    }

    #[test]
    fn a_track_change_drops_the_previous_tracks_timeline() {
        let reader = FakeReader::with_icon(ICON);
        let mut info = track_info(&reader);
        let start = Instant::now();
        info.sync(Some(&whole_system()), start);
        let first = timed("First", true, 170, 180, start);
        let left = first.timeline;
        reader.read(Some(first));
        info.sync(Some(&whole_system()), start);

        let later = start + POLL_INTERVAL;
        reader.read(Some(Track {
            timeline: left,
            ..track("Spotify.exe", "Second", true, Some(COVER))
        }));
        let sent = info
            .sync(Some(&whole_system()), later)
            .expect("the new track is sent");
        assert_eq!(
            sent.track,
            fields("Second", "Second artist", "Second album")
        );
        assert_eq!(
            sent.progress, None,
            "the old timeline isn't the new track's"
        );

        let next = later + POLL_INTERVAL;
        reader.read(Some(timed("Second", true, 2, 240, next)));
        let sent = info
            .sync(Some(&whole_system()), next)
            .expect("its own timeline is sent");
        assert_eq!(sent.progress, progress(2, 240));
    }

    #[test]
    fn stopping_clears_the_progress_with_the_rest() {
        let reader = FakeReader::with_icon(ICON);
        let mut info = track_info(&reader);
        let start = Instant::now();
        info.sync(Some(&whole_system()), start);
        reader.read(Some(timed("Song", true, 30, 180, start)));
        info.sync(Some(&whole_system()), start);
        assert_eq!(info.sync(None, start), Some(NowPlaying::default()));
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
