//! The sender a host app drives: discovery, speakers, one shared audio
//! stream, events and counters, on the crate's own threads (P6).
//!
//! [`Sender::start`] starts the stream and its driver thread and gives the
//! host the [`InputWriter`] for its capture thread. Every other call is a
//! message to the driver: it returns at once, never touches the network
//! and never blocks on a speaker. What happens comes back as
//! [`SenderEvent`]s, which the host reads with [`Sender::poll_event`] or
//! [`Sender::wait_event`] from its own thread; counters are read without
//! locks through the handles [`Sender::add_speaker`] returns (P144).
//!
//! ```no_run
//! use std::time::{Duration, Instant};
//! use audiosync_airplay::sender::{Sender, SenderConfig, SenderEvent, ReceiverEvent, SpeakerOptions};
//! use audiosync_airplay::volume::Volume;
//!
//! let (sender, mut input) = Sender::start(SenderConfig::default())?;
//! sender.start_discovery()?;
//! // On the capture thread: input.write_f32(&samples, Instant::now());
//! # let _ = &mut input;
//! while let Some(event) = sender.wait_event(Duration::from_secs(1)) {
//!     if let SenderEvent::Receiver(ReceiverEvent::Appeared(info)) = event {
//!         if info.plan.is_ok() {
//!             let counters = sender.add_speaker(info.device.clone(), SpeakerOptions::new(Volume::Db(-20.0)));
//!             # let _ = counters;
//!         }
//!     }
//! }
//! sender.shutdown();
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```
//!
//! Each speaker gets its own session, sockets and threads (P3); a speaker
//! that fails never stops another one or the input (P141), and one that is
//! lost is tried again with backoff and rejoins the running timeline
//! (P142, P103). [`Sender::shutdown`] tears every speaker down in parallel
//! and returns within 2 s (P129).
//!
//! An Apple TV that needs the legacy pairing (P46) is paired once:
//! [`Sender::start_pairing`] makes it show a PIN and reports
//! [`PairingEvent::PinRequested`]; the host asks the user and calls
//! [`Sender::submit_pin`]; [`PairingEvent::Paired`] then carries the
//! [`Credentials`], which the host stores (for example with
//! [`Credentials::to_bytes`]) and passes in
//! [`SpeakerOptions::credentials`] whenever it adds that speaker. Every
//! session to it then verifies with them before ANNOUNCE (P55).

mod driver;

use std::cell::RefCell;
use std::collections::HashMap;
use std::fmt;
use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::clock::SenderClock;
use crate::discovery::mdns::{DiscoveryError, MdnsBrowser};
use crate::discovery::{Browser, Device, DeviceKey};
use crate::error::{ErrorKind, SessionError, UserReason, Warning};
use crate::input::InputWriter;
use crate::metadata::{Metadata, MetadataError, TrackInfo};
use crate::pairing::{self, Credentials, PairingEvent, PinError};
use crate::rate::NOMINAL_RATE;
use crate::rtsp::JackStatus;
use crate::stream::{
    DEFAULT_INPUT_FRAMES, DEFAULT_LATENCY_FRAMES, MAX_DELAY, SpeakerCounters, SpeakerLatency,
    Stream, StreamConfig, StreamCounters,
};
use crate::volume::Volume;

use driver::{Command, Driver, Input};

/// How a sender starts.
#[derive(Debug, Clone, Copy)]
pub struct SenderConfig {
    /// The base latency Lf in frames, 11 025 to 88 200 (P98).
    pub latency_frames: u32,
    /// The input ring in frames, at least one second (P111).
    pub input_frames: usize,
    /// The sender clock (P89): the host passes the clock its local outputs
    /// run on, or leaves it `None` for one that starts now.
    pub clock: Option<SenderClock>,
}

impl Default for SenderConfig {
    fn default() -> SenderConfig {
        SenderConfig {
            latency_frames: DEFAULT_LATENCY_FRAMES,
            input_frames: DEFAULT_INPUT_FRAMES,
            clock: None,
        }
    }
}

/// How the host wants a speaker played.
#[derive(Clone, PartialEq)]
pub struct SpeakerOptions {
    /// d_s, 0 to 3000 ms (P99).
    pub delay: Duration,
    /// The speaker volume (P118).
    pub volume: Volume,
    /// The speaker's password, if it has one (P39). Never logged.
    pub password: Option<String>,
    /// The host enabled PCM for this speaker (P74).
    pub pcm: bool,
    /// The credentials of an earlier pairing with this Apple TV (P54): each
    /// session verifies with them before ANNOUNCE (P55). Never logged.
    pub credentials: Option<Credentials>,
}

impl SpeakerOptions {
    /// No delay, no password, ALAC unless PCM is the speaker's only codec.
    pub fn new(volume: Volume) -> SpeakerOptions {
        SpeakerOptions {
            delay: Duration::ZERO,
            volume,
            password: None,
            pcm: false,
            credentials: None,
        }
    }
}

impl fmt::Debug for SpeakerOptions {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SpeakerOptions")
            .field("delay", &self.delay)
            .field("volume", &self.volume)
            .field("password", &self.password.as_ref().map(|_| "<hidden>"))
            .field("pcm", &self.pcm)
            .field(
                "credentials",
                &self.credentials.as_ref().map(Credentials::client_id),
            )
            .finish()
    }
}

/// Track progress, for receivers that show it (P126): how far into the
/// track the audio arriving now is, and how long the track is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Progress {
    pub elapsed: Duration,
    pub length: Duration,
}

/// What is playing, sent to every speaker that lists the type (P123 to
/// P127). Each item applies from the audio arriving when it is set.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NowPlaying {
    pub track: Option<TrackInfo>,
    /// A JPEG of at most 512 KiB (P125).
    pub artwork: Option<Vec<u8>>,
    /// Only when the host knows the track's start and length (P126).
    pub progress: Option<Progress>,
}

/// A receiver as discovery sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReceiverInfo {
    pub device: Device,
    /// Whether a session can start without credentials: `Ok`, or the
    /// error it would end with before connecting: Unsupported with its
    /// reason (P139, P2), or PairingRequired for an Apple TV that needs the
    /// legacy pairing (P46). Such an Apple TV plays once the host pairs it
    /// ([`Sender::start_pairing`]) and adds it with the credentials.
    pub plan: Result<(), ErrorKind>,
}

impl ReceiverInfo {
    pub fn new(device: Device) -> ReceiverInfo {
        let plan = plan(&device);
        ReceiverInfo { device, plan }
    }
}

/// The AirPlay 1 path with the AirPlay 2 path off (P2), then the start
/// conditions without credentials (P46, P139).
pub(crate) fn plan(device: &Device) -> Result<(), ErrorKind> {
    plan_with(device, false)
}

/// The same, for a speaker the host has (or hasn't) credentials for.
pub(crate) fn plan_with(device: &Device, has_credentials: bool) -> Result<(), ErrorKind> {
    device
        .choose_path(false, None)
        .map_err(ErrorKind::Unsupported)?;
    device.airplay1_plan(has_credentials)
}

/// A change discovery saw (P7, P18, P19).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReceiverEvent {
    Appeared(Box<ReceiverInfo>),
    /// Its records or verdict changed; running sessions are untouched.
    Changed(Box<ReceiverInfo>),
    /// It is gone from the network (or discovery stopped); a running
    /// session goes on until its own connection says otherwise (P18).
    Gone(DeviceKey),
}

/// Where a speaker is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpeakerState {
    /// A session is starting: connect, then OPTIONS up to RECORD.
    Connecting,
    /// RECORD succeeded: the speaker plays the shared timeline.
    Playing,
    /// The last session failed; the next starts at `at` (P142).
    Retrying { at: Instant },
    /// The last session failed with an error that needs the user; no retry
    /// until the host adds the speaker again (P142).
    Failed,
    /// The speaker was removed, or the sender shut down, and its session is
    /// closed.
    Stopped,
}

/// An error that ended a speaker's session (P133).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpeakerError {
    pub error: SessionError,
    /// What to tell the user.
    pub reason: UserReason,
    /// When the next session starts, or `None` when the user must act
    /// (P142).
    pub retry_at: Option<Instant>,
}

/// Something about one speaker.
#[derive(Debug, Clone, PartialEq)]
pub enum SpeakerEvent {
    State(SpeakerState),
    Error(SpeakerError),
    /// Something to show; the session goes on.
    Warning(Warning),
    /// Its latencies, sent when it starts playing and whenever its first
    /// sound time moves (a delay change, an Lf change, a restart): L + d_s,
    /// the receiver's own `Audio-Latency`, and when its first packet is due
    /// to be heard (P110, P113, P115).
    Latency(SpeakerLatency),
    /// The OPTIONS answer's methods (P27).
    Methods(Vec<String>),
    /// The receiver's `Server` header (P24).
    Server(String),
    /// `Audio-Jack-Status` (P24).
    AudioJack(JackStatus),
    /// The level a volume read reported (P122).
    VolumeReported(f64),
}

/// What the sender reports.
#[derive(Debug, Clone, PartialEq)]
pub enum SenderEvent {
    Receiver(ReceiverEvent),
    Speaker {
        key: DeviceKey,
        event: SpeakerEvent,
    },
    /// Something about the pairing with a receiver (P48 to P54, P136).
    Pairing {
        key: DeviceKey,
        event: PairingEvent,
    },
    /// L and the timeline's mapping: frame n of the timeline is the host's
    /// frame at input position n + `origin`, and it sounds on every speaker
    /// at c(n) + L + d_s. Sent once at the start and again when Lf changes,
    /// the timeline restarts, or a paused capture resumes; the host
    /// re-anchors its local outputs to it (P98, P114, P143).
    Timeline {
        latency: Duration,
        origin: i64,
        restarted: bool,
    },
}

/// The sender: one stream, any number of speakers, optional discovery.
pub struct Sender {
    inputs: mpsc::Sender<Input>,
    events: mpsc::Receiver<SenderEvent>,
    driver: Option<JoinHandle<()>>,
    clock: SenderClock,
    stream_counters: Arc<StreamCounters>,
    latency_frames: Arc<AtomicU32>,
    speaker_counters: RefCell<HashMap<DeviceKey, Arc<SpeakerCounters>>>,
}

impl fmt::Debug for Sender {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Sender")
            .field("clock", &self.clock)
            .field("latency_frames", &self.latency_frames)
            .finish()
    }
}

impl Sender {
    /// Starts the stream, whose timeline begins now, and the driver thread,
    /// and gives the host its audio input (P6, P96). All stream buffers are
    /// allocated here (P111).
    pub fn start(config: SenderConfig) -> io::Result<(Sender, InputWriter)> {
        let clock = config
            .clock
            .unwrap_or_else(|| SenderClock::new(Instant::now()));
        let stream_config = StreamConfig {
            latency_frames: config.latency_frames,
            input_frames: config.input_frames,
        };
        let (stream, writer) = Stream::new(stream_config, clock, Instant::now());
        let stream_counters = stream.counters();
        let latency_frames = Arc::new(AtomicU32::new(stream.latency_frames()));
        let (inputs, receiver) = mpsc::channel();
        let (events_out, events) = mpsc::channel();
        let driver = Driver::new(
            stream,
            receiver,
            inputs.clone(),
            events_out,
            latency_frames.clone(),
        );
        let handle = thread::Builder::new()
            .name("airplay-driver".to_owned())
            .spawn(move || driver.run())?;
        let sender = Sender {
            inputs,
            events,
            driver: Some(handle),
            clock,
            stream_counters,
            latency_frames,
            speaker_counters: RefCell::new(HashMap::new()),
        };
        Ok((sender, writer))
    }

    fn command(&self, command: Command) {
        let _ = self.inputs.send(Input::Command(command));
    }

    /// The sender clock: the host schedules its local outputs on it (P89).
    pub fn clock(&self) -> SenderClock {
        self.clock
    }

    /// L, the base latency (P98, P113). Read without locks.
    pub fn latency(&self) -> Duration {
        let frames = self.latency_frames.load(Ordering::Relaxed);
        Duration::from_secs_f64(f64::from(frames) / NOMINAL_RATE)
    }

    /// The delay a host gives a local output with trim `local` (0 to
    /// 3000 ms) so it plays in step with the speakers: L + d_local, counted
    /// on the sender clock from c(n) (P114).
    pub fn local_output_delay(&self, local: Duration) -> Duration {
        self.latency() + local.min(MAX_DELAY)
    }

    /// c(n): when timeline frame `n` arrived, as the rate model gives it
    /// (P96, P113). Asks the driver and waits for its answer, at most one
    /// second.
    pub fn arrival_time(&self, n: i64) -> Option<Instant> {
        let (reply, answer) = mpsc::sync_channel(1);
        self.command(Command::ArrivalTime(n, reply));
        answer.recv_timeout(Duration::from_secs(1)).ok()
    }

    /// The stream's counters (P144), read without locks.
    pub fn stream_counters(&self) -> Arc<StreamCounters> {
        self.stream_counters.clone()
    }

    /// Starts browsing for receivers on the local network (P7). Receivers
    /// arrive as [`ReceiverEvent`]s.
    pub fn start_discovery(&self) -> Result<(), DiscoveryError> {
        let browser = MdnsBrowser::new()?;
        self.start_discovery_with(Box::new(browser));
        Ok(())
    }

    /// Starts discovery with a browser the host or a test supplies.
    pub fn start_discovery_with(&self, browser: Box<dyn Browser + Send>) {
        self.command(Command::StartDiscovery(browser));
    }

    /// Stops browsing; every receiver discovery listed is reported gone.
    /// Speakers keep playing.
    pub fn stop_discovery(&self) {
        self.command(Command::StopDiscovery);
    }

    /// Adds a speaker, or updates one already added. A new speaker starts a
    /// session at once. For one already added, the delay and volume apply
    /// to the running session, the device, password and PCM choice to the
    /// next one, and a speaker that failed or waits for a retry starts a new
    /// session now.
    ///
    /// The returned counters stay valid for as long as the speaker is
    /// added, across its reconnects (P144).
    pub fn add_speaker(&self, device: Device, options: SpeakerOptions) -> Arc<SpeakerCounters> {
        let counters = self
            .speaker_counters
            .borrow_mut()
            .entry(device.key.clone())
            .or_insert_with(|| Arc::new(SpeakerCounters::for_stream(self.stream_counters.clone())))
            .clone();
        self.command(Command::AddSpeaker {
            device: Box::new(device),
            options,
            counters: counters.clone(),
        });
        counters
    }

    /// Stops a speaker: its packets stop, TEARDOWN goes out, and its
    /// connection closes after the answer or 2 s (P129). Retries end.
    pub fn remove_speaker(&self, key: &DeviceKey) {
        self.speaker_counters.borrow_mut().remove(key);
        self.command(Command::RemoveSpeaker(key.clone()));
    }

    /// Pairs with an Apple TV that needs the legacy pairing (P46 to P54): on
    /// its own connection to the receiver's RAOP port, the Apple TV is asked
    /// to show a PIN, and [`PairingEvent::PinRequested`] follows. The
    /// pairing then waits, without a timeout, for [`Sender::submit_pin`]
    /// (P51), and ends with [`PairingEvent::Paired`] and the credentials, or
    /// [`PairingEvent::Failed`]. A pairing already running for the device is
    /// replaced. Playing speakers are not touched.
    pub fn start_pairing(&self, device: Device) {
        self.command(Command::StartPairing(Box::new(device)));
    }

    /// The PIN the user read off the Apple TV (P50, P51). Digits only;
    /// anything else is refused here. A PIN for a receiver whose pairing
    /// isn't waiting for one is ignored.
    pub fn submit_pin(&self, key: &DeviceKey, pin: &str) -> Result<(), PinError> {
        pairing::check_pin(pin)?;
        self.command(Command::SubmitPin(key.clone(), pin.to_owned()));
        Ok(())
    }

    /// Stops a pairing; [`PairingEvent::Stopped`] follows.
    pub fn cancel_pairing(&self, key: &DeviceKey) {
        self.command(Command::CancelPairing(key.clone()));
    }

    /// Sets a speaker's delay d_s, 0 to 3000 ms (P99, P100).
    pub fn set_delay(&self, key: &DeviceKey, delay: Duration) {
        self.command(Command::SetDelay(key.clone(), delay));
    }

    /// Sets a speaker's volume (P118, P120, P121). NaN levels are ignored.
    pub fn set_volume(&self, key: &DeviceKey, volume: Volume) {
        self.command(Command::SetVolume(key.clone(), volume));
    }

    /// Asks a playing speaker for its volume (P122); the answer comes as
    /// [`SpeakerEvent::VolumeReported`].
    pub fn read_volume(&self, key: &DeviceKey) {
        self.command(Command::ReadVolume(key.clone()));
    }

    /// Sets what is playing and sends it to every speaker that lists the
    /// type, now and when a speaker (re)starts (P123 to P127). Artwork that
    /// isn't a JPEG of at most 512 KiB is refused here.
    pub fn set_now_playing(&self, now_playing: NowPlaying) -> Result<(), MetadataError> {
        if let Some(jpeg) = &now_playing.artwork {
            Metadata::Artwork {
                jpeg: jpeg.clone(),
                rtptime: 0,
            }
            .validate()?;
        }
        self.command(Command::SetNowPlaying(now_playing));
        Ok(())
    }

    /// Sets Lf, 11 025 to 88 200 frames; every speaker is flushed and the
    /// new L comes as a [`SenderEvent::Timeline`] (P98).
    pub fn set_latency(&self, frames: u32) {
        self.command(Command::SetLatency(frames));
    }

    /// The host restarted its capture (P143): every speaker is flushed and
    /// the timeline starts over; the new mapping comes as a
    /// [`SenderEvent::Timeline`].
    pub fn restart_timeline(&self) {
        self.command(Command::RestartTimeline);
    }

    /// The next event, if one is waiting.
    pub fn poll_event(&self) -> Option<SenderEvent> {
        self.events.try_recv().ok()
    }

    /// The next event, waiting at most `timeout` for one.
    pub fn wait_event(&self, timeout: Duration) -> Option<SenderEvent> {
        self.events.recv_timeout(timeout).ok()
    }

    /// Stops everything: every speaker is torn down in parallel (TEARDOWN,
    /// then close after the answer, or by force 1.75 s after this call),
    /// pairings stop, discovery stops, and the driver's threads end (P129).
    /// Returns within
    /// 2 s. A writer blocked in a send to a receiver that stopped reading is
    /// not waited for; it ends at its own send timeout.
    pub fn shutdown(mut self) {
        self.stop();
    }

    fn stop(&mut self) {
        if let Some(handle) = self.driver.take() {
            self.command(Command::Shutdown);
            let _ = handle.join();
        }
    }
}

impl Drop for Sender {
    fn drop(&mut self) {
        self.stop();
    }
}
