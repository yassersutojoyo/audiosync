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
//! session to it then verifies with them before ANNOUNCE (P55). An Apple
//! TV that also asks for a password is paired with
//! [`Sender::start_pairing_with_password`] (P45, P46).
//!
//! Discovery only hints that a receiver needs pairing, through its
//! verdict ([`Verdict::needs_pairing`](crate::discovery::Verdict::needs_pairing),
//! and [`Verdict::legacy_pairing`](crate::discovery::Verdict::legacy_pairing)
//! for an Apple TV this PIN pairing serves), as it hints that one is busy
//! (P137). PairingRequired itself comes only from a connection: a session
//! whose OPTIONS answer calls for pairing, or whose verification fails
//! (P46, P58).

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
use crate::discovery::{Browser, Device, DeviceKey, ProtocolPath};
use crate::error::{ErrorKind, SessionError, UnsupportedReason, UserReason, Warning};
use crate::homekit::HomeKitCredentials;
use crate::input::InputWriter;
use crate::metadata::{Metadata, MetadataError, TrackInfo};
use crate::pairing::{self, Credentials, PairingEvent, PinError};
pub use crate::ptp::PtpPorts;
use crate::rate::NOMINAL_RATE;
use crate::rtsp::JackStatus;
use crate::secret::Secret;
use crate::stream::{
    DEFAULT_INPUT_FRAMES, DEFAULT_LATENCY_FRAMES, MAX_DELAY, SpeakerCounters, SpeakerLatency,
    Stream, StreamConfig, StreamCounters,
};
use crate::targets::{Target, TargetChange, TargetId};
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
    /// The hidden AirPlay 2 switch (P2), off by default until the real
    /// receiver checks pass. A host sets it from [`AIRPLAY2_SWITCH_ENV`]
    /// with [`airplay2_switch`]; while it is off nothing of the AirPlay 2
    /// path runs (P170).
    pub airplay2: bool,
    /// The AirPlay 2 device ID (P5, P43): a random value the host made once
    /// and stored ([`random_airplay2_device_id`]). `None` gives a random one
    /// for this sender's lifetime, until [`Sender::set_airplay2_device_id`].
    pub airplay2_device_id: Option<[u8; 6]>,
    /// The ports the PTP service binds, 319 and 320 (P158); a test picks
    /// others so it doesn't need the real ones.
    pub ptp_ports: PtpPorts,
    /// The receiver's PTP ports it sends to, 319 and 320 (P158).
    pub ptp_receiver_ports: PtpPorts,
}

/// The environment variable a host reads for [`SenderConfig::airplay2`].
pub const AIRPLAY2_SWITCH_ENV: &str = "AUDIOSYNC_AIRPLAY2";

/// The switch's value from the environment variable's: on only for `1`.
pub fn airplay2_switch(value: Option<&str>) -> bool {
    value == Some("1")
}

/// The debug hook that lets the devices it names use PCM where they offer
/// it (P74, G10): a comma-separated list of speaker IDs, with no UI.
pub const AIRPLAY_PCM_ENV: &str = "AUDIOSYNC_AIRPLAY_PCM";

/// The speaker IDs in [`AIRPLAY_PCM_ENV`]'s value, trimmed, empty ones
/// skipped. A device ID written with colons or in lowercase
/// (`aa:bb:cc:dd:ee:ff`) becomes the form speaker IDs use, `AABBCCDDEEFF`.
pub fn pcm_debug_devices(value: Option<&str>) -> std::collections::BTreeSet<String> {
    value
        .unwrap_or_default()
        .split(',')
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .map(|id| {
            let hex: String = id.chars().filter(|c| *c != ':').collect();
            if hex.len() == 12 && hex.chars().all(|c| c.is_ascii_hexdigit()) {
                hex.to_ascii_uppercase()
            } else {
                id.to_owned()
            }
        })
        .collect()
}

/// The options [`Sender::add_target`] gives each member of `target`:
/// `member` fills in its own password and credentials, and the target's
/// delay, volume and PCM choice override whatever `member` set (P179, P180).
pub fn member_options(
    target: &Target,
    options: &SpeakerOptions,
    mut member: impl FnMut(&DeviceKey, &mut SpeakerOptions),
) -> Vec<(DeviceKey, SpeakerOptions)> {
    target
        .members
        .iter()
        .map(|target_member| {
            let key = &target_member.key;
            let mut own = options.clone();
            member(key, &mut own);
            own.delay = options.delay;
            own.volume = options.volume;
            own.pcm = options.pcm;
            (key.clone(), own)
        })
        .collect()
}

/// A new random AirPlay 2 device ID for a host to store once (P5), or
/// `None` when the random source is unavailable.
pub fn random_airplay2_device_id() -> Option<[u8; 6]> {
    use crate::ids::{Entropy, OsEntropy};
    let mut id = [0u8; 6];
    OsEntropy.fill(&mut id).ok()?;
    Some(id)
}

impl Default for SenderConfig {
    fn default() -> SenderConfig {
        SenderConfig {
            latency_frames: DEFAULT_LATENCY_FRAMES,
            input_frames: DEFAULT_INPUT_FRAMES,
            clock: None,
            airplay2: false,
            airplay2_device_id: None,
            ptp_ports: PtpPorts::default(),
            ptp_receiver_ports: PtpPorts::default(),
        }
    }
}

/// How the host wants a speaker played.
#[derive(Clone, PartialEq)]
pub struct SpeakerOptions {
    /// d_s, 0 to 20 000 ms (P99).
    pub delay: Duration,
    /// The speaker volume (P118).
    pub volume: Volume,
    /// The speaker's password, if it has one (P39). Never logged; wiped
    /// from memory when dropped.
    pub password: Option<Secret>,
    /// The host enabled PCM for this speaker (P74).
    pub pcm: bool,
    /// The credentials of an earlier pairing with this Apple TV (P54): each
    /// session verifies with them before ANNOUNCE (P55). Never logged.
    pub credentials: Option<Credentials>,
    /// HomeKit credentials of an earlier pairing on the AirPlay 2 path
    /// (P152): its sessions pair-verify with them (P61). Never logged.
    pub homekit_credentials: Option<HomeKitCredentials>,
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
            homekit_credentials: None,
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
            .field(
                "homekit_credentials",
                &self.homekit_credentials.as_ref().map(|_| "<hidden>"),
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
    /// Only when the host knows the track's start and length (P126), and
    /// only while the track plays: a speaker that starts later gets it
    /// carried on by the time since it was set.
    pub progress: Option<Progress>,
}

/// A receiver as discovery sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReceiverInfo {
    pub device: Device,
    /// Whether a session can start: `Ok`, or the error it would end with
    /// before connecting, Unsupported with its reason (P139, P2). Never
    /// PairingRequired, which only a connection reports (P46): an Apple TV
    /// whose flags call for the legacy pairing is `Ok` here, and discovery's
    /// hint that it likely needs a PIN is its verdict's `needs_pairing`
    /// (P16) and `legacy_pairing`, passed like the busy hint (P137). Added
    /// without credentials, such a speaker is asked OPTIONS and then
    /// reports PairingRequired, or Unsupported when it can't take AirPlay 1
    /// audio; it plays once the host pairs it ([`Sender::start_pairing`])
    /// and adds it with the credentials.
    pub plan: Result<(), ErrorKind>,
    /// The stereo pair or home theater group it is a member of (P176). Its
    /// shared group key, when it has one and no target, is only
    /// information ([`Device::group_keys`], P175).
    pub target: Option<TargetId>,
}

impl ReceiverInfo {
    pub fn new(device: Device) -> ReceiverInfo {
        let plan = plan(&device);
        ReceiverInfo {
            device,
            plan,
            target: None,
        }
    }
}

/// The protocol path (P2), then the start conditions known before
/// connecting (P139). The legacy pairing of an Apple TV without
/// credentials is no such condition: only a connection reports
/// PairingRequired (P46), so the plan stays `Ok` for it.
pub(crate) fn plan(device: &Device) -> Result<(), ErrorKind> {
    match plan_with(device, false, false) {
        Err(ErrorKind::PairingRequired) => Ok(()),
        plan => plan,
    }
}

/// The same, for a speaker the host has (or hasn't) credentials for, with
/// the AirPlay 2 switch as the host set it.
pub(crate) fn plan_with(
    device: &Device,
    has_credentials: bool,
    airplay2: bool,
) -> Result<(), ErrorKind> {
    match device.choose_path(airplay2, None) {
        Ok(ProtocolPath::AirPlay1) => device.airplay1_plan(has_credentials),
        // The driver starts an AirPlay 2 session before it asks for this
        // plan, so only a host's view of the device lands here.
        Ok(ProtocolPath::AirPlay2) => Err(ErrorKind::Unsupported(UnsupportedReason::AirPlay2Only)),
        Err(reason) => Err(ErrorKind::Unsupported(reason)),
    }
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
    /// A stereo pair or home theater group appeared or changed, or went
    /// away (P176). Its members are reported as devices too, before it.
    Target(TargetChange),
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
    /// Its session bound the PTP ports after the host was told they were
    /// taken ([`Warning::PtpUnavailable`]): a clash after this is told
    /// again (P158).
    PtpOpened,
    /// A PIN or password pairing on the AirPlay 2 path gave these HomeKit
    /// credentials (P152): the host stores them where secrets go and hands
    /// them back in [`SpeakerOptions::homekit_credentials`]. The sender
    /// keeps them too, until it stops. `Debug` shows the identifiers only.
    HomeKitPaired(HomeKitCredentials),
    /// The receiver no longer takes the speaker's HomeKit credentials
    /// (P154): the sender dropped its copy, and the host deletes the stored
    /// one, so the next start pairs again. A PairingRequired error follows.
    HomeKitRejected,
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
    /// Discovery stopped for good: its browser can never report anything
    /// again ([`Browser::is_dead`]), which only a bug in the browser causes.
    /// Every receiver it listed was reported gone just before. Speakers keep
    /// playing; a host that wants discovery back starts it again, or
    /// replaces the sender.
    DiscoveryFailed,
    /// The members of a target the host added that play nothing now: out
    /// of view at the start, or lost and waiting for a retry (P181). Sent
    /// whenever the list changes, empty once every member plays again.
    TargetMissing {
        id: TargetId,
        missing: Vec<DeviceKey>,
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
        )
        .with_airplay2(config.airplay2, config.airplay2_device_id)
        .with_ptp_ports(config.ptp_ports, config.ptp_receiver_ports);
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
        let _ = self.inputs.send(Input::Command(Box::new(command)));
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
    /// 20 000 ms) so it plays in step with the speakers: L + d_local, counted
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

    /// Plays a stereo pair or home theater group: every member in view gets
    /// a session of its own, on one path, with one delay and one volume,
    /// those of `options`; the control device of a home theater group gets
    /// none (P177 to P180). `member` fills in each member's own password
    /// and credentials. A member out of view joins when discovery sees it
    /// again (P181); members that join the target later wait for the next
    /// add (P176). Adding a target already added updates its delay and
    /// volume.
    ///
    /// Speaker events come for each member under its own key; the returned
    /// counters are the members'.
    pub fn add_target(
        &self,
        target: Target,
        options: SpeakerOptions,
        member: impl FnMut(&DeviceKey, &mut SpeakerOptions),
    ) -> Vec<(DeviceKey, Arc<SpeakerCounters>)> {
        let members: Vec<_> = member_options(&target, &options, member)
            .into_iter()
            .map(|(key, own)| {
                let counters = self
                    .speaker_counters
                    .borrow_mut()
                    .entry(key.clone())
                    .or_insert_with(|| {
                        Arc::new(SpeakerCounters::for_stream(self.stream_counters.clone()))
                    })
                    .clone();
                (key, own, counters)
            })
            .collect();
        let counters = members
            .iter()
            .map(|(key, _, counters)| (key.clone(), counters.clone()))
            .collect();
        self.command(Command::AddTarget {
            target: Box::new(target),
            members,
        });
        counters
    }

    /// Stops every member of a target, as [`Sender::remove_speaker`] does.
    pub fn remove_target(&self, id: &TargetId) {
        self.command(Command::RemoveTarget(id.clone()));
    }

    /// Sets a target's one delay d_s for all its members (P179).
    pub fn set_target_delay(&self, id: &TargetId, delay: Duration) {
        self.command(Command::SetTargetDelay(id.clone(), delay));
    }

    /// Sets a target's one volume, sent to every member (P180).
    pub fn set_target_volume(&self, id: &TargetId, volume: Volume) {
        self.command(Command::SetTargetVolume(id.clone(), volume));
    }

    /// Asks a target's leader for its volume, or the first member that
    /// plays when the leader doesn't (P180); the answer comes as
    /// [`SpeakerEvent::VolumeReported`] under that member's key.
    pub fn read_target_volume(&self, id: &TargetId) {
        self.command(Command::ReadTargetVolume(id.clone()));
    }

    /// The stored AirPlay 2 device ID (P5, P43), for the AirPlay 2 sessions
    /// that start from now on; a running one keeps the ID it sent.
    pub fn set_airplay2_device_id(&self, id: [u8; 6]) {
        self.command(Command::SetAirPlay2DeviceId(id));
    }

    /// Stops a speaker: its packets stop, TEARDOWN goes out, and its
    /// connection closes after the answer or 2 s (P129). Retries end.
    pub fn remove_speaker(&self, key: &DeviceKey) {
        self.speaker_counters.borrow_mut().remove(key);
        self.command(Command::RemoveSpeaker(key.clone()));
    }

    /// Pairs with an Apple TV that needs the legacy pairing (P46 to P54): on
    /// its own connection to the receiver's RAOP port, it is asked OPTIONS
    /// first (a receiver that can't take AirPlay 1 audio ends the pairing
    /// as Unsupported, with no PIN shown), then to show a PIN, and
    /// [`PairingEvent::PinRequested`] follows. The pairing then waits,
    /// without a timeout, for [`Sender::submit_pin`] (P51), and ends with
    /// [`PairingEvent::Paired`] and the credentials, or
    /// [`PairingEvent::Failed`]. A pairing already running for the device is
    /// replaced. Playing speakers are not touched. The same as
    /// [`Sender::start_pairing_with_password`] without a password: a
    /// receiver that also asks for one ends the pairing with
    /// PasswordRequired before it shows a PIN.
    pub fn start_pairing(&self, device: Device) {
        self.start_pairing_with_password(device, None);
    }

    /// [`Sender::start_pairing`] with the speaker's password, if the host
    /// has one (P39, P45, P46): a 401 to the pairing's OPTIONS is answered
    /// with digest authentication, and every later pairing request carries
    /// `Authorization` (P36, P38). Without a password that 401 ends the
    /// pairing with PasswordRequired before `/pair-pin-start`; a password
    /// the receiver turns down ends it with WrongPassword (P135). The
    /// password is never logged.
    pub fn start_pairing_with_password(&self, device: Device, password: Option<Secret>) {
        self.command(Command::StartPairing {
            device: Box::new(device),
            password,
        });
    }

    /// The PIN the user read off the Apple TV (P50, P51). Digits only;
    /// anything else is refused here. A PIN for a receiver whose pairing
    /// isn't waiting for one is ignored. The copy that travels to the
    /// sender's thread is wiped from memory once used.
    pub fn submit_pin(&self, key: &DeviceKey, pin: &str) -> Result<(), PinError> {
        pairing::check_pin(pin)?;
        self.command(Command::SubmitPin(key.clone(), Secret::from(pin)));
        Ok(())
    }

    /// Stops a pairing; [`PairingEvent::Stopped`] follows.
    pub fn cancel_pairing(&self, key: &DeviceKey) {
        self.command(Command::CancelPairing(key.clone()));
    }

    /// Forgets the AirPlay 2 credentials the sender keeps for a device,
    /// after the user forgot its pairing: its next session pairs again. The
    /// host hears nothing back; a session already playing goes on.
    pub fn forget_homekit(&self, key: &DeviceKey) {
        self.command(Command::ForgetHomeKit(key.clone()));
    }

    /// Sets a speaker's delay d_s, 0 to 20 000 ms (P99, P100).
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

    /// False once the driver thread has ended while this sender still
    /// exists, which only a bug does (a panic, in a build that unwinds):
    /// the sender then no longer lists, plays or reports anything, and a
    /// host drops it and starts a new one. Read without locks or waiting.
    pub fn is_running(&self) -> bool {
        self.driver
            .as_ref()
            .is_some_and(|driver| !driver.is_finished())
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
