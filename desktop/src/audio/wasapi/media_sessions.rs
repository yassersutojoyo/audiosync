//! The media sessions Windows lists (the system media transport controls,
//! `Windows.Media.Control`), read on one thread for both the receivers'
//! track info ([`MediaSessionReader`]) and the Now Playing card
//! ([`MediaCardReader`]).
//!
//! The thread sleeps while neither follows a source, then reads every
//! [`POLL_INTERVAL`]: it lists the sessions once, picks the one that counts
//! for each reader ([`pick`]) and reads a picked session's thumbnail once.
//! Every wait on an app is bounded by [`APP_WAIT`], and a thumbnail that
//! missed it isn't asked for again for [`MISS_RETRY`]. The receivers'
//! reading goes out before any work only the card needs. Artwork is made
//! from the thumbnail's bytes once per track ([`media`]), and the receivers'
//! track carries its session's timeline. A card button goes to the session that
//! counts when the thread takes it, and the sessions are read again
//! [`AFTER_COMMAND`] later, so the card shows the new state without waiting
//! for the next poll. Nothing else waits on the thread: the readers meet it
//! only under a lock held for a copy. Errors are logged once until reading
//! works again, and count as no track.

use std::collections::HashMap;
use std::future::{Future, IntoFuture};
use std::pin::pin;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::task::{Context, Poll, Wake, Waker};
use std::thread::{self, Thread};
use std::time::{Duration, Instant, SystemTime};

use windows::Media::Control::{
    GlobalSystemMediaTransportControlsSession as Session,
    GlobalSystemMediaTransportControlsSessionManager as SessionManager,
    GlobalSystemMediaTransportControlsSessionMediaProperties as MediaProperties,
};
use windows::Storage::Streams::{
    Buffer, DataReader, IRandomAccessStreamWithContentType, InputStreamOptions,
};
use windows::Win32::Foundation::{E_FAIL, ERROR_TIMEOUT};
use windows::core::{Interface, RuntimeType};
use windows_future::{IAsyncInfo, IAsyncOperation};

use super::com::Apartment;
use super::media;
use crate::audio::now_playing::{
    Follow, MediaSession, POLL_INTERVAL, RawTimeline, Reading, Timeline, Track, TrackReader, pick,
    wall_ticks,
};
use crate::debug_log::DebugLog;
use crate::now_playing_card::{ArtworkCache, CardSource, CardTrack, MediaControls, Transport};

/// How soon the sessions are read again after a button: long enough for
/// most apps to have changed their state.
const AFTER_COMMAND: Duration = Duration::from_millis(400);

/// How long a listing waits on apps for their sessions' properties, a
/// reading for a thumbnail, and a button for the app to take it. Past it the
/// session is left out of that listing, or its track shows without artwork.
const APP_WAIT: Duration = Duration::from_secs(1);

/// How long a track's thumbnail that couldn't be read in time isn't asked
/// for again.
const MISS_RETRY: Duration = Duration::from_secs(30);

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
        let mut listing = reader.listing(log);
        // The receivers' reading goes out first, so a thumbnail only the
        // card needs never holds it back.
        if let Some(follow) = &track {
            let track = reader.track(&mut listing, follow, log);
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

    /// The session's timeline as it reports it now.
    fn timeline(&self, session: &Self::Session) -> windows::core::Result<RawTimeline>;

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
        let sessions: Vec<Session> = (0..list.Size()?)
            .filter_map(|index| list.GetAt(index).ok())
            .collect();
        Ok(describe_each(
            sessions,
            |session| session.TryGetMediaPropertiesAsync(),
            |session, properties| {
                let described = media::describe(&session, &properties)?;
                Ok((described, (session, properties)))
            },
        ))
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

    fn timeline(&self, (session, _): &Self::Session) -> windows::core::Result<RawTimeline> {
        let timeline = session.GetTimelineProperties()?;
        Ok(RawTimeline {
            start: timeline.StartTime()?.Duration,
            end: timeline.EndTime()?.Duration,
            position: timeline.Position()?.Duration,
            updated: timeline.LastUpdatedTime()?.UniversalTime,
        })
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

impl<S> Listing<S> {
    fn pick(&self, follow: &Follow) -> Option<usize> {
        let described: Vec<MediaSession> = self
            .listed
            .iter()
            .map(|(session, _)| session.clone())
            .collect();
        pick(&described, follow)
    }
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
        log: &DebugLog,
    ) -> Option<Track> {
        let index = listing.pick(follow);
        self.log_track(index.map(|index| &listing.listed[index].0), log);
        let index = index?;
        let session = listing.listed[index].0.clone();
        // Read at each poll rather than on TimelinePropertiesChanged, which
        // needs a handler on every session as they come and go: track info
        // passes on only a jump, so a poll that finds none sends nothing.
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

    /// The card's track for `follow`, or `None` when no session counts.
    fn card(
        &mut self,
        listing: &mut Listing<L::Session>,
        follow: &Follow,
        log: &DebugLog,
    ) -> Option<CardTrack> {
        let index = listing.pick(follow);
        self.log_card(index.map(|index| &listing.listed[index].0), log);
        let index = index?;
        let session = listing.listed[index].0.clone();
        let key = track_key(&session);
        let artwork = match self.tiles.get(&key) {
            Some(made) => made,
            None => self.thumbnail(listing, index, &key, log).and_then(|image| {
                let (tile, error) = self.tiles.keep(&key, media::card_artwork(&image));
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
        let deadline = Instant::now() + APP_WAIT;
        let read = self.sessions.thumbnail(entry).and_then(|open| match open {
            Some(open) => read_thumbnail(open, deadline),
            None => Ok(None),
        });
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

/// Each session described from its properties. They are asked for all at
/// once and waited for together for at most [`APP_WAIT`]; a session whose
/// app doesn't answer in time, or answers with an error, is left out.
fn describe_each<S, P, T>(
    sessions: Vec<S>,
    properties: impl Fn(&S) -> windows::core::Result<IAsyncOperation<P>>,
    describe: impl Fn(S, P) -> windows::core::Result<T>,
) -> Vec<T>
where
    P: RuntimeType + 'static,
{
    let asked: Vec<(S, IAsyncOperation<P>)> = sessions
        .into_iter()
        .filter_map(|session| {
            let operation = properties(&session).ok()?;
            Some((session, operation))
        })
        .collect();
    let deadline = Instant::now() + APP_WAIT;
    asked
        .into_iter()
        .filter_map(|(session, operation)| {
            within(operation, deadline)
                .and_then(|answer| describe(session, answer))
                .ok()
        })
        .collect()
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
    use windows::core::HSTRING;

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

        fn timeline(&self, _: &()) -> windows::core::Result<RawTimeline> {
            Ok(TIMELINE)
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

    /// One poll for both readers, as the thread makes it.
    fn poll(reader: &mut Reader<Fake>) -> (Option<Track>, Option<CardTrack>) {
        let log = DebugLog::default();
        let mut listing = reader.listing(&log);
        let track = reader.track(&mut listing, &Follow::AnyApp, &log);
        let card = reader.card(&mut listing, &Follow::AnyApp, &log);
        (track, card)
    }

    #[test]
    fn one_listing_and_one_thumbnail_serve_both_readers() {
        let _apartment = Apartment::enter().unwrap();
        let (fake, counts, _gate) = fake(Thumbnail::Image);
        let mut reader = Reader::new(fake);
        let (track, card) = poll(&mut reader);
        assert_eq!(counts.lists(), 1, "one listing serves both readers");
        assert_eq!(counts.thumbnails(), 1, "one thumbnail read serves both");
        let track = track.expect("the receivers get the track");
        assert_eq!(track.session, playing());
        assert!(track.artwork.is_some());
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
        let _apartment = Apartment::enter().unwrap();
        let (fake, counts, _gate) = fake(Thumbnail::Hung);
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
        let _apartment = Apartment::enter().unwrap();
        let (fake, counts, _gate) = fake(Thumbnail::Hung);
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

    #[test]
    fn a_session_whose_properties_hang_is_left_out_in_time() {
        let _apartment = Apartment::enter().unwrap();
        let gate = Arc::new(Gate::default());
        let _open = OpensOnDrop(gate.clone());
        let started = Instant::now();
        let described = describe_each(
            vec![0, 1, 2],
            |&session| {
                let gate = gate.clone();
                Ok(IAsyncOperation::spawn(move || {
                    if session == 1 {
                        gate.wait();
                    }
                    Ok(HSTRING::from(format!("session {session}")))
                }))
            },
            |session, name: HSTRING| Ok((session, name.to_string())),
        );
        let took = started.elapsed();
        assert!(took >= APP_WAIT && took < APP_WAIT * 2, "took {took:?}");
        let names = |session: i32| (session, format!("session {session}"));
        assert_eq!(described, vec![names(0), names(2)]);
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
                let progress = track
                    .timeline
                    .and_then(|timeline| timeline.progress_at(Instant::now()));
                println!("timeline: {:?}, progress now: {progress:?}", track.timeline);
            }
            None => println!("no media session counts"),
        }
        if let Some(shown) = shown {
            println!("card: {} / {}", shown.title, shown.subtitle());
        }
    }
}
