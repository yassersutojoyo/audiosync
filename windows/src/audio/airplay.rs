//! AirPlay speakers, through AudioSync's own sender (`audiosync-airplay`).
//!
//! The sender starts with the app and browses the network for receivers.
//! Every receiver it finds is listed, with whether it can play: this sender
//! speaks AirPlay 1, so a receiver that only AirPlay 2 can play is listed
//! with that reason and never connected. A receiver that asks for a password
//! gets one when the user switches it on: the UI prompts for it
//! ([`AirPlaySpeakers::password_prompt`]), and the password stays in memory
//! for its sessions until AudioSync quits, as the Mac app asks for it once
//! per launch. Until a speaker the user switched on plays, a session that
//! asks for a password, announced or not, or turns the one it got down,
//! prompts again (the password is forgotten); later, or for a speaker
//! auto-connect switched on, it goes off with a notice. An Apple TV that
//! asks for a PIN is paired once: when the user switches it on, the sender
//! asks it to show a PIN, the UI prompts for it
//! ([`AirPlaySpeakers::pin_prompt`]), and the credentials the pairing gives
//! are kept in the [`PairingStore`] and handed back each time the
//! speaker joins, so no PIN is asked again. A wrong PIN starts a new pairing
//! (the Apple TV shows a new PIN); any other failure leaves the speaker off
//! with a notice. A pairing the user cancels and starts again at once waits
//! for the cancelled one's end, and nothing the cancelled one reports reaches
//! the UI, though credentials it got are kept. A speaker the user turns on
//! joins the sender's stream while streaming is on, and leaves it when it is
//! turned off or streaming stops. Its volume and its own delay d_s go to the
//! sender as they change. Errors come back as backend events, which the UI
//! turns into notices: a speaker that needs the user (a password, pairing, a
//! format it can't take) is turned off, while one that stopped answering
//! stays on as the sender keeps trying (spec P142).
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
//! A sender that stopped after an internal error, or whose discovery did or
//! couldn't start, is reported once as [`BackendEvent::AirPlayStopped`]; the
//! user's Retry replaces it with a new one from a [`SenderStarter`], and the
//! speakers that were on keep their settings and play again once listed. A
//! sender that couldn't start at all is reported the same way (by the
//! controller), and Retry starts one.
//!
//! The sender sits behind [`AirPlayLink`], so the backend logic is tested
//! with a fake; [`SenderLink`] is the real one.

use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use audiosync_airplay::discovery::mdns::DiscoveryError;
use audiosync_airplay::discovery::{Device, DeviceKey, ProtocolPath, ServiceKind};
use audiosync_airplay::error::{ErrorKind, UnsupportedReason, UserReason};
use audiosync_airplay::homekit::HomeKitCredentials;
use audiosync_airplay::input::InputWriter;
use audiosync_airplay::metadata::MetadataError;
use audiosync_airplay::pairing::{Credentials, PairingEvent, PinError};
use audiosync_airplay::secret::Secret;
use audiosync_airplay::sender::{
    AIRPLAY2_SWITCH_ENV, NowPlaying, ReceiverEvent, ReceiverInfo, Sender, SenderConfig,
    SenderEvent, SpeakerError, SpeakerEvent, SpeakerOptions, SpeakerState, airplay2_switch,
};
use audiosync_airplay::stream::{MAX_DELAY, SpeakerCounters, StreamCounters};
use audiosync_airplay::targets::{Target, TargetChange, TargetId, TargetKind};
use audiosync_airplay::volume::{MIN_DB, Volume};
use audiosync_core::fade::InputFade;
use audiosync_core::resample::Resampler;
use audiosync_core::volume::{DEFAULT_LOCAL_POSITION, sanitize_position};

use super::SESSION_RATE;
use super::backend::{
    AirPlayAvailability, AirPlayGroup, AirPlayGroupKind, AirPlayInfo, AirPlayPasswordPrompt,
    AirPlayPinPrompt, AirPlayProblem, BackendEvent, PairingPhase,
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

    /// Plays a stereo pair or home theater group with one delay and one
    /// volume, those of `options`; `members` holds each member's own
    /// password and credentials (P177). Returns the members' counters when
    /// the sender keeps them.
    fn add_target(
        &mut self,
        target: &Target,
        options: SpeakerOptions,
        members: BTreeMap<DeviceKey, SpeakerOptions>,
    ) -> Vec<(DeviceKey, Arc<SpeakerCounters>)>;

    /// Stops every member of a target.
    fn remove_target(&mut self, id: &TargetId);

    /// Sets a target's one volume (P180).
    fn set_target_volume(&mut self, id: &TargetId, volume: Volume);

    /// Sets a target's one delay d_s (P179).
    fn set_target_delay(&mut self, id: &TargetId, delay: Duration);

    /// Sets a speaker's volume.
    fn set_volume(&mut self, key: &DeviceKey, volume: Volume);

    /// Sets a speaker's own delay d_s.
    fn set_delay(&mut self, key: &DeviceKey, delay: Duration);

    /// The capture restarted: every speaker is flushed and the timeline
    /// starts over (P143).
    fn restart_timeline(&mut self);

    /// What is playing, for every speaker that shows it, now and when one
    /// starts (P123 to P127).
    fn set_now_playing(&mut self, now_playing: NowPlaying) -> Result<(), MetadataError>;

    /// L, the sender's base latency.
    fn latency(&self) -> Duration;

    /// The stream's counters, when the sender keeps them.
    fn stream_counters(&self) -> Option<Arc<StreamCounters>>;

    /// The next event, if one is waiting; never blocks.
    fn poll_event(&mut self) -> Option<SenderEvent>;

    /// Starts pairing with an Apple TV that asks for a PIN, with the
    /// speaker's password when AudioSync has one (an Apple TV can ask for
    /// both); a pairing already running for it is replaced.
    fn start_pairing(&mut self, device: &Device, password: Option<Secret>);

    /// The PIN the user typed: digits only.
    fn submit_pin(&mut self, key: &DeviceKey, pin: &str) -> Result<(), PinError>;

    /// Stops a pairing.
    fn cancel_pairing(&mut self, key: &DeviceKey);

    /// Forgets the AirPlay 2 credentials the sender keeps for a device.
    fn forget_homekit(&mut self, key: &DeviceKey);

    /// False once the sender stopped by itself after an internal error (its
    /// driver thread ended): it lists, plays and reports nothing anymore,
    /// and only a new sender brings AirPlay back.
    fn is_running(&self) -> bool;

    /// Whether the hidden AirPlay 2 switch is on for this sender.
    fn airplay2(&self) -> bool {
        false
    }

    /// The stored AirPlay 2 device ID, for its AirPlay 2 sessions.
    fn set_airplay2_device_id(&mut self, _id: [u8; 6]) {}
}

/// A sender a [`SenderStarter`] started.
pub struct StartedSender {
    /// The sender.
    pub link: Box<dyn AirPlayLink>,
    /// The feed for its input.
    pub feed: Option<FeedSlot>,
    /// Whether it browses for receivers: false when its discovery couldn't
    /// start, so it lists nothing.
    pub browsing: bool,
}

impl StartedSender {
    /// A sender that browses for receivers, with `feed` for its input.
    #[cfg(test)]
    pub fn browsing(link: Box<dyn AirPlayLink>, feed: Option<FeedSlot>) -> StartedSender {
        StartedSender {
            link,
            feed,
            browsing: true,
        }
    }
}

/// Starts a sender: at launch, and when the user retries after one stopped
/// or couldn't start; `None` when it couldn't start (it logs why).
pub type SenderStarter = Box<dyn FnMut() -> Option<StartedSender>>;

/// The real sender.
pub struct SenderLink {
    sender: Sender,
    airplay2: bool,
}

impl SenderLink {
    /// Starts the sender, whose stream and threads run from now on, and
    /// gives its audio input for the capture.
    pub fn start() -> io::Result<(SenderLink, InputWriter)> {
        // The hidden AirPlay 2 switch (spec P2): an environment variable, so
        // it flips without a rebuild and no control shows it.
        let config = SenderConfig {
            airplay2: airplay2_switch(std::env::var(AIRPLAY2_SWITCH_ENV).ok().as_deref()),
            ..SenderConfig::default()
        };
        let airplay2 = config.airplay2;
        let (sender, input) = Sender::start(config)?;
        Ok((SenderLink { sender, airplay2 }, input))
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

    fn add_target(
        &mut self,
        target: &Target,
        options: SpeakerOptions,
        members: BTreeMap<DeviceKey, SpeakerOptions>,
    ) -> Vec<(DeviceKey, Arc<SpeakerCounters>)> {
        self.sender.add_target(target.clone(), options, |key, own| {
            if let Some(member) = members.get(key) {
                *own = member.clone();
            }
        })
    }

    fn remove_target(&mut self, id: &TargetId) {
        self.sender.remove_target(id);
    }

    fn set_target_volume(&mut self, id: &TargetId, volume: Volume) {
        self.sender.set_target_volume(id, volume);
    }

    fn set_target_delay(&mut self, id: &TargetId, delay: Duration) {
        self.sender.set_target_delay(id, delay);
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

    fn set_now_playing(&mut self, now_playing: NowPlaying) -> Result<(), MetadataError> {
        self.sender.set_now_playing(now_playing)
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

    fn start_pairing(&mut self, device: &Device, password: Option<Secret>) {
        self.sender
            .start_pairing_with_password(device.clone(), password);
    }

    fn submit_pin(&mut self, key: &DeviceKey, pin: &str) -> Result<(), PinError> {
        self.sender.submit_pin(key, pin)
    }

    fn cancel_pairing(&mut self, key: &DeviceKey) {
        self.sender.cancel_pairing(key);
    }

    fn forget_homekit(&mut self, key: &DeviceKey) {
        self.sender.forget_homekit(key);
    }

    fn is_running(&self) -> bool {
        self.sender.is_running()
    }

    fn airplay2(&self) -> bool {
        self.airplay2
    }

    fn set_airplay2_device_id(&mut self, id: [u8; 6]) {
        self.sender.set_airplay2_device_id(id);
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
        PairingEvent::HomeKitPaired(_) => "paired on AirPlay 2".to_owned(),
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

/// Whether a receiver can play from this sender: AirPlay 1, once it has its
/// password if it asks for one, and for an Apple TV that asks for a PIN, once
/// `paired`.
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
        Ok(()) if verdict.needs_password => AirPlayAvailability::NeedsPassword,
        Ok(()) if verdict.needs_pairing && !verdict.legacy_pairing => {
            AirPlayAvailability::PairingNotYet
        }
        Ok(()) => AirPlayAvailability::Open,
    }
}

/// Whether the sender reaches a receiver over AirPlay 2: only with the
/// switch on, and only one that AirPlay 1 can't drive, by discovery or by
/// how its last AirPlay 1 attempt ended (`airplay1_outcome`, P2), the input
/// the sender decides on.
pub fn reached_over_airplay2(
    info: &ReceiverInfo,
    airplay2: bool,
    airplay1_outcome: Option<ErrorKind>,
) -> bool {
    airplay2 && info.device.choose_path(true, airplay1_outcome) == Ok(ProtocolPath::AirPlay2)
}

/// Whether a receiver reached over AirPlay 2 can play: one whose flags ask
/// for a PIN, once `paired` (P61); one that asks for a password, with it
/// or once paired.
pub fn airplay2_availability(info: &ReceiverInfo, paired: bool) -> AirPlayAvailability {
    let verdict = &info.device.verdict;
    if paired {
        AirPlayAvailability::Open
    } else if verdict.needs_pairing {
        AirPlayAvailability::NeedsPin
    } else if verdict.needs_password {
        AirPlayAvailability::NeedsPassword
    } else {
        AirPlayAvailability::Open
    }
}

/// What to tell the user about an error that ended a speaker's session.
pub fn problem(reason: UserReason) -> AirPlayProblem {
    match reason {
        UserReason::NotResponding => AirPlayProblem::NotResponding,
        UserReason::ConnectionLost => AirPlayProblem::ConnectionLost,
        UserReason::Busy => AirPlayProblem::Busy,
        UserReason::Refused | UserReason::Incompatible => AirPlayProblem::Refused,
        UserReason::PasswordRequired | UserReason::WrongPassword => AirPlayProblem::NeedsPassword,
        // A session to an Apple TV whose credentials it turned down, or a
        // receiver that wants another kind of pairing.
        UserReason::PairingRequired => AirPlayProblem::PairingNotYet,
        UserReason::Unsupported(UnsupportedReason::AirPlay2Only) => AirPlayProblem::AirPlay2NotYet,
        UserReason::Unsupported(UnsupportedReason::Timing) => AirPlayProblem::TimingUnavailable,
        UserReason::Unsupported(UnsupportedReason::OneAirPlay2Session) => {
            AirPlayProblem::OneAirPlay2Speaker
        }
        UserReason::Unsupported(UnsupportedReason::PlaysThroughSpeakers) => {
            AirPlayProblem::PlaysThroughSpeakers
        }
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

/// A delay setting as the sender's d_s, 0 to 20 000 ms.
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
        AirPlayAvailability::NeedsPassword => "asks for a password",
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
    /// d_s in ms, 0 to 20 000.
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

/// A password prompt waiting for the user.
struct PasswordAsk {
    /// The speaker turned the last password down.
    wrong: bool,
    /// When it was asked for (counts up): the oldest prompt shows first.
    order: u64,
    /// It is for a pairing with an Apple TV that asks for a password too,
    /// which starts again with it.
    pair: bool,
    /// The stereo pair or home theater group whose member asks: the
    /// password typed switches that target on.
    target: Option<String>,
}

/// What the sender plays for one row: a speaker, or a stereo pair or home
/// theater group, whose members each get a session (P177).
enum Joined {
    Speaker(DeviceKey),
    Target(TargetId),
}

/// A speaker in the sender.
struct Added {
    joined: Joined,
    /// A target's state is the best of its members' states: it plays while
    /// one of them does (P181).
    state: SpeakerState,
    /// Each session's counters, by speaker ID: the speaker's own, or each
    /// member's.
    counters: Vec<(String, Arc<SpeakerCounters>)>,
    /// A target's members by speaker ID, with the last state each reported.
    members: BTreeMap<String, Option<SpeakerState>>,
}

impl Added {
    fn speaker(key: DeviceKey, counters: Option<Arc<SpeakerCounters>>) -> Added {
        let id = speaker_id(&key);
        Added {
            joined: Joined::Speaker(key),
            state: SpeakerState::Connecting,
            counters: counters
                .map(|counters| (id, counters))
                .into_iter()
                .collect(),
            members: BTreeMap::new(),
        }
    }

    fn key(&self) -> Option<&DeviceKey> {
        match &self.joined {
            Joined::Speaker(key) => Some(key),
            Joined::Target(_) => None,
        }
    }

    fn set_volume(&self, link: &mut dyn AirPlayLink, volume: Volume) {
        match &self.joined {
            Joined::Speaker(key) => link.set_volume(key, volume),
            Joined::Target(id) => link.set_target_volume(id, volume),
        }
    }

    fn set_delay(&self, link: &mut dyn AirPlayLink, delay: Duration) {
        match &self.joined {
            Joined::Speaker(key) => link.set_delay(key, delay),
            Joined::Target(id) => link.set_target_delay(id, delay),
        }
    }

    fn remove(&self, link: &mut dyn AirPlayLink) {
        match &self.joined {
            Joined::Speaker(key) => link.remove_speaker(key),
            Joined::Target(id) => link.remove_target(id),
        }
    }
}

/// How a member's state ranks for its target's: playing first, then
/// connecting, retrying, failed, stopped.
fn state_rank(state: &SpeakerState) -> u8 {
    match state {
        SpeakerState::Playing => 0,
        SpeakerState::Connecting => 1,
        SpeakerState::Retrying { .. } => 2,
        SpeakerState::Failed => 3,
        SpeakerState::Stopped => 4,
    }
}

/// The AirPlay side of the backend: what discovery lists, the user's
/// settings, and the speakers in the sender. See the module documentation.
pub struct AirPlaySpeakers {
    link: Box<dyn AirPlayLink>,
    /// The receivers listed now, by speaker ID.
    listed: BTreeMap<String, Listed>,
    /// The settings of every speaker the user changed, kept while it's gone.
    settings: BTreeMap<String, Settings>,
    /// The speakers in the sender, by speaker ID; a stereo pair or home
    /// theater group by its target ID.
    added: BTreeMap<String, Added>,
    /// The stereo pairs and home theater groups listed now, by target ID,
    /// only while the AirPlay 2 switch is on (P176).
    targets: BTreeMap<String, Target>,
    /// The members of each added target that play nothing now, as the
    /// sender last reported them (P181).
    target_missing: BTreeMap<String, Vec<String>>,
    /// The targets discovery no longer reports that the sender still plays:
    /// they stay in `targets` until they leave it, so their row can be
    /// switched off and their members stay held.
    gone_targets: BTreeSet<String>,
    /// The last name of every speaker seen, for logs and notices.
    names: BTreeMap<String, String>,
    /// The credentials of the paired Apple TVs.
    store: PairingStore,
    /// The key of every speaker the sender paired on AirPlay 2, by speaker
    /// ID: Forget Pairing drops the sender's copy of a speaker no longer
    /// listed or added through it.
    homekit_keys: BTreeMap<String, DeviceKey>,
    /// The pairings running, by speaker ID.
    pairings: BTreeMap<String, PairingRun>,
    /// The speakers whose cancelled pairing hasn't reported its end yet.
    /// The sender's pairing events name the receiver only, so until that
    /// end every one for the speaker is the cancelled pairing's: it never
    /// reaches the UI or the pairing in `pairings`. A pairing the user
    /// starts meanwhile waits in `pairings` and starts on the sender once
    /// that end comes (a pairing ends once), so the two never run at once;
    /// when that end is a success, it never starts and ends as paired.
    ending: BTreeSet<String>,
    /// The next `PairingRun::shown_order`.
    next_shown_order: u64,
    /// The passwords the user typed, by speaker ID, until AudioSync quits.
    /// Never logged or written anywhere.
    passwords: BTreeMap<String, Secret>,
    /// The password prompts waiting, by speaker ID.
    password_asks: BTreeMap<String, PasswordAsk>,
    /// The next `PasswordAsk::order`.
    next_ask_order: u64,
    /// The speakers the user switched on, or typed a password for, that
    /// haven't played since: a session that asks for a password or turns
    /// the one it got down prompts for it instead of turning them off.
    password_attempts: BTreeSet<String>,
    /// The speakers whose session asked for a password discovery didn't
    /// announce: they count as asking for one until AudioSync quits.
    password_seen: BTreeSet<String>,
    /// The speakers reached over AirPlay 2 whose session asked for a PIN
    /// pairing their flags didn't announce (P61): they count as asking for
    /// one until paired or AudioSync quits.
    pin_seen: BTreeSet<String>,
    /// How the last AirPlay 1 attempt of a speaker ended when that moved it
    /// to AirPlay 2, as the sender keeps it while the speaker stays added.
    airplay1_outcome: BTreeMap<String, ErrorKind>,
    streaming: bool,
    master: f64,
    latency: Duration,
    /// Added to every speaker's d_s while a Google Cast speaker plays later
    /// than L (C77), in ms.
    hold_back_ms: f64,
    /// While Calibrate All plays a speaker's sweep, the one speaker heard;
    /// the others are muted.
    calibration_solo: Option<String>,
    /// The sender stopped after an internal error, or its discovery did;
    /// reported once, until a new sender replaces it.
    stopped: bool,
    /// The last write of the pairings' file failed.
    store_unsaved: bool,
    /// Whether the pairings' file can be written changed since the last
    /// poll, which reports it ([`BackendEvent::PairingStore`]).
    store_change: Option<bool>,
    /// The file name of the copy a write of the pairings' file kept aside
    /// since the last poll, which reports it
    /// ([`BackendEvent::PairingsKeptAside`]).
    store_kept_aside: Option<String>,
    /// What is playing, as the sender was given it last: a new sender gets
    /// it too.
    now_playing: Option<NowPlaying>,
    /// The speaker IDs that get PCM instead of ALAC, from
    /// [`audiosync_airplay::sender::PCM_DEVICES_ENV`] at launch.
    pcm_devices: BTreeSet<String>,
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
        let mut speakers = AirPlaySpeakers {
            link,
            listed: BTreeMap::new(),
            settings: BTreeMap::new(),
            added: BTreeMap::new(),
            targets: BTreeMap::new(),
            target_missing: BTreeMap::new(),
            gone_targets: BTreeSet::new(),
            names: BTreeMap::new(),
            store,
            homekit_keys: BTreeMap::new(),
            pairings: BTreeMap::new(),
            ending: BTreeSet::new(),
            next_shown_order: 0,
            passwords: BTreeMap::new(),
            password_asks: BTreeMap::new(),
            next_ask_order: 0,
            password_attempts: BTreeSet::new(),
            password_seen: BTreeSet::new(),
            pin_seen: BTreeSet::new(),
            airplay1_outcome: BTreeMap::new(),
            streaming: false,
            master,
            latency,
            hold_back_ms: 0.0,
            calibration_solo: None,
            stopped: false,
            store_unsaved: false,
            store_change: None,
            store_kept_aside: None,
            now_playing: None,
            pcm_devices: audiosync_airplay::sender::pcm_devices(
                std::env::var(audiosync_airplay::sender::PCM_DEVICES_ENV)
                    .ok()
                    .as_deref(),
            ),
            log,
        };
        speakers.open_airplay2();
        speakers
    }

    /// Hands a sender with the hidden AirPlay 2 switch on the stored device
    /// ID; with the switch off the identity file is never touched.
    fn open_airplay2(&mut self) {
        if !self.link.airplay2() {
            return;
        }
        let (id, found) = self.store.open_airplay2();
        self.log.log(|| format!("AirPlay 2 identity: {found}"));
        self.link.set_airplay2_device_id(id);
    }

    /// Takes in whether a write of the pairings' file worked: a change from
    /// working to failing, or back, is reported at the next poll, so the
    /// user is told once while the writes keep failing. A file ignored at
    /// launch that the write kept aside is reported at the next poll too
    /// (the store tells that once).
    fn store_written(&mut self, saved: bool) {
        if saved == self.store_unsaved {
            self.store_unsaved = !saved;
            self.store_change = Some(saved);
        }
        self.note_kept_aside();
    }

    /// Takes in where the store kept the pairings' file ignored at launch
    /// aside, if it just did, for the next poll to report.
    fn note_kept_aside(&mut self) {
        if let Some(kept) = self.store.take_kept_aside() {
            let file = kept.file_name().unwrap_or_default().to_string_lossy();
            self.store_kept_aside = Some(file.into_owned());
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
        self.targets.clear();
        self.target_missing.clear();
        self.gone_targets.clear();
        self.homekit_keys.clear();
        self.pairings.clear();
        self.ending.clear();
        // A password prompt still waiting (discovery stopped, the sessions
        // didn't) stays, as the speaker joins with it once listed again;
        // one for a pairing goes with the pairing, and the speaker stays off.
        self.password_asks.retain(|_, ask| !ask.pair);
        self.latency = self.link.latency();
        self.stopped = false;
        self.open_airplay2();
        self.log.log(|| {
            format!(
                "AirPlay: a new sender runs, L = {} ms; the speakers that were on join once listed",
                self.latency.as_millis()
            )
        });
        if let Some(now_playing) = self.now_playing.clone() {
            self.send_now_playing(now_playing);
        }
    }

    /// What is playing, for the speakers that show it: the sender keeps it
    /// and sends it to every speaker that starts.
    pub fn set_now_playing(&mut self, now_playing: NowPlaying) {
        self.now_playing = Some(now_playing.clone());
        self.send_now_playing(now_playing);
    }

    fn send_now_playing(&mut self, now_playing: NowPlaying) {
        if let Err(error) = self.link.set_now_playing(now_playing) {
            self.log
                .log(|| format!("AirPlay: the sender refused what is playing: {error}"));
        }
    }

    /// The sender stopped by itself: nothing it listed, played or paired is
    /// there anymore. Returns whether the speaker list changed.
    fn sender_stopped(&mut self, events: &mut Vec<BackendEvent>) -> bool {
        let changed = !(self.listed.is_empty()
            && self.added.is_empty()
            && self.pairings.is_empty()
            && self.password_asks.is_empty());
        self.listed.clear();
        self.added.clear();
        self.targets.clear();
        self.target_missing.clear();
        self.gone_targets.clear();
        self.homekit_keys.clear();
        self.pairings.clear();
        self.ending.clear();
        self.password_attempts.clear();
        // Its prompts go with it, as on the Mac. A speaker left on without
        // its password would stay silent, so it goes off with a notice.
        for (id, ask) in std::mem::take(&mut self.password_asks) {
            let owner = ask.target.unwrap_or(id);
            self.turn_off(&owner, AirPlayProblem::NeedsPassword, events);
        }
        if changed || !self.stopped {
            self.log.log(|| {
                "AirPlay: the sender stopped after an internal error; AirPlay stays silent until Retry"
                    .to_owned()
            });
        }
        self.mark_stopped(events);
        changed
    }

    /// The sender's discovery couldn't start, so it lists nothing: reported
    /// as when discovery stops later, so that Retry starts a new sender.
    pub fn discovery_unavailable(&mut self, events: &mut Vec<BackendEvent>) {
        self.log.log(|| {
            "AirPlay: discovery couldn't start, so no receiver is listed; Retry starts a new sender"
                .to_owned()
        });
        self.mark_stopped(events);
    }

    /// Counts the sender as stopped, reported once until a new one replaces
    /// it.
    fn mark_stopped(&mut self, events: &mut Vec<BackendEvent>) {
        if !self.stopped {
            self.stopped = true;
            events.push(BackendEvent::AirPlayStopped);
        }
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
        if let Some(target) = self.targets.get(id) {
            return Some(self.target_availability(target));
        }
        self.listed
            .get(id)
            .map(|listed| match self.availability_of(id, &listed.info) {
                AirPlayAvailability::Open if self.password_seen.contains(id) => {
                    AirPlayAvailability::NeedsPassword
                }
                availability => availability,
            })
    }

    /// What discovery and the credentials kept say about a receiver, on
    /// the path the sender takes to it.
    fn availability_of(&self, id: &str, info: &ReceiverInfo) -> AirPlayAvailability {
        if !self.reached_over_airplay2(id, info) {
            return availability(info, self.store.contains(id));
        }
        let paired = self.store.homekit(id).is_some();
        match airplay2_availability(info, paired) {
            AirPlayAvailability::Open if !paired && self.pin_seen.contains(id) => {
                AirPlayAvailability::NeedsPin
            }
            availability => availability,
        }
    }

    /// Whether the sender reaches the listed speaker `id` over AirPlay 2.
    fn over_airplay2(&self, id: &str) -> bool {
        self.listed
            .get(id)
            .is_some_and(|listed| self.reached_over_airplay2(id, &listed.info))
    }

    fn reached_over_airplay2(&self, id: &str, info: &ReceiverInfo) -> bool {
        let outcome = self.airplay1_outcome.get(id).copied();
        reached_over_airplay2(info, self.link.airplay2(), outcome)
    }

    /// Whether a listed speaker can join the stream: it can play, or it asks
    /// for a password the user typed.
    fn can_join(&self, id: &str) -> bool {
        if self.held_by_target(id) {
            return false;
        }
        match self.listed_availability(id) {
            Some(AirPlayAvailability::Open) => true,
            Some(AirPlayAvailability::NeedsPassword) => self.passwords.contains_key(id),
            _ => false,
        }
    }

    /// Whether the speaker `id` plays only as part of its target: the sender
    /// holds it for one, or it is off and its target is listed (P176). One
    /// left on by itself before its target was listed still plays alone.
    fn held_by_target(&self, id: &str) -> bool {
        self.added_target_of(id).is_some()
            || (self.target_of(id).is_some() && !self.settings(id).enabled)
    }

    /// The name of the stereo pair or home theater group that holds the
    /// speaker `id`, which then stays off by itself
    /// ([`AirPlayProblem::PlaysInGroup`]).
    pub fn holding_target(&self, id: &str) -> Option<String> {
        if !self.held_by_target(id) {
            return None;
        }
        let target = self
            .added_target_of(id)
            .or_else(|| self.target_of(id).map(|target| target.id.to_string()))?;
        self.targets.get(&target).map(|target| target.name.clone())
    }

    /// The listed stereo pair or home theater group that the listed speaker
    /// `id` is a member of (P176).
    fn target_of(&self, id: &str) -> Option<&Target> {
        let target = self.listed.get(id)?.info.target.as_ref()?;
        self.targets.get(&target.to_string())
    }

    /// Whether the members of `target` in view can play; otherwise why the
    /// first that can't doesn't. A member that asks for a PIN is paired
    /// from its own row only, which a target hides; one that asks for a
    /// password is asked for it by name when the target is switched on.
    fn target_availability(&self, target: &Target) -> AirPlayAvailability {
        target
            .members
            .iter()
            .map(|member| speaker_id(&member.key))
            .filter_map(|id| match self.listed_availability(&id)? {
                AirPlayAvailability::Open => None,
                AirPlayAvailability::NeedsPassword if self.passwords.contains_key(&id) => None,
                AirPlayAvailability::NeedsPin | AirPlayAvailability::PairingNotYet => {
                    Some(AirPlayAvailability::PairingNotYet)
                }
                availability => Some(availability),
            })
            .next()
            .unwrap_or(AirPlayAvailability::Open)
    }

    /// The listed home theater group whose Apple TV is `id`.
    fn group_controlled_by(&self, id: &str) -> Option<&Target> {
        self.targets.values().find(|target| {
            target
                .control
                .as_ref()
                .is_some_and(|control| speaker_id(control) == id)
        })
    }

    /// Whether the speaker or target `id` is on and plays, or can.
    fn on_and_able(&self, id: &str) -> bool {
        self.settings(id).enabled && (self.can_join(id) || self.is_playing(id))
    }

    /// Why the speaker or target `id` can't be switched on next to the
    /// other side of a home theater group: an Apple TV and its group never
    /// play at once (P177).
    fn home_theater_conflict(&self, id: &str) -> Option<AirPlayProblem> {
        if let Some(target) = self.targets.get(id)
            && let Some(control) = &target.control
            && self.on_and_able(&speaker_id(control))
        {
            return Some(AirPlayProblem::AppleTvPlays);
        }
        let group = self.group_controlled_by(id)?;
        let group = group.id.to_string();
        self.on_and_able(&group)
            .then_some(AirPlayProblem::PlaysThroughSpeakers)
    }

    /// What a target's row says about its members (P176, P181).
    fn group_info(&self, target: &Target) -> AirPlayGroup {
        let id = target.id.to_string();
        let name_of = |member: &str| {
            target
                .members
                .iter()
                .find(|known| speaker_id(&known.key) == member)
                .map(|known| known.name.clone())
                .or_else(|| self.names.get(member).cloned())
        };
        let mut missing: Vec<String> = target
            .missing()
            .filter_map(|key| name_of(&speaker_id(key)))
            .collect();
        let silent = self
            .target_missing
            .get(&id)
            .map(Vec::as_slice)
            .unwrap_or(&[]);
        for member in silent {
            if let Some(name) = name_of(member)
                && !missing.contains(&name)
            {
                missing.push(name);
            }
        }
        AirPlayGroup {
            kind: match target.id.kind {
                TargetKind::StereoPair => AirPlayGroupKind::StereoPair,
                TargetKind::HomeTheater => AirPlayGroupKind::HomeTheater,
            },
            members: target
                .members
                .iter()
                .map(|member| member.name.clone())
                .collect(),
            missing,
            incomplete: !target.complete() || !silent.is_empty(),
        }
    }

    /// Whether an AirPlay speaker is chosen: on, and listed as able to play
    /// or playing (S79, see the module documentation).
    pub fn chosen(&self) -> bool {
        self.settings
            .iter()
            .any(|(id, settings)| settings.enabled && (self.can_join(id) || self.is_playing(id)))
    }

    /// The speakers to show: the listed ones, and any that still plays
    /// while its announcement lapsed, by name.
    pub fn speakers(&self) -> Vec<AirPlayInfo> {
        // A member of a listed target shows as part of it, unless it is on
        // by itself, so it can be switched off (P176).
        let mut ids: Vec<&String> = self
            .listed
            .keys()
            .filter(|id| !self.held_by_target(id))
            .collect();
        ids.extend(self.targets.keys());
        ids.extend(self.added.keys().filter(|id| {
            !self.listed.contains_key(*id) && !self.targets.contains_key(*id) && self.is_playing(id)
        }));
        let mut speakers: Vec<AirPlayInfo> = ids
            .into_iter()
            .map(|id| {
                let settings = self.settings(id);
                if let Some(target) = self.targets.get(id) {
                    return AirPlayInfo {
                        id: id.clone(),
                        name: target.name.clone(),
                        availability: self.target_availability(target),
                        enabled: settings.enabled,
                        volume: settings.volume,
                        delay_ms: settings.delay_ms,
                        playing: self.is_playing(id),
                        paired: false,
                        pairing: false,
                        airplay2: target
                            .members
                            .iter()
                            .any(|member| self.over_airplay2(&speaker_id(&member.key))),
                        group: Some(self.group_info(target)),
                    };
                }
                AirPlayInfo {
                    id: id.clone(),
                    name: self.names.get(id).cloned().unwrap_or_else(|| id.clone()),
                    // One whose password the user typed can play.
                    availability: match self.listed_availability(id) {
                        Some(AirPlayAvailability::NeedsPassword) if self.can_join(id) => {
                            AirPlayAvailability::Open
                        }
                        availability => availability.unwrap_or(AirPlayAvailability::Open),
                    },
                    enabled: settings.enabled,
                    volume: settings.volume,
                    delay_ms: settings.delay_ms,
                    playing: self.is_playing(id),
                    paired: self.store.contains(id) || self.store.homekit(id).is_some(),
                    pairing: self.pairings.contains_key(id),
                    airplay2: self.over_airplay2(id),
                    group: None,
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
        if enabled && self.held_by_target(id) {
            self.log.log(|| {
                format!(
                    "AirPlay speaker {} stays off: it plays as part of its stereo pair or home theater group",
                    self.label(id)
                )
            });
            return Err(AirPlayProblem::PlaysInGroup);
        }
        if enabled
            && !self.can_join(id)
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
        if enabled && let Some(problem) = self.home_theater_conflict(id) {
            self.log.log(|| {
                format!(
                    "AirPlay speaker {} stays off: its home theater group and Apple TV can't play at once",
                    self.label(id)
                )
            });
            return Err(problem);
        }
        self.log.log(|| {
            let state = if enabled { "on" } else { "off" };
            format!("AirPlay speaker {} switched {state}", self.label(id))
        });
        // The target takes over its members that played on their own.
        if enabled && let Some(target) = self.targets.get(id) {
            let members: Vec<String> = target
                .members
                .iter()
                .map(|member| speaker_id(&member.key))
                .filter(|member| self.settings(member).enabled)
                .collect();
            for member in members {
                self.log.log(|| {
                    format!(
                        "AirPlay speaker {} switched off: it plays as part of {}",
                        self.label(&member),
                        self.label(id)
                    )
                });
                self.drop_password_ask(&member, "it plays as part of its target");
                self.entry(&member).enabled = false;
            }
        }
        if !enabled {
            self.stop_pairing(id, "the speaker was switched off");
            self.drop_password_ask(id, "the speaker was switched off");
        }
        self.entry(id).enabled = enabled;
        self.reconcile();
        Ok(())
    }

    /// Starts pairing with an Apple TV that asks for a PIN, which the user
    /// switched on. A pairing already running for it goes on; one the user
    /// cancelled just before that hasn't reported its end yet goes first,
    /// and this one starts on the sender once it has, or, when that one
    /// paired after all, ends there as paired with no PIN (see `ending`).
    /// Paired, the speaker is turned on ([`BackendEvent::AirPlayPaired`]).
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
        if self.ending.contains(id) {
            self.log.log(|| {
                format!(
                    "AirPlay pairing {}: starts once the cancelled pairing has ended",
                    self.label(id)
                )
            });
        } else {
            self.begin_pairing(id, &device);
        }
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

    /// Has the sender start a pairing with `device`, the speaker `id`.
    fn begin_pairing(&mut self, id: &str, device: &Device) {
        self.log.log(|| {
            format!(
                "AirPlay pairing {}: started, asking the Apple TV to show a PIN",
                self.label(id)
            )
        });
        let password = self.password(id);
        self.link.start_pairing(device, password);
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
    /// Until the sender reports its end, the speaker is `ending`.
    fn stop_pairing(&mut self, id: &str, why: &str) {
        if let Some(run) = self.pairings.remove(id) {
            // One that waited for a cancelled pairing's end never started
            // on the sender, and now never will.
            if !self.ending.contains(id) {
                self.link.cancel_pairing(&run.device.key);
                self.ending.insert(id.to_owned());
            }
            self.log
                .log(|| format!("AirPlay pairing {}: stopped, {why}", self.label(id)));
        }
    }

    /// Forgets the speaker's credentials: it is turned off, and pairs again
    /// the next time the user switches it on.
    pub fn forget_pairing(&mut self, id: &str) {
        self.stop_pairing(id, "the user forgot the pairing");
        let homekit = self.forget_homekit(id);
        match self.store.remove(id) {
            Ok(true) => {
                self.store_written(true);
                self.log
                    .log(|| format!("AirPlay pairing {}: credentials forgotten", self.label(id)));
            }
            Ok(false) if !homekit => return,
            Ok(false) => {}
            Err(error) => {
                self.store_written(false);
                self.log.log(|| {
                    format!(
                        "AirPlay pairing {}: credentials forgotten for this run, but the store couldn't be written: {error}",
                        self.label(id)
                    )
                });
            }
        }
        if self.settings(id).enabled {
            self.entry(id).enabled = false;
            self.reconcile();
        }
    }

    /// Forgets the speaker's AirPlay 2 credentials, if AudioSync keeps any.
    /// Returns whether it did.
    fn forget_homekit(&mut self, id: &str) -> bool {
        if self.store.homekit(id).is_none() {
            return false;
        }
        let key = self
            .listed
            .get(id)
            .map(|listed| listed.device().key.clone())
            .or_else(|| self.added.get(id).and_then(|added| added.key().cloned()))
            .or_else(|| self.homekit_keys.get(id).cloned());
        self.homekit_keys.remove(id);
        if let Some(key) = key {
            self.link.forget_homekit(&key);
        }
        let saved = self.store.remove_homekit(id);
        self.store_written(saved.is_ok());
        self.log.log(|| {
            let saved = match &saved {
                Ok(()) => String::new(),
                Err(error) => format!(" (the file couldn't be written: {error})"),
            };
            format!(
                "AirPlay pairing {}: AirPlay 2 credentials forgotten{saved}",
                self.label(id)
            )
        });
        true
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

    /// The user switched the speaker on themselves, and it is on: a new
    /// attempt (see `password_attempts`). Auto-connect and restored switches
    /// never start one.
    pub fn begin_attempt(&mut self, id: &str) {
        if self.settings(id).enabled {
            self.password_attempts.insert(id.to_owned());
        }
    }

    /// Asks the user for the password of a speaker they switched on that
    /// asks for one AudioSync doesn't have. It stays off until the password
    /// is typed ([`AirPlaySpeakers::submit_password`]).
    pub fn ask_password(&mut self, id: &str) -> Result<(), AirPlayProblem> {
        // A target asks for its member's password by the member's name.
        if let Some(target) = self.targets.get(id) {
            let member = target
                .members
                .iter()
                .map(|member| speaker_id(&member.key))
                .find(|member| {
                    self.listed_availability(member) == Some(AirPlayAvailability::NeedsPassword)
                        && !self.passwords.contains_key(member)
                });
            let Some(member) = member else {
                return Err(AirPlayProblem::Unsupported);
            };
            self.queue_password_ask(&member, false, false, Some(id));
            return Ok(());
        }
        if !self.listed.contains_key(id) {
            self.log.log(|| {
                format!(
                    "AirPlay speaker {}: no password prompt, the speaker isn't listed",
                    self.label(id)
                )
            });
            return Err(AirPlayProblem::Unsupported);
        }
        self.queue_password_ask(id, false, false, None);
        Ok(())
    }

    /// Queues the speaker's password prompt; one already waiting keeps its
    /// place.
    fn queue_password_ask(&mut self, id: &str, wrong: bool, pair: bool, target: Option<&str>) {
        let order = match self.password_asks.get(id) {
            Some(ask) => ask.order,
            None => {
                self.next_ask_order += 1;
                self.next_ask_order
            }
        };
        self.password_asks.insert(
            id.to_owned(),
            PasswordAsk {
                wrong,
                order,
                pair,
                target: target.map(str::to_owned),
            },
        );
        self.log.log(|| {
            let again = if wrong {
                " again, it turned the last one down"
            } else {
                ""
            };
            format!(
                "AirPlay speaker {}: asking for its password{again}",
                self.label(id)
            )
        });
    }

    /// Takes the password the user typed for the speaker, kept until
    /// AudioSync quits and never logged: the speaker is turned on, or the
    /// Apple TV pairing that asked for it starts again with it. Returns
    /// whether it was taken: not empty, while its prompt waits.
    pub fn submit_password(&mut self, id: &str, password: &str) -> bool {
        if password.is_empty() {
            return false;
        }
        let Some(ask) = self.password_asks.remove(id) else {
            return false;
        };
        self.passwords.insert(id.to_owned(), Secret::from(password));
        if ask.pair {
            self.log.log(|| {
                format!(
                    "AirPlay pairing {}: password typed, the pairing starts again",
                    self.label(id)
                )
            });
            return self.start_pairing(id).is_ok();
        }
        if let Some(target) = ask.target {
            self.log.log(|| {
                format!(
                    "AirPlay speaker {}: password typed, {} switched on",
                    self.label(id),
                    self.label(&target)
                )
            });
            self.password_attempts.insert(target.clone());
            self.entry(&target).enabled = true;
            self.reconcile();
            return true;
        }
        self.log.log(|| {
            format!(
                "AirPlay speaker {}: password typed, switched on",
                self.label(id)
            )
        });
        self.password_attempts.insert(id.to_owned());
        self.entry(id).enabled = true;
        self.reconcile();
        true
    }

    /// The user cancelled the speaker's password prompt: it is off.
    pub fn cancel_password(&mut self, id: &str) {
        let owner = self
            .password_asks
            .get(id)
            .and_then(|ask| ask.target.clone())
            .unwrap_or_else(|| id.to_owned());
        if self.drop_password_ask(id, "cancelled by the user") && self.settings(&owner).enabled {
            self.password_attempts.remove(&owner);
            self.entry(&owner).enabled = false;
            self.reconcile();
        }
    }

    /// Drops the speaker's password prompt, if one waits, saying why in the
    /// log, and ends its attempt; for a target, its members' prompts.
    /// Returns whether a prompt waited.
    fn drop_password_ask(&mut self, id: &str, why: &str) -> bool {
        self.password_attempts.remove(id);
        let askers: Vec<String> = self
            .password_asks
            .iter()
            .filter(|(asker, ask)| asker.as_str() == id || ask.target.as_deref() == Some(id))
            .map(|(asker, _)| asker.clone())
            .collect();
        for asker in &askers {
            self.password_asks.remove(asker);
            self.log.log(|| {
                format!(
                    "AirPlay speaker {}: password prompt closed, {why}",
                    self.label(asker)
                )
            });
        }
        !askers.is_empty()
    }

    /// The password prompt to show: the oldest one waiting, while no PIN
    /// prompt is open, so one dialog asks at a time.
    pub fn password_prompt(&self) -> Option<AirPlayPasswordPrompt> {
        if self.pin_prompt().is_some() {
            return None;
        }
        self.password_asks
            .iter()
            .min_by_key(|(_, ask)| ask.order)
            .map(|(id, ask)| AirPlayPasswordPrompt {
                id: id.clone(),
                name: self.names.get(id).cloned().unwrap_or_else(|| id.clone()),
                wrong: ask.wrong,
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
        let volume = self.volume_of(id);
        self.log.log(|| {
            format!(
                "AirPlay speaker {} volume: {position} ({})",
                self.label(id),
                describe_volume(volume)
            )
        });
        if let Some(added) = self.added.get(id) {
            added.set_volume(self.link.as_mut(), volume);
        }
    }

    /// Sets the master position, which scales every speaker, live.
    pub fn set_master(&mut self, master: f64) {
        self.master = master;
        self.send_volumes();
    }

    /// Only speaker `id` is heard, `None`: every speaker is, at its volume
    /// again. Calibrate All measures one AirPlay speaker at a time.
    pub fn set_calibration_solo(&mut self, id: Option<&str>) {
        if self.calibration_solo.as_deref() == id {
            return;
        }
        self.calibration_solo = id.map(str::to_owned);
        self.log.log(|| match id {
            Some(id) => format!(
                "calibration: only AirPlay speaker {} is heard",
                self.label(id)
            ),
            None => "calibration: every AirPlay speaker is heard again".to_owned(),
        });
        self.send_volumes();
    }

    /// The volume speaker `id` gets now: muted while a calibration lets
    /// another speaker alone be heard.
    fn volume_of(&self, id: &str) -> Volume {
        match self.calibration_solo.as_deref() {
            Some(solo) if solo != id => Volume::Muted,
            _ => airplay_volume(self.settings(id).volume, self.master),
        }
    }

    fn send_volumes(&mut self) {
        for (id, added) in &self.added {
            let volume = self.volume_of(id);
            added.set_volume(self.link.as_mut(), volume);
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
        self.send_delay(id);
    }

    /// Holds every speaker back by `hold_back_ms` on top of its own d_s, live,
    /// so they meet a Google Cast speaker whose offset is longer than L (C77).
    pub fn set_hold_back(&mut self, hold_back_ms: f64) {
        let hold_back_ms = if hold_back_ms.is_finite() {
            hold_back_ms.max(0.0)
        } else {
            0.0
        };
        if hold_back_ms == self.hold_back_ms {
            return;
        }
        self.hold_back_ms = hold_back_ms;
        self.log.log(|| {
            format!("AirPlay speakers held back {hold_back_ms:.0} ms for the Google Cast speaker")
        });
        let ids: Vec<String> = self.added.keys().cloned().collect();
        for id in ids {
            self.send_delay(&id);
        }
    }

    /// How far ahead of the hold-back the speaker that falls furthest short
    /// plays, in ms, because the sender caps d_s (P99); 0 while none does.
    pub fn ahead_ms(&self) -> f64 {
        self.added
            .keys()
            .map(|id| {
                self.settings(id).delay_ms + self.hold_back_ms
                    - self.sent_delay(id).as_secs_f64() * 1000.0
            })
            .fold(0.0, f64::max)
    }

    /// d_s plus the hold-back, as the sender plays it.
    fn sent_delay(&self, id: &str) -> Duration {
        speaker_delay(self.settings(id).delay_ms + self.hold_back_ms)
    }

    /// Gives the sender a speaker's delay, if the speaker is in it.
    fn send_delay(&mut self, id: &str) {
        let delay = self.sent_delay(id);
        let short_ms =
            self.settings(id).delay_ms + self.hold_back_ms - delay.as_secs_f64() * 1000.0;
        if short_ms > 0.5 {
            // The sender caps d_s (P99): past that the speaker stays early.
            self.log.log(|| {
                format!(
                    "AirPlay speaker {} plays {short_ms:.0} ms early: its delay stops at {} ms",
                    self.label(id),
                    MAX_DELAY.as_millis()
                )
            });
        }
        if let Some(added) = self.added.get(id) {
            added.set_delay(self.link.as_mut(), delay);
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
    /// with an Apple TV that asks for both.
    fn password(&self, id: &str) -> Option<Secret> {
        self.passwords.get(id).cloned()
    }

    /// The options a speaker joins with.
    fn options(&self, id: &str) -> SpeakerOptions {
        let mut options = SpeakerOptions::new(self.volume_of(id));
        options.delay = self.sent_delay(id);
        options.credentials = self.store.get(id).cloned();
        options.homekit_credentials = self.store.homekit(id).cloned();
        options.password = self.password(id);
        options.pcm = self.pcm_devices.contains(id);
        options
    }

    /// Logs that speaker `id` joins with PCM, from the debug variable.
    fn log_pcm(&self, id: &str) {
        self.log.log(|| {
            format!(
                "AirPlay speaker {}: PCM instead of ALAC ({})",
                self.label(id),
                audiosync_airplay::sender::PCM_DEVICES_ENV
            )
        });
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
                added.remove(self.link.as_mut());
                self.airplay1_outcome.remove(&id);
                self.target_missing.remove(&id);
                if self.gone_targets.remove(&id) {
                    self.targets.remove(&id);
                }
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
                settings.enabled && !self.added.contains_key(*id) && self.can_join(id)
            })
            .map(|(id, _)| id.clone())
            .collect();
        for id in joining {
            let options = self.options(&id);
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
            if options.pcm {
                self.log_pcm(&id);
            }
            let added = match self.targets.get(&id) {
                Some(target) => self.add_target(target.clone(), options),
                None => {
                    let device = self.listed[&id].device();
                    let key = device.key.clone();
                    let counters = self.link.add_speaker(device, options);
                    Added::speaker(key, counters)
                }
            };
            self.added.insert(id, added);
        }
    }

    /// Hands the sender a target to play; each member joins with its own
    /// password and credentials, and the target's delay and volume.
    fn add_target(&mut self, target: Target, options: SpeakerOptions) -> Added {
        let members: BTreeMap<DeviceKey, SpeakerOptions> = target
            .members
            .iter()
            .map(|member| {
                let id = speaker_id(&member.key);
                let mut own = options.clone();
                own.credentials = self.store.get(&id).cloned();
                own.homekit_credentials = self.store.homekit(&id).cloned();
                own.password = self.password(&id);
                own.pcm = self.pcm_devices.contains(&id);
                if own.pcm {
                    self.log_pcm(&id);
                }
                (member.key.clone(), own)
            })
            .collect();
        let counters = self.link.add_target(&target, options, members);
        Added {
            members: target
                .members
                .iter()
                .map(|member| (speaker_id(&member.key), None))
                .collect(),
            joined: Joined::Target(target.id),
            state: SpeakerState::Connecting,
            counters: counters
                .into_iter()
                .map(|(key, counters)| (speaker_id(&key), counters))
                .collect(),
        }
    }

    /// Turns a speaker off because it can't play, and says so.
    fn turn_off(&mut self, id: &str, problem: AirPlayProblem, events: &mut Vec<BackendEvent>) {
        self.drop_password_ask(id, "the speaker was turned off");
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
                    match event {
                        SpeakerEvent::HomeKitPaired(_) => {
                            self.homekit_keys.insert(id.clone(), key);
                        }
                        SpeakerEvent::HomeKitRejected => {
                            self.homekit_keys.remove(&id);
                        }
                        _ => {}
                    }
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
                    if matches!(event, PairingEvent::HomeKitPaired(_)) {
                        self.homekit_keys.insert(id.clone(), key);
                    }
                    changed |= self.pairing_event(&id, event, &mut events);
                }
                // The receivers it listed were reported gone just before;
                // speakers keep playing until Retry replaces the sender.
                SenderEvent::DiscoveryFailed => {
                    self.log.log(|| {
                        "AirPlay: discovery stopped after an internal error; Retry starts a new sender"
                            .to_owned()
                    });
                    self.mark_stopped(&mut events);
                }
                SenderEvent::TargetMissing { id, missing } => {
                    let id = id.to_string();
                    let missing: Vec<String> = missing.iter().map(speaker_id).collect();
                    self.log.log(|| {
                        let names: Vec<String> =
                            missing.iter().map(|member| self.label(member)).collect();
                        format!(
                            "AirPlay target {}: members playing nothing: {}",
                            self.label(&id),
                            if names.is_empty() {
                                "none".to_owned()
                            } else {
                                names.join("; ")
                            }
                        )
                    });
                    changed = true;
                    if missing.is_empty() {
                        self.target_missing.remove(&id);
                    } else {
                        self.target_missing.insert(id, missing);
                    }
                }
            }
        }
        // Events it sent before it stopped were taken in above.
        if !self.link.is_running() {
            changed |= self.sender_stopped(&mut events);
        }
        if let Some(saved) = self.store_change.take() {
            events.push(BackendEvent::PairingStore { saved });
        }
        // A file ignored at launch is reported once kept aside: at the
        // first poll, or a few seconds after it could be read again.
        self.store.keep_aside_if_due();
        self.note_kept_aside();
        if let Some(file) = self.store_kept_aside.take() {
            events.push(BackendEvent::PairingsKeptAside { file });
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
        if self.ending.contains(id) {
            return self.ending_pairing_event(id, event, events);
        }
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
                self.keep_credentials(id, credentials);
                // Credentials no running pairing asked for (a cancelled
                // pairing's come while it is `ending`) leave the speaker
                // off.
                if wanted {
                    self.turn_on_paired(id, events);
                }
                true
            }
            PairingEvent::HomeKitPaired(credentials) => {
                let wanted = self.pairings.remove(id).is_some();
                self.keep_homekit(id, credentials);
                if wanted {
                    self.turn_on_paired(id, events);
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
            // An Apple TV that asks for a password as well: the pairing
            // starts again once the user typed it.
            PairingEvent::Failed { error, .. }
                if matches!(
                    error.kind,
                    ErrorKind::PasswordRequired | ErrorKind::WrongPassword
                ) =>
            {
                if self.pairings.remove(id).is_none() {
                    return false;
                }
                let wrong = self.passwords.remove(id).is_some()
                    || matches!(error.kind, ErrorKind::WrongPassword);
                self.queue_password_ask(id, wrong, true, None);
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
            // A shutdown, already taken in. (The end of a pairing the user
            // cancelled comes while it is `ending`.)
            PairingEvent::Stopped => false,
        }
    }

    /// Takes in what a pairing the user cancelled reported before its end
    /// (see `ending`): nothing reaches the UI or the pairing running now.
    /// The credentials it got are real, so they are kept, while the speaker
    /// stays as the user left it. Its end (it paired, failed or stopped: a
    /// pairing ends once) starts the pairing the user started since, if
    /// any; but when it paired, the Apple TV asks for no PIN any more, so
    /// that pairing never starts on the sender and ends there as paired:
    /// the speaker is turned on ([`BackendEvent::AirPlayPaired`]), as for a
    /// pairing that paired. Returns whether the speaker list changed.
    fn ending_pairing_event(
        &mut self,
        id: &str,
        event: PairingEvent,
        events: &mut Vec<BackendEvent>,
    ) -> bool {
        self.log.log(|| {
            format!(
                "AirPlay pairing {}: the cancelled pairing reported: {}; not shown",
                self.label(id),
                pairing_summary(&event)
            )
        });
        let paired = match event {
            // Its PIN is no one's to type now.
            PairingEvent::PinRequested => return false,
            PairingEvent::Paired(credentials) => {
                self.keep_credentials(id, credentials);
                true
            }
            PairingEvent::HomeKitPaired(credentials) => {
                self.keep_homekit(id, credentials);
                true
            }
            // A wrong PIN starts nothing again: the user cancelled it.
            PairingEvent::Failed { .. } | PairingEvent::Stopped => false,
        };
        self.ending.remove(id);
        if paired {
            if self.pairings.remove(id).is_some() {
                self.log.log(|| {
                    format!(
                        "AirPlay pairing {}: paired by the cancelled pairing, so the one started since asks for no PIN",
                        self.label(id)
                    )
                });
                self.turn_on_paired(id, events);
            }
        } else if let Some(device) = self.pairings.get(id).map(|run| run.device.clone()) {
            self.begin_pairing(id, &device);
        }
        paired
    }

    /// The pairing the user started for the speaker `id` paired: it is
    /// turned on.
    fn turn_on_paired(&mut self, id: &str, events: &mut Vec<BackendEvent>) {
        self.entry(id).enabled = true;
        self.reconcile();
        events.push(BackendEvent::AirPlayPaired { id: id.to_owned() });
    }

    /// Stores the credentials a pairing with the speaker `id` gave.
    fn keep_credentials(&mut self, id: &str, credentials: Credentials) {
        let stored = self.store.insert(id, credentials);
        self.store_written(stored.is_ok());
        match stored {
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
    }

    /// Keeps what the sender learns from an AirPlay 1 attempt that ended in
    /// Unsupported or Refused with a retry: with the switch on, it moves the
    /// speaker to AirPlay 2 at once (P2).
    fn note_path_switch(&mut self, id: &str, kind: ErrorKind, retrying: bool) {
        let moved = retrying
            && self.link.airplay2()
            && matches!(kind, ErrorKind::Unsupported(_) | ErrorKind::Refused)
            && self.listed.get(id).is_some_and(|listed| {
                let device = &listed.info.device;
                device.choose_path(true, None) == Ok(ProtocolPath::AirPlay1)
                    && device.choose_path(true, Some(kind)) == Ok(ProtocolPath::AirPlay2)
            });
        if moved {
            self.airplay1_outcome.insert(id.to_owned(), kind);
        }
    }

    /// Keeps the AirPlay 2 credentials a pairing with the speaker `id` gave
    /// (P152); when the file can't be written they last until AudioSync
    /// quits.
    fn keep_homekit(&mut self, id: &str, credentials: HomeKitCredentials) {
        self.pin_seen.remove(id);
        let saved = self.store.insert_homekit(id, credentials);
        self.store_written(saved.is_ok());
        self.log.log(|| {
            let saved = match &saved {
                Ok(()) => String::new(),
                Err(error) => format!(" for this run only ({error})"),
            };
            format!(
                "AirPlay speaker {}: paired on AirPlay 2, credentials kept{saved}",
                self.label(id)
            )
        });
    }

    fn receiver_event(&mut self, event: ReceiverEvent) {
        match event {
            ReceiverEvent::Appeared(info) | ReceiverEvent::Changed(info) => {
                let id = speaker_id(&info.device.key);
                let availability = self.availability_of(&id, &info);
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
                self.listed.insert(id.clone(), Listed { info: *info });
                if let Some(added) = self.added.get(&id)
                    && matches!(added.state, SpeakerState::Retrying { .. })
                    && self.can_join(&id)
                {
                    let options = self.options(&id);
                    let _ = self.link.add_speaker(self.listed[&id].device(), options);
                }
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
            // Listed only while the AirPlay 2 switch is on, as on the Mac.
            ReceiverEvent::Target(_) if !self.link.airplay2() => {}
            ReceiverEvent::Target(TargetChange::Updated(target)) => {
                let id = target.id.to_string();
                self.names.insert(id.clone(), target.name.clone());
                self.log.log(|| {
                    let kind = match target.id.kind {
                        TargetKind::StereoPair => "stereo pair",
                        TargetKind::HomeTheater => "home theater group",
                    };
                    let members: Vec<String> = target
                        .members
                        .iter()
                        .map(|member| {
                            let seen = if member.in_view() {
                                ""
                            } else {
                                ", out of view"
                            };
                            format!("\"{}\" ({}{seen})", member.name, speaker_id(&member.key))
                        })
                        .collect();
                    let complete = if target.complete() {
                        ""
                    } else {
                        ", incomplete"
                    };
                    format!(
                        "AirPlay {kind} {}: {}{complete}",
                        self.label(&id),
                        members.join(", ")
                    )
                });
                self.gone_targets.remove(&id);
                self.targets.insert(id, *target);
                self.reconcile();
            }
            ReceiverEvent::Target(TargetChange::Gone(id)) => {
                let id = id.to_string();
                // One the sender plays stays until it is switched off, so its
                // members never join twice.
                if self.added.contains_key(&id) {
                    if self.targets.contains_key(&id) && self.gone_targets.insert(id.clone()) {
                        self.log.log(|| {
                            format!(
                                "AirPlay target gone: {}; kept while it plays",
                                self.label(&id)
                            )
                        });
                    }
                } else if self.targets.remove(&id).is_some() {
                    self.log
                        .log(|| format!("AirPlay target gone: {}", self.label(&id)));
                }
            }
        }
    }

    /// The added target whose member is the speaker `id`.
    fn added_target_of(&self, id: &str) -> Option<String> {
        self.added
            .iter()
            .find(|(_, added)| added.members.contains_key(id))
            .map(|(target, _)| target.clone())
    }

    fn log_state(&self, id: &str, state: SpeakerState) {
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
    }

    /// Takes in the new state of an added speaker or target. Returns whether
    /// the speaker list changed.
    fn take_state(
        &mut self,
        id: &str,
        state: SpeakerState,
        events: &mut Vec<BackendEvent>,
    ) -> bool {
        let Some(added) = self.added.get_mut(id) else {
            return false;
        };
        let was_playing = added.state == SpeakerState::Playing;
        added.state = state;
        if state == SpeakerState::Playing {
            // Its password worked: one turned down later turns it off.
            self.password_attempts.remove(id);
        }
        if state == SpeakerState::Playing && !was_playing {
            events.push(BackendEvent::AirPlayPlaying { id: id.to_owned() });
        }
        was_playing != (state == SpeakerState::Playing)
    }

    /// Takes in a state or error of a member of the added target `target`:
    /// the target plays while one member does, and a member's error is the
    /// target's only while no other member plays or connects (P181).
    /// `None` for any other event, which is the member's own.
    fn member_event(
        &mut self,
        target: &str,
        member: &str,
        event: &SpeakerEvent,
        events: &mut Vec<BackendEvent>,
    ) -> Option<bool> {
        match event {
            SpeakerEvent::State(state) => {
                self.log_state(member, *state);
                self.added
                    .get_mut(target)?
                    .members
                    .insert(member.to_owned(), Some(*state));
                Some(self.target_state(target, events))
            }
            SpeakerEvent::Error(SpeakerError {
                error,
                reason,
                retry_at,
            }) => {
                self.note_path_switch(member, error.kind, retry_at.is_some());
                self.log.log(|| {
                    format!(
                        "AirPlay speaker {} of {} error: {error} ({reason:?})",
                        self.label(member),
                        self.label(target)
                    )
                });
                let problem = match problem(*reason) {
                    AirPlayProblem::PlaysThroughSpeakers => AirPlayProblem::AppleTvPlays,
                    AirPlayProblem::PairingNotYet
                        if self.over_airplay2(member) && self.asks_password_only(member) =>
                    {
                        AirPlayProblem::NeedsPassword
                    }
                    problem => problem,
                };
                if problem == AirPlayProblem::NeedsPassword {
                    return Some(self.member_password(target, member, *reason, events));
                }
                let added = self.added.get_mut(target)?;
                added.members.insert(member.to_owned(), None);
                let others_on =
                    added.members.values().flatten().any(|state| {
                        matches!(state, SpeakerState::Playing | SpeakerState::Connecting)
                    });
                if others_on {
                    return Some(self.target_state(target, events));
                }
                if let Some(at) = retry_at
                    && problem.retried()
                {
                    added.state = SpeakerState::Retrying { at: *at };
                    events.push(BackendEvent::AirPlayTrouble {
                        id: target.to_owned(),
                        problem,
                    });
                } else if problem == AirPlayProblem::OneAirPlay2Speaker {
                    events.push(BackendEvent::AirPlayTrouble {
                        id: target.to_owned(),
                        problem,
                    });
                } else {
                    self.turn_off(target, problem, events);
                }
                Some(true)
            }
            _ => None,
        }
    }

    /// Takes the best state of the added target's members as its own.
    /// Returns whether the speaker list changed.
    fn target_state(&mut self, target: &str, events: &mut Vec<BackendEvent>) -> bool {
        let Some(added) = self.added.get(target) else {
            return false;
        };
        let best = added
            .members
            .values()
            .flatten()
            .min_by_key(|state| state_rank(state))
            .copied();
        match best {
            Some(best) if best != added.state => self.take_state(target, best, events),
            _ => false,
        }
    }

    /// A member of the added target `target` asks for a password, or turned
    /// the one typed down. On the user's own switch-on the prompt asks for
    /// it by the member's name while the target waits; otherwise, or once
    /// discovery dropped the target, the target goes off with a notice that
    /// names the member, and switching it on asks. Returns whether the
    /// speaker list changed.
    fn member_password(
        &mut self,
        target: &str,
        member: &str,
        reason: UserReason,
        events: &mut Vec<BackendEvent>,
    ) -> bool {
        let turned_down =
            self.passwords.remove(member).is_some() || reason == UserReason::WrongPassword;
        self.password_seen.insert(member.to_owned());
        if self.password_attempts.contains(target)
            && !self.gone_targets.contains(target)
            && let Some(added) = self.added.remove(target)
        {
            added.remove(self.link.as_mut());
            self.airplay1_outcome.remove(target);
            self.target_missing.remove(target);
            self.queue_password_ask(member, turned_down, false, Some(target));
            return true;
        }
        self.drop_password_ask(target, "its member asks for a password");
        if self.settings(target).enabled {
            self.entry(target).enabled = false;
            self.reconcile();
            events.push(BackendEvent::AirPlayMemberNeedsPassword {
                id: target.to_owned(),
                member: self
                    .names
                    .get(member)
                    .cloned()
                    .unwrap_or_else(|| member.to_owned()),
            });
        }
        true
    }

    /// Whether discovery says the speaker asks for a password and no
    /// pairing.
    fn asks_password_only(&self, id: &str) -> bool {
        self.listed.get(id).is_some_and(|listed| {
            let verdict = &listed.info.device.verdict;
            verdict.needs_password && !verdict.needs_pairing
        })
    }

    /// Returns whether the speaker list changed.
    fn speaker_event(
        &mut self,
        id: &str,
        event: SpeakerEvent,
        events: &mut Vec<BackendEvent>,
    ) -> bool {
        if let Some(target) = self.added_target_of(id)
            && let Some(changed) = self.member_event(&target, id, &event, events)
        {
            return changed;
        }
        match event {
            SpeakerEvent::State(state) => {
                self.log_state(id, state);
                self.take_state(id, state, events)
            }
            SpeakerEvent::Error(SpeakerError {
                error,
                reason,
                retry_at,
            }) => {
                let mut problem = problem(reason);
                self.note_path_switch(id, error.kind, retry_at.is_some());
                // A speaker reached over AirPlay 2 that wants a pairing:
                // switching it on pairs it with a PIN, or with its password
                // when it asks for one (P61, P154).
                if problem == AirPlayProblem::PairingNotYet && self.over_airplay2(id) {
                    if self.asks_password_only(id) {
                        problem = AirPlayProblem::NeedsPassword;
                    } else {
                        problem = AirPlayProblem::NeedsPin;
                        self.pin_seen.insert(id.to_owned());
                    }
                } else if problem == AirPlayProblem::PairingNotYet && self.store.contains(id) {
                    // The Apple TV turned the stored credentials down (it
                    // was reset, or the pairing was removed on it): they are
                    // forgotten, and switching it on pairs it again.
                    problem = AirPlayProblem::NeedsPin;
                    let forgotten = self.store.remove(id);
                    self.store_written(forgotten.is_ok());
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
                // The password it had, if any, is no good: it asks for one
                // even if discovery didn't say so.
                let turned_down = problem == AirPlayProblem::NeedsPassword
                    && (self.passwords.remove(id).is_some() || reason == UserReason::WrongPassword);
                if problem == AirPlayProblem::NeedsPassword {
                    self.password_seen.insert(id.to_owned());
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
                if problem == AirPlayProblem::NeedsPassword
                    && self.password_attempts.contains(id)
                    && let Some(added) = self.added.remove(id)
                {
                    // The user's switch-on asks for it; the speaker stays on
                    // meanwhile.
                    added.remove(self.link.as_mut());
                    self.airplay1_outcome.remove(id);
                    self.queue_password_ask(id, turned_down, false, None);
                    return true;
                }
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
                } else if problem == AirPlayProblem::OneAirPlay2Speaker {
                    // The sender starts it once the other session ends.
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
            SpeakerEvent::HomeKitPaired(credentials) => {
                self.keep_homekit(id, credentials);
                false
            }
            SpeakerEvent::HomeKitRejected => {
                let saved = self.store.remove_homekit(id);
                self.store_written(saved.is_ok());
                self.log.log(|| {
                    let saved = match &saved {
                        Ok(()) => String::new(),
                        Err(error) => format!(" (the file couldn't be written: {error})"),
                    };
                    format!(
                        "AirPlay speaker {}: turned down its AirPlay 2 credentials, forgotten{saved}",
                        self.label(id)
                    )
                });
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
            for (member, counters) in &added.counters {
                if !added.members.is_empty() {
                    text.push_str(&format!("\n    member {}", self.label(member)));
                }
                text.push_str(&format!(
                    "; {} packets sent, {} late, {} resend requests, {} resent, {} timing replies, \
                     {} datagrams from other addresses dropped",
                    counters.packets_sent(),
                    counters.late_packets(),
                    counters.resend_requests(),
                    counters.packets_resent(),
                    counters.timing_replies(),
                    counters.foreign_datagrams(),
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
    use audiosync_airplay::targets::Member;

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

    /// A receiver with only its AirPlay service, which asks for a PIN
    /// pairing (status flag bit 3, P61).
    pub fn airplay2_tv(id: u8, name: &str) -> Device {
        airplay2_device(id, name, true)
    }

    /// A receiver with only its AirPlay service; `pin`: its flags ask for a
    /// PIN pairing.
    pub fn airplay2_device(id: u8, name: &str, pin: bool) -> Device {
        let device_id = DeviceId([0x02, 0x00, 0x5E, 0x00, 0x53, id]);
        let record = ServiceRecord {
            kind: ServiceKind::AirPlay,
            instance: name.to_owned(),
            host: format!("speaker-{id}.local"),
            port: 7000,
            txt: vec![
                format!("deviceid={device_id}").into_bytes(),
                b"features=0x0,0x40".to_vec(),
                if pin {
                    b"flags=0x8".to_vec()
                } else {
                    b"flags=0x0".to_vec()
                },
            ],
            addresses: vec![ScopedAddr::new(IpAddr::V4(Ipv4Addr::new(192, 0, 2, 13)))],
        };
        Device::from_records(DeviceKey::Id(device_id), None, Some(&record)).expect("a device")
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
        /// A target added, with its members' speaker IDs.
        AddTarget {
            id: String,
            volume: Volume,
            delay: Duration,
            members: Vec<String>,
        },
        RemoveTarget(String),
        TargetVolume(String, Volume),
        TargetDelay(String, Duration),
        RestartTimeline,
        NowPlaying(NowPlaying),
        /// The speaker added just before came with credentials of this
        /// client ID.
        Credentials(String, String),
        /// The speaker added just before came with AirPlay 2 credentials.
        HomeKit(String),
        /// The speaker added or the pairing started just before came with
        /// this password.
        Password(String, String),
        StartPairing(String),
        SubmitPin(String, String),
        CancelPairing(String),
        ForgetHomeKit(String),
    }

    #[derive(Default)]
    struct FakeState {
        calls: Vec<Call>,
        events: VecDeque<SenderEvent>,
        latency: Duration,
        /// The sender stopped after an internal error.
        stopped: bool,
        /// The AirPlay 2 switch.
        airplay2: bool,
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

        /// Turns the AirPlay 2 switch on for the links made after.
        pub fn set_airplay2(&self, on: bool) {
            self.0.borrow_mut().airplay2 = on;
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

        /// Discovery found `device`, a member of the target `target`.
        pub fn appear_in(&self, device: &Device, target: &Target) {
            let mut info = ReceiverInfo::new(device.clone());
            info.target = Some(target.id.clone());
            self.push(SenderEvent::Receiver(ReceiverEvent::Appeared(Box::new(
                info,
            ))));
        }

        /// Discovery reported `target`, new or changed.
        pub fn target(&self, target: &Target) {
            self.push(SenderEvent::Receiver(ReceiverEvent::Target(
                TargetChange::Updated(Box::new(target.clone())),
            )));
        }

        /// Discovery no longer reports `target`.
        pub fn target_gone(&self, target: &Target) {
            self.push(SenderEvent::Receiver(ReceiverEvent::Target(
                TargetChange::Gone(target.id.clone()),
            )));
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

        /// The stopped fake runs again on the same link. A real sender never
        /// does (Retry replaces it), but a test then sees what the stop alone
        /// cleared, without `replace_link` clearing it too.
        pub fn run_again(&self) {
            self.0.borrow_mut().stopped = false;
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
            if options.homekit_credentials.is_some() {
                state.calls.push(Call::HomeKit(id_of(device)));
            }
            if let Some(password) = &options.password {
                state
                    .calls
                    .push(Call::Password(id_of(device), password.expose().to_owned()));
            }
            None
        }

        fn remove_speaker(&mut self, key: &DeviceKey) {
            self.0
                .borrow_mut()
                .calls
                .push(Call::Remove(speaker_id(key)));
        }

        fn add_target(
            &mut self,
            target: &Target,
            options: SpeakerOptions,
            members: BTreeMap<DeviceKey, SpeakerOptions>,
        ) -> Vec<(DeviceKey, Arc<SpeakerCounters>)> {
            self.0.borrow_mut().calls.push(Call::AddTarget {
                id: target.id.to_string(),
                volume: options.volume,
                delay: options.delay,
                members: members.keys().map(speaker_id).collect(),
            });
            Vec::new()
        }

        fn remove_target(&mut self, id: &TargetId) {
            self.0
                .borrow_mut()
                .calls
                .push(Call::RemoveTarget(id.to_string()));
        }

        fn set_target_volume(&mut self, id: &TargetId, volume: Volume) {
            self.0
                .borrow_mut()
                .calls
                .push(Call::TargetVolume(id.to_string(), volume));
        }

        fn set_target_delay(&mut self, id: &TargetId, delay: Duration) {
            self.0
                .borrow_mut()
                .calls
                .push(Call::TargetDelay(id.to_string(), delay));
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

        fn set_now_playing(&mut self, now_playing: NowPlaying) -> Result<(), MetadataError> {
            self.0
                .borrow_mut()
                .calls
                .push(Call::NowPlaying(now_playing));
            Ok(())
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

        fn start_pairing(&mut self, device: &Device, password: Option<Secret>) {
            let mut state = self.0.borrow_mut();
            state.calls.push(Call::StartPairing(id_of(device)));
            if let Some(password) = password {
                state
                    .calls
                    .push(Call::Password(id_of(device), password.expose().to_owned()));
            }
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

        fn forget_homekit(&mut self, key: &DeviceKey) {
            self.0
                .borrow_mut()
                .calls
                .push(Call::ForgetHomeKit(speaker_id(key)));
        }

        fn is_running(&self) -> bool {
            !self.0.borrow().stopped
        }

        fn airplay2(&self) -> bool {
            self.0.borrow().airplay2
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

    #[test]
    fn pcm_variable_lists_trimmed_device_ids() {
        use audiosync_airplay::sender::pcm_devices;
        assert!(pcm_devices(None).is_empty());
        assert!(pcm_devices(Some("")).is_empty());
        assert!(pcm_devices(Some(" , ,")).is_empty());
        assert_eq!(
            pcm_devices(Some("raop:Kitchen")),
            BTreeSet::from(["raop:Kitchen".to_owned()])
        );
        assert_eq!(
            pcm_devices(Some(" raop:Kitchen , airplay:Den,,AA:BB:CC:DD:EE:FF ")),
            BTreeSet::from([
                "raop:Kitchen".to_owned(),
                "airplay:Den".to_owned(),
                "AA:BB:CC:DD:EE:FF".to_owned(),
            ])
        );
    }

    #[test]
    fn only_listed_speakers_join_with_pcm() {
        let fake = FakeAirPlay::new();
        let mut airplay = speakers(&fake);
        assert!(!airplay.options("raop:Kitchen").pcm);
        airplay.pcm_devices =
            audiosync_airplay::sender::pcm_devices(Some("raop:Kitchen, airplay:Den"));
        assert!(airplay.options("raop:Kitchen").pcm);
        assert!(airplay.options("airplay:Den").pcm);
        assert!(!airplay.options("raop:Unknown").pcm);
        assert!(!airplay.options("Kitchen").pcm);
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
                ("Study", AirPlayAvailability::NeedsPassword, false),
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
    fn homekit_credentials_a_receiver_rejects_are_deleted_from_the_file() {
        use crate::audio::pairings::FILE_NAME;
        use crate::audio::pairings::tests::{homekit, sealer};

        let path = crate::debug_log::test_dir("airplay2-homekit-rejected").join(FILE_NAME);
        let (mut store, _) = PairingStore::open(path.clone(), sealer());
        store.open_airplay2();
        let tv = device(6, "Den TV", APPLE_TV_TXT);
        let id = id_of(&tv);
        store.insert_homekit(&id, homekit(0x5A)).expect("saved");
        let fake = FakeAirPlay::new();
        let mut airplay = AirPlaySpeakers::new(fake.link(), 100.0, DebugLog::default(), store);
        fake.push(SenderEvent::Speaker {
            key: tv.key.clone(),
            event: SpeakerEvent::HomeKitRejected,
        });
        airplay.poll();
        assert_eq!(airplay.store.homekit(&id), None);
        drop(airplay);

        let (mut store, _) = PairingStore::open(path, sealer());
        store.open_airplay2();
        assert_eq!(store.homekit(&id), None, "gone from the file too");
    }

    #[test]
    fn an_airplay2_speaker_pairs_with_a_pin_only_while_the_switch_is_on() {
        use crate::audio::pairings::tests::homekit;

        let tv = airplay2_tv(9, "Loft");
        let id = id_of(&tv);
        let fake = FakeAirPlay::new();
        let mut airplay = speakers(&fake);
        fake.appear(&tv);
        airplay.poll();
        let row = &airplay.speakers()[0];
        assert_eq!(row.availability, AirPlayAvailability::AirPlay2NotYet);
        assert!(!row.airplay2, "the switch is off");
        assert_eq!(
            airplay.start_pairing(&id),
            Err(AirPlayProblem::AirPlay2NotYet)
        );
        assert!(fake.take_calls().is_empty());

        let fake = FakeAirPlay::new();
        fake.set_airplay2(true);
        let mut airplay = speakers(&fake);
        pin_shown(&fake, &mut airplay, &tv);
        assert!(airplay.speakers()[0].airplay2);
        assert!(airplay.submit_pin(&id, "1111"));
        fake.take_calls();
        fake.pairing(&tv, pairing_failed(ErrorKind::PairingFailed, true));
        airplay.poll();
        assert_eq!(fake.take_calls(), [Call::StartPairing(id.clone())]);
        assert!(airplay.pin_prompt().unwrap().wrong_pin);
        fake.pairing(&tv, PairingEvent::PinRequested);
        airplay.poll();
        assert!(airplay.submit_pin(&id, "4821"));

        fake.pairing(&tv, PairingEvent::HomeKitPaired(homekit(0x33)));
        assert_eq!(
            airplay.poll(),
            [
                BackendEvent::AirPlayChanged,
                BackendEvent::AirPlayPaired { id: id.clone() }
            ]
        );
        assert_eq!(airplay.pin_prompt(), None);
        assert_eq!(airplay.store.homekit(&id), Some(&homekit(0x33)));
        assert_eq!(airplay.store.get(&id), None, "no AirPlay 1 credentials");
        let row = &airplay.speakers()[0];
        assert!(row.paired && row.enabled);
        assert_eq!(row.availability, AirPlayAvailability::Open);

        fake.take_calls();
        airplay.forget_pairing(&id);
        assert_eq!(airplay.store.homekit(&id), None);
        assert!(
            fake.take_calls().contains(&Call::ForgetHomeKit(id.clone())),
            "the sender forgets its copy too"
        );
        assert_eq!(
            airplay.speakers()[0].availability,
            AirPlayAvailability::NeedsPin
        );
    }

    #[test]
    fn forgetting_an_airplay2_pairing_reaches_the_senders_copy_once_the_speaker_is_gone() {
        use crate::audio::pairings::tests::homekit;

        let tv = airplay2_tv(9, "Loft");
        let id = id_of(&tv);
        let fake = FakeAirPlay::new();
        fake.set_airplay2(true);
        let mut airplay = speakers(&fake);
        pin_shown(&fake, &mut airplay, &tv);
        assert!(airplay.submit_pin(&id, "4821"));
        fake.pairing(&tv, PairingEvent::HomeKitPaired(homekit(0x33)));
        airplay.poll();
        assert_eq!(airplay.set_enabled(&id, false), Ok(()));
        fake.gone(&tv);
        airplay.poll();
        assert!(!airplay.listed.contains_key(&id) && !airplay.added.contains_key(&id));

        fake.take_calls();
        airplay.forget_pairing(&id);
        assert_eq!(airplay.store.homekit(&id), None);
        assert!(
            fake.take_calls().contains(&Call::ForgetHomeKit(id.clone())),
            "the sender forgets its copy too"
        );
    }

    #[test]
    fn forgetting_a_session_paired_speaker_reaches_only_the_sender_that_paired_it() {
        use crate::audio::pairings::tests::homekit;

        let tv = airplay2_tv(9, "Loft");
        let id = id_of(&tv);
        let paired = |fake: &FakeAirPlay| {
            fake.set_airplay2(true);
            let mut airplay = speakers(fake);
            fake.push(SenderEvent::Speaker {
                key: tv.key.clone(),
                event: SpeakerEvent::HomeKitPaired(homekit(0x33)),
            });
            airplay.poll();
            fake.take_calls();
            airplay
        };

        let fake = FakeAirPlay::new();
        let mut airplay = paired(&fake);
        airplay.forget_pairing(&id);
        assert!(fake.take_calls().contains(&Call::ForgetHomeKit(id.clone())));

        let fake = FakeAirPlay::new();
        let mut airplay = paired(&fake);
        let second = FakeAirPlay::new();
        airplay.replace_link(second.link());
        second.take_calls();
        airplay.forget_pairing(&id);
        assert_eq!(airplay.store.homekit(&id), None);
        assert!(
            !second
                .take_calls()
                .contains(&Call::ForgetHomeKit(id.clone())),
            "a new sender never had the copy"
        );

        let fake = FakeAirPlay::new();
        let mut airplay = paired(&fake);
        fake.stop_running();
        airplay.poll();
        fake.take_calls();
        airplay.forget_pairing(&id);
        assert!(
            !fake.take_calls().contains(&Call::ForgetHomeKit(id.clone())),
            "a stopped sender's copies are gone"
        );
    }

    #[test]
    fn a_receiver_rejecting_its_airplay2_credentials_drops_the_senders_key() {
        use crate::audio::pairings::tests::homekit;

        let tv = airplay2_tv(9, "Loft");
        let id = id_of(&tv);
        let fake = FakeAirPlay::new();
        fake.set_airplay2(true);
        let mut airplay = speakers(&fake);
        fake.push(SenderEvent::Speaker {
            key: tv.key.clone(),
            event: SpeakerEvent::HomeKitPaired(homekit(0x33)),
        });
        airplay.poll();
        assert!(airplay.homekit_keys.contains_key(&id));
        fake.push(SenderEvent::Speaker {
            key: tv.key.clone(),
            event: SpeakerEvent::HomeKitRejected,
        });
        airplay.poll();
        assert!(!airplay.homekit_keys.contains_key(&id));
        assert_eq!(airplay.store.homekit(&id), None);
    }

    #[test]
    fn a_forgotten_airplay2_pairing_the_file_cant_take_is_reported() {
        use crate::audio::pairings::tests::{homekit, sealer};
        use crate::audio::pairings::{AIRPLAY2_FILE_NAME, FILE_NAME};

        let folder = crate::debug_log::test_dir("airplay2-forget-unsaved");
        let fake = FakeAirPlay::new();
        fake.set_airplay2(true);
        let (store, _) = PairingStore::open(folder.join(FILE_NAME), sealer());
        let mut airplay = AirPlaySpeakers::new(fake.link(), 100.0, DebugLog::default(), store);
        let tv = airplay2_tv(9, "Loft");
        let id = id_of(&tv);
        pin_shown(&fake, &mut airplay, &tv);
        assert!(airplay.submit_pin(&id, "4821"));
        fake.pairing(&tv, PairingEvent::HomeKitPaired(homekit(0x44)));
        let events = airplay.poll();
        assert!(
            !events.contains(&BackendEvent::PairingStore { saved: false }),
            "{events:?}"
        );
        let file = folder.join(AIRPLAY2_FILE_NAME);
        assert!(file.exists(), "written at once");

        // A folder where the file goes: nothing can be written there.
        std::fs::remove_file(&file).expect("the file is removed");
        std::fs::create_dir(&file).expect("the folder is made");
        airplay.forget_pairing(&id);
        let events = airplay.poll();
        assert!(
            events.contains(&BackendEvent::PairingStore { saved: false }),
            "{events:?}"
        );
        assert_eq!(airplay.store.homekit(&id), None, "forgotten all the same");
    }

    #[test]
    fn a_speaker_airplay1_refuses_follows_the_sender_to_airplay2() {
        let fake = FakeAirPlay::new();
        fake.set_airplay2(true);
        let mut airplay = speakers(&fake);
        let both = den(true);
        let id = id_of(&both);
        fake.appear(&both);
        airplay.poll();
        assert!(!airplay.speakers()[0].airplay2, "AirPlay 1 first");
        assert_eq!(airplay.set_enabled(&id, true), Ok(()));
        airplay.set_streaming(true);

        // Refused on AirPlay 1: the sender moves it to AirPlay 2 at once.
        fake.fail(&both, ErrorKind::Refused);
        airplay.poll();
        assert!(airplay.speakers()[0].airplay2);
        assert_eq!(
            airplay.speakers()[0].availability,
            AirPlayAvailability::Open
        );

        // Off, the sender forgets the refusal and tries AirPlay 1 again.
        assert_eq!(airplay.set_enabled(&id, false), Ok(()));
        assert!(!airplay.speakers()[0].airplay2);

        // With the switch off, a refusal moves nothing.
        let fake = FakeAirPlay::new();
        let mut airplay = speakers(&fake);
        fake.appear(&both);
        airplay.poll();
        assert_eq!(airplay.set_enabled(&id, true), Ok(()));
        airplay.set_streaming(true);
        fake.fail(&both, ErrorKind::Refused);
        airplay.poll();
        assert!(!airplay.speakers()[0].airplay2);
    }

    #[test]
    fn an_airplay2_session_that_asks_for_a_pairing_turns_into_a_pin_request() {
        let fake = FakeAirPlay::new();
        fake.set_airplay2(true);
        let mut airplay = speakers(&fake);
        // Its flags announce no pairing: the session finds out (P61).
        let tv = airplay2_device(9, "Loft", false);
        let id = id_of(&tv);
        fake.appear(&tv);
        airplay.poll();
        assert_eq!(
            airplay.speakers()[0].availability,
            AirPlayAvailability::Open
        );
        assert_eq!(airplay.set_enabled(&id, true), Ok(()));
        fake.fail(&tv, ErrorKind::PairingRequired);
        airplay.poll();
        assert_eq!(
            airplay.speakers()[0].availability,
            AirPlayAvailability::NeedsPin
        );
        airplay.start_pairing(&id).expect("the pairing starts");
        assert!(fake.take_calls().contains(&Call::StartPairing(id)));
    }

    #[test]
    fn a_pairing_the_store_cant_write_is_reported_once_until_a_write_works() {
        use crate::audio::pairings::FILE_NAME;
        use crate::audio::pairings::tests::{credentials, sealer};

        // A folder where the file goes: nothing can be written there.
        let path = crate::debug_log::test_dir("airplay-pairing-unsaved").join(FILE_NAME);
        std::fs::create_dir(&path).expect("the folder is made");
        let fake = FakeAirPlay::new();
        let (store, _) = PairingStore::open(path.clone(), sealer());
        let mut airplay = AirPlaySpeakers::new(fake.link(), 100.0, DebugLog::default(), store);
        let tv = device(6, "Den TV", APPLE_TV_TXT);
        let id = id_of(&tv);
        pin_shown(&fake, &mut airplay, &tv);
        assert!(airplay.submit_pin(&id, "4821"));
        fake.pairing(
            &tv,
            PairingEvent::Paired(credentials("0123456789ABCDEF", 0xA5)),
        );
        let events = airplay.poll();
        assert!(
            events.contains(&BackendEvent::PairingStore { saved: false }),
            "{events:?}"
        );
        assert_eq!(
            airplay.enabled_ids(),
            std::slice::from_ref(&id),
            "paired for this run all the same"
        );
        // Told once while the writes keep failing.
        airplay.forget_pairing(&id);
        let events = airplay.poll();
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, BackendEvent::PairingStore { .. })),
            "{events:?}"
        );
        // Writable again: the next write works, and that is told too.
        std::fs::remove_dir(&path).expect("the folder is removed");
        fake.take_calls();
        pin_shown(&fake, &mut airplay, &tv);
        assert!(airplay.submit_pin(&id, "1234"));
        fake.pairing(
            &tv,
            PairingEvent::Paired(credentials("0123456789ABCDEE", 0x5A)),
        );
        let events = airplay.poll();
        assert!(
            events.contains(&BackendEvent::PairingStore { saved: true }),
            "{events:?}"
        );
        assert!(path.is_file(), "the pairing is written");
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

    /// An Apple TV listed and switched on by the user, its pairing started
    /// but no PIN shown yet.
    fn pairing_started(fake: &FakeAirPlay, airplay: &mut AirPlaySpeakers, tv: &Device) {
        fake.appear(tv);
        airplay.poll();
        let id = id_of(tv);
        assert_eq!(
            airplay.set_enabled(&id, true),
            Err(AirPlayProblem::NeedsPin)
        );
        airplay.start_pairing(&id).expect("the pairing starts");
        assert_eq!(fake.take_calls(), [Call::StartPairing(id)]);
    }

    /// The user cancels the Apple TV's pairing and starts it again at once,
    /// before the cancelled pairing reported its end. The sender's pairing
    /// events name the receiver only, so whatever it reports next for the
    /// Apple TV may still be the cancelled pairing's.
    fn cancel_and_start_again(fake: &FakeAirPlay, airplay: &mut AirPlaySpeakers, tv: &Device) {
        let id = id_of(tv);
        airplay.cancel_pairing(&id);
        airplay
            .start_pairing(&id)
            .expect("the new pairing is taken");
        assert_eq!(
            fake.take_calls().first(),
            Some(&Call::CancelPairing(id)),
            "the cancelled pairing is stopped first"
        );
        assert!(airplay.speakers()[0].pairing);
        assert_eq!(airplay.pin_prompt(), None, "the new pairing showed no PIN");
    }

    /// The pairing started after a cancel goes on as its own: it starts on
    /// the sender only once the cancelled one has ended, and then asks for
    /// its own PIN and takes it.
    fn the_new_pairing_goes_on(fake: &FakeAirPlay, airplay: &mut AirPlaySpeakers, tv: &Device) {
        let id = id_of(tv);
        assert_eq!(
            fake.take_calls(),
            [Call::StartPairing(id.clone())],
            "the new pairing starts once the cancelled one has ended"
        );
        assert!(airplay.speakers()[0].pairing, "the new pairing is kept");
        assert_eq!(airplay.pin_prompt(), None);
        assert!(!airplay.submit_pin(&id, "4821"), "no PIN asked for yet");
        fake.pairing(tv, PairingEvent::PinRequested);
        assert_eq!(airplay.poll(), [BackendEvent::AirPlayChanged]);
        let prompt = airplay.pin_prompt().expect("its own PIN shows");
        assert_eq!(prompt.phase, PairingPhase::WaitingForPin);
        assert!(!prompt.wrong_pin);
        assert!(airplay.submit_pin(&id, "4821"));
        assert_eq!(fake.take_calls(), [Call::SubmitPin(id, "4821".to_owned())]);
    }

    #[test]
    fn a_cancelled_pairings_late_failure_leaves_the_next_pairing_alone() {
        let fake = FakeAirPlay::new();
        let mut airplay = speakers(&fake);
        let tv = device(6, "Den TV", APPLE_TV_TXT);
        pin_shown(&fake, &mut airplay, &tv);
        cancel_and_start_again(&fake, &mut airplay, &tv);
        // The cancelled pairing failed before the cancel reached it.
        fake.pairing(&tv, pairing_failed(ErrorKind::PairingCancelled, false));
        assert!(
            airplay.poll().is_empty(),
            "no notice for a pairing the user cancelled"
        );
        assert!(airplay.enabled_ids().is_empty(), "off until paired");
        the_new_pairing_goes_on(&fake, &mut airplay, &tv);
    }

    #[test]
    fn a_cancelled_pairings_late_pin_request_shows_no_prompt() {
        let fake = FakeAirPlay::new();
        let mut airplay = speakers(&fake);
        let tv = device(6, "Den TV", APPLE_TV_TXT);
        let id = id_of(&tv);
        pairing_started(&fake, &mut airplay, &tv);
        cancel_and_start_again(&fake, &mut airplay, &tv);
        // The cancelled pairing's PIN request was already on its way.
        fake.pairing(&tv, PairingEvent::PinRequested);
        assert!(airplay.poll().is_empty());
        assert_eq!(
            airplay.pin_prompt(),
            None,
            "no prompt for the PIN the cancelled pairing asked for"
        );
        assert!(
            !airplay.submit_pin(&id, "4821"),
            "a PIN typed now would go to the cancelled pairing"
        );
        fake.pairing(&tv, PairingEvent::Stopped);
        assert!(airplay.poll().is_empty());
        the_new_pairing_goes_on(&fake, &mut airplay, &tv);
    }

    #[test]
    fn a_cancelled_pairings_late_success_pairs_the_next_pairing_without_a_pin() {
        let fake = FakeAirPlay::new();
        let mut airplay = speakers(&fake);
        let tv = device(6, "Den TV", APPLE_TV_TXT);
        let id = id_of(&tv);
        pin_shown(&fake, &mut airplay, &tv);
        assert!(airplay.submit_pin(&id, "4821"));
        fake.take_calls();
        cancel_and_start_again(&fake, &mut airplay, &tv);
        // The Apple TV took the PIN before the cancel reached the pairing.
        fake.pairing(
            &tv,
            PairingEvent::Paired(crate::audio::pairings::tests::credentials(
                "0123456789ABCDEF",
                1,
            )),
        );
        // The Apple TV is paired now: the pairing started since ends there,
        // as a pairing that paired, instead of asking for a second PIN.
        assert_eq!(
            airplay.poll(),
            [
                BackendEvent::AirPlayChanged,
                BackendEvent::AirPlayPaired { id: id.clone() }
            ]
        );
        assert!(
            !fake.take_calls().contains(&Call::StartPairing(id.clone())),
            "no second pairing starts on the sender"
        );
        assert_eq!(airplay.pin_prompt(), None, "no second PIN shows");
        assert!(!airplay.submit_pin(&id, "4821"), "no pairing runs");
        assert_eq!(
            airplay.enabled_ids(),
            std::slice::from_ref(&id),
            "paired, it is on"
        );
        let row = &airplay.speakers()[0];
        assert!(row.paired && !row.pairing);
        assert_eq!(row.availability, AirPlayAvailability::Open);
    }

    #[test]
    fn a_cancelled_pairings_late_success_with_nothing_waiting_leaves_the_speaker_off() {
        let fake = FakeAirPlay::new();
        let mut airplay = speakers(&fake);
        let tv = device(6, "Den TV", APPLE_TV_TXT);
        let id = id_of(&tv);
        pairing_started(&fake, &mut airplay, &tv);
        cancel_and_start_again(&fake, &mut airplay, &tv);
        // The pairing started since is cancelled too, before it started.
        airplay.cancel_pairing(&id);
        fake.take_calls();
        fake.pairing(
            &tv,
            PairingEvent::Paired(crate::audio::pairings::tests::credentials(
                "0123456789ABCDEF",
                1,
            )),
        );
        assert_eq!(
            airplay.poll(),
            [BackendEvent::AirPlayChanged],
            "the credentials are stored, but the user cancelled both pairings"
        );
        assert!(airplay.speakers()[0].paired, "its credentials are real");
        assert!(airplay.enabled_ids().is_empty());
        assert!(fake.take_calls().is_empty(), "nothing starts");
    }

    #[test]
    fn a_cancelled_pairings_late_wrong_pin_starts_nothing_again() {
        let fake = FakeAirPlay::new();
        let mut airplay = speakers(&fake);
        let tv = device(6, "Den TV", APPLE_TV_TXT);
        let id = id_of(&tv);
        pin_shown(&fake, &mut airplay, &tv);
        assert!(airplay.submit_pin(&id, "1111"));
        fake.take_calls();
        cancel_and_start_again(&fake, &mut airplay, &tv);
        fake.pairing(&tv, pairing_failed(ErrorKind::PairingFailed, true));
        assert!(
            airplay.poll().is_empty(),
            "the wrong PIN was the cancelled pairing's"
        );
        the_new_pairing_goes_on(&fake, &mut airplay, &tv);
    }

    #[test]
    fn a_pairing_cancelled_before_it_started_never_starts() {
        let fake = FakeAirPlay::new();
        let mut airplay = speakers(&fake);
        let tv = device(6, "Den TV", APPLE_TV_TXT);
        let id = id_of(&tv);
        pairing_started(&fake, &mut airplay, &tv);
        cancel_and_start_again(&fake, &mut airplay, &tv);
        // Cancelled again while it waits for the first one's end.
        airplay.cancel_pairing(&id);
        assert!(
            fake.take_calls().is_empty(),
            "nothing to cancel on the sender: it never started there"
        );
        assert!(!airplay.speakers()[0].pairing);
        fake.pairing(&tv, PairingEvent::Stopped);
        assert!(airplay.poll().is_empty());
        assert!(fake.take_calls().is_empty(), "it never starts");

        // With the cancelled pairing ended, a new one starts at once.
        airplay.start_pairing(&id).unwrap();
        assert_eq!(fake.take_calls(), [Call::StartPairing(id)]);
    }

    #[test]
    fn a_new_sender_gets_what_is_playing_from_the_last() {
        let fake = FakeAirPlay::new();
        let mut airplay = speakers(&fake);
        let now_playing = NowPlaying {
            track: Some(audiosync_airplay::metadata::TrackInfo {
                title: "Song".to_owned(),
                artist: "Artist".to_owned(),
                album: "Album".to_owned(),
            }),
            artwork: Some(b"cover".to_vec()),
            progress: None,
        };
        airplay.set_now_playing(now_playing.clone());
        assert_eq!(fake.take_calls(), [Call::NowPlaying(now_playing.clone())]);

        let second = FakeAirPlay::new();
        airplay.replace_link(second.link());
        assert_eq!(second.take_calls(), [Call::NowPlaying(now_playing)]);
        assert!(fake.take_calls().is_empty());
    }

    #[test]
    fn a_new_sender_starts_a_pairing_at_once_after_a_cancel() {
        let fake = FakeAirPlay::new();
        let mut airplay = speakers(&fake);
        let tv = device(6, "Den TV", APPLE_TV_TXT);
        let id = id_of(&tv);
        pairing_started(&fake, &mut airplay, &tv);
        airplay.cancel_pairing(&id);
        // The old sender, and the end it owed, are gone with it.
        let fake = FakeAirPlay::new();
        airplay.replace_link(fake.link());
        fake.appear(&tv);
        airplay.poll();
        airplay.start_pairing(&id).unwrap();
        assert_eq!(fake.take_calls(), [Call::StartPairing(id.clone())]);
        fake.pairing(&tv, PairingEvent::PinRequested);
        airplay.poll();
        assert_eq!(
            airplay.pin_prompt().map(|prompt| prompt.phase),
            Some(PairingPhase::WaitingForPin)
        );
    }

    #[test]
    fn a_pairing_starts_at_once_after_the_sender_stopped_during_a_cancel() {
        let fake = FakeAirPlay::new();
        let mut airplay = speakers(&fake);
        let tv = device(6, "Den TV", APPLE_TV_TXT);
        let id = id_of(&tv);
        pairing_started(&fake, &mut airplay, &tv);
        airplay.cancel_pairing(&id);
        airplay.start_pairing(&id).unwrap();
        assert_eq!(
            fake.take_calls(),
            [Call::CancelPairing(id.clone())],
            "the new pairing waits for the cancelled one's end"
        );
        assert!(airplay.speakers()[0].pairing);
        // The sender stops before the cancelled pairing reported its end:
        // that end never comes, and the waiting pairing is gone with it.
        fake.stop_running();
        assert_eq!(
            airplay.poll(),
            [BackendEvent::AirPlayChanged, BackendEvent::AirPlayStopped]
        );
        assert!(fake.take_calls().is_empty(), "nothing starts on a stop");
        assert_eq!(airplay.pin_prompt(), None);

        // On the same link, with no `replace_link` to clear it as well,
        // nothing of the cancelled pairing is left waiting.
        fake.run_again();
        fake.appear(&tv);
        airplay.poll();
        assert!(!airplay.speakers()[0].pairing, "no pairing stays parked");
        airplay.start_pairing(&id).unwrap();
        assert_eq!(
            fake.take_calls(),
            [Call::StartPairing(id.clone())],
            "it starts at once"
        );
        fake.pairing(&tv, PairingEvent::PinRequested);
        assert_eq!(airplay.poll(), [BackendEvent::AirPlayChanged]);
        assert_eq!(
            airplay.pin_prompt().map(|prompt| prompt.phase),
            Some(PairingPhase::WaitingForPin),
            "its PIN request is its own, not the cancelled pairing's"
        );
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
            Err(AirPlayProblem::NeedsPassword)
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
        airplay.set_delay(&id, 25_000.0);
        airplay.set_volume(&id, 0.0);
        assert_eq!(
            fake.take_calls(),
            [
                Call::Volume(id.clone(), Volume::Db(-15.0)),
                Call::Volume(id.clone(), Volume::Db(-22.5)),
                Call::Delay(id.clone(), Duration::from_millis(120)),
                Call::Delay(id.clone(), Duration::from_millis(20_000)),
                Call::Volume(id.clone(), Volume::Muted),
            ]
        );
        assert_eq!(airplay.speakers()[0].delay_ms, 20_000.0);
    }

    #[test]
    fn a_delay_up_to_twenty_seconds_reaches_the_sender_in_full() {
        // Calibrate All gives an AirPlay row up to the core's 20 s (P99).
        assert_eq!(
            MAX_DELAY.as_secs_f64() * 1000.0,
            audiosync_core::policy::delay::MAX_DELAY_MS
        );
        let fake = FakeAirPlay::new();
        let mut airplay = speakers(&fake);
        let kitchen = device(1, "Kitchen", OPEN_TXT);
        let id = id_of(&kitchen);
        fake.appear(&kitchen);
        airplay.poll();
        airplay.set_enabled(&id, true).unwrap();
        airplay.set_streaming(true);
        fake.take_calls();
        airplay.set_delay(&id, 13_000.0);
        airplay.set_hold_back(4000.0);
        airplay.set_hold_back(9000.0);
        assert_eq!(
            fake.take_calls(),
            [
                Call::Delay(id.clone(), Duration::from_millis(13_000)),
                Call::Delay(id.clone(), Duration::from_millis(17_000)),
                Call::Delay(id.clone(), Duration::from_millis(20_000)),
            ]
        );
        assert_eq!(airplay.speakers()[0].delay_ms, 13_000.0);
    }

    #[test]
    fn a_calibration_solo_mutes_the_other_speakers_until_it_ends() {
        let fake = FakeAirPlay::new();
        let mut airplay = speakers(&fake);
        let kitchen = device(1, "Kitchen", OPEN_TXT);
        let study = device(2, "Study", OPEN_TXT);
        let (k, s) = (id_of(&kitchen), id_of(&study));
        fake.appear(&kitchen);
        fake.appear(&study);
        airplay.poll();
        airplay.set_enabled(&k, true).unwrap();
        airplay.set_enabled(&s, true).unwrap();
        airplay.set_streaming(true);
        airplay.set_volume(&s, 50.0);
        fake.take_calls();
        airplay.set_calibration_solo(Some(&k));
        let calls = fake.take_calls();
        assert!(calls.contains(&Call::Volume(s.clone(), Volume::Muted)));
        assert!(!calls.contains(&Call::Volume(k.clone(), Volume::Muted)));
        // The user's slider moves while muted: kept, still muted.
        airplay.set_volume(&s, 100.0);
        assert_eq!(fake.take_calls(), [Call::Volume(s.clone(), Volume::Muted)]);
        airplay.set_calibration_solo(None);
        let calls = fake.take_calls();
        assert!(calls.contains(&Call::Volume(s.clone(), airplay_volume(100.0, 100.0))));
        assert!(
            !calls
                .iter()
                .any(|call| matches!(call, Call::Volume(_, Volume::Muted)))
        );
    }

    #[test]
    fn the_cast_hold_back_comes_on_top_of_each_speakers_own_delay() {
        let fake = FakeAirPlay::new();
        let mut airplay = speakers(&fake);
        let kitchen = device(1, "Kitchen", OPEN_TXT);
        let id = id_of(&kitchen);
        fake.appear(&kitchen);
        airplay.poll();
        airplay.set_hold_back(1000.0);
        airplay.set_enabled(&id, true).unwrap();
        airplay.set_streaming(true);
        assert!(fake.take_calls().iter().any(|call| matches!(
            call,
            Call::Add { delay, .. } if *delay == Duration::from_millis(1000)
        )));
        airplay.set_delay(&id, 200.0);
        airplay.set_hold_back(1250.0);
        airplay.set_hold_back(1250.0);
        airplay.set_hold_back(5000.0);
        airplay.set_hold_back(f64::NAN);
        assert_eq!(
            fake.take_calls(),
            [
                Call::Delay(id.clone(), Duration::from_millis(1200)),
                Call::Delay(id.clone(), Duration::from_millis(1450)),
                Call::Delay(id.clone(), Duration::from_millis(5200)),
                Call::Delay(id.clone(), Duration::from_millis(200)),
            ]
        );
        assert_eq!(airplay.speakers()[0].delay_ms, 200.0, "the user's own d_s");
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
                    problem: AirPlayProblem::NeedsPassword
                }
            ]
        );
        assert!(airplay.enabled_ids().is_empty(), "turned off");
        assert_eq!(fake.take_calls(), [Call::Remove(id.clone())]);
        assert_eq!(
            airplay.set_enabled(&id, true),
            Err(AirPlayProblem::NeedsPassword),
            "it asks for a password from then on"
        );
    }

    #[test]
    fn a_speaker_waiting_for_the_airplay2_slot_stays_on_and_one_without_ptp_is_off() {
        let fake = FakeAirPlay::new();
        let mut airplay = speakers(&fake);
        let kitchen = device(1, "Kitchen", OPEN_TXT);
        let id = id_of(&kitchen);
        fake.appear(&kitchen);
        airplay.poll();
        airplay.set_enabled(&id, true).unwrap();
        airplay.set_streaming(true);
        airplay.poll();
        fake.fail(
            &kitchen,
            ErrorKind::Unsupported(UnsupportedReason::OneAirPlay2Session),
        );
        assert_eq!(
            airplay.poll(),
            [
                BackendEvent::AirPlayChanged,
                BackendEvent::AirPlayTrouble {
                    id: id.clone(),
                    problem: AirPlayProblem::OneAirPlay2Speaker
                }
            ]
        );
        assert_eq!(airplay.enabled_ids(), std::slice::from_ref(&id));
        fake.take_calls();
        fake.fail(&kitchen, ErrorKind::Unsupported(UnsupportedReason::Timing));
        assert_eq!(
            airplay.poll(),
            [
                BackendEvent::AirPlayChanged,
                BackendEvent::AirPlayFailed {
                    id: id.clone(),
                    problem: AirPlayProblem::TimingUnavailable
                }
            ]
        );
        assert!(airplay.enabled_ids().is_empty());
    }

    /// The password receiver Study, listed while streaming, with its
    /// password prompt open after the user switched it on.
    fn password_asked(fake: &FakeAirPlay, airplay: &mut AirPlaySpeakers) -> Device {
        let study = device(2, "Study", PASSWORD_TXT);
        let id = id_of(&study);
        fake.appear(&study);
        airplay.poll();
        airplay.set_streaming(true);
        assert_eq!(
            airplay.set_enabled(&id, true),
            Err(AirPlayProblem::NeedsPassword)
        );
        airplay.ask_password(&id).expect("the prompt opens");
        assert_eq!(
            airplay.password_prompt(),
            Some(AirPlayPasswordPrompt {
                id,
                name: "Study".to_owned(),
                wrong: false,
            })
        );
        assert!(fake.take_calls().is_empty(), "nothing joins before it");
        study
    }

    fn joined_with(id: &str, password: &str) -> [Call; 2] {
        [
            Call::Add {
                id: id.to_owned(),
                volume: airplay_volume(DEFAULT_LOCAL_POSITION, 100.0),
                delay: Duration::ZERO,
            },
            Call::Password(id.to_owned(), password.to_owned()),
        ]
    }

    #[test]
    fn a_password_typed_switches_the_speaker_on_and_is_kept_until_quit() {
        let fake = FakeAirPlay::new();
        let mut airplay = speakers(&fake);
        let study = password_asked(&fake, &mut airplay);
        let id = id_of(&study);
        assert!(
            !airplay.submit_password(&id, ""),
            "an empty one isn't taken"
        );
        assert!(airplay.submit_password(&id, "hunter2"));
        assert_eq!(airplay.password_prompt(), None);
        assert_eq!(airplay.enabled_ids(), std::slice::from_ref(&id));
        assert_eq!(fake.take_calls(), joined_with(&id, "hunter2"));
        assert!(airplay.chosen());
        assert_eq!(
            airplay.speakers()[0].availability,
            AirPlayAvailability::Open
        );
        assert!(!airplay.submit_password(&id, "again"), "no prompt waits");

        airplay.set_enabled(&id, false).unwrap();
        fake.take_calls();
        airplay
            .set_enabled(&id, true)
            .expect("no prompt the second time");
        assert_eq!(fake.take_calls(), joined_with(&id, "hunter2"));
    }

    #[test]
    fn a_wrong_password_asks_again_and_cancel_turns_the_speaker_off() {
        let fake = FakeAirPlay::new();
        let mut airplay = speakers(&fake);
        let study = password_asked(&fake, &mut airplay);
        let id = id_of(&study);
        assert!(airplay.submit_password(&id, "hunter1"));
        fake.take_calls();
        fake.fail(&study, ErrorKind::WrongPassword);
        assert_eq!(airplay.poll(), [BackendEvent::AirPlayChanged], "no notice");
        assert_eq!(fake.take_calls(), [Call::Remove(id.clone())]);
        assert_eq!(
            airplay.password_prompt().map(|prompt| prompt.wrong),
            Some(true)
        );
        assert_eq!(
            airplay.enabled_ids(),
            std::slice::from_ref(&id),
            "on while it asks"
        );
        assert!(!airplay.chosen(), "local outputs don't wait for it");

        assert!(airplay.submit_password(&id, "hunter2"));
        assert_eq!(fake.take_calls(), joined_with(&id, "hunter2"));
        fake.fail(&study, ErrorKind::WrongPassword);
        airplay.poll();
        fake.take_calls();
        airplay.cancel_password(&id);
        assert_eq!(airplay.password_prompt(), None);
        assert!(airplay.enabled_ids().is_empty(), "cancel turns it off");
        assert!(fake.take_calls().is_empty(), "it had left already");
        assert_eq!(
            airplay.set_enabled(&id, true),
            Err(AirPlayProblem::NeedsPassword),
            "the password turned down is forgotten"
        );
    }

    #[test]
    fn a_password_turned_down_after_playing_turns_the_speaker_off() {
        let fake = FakeAirPlay::new();
        let mut airplay = speakers(&fake);
        let study = password_asked(&fake, &mut airplay);
        let id = id_of(&study);
        assert!(airplay.submit_password(&id, "hunter2"));
        fake.state(&study, SpeakerState::Playing);
        airplay.poll();
        fake.fail(&study, ErrorKind::WrongPassword);
        assert_eq!(
            airplay.poll(),
            [
                BackendEvent::AirPlayChanged,
                BackendEvent::AirPlayFailed {
                    id: id.clone(),
                    problem: AirPlayProblem::NeedsPassword
                }
            ]
        );
        assert_eq!(airplay.password_prompt(), None);
        assert!(airplay.enabled_ids().is_empty());
    }

    #[test]
    fn a_password_discovery_didnt_announce_prompts_at_once_on_the_users_switch_on() {
        let fake = FakeAirPlay::new();
        let mut airplay = speakers(&fake);
        let kitchen = device(1, "Kitchen", OPEN_TXT);
        let id = id_of(&kitchen);
        fake.appear(&kitchen);
        airplay.poll();
        airplay.set_streaming(true);
        airplay.set_enabled(&id, true).unwrap();
        airplay.begin_attempt(&id);
        fake.take_calls();
        fake.fail(&kitchen, ErrorKind::PasswordRequired);
        assert_eq!(airplay.poll(), [BackendEvent::AirPlayChanged], "no notice");
        assert_eq!(fake.take_calls(), [Call::Remove(id.clone())]);
        assert_eq!(
            airplay.password_prompt().map(|prompt| prompt.wrong),
            Some(false),
            "asked for the first time"
        );
        assert_eq!(airplay.enabled_ids(), std::slice::from_ref(&id));
        assert!(airplay.submit_password(&id, "hunter2"));
        assert_eq!(fake.take_calls(), joined_with(&id, "hunter2"));
    }

    #[test]
    fn a_sender_that_stops_during_a_password_prompt_turns_the_speaker_off() {
        let fake = FakeAirPlay::new();
        let mut airplay = speakers(&fake);
        let study = password_asked(&fake, &mut airplay);
        let id = id_of(&study);
        assert!(airplay.submit_password(&id, "hunter1"));
        fake.fail(&study, ErrorKind::WrongPassword);
        airplay.poll();
        assert!(airplay.password_prompt().is_some());
        fake.stop_running();
        let events = airplay.poll();
        assert!(events.contains(&BackendEvent::AirPlayFailed {
            id: id.clone(),
            problem: AirPlayProblem::NeedsPassword
        }));
        assert_eq!(airplay.password_prompt(), None);
        assert!(airplay.enabled_ids().is_empty(), "not left on and silent");
    }

    #[test]
    fn a_new_sender_drops_the_password_prompt_of_a_pairing() {
        let fake = FakeAirPlay::new();
        let mut airplay = speakers(&fake);
        let tv = device(6, "Den TV", APPLE_TV_TXT);
        pin_shown(&fake, &mut airplay, &tv);
        fake.pairing(&tv, pairing_failed(ErrorKind::PasswordRequired, false));
        airplay.poll();
        assert!(airplay.password_prompt().is_some());
        airplay.replace_link(FakeAirPlay::new().link());
        assert_eq!(airplay.password_prompt(), None);
        assert!(airplay.enabled_ids().is_empty());
    }

    #[test]
    fn switching_off_closes_the_password_prompt() {
        let fake = FakeAirPlay::new();
        let mut airplay = speakers(&fake);
        let study = password_asked(&fake, &mut airplay);
        airplay.set_enabled(&id_of(&study), false).unwrap();
        assert_eq!(airplay.password_prompt(), None);
    }

    #[test]
    fn an_apple_tv_that_asks_for_a_password_pairs_again_with_it() {
        let fake = FakeAirPlay::new();
        let mut airplay = speakers(&fake);
        let tv = device(6, "Den TV", APPLE_TV_TXT);
        let id = id_of(&tv);
        pin_shown(&fake, &mut airplay, &tv);
        assert_eq!(airplay.password_prompt(), None, "one prompt at a time");
        fake.pairing(&tv, pairing_failed(ErrorKind::PasswordRequired, false));
        assert_eq!(airplay.poll(), [BackendEvent::AirPlayChanged], "no notice");
        assert_eq!(airplay.pin_prompt(), None);
        assert_eq!(
            airplay.password_prompt().map(|prompt| prompt.wrong),
            Some(false)
        );
        assert!(airplay.submit_password(&id, "hunter2"));
        assert_eq!(
            fake.take_calls(),
            [
                Call::StartPairing(id.clone()),
                Call::Password(id.clone(), "hunter2".to_owned())
            ]
        );
        fake.pairing(&tv, pairing_failed(ErrorKind::WrongPassword, false));
        airplay.poll();
        assert_eq!(
            airplay.password_prompt().map(|prompt| prompt.wrong),
            Some(true)
        );
        airplay.cancel_password(&id);
        assert_eq!(airplay.password_prompt(), None);
        assert!(airplay.enabled_ids().is_empty());
        assert!(fake.take_calls().is_empty(), "no pairing runs any more");
    }

    fn member_of(device: &Device) -> Member {
        Member {
            key: device.key.clone(),
            name: device.name.clone(),
            device: Some(Box::new(device.clone())),
        }
    }

    /// A stereo pair of two AirPlay 2 speakers, Left and Right (P173).
    fn stereo_pair() -> (Target, Device, Device) {
        let left = airplay2_device(21, "Left", false);
        let right = airplay2_device(22, "Right", false);
        let target = Target {
            id: TargetId::parse("pair:5f0c").expect("a target ID"),
            name: "Living Room".to_owned(),
            members: vec![member_of(&left), member_of(&right)],
            leader: Some(left.key.clone()),
            control: None,
            expected_members: Some(2),
            partner_unreachable: false,
        };
        (target, left, right)
    }

    const PAIR: &str = "pair:5f0c";

    /// The AirPlay side with the switch on and `target` listed.
    fn with_target(fake: &FakeAirPlay, target: &Target, members: &[&Device]) -> AirPlaySpeakers {
        fake.set_airplay2(true);
        let mut airplay = speakers(fake);
        for member in members {
            fake.appear_in(member, target);
        }
        fake.target(target);
        airplay.poll();
        airplay
    }

    #[test]
    fn a_stereo_pair_is_one_row_that_names_its_members() {
        let fake = FakeAirPlay::new();
        let (pair, left, right) = stereo_pair();
        let airplay = with_target(&fake, &pair, &[&left, &right]);
        let rows = airplay.speakers();
        assert_eq!(
            rows.len(),
            1,
            "the members show as part of the pair: {rows:?}"
        );
        let row = &rows[0];
        assert_eq!((row.id.as_str(), row.name.as_str()), (PAIR, "Living Room"));
        assert_eq!(row.availability, AirPlayAvailability::Open);
        assert!(row.airplay2);
        assert_eq!(
            row.group,
            Some(AirPlayGroup {
                kind: AirPlayGroupKind::StereoPair,
                members: vec!["Left".to_owned(), "Right".to_owned()],
                missing: Vec::new(),
                incomplete: false,
            })
        );
    }

    #[test]
    fn targets_are_not_listed_with_the_airplay2_switch_off() {
        let fake = FakeAirPlay::new();
        let (pair, left, right) = stereo_pair();
        let mut airplay = speakers(&fake);
        fake.appear_in(&left, &pair);
        fake.appear_in(&right, &pair);
        fake.target(&pair);
        airplay.poll();
        let ids: Vec<String> = airplay.speakers().into_iter().map(|row| row.id).collect();
        assert_eq!(ids, [id_of(&left), id_of(&right)]);
    }

    #[test]
    fn the_pair_row_plays_both_members_with_one_switch_volume_and_delay() {
        let fake = FakeAirPlay::new();
        let (pair, left, right) = stereo_pair();
        let mut airplay = with_target(&fake, &pair, &[&left, &right]);
        airplay.set_streaming(true);
        airplay.set_enabled(PAIR, true).expect("the pair can play");
        let volume = airplay_volume(DEFAULT_LOCAL_POSITION, 100.0);
        assert_eq!(
            fake.take_calls(),
            [Call::AddTarget {
                id: PAIR.to_owned(),
                volume,
                delay: Duration::ZERO,
                members: vec![id_of(&left), id_of(&right)],
            }]
        );
        assert!(airplay.chosen());

        airplay.set_volume(PAIR, 40.0);
        airplay.set_delay(PAIR, 120.0);
        assert_eq!(
            fake.take_calls(),
            [
                Call::TargetVolume(PAIR.to_owned(), airplay_volume(40.0, 100.0)),
                Call::TargetDelay(PAIR.to_owned(), Duration::from_millis(120)),
            ]
        );

        // The pair plays once one member does.
        fake.state(&left, SpeakerState::Connecting);
        fake.state(&right, SpeakerState::Connecting);
        fake.state(&right, SpeakerState::Playing);
        let events = airplay.poll();
        assert!(events.contains(&BackendEvent::AirPlayPlaying {
            id: PAIR.to_owned()
        }));
        assert!(airplay.speakers()[0].playing);

        airplay.set_enabled(PAIR, false).expect("off");
        assert_eq!(fake.take_calls(), [Call::RemoveTarget(PAIR.to_owned())]);
    }

    #[test]
    fn a_member_out_of_view_or_playing_nothing_is_named_missing() {
        let fake = FakeAirPlay::new();
        let (mut pair, left, right) = stereo_pair();
        pair.members[1].device = None;
        let mut airplay = with_target(&fake, &pair, &[&left]);
        let group = airplay.speakers()[0].group.clone().expect("a group");
        assert!(group.incomplete);
        assert_eq!(group.missing, ["Right"]);

        // In view again, but the sender says it plays nothing (P181).
        pair.members[1] = member_of(&right);
        fake.appear_in(&right, &pair);
        fake.target(&pair);
        airplay.set_streaming(true);
        airplay.set_enabled(PAIR, true).expect("the pair can play");
        fake.push(SenderEvent::TargetMissing {
            id: pair.id.clone(),
            missing: vec![right.key.clone()],
        });
        let events = airplay.poll();
        assert!(events.contains(&BackendEvent::AirPlayChanged));
        let group = airplay.speakers()[0].group.clone().expect("a group");
        assert!(group.incomplete);
        assert_eq!(group.missing, ["Right"]);

        fake.push(SenderEvent::TargetMissing {
            id: pair.id.clone(),
            missing: Vec::new(),
        });
        airplay.poll();
        let group = airplay.speakers()[0].group.clone().expect("a group");
        assert!(!group.incomplete && group.missing.is_empty());

        // A member that reports its partner unreachable, with no name known.
        pair.partner_unreachable = true;
        fake.target(&pair);
        airplay.poll();
        assert!(
            airplay.speakers()[0]
                .group
                .clone()
                .expect("a group")
                .incomplete
        );
    }

    #[test]
    fn a_member_on_by_itself_shows_until_the_pair_takes_it_over() {
        let fake = FakeAirPlay::new();
        fake.set_airplay2(true);
        let mut airplay = speakers(&fake);
        let (pair, left, right) = stereo_pair();
        fake.appear(&left);
        airplay.poll();
        airplay.set_streaming(true);
        airplay
            .set_enabled(&id_of(&left), true)
            .expect("left plays");
        fake.take_calls();

        fake.appear_in(&left, &pair);
        fake.appear_in(&right, &pair);
        fake.target(&pair);
        airplay.poll();
        let ids: Vec<String> = airplay.speakers().into_iter().map(|row| row.id).collect();
        assert_eq!(
            ids,
            [id_of(&left), PAIR.to_owned()],
            "Left stays to be switched off"
        );
        assert!(fake.take_calls().is_empty(), "Left keeps playing");

        airplay.set_enabled(PAIR, true).expect("the pair can play");
        let calls = fake.take_calls();
        assert_eq!(calls[0], Call::Remove(id_of(&left)));
        assert!(matches!(&calls[1], Call::AddTarget { id, .. } if id == PAIR));
        let ids: Vec<String> = airplay.speakers().into_iter().map(|row| row.id).collect();
        assert_eq!(ids, [PAIR]);
    }

    #[test]
    fn a_member_error_is_the_pairs_only_while_no_other_member_plays() {
        let fake = FakeAirPlay::new();
        let (pair, left, right) = stereo_pair();
        let mut airplay = with_target(&fake, &pair, &[&left, &right]);
        airplay.set_streaming(true);
        airplay.set_enabled(PAIR, true).expect("the pair can play");
        fake.state(&left, SpeakerState::Playing);
        fake.fail(&right, ErrorKind::Unreachable);
        let events = airplay.poll();
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, BackendEvent::AirPlayTrouble { .. })),
            "{events:?}"
        );

        fake.fail(&left, ErrorKind::Lost);
        let events = airplay.poll();
        assert!(events.contains(&BackendEvent::AirPlayTrouble {
            id: PAIR.to_owned(),
            problem: AirPlayProblem::ConnectionLost,
        }));
        assert!(airplay.speakers()[0].enabled, "AudioSync keeps trying");
    }

    #[test]
    fn an_apple_tv_and_its_home_theater_group_never_play_at_once() {
        let fake = FakeAirPlay::new();
        let (_, left, right) = stereo_pair();
        let tv = airplay2_device(23, "Den TV", false);
        let theater = Target {
            id: TargetId::parse("theater:9a1e").expect("a target ID"),
            name: "Den".to_owned(),
            members: vec![member_of(&left), member_of(&right)],
            leader: Some(left.key.clone()),
            control: Some(tv.key.clone()),
            expected_members: Some(2),
            partner_unreachable: false,
        };
        let group = "theater:9a1e";
        let mut airplay = with_target(&fake, &theater, &[&left, &right]);
        fake.appear(&tv);
        airplay.poll();
        let rows: Vec<String> = airplay.speakers().into_iter().map(|row| row.id).collect();
        assert_eq!(rows, [group.to_owned(), id_of(&tv)]);
        let row = &airplay.speakers()[0];
        assert_eq!(
            row.group.as_ref().map(|group| group.kind),
            Some(AirPlayGroupKind::HomeTheater)
        );

        airplay.set_streaming(true);
        airplay
            .set_enabled(&id_of(&tv), true)
            .expect("the Apple TV plays");
        assert_eq!(
            airplay.set_enabled(group, true),
            Err(AirPlayProblem::AppleTvPlays)
        );
        airplay.set_enabled(&id_of(&tv), false).expect("off");
        airplay.set_enabled(group, true).expect("the group plays");
        assert_eq!(
            airplay.set_enabled(&id_of(&tv), true),
            Err(AirPlayProblem::PlaysThroughSpeakers)
        );

        // The sender refuses too (P177): the group goes off with a notice.
        fake.fail(
            &left,
            ErrorKind::Unsupported(UnsupportedReason::PlaysThroughSpeakers),
        );
        let events = airplay.poll();
        assert!(events.contains(&BackendEvent::AirPlayFailed {
            id: group.to_owned(),
            problem: AirPlayProblem::AppleTvPlays,
        }));
        assert!(!airplay.speakers()[0].enabled);

        // And an Apple TV it refuses says it plays through its speakers.
        airplay
            .set_enabled(&id_of(&tv), true)
            .expect("the Apple TV plays");
        fake.fail(
            &tv,
            ErrorKind::Unsupported(UnsupportedReason::PlaysThroughSpeakers),
        );
        let events = airplay.poll();
        assert!(events.contains(&BackendEvent::AirPlayFailed {
            id: id_of(&tv),
            problem: AirPlayProblem::PlaysThroughSpeakers,
        }));
    }

    #[test]
    fn calibration_hears_a_playing_pair_as_one_airplay_speaker() {
        let fake = FakeAirPlay::new();
        let (pair, left, right) = stereo_pair();
        let kitchen = device(1, "Kitchen", OPEN_TXT);
        let mut airplay = with_target(&fake, &pair, &[&left, &right]);
        fake.appear(&kitchen);
        airplay.poll();
        airplay.set_streaming(true);
        airplay.set_enabled(PAIR, true).expect("the pair can play");
        airplay
            .set_enabled(&id_of(&kitchen), true)
            .expect("Kitchen plays");
        fake.state(&left, SpeakerState::Playing);
        fake.state(&right, SpeakerState::Playing);
        fake.state(&kitchen, SpeakerState::Playing);
        airplay.poll();
        let playing: Vec<String> = airplay
            .speakers()
            .into_iter()
            .filter(|row| row.enabled && row.playing)
            .map(|row| row.id)
            .collect();
        assert_eq!(playing, [id_of(&kitchen), PAIR.to_owned()]);

        fake.take_calls();
        airplay.set_calibration_solo(Some(PAIR));
        let calls = fake.take_calls();
        assert!(calls.contains(&Call::Volume(id_of(&kitchen), Volume::Muted)));
        assert!(calls.contains(&Call::TargetVolume(
            PAIR.to_owned(),
            airplay_volume(DEFAULT_LOCAL_POSITION, 100.0)
        )));
    }

    #[test]
    fn a_pair_gone_while_playing_keeps_its_row_and_its_members() {
        let fake = FakeAirPlay::new();
        let (pair, left, right) = stereo_pair();
        let mut airplay = with_target(&fake, &pair, &[&left, &right]);
        airplay.set_streaming(true);
        airplay.set_enabled(PAIR, true).expect("the pair can play");
        fake.state(&left, SpeakerState::Playing);
        fake.target_gone(&pair);
        airplay.poll();
        fake.take_calls();
        let ids: Vec<String> = airplay.speakers().into_iter().map(|row| row.id).collect();
        assert_eq!(ids, [PAIR], "the pair stays to be switched off");

        // The sender holds Left for the pair: switched on by itself, it
        // would join nothing and stay on Connecting.
        assert_eq!(
            airplay.set_enabled(&id_of(&left), true),
            Err(AirPlayProblem::PlaysInGroup)
        );
        assert!(fake.take_calls().is_empty());
        assert_eq!(airplay.enabled_ids(), [PAIR]);

        airplay.set_enabled(PAIR, false).expect("off");
        assert_eq!(fake.take_calls(), [Call::RemoveTarget(PAIR.to_owned())]);
        let ids: Vec<String> = airplay.speakers().into_iter().map(|row| row.id).collect();
        assert_eq!(ids, [id_of(&left), id_of(&right)]);
        airplay
            .set_enabled(&id_of(&left), true)
            .expect("Left plays by itself");
        assert!(matches!(&fake.take_calls()[..], [Call::Add { id, .. }] if *id == id_of(&left)));
    }

    #[test]
    fn a_member_left_on_before_its_pair_was_listed_still_joins() {
        let fake = FakeAirPlay::new();
        fake.set_airplay2(true);
        let mut airplay = speakers(&fake);
        let (pair, left, right) = stereo_pair();
        fake.appear(&left);
        airplay.poll();
        airplay
            .set_enabled(&id_of(&left), true)
            .expect("left is on");
        fake.appear_in(&left, &pair);
        fake.appear_in(&right, &pair);
        fake.target(&pair);
        airplay.poll();
        assert!(airplay.chosen(), "Left counts as chosen");
        airplay.set_streaming(true);
        assert!(matches!(&fake.take_calls()[..], [Call::Add { id, .. }] if *id == id_of(&left)));

        // Right, held back by the pair, says why it stays off.
        assert_eq!(
            airplay.set_enabled(&id_of(&right), true),
            Err(AirPlayProblem::PlaysInGroup)
        );
    }

    #[test]
    fn a_member_stays_held_while_its_pair_is_listed_even_with_the_pair_off() {
        let fake = FakeAirPlay::new();
        let (pair, left, right) = stereo_pair();
        let mut airplay = with_target(&fake, &pair, &[&left, &right]);
        airplay.set_streaming(true);
        assert!(airplay.enabled_ids().is_empty(), "the pair is off");
        assert_eq!(
            airplay.set_enabled(&id_of(&left), true),
            Err(AirPlayProblem::PlaysInGroup)
        );
        assert_eq!(
            airplay.holding_target(&id_of(&left)).as_deref(),
            Some("Living Room")
        );

        airplay.set_enabled(PAIR, true).expect("the pair can play");
        airplay.set_enabled(PAIR, false).expect("off");
        assert_eq!(
            airplay.set_enabled(&id_of(&right), true),
            Err(AirPlayProblem::PlaysInGroup),
            "turning the pair off doesn't free it"
        );
    }

    #[test]
    fn a_pair_whose_playing_member_drops_waits_on_the_one_connecting() {
        let fake = FakeAirPlay::new();
        let (pair, left, right) = stereo_pair();
        let mut airplay = with_target(&fake, &pair, &[&left, &right]);
        airplay.set_streaming(true);
        airplay.set_enabled(PAIR, true).expect("the pair can play");
        fake.state(&left, SpeakerState::Playing);
        fake.state(&right, SpeakerState::Connecting);
        airplay.poll();
        assert!(airplay.speakers()[0].playing);
        fake.fail(&left, ErrorKind::Lost);
        airplay.poll();
        assert!(
            !airplay.speakers()[0].playing,
            "Right only connects, so the pair doesn't play"
        );
    }

    #[test]
    fn a_member_that_wants_a_password_is_asked_for_by_name() {
        let fake = FakeAirPlay::new();
        let (pair, left, right) = stereo_pair();
        let mut airplay = with_target(&fake, &pair, &[&left, &right]);
        airplay.set_streaming(true);
        airplay.set_enabled(PAIR, true).expect("the pair can play");
        airplay.begin_attempt(PAIR);
        fake.take_calls();
        fake.fail(&right, ErrorKind::PasswordRequired);
        let events = airplay.poll();
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, BackendEvent::AirPlayFailed { .. })),
            "the prompt asks instead: {events:?}"
        );
        assert_eq!(fake.take_calls(), [Call::RemoveTarget(PAIR.to_owned())]);
        let prompt = airplay.password_prompt().expect("a prompt");
        assert_eq!(
            (prompt.id.as_str(), prompt.name.as_str()),
            (id_of(&right).as_str(), "Right")
        );
        assert!(airplay.submit_password(&id_of(&right), "hunter2"));
        assert!(matches!(&fake.take_calls()[..], [Call::AddTarget { id, .. }] if id == PAIR));
        assert_eq!(
            airplay.enabled_ids(),
            [PAIR],
            "Right stays part of the pair"
        );
        fake.state(&left, SpeakerState::Playing);
        airplay.poll();

        // Turned down again with no switch-on of the user's: the pair goes
        // off, and switching it on asks for Right's password.
        fake.fail(&right, ErrorKind::WrongPassword);
        let events = airplay.poll();
        assert!(events.contains(&BackendEvent::AirPlayMemberNeedsPassword {
            id: PAIR.to_owned(),
            member: "Right".to_owned(),
        }));
        assert_eq!(
            airplay.set_enabled(PAIR, true),
            Err(AirPlayProblem::NeedsPassword)
        );
        airplay.ask_password(PAIR).expect("the prompt opens");
        let prompt = airplay.password_prompt().expect("a prompt");
        assert_eq!(prompt.name, "Right");
        airplay.cancel_password(&id_of(&right));
        assert!(airplay.enabled_ids().is_empty());
        assert_eq!(airplay.password_prompt(), None);
    }

    #[test]
    fn a_gone_pair_whose_member_wants_a_password_goes_off_without_a_prompt() {
        let fake = FakeAirPlay::new();
        let (pair, left, right) = stereo_pair();
        let mut airplay = with_target(&fake, &pair, &[&left, &right]);
        airplay.set_streaming(true);
        airplay.set_enabled(PAIR, true).expect("the pair can play");
        airplay.begin_attempt(PAIR);
        fake.state(&left, SpeakerState::Connecting);
        fake.target_gone(&pair);
        airplay.poll();
        fake.fail(&right, ErrorKind::PasswordRequired);
        let events = airplay.poll();
        assert_eq!(
            airplay.password_prompt(),
            None,
            "no prompt for a pair discovery dropped"
        );
        assert!(events.contains(&BackendEvent::AirPlayMemberNeedsPassword {
            id: PAIR.to_owned(),
            member: "Right".to_owned(),
        }));
        assert!(!airplay.submit_password(&id_of(&right), "hunter2"));
        assert!(airplay.enabled_ids().is_empty());
        let ids: Vec<String> = airplay.speakers().into_iter().map(|row| row.id).collect();
        assert_eq!(ids, [id_of(&left), id_of(&right)]);
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
