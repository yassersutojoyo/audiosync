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
use std::panic::{self, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{
    self, Receiver, RecvTimeoutError, Sender, SyncSender, TryRecvError, TrySendError,
};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use serde_json::{Map, Value, json};

use crate::audio::{AudioReader, Codec, StreamFormat};
use crate::client::{FingerprintBook, QUIT_BUDGET};
use crate::discovery::DeviceId;
use crate::group::GroupRecords;
use crate::media::{IdleReason, MediaError, Metadata};
use crate::session::{
    CastSession, EventSink, Fingerprints, SessionConfig, SessionEnd, SessionError, SessionEvent,
};

/// How long the worker drives a session between two looks at the command
/// queue.
const TICK: Duration = Duration::from_millis(50);
/// A panic within this long of the one before counts as recurring (C107).
const PANIC_WINDOW: Duration = Duration::from_secs(1);
/// The first and the longest pause after a recurring panic.
const PANIC_PAUSE: Duration = Duration::from_millis(100);
const MAX_PANIC_PAUSE: Duration = Duration::from_secs(5);
const PANIC_PAUSE_SLICE: Duration = Duration::from_millis(50);
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

    /// Stores `offset` capped at [`MAX_OFFSET`], as a restart reads it.
    pub fn set_offset(&mut self, device: DeviceId, offset: Duration) {
        self.offsets.insert(device, offset.min(MAX_OFFSET));
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
        DeviceMemory::read_json(text).unwrap_or_default()
    }

    /// [`DeviceMemory::from_json`], or `None` when `text` isn't a JSON
    /// object at all, so a host can keep that file before it saves over it.
    /// An object whose entries are all damaged reads as an empty memory,
    /// not `None`: its entries are lost at the next save.
    pub fn read_json(text: &str) -> Option<DeviceMemory> {
        let mut memory = DeviceMemory::default();
        let Ok(Value::Object(entries)) = serde_json::from_str::<Value>(text) else {
            return None;
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
        Some(memory)
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
        announced: Arc<AtomicBool>,
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
    announced: Arc<AtomicBool>,
}

struct Shared {
    active: Mutex<Option<Active>>,
    memory: Mutex<DeviceMemory>,
    /// Set before the quit command is sent: teardowns from then on keep to
    /// C88's budget.
    quitting: Arc<AtomicBool>,
    /// How many of the next starts make the worker panic.
    #[cfg(test)]
    panic_in_start: std::sync::atomic::AtomicU32,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Sends held events in order until the outbox is full.
fn flush(held: &mut VecDeque<HostEvent>, outbox: &SyncSender<HostEvent>) {
    while let Some(event) = held.pop_front() {
        match outbox.try_send(event) {
            Ok(()) => {}
            Err(TrySendError::Full(event)) => {
                held.push_front(event);
                return;
            }
            Err(TrySendError::Disconnected(_)) => {
                held.clear();
                return;
            }
        }
    }
}

/// Sends `event` live from outside the worker loop, after the held events;
/// it comes back when they or a full outbox are still in the way.
fn send_after_held(
    held: &Mutex<VecDeque<HostEvent>>,
    outbox: &SyncSender<HostEvent>,
    event: HostEvent,
) -> Option<HostEvent> {
    let mut held = lock(held);
    flush(&mut held, outbox);
    if !held.is_empty() {
        return Some(event);
    }
    match outbox.try_send(event) {
        Err(TrySendError::Full(event)) => Some(event),
        _ => None,
    }
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
            #[cfg(test)]
            panic_in_start: std::sync::atomic::AtomicU32::new(0),
        });
        let worker = {
            let shared = Arc::clone(&shared);
            thread::Builder::new()
                .name("cast-host".to_owned())
                .spawn(move || {
                    let mut worker = Worker::new(inbox, outbox, shared);
                    let mut pause = Duration::ZERO;
                    let mut last_panic: Option<Instant> = None;
                    while panic::catch_unwind(AssertUnwindSafe(|| worker.run())).is_err() {
                        worker.recover();
                        if worker.quitting() {
                            worker.flush();
                            break;
                        }
                        // A fault that recurs at once would otherwise spin the
                        // worker at full speed.
                        let recurring = last_panic.is_some_and(|at| at.elapsed() < PANIC_WINDOW);
                        pause = if recurring {
                            (pause * 2).clamp(PANIC_PAUSE, MAX_PANIC_PAUSE)
                        } else {
                            Duration::ZERO
                        };
                        // In slices, so a quit meanwhile keeps to C88's budget.
                        let until = Instant::now() + pause;
                        while !worker.quitting() {
                            let left = until.saturating_duration_since(Instant::now());
                            if left.is_zero() {
                                break;
                            }
                            thread::sleep(left.min(PANIC_PAUSE_SLICE));
                        }
                        last_panic = Some(Instant::now());
                    }
                })?
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
        let announced = Arc::new(AtomicBool::new(false));
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
                announced: Arc::clone(&announced),
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
            announced,
        };
        if self.commands.send(command).is_err() {
            self.shared.release(attempt);
            return Err(StartRefused::Closed);
        }
        Ok(attempt)
    }

    /// Whether the worker still runs: false after [`CastHost::shutdown`].
    /// A panic in the worker ends the attempt it was handling as failed
    /// and the worker carries on (C107).
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

    /// `device` announced itself on the network: a session reconnecting to
    /// it retries at once and spaces its reconnects from the start (C81).
    pub fn announced(&self, device: DeviceId) {
        if let Some(active) = lock(&self.shared.active).as_ref()
            && active.device == device
        {
            active.announced.store(true, Ordering::SeqCst);
        }
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
    /// Shared with the session's recovery sink, which sends them first.
    held: Arc<Mutex<VecDeque<HostEvent>>>,
    shared: Arc<Shared>,
    running: Option<Running>,
    /// Lives as long as the worker, so a device's next session sees the
    /// certificate of its last one (C21).
    fingerprints: Arc<Mutex<FingerprintBook>>,
    /// The attempt from its start until its `Ended`, which a panic in
    /// between still owes the host (C107).
    in_hand: Option<(AttemptId, DeviceId)>,
}

impl Worker {
    fn new(inbox: Receiver<Command>, outbox: SyncSender<HostEvent>, shared: Arc<Shared>) -> Worker {
        Worker {
            inbox,
            outbox,
            held: Arc::new(Mutex::new(VecDeque::new())),
            shared,
            running: None,
            fingerprints: Arc::new(Mutex::new(FingerprintBook::new())),
            in_hand: None,
        }
    }

    /// After a panic: the session in hand is torn down as far as it still
    /// can be and its attempt ends as failed, which frees the slot (C107).
    fn recover(&mut self) {
        if let Some(running) = self.running.take() {
            let quitting = self.quitting();
            let _ = panic::catch_unwind(AssertUnwindSafe(move || {
                if quitting {
                    running.session.quit();
                } else {
                    running.session.stop();
                }
            }));
        }
        if let Some((attempt, device)) = self.in_hand.take() {
            self.ended(attempt, device, SessionEnd::Failed(SessionError::Internal));
        }
    }

    fn emit(&mut self, event: HostEvent) {
        let mut held = lock(&self.held);
        flush(&mut held, &self.outbox);
        if event.is_routine() {
            // Held events go first; a routine one behind them is stale by
            // the time there is room.
            if held.is_empty() {
                let _ = self.outbox.try_send(event);
            }
            return;
        }
        if held.len() >= MAX_HOST_EVENTS {
            let oldest_session = held
                .iter()
                .position(|held| matches!(held, HostEvent::Session { .. }));
            if let Some(index) = oldest_session {
                held.remove(index);
            }
        }
        held.push_back(event);
        flush(&mut held, &self.outbox);
    }

    fn flush(&mut self) {
        flush(&mut lock(&self.held), &self.outbox);
    }

    fn nothing_held(&self) -> bool {
        lock(&self.held).is_empty()
    }

    fn quitting(&self) -> bool {
        self.shared.quitting.load(Ordering::SeqCst)
    }

    fn run(&mut self) {
        loop {
            self.flush();
            let command = if self.running.is_some() {
                match self.inbox.try_recv() {
                    Ok(command) => Some(command),
                    Err(TryRecvError::Empty) => None,
                    Err(TryRecvError::Disconnected) => Some(Command::Quit),
                }
            } else {
                let wait = if self.nothing_held() {
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
                announced,
            } => self.start(
                attempt,
                target,
                format,
                (metadata, artwork),
                reader,
                (selected, announced),
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
        (selected, announced): (Arc<AtomicBool>, Arc<AtomicBool>),
    ) {
        let device = target.id;
        self.in_hand = Some((attempt, device));
        #[cfg(test)]
        {
            let left = self.shared.panic_in_start.load(Ordering::SeqCst);
            if left > 0 {
                self.shared.panic_in_start.store(left - 1, Ordering::SeqCst);
                panic!("a test panic in the Cast worker");
            }
        }
        if !selected.load(Ordering::SeqCst) {
            return self.ended(attempt, device, SessionEnd::Deselected);
        }
        let config = SessionConfig {
            addresses: target.addresses,
            format,
            metadata,
            artwork,
            group: target.group,
            fingerprints: Some(Fingerprints {
                id: device,
                book: Arc::clone(&self.fingerprints),
            }),
        };
        // Connect retries go out live, while the start still runs; one that
        // finds the outbox full, or held events still in the way, is dropped
        // rather than block the worker or overtake them.
        let outbox = self.outbox.clone();
        let held = Arc::clone(&self.held);
        let quitting = Arc::clone(&self.shared.quitting);
        let result = CastSession::start_with(
            config,
            reader,
            Arc::clone(&selected),
            quitting,
            |error, retry_in| {
                let event = SessionEvent::ConnectFailed { error, retry_in };
                let session = HostEvent::Session {
                    attempt,
                    device,
                    event,
                };
                let _ = send_after_held(&held, &outbox, session);
            },
        );
        match result {
            Ok(mut session) => {
                session.set_recovery_sink(self.recovery_sink(attempt, device));
                session.set_announcements(announced);
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

    /// Sends a recovering session's events straight to the host (C80,
    /// C81), after the held ones; the worker sits in the session's retries
    /// until they end, so it can't flush them itself.
    // The sink hands an undelivered event back by value so the session keeps
    // it; boxing would allocate per undelivered event and change the
    // EventSink signature.
    #[allow(clippy::result_large_err)]
    fn recovery_sink(&self, attempt: AttemptId, device: DeviceId) -> EventSink {
        let outbox = self.outbox.clone();
        let held = Arc::clone(&self.held);
        Box::new(move |event| {
            let session = HostEvent::Session {
                attempt,
                device,
                event,
            };
            match send_after_held(&held, &outbox, session) {
                Some(HostEvent::Session { event, .. }) => Err(event),
                _ => Ok(()),
            }
        })
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
        if self.in_hand.is_some_and(|(held, _)| held == attempt) {
            self.in_hand = None;
        }
        self.shared.release(attempt);
        self.emit(HostEvent::Ended {
            attempt,
            device,
            end,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::audio_ring;

    fn target(byte: u8) -> CastTarget {
        CastTarget {
            id: DeviceId([byte; 16]),
            addresses: Vec::new(),
            group: None,
        }
    }

    fn start(host: &mut CastHost, byte: u8) -> Result<AttemptId, StartRefused> {
        let (_writer, reader) = audio_ring(48_000);
        host.start(target(byte), 48_000, Metadata::default(), reader)
    }

    fn attempt_of(event: &HostEvent) -> Option<AttemptId> {
        match event {
            HostEvent::Session { attempt, .. }
            | HostEvent::Started { attempt, .. }
            | HostEvent::Ended { attempt, .. } => Some(*attempt),
            _ => None,
        }
    }

    fn next_of(host: &CastHost, attempt: AttemptId) -> HostEvent {
        let until = Instant::now() + Duration::from_secs(5);
        while Instant::now() < until {
            while let Some(event) = host.poll_event() {
                if attempt_of(&event) == Some(attempt) {
                    return event;
                }
            }
            thread::sleep(Duration::from_millis(10));
        }
        panic!("no event of {attempt:?}");
    }

    // covers: C107
    #[test]
    fn a_panic_in_the_worker_ends_the_attempt_and_the_next_start_runs() {
        let mut host = CastHost::new(DeviceMemory::default()).expect("worker");
        host.shared.panic_in_start.store(1, Ordering::SeqCst);
        let first = start(&mut host, 1).expect("starts");
        let end = next_of(&host, first);
        assert!(
            matches!(
                end,
                HostEvent::Ended {
                    end: SessionEnd::Failed(SessionError::Internal),
                    ..
                }
            ),
            "{end:?}"
        );
        assert!(host.is_running());
        assert_eq!(host.playing(), None);
        let second = start(&mut host, 2).expect("the slot is free again");
        let mut event = next_of(&host, second);
        if !matches!(event, HostEvent::Ended { .. }) {
            host.stop();
            while !matches!(event, HostEvent::Ended { .. }) {
                event = next_of(&host, second);
            }
        }
        assert!(host.shutdown());
    }

    // A quit during the pause after recurring panics waits one slice, not
    // the pause, which by the sixth panic is 1.6 s.
    #[test]
    fn a_quit_during_the_pause_after_recurring_panics_ends_at_once() {
        let mut host = CastHost::new(DeviceMemory::default()).expect("worker");
        host.shared.panic_in_start.store(6, Ordering::SeqCst);
        for byte in 1..=6 {
            let attempt = start(&mut host, byte).expect("starts");
            assert!(matches!(next_of(&host, attempt), HostEvent::Ended { .. }));
        }
        let quit = Instant::now();
        assert!(host.shutdown());
        assert!(
            quit.elapsed() < Duration::from_secs(1),
            "{:?}",
            quit.elapsed()
        );
    }
}
