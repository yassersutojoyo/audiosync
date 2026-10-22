//! One Cast session end to end (sections 5 to 8): connect with retries,
//! launch, the stream server, `LOAD` until `PLAYING`, recovery, Resync,
//! volume, diagnostics and teardown. Driven on the host's thread through
//! [`CastSession::run_for`]; the stream server runs on its own threads.

use std::collections::VecDeque;
use std::fmt;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use crate::audio::{AudioReader, Codec, StreamFormat};
use crate::channel::{Event, PlaybackEnd};
use crate::client::{CastClient, QUIT_BUDGET};
use crate::error::{Backoff, ConnectError, EndReason, LaunchError};
use crate::group::{GroupMember, GroupRecords, MemberList, MultizoneMessage};
use crate::ids::RequestIds;
use crate::media::{
    IdleReason, LoadRequest, MediaError, MediaStatus, Metadata, PlayerState, VolumeError,
};
use crate::status::{AppSession, DeviceVolume};
use crate::stream::{ServerEvent, StreamConfig, StreamError, StreamServer};

/// How long the device may take to fetch the stream after the `LOAD`
/// reply (C85).
pub const FETCH_TIMEOUT: Duration = Duration::from_secs(10);
/// The diagnostics interval while playing (C78).
pub const DIAGNOSTICS_INTERVAL: Duration = Duration::from_secs(1);
/// Drift since the `LOAD` above which a Resync is offered (C79).
pub const RESYNC_DRIFT: Duration = Duration::from_millis(250);
/// The teardown budget when the user stops: two steps of 2 s and the
/// closes (C87).
pub const STOP_BUDGET: Duration = Duration::from_secs(5);
/// Session events kept for a host that doesn't poll.
pub const MAX_SESSION_EVENTS: usize = 64;
const SLICE: Duration = Duration::from_millis(50);

/// Takes a session event at once, or hands it back when it can't.
pub type EventSink = Box<dyn FnMut(SessionEvent) -> Result<(), SessionEvent> + Send>;

/// Why a session could not start or carry on.
#[derive(Debug)]
pub enum SessionError {
    Connect(ConnectError),
    Launch(LaunchError),
    Media(MediaError),
    Stream(StreamError),
}

impl fmt::Display for SessionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SessionError::Connect(error) => error.fmt(f),
            SessionError::Launch(error) => error.fmt(f),
            SessionError::Media(error) => error.fmt(f),
            SessionError::Stream(error) => error.fmt(f),
        }
    }
}

impl std::error::Error for SessionError {}

impl From<ConnectError> for SessionError {
    fn from(error: ConnectError) -> SessionError {
        SessionError::Connect(error)
    }
}

impl From<LaunchError> for SessionError {
    fn from(error: LaunchError) -> SessionError {
        SessionError::Launch(error)
    }
}

impl From<MediaError> for SessionError {
    fn from(error: MediaError) -> SessionError {
        SessionError::Media(error)
    }
}

impl From<StreamError> for SessionError {
    fn from(error: StreamError) -> SessionError {
        SessionError::Stream(error)
    }
}

/// How a session ended.
#[derive(Debug)]
pub enum SessionEnd {
    /// The sender's own teardown (C87).
    Stopped,
    /// Someone else stopped or replaced the app or the stream (C43, C86).
    TakenOver,
    /// The user deselected the device while the sender retried (C80).
    Deselected,
    /// A failure the session could not recover from.
    Failed(SessionError),
}

/// The sender's view of a playing session, once per second (C78).
#[derive(Clone, Debug, PartialEq)]
pub struct Diagnostics {
    /// Audio sent since the `LOAD`.
    pub sent: Duration,
    /// Audio waiting in the sender's queue (C64).
    pub queued: Duration,
    pub current_time: Option<f64>,
    pub player_state: Option<PlayerState>,
    /// Frames dropped since the `LOAD` (C64).
    pub frames_dropped: u64,
    /// `BUFFERING` after `PLAYING` since the `LOAD` (C51).
    pub rebuffers: u32,
    /// Dropped audio plus time spent rebuffering, since the `LOAD` (C79).
    pub drift: Duration,
    /// The drift is over [`RESYNC_DRIFT`]; the host may offer Resync. The
    /// sender never restarts on its own (C79).
    pub resync_offered: bool,
}

/// What the session reports to the host.
#[derive(Debug)]
pub enum SessionEvent {
    /// The device plays the stream (C49).
    Playing,
    Diagnostics(Diagnostics),
    /// The receiver ran dry (C51).
    Rebuffering,
    /// The device's volume as its latest status reports it; the host moves
    /// its control there and sends nothing back (C69, C70).
    Volume(DeviceVolume),
    /// A connect attempt failed; the next one follows after `retry_in`
    /// (C80).
    ConnectFailed {
        error: ConnectError,
        retry_in: Duration,
    },
    /// The channel failed and the sender reconnects (C81, C86).
    Recovering(EndReason),
    /// The session carries on; `relaunched` when the app had to be
    /// launched and loaded again (C81).
    Recovered {
        relaunched: bool,
    },
    /// The device saw the stream end while the sender kept it open, and the
    /// sender loaded it again once (C86).
    Reloaded,
    /// The device refused WAV before it played, and the sender loads FLAC
    /// once instead (C84).
    FormatFallback {
        from: Codec,
        to: Codec,
        reason: MediaError,
    },
    /// From the stream server, for the log.
    Stream(ServerEvent),
    /// A group's member list changed (C94); the host matches the members
    /// to the devices it found (C95).
    Members(Vec<GroupMember>),
    /// A multizone message type the session doesn't act on, once per type
    /// and session, for the log (C96).
    GroupMessage(String),
    Ended(SessionEnd),
}

/// What a session plays and where.
#[derive(Clone, Debug)]
pub struct SessionConfig {
    /// The device's addresses and port (C15, C16).
    pub addresses: Vec<SocketAddr>,
    /// The format to try first: WAV, or the one that last worked on this
    /// device (C84).
    pub format: StreamFormat,
    pub metadata: Metadata,
    /// PNG or JPEG artwork served before the first `LOAD`; its URL joins
    /// `metadata` (C53). Artwork the stream server refuses is left out.
    pub artwork: Option<Vec<u8>>,
    /// Set for a Cast group: every connect takes the addresses from its
    /// latest records instead of `addresses` (C90, C93), and the stream
    /// server answers its members too (C103).
    pub group: Option<GroupRecords>,
}

/// Whether a failed first `LOAD` in `codec` is retried in FLAC (C84):
/// `LOAD_FAILED`, or `IDLE` with `ERROR` before the first `PLAYING`, with
/// WAV.
pub fn falls_back_to_flac(codec: Codec, error: &MediaError) -> bool {
    codec == Codec::Wav
        && matches!(
            error,
            MediaError::LoadFailed { .. } | MediaError::Idle(IdleReason::Error)
        )
}

/// Connects, retrying while `selected` stays true: after 1 s, then doubling
/// up to 30 s (C80). `on_failure` hears each failure with the wait before
/// the next attempt. Errors no retry can fix return at once.
pub fn connect_retrying(
    addresses: &[SocketAddr],
    selected: &AtomicBool,
    on_failure: impl FnMut(ConnectError, Duration),
) -> Result<CastClient, Option<ConnectError>> {
    connect_retrying_with(|| addresses.to_vec(), selected, on_failure)
}

/// [`connect_retrying`] with the addresses read again before every
/// attempt, as a group's records may move while it retries (C90).
pub fn connect_retrying_with(
    mut addresses: impl FnMut() -> Vec<SocketAddr>,
    selected: &AtomicBool,
    mut on_failure: impl FnMut(ConnectError, Duration),
) -> Result<CastClient, Option<ConnectError>> {
    let mut backoff = Backoff::new();
    loop {
        if !selected.load(Ordering::SeqCst) {
            return Err(None);
        }
        match CastClient::connect_while(&addresses(), RequestIds::random(), selected) {
            Ok(client) => return Ok(client),
            Err(_) if !selected.load(Ordering::SeqCst) => return Err(None),
            Err(error @ ConnectError::NoAddress) => {
                return Err(Some(error));
            }
            Err(error) => {
                let wait = backoff.next_wait();
                on_failure(error, wait);
                let until = Instant::now() + wait;
                while Instant::now() < until {
                    if !selected.load(Ordering::SeqCst) {
                        return Err(None);
                    }
                    thread::sleep(SLICE.min(until.saturating_duration_since(Instant::now())));
                }
            }
        }
    }
}

/// One Cast session.
pub struct CastSession {
    client: CastClient,
    server: StreamServer,
    addresses: Vec<SocketAddr>,
    app: AppSession,
    load: LoadRequest,
    media_session_id: Option<i64>,
    selected: Arc<AtomicBool>,
    events: VecDeque<SessionEvent>,
    /// Takes the events of a recovery as they happen (C80, C81), which
    /// would otherwise wait in `events` until the recovery is over.
    recovery_sink: Option<EventSink>,
    recovering: bool,
    ended: bool,
    last_volume: Option<DeviceVolume>,
    last_media: Option<MediaStatus>,
    playing: bool,
    load_sent_frames: u64,
    load_dropped_frames: u64,
    rebuffers: u32,
    stalled: Duration,
    stall_since: Option<Instant>,
    next_diagnostics: Instant,
    error_recovered: bool,
    finished_reloaded: bool,
    /// Set by the host once it quits: every teardown from then on keeps to
    /// [`QUIT_BUDGET`] (C88).
    quitting: Arc<AtomicBool>,
    group: Option<GroupRecords>,
    members: MemberList,
    /// Multizone types already logged this session (C96).
    group_types_logged: Vec<String>,
}

fn teardown_budget(quitting: &AtomicBool) -> Duration {
    if quitting.load(Ordering::SeqCst) {
        QUIT_BUDGET
    } else {
        STOP_BUDGET
    }
}

impl fmt::Debug for CastSession {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CastSession")
            .field("client", &self.client)
            .field("server", &self.server)
            .finish()
    }
}

impl CastSession {
    /// Connects (retrying while `selected` stays true, C80), launches or
    /// reuses the Default Media Receiver, opens the stream server on the
    /// channel's local address and loads the stream until it plays.
    pub fn start(
        config: SessionConfig,
        reader: AudioReader,
        selected: Arc<AtomicBool>,
        on_failure: impl FnMut(ConnectError, Duration),
    ) -> Result<CastSession, SessionError> {
        let quitting = Arc::new(AtomicBool::new(false));
        CastSession::start_with(config, reader, selected, quitting, on_failure)
    }

    /// [`CastSession::start`] with the host's quit flag: once it is set,
    /// teardowns keep to [`QUIT_BUDGET`].
    pub fn start_with(
        config: SessionConfig,
        reader: AudioReader,
        selected: Arc<AtomicBool>,
        quitting: Arc<AtomicBool>,
        on_failure: impl FnMut(ConnectError, Duration),
    ) -> Result<CastSession, SessionError> {
        let group = config.group.clone();
        let connected = match &group {
            Some(records) => connect_retrying_with(|| records.get().group, &selected, on_failure),
            None => connect_retrying(&config.addresses, &selected, on_failure),
        };
        let mut client = match connected {
            Ok(client) => client,
            Err(error) => {
                return Err(SessionError::Connect(
                    error.unwrap_or(ConnectError::NoAddress),
                ));
            }
        };
        if group.is_some() {
            client.request_members();
        }
        client.set_selected(Arc::clone(&selected));
        let app = match client.launch() {
            Ok(app) => app,
            Err(error) => {
                client.stop(teardown_budget(&quitting));
                return Err(error.into());
            }
        };
        let allowed: Vec<IpAddr> = match &group {
            Some(_) => Vec::new(),
            None => {
                let mut allowed: Vec<IpAddr> =
                    config.addresses.iter().map(SocketAddr::ip).collect();
                allowed.push(client.address().ip());
                allowed.sort();
                allowed.dedup();
                allowed
            }
        };
        let stream_config = StreamConfig {
            local_ip: client.local_address().ip(),
            allowed,
            format: config.format,
        };
        let server = match StreamServer::start(stream_config, reader) {
            Ok(server) => server,
            Err(error) => {
                client.stop(teardown_budget(&quitting));
                return Err(error.into());
            }
        };
        if let Some(records) = &group {
            server.follow_group(records.clone());
            server.set_group_limit(1);
        }
        let mut addresses = vec![client.address()];
        addresses.extend(config.addresses.iter().filter(|a| **a != client.address()));
        let mut metadata = config.metadata;
        if let Some(url) = config
            .artwork
            .and_then(|bytes| server.set_artwork(bytes).ok())
        {
            metadata.images = vec![url];
        }
        let load = LoadRequest {
            url: server.url(),
            content_type: config.format.content_type().to_owned(),
            metadata,
        };
        let mut session = CastSession {
            client,
            server,
            addresses,
            app,
            load,
            media_session_id: None,
            selected,
            events: VecDeque::new(),
            recovery_sink: None,
            recovering: false,
            ended: false,
            last_volume: None,
            last_media: None,
            playing: false,
            load_sent_frames: 0,
            load_dropped_frames: 0,
            rebuffers: 0,
            stalled: Duration::ZERO,
            stall_since: None,
            next_diagnostics: Instant::now(),
            error_recovered: false,
            finished_reloaded: false,
            quitting,
            group,
            members: MemberList::new(),
            group_types_logged: Vec::new(),
        };
        if let Err(error) = session.load_with_fallback() {
            session.teardown(teardown_budget(&session.quitting));
            return Err(error.into());
        }
        Ok(session)
    }

    /// The format the device plays, after any fallback (C84).
    pub fn format(&self) -> StreamFormat {
        self.server.format()
    }

    /// The flag the host clears when the user deselects the device; retries
    /// stop with it (C80).
    pub fn selected(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.selected)
    }

    /// The stream URL sent as `contentId` (C55 to C57).
    pub fn stream_url(&self) -> &str {
        &self.load.url
    }

    pub fn media_session_id(&self) -> Option<i64> {
        self.media_session_id
    }

    pub fn app(&self) -> &AppSession {
        &self.app
    }

    pub fn is_ended(&self) -> bool {
        self.ended
    }

    pub fn poll_event(&mut self) -> Option<SessionEvent> {
        self.events.pop_front()
    }

    /// The device volume of the latest `RECEIVER_STATUS` (C69).
    pub fn volume(&self) -> Option<DeviceVolume> {
        self.client.status().map(|status| status.volume.clone())
    }

    /// Sets the device level (C65, C67, C70); the stream's samples are
    /// never scaled (C71).
    pub fn set_volume(&mut self, level: f64) -> Result<(), VolumeError> {
        self.client.set_volume(level)
    }

    pub fn set_muted(&mut self, muted: bool) -> Result<(), VolumeError> {
        self.client.set_muted(muted)
    }

    pub fn step_volume(&mut self, up: bool) -> Result<f64, VolumeError> {
        self.client.step_volume(up)
    }

    /// Keeps `metadata` for the next `LOAD` (a Resync); the playing stream
    /// is not reloaded for it (C54). Its artwork, if any, follows through
    /// [`CastSession::set_artwork`].
    pub fn set_metadata(&mut self, metadata: Metadata) {
        self.load.metadata = metadata;
    }

    /// Serves artwork for the metadata (C53) and puts its URL in the
    /// metadata of the next `LOAD`; returns the URL.
    pub fn set_artwork(&mut self, bytes: Vec<u8>) -> Result<String, StreamError> {
        let url = self.server.set_artwork(bytes)?;
        self.load.metadata.images = vec![url.clone()];
        Ok(url)
    }

    /// The sender's view of the session now (C78).
    pub fn diagnostics(&self) -> Diagnostics {
        let stats = self.server.stats();
        let rate = self.server.format().rate() as f64;
        let frames = |count: u64| Duration::from_secs_f64(count as f64 / rate);
        let frames_dropped = stats
            .frames_dropped
            .saturating_sub(self.load_dropped_frames);
        let stall = self.stalled
            + self
                .stall_since
                .map_or(Duration::ZERO, |since| since.elapsed());
        let drift = frames(frames_dropped) + stall;
        Diagnostics {
            sent: frames(stats.frames_sent.saturating_sub(self.load_sent_frames)),
            queued: frames(stats.frames_queued),
            current_time: self
                .last_media
                .as_ref()
                .and_then(|media| media.current_time),
            player_state: self
                .last_media
                .as_ref()
                .map(|media| media.player_state.clone()),
            frames_dropped,
            rebuffers: self.rebuffers,
            drift,
            resync_offered: drift > RESYNC_DRIFT,
        }
    }

    /// The user's Resync (C79): a new `LOAD` on the same app, which brings
    /// the latency back to a fresh start at the cost of a short gap. Never
    /// called by the sender itself.
    pub fn resync(&mut self) -> Result<(), MediaError> {
        if self.ended {
            return Err(MediaError::NotRunning);
        }
        self.load_until_playing()
    }

    /// Sends the events of every later recovery to `sink` as they happen:
    /// `Recovering`, each connect failure while the device stays
    /// unreachable, and `Recovered`. An event the sink hands back stays
    /// queued ahead of the later ones, which keeps the order; the sink gets
    /// it again with the next event, or [`CastSession::poll_event`] does.
    /// `Ended` always waits for `poll_event`.
    pub fn set_recovery_sink(&mut self, sink: EventSink) {
        self.recovery_sink = Some(sink);
    }

    /// Drives the session for `duration`: the channel, recovery and the
    /// per-second diagnostics.
    pub fn run_for(&mut self, duration: Duration) {
        let until = Instant::now() + duration;
        while !self.ended {
            let now = Instant::now();
            if now >= until {
                break;
            }
            self.client.run_for((until - now).min(SLICE));
            while let Some(event) = self.client.poll_event() {
                self.on_event(event);
                if self.ended {
                    return;
                }
            }
            self.collect_server_events();
            if self.playing && Instant::now() >= self.next_diagnostics {
                let diagnostics = self.diagnostics();
                self.report(SessionEvent::Diagnostics(diagnostics));
                self.next_diagnostics += DIAGNOSTICS_INTERVAL;
                if self.next_diagnostics < Instant::now() {
                    self.next_diagnostics = Instant::now() + DIAGNOSTICS_INTERVAL;
                }
            }
        }
    }

    /// The user stopped the stream (C87).
    pub fn stop(self) -> EndReason {
        let budget = teardown_budget(&self.quitting);
        self.finish(budget)
    }

    /// The host quits: the teardown of C87 within 3 s in total, then
    /// whatever is left is dropped (C88).
    pub fn quit(self) -> EndReason {
        self.finish(QUIT_BUDGET)
    }

    fn finish(mut self, budget: Duration) -> EndReason {
        self.teardown(budget)
    }

    fn teardown(&mut self, budget: Duration) -> EndReason {
        self.ended = true;
        let reason = self.client.stop_in_place(budget);
        self.server.stop_in_place();
        reason
    }

    fn report(&mut self, event: SessionEvent) {
        make_room(&mut self.events);
        self.events.push_back(event);
        if self.recovering {
            self.send_live();
        }
    }

    /// Hands the waiting events to the recovery sink in order, up to the
    /// first it refuses or an `Ended`, which the host takes by polling.
    fn send_live(&mut self) {
        let Some(sink) = self.recovery_sink.as_mut() else {
            return;
        };
        while let Some(event) = self.events.pop_front() {
            if matches!(event, SessionEvent::Ended(_)) {
                self.events.push_front(event);
                return;
            }
            if let Err(back) = sink(event) {
                self.events.push_front(back);
                return;
            }
        }
    }

    fn end(&mut self, end: SessionEnd) {
        if self.ended {
            return;
        }
        self.teardown(teardown_budget(&self.quitting));
        self.report(SessionEvent::Ended(end));
    }

    fn on_multizone(&mut self, message: MultizoneMessage) {
        if let MultizoneMessage::Other(kind) = &message {
            if !self.group_types_logged.contains(kind) {
                self.group_types_logged.push(kind.clone());
                self.report(SessionEvent::GroupMessage(kind.clone()));
            }
            return;
        }
        if self.members.apply(&message) {
            let members = self.members.members().unwrap_or_default().to_vec();
            self.server.set_group_limit(members.len() + 1);
            self.report(SessionEvent::Members(members));
        }
    }

    fn collect_server_events(&mut self) {
        while let Some(event) = self.server.poll_event() {
            self.report(SessionEvent::Stream(event));
        }
    }

    /// The first `LOAD`, and the one retry in FLAC that C84 allows.
    fn load_with_fallback(&mut self) -> Result<(), MediaError> {
        let error = match self.load_until_playing() {
            Ok(()) => return Ok(()),
            Err(error) => error,
        };
        let from = self.server.format().codec();
        if !falls_back_to_flac(from, &error) {
            return Err(error);
        }
        self.load.url = self.server.set_codec(Codec::Flac);
        self.load.content_type = self.server.format().content_type().to_owned();
        self.report(SessionEvent::FormatFallback {
            from,
            to: Codec::Flac,
            reason: error,
        });
        self.load_until_playing()
    }

    /// Sends `LOAD` and waits for `PLAYING` (C49), for the device's fetch
    /// (C85) and for failures (C83); resets the drift counters (C79).
    fn load_until_playing(&mut self) -> Result<(), MediaError> {
        self.playing = false;
        let fetches = self.server.stats().fetches;
        let media_session_id = self.client.load(&self.load)?;
        self.media_session_id = Some(media_session_id);
        let replied = Instant::now();
        loop {
            self.client.run_for(SLICE);
            while let Some(event) = self.client.poll_event() {
                match event {
                    Event::Playing { media_session_id } => {
                        self.media_session_id = Some(media_session_id);
                        self.started_playing();
                        return Ok(());
                    }
                    Event::LoadFailed(error) => return Err(error),
                    Event::Ended(reason) => return Err(MediaError::Ended(reason)),
                    other => self.on_event(other),
                }
            }
            self.collect_server_events();
            if let Some(reason) = self.client.end_reason() {
                return Err(MediaError::Ended(reason));
            }
            if self.server.stats().fetches == fetches && replied.elapsed() >= FETCH_TIMEOUT {
                return Err(MediaError::NoFetch);
            }
            if !self.selected.load(Ordering::SeqCst) {
                return Err(MediaError::Ended(EndReason::Stopped));
            }
        }
    }

    fn started_playing(&mut self) {
        let stats = self.server.stats();
        self.playing = true;
        self.load_sent_frames = stats.frames_sent;
        self.load_dropped_frames = stats.frames_dropped;
        self.rebuffers = 0;
        self.stalled = Duration::ZERO;
        self.stall_since = None;
        self.next_diagnostics = Instant::now() + DIAGNOSTICS_INTERVAL;
        self.report(SessionEvent::Playing);
    }

    fn on_event(&mut self, event: Event) {
        match event {
            Event::Status(status) => {
                if self.last_volume.as_ref() != Some(&status.volume) {
                    self.last_volume = Some(status.volume.clone());
                    self.report(SessionEvent::Volume(status.volume));
                }
            }
            Event::MediaStatus(status) => self.last_media = Some(status),
            Event::Multizone(message) => self.on_multizone(message),
            Event::Rebuffering => {
                self.rebuffers += 1;
                self.stall_since = Some(Instant::now());
                self.report(SessionEvent::Rebuffering);
            }
            Event::Resumed => {
                if let Some(since) = self.stall_since.take() {
                    self.stalled += since.elapsed();
                }
            }
            Event::PlaybackEnded(PlaybackEnd::Replaced) => self.end(SessionEnd::TakenOver),
            Event::PlaybackEnded(PlaybackEnd::Error(reason)) => {
                if self.error_recovered {
                    let reason = reason.unwrap_or(IdleReason::Error);
                    self.end(SessionEnd::Failed(MediaError::Idle(reason).into()));
                } else {
                    self.error_recovered = true;
                    self.recover(EndReason::Io("playback error".to_owned()), true);
                }
            }
            Event::PlaybackEnded(PlaybackEnd::Finished) => {
                if self.finished_reloaded {
                    self.end(SessionEnd::Failed(
                        MediaError::Idle(IdleReason::Finished).into(),
                    ));
                } else {
                    self.finished_reloaded = true;
                    match self.load_until_playing() {
                        Ok(()) => self.report(SessionEvent::Reloaded),
                        Err(error) => self.end(SessionEnd::Failed(error.into())),
                    }
                }
            }
            Event::Ended(reason) => match reason {
                EndReason::TakenOver => self.end(SessionEnd::TakenOver),
                EndReason::Stopped => self.end(SessionEnd::Stopped),
                EndReason::AppClosed => self.end(SessionEnd::Failed(SessionError::Launch(
                    LaunchError::Ended(EndReason::AppClosed),
                ))),
                other => self.recover(other, false),
            },
            _ => {}
        }
    }

    /// C81: one reconnect at once; the running app is picked back up, or
    /// launched and loaded again. A failed reconnect retries as C80 while
    /// the device stays selected. `reload` loads the stream again even when
    /// the app still runs, after a playback error (C86).
    fn recover(&mut self, reason: EndReason, reload: bool) {
        // A recovery inside another's load must not end the outer one's
        // live events.
        let was = std::mem::replace(&mut self.recovering, true);
        self.recover_now(reason, reload);
        self.recovering = was;
    }

    fn recover_now(&mut self, reason: EndReason, reload: bool) {
        self.report(SessionEvent::Recovering(reason));
        self.playing = false;
        // One connection per device at a time; the first attempt of the
        // C80 loop is the reconnect at once.
        self.client.disconnect();
        while self.client.poll_event().is_some() {}
        let addresses = self.addresses.clone();
        let selected = Arc::clone(&self.selected);
        let group = self.group.clone();
        let on_failure = |error, retry_in| {
            self.report(SessionEvent::ConnectFailed { error, retry_in });
        };
        // A group is resolved afresh: its leader may have handed over (C90,
        // C97).
        let result = match &group {
            Some(records) => connect_retrying_with(|| records.get().group, &selected, on_failure),
            None => connect_retrying(&addresses, &selected, on_failure),
        };
        self.client = match result {
            Ok(mut client) => {
                client.set_selected(Arc::clone(&self.selected));
                if group.is_some() {
                    client.request_members();
                }
                client
            }
            Err(None) => return self.end(SessionEnd::Deselected),
            Err(Some(error)) => return self.end(SessionEnd::Failed(error.into())),
        };
        let resumed = if reload {
            self.client.resume(&self.app, None, &self.load.url)
        } else {
            self.client
                .resume(&self.app, self.media_session_id, &self.load.url)
        };
        let result = if resumed && !reload {
            self.playing = true;
            self.next_diagnostics = Instant::now() + DIAGNOSTICS_INTERVAL;
            Ok(false)
        } else if resumed {
            self.load_until_playing()
                .map(|()| false)
                .map_err(SessionError::from)
        } else {
            match self.client.launch() {
                Ok(app) => {
                    self.app = app;
                    self.load_until_playing()
                        .map(|()| true)
                        .map_err(SessionError::from)
                }
                Err(error) => Err(error.into()),
            }
        };
        match result {
            Ok(relaunched) => self.report(SessionEvent::Recovered { relaunched }),
            Err(_) if !self.selected.load(Ordering::SeqCst) => self.end(SessionEnd::Deselected),
            Err(error) => self.end(SessionEnd::Failed(error)),
        }
    }
}

/// Frees a place in a full event queue: the oldest routine event goes
/// first, then the oldest connect failure, which a long outage repeats.
fn make_room(events: &mut VecDeque<SessionEvent>) {
    if events.len() < MAX_SESSION_EVENTS {
        return;
    }
    let routine = events.iter().position(|event| {
        matches!(
            event,
            SessionEvent::Diagnostics(_)
                | SessionEvent::Stream(_)
                | SessionEvent::Volume(_)
                | SessionEvent::GroupMessage(_)
        )
    });
    let oldest = routine.or_else(|| {
        events
            .iter()
            .position(|event| matches!(event, SessionEvent::ConnectFailed { .. }))
    });
    if let Some(index) = oldest {
        events.remove(index);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn failure() -> SessionEvent {
        SessionEvent::ConnectFailed {
            error: ConnectError::NoStatus,
            retry_in: Duration::from_secs(30),
        }
    }

    #[test]
    fn a_long_outage_keeps_the_queue_bounded_and_its_other_events() {
        let mut events = VecDeque::new();
        events.push_back(SessionEvent::Recovering(EndReason::Silent));
        for _ in 0..MAX_SESSION_EVENTS * 3 {
            make_room(&mut events);
            events.push_back(failure());
        }
        assert_eq!(events.len(), MAX_SESSION_EVENTS);
        assert!(matches!(events.front(), Some(SessionEvent::Recovering(_))));

        make_room(&mut events);
        events.push_back(SessionEvent::Ended(SessionEnd::Deselected));
        assert_eq!(events.len(), MAX_SESSION_EVENTS);
        assert!(matches!(events.front(), Some(SessionEvent::Recovering(_))));
        assert!(matches!(
            events.back(),
            Some(SessionEvent::Ended(SessionEnd::Deselected))
        ));
    }

    #[test]
    fn routine_events_make_room_before_connect_failures() {
        let mut events: VecDeque<SessionEvent> =
            (0..MAX_SESSION_EVENTS - 1).map(|_| failure()).collect();
        events.insert(5, SessionEvent::GroupMessage("x".to_owned()));
        make_room(&mut events);
        assert_eq!(events.len(), MAX_SESSION_EVENTS - 1);
        assert!(
            events
                .iter()
                .all(|event| matches!(event, SessionEvent::ConnectFailed { .. }))
        );
    }
}
