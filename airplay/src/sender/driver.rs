//! The driver thread: it owns the stream, every speaker's session, the
//! pairings in progress and the discovery directory, feeds them the time,
//! the host's commands and what the I/O threads read, and carries out what
//! they ask (P3, P6, P47, P129, P141, P142).
//!
//! One loop does it all. Each turn runs the timers that are due (the
//! stream's cuts and sends, the sessions' timeouts, retries, discovery),
//! then hands every output where it goes: requests to a session's writer
//! thread, datagrams to its sockets, events to the host. Then it waits for
//! the next deadline or the next message. The stream's packets are due
//! about every 8 ms, so the wait for them is a plain sleep, which every
//! platform keeps to well under a millisecond; messages that come in
//! meanwhile wait at most that long.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::audio::SAMPLE_RATE;
use crate::discovery::{BrowseEvent, Browser, Device, DeviceKey, Directory, DirectoryEvent};
use crate::error::{ErrorKind, SessionError};
use crate::ids::{Entropy, OsEntropy, SessionIds};
use crate::metadata::Metadata;
use crate::pairing::{self, LegacyVerifier, Pairing, PairingEvent, PairingState};
use crate::reconnect::Backoff;
use crate::sdp::Codec;
use crate::session::{Output, Session, SessionConfig, SessionEvent, State, TEARDOWN_TIMEOUT};
use crate::stream::{self, SpeakerConfig, SpeakerCounters, Stream, StreamEvent, Transmit};
use crate::transport::{self, IoEvent, RtspIo, SessionIo, SessionKey};
use crate::volume::Volume;

use super::{
    NowPlaying, ReceiverEvent, ReceiverInfo, SenderEvent, SpeakerError, SpeakerEvent,
    SpeakerOptions, SpeakerState,
};

/// Waits shorter than this are slept precisely; longer ones wait on the
/// message channel first, whose wake-up some platforms round to a coarse
/// timer tick.
const PRECISE_WAIT: Duration = Duration::from_millis(20);
/// The longest the loop goes without a turn.
const IDLE_TURN: Duration = Duration::from_millis(100);
/// How often discovery is polled.
const DISCOVERY_POLL: Duration = Duration::from_millis(20);
/// How long the end of a shutdown waits for the I/O threads. They end
/// within `transport::POLL_INTERVAL` of their close, except a writer blocked
/// in a send that a socket shutdown doesn't wake (Windows), which ends at its
/// own send timeout; nothing waits for that one.
const THREAD_GRACE: Duration = Duration::from_millis(100);
/// Room for the last turns and the thread wait, so a shutdown finishes
/// within 2 s overall (P129): sessions still open 2 s minus this after the
/// stop are closed.
const SHUTDOWN_RESERVE: Duration = Duration::from_millis(250);
/// Messages handled before the timers get their turn again, so a flood of
/// datagrams can't hold back the audio.
const INPUTS_PER_TURN: usize = 256;

/// What the host asks of the driver.
pub(crate) enum Command {
    AddSpeaker {
        device: Box<Device>,
        options: SpeakerOptions,
        counters: Arc<SpeakerCounters>,
    },
    RemoveSpeaker(DeviceKey),
    SetDelay(DeviceKey, Duration),
    SetVolume(DeviceKey, Volume),
    ReadVolume(DeviceKey),
    SetNowPlaying(NowPlaying),
    SetLatency(u32),
    RestartTimeline,
    StartDiscovery(Box<dyn Browser + Send>),
    StopDiscovery,
    ArrivalTime(i64, mpsc::SyncSender<Instant>),
    StartPairing(Box<Device>),
    SubmitPin(DeviceKey, String),
    CancelPairing(DeviceKey),
    Shutdown,
}

/// Everything that reaches the driver, on one channel.
pub(crate) enum Input {
    Command(Command),
    Io(IoEvent),
}

/// A session that is starting, playing or closing.
struct Active {
    key: SessionKey,
    session: Session,
    codec: Codec,
    software_volume: bool,
    software_mute: bool,
    /// The connect attempt in progress, so a late result of an earlier one
    /// is dropped.
    attempt: u32,
    io: Option<SessionIo>,
    stream_id: Option<stream::SpeakerId>,
}

enum Phase {
    Active(Box<Active>),
    /// A new session starts at this time (P142).
    Waiting(Instant),
    /// Waits for the user (P142).
    Failed,
    /// Nothing runs: after a teardown during shutdown.
    Idle,
}

/// A pairing in progress, on a connection of its own (P47).
struct PairingRun {
    key: SessionKey,
    pairing: Pairing,
    /// The connect attempt in progress, so a late result of an earlier one
    /// is dropped.
    attempt: u32,
    io: Option<RtspIo>,
}

/// A speaker the host added.
struct Speaker {
    device: Device,
    options: SpeakerOptions,
    counters: Arc<SpeakerCounters>,
    backoff: Backoff,
    phase: Phase,
    /// The host removed it: forget it once its session is closed.
    removing: bool,
}

pub(crate) struct Driver {
    stream: Stream,
    inputs: mpsc::Receiver<Input>,
    /// A sender for the I/O threads, into the same channel.
    io_sink: mpsc::Sender<Input>,
    events: mpsc::Sender<SenderEvent>,
    latency_frames: Arc<AtomicU32>,
    speakers: BTreeMap<DeviceKey, Speaker>,
    sessions: HashMap<SessionKey, DeviceKey>,
    pairings: BTreeMap<DeviceKey, PairingRun>,
    pairing_sessions: HashMap<SessionKey, DeviceKey>,
    stream_ids: HashMap<stream::SpeakerId, DeviceKey>,
    next_session: u64,
    directory: Directory,
    browser: Option<Box<dyn Browser + Send>>,
    reported: BTreeSet<DeviceKey>,
    now_playing: Option<NowPlaying>,
    retired: Vec<JoinHandle<()>>,
    /// Set by shutdown: sessions still open at this time are closed.
    stopping: Option<Instant>,
}

impl Driver {
    pub(crate) fn new(
        stream: Stream,
        inputs: mpsc::Receiver<Input>,
        io_sink: mpsc::Sender<Input>,
        events: mpsc::Sender<SenderEvent>,
        latency_frames: Arc<AtomicU32>,
    ) -> Driver {
        Driver {
            stream,
            inputs,
            io_sink,
            events,
            latency_frames,
            speakers: BTreeMap::new(),
            sessions: HashMap::new(),
            pairings: BTreeMap::new(),
            pairing_sessions: HashMap::new(),
            stream_ids: HashMap::new(),
            next_session: 0,
            directory: Directory::new(),
            browser: None,
            reported: BTreeSet::new(),
            now_playing: None,
            retired: Vec::new(),
            stopping: None,
        }
    }

    pub(crate) fn run(mut self) {
        self.emit(SenderEvent::Timeline {
            latency: self.stream.latency(),
            origin: self.stream.origin(),
            restarted: false,
        });
        loop {
            let now = Instant::now();
            self.run_timers(now);
            self.flush(now);
            if self.finished(Instant::now()) {
                break;
            }
            let deadline = self.next_deadline(Instant::now());
            self.wait(deadline);
        }
        self.close_all();
        self.join_threads(Instant::now() + THREAD_GRACE);
    }

    /// Joins the I/O threads that end by `deadline`. A thread still running
    /// then is blocked in a send that only its own timeout ends; it holds
    /// nothing but its socket, and is left to end by itself (P129).
    fn join_threads(&mut self, deadline: Instant) {
        loop {
            let (done, running): (Vec<_>, Vec<_>) = self
                .retired
                .drain(..)
                .partition(|handle| handle.is_finished());
            self.retired = running;
            for handle in done {
                let _ = handle.join();
            }
            if self.retired.is_empty() || Instant::now() >= deadline {
                break;
            }
            thread::sleep(Duration::from_millis(5));
        }
        self.retired.clear();
    }

    fn emit(&self, event: SenderEvent) {
        let _ = self.events.send(event);
    }

    fn emit_speaker(&self, key: &DeviceKey, event: SpeakerEvent) {
        self.emit(SenderEvent::Speaker {
            key: key.clone(),
            event,
        });
    }

    fn notify(&self) -> impl transport::Notify {
        let sink = self.io_sink.clone();
        move |event| sink.send(Input::Io(event)).is_ok()
    }

    /// Handles what is waiting, or waits until `deadline` for something.
    fn wait(&mut self, deadline: Instant) {
        let mut handled = 0;
        while handled < INPUTS_PER_TURN {
            let Ok(input) = self.inputs.try_recv() else {
                break;
            };
            self.handle(input);
            handled += 1;
        }
        if handled > 0 {
            return;
        }
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return;
        }
        if left > PRECISE_WAIT {
            if let Ok(input) = self.inputs.recv_timeout(left - PRECISE_WAIT) {
                self.handle(input);
            }
            return;
        }
        thread::sleep(left);
    }

    fn handle(&mut self, input: Input) {
        let now = Instant::now();
        match input {
            Input::Command(command) => self.command(command, now),
            Input::Io(event) => self.io_event(event, now),
        }
    }

    fn next_deadline(&self, now: Instant) -> Instant {
        let mut next = now + IDLE_TURN;
        if let Some(at) = self.stream.poll_timeout() {
            next = next.min(at);
        }
        for speaker in self.speakers.values() {
            match &speaker.phase {
                Phase::Active(active) => {
                    if let Some(at) = active.session.poll_timeout() {
                        next = next.min(at);
                    }
                }
                Phase::Waiting(at) => next = next.min(*at),
                Phase::Failed | Phase::Idle => {}
            }
        }
        for run in self.pairings.values() {
            if let Some(at) = run.pairing.poll_timeout() {
                next = next.min(at);
            }
        }
        if self.browser.is_some() {
            next = next.min(now + DISCOVERY_POLL);
        }
        if let Some(stopping) = self.stopping {
            next = next.min(stopping);
        }
        next
    }

    fn finished(&self, now: Instant) -> bool {
        let Some(stopping) = self.stopping else {
            return false;
        };
        let open = self
            .speakers
            .values()
            .any(|speaker| matches!(speaker.phase, Phase::Active(_)));
        !open || now >= stopping
    }

    fn run_timers(&mut self, now: Instant) {
        if self.stream.poll_timeout().is_some_and(|at| at <= now) {
            self.stream.handle_timeout(now);
        }
        let mut due = Vec::new();
        for (key, speaker) in &mut self.speakers {
            match &mut speaker.phase {
                Phase::Active(active) => {
                    if active.session.poll_timeout().is_some_and(|at| at <= now) {
                        active.session.handle_timeout(now);
                    }
                }
                Phase::Waiting(at) if *at <= now && self.stopping.is_none() => {
                    due.push(key.clone());
                }
                _ => {}
            }
        }
        for key in due {
            self.start_session(&key, now);
        }
        for run in self.pairings.values_mut() {
            if run.pairing.poll_timeout().is_some_and(|at| at <= now) {
                run.pairing.handle_timeout(now);
            }
        }
        self.poll_discovery();
        let (done, running): (Vec<_>, Vec<_>) = self
            .retired
            .drain(..)
            .partition(|handle| handle.is_finished());
        self.retired = running;
        for handle in done {
            let _ = handle.join();
        }
    }

    /// Hands every output where it goes, until nothing more comes.
    fn flush(&mut self, now: Instant) {
        loop {
            let mut progressed = false;
            let keys: Vec<DeviceKey> = self
                .speakers
                .iter()
                .filter(|(_, speaker)| matches!(speaker.phase, Phase::Active(_)))
                .map(|(key, _)| key.clone())
                .collect();
            for key in keys {
                while let Some(output) = self.next_output(&key) {
                    progressed = true;
                    self.apply_output(&key, output, now);
                }
            }
            let pairings: Vec<DeviceKey> = self.pairings.keys().cloned().collect();
            for key in pairings {
                while let Some(output) = self
                    .pairings
                    .get_mut(&key)
                    .and_then(|run| run.pairing.poll_output())
                {
                    progressed = true;
                    self.apply_pairing_output(&key, output, now);
                }
                if self
                    .pairings
                    .get(&key)
                    .is_some_and(|run| run.pairing.state() == PairingState::Closed)
                {
                    self.end_pairing(&key);
                }
            }
            while let Some(transmit) = self.stream.poll_transmit() {
                progressed = true;
                self.transmit(transmit, now);
            }
            while let Some(event) = self.stream.poll_event() {
                progressed = true;
                self.stream_event(event, now);
            }
            if !progressed {
                break;
            }
        }
    }

    fn active(&mut self, key: &DeviceKey) -> Option<&mut Active> {
        match &mut self.speakers.get_mut(key)?.phase {
            Phase::Active(active) => Some(active),
            _ => None,
        }
    }

    fn next_output(&mut self, key: &DeviceKey) -> Option<Output> {
        self.active(key)?.session.poll_output()
    }

    fn apply_output(&mut self, key: &DeviceKey, output: Output, now: Instant) {
        match output {
            Output::Connect(address) => {
                let notify = self.notify();
                let Some(active) = self.active(key) else {
                    return;
                };
                active.attempt += 1;
                if transport::connect(active.key, active.attempt, address, notify).is_err() {
                    active.session.handle_connect_failed(now);
                }
            }
            Output::Send(bytes) => {
                if let Some(io) = self.active(key).and_then(|active| active.io.as_ref()) {
                    io.write(bytes);
                }
            }
            Output::Close => self.release(key),
            Output::Event(event) => self.session_event(key, event, now),
        }
        // Packets stop as soon as the session no longer plays (P35, P129).
        let stopped = self
            .active(key)
            .filter(|active| !active.session.media_allowed())
            .and_then(|active| active.stream_id.take());
        if let Some(id) = stopped {
            self.stream.remove_speaker(id);
            self.stream_ids.remove(&id);
        }
    }

    fn apply_pairing_output(&mut self, key: &DeviceKey, output: pairing::Output, now: Instant) {
        let notify = self.notify();
        let Some(run) = self.pairings.get_mut(key) else {
            return;
        };
        match output {
            pairing::Output::Connect(address) => {
                run.attempt += 1;
                if transport::connect(run.key, run.attempt, address, notify).is_err() {
                    run.pairing.handle_connect_failed(now);
                }
            }
            pairing::Output::Send(bytes) => {
                if let Some(io) = &run.io {
                    io.write(bytes);
                }
            }
            pairing::Output::Close => {
                if let Some(io) = run.io.take() {
                    self.retired.extend(io.close());
                }
            }
            pairing::Output::Event(event) => self.emit(SenderEvent::Pairing {
                key: key.clone(),
                event,
            }),
        }
    }

    /// Forgets a pairing, closing its connection if it is still open.
    fn end_pairing(&mut self, key: &DeviceKey) {
        if let Some(mut run) = self.pairings.remove(key) {
            self.pairing_sessions.remove(&run.key);
            if let Some(io) = run.io.take() {
                self.retired.extend(io.close());
            }
        }
    }

    /// Starts a pairing with a device on a connection of its own (P47); one
    /// already running for it is dropped.
    fn start_pairing(&mut self, device: Device, now: Instant) {
        if self.stopping.is_some() {
            return;
        }
        let key = device.key.clone();
        self.end_pairing(&key);
        let mut entropy = OsEntropy;
        let mut client = [0u8; 8];
        let ids = SessionIds::generate(&mut entropy);
        let (Ok(ids), Ok(())) = (ids, entropy.fill(&mut client)) else {
            let mut error = SessionError::new(ErrorKind::PairingFailed);
            error.detail = Some("the random number source is unavailable");
            self.emit(SenderEvent::Pairing {
                key,
                event: PairingEvent::Failed {
                    error,
                    wrong_pin: false,
                },
            });
            return;
        };
        let session_key = SessionKey(self.next_session);
        self.next_session += 1;
        let pairing = Pairing::new(
            device.connect_socket_addrs(),
            ids,
            u64::from_be_bytes(client),
            now,
        );
        self.pairing_sessions.insert(session_key, key.clone());
        self.pairings.insert(
            key,
            PairingRun {
                key: session_key,
                pairing,
                attempt: 0,
                io: None,
            },
        );
    }

    /// What a pairing's I/O threads report.
    fn pairing_io_event(&mut self, key: &DeviceKey, event: IoEvent, now: Instant) {
        let notify = self.notify();
        let Some(run) = self.pairings.get_mut(key) else {
            return;
        };
        match event {
            IoEvent::Connected {
                session,
                attempt,
                stream,
            } => {
                if attempt != run.attempt || run.pairing.state() != PairingState::Connecting {
                    return;
                }
                match RtspIo::open(session, stream, notify) {
                    Ok(io) => {
                        run.io = Some(io);
                        run.pairing.handle_connected(now);
                    }
                    Err(_) => run.pairing.handle_connect_failed(now),
                }
            }
            IoEvent::ConnectFailed { attempt, .. } => {
                if attempt == run.attempt {
                    run.pairing.handle_connect_failed(now);
                }
            }
            IoEvent::Rtsp { bytes, .. } => run.pairing.handle_input(&bytes, now),
            IoEvent::RtspClosed { .. } => run.pairing.handle_connection_closed(now),
            IoEvent::Control { .. } | IoEvent::TimingRequest { .. } => {}
        }
    }

    /// Closes a session's sockets and takes it off the stream (P141).
    fn release(&mut self, key: &DeviceKey) {
        let Some(active) = self.active(key) else {
            return;
        };
        let io = active.io.take();
        let stream_id = active.stream_id.take();
        if let Some(io) = io {
            self.retired.extend(io.close());
        }
        if let Some(id) = stream_id {
            self.stream.remove_speaker(id);
            self.stream_ids.remove(&id);
        }
    }

    /// Ends the active session's bookkeeping and sets the next phase.
    fn end_session(&mut self, key: &DeviceKey, phase: Phase) {
        self.release(key);
        let Some(speaker) = self.speakers.get_mut(key) else {
            return;
        };
        if let Phase::Active(active) = std::mem::replace(&mut speaker.phase, phase) {
            self.sessions.remove(&active.key);
        }
    }

    fn session_event(&mut self, key: &DeviceKey, event: SessionEvent, now: Instant) {
        match event {
            SessionEvent::Methods(methods) => {
                self.emit_speaker(key, SpeakerEvent::Methods(methods))
            }
            SessionEvent::Server(server) => self.emit_speaker(key, SpeakerEvent::Server(server)),
            SessionEvent::AudioJack(jack) => self.emit_speaker(key, SpeakerEvent::AudioJack(jack)),
            SessionEvent::VolumeReported(db) => {
                self.emit_speaker(key, SpeakerEvent::VolumeReported(db))
            }
            SessionEvent::Warning(warning) => {
                self.emit_speaker(key, SpeakerEvent::Warning(warning))
            }
            SessionEvent::Flushed { .. } => {}
            SessionEvent::Recording {
                first_seq,
                first_rtptime,
                audio_latency,
                destinations,
                ..
            } => self.on_recording(
                key,
                first_seq,
                first_rtptime,
                audio_latency,
                destinations,
                now,
            ),
            SessionEvent::Failed(error) => self.on_failed(key, error, now),
            SessionEvent::Closed => self.on_closed(key, now),
        }
    }

    /// RECORD succeeded: the speaker joins the running timeline (P103).
    fn on_recording(
        &mut self,
        key: &DeviceKey,
        first_seq: u16,
        first_rtptime: u32,
        audio_latency: Option<u32>,
        destinations: crate::session::MediaDestinations,
        now: Instant,
    ) {
        let Some(speaker) = self.speakers.get_mut(key) else {
            return;
        };
        let Phase::Active(active) = &mut speaker.phase else {
            return;
        };
        let config = SpeakerConfig {
            codec: active.codec,
            destinations,
            ssrc: active.session.ids().ssrc,
            first_seq,
            first_rtptime,
            delay: speaker.options.delay,
            software_volume: active.software_volume.then_some(speaker.options.volume),
            software_mute: active.software_mute,
            audio_latency,
        };
        let id = self
            .stream
            .add_speaker_with_counters(config, speaker.counters.clone(), now);
        if active.software_mute && speaker.options.volume == Volume::Muted {
            self.stream.set_software_volume(id, Volume::Muted);
        }
        active.stream_id = Some(id);
        speaker.backoff.on_playing();
        self.stream_ids.insert(id, key.clone());
        self.emit_speaker(key, SpeakerEvent::State(SpeakerState::Playing));
        self.send_now_playing(key, now);
    }

    /// A session failed (P133): a PCM refusal is retried with ALAC at once
    /// (P74); anything else waits for the backoff, or for the user (P142).
    fn on_failed(&mut self, key: &DeviceKey, error: SessionError, now: Instant) {
        let Some(speaker) = self.speakers.get_mut(key) else {
            return;
        };
        let closing = speaker.removing || self.stopping.is_some();
        let alac_retry = error.fallback == Some(Codec::Alac) && !closing;
        let retry_at = if closing {
            None
        } else if alac_retry {
            Some(now)
        } else {
            speaker.backoff.on_failure(error.kind, now)
        };
        let next = match retry_at {
            _ if closing => Phase::Idle,
            Some(at) => Phase::Waiting(at),
            None => Phase::Failed,
        };
        self.end_session(key, next);
        let reason = error.kind.user_reason();
        self.emit_speaker(
            key,
            SpeakerEvent::Error(SpeakerError {
                error,
                reason,
                retry_at,
            }),
        );
        if closing {
            self.stopped(key);
        } else if alac_retry {
            self.directory.remember_pcm_refused(key);
            self.start_session(key, now);
        } else {
            let state = retry_at.map_or(SpeakerState::Failed, |at| SpeakerState::Retrying { at });
            self.emit_speaker(key, SpeakerEvent::State(state));
        }
    }

    /// A teardown finished.
    fn on_closed(&mut self, key: &DeviceKey, now: Instant) {
        self.end_session(key, Phase::Idle);
        let Some(speaker) = self.speakers.get(key) else {
            return;
        };
        if speaker.removing || self.stopping.is_some() {
            self.stopped(key);
        } else {
            // Added again while it was being removed.
            self.start_session(key, now);
        }
    }

    /// Reports a speaker stopped, and forgets it if the host removed it.
    fn stopped(&mut self, key: &DeviceKey) {
        if self
            .speakers
            .get(key)
            .is_some_and(|speaker| speaker.removing)
        {
            self.speakers.remove(key);
        }
        self.emit_speaker(key, SpeakerEvent::State(SpeakerState::Stopped));
    }

    fn transmit(&mut self, transmit: Transmit, now: Instant) {
        let Some(key) = self.stream_ids.get(&transmit.speaker).cloned() else {
            return;
        };
        let Some(active) = self.active(&key) else {
            return;
        };
        if !active.session.media_allowed() {
            return;
        }
        let Some(io) = &active.io else {
            return;
        };
        if let Some(result) = io.send(transmit.from, transmit.to, &transmit.bytes) {
            active.session.handle_udp_send(result, now);
        }
    }

    fn stream_event(&mut self, event: StreamEvent, now: Instant) {
        match event {
            StreamEvent::Flush {
                speaker,
                seq,
                rtptime,
            } => {
                let Some(key) = self.stream_ids.get(&speaker).cloned() else {
                    return;
                };
                if let Some(active) = self.active(&key) {
                    active.session.flush(seq, rtptime, now);
                }
            }
            StreamEvent::FirstSound { speaker, .. } => {
                let Some(key) = self.stream_ids.get(&speaker).cloned() else {
                    return;
                };
                if let Some(latency) = self.stream.speaker_latency(speaker) {
                    self.emit_speaker(&key, SpeakerEvent::Latency(latency));
                }
            }
            StreamEvent::Timeline {
                latency,
                origin,
                restarted,
            } => {
                self.latency_frames
                    .store(self.stream.latency_frames(), Ordering::Relaxed);
                self.emit(SenderEvent::Timeline {
                    latency,
                    origin,
                    restarted,
                });
            }
        }
    }

    fn io_event(&mut self, event: IoEvent, now: Instant) {
        let session = match &event {
            IoEvent::Connected { session, .. }
            | IoEvent::ConnectFailed { session, .. }
            | IoEvent::Rtsp { session, .. }
            | IoEvent::RtspClosed { session }
            | IoEvent::Control { session, .. }
            | IoEvent::TimingRequest { session, .. } => *session,
        };
        if let Some(key) = self.pairing_sessions.get(&session).cloned() {
            return self.pairing_io_event(&key, event, now);
        }
        let Some(key) = self.sessions.get(&session).cloned() else {
            return;
        };
        let notify = self.notify();
        let Some(speaker) = self.speakers.get_mut(&key) else {
            return;
        };
        let counters = speaker.counters.clone();
        let Phase::Active(active) = &mut speaker.phase else {
            return;
        };
        if active.key != session {
            return;
        }
        match event {
            IoEvent::Connected {
                attempt, stream, ..
            } => {
                if attempt != active.attempt || active.session.state() != State::Connecting {
                    return;
                }
                let clock = self.stream.clock();
                match SessionIo::open(session, stream, clock, counters, notify) {
                    Ok((io, connection)) => {
                        active.io = Some(io);
                        active.session.handle_connected(connection, now);
                    }
                    Err(_) => active.session.handle_connect_failed(now),
                }
            }
            IoEvent::ConnectFailed { attempt, .. } => {
                if attempt == active.attempt {
                    active.session.handle_connect_failed(now);
                }
            }
            IoEvent::Rtsp { bytes, .. } => active.session.handle_input(&bytes, now),
            IoEvent::RtspClosed { .. } => active.session.handle_connection_closed(now),
            IoEvent::Control { bytes, .. } => {
                if let Some(id) = active.stream_id {
                    self.stream.handle_control_packet(id, &bytes, now);
                }
            }
            IoEvent::TimingRequest { at, .. } => active.session.handle_timing_request(at),
        }
    }

    fn command(&mut self, command: Command, now: Instant) {
        match command {
            Command::AddSpeaker {
                device,
                options,
                counters,
            } => self.add_speaker(*device, options, counters, now),
            Command::RemoveSpeaker(key) => self.remove_speaker(&key, now),
            Command::SetDelay(key, delay) => {
                let Some(speaker) = self.speakers.get_mut(&key) else {
                    return;
                };
                speaker.options.delay = delay;
                if let Phase::Active(active) = &speaker.phase
                    && let Some(id) = active.stream_id
                {
                    self.stream.set_delay(id, delay, now);
                }
            }
            Command::SetVolume(key, volume) => self.set_volume(&key, volume, now),
            Command::ReadVolume(key) => {
                if let Some(active) = self.active(&key) {
                    active.session.read_volume(now);
                }
            }
            Command::SetNowPlaying(now_playing) => {
                self.now_playing = Some(now_playing);
                let keys: Vec<DeviceKey> = self.stream_ids.values().cloned().collect();
                for key in keys {
                    self.send_now_playing(&key, now);
                }
            }
            Command::SetLatency(frames) => self.stream.set_latency(frames, now),
            Command::RestartTimeline => self.stream.restart(now),
            Command::StartDiscovery(browser) => {
                self.browser = Some(browser);
                self.poll_discovery();
            }
            Command::StopDiscovery => self.stop_discovery(),
            Command::ArrivalTime(n, reply) => {
                let _ = reply.try_send(self.stream.arrival_time(n));
            }
            Command::StartPairing(device) => self.start_pairing(*device, now),
            Command::SubmitPin(key, pin) => {
                if let Some(run) = self.pairings.get_mut(&key) {
                    // A PIN while none is awaited is ignored (Sender::submit_pin).
                    let _ = run.pairing.submit_pin(&pin, &mut OsEntropy, now);
                }
            }
            Command::CancelPairing(key) => {
                if let Some(run) = self.pairings.get_mut(&key) {
                    run.pairing.cancel();
                }
            }
            Command::Shutdown => self.shutdown(now),
        }
    }

    fn add_speaker(
        &mut self,
        device: Device,
        options: SpeakerOptions,
        counters: Arc<SpeakerCounters>,
        now: Instant,
    ) {
        if self.stopping.is_some() {
            return;
        }
        let key = device.key.clone();
        let Some(speaker) = self.speakers.get_mut(&key) else {
            self.speakers.insert(
                key.clone(),
                Speaker {
                    device,
                    options,
                    counters,
                    backoff: Backoff::new(),
                    phase: Phase::Idle,
                    removing: false,
                },
            );
            self.start_session(&key, now);
            return;
        };
        let (delay, volume) = (options.delay, options.volume);
        speaker.device = device;
        speaker.options = options;
        speaker.counters = counters;
        speaker.removing = false;
        match speaker.phase {
            Phase::Active(_) => {
                self.command(Command::SetDelay(key.clone(), delay), now);
                self.set_volume(&key, volume, now);
            }
            Phase::Waiting(_) | Phase::Failed | Phase::Idle => {
                speaker.backoff = Backoff::new();
                self.start_session(&key, now);
            }
        }
    }

    fn remove_speaker(&mut self, key: &DeviceKey, now: Instant) {
        let Some(speaker) = self.speakers.get_mut(key) else {
            return;
        };
        speaker.removing = true;
        match &mut speaker.phase {
            Phase::Active(active) => active.session.teardown(now),
            Phase::Waiting(_) | Phase::Failed | Phase::Idle => self.stopped(key),
        }
    }

    fn set_volume(&mut self, key: &DeviceKey, volume: Volume, now: Instant) {
        if matches!(volume, Volume::Db(db) if db.is_nan()) {
            return;
        }
        let Some(speaker) = self.speakers.get_mut(key) else {
            return;
        };
        speaker.options.volume = volume;
        let Phase::Active(active) = &mut speaker.phase else {
            return;
        };
        active.session.set_volume(volume, now);
        if (active.software_volume || active.software_mute)
            && let Some(id) = active.stream_id
        {
            self.stream.set_software_volume(id, volume);
        }
    }

    /// Starts a new session for a speaker: fresh identifiers, the device's
    /// current addresses and codec (P17, P74, P142).
    fn start_session(&mut self, key: &DeviceKey, now: Instant) {
        let planned = {
            let Some(speaker) = self.speakers.get_mut(key) else {
                return;
            };
            if let Some(device) = self.directory.device(key) {
                speaker.device = device.clone();
            }
            let pcm = speaker.options.pcm && !self.directory.pcm_refused(key);
            let device = &speaker.device;
            super::plan_with(device, speaker.options.credentials.is_some())
                .and_then(|()| device.choose_codec(pcm).map_err(ErrorKind::Unsupported))
        };
        let codec = match planned {
            Ok(codec) => codec,
            Err(kind) => return self.refuse(key, SessionError::new(kind), now),
        };
        let mut entropy = OsEntropy;
        let Ok(ids) = SessionIds::generate(&mut entropy) else {
            let mut error = SessionError::new(ErrorKind::Unreachable);
            error.detail = Some("the random number source is unavailable");
            return self.refuse(key, error, now);
        };
        // The pair-verify of a paired Apple TV, with a fresh X25519 key for
        // this connection (P55).
        let verifier = match self
            .speakers
            .get(key)
            .and_then(|speaker| speaker.options.credentials.as_ref())
        {
            Some(credentials) => match LegacyVerifier::new(credentials, &mut entropy) {
                Ok(verifier) => Some(verifier),
                Err(_) => {
                    let mut error = SessionError::new(ErrorKind::Unreachable);
                    error.detail = Some("the random number source is unavailable");
                    return self.refuse(key, error, now);
                }
            },
            None => None,
        };
        let Some(speaker) = self.speakers.get_mut(key) else {
            return;
        };
        let device = &speaker.device;
        let options = &speaker.options;
        let raop = device.raop_txt.as_ref();
        let software_volume = raop.and_then(|txt| txt.software_volume) == Some(true);
        let software_mute =
            !software_volume && raop.and_then(|txt| txt.software_mute) == Some(true);
        let mut config = SessionConfig::new(device.connect_socket_addrs(), options.volume);
        config.codec = codec;
        config.alac_fallback = device.alac_fallback();
        config.apple_tv_without_credentials = device.is_apple_tv() && options.credentials.is_none();
        config.verifier = verifier.map(|verifier| Box::new(verifier) as _);
        config.password = options.password.clone();
        config.software_volume = software_volume;
        config.software_mute = software_mute;
        config.metadata = device.metadata_support();
        if device.needs_auth_setup() {
            // A fresh X25519 public key whose secret is thrown away; the
            // answer is discarded either way (P60).
            config.auth_setup_key = pairing::auth_setup_key(&mut entropy).ok();
        }
        let session_key = SessionKey(self.next_session);
        self.next_session += 1;
        speaker.phase = Phase::Active(Box::new(Active {
            key: session_key,
            session: Session::new(config, ids, now),
            codec,
            software_volume,
            software_mute,
            attempt: 0,
            io: None,
            stream_id: None,
        }));
        self.sessions.insert(session_key, key.clone());
        self.emit_speaker(key, SpeakerEvent::State(SpeakerState::Connecting));
    }

    /// A session that can't start (P46, P74, P139): reported like a failed
    /// session, without connecting.
    fn refuse(&mut self, key: &DeviceKey, error: SessionError, now: Instant) {
        let Some(speaker) = self.speakers.get_mut(key) else {
            return;
        };
        let retry_at = speaker.backoff.on_failure(error.kind, now);
        speaker.phase = retry_at.map_or(Phase::Failed, Phase::Waiting);
        let reason = error.kind.user_reason();
        self.emit_speaker(
            key,
            SpeakerEvent::Error(SpeakerError {
                error,
                reason,
                retry_at,
            }),
        );
        let state = retry_at.map_or(SpeakerState::Failed, |at| SpeakerState::Retrying { at });
        self.emit_speaker(key, SpeakerEvent::State(state));
    }

    /// Sends what is playing to one playing speaker, as far as it lists the
    /// types (P123 to P127). Each item applies from the audio arriving now.
    fn send_now_playing(&mut self, key: &DeviceKey, now: Instant) {
        let Some(now_playing) = self.now_playing.clone() else {
            return;
        };
        let Some(speaker) = self.speakers.get_mut(key) else {
            return;
        };
        let Phase::Active(active) = &mut speaker.phase else {
            return;
        };
        let Some(rtptime) = active.stream_id.and_then(|id| self.stream.rtptime_now(id)) else {
            return;
        };
        let frames = |duration: Duration| {
            (duration.as_secs_f64() * f64::from(SAMPLE_RATE)).round() as u64 as u32
        };
        let session = &mut active.session;
        if let Some(info) = now_playing.track {
            let _ = session.send_metadata(Metadata::Track { info, rtptime }, now);
        }
        if let Some(jpeg) = now_playing.artwork {
            let _ = session.send_metadata(Metadata::Artwork { jpeg, rtptime }, now);
        }
        if let Some(progress) = now_playing.progress {
            let start = rtptime.wrapping_sub(frames(progress.elapsed));
            let end = start.wrapping_add(frames(progress.length));
            let metadata = Metadata::Progress {
                start,
                current: rtptime,
                end,
            };
            let _ = session.send_metadata(metadata, now);
        }
    }

    fn poll_discovery(&mut self) {
        let Some(browser) = self.browser.as_mut() else {
            return;
        };
        let mut changes = Vec::new();
        while let Some(event) = browser.poll_event() {
            changes.extend(self.directory.apply(event));
        }
        for change in changes {
            match change {
                DirectoryEvent::Updated(device) => {
                    // The next session uses the new records; a running one
                    // is untouched (P18, P19).
                    if let Some(speaker) = self.speakers.get_mut(&device.key) {
                        speaker.device = (*device).clone();
                    }
                    let key = device.key.clone();
                    let info = Box::new(ReceiverInfo::new(*device));
                    let event = if self.reported.insert(key) {
                        ReceiverEvent::Appeared(info)
                    } else {
                        ReceiverEvent::Changed(info)
                    };
                    self.emit(SenderEvent::Receiver(event));
                }
                DirectoryEvent::Removed(key) => {
                    if self.reported.remove(&key) {
                        self.emit(SenderEvent::Receiver(ReceiverEvent::Gone(key)));
                    }
                }
            }
        }
    }

    /// Stops browsing and reports every listed receiver gone. The
    /// directory keeps what it learned about PCM refusals (P74).
    fn stop_discovery(&mut self) {
        self.browser = None;
        let services: Vec<BrowseEvent> = self
            .directory
            .devices()
            .flat_map(|device| {
                let raop = device.raop.as_ref().map(|endpoint| BrowseEvent::Removed {
                    kind: crate::discovery::ServiceKind::Raop,
                    instance: endpoint.instance.clone(),
                });
                let airplay = device
                    .airplay
                    .as_ref()
                    .map(|endpoint| BrowseEvent::Removed {
                        kind: crate::discovery::ServiceKind::AirPlay,
                        instance: endpoint.instance.clone(),
                    });
                raop.into_iter().chain(airplay)
            })
            .collect();
        for service in services {
            self.directory.apply(service);
        }
        for key in std::mem::take(&mut self.reported) {
            self.emit(SenderEvent::Receiver(ReceiverEvent::Gone(key)));
        }
    }

    /// Tears every speaker down in parallel; the loop ends once all are
    /// closed, or early enough that the whole shutdown, threads included,
    /// stays within 2 s (P129).
    fn shutdown(&mut self, now: Instant) {
        if self.stopping.is_some() {
            return;
        }
        self.stopping = Some(now + TEARDOWN_TIMEOUT - SHUTDOWN_RESERVE);
        self.browser = None;
        for run in self.pairings.values_mut() {
            run.pairing.cancel();
        }
        let keys: Vec<DeviceKey> = self.speakers.keys().cloned().collect();
        for key in keys {
            let Some(speaker) = self.speakers.get_mut(&key) else {
                continue;
            };
            match &mut speaker.phase {
                Phase::Active(active) => active.session.teardown(now),
                _ => {
                    speaker.phase = Phase::Idle;
                    self.stopped(&key);
                }
            }
        }
    }

    /// Closes whatever is still open when the loop ends.
    fn close_all(&mut self) {
        let pairings: Vec<DeviceKey> = self.pairings.keys().cloned().collect();
        for key in pairings {
            self.end_pairing(&key);
        }
        let keys: Vec<DeviceKey> = self.speakers.keys().cloned().collect();
        for key in keys {
            if self
                .speakers
                .get(&key)
                .is_some_and(|speaker| matches!(speaker.phase, Phase::Active(_)))
            {
                self.end_session(&key, Phase::Idle);
                self.emit_speaker(&key, SpeakerEvent::State(SpeakerState::Stopped));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::SenderClock;
    use crate::stream::StreamConfig;

    // A thread that outlives its session's close, such as an RTSP writer
    // blocked in a send that a socket shutdown doesn't wake (Windows) and
    // only its own timeout ends, doesn't hold up shutdown (P129).
    #[test]
    fn shutdown_does_not_wait_for_a_thread_stuck_after_its_close() {
        let now = Instant::now();
        let (stream, _writer) = Stream::new(StreamConfig::default(), SenderClock::new(now), now);
        let (inputs, receiver) = mpsc::channel();
        let (events, _events) = mpsc::channel();
        let latency = Arc::new(AtomicU32::new(0));
        let mut driver = Driver::new(stream, receiver, inputs.clone(), events, latency);
        let (release, stuck) = mpsc::channel::<()>();
        driver.retired.push(thread::spawn(move || {
            let _ = stuck.recv_timeout(Duration::from_secs(10));
        }));
        let run = thread::spawn(move || driver.run());
        let started = Instant::now();
        inputs
            .send(Input::Command(Command::Shutdown))
            .expect("the driver runs");
        run.join().expect("the driver ends");
        let took = started.elapsed();
        let _ = release.send(());
        assert!(took < Duration::from_secs(1), "shutdown took {took:?}");
    }
}
