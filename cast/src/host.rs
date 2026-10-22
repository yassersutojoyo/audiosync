//! The host apps' interface to the sender: [`CastHost`].
//!
//! Every Cast network call (connect, launch, `LOAD`, volume, Resync,
//! teardown) runs on one worker thread that owns the session. The host's
//! UI thread only sends commands and polls [`HostEvent`]s; none of its
//! calls waits on the network, and the only lock it takes is held for a
//! copy. A Cast failure arrives as an event and touches nothing outside
//! the sender (C7).
//!
//! Use from a host:
//!
//! 1. [`CastHost::new`] with the [`DeviceMemory`] the host stored last time.
//! 2. [`CastHost::start`] with the device, the stream's rate, the metadata
//!    and the reading end of an [`audio_ring`](crate::audio::audio_ring);
//!    the capture keeps writing into the other end. A second device is
//!    refused while one runs (C2).
//! 3. Each UI tick, drain [`CastHost::poll_event`]. Every event of an
//!    attempt carries the [`AttemptId`] that `start` returned, and every
//!    attempt ends with exactly one [`HostEvent::Ended`];
//!    [`HostEvent::MemoryChanged`] says when to store [`CastHost::memory`].
//! 4. [`CastHost::stop`] when the user stops or deselects the device; the
//!    connect retries and the waits for the launch and `PLAYING` stop with
//!    it.
//! 5. [`CastHost::shutdown`] when the app quits (C88).

use std::collections::{BTreeMap, VecDeque};
use std::fmt;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{
    self, Receiver, RecvTimeoutError, Sender, SyncSender, TryRecvError, TrySendError,
};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use serde_json::{Map, Value, json};

use crate::audio::{AudioReader, Codec, StreamFormat};
use crate::client::QUIT_BUDGET;
use crate::discovery::DeviceId;
use crate::group::GroupRecords;
use crate::media::{IdleReason, MediaError, Metadata};
use crate::session::{CastSession, SessionConfig, SessionEnd, SessionError, SessionEvent};

/// How long the worker drives a session between two looks at the command
/// queue.
const TICK: Duration = Duration::from_millis(50);
/// Events waiting for a host that doesn't poll.
pub const MAX_HOST_EVENTS: usize = 256;
/// What [`CastHost::shutdown`] waits beyond the teardown budget of C88.
pub const SHUTDOWN_MARGIN: Duration = Duration::from_millis(500);

/// How a Cast output relates to the host's other outputs (C73, C77).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OutputSync {
    /// Kept in step only by delaying the other outputs by the device's
    /// measured latency; never sample-accurate.
    MeasuredOffset,
}

/// What every Cast output is: the protocol has no shared clock (C73).
pub const OUTPUT_SYNC: OutputSync = OutputSync::MeasuredOffset;

/// The longest stored offset: the host's ceiling for holding local outputs
/// back, so a damaged file can't make them wait longer.
pub const MAX_OFFSET: Duration = Duration::from_secs(20);

/// What the host stores per device `id` between runs: the format that last
/// worked (C84) and the measured offset (C77). Only the user, through the
/// host, changes an offset; the sender never corrects one on its own.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DeviceMemory {
    formats: BTreeMap<DeviceId, Codec>,
    offsets: BTreeMap<DeviceId, Duration>,
}

impl DeviceMemory {
    /// The codec to try first on `device`: the one that worked last, else
    /// WAV.
    pub fn format(&self, device: &DeviceId) -> Codec {
        self.formats.get(device).copied().unwrap_or(Codec::Wav)
    }

    pub fn offset(&self, device: &DeviceId) -> Option<Duration> {
        self.offsets.get(device).copied()
    }

    pub fn set_offset(&mut self, device: DeviceId, offset: Duration) {
        self.offsets.insert(device, offset);
    }

    fn remember_format(&mut self, device: DeviceId, codec: Codec) -> bool {
        let before = self.format(&device);
        if codec == Codec::Wav {
            self.formats.remove(&device);
        } else {
            self.formats.insert(device, codec);
        }
        before != codec
    }

    /// A JSON object keyed by device `id` in hex, each entry with an
    /// optional `format` and `offsetMs`.
    pub fn to_json(&self) -> String {
        let mut entries = Map::new();
        for (device, codec) in &self.formats {
            entries.insert(device.to_string(), json!({"format": codec.name()}));
        }
        for (device, offset) in &self.offsets {
            let entry = entries
                .entry(device.to_string())
                .or_insert_with(|| json!({}));
            entry["offsetMs"] = json!(offset.as_millis() as u64);
        }
        Value::Object(entries).to_string()
    }

    /// Reads [`DeviceMemory::to_json`]; entries it can't read are skipped,
    /// so a damaged file costs only those devices, and an offset past
    /// [`MAX_OFFSET`] reads as that.
    pub fn from_json(text: &str) -> DeviceMemory {
        let mut memory = DeviceMemory::default();
        let Ok(Value::Object(entries)) = serde_json::from_str::<Value>(text) else {
            return memory;
        };
        for (key, entry) in entries {
            let Some(device) = DeviceId::parse(&key) else {
                continue;
            };
            if let Some(codec) = entry["format"].as_str().and_then(Codec::from_name)
                && codec != Codec::Wav
            {
                memory.formats.insert(device, codec);
            }
            if let Some(ms) = entry["offsetMs"].as_u64() {
                memory
                    .offsets
                    .insert(device, Duration::from_millis(ms).min(MAX_OFFSET));
            }
        }
        memory
    }
}

/// The device a session plays on.
#[derive(Clone, Debug, PartialEq)]
pub struct CastTarget {
    pub id: DeviceId,
    /// Where to connect, as the device list has them (C15, C16).
    pub addresses: Vec<SocketAddr>,
    /// Set when the target is a Cast group, which plays as one device
    /// (C3, C93); the host keeps it current from discovery (C90, C103).
    pub group: Option<GroupRecords>,
}

/// Why [`CastHost::start`] refused at once, before any network call.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StartRefused {
    /// A session on `playing` runs or starts; one Cast device at a time
    /// (C2).
    Busy { playing: DeviceId },
    /// A rate other than 44 100 or 48 000 Hz (C63).
    Rate(u32),
    /// The worker is gone (after [`CastHost::shutdown`]).
    Closed,
}

impl fmt::Display for StartRefused {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StartRefused::Busy { .. } => {
                f.write_str("another Cast device is playing; stop it before starting this one")
            }
            StartRefused::Rate(rate) => write!(f, "a Cast stream can't run at {rate} Hz"),
            StartRefused::Closed => f.write_str("Cast is shutting down"),
        }
    }
}

impl std::error::Error for StartRefused {}

/// A request the worker could not carry out.
#[derive(Debug)]
pub enum CommandError {
    /// No session runs.
    NoSession,
    Volume(crate::media::VolumeError),
    Media(MediaError),
    Stream(crate::stream::StreamError),
}

impl fmt::Display for CommandError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CommandError::NoSession => f.write_str("no Cast device is playing"),
            CommandError::Volume(error) => error.fmt(f),
            CommandError::Media(error) => error.fmt(f),
            CommandError::Stream(error) => error.fmt(f),
        }
    }
}

/// One call of [`CastHost::start`]; every event of that attempt carries it,
/// so an old attempt's events never pass for a newer one on the same
/// device.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct AttemptId(pub u64);

/// What the worker reports to the host.
#[derive(Debug)]
pub enum HostEvent {
    /// Everything the session reports while it starts and runs: connect
    /// retries, the format fallback, `Playing`, diagnostics, volume.
    Session {
        attempt: AttemptId,
        device: DeviceId,
        event: SessionEvent,
    },
    /// The device plays the stream in `format`.
    Started {
        attempt: AttemptId,
        device: DeviceId,
        format: StreamFormat,
    },
    /// The attempt is over; exactly one per attempt, its last event.
    Ended {
        attempt: AttemptId,
        device: DeviceId,
        end: SessionEnd,
    },
    /// A volume, metadata, artwork or Resync request failed; `attempt` is
    /// `None` when no session ran.
    CommandFailed {
        attempt: Option<AttemptId>,
        error: CommandError,
    },
    /// [`CastHost::memory`] changed and should be stored.
    MemoryChanged,
}

impl HostEvent {
    /// Events that may be dropped when the host falls behind: a newer one
    /// of the same kind follows.
    fn is_routine(&self) -> bool {
        match self {
            HostEvent::Session { event, .. } => matches!(
                event,
                SessionEvent::Diagnostics(_)
                    | SessionEvent::Stream(_)
                    | SessionEvent::Volume(_)
                    | SessionEvent::Rebuffering
                    | SessionEvent::GroupMessage(_)
            ),
            HostEvent::CommandFailed { .. } => true,
            _ => false,
        }
    }
}

enum Command {
    Start {
        attempt: AttemptId,
        target: CastTarget,
        format: StreamFormat,
        metadata: Metadata,
        artwork: Option<Vec<u8>>,
        reader: AudioReader,
        selected: Arc<AtomicBool>,
    },
    Stop,
    SetVolume(f64),
    SetMuted(bool),
    StepVolume(bool),
    SetMetadata(Metadata),
    SetArtwork(Vec<u8>),
    Resync,
    Quit,
}

struct Active {
    attempt: AttemptId,
    device: DeviceId,
    selected: Arc<AtomicBool>,
}

struct Shared {
    active: Mutex<Option<Active>>,
    memory: Mutex<DeviceMemory>,
    /// Set before the quit command is sent: teardowns from then on keep to
    /// C88's budget.
    quitting: Arc<AtomicBool>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Shared {
    /// Frees the slot when it still belongs to `attempt`.
    fn release(&self, attempt: AttemptId) {
        let mut active = lock(&self.active);
        if active.as_ref().map(|a| a.attempt) == Some(attempt) {
            *active = None;
        }
    }
}

/// The sender as the host's UI thread sees it. See the module docs.
pub struct CastHost {
    commands: Sender<Command>,
    events: Receiver<HostEvent>,
    shared: Arc<Shared>,
    next_attempt: u64,
    worker: Option<JoinHandle<()>>,
}

impl fmt::Debug for CastHost {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CastHost")
            .field("playing", &self.playing())
            .finish()
    }
}

impl CastHost {
    /// Starts the worker thread. `memory` is what the host stored from
    /// [`CastHost::memory`] last time, or the default.
    pub fn new(memory: DeviceMemory) -> std::io::Result<CastHost> {
        let (commands, inbox) = mpsc::channel();
        let (outbox, events) = mpsc::sync_channel(MAX_HOST_EVENTS);
        let shared = Arc::new(Shared {
            active: Mutex::new(None),
            memory: Mutex::new(memory),
            quitting: Arc::new(AtomicBool::new(false)),
        });
        let worker = {
            let shared = Arc::clone(&shared);
            thread::Builder::new()
                .name("cast-host".to_owned())
                .spawn(move || Worker::new(inbox, outbox, shared).run())?
        };
        Ok(CastHost {
            commands,
            events,
            shared,
            next_attempt: 0,
            worker: Some(worker),
        })
    }

    /// Starts a session on `target` in the background, trying the format
    /// that last worked there first (C84). Refused at once while another
    /// session runs or starts (C2).
    pub fn start(
        &mut self,
        target: CastTarget,
        rate: u32,
        metadata: Metadata,
        reader: AudioReader,
    ) -> Result<AttemptId, StartRefused> {
        self.start_with_artwork(target, rate, metadata, None, reader)
    }

    /// [`CastHost::start`] with PNG or JPEG artwork that the first `LOAD`
    /// already names (C53).
    pub fn start_with_artwork(
        &mut self,
        target: CastTarget,
        rate: u32,
        metadata: Metadata,
        artwork: Option<Vec<u8>>,
        reader: AudioReader,
    ) -> Result<AttemptId, StartRefused> {
        if self.worker.is_none() {
            return Err(StartRefused::Closed);
        }
        let codec = lock(&self.shared.memory).format(&target.id);
        let format = StreamFormat::new(codec, rate).ok_or(StartRefused::Rate(rate))?;
        let selected = Arc::new(AtomicBool::new(true));
        let attempt = AttemptId(self.next_attempt);
        {
            let mut active = lock(&self.shared.active);
            if let Some(active) = active.as_ref() {
                return Err(StartRefused::Busy {
                    playing: active.device,
                });
            }
            *active = Some(Active {
                attempt,
                device: target.id,
                selected: Arc::clone(&selected),
            });
        }
        self.next_attempt += 1;
        let command = Command::Start {
            attempt,
            target,
            format,
            metadata,
            artwork,
            reader,
            selected,
        };
        if self.commands.send(command).is_err() {
            self.shared.release(attempt);
            return Err(StartRefused::Closed);
        }
        Ok(attempt)
    }

    /// Whether the worker still runs: false after [`CastHost::shutdown`],
    /// and when a panic ended it, which no event reports.
    pub fn is_running(&self) -> bool {
        self.worker
            .as_ref()
            .is_some_and(|worker| !worker.is_finished())
    }

    /// The device of the session that runs or starts, if any.
    pub fn playing(&self) -> Option<DeviceId> {
        lock(&self.shared.active).as_ref().map(|a| a.device)
    }

    /// Stops the session (C87), or the attempt to start one: connect
    /// retries (C80) and the waits for the launch, the `LOAD` reply and
    /// `PLAYING` end at once. The slot is free for another device right
    /// away; the worker tears this session down before it starts the next
    /// one.
    pub fn stop(&self) {
        self.deselect();
        let _ = self.commands.send(Command::Stop);
    }

    fn deselect(&self) {
        if let Some(active) = lock(&self.shared.active).take() {
            active.selected.store(false, Ordering::SeqCst);
        }
    }

    /// The device level, 0.0 to 1.0 (C65, C67, C70).
    pub fn set_volume(&self, level: f64) {
        let _ = self.commands.send(Command::SetVolume(level));
    }

    pub fn set_muted(&self, muted: bool) {
        let _ = self.commands.send(Command::SetMuted(muted));
    }

    /// One `stepInterval` up or down (C69).
    pub fn step_volume(&self, up: bool) {
        let _ = self.commands.send(Command::StepVolume(up));
    }

    /// Kept for the next `LOAD`; the playing stream is not reloaded (C54).
    pub fn set_metadata(&self, metadata: Metadata) {
        let _ = self.commands.send(Command::SetMetadata(metadata));
    }

    /// Serves PNG or JPEG artwork (C53) for the metadata of the next
    /// `LOAD`; send it after the [`CastHost::set_metadata`] it belongs to,
    /// which drops the artwork of the track before.
    pub fn set_artwork(&self, bytes: Vec<u8>) {
        let _ = self.commands.send(Command::SetArtwork(bytes));
    }

    /// The user's Resync (C79).
    pub fn resync(&self) {
        let _ = self.commands.send(Command::Resync);
    }

    /// What the host should store per device (C77, C84).
    pub fn memory(&self) -> DeviceMemory {
        lock(&self.shared.memory).clone()
    }

    /// The user's measured offset for `device` (C77).
    pub fn set_offset(&self, device: DeviceId, offset: Duration) {
        lock(&self.shared.memory).set_offset(device, offset);
    }

    /// The next event. At most [`MAX_HOST_EVENTS`] wait; beyond that,
    /// routine ones (diagnostics, volume, stream log, failed commands) are
    /// dropped, never `Started`, `Ended` or `MemoryChanged`.
    pub fn poll_event(&self) -> Option<HostEvent> {
        self.events.try_recv().ok()
    }

    /// The app quits: the teardown of C88 within its 3 s and a margin,
    /// whatever runs or starts; what is left after that is dropped. The one
    /// call that waits, at most that long, so call it from the app's quit
    /// path only. Returns whether the worker finished in time. The last
    /// events, an `Ended` among them, can still be polled afterwards.
    pub fn shutdown(&mut self) -> bool {
        self.quit();
        let Some(worker) = self.worker.take() else {
            return true;
        };
        let until = Instant::now() + QUIT_BUDGET + SHUTDOWN_MARGIN;
        while !worker.is_finished() && Instant::now() < until {
            thread::sleep(Duration::from_millis(10));
        }
        let finished = worker.is_finished();
        if finished {
            let _ = worker.join();
        }
        finished
    }

    fn quit(&self) {
        self.shared.quitting.store(true, Ordering::SeqCst);
        self.deselect();
        let _ = self.commands.send(Command::Quit);
    }
}

impl Drop for CastHost {
    /// Asks the worker to quit and does not wait for it.
    fn drop(&mut self) {
        if self.worker.is_some() {
            self.quit();
        }
    }
}

struct Running {
    attempt: AttemptId,
    device: DeviceId,
    session: CastSession,
}

struct Worker {
    inbox: Receiver<Command>,
    outbox: SyncSender<HostEvent>,
    /// Events that must not be dropped, waiting for room in the outbox.
    held: VecDeque<HostEvent>,
    shared: Arc<Shared>,
    running: Option<Running>,
}

impl Worker {
    fn new(inbox: Receiver<Command>, outbox: SyncSender<HostEvent>, shared: Arc<Shared>) -> Worker {
        Worker {
            inbox,
            outbox,
            held: VecDeque::new(),
            shared,
            running: None,
        }
    }

    fn emit(&mut self, event: HostEvent) {
        self.flush();
        if event.is_routine() {
            // Held events go first; a routine one behind them is stale by
            // the time there is room.
            if self.held.is_empty() {
                let _ = self.outbox.try_send(event);
            }
            return;
        }
        if self.held.len() >= MAX_HOST_EVENTS {
            let oldest_session = self
                .held
                .iter()
                .position(|held| matches!(held, HostEvent::Session { .. }));
            if let Some(index) = oldest_session {
                self.held.remove(index);
            }
        }
        self.held.push_back(event);
        self.flush();
    }

    fn flush(&mut self) {
        while let Some(event) = self.held.pop_front() {
            match self.outbox.try_send(event) {
                Ok(()) => {}
                Err(TrySendError::Full(event)) => {
                    self.held.push_front(event);
                    return;
                }
                Err(TrySendError::Disconnected(_)) => {
                    self.held.clear();
                    return;
                }
            }
        }
    }

    fn quitting(&self) -> bool {
        self.shared.quitting.load(Ordering::SeqCst)
    }

    fn run(mut self) {
        loop {
            self.flush();
            let command = if self.running.is_some() {
                match self.inbox.try_recv() {
                    Ok(command) => Some(command),
                    Err(TryRecvError::Empty) => None,
                    Err(TryRecvError::Disconnected) => Some(Command::Quit),
                }
            } else {
                let wait = if self.held.is_empty() {
                    Duration::from_secs(1)
                } else {
                    TICK
                };
                match self.inbox.recv_timeout(wait) {
                    Ok(command) => Some(command),
                    Err(RecvTimeoutError::Timeout) => continue,
                    Err(RecvTimeoutError::Disconnected) => Some(Command::Quit),
                }
            };
            match command {
                Some(Command::Quit) => {
                    self.stop_running();
                    self.flush();
                    return;
                }
                Some(command) => {
                    self.handle(command);
                    continue;
                }
                None => {}
            }
            self.drive();
        }
    }

    /// Tears the running session down: C87, or within C88's budget once
    /// the host quits.
    fn stop_running(&mut self) {
        if let Some(running) = self.running.take() {
            if self.quitting() {
                running.session.quit();
            } else {
                running.session.stop();
            }
            self.ended(running.attempt, running.device, SessionEnd::Stopped);
        }
    }

    fn handle(&mut self, command: Command) {
        match command {
            Command::Start {
                attempt,
                target,
                format,
                metadata,
                artwork,
                reader,
                selected,
            } => self.start(
                attempt,
                target,
                format,
                (metadata, artwork),
                reader,
                selected,
            ),
            Command::Stop => self.stop_running(),
            Command::SetVolume(level) => {
                self.with_session(|session| session.set_volume(level).map_err(CommandError::Volume))
            }
            Command::SetMuted(muted) => {
                self.with_session(|session| session.set_muted(muted).map_err(CommandError::Volume))
            }
            Command::StepVolume(up) => self.with_session(|session| {
                session
                    .step_volume(up)
                    .map(|_| ())
                    .map_err(CommandError::Volume)
            }),
            Command::SetMetadata(metadata) => self.with_session(|session| {
                session.set_metadata(metadata);
                Ok(())
            }),
            Command::SetArtwork(bytes) => self.with_session(|session| {
                session
                    .set_artwork(bytes)
                    .map(|_| ())
                    .map_err(CommandError::Stream)
            }),
            Command::Resync => {
                self.with_session(|session| session.resync().map_err(CommandError::Media))
            }
            Command::Quit => {}
        }
    }

    fn with_session(&mut self, action: impl FnOnce(&mut CastSession) -> Result<(), CommandError>) {
        let Some(running) = self.running.as_mut() else {
            return self.emit(HostEvent::CommandFailed {
                attempt: None,
                error: CommandError::NoSession,
            });
        };
        let attempt = running.attempt;
        if let Err(error) = action(&mut running.session) {
            self.emit(HostEvent::CommandFailed {
                attempt: Some(attempt),
                error,
            });
        }
        self.forward_session_events();
    }

    fn start(
        &mut self,
        attempt: AttemptId,
        target: CastTarget,
        format: StreamFormat,
        (metadata, artwork): (Metadata, Option<Vec<u8>>),
        reader: AudioReader,
        selected: Arc<AtomicBool>,
    ) {
        let device = target.id;
        if !selected.load(Ordering::SeqCst) {
            return self.ended(attempt, device, SessionEnd::Deselected);
        }
        let config = SessionConfig {
            addresses: target.addresses,
            format,
            metadata,
            artwork,
            group: target.group,
        };
        // Connect retries go out live, while the start still runs; one that
        // finds the outbox full, or held events not yet sent, is dropped
        // rather than block the worker or overtake them.
        let outbox = self.outbox.clone();
        let in_order = self.held.is_empty();
        let quitting = Arc::clone(&self.shared.quitting);
        let result = CastSession::start_with(
            config,
            reader,
            Arc::clone(&selected),
            quitting,
            |error, retry_in| {
                if !in_order {
                    return;
                }
                let event = SessionEvent::ConnectFailed { error, retry_in };
                let _ = outbox.try_send(HostEvent::Session {
                    attempt,
                    device,
                    event,
                });
            },
        );
        match result {
            Ok(mut session) => {
                let format = session.format();
                if lock(&self.shared.memory).remember_format(device, format.codec()) {
                    self.emit(HostEvent::MemoryChanged);
                }
                while let Some(event) = session.poll_event() {
                    self.emit(HostEvent::Session {
                        attempt,
                        device,
                        event,
                    });
                }
                self.emit(HostEvent::Started {
                    attempt,
                    device,
                    format,
                });
                self.running = Some(Running {
                    attempt,
                    device,
                    session,
                });
                if !selected.load(Ordering::SeqCst) {
                    self.stop_running();
                }
            }
            Err(_) if !selected.load(Ordering::SeqCst) => {
                self.ended(attempt, device, SessionEnd::Deselected);
            }
            Err(error) => {
                // A remembered FLAC the device now refuses is forgotten, so
                // the next start begins with WAV again.
                let refused = matches!(
                    error,
                    SessionError::Media(
                        MediaError::LoadFailed { .. } | MediaError::Idle(IdleReason::Error)
                    )
                );
                if refused
                    && format.codec() != Codec::Wav
                    && lock(&self.shared.memory).remember_format(device, Codec::Wav)
                {
                    self.emit(HostEvent::MemoryChanged);
                }
                self.ended(attempt, device, SessionEnd::Failed(error));
            }
        }
    }

    fn drive(&mut self) {
        let Some(running) = self.running.as_mut() else {
            return;
        };
        running.session.run_for(TICK);
        self.forward_session_events();
    }

    fn forward_session_events(&mut self) {
        let Some(mut running) = self.running.take() else {
            return;
        };
        let mut end = None;
        while let Some(event) = running.session.poll_event() {
            match event {
                SessionEvent::Ended(session_end) => end = Some(session_end),
                event => self.emit(HostEvent::Session {
                    attempt: running.attempt,
                    device: running.device,
                    event,
                }),
            }
        }
        match end {
            Some(end) => self.ended(running.attempt, running.device, end),
            None if running.session.is_ended() => {
                self.ended(running.attempt, running.device, SessionEnd::Stopped);
            }
            None => self.running = Some(running),
        }
    }

    fn ended(&mut self, attempt: AttemptId, device: DeviceId, end: SessionEnd) {
        self.shared.release(attempt);
        self.emit(HostEvent::Ended {
            attempt,
            device,
            end,
        });
    }
}
