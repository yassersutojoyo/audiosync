//! The system's media sessions, read on one thread for both the receivers'
//! track info ([`MediaSessionReader`]) and the Now Playing card
//! ([`MediaCardReader`]). What lists and drives the sessions is a
//! [`SessionList`]: Windows' system media transport controls
//! (`wasapi::read_media_sessions`) or the MPRIS players on Linux
//! (`mpris::read_media_sessions`).
//!
//! The thread sleeps while neither follows a source, then reads every
//! [`POLL_INTERVAL`](crate::audio::now_playing::POLL_INTERVAL): it lists the sessions once, picks the one that counts
//! for each reader ([`SessionList::pick`]) and reads a picked session's
//! thumbnail once. Every wait on an app is bounded by [`APP_WAIT`], and a
//! thumbnail that missed it isn't asked for again for [`MISS_RETRY`]. The
//! receivers' reading goes out before any work only the card needs. Artwork
//! is made from the thumbnail's bytes once per track, and the receivers'
//! track carries its session's timeline. A card button goes to the session
//! that counts when the thread takes it, and the sessions are read again
//! [`AFTER_COMMAND`] later, so the card shows the new state without waiting
//! for the next poll. Nothing else waits on the thread: the readers meet it
//! only under a lock held for a copy. Errors are logged once until reading
//! works again, and count as no track.

use std::collections::HashMap;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use crate::audio::now_playing::{
    FittedArtwork, Follow, MediaSession, RawTimeline, Reading, Timeline, Track, TrackReader, pick,
    wall_ticks,
};
use crate::debug_log::DebugLog;
use crate::now_playing_card::{
    ArtworkCache, CardArtwork, CardSource, CardTrack, MediaControls, Transport,
};

/// How soon the sessions are read again after a button: long enough for
/// most apps to have changed their state.
const AFTER_COMMAND: Duration = Duration::from_millis(400);

/// How long a listing waits on apps for their sessions' properties, a
/// reading for a thumbnail, and a button for the app to take it. Past it the
/// session is left out of that listing, or its track shows without artwork.
pub const APP_WAIT: Duration = Duration::from_secs(1);

/// How long a track's thumbnail that couldn't be read in time isn't asked
/// for again.
const MISS_RETRY: Duration = Duration::from_secs(30);

/// The largest thumbnail read; an app's is a few hundred kilobytes at most.
pub const THUMBNAIL_LIMIT: u64 = 16 << 20;

/// Artwork kept for at most this many tracks.
const ARTWORK_CACHE: usize = 32;

/// Where the thread reads sessions from, and how it makes their artwork:
/// the system's, or a fake in the tests. Errors are text for the log.
pub trait SessionList {
    type Session;

    /// Every session listed now, described. One app's session that fails to
    /// answer within [`APP_WAIT`] is left out.
    fn list(&mut self) -> Result<Vec<(MediaSession, Self::Session)>, String>;

    /// The listed session whose track counts for `follow`. A system that
    /// tells an app's sessions apart better than by their app IDs replaces
    /// this.
    fn pick(&self, listed: &[(MediaSession, Self::Session)], follow: &Follow) -> Option<usize> {
        let described: Vec<MediaSession> =
            listed.iter().map(|(session, _)| session.clone()).collect();
        pick(&described, follow)
    }

    /// The session's thumbnail as an encoded image, read by `deadline`;
    /// `None` while it has none.
    fn thumbnail(
        &self,
        session: &Self::Session,
        deadline: Instant,
    ) -> Result<Option<Vec<u8>>, String>;

    /// The session's timeline as it reports it now.
    fn timeline(&self, session: &Self::Session) -> Result<RawTimeline, String>;

    /// Sends `command` to the session and waits for the app until
    /// `deadline`: whether it took it.
    fn send(
        &self,
        session: &Self::Session,
        command: Transport,
        deadline: Instant,
    ) -> Result<bool, String>;

    /// Forgets what a failed listing may have left broken.
    fn reset(&mut self);

    /// An encoded image as fitted receiver artwork
    /// ([`crate::audio::now_playing::fit_artwork`]); `None` when not even
    /// the smallest size fits.
    fn fit_image(&self, image: &[u8]) -> Result<Option<FittedArtwork>, String>;

    /// An encoded image as the card's tile; `None` for an empty image.
    fn card_artwork(&self, image: &[u8]) -> Result<Option<CardArtwork>, String>;

    /// AudioSync's icon fitted as receiver artwork; `None`, logged, when it
    /// can't be.
    fn icon(&self, log: &DebugLog) -> Option<FittedArtwork>;
}

#[derive(Default)]
struct State {
    track: Option<Follow>,
    /// Counts follow changes, so a reading made for an earlier one is
    /// dropped.
    track_generation: u64,
    track_reading: Option<Reading>,
    /// The app that played last, as track info tells it.
    track_played_last: Option<String>,
    icon: Option<FittedArtwork>,
    card: Option<CardSource>,
    card_generation: u64,
    card_reading: Option<Option<CardTrack>>,
    commands: Vec<Transport>,
    /// Controls a speaker's remote asked for, sent to the session that
    /// counts for `track`.
    remote: Vec<Transport>,
    /// The readers not dropped yet.
    readers: u8,
}

#[derive(Default)]
struct Shared {
    state: Mutex<State>,
    wake: Condvar,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// The receivers' track info, read on the shared thread.
pub struct MediaSessionReader {
    shared: Arc<Shared>,
}

impl TrackReader for MediaSessionReader {
    fn follow(&mut self, follow: Option<Follow>) {
        let mut state = self.shared.lock();
        if state.track != follow {
            state.track = follow;
            state.track_generation += 1;
            state.track_reading = None;
            state.track_played_last = None;
            state.remote.clear();
            self.shared.wake.notify_all();
        }
    }

    fn take_reading(&mut self) -> Option<Reading> {
        self.shared.lock().track_reading.take()
    }

    fn icon(&mut self) -> Option<FittedArtwork> {
        self.shared.lock().icon.clone()
    }

    fn send(&mut self, command: Transport) {
        let mut state = self.shared.lock();
        if state.track.is_some() {
            state.remote.push(command);
            self.shared.wake.notify_all();
        }
    }

    fn played_last(&mut self, app_id: Option<&str>) {
        self.shared.lock().track_played_last = app_id.map(str::to_owned);
    }
}

impl Drop for MediaSessionReader {
    // Not joined: the thread may be waiting on an app's session.
    fn drop(&mut self) {
        let mut state = self.shared.lock();
        state.track = None;
        state.track_generation += 1;
        state.readers -= 1;
        self.shared.wake.notify_all();
    }
}

/// The Now Playing card's sessions, read and driven on the shared thread.
pub struct MediaCardReader {
    shared: Arc<Shared>,
}

impl MediaControls for MediaCardReader {
    fn follow(&mut self, source: Option<CardSource>) {
        let mut state = self.shared.lock();
        if state.card != source {
            state.card = source;
            state.card_generation += 1;
            state.card_reading = None;
            state.commands.clear();
            self.shared.wake.notify_all();
        }
    }

    fn take_reading(&mut self) -> Option<Option<CardTrack>> {
        self.shared.lock().card_reading.take()
    }

    fn send(&mut self, command: Transport) {
        self.shared.lock().commands.push(command);
        self.shared.wake.notify_all();
    }
}

impl Drop for MediaCardReader {
    fn drop(&mut self) {
        let mut state = self.shared.lock();
        state.card = None;
        state.card_generation += 1;
        state.commands.clear();
        state.readers -= 1;
        self.shared.wake.notify_all();
    }
}

/// Starts the thread that reads the sessions `make` opens on it, for both
/// readers. It reads nothing until one follows a source and ends once both
/// are dropped. A thread that can't start, or whose sessions can't be
/// opened, is logged, and neither reader ever reads.
pub fn spawn<L, M>(make: M, poll: Duration, log: DebugLog) -> (MediaSessionReader, MediaCardReader)
where
    L: SessionList,
    M: FnOnce() -> Result<L, String> + Send + 'static,
{
    let shared = Arc::new(Shared::default());
    shared.lock().readers = 2;
    let thread_shared = shared.clone();
    let thread_log = log.clone();
    let spawned = thread::Builder::new()
        .name("audiosync-now-playing".to_owned())
        .spawn(move || run(&thread_shared, make, poll, &thread_log));
    if let Err(error) = spawned {
        log.log(|| format!("now playing: the reading thread couldn't start: {error}"));
    }
    (
        MediaSessionReader {
            shared: shared.clone(),
        },
        MediaCardReader { shared },
    )
}

fn run<L: SessionList>(
    shared: &Shared,
    make: impl FnOnce() -> Result<L, String>,
    poll: Duration,
    log: &DebugLog,
) {
    let sessions = match make() {
        Ok(sessions) => sessions,
        Err(error) => {
            log.log(|| format!("now playing: {error}"));
            return;
        }
    };
    shared.lock().icon = sessions.icon(log);
    let mut reader = Reader::new(sessions);
    loop {
        let (track, track_generation, played_last, card, card_generation, commands, remote) = {
            let mut state = shared
                .wake
                .wait_while(shared.lock(), |state| {
                    state.track.is_none() && state.card.is_none() && state.readers > 0
                })
                .unwrap_or_else(PoisonError::into_inner);
            if state.readers == 0 {
                return;
            }
            let commands = std::mem::take(&mut state.commands);
            let remote = std::mem::take(&mut state.remote);
            (
                state.track.clone(),
                state.track_generation,
                state.track_played_last.clone(),
                state.card.clone().map(follow_for),
                state.card_generation,
                commands,
                remote,
            )
        };
        // The host logged the command already (S279), so the send stays quiet.
        if let Some(track) = &track {
            for command in &remote {
                reader.send(
                    track,
                    played_last.as_deref(),
                    *command,
                    &DebugLog::default(),
                );
            }
        }
        if let Some(card) = &card {
            for command in &commands {
                reader.send(card, None, *command, log);
            }
        }
        let mut listing = reader.listing(log);
        // The receivers' reading goes out first, so a thumbnail only the
        // card needs never holds it back.
        if let Some(follow) = &track {
            let track = reader.track(&mut listing, follow, played_last.as_deref(), log);
            let mut state = shared.lock();
            if state.track_generation == track_generation {
                state.track_reading = Some(Reading { track });
            }
        }
        if let Some(follow) = &card {
            let card = reader.card(&mut listing, follow, log);
            let mut state = shared.lock();
            if state.card_generation == card_generation {
                state.card_reading = Some(card);
            }
        }
        let state = shared.lock();
        let sent =
            (card.is_some() && !commands.is_empty()) || (track.is_some() && !remote.is_empty());
        let wait = if sent { AFTER_COMMAND } else { poll };
        let _ = shared
            .wake
            .wait_timeout_while(state, wait, |state| {
                state.track_generation == track_generation
                    && state.card_generation == card_generation
                    && state.commands.is_empty()
                    && state.remote.is_empty()
                    && state.readers > 0
            })
            .unwrap_or_else(PoisonError::into_inner);
    }
}

fn follow_for(source: CardSource) -> Follow {
    match source {
        CardSource::AnyApp => Follow::AnyApp,
        CardSource::App { executable, pid } => Follow::App { executable, pid },
    }
}

/// The reading thread's own state.
struct Reader<L> {
    sessions: L,
    /// Receiver artwork by track; `None` for a thumbnail that couldn't be
    /// fitted.
    jpegs: HashMap<String, Option<FittedArtwork>>,
    /// Card artwork by track.
    tiles: ArtworkCache,
    /// When each track's thumbnail last failed to be read in time. It is
    /// not asked again for [`MISS_RETRY`], so an app that hangs costs the
    /// thread one wait per track, not one per poll.
    misses: HashMap<String, Instant>,
    /// The last listing failed, and said so.
    failing: bool,
    /// The last tracks read, for the log.
    last_track: Option<MediaSession>,
    last_card: Option<MediaSession>,
}

/// One listing of the sessions, which both readers' readings come from.
struct Listing<S> {
    listed: Vec<(MediaSession, S)>,
    /// Thumbnails read for this listing, by session index.
    thumbnails: Vec<(usize, Option<Vec<u8>>)>,
}

impl<L: SessionList> Reader<L> {
    fn new(sessions: L) -> Reader<L> {
        Reader {
            sessions,
            jpegs: HashMap::new(),
            tiles: ArtworkCache::default(),
            misses: HashMap::new(),
            failing: false,
            last_track: None,
            last_card: None,
        }
    }

    /// The sessions listed now; none when listing fails.
    fn listing(&mut self, log: &DebugLog) -> Listing<L::Session> {
        Listing {
            listed: self.listed(log),
            thumbnails: Vec::new(),
        }
    }

    /// The receivers' track for `follow`, or `None` when no session counts.
    fn track(
        &mut self,
        listing: &mut Listing<L::Session>,
        follow: &Follow,
        played_last: Option<&str>,
        log: &DebugLog,
    ) -> Option<Track> {
        let index = self.counting(&listing.listed, follow, played_last);
        self.log_track(index.map(|index| &listing.listed[index].0), log);
        let index = index?;
        let session = listing.listed[index].0.clone();
        // Read at each poll rather than on a change notice, which needs a
        // handler on every session as they come and go: track info passes
        // on only a jump, so a poll that finds none sends nothing.
        let timeline = self
            .sessions
            .timeline(&listing.listed[index].1)
            .ok()
            .and_then(|raw| Timeline::read(raw, wall_ticks(SystemTime::now()), Instant::now()));
        let key = track_key(&session);
        let artwork = match self.jpegs.get(&key) {
            Some(made) => made.clone(),
            None => self
                .thumbnail(listing, index, &key, log)
                .and_then(|image| self.jpeg(key, &session, &image, log)),
        };
        Some(Track {
            session,
            artwork,
            timeline,
        })
    }

    /// The listed session that counts for `follow`, with `played_last` the
    /// app that played last.
    fn counting(
        &self,
        listed: &[(MediaSession, L::Session)],
        follow: &Follow,
        played_last: Option<&str>,
    ) -> Option<usize> {
        let index = self.sessions.pick(listed, follow);
        // Another app's paused track never takes its place, as on the Mac.
        if let (Follow::AnyApp, Some(app_id)) = (follow, played_last)
            && !index.is_some_and(|index| listed[index].0.playing)
        {
            return listed.iter().position(|(session, _)| {
                session.app_id == app_id && !session.title.trim().is_empty()
            });
        }
        index
    }

    /// The card's track for `follow`, or `None` when no session counts.
    fn card(
        &mut self,
        listing: &mut Listing<L::Session>,
        follow: &Follow,
        log: &DebugLog,
    ) -> Option<CardTrack> {
        let index = self.sessions.pick(&listing.listed, follow);
        self.log_card(index.map(|index| &listing.listed[index].0), log);
        let index = index?;
        let session = listing.listed[index].0.clone();
        let key = track_key(&session);
        let artwork = match self.tiles.get(&key) {
            Some(made) => made,
            None => self.thumbnail(listing, index, &key, log).and_then(|image| {
                let made = self.sessions.card_artwork(&image);
                let (tile, error) = self.tiles.keep(&key, made);
                if let Some(error) = error {
                    log.log(|| {
                        format!(
                            "now playing card: the artwork of \"{}\" couldn't be decoded: {error}",
                            session.title
                        )
                    });
                }
                tile
            }),
        };
        Some(CardTrack {
            app_id: session.app_id,
            title: session.title,
            artist: session.artist,
            playing: session.playing,
            artwork,
        })
    }

    /// Sends `command` to the session that counts for `follow` now.
    fn send(
        &mut self,
        follow: &Follow,
        played_last: Option<&str>,
        command: Transport,
        log: &DebugLog,
    ) {
        let sent = self.sessions.list().and_then(|listed| {
            let Some(index) = self.counting(&listed, follow, played_last) else {
                return Ok(None);
            };
            let (session, entry) = &listed[index];
            let taken = self
                .sessions
                .send(entry, command, Instant::now() + APP_WAIT)?;
            Ok(Some((session.app_id.clone(), taken)))
        });
        log.log(|| match &sent {
            Ok(Some((app_id, true))) => format!("now playing card: {command:?} sent to {app_id}"),
            Ok(Some((app_id, false))) => {
                format!("now playing card: {app_id} turned down {command:?}")
            }
            Ok(None) => format!("now playing card: {command:?} found no session that counts"),
            Err(error) => format!("now playing card: {command:?} couldn't be sent: {error}"),
        });
    }

    fn listed(&mut self, log: &DebugLog) -> Vec<(MediaSession, L::Session)> {
        match self.sessions.list() {
            Ok(listed) => {
                if self.failing {
                    self.failing = false;
                    log.log(|| "now playing: reading the media sessions works again".to_owned());
                }
                listed
            }
            Err(error) => {
                if !self.failing {
                    self.failing = true;
                    log.log(|| format!("now playing: reading the media sessions failed: {error}"));
                }
                self.sessions.reset();
                Vec::new()
            }
        }
    }

    fn log_track(&mut self, session: Option<&MediaSession>, log: &DebugLog) {
        if session == self.last_track.as_ref() {
            return;
        }
        self.last_track = session.cloned();
        log.log(|| match session {
            Some(session) => format!(
                "now playing: read \"{}\" by \"{}\" from {} ({})",
                session.title,
                session.artist,
                session.app_id,
                if session.playing { "playing" } else { "paused" }
            ),
            None => "now playing: no media session counts".to_owned(),
        });
    }

    fn log_card(&mut self, session: Option<&MediaSession>, log: &DebugLog) {
        if session == self.last_card.as_ref() {
            return;
        }
        self.last_card = session.cloned();
        log.log(|| match session {
            Some(session) => format!(
                "now playing card: shows \"{}\" from {} ({})",
                session.title,
                session.app_id,
                if session.playing { "playing" } else { "paused" }
            ),
            None => "now playing card: no media session counts".to_owned(),
        });
    }

    /// The thumbnail of the listing's session at `index`, read at most once
    /// per listing and within [`APP_WAIT`]; `None` while it has none, or
    /// while a failed read waits out [`MISS_RETRY`]. A session without a
    /// thumbnail may get one a moment later, so that is asked again.
    fn thumbnail(
        &mut self,
        listing: &mut Listing<L::Session>,
        index: usize,
        key: &str,
        log: &DebugLog,
    ) -> Option<Vec<u8>> {
        if let Some((_, image)) = listing.thumbnails.iter().find(|(read, _)| *read == index) {
            return image.clone();
        }
        if self
            .misses
            .get(key)
            .is_some_and(|missed| missed.elapsed() < MISS_RETRY)
        {
            return None;
        }
        let (session, entry) = &listing.listed[index];
        let read = self.sessions.thumbnail(entry, Instant::now() + APP_WAIT);
        let image = match read {
            Ok(image) => {
                self.misses.remove(key);
                image
            }
            Err(error) => {
                if self.misses.len() >= ARTWORK_CACHE && !self.misses.contains_key(key) {
                    self.misses.clear();
                }
                if self.misses.insert(key.to_owned(), Instant::now()).is_none() {
                    log.log(|| {
                        format!(
                            "now playing: the artwork of \"{}\" couldn't be read: {error}",
                            session.title
                        )
                    });
                }
                None
            }
        };
        listing.thumbnails.push((index, image.clone()));
        image
    }

    /// Receiver artwork fitted from `image`, kept for the track.
    fn jpeg(
        &mut self,
        key: String,
        session: &MediaSession,
        image: &[u8],
        log: &DebugLog,
    ) -> Option<FittedArtwork> {
        let artwork = match self.sessions.fit_image(image) {
            Ok(Some(artwork)) => Some(artwork),
            Ok(None) => {
                log.log(|| {
                    format!(
                        "now playing: the artwork of \"{}\" doesn't fit",
                        session.title
                    )
                });
                None
            }
            Err(error) => {
                log.log(|| {
                    format!(
                        "now playing: the artwork of \"{}\" couldn't be decoded: {error}",
                        session.title
                    )
                });
                None
            }
        };
        if self.jpegs.len() >= ARTWORK_CACHE {
            self.jpegs.clear();
        }
        self.jpegs.insert(key, artwork.clone());
        artwork
    }
}

fn track_key(session: &MediaSession) -> String {
    format!("{}|{}|{}", session.app_id, session.title, session.album)
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    use super::*;

    #[derive(Clone, Copy)]
    enum Thumbnail {
        Image,
        /// Never read in time.
        Hung,
    }

    #[derive(Default)]
    struct Counts {
        lists: AtomicUsize,
        thumbnails: AtomicUsize,
        sends: AtomicUsize,
        /// The app IDs of the sessions sent to.
        sent_to: Mutex<Vec<String>>,
        ended: AtomicBool,
    }

    impl Counts {
        fn lists(&self) -> usize {
            self.lists.load(Ordering::SeqCst)
        }

        fn thumbnails(&self) -> usize {
            self.thumbnails.load(Ordering::SeqCst)
        }
    }

    struct Fake {
        thumbnail: Thumbnail,
        sessions: Vec<MediaSession>,
        counts: Arc<Counts>,
    }

    impl Drop for Fake {
        fn drop(&mut self) {
            self.counts.ended.store(true, Ordering::SeqCst);
        }
    }

    const IMAGE: &[u8] = b"image";
    const JPEG: &[u8] = b"jpeg";

    impl SessionList for Fake {
        type Session = String;

        fn list(&mut self) -> Result<Vec<(MediaSession, String)>, String> {
            self.counts.lists.fetch_add(1, Ordering::SeqCst);
            Ok(self
                .sessions
                .iter()
                .map(|s| (s.clone(), s.app_id.clone()))
                .collect())
        }

        fn thumbnail(&self, _: &String, deadline: Instant) -> Result<Option<Vec<u8>>, String> {
            self.counts.thumbnails.fetch_add(1, Ordering::SeqCst);
            match self.thumbnail {
                Thumbnail::Image => Ok(Some(IMAGE.to_vec())),
                Thumbnail::Hung => {
                    thread::sleep(deadline.saturating_duration_since(Instant::now()));
                    Err("the app didn't answer in time".to_owned())
                }
            }
        }

        fn timeline(&self, _: &String) -> Result<RawTimeline, String> {
            Ok(TIMELINE)
        }

        fn send(&self, app_id: &String, _: Transport, _: Instant) -> Result<bool, String> {
            self.counts.sends.fetch_add(1, Ordering::SeqCst);
            self.counts.sent_to.lock().unwrap().push(app_id.clone());
            Ok(true)
        }

        fn reset(&mut self) {}

        fn fit_image(&self, image: &[u8]) -> Result<Option<FittedArtwork>, String> {
            assert_eq!(image, IMAGE);
            Ok(Some(FittedArtwork::single(JPEG.into())))
        }

        fn card_artwork(&self, image: &[u8]) -> Result<Option<CardArtwork>, String> {
            assert_eq!(image, IMAGE);
            Ok(Some(CardArtwork {
                side: 1,
                rgba: Arc::from(&[0u8, 0, 0, 255][..]),
            }))
        }

        fn icon(&self, _: &DebugLog) -> Option<FittedArtwork> {
            Some(FittedArtwork::single(JPEG.into()))
        }
    }

    /// 50 s into a 200 s track that starts at 10 s, with no update time.
    const TIMELINE: RawTimeline = RawTimeline {
        start: 100_000_000,
        end: 2_100_000_000,
        position: 600_000_000,
        updated: 0,
    };

    fn playing() -> MediaSession {
        MediaSession {
            app_id: "Spotify.exe".to_owned(),
            title: "Song".to_owned(),
            artist: "Artist".to_owned(),
            album: "Album".to_owned(),
            playing: true,
            paused: false,
        }
    }

    fn fake(thumbnail: Thumbnail) -> (Fake, Arc<Counts>) {
        let counts = Arc::new(Counts::default());
        let fake = Fake {
            thumbnail,
            sessions: vec![playing()],
            counts: counts.clone(),
        };
        (fake, counts)
    }

    /// One poll for both readers, as the thread makes it.
    fn poll(reader: &mut Reader<Fake>) -> (Option<Track>, Option<CardTrack>) {
        let log = DebugLog::default();
        let mut listing = reader.listing(&log);
        let track = reader.track(&mut listing, &Follow::AnyApp, None, &log);
        let card = reader.card(&mut listing, &Follow::AnyApp, &log);
        (track, card)
    }

    // covers: S218
    #[test]
    fn one_listing_and_one_thumbnail_serve_both_readers() {
        let (fake, counts) = fake(Thumbnail::Image);
        let mut reader = Reader::new(fake);
        let (track, card) = poll(&mut reader);
        assert_eq!(counts.lists(), 1, "one listing serves both readers");
        assert_eq!(counts.thumbnails(), 1, "one thumbnail read serves both");
        let track = track.expect("the receivers get the track");
        assert_eq!(track.session, playing());
        assert_eq!(track.artwork, Some(FittedArtwork::single(JPEG.into())));
        let timeline = track.timeline.expect("the track carries its timeline");
        assert_eq!(
            (timeline.elapsed, timeline.length, timeline.raw),
            (Duration::from_secs(50), Duration::from_secs(200), TIMELINE)
        );
        let card = card.expect("the card gets the track");
        assert_eq!((card.title.as_str(), card.playing), ("Song", true));
        assert!(card.artwork.is_some());
        let _ = poll(&mut reader);
        assert_eq!(counts.lists(), 2);
        assert_eq!(counts.thumbnails(), 1, "artwork is made once per track");
    }

    #[test]
    fn a_hung_thumbnail_leaves_the_track_without_artwork_in_time() {
        let (fake, counts) = fake(Thumbnail::Hung);
        let mut reader = Reader::new(fake);
        let started = Instant::now();
        let (track, card) = poll(&mut reader);
        let took = started.elapsed();
        assert!(took >= APP_WAIT && took < APP_WAIT * 2, "took {took:?}");
        assert_eq!(counts.thumbnails(), 1, "one wait covers both readers");
        let track = track.expect("the track still shows");
        assert_eq!((track.session, track.artwork), (playing(), None));
        let card = card.expect("the card still shows");
        assert_eq!((card.title.as_str(), card.artwork), ("Song", None));
    }

    #[test]
    fn a_hung_thumbnail_is_tried_once_per_track_not_once_per_poll() {
        let (fake, counts) = fake(Thumbnail::Hung);
        let mut reader = Reader::new(fake);
        let _ = poll(&mut reader);
        let started = Instant::now();
        for _ in 0..3 {
            let (track, _) = poll(&mut reader);
            assert!(track.is_some_and(|track| track.artwork.is_none()));
        }
        assert!(started.elapsed() < APP_WAIT, "later polls don't wait");
        assert_eq!(counts.thumbnails(), 1);
    }

    // covers: S218
    #[test]
    fn with_none_playing_the_app_that_played_last_is_read_or_none() {
        let paused = |app_id: &str, title: &str| MediaSession {
            app_id: app_id.to_owned(),
            title: title.to_owned(),
            playing: false,
            paused: true,
            ..playing()
        };
        let (mut fake, _) = fake(Thumbnail::Image);
        fake.sessions = vec![paused("Chrome", "Video"), paused("Spotify.exe", "Song")];
        let mut reader = Reader::new(fake);
        let log = DebugLog::default();
        let mut read = |played_last: Option<&str>| {
            let mut listing = reader.listing(&log);
            let track = reader.track(&mut listing, &Follow::AnyApp, played_last, &log);
            track.map(|track| track.session.title)
        };
        assert_eq!(read(Some("Spotify.exe")).as_deref(), Some("Song"));
        assert_eq!(read(None).as_deref(), Some("Video"));
        assert_eq!(read(Some("Vlc.exe")), None, "it quit");
    }

    // covers: S274
    #[test]
    fn a_button_goes_to_the_session_that_counts() {
        let (mut fake, counts) = fake(Thumbnail::Image);
        fake.sessions.clear();
        let mut reader = Reader::new(fake);
        reader.send(&Follow::AnyApp, None, Transport::Next, &DebugLog::default());
        assert_eq!(counts.sends.load(Ordering::SeqCst), 0, "no session counts");
        reader.sessions.sessions.push(playing());
        reader.send(&Follow::AnyApp, None, Transport::Next, &DebugLog::default());
        assert_eq!(counts.sends.load(Ordering::SeqCst), 1);
    }

    // covers: S218, S274
    #[test]
    fn a_remote_button_goes_to_the_paused_session_the_receivers_read() {
        let paused = |app_id: &str, title: &str| MediaSession {
            app_id: app_id.to_owned(),
            title: title.to_owned(),
            playing: false,
            paused: true,
            ..playing()
        };
        let (mut fake, counts) = fake(Thumbnail::Image);
        fake.sessions = vec![paused("Chrome", "Video"), paused("Spotify.exe", "Song")];
        let (mut track, _card) = spawn(
            move || Ok(fake),
            Duration::from_millis(10),
            DebugLog::default(),
        );
        track.follow(Some(Follow::AnyApp));
        track.played_last(Some("Spotify.exe"));
        track.send(Transport::PlayPause);
        let deadline = Instant::now() + Duration::from_secs(10);
        while counts.sends.load(Ordering::SeqCst) == 0 {
            assert!(Instant::now() < deadline, "nothing was sent");
            thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(*counts.sent_to.lock().unwrap(), ["Spotify.exe"]);
    }

    #[test]
    fn sessions_that_cant_be_opened_leave_both_readers_without_readings() {
        let (mut track, mut card) = spawn(
            || Err::<Fake, _>("no sessions here".to_owned()),
            Duration::from_millis(10),
            DebugLog::default(),
        );
        track.follow(Some(Follow::AnyApp));
        card.follow(Some(CardSource::AnyApp));
        thread::sleep(Duration::from_millis(100));
        assert_eq!(track.take_reading(), None);
        assert_eq!(card.take_reading(), None);
    }

    #[test]
    fn one_thread_reads_for_both_readers_and_rests_when_neither_follows() {
        let poll = Duration::from_millis(50);
        let (fake, counts) = fake(Thumbnail::Image);
        let (mut track, mut card) = spawn(move || Ok(fake), poll, DebugLog::default());
        track.follow(Some(Follow::AnyApp));
        card.follow(Some(CardSource::AnyApp));
        let deadline = Instant::now() + Duration::from_secs(10);
        let (mut track_read, mut card_read) = (false, false);
        while !(track_read && card_read) {
            assert!(Instant::now() < deadline, "no reading");
            track_read |= track.take_reading().is_some_and(|r| r.track.is_some());
            card_read |= card.take_reading().is_some_and(|r| r.is_some());
            thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(track.icon(), Some(FittedArtwork::single(JPEG.into())));
        let (before, started) = (counts.lists(), Instant::now());
        thread::sleep(Duration::from_millis(600));
        let lists = counts.lists() - before;
        let polls = started.elapsed().as_millis() / poll.as_millis() + 1;
        assert!(
            lists as u128 <= polls,
            "{lists} listings in {polls} polls: one per poll serves both"
        );
        track.follow(None);
        card.follow(None);
        // A listing a slow runner paused mid-way may still land; wait for two quiet polls.
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut resting = counts.lists();
        loop {
            thread::sleep(poll * 2);
            let now = counts.lists();
            if now == resting {
                break;
            }
            assert!(Instant::now() < deadline, "the reader never rests");
            resting = now;
        }
        thread::sleep(poll * 6);
        assert_eq!(counts.lists(), resting, "nothing is read while unfollowed");
        drop(track);
        assert!(
            !counts.ended.load(Ordering::SeqCst),
            "the card still holds it"
        );
        drop(card);
        let deadline = Instant::now() + Duration::from_secs(5);
        while !counts.ended.load(Ordering::SeqCst) {
            assert!(Instant::now() < deadline, "the thread didn't end");
            thread::sleep(Duration::from_millis(5));
        }
    }
}
