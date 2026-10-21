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
//! 3. Each UI tick, drain [`CastHost::poll_event`]. Every attempt ends with
//!    exactly one [`HostEvent::Ended`]; [`HostEvent::MemoryChanged`] says
//!    when to store [`CastHost::memory`] again.
//! 4. [`CastHost::stop`] when the user stops or deselects the device; the
//!    connect retries and the wait for `PLAYING` stop with it.
//! 5. [`CastHost::shutdown`] when the app quits (C88).

use std::collections::BTreeMap;
use std::fmt;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, TryRecvError};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use serde_json::{Map, Value, json};

use crate::audio::{AudioReader, Codec, StreamFormat};
use crate::client::QUIT_BUDGET;
use crate::discovery::DeviceId;
use crate::media::{IdleReason, MediaError, Metadata};
use crate::session::{CastSession, SessionConfig, SessionEnd, SessionError, SessionEvent};

/// How long the worker drives a session between two looks at the command
/// queue.
const TICK: Duration = Duration::from_millis(50);
/// What [`CastHost::shutdown`] waits beyond the teardown budget of C88.
const SHUTDOWN_MARGIN: Duration = Duration::from_millis(500);

/// How a Cast output relates to the host's other outputs (C73, C77).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OutputSync {
    /// Kept in step only by delaying the other outputs by the device's
    /// measured latency; never sample-accurate.
    MeasuredOffset,
}

/// What every Cast output is: the protocol has no shared clock (C73).
pub const OUTPUT_SYNC: OutputSync = OutputSync::MeasuredOffset;

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
    /// so a damaged file costs only those devices.
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
                memory.offsets.insert(device, Duration::from_millis(ms));
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

/// What the worker reports to the host.
#[derive(Debug)]
pub enum HostEvent {
    /// Everything the session reports while it starts and runs: connect
    /// retries, the format fallback, `Playing`, diagnostics, volume.
    Session {
        device: DeviceId,
        event: SessionEvent,
    },
    /// The device plays the stream in `format`.
    Started {
        device: DeviceId,
        format: StreamFormat,
    },
    /// The attempt or session is over; the last event of each one.
    Ended { device: DeviceId, end: SessionEnd },
    /// A volume, metadata, artwork or Resync request failed.
    CommandFailed {
        device: Option<DeviceId>,
        error: CommandError,
    },
    /// [`CastHost::memory`] changed and should be stored.
    MemoryChanged,
}

enum Command {
    Start {
        generation: u64,
        target: CastTarget,
        format: StreamFormat,
        metadata: Metadata,
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
    generation: u64,
    device: DeviceId,
    selected: Arc<AtomicBool>,
}

struct Shared {
    active: Mutex<Option<Active>>,
    memory: Mutex<DeviceMemory>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Shared {
    /// Frees the slot when it still belongs to `generation`.
    fn release(&self, generation: u64) {
        let mut active = lock(&self.active);
        if active.as_ref().map(|a| a.generation) == Some(generation) {
            *active = None;
        }
    }
}

/// The sender as the host's UI thread sees it. See the module docs.
pub struct CastHost {
    commands: Sender<Command>,
    events: Receiver<HostEvent>,
    shared: Arc<Shared>,
    next_generation: u64,
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
        let (outbox, events) = mpsc::channel();
        let shared = Arc::new(Shared {
            active: Mutex::new(None),
            memory: Mutex::new(memory),
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
            next_generation: 0,
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
    ) -> Result<(), StartRefused> {
        let codec = lock(&self.shared.memory).format(&target.id);
        let format = StreamFormat::new(codec, rate).ok_or(StartRefused::Rate(rate))?;
        let selected = Arc::new(AtomicBool::new(true));
        let generation = self.next_generation;
        {
            let mut active = lock(&self.shared.active);
            if let Some(active) = active.as_ref() {
                return Err(StartRefused::Busy {
                    playing: active.device,
                });
            }
            *active = Some(Active {
                generation,
                device: target.id,
                selected: Arc::clone(&selected),
            });
        }
        self.next_generation += 1;
        let command = Command::Start {
            generation,
            target,
            format,
            metadata,
            reader,
            selected,
        };
        if self.commands.send(command).is_err() {
            self.shared.release(generation);
            return Err(StartRefused::Closed);
        }
        Ok(())
    }

    /// The device of the session that runs or starts, if any.
    pub fn playing(&self) -> Option<DeviceId> {
        lock(&self.shared.active).as_ref().map(|a| a.device)
    }

    /// Stops the session (C87), or the attempt to start one: connect
    /// retries (C80) and the wait for `PLAYING` end at once. The slot is
    /// free for another device right away; the worker tears this session
    /// down before it starts the next one.
    pub fn stop(&self) {
        if let Some(active) = lock(&self.shared.active).take() {
            active.selected.store(false, Ordering::SeqCst);
        }
        let _ = self.commands.send(Command::Stop);
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

    /// Serves PNG or JPEG artwork (C53) and puts its URL in the metadata of
    /// the next `LOAD`.
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

    pub fn poll_event(&self) -> Option<HostEvent> {
        self.events.try_recv().ok()
    }

    /// The app quits: the teardown of C88, within its 3 s and a margin;
    /// whatever is left after that is dropped. The one call that waits, at
    /// most that long, so call it from the app's quit path only.
    pub fn shutdown(mut self) -> bool {
        self.stop_worker();
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

    fn stop_worker(&self) {
        if let Some(active) = lock(&self.shared.active).take() {
            active.selected.store(false, Ordering::SeqCst);
        }
        let _ = self.commands.send(Command::Quit);
    }
}

impl Drop for CastHost {
    /// Asks the worker to quit and does not wait for it.
    fn drop(&mut self) {
        if self.worker.is_some() {
            self.stop_worker();
        }
    }
}

struct Running {
    generation: u64,
    device: DeviceId,
    session: CastSession,
}

struct Worker {
    inbox: Receiver<Command>,
    outbox: Sender<HostEvent>,
    shared: Arc<Shared>,
    running: Option<Running>,
}

impl Worker {
    fn new(inbox: Receiver<Command>, outbox: Sender<HostEvent>, shared: Arc<Shared>) -> Worker {
        Worker {
            inbox,
            outbox,
            shared,
            running: None,
        }
    }

    fn emit(&self, event: HostEvent) {
        let _ = self.outbox.send(event);
    }

    fn run(mut self) {
        loop {
            let command = if self.running.is_some() {
                match self.inbox.try_recv() {
                    Ok(command) => Some(command),
                    Err(TryRecvError::Empty) => None,
                    Err(TryRecvError::Disconnected) => Some(Command::Quit),
                }
            } else {
                match self.inbox.recv_timeout(Duration::from_secs(1)) {
                    Ok(command) => Some(command),
                    Err(RecvTimeoutError::Timeout) => continue,
                    Err(RecvTimeoutError::Disconnected) => Some(Command::Quit),
                }
            };
            match command {
                Some(Command::Quit) => {
                    if let Some(running) = self.running.take() {
                        running.session.quit();
                        self.ended(running.generation, running.device, SessionEnd::Stopped);
                    }
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

    fn handle(&mut self, command: Command) {
        match command {
            Command::Start {
                generation,
                target,
                format,
                metadata,
                reader,
                selected,
            } => self.start(generation, target, format, metadata, reader, selected),
            Command::Stop => {
                if let Some(running) = self.running.take() {
                    running.session.stop();
                    self.ended(running.generation, running.device, SessionEnd::Stopped);
                }
            }
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
                device: None,
                error: CommandError::NoSession,
            });
        };
        let device = running.device;
        if let Err(error) = action(&mut running.session) {
            self.emit(HostEvent::CommandFailed {
                device: Some(device),
                error,
            });
        }
        self.forward_session_events();
    }

    fn start(
        &mut self,
        generation: u64,
        target: CastTarget,
        format: StreamFormat,
        metadata: Metadata,
        reader: AudioReader,
        selected: Arc<AtomicBool>,
    ) {
        let device = target.id;
        if !selected.load(Ordering::SeqCst) {
            return self.ended(generation, device, SessionEnd::Deselected);
        }
        let config = SessionConfig {
            addresses: target.addresses,
            format,
            metadata,
        };
        let outbox = self.outbox.clone();
        let result =
            CastSession::start(config, reader, Arc::clone(&selected), |error, retry_in| {
                let event = SessionEvent::ConnectFailed { error, retry_in };
                let _ = outbox.send(HostEvent::Session { device, event });
            });
        match result {
            Ok(mut session) => {
                let format = session.format();
                if lock(&self.shared.memory).remember_format(device, format.codec()) {
                    self.emit(HostEvent::MemoryChanged);
                }
                while let Some(event) = session.poll_event() {
                    self.emit(HostEvent::Session { device, event });
                }
                self.emit(HostEvent::Started { device, format });
                self.running = Some(Running {
                    generation,
                    device,
                    session,
                });
                if !selected.load(Ordering::SeqCst) {
                    self.handle(Command::Stop);
                }
            }
            Err(_) if !selected.load(Ordering::SeqCst) => {
                self.ended(generation, device, SessionEnd::Deselected);
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
                self.ended(generation, device, SessionEnd::Failed(error));
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
                    device: running.device,
                    event,
                }),
            }
        }
        match end {
            Some(end) => self.ended(running.generation, running.device, end),
            None if running.session.is_ended() => {
                self.ended(running.generation, running.device, SessionEnd::Stopped);
            }
            None => self.running = Some(running),
        }
    }

    fn ended(&self, generation: u64, device: DeviceId, end: SessionEnd) {
        self.shared.release(generation);
        self.emit(HostEvent::Ended { device, end });
    }
}
