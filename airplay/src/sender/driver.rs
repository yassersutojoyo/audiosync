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

mod ap2;
mod dacp;
mod targets;

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::ap2::PinPairing;
use crate::audio::SAMPLE_RATE;
use crate::connects::{ConnectQueue, StartKind};
use crate::discovery::{
    BrowseEvent, Browser, Device, DeviceKey, Directory, DirectoryEvent, ProtocolPath,
};
use crate::error::{ErrorKind, SessionError, UnsupportedReason, UserReason, Warning};
use crate::homekit::{HomeKitCredentials, SrpDone, SrpWork};
use crate::ids::{Entropy, OsEntropy, SessionIds};
use crate::latency::ReportedLatency;
use crate::metadata::Metadata;
use crate::pairing::{self, LegacyVerifier, Pairing, PairingEvent, PairingState};
use crate::reconnect::Backoff;
use crate::sdp::Codec;
use crate::secret::Secret;
use crate::session::{Output, Session, SessionConfig, SessionEvent, State, TEARDOWN_TIMEOUT};
use crate::stream::{self, SpeakerConfig, SpeakerCounters, Stream, StreamEvent, Transmit};
use crate::targets::{Target, TargetId, Targets};
use crate::transport::{self, IoEvent, RtspIo, SessionIo, SessionKey};
use crate::volume::Volume;

use super::{
    NowPlaying, Progress, ReceiverEvent, ReceiverInfo, SenderEvent, SpeakerError, SpeakerEvent,
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
/// How long the end of a shutdown waits for the I/O threads. The UDP
/// threads end once their close's wake-up datagram reaches them, the RTSP
/// reader within `transport::POLL_INTERVAL`. Two end later and nothing
/// waits for them: a UDP thread whose wake-up couldn't be sent, at
/// `transport::UDP_BACKSTOP`, and a writer blocked in a send that a socket
/// shutdown doesn't wake (Windows), at its own send timeout.
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
    KeepVolume(DeviceKey, Volume),
    ReadVolume(DeviceKey),
    SetNowPlaying(NowPlaying),
    SetLatency(u32),
    RestartTimeline,
    Pause,
    Resume,
    StartDiscovery(Box<dyn Browser + Send>),
    StopDiscovery,
    ArrivalTime(i64, mpsc::SyncSender<Instant>),
    StartPairing {
        device: Box<Device>,
        /// The speaker's password, if the host has one (P39, P46).
        password: Option<Secret>,
    },
    SubmitPin(DeviceKey, Secret),
    CancelPairing(DeviceKey),
    ForgetHomeKit(DeviceKey),
    SetAirPlay2DeviceId([u8; 6]),
    AddTarget {
        target: Box<Target>,
        members: Vec<(DeviceKey, SpeakerOptions, Arc<SpeakerCounters>)>,
    },
    RemoveTarget(TargetId),
    SetTargetDelay(TargetId, Duration),
    SetTargetVolume(TargetId, Volume),
    ReadTargetVolume(TargetId),
    SetDacpAdvertiser(Box<dyn crate::dacp::DacpAdvertiser + Send>),
    Shutdown,
}

/// Everything that reaches the driver, on one channel.
pub(crate) enum Input {
    /// Boxed: a speaker's options carry its HomeKit credentials.
    Command(Box<Command>),
    Io(IoEvent),
    Remote(super::remote::RemoteInput),
    /// A pair-setup's SRP step, run on its own thread (P223).
    Computed {
        session: SessionKey,
        done: Box<SrpDone>,
    },
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
    /// An AirPlay 2 session (P2), at most one at a time (P169).
    Ap2(Box<ap2::Ap2Active>),
    /// A new session starts at this time (P142).
    Waiting(Instant),
    /// The start fell due and waits for its connect's turn (P222); `ap2`
    /// when it goes over AirPlay 2, so it holds that place (P169).
    Queued {
        ap2: bool,
    },
    /// Waits for the user (P142).
    Failed,
    /// Nothing runs: after a teardown during shutdown.
    Idle,
}

/// A pairing the host asked for: the legacy one of an Apple TV (P46), or a
/// PIN pairing on the AirPlay 2 path while the switch is on (P61, P170).
enum Job {
    Legacy(Box<Pairing>),
    HomeKit(Box<PinPairing>),
}

impl Job {
    fn state(&self) -> PairingState {
        match self {
            Job::Legacy(pairing) => pairing.state(),
            Job::HomeKit(pairing) => pairing.state(),
        }
    }

    fn poll_output(&mut self) -> Option<pairing::Output> {
        match self {
            Job::Legacy(pairing) => pairing.poll_output(),
            Job::HomeKit(pairing) => pairing.poll_output(),
        }
    }

    fn poll_timeout(&self) -> Option<Instant> {
        match self {
            Job::Legacy(pairing) => pairing.poll_timeout(),
            Job::HomeKit(pairing) => pairing.poll_timeout(),
        }
    }

    fn handle_timeout(&mut self, now: Instant) {
        match self {
            Job::Legacy(pairing) => pairing.handle_timeout(now),
            Job::HomeKit(pairing) => pairing.handle_timeout(now),
        }
    }

    fn handle_connected(&mut self, now: Instant) {
        match self {
            Job::Legacy(pairing) => pairing.handle_connected(now),
            Job::HomeKit(pairing) => pairing.handle_connected(now),
        }
    }

    fn handle_connect_failed(&mut self, now: Instant) {
        match self {
            Job::Legacy(pairing) => pairing.handle_connect_failed(now),
            Job::HomeKit(pairing) => pairing.handle_connect_failed(now),
        }
    }

    fn handle_input(&mut self, bytes: &[u8], now: Instant) {
        match self {
            Job::Legacy(pairing) => pairing.handle_input(bytes, now),
            Job::HomeKit(pairing) => pairing.handle_input(bytes, now),
        }
    }

    fn handle_connection_closed(&mut self, now: Instant) {
        match self {
            Job::Legacy(pairing) => pairing.handle_connection_closed(now),
            Job::HomeKit(pairing) => pairing.handle_connection_closed(now),
        }
    }

    /// A PIN while none is awaited is ignored (Sender::submit_pin).
    fn submit_pin(&mut self, pin: &str, now: Instant) {
        let _ = match self {
            Job::Legacy(pairing) => pairing.submit_pin(pin, &mut OsEntropy, now),
            Job::HomeKit(pairing) => pairing.submit_pin(pin, now),
        };
    }

    fn cancel(&mut self, now: Instant) {
        match self {
            Job::Legacy(pairing) => pairing.cancel(),
            Job::HomeKit(pairing) => pairing.cancel(now),
        }
    }
}

/// What waits in the connect queue (P222).
#[derive(Debug, Clone, PartialEq, Eq)]
enum Turn {
    Session(DeviceKey),
    /// A pairing the host asked for outside a session (P47, P153).
    Pairing(DeviceKey),
}

/// A pairing in progress, on a connection of its own (P47).
struct PairingRun {
    key: SessionKey,
    pairing: Job,
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
    /// The AirPlay 1 attempt ended in Unsupported or Refused, which sends
    /// the speaker to the AirPlay 2 path while the switch is on (P2).
    airplay1_outcome: Option<ErrorKind>,
    /// Refused only because another AirPlay 2 session ran (P169): it starts
    /// once that session ends for good.
    ap2_waiting: bool,
    /// When it was refused for that, as a count of such refusals: the
    /// lowest waiting one is the first refused (P169).
    ap2_refusal: u64,
    /// Its AirPlay 2 session failed and waits for its own retry, so it
    /// keeps the one AirPlay 2 place (P169).
    ap2_retry: bool,
    /// The target the host added it with (P177).
    target: Option<TargetId>,
    /// The host asked for its volume (P122), so a new AirPlay 2 session
    /// reads once after its initial volume (P184).
    reads_volume: bool,
    /// How the start queues when `Phase::Waiting` falls due (P222).
    next_start: StartKind,
    /// Reported Connecting while it waited for its turn (P224).
    told_connecting: bool,
}

impl Speaker {
    fn new(
        device: Device,
        options: SpeakerOptions,
        counters: Arc<SpeakerCounters>,
        target: Option<TargetId>,
    ) -> Speaker {
        Speaker {
            device,
            options,
            counters,
            backoff: Backoff::new(),
            phase: Phase::Idle,
            removing: false,
            airplay1_outcome: None,
            ap2_waiting: false,
            ap2_refusal: 0,
            ap2_retry: false,
            reads_volume: false,
            target,
            next_start: StartKind::Host,
            told_connecting: false,
        }
    }

    /// Runs the AirPlay 2 session, waits to retry it, or waits for the
    /// turn of a session that will (P169, P222).
    fn holds_ap2(&self) -> bool {
        match self.phase {
            Phase::Ap2(_) => true,
            Phase::Waiting(_) => self.ap2_retry,
            Phase::Queued { ap2 } => ap2 || self.ap2_retry,
            _ => false,
        }
    }

    /// Waits for its turn, then starts as `kind` (P222).
    fn wait_until(&mut self, at: Instant, kind: StartKind) {
        self.phase = Phase::Waiting(at);
        self.next_start = kind;
    }
}

pub(crate) struct Driver {
    stream: Stream,
    inputs: mpsc::Receiver<Input>,
    /// A sender for the I/O threads, into the same channel.
    io_sink: mpsc::Sender<Input>,
    events: mpsc::Sender<SenderEvent>,
    latency_frames: Arc<AtomicU32>,
    sounding: Arc<AtomicBool>,
    speakers: BTreeMap<DeviceKey, Speaker>,
    sessions: HashMap<SessionKey, DeviceKey>,
    pairings: BTreeMap<DeviceKey, PairingRun>,
    pairing_sessions: HashMap<SessionKey, DeviceKey>,
    /// The connections of AirPlay 2 PIN pairings that paired, waiting for
    /// the speaker's session (P153).
    held: BTreeMap<DeviceKey, ap2::Held>,
    /// How long a held pairing connection waits to be claimed (P153).
    held_for: Duration,
    stream_ids: HashMap<stream::SpeakerId, DeviceKey>,
    next_session: u64,
    /// Refusals for the one AirPlay 2 place so far (P169).
    ap2_refusals: u64,
    directory: Directory,
    browser: Option<Box<dyn Browser + Send>>,
    reported: BTreeSet<DeviceKey>,
    /// What is playing, and when the host set it.
    now_playing: Option<(NowPlaying, Instant)>,
    retired: Vec<JoinHandle<()>>,
    /// The threads of a closed PTP service, which keep its ports bound
    /// until they end (P158).
    ptp_closing: Vec<JoinHandle<()>>,
    /// Set by shutdown: sessions still open at this time are closed.
    stopping: Option<Instant>,
    /// The AirPlay 2 switch (P2, P170).
    airplay2: bool,
    /// Devices whose session SETUP took PTP timing (P157).
    ptp_timing: BTreeSet<DeviceKey>,
    /// HomeKit credentials a password pairing gave during this run (P152).
    homekit: BTreeMap<DeviceKey, HomeKitCredentials>,
    /// Devices whose HomeKit credentials an answer turned down during this
    /// run (P154); a new pairing takes them off.
    homekit_turned_down: BTreeSet<DeviceKey>,
    /// The random device ID of the AirPlay 2 session SETUP (P5, P43).
    device_id: [u8; 6],
    /// The PTP service, bound only while an AirPlay 2 session may need it
    /// (P158, P170).
    ptp: Option<ap2::PtpRun>,
    ptp_ports: crate::ptp::PtpPorts,
    ptp_receiver_ports: crate::ptp::PtpPorts,
    /// The host was told that PTP is unavailable (P158).
    ptp_unavailable_told: bool,
    /// The stereo pairs and home theater groups in view (P176).
    targets: Targets,
    /// The targets the host added (P177).
    driven: BTreeMap<TargetId, targets::TargetRun>,
    /// When the sender last tore down a member of a target (P181).
    teardowns: HashMap<DeviceKey, Instant>,
    /// Speakers told their session goes without PTP because the service
    /// runs in another address family (P158, P179).
    ptp_family_told: BTreeSet<DeviceKey>,
    /// The PTP family of a speaker's next session after one connected
    /// outside the service's: IPv6 when true, or None for NTP only (P158,
    /// P179).
    ptp_restart: BTreeMap<DeviceKey, Option<bool>>,
    /// The family, IPv6 when true, the PTP service moved to for a speaker:
    /// its later sessions start there while it still has an address in it.
    ptp_moved: BTreeMap<DeviceKey, bool>,
    /// Speakers whose current session is the restart after a move, so a
    /// fallback in it goes to NTP instead of moving the service back.
    ptp_retrying: BTreeSet<DeviceKey>,
    /// The remote-control command server (P186 to P195).
    dacp: super::remote::Server,
    /// Sessions and pairings waiting for their connect's turn (P222).
    connects: ConnectQueue<Turn>,
    /// The pairings the host asked for that wait for their turn.
    queued_pairings: BTreeMap<DeviceKey, (Box<Device>, Option<Secret>)>,
}

impl Driver {
    pub(crate) fn new(
        stream: Stream,
        inputs: mpsc::Receiver<Input>,
        io_sink: mpsc::Sender<Input>,
        events: mpsc::Sender<SenderEvent>,
        latency_frames: Arc<AtomicU32>,
    ) -> Driver {
        let sink = io_sink.clone();
        let dacp = super::remote::Server::new(Arc::new(move |input| {
            sink.send(Input::Remote(input)).is_ok()
        }));
        Driver {
            stream,
            inputs,
            io_sink,
            events,
            latency_frames,
            sounding: Arc::new(AtomicBool::new(false)),
            speakers: BTreeMap::new(),
            sessions: HashMap::new(),
            pairings: BTreeMap::new(),
            pairing_sessions: HashMap::new(),
            held: BTreeMap::new(),
            held_for: super::PAIRING_HOLD,
            stream_ids: HashMap::new(),
            next_session: 0,
            ap2_refusals: 0,
            directory: Directory::new(),
            browser: None,
            reported: BTreeSet::new(),
            now_playing: None,
            retired: Vec::new(),
            ptp_closing: Vec::new(),
            stopping: None,
            airplay2: false,
            ptp_timing: BTreeSet::new(),
            homekit: BTreeMap::new(),
            homekit_turned_down: BTreeSet::new(),
            device_id: random_device_id(),
            ptp: None,
            ptp_ports: crate::ptp::PtpPorts::default(),
            ptp_receiver_ports: crate::ptp::PtpPorts::default(),
            ptp_unavailable_told: false,
            targets: Targets::new(),
            driven: BTreeMap::new(),
            teardowns: HashMap::new(),
            ptp_family_told: BTreeSet::new(),
            ptp_restart: BTreeMap::new(),
            ptp_moved: BTreeMap::new(),
            ptp_retrying: BTreeSet::new(),
            dacp,
            connects: ConnectQueue::new(),
            queued_pairings: BTreeMap::new(),
        }
    }

    pub(crate) fn with_ptp_ports(
        mut self,
        local: crate::ptp::PtpPorts,
        receiver: crate::ptp::PtpPorts,
    ) -> Driver {
        self.ptp_ports = local;
        self.ptp_receiver_ports = receiver;
        self
    }

    /// Where the driver publishes whether a speaker sounds (P201).
    pub(crate) fn with_sounding(mut self, sounding: Arc<AtomicBool>) -> Driver {
        self.sounding = sounding;
        self
    }

    pub(crate) fn dacp_counters(&self) -> Arc<super::DacpCounters> {
        self.dacp.counters()
    }

    pub(crate) fn with_pairing_hold(mut self, hold: Duration) -> Driver {
        self.held_for = hold;
        self
    }

    pub(crate) fn with_airplay2(mut self, on: bool, device_id: Option<[u8; 6]>) -> Driver {
        self.airplay2 = on;
        if let Some(id) = device_id {
            self.device_id = id;
        }
        self
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
            self.remote_turn(now);
            self.report_missing_members();
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
        let closing = std::mem::take(&mut self.ptp_closing);
        self.retired.extend(closing);
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
            Input::Command(command) => self.command(*command, now),
            Input::Io(event) => self.io_event(event, now),
            Input::Remote(input) => self.remote_input(input),
            Input::Computed { session, done } => self.computed(session, *done, now),
        }
    }

    /// Runs a pair-setup's SRP step off this thread: the key exchange
    /// takes long enough to hold back a playing speaker's packets (P223).
    fn compute(&mut self, session: SessionKey, work: SrpWork) {
        let sink = self.io_sink.clone();
        self.retired.push(thread::spawn(move || {
            let done = Box::new(work.run());
            let _ = sink.send(Input::Computed { session, done });
        }));
    }

    fn computed(&mut self, session: SessionKey, done: SrpDone, now: Instant) {
        if let Some(key) = self.pairing_sessions.get(&session).cloned() {
            if let Some(PairingRun {
                pairing: Job::HomeKit(pairing),
                ..
            }) = self.pairings.get_mut(&key)
            {
                pairing.handle_computed(done, now);
            }
            return;
        }
        let Some(key) = self.sessions.get(&session).cloned() else {
            return;
        };
        if let Some(active) = self.ap2_active(&key)
            && active.key == session
        {
            active.session.handle_computed(done, now);
        }
    }

    fn next_deadline(&self, now: Instant) -> Instant {
        let mut next = now + IDLE_TURN;
        if let Some(at) = self.stream.poll_timeout() {
            next = next.min(at);
        }
        if let Some(at) = self.ptp.as_ref().and_then(|run| run.service.poll_timeout()) {
            next = next.min(at);
        }
        for speaker in self.speakers.values() {
            match &speaker.phase {
                Phase::Active(active) => {
                    if let Some(at) = active.session.poll_timeout() {
                        next = next.min(at);
                    }
                }
                Phase::Ap2(active) => {
                    if let Some(at) = active.session.poll_timeout() {
                        next = next.min(at);
                    }
                }
                Phase::Waiting(at) => next = next.min(*at),
                Phase::Queued { .. } | Phase::Failed | Phase::Idle => {}
            }
        }
        if let Some(at) = self.connects.poll_timeout() {
            next = next.min(at);
        }
        for run in self.pairings.values() {
            if let Some(at) = run.pairing.poll_timeout() {
                next = next.min(at);
            }
        }
        for held in self.held.values() {
            next = next.min(held.until);
        }
        if self.browser.is_some() {
            next = next.min(now + DISCOVERY_POLL);
        }
        if let Some(stopping) = self.stopping {
            next = next.min(stopping);
        }
        if let Some(at) = self.dacp.poll_timeout() {
            next = next.min(at);
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
            .any(|speaker| matches!(speaker.phase, Phase::Active(_) | Phase::Ap2(_)));
        !open || now >= stopping
    }

    fn run_timers(&mut self, now: Instant) {
        if self.stream.poll_timeout().is_some_and(|at| at <= now) {
            self.stream.handle_timeout(now);
        }
        self.sounding
            .store(self.stream.is_sounding(now), Ordering::Relaxed);
        if let Some(run) = &mut self.ptp
            && run.service.poll_timeout().is_some_and(|at| at <= now)
        {
            run.service.handle_timeout(now);
        }
        let mut due = Vec::new();
        for (key, speaker) in &mut self.speakers {
            match &mut speaker.phase {
                Phase::Active(active) => {
                    if active.session.poll_timeout().is_some_and(|at| at <= now) {
                        active.session.handle_timeout(now);
                    }
                }
                Phase::Ap2(active) => {
                    if active.session.poll_timeout().is_some_and(|at| at <= now) {
                        active.session.handle_timeout(now);
                    }
                }
                Phase::Waiting(at) if *at <= now && self.stopping.is_none() => {
                    due.push((key.clone(), speaker.next_start, *at));
                }
                _ => {}
            }
        }
        due.sort_by_key(|(_, _, at)| *at);
        self.start_sessions(due, now);
        for run in self.pairings.values_mut() {
            if run.pairing.poll_timeout().is_some_and(|at| at <= now) {
                run.pairing.handle_timeout(now);
            }
        }
        let unclaimed: Vec<DeviceKey> = self
            .held
            .iter()
            .filter(|(_, held)| held.until <= now)
            .map(|(key, _)| key.clone())
            .collect();
        for key in unclaimed {
            self.drop_held(&key);
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
            let ap2: Vec<DeviceKey> = self
                .speakers
                .iter()
                .filter(|(_, speaker)| matches!(speaker.phase, Phase::Ap2(_)))
                .map(|(key, _)| key.clone())
                .collect();
            for key in ap2 {
                while let Some(output) = self.next_ap2_output(&key) {
                    progressed = true;
                    self.apply_ap2_output(&key, output, now);
                }
                if let Some((session, work)) = self
                    .ap2_active(&key)
                    .and_then(|active| Some((active.key, active.session.take_work()?)))
                {
                    self.compute(session, work);
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
                if let Some(PairingRun {
                    key: session,
                    pairing: Job::HomeKit(pairing),
                    ..
                }) = self.pairings.get_mut(&key)
                    && let Some(work) = pairing.take_work()
                {
                    let session = *session;
                    self.compute(session, work);
                }
                if self
                    .pairings
                    .get(&key)
                    .is_some_and(|run| run.pairing.state() == PairingState::Closed)
                {
                    self.finish_pairing(&key, now);
                }
            }
            while let Some(transmit) = self.stream.poll_transmit() {
                progressed = true;
                self.transmit(transmit, now);
            }
            if self.send_ptp() {
                progressed = true;
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
            pairing::Output::Event(event) => {
                if let PairingEvent::HomeKitPaired(credentials) = &event {
                    self.keep_homekit(key, credentials.clone());
                }
                self.emit(SenderEvent::Pairing {
                    key: key.clone(),
                    event,
                })
            }
        }
    }

    /// Forgets a pairing, closing its connection if it is still open, and
    /// the connection of one that paired.
    fn end_pairing(&mut self, key: &DeviceKey) {
        if self.queued_pairings.remove(key).is_some() {
            self.connects.remove(&Turn::Pairing(key.clone()));
        }
        if let Some(mut run) = self.pairings.remove(key) {
            self.pairing_sessions.remove(&run.key);
            if let Some(io) = run.io.take() {
                self.retired.extend(io.close());
            }
        }
        self.drop_held(key);
    }

    /// A pairing ended. An AirPlay 2 PIN pairing that paired keeps its
    /// connection for the speaker's session (P153); any other closes.
    fn finish_pairing(&mut self, key: &DeviceKey, now: Instant) {
        let Some(run) = self.pairings.remove(key) else {
            return;
        };
        let PairingRun {
            key: session_key,
            pairing,
            attempt,
            io,
        } = run;
        let session = match pairing {
            Job::HomeKit(pairing) => pairing.into_session(),
            Job::Legacy(_) => None,
        };
        match (session, io) {
            (Some(session), Some(io)) => self.hold(key, session_key, attempt, session, io, now),
            (_, io) => {
                self.pairing_sessions.remove(&session_key);
                if let Some(io) = io {
                    self.retired.extend(io.close());
                }
            }
        }
    }

    /// Starts a pairing with a device on a connection of its own (P47), with
    /// the speaker's password when the host gave one (P46); one already
    /// running for it is dropped. With the switch on, a device on the
    /// AirPlay 2 path gets the PIN pairing of P61 instead (P153). Its
    /// connect takes a turn in the queue (P222).
    fn start_pairing(&mut self, device: Device, password: Option<Secret>, now: Instant) {
        if self.stopping.is_some() {
            return;
        }
        let key = device.key.clone();
        self.end_pairing(&key);
        let homekit_paired = self.homekit_paired(&key);
        // A paired speaker never pairs again over the legacy path, which
        // goes to whoever published its RAOP record (P2).
        if homekit_paired && !self.airplay2 {
            let error = SessionError::new(ErrorKind::Unsupported(UnsupportedReason::AirPlay2Only));
            self.emit(SenderEvent::Pairing {
                key,
                event: PairingEvent::Failed {
                    error,
                    wrong_pin: false,
                },
            });
            return;
        }
        self.queued_pairings
            .insert(key.clone(), (Box::new(device), password));
        self.connects.push(Turn::Pairing(key), StartKind::Host, now);
        self.take_turns(now);
    }

    /// A queued pairing's turn came: it opens its connection. False when
    /// it ended before connecting.
    fn begin_pairing(&mut self, key: &DeviceKey, now: Instant) -> bool {
        let Some((device, password)) = self.queued_pairings.remove(key) else {
            return false;
        };
        let device = *device;
        let key = key.clone();
        let homekit_paired = self.homekit_paired(&key);
        let mut entropy = OsEntropy;
        let mut client = [0u8; 8];
        let ids = self.session_ids();
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
            return false;
        };
        let session_key = SessionKey(self.next_session);
        self.next_session += 1;
        let airplay1_outcome = self
            .speakers
            .get(&key)
            .and_then(|speaker| speaker.airplay1_outcome);
        let ap2 = self.airplay2
            && device.choose_path_paired(true, airplay1_outcome, homekit_paired)
                == Ok(ProtocolPath::AirPlay2);
        let pairing = if ap2 {
            Job::HomeKit(Box::new(self.pin_pairing(&device, password, ids, now)))
        } else {
            Job::Legacy(Box::new(
                Pairing::new(
                    device.connect_socket_addrs(),
                    ids,
                    u64::from_be_bytes(client),
                    now,
                )
                .with_password(password),
            ))
        };
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
        true
    }

    /// What a pairing's I/O threads report.
    fn pairing_io_event(&mut self, key: &DeviceKey, event: IoEvent, now: Instant) {
        let notify = self.notify();
        let Some(run) = self.pairings.get_mut(key) else {
            return self.held_io_event(key, &event);
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
                match RtspIo::open(session, attempt, stream, notify) {
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
            // Only the current connection's: a late event from an earlier
            // attempt is dropped.
            IoEvent::Rtsp { attempt, bytes, .. } => {
                if attempt == run.attempt {
                    run.pairing.handle_input(&bytes, now);
                }
            }
            IoEvent::RtspClosed { attempt, .. } => {
                if attempt == run.attempt {
                    run.pairing.handle_connection_closed(now);
                }
            }
            IoEvent::Control { .. } | IoEvent::TimingRequest { .. } | IoEvent::Ptp { .. } => {}
        }
    }

    /// A session's FLUSH got its answer, success or not. A resume from a
    /// pause waits until no FLUSH of that session is queued or in flight,
    /// so the answer to an earlier delay FLUSH doesn't release it (P213).
    fn flush_answered(&mut self, key: &DeviceKey) {
        let id = match self.active(key) {
            Some(active) => (!active.session.flushing()).then_some(active.stream_id),
            None => self
                .ap2_active(key)
                .and_then(|active| (!active.session.flushing()).then_some(active.stream_id)),
        };
        if let Some(Some(id)) = id {
            self.stream.flush_answered(id);
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
        self.release_ap2(key);
        let Some(speaker) = self.speakers.get_mut(key) else {
            return;
        };
        match std::mem::replace(&mut speaker.phase, phase) {
            Phase::Active(active) => {
                self.sessions.remove(&active.key);
            }
            Phase::Ap2(active) => {
                self.sessions.remove(&active.key);
                self.sessions.remove(&active.events_key);
                // The members of a target share the service (P179).
                let shared = self
                    .speakers
                    .values()
                    .any(|speaker| matches!(speaker.phase, Phase::Ap2(_)));
                if !shared {
                    self.close_ptp();
                }
            }
            Phase::Queued { .. } => {
                self.connects.remove(&Turn::Session(key.clone()));
            }
            Phase::Waiting(_) | Phase::Failed | Phase::Idle => {}
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
            SessionEvent::Flushed { .. } => self.flush_answered(key),
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
            reported_latency: ReportedLatency::from_audio_latency(audio_latency),
            airplay2: None,
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
        let was_ap2 = matches!(speaker.phase, Phase::Ap2(_));
        let closing = speaker.removing || self.stopping.is_some();
        let alac_retry = error.fallback == Some(Codec::Alac) && !closing;
        // An AirPlay 1 attempt that ended in Unsupported or Refused moves
        // the speaker to the AirPlay 2 path at once, when it is on (P2).
        let path_switch = self.airplay2
            && !closing
            && !alac_retry
            && matches!(speaker.phase, Phase::Active(_))
            && matches!(error.kind, ErrorKind::Unsupported(_) | ErrorKind::Refused)
            && speaker.device.choose_path(true, Some(error.kind)) == Ok(ProtocolPath::AirPlay2);
        if path_switch {
            speaker.airplay1_outcome = Some(error.kind);
        }
        let target = speaker.target.clone();
        // Soon after the sender's own TEARDOWN, a member that refuses or
        // asks for pairing is still letting go of the old session (P181).
        let settling = !closing
            && !alac_retry
            && !path_switch
            && target.is_some()
            && self.settling(key, error.kind, now);
        if settling {
            self.teardowns.remove(key);
        }
        let Some(speaker) = self.speakers.get_mut(key) else {
            return;
        };
        let retry_at = if closing {
            None
        } else if alac_retry || path_switch {
            Some(now)
        } else if settling {
            speaker.backoff.on_failure(ErrorKind::Lost, now)
        } else if let Some(wait) = error.retry_after {
            Some(speaker.backoff.on_receiver_state(wait, now))
        } else {
            speaker.backoff.on_failure(error.kind, now)
        };
        speaker.ap2_retry = was_ap2 && retry_at.is_some();
        speaker.next_start = StartKind::Retry;
        let next = match retry_at {
            _ if closing => Phase::Idle,
            Some(at) => Phase::Waiting(at),
            None => Phase::Failed,
        };
        self.end_session(key, next);
        // A refusal for the receiver's state is retried, so the host must not
        // read it as a request to pair again (P154).
        let reason = if error.retry_after.is_some() {
            UserReason::Refused
        } else {
            error.kind.user_reason()
        };
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
            self.start_session(key, StartKind::Host, now);
        } else if path_switch {
            if let Some(id) = &target {
                self.move_target_to_airplay2(id, key, now);
            }
            self.start_session(key, StartKind::Host, now);
        } else {
            let state = retry_at.map_or(SpeakerState::Failed, |at| SpeakerState::Retrying { at });
            self.emit_speaker(key, SpeakerEvent::State(state));
        }
        // A session that retries keeps its place; one that gave up hands it on.
        if was_ap2 && retry_at.is_none() {
            self.wake_ap2_waiter(now);
        }
    }

    /// A teardown finished.
    fn on_closed(&mut self, key: &DeviceKey, now: Instant) {
        let was_ap2 = self
            .speakers
            .get(key)
            .is_some_and(|speaker| matches!(speaker.phase, Phase::Ap2(_)));
        self.end_session(key, Phase::Idle);
        let Some(speaker) = self.speakers.get(key) else {
            return;
        };
        if speaker.removing || self.stopping.is_some() {
            self.stopped(key);
            if was_ap2 {
                self.wake_ap2_waiter(now);
            }
        } else {
            // Added again while it was being removed.
            self.start_session(key, StartKind::Host, now);
        }
    }

    /// The AirPlay 2 session ended for good: the first speaker refused only
    /// because it ran starts now (P169), through the usual restart.
    fn wake_ap2_waiter(&mut self, now: Instant) {
        if self.stopping.is_some() || self.speakers.values().any(Speaker::holds_ap2) {
            return;
        }
        let Some((key, speaker)) = self
            .speakers
            .iter_mut()
            .filter(|(_, speaker)| {
                speaker.ap2_waiting && !speaker.removing && matches!(speaker.phase, Phase::Failed)
            })
            .min_by_key(|(_, speaker)| speaker.ap2_refusal)
        else {
            return;
        };
        speaker.ap2_waiting = false;
        speaker.wait_until(now, StartKind::Host);
        let key = key.clone();
        let target = speaker.target.clone();
        self.emit_speaker(
            &key,
            SpeakerEvent::State(SpeakerState::Retrying { at: now }),
        );
        if let Some(target) = target {
            self.wake_target_waiters(&target, now);
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
        if self
            .speakers
            .get(&key)
            .is_some_and(|speaker| matches!(speaker.phase, Phase::Ap2(_)))
        {
            return self.ap2_transmit(&key, transmit, now);
        }
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
                let sent = match self.active(&key) {
                    Some(active) => active.session.flush(seq, rtptime, now),
                    None => self.ap2_flush(&key, seq, rtptime, now),
                };
                if !sent {
                    self.stream.flush_answered(speaker);
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
            StreamEvent::LatencyHeld { frames } => {
                self.emit(SenderEvent::LatencyHeld { frames });
            }
            StreamEvent::LatencyShortfall {
                speaker,
                reported_us,
                latency_frames,
            } => {
                let Some(key) = self.stream_ids.get(&speaker).cloned() else {
                    return;
                };
                let warning = Warning::LatencyShortfall {
                    reported_us,
                    latency_frames,
                };
                self.emit_speaker(&key, SpeakerEvent::Warning(warning));
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
        if let IoEvent::Ptp { bytes, source, at } = event {
            if let Some(run) = &mut self.ptp {
                run.service.handle_message(&bytes, source, at);
            }
            return;
        }
        let session = match &event {
            IoEvent::Connected { session, .. }
            | IoEvent::ConnectFailed { session, .. }
            | IoEvent::Rtsp { session, .. }
            | IoEvent::RtspClosed { session, .. }
            | IoEvent::Control { session, .. }
            | IoEvent::TimingRequest { session, .. } => *session,
            IoEvent::Ptp { .. } => return,
        };
        if let Some(key) = self.pairing_sessions.get(&session).cloned() {
            return self.pairing_io_event(&key, event, now);
        }
        let Some(key) = self.sessions.get(&session).cloned() else {
            return;
        };
        if let IoEvent::Connected {
            attempt, stream, ..
        } = &event
        {
            self.remote_connected(&key, session, *attempt, stream);
        }
        if self
            .speakers
            .get(&key)
            .is_some_and(|speaker| matches!(speaker.phase, Phase::Ap2(_)))
        {
            return self.ap2_io_event(&key, event, now);
        }
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
                match SessionIo::open(session, attempt, stream, clock, counters, notify) {
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
            // Only the current connection's: after the reconnect of P58 the
            // earlier connection's reader and writer may still report its
            // close, which must not end the new attempt.
            IoEvent::Rtsp { attempt, bytes, .. } => {
                if attempt == active.attempt {
                    active.session.handle_input(&bytes, now);
                }
            }
            IoEvent::RtspClosed { attempt, .. } => {
                if attempt == active.attempt {
                    active.session.handle_connection_closed(now);
                }
            }
            IoEvent::Control { bytes, .. } => {
                if let Some(id) = active.stream_id {
                    self.stream.handle_control_packet(id, &bytes, now);
                }
            }
            IoEvent::TimingRequest { at, .. } => active.session.handle_timing_request(at),
            IoEvent::Ptp { .. } => {}
        }
    }

    fn command(&mut self, command: Command, now: Instant) {
        match command {
            // A target's members stay the target's: the host adds and
            // removes them through the target.
            Command::AddSpeaker {
                device,
                options,
                counters,
            } => {
                if !self.is_target_member(&device.key) {
                    self.add_speaker(*device, options, counters, now);
                }
            }
            Command::RemoveSpeaker(key) => {
                if !self.is_target_member(&key) {
                    self.remove_speaker(&key, now);
                }
            }
            Command::SetDelay(key, delay) => {
                let Some(speaker) = self.speakers.get_mut(&key) else {
                    return;
                };
                speaker.options.delay = delay;
                let id = match &speaker.phase {
                    Phase::Active(active) => active.stream_id,
                    Phase::Ap2(active) => active.stream_id,
                    _ => None,
                };
                if let Some(id) = id {
                    self.stream.set_delay(id, delay, now);
                }
            }
            Command::SetVolume(key, volume) => self.set_volume(&key, volume, now),
            Command::KeepVolume(key, volume) => self.keep_volume(&key, volume),
            Command::ReadVolume(key) => self.read_volume(&key, now),
            Command::SetNowPlaying(now_playing) => {
                self.now_playing = Some((now_playing, now));
                let keys: Vec<DeviceKey> = self.stream_ids.values().cloned().collect();
                for key in keys {
                    self.send_now_playing(&key, now);
                }
            }
            Command::SetLatency(frames) => self.stream.set_latency(frames, now),
            Command::RestartTimeline => self.stream.restart(now),
            Command::Pause => {
                self.stream.pause(now);
            }
            Command::Resume => {
                self.stream.resume(now);
            }
            Command::SetAirPlay2DeviceId(id) => self.device_id = id,
            Command::StartDiscovery(browser) => {
                self.browser = Some(browser);
                self.poll_discovery();
            }
            Command::StopDiscovery => self.stop_discovery(),
            Command::ArrivalTime(n, reply) => {
                let _ = reply.try_send(self.stream.arrival_time(n));
            }
            Command::StartPairing { device, password } => {
                self.start_pairing(*device, password, now);
            }
            Command::SubmitPin(key, pin) => {
                if let Some(run) = self.pairings.get_mut(&key) {
                    run.pairing.submit_pin(pin.expose(), now);
                }
            }
            Command::CancelPairing(key) => {
                if self.queued_pairings.remove(&key).is_some() {
                    self.connects.remove(&Turn::Pairing(key.clone()));
                    self.emit(SenderEvent::Pairing {
                        key: key.clone(),
                        event: PairingEvent::Stopped,
                    });
                }
                if let Some(run) = self.pairings.get_mut(&key) {
                    run.pairing.cancel(now);
                }
                self.drop_held(&key);
            }
            Command::ForgetHomeKit(key) => {
                self.drop_held(&key);
                self.forget_homekit(&key);
            }
            Command::AddTarget { target, members } => self.add_target(*target, members, now),
            Command::RemoveTarget(id) => self.remove_target(&id, now),
            Command::SetTargetDelay(id, delay) => self.set_target_delay(&id, delay, now),
            Command::SetTargetVolume(id, volume) => self.set_target_volume(&id, volume, now),
            Command::SetDacpAdvertiser(advertiser) => self.set_dacp_advertiser(advertiser),
            Command::ReadTargetVolume(id) => self.read_target_volume(&id, now),
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
            self.speakers
                .insert(key.clone(), Speaker::new(device, options, counters, None));
            if self.controls_driven_target(&key) {
                let reason = crate::error::UnsupportedReason::PlaysThroughSpeakers;
                return self.refuse(&key, SessionError::new(ErrorKind::Unsupported(reason)), now);
            }
            self.start_session(&key, StartKind::Host, now);
            return;
        };
        let (delay, volume) = (options.delay, options.volume);
        speaker.device = device;
        speaker.options = options;
        speaker.counters = counters;
        speaker.removing = false;
        // Its target went while it tore down: the host owns it alone now.
        if speaker
            .target
            .as_ref()
            .is_some_and(|id| !self.driven.contains_key(id))
        {
            speaker.target = None;
        }
        match speaker.phase {
            Phase::Active(_) | Phase::Ap2(_) => {
                self.command(Command::SetDelay(key.clone(), delay), now);
                self.set_volume(&key, volume, now);
            }
            // Asked again while waiting for its turn: a host start now.
            Phase::Queued { .. } => {
                speaker.backoff = Backoff::new();
                self.connects.push(Turn::Session(key), StartKind::Host, now);
                self.take_turns(now);
            }
            Phase::Waiting(_) | Phase::Failed | Phase::Idle => {
                speaker.backoff = Backoff::new();
                speaker.airplay1_outcome = None;
                self.start_session(&key, StartKind::Host, now);
            }
        }
    }

    fn remove_speaker(&mut self, key: &DeviceKey, now: Instant) {
        let Some(speaker) = self.speakers.get_mut(key) else {
            return;
        };
        speaker.removing = true;
        if speaker.target.is_some() {
            self.note_teardown(key.clone(), now);
        }
        self.ptp_family_told.remove(key);
        self.ptp_restart.remove(key);
        self.ptp_moved.remove(key);
        self.ptp_retrying.remove(key);
        let Some(speaker) = self.speakers.get_mut(key) else {
            return;
        };
        let held = speaker.holds_ap2();
        match &mut speaker.phase {
            Phase::Active(active) => active.session.teardown(now),
            Phase::Ap2(active) => active.session.teardown(now),
            Phase::Queued { .. } | Phase::Waiting(_) | Phase::Failed | Phase::Idle => {
                // A start still waiting for its turn opens no connection (P224).
                self.connects.remove(&Turn::Session(key.clone()));
                self.stopped(key);
                if held {
                    self.wake_ap2_waiter(now);
                }
            }
        }
    }

    fn keep_volume(&mut self, key: &DeviceKey, volume: Volume) {
        if matches!(volume, Volume::Db(db) if db.is_nan()) {
            return;
        }
        let Some(speaker) = self.speakers.get_mut(key) else {
            return;
        };
        speaker.options.volume = volume;
        if let Phase::Ap2(active) = &mut speaker.phase {
            active.session.keep_volume(volume);
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
        if let Phase::Ap2(active) = &mut speaker.phase {
            return active.session.set_volume(volume, now);
        }
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

    /// A start of a new session fell due: it waits in the connect queue
    /// for its turn, reported Connecting meanwhile (P222, P224). Whether it
    /// takes the one AirPlay 2 place is decided now, so a later AirPlay 2
    /// start can't take the place ahead of it (P169).
    fn start_session(&mut self, key: &DeviceKey, kind: StartKind, now: Instant) {
        self.start_sessions(vec![(key.clone(), kind, now)], now);
    }

    /// Starts that fell due together, with the times they fell due: all
    /// are queued before any takes a turn, so the turns go by kind and time.
    fn start_sessions(&mut self, starts: Vec<(DeviceKey, StartKind, Instant)>, now: Instant) {
        let mut queued = Vec::new();
        for (key, kind, due) in starts {
            if self.queue_start(&key, kind, due, now) {
                queued.push(key);
            }
        }
        self.take_turns(now);
        for key in queued {
            let Some(speaker) = self.speakers.get_mut(&key) else {
                continue;
            };
            if matches!(speaker.phase, Phase::Queued { .. }) && !speaker.told_connecting {
                speaker.told_connecting = true;
                self.emit_speaker(&key, SpeakerEvent::State(SpeakerState::Connecting));
            }
        }
    }

    /// Puts a start in the connect queue; false when it was refused at
    /// once instead.
    fn queue_start(
        &mut self,
        key: &DeviceKey,
        kind: StartKind,
        due: Instant,
        now: Instant,
    ) -> bool {
        if !self.speakers.contains_key(key) {
            return false;
        }
        let ap2 = self.plans_ap2(key);
        let kind = if ap2 && self.held.contains_key(key) {
            StartKind::OpenConnection
        } else {
            kind
        };
        let taken = self.speakers.iter().any(|(other, speaker)| {
            other != key && speaker.holds_ap2() && !self.same_target(key, other)
        });
        if ap2 && taken {
            // Refused at once, without a connection.
            self.begin_session(key, now);
            return false;
        }
        if let Some(speaker) = self.speakers.get_mut(key) {
            speaker.phase = Phase::Queued { ap2 };
        }
        self.connects.push(Turn::Session(key.clone()), kind, due);
        true
    }

    /// Starts every queued session and pairing whose turn has come; one
    /// that ends before connecting leaves the turn to the next (P222).
    fn take_turns(&mut self, now: Instant) {
        while let Some((turn, kind)) = self.connects.pop(now) {
            let connecting = match &turn {
                Turn::Session(key) => self.begin_session(key, now),
                Turn::Pairing(key) => self.begin_pairing(key, now),
            };
            if connecting && kind != StartKind::OpenConnection {
                self.connects.connected(now);
            }
        }
    }

    /// Starts a new session for a speaker: fresh identifiers, the device's
    /// current addresses and codec (P17, P74, P142). A speaker that needs
    /// the legacy pairing without credentials still connects: it is asked
    /// OPTIONS before PairingRequired is reported, so a receiver that can't
    /// take AirPlay 1 audio is Unsupported instead (P46). True when a new
    /// connection opens.
    fn begin_session(&mut self, key: &DeviceKey, now: Instant) -> bool {
        let held = self.speakers.get_mut(key).is_some_and(|speaker| {
            speaker.ap2_waiting = false;
            std::mem::take(&mut speaker.ap2_retry)
        });
        self.open_session(key, now);
        if let Some(speaker) = self.speakers.get_mut(key) {
            speaker.told_connecting = false;
        }
        // A retry that ends anywhere but a running AirPlay 2 session hands the place on (P169).
        if held {
            self.wake_ap2_waiter(now);
        }
        match self.speakers.get(key).map(|speaker| &speaker.phase) {
            Some(Phase::Active(_)) => true,
            // One that went on over its pairing's connection opened none.
            Some(Phase::Ap2(active)) => active.io.is_none(),
            _ => false,
        }
    }

    /// Whether the speaker's next session goes over AirPlay 2 (P2).
    fn plans_ap2(&mut self, key: &DeviceKey) -> bool {
        if self.homekit_paired(key) {
            return self.airplay2;
        }
        if !self.airplay2 {
            return false;
        }
        if let Some(device) = self.directory.device(key).cloned()
            && let Some(speaker) = self.speakers.get_mut(key)
        {
            speaker.device = device;
        }
        self.speakers.get(key).is_some_and(|speaker| {
            speaker.device.choose_path(true, speaker.airplay1_outcome) == Ok(ProtocolPath::AirPlay2)
                || (speaker.device.verdict.airplay2 && self.target_on_airplay2(speaker))
        })
    }

    /// Reports Connecting, unless the speaker was told so while it waited
    /// for its turn.
    pub(super) fn report_connecting(&mut self, key: &DeviceKey) {
        let told = self
            .speakers
            .get_mut(key)
            .is_some_and(|speaker| std::mem::take(&mut speaker.told_connecting));
        if !told {
            self.emit_speaker(key, SpeakerEvent::State(SpeakerState::Connecting));
        }
    }

    fn open_session(&mut self, key: &DeviceKey, now: Instant) {
        if self.homekit_paired(key) {
            return self.open_paired(key, now);
        }
        if self.plans_ap2(key) {
            return self.start_ap2(key, now);
        }
        let planned = {
            let Some(speaker) = self.speakers.get_mut(key) else {
                return;
            };
            if let Some(device) = self.directory.device(key) {
                speaker.device = device.clone();
            }
            let pcm = speaker.options.pcm && !self.directory.pcm_refused(key);
            let device = &speaker.device;
            let (needs_pairing, plan) = match super::plan_with(
                device,
                speaker.options.credentials.is_some(),
                self.airplay2,
            ) {
                Err(ErrorKind::PairingRequired) => (true, Ok(())),
                plan => (false, plan),
            };
            plan.and_then(|()| device.choose_codec(pcm).map_err(ErrorKind::Unsupported))
                .map(|codec| (codec, needs_pairing))
        };
        let (codec, needs_pairing) = match planned {
            Ok(planned) => planned,
            Err(kind) => return self.refuse(key, SessionError::new(kind), now),
        };
        let mut entropy = OsEntropy;
        let Ok(ids) = self.session_ids() else {
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
        config.needs_pairing = needs_pairing;
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
        let remote = ids.active_remote;
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
        self.remote_open(key, remote, now);
        self.report_connecting(key);
    }

    /// Whether the sender holds HomeKit credentials for a device, from the
    /// host or from a PIN pairing in this run.
    fn homekit_paired(&self, key: &DeviceKey) -> bool {
        self.homekit.contains_key(key)
            || self
                .speakers
                .get(key)
                .is_some_and(|speaker| speaker.options.homekit_credentials.is_some())
    }

    /// A HomeKit-paired speaker plays over AirPlay 2 or not at all (P2).
    /// Without its `_airplay._tcp` service in view it is unreachable and
    /// retried on the usual schedule.
    fn open_paired(&mut self, key: &DeviceKey, now: Instant) {
        let Some(speaker) = self.speakers.get_mut(key) else {
            return;
        };
        if let Some(device) = self.directory.device(key) {
            speaker.device = device.clone();
        }
        let path = speaker
            .device
            .choose_path_paired(self.airplay2, speaker.airplay1_outcome, true);
        if let Err(reason) = path {
            return self.refuse(key, SessionError::new(ErrorKind::Unsupported(reason)), now);
        }
        if speaker.device.airplay_socket_addrs().is_empty() {
            let mut error = SessionError::new(ErrorKind::Unreachable);
            error.detail = Some("the speaker's AirPlay 2 service is not in view");
            return self.refuse(key, error, now);
        }
        self.start_ap2(key, now)
    }

    /// A session that can't start (P46, P74, P139): reported like a failed
    /// session, without connecting.
    fn refuse(&mut self, key: &DeviceKey, error: SessionError, now: Instant) {
        let Some(speaker) = self.speakers.get_mut(key) else {
            return;
        };
        let retry_at = speaker.backoff.on_failure(error.kind, now);
        speaker.phase = retry_at.map_or(Phase::Failed, Phase::Waiting);
        speaker.next_start = StartKind::Retry;
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

    /// Sends what is playing to one playing speaker, on either path, as far
    /// as it lists the types (P123 to P127). Each item applies from the
    /// audio arriving now.
    fn send_now_playing(&mut self, key: &DeviceKey, now: Instant) {
        let Some((now_playing, set_at)) = self.now_playing.clone() else {
            return;
        };
        let Some(speaker) = self.speakers.get_mut(key) else {
            return;
        };
        let stream_id = match &speaker.phase {
            Phase::Active(active) => active.stream_id,
            Phase::Ap2(active) => active.stream_id,
            _ => None,
        };
        let Some(rtptime) = stream_id.and_then(|id| self.stream.rtptime_now(id)) else {
            return;
        };
        let frames = |duration: Duration| {
            (duration.as_secs_f64() * f64::from(SAMPLE_RATE)).round() as u64 as u32
        };
        let track = now_playing
            .track
            .map(|info| Metadata::Track { info, rtptime });
        let progress = now_playing
            .progress
            .and_then(|progress| progress_at(progress, set_at, now))
            .map(|progress| {
                let start = rtptime.wrapping_sub(frames(progress.elapsed));
                Metadata::Progress {
                    start,
                    current: rtptime,
                    end: start.wrapping_add(frames(progress.length)),
                }
            });
        // Each session picks its own path's image (P185); refusals are the
        // session's to report.
        let artwork = now_playing.artwork;
        match &mut speaker.phase {
            Phase::Active(active) => {
                let session = &mut active.session;
                if let Some(track) = track {
                    let _ = session.send_metadata(track, now);
                }
                if let Some(artwork) = artwork {
                    let _ = session.send_artwork(artwork, rtptime, now);
                }
                if let Some(progress) = progress {
                    let _ = session.send_metadata(progress, now);
                }
            }
            Phase::Ap2(active) => {
                let session = &mut active.session;
                if let Some(track) = track {
                    let _ = session.send_metadata(track, now);
                }
                if let Some(artwork) = artwork {
                    let _ = session.send_artwork(artwork, rtptime, now);
                }
                if let Some(progress) = progress {
                    let _ = session.send_metadata(progress, now);
                }
            }
            _ => {}
        }
    }

    /// A read-back of a playing speaker's volume (P122); on the AirPlay 2
    /// path it goes at the session's next 1 s read tick (P184).
    fn read_volume(&mut self, key: &DeviceKey, now: Instant) {
        let Some(speaker) = self.speakers.get_mut(key) else {
            return;
        };
        speaker.reads_volume = true;
        match &mut speaker.phase {
            Phase::Active(active) => {
                active.session.read_volume(now);
            }
            Phase::Ap2(active) if active.stream_id.is_some() => active.session.read_volume(now),
            _ => {}
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
        let dead = browser.is_dead();
        self.report_changes(changes);
        if dead {
            // Nothing will change the list any more: drop it rather than
            // keep one that silently goes stale, and tell the host.
            self.stop_discovery();
            self.emit(SenderEvent::DiscoveryFailed);
        }
    }

    /// Reports what discovery changed in the directory to the host: the
    /// devices first, each with its target, then the targets (P176).
    fn report_changes(&mut self, changes: Vec<DirectoryEvent>) {
        if changes.is_empty() {
            return;
        }
        let before = self.target_refs();
        let target_changes = self.targets.update(self.directory.devices());
        let after = self.target_refs();
        let mut updated = BTreeSet::new();
        for change in changes {
            match change {
                DirectoryEvent::Updated(device) => {
                    // The next session uses the new records; a running one
                    // is untouched (P18, P19).
                    if let Some(speaker) = self.speakers.get_mut(&device.key) {
                        speaker.device = (*device).clone();
                    }
                    let key = device.key.clone();
                    updated.insert(key.clone());
                    let mut info = Box::new(ReceiverInfo::new(*device));
                    info.target = after.get(&key).cloned();
                    self.rejoin_member(&key, Instant::now());
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
        let moved: Vec<DeviceKey> = self
            .reported
            .iter()
            .filter(|key| !updated.contains(*key) && before.get(*key) != after.get(*key))
            .cloned()
            .collect();
        for key in moved {
            if let Some(device) = self.directory.device(&key).cloned() {
                let mut info = Box::new(ReceiverInfo::new(device));
                info.target = after.get(&key).cloned();
                self.emit(SenderEvent::Receiver(ReceiverEvent::Changed(info)));
            }
        }
        for change in target_changes {
            self.emit(SenderEvent::Receiver(ReceiverEvent::Target(change)));
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
        for change in self.targets.update(std::iter::empty()) {
            self.emit(SenderEvent::Receiver(ReceiverEvent::Target(change)));
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
        // Stopping drops every start still waiting for its turn (P224).
        for turn in self.connects.clear() {
            if let Turn::Pairing(key) = turn {
                self.queued_pairings.remove(&key);
                self.emit(SenderEvent::Pairing {
                    key,
                    event: PairingEvent::Stopped,
                });
            }
        }
        for run in self.pairings.values_mut() {
            run.pairing.cancel(now);
        }
        let held: Vec<DeviceKey> = self.held.keys().cloned().collect();
        for key in held {
            self.drop_held(&key);
        }
        let keys: Vec<DeviceKey> = self.speakers.keys().cloned().collect();
        for key in keys {
            let Some(speaker) = self.speakers.get_mut(&key) else {
                continue;
            };
            match &mut speaker.phase {
                Phase::Active(active) => active.session.teardown(now),
                Phase::Ap2(active) => active.session.teardown(now),
                _ => {
                    speaker.phase = Phase::Idle;
                    self.stopped(&key);
                }
            }
        }
    }

    /// Closes whatever is still open when the loop ends.
    fn close_all(&mut self) {
        self.stop_remote();
        let pairings: Vec<DeviceKey> = self
            .pairings
            .keys()
            .chain(self.held.keys())
            .cloned()
            .collect();
        for key in pairings {
            self.end_pairing(&key);
        }
        let keys: Vec<DeviceKey> = self.speakers.keys().cloned().collect();
        for key in keys {
            if self
                .speakers
                .get(&key)
                .is_some_and(|speaker| matches!(speaker.phase, Phase::Active(_) | Phase::Ap2(_)))
            {
                self.end_session(&key, Phase::Idle);
                self.emit_speaker(&key, SpeakerEvent::State(SpeakerState::Stopped));
            }
        }
    }
}

/// Six random bytes for the AirPlay 2 device ID when the host stores none
/// (P5). Without a random source the ID is all zeros, which the session
/// SETUP still marks as locally administered.
fn random_device_id() -> [u8; 6] {
    let mut bytes = [0u8; 6];
    let _ = OsEntropy.fill(&mut bytes);
    bytes
}

/// `progress` as the host set it at `set_at`, carried on to `now`, so a
/// speaker that starts later shows where the track is by then (the host
/// sets progress only while the track plays); `None` once past its end.
fn progress_at(progress: Progress, set_at: Instant, now: Instant) -> Option<Progress> {
    let elapsed = progress.elapsed + now.saturating_duration_since(set_at);
    (elapsed <= progress.length).then_some(Progress {
        elapsed,
        length: progress.length,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::SenderClock;
    use crate::stream::StreamConfig;

    #[test]
    fn progress_set_earlier_is_carried_to_a_speaker_that_starts_later() {
        let set_at = Instant::now();
        let progress = Progress {
            elapsed: Duration::from_secs(30),
            length: Duration::from_secs(40),
        };
        assert_eq!(progress_at(progress, set_at, set_at), Some(progress));
        let later = progress_at(progress, set_at, set_at + Duration::from_secs(4));
        assert_eq!(
            later.map(|later| later.elapsed),
            Some(Duration::from_secs(34))
        );
        assert_eq!(
            progress_at(progress, set_at, set_at + Duration::from_secs(11)),
            None
        );
    }

    // A pause FLUSH that no session sends (no AirPlay 1 session, and
    // `ap2_flush` gives false) doesn't hold the resume (P213).
    #[test]
    fn a_pause_flush_no_session_sends_does_not_hold_the_resume() {
        use crate::discovery::DeviceId;
        use crate::session::{MediaDestinations, ReceiverPorts};

        let now = Instant::now();
        let (stream, _writer) = Stream::new(StreamConfig::default(), SenderClock::new(now), now);
        let (inputs, receiver) = mpsc::channel();
        let (events, _events) = mpsc::channel();
        let latency = Arc::new(AtomicU32::new(0));
        let mut driver = Driver::new(stream, receiver, inputs, events, latency);
        let destinations = MediaDestinations::new(
            "192.0.2.21:7000".parse().expect("address"),
            ReceiverPorts {
                server_port: 50_000,
                control_port: 50_001,
                timing_port: 50_002,
            },
        );
        let config = SpeakerConfig::new(Codec::Alac, destinations, 1, 7, 99);
        let id = driver.stream.add_speaker(config, now);
        let key = DeviceKey::Id(DeviceId([0x02, 0x00, 0x5E, 0x00, 0x53, 1]));
        driver.stream_ids.insert(id, key);
        let mut later = now;
        for _ in 0..100 {
            later += Duration::from_millis(10);
            driver.stream.handle_timeout(later);
        }
        while driver.stream.poll_transmit().is_some() {}
        while driver.stream.poll_event().is_some() {}

        driver.command(Command::Pause, later);
        assert!(driver.stream.waits_for_flush(id));
        while let Some(event) = driver.stream.poll_event() {
            driver.stream_event(event, later);
        }
        assert!(!driver.stream.waits_for_flush(id));
    }

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
            .send(Input::Command(Box::new(Command::Shutdown)))
            .expect("the driver runs");
        run.join().expect("the driver ends");
        let took = started.elapsed();
        let _ = release.send(());
        assert!(took < Duration::from_secs(1), "shutdown took {took:?}");
    }

    // The old PTP service's thread still runs when a session wants the
    // service bound again: the start waits for a later turn instead of
    // joining it, and the speaker keeps the AirPlay 2 place (P158, P169).
    #[test]
    fn a_ptp_bind_waits_for_the_old_service_without_stalling_the_driver() {
        // The picked ports are let go before the driver binds them, so a
        // parallel test can take one; only that clash picks again.
        for _ in 0..5 {
            if ptp_bind_after_the_old_service() {
                return;
            }
        }
        panic!("another socket took a picked PTP port on every pick");
    }

    /// False when a picked port was taken before the driver bound it, which
    /// the driver tells as PTP unavailable.
    fn ptp_bind_after_the_old_service() -> bool {
        use crate::discovery::{DeviceId, ScopedAddr, ServiceKind, ServiceRecord};
        use std::net::{IpAddr, Ipv4Addr, TcpListener, UdpSocket};

        let now = Instant::now();
        let (stream, _writer) = Stream::new(StreamConfig::default(), SenderClock::new(now), now);
        let (inputs, receiver) = mpsc::channel();
        let (events, _events) = mpsc::channel();
        let latency = Arc::new(AtomicU32::new(0));
        // Both sockets are held until both are bound, so the two ports
        // differ, and on the address the driver binds.
        let bind_ip = crate::ptp::service_bind_ip(IpAddr::V4(Ipv4Addr::LOCALHOST));
        let free = || {
            let [event, general] = [(); 2].map(|()| {
                UdpSocket::bind((bind_ip, 0))
                    .and_then(|socket| Ok((socket.local_addr()?.port(), socket)))
                    .expect("a free port")
            });
            crate::ptp::PtpPorts {
                event: event.0,
                general: general.0,
            }
        };
        let (local, remote) = (free(), free());
        let mut driver = Driver::new(stream, receiver, inputs, events, latency)
            .with_airplay2(true, Some([0x10, 0x20, 0x30, 0x40, 0x50, 0x60]))
            .with_ptp_ports(local, remote);

        // Receivers that take the connection and never answer.
        let listener = TcpListener::bind("127.0.0.1:0").expect("a listener");
        let port = listener.local_addr().expect("an address").port();
        let device = |n: u8| {
            let id = DeviceId([0x02, 0x00, 0x5E, 0x00, 0x53, n]);
            let record = ServiceRecord {
                kind: ServiceKind::AirPlay,
                instance: format!("Speaker {n}"),
                host: format!("speaker-{n}.local"),
                port,
                txt: [
                    "features=0x0,0x10240".to_owned(),
                    "flags=0x4".to_owned(),
                    "model=Speaker1,1".to_owned(),
                    format!("deviceid={id}"),
                ]
                .into_iter()
                .map(String::into_bytes)
                .collect(),
                addresses: vec![ScopedAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST))],
            };
            Device::from_records(DeviceKey::Id(id), None, Some(&record)).expect("a device")
        };
        let options = || SpeakerOptions::new(Volume::Db(-20.0));
        let counters = || Arc::new(SpeakerCounters::default());

        let (release, stuck) = mpsc::channel::<()>();
        driver.ptp_closing.push(thread::spawn(move || {
            let _ = stuck.recv_timeout(Duration::from_secs(10));
        }));
        let (first, second) = (device(1), device(2));
        let (first_key, second_key) = (first.key.clone(), second.key.clone());
        let started = Instant::now();
        driver.add_speaker(first, options(), counters(), now);
        let took = started.elapsed();
        assert!(took < Duration::from_secs(1), "the driver waited {took:?}");
        assert!(driver.speakers[&first_key].holds_ap2());
        assert!(driver.ptp.is_none());
        driver.add_speaker(second, options(), counters(), now);
        assert!(
            driver.speakers[&second_key].ap2_waiting,
            "a second AirPlay 2 speaker is refused meanwhile"
        );

        let _ = release.send(());
        let deadline = Instant::now() + Duration::from_secs(5);
        while driver
            .ptp_closing
            .iter()
            .any(|thread| !thread.is_finished())
        {
            assert!(Instant::now() < deadline, "the old thread ends");
            thread::sleep(Duration::from_millis(5));
        }
        driver.run_timers(now + Duration::from_secs(1));
        assert!(matches!(driver.speakers[&first_key].phase, Phase::Ap2(_)));
        if driver.ptp_unavailable_told {
            return false;
        }
        assert!(driver.ptp.is_some(), "the service is bound");
        assert!(driver.speakers[&second_key].ap2_waiting);
        drop(listener);
        true
    }

    /// A verifier without keys, for driving a session by hand.
    struct OpenVerifier;

    impl crate::session::Verifier for OpenVerifier {
        fn first_request(&mut self) -> Vec<u8> {
            vec![1, 0, 0, 0]
        }

        fn second_request(
            &mut self,
            _first_response: &[u8],
        ) -> Result<Vec<u8>, crate::session::VerifyFailure> {
            Ok(vec![0, 0, 0, 0])
        }
    }

    fn answer(cseq: u32, body: &[u8]) -> Vec<u8> {
        let mut response = crate::rtsp::Response::new(200, "OK");
        response.headers.push("CSeq", cseq.to_string());
        response.body = body.to_vec();
        response.encode()
    }

    // After the reconnect of P58, the earlier connection's reader and writer
    // can both still report its close (and the reader its last bytes). Those
    // carry the earlier connect attempt and are dropped, so they don't end
    // the new attempt as Unreachable.
    #[test]
    fn late_events_from_the_connection_before_a_reconnect_are_dropped() {
        use crate::discovery::{ScopedAddr, ServiceKind, ServiceRecord};
        use crate::session::Connection;

        let now = Instant::now();
        let (stream, _writer) = Stream::new(StreamConfig::default(), SenderClock::new(now), now);
        let (inputs, receiver) = mpsc::channel();
        let (events, emitted) = mpsc::channel();
        let latency = Arc::new(AtomicU32::new(0));
        let mut driver = Driver::new(stream, receiver, inputs, events, latency);

        // A receiver address that takes the reconnect and never answers.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("a listener");
        let address = listener.local_addr().expect("an address");
        let record = ServiceRecord {
            kind: ServiceKind::Raop,
            instance: "0123456789AB@Living Room".to_owned(),
            host: "living.local".to_owned(),
            port: address.port(),
            txt: vec![b"et=0".to_vec()],
            addresses: vec![ScopedAddr::new(address.ip())],
        };
        let device =
            Device::from_records(record.device_key(), Some(&record), None).expect("a device");
        let key = device.key.clone();

        // A session whose second pair-verify step was answered 200, with
        // OPTIONS out: a close now gets the one reconnect.
        let mut config = SessionConfig::new(vec![address], Volume::Db(-20.0));
        config.verifier = Some(Box::new(OpenVerifier));
        let ids = SessionIds::generate(&mut OsEntropy).expect("identifiers");
        let mut session = Session::new(config, ids, now);
        let connection = Connection {
            local: address.ip(),
            control_port: 1,
            timing_port: 2,
        };
        session.handle_connected(connection, now);
        session.handle_input(&answer(0, &[0x5A; 96]), now);
        session.handle_input(&answer(1, &[]), now);
        while session.poll_output().is_some() {}
        let session_key = SessionKey(0);
        driver.next_session = 1;
        driver.sessions.insert(session_key, key.clone());
        driver.speakers.insert(
            key.clone(),
            Speaker {
                device,
                options: SpeakerOptions::new(Volume::Db(-20.0)),
                counters: Arc::new(SpeakerCounters::default()),
                backoff: Backoff::new(),
                phase: Phase::Active(Box::new(Active {
                    key: session_key,
                    session,
                    codec: Codec::Alac,
                    software_volume: false,
                    software_mute: false,
                    attempt: 1,
                    io: None,
                    stream_id: None,
                })),
                removing: false,
                airplay1_outcome: None,
                ap2_waiting: false,
                ap2_refusal: 0,
                ap2_retry: false,
                reads_volume: false,
                target: None,
                next_start: StartKind::Host,
                told_connecting: false,
            },
        );

        // The reader's close: the session connects again, as attempt 2.
        let closed = |attempt| IoEvent::RtspClosed {
            session: session_key,
            attempt,
        };
        driver.io_event(closed(1), now);
        driver.flush(now);
        let state = |driver: &mut Driver| {
            driver
                .active(&key)
                .map(|active| (active.session.state(), active.attempt))
        };
        assert_eq!(state(&mut driver), Some((State::Connecting, 2)));

        // The writer's close and the reader's last bytes, late: dropped.
        driver.io_event(closed(1), now);
        driver.io_event(closed(1), now);
        driver.io_event(
            IoEvent::Rtsp {
                session: session_key,
                attempt: 1,
                bytes: answer(2, &[]),
            },
            now,
        );
        driver.flush(now);
        assert_eq!(state(&mut driver), Some((State::Connecting, 2)));
        let failed = emitted.try_iter().any(|event| {
            matches!(
                event,
                SenderEvent::Speaker {
                    event: SpeakerEvent::Error(_),
                    ..
                }
            )
        });
        assert!(!failed, "no error from the earlier connection's events");

        // The current attempt's close still counts (here: Unreachable, as
        // it closes while connecting).
        driver.io_event(closed(2), now);
        driver.flush(now);
        let failed = emitted.try_iter().any(|event| {
            matches!(
                event,
                SenderEvent::Speaker {
                    event: SpeakerEvent::Error(_),
                    ..
                }
            )
        });
        assert!(failed);
        drop(listener);
    }
}
