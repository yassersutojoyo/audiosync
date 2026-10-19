//! AirPlay speakers, through AudioSync's own sender (`audiosync-airplay`).
//!
//! The sender starts with the app and browses the network for receivers.
//! Every receiver it finds is listed, with whether it can play: this sender
//! speaks AirPlay 1 without a password, so a receiver that asks for a
//! password, or that only AirPlay 2 can play, is listed with that reason and
//! never connected. An Apple TV that asks for a PIN is paired once: when the
//! user switches it on, the sender asks it to show a PIN, the UI prompts for
//! it ([`AirPlaySpeakers::pin_prompt`]), and the credentials the pairing
//! gives are kept in the [`PairingStore`] and handed back each time the
//! speaker joins, so no PIN is asked again. A wrong PIN starts a new pairing
//! (the Apple TV shows a new PIN); any other failure leaves the speaker off
//! with a notice. A speaker the user turns on joins the
//! sender's stream while streaming is on, and leaves it when it is turned off
//! or streaming stops. Its volume and its own delay d_s go to the sender as
//! they change. Errors come back as backend events, which the UI turns into
//! notices: a speaker that needs the user (a password, pairing, a format it
//! can't take) is turned off, while one that stopped answering stays on as
//! the sender keeps trying (spec P142).
//!
//! An AirPlay speaker counts as chosen (S79) while it is on and either listed
//! by discovery as able to play, or playing: a receiver whose announcement
//! lapses while it plays still plays, and one that is gone, or listed as
//! unable to play, and not playing doesn't hold the local outputs back.
//!
//! What discovery reports never turns a speaker off: a speaker with two
//! services whose RAOP record lapses first reads "AirPlay 2 only" for a
//! moment before it is gone (EC12). Such a speaker stays on, its session
//! goes on, and it only stays out of new joins until it is listed as able to
//! play again. Only a session error that needs the user, or the user, turns
//! a speaker off. While one is chosen, local outputs wait for the AirPlay
//! speakers: L, the sender's latency plus the user's AirPlay delay
//! adjustment, comes on top of their hold-backs (spec S176, S178, see
//! [`super::outputs`]).
//!
//! The capture feeds the sender through an [`AirPlayFeed`]: after the
//! equalizer, at the session rate, each block is resampled to 44.1 kHz and
//! handed to the sender's input. That runs on the capture thread, so it never
//! allocates, locks, blocks or logs (P6). One feed exists for the app's
//! lifetime; each capture session borrows it through a [`FeedSlot`] and gives
//! it back when the capture ends.
//!
//! A sender that stopped after an internal error, or whose discovery did,
//! is reported once as [`BackendEvent::AirPlayStopped`]; the user's Retry
//! replaces it with a new one from a [`SenderStarter`], and the speakers
//! that were on keep their settings and play again once listed.
//!
//! The sender sits behind [`AirPlayLink`], so the backend logic is tested
//! with a fake; [`SenderLink`] is the real one.

use std::collections::BTreeMap;
use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use audiosync_airplay::discovery::mdns::DiscoveryError;
use audiosync_airplay::discovery::{Device, DeviceKey, ServiceKind};
use audiosync_airplay::error::{ErrorKind, UnsupportedReason, UserReason};
use audiosync_airplay::input::InputWriter;
use audiosync_airplay::pairing::{PairingEvent, PinError};
use audiosync_airplay::sender::{
    ReceiverEvent, ReceiverInfo, Sender, SenderConfig, SenderEvent, SpeakerError, SpeakerEvent,
    SpeakerOptions, SpeakerState,
};
use audiosync_airplay::stream::{MAX_DELAY, SpeakerCounters, StreamCounters};
use audiosync_airplay::volume::{MIN_DB, Volume};
use audiosync_core::fade::InputFade;
use audiosync_core::resample::Resampler;
use audiosync_core::volume::{DEFAULT_LOCAL_POSITION, sanitize_position};

use super::SESSION_RATE;
use super::backend::{
    AirPlayAvailability, AirPlayInfo, AirPlayPinPrompt, AirPlayProblem, BackendEvent, PairingPhase,
};
use super::pairings::PairingStore;
use crate::debug_log::DebugLog;

/// The sender's input rate (P72).
const AIRPLAY_RATE: u32 = audiosync_airplay::audio::SAMPLE_RATE;

/// Session-rate frames the feed resamples per step; bounds its buffer.
const FEED_CHUNK: usize = 1024;

/// The calls the backend makes on the AirPlay sender.
pub trait AirPlayLink {
    /// Adds a speaker, or updates one already added (its delay and volume at
    /// once, its device for its next session). Returns its counters when the
    /// sender keeps them.
    fn add_speaker(
        &mut self,
        device: &Device,
        options: SpeakerOptions,
    ) -> Option<Arc<SpeakerCounters>>;

    /// Stops a speaker: TEARDOWN, then its connection closes.
    fn remove_speaker(&mut self, key: &DeviceKey);

    /// Sets a speaker's volume.
    fn set_volume(&mut self, key: &DeviceKey, volume: Volume);

    /// Sets a speaker's own delay d_s.
    fn set_delay(&mut self, key: &DeviceKey, delay: Duration);

    /// The capture restarted: every speaker is flushed and the timeline
    /// starts over (P143).
    fn restart_timeline(&mut self);

    /// L, the sender's base latency.
    fn latency(&self) -> Duration;

    /// The stream's counters, when the sender keeps them.
    fn stream_counters(&self) -> Option<Arc<StreamCounters>>;

    /// The next event, if one is waiting; never blocks.
    fn poll_event(&mut self) -> Option<SenderEvent>;

    /// Starts pairing with an Apple TV that asks for a PIN, with the
    /// speaker's password when AudioSync has one (an Apple TV can ask for
    /// both); a pairing already running for it is replaced.
    fn start_pairing(&mut self, device: &Device, password: Option<String>);

    /// The PIN the user typed: digits only.
    fn submit_pin(&mut self, key: &DeviceKey, pin: &str) -> Result<(), PinError>;

    /// Stops a pairing.
    fn cancel_pairing(&mut self, key: &DeviceKey);

    /// False once the sender stopped by itself after an internal error (its
    /// driver thread ended): it lists, plays and reports nothing anymore,
    /// and only a new sender brings AirPlay back.
    fn is_running(&self) -> bool;
}

/// Starts a new sender, with the feed for its input, to replace one that
/// stopped; `None` when it couldn't start (it logs why).
pub type SenderStarter = Box<dyn FnMut() -> Option<(Box<dyn AirPlayLink>, Option<FeedSlot>)>>;

/// The real sender.
pub struct SenderLink {
    sender: Sender,
}

impl SenderLink {
    /// Starts the sender, whose stream and threads run from now on, and
    /// gives its audio input for the capture.
    pub fn start() -> io::Result<(SenderLink, InputWriter)> {
        let (sender, input) = Sender::start(SenderConfig::default())?;
        Ok((SenderLink { sender }, input))
    }

    /// Starts browsing for receivers.
    pub fn start_discovery(&self) -> Result<(), DiscoveryError> {
        self.sender.start_discovery()
    }
}

impl AirPlayLink for SenderLink {
    fn add_speaker(
        &mut self,
        device: &Device,
        options: SpeakerOptions,
    ) -> Option<Arc<SpeakerCounters>> {
        Some(self.sender.add_speaker(device.clone(), options))
    }

    fn remove_speaker(&mut self, key: &DeviceKey) {
        self.sender.remove_speaker(key);
    }

    fn set_volume(&mut self, key: &DeviceKey, volume: Volume) {
        self.sender.set_volume(key, volume);
    }

    fn set_delay(&mut self, key: &DeviceKey, delay: Duration) {
        self.sender.set_delay(key, delay);
    }

    fn restart_timeline(&mut self) {
        self.sender.restart_timeline();
    }

    fn latency(&self) -> Duration {
        self.sender.latency()
    }

    fn stream_counters(&self) -> Option<Arc<StreamCounters>> {
        Some(self.sender.stream_counters())
    }

    fn poll_event(&mut self) -> Option<SenderEvent> {
        self.sender.poll_event()
    }

    fn start_pairing(&mut self, device: &Device, password: Option<String>) {
        self.sender
            .start_pairing_with_password(device.clone(), password);
    }

    fn submit_pin(&mut self, key: &DeviceKey, pin: &str) -> Result<(), PinError> {
        self.sender.submit_pin(key, pin)
    }

    fn cancel_pairing(&mut self, key: &DeviceKey) {
        self.sender.cancel_pairing(key);
    }

    fn is_running(&self) -> bool {
        self.sender.is_running()
    }
}

/// The capture's way into the sender: resamples equalized session-rate
/// audio to 44.1 kHz and hands it to the sender's input, faded in over the
/// first 20 ms of each capture and out after a stop request (S189). Every
/// buffer is allocated by [`AirPlayFeed::new`].
pub struct AirPlayFeed {
    input: InputWriter,
    resampler: Resampler,
    /// Resampler output, stereo.
    resampled: Box<[f32]>,
    /// The fade at the session rate, before the resampler.
    fade: InputFade,
}

impl AirPlayFeed {
    /// A feed into `input`.
    pub fn new(input: InputWriter) -> AirPlayFeed {
        let resampler = Resampler::new(SESSION_RATE, AIRPLAY_RATE)
            .expect("both rates are supported resampler rates");
        let frames = (FEED_CHUNK as u64 * u64::from(AIRPLAY_RATE)).div_ceil(u64::from(SESSION_RATE))
            as usize
            + 2;
        AirPlayFeed {
            input,
            resampler,
            resampled: vec![0.0; frames * 2].into_boxed_slice(),
            fade: InputFade::new(SESSION_RATE, FEED_CHUNK),
        }
    }

    /// Hands the first `frames` frames of interleaved stereo `block`, at the
    /// session rate, to the sender as audio that arrived at `at`, faded
    /// (S189); `block` itself stays as it is. Never allocates, locks, blocks
    /// or logs (P6).
    pub fn push(&mut self, block: &[f32], frames: usize, at: Instant) {
        let input = &block[..(frames * 2).min(block.len())];
        let (resampler, resampled, writer) =
            (&mut self.resampler, &mut self.resampled, &mut self.input);
        self.fade.process(input, |piece| {
            resample_into(resampler, resampled, writer, piece, at);
        });
    }

    /// The sender's input position: frames handed over so far.
    #[cfg(test)]
    pub fn position(&self) -> u64 {
        self.input.position()
    }
}

/// Resamples `block` into `resampled` and hands it to `writer` as audio that
/// arrived `at`.
fn resample_into(
    resampler: &mut Resampler,
    resampled: &mut [f32],
    writer: &mut InputWriter,
    block: &[f32],
    at: Instant,
) {
    let mut consumed = 0;
    while consumed * 2 < block.len() {
        let (used, produced) = resampler.process(&block[consumed * 2..], resampled);
        if produced > 0 {
            writer.write_f32(&resampled[..produced * 2], at);
        }
        consumed += used;
        if used == 0 && produced == 0 {
            break;
        }
    }
}

/// Where the one [`AirPlayFeed`] waits between capture sessions.
#[derive(Clone)]
pub struct FeedSlot {
    feed: Arc<Mutex<Option<AirPlayFeed>>>,
    /// A stop asked the capture's feed to fade out (S186, S189); cleared by
    /// the next lease.
    closing: Arc<AtomicBool>,
}

impl FeedSlot {
    /// A slot holding a feed into `input`.
    pub fn new(input: InputWriter) -> FeedSlot {
        FeedSlot {
            feed: Arc::new(Mutex::new(Some(AirPlayFeed::new(input)))),
            closing: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Lends the feed to a capture, starting its resampler afresh and its
    /// fade-in from silence; `None` while another capture holds it.
    pub fn lease(&self) -> Option<FeedLease> {
        let mut feed = self
            .feed
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()?;
        feed.resampler.reset();
        feed.fade.restart(SESSION_RATE);
        self.closing.store(false, Ordering::Release);
        Some(FeedLease {
            feed: Some(feed),
            slot: Arc::clone(&self.feed),
            closing: Arc::clone(&self.closing),
        })
    }

    /// Asks the capture's feed to fade out over the next 20 ms it pushes and
    /// to hand the sender zeros after that, until the capture ends (S186,
    /// S189). Any thread; a release store.
    pub fn request_fade_out(&self) {
        self.closing.store(true, Ordering::Release);
    }

    /// Whether the feed is in the slot, lent to no capture.
    #[cfg(test)]
    pub fn is_free(&self) -> bool {
        self.feed
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .is_some()
    }
}

/// The feed, lent to one capture. Dropping it, when the capture ends, puts
/// the feed back in its slot.
pub struct FeedLease {
    feed: Option<AirPlayFeed>,
    slot: Arc<Mutex<Option<AirPlayFeed>>>,
    closing: Arc<AtomicBool>,
}

impl FeedLease {
    /// See [`AirPlayFeed::push`]; after a stop request the feed fades out.
    pub fn push(&mut self, block: &[f32], frames: usize, at: Instant) {
        if let Some(feed) = self.feed.as_mut() {
            if self.closing.load(Ordering::Acquire) {
                feed.fade.fade_out();
            }
            feed.push(block, frames, at);
        }
    }

    /// The sender's input position: frames handed over so far.
    #[cfg(test)]
    pub fn position(&self) -> u64 {
        self.feed.as_ref().map_or(0, AirPlayFeed::position)
    }
}

impl Drop for FeedLease {
    fn drop(&mut self) {
        if let Some(feed) = self.feed.take() {
            *self.slot.lock().unwrap_or_else(PoisonError::into_inner) = Some(feed);
        }
    }
}

/// A speaker's ID: the device ID it announces (12 hex digits), or, for a
/// service without one, its kind and instance name.
pub fn speaker_id(key: &DeviceKey) -> String {
    match key {
        DeviceKey::Id(id) => id.to_string(),
        DeviceKey::Instance(ServiceKind::Raop, instance) => format!("raop:{instance}"),
        DeviceKey::Instance(ServiceKind::AirPlay, instance) => format!("airplay:{instance}"),
    }
}

/// A pairing event for the debug log. It never carries the PIN, the
/// credentials' secret or a key: only the client ID, which the pairing sends
/// in the clear anyway, and the error.
pub fn pairing_summary(event: &PairingEvent) -> String {
    match event {
        PairingEvent::PinRequested => "the Apple TV shows a PIN".to_owned(),
        PairingEvent::Paired(credentials) => {
            format!("paired as client {}", credentials.client_id())
        }
        PairingEvent::Failed { error, wrong_pin } => {
            let pin = if *wrong_pin { " (wrong PIN)" } else { "" };
            format!("failed: {error}{pin}")
        }
        PairingEvent::Stopped => "stopped".to_owned(),
    }
}

/// Whether a receiver can play from this sender: AirPlay 1, without a
/// password, and for an Apple TV that asks for a PIN, once `paired`.
/// Discovery only hints at pairing (its verdict's flags); the sender
/// reports PairingRequired only from a connection.
pub fn availability(info: &ReceiverInfo, paired: bool) -> AirPlayAvailability {
    let verdict = &info.device.verdict;
    match info.plan {
        Err(ErrorKind::Unsupported(UnsupportedReason::AirPlay2Only)) => {
            AirPlayAvailability::AirPlay2NotYet
        }
        Err(_) => AirPlayAvailability::Unsupported,
        // The legacy pairing's status flags stay set once paired, so they
        // don't count then.
        Ok(()) if verdict.legacy_pairing && !paired => AirPlayAvailability::NeedsPin,
        Ok(()) if verdict.needs_password => AirPlayAvailability::PasswordNotYet,
        Ok(()) if verdict.needs_pairing && !verdict.legacy_pairing => {
            AirPlayAvailability::PairingNotYet
        }
        Ok(()) => AirPlayAvailability::Open,
    }
}

/// What to tell the user about an error that ended a speaker's session.
pub fn problem(reason: UserReason) -> AirPlayProblem {
    match reason {
        UserReason::NotResponding => AirPlayProblem::NotResponding,
        UserReason::ConnectionLost => AirPlayProblem::ConnectionLost,
        UserReason::Busy => AirPlayProblem::Busy,
        UserReason::Refused | UserReason::Incompatible => AirPlayProblem::Refused,
        UserReason::PasswordRequired | UserReason::WrongPassword => AirPlayProblem::PasswordNotYet,
        // A session to an Apple TV whose credentials it turned down, or a
        // receiver that wants another kind of pairing.
        UserReason::PairingRequired => AirPlayProblem::PairingNotYet,
        UserReason::Unsupported(UnsupportedReason::AirPlay2Only) => AirPlayProblem::AirPlay2NotYet,
        UserReason::Unsupported(_) => AirPlayProblem::Unsupported,
    }
}

/// The volume an AirPlay speaker gets for its slider position and the master
/// position (P118 leaves the mapping to the host): the product of both, 0 to
/// 1, spread evenly over −30 to 0 dB, and muted at 0.
pub fn airplay_volume(position: f64, master: f64) -> Volume {
    let level = (position / 100.0).clamp(0.0, 1.0) * (master / 100.0).clamp(0.0, 1.0);
    if level > 0.0 {
        Volume::Db(MIN_DB * (1.0 - level))
    } else {
        Volume::Muted
    }
}

/// A delay setting as the sender's d_s, 0 to 3000 ms.
fn speaker_delay(delay_ms: f64) -> Duration {
    Duration::from_secs_f64(delay_ms.clamp(0.0, MAX_DELAY.as_secs_f64() * 1000.0) / 1000.0)
}

/// A volume as the debug log writes it.
fn describe_volume(volume: Volume) -> String {
    match volume {
        Volume::Muted => "muted".to_owned(),
        Volume::Db(db) => format!("{db:.1} dB"),
    }
}

/// An availability as the debug log writes it.
fn describe_availability(availability: AirPlayAvailability) -> &'static str {
    match availability {
        AirPlayAvailability::Open => "can play",
        AirPlayAvailability::PasswordNotYet => "asks for a password (not supported yet)",
        AirPlayAvailability::NeedsPin => "needs pairing with a PIN",
        AirPlayAvailability::PairingNotYet => "needs pairing (not supported yet)",
        AirPlayAvailability::AirPlay2NotYet => "AirPlay 2 only (not supported yet)",
        AirPlayAvailability::Unsupported => "not supported",
    }
}

/// The settings of one AirPlay speaker.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Settings {
    enabled: bool,
    /// The slider position, 0 to 100.
    volume: f64,
    /// d_s in ms, 0 to 3000.
    delay_ms: f64,
}

impl Default for Settings {
    fn default() -> Settings {
        Settings {
            enabled: false,
            volume: DEFAULT_LOCAL_POSITION,
            delay_ms: 0.0,
        }
    }
}

/// A receiver discovery lists.
struct Listed {
    info: ReceiverInfo,
}

impl Listed {
    fn device(&self) -> &Device {
        &self.info.device
    }
}

/// A pairing with an Apple TV the user switched on.
struct PairingRun {
    /// The device it pairs with, to start again after a wrong PIN.
    device: Device,
    phase: PairingPhase,
    /// The Apple TV turned the last PIN down.
    wrong_pin: bool,
    /// The Apple TV showed a PIN, so the UI prompts for it until the pairing
    /// ends.
    shown: bool,
    /// When its PIN first showed (counts up), so the prompt stays with the
    /// Apple TV that asked first while another one asks too.
    shown_order: u64,
}

/// A speaker in the sender.
struct Added {
    key: DeviceKey,
    state: SpeakerState,
    counters: Option<Arc<SpeakerCounters>>,
}

/// The AirPlay side of the backend: what discovery lists, the user's
/// settings, and the speakers in the sender. See the module documentation.
pub struct AirPlaySpeakers {
    link: Box<dyn AirPlayLink>,
    /// The receivers listed now, by speaker ID.
    listed: BTreeMap<String, Listed>,
    /// The settings of every speaker the user changed, kept while it's gone.
    settings: BTreeMap<String, Settings>,
    /// The speakers in the sender, by speaker ID.
    added: BTreeMap<String, Added>,
    /// The last name of every speaker seen, for logs and notices.
    names: BTreeMap<String, String>,
    /// The credentials of the paired Apple TVs.
    store: PairingStore,
    /// The pairings running, by speaker ID.
    pairings: BTreeMap<String, PairingRun>,
    /// The next `PairingRun::shown_order`.
    next_shown_order: u64,
    streaming: bool,
    master: f64,
    latency: Duration,
    /// The sender stopped after an internal error, or its discovery did;
    /// reported once, until a new sender replaces it.
    stopped: bool,
    log: DebugLog,
}

impl AirPlaySpeakers {
    /// The AirPlay side over `link`, with nothing listed or on, and the
    /// paired Apple TVs' credentials from `store`.
    pub fn new(
        link: Box<dyn AirPlayLink>,
        master: f64,
        log: DebugLog,
        store: PairingStore,
    ) -> AirPlaySpeakers {
        let latency = link.latency();
        AirPlaySpeakers {
            link,
            listed: BTreeMap::new(),
            settings: BTreeMap::new(),
            added: BTreeMap::new(),
            names: BTreeMap::new(),
            store,
            pairings: BTreeMap::new(),
            next_shown_order: 0,
            streaming: false,
            master,
            latency,
            stopped: false,
            log,
        }
    }

    /// Replaces the sender with `link`, a new one: what the old one listed,
    /// played and paired is gone with it, while the user's settings stay, so
    /// the speakers that were on join again once the new one lists them.
    /// Whether the sender stopped by itself (see `sender_stopped`); only
    /// then does a Retry replace it.
    pub fn is_stopped(&self) -> bool {
        self.stopped
    }

    pub fn replace_link(&mut self, link: Box<dyn AirPlayLink>) {
        // The old sender stops as it is dropped.
        self.link = link;
        self.listed.clear();
        self.added.clear();
        self.pairings.clear();
        self.latency = self.link.latency();
        self.stopped = false;
        self.log.log(|| {
            format!(
                "AirPlay: a new sender runs, L = {} ms; the speakers that were on join once listed",
                self.latency.as_millis()
            )
        });
    }

    /// The sender stopped by itself: nothing it listed, played or paired is
    /// there anymore. Returns whether the speaker list changed.
    fn sender_stopped(&mut self, events: &mut Vec<BackendEvent>) -> bool {
        let changed =
            !(self.listed.is_empty() && self.added.is_empty() && self.pairings.is_empty());
        self.listed.clear();
        self.added.clear();
        self.pairings.clear();
        if changed || !self.stopped {
            self.log.log(|| {
                "AirPlay: the sender stopped after an internal error; AirPlay stays silent until Retry"
                    .to_owned()
            });
        }
        if !self.stopped {
            self.stopped = true;
            events.push(BackendEvent::AirPlayStopped);
        }
        changed
    }

    /// Sends what happens to `log`.
    pub fn set_debug_log(&mut self, log: DebugLog) {
        self.log = log;
    }

    /// L, as the sender last reported it.
    pub fn latency(&self) -> Duration {
        self.latency
    }

    fn settings(&self, id: &str) -> Settings {
        self.settings.get(id).copied().unwrap_or_default()
    }

    fn entry(&mut self, id: &str) -> &mut Settings {
        self.settings.entry(id.to_owned()).or_default()
    }

    /// How the log names a speaker.
    fn label(&self, id: &str) -> String {
        match self.names.get(id) {
            Some(name) => format!("\"{name}\" ({id})"),
            None => format!("({id})"),
        }
    }

    fn is_playing(&self, id: &str) -> bool {
        self.added
            .get(id)
            .is_some_and(|added| added.state == SpeakerState::Playing)
    }

    /// Whether a listed speaker can play now, with the credentials kept.
    fn listed_availability(&self, id: &str) -> Option<AirPlayAvailability> {
        self.listed
            .get(id)
            .map(|listed| availability(&listed.info, self.store.contains(id)))
    }

    fn is_open(&self, id: &str) -> bool {
        self.listed_availability(id) == Some(AirPlayAvailability::Open)
    }

    /// Whether an AirPlay speaker is chosen: on, and listed as able to play
    /// or playing (S79, see the module documentation).
    pub fn chosen(&self) -> bool {
        self.settings
            .iter()
            .any(|(id, settings)| settings.enabled && (self.is_open(id) || self.is_playing(id)))
    }

    /// The speakers to show: the listed ones, and any that still plays
    /// while its announcement lapsed, by name.
    pub fn speakers(&self) -> Vec<AirPlayInfo> {
        let mut ids: Vec<&String> = self.listed.keys().collect();
        ids.extend(
            self.added
                .keys()
                .filter(|id| !self.listed.contains_key(*id) && self.is_playing(id)),
        );
        let mut speakers: Vec<AirPlayInfo> = ids
            .into_iter()
            .map(|id| {
                let settings = self.settings(id);
                AirPlayInfo {
                    id: id.clone(),
                    name: self.names.get(id).cloned().unwrap_or_else(|| id.clone()),
                    availability: self
                        .listed_availability(id)
                        .unwrap_or(AirPlayAvailability::Open),
                    enabled: settings.enabled,
                    volume: settings.volume,
                    delay_ms: settings.delay_ms,
                    playing: self.is_playing(id),
                    paired: self.store.contains(id),
                    pairing: self.pairings.contains_key(id),
                }
            })
            .collect();
        speakers.sort_by(|a, b| {
            a.name
                .to_lowercase()
                .cmp(&b.name.to_lowercase())
                .then_with(|| a.id.cmp(&b.id))
        });
        speakers
    }

    /// The IDs of the speakers turned on, listed or not, sorted.
    pub fn enabled_ids(&self) -> Vec<String> {
        self.settings
            .iter()
            .filter(|(_, settings)| settings.enabled)
            .map(|(id, _)| id.clone())
            .collect()
    }

    /// Turns a speaker on or off; one that can't play stays off, and an
    /// Apple TV that isn't paired yet returns [`AirPlayProblem::NeedsPin`]
    /// (see [`AirPlaySpeakers::start_pairing`]). Turning a speaker off stops
    /// its pairing.
    pub fn set_enabled(&mut self, id: &str, enabled: bool) -> Result<(), AirPlayProblem> {
        if enabled
            && let Some(availability) = self.listed_availability(id)
            && let Some(problem) = availability.problem()
        {
            self.log.log(|| {
                format!(
                    "AirPlay speaker {} stays off: it {}",
                    self.label(id),
                    describe_availability(availability)
                )
            });
            return Err(problem);
        }
        self.log.log(|| {
            let state = if enabled { "on" } else { "off" };
            format!("AirPlay speaker {} switched {state}", self.label(id))
        });
        if !enabled {
            self.stop_pairing(id, "the speaker was switched off");
        }
        self.entry(id).enabled = enabled;
        self.reconcile();
        Ok(())
    }

    /// Starts pairing with an Apple TV that asks for a PIN, which the user
    /// switched on. A pairing already running for it goes on. Paired, the
    /// speaker is turned on ([`BackendEvent::AirPlayPaired`]).
    pub fn start_pairing(&mut self, id: &str) -> Result<(), AirPlayProblem> {
        let Some(listed) = self.listed.get(id) else {
            self.log.log(|| {
                format!(
                    "AirPlay pairing {}: not started, the speaker isn't listed",
                    self.label(id)
                )
            });
            return Err(AirPlayProblem::Unsupported);
        };
        match self.listed_availability(id) {
            Some(AirPlayAvailability::NeedsPin) => {}
            Some(availability) => return availability.problem().map_or(Ok(()), Err),
            None => return Err(AirPlayProblem::Unsupported),
        }
        if self.pairings.contains_key(id) {
            self.log.log(|| {
                format!(
                    "AirPlay pairing {}: already running, it goes on",
                    self.label(id)
                )
            });
            return Ok(());
        }
        let device = listed.device().clone();
        self.log.log(|| {
            format!(
                "AirPlay pairing {}: started, asking the Apple TV to show a PIN",
                self.label(id)
            )
        });
        let password = self.password(id);
        self.link.start_pairing(&device, password);
        self.pairings.insert(
            id.to_owned(),
            PairingRun {
                device,
                phase: PairingPhase::Starting,
                wrong_pin: false,
                shown: false,
                shown_order: 0,
            },
        );
        Ok(())
    }

    /// Hands the PIN the user typed to the speaker's pairing. Returns whether
    /// it went to the Apple TV: only digits, while the pairing waits for a
    /// PIN. The PIN is never logged.
    pub fn submit_pin(&mut self, id: &str, pin: &str) -> bool {
        let Some(run) = self.pairings.get_mut(id) else {
            return false;
        };
        if run.phase != PairingPhase::WaitingForPin {
            return false;
        }
        let key = run.device.key.clone();
        if let Err(error) = self.link.submit_pin(&key, pin) {
            self.log
                .log(|| format!("AirPlay pairing {}: PIN refused: {error}", self.label(id)));
            return false;
        }
        if let Some(run) = self.pairings.get_mut(id) {
            run.phase = PairingPhase::Checking;
            run.wrong_pin = false;
        }
        self.log.log(|| {
            format!(
                "AirPlay pairing {}: PIN sent, waiting for the Apple TV",
                self.label(id)
            )
        });
        true
    }

    /// The user cancelled the speaker's pairing; it stays off.
    pub fn cancel_pairing(&mut self, id: &str) {
        self.stop_pairing(id, "cancelled by the user");
    }

    /// Stops the speaker's pairing, if one runs, saying why in the log.
    fn stop_pairing(&mut self, id: &str, why: &str) {
        if let Some(run) = self.pairings.remove(id) {
            self.link.cancel_pairing(&run.device.key);
            self.log
                .log(|| format!("AirPlay pairing {}: stopped, {why}", self.label(id)));
        }
    }

    /// Forgets the speaker's credentials: it is turned off, and pairs again
    /// the next time the user switches it on.
    pub fn forget_pairing(&mut self, id: &str) {
        self.stop_pairing(id, "the user forgot the pairing");
        match self.store.remove(id) {
            Ok(true) => self.log.log(|| {
                format!(
                    "AirPlay pairing {}: credentials forgotten",
                    self.label(id)
                )
            }),
            Ok(false) => return,
            Err(error) => self.log.log(|| {
                format!(
                    "AirPlay pairing {}: credentials forgotten for this run, but the store couldn't be written: {error}",
                    self.label(id)
                )
            }),
        }
        if self.settings(id).enabled {
            self.entry(id).enabled = false;
            self.reconcile();
        }
    }

    /// The PIN prompt to show: the first pairing whose Apple TV showed a PIN.
    pub fn pin_prompt(&self) -> Option<AirPlayPinPrompt> {
        // The Apple TV that asked first keeps the prompt until its pairing
        // ends, so a second one asking meanwhile never takes over the field.
        self.pairings
            .iter()
            .filter(|(_, run)| run.shown)
            .min_by_key(|(_, run)| run.shown_order)
            .map(|(id, run)| AirPlayPinPrompt {
                id: id.clone(),
                name: self.names.get(id).cloned().unwrap_or_else(|| id.clone()),
                phase: run.phase,
                wrong_pin: run.wrong_pin,
            })
    }

    /// Sets a speaker's slider position, live.
    pub fn set_volume(&mut self, id: &str, position: f64) {
        let Some(position) = sanitize_position(position) else {
            return;
        };
        if self.settings(id).volume == position {
            return;
        }
        self.entry(id).volume = position;
        let volume = airplay_volume(position, self.master);
        self.log.log(|| {
            format!(
                "AirPlay speaker {} volume: {position} ({})",
                self.label(id),
                describe_volume(volume)
            )
        });
        if let Some(added) = self.added.get(id) {
            self.link.set_volume(&added.key, volume);
        }
    }

    /// Sets the master position, which scales every speaker, live.
    pub fn set_master(&mut self, master: f64) {
        self.master = master;
        for (id, added) in &self.added {
            let volume = airplay_volume(self.settings(id).volume, master);
            self.link.set_volume(&added.key, volume);
        }
    }

    /// Sets a speaker's own delay d_s, live.
    pub fn set_delay(&mut self, id: &str, delay_ms: f64) {
        if !delay_ms.is_finite() {
            return;
        }
        let delay = speaker_delay(delay_ms);
        let delay_ms = delay.as_secs_f64() * 1000.0;
        if self.settings(id).delay_ms == delay_ms {
            return;
        }
        self.entry(id).delay_ms = delay_ms;
        self.log
            .log(|| format!("AirPlay speaker {} delay: {delay_ms} ms", self.label(id)));
        if let Some(added) = self.added.get(id) {
            self.link.set_delay(&added.key, delay);
        }
    }

    /// Streaming started or stopped: speakers that are on join the stream or
    /// leave it.
    pub fn set_streaming(&mut self, streaming: bool) {
        self.streaming = streaming;
        self.reconcile();
    }

    /// A new capture started (P143).
    pub fn restart_timeline(&mut self) {
        self.link.restart_timeline();
    }

    /// The speaker's password, the one its sessions get too, for a pairing
    /// with an Apple TV that asks for both. AudioSync asks for no AirPlay
    /// password yet, so this is `None` for now.
    fn password(&self, id: &str) -> Option<String> {
        self.options(id).password
    }

    /// The options a speaker joins with.
    fn options(&self, id: &str) -> SpeakerOptions {
        let settings = self.settings(id);
        let mut options = SpeakerOptions::new(airplay_volume(settings.volume, self.master));
        options.delay = speaker_delay(settings.delay_ms);
        options.credentials = self.store.get(id).cloned();
        options
    }

    /// Adds the speakers that should play and removes the ones that
    /// shouldn't: while streaming, every speaker that is on and listed as
    /// able to play joins; one already in the sender stays while it is on,
    /// listed or not, as the sender keeps trying to reach it.
    fn reconcile(&mut self) {
        let leaving: Vec<String> = self
            .added
            .keys()
            .filter(|id| !self.streaming || !self.settings(id).enabled)
            .cloned()
            .collect();
        for id in leaving {
            if let Some(added) = self.added.remove(&id) {
                self.link.remove_speaker(&added.key);
                self.log
                    .log(|| format!("AirPlay speaker {} removed", self.label(&id)));
            }
        }
        if !self.streaming {
            return;
        }
        let joining: Vec<String> = self
            .settings
            .iter()
            .filter(|(id, settings)| {
                settings.enabled && !self.added.contains_key(*id) && self.is_open(id)
            })
            .map(|(id, _)| id.clone())
            .collect();
        for id in joining {
            let options = self.options(&id);
            let device = self.listed[&id].device();
            let key = device.key.clone();
            self.log.log(|| {
                let paired = if options.credentials.is_some() {
                    ", with the stored pairing credentials"
                } else {
                    ""
                };
                format!(
                    "AirPlay speaker {} added: volume {} ({}), delay {} ms{paired}",
                    self.label(&id),
                    self.settings(&id).volume,
                    describe_volume(options.volume),
                    options.delay.as_millis()
                )
            });
            let counters = self.link.add_speaker(device, options);
            self.added.insert(
                id,
                Added {
                    key,
                    state: SpeakerState::Connecting,
                    counters,
                },
            );
        }
    }

    /// Turns a speaker off because it can't play, and says so.
    fn turn_off(&mut self, id: &str, problem: AirPlayProblem, events: &mut Vec<BackendEvent>) {
        if !self.settings(id).enabled {
            return;
        }
        self.entry(id).enabled = false;
        self.reconcile();
        events.push(BackendEvent::AirPlayFailed {
            id: id.to_owned(),
            problem,
        });
    }

    /// Takes in what the sender reported since the last call and returns
    /// what the UI should know.
    pub fn poll(&mut self) -> Vec<BackendEvent> {
        let mut events = Vec::new();
        let mut changed = false;
        while let Some(event) = self.link.poll_event() {
            match event {
                SenderEvent::Receiver(event) => {
                    changed = true;
                    self.receiver_event(event);
                }
                SenderEvent::Speaker { key, event } => {
                    let id = speaker_id(&key);
                    changed |= self.speaker_event(&id, event, &mut events);
                }
                SenderEvent::Timeline {
                    latency,
                    origin,
                    restarted,
                } => {
                    self.log.log(|| {
                        let restarted = if restarted { ", restarted" } else { "" };
                        format!(
                            "AirPlay timeline: L = {} ms, origin {origin}{restarted}",
                            latency.as_millis()
                        )
                    });
                    self.latency = latency;
                }
                SenderEvent::Pairing { key, event } => {
                    let id = speaker_id(&key);
                    changed |= self.pairing_event(&id, event, &mut events);
                }
                // The receivers it listed were reported gone just before;
                // speakers keep playing until Retry replaces the sender.
                SenderEvent::DiscoveryFailed => {
                    self.log.log(|| {
                        "AirPlay: discovery stopped after an internal error; Retry starts a new sender"
                            .to_owned()
                    });
                    if !self.stopped {
                        self.stopped = true;
                        events.push(BackendEvent::AirPlayStopped);
                    }
                }
            }
        }
        // Events it sent before it stopped were taken in above.
        if !self.link.is_running() {
            changed |= self.sender_stopped(&mut events);
        }
        if changed {
            events.insert(0, BackendEvent::AirPlayChanged);
        }
        events
    }

    /// Takes in what a pairing reported. Returns whether the speaker list
    /// changed.
    fn pairing_event(
        &mut self,
        id: &str,
        event: PairingEvent,
        events: &mut Vec<BackendEvent>,
    ) -> bool {
        self.log.log(|| {
            format!(
                "AirPlay pairing {}: {}",
                self.label(id),
                pairing_summary(&event)
            )
        });
        match event {
            PairingEvent::PinRequested => {
                let Some(run) = self.pairings.get_mut(id) else {
                    return false;
                };
                run.phase = PairingPhase::WaitingForPin;
                if !run.shown {
                    run.shown = true;
                    run.shown_order = self.next_shown_order;
                    self.next_shown_order += 1;
                }
                self.log
                    .log(|| format!("AirPlay pairing {}: PIN prompt shown", self.label(id)));
                true
            }
            PairingEvent::Paired(credentials) => {
                let wanted = self.pairings.remove(id).is_some();
                match self.store.insert(id, credentials) {
                    Ok(()) => self.log.log(|| {
                        format!(
                            "AirPlay pairing {}: credentials stored, encrypted for this Windows user",
                            self.label(id)
                        )
                    }),
                    Err(error) => self.log.log(|| {
                        format!(
                            "AirPlay pairing {}: credentials kept until AudioSync quits; the store couldn't be written: {error}",
                            self.label(id)
                        )
                    }),
                }
                // A pairing the user cancelled meanwhile still keeps its
                // credentials, but leaves the speaker off.
                if wanted {
                    self.entry(id).enabled = true;
                    self.reconcile();
                    events.push(BackendEvent::AirPlayPaired { id: id.to_owned() });
                }
                true
            }
            PairingEvent::Failed {
                wrong_pin: true, ..
            } => {
                let Some(run) = self.pairings.get_mut(id) else {
                    return false;
                };
                // The Apple TV shows a new PIN for a new pairing.
                run.phase = PairingPhase::Starting;
                run.wrong_pin = true;
                let device = run.device.clone();
                let password = self.password(id);
                self.link.start_pairing(&device, password);
                self.log.log(|| {
                    format!(
                        "AirPlay pairing {}: wrong PIN, started again for a new PIN",
                        self.label(id)
                    )
                });
                true
            }
            PairingEvent::Failed { .. } => {
                if self.pairings.remove(id).is_none() {
                    return false;
                }
                events.push(BackendEvent::AirPlayFailed {
                    id: id.to_owned(),
                    problem: AirPlayProblem::PairingFailed,
                });
                true
            }
            // A cancel or a shutdown, already taken in.
            PairingEvent::Stopped => false,
        }
    }

    fn receiver_event(&mut self, event: ReceiverEvent) {
        match event {
            ReceiverEvent::Appeared(info) | ReceiverEvent::Changed(info) => {
                let id = speaker_id(&info.device.key);
                let availability = availability(&info, self.store.contains(&id));
                let appeared = !self.listed.contains_key(&id);
                self.names.insert(id.clone(), info.device.name.clone());
                self.log.log(|| {
                    let what = if appeared { "found" } else { "changed" };
                    format!(
                        "AirPlay receiver {what}: {}, {}",
                        self.label(&id),
                        describe_availability(availability)
                    )
                });
                // A speaker the sender is trying to reach again takes the
                // new records for its next session at once (P142).
                if let Some(added) = self.added.get(&id)
                    && matches!(added.state, SpeakerState::Retrying { .. })
                    && availability == AirPlayAvailability::Open
                {
                    let options = self.options(&id);
                    let _ = self.link.add_speaker(&info.device, options);
                }
                self.listed.insert(id.clone(), Listed { info: *info });
                // A speaker that is on stays on whatever discovery says
                // (see the module documentation); one listed as unable to
                // play just doesn't join.
                self.reconcile();
            }
            ReceiverEvent::Gone(key) => {
                let id = speaker_id(&key);
                if self.listed.remove(&id).is_some() {
                    self.log
                        .log(|| format!("AirPlay receiver gone: {}", self.label(&id)));
                }
            }
        }
    }

    /// Returns whether the speaker list changed.
    fn speaker_event(
        &mut self,
        id: &str,
        event: SpeakerEvent,
        events: &mut Vec<BackendEvent>,
    ) -> bool {
        match event {
            SpeakerEvent::State(state) => {
                self.log.log(|| {
                    let state = match state {
                        SpeakerState::Connecting => "connecting".to_owned(),
                        SpeakerState::Playing => "playing".to_owned(),
                        SpeakerState::Retrying { at } => format!(
                            "trying again in {:.1} s",
                            at.saturating_duration_since(Instant::now()).as_secs_f64()
                        ),
                        SpeakerState::Failed => "failed".to_owned(),
                        SpeakerState::Stopped => "stopped".to_owned(),
                    };
                    format!("AirPlay speaker {}: {state}", self.label(id))
                });
                let Some(added) = self.added.get_mut(id) else {
                    return false;
                };
                let was_playing = added.state == SpeakerState::Playing;
                added.state = state;
                if state == SpeakerState::Playing && !was_playing {
                    events.push(BackendEvent::AirPlayPlaying { id: id.to_owned() });
                }
                was_playing != (state == SpeakerState::Playing)
            }
            SpeakerEvent::Error(SpeakerError {
                error,
                reason,
                retry_at,
            }) => {
                let mut problem = problem(reason);
                // The Apple TV turned the stored credentials down (it was
                // reset, or the pairing was removed on it): they are
                // forgotten, and switching it on pairs it again.
                if problem == AirPlayProblem::PairingNotYet && self.store.contains(id) {
                    problem = AirPlayProblem::NeedsPin;
                    let forgotten = self.store.remove(id);
                    self.log.log(|| {
                        let saved = match &forgotten {
                            Ok(_) => String::new(),
                            Err(error) => format!(" (the store couldn't be written: {error})"),
                        };
                        format!(
                            "AirPlay pairing {}: the Apple TV turned the stored credentials down; they are forgotten{saved}",
                            self.label(id)
                        )
                    });
                }
                self.log.log(|| {
                    let next = match retry_at {
                        Some(at) => format!(
                            "trying again in {:.1} s",
                            at.saturating_duration_since(Instant::now()).as_secs_f64()
                        ),
                        None => "it is turned off".to_owned(),
                    };
                    format!(
                        "AirPlay speaker {} error: {error} ({reason:?}); {next}",
                        self.label(id)
                    )
                });
                let Some(added) = self.added.get_mut(id) else {
                    return false;
                };
                if let Some(at) = retry_at
                    && problem.retried()
                {
                    added.state = SpeakerState::Retrying { at };
                    events.push(BackendEvent::AirPlayTrouble {
                        id: id.to_owned(),
                        problem,
                    });
                } else {
                    self.turn_off(id, problem, events);
                }
                true
            }
            SpeakerEvent::Warning(warning) => {
                self.log
                    .log(|| format!("AirPlay speaker {} warning: {warning:?}", self.label(id)));
                false
            }
            SpeakerEvent::Latency(latency) => {
                self.log.log(|| {
                    format!(
                        "AirPlay speaker {} latency: L + d_s = {} ms, receiver reports {}, first sound in {} ms",
                        self.label(id),
                        latency.arrival_to_sound.as_millis(),
                        latency
                            .receiver_latency_frames
                            .map_or("nothing".to_owned(), |frames| format!("{frames} frames")),
                        latency
                            .first_sound
                            .saturating_duration_since(Instant::now())
                            .as_millis()
                    )
                });
                false
            }
            SpeakerEvent::Server(server) => {
                self.log
                    .log(|| format!("AirPlay speaker {} server: {server}", self.label(id)));
                false
            }
            SpeakerEvent::Methods(_)
            | SpeakerEvent::AudioJack(_)
            | SpeakerEvent::VolumeReported(_) => false,
        }
    }

    /// The status lines for the debug log: the stream's counters and each
    /// speaker's.
    pub fn status(&self) -> Option<String> {
        if self.added.is_empty() {
            return None;
        }
        let mut text = format!("AirPlay: L = {} ms", self.latency.as_millis());
        if let Some(stream) = self.link.stream_counters() {
            text.push_str(&format!(
                ", input rate {:.1} Hz, {} packets encoded, {} silent frames, {} input frames dropped",
                stream.rate(),
                stream.packets_encoded(),
                stream.silent_frames(),
                stream.input_frames_dropped(),
            ));
        }
        for (id, added) in &self.added {
            text.push_str(&format!(
                "\n  AirPlay speaker {}: {:?}",
                self.label(id),
                added.state
            ));
            if let Some(counters) = &added.counters {
                text.push_str(&format!(
                    "; {} packets sent, {} late, {} resend requests, {} resent, {} timing replies",
                    counters.packets_sent(),
                    counters.late_packets(),
                    counters.resend_requests(),
                    counters.packets_resent(),
                    counters.timing_replies(),
                ));
            }
        }
        Some(text)
    }
}

impl Drop for AirPlaySpeakers {
    fn drop(&mut self) {
        if !self.added.is_empty() {
            self.log
                .log(|| "AirPlay: the sender stops with its speakers".to_owned());
        }
    }
}

#[cfg(test)]
pub mod tests {
    use std::cell::RefCell;
    use std::collections::VecDeque;
    use std::io::{Read, Write};
    use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener, TcpStream, UdpSocket};
    use std::rc::Rc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::thread::{self, JoinHandle};

    use audiosync_airplay::clock::SenderClock;
    use audiosync_airplay::discovery::{BrowseEvent, Browser, DeviceId, ScopedAddr, ServiceRecord};
    use audiosync_airplay::error::SessionError;
    use audiosync_airplay::rtsp::{Message, Method, Parser, Request, Response};
    use audiosync_airplay::stream::{Stream, StreamConfig};

    use super::*;
    use crate::audio::backend::{AudioBackend, SourceId};
    use crate::audio::mock::MockBackend;

    #[test]
    fn pairing_events_are_logged_without_secrets() {
        use audiosync_airplay::pairing::Credentials;

        let secret = [0xA7u8; 32];
        let credentials = Credentials::new("0123456789ABCDEF", secret).expect("credentials");
        let paired = pairing_summary(&PairingEvent::Paired(credentials));
        assert_eq!(paired, "paired as client 0123456789ABCDEF");
        assert!(!paired.to_ascii_lowercase().contains("a7a7"));
        let mut error = SessionError::new(ErrorKind::PairingFailed);
        error.status = Some(470);
        let failed = pairing_summary(&PairingEvent::Failed {
            error,
            wrong_pin: true,
        });
        assert!(
            failed.contains("PairingFailed") && failed.contains("wrong PIN"),
            "{failed}"
        );
        assert_eq!(
            pairing_summary(&PairingEvent::PinRequested),
            "the Apple TV shows a PIN"
        );
    }

    /// The TXT strings of a receiver this sender can play: AirPlay 1, no
    /// password, unencrypted, PCM or ALAC over UDP.
    pub const OPEN_TXT: &[&str] = &[
        "txtvers=1",
        "ch=2",
        "cn=0,1",
        "et=0",
        "sr=44100",
        "ss=16",
        "tp=UDP",
        "md=0,1,2",
    ];

    /// The TXT strings of an Apple TV that asks for a PIN once (the legacy
    /// pairing, P46): its model, and status flag bit 3.
    pub const APPLE_TV_TXT: &[&str] = &[
        "txtvers=1",
        "ch=2",
        "cn=0,1",
        "et=0,3,5",
        "sr=44100",
        "ss=16",
        "tp=UDP",
        "md=0,1,2",
        "am=AppleTV3,2",
        "sf=0x8",
    ];

    /// A device whose RAOP service answers at 192.0.2.10 (a documentation
    /// address), port `port`.
    pub fn device(id: u8, name: &str, txt: &[&str]) -> Device {
        device_at(
            id,
            name,
            txt,
            IpAddr::V4(Ipv4Addr::new(192, 0, 2, 10)),
            7000,
        )
    }

    /// A device whose RAOP service answers at `ip` and `port`.
    pub fn device_at(id: u8, name: &str, txt: &[&str], ip: IpAddr, port: u16) -> Device {
        let device_id = DeviceId([0x02, 0x00, 0x5E, 0x00, 0x53, id]);
        let record = ServiceRecord {
            kind: ServiceKind::Raop,
            instance: format!("{device_id}@{name}"),
            host: format!("speaker-{id}.local"),
            port,
            txt: txt.iter().map(|s| s.as_bytes().to_vec()).collect(),
            addresses: vec![ScopedAddr::new(ip)],
        };
        Device::from_records(DeviceKey::Id(device_id), Some(&record), None).expect("a device")
    }

    /// The ID the backend gives `device`.
    pub fn id_of(device: &Device) -> String {
        speaker_id(&device.key)
    }

    /// A call the fake link received.
    #[derive(Clone, Debug, PartialEq)]
    pub enum Call {
        Add {
            id: String,
            volume: Volume,
            delay: Duration,
        },
        Remove(String),
        Volume(String, Volume),
        Delay(String, Duration),
        RestartTimeline,
        /// The speaker added just before came with credentials of this
        /// client ID.
        Credentials(String, String),
        StartPairing(String),
        SubmitPin(String, String),
        CancelPairing(String),
    }

    #[derive(Default)]
    struct FakeState {
        calls: Vec<Call>,
        events: VecDeque<SenderEvent>,
        latency: Duration,
        /// The sender stopped after an internal error.
        stopped: bool,
    }

    /// A fake sender: records every call, and hands out the events a test
    /// queues.
    #[derive(Clone)]
    pub struct FakeAirPlay(Rc<RefCell<FakeState>>);

    impl FakeAirPlay {
        /// A fake with the sender's default L.
        pub fn new() -> FakeAirPlay {
            FakeAirPlay(Rc::new(RefCell::new(FakeState {
                latency: Duration::from_millis(1750),
                ..FakeState::default()
            })))
        }

        /// A link for the backend.
        pub fn link(&self) -> Box<dyn AirPlayLink> {
            Box::new(self.clone())
        }

        /// Queues an event for the backend's next poll.
        pub fn push(&self, event: SenderEvent) {
            self.0.borrow_mut().events.push_back(event);
        }

        /// Discovery found `device`.
        pub fn appear(&self, device: &Device) {
            self.push(SenderEvent::Receiver(ReceiverEvent::Appeared(Box::new(
                ReceiverInfo::new(device.clone()),
            ))));
        }

        /// Discovery saw `device`'s records change.
        pub fn change(&self, device: &Device) {
            self.push(SenderEvent::Receiver(ReceiverEvent::Changed(Box::new(
                ReceiverInfo::new(device.clone()),
            ))));
        }

        /// Discovery lost `device`.
        pub fn gone(&self, device: &Device) {
            self.push(SenderEvent::Receiver(ReceiverEvent::Gone(
                device.key.clone(),
            )));
        }

        /// A speaker's state changed.
        pub fn state(&self, device: &Device, state: SpeakerState) {
            self.push(SenderEvent::Speaker {
                key: device.key.clone(),
                event: SpeakerEvent::State(state),
            });
        }

        /// A speaker's session ended with `kind`; the sender tries again
        /// unless the kind needs the user.
        pub fn fail(&self, device: &Device, kind: ErrorKind) {
            let retry_at = (!kind.needs_user()).then(|| Instant::now() + Duration::from_secs(1));
            self.push(SenderEvent::Speaker {
                key: device.key.clone(),
                event: SpeakerEvent::Error(SpeakerError {
                    error: SessionError::new(kind),
                    reason: kind.user_reason(),
                    retry_at,
                }),
            });
        }

        /// The timeline restarted with a new L.
        pub fn set_latency(&self, latency: Duration) {
            self.0.borrow_mut().latency = latency;
            self.push(SenderEvent::Timeline {
                latency,
                origin: 0,
                restarted: false,
            });
        }

        /// A pairing with `device` reported `event`.
        pub fn pairing(&self, device: &Device, event: PairingEvent) {
            self.push(SenderEvent::Pairing {
                key: device.key.clone(),
                event,
            });
        }

        /// The sender stops by itself, as after a panic in its driver.
        pub fn stop_running(&self) {
            self.0.borrow_mut().stopped = true;
        }

        /// The calls so far, and forgets them.
        pub fn take_calls(&self) -> Vec<Call> {
            std::mem::take(&mut self.0.borrow_mut().calls)
        }
    }

    impl AirPlayLink for FakeAirPlay {
        fn add_speaker(
            &mut self,
            device: &Device,
            options: SpeakerOptions,
        ) -> Option<Arc<SpeakerCounters>> {
            let mut state = self.0.borrow_mut();
            state.calls.push(Call::Add {
                id: id_of(device),
                volume: options.volume,
                delay: options.delay,
            });
            if let Some(credentials) = &options.credentials {
                state.calls.push(Call::Credentials(
                    id_of(device),
                    credentials.client_id().to_owned(),
                ));
            }
            None
        }

        fn remove_speaker(&mut self, key: &DeviceKey) {
            self.0
                .borrow_mut()
                .calls
                .push(Call::Remove(speaker_id(key)));
        }

        fn set_volume(&mut self, key: &DeviceKey, volume: Volume) {
            self.0
                .borrow_mut()
                .calls
                .push(Call::Volume(speaker_id(key), volume));
        }

        fn set_delay(&mut self, key: &DeviceKey, delay: Duration) {
            self.0
                .borrow_mut()
                .calls
                .push(Call::Delay(speaker_id(key), delay));
        }

        fn restart_timeline(&mut self) {
            self.0.borrow_mut().calls.push(Call::RestartTimeline);
        }

        fn latency(&self) -> Duration {
            self.0.borrow().latency
        }

        fn stream_counters(&self) -> Option<Arc<StreamCounters>> {
            None
        }

        fn poll_event(&mut self) -> Option<SenderEvent> {
            self.0.borrow_mut().events.pop_front()
        }

        fn start_pairing(&mut self, device: &Device, password: Option<String>) {
            assert_eq!(password, None, "AudioSync has no AirPlay passwords yet");
            self.0
                .borrow_mut()
                .calls
                .push(Call::StartPairing(id_of(device)));
        }

        fn submit_pin(&mut self, key: &DeviceKey, pin: &str) -> Result<(), PinError> {
            audiosync_airplay::pairing::check_pin(pin)?;
            self.0
                .borrow_mut()
                .calls
                .push(Call::SubmitPin(speaker_id(key), pin.to_owned()));
            Ok(())
        }

        fn cancel_pairing(&mut self, key: &DeviceKey) {
            self.0
                .borrow_mut()
                .calls
                .push(Call::CancelPairing(speaker_id(key)));
        }

        fn is_running(&self) -> bool {
            !self.0.borrow().stopped
        }
    }

    fn speakers(fake: &FakeAirPlay) -> AirPlaySpeakers {
        AirPlaySpeakers::new(
            fake.link(),
            100.0,
            DebugLog::default(),
            PairingStore::in_memory(),
        )
    }

    const PASSWORD_TXT: &[&str] = &[
        "txtvers=1",
        "ch=2",
        "cn=0,1",
        "et=0",
        "sr=44100",
        "ss=16",
        "tp=UDP",
        "pw=true",
    ];

    #[test]
    fn a_receiver_that_appears_is_listed_with_whether_it_can_play() {
        let fake = FakeAirPlay::new();
        let mut airplay = speakers(&fake);
        let kitchen = device(1, "Kitchen", OPEN_TXT);
        let study = device(2, "Study", PASSWORD_TXT);
        fake.appear(&study);
        fake.appear(&kitchen);
        assert_eq!(airplay.poll(), [BackendEvent::AirPlayChanged]);
        let listed = airplay.speakers();
        let rows: Vec<(&str, AirPlayAvailability, bool)> = listed
            .iter()
            .map(|s| (s.name.as_str(), s.availability, s.enabled))
            .collect();
        assert_eq!(
            rows,
            [
                ("Kitchen", AirPlayAvailability::Open, false),
                ("Study", AirPlayAvailability::PasswordNotYet, false),
            ]
        );
        assert_eq!(listed[0].id, "02005E005301");
        fake.gone(&kitchen);
        airplay.poll();
        assert_eq!(airplay.speakers().len(), 1);
    }

    /// A failed pairing, wrong PIN or not.
    fn pairing_failed(kind: ErrorKind, wrong_pin: bool) -> PairingEvent {
        PairingEvent::Failed {
            error: SessionError::new(kind),
            wrong_pin,
        }
    }

    /// An Apple TV listed, switched on by the user, with its PIN shown.
    fn pin_shown(fake: &FakeAirPlay, airplay: &mut AirPlaySpeakers, tv: &Device) {
        fake.appear(tv);
        airplay.poll();
        let id = id_of(tv);
        assert_eq!(
            airplay.set_enabled(&id, true),
            Err(AirPlayProblem::NeedsPin)
        );
        airplay.start_pairing(&id).expect("the pairing starts");
        assert_eq!(fake.take_calls(), [Call::StartPairing(id.clone())]);
        assert!(airplay.speakers()[0].pairing);
        assert_eq!(airplay.pin_prompt(), None, "no prompt before the PIN shows");
        fake.pairing(tv, PairingEvent::PinRequested);
        assert_eq!(airplay.poll(), [BackendEvent::AirPlayChanged]);
        assert_eq!(
            airplay.pin_prompt(),
            Some(AirPlayPinPrompt {
                id,
                name: tv.name.clone(),
                phase: PairingPhase::WaitingForPin,
                wrong_pin: false,
            })
        );
    }

    #[test]
    fn an_apple_tv_pairs_once_and_its_credentials_come_back_on_every_add() {
        use crate::audio::pairings::tests::{credentials, sealer};
        use crate::audio::pairings::{FILE_NAME, Loaded};

        let path = crate::debug_log::test_dir("airplay-pairing-reused").join(FILE_NAME);
        let fake = FakeAirPlay::new();
        let (store, _) = PairingStore::open(path.clone(), sealer());
        let mut airplay = AirPlaySpeakers::new(fake.link(), 100.0, DebugLog::default(), store);
        let tv = device(6, "Den TV", APPLE_TV_TXT);
        let id = id_of(&tv);
        pin_shown(&fake, &mut airplay, &tv);
        assert_eq!(
            airplay.speakers()[0].availability,
            AirPlayAvailability::NeedsPin
        );
        assert!(!airplay.submit_pin(&id, "12a4"), "digits only");
        assert!(airplay.submit_pin(&id, "4821"));
        assert_eq!(
            fake.take_calls(),
            [Call::SubmitPin(id.clone(), "4821".to_owned())]
        );
        assert_eq!(airplay.pin_prompt().unwrap().phase, PairingPhase::Checking);

        fake.pairing(
            &tv,
            PairingEvent::Paired(credentials("0123456789ABCDEF", 0xA5)),
        );
        assert_eq!(
            airplay.poll(),
            [
                BackendEvent::AirPlayChanged,
                BackendEvent::AirPlayPaired { id: id.clone() }
            ]
        );
        assert_eq!(airplay.pin_prompt(), None);
        assert_eq!(
            airplay.enabled_ids(),
            std::slice::from_ref(&id),
            "paired, it is on"
        );
        let row = &airplay.speakers()[0];
        assert!(row.paired && !row.pairing);
        assert_eq!(row.availability, AirPlayAvailability::Open);
        airplay.set_streaming(true);
        let calls = fake.take_calls();
        assert!(matches!(
            &calls[..],
            [Call::Add { .. }, Call::Credentials(..)]
        ));
        assert_eq!(
            calls[1],
            Call::Credentials(id.clone(), "0123456789ABCDEF".to_owned())
        );
        drop(airplay);

        // The next launch: no PIN, the stored credentials go with the add.
        let fake = FakeAirPlay::new();
        let (store, loaded) = PairingStore::open(path, sealer());
        assert_eq!(loaded, Loaded::Speakers(1));
        let mut airplay = AirPlaySpeakers::new(fake.link(), 100.0, DebugLog::default(), store);
        fake.appear(&tv);
        airplay.poll();
        assert_eq!(airplay.set_enabled(&id, true), Ok(()));
        airplay.set_streaming(true);
        assert_eq!(
            fake.take_calls()[1..],
            [Call::Credentials(id, "0123456789ABCDEF".to_owned())]
        );
    }

    #[test]
    fn a_wrong_pin_starts_a_new_pairing_and_keeps_the_prompt() {
        let fake = FakeAirPlay::new();
        let mut airplay = speakers(&fake);
        let tv = device(6, "Den TV", APPLE_TV_TXT);
        let id = id_of(&tv);
        pin_shown(&fake, &mut airplay, &tv);
        assert!(airplay.submit_pin(&id, "1111"));
        fake.take_calls();
        fake.pairing(&tv, pairing_failed(ErrorKind::PairingFailed, true));
        assert_eq!(airplay.poll(), [BackendEvent::AirPlayChanged]);
        assert_eq!(
            fake.take_calls(),
            [Call::StartPairing(id.clone())],
            "a new PIN needs a new pairing"
        );
        let prompt = airplay.pin_prompt().expect("the prompt stays");
        assert_eq!(prompt.phase, PairingPhase::Starting);
        assert!(prompt.wrong_pin);
        assert!(!airplay.submit_pin(&id, "2222"), "no PIN asked for yet");
        fake.pairing(&tv, PairingEvent::PinRequested);
        airplay.poll();
        let prompt = airplay.pin_prompt().unwrap();
        assert_eq!(prompt.phase, PairingPhase::WaitingForPin);
        assert!(prompt.wrong_pin, "the message stays until the next try");
        assert!(airplay.submit_pin(&id, "2222"));
        assert!(!airplay.pin_prompt().unwrap().wrong_pin);
        assert!(airplay.enabled_ids().is_empty(), "off until paired");
    }

    #[test]
    fn the_prompt_stays_with_the_apple_tv_that_asked_first() {
        let fake = FakeAirPlay::new();
        let mut airplay = speakers(&fake);
        // The first to ask sorts after the second, so ID order alone would
        // move the prompt.
        let first = device(7, "Living Room TV", APPLE_TV_TXT);
        let second = device(6, "Den TV", APPLE_TV_TXT);
        pin_shown(&fake, &mut airplay, &first);
        fake.appear(&second);
        airplay.poll();
        let second_id = id_of(&second);
        assert_eq!(
            airplay.set_enabled(&second_id, true),
            Err(AirPlayProblem::NeedsPin)
        );
        airplay
            .start_pairing(&second_id)
            .expect("the second pairing starts");
        fake.pairing(&second, PairingEvent::PinRequested);
        airplay.poll();
        assert_eq!(
            airplay.pin_prompt().map(|prompt| prompt.id),
            Some(id_of(&first)),
            "the field keeps belonging to the first Apple TV"
        );
        airplay.cancel_pairing(&id_of(&first));
        assert_eq!(
            airplay.pin_prompt().map(|prompt| prompt.id),
            Some(second_id),
            "then the second one gets the prompt"
        );
    }

    #[test]
    fn cancel_stops_the_pairing_and_leaves_the_speaker_off() {
        let fake = FakeAirPlay::new();
        let mut airplay = speakers(&fake);
        let tv = device(6, "Den TV", APPLE_TV_TXT);
        let id = id_of(&tv);
        pin_shown(&fake, &mut airplay, &tv);
        airplay.cancel_pairing(&id);
        assert_eq!(fake.take_calls(), [Call::CancelPairing(id.clone())]);
        assert_eq!(airplay.pin_prompt(), None);
        fake.pairing(&tv, PairingEvent::Stopped);
        assert!(airplay.poll().is_empty(), "no notice for a cancel");
        assert!(airplay.enabled_ids().is_empty());
        assert!(!airplay.speakers()[0].pairing);

        // Switching it off while it pairs cancels too.
        airplay.start_pairing(&id).unwrap();
        fake.take_calls();
        airplay.set_enabled(&id, false).unwrap();
        assert_eq!(fake.take_calls(), [Call::CancelPairing(id.clone())]);
        // A pairing that still succeeds keeps its credentials, but the
        // speaker stays off.
        fake.pairing(
            &tv,
            PairingEvent::Paired(crate::audio::pairings::tests::credentials(
                "0123456789ABCDEF",
                1,
            )),
        );
        assert_eq!(airplay.poll(), [BackendEvent::AirPlayChanged]);
        assert!(airplay.enabled_ids().is_empty());
        assert!(airplay.speakers()[0].paired);
    }

    #[test]
    fn a_failed_pairing_turns_the_speaker_off_with_a_notice() {
        let fake = FakeAirPlay::new();
        let mut airplay = speakers(&fake);
        let tv = device(6, "Den TV", APPLE_TV_TXT);
        let id = id_of(&tv);
        pin_shown(&fake, &mut airplay, &tv);
        fake.pairing(&tv, pairing_failed(ErrorKind::PairingCancelled, false));
        assert_eq!(
            airplay.poll(),
            [
                BackendEvent::AirPlayChanged,
                BackendEvent::AirPlayFailed {
                    id,
                    problem: AirPlayProblem::PairingFailed
                }
            ]
        );
        assert_eq!(airplay.pin_prompt(), None);
        assert!(airplay.enabled_ids().is_empty());
        assert_eq!(
            airplay.speakers()[0].availability,
            AirPlayAvailability::NeedsPin
        );
    }

    #[test]
    fn forgetting_a_pairing_turns_the_speaker_off_and_asks_for_a_pin_again() {
        let fake = FakeAirPlay::new();
        let mut store = PairingStore::in_memory();
        let tv = device(6, "Den TV", APPLE_TV_TXT);
        let id = id_of(&tv);
        store
            .insert(
                &id,
                crate::audio::pairings::tests::credentials("0123456789ABCDEF", 2),
            )
            .unwrap();
        let mut airplay = AirPlaySpeakers::new(fake.link(), 100.0, DebugLog::default(), store);
        fake.appear(&tv);
        airplay.poll();
        airplay.set_enabled(&id, true).unwrap();
        airplay.set_streaming(true);
        fake.take_calls();
        airplay.forget_pairing(&id);
        assert_eq!(fake.take_calls(), [Call::Remove(id.clone())]);
        assert!(airplay.enabled_ids().is_empty());
        let row = &airplay.speakers()[0];
        assert!(!row.paired);
        assert_eq!(row.availability, AirPlayAvailability::NeedsPin);
    }

    #[test]
    fn credentials_the_apple_tv_turns_down_are_forgotten() {
        let fake = FakeAirPlay::new();
        let mut store = PairingStore::in_memory();
        let tv = device(6, "Den TV", APPLE_TV_TXT);
        let id = id_of(&tv);
        store
            .insert(
                &id,
                crate::audio::pairings::tests::credentials("0123456789ABCDEF", 3),
            )
            .unwrap();
        let mut airplay = AirPlaySpeakers::new(fake.link(), 100.0, DebugLog::default(), store);
        fake.appear(&tv);
        airplay.poll();
        airplay.set_enabled(&id, true).unwrap();
        airplay.set_streaming(true);
        fake.fail(&tv, ErrorKind::PairingRequired);
        let events = airplay.poll();
        assert!(events.contains(&BackendEvent::AirPlayFailed {
            id: id.clone(),
            problem: AirPlayProblem::NeedsPin
        }));
        assert!(!airplay.speakers()[0].paired);
        assert!(airplay.enabled_ids().is_empty());
    }

    #[test]
    fn the_log_tells_the_pairing_story_without_the_pin_or_the_secret() {
        let path = crate::debug_log::test_dir("airplay-pairing-log").join("debug.log");
        let log = DebugLog::new(Some(path.clone()));
        let fake = FakeAirPlay::new();
        let mut airplay =
            AirPlaySpeakers::new(fake.link(), 100.0, log.clone(), PairingStore::in_memory());
        let tv = device(6, "Den TV", APPLE_TV_TXT);
        let id = id_of(&tv);
        pin_shown(&fake, &mut airplay, &tv);
        airplay.submit_pin(&id, "4821");
        fake.pairing(&tv, pairing_failed(ErrorKind::PairingFailed, true));
        airplay.poll();
        fake.pairing(&tv, PairingEvent::PinRequested);
        airplay.poll();
        airplay.submit_pin(&id, "7395");
        fake.pairing(
            &tv,
            PairingEvent::Paired(crate::audio::pairings::tests::credentials(
                "0123456789ABCDEF",
                0xA5,
            )),
        );
        airplay.poll();
        airplay.forget_pairing(&id);
        airplay.start_pairing(&id).unwrap();
        airplay.cancel_pairing(&id);
        assert!(log.flush(Duration::from_secs(5)));
        let messages = crate::debug_log::read_messages(&path);
        crate::debug_log::assert_in_order(
            &messages,
            &[
                "pairing \"Den TV\" (02005E005306): started, asking the Apple TV to show a PIN",
                "PIN prompt shown",
                "PIN sent",
                "wrong PIN",
                "started again for a new PIN",
                "PIN sent",
                "paired as client 0123456789ABCDEF",
                "credentials forgotten",
                "started, asking",
                "stopped, cancelled by the user",
            ],
        );
        let text = messages.join("\n");
        for secret in ["4821", "7395", "a5a5", "A5A5", "165, 165", "[165"] {
            assert!(!text.contains(secret), "{secret:?} is in the log:\n{text}");
        }
    }

    #[test]
    fn receivers_that_need_pairing_or_airplay_2_are_not_supported_yet() {
        let pin = device(
            3,
            "TV",
            &["txtvers=1", "cn=0,1", "et=0", "tp=UDP", "sf=0x8"],
        );
        assert_eq!(
            availability(&ReceiverInfo::new(pin), false),
            AirPlayAvailability::PairingNotYet
        );
        let rsa = device(4, "Old", &["txtvers=1", "cn=0,1", "et=1", "tp=UDP"]);
        assert_eq!(
            availability(&ReceiverInfo::new(rsa), false),
            AirPlayAvailability::Unsupported
        );
        // An Apple TV that asks for a PIN can play once paired.
        let tv = device(6, "Den TV", APPLE_TV_TXT);
        assert_eq!(
            availability(&ReceiverInfo::new(tv.clone()), false),
            AirPlayAvailability::NeedsPin
        );
        assert_eq!(
            availability(&ReceiverInfo::new(tv), true),
            AirPlayAvailability::Open
        );
        // AirPlay 2 only: no RAOP service, features with an AirPlay 2 bit.
        let record = ServiceRecord {
            kind: ServiceKind::AirPlay,
            instance: "Living Room".to_owned(),
            host: "living.local".to_owned(),
            port: 7000,
            txt: vec![
                b"deviceid=02:00:5E:00:53:05".to_vec(),
                b"features=0x0,0x40".to_vec(),
            ],
            addresses: vec![ScopedAddr::new(IpAddr::V4(Ipv4Addr::new(192, 0, 2, 11)))],
        };
        let key = record.device_key();
        let living = Device::from_records(key, None, Some(&record)).expect("a device");
        assert_eq!(
            availability(&ReceiverInfo::new(living), false),
            AirPlayAvailability::AirPlay2NotYet
        );
    }

    #[test]
    fn a_speaker_joins_the_stream_while_on_and_streaming() {
        let fake = FakeAirPlay::new();
        let mut airplay = speakers(&fake);
        let kitchen = device(1, "Kitchen", OPEN_TXT);
        fake.appear(&kitchen);
        airplay.poll();
        let id = id_of(&kitchen);
        airplay.set_enabled(&id, true).unwrap();
        assert!(fake.take_calls().is_empty(), "nothing streams yet");
        assert!(airplay.chosen(), "on and listed: chosen (S79)");
        airplay.set_streaming(true);
        assert_eq!(
            fake.take_calls(),
            [Call::Add {
                id: id.clone(),
                volume: Volume::Db(-12.0),
                delay: Duration::ZERO,
            }]
        );
        airplay.set_streaming(false);
        assert_eq!(fake.take_calls(), [Call::Remove(id.clone())]);
        airplay.set_streaming(true);
        airplay.set_enabled(&id, false).unwrap();
        assert_eq!(fake.take_calls()[1], Call::Remove(id));
        assert!(!airplay.chosen());
    }

    #[test]
    fn a_speaker_that_cant_play_stays_off_and_is_never_added() {
        let fake = FakeAirPlay::new();
        let mut airplay = speakers(&fake);
        let study = device(2, "Study", PASSWORD_TXT);
        fake.appear(&study);
        airplay.poll();
        airplay.set_streaming(true);
        assert_eq!(
            airplay.set_enabled(&id_of(&study), true),
            Err(AirPlayProblem::PasswordNotYet)
        );
        assert!(airplay.enabled_ids().is_empty());
        assert!(fake.take_calls().is_empty());
    }

    #[test]
    fn volume_and_delay_go_to_the_sender_live() {
        let fake = FakeAirPlay::new();
        let mut airplay = speakers(&fake);
        let kitchen = device(1, "Kitchen", OPEN_TXT);
        let id = id_of(&kitchen);
        fake.appear(&kitchen);
        airplay.poll();
        airplay.set_enabled(&id, true).unwrap();
        airplay.set_streaming(true);
        fake.take_calls();
        airplay.set_volume(&id, 50.0);
        airplay.set_master(50.0);
        airplay.set_delay(&id, 120.0);
        airplay.set_delay(&id, 9000.0);
        airplay.set_volume(&id, 0.0);
        assert_eq!(
            fake.take_calls(),
            [
                Call::Volume(id.clone(), Volume::Db(-15.0)),
                Call::Volume(id.clone(), Volume::Db(-22.5)),
                Call::Delay(id.clone(), Duration::from_millis(120)),
                Call::Delay(id.clone(), Duration::from_millis(3000)),
                Call::Volume(id.clone(), Volume::Muted),
            ]
        );
        assert_eq!(airplay.speakers()[0].delay_ms, 3000.0);
    }

    #[test]
    fn a_lost_speaker_stays_on_and_one_that_needs_the_user_is_turned_off() {
        let fake = FakeAirPlay::new();
        let mut airplay = speakers(&fake);
        let kitchen = device(1, "Kitchen", OPEN_TXT);
        let id = id_of(&kitchen);
        fake.appear(&kitchen);
        airplay.poll();
        airplay.set_enabled(&id, true).unwrap();
        airplay.set_streaming(true);
        fake.state(&kitchen, SpeakerState::Playing);
        assert_eq!(
            airplay.poll(),
            [
                BackendEvent::AirPlayChanged,
                BackendEvent::AirPlayPlaying { id: id.clone() }
            ]
        );
        fake.fail(&kitchen, ErrorKind::Lost);
        assert_eq!(
            airplay.poll(),
            [
                BackendEvent::AirPlayChanged,
                BackendEvent::AirPlayTrouble {
                    id: id.clone(),
                    problem: AirPlayProblem::ConnectionLost
                }
            ]
        );
        assert_eq!(
            airplay.enabled_ids(),
            std::slice::from_ref(&id),
            "it stays on"
        );
        fake.take_calls();
        fake.fail(&kitchen, ErrorKind::PasswordRequired);
        assert_eq!(
            airplay.poll(),
            [
                BackendEvent::AirPlayChanged,
                BackendEvent::AirPlayFailed {
                    id: id.clone(),
                    problem: AirPlayProblem::PasswordNotYet
                }
            ]
        );
        assert!(airplay.enabled_ids().is_empty(), "turned off");
        assert_eq!(fake.take_calls(), [Call::Remove(id)]);
    }

    #[test]
    fn a_speaker_counts_as_chosen_while_listed_or_playing() {
        let fake = FakeAirPlay::new();
        let mut airplay = speakers(&fake);
        let kitchen = device(1, "Kitchen", OPEN_TXT);
        let id = id_of(&kitchen);
        fake.appear(&kitchen);
        airplay.poll();
        airplay.set_enabled(&id, true).unwrap();
        airplay.set_streaming(true);
        fake.state(&kitchen, SpeakerState::Playing);
        fake.gone(&kitchen);
        airplay.poll();
        assert!(airplay.chosen(), "its announcement lapsed, but it plays");
        assert_eq!(airplay.speakers().len(), 1, "and it stays listed");
        fake.state(
            &kitchen,
            SpeakerState::Retrying {
                at: Instant::now() + Duration::from_secs(1),
            },
        );
        airplay.poll();
        assert!(!airplay.chosen(), "gone and not playing");
        assert_eq!(airplay.enabled_ids(), [id], "still on");
    }

    /// Den, a speaker with both services, or with its AirPlay service alone.
    fn den(with_raop: bool) -> Device {
        let address = ScopedAddr::new(IpAddr::V4(Ipv4Addr::new(192, 0, 2, 12)));
        let raop = ServiceRecord {
            kind: ServiceKind::Raop,
            instance: "02005E005307@Den".to_owned(),
            host: "den.local".to_owned(),
            port: 7000,
            txt: OPEN_TXT.iter().map(|s| s.as_bytes().to_vec()).collect(),
            addresses: vec![address],
        };
        let airplay = ServiceRecord {
            kind: ServiceKind::AirPlay,
            instance: "Den".to_owned(),
            host: "den.local".to_owned(),
            port: 7000,
            txt: vec![
                b"deviceid=02:00:5E:00:53:07".to_vec(),
                b"features=0x0,0x40".to_vec(),
            ],
            addresses: vec![address],
        };
        let key = DeviceKey::Id(DeviceId([0x02, 0x00, 0x5E, 0x00, 0x53, 0x07]));
        Device::from_records(key, with_raop.then_some(&raop), Some(&airplay)).expect("a device")
    }

    #[test]
    fn a_two_service_speaker_that_loses_its_raop_record_first_stays_on() {
        let fake = FakeAirPlay::new();
        let mut airplay = speakers(&fake);
        let both = den(true);
        let id = id_of(&both);
        fake.appear(&both);
        airplay.poll();
        assert_eq!(
            airplay.speakers()[0].availability,
            AirPlayAvailability::Open
        );
        airplay.set_enabled(&id, true).unwrap();
        airplay.set_streaming(true);
        fake.state(&both, SpeakerState::Playing);
        airplay.poll();
        fake.take_calls();
        // Its RAOP record lapses first: "AirPlay 2 only" for a moment, then
        // gone (EC12).
        fake.change(&den(false));
        fake.gone(&both);
        let events = airplay.poll();
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, BackendEvent::AirPlayFailed { .. })),
            "no notice: {events:?}"
        );
        assert_eq!(
            airplay.enabled_ids(),
            std::slice::from_ref(&id),
            "it stays on"
        );
        assert!(fake.take_calls().is_empty(), "its session goes on");
        assert!(airplay.chosen(), "it still plays");
        // Back with both records, it is listed as able to play again.
        fake.appear(&both);
        airplay.poll();
        assert_eq!(
            airplay.speakers()[0].availability,
            AirPlayAvailability::Open
        );
        assert_eq!(airplay.enabled_ids(), [id]);
    }

    #[test]
    fn a_listed_speaker_that_cant_play_doesnt_hold_local_outputs_back() {
        let fake = FakeAirPlay::new();
        let mut airplay = speakers(&fake);
        let both = den(true);
        let id = id_of(&both);
        fake.appear(&both);
        airplay.poll();
        airplay.set_enabled(&id, true).unwrap();
        // Not streaming, so not playing: listed as AirPlay 2 only, it can't
        // join, so it isn't chosen.
        fake.change(&den(false));
        airplay.poll();
        assert_eq!(airplay.enabled_ids(), std::slice::from_ref(&id));
        assert!(!airplay.chosen());
        airplay.set_streaming(true);
        assert!(fake.take_calls().is_empty(), "it doesn't join");
    }

    #[test]
    fn the_slider_spreads_over_30_db() {
        assert_eq!(airplay_volume(100.0, 100.0), Volume::Db(0.0));
        assert_eq!(airplay_volume(50.0, 100.0), Volume::Db(-15.0));
        assert_eq!(airplay_volume(60.0, 100.0), Volume::Db(-12.0));
        assert_eq!(airplay_volume(100.0, 0.0), Volume::Muted);
        assert_eq!(airplay_volume(0.0, 100.0), Volume::Muted);
    }

    #[test]
    fn the_feed_hands_44_1_khz_audio_to_the_sender() {
        let clock = SenderClock::new(Instant::now());
        let (_stream, input) = Stream::new(StreamConfig::default(), clock, Instant::now());
        let slot = FeedSlot::new(input);
        let mut lease = slot.lease().expect("the feed is free");
        assert!(slot.lease().is_none(), "one capture at a time");
        // One second at the session rate, in blocks like a capture's.
        let block = vec![0.25_f32; 480 * 2];
        let at = Instant::now();
        for _ in 0..100 {
            lease.push(&block, 480, at);
        }
        let position = lease.position();
        // The resampler holds back a few frames of history.
        assert!((44_000..=44_100).contains(&position), "{position}");
        drop(lease);
        assert!(slot.is_free(), "the feed is back when the capture ends");
    }

    /// A receiver on 127.0.0.1 that answers every RTSP request with
    /// success, as an open AirPlay 1 speaker does, and notes the methods it
    /// got. Its audio, control and timing sockets are bound and never read.
    struct LoopbackReceiver {
        port: u16,
        methods: Arc<Mutex<Vec<String>>>,
        stop: Arc<AtomicBool>,
        thread: Option<JoinHandle<()>>,
    }

    impl LoopbackReceiver {
        fn start() -> LoopbackReceiver {
            let loopback = SocketAddr::from((Ipv4Addr::LOCALHOST, 0));
            let listener = TcpListener::bind(loopback).expect("a loopback port");
            listener.set_nonblocking(true).unwrap();
            let port = listener.local_addr().unwrap().port();
            let udp: Vec<UdpSocket> = (0..3)
                .map(|_| UdpSocket::bind(loopback).expect("a loopback port"))
                .collect();
            let transport = format!(
                "RTP/AVP/UDP;unicast;mode=record;server_port={};control_port={};timing_port={}",
                udp[0].local_addr().unwrap().port(),
                udp[1].local_addr().unwrap().port(),
                udp[2].local_addr().unwrap().port(),
            );
            let methods = Arc::new(Mutex::new(Vec::new()));
            let stop = Arc::new(AtomicBool::new(false));
            let thread = {
                let (methods, stop) = (Arc::clone(&methods), Arc::clone(&stop));
                thread::spawn(move || {
                    let _udp = udp;
                    while !stop.load(Ordering::SeqCst) {
                        match listener.accept() {
                            Ok((stream, _)) => serve(stream, &transport, &methods, &stop),
                            Err(_) => thread::sleep(Duration::from_millis(10)),
                        }
                    }
                })
            };
            LoopbackReceiver {
                port,
                methods,
                stop,
                thread: Some(thread),
            }
        }

        /// What discovery would report for it.
        fn record(&self) -> ServiceRecord {
            ServiceRecord {
                kind: ServiceKind::Raop,
                instance: "02005E005309@Loopback".to_owned(),
                host: "loopback.local".to_owned(),
                port: self.port,
                txt: OPEN_TXT.iter().map(|s| s.as_bytes().to_vec()).collect(),
                addresses: vec![ScopedAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST))],
            }
        }

        fn methods(&self) -> Vec<String> {
            self.methods
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .clone()
        }
    }

    impl Drop for LoopbackReceiver {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::SeqCst);
            if let Some(thread) = self.thread.take() {
                let _ = thread.join();
            }
        }
    }

    /// Serves one RTSP connection until the sender closes it.
    fn serve(
        mut stream: TcpStream,
        transport: &str,
        methods: &Mutex<Vec<String>>,
        stop: &AtomicBool,
    ) {
        let _ = stream.set_nonblocking(false);
        let _ = stream.set_read_timeout(Some(Duration::from_millis(50)));
        let mut parser = Parser::new();
        let mut buf = [0u8; 8192];
        while !stop.load(Ordering::SeqCst) {
            match stream.read(&mut buf) {
                Ok(0) => return,
                Ok(len) => parser.push(&buf[..len]),
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                    ) =>
                {
                    continue;
                }
                Err(_) => return,
            }
            while let Ok(Some(Message::Request(request))) = parser.next_message() {
                methods
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .push(request.method.as_str().to_owned());
                if stream
                    .write_all(&answer(&request, transport).encode())
                    .is_err()
                {
                    return;
                }
            }
        }
    }

    /// Success, with what each method's answer carries.
    fn answer(request: &Request, transport: &str) -> Response {
        let mut response = Response::new(200, "OK");
        let cseq = request.headers.get("CSeq").unwrap_or("0").to_owned();
        response.headers.push("CSeq", cseq);
        match request.method {
            Method::Options => response.headers.push(
                "Public",
                "ANNOUNCE, SETUP, RECORD, PAUSE, FLUSH, TEARDOWN, OPTIONS, GET_PARAMETER, SET_PARAMETER",
            ),
            Method::Setup => {
                response.headers.push("Transport", transport);
                response.headers.push("Session", "1");
            }
            Method::Flush => response.headers.push("RTP-Info", "rtptime=1"),
            _ => {}
        }
        response
    }

    /// A browser that reports one service, once.
    struct OneService(Option<BrowseEvent>);

    impl Browser for OneService {
        fn poll_event(&mut self) -> Option<BrowseEvent> {
            self.0.take()
        }
    }

    /// Polls `backend` until `done` holds for an event or the backend, at
    /// most 15 s.
    fn poll_until(
        backend: &mut MockBackend,
        mut done: impl FnMut(&MockBackend, &[BackendEvent]) -> bool,
    ) -> bool {
        let until = Instant::now() + Duration::from_secs(15);
        while Instant::now() < until {
            let events = backend.poll();
            if done(backend, &events) {
                return true;
            }
            thread::sleep(Duration::from_millis(20));
        }
        false
    }

    #[test]
    fn a_loopback_receiver_is_listed_and_plays_through_the_real_sender() {
        let receiver = LoopbackReceiver::start();
        let (link, input) = SenderLink::start().expect("the sender starts");
        link.sender
            .start_discovery_with(Box::new(OneService(Some(BrowseEvent::Resolved(
                receiver.record(),
            )))));
        let mut backend = MockBackend::empty();
        backend.connect_output("speakers", "Speakers");
        backend.set_output_enabled("speakers", true).unwrap();
        backend.list_outputs().unwrap();
        backend.set_airplay(Box::new(link), Some(FeedSlot::new(input)));

        // A receiver appears: it is listed, and can play.
        assert!(
            poll_until(&mut backend, |backend, _| !backend
                .airplay_speakers()
                .is_empty()),
            "the receiver is listed"
        );
        let speaker = backend.airplay_speakers().remove(0);
        assert_eq!(speaker.name, "Loopback");
        assert_eq!(speaker.availability, AirPlayAvailability::Open);

        // Turning it on while streaming starts a session, and the local
        // output moves to L + its delay.
        backend.start(&SourceId::WholeSystem).unwrap();
        assert_eq!(backend.playing_params("speakers").unwrap().delay_frames, 0);
        backend.set_airplay_enabled(&speaker.id, true).unwrap();
        assert!(
            poll_until(&mut backend, |_, events| events.contains(
                &BackendEvent::AirPlayPlaying {
                    id: speaker.id.clone()
                }
            )),
            "it plays: {:?}",
            receiver.methods()
        );
        assert!(backend.airplay_speakers()[0].playing);
        assert_eq!(
            backend.playing_params("speakers").unwrap().delay_frames,
            84_000,
            "L = 1750 ms at 48 kHz"
        );
        let methods = receiver.methods();
        for method in ["OPTIONS", "ANNOUNCE", "SETUP", "RECORD"] {
            assert!(
                methods.iter().any(|m| m == method),
                "{method} in {methods:?}"
            );
        }

        // Turning it off tears its session down and the local output goes
        // back to its own delay.
        backend.set_airplay_enabled(&speaker.id, false).unwrap();
        assert_eq!(backend.playing_params("speakers").unwrap().delay_frames, 0);
        let until = Instant::now() + Duration::from_secs(5);
        while !receiver.methods().iter().any(|m| m == "TEARDOWN") && Instant::now() < until {
            thread::sleep(Duration::from_millis(20));
        }
        assert!(receiver.methods().iter().any(|m| m == "TEARDOWN"));
        backend.stop();
    }
}
