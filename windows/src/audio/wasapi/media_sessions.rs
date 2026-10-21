//! The media sessions Windows lists (the system media transport controls,
//! `Windows.Media.Control`), read on one thread for both the receivers'
//! track info ([`MediaSessionReader`]) and the Now Playing card
//! ([`MediaCardReader`]).
//!
//! The thread sleeps while neither follows a source, then reads every
//! [`POLL_INTERVAL`]: it lists the sessions once, picks the one that counts
//! for each reader ([`pick`]) and reads a picked session's thumbnail once,
//! giving the app [`APP_WAIT`] to hand it over. Artwork is made from those
//! bytes once per track ([`media`]). A card button goes to the session that
//! counts when the thread takes it, and the sessions are read again
//! [`AFTER_COMMAND`] later, so the card shows the new state without waiting
//! for the next poll. Nothing else waits on the thread: the readers meet it
//! only under a lock held for a copy. Errors are logged once until reading
//! works again, and count as no track.

use std::collections::{HashMap, HashSet};
use std::future::{Future, IntoFuture};
use std::pin::pin;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::task::{Context, Poll, Wake, Waker};
use std::thread::{self, Thread};
use std::time::{Duration, Instant};

use windows::Media::Control::{
    GlobalSystemMediaTransportControlsSession as Session,
    GlobalSystemMediaTransportControlsSessionManager as SessionManager,
    GlobalSystemMediaTransportControlsSessionMediaProperties as MediaProperties,
};
use windows::Storage::Streams::{
    Buffer, DataReader, IRandomAccessStreamWithContentType, InputStreamOptions,
};
use windows::Win32::Foundation::{E_FAIL, ERROR_TIMEOUT};
use windows::core::Interface;
use windows_future::{IAsyncInfo, IAsyncOperation};

use super::com::Apartment;
use super::media;
use crate::audio::now_playing::{
    Follow, MediaSession, POLL_INTERVAL, Reading, Track, TrackReader, pick,
};
use crate::debug_log::DebugLog;
use crate::now_playing_card::{ArtworkCache, CardSource, CardTrack, MediaControls, Transport};

/// How soon the sessions are read again after a button: long enough for
/// most apps to have changed their state.
const AFTER_COMMAND: Duration = Duration::from_millis(400);

/// How long one reading waits on apps for a thumbnail, and a button on the
/// app to take it. Past it the track shows without its artwork.
const APP_WAIT: Duration = Duration::from_secs(1);

/// The largest thumbnail read; an app's is a few hundred kilobytes at most.
const THUMBNAIL_LIMIT: u64 = 16 << 20;

/// Artwork kept for at most this many tracks.
const ARTWORK_CACHE: usize = 32;

/// Starts the thread that reads the sessions for both readers. It reads
/// nothing until one follows a source and ends once both are dropped. A
/// thread that can't start is logged, and neither reader ever reads.
pub fn read_media_sessions(log: DebugLog) -> (MediaSessionReader, MediaCardReader) {
    spawn(WindowsSessions::default, POLL_INTERVAL, log)
}

#[derive(Default)]
struct State {
    track: Option<Follow>,
    /// Counts follow changes, so a reading made for an earlier one is
    /// dropped.
    track_generation: u64,
    track_reading: Option<Reading>,
    icon: Option<Arc<[u8]>>,
    card: Option<CardSource>,
    card_generation: u64,
    card_reading: Option<Option<CardTrack>>,
    commands: Vec<Transport>,
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
            self.shared.wake.notify_all();
        }
    }

    fn take_reading(&mut self) -> Option<Reading> {
        self.shared.lock().track_reading.take()
    }

    fn icon(&mut self) -> Option<Arc<[u8]>> {
        self.shared.lock().icon.clone()
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

fn spawn<L, M>(make: M, poll: Duration, log: DebugLog) -> (MediaSessionReader, MediaCardReader)
where
    L: SessionList,
    M: FnOnce() -> L + Send + 'static,
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

fn run<L: SessionList>(shared: &Shared, make: impl FnOnce() -> L, poll: Duration, log: &DebugLog) {
    let _apartment = match Apartment::enter() {
        Ok(apartment) => apartment,
        Err(error) => {
            log.log(|| format!("now playing: COM couldn't start on the reading thread: {error}"));
            return;
        }
    };
    shared.lock().icon = media::fitted_icon(log);
    let mut reader = Reader::new(make());
    loop {
        let (track, track_generation, card, card_generation, commands) = {
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
            (
                state.track.clone(),
                state.track_generation,
                state.card.clone().map(follow_for),
                state.card_generation,
                commands,
            )
        };
        if let Some(card) = &card {
            for command in &commands {
                reader.send(card, *command, log);
            }
        }
        let (track_reading, card_reading) = reader.read(track.as_ref(), card.as_ref(), log);
        let mut state = shared.lock();
        if let Some(track) = track_reading
            && state.track_generation == track_generation
        {
            state.track_reading = Some(Reading { track });
        }
        if let Some(card) = card_reading
            && state.card_generation == card_generation
        {
            state.card_reading = Some(card);
        }
        let wait = if card.is_some() && !commands.is_empty() {
            AFTER_COMMAND
        } else {
            poll
        };
        let _ = shared
            .wake
            .wait_timeout_while(state, wait, |state| {
                state.track_generation == track_generation
                    && state.card_generation == card_generation
                    && state.commands.is_empty()
                    && state.readers > 0
            })
            .unwrap_or_else(PoisonError::into_inner);
    }
}

fn follow_for(source: CardSource) -> Follow {
    match source {
        CardSource::AnyApp => Follow::AnyApp,
        CardSource::App { executable } => Follow::App { executable },
    }
}

/// Where the thread reads sessions from: Windows', or a fake in the tests.
trait SessionList {
    type Session;

    /// Every session listed now, described. One app's session that fails to
    /// answer is left out.
    fn list(&mut self) -> windows::core::Result<Vec<(MediaSession, Self::Session)>>;

    /// Opens the session's thumbnail; `None` while it has none.
    fn thumbnail(
        &self,
        session: &Self::Session,
    ) -> windows::core::Result<Option<IAsyncOperation<IRandomAccessStreamWithContentType>>>;

    /// Sends `command` to the session; the operation says whether the app
    /// took it.
    fn send(
        &self,
        session: &Self::Session,
        command: Transport,
    ) -> windows::core::Result<IAsyncOperation<bool>>;

    /// Forgets what a failed listing may have left broken.
    fn reset(&mut self);
}

#[derive(Default)]
struct WindowsSessions {
    manager: Option<SessionManager>,
}

impl SessionList for WindowsSessions {
    type Session = (Session, MediaProperties);

    fn list(&mut self) -> windows::core::Result<Vec<(MediaSession, Self::Session)>> {
        let manager = match &self.manager {
            Some(manager) => manager.clone(),
            None => {
                let manager = SessionManager::RequestAsync()?.join()?;
                self.manager = Some(manager.clone());
                manager
            }
        };
        let list = manager.GetSessions()?;
        let mut found = Vec::new();
        for index in 0..list.Size()? {
            if let Ok(session) = list.GetAt(index)
                && let Ok((described, properties)) = media::describe(&session)
            {
                found.push((described, (session, properties)));
            }
        }
        Ok(found)
    }

    fn thumbnail(
        &self,
        (_, properties): &Self::Session,
    ) -> windows::core::Result<Option<IAsyncOperation<IRandomAccessStreamWithContentType>>> {
        let Ok(thumbnail) = properties.Thumbnail() else {
            return Ok(None);
        };
        thumbnail.OpenReadAsync().map(Some)
    }

    fn send(
        &self,
        (session, _): &Self::Session,
        command: Transport,
    ) -> windows::core::Result<IAsyncOperation<bool>> {
        match command {
            Transport::Previous => session.TrySkipPreviousAsync(),
            Transport::PlayPause => session.TryTogglePlayPauseAsync(),
            Transport::Next => session.TrySkipNextAsync(),
        }
    }

    fn reset(&mut self) {
        self.manager = None;
    }
}

/// The reading thread's own state.
struct Reader<L> {
    sessions: L,
    /// Receiver artwork by track; `None` for a thumbnail that couldn't be
    /// fitted.
    jpegs: HashMap<String, Option<Arc<[u8]>>>,
    /// Card artwork by track.
    tiles: ArtworkCache,
    /// Tracks whose thumbnail couldn't be read, so that is logged once. A
    /// thumbnail missing or unread is asked again at the next reading.
    unread: HashSet<String>,
    /// The last listing failed, and said so.
    failing: bool,
    /// The last tracks read, for the log.
    last_track: Option<MediaSession>,
    last_card: Option<MediaSession>,
}

/// Thumbnails read during one reading, by session index.
type Thumbnails = Vec<(usize, Option<Vec<u8>>)>;

impl<L: SessionList> Reader<L> {
    fn new(sessions: L) -> Reader<L> {
        Reader {
            sessions,
            jpegs: HashMap::new(),
            tiles: ArtworkCache::default(),
            unread: HashSet::new(),
            failing: false,
            last_track: None,
            last_card: None,
        }
    }

    /// One reading for each reader that follows a source: the track that
    /// counts, or `None` when no session does.
    fn read(
        &mut self,
        track: Option<&Follow>,
        card: Option<&Follow>,
        log: &DebugLog,
    ) -> (Option<Option<Track>>, Option<Option<CardTrack>>) {
        let listed = self.listed(log);
        let described: Vec<MediaSession> =
            listed.iter().map(|(session, _)| session.clone()).collect();
        let track_index = track.map(|follow| pick(&described, follow));
        let card_index = card.map(|follow| pick(&described, follow));
        if let Some(index) = track_index {
            self.log_track(index.map(|index| &described[index]), log);
        }
        if let Some(index) = card_index {
            self.log_card(index.map(|index| &described[index]), log);
        }
        let deadline = Instant::now() + APP_WAIT;
        let mut thumbnails = Thumbnails::new();
        let mut track_reading = None;
        if let Some(index) = track_index {
            let track = match index {
                Some(index) => {
                    let (session, entry) = &listed[index];
                    let key = track_key(session);
                    let artwork = match self.jpegs.get(&key) {
                        Some(made) => made.clone(),
                        None => self
                            .thumbnail(&mut thumbnails, index, &key, session, entry, deadline, log)
                            .and_then(|image| self.jpeg(key, session, &image, log)),
                    };
                    Some(Track {
                        session: session.clone(),
                        artwork,
                    })
                }
                None => None,
            };
            track_reading = Some(track);
        }
        let mut card_reading = None;
        if let Some(index) = card_index {
            let card = match index {
                Some(index) => {
                    let (session, entry) = &listed[index];
                    let key = track_key(session);
                    let artwork = match self.tiles.get(&key) {
                        Some(made) => made,
                        None => self
                            .thumbnail(&mut thumbnails, index, &key, session, entry, deadline, log)
                            .and_then(|image| {
                                let (tile, error) =
                                    self.tiles.keep(&key, media::card_artwork(&image));
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
                        app_id: session.app_id.clone(),
                        title: session.title.clone(),
                        artist: session.artist.clone(),
                        playing: session.playing,
                        artwork,
                    })
                }
                None => None,
            };
            card_reading = Some(card);
        }
        (track_reading, card_reading)
    }

    /// Sends `command` to the session that counts for `follow` now.
    fn send(&mut self, follow: &Follow, command: Transport, log: &DebugLog) {
        let sent = self.sessions.list().and_then(|listed| {
            let described: Vec<MediaSession> =
                listed.iter().map(|(session, _)| session.clone()).collect();
            let Some(index) = pick(&described, follow) else {
                return Ok(None);
            };
            let (session, entry) = &listed[index];
            let operation = self.sessions.send(entry, command)?;
            let taken = within(operation, Instant::now() + APP_WAIT)?;
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

    /// The session's thumbnail, read at most once per reading and within
    /// `deadline`; `None` while it has none or it couldn't be read.
    #[allow(clippy::too_many_arguments)]
    fn thumbnail(
        &mut self,
        thumbnails: &mut Thumbnails,
        index: usize,
        key: &str,
        session: &MediaSession,
        entry: &L::Session,
        deadline: Instant,
        log: &DebugLog,
    ) -> Option<Vec<u8>> {
        if let Some((_, image)) = thumbnails.iter().find(|(read, _)| *read == index) {
            return image.clone();
        }
        let read = self.sessions.thumbnail(entry).and_then(|open| match open {
            Some(open) => read_thumbnail(open, deadline),
            None => Ok(None),
        });
        let image = match read {
            Ok(image) => {
                self.unread.remove(key);
                image
            }
            Err(error) => {
                if self.unread.len() >= ARTWORK_CACHE {
                    self.unread.clear();
                }
                if self.unread.insert(key.to_owned()) {
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
        thumbnails.push((index, image.clone()));
        image
    }

    /// Receiver artwork fitted from `image`, kept for the track.
    fn jpeg(
        &mut self,
        key: String,
        session: &MediaSession,
        image: &[u8],
        log: &DebugLog,
    ) -> Option<Arc<[u8]>> {
        let artwork = match media::fit_image(image) {
            Ok(Some(jpeg)) => Some(Arc::from(jpeg)),
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

/// The thumbnail's bytes, read within `deadline`.
fn read_thumbnail(
    open: IAsyncOperation<IRandomAccessStreamWithContentType>,
    deadline: Instant,
) -> windows::core::Result<Option<Vec<u8>>> {
    let stream = within(open, deadline)?;
    let size = stream.Size()?;
    if size > THUMBNAIL_LIMIT {
        return Err(windows::core::Error::new(
            E_FAIL,
            format!("the thumbnail has {size} bytes"),
        ));
    }
    let size = size as usize;
    let mut image = Vec::with_capacity(size);
    while image.len() < size {
        let wanted = (size - image.len()) as u32;
        let buffer = Buffer::Create(wanted)?;
        let filled = within(
            stream.ReadAsync(&buffer, wanted, InputStreamOptions::None)?,
            deadline,
        )?;
        let start = image.len();
        image.resize(start + filled.Length()? as usize, 0);
        if image.len() == start {
            break;
        }
        DataReader::FromBuffer(&filled)?.ReadBytes(&mut image[start..])?;
    }
    Ok((!image.is_empty()).then_some(image))
}

/// The result of `operation`, waited for until `deadline`. Past it the
/// operation is cancelled and the wait fails, so an app that never answers
/// costs the thread no more than that.
fn within<O, T>(operation: O, deadline: Instant) -> windows::core::Result<T>
where
    O: Interface + IntoFuture<Output = windows::core::Result<T>>,
{
    let info: IAsyncInfo = operation.cast()?;
    let waker = Waker::from(Arc::new(Unpark(thread::current())));
    let mut context = Context::from_waker(&waker);
    let mut future = pin!(operation.into_future());
    loop {
        if let Poll::Ready(result) = future.as_mut().poll(&mut context) {
            return result;
        }
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            // An app may ignore the cancel; the thread moves on either way.
            let _ = info.Cancel();
            return Err(windows::core::Error::new(
                ERROR_TIMEOUT.to_hresult(),
                "the app didn't answer in time",
            ));
        }
        thread::park_timeout(left);
    }
}

/// Wakes the waiting thread when an operation completes.
struct Unpark(Thread);

impl Wake for Unpark {
    fn wake(self: Arc<Self>) {
        self.0.unpark();
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    use windows::Storage::Streams::RandomAccessStreamReference;

    use super::*;

    #[derive(Clone, Copy)]
    enum Thumbnail {
        Image,
        Hung,
    }

    #[derive(Default)]
    struct Counts {
        lists: AtomicUsize,
        thumbnails: AtomicUsize,
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

    /// What hung operations wait on; opened when the test ends, so the
    /// thread pool's threads finish, and given up after `GATE_LIMIT` even if
    /// a test never ends.
    #[derive(Default)]
    struct Gate {
        open: Mutex<bool>,
        opened: Condvar,
    }

    const GATE_LIMIT: Duration = Duration::from_secs(30);

    impl Gate {
        fn wait(&self) {
            let open = self.open.lock().unwrap_or_else(PoisonError::into_inner);
            drop(
                self.opened
                    .wait_timeout_while(open, GATE_LIMIT, |open| !*open),
            );
        }
    }

    struct OpensOnDrop(Arc<Gate>);

    impl Drop for OpensOnDrop {
        fn drop(&mut self) {
            *self.0.open.lock().unwrap_or_else(PoisonError::into_inner) = true;
            self.0.opened.notify_all();
        }
    }

    struct Fake {
        thumbnail: Thumbnail,
        counts: Arc<Counts>,
        gate: Arc<Gate>,
    }

    impl Drop for Fake {
        fn drop(&mut self) {
            self.counts.ended.store(true, Ordering::SeqCst);
        }
    }

    impl SessionList for Fake {
        type Session = ();

        fn list(&mut self) -> windows::core::Result<Vec<(MediaSession, ())>> {
            self.counts.lists.fetch_add(1, Ordering::SeqCst);
            Ok(vec![(playing(), ())])
        }

        fn thumbnail(
            &self,
            _: &(),
        ) -> windows::core::Result<Option<IAsyncOperation<IRandomAccessStreamWithContentType>>>
        {
            self.counts.thumbnails.fetch_add(1, Ordering::SeqCst);
            match self.thumbnail {
                Thumbnail::Image => RandomAccessStreamReference::CreateFromStream(
                    &media::memory_stream(media::ICON_PNG)?,
                )?
                .OpenReadAsync()
                .map(Some),
                Thumbnail::Hung => {
                    let gate = self.gate.clone();
                    Ok(Some(IAsyncOperation::spawn(move || {
                        gate.wait();
                        Err(E_FAIL.into())
                    })))
                }
            }
        }

        fn send(&self, _: &(), _: Transport) -> windows::core::Result<IAsyncOperation<bool>> {
            let gate = self.gate.clone();
            Ok(IAsyncOperation::spawn(move || {
                gate.wait();
                Ok(true)
            }))
        }

        fn reset(&mut self) {}
    }

    fn playing() -> MediaSession {
        MediaSession {
            app_id: "Spotify.exe".to_owned(),
            title: "Song".to_owned(),
            artist: "Artist".to_owned(),
            album: "Album".to_owned(),
            playing: true,
        }
    }

    fn fake(thumbnail: Thumbnail) -> (Fake, Arc<Counts>, OpensOnDrop) {
        let counts = Arc::new(Counts::default());
        let gate = Arc::new(Gate::default());
        let fake = Fake {
            thumbnail,
            counts: counts.clone(),
            gate: gate.clone(),
        };
        (fake, counts, OpensOnDrop(gate))
    }

    #[test]
    fn one_listing_and_one_thumbnail_serve_both_readers() {
        let _apartment = Apartment::enter().unwrap();
        let (fake, counts, _gate) = fake(Thumbnail::Image);
        let mut reader = Reader::new(fake);
        let log = DebugLog::default();
        let any = Follow::AnyApp;
        let (track, card) = reader.read(Some(&any), Some(&any), &log);
        assert_eq!(counts.lists(), 1, "one listing serves both readers");
        assert_eq!(counts.thumbnails(), 1, "one thumbnail read serves both");
        let track = track.flatten().expect("the receivers get the track");
        assert_eq!(track.session, playing());
        assert!(track.artwork.is_some());
        let card = card.flatten().expect("the card gets the track");
        assert_eq!((card.title.as_str(), card.playing), ("Song", true));
        assert!(card.artwork.is_some());
        let _ = reader.read(Some(&any), Some(&any), &log);
        assert_eq!(counts.lists(), 2);
        assert_eq!(counts.thumbnails(), 1, "artwork is made once per track");
        let (track, card) = reader.read(Some(&any), None, &log);
        assert!(track.flatten().is_some() && card.is_none());
    }

    #[test]
    fn a_hung_thumbnail_leaves_the_track_without_artwork_in_time() {
        let _apartment = Apartment::enter().unwrap();
        let (fake, counts, _gate) = fake(Thumbnail::Hung);
        let mut reader = Reader::new(fake);
        let log = DebugLog::default();
        let any = Follow::AnyApp;
        let started = Instant::now();
        let (track, card) = reader.read(Some(&any), Some(&any), &log);
        let took = started.elapsed();
        assert!(took >= APP_WAIT && took < APP_WAIT * 2, "took {took:?}");
        assert_eq!(counts.thumbnails(), 1, "one wait covers both readers");
        let track = track.flatten().expect("the track still shows");
        assert_eq!((track.session, track.artwork), (playing(), None));
        let card = card.flatten().expect("the card still shows");
        assert_eq!((card.title.as_str(), card.artwork), ("Song", None));
        let _ = reader.read(Some(&any), None, &log);
        assert_eq!(counts.thumbnails(), 2, "the thumbnail is asked again");
    }

    #[test]
    fn a_hung_button_holds_the_thread_no_longer_than_the_wait() {
        let _apartment = Apartment::enter().unwrap();
        let (fake, _counts, _gate) = fake(Thumbnail::Image);
        let mut reader = Reader::new(fake);
        let started = Instant::now();
        reader.send(&Follow::AnyApp, Transport::Next, &DebugLog::default());
        let took = started.elapsed();
        assert!(took >= APP_WAIT && took < APP_WAIT * 2, "took {took:?}");
    }

    #[test]
    fn one_thread_reads_for_both_readers_and_rests_when_neither_follows() {
        let poll = Duration::from_millis(50);
        let (fake, counts, _gate) = fake(Thumbnail::Image);
        let (mut track, mut card) = spawn(move || fake, poll, DebugLog::default());
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
        thread::sleep(poll * 3);
        let resting = counts.lists();
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

    /// Reads this PC's media sessions; play something first. Run with
    /// `cargo test -- --ignored --nocapture reads_this_pcs`.
    #[test]
    #[ignore = "reads this PC's media sessions"]
    fn reads_this_pcs_media_sessions() {
        let (mut track, mut card) = read_media_sessions(DebugLog::default());
        track.follow(Some(Follow::AnyApp));
        card.follow(Some(CardSource::AnyApp));
        let deadline = Instant::now() + Duration::from_secs(20);
        let (mut reading, mut shown) = (None, None);
        while reading.is_none() || shown.is_none() {
            assert!(Instant::now() < deadline, "no reading");
            reading = reading.or_else(|| track.take_reading());
            shown = shown.or_else(|| card.take_reading());
            thread::sleep(Duration::from_millis(50));
        }
        let (reading, shown) = (reading.unwrap(), shown.unwrap());
        let icon = track
            .icon()
            .expect("the icon is made before the first reading");
        println!("icon: {} bytes", icon.len());
        match reading.track {
            Some(track) => {
                let artwork = track.artwork.map(|jpeg| jpeg.len());
                println!("{:?}, artwork: {artwork:?} bytes", track.session);
            }
            None => println!("no media session counts"),
        }
        if let Some(shown) = shown {
            println!("card: {} / {}", shown.title, shown.subtitle());
        }
    }
}
