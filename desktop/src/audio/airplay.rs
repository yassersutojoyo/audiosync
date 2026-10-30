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
//! (the Apple TV shows a PIN again); any other failure leaves the speaker off
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
use std::net::IpAddr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use audiosync_airplay::dacp::RemoteCommand;
#[cfg(os_backend)]
use audiosync_airplay::discovery::mdns::DiscoveryError;
use audiosync_airplay::discovery::{Device, DeviceKey, ProtocolPath, ServiceKind};
use audiosync_airplay::error::{ErrorKind, UnsupportedReason, UserReason, Warning};
use audiosync_airplay::homekit::HomeKitCredentials;
use audiosync_airplay::input::InputWriter;
use audiosync_airplay::latency::{
    LatencySources, exceeds_largest_latency, frames_to_micros, session_latency_frames,
};
use audiosync_airplay::metadata::MetadataError;
use audiosync_airplay::pairing::{Credentials, PairingEvent, PinError};
use audiosync_airplay::rtsp::Method;
use audiosync_airplay::secret::Secret;
use audiosync_airplay::sender::{
    AIRPLAY_PCM_ENV, AIRPLAY2_AUTO_ENV, AIRPLAY2_SWITCH_ENV, NowPlaying, ReceiverEvent,
    ReceiverInfo, Sender, SenderConfig, SenderEvent, SpeakerError, SpeakerEvent, SpeakerOptions,
    SpeakerState, airplay2_auto, airplay2_switch, pcm_debug_devices,
};
use audiosync_airplay::stream::{
    DEFAULT_LATENCY_FRAMES, MAX_DELAY, SpeakerCounters, StreamCounters,
};
use audiosync_airplay::targets::{Target, TargetChange, TargetId, TargetKind};
use audiosync_airplay::volume::{MIN_DB, MUTED_DB, Volume};
use audiosync_core::fade::InputFade;
use audiosync_core::policy::remote::{
    Action, App, Command, Control, Event as RemoteEvent, OwnVolume, Playback, Remote,
};
use audiosync_core::resample::Resampler;
use audiosync_core::volume::{
    DEFAULT_LOCAL_POSITION, NO_ZONE_POSITION, ReadBack, device_level, device_level_for_db,
    is_echo_of, level_on_speaker, level_set_on_speaker, own_for_device_level, row_move,
    sanitize_own, sanitize_position, set_grid_db,
};

use super::SESSION_RATE;
use super::backend::{
    AirPlayAvailability, AirPlayGroup, AirPlayGroupKind, AirPlayInfo, AirPlayPasswordPrompt,
    AirPlayPinPrompt, AirPlayProblem, BackendEvent, InTarget, PairingPhase,
};
use super::explicit_pause::{ExplicitPause, Step, sound_frames};
use super::pairings::PairingStore;
use crate::debug_log::DebugLog;
use crate::now_playing_card::Transport;

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

    /// Takes a level set on the speaker itself as its volume, sending
    /// nothing (S291).
    fn keep_volume(&mut self, key: &DeviceKey, volume: Volume);

    /// Asks a playing speaker for its own volume (P122); the answer comes
    /// as [`SpeakerEvent::VolumeReported`].
    fn read_volume(&mut self, key: &DeviceKey);

    /// Asks a target's leader, or the first member that plays, for its
    /// volume (P180); the answer comes under that member's key.
    fn read_target_volume(&mut self, id: &TargetId);

    /// Sets a speaker's own delay d_s.
    fn set_delay(&mut self, key: &DeviceKey, delay: Duration);

    /// The user paused on purpose: every speaker gets a FLUSH and no audio
    /// goes out until [`AirPlayLink::resume`] (P213).
    fn pause(&mut self);

    /// Continues after [`AirPlayLink::pause`] (P213).
    fn resume(&mut self);

    /// What is playing, for every speaker that shows it, now and when one
    /// starts (P123 to P127).
    fn set_now_playing(&mut self, now_playing: NowPlaying) -> Result<(), MetadataError>;

    /// L, the sender's base latency.
    fn latency(&self) -> Duration;

    /// Sets the stream's Lf in frames; the sender holds it while a speaker
    /// sounds (P201).
    fn set_latency(&mut self, frames: u32);

    /// Whether a speaker of the stream reached its first-sound time.
    fn sounding(&self) -> bool;

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

    /// Whether this sender prefers AirPlay 2 for every receiver that speaks
    /// it (P214, S312).
    fn airplay2_preferred(&self) -> bool {
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

/// Whether the AirPlay 2 path runs (spec P2, P170): when the saved setting
/// is on, or the environment variable turns it on without a rebuild.
pub fn airplay2_on(saved: bool, env: Option<&str>) -> bool {
    saved || airplay2_switch(env)
}

/// `AUDIOSYNC_AIRPLAY2` as this launch reads it, for [`airplay2_on`].
pub fn airplay2_switch_env() -> Option<String> {
    std::env::var(AIRPLAY2_SWITCH_ENV).ok()
}

/// Whether AirPlay 2 is automatic per receiver for this launch (S312): the
/// build's flag, or `AUDIOSYNC_AIRPLAY2_AUTO`.
pub fn airplay2_auto_on() -> bool {
    airplay2_auto(std::env::var(AIRPLAY2_AUTO_ENV).ok().as_deref())
}

/// The sender's AirPlay 2 path and preference (S312): the switch is the
/// saved setting or `switch_env`, and the flag `auto` decides what it means.
/// On Windows the driver thread runs as a Pro Audio task (S323).
pub fn airplay2_config(saved: bool, switch_env: Option<&str>, auto: bool) -> SenderConfig {
    SenderConfig {
        #[cfg(windows)]
        on_driver_thread: Some(super::wasapi::pro_audio_thread),
        ..SenderConfig::default()
    }
    .with_airplay2_switch(auto, airplay2_on(saved, switch_env))
}

/// The real sender.
pub struct SenderLink {
    sender: Sender,
    airplay2: bool,
    airplay2_preferred: bool,
}

impl SenderLink {
    /// Starts the sender, whose stream and threads run from now on, and
    /// gives its audio input for the capture. `saved_airplay2` is File >
    /// AirPlay 2 (Experimental), or Prefer AirPlay 2 under the flag, as read
    /// at launch.
    pub fn start(saved_airplay2: bool) -> io::Result<(SenderLink, InputWriter)> {
        let config = airplay2_config(
            saved_airplay2,
            std::env::var(AIRPLAY2_SWITCH_ENV).ok().as_deref(),
            airplay2_auto_on(),
        );
        let airplay2 = config.airplay2;
        let airplay2_preferred = config.airplay2_preferred;
        let (sender, input) = Sender::start(config)?;
        Ok((
            SenderLink {
                sender,
                airplay2,
                airplay2_preferred,
            },
            input,
        ))
    }

    /// Starts browsing for receivers, and gives the remote control its
    /// `_dacp._tcp` advertiser, whose daemon starts with the first session
    /// (P188).
    #[cfg(os_backend)]
    pub fn start_discovery(&self) -> Result<(), DiscoveryError> {
        self.sender.start_discovery()?;
        self.sender.set_dacp_advertiser(Box::new(
            audiosync_airplay::discovery::mdns::MdnsAdvertiser::new(),
        ));
        Ok(())
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

    fn keep_volume(&mut self, key: &DeviceKey, volume: Volume) {
        self.sender.keep_volume(key, volume);
    }

    fn read_volume(&mut self, key: &DeviceKey) {
        self.sender.read_volume(key);
    }

    fn read_target_volume(&mut self, id: &TargetId) {
        self.sender.read_target_volume(id);
    }

    fn set_delay(&mut self, key: &DeviceKey, delay: Duration) {
        self.sender.set_delay(key, delay);
    }

    fn pause(&mut self) {
        self.sender.pause();
    }

    fn resume(&mut self) {
        self.sender.resume();
    }

    fn set_now_playing(&mut self, now_playing: NowPlaying) -> Result<(), MetadataError> {
        self.sender.set_now_playing(now_playing)
    }

    fn latency(&self) -> Duration {
        self.sender.latency()
    }

    fn set_latency(&mut self, frames: u32) {
        self.sender.set_latency(frames);
    }

    fn sounding(&self) -> bool {
        self.sender.sounding()
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

    fn airplay2_preferred(&self) -> bool {
        self.airplay2_preferred
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
    /// Session-rate frames pushed with a sample above -60 dBFS, a running
    /// total that a paused stream resumes on (P213).
    sound: Arc<AtomicU64>,
}

impl FeedSlot {
    /// A slot holding a feed into `input`.
    pub fn new(input: InputWriter) -> FeedSlot {
        FeedSlot {
            feed: Arc::new(Mutex::new(Some(AirPlayFeed::new(input)))),
            closing: Arc::new(AtomicBool::new(false)),
            sound: Arc::new(AtomicU64::new(0)),
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
        // Its first block follows the silence the stream sent meanwhile, on
        // the same timeline (P143).
        feed.input.new_capture();
        self.closing.store(false, Ordering::Release);
        Some(FeedLease {
            feed: Some(feed),
            slot: Arc::clone(&self.feed),
            closing: Arc::clone(&self.closing),
            sound: Arc::clone(&self.sound),
        })
    }

    /// Asks the capture's feed to fade out over the next 20 ms it pushes and
    /// to hand the sender zeros after that, until the capture ends (S186,
    /// S189). Any thread; a release store.
    pub fn request_fade_out(&self) {
        self.closing.store(true, Ordering::Release);
    }

    /// Whether a stop asked the feed to fade out since its last lease.
    #[cfg(test)]
    pub fn fading_out(&self) -> bool {
        self.closing.load(Ordering::Acquire)
    }

    /// Seconds of sound the captures pushed so far, a running total (P213).
    pub fn sound_seconds(&self) -> f64 {
        self.sound.load(Ordering::Relaxed) as f64 / f64::from(SESSION_RATE)
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
    sound: Arc<AtomicU64>,
}

impl FeedLease {
    /// See [`AirPlayFeed::push`]; after a stop request the feed fades out.
    pub fn push(&mut self, block: &[f32], frames: usize, at: Instant) {
        let loud = sound_frames(&block[..(frames * 2).min(block.len())]);
        if loud > 0 {
            self.sound.fetch_add(loud, Ordering::Relaxed);
        }
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

/// A playback reading as [`ExplicitPause`] takes it.
fn playback_reading(playback: &Option<(String, bool)>) -> Option<(&str, bool)> {
    playback
        .as_ref()
        .map(|(app, playing)| (app.as_str(), *playing))
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
        PairingEvent::PairedUnverified(_) => {
            "paired on AirPlay 2, but the verify after it failed".to_owned()
        }
        PairingEvent::PairedForSession => {
            "paired on AirPlay 2 for this session, nothing stored".to_owned()
        }
        PairingEvent::Paired(credentials) => {
            format!("paired as client {}", credentials.client_id())
        }
        PairingEvent::Failed { error, wrong_pin } => {
            let pin = if *wrong_pin { " (wrong PIN)" } else { "" };
            format!("failed: {error}{pin}")
        }
        PairingEvent::Stopped => "stopped".to_owned(),
        PairingEvent::PinTurnedDown(why) => why.clone(),
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
/// switch on, and then always with HomeKit credentials stored
/// (`homekit_stored`), otherwise only when AirPlay 1 can't drive it, by
/// discovery or by how its last AirPlay 1 attempt ended
/// (`airplay1_outcome`, P2), the input the sender decides on, or when the
/// sender prefers AirPlay 2 (`preferred`, P214).
pub fn reached_over_airplay2(
    info: &ReceiverInfo,
    airplay2: bool,
    preferred: bool,
    airplay1_outcome: Option<ErrorKind>,
    homekit_stored: bool,
) -> bool {
    airplay2
        && info
            .device
            .choose_path_preferring(true, preferred, airplay1_outcome, homekit_stored)
            == Ok(ProtocolPath::AirPlay2)
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
        UserReason::ReceiverUnverified => AirPlayProblem::ReceiverUnverified,
        UserReason::Unsupported(UnsupportedReason::AirPlay2Only) => AirPlayProblem::AirPlay2NotYet,
        UserReason::Unsupported(UnsupportedReason::Timing) => AirPlayProblem::TimingUnavailable,
        UserReason::Unsupported(UnsupportedReason::TooManyAirPlay2Sessions) => {
            AirPlayProblem::TooManyAirPlay2Speakers
        }
        UserReason::Unsupported(UnsupportedReason::PtpOtherNetwork) => {
            AirPlayProblem::PtpOtherNetwork
        }
        UserReason::Unsupported(UnsupportedReason::PlaysThroughSpeakers) => {
            AirPlayProblem::PlaysThroughSpeakers
        }
        UserReason::Unsupported(_) => AirPlayProblem::Unsupported,
    }
}

/// The volume an AirPlay speaker gets for its own volume, its zone's and
/// the master's (P118 leaves the mapping to the host; S249, S290): the
/// device level min(100, s × k) on the default line, muted at 0.
#[cfg(test)]
pub fn airplay_volume(position: f64, zone: f64, master: f64) -> Volume {
    device_volume(device_level(position, zone, master))
}

/// The volume an AirPlay speaker gets for device level `level`, 0 to 100,
/// on the default line (S289): −30 + 0.3 × D dB, muted at 0.
fn device_volume(level: f64) -> Volume {
    if level > 0.0 {
        Volume::Db(MIN_DB * (1.0 - level.min(100.0) / 100.0))
    } else {
        Volume::Muted
    }
}

/// The level a row holds after `level` was set on the speaker: none while k
/// is above 0, where the own volume carries it (S290).
fn held_level(level: f64, zone: f64, master: f64) -> Option<f64> {
    match own_for_device_level(level, zone, master) {
        Some(_) => None,
        None => Some(level),
    }
}

/// How often playing speakers are asked for their own volume (P122, S291).
const VOLUME_READ_INTERVAL: Duration = Duration::from_secs(1);

/// How long after a set a DACP volume on the row sent is the speaker's
/// echo, not a person's change (S271, S276).
const VOLUME_SETTLE: Duration = Duration::from_secs(3);

/// How long a slider that started editing holds its row with no move: a
/// screen reader's set never sends the release (S271). It mirrors
/// [`VOLUME_SETTLE`], a design value, not a measurement.
const DRAG_IDLE: Duration = Duration::from_secs(3);

/// The address a receiver announces first, AirPlay service before RAOP.
fn first_address(device: &Device) -> Option<IpAddr> {
    device
        .airplay
        .iter()
        .chain(&device.raop)
        .flat_map(|endpoint| &endpoint.addresses)
        .map(|address| address.ip)
        .next()
}

/// The most receivers listed under one announced address, a bridge that
/// fronts many speakers from one host included. The announcer picks that
/// address, so this bounds one address, not one host.
const MAX_LISTED_PER_ADDRESS: usize = 64;

/// The volume reads of one row in the sender.
#[derive(Clone, Copy, Debug, PartialEq)]
struct VolumeRead {
    /// A read was asked since the volume was last set.
    reading: bool,
    /// The row's slider started editing and wasn't released; it holds the
    /// row only while [`VolumeRead::held`] (S271).
    dragging: bool,
    /// When the volume was last set (its own, the master or a calibration)
    /// or the held slider last moved.
    set_at: Instant,
    /// It turned a read down, or its AirPlay 2 reads stopped (P184): it
    /// isn't asked again.
    unreadable: bool,
    /// The echo of the level last sent and the baseline of its reads (S291).
    echo: ReadBack,
}

impl VolumeRead {
    fn new(now: Instant) -> VolumeRead {
        VolumeRead {
            reading: false,
            dragging: false,
            set_at: now,
            unreadable: false,
            echo: ReadBack::default(),
        }
    }

    /// The last set is recent enough for a reading near it to be its echo.
    fn recent(&self, now: Instant) -> bool {
        now.saturating_duration_since(self.set_at) < VOLUME_SETTLE
    }

    /// The slider is held, so a reading must not move the row: it started
    /// editing and moved within [`DRAG_IDLE`] (S271).
    fn held(&self, now: Instant) -> bool {
        self.dragging && now.saturating_duration_since(self.set_at) < DRAG_IDLE
    }

    /// The held slider moved without a set, so its hold restarts.
    fn moved(&mut self, now: Instant) {
        if self.dragging {
            self.set_at = now;
        }
    }

    /// The volume was set: an answer to a read asked before is old.
    fn set(&mut self, now: Instant) {
        self.reading = false;
        self.set_at = now;
    }

    /// Asks unless it can't answer; returns whether it asked.
    fn ask(&mut self) -> bool {
        if self.unreadable {
            return false;
        }
        self.reading = true;
        true
    }

    /// Whether an answer arriving now counts: one was asked since the last
    /// set. The read is answered either way.
    fn answered(&mut self) -> bool {
        std::mem::take(&mut self.reading)
    }
}

/// A delay setting as the sender's d_s, 0 to 20 000 ms.
fn speaker_delay(delay_ms: f64) -> Duration {
    Duration::from_secs_f64(delay_ms.clamp(0.0, MAX_DELAY.as_secs_f64() * 1000.0) / 1000.0)
}

/// What a receiver reported as its latency, as the debug log writes it
/// (S282).
fn describe_reported(us: u64, sources: LatencySources) -> String {
    let names: Vec<&str> = [
        (LatencySources::AUDIO_LATENCY, "Audio-Latency"),
        (LatencySources::AUDIO_LATENCIES, "audioLatencies"),
        (LatencySources::ARRIVAL_TO_RENDER, "arrivalToRender"),
    ]
    .into_iter()
    .filter(|(source, _)| sources.contains(*source))
    .map(|(_, name)| name)
    .collect();
    if names.is_empty() {
        return "reports nothing".to_owned();
    }
    format!(
        "reports {:.0} ms ({})",
        us as f64 / 1000.0,
        names.join(", ")
    )
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
    /// The own volume s, unrounded, 0 up and above 100 when the zone and
    /// the master are below 100 (S290).
    volume: f64,
    /// d_s in ms, 0 to 20 000.
    delay_ms: f64,
    /// The level set on the speaker while k is 0, which s can't hold; the
    /// row shows it until s, the zone or the master moves (S290).
    held_level: Option<f64>,
}

impl Default for Settings {
    fn default() -> Settings {
        Settings {
            enabled: false,
            volume: DEFAULT_LOCAL_POSITION,
            delay_ms: 0.0,
            held_level: None,
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
    /// A stream ended while it ran: failing, it leaves the speaker needing a
    /// PIN, since the click's PIN session went with that stream (S223).
    outlived_stream: bool,
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
    /// For a pairing whose stream ended: as [`PairingRun::outlived_stream`].
    outlived_stream: bool,
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
    read: VolumeRead,
    /// Retried errors since it last played or went unannounced.
    retried: u8,
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
            read: VolumeRead::new(Instant::now()),
            retried: 0,
        }
    }

    /// Counts a retried error; true at the second failed retry, about 3 s
    /// in, so a blip its first retry heals never shows, and a dismissed
    /// notice stays dismissed until it plays again (S222).
    fn reconnecting(&mut self) -> bool {
        self.retried = self.retried.saturating_add(1);
        self.retried == 3
    }

    fn key(&self) -> Option<&DeviceKey> {
        match &self.joined {
            Joined::Speaker(key) => Some(key),
            Joined::Target(_) => None,
        }
    }

    /// Sends `volume`; the first reading near it is its echo (S291).
    fn set_volume(&mut self, link: &mut dyn AirPlayLink, volume: Volume) {
        self.read.echo.sent(volume.wire_db());
        match &self.joined {
            Joined::Speaker(key) => link.set_volume(key, volume),
            Joined::Target(id) => link.set_target_volume(id, volume),
        }
    }

    /// Keeps a level set on the speaker. A target's members still get it
    /// as a set, so they stay at one level (P180).
    fn keep_volume(&mut self, link: &mut dyn AirPlayLink, volume: Volume) {
        match &self.joined {
            Joined::Speaker(key) => link.keep_volume(key, volume),
            Joined::Target(_) => self.set_volume(link, volume),
        }
    }

    fn read_volume(&self, link: &mut dyn AirPlayLink) {
        match &self.joined {
            Joined::Speaker(key) => link.read_volume(key),
            Joined::Target(id) => link.read_target_volume(id),
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
/// connecting, retrying, unannounced, failed, stopped.
fn state_rank(state: &SpeakerState) -> u8 {
    match state {
        SpeakerState::Playing => 0,
        SpeakerState::Connecting => 1,
        SpeakerState::Retrying { .. } => 2,
        SpeakerState::Unannounced => 3,
        SpeakerState::Failed => 4,
        SpeakerState::Stopped => 5,
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
    /// The addresses that announced more receivers than
    /// [`MAX_LISTED_PER_ADDRESS`], each logged once.
    crowded: BTreeSet<IpAddr>,
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
    /// The speakers in `ending` whose pairing the user forgot since: what
    /// their cancelled pairing still reports pairs nothing.
    forgotten_while_ending: BTreeSet<String>,
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
    /// The speakers whose stored credentials an answer turned down: listed
    /// as needing a PIN until a new pairing replaces the credentials or
    /// AudioSync quits. The credentials stay, since a reset receiver and an
    /// impostor at its address answer alike (P154).
    credentials_turned_down: BTreeSet<String>,
    /// The speakers in `password_attempts` that paired with a PIN during
    /// it: one that asks for a pairing again is turned off, not paired again.
    paired_in_attempt: BTreeSet<String>,
    /// Speakers that ask for a code every time (flags bit 3) and passed a
    /// PIN pairing that stored nothing: paired until their session ends or
    /// they leave the sender (P182).
    paired_for_session: BTreeSet<String>,
    /// The speakers a pairing the user started turned on in this stream:
    /// they count as switched on by the user, so a lapsed session pairing
    /// doesn't turn them off (S223).
    paired_by_user: BTreeSet<String>,
    /// A stream that ran stopped since the last poll, which ends the PIN
    /// sessions the user's clicks bought (S223).
    stream_ended: bool,
    /// How the last AirPlay 1 attempt of a speaker ended when that moved it
    /// to AirPlay 2, as the sender keeps it while the speaker stays added.
    airplay1_outcome: BTreeMap<String, ErrorKind>,
    /// The path each running session takes, as the sender reported it: the
    /// row names that path until the session ends (P214, spec S313).
    session_paths: BTreeMap<String, ProtocolPath>,
    streaming: bool,
    master: f64,
    /// The zone volume of each speaker ID in a zone (S249); one not here is
    /// in no zone.
    zones: BTreeMap<String, f64>,
    /// L_s, as the sender last reported it.
    latency: Duration,
    /// The latest R_s each receiver reported in this run, in microseconds,
    /// by speaker ID; dropped when discovery loses it (S280).
    reported_us: BTreeMap<String, u64>,
    /// The Lf last given to the sender, in frames.
    latency_frames: u32,
    /// E: the largest reported latency among the AirPlay speakers in the
    /// stream, in ms, as it applies now (S283).
    estimate_ms: f64,
    /// A new E held while a speaker sounds, once logged.
    estimate_held_ms: Option<f64>,
    /// The e_s each row's d′_s uses, in ms: fixed while a speaker sounds,
    /// so a target member joining or leaving moves no delay until E may
    /// change (S284).
    row_estimates: BTreeMap<String, f64>,
    /// The sender holds an Lf until the stream is empty or restarts.
    latency_held: bool,
    /// The speakers warned this session that their R_s needs more than the
    /// largest Lf (P200).
    latency_warned: BTreeSet<String>,
    /// Added to every speaker's d_s while a Google Cast speaker plays later
    /// than L (C77), in ms.
    hold_back_ms: f64,
    /// While Calibrate All plays a speaker's sweep, the one speaker heard;
    /// the others are muted.
    calibration_solo: Option<String>,
    /// When the playing speakers were last asked for their own volume.
    volume_read_at: Instant,
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
    /// The speaker IDs [`AIRPLAY_PCM_ENV`] allows PCM for (spec P74, G10).
    pcm_devices: BTreeSet<String>,
    /// What speakers' remotes asked for so far (S273 to S279).
    remote: Remote,
    /// The session the now-playing control follows, by app ID, and whether
    /// it plays; `None` with no app to control.
    playback: Option<(String, bool)>,
    /// Transport controls speakers' remotes asked for, for the now-playing
    /// control.
    remote_controls: Vec<Transport>,
    /// Whether the user paused the speakers on purpose (P213).
    pause: ExplicitPause,
    /// The clock the remote decisions count on.
    created_at: Instant,
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
            crowded: BTreeSet::new(),
            store,
            homekit_keys: BTreeMap::new(),
            pairings: BTreeMap::new(),
            ending: BTreeSet::new(),
            forgotten_while_ending: BTreeSet::new(),
            next_shown_order: 0,
            passwords: BTreeMap::new(),
            password_asks: BTreeMap::new(),
            next_ask_order: 0,
            password_attempts: BTreeSet::new(),
            password_seen: BTreeSet::new(),
            pin_seen: BTreeSet::new(),
            credentials_turned_down: BTreeSet::new(),
            paired_in_attempt: BTreeSet::new(),
            paired_for_session: BTreeSet::new(),
            paired_by_user: BTreeSet::new(),
            stream_ended: false,
            airplay1_outcome: BTreeMap::new(),
            session_paths: BTreeMap::new(),
            streaming: false,
            master,
            zones: BTreeMap::new(),
            latency,
            reported_us: BTreeMap::new(),
            latency_frames: DEFAULT_LATENCY_FRAMES,
            estimate_ms: 0.0,
            estimate_held_ms: None,
            row_estimates: BTreeMap::new(),
            latency_held: false,
            latency_warned: BTreeSet::new(),
            hold_back_ms: 0.0,
            calibration_solo: None,
            volume_read_at: Instant::now(),
            stopped: false,
            store_unsaved: false,
            store_change: None,
            store_kept_aside: None,
            now_playing: None,
            pcm_devices: pcm_debug_devices(std::env::var(AIRPLAY_PCM_ENV).ok().as_deref()),
            remote: Remote::default(),
            playback: None,
            remote_controls: Vec::new(),
            pause: ExplicitPause::default(),
            created_at: Instant::now(),
            log,
        };
        speakers.open_airplay2();
        speakers
    }

    /// Hands a sender with the hidden AirPlay 2 switch on the stored device
    /// ID; with the switch off only Forget Pairing opens the identity file.
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

    /// Whether the sender stopped by itself (see `sender_stopped`); only
    /// then does a Retry replace it.
    pub fn is_stopped(&self) -> bool {
        self.stopped
    }

    /// Replaces the sender with `link`, a new one: what the old one listed,
    /// played and paired is gone with it, while the user's settings stay, so
    /// the speakers that were on join again once the new one lists them.
    pub fn replace_link(&mut self, link: Box<dyn AirPlayLink>) {
        // The old sender stops as it is dropped.
        self.link = link;
        self.pause.forget();
        self.listed.clear();
        self.added.clear();
        self.paired_for_session.clear();
        self.paired_by_user.clear();
        self.targets.clear();
        self.target_missing.clear();
        self.gone_targets.clear();
        self.homekit_keys.clear();
        self.pairings.clear();
        self.ending.clear();
        self.forgotten_while_ending.clear();
        self.airplay1_outcome.clear();
        self.session_paths.clear();
        // A password prompt still waiting (discovery stopped, the sessions
        // didn't) stays, as the speaker joins with it once listed again;
        // one for a pairing goes with the pairing, and the speaker stays off.
        self.password_asks.retain(|_, ask| !ask.pair);
        self.latency = self.link.latency();
        self.reported_us.clear();
        self.latency_frames = DEFAULT_LATENCY_FRAMES;
        self.estimate_ms = 0.0;
        self.estimate_held_ms = None;
        self.row_estimates.clear();
        self.latency_held = false;
        self.latency_warned.clear();
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
        // Retry starts a new stream, so this one ends here.
        self.every_time_stream_ended(events);
        self.listed.clear();
        self.added.clear();
        self.paired_for_session.clear();
        self.targets.clear();
        self.target_missing.clear();
        self.gone_targets.clear();
        self.homekit_keys.clear();
        self.pairings.clear();
        self.ending.clear();
        self.forgotten_while_ending.clear();
        self.password_attempts.clear();
        self.airplay1_outcome.clear();
        self.session_paths.clear();
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

    /// A stream ended: the PIN session each click bought ends with it, so a
    /// speaker asking for a code every time that is still on goes off with
    /// the notice that it needs a PIN, and the next click asks again (S223).
    /// One whose pairing or prompt is under way waits for it instead.
    fn every_time_stream_ended(&mut self, events: &mut Vec<BackendEvent>) -> bool {
        self.paired_by_user.clear();
        // Stopped and started again before this ran: a failure now comes
        // within the new stream.
        if !self.streaming {
            let outliving: Vec<String> = self
                .pairings
                .keys()
                .chain(self.password_asks.keys())
                .filter(|id| self.asks_every_time(id))
                .cloned()
                .collect();
            for id in outliving {
                self.mark_outlived_stream(&id, true);
            }
        }
        let waiting = self.waiting_for_user();
        let asking: Vec<String> = self
            .settings
            .iter()
            .filter(|(id, settings)| {
                settings.enabled && !waiting.contains(*id) && self.asks_every_time(id)
            })
            .map(|(id, _)| id.clone())
            .collect();
        for id in &asking {
            self.log.log(|| {
                format!(
                    "AirPlay speaker {} turned off: it asks for a code every time and its stream ended",
                    self.label(id)
                )
            });
            self.turn_off(id, AirPlayProblem::NeedsPin, events);
        }
        !asking.is_empty()
    }

    fn mark_outlived_stream(&mut self, id: &str, outlived: bool) {
        if let Some(run) = self.pairings.get_mut(id) {
            run.outlived_stream = outlived;
        }
        if let Some(ask) = self.password_asks.get_mut(id) {
            ask.outlived_stream = ask.pair && outlived;
        }
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

    /// L_s + E, the latency the local outputs and the AirPlay delay count
    /// from (S283).
    pub fn latency(&self) -> Duration {
        // E comes from whole microseconds, so this stays exact.
        self.latency + Duration::from_micros((self.estimate_ms * 1000.0).round() as u64)
    }

    /// The speaker IDs a row plays to: a target's members, or itself.
    fn members_of(&self, row: &str) -> Vec<String> {
        if let Some(target) = self.targets.get(row) {
            return target
                .members
                .iter()
                .map(|member| speaker_id(&member.key))
                .collect();
        }
        match self.added.get(row) {
            Some(added) if !added.members.is_empty() => added.members.keys().cloned().collect(),
            _ => vec![row.to_owned()],
        }
    }

    /// R_s of a speaker in ms, 0 when it reported nothing.
    fn reported_ms(&self, id: &str) -> f64 {
        self.reported_us
            .get(id)
            .map_or(0.0, |&us| us as f64 / 1000.0)
    }

    /// e_s of a row in ms: its R_s, or for a target, which plays its
    /// members at one delay, the largest of theirs.
    fn estimate_of(&self, row: &str) -> f64 {
        self.members_of(row)
            .iter()
            .map(|id| self.reported_ms(id))
            .fold(0.0, f64::max)
    }

    /// Whether a row plays several members at one delay.
    fn is_target(&self, row: &str) -> bool {
        self.members_of(row) != [row]
    }

    /// e_s as a row's d′_s uses it: a target's stays as it was while a
    /// speaker sounds, so a member joining or leaving moves no delay (S284).
    fn row_estimate(&self, row: &str) -> f64 {
        self.row_estimates
            .get(row)
            .copied()
            .unwrap_or_else(|| self.estimate_of(row))
    }

    /// Whether a target's e_s moved since its d′_s was last allowed to change.
    fn row_estimates_stale(&self) -> bool {
        self.row_estimates
            .iter()
            .any(|(row, &held)| held != self.estimate_of(row))
    }

    /// The rows in the stream and the ones about to join it.
    fn stream_rows(&self, joining: &[String]) -> Vec<String> {
        let mut rows: Vec<String> = self.added.keys().cloned().collect();
        rows.extend(joining.iter().cloned());
        rows
    }

    /// Gives the sender the Lf of every speaker in `rows` from the R_s each
    /// reported (S280, P200), then sets E (S283).
    fn sync_latency(&mut self, rows: &[String]) {
        let members: Vec<String> = rows.iter().flat_map(|row| self.members_of(row)).collect();
        let frames = session_latency_frames(
            members
                .iter()
                .map(|id| self.reported_us.get(id).copied().unwrap_or(0)),
        );
        for id in &members {
            let reported = self.reported_us.get(id).copied().unwrap_or(0);
            if exceeds_largest_latency(reported) && self.latency_warned.insert(id.clone()) {
                self.log.log(|| {
                    format!(
                        "AirPlay speaker {} warning: reports {:.0} ms, more than the largest Lf of 2000 ms leaves room for; it plays at 2000 ms",
                        self.label(id),
                        reported as f64 / 1000.0
                    )
                });
            }
        }
        if frames != self.latency_frames {
            self.latency_frames = frames;
            self.link.set_latency(frames);
            self.log_latency(&format!("Lf set to {frames} frames"), rows);
        }
        let empty = self.added.is_empty();
        self.sync_estimate(rows, empty);
    }

    /// Applies E for `rows` while no speaker sounds, or `now` (the stream
    /// emptied); otherwise holds it (S283, P201).
    fn sync_estimate(&mut self, rows: &[String], now: bool) {
        let free = now || rows.is_empty() || !self.link.sounding();
        if free {
            self.row_estimates = rows
                .iter()
                .map(|row| (row.clone(), self.estimate_of(row)))
                .collect();
        } else {
            // A speaker's own report moves its d′_s (S283); a target's members
            // joining or leaving do not until E may change (S284).
            self.row_estimates.retain(|row, _| rows.contains(row));
            for row in rows {
                if !self.is_target(row) || !self.row_estimates.contains_key(row) {
                    let estimate = self.estimate_of(row);
                    self.row_estimates.insert(row.clone(), estimate);
                }
            }
        }
        let wanted = rows
            .iter()
            .map(|row| self.estimate_of(row))
            .fold(0.0, f64::max);
        if wanted == self.estimate_ms {
            self.estimate_held_ms = None;
            return;
        }
        if !now && !rows.is_empty() && self.link.sounding() {
            if self.estimate_held_ms != Some(wanted) {
                self.estimate_held_ms = Some(wanted);
                let estimate_ms = self.estimate_ms;
                self.log.log(|| {
                    format!(
                        "AirPlay: E {wanted:.0} ms held while a speaker sounds; it stays {estimate_ms:.0} ms until the stream is empty"
                    )
                });
                for row in rows {
                    let late_ms = self.estimate_of(row) - estimate_ms;
                    if late_ms > 0.0 {
                        self.log.log(|| {
                            format!(
                                "AirPlay speaker {} plays {late_ms:.0} ms late until then",
                                self.label(row)
                            )
                        });
                    }
                }
            }
            return;
        }
        self.estimate_ms = wanted;
        self.estimate_held_ms = None;
        self.log_latency(&format!("E set to {wanted:.0} ms"), rows);
    }

    /// Each added row's d′_s plus the hold-back, as sent.
    fn sent_delays(&self) -> Vec<(String, Duration)> {
        self.added
            .keys()
            .map(|row| (row.clone(), self.sent_delay(row)))
            .collect()
    }

    /// Gives the sender every delay that moved since `before`.
    fn send_changed(&mut self, before: Vec<(String, Duration)>) {
        for (row, delay) in before {
            if self.sent_delay(&row) != delay {
                self.send_delay(&row);
            }
        }
    }

    /// Keeps the R_s a speaker reported and sets Lf and E from it (S280,
    /// S283); every row whose d′_s moved gets it.
    fn take_reported(&mut self, id: &str, reported_us: u64) {
        let before = self.sent_delays();
        self.reported_us.insert(id.to_owned(), reported_us);
        let rows = self.stream_rows(&[]);
        self.sync_latency(&rows);
        self.send_changed(before);
    }

    /// The debug line for a new Lf or E: both values, the rows they were
    /// computed over and each one's d′_s (S282).
    fn log_latency(&self, what: &str, rows: &[String]) {
        self.log.log(|| {
            let delays: Vec<String> = rows
                .iter()
                .map(|row| {
                    format!(
                        "{} d′ {} ms",
                        self.label(row),
                        self.sent_delay(row).as_millis()
                    )
                })
                .collect();
            format!(
                "AirPlay: {what}: Lf {} frames ({} ms), E {:.0} ms, over {}",
                self.latency_frames,
                frames_to_micros(self.latency_frames) / 1000,
                self.estimate_ms,
                if delays.is_empty() {
                    "no speaker".to_owned()
                } else {
                    delays.join("; ")
                }
            )
        });
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
        // A speaker with HomeKit credentials never plays over AirPlay 1, so
        // its RAOP record says nothing about it (P2).
        if self.store.homekit(id).is_some() {
            if !self.link.airplay2() {
                return AirPlayAvailability::AirPlay2NotYet;
            }
            // Only its RAOP record is in view: switched on, the sender
            // reports it unreachable instead of playing over AirPlay 1.
            if info.device.airplay.is_none() {
                return AirPlayAvailability::Open;
            }
        }
        if !self.reached_over_airplay2(id, info) {
            return availability(info, self.legacy_paired(id));
        }
        let paired = self.homekit_paired(id) || self.paired_for_session.contains(id);
        match airplay2_availability(info, paired) {
            AirPlayAvailability::Open if !paired && self.pin_seen.contains(id) => {
                AirPlayAvailability::NeedsPin
            }
            availability => availability,
        }
    }

    /// Whether stored AirPlay 2 credentials pair the speaker. A flag asking
    /// for a code every time doesn't count against them: it arrives
    /// unauthenticated, and only a rejected verify turns them down (P154,
    /// P182).
    fn homekit_paired(&self, id: &str) -> bool {
        self.store.homekit(id).is_some() && !self.credentials_turned_down.contains(id)
    }

    /// Whether stored legacy credentials pair the speaker.
    fn legacy_paired(&self, id: &str) -> bool {
        self.store.contains(id) && !self.credentials_turned_down.contains(id)
    }

    /// Whether the listed speaker `id`, reached over AirPlay 2, asks for a
    /// code every time it connects: by its flags, unless it holds stored
    /// credentials no answer turned down (P182). Over AirPlay 1 the same
    /// flag marks the legacy pairing, which asks once.
    fn asks_every_time(&self, id: &str) -> bool {
        self.listed.get(id).is_some_and(|listed| {
            listed.info.device.verdict.code_every_time
                && self.reached_over_airplay2(id, &listed.info)
                && !self.homekit_paired(id)
        })
    }

    /// Whether a session of `id` that plays proved the receiver holds the
    /// stored credentials, so whatever turned them down is gone (P154):
    /// AirPlay 2 verifies its signature with them, AirPlay 1 only when they
    /// carry its key (P56), and a password the user typed proves nothing.
    fn playing_proves_credentials(&self, id: &str) -> bool {
        if self.passwords.contains_key(id) {
            return false;
        }
        if self.over_airplay2(id) {
            self.store.homekit(id).is_some()
        } else {
            self.store
                .get(id)
                .is_some_and(|credentials| credentials.receiver_key().is_some())
        }
    }

    /// Whether the sender reaches the listed speaker `id` over AirPlay 2.
    fn over_airplay2(&self, id: &str) -> bool {
        self.listed
            .get(id)
            .is_some_and(|listed| self.reached_over_airplay2(id, &listed.info))
    }

    /// A running session's own path comes first (spec S313).
    fn reached_over_airplay2(&self, id: &str, info: &ReceiverInfo) -> bool {
        if let Some(path) = self.session_paths.get(id) {
            return *path == ProtocolPath::AirPlay2;
        }
        let outcome = self.airplay1_outcome.get(id).copied();
        let homekit_stored = self.store.homekit(id).is_some();
        reached_over_airplay2(
            info,
            self.link.airplay2(),
            self.link.airplay2_preferred(),
            outcome,
            homekit_stored,
        )
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

    /// The stereo pair or home theater group that holds the speaker `id`,
    /// which then stays off by itself ([`AirPlayProblem::PlaysInGroup`]).
    pub fn holding_target(&self, id: &str) -> Option<InTarget> {
        if !self.held_by_target(id) {
            return None;
        }
        let target = self
            .added_target_of(id)
            .or_else(|| self.target_of(id).map(|target| target.id.to_string()))?;
        let name = self.targets.get(&target)?.name.clone();
        Some(InTarget { id: target, name })
    }

    /// The name the speaker or target `id` was last listed under, a member
    /// its target holds included.
    pub fn listed_name(&self, id: &str) -> Option<&str> {
        self.names.get(id).map(String::as_str)
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

    /// The speakers to show: the listed ones, and any in the sender whose
    /// announcement lapsed, by name.
    pub fn speakers(&self) -> Vec<AirPlayInfo> {
        // A member of a listed target shows as part of it, unless it is on
        // by itself, so it can be switched off (P176).
        let mut ids: Vec<&String> = self
            .listed
            .keys()
            .filter(|id| !self.held_by_target(id))
            .collect();
        ids.extend(self.targets.keys());
        ids.extend(
            self.added
                .keys()
                .filter(|id| !self.listed.contains_key(*id) && !self.targets.contains_key(*id)),
        );
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
                        device_level: self.device_level_of(id),
                        delay_ms: settings.delay_ms,
                        reported_latency_ms: self.estimate_of(id),
                        playing: self.is_playing(id),
                        paired: false,
                        holds_password: false,
                        holds_credentials: false,
                        pairing: false,
                        code_every_time: false,
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
                    device_level: self.device_level_of(id),
                    delay_ms: settings.delay_ms,
                    reported_latency_ms: self.estimate_of(id),
                    playing: self.is_playing(id),
                    paired: self.legacy_paired(id) || self.homekit_paired(id),
                    holds_password: self.passwords.contains_key(id),
                    holds_credentials: self.store.contains(id) || self.store.homekit(id).is_some(),
                    pairing: self.pairings.contains_key(id),
                    code_every_time: self.asks_every_time(id),
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
                outlived_stream: false,
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
        self.off_after_pairing(id);
    }

    /// A speaker switched on while its pairing ran (auto-connect) never
    /// joined, so a pairing ending unpaired leaves it off.
    fn off_after_pairing(&mut self, id: &str) {
        if self.settings(id).enabled {
            self.entry(id).enabled = false;
            self.reconcile();
        }
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

    /// Forgets the speaker's credentials and the password typed for it: it
    /// is turned off, and pairs or asks for the password again the next time
    /// the user switches it on.
    pub fn forget_pairing(&mut self, id: &str) {
        self.stop_pairing(id, "the user forgot the pairing");
        if self.ending.contains(id) {
            self.forgotten_while_ending.insert(id.to_owned());
        }
        let password = self.passwords.remove(id).is_some();
        if password {
            self.log
                .log(|| format!("AirPlay {}: password forgotten", self.label(id)));
        }
        self.credentials_turned_down.remove(id);
        let homekit = self.forget_homekit(id);
        let homekit_or_password = homekit.is_some() || password;
        let (saved, pruned) = self.store.remove(id);
        match saved {
            Ok(true) => {
                // An AirPlay 2 write that failed stays reported.
                self.store_written(pruned.is_ok() && homekit != Some(false));
                self.log
                    .log(|| format!("AirPlay pairing {}: credentials forgotten", self.label(id)));
            }
            Ok(false) if pruned.is_err() => self.store_written(false),
            Ok(false) if !homekit_or_password => return,
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
        if let Err(error) = pruned {
            self.log.log(|| {
                format!(
                    "AirPlay pairing {}: a copy kept aside that holds its credentials couldn't be deleted: {error}",
                    self.label(id)
                )
            });
        }
        self.password_attempts.remove(id);
        if self.settings(id).enabled {
            self.entry(id).enabled = false;
            self.reconcile();
        }
    }

    /// Forgets the speaker's AirPlay 2 credentials and timing entry (P217),
    /// here and in the sender, if AudioSync keeps any. Returns whether their
    /// file was written, `None` when it had no credentials.
    fn forget_homekit(&mut self, id: &str) -> Option<bool> {
        let (on_disk, saved) = self.store.forget_airplay2_closed(id);
        if on_disk {
            return Some(self.homekit_forgotten(id, saved));
        }
        let homekit = self.store.homekit(id).is_some();
        if !homekit && self.store.timing(id).is_none() {
            return None;
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
        if !homekit {
            let _ = self.store.forget_timing(id);
            self.log
                .log(|| format!("AirPlay speaker {}: timing entry forgotten", self.label(id)));
            return None;
        }
        let saved = self.store.remove_homekit(id);
        Some(self.homekit_forgotten(id, saved))
    }

    /// Reports the speaker's forgotten AirPlay 2 credentials and how their
    /// file's write went. Returns whether it was written.
    fn homekit_forgotten(&mut self, id: &str, saved: io::Result<()>) -> bool {
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
        saved.is_ok()
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
            self.paired_in_attempt.remove(id);
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
        let (order, outlived_stream) = match self.password_asks.get(id) {
            Some(ask) => (ask.order, ask.outlived_stream),
            None => {
                self.next_ask_order += 1;
                (self.next_ask_order, false)
            }
        };
        self.password_asks.insert(
            id.to_owned(),
            PasswordAsk {
                wrong,
                order,
                pair,
                target: target.map(str::to_owned),
                outlived_stream,
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
            let started = self.start_pairing(id).is_ok();
            if ask.outlived_stream {
                self.mark_outlived_stream(id, true);
            }
            return started;
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

    /// The speakers whose pairing runs or whose password prompt waits, shown
    /// or queued, with the target a member's prompt switches on.
    pub fn waiting_for_user(&self) -> BTreeSet<String> {
        let mut waiting: BTreeSet<String> = self.pairings.keys().cloned().collect();
        for (id, ask) in &self.password_asks {
            waiting.insert(id.clone());
            waiting.extend(ask.target.clone());
        }
        waiting
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

    /// Sets a speaker's own volume s, live: it plays at min(100, s × k)
    /// (S290). A saved speaker and a preset's member come back this way,
    /// unrounded and above 100 when S290 stored it so.
    pub fn set_volume(&mut self, id: &str, position: f64) {
        if !position.is_finite() {
            return;
        }
        let position = sanitize_own(position);
        let settings = self.settings(id);
        if settings.volume == position && settings.held_level.is_none() {
            return;
        }
        let entry = self.entry(id);
        entry.volume = position;
        entry.held_level = None;
        let volume = self.volume_of(id);
        self.log.log(|| {
            format!(
                "AirPlay speaker {} volume: {position} ({})",
                self.label(id),
                describe_volume(volume)
            )
        });
        if let Some(added) = self.added.get_mut(id) {
            added.read.set(Instant::now());
            added.set_volume(self.link.as_mut(), volume);
        }
        self.remote.forget(id);
    }

    /// The speaker's row moved to `row` (S290): it gets device level `row`
    /// and stores the own volume row ÷ k, unrounded and above 100 when the
    /// zone and the master are below 100. Nothing changes with either at 0.
    pub fn move_row(&mut self, id: &str, row: f64) {
        let (zone, master) = (self.zone(id), self.master);
        let Some(moved) = row_move(row, zone, master) else {
            // Nothing is sent, but the move still keeps the slider's hold.
            if let Some(added) = self.added.get_mut(id) {
                added.read.moved(Instant::now());
            }
            return;
        };
        let entry = self.entry(id);
        entry.volume = moved.own_position;
        entry.held_level = None;
        let volume = self.volume_of(id);
        self.log.log(|| {
            format!(
                "AirPlay speaker {} row: {} (own volume {}, {})",
                self.label(id),
                moved.device_level,
                moved.own_position,
                describe_volume(volume)
            )
        });
        if let Some(added) = self.added.get_mut(id) {
            added.read.set(Instant::now());
            added.set_volume(self.link.as_mut(), volume);
        }
        self.remote.forget(id);
    }

    /// The device level D speaker `id` has: min(100, s × k), or with k at 0
    /// the level set on the speaker since (S290).
    fn device_level_of(&self, id: &str) -> f64 {
        let settings = self.settings(id);
        let level = device_level(settings.volume, self.zone(id), self.master);
        match settings.held_level {
            Some(held) if level == 0.0 => held,
            _ => level,
        }
    }

    /// The speaker's slider is held (true) or let go (false); a read asked
    /// before either is old.
    pub fn set_volume_dragging(&mut self, id: &str, dragging: bool) {
        if let Some(added) = self.added.get_mut(id) {
            added.read.dragging = dragging;
            added.read.reading = false;
            if dragging {
                added.read.set_at = Instant::now();
            }
        }
    }

    /// Sets the master position, which scales every speaker, live.
    pub fn set_master(&mut self, master: f64) {
        // A master move keeps s, so every row shows min(100, s × k) (S290).
        if master != self.master {
            for settings in self.settings.values_mut() {
                settings.held_level = None;
            }
        }
        self.master = master;
        self.send_volumes();
    }

    /// Sets the zone volume of every speaker ID in a zone (S249), the rest
    /// being in no zone; a speaker whose zone volume changed gets its new
    /// level at once, as a master change gives it.
    pub fn set_zones(&mut self, zones: BTreeMap<String, f64>) {
        let zones: BTreeMap<String, f64> = zones
            .into_iter()
            .filter_map(|(id, zone)| sanitize_position(zone).map(|zone| (id, zone)))
            .filter(|(_, zone)| *zone != NO_ZONE_POSITION)
            .collect();
        let changed: BTreeSet<String> = self
            .zones
            .keys()
            .chain(zones.keys())
            .filter(|id| self.zones.get(*id) != zones.get(*id))
            .cloned()
            .collect();
        self.zones = zones;
        for id in &changed {
            if let Some(settings) = self.settings.get_mut(id) {
                settings.held_level = None;
            }
        }
        let now = Instant::now();
        for id in changed {
            let volume = self.volume_of(&id);
            if let Some(added) = self.added.get_mut(&id) {
                added.read.set(now);
                added.set_volume(self.link.as_mut(), volume);
            }
        }
    }

    /// The zone volume of speaker `id`: 100 when it is in no zone.
    fn zone(&self, id: &str) -> f64 {
        self.zones.get(id).copied().unwrap_or(NO_ZONE_POSITION)
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

    /// The volume speaker `id` gets now, a level held at k = 0 included
    /// (S290): muted while a calibration lets another speaker alone be heard.
    fn volume_of(&self, id: &str) -> Volume {
        match self.calibration_solo.as_deref() {
            Some(solo) if solo != id => Volume::Muted,
            _ => device_volume(self.device_level_of(id)),
        }
    }

    fn send_volumes(&mut self) {
        let now = Instant::now();
        let volumes: Vec<(String, Volume)> = self
            .added
            .keys()
            .map(|id| (id.clone(), self.volume_of(id)))
            .collect();
        for (id, volume) in volumes {
            if let Some(added) = self.added.get_mut(&id) {
                added.read.set(now);
                added.set_volume(self.link.as_mut(), volume);
            }
        }
    }

    /// Asks every playing row that may answer for its own volume, every
    /// [`VOLUME_READ_INTERVAL`]: a level changed on the speaker moves its
    /// slider (P122), a target's through its leader (P180). Not a speaker
    /// that scales the samples itself (`sv`), whose level isn't the one the
    /// slider sets (P121).
    fn read_volumes(&mut self, now: Instant) {
        if now.saturating_duration_since(self.volume_read_at) < VOLUME_READ_INTERVAL {
            return;
        }
        self.volume_read_at = now;
        for (id, added) in &mut self.added {
            let software = self
                .listed
                .get(id)
                .and_then(|listed| listed.device().raop_txt.as_ref())
                .and_then(|txt| txt.software_volume)
                == Some(true);
            if software || added.state != SpeakerState::Playing {
                continue;
            }
            if added.read.ask() {
                added.read_volume(self.link.as_mut());
            }
        }
    }

    /// What the now-playing control follows: the session that counts, by
    /// app ID, and whether it plays (S274), as read at `now`. Set before
    /// each poll. `player` is the same session as the explicit pause reads
    /// it, `None` unless it plays or is paused: the app seen pausing pauses a
    /// stream that plays, and a paused stream resumes once the reading no
    /// longer shows the paused app paused (P213).
    pub fn set_playback(
        &mut self,
        playback: Option<(String, bool)>,
        player: Option<(String, bool)>,
        now: Instant,
    ) {
        self.playback = playback;
        let step = self
            .pause
            .read(playback_reading(&player), now, self.streaming);
        self.apply_pause(step, "player reading");
    }

    /// The window's play-pause went to `app`, shown `playing`: the speakers
    /// pause only when the reading agrees that this app plays (P213).
    pub fn play_pause_pressed(&mut self, app: &str, playing: bool, now: Instant) {
        let agreed = playback_reading(&self.playback).filter(|&reading| reading == (app, playing));
        let step = self.pause.toggled(agreed, now);
        self.apply_pause(step, "app button");
    }

    /// The capture carried `sound` seconds of sound so far, a running total
    /// ([`FeedSlot::sound_seconds`]): a paused stream may resume (P213).
    pub fn heard(&mut self, sound: f64, now: Instant) {
        let step = self.pause.heard(sound, now);
        self.apply_pause(step, "sound heard");
    }

    /// Hands the sender an explicit pause or resume (P213). `cause` names what
    /// resumed the speakers in the debug log, so a resume with nothing audible
    /// can be traced.
    fn apply_pause(&mut self, step: Step, cause: &'static str) {
        match step {
            Step::Pause => {
                self.log
                    .log(|| "AirPlay: paused on purpose, every speaker flushed".to_owned());
                self.link.pause();
            }
            Step::Resume => {
                self.log
                    .log(|| format!("AirPlay: resumed after a pause ({cause})"));
                self.link.resume();
            }
            Step::Nothing => {}
        }
    }

    /// The transport controls speakers' remotes asked for since the last
    /// call, for the now-playing control (S274).
    pub fn take_remote_controls(&mut self) -> Vec<Transport> {
        std::mem::take(&mut self.remote_controls)
    }

    /// A speaker's remote asked for `command` (P193): the core's decision
    /// (S273 to S279) acts on the speaker's row, or its target's, and its
    /// line goes to the debug log. Returns whether the speaker list changed.
    fn remote_command(
        &mut self,
        key: &DeviceKey,
        target: Option<&TargetId>,
        command: RemoteCommand,
        now: Instant,
    ) -> bool {
        let id = speaker_id(key);
        let row = match target.map(ToString::to_string) {
            Some(target) if self.added.contains_key(&target) => Some(target),
            _ if self.added.contains_key(&id) => Some(id.clone()),
            _ => None,
        };
        let command = match command {
            RemoteCommand::PlayPause => Command::PlayPause,
            RemoteCommand::Play => Command::Play,
            RemoteCommand::Pause => Command::Pause,
            RemoteCommand::Next => Command::Next,
            RemoteCommand::Previous => Command::Previous,
            RemoteCommand::VolumeUp => Command::VolumeUp,
            RemoteCommand::VolumeDown => Command::VolumeDown,
            RemoteCommand::MuteToggle => Command::MuteToggle,
            RemoteCommand::DeviceVolume(Volume::Muted) => Command::DeviceVolume(MUTED_DB),
            RemoteCommand::DeviceVolume(Volume::Db(db)) => Command::DeviceVolume(db),
        };
        let name = self.label(&id);
        let volume = self.own_volume(row.as_deref(), command, now);
        let event = RemoteEvent {
            speaker: &name,
            row: row.as_deref(),
            member: target.is_some(),
            command,
            app: self.playback.as_ref().map(|(app, playing)| App {
                name: app,
                playback: if *playing {
                    Playback::Playing
                } else {
                    Playback::Paused
                },
            }),
            volume,
            at: now.saturating_duration_since(self.created_at).as_secs_f64(),
        };
        let decision = self.remote.decide(&event);
        let line = decision.line;
        self.log.log(|| line);
        match (decision.action, row) {
            (Action::Send(control), Some(_)) => {
                if control == Control::PlayPause {
                    let step = self.pause.toggled(playback_reading(&self.playback), now);
                    self.apply_pause(step, "speaker remote");
                }
                self.remote_controls.push(match control {
                    Control::PlayPause => Transport::PlayPause,
                    Control::Next => Transport::Next,
                    Control::Previous => Transport::Previous,
                });
                false
            }
            (Action::Move { to, .. }, Some(row)) => {
                let on_speaker = matches!(command, Command::DeviceVolume(_));
                self.remote_move(&row, to, on_speaker, now)
            }
            _ => false,
        }
    }

    /// The volume of the added row as a remote decision sees it: its device
    /// level (S290); for a DeviceVolume with S271's echo window and the level
    /// it asks for.
    fn own_volume(&self, row: Option<&str>, command: Command, now: Instant) -> OwnVolume {
        let mut own = OwnVolume {
            level: 0.0,
            dragging: false,
            echo: false,
            reported: None,
        };
        let Some((row, added)) = row.and_then(|row| Some((row, self.added.get(row)?))) else {
            return own;
        };
        own.level = self.device_level_of(row);
        own.dragging = added.read.held(now);
        if let Command::DeviceVolume(db) = command {
            // The level the speaker has, the held one with k at 0 (S290).
            let has = match self.calibration_solo.as_deref() {
                Some(solo) if solo != row => Volume::Muted,
                _ => device_volume(own.level),
            }
            .wire_db();
            own.echo = added.read.recent(now) && is_echo_of(db, has, self.set_grid(row));
            own.reported = device_level_for_db(db);
        }
        own
    }

    /// A remote moved `row` to device level `to`: as at the end of a drag,
    /// the speaker gets it, which starts S271's window (S275 to S277). A
    /// DeviceVolume is a change made on the speaker, any other command a row
    /// move (S290). Returns whether the speaker list changed.
    fn remote_move(&mut self, row: &str, to: f64, on_speaker: bool, now: Instant) -> bool {
        let (zone, master) = (self.zone(row), self.master);
        let own = self.settings(row).volume;
        let moved = if on_speaker {
            level_on_speaker(to, own, zone, master)
        } else {
            let Some(moved) = row_move(to, zone, master) else {
                return false;
            };
            moved
        };
        let entry = self.entry(row);
        entry.volume = moved.own_position;
        entry.held_level = if on_speaker {
            held_level(moved.device_level, zone, master)
        } else {
            None
        };
        let heard = self
            .calibration_solo
            .as_deref()
            .is_none_or(|solo| solo == row);
        let volume = if on_speaker && heard {
            device_volume(moved.device_level)
        } else {
            self.volume_of(row)
        };
        if let Some(added) = self.added.get_mut(row) {
            added.read.set(now);
            added.set_volume(self.link.as_mut(), volume);
        }
        true
    }

    /// The grid speaker `id` keeps a level sent on, by its make (S291).
    fn set_grid(&self, id: &str) -> Option<f64> {
        let txt = self.listed.get(id)?.device().airplay_txt.as_ref()?;
        set_grid_db(txt.manufacturer.as_deref(), txt.features.map(|bits| bits.0))
    }

    /// The added row `id` reports for: itself, or the target it's a member
    /// of.
    fn row_of(&self, id: &str) -> Option<String> {
        if self.added.contains_key(id) {
            return Some(id.to_owned());
        }
        self.added_target_of(id)
    }

    /// A read answered `db` (S291): read after the volume was last set and
    /// not during a drag, the first reading near a level sent is its echo
    /// and the baseline; any other reading away from the baseline is a
    /// level set on the speaker, which sets the row's device level and the
    /// own volume (S290). The sender keeps that level without sending it
    /// back, so a session that starts again or a member that joins late
    /// gets it. Returns whether the speaker list changed.
    fn volume_reported(&mut self, id: &str, db: f64) -> bool {
        let Some(row) = self.row_of(id) else {
            return false;
        };
        let silenced = self
            .calibration_solo
            .as_deref()
            .is_some_and(|solo| solo != row);
        let (zone, master) = (self.zone(&row), self.master);
        let own = self.settings(&row).volume;
        let shown = self.device_level_of(&row);
        let grid = self.set_grid(id);
        let Some(added) = self.added.get_mut(&row) else {
            return false;
        };
        if !added.read.answered() || added.read.held(Instant::now()) {
            return false;
        }
        if !added.read.echo.set_on_speaker(db, grid) {
            return false;
        }
        let Some(moved) = level_set_on_speaker(db, own, shown, zone, master) else {
            return false;
        };
        // The sender keeps the level as read, past the wire range its nearest.
        let level = if db <= MUTED_DB {
            Volume::Muted
        } else {
            Volume::from_db(db).unwrap_or(Volume::Muted)
        };
        // A silenced speaker that reconnects during the solo starts muted; the solo end sends the new level.
        let kept = if silenced { Volume::Muted } else { level };
        added.keep_volume(self.link.as_mut(), kept);
        let entry = self.entry(&row);
        entry.volume = moved.own_position;
        entry.held_level = held_level(moved.device_level, zone, master);
        self.log.log(|| {
            format!(
                "AirPlay speaker {} volume changed on the speaker to {}: own volume {}",
                self.label(&row),
                describe_volume(level),
                moved.own_position
            )
        });
        true
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
            .map(|id| self.wanted_delay_ms(id) - self.sent_delay(id).as_secs_f64() * 1000.0)
            .fold(0.0, f64::max)
    }

    /// d′_s = d_s + E − e_s (S283) plus the hold-back, in ms, before the
    /// sender's limits. While E is held, a speaker reporting more than E
    /// gets no offset and plays the difference late, as S283 says.
    fn wanted_delay_ms(&self, id: &str) -> f64 {
        let offset_ms = (self.estimate_ms - self.row_estimate(id)).max(0.0);
        self.settings(id).delay_ms + offset_ms + self.hold_back_ms
    }

    /// d′_s plus the hold-back, as the sender plays it.
    fn sent_delay(&self, id: &str) -> Duration {
        speaker_delay(self.wanted_delay_ms(id))
    }

    /// Gives the sender a speaker's delay, if the speaker is in it.
    fn send_delay(&mut self, id: &str) {
        let delay = self.sent_delay(id);
        let short_ms = self.wanted_delay_ms(id) - delay.as_secs_f64() * 1000.0;
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
        if !streaming {
            let step = self.pause.clear();
            self.apply_pause(step, "streaming stopped");
            self.stream_ended |= self.streaming;
        } else if !self.streaming {
            // A pairing that fails from now on fails within this stream (S223).
            let ids: Vec<String> = self
                .pairings
                .keys()
                .chain(self.password_asks.keys())
                .cloned()
                .collect();
            for id in ids {
                self.mark_outlived_stream(&id, false);
            }
        }
        self.streaming = streaming;
        self.reconcile();
    }

    /// The PC is going to sleep, which ends the stream as stopping it does
    /// (S223).
    pub fn sleeping(&mut self) {
        self.stream_ended |= self.streaming;
    }

    /// A new capture started (P143): a paused stream resumes. The timeline
    /// goes on and the speakers get no FLUSH, so a held E stays held.
    pub fn capture_started(&mut self) {
        let step = self.pause.clear();
        self.apply_pause(step, "new capture");
    }

    /// The speaker's password, the one its sessions get too, for a pairing
    /// with an Apple TV that asks for both.
    fn password(&self, id: &str) -> Option<Secret> {
        self.passwords.get(id).cloned()
    }

    /// The options a speaker joins with, at the level k gives: one held
    /// at k = 0 goes, since the speaker gets the other (S290).
    fn options(&mut self, id: &str) -> SpeakerOptions {
        if let Some(settings) = self.settings.get_mut(id) {
            settings.held_level = None;
        }
        let mut options = SpeakerOptions::new(self.volume_of(id));
        options.delay = self.sent_delay(id);
        options.credentials = self.store.get(id).cloned();
        options.homekit_credentials = self.store.homekit(id).cloned();
        options.timing = self.store.timing(id).cloned();
        options.password = self.password(id);
        options.pcm = self.pcm_devices.contains(id);
        options.trace = self.log.enabled();
        options
    }

    /// Adds the speakers that should play and removes the ones that
    /// shouldn't: while streaming, every speaker that is on and listed as
    /// able to play joins; one already in the sender stays while it is on,
    /// listed or not, as the sender keeps trying to reach it or waits until
    /// it is announced again (P215).
    fn reconcile(&mut self) {
        let leaving: Vec<String> = self
            .added
            .iter()
            .filter(|(id, _)| !self.streaming || !self.settings(id).enabled)
            .map(|(id, _)| id.clone())
            .collect();
        let left = !leaving.is_empty();
        for id in leaving {
            if let Some(added) = self.added.remove(&id) {
                added.remove(self.link.as_mut());
                self.paired_for_session.remove(&id);
                self.forget_outcomes(&id, &added);
                self.target_missing.remove(&id);
                if self.gone_targets.remove(&id) {
                    self.targets.remove(&id);
                }
                self.log
                    .log(|| format!("AirPlay speaker {} removed", self.label(&id)));
            }
        }
        if self.added.is_empty() {
            self.latency_warned.clear();
        }
        let joining: Vec<String> = if self.streaming {
            self.settings
                .iter()
                // While its PIN pairing runs, a session would pair anew and
                // the TV would show a new code.
                .filter(|(id, settings)| {
                    settings.enabled
                        && !self.added.contains_key(*id)
                        && !self.pairings.contains_key(*id)
                        && self.can_join(id)
                })
                .map(|(id, _)| id.clone())
                .collect()
        } else {
            Vec::new()
        };
        // Lf and E are set before a speaker is added, so one the stream
        // starts with gets them with no change at all (P201).
        if left || !joining.is_empty() {
            let before = self.sent_delays();
            let rows = self.stream_rows(&joining);
            self.sync_latency(&rows);
            self.send_changed(before);
        }
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
                self.log.log(|| {
                    format!(
                        "AirPlay speaker {}: PCM allowed by {AIRPLAY_PCM_ENV}",
                        self.label(&id)
                    )
                });
            }
            let initial = options.volume;
            let mut added = match self.targets.get(&id) {
                Some(target) => self.add_target(target.clone(), options),
                None => {
                    let device = self.listed[&id].device();
                    let key = device.key.clone();
                    let counters = self.link.add_speaker(device, options);
                    Added::speaker(key, counters)
                }
            };
            added.read.echo.sent(initial.wire_db());
            self.remote.forget(&id);
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
                own.timing = self.store.timing(&id).cloned();
                own.password = self.password(&id);
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
            read: VolumeRead::new(Instant::now()),
            retried: 0,
        }
    }

    /// The speaker `id` the user switched on turned its stored AirPlay 2
    /// credentials down: it pairs with a PIN in the same attempt, and its
    /// session goes on over the pairing connection (P153). It stays off
    /// meanwhile, as on a first pairing; with no pairing it is turned off.
    fn pair_in_attempt(&mut self, id: &str, events: &mut Vec<BackendEvent>) -> bool {
        self.drop_password_ask(id, "the speaker pairs with a PIN");
        self.entry(id).enabled = false;
        self.reconcile();
        if let Err(problem) = self.start_pairing(id) {
            events.push(BackendEvent::AirPlayFailed {
                id: id.to_owned(),
                problem,
            });
        }
        true
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

    /// While streaming, turns off with a notice each speaker that is on,
    /// listed as asking for a PIN or a password it has no answer for, and
    /// not in the user's own attempt: one restored from the last run or
    /// switched on by auto-connect, which never pair or prompt (S222, S223).
    /// Left on it would never join. Returns whether one went off.
    fn turn_off_uninvited(&mut self, events: &mut Vec<BackendEvent>) -> bool {
        if !self.streaming {
            return false;
        }
        let uninvited: Vec<(String, AirPlayAvailability)> = self
            .settings
            .iter()
            .filter(|(id, settings)| {
                settings.enabled
                    && !self.added.contains_key(*id)
                    && !self.pairings.contains_key(*id)
                    && !self.password_attempts.contains(*id)
                    && !self.password_asks.contains_key(*id)
                    && !self.paired_by_user.contains(*id)
                    && !self.held_by_target(id)
            })
            .filter_map(|(id, _)| {
                let availability = self.listed_availability(id)?;
                let asks = match availability {
                    AirPlayAvailability::NeedsPin => true,
                    AirPlayAvailability::NeedsPassword => !self.passwords.contains_key(id),
                    _ => false,
                };
                asks.then(|| (id.clone(), availability))
            })
            .collect();
        for (id, availability) in &uninvited {
            self.log.log(|| {
                format!(
                    "AirPlay speaker {} turned off: it {} and the user didn't switch it on",
                    self.label(id),
                    describe_availability(*availability)
                )
            });
            if let Some(problem) = availability.problem() {
                self.turn_off(id, problem, events);
            }
        }
        !uninvited.is_empty()
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
                    if let SpeakerEvent::HomeKitPaired(_) = event {
                        self.homekit_keys.insert(id.clone(), key);
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
                    if std::mem::take(&mut self.latency_held) {
                        self.log
                            .log(|| "AirPlay: the held Lf applies now".to_owned());
                    }
                    self.latency = latency;
                }
                SenderEvent::LatencyHeld { frames } => {
                    self.latency_held = true;
                    self.log.log(|| {
                        format!(
                            "AirPlay: Lf {frames} frames held while a speaker sounds, until the stream is empty or restarts"
                        )
                    });
                }
                SenderEvent::Pairing { key, event } => {
                    let id = speaker_id(&key);
                    if matches!(
                        event,
                        PairingEvent::HomeKitPaired(_) | PairingEvent::PairedUnverified(_)
                    ) {
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
                SenderEvent::Remote {
                    key,
                    target,
                    command,
                } => changed |= self.remote_command(&key, target.as_ref(), command, Instant::now()),
                SenderEvent::RemoteLog(line) => self.log.log(|| line),
                SenderEvent::PathLog { key, line } => {
                    let id = speaker_id(&key);
                    self.log
                        .log(|| format!("AirPlay {}: {line}", self.label(&id)));
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
        // A new E held while a speaker sounded applies once none does.
        if self.estimate_held_ms.is_some() || self.row_estimates_stale() {
            let before = self.sent_delays();
            let rows = self.stream_rows(&[]);
            self.sync_estimate(&rows, false);
            self.send_changed(before);
        }
        // Events it sent before it stopped were taken in above.
        if self.link.is_running() {
            self.read_volumes(Instant::now());
        } else {
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
        if std::mem::take(&mut self.stream_ended) {
            changed |= self.every_time_stream_ended(&mut events);
        }
        changed |= self.turn_off_uninvited(&mut events);
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
                // An AirPlay 2 receiver that turned the PIN down asks again
                // on the same connection, with the same code (P150).
                if run.phase == PairingPhase::Checking {
                    run.wrong_pin = true;
                }
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
                    if self.password_attempts.contains(id) {
                        self.paired_in_attempt.insert(id.to_owned());
                    }
                    self.turn_on_paired(id, events);
                }
                true
            }
            // Kept only: the Failed that follows ends the pairing (P153).
            PairingEvent::PairedUnverified(credentials) => {
                self.keep_homekit(id, credentials);
                true
            }
            PairingEvent::PairedForSession => {
                if self.pairings.remove(id).is_none() {
                    return false;
                }
                // Stored keys stay: the flags that made this pairing one for
                // the session came unauthenticated (P61, P154).
                self.paired_for_session.insert(id.to_owned());
                if self.password_attempts.contains(id) {
                    self.paired_in_attempt.insert(id.to_owned());
                }
                self.turn_on_paired(id, events);
                true
            }
            PairingEvent::Failed {
                wrong_pin: true, ..
            } => {
                let Some(run) = self.pairings.get_mut(id) else {
                    return false;
                };
                // An AirPlay 1 pairing ends at a wrong PIN and starts again
                // from `/pair-pin-start` (P52).
                run.phase = PairingPhase::Starting;
                run.wrong_pin = true;
                let device = run.device.clone();
                let password = self.password(id);
                self.link.start_pairing(&device, password);
                self.log.log(|| {
                    format!(
                        "AirPlay pairing {}: wrong PIN, pairing started again",
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
                let Some(run) = self.pairings.remove(id) else {
                    return false;
                };
                let wrong = self.passwords.remove(id).is_some()
                    || matches!(error.kind, ErrorKind::WrongPassword);
                self.queue_password_ask(id, wrong, true, None);
                if run.outlived_stream {
                    self.mark_outlived_stream(id, true);
                }
                true
            }
            PairingEvent::Failed { error, .. } => {
                let Some(run) = self.pairings.remove(id) else {
                    return false;
                };
                self.off_after_pairing(id);
                let problem = if let ErrorKind::Unsupported(_) = error.kind {
                    problem(error.kind.user_reason())
                } else if run.outlived_stream {
                    AirPlayProblem::NeedsPin
                } else {
                    AirPlayProblem::PairingFailed
                };
                events.push(BackendEvent::AirPlayFailed {
                    id: id.to_owned(),
                    problem,
                });
                true
            }
            // A shutdown, already taken in. (The end of a pairing the user
            // cancelled comes while it is `ending`.)
            PairingEvent::Stopped => false,
            // Logged above; the PIN prompt follows.
            PairingEvent::PinTurnedDown(_) => false,
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
        let event = match event {
            PairingEvent::Paired(_)
            | PairingEvent::HomeKitPaired(_)
            | PairingEvent::PairedUnverified(_)
                if self.forgotten_while_ending.contains(id) =>
            {
                if let Some(key) = self.homekit_keys.remove(id) {
                    self.link.forget_homekit(&key);
                }
                // Not its end: the Failed that follows ends it.
                if matches!(event, PairingEvent::PairedUnverified(_)) {
                    return false;
                }
                PairingEvent::Stopped
            }
            event => event,
        };
        let paired = match event {
            // Its PIN is no one's to type now.
            PairingEvent::PinRequested | PairingEvent::PinTurnedDown(_) => return false,
            PairingEvent::Paired(credentials) => {
                self.keep_credentials(id, credentials);
                true
            }
            PairingEvent::HomeKitPaired(credentials) => {
                self.keep_homekit(id, credentials);
                true
            }
            PairingEvent::PairedUnverified(credentials) => {
                self.keep_homekit(id, credentials);
                return true;
            }
            // A wrong PIN starts nothing again: the user cancelled it. A
            // session-only pairing left with its connection.
            PairingEvent::PairedForSession
            | PairingEvent::Failed { .. }
            | PairingEvent::Stopped => false,
        };
        self.ending.remove(id);
        self.forgotten_while_ending.remove(id);
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
        self.paired_by_user.insert(id.to_owned());
        self.entry(id).enabled = true;
        self.reconcile();
        events.push(BackendEvent::AirPlayPaired { id: id.to_owned() });
    }

    /// Stores the credentials a pairing with the speaker `id` gave.
    fn keep_credentials(&mut self, id: &str, credentials: Credentials) {
        self.credentials_turned_down.remove(id);
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
                device.choose_path_preferring(true, self.link.airplay2_preferred(), None, false)
                    == Ok(ProtocolPath::AirPlay1)
                    && device.choose_path(true, Some(kind)) == Ok(ProtocolPath::AirPlay2)
            });
        if moved {
            self.airplay1_outcome.insert(id.to_owned(), kind);
        }
    }

    /// Forgets what AirPlay 1 said to the row `id` that left the sender,
    /// and to its members that didn't join on their own.
    fn forget_outcomes(&mut self, id: &str, added: &Added) {
        self.airplay1_outcome.remove(id);
        for member in added.members.keys() {
            if !self.added.contains_key(member) {
                self.airplay1_outcome.remove(member);
            }
        }
        self.forget_reported(id, added);
    }

    /// Drops the R_s of the row `id` that left the sender, and of its
    /// members, once discovery doesn't list them either (S280).
    fn forget_reported(&mut self, id: &str, added: &Added) {
        for left in std::iter::once(id).chain(added.members.keys().map(String::as_str)) {
            if !self.listed.contains_key(left) && !self.in_sender(left) {
                self.reported_us.remove(left);
            }
        }
    }

    /// Lists the speaker `id` as needing a PIN after an answer turned its
    /// stored `what` down. They stay stored: a reset receiver and an
    /// impostor at its address answer alike, so only a new pairing replaces
    /// them (P154).
    fn turn_down_credentials(&mut self, id: &str, what: &str) {
        if self.credentials_turned_down.insert(id.to_owned()) {
            self.log.log(|| {
                format!(
                    "AirPlay pairing {}: the speaker turned the stored {what} down; they stay until a new pairing replaces them",
                    self.label(id)
                )
            });
        }
    }

    /// Keeps the AirPlay 2 credentials a pairing with the speaker `id` gave
    /// (P152); when the file can't be written they last until AudioSync
    /// quits.
    fn keep_homekit(&mut self, id: &str, credentials: HomeKitCredentials) {
        self.pin_seen.remove(id);
        self.credentials_turned_down.remove(id);
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
                if !self.listed.contains_key(&id)
                    && let Some(address) = first_address(&info.device)
                    && self.listed_at(address) >= MAX_LISTED_PER_ADDRESS
                {
                    if self.crowded.insert(address) {
                        self.log.log(|| {
                            format!(
                                "AirPlay receivers at {address} past the first {MAX_LISTED_PER_ADDRESS} not listed"
                            )
                        });
                    }
                    return;
                }
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
                // The crate reports R_s once, at first sound, so a speaker
                // still in the sender keeps it (S280).
                if !self.in_sender(&id) {
                    self.reported_us.remove(&id);
                }
                if self.listed.remove(&id).is_some() {
                    self.log
                        .log(|| format!("AirPlay receiver gone: {}", self.label(&id)));
                }
                self.reconcile();
                self.forget_name_unless_used(&id);
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
                } else if let Some(target) = self.targets.remove(&id) {
                    self.log
                        .log(|| format!("AirPlay target gone: {}", self.label(&id)));
                    self.forget_name_unless_used(&id);
                    for member in &target.members {
                        self.forget_name_unless_used(&speaker_id(&member.key));
                    }
                }
            }
        }
    }

    /// How many listed receivers announce `address` first.
    fn listed_at(&self, address: IpAddr) -> usize {
        self.listed
            .values()
            .filter(|listed| first_address(listed.device()) == Some(address))
            .count()
    }

    /// Drops the name of a speaker or target that left discovery, unless
    /// a setting, session, prompt, pairing, zone or target still names it:
    /// otherwise every record a LAN host announces would stay until quit.
    fn forget_name_unless_used(&mut self, id: &str) {
        let used = self.listed.contains_key(id)
            || self.settings.contains_key(id)
            || self.added.contains_key(id)
            || self.added_target_of(id).is_some()
            || self.targets.contains_key(id)
            || self.targets.values().any(|target| {
                target
                    .members
                    .iter()
                    .any(|member| speaker_id(&member.key) == id)
            })
            || self.pairings.contains_key(id)
            || self.ending.contains(id)
            || self.password_asks.contains_key(id)
            || self.passwords.contains_key(id)
            || self.homekit_keys.contains_key(id)
            || self.zones.contains_key(id);
        if !used {
            self.names.remove(id);
        }
    }

    /// Whether the speaker `id` is in the sender, alone or in a target.
    fn in_sender(&self, id: &str) -> bool {
        self.added.contains_key(id) || self.added_target_of(id).is_some()
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
                SpeakerState::Unannounced => "waiting until it is announced again".to_owned(),
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
        let was_unannounced = added.state == SpeakerState::Unannounced;
        added.state = state;
        if matches!(state, SpeakerState::Playing | SpeakerState::Unannounced) {
            added.retried = 0;
        }
        let mut relisted = false;
        if state == SpeakerState::Playing {
            // Its password worked: one turned down later turns it off.
            self.password_attempts.remove(id);
            self.paired_in_attempt.remove(id);
            relisted =
                self.playing_proves_credentials(id) && self.credentials_turned_down.remove(id);
        }
        if state == SpeakerState::Playing && !was_playing {
            events.push(BackendEvent::AirPlayPlaying { id: id.to_owned() });
        } else if was_unannounced != (state == SpeakerState::Unannounced) {
            events.push(BackendEvent::AirPlayUnannounced {
                id: id.to_owned(),
                unannounced: !was_unannounced,
            });
        }
        relisted || was_playing != (state == SpeakerState::Playing)
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
                    if added.reconnecting() {
                        events.push(BackendEvent::AirPlayTrouble {
                            id: target.to_owned(),
                            problem,
                        });
                    }
                } else if problem.waits() {
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
            self.forget_outcomes(target, &added);
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

    /// Whether the listed speaker `id` asked for nothing: no PIN or password
    /// in its flags and no stored AirPlay 2 credentials, so a PairingRequired
    /// from it came before any pairing step and its notice names no code.
    fn refused_unasked(&self, id: &str) -> bool {
        self.store.homekit(id).is_none()
            && self.listed.get(id).is_some_and(|listed| {
                let verdict = &listed.info.device.verdict;
                !verdict.needs_password && !verdict.needs_pairing
            })
    }

    /// Returns whether the speaker list changed.
    fn speaker_event(
        &mut self,
        id: &str,
        event: SpeakerEvent,
        events: &mut Vec<BackendEvent>,
    ) -> bool {
        let path_changed = match &event {
            SpeakerEvent::Path(path) => return self.note_session_path(id, Some(*path)),
            SpeakerEvent::Error(_)
            | SpeakerEvent::State(SpeakerState::Stopped | SpeakerState::Failed) => {
                self.note_session_path(id, None)
            }
            _ => false,
        };
        self.session_event(id, event, events) || path_changed
    }

    /// Keeps the path a session of `id` takes, or forgets it once the
    /// session ends (`None`). Returns whether its row's path changed.
    fn note_session_path(&mut self, id: &str, path: Option<ProtocolPath>) -> bool {
        let before = self.over_airplay2(id);
        match path {
            Some(path) => self.session_paths.insert(id.to_owned(), path),
            None => self.session_paths.remove(id),
        };
        before != self.over_airplay2(id)
    }

    /// Takes in any other event of a session. Returns whether the speaker
    /// list changed.
    fn session_event(
        &mut self,
        id: &str,
        event: SpeakerEvent,
        events: &mut Vec<BackendEvent>,
    ) -> bool {
        // P184 stops reads for one session only: the next one is asked again.
        if let SpeakerEvent::State(state) = &event
            && *state != SpeakerState::Playing
            && let Some(row) = self.row_of(id)
            && let Some(added) = self.added.get_mut(&row)
        {
            added.read.unreadable = false;
        }
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
                self.paired_for_session.remove(id);
                self.note_path_switch(id, error.kind, retry_at.is_some());
                // Outside the user's attempt its row keeps no code request:
                // switching it on starts one, which pairs it.
                let unasked = problem == AirPlayProblem::PairingNotYet
                    && self.over_airplay2(id)
                    && !self.password_attempts.contains(id)
                    && self.refused_unasked(id);
                // A speaker reached over AirPlay 2 that wants a pairing:
                // switching it on pairs it with a PIN, or with its password
                // when it asks for one (P61, P154).
                if problem == AirPlayProblem::PairingNotYet && self.over_airplay2(id) {
                    if self.store.homekit(id).is_some() {
                        self.turn_down_credentials(id, "AirPlay 2 credentials");
                    }
                    if self.asks_password_only(id) {
                        problem = AirPlayProblem::NeedsPassword;
                    } else {
                        problem = AirPlayProblem::NeedsPin;
                        if !unasked {
                            self.pin_seen.insert(id.to_owned());
                        }
                    }
                } else if problem == AirPlayProblem::PairingNotYet && self.store.contains(id) {
                    // Switching it on pairs it again (P154).
                    problem = AirPlayProblem::NeedsPin;
                    self.turn_down_credentials(id, "credentials");
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
                if problem == AirPlayProblem::NeedsPin
                    && self.over_airplay2(id)
                    && self.password_attempts.contains(id)
                    && !self.paired_in_attempt.contains(id)
                    && self.added.contains_key(id)
                {
                    return self.pair_in_attempt(id, events);
                }
                if problem == AirPlayProblem::NeedsPassword
                    && self.password_attempts.contains(id)
                    && let Some(added) = self.added.remove(id)
                {
                    // The user's switch-on asks for it; the speaker stays on
                    // meanwhile.
                    added.remove(self.link.as_mut());
                    self.airplay1_outcome.remove(id);
                    self.forget_reported(id, &added);
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
                    if !self.listed.contains_key(id) {
                        // It played on after its announcement lapsed: it
                        // stays in the sender, with no retry notice for a
                        // speaker the list no longer shows.
                        return true;
                    }
                    if added.reconnecting() {
                        events.push(BackendEvent::AirPlayTrouble {
                            id: id.to_owned(),
                            problem,
                        });
                    }
                } else if problem.waits() {
                    // The sender starts it once it has a place or PTP is free.
                    events.push(BackendEvent::AirPlayTrouble {
                        id: id.to_owned(),
                        problem,
                    });
                } else if unasked {
                    self.turn_off(id, AirPlayProblem::NotAccepted, events);
                } else {
                    self.turn_off(id, problem, events);
                }
                true
            }
            SpeakerEvent::Warning(warning) => {
                self.log
                    .log(|| format!("AirPlay speaker {} warning: {warning:?}", self.label(id)));
                let unreadable = matches!(
                    warning,
                    Warning::RequestFailed {
                        method: Method::GetParameter,
                        ..
                    } | Warning::VolumeReadsStopped
                );
                if unreadable
                    && let Some(row) = self.row_of(id)
                    && let Some(added) = self.added.get_mut(&row)
                {
                    added.read.unreadable = true;
                    added.read.reading = false;
                }
                if warning == Warning::PtpUnavailable {
                    events.push(BackendEvent::AirPlayPtpPortsTaken);
                }
                false
            }
            // A bind that worked proves the ports free, whether or not this
            // session ever plays.
            SpeakerEvent::PtpOpened => {
                events.push(BackendEvent::AirPlayPtpFree);
                false
            }
            SpeakerEvent::VolumeReported(db) => self.volume_reported(id, db),
            SpeakerEvent::Latency(latency) => {
                self.log.log(|| {
                    format!(
                        "AirPlay speaker {} latency: L + d_s = {} ms, {}, first sound in {} ms",
                        self.label(id),
                        latency.arrival_to_sound.as_millis(),
                        describe_reported(
                            latency.receiver_latency_us,
                            latency.receiver_latency_sources
                        ),
                        latency
                            .first_sound
                            .saturating_duration_since(Instant::now())
                            .as_millis()
                    )
                });
                self.take_reported(id, latency.receiver_latency_us);
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
            SpeakerEvent::TimingRemembered(memory) => {
                let timing = memory.timing;
                let saved = self.store.keep_timing(id, memory);
                self.log.log(|| {
                    let saved = match &saved {
                        Ok(()) => String::new(),
                        Err(error) => format!(" for this run only ({error})"),
                    };
                    format!(
                        "AirPlay speaker {}: {timing:?} timing kept{saved}",
                        self.label(id)
                    )
                });
                false
            }
            SpeakerEvent::TimingForgotten => {
                let _ = self.store.forget_timing(id);
                self.log
                    .log(|| format!("AirPlay speaker {}: timing entry forgotten", self.label(id)));
                false
            }
            SpeakerEvent::Trace(line) => {
                self.log
                    .log(|| format!("AirPlay speaker {} trace: {line}", self.label(id)));
                false
            }
            SpeakerEvent::Methods(_) | SpeakerEvent::AudioJack(_) | SpeakerEvent::Path(_) => false,
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
                text.push_str(&describe_speaker_counters(counters));
            }
        }
        Some(text)
    }
}

/// A speaker's part of the AirPlay status lines, as read from the counters
/// its sender keeps without locks.
fn describe_speaker_counters(counters: &SpeakerCounters) -> String {
    format!(
        "; {} packets sent, {} late, {} resend requests, {} resent, {} not held, \
         {} timing replies, {} datagrams from other addresses dropped, {} silent frames",
        counters.packets_sent(),
        counters.late_packets(),
        counters.resend_requests(),
        counters.packets_resent(),
        counters.not_held(),
        counters.timing_replies(),
        counters.foreign_datagrams(),
        counters.silent_frames(),
    )
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
    use audiosync_airplay::stream::{SpeakerLatency, Stream, StreamConfig};
    use audiosync_airplay::targets::Member;
    use audiosync_airplay::volume::parse_reported;

    use super::*;
    use crate::audio::backend::{AudioBackend, SourceId};
    use crate::audio::explicit_pause::STALE_READING;
    use crate::audio::mock::MockBackend;
    use audiosync_core::volume::row_level;

    #[test]
    fn a_speaker_status_ends_with_the_silent_frames_its_packets_carried() {
        // The sender counts them (its own tests drive that); the status
        // appends them after the fields it always had, in their order.
        assert_eq!(
            describe_speaker_counters(&SpeakerCounters::default()),
            "; 0 packets sent, 0 late, 0 resend requests, 0 resent, 0 not held, \
             0 timing replies, 0 datagrams from other addresses dropped, 0 silent frames"
        );
    }

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
        let why = "the receiver turned the PIN down: error 2 (authentication) in answer to M3";
        assert_eq!(
            pairing_summary(&PairingEvent::PinTurnedDown(why.to_owned())),
            why
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
    /// PIN pairing once.
    pub fn airplay2_device(id: u8, name: &str, pin: bool) -> Device {
        airplay2_flagged(id, name, if pin { 0x200 } else { 0 })
    }

    /// A TV whose flags ask for a code every time (bit 3).
    pub fn airplay2_every_time_tv(id: u8, name: &str) -> Device {
        airplay2_flagged(id, name, 0x4c)
    }

    fn airplay2_flagged(id: u8, name: &str, flags: u64) -> Device {
        let device_id = DeviceId([0x02, 0x00, 0x5E, 0x00, 0x53, id]);
        let record = ServiceRecord {
            kind: ServiceKind::AirPlay,
            instance: name.to_owned(),
            host: format!("speaker-{id}.local"),
            port: 7000,
            txt: vec![
                format!("deviceid={device_id}").into_bytes(),
                b"features=0x0,0x40".to_vec(),
                format!("flags={flags:#x}").into_bytes(),
            ],
            addresses: vec![ScopedAddr::new(IpAddr::V4(Ipv4Addr::new(192, 0, 2, 13)))],
        };
        Device::from_records(DeviceKey::Id(device_id), None, Some(&record)).expect("a device")
    }

    /// A receiver that speaks both: an open RAOP service and an AirPlay
    /// service that asks for nothing.
    pub fn dual_device(id: u8, name: &str) -> Device {
        let device_id = DeviceId([0x02, 0x00, 0x5E, 0x00, 0x53, id]);
        let address = vec![ScopedAddr::new(IpAddr::V4(Ipv4Addr::new(192, 0, 2, 14)))];
        let raop = ServiceRecord {
            kind: ServiceKind::Raop,
            instance: format!("{device_id}@{name}"),
            host: format!("speaker-{id}.local"),
            port: 7000,
            txt: OPEN_TXT.iter().map(|s| s.as_bytes().to_vec()).collect(),
            addresses: address.clone(),
        };
        let airplay = ServiceRecord {
            kind: ServiceKind::AirPlay,
            instance: name.to_owned(),
            host: format!("speaker-{id}.local"),
            port: 7000,
            txt: vec![
                format!("deviceid={device_id}").into_bytes(),
                b"features=0x0,0x40".to_vec(),
                b"flags=0x0".to_vec(),
            ],
            addresses: address,
        };
        Device::from_records(DeviceKey::Id(device_id), Some(&raop), Some(&airplay))
            .expect("a device")
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
        KeepVolume(String, Volume),
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
        Pause,
        Resume,
        /// Lf, in frames.
        Latency(u32),
        NowPlaying(NowPlaying),
        /// The speaker added just before came with credentials of this
        /// client ID.
        Credentials(String, String),
        /// The speaker added just before came with AirPlay 2 credentials.
        HomeKit(String),
        /// The speaker added or the pairing started just before came with
        /// this password.
        Password(String, String),
        /// The speaker added just before may use PCM.
        Pcm(String),
        /// The speaker added just before came with this stored timing entry
        /// (P217).
        Timing(String, audiosync_airplay::ap2::Timing),
        StartPairing(String),
        SubmitPin(String, String),
        CancelPairing(String),
        ForgetHomeKit(String),
    }

    #[derive(Default)]
    struct FakeState {
        calls: Vec<Call>,
        /// Each volume read asked: a speaker ID, or `target ` and a target ID.
        reads: Vec<String>,
        events: VecDeque<SenderEvent>,
        latency: Duration,
        /// A speaker of the stream reached its first-sound time.
        sounding: bool,
        /// The sender stopped after an internal error.
        stopped: bool,
        /// The AirPlay 2 switch.
        airplay2: bool,
        /// The AirPlay 2 preference.
        airplay2_preferred: bool,
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

        /// Sets the AirPlay 2 preference (P214) for the links made after.
        pub fn set_airplay2_preferred(&self, on: bool) {
            self.0.borrow_mut().airplay2_preferred = on;
        }

        /// A link for the backend.
        pub fn link(&self) -> Box<dyn AirPlayLink> {
            Box::new(self.clone())
        }

        /// Every link handed out was dropped, which tears the sender down.
        #[cfg_attr(not(windows), allow(dead_code))]
        pub fn links_dropped(&self) -> bool {
            Rc::strong_count(&self.0) == 1
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

        /// [`FakeAirPlay::fail`], and its retries at 1 s and 3 s fail too.
        pub fn lose(&self, device: &Device, kind: ErrorKind) {
            for _ in 0..3 {
                self.fail(device, kind);
            }
        }

        /// A pair-verify error about the receiver's state ended a session:
        /// the sender reports a refusal and retries after the wait (P154).
        pub fn refuse_for_state(&self, device: &Device) {
            let mut error = SessionError::new(ErrorKind::PairingFailed);
            error.retry_after = Some(Duration::from_secs(5));
            self.push(SenderEvent::Speaker {
                key: device.key.clone(),
                event: SpeakerEvent::Error(SpeakerError {
                    error,
                    reason: UserReason::Refused,
                    retry_at: Some(Instant::now() + Duration::from_secs(5)),
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

        pub fn take_reads(&self) -> Vec<String> {
            std::mem::take(&mut self.0.borrow_mut().reads)
        }

        /// A speaker of the stream sounds now, or none does.
        pub fn set_sounding(&self, sounding: bool) {
            self.0.borrow_mut().sounding = sounding;
        }

        /// `device` reported its latency: `reported_us` from `sources`.
        pub fn report_latency(&self, device: &Device, reported_us: u64, sources: LatencySources) {
            self.push(SenderEvent::Speaker {
                key: device.key.clone(),
                event: SpeakerEvent::Latency(SpeakerLatency {
                    arrival_to_sound: Duration::from_millis(1750),
                    receiver_latency_frames: None,
                    receiver_latency_us: reported_us,
                    receiver_latency_sources: sources,
                    first_sound: Instant::now() + Duration::from_secs(1),
                }),
            });
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
            if options.pcm {
                state.calls.push(Call::Pcm(id_of(device)));
            }
            if let Some(memory) = &options.timing {
                state.calls.push(Call::Timing(id_of(device), memory.timing));
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

        fn keep_volume(&mut self, key: &DeviceKey, volume: Volume) {
            self.0
                .borrow_mut()
                .calls
                .push(Call::KeepVolume(speaker_id(key), volume));
        }

        fn read_volume(&mut self, key: &DeviceKey) {
            self.0.borrow_mut().reads.push(speaker_id(key));
        }

        fn read_target_volume(&mut self, id: &TargetId) {
            self.0.borrow_mut().reads.push(format!("target {id}"));
        }

        fn set_delay(&mut self, key: &DeviceKey, delay: Duration) {
            self.0
                .borrow_mut()
                .calls
                .push(Call::Delay(speaker_id(key), delay));
        }

        fn pause(&mut self) {
            self.0.borrow_mut().calls.push(Call::Pause);
        }

        fn resume(&mut self) {
            self.0.borrow_mut().calls.push(Call::Resume);
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

        fn set_latency(&mut self, frames: u32) {
            self.0.borrow_mut().calls.push(Call::Latency(frames));
        }

        fn sounding(&self) -> bool {
            self.0.borrow().sounding
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

        fn airplay2_preferred(&self) -> bool {
            self.0.borrow().airplay2_preferred
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
                ("Study", AirPlayAvailability::NeedsPassword, false),
            ]
        );
        assert_eq!(listed[0].id, "02005E005301");
        fake.gone(&kitchen);
        airplay.poll();
        assert_eq!(airplay.speakers().len(), 1);
    }

    // covers: S166, S147
    #[test]
    fn long_names_that_differ_only_at_the_end_sort_whole_and_stay_unchanged() {
        let fake = FakeAirPlay::new();
        let mut airplay = speakers(&fake);
        let long = "Living Room Speaker With A Very Long Name ".repeat(6) + "End";
        let (a, b) = (format!("{long} A"), format!("{long} B"));
        assert_eq!(a.chars().count(), 257);
        fake.appear(&device(1, &b, OPEN_TXT));
        fake.appear(&device(2, &a, OPEN_TXT));
        fake.appear(&device(3, "Den", OPEN_TXT));
        airplay.poll();
        let names: Vec<String> = airplay.speakers().into_iter().map(|s| s.name).collect();
        assert_eq!(names, ["Den".to_owned(), a, b]);
    }

    #[test]
    fn a_gone_speaker_keeps_its_name_only_while_something_refers_to_it() {
        let fake = FakeAirPlay::new();
        let mut airplay = speakers(&fake);
        let forged: Vec<Device> = (1..=50)
            .map(|n| device(100 + n, &format!("Forged {n}"), OPEN_TXT))
            .collect();
        let kitchen = device(1, "Kitchen", OPEN_TXT);
        for speaker in forged.iter().chain([&kitchen]) {
            fake.appear(speaker);
        }
        airplay.poll();
        airplay.set_delay(&id_of(&kitchen), 40.0);
        for speaker in forged.iter().chain([&kitchen]) {
            fake.gone(speaker);
        }
        airplay.poll();
        assert!(
            forged
                .iter()
                .all(|speaker| airplay.listed_name(&id_of(speaker)).is_none())
        );
        assert_eq!(airplay.listed_name(&id_of(&kitchen)), Some("Kitchen"));
    }

    #[test]
    fn one_address_lists_at_most_its_share_of_speakers() {
        let path = crate::debug_log::test_dir("airplay-crowded-address").join("log.txt");
        let fake = FakeAirPlay::new();
        let mut airplay = speakers(&fake);
        airplay.set_debug_log(DebugLog::new(Some(path.clone())));
        let flood = IpAddr::V4(Ipv4Addr::new(192, 0, 2, 66));
        let announced: Vec<Device> = (1..=MAX_LISTED_PER_ADDRESS as u8 + 6)
            .map(|n| device_at(n, &format!("Forged {n}"), OPEN_TXT, flood, 7000))
            .collect();
        for speaker in &announced {
            fake.appear(speaker);
        }
        let kitchen = device(200, "Kitchen", OPEN_TXT);
        fake.appear(&kitchen);
        airplay.poll();
        let listed = airplay.speakers();
        assert_eq!(listed.len(), MAX_LISTED_PER_ADDRESS + 1);
        assert!(listed.iter().any(|speaker| speaker.name == "Kitchen"));
        // A listed one that changes keeps its row; the unlisted ones that
        // announce again stay out and log nothing new.
        fake.change(&device_at(1, "Renamed", OPEN_TXT, flood, 7000));
        for speaker in &announced[MAX_LISTED_PER_ADDRESS..] {
            fake.change(speaker);
        }
        airplay.poll();
        let listed = airplay.speakers();
        assert_eq!(listed.len(), MAX_LISTED_PER_ADDRESS + 1);
        assert!(listed.iter().any(|speaker| speaker.name == "Renamed"));
        assert!(airplay.log.flush(crate::debug_log::TEST_FLUSH));
        let crowded = crate::debug_log::read_messages(&path)
            .iter()
            .filter(|message| message.contains("192.0.2.66 past the first"))
            .count();
        assert_eq!(crowded, 1);
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
        // A wrong PIN: the receiver asks again on the same connection, with
        // the code it still shows (P150).
        fake.pairing(&tv, PairingEvent::PinRequested);
        airplay.poll();
        assert!(fake.take_calls().is_empty(), "no new pairing");
        let prompt = airplay.pin_prompt().unwrap();
        assert!(prompt.wrong_pin);
        assert_eq!(prompt.phase, PairingPhase::WaitingForPin);
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

    // covers: P153
    #[test]
    fn credentials_of_a_pin_pairing_whose_verify_failed_are_kept() {
        use crate::audio::pairings::tests::homekit;

        let tv = airplay2_tv(9, "Loft");
        let id = id_of(&tv);
        let fake = FakeAirPlay::new();
        fake.set_airplay2(true);
        let mut airplay = speakers(&fake);
        pin_shown(&fake, &mut airplay, &tv);
        assert!(airplay.submit_pin(&id, "4821"));
        fake.pairing(&tv, PairingEvent::PairedUnverified(homekit(0x33)));
        fake.pairing(
            &tv,
            PairingEvent::Failed {
                error: SessionError::new(ErrorKind::Lost),
                wrong_pin: false,
            },
        );
        let events = airplay.poll();
        let failed = BackendEvent::AirPlayFailed {
            id: id.clone(),
            problem: AirPlayProblem::PairingFailed,
        };
        assert!(events.contains(&failed), "{events:?}");
        assert_eq!(airplay.store.homekit(&id), Some(&homekit(0x33)));
        assert!(!airplay.speakers()[0].enabled);
    }

    /// A speaker under the id of device 12 whose RAOP record, with `raop`
    /// as its TXT, points at another host; with `airplay_flags`, next to
    /// the speaker's own AirPlay record with those status flags.
    fn den_with_forged_raop(raop: &[&str], airplay_flags: Option<&str>) -> Device {
        let id = 12;
        let device_id = DeviceId([0x02, 0x00, 0x5E, 0x00, 0x53, id]);
        let airplay = airplay_flags.map(|flags| ServiceRecord {
            kind: ServiceKind::AirPlay,
            instance: "Den".to_owned(),
            host: format!("speaker-{id}.local"),
            port: 7000,
            txt: vec![
                format!("deviceid={device_id}").into_bytes(),
                b"features=0x0,0x40".to_vec(),
                format!("flags={flags}").into_bytes(),
            ],
            addresses: vec![ScopedAddr::new(IpAddr::V4(Ipv4Addr::new(192, 0, 2, 13)))],
        });
        let forged_raop = ServiceRecord {
            kind: ServiceKind::Raop,
            instance: format!("{device_id}@Den"),
            host: format!("forger-{id}.local"),
            port: 5000,
            txt: raop.iter().map(|s| s.as_bytes().to_vec()).collect(),
            addresses: vec![ScopedAddr::new(IpAddr::V4(Ipv4Addr::new(192, 0, 2, 66)))],
        };
        Device::from_records(
            DeviceKey::Id(device_id),
            Some(&forged_raop),
            airplay.as_ref(),
        )
        .expect("a device")
    }

    /// A speaker with stored AirPlay 2 credentials shows nothing a RAOP
    /// record says, since any host can publish one under its id (P2).
    #[test]
    fn a_homekit_paired_speaker_never_shows_its_raop_record() {
        use crate::audio::pairings::tests::homekit;

        let den = den_with_forged_raop(&["et=0", "pw=true"], Some("0x0"));
        let id = id_of(&den);
        for airplay2 in [true, false] {
            let fake = FakeAirPlay::new();
            fake.set_airplay2(airplay2);
            let mut airplay = speakers(&fake);
            fake.appear(&den);
            airplay.poll();
            let row = &airplay.speakers()[0];
            assert!(!row.airplay2, "unpaired, AirPlay 1 serves it");
            assert_eq!(row.availability, AirPlayAvailability::NeedsPassword);

            airplay.store.open_airplay2();
            airplay.store.insert_homekit(&id, homekit(0x33)).unwrap();
            let row = &airplay.speakers()[0];
            assert_eq!(row.airplay2, airplay2);
            let expected = if airplay2 {
                AirPlayAvailability::Open
            } else {
                AirPlayAvailability::AirPlay2NotYet
            };
            assert_eq!(row.availability, expected);
        }
    }

    /// Asking for a code every time, by its own flags or by a forged RAOP
    /// `sf`, hides neither the pairing (P182) nor Forget Pairing, the one
    /// way out of the AirPlay 2 pin (P2).
    #[test]
    fn a_pinned_speaker_asking_for_a_code_every_time_keeps_forget_pairing() {
        use crate::audio::pairings::tests::homekit;

        for (raop, flags) in [
            (&["et=0", "pw=true"][..], "0x4c"),
            (&["et=0", "pw=true", "sf=0x8"][..], "0x0"),
        ] {
            let den = den_with_forged_raop(raop, Some(flags));
            let id = id_of(&den);
            let fake = FakeAirPlay::new();
            let mut airplay = speakers(&fake);
            airplay.store.open_airplay2();
            airplay.store.insert_homekit(&id, homekit(0x33)).unwrap();
            fake.appear(&den);
            airplay.poll();
            let row = &airplay.speakers()[0];
            assert!(row.paired, "{flags}: the flag alone turns nothing down");
            assert!(row.holds_credentials, "{flags}: Forget Pairing is on offer");
            assert!(!row.airplay2, "{flags}: the switch is off");
            assert_eq!(row.availability, AirPlayAvailability::AirPlay2NotYet);

            airplay.forget_pairing(&id);
            assert_eq!(airplay.store.homekit(&id), None);
            let row = &airplay.speakers()[0];
            assert!(!row.holds_credentials);
            assert_eq!(
                row.availability,
                AirPlayAvailability::NeedsPassword,
                "{flags}: unpinned, AirPlay 1 serves it as before"
            );
        }
    }

    /// With the switch on and only a RAOP record in view, a pinned speaker
    /// reads as its AirPlay 2 path, whatever the record's flags say (P2).
    #[test]
    fn a_pinned_speaker_with_only_a_raop_record_shows_its_airplay2_view() {
        use crate::audio::pairings::tests::homekit;

        let den = den_with_forged_raop(&["et=0", "pw=true", "sf=0x8"], None);
        let id = id_of(&den);
        let fake = FakeAirPlay::new();
        fake.set_airplay2(true);
        let mut airplay = speakers(&fake);
        airplay.store.open_airplay2();
        airplay.store.insert_homekit(&id, homekit(0x33)).unwrap();
        fake.appear(&den);
        airplay.poll();
        let row = &airplay.speakers()[0];
        assert!(row.airplay2);
        assert!(row.holds_credentials);
        assert_eq!(row.availability, AirPlayAvailability::Open);
    }

    /// Loft paired with a PIN, played, and was switched off; the user
    /// switches it on again and it turns its stored key down.
    fn loft_turns_its_key_down_on_a_switch_on(fake: &FakeAirPlay, tv: &Device) -> AirPlaySpeakers {
        use crate::audio::pairings::tests::homekit;

        let id = id_of(tv);
        fake.set_airplay2(true);
        let mut airplay = speakers(fake);
        airplay.set_streaming(true);
        pin_shown(fake, &mut airplay, tv);
        assert!(airplay.submit_pin(&id, "4821"));
        fake.pairing(tv, PairingEvent::HomeKitPaired(homekit(0x33)));
        airplay.poll();
        fake.state(tv, SpeakerState::Playing);
        airplay.poll();
        assert_eq!(airplay.set_enabled(&id, false), Ok(()));

        assert_eq!(airplay.set_enabled(&id, true), Ok(()));
        airplay.begin_attempt(&id);
        fake.take_calls();
        fake.fail(tv, ErrorKind::PairingRequired);
        let events = airplay.poll();
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, BackendEvent::AirPlayFailed { .. })),
            "{events:?}"
        );
        let calls = fake.take_calls();
        assert!(
            calls.contains(&Call::StartPairing(id.clone())),
            "{calls:?} {events:?}"
        );
        assert!(!airplay.speakers()[0].enabled, "off while it pairs");
        fake.pairing(tv, PairingEvent::PinRequested);
        airplay.poll();
        assert_eq!(airplay.pin_prompt().unwrap().id, id);
        airplay
    }

    #[test]
    fn an_airplay2_speaker_that_turns_its_key_down_pairs_again_in_the_same_attempt() {
        use crate::audio::pairings::tests::homekit;

        let tv = airplay2_tv(9, "Loft");
        let id = id_of(&tv);
        let fake = FakeAirPlay::new();
        let mut airplay = loft_turns_its_key_down_on_a_switch_on(&fake, &tv);
        assert!(airplay.submit_pin(&id, "4821"));
        fake.pairing(&tv, PairingEvent::HomeKitPaired(homekit(0x44)));
        let events = airplay.poll();
        assert!(
            events.contains(&BackendEvent::AirPlayPaired { id: id.clone() }),
            "{events:?}"
        );
        assert!(airplay.speakers()[0].enabled);

        // Turned down again in the same attempt: no pairing loop, it stops
        // with the notice.
        fake.take_calls();
        fake.fail(&tv, ErrorKind::PairingRequired);
        let events = airplay.poll();
        assert!(
            events.contains(&BackendEvent::AirPlayFailed {
                id: id.clone(),
                problem: AirPlayProblem::NeedsPin,
            }),
            "{events:?}"
        );
        assert!(
            !fake
                .take_calls()
                .iter()
                .any(|call| matches!(call, Call::StartPairing(_)))
        );
    }

    #[test]
    fn a_tv_that_asks_every_time_plays_after_its_pin_and_stores_nothing() {
        use crate::audio::pairings::tests::homekit;

        let tv = airplay2_every_time_tv(9, "Den");
        let id = id_of(&tv);
        let fake = FakeAirPlay::new();
        fake.set_airplay2(true);
        let mut airplay = speakers(&fake);
        airplay.set_streaming(true);
        // Keys kept from before the TV was set to ask every time, which a
        // verify with them already had refused.
        airplay
            .store
            .insert_homekit(&id, homekit(0x5A))
            .expect("saved");
        airplay.credentials_turned_down.insert(id.clone());
        pin_shown(&fake, &mut airplay, &tv);
        let row = &airplay.speakers()[0];
        assert!(!row.paired);
        assert_eq!(row.availability, AirPlayAvailability::NeedsPin);
        assert!(airplay.submit_pin(&id, "4821"));
        fake.take_calls();

        fake.pairing(&tv, PairingEvent::PairedForSession);
        let events = airplay.poll();
        assert!(
            events.contains(&BackendEvent::AirPlayPaired { id: id.clone() }),
            "{events:?}"
        );
        assert_eq!(
            airplay.store.homekit(&id),
            Some(&homekit(0x5A)),
            "an answer during the pairing proves nothing about them"
        );
        let row = &airplay.speakers()[0];
        assert!(row.enabled && !row.paired);
        assert_eq!(row.availability, AirPlayAvailability::Open);
        assert!(
            fake.take_calls()
                .iter()
                .any(|call| matches!(call, Call::Add { id: added, .. } if *added == id)),
            "added for its session"
        );

        // Its session ends: the next one asks for a code again.
        fake.fail(&tv, ErrorKind::PairingRequired);
        airplay.poll();
        assert_eq!(
            airplay.speakers()[0].availability,
            AirPlayAvailability::NeedsPin
        );
    }

    // covers: S223
    #[test]
    fn an_every_time_pairing_failing_after_a_stop_and_start_before_a_poll_says_pairing_failed() {
        let tv = airplay2_every_time_tv(9, "Den");
        let id = id_of(&tv);
        let fake = FakeAirPlay::new();
        fake.set_airplay2(true);
        let mut airplay = speakers(&fake);
        airplay.set_streaming(true);
        pin_shown(&fake, &mut airplay, &tv);

        // The poll that takes in the ended stream runs once the new one has
        // started.
        airplay.set_streaming(false);
        airplay.set_streaming(true);
        airplay.poll();
        assert!(airplay.submit_pin(&id, "4821"));
        fake.pairing(&tv, pairing_failed(ErrorKind::PairingFailed, false));
        let events = airplay.poll();
        assert!(
            events.contains(&BackendEvent::AirPlayFailed {
                id: id.clone(),
                problem: AirPlayProblem::PairingFailed,
            }),
            "it failed within the new stream: {events:?}"
        );
    }

    // covers: S223
    #[test]
    fn a_tv_that_asks_every_time_needs_its_pin_again_on_a_new_sender() {
        let tv = airplay2_every_time_tv(9, "Den");
        let id = id_of(&tv);
        let paired = |fake: &FakeAirPlay| {
            fake.set_airplay2(true);
            let mut airplay = speakers(fake);
            airplay.set_streaming(true);
            pin_shown(fake, &mut airplay, &tv);
            assert!(airplay.submit_pin(&id, "4821"));
            fake.pairing(&tv, PairingEvent::PairedForSession);
            airplay.poll();
            assert_eq!(
                airplay.speakers()[0].availability,
                AirPlayAvailability::Open
            );
            airplay
        };
        let retried = |airplay: &mut AirPlaySpeakers, mut events: Vec<BackendEvent>| {
            let second = FakeAirPlay::new();
            second.set_airplay2(true);
            airplay.replace_link(second.link());
            second.appear(&tv);
            events.extend(airplay.poll());
            assert_eq!(
                airplay.speakers()[0].availability,
                AirPlayAvailability::NeedsPin
            );
            assert!(!airplay.speakers()[0].enabled, "left on it never joins");
            assert!(
                events.contains(&BackendEvent::AirPlayFailed {
                    id: id.clone(),
                    problem: AirPlayProblem::NeedsPin,
                }),
                "off with the notice: {events:?}"
            );
        };

        let fake = FakeAirPlay::new();
        let mut airplay = paired(&fake);
        fake.stop_running();
        let events = airplay.poll();
        retried(&mut airplay, events);

        let fake = FakeAirPlay::new();
        let mut airplay = paired(&fake);
        retried(&mut airplay, Vec::new());
    }

    #[test]
    fn a_pairing_in_the_attempt_that_fails_stops_with_the_notice() {
        let tv = airplay2_tv(9, "Loft");
        let id = id_of(&tv);
        let fake = FakeAirPlay::new();
        let mut airplay = loft_turns_its_key_down_on_a_switch_on(&fake, &tv);
        assert!(airplay.submit_pin(&id, "4821"));
        fake.pairing(&tv, pairing_failed(ErrorKind::PairingFailed, false));
        let events = airplay.poll();
        assert!(
            events.contains(&BackendEvent::AirPlayFailed {
                id: id.clone(),
                problem: AirPlayProblem::PairingFailed,
            }),
            "{events:?}"
        );
        assert_eq!(airplay.pin_prompt(), None);
        assert!(!airplay.speakers()[0].enabled);
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
    fn a_receiver_turning_its_airplay2_credentials_down_keeps_the_senders_key() {
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
        fake.fail(&tv, ErrorKind::PairingRequired);
        airplay.poll();
        assert!(airplay.homekit_keys.contains_key(&id));
        assert_eq!(airplay.store.homekit(&id), Some(&homekit(0x33)));
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

    // covers: S224
    #[test]
    fn an_airplay2_file_forget_couldnt_write_is_reported_after_the_airplay1_one_was() {
        use crate::audio::pairings::tests::{credentials, homekit, sealer};
        use crate::audio::pairings::{AIRPLAY2_FILE_NAME, FILE_NAME};

        let folder = crate::debug_log::test_dir("airplay2-forget-unsaved-both");
        let tv = airplay2_tv(9, "Loft");
        let id = id_of(&tv);
        let (mut store, _) = PairingStore::open(folder.join(FILE_NAME), sealer());
        store.open_airplay2();
        store.insert_homekit(&id, homekit(0x44)).expect("saved");
        store
            .insert(&id, credentials("0123456789ABCDEF", 2))
            .expect("saved");
        let fake = FakeAirPlay::new();
        fake.set_airplay2(true);
        let mut airplay = AirPlaySpeakers::new(fake.link(), 100.0, DebugLog::default(), store);
        // A folder where the AirPlay 2 file goes: nothing can be written there.
        let file = folder.join(AIRPLAY2_FILE_NAME);
        std::fs::remove_file(&file).expect("the file is removed");
        std::fs::create_dir(&file).expect("the folder is made");
        airplay.forget_pairing(&id);
        let events = airplay.poll();
        assert!(
            events.contains(&BackendEvent::PairingStore { saved: false }),
            "{events:?}"
        );
    }

    // covers: S224
    #[test]
    fn a_forgotten_speakers_identity_copy_that_cant_be_deleted_is_reported() {
        use crate::audio::pairings::tests::{failing_copy_deletes, homekit, sealer};
        use crate::audio::pairings::{AIRPLAY2_FILE_NAME, FILE_NAME};

        let folder = crate::debug_log::test_dir("airplay2-forget-copy-held");
        let tv = airplay2_tv(9, "Loft");
        let id = id_of(&tv);
        let (mut store, _) = PairingStore::open(folder.join(FILE_NAME), sealer());
        store.open_airplay2();
        store.insert_homekit(&id, homekit(0x44)).expect("saved");
        let file = folder.join(AIRPLAY2_FILE_NAME);
        let bytes = std::fs::read(&file).expect("written");
        crate::data_folder::keep_bytes_aside(&file, "airplay2-identity.ignored.dat", &bytes)
            .expect("kept");
        failing_copy_deletes(&mut store);
        let fake = FakeAirPlay::new();
        fake.set_airplay2(true);
        let mut airplay = AirPlaySpeakers::new(fake.link(), 100.0, DebugLog::default(), store);
        airplay.forget_pairing(&id);
        let events = airplay.poll();
        assert!(
            events.contains(&BackendEvent::PairingStore { saved: false }),
            "{events:?}"
        );
    }

    // covers: S224
    #[test]
    fn a_pairings_copy_forget_cant_delete_is_logged_apart_from_the_written_file() {
        use crate::audio::pairings::tests::{credentials, failing_copy_deletes, sealer};
        use crate::audio::pairings::{FILE_NAME, IGNORED_FILE_NAME};

        let folder = crate::debug_log::test_dir("airplay-forget-copy-held");
        let tv = device(6, "Den TV", APPLE_TV_TXT);
        let id = id_of(&tv);
        let file = folder.join(FILE_NAME);
        let (mut store, _) = PairingStore::open(file.clone(), sealer());
        store
            .insert(&id, credentials("0123456789ABCDEF", 2))
            .expect("saved");
        let bytes = std::fs::read(&file).expect("written");
        crate::data_folder::keep_bytes_aside(&file, IGNORED_FILE_NAME, &bytes).expect("kept");
        failing_copy_deletes(&mut store);
        let path = folder.join("debug.log");
        let log = DebugLog::new(Some(path.clone()));
        let fake = FakeAirPlay::new();
        let mut airplay = AirPlaySpeakers::new(fake.link(), 100.0, log.clone(), store);
        airplay.forget_pairing(&id);
        assert!(log.flush(Duration::from_secs(5)));
        let text = crate::debug_log::read_messages(&path).join("\n");
        assert!(text.contains("credentials forgotten"), "{text}");
        assert!(text.contains("couldn't be deleted"), "{text}");
        assert!(!text.contains("couldn't be written"), "{text}");
        let events = airplay.poll();
        assert!(
            events.contains(&BackendEvent::PairingStore { saved: false }),
            "{events:?}"
        );
    }

    // covers: S224
    #[test]
    fn forgetting_with_the_airplay2_switch_off_drops_the_airplay2_credentials_on_disk() {
        use crate::audio::pairings::FILE_NAME;
        use crate::audio::pairings::tests::{credentials, homekit, sealer};
        use audiosync_airplay::ap2::Timing;
        use audiosync_airplay::sender::TimingMemory;

        let folder = crate::debug_log::test_dir("airplay2-forget-switched-off");
        let tv = airplay2_tv(9, "Loft");
        let id = id_of(&tv);
        let (mut store, _) = PairingStore::open(folder.join(FILE_NAME), sealer());
        store.open_airplay2();
        store.insert_homekit(&id, homekit(0x44)).expect("saved");
        store
            .keep_timing(&id, TimingMemory::of(&tv, Timing::Ptp))
            .expect("saved");
        store
            .insert(&id, credentials("0123456789ABCDEF", 2))
            .expect("saved");
        drop(store);

        let fake = FakeAirPlay::new();
        fake.set_airplay2(false);
        let (store, _) = PairingStore::open(folder.join(FILE_NAME), sealer());
        let mut airplay = AirPlaySpeakers::new(fake.link(), 100.0, DebugLog::default(), store);
        airplay.forget_pairing(&id);
        drop(airplay);

        let (mut again, _) = PairingStore::open(folder.join(FILE_NAME), sealer());
        again.open_airplay2();
        assert_eq!(
            again.homekit(&id),
            None,
            "no PIN-less pairing once it is on again"
        );
        assert_eq!(again.timing(&id), None);
    }

    // Forget Pairing stops the pairing while its verify step still runs:
    // the credentials it reports after that are the forgotten pairing's.
    // covers: S224
    #[test]
    fn credentials_a_pairing_ending_after_a_forget_reports_are_dropped() {
        use crate::audio::pairings::FILE_NAME;
        use crate::audio::pairings::tests::{homekit, sealer};

        for (name, late) in [
            (
                "airplay2-forget-while-ending",
                vec![PairingEvent::HomeKitPaired(homekit(0x44))],
            ),
            (
                "airplay2-forget-while-ending-unverified",
                vec![
                    PairingEvent::PairedUnverified(homekit(0x44)),
                    pairing_failed(ErrorKind::Lost, false),
                ],
            ),
        ] {
            let folder = crate::debug_log::test_dir(name);
            let fake = FakeAirPlay::new();
            fake.set_airplay2(true);
            let (store, _) = PairingStore::open(folder.join(FILE_NAME), sealer());
            let mut airplay = AirPlaySpeakers::new(fake.link(), 100.0, DebugLog::default(), store);
            let tv = airplay2_tv(9, "Loft");
            let id = id_of(&tv);
            pin_shown(&fake, &mut airplay, &tv);
            assert!(airplay.submit_pin(&id, "4821"));
            airplay.forget_pairing(&id);
            fake.take_calls();
            for event in late {
                fake.pairing(&tv, event);
            }
            airplay.poll();
            assert_eq!(airplay.store.homekit(&id), None, "{name}");
            assert!(
                fake.take_calls().contains(&Call::ForgetHomeKit(id.clone())),
                "{name}: the sender's copy goes"
            );
            assert!(!airplay.homekit_keys.contains_key(&id), "{name}");
            assert!(!airplay.ending.contains(&id), "{name}");
            drop(airplay);
            let (mut again, _) = PairingStore::open(folder.join(FILE_NAME), sealer());
            again.open_airplay2();
            assert_eq!(again.homekit(&id), None, "{name}");
        }
    }

    // covers: P217
    #[test]
    fn a_timing_entry_reaches_the_next_launch_and_leaves_the_identity_file_alone() {
        use crate::audio::pairings::tests::sealer;
        use crate::audio::pairings::{AIRPLAY2_FILE_NAME, FILE_NAME};
        use audiosync_airplay::ap2::Timing;
        use audiosync_airplay::sender::TimingMemory;

        let folder = crate::debug_log::test_dir("airplay2-timing-relaunch");
        let speaker = airplay2_device(12, "Study", false);
        let id = id_of(&speaker);
        let launch = |fake: &FakeAirPlay| {
            fake.set_airplay2(true);
            let (store, _) = PairingStore::open(folder.join(FILE_NAME), sealer());
            AirPlaySpeakers::new(fake.link(), 100.0, DebugLog::default(), store)
        };

        let fake = FakeAirPlay::new();
        let mut airplay = launch(&fake);
        let identity = folder.join(AIRPLAY2_FILE_NAME);
        let before = std::fs::read(&identity).expect("the identity file");
        fake.push(SenderEvent::Speaker {
            key: speaker.key.clone(),
            event: SpeakerEvent::TimingRemembered(TimingMemory::of(&speaker, Timing::Ptp)),
        });
        airplay.poll();
        // An older AudioSync reads only identity format 1, and a newer one
        // would cost it every AirPlay 2 pairing.
        let after = std::fs::read(&identity).expect("the identity file");
        assert_eq!(after, before, "the identity file is left as it was");
        let plain = sealer().open(&after).expect("sealed by the test sealer");
        assert_eq!(
            plain[audiosync_core::airplay2_identity::MAGIC.len()],
            1,
            "identity format 1"
        );
        drop(airplay);

        let fake = FakeAirPlay::new();
        let mut airplay = launch(&fake);
        fake.appear(&speaker);
        airplay.poll();
        assert_eq!(airplay.set_enabled(&id, true), Ok(()));
        airplay.set_streaming(true);
        airplay.poll();
        let calls = fake.take_calls();
        assert!(
            calls.contains(&Call::Timing(id.clone(), Timing::Ptp)),
            "{calls:?}"
        );
    }

    // covers: P217
    #[test]
    fn forget_pairing_drops_the_timing_entry_of_a_speaker_without_airplay2_credentials() {
        use audiosync_airplay::ap2::Timing;
        use audiosync_airplay::sender::TimingMemory;

        let fake = FakeAirPlay::new();
        fake.set_airplay2(true);
        let mut airplay = speakers(&fake);
        let speaker = airplay2_device(13, "Porch", false);
        let id = id_of(&speaker);
        fake.appear(&speaker);
        fake.push(SenderEvent::Speaker {
            key: speaker.key.clone(),
            event: SpeakerEvent::TimingRemembered(TimingMemory::of(&speaker, Timing::Ptp)),
        });
        airplay.poll();
        airplay
            .passwords
            .insert(id.clone(), Secret::from("hunter2"));
        assert!(airplay.store.timing(&id).is_some());
        assert!(airplay.store.homekit(&id).is_none(), "a password only");
        fake.take_calls();

        airplay.forget_pairing(&id);
        assert!(airplay.store.timing(&id).is_none(), "the entry goes too");
        assert!(
            fake.take_calls().contains(&Call::ForgetHomeKit(id.clone())),
            "the sender drops its copy too"
        );
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
    fn a_stopped_or_new_sender_tries_airplay1_first_again() {
        let fake = FakeAirPlay::new();
        fake.set_airplay2(true);
        let mut airplay = speakers(&fake);
        let both = den(true);
        let id = id_of(&both);
        fake.appear(&both);
        airplay.poll();
        airplay.set_enabled(&id, true).unwrap();
        airplay.set_streaming(true);
        fake.fail(&both, ErrorKind::Refused);
        airplay.poll();
        assert!(airplay.speakers()[0].airplay2);

        fake.stop_running();
        airplay.poll();
        fake.run_again();
        fake.appear(&both);
        airplay.poll();
        assert!(
            !airplay.speakers()[0].airplay2,
            "the stop forgot the refusal"
        );

        fake.fail(&both, ErrorKind::Refused);
        airplay.poll();
        assert!(airplay.speakers()[0].airplay2);
        let second = FakeAirPlay::new();
        second.set_airplay2(true);
        airplay.replace_link(second.link());
        second.appear(&both);
        airplay.poll();
        assert!(!airplay.speakers()[0].airplay2, "Retry forgot the refusal");
    }

    /// A sender that stops, or Retry's new one, ends every session, and no
    /// Stopped comes from it: a dual speaker that played over AirPlay 1
    /// reads the path a new session takes again (spec S313).
    // covers: S313
    #[test]
    fn a_stopped_or_new_sender_forgets_the_path_a_session_took() {
        let fake = FakeAirPlay::new();
        fake.set_airplay2(true);
        fake.set_airplay2_preferred(true);
        let mut airplay = speakers(&fake);
        let den = dual_device(12, "Den");
        let id = id_of(&den);
        fake.appear(&den);
        airplay.poll();
        assert!(
            airplay.speakers()[0].airplay2,
            "the preference picks AirPlay 2"
        );
        airplay.set_enabled(&id, true).unwrap();
        airplay.set_streaming(true);
        let on_airplay1 = |fake: &FakeAirPlay| {
            fake.push(SenderEvent::Speaker {
                key: den.key.clone(),
                event: SpeakerEvent::Path(ProtocolPath::AirPlay1),
            });
        };
        on_airplay1(&fake);
        airplay.poll();
        assert!(
            !airplay.speakers()[0].airplay2,
            "its session plays over AirPlay 1"
        );

        fake.stop_running();
        airplay.poll();
        fake.run_again();
        fake.appear(&den);
        airplay.poll();
        assert!(airplay.speakers()[0].airplay2, "the stop ended the session");

        on_airplay1(&fake);
        airplay.poll();
        assert!(!airplay.speakers()[0].airplay2);
        let second = FakeAirPlay::new();
        second.set_airplay2(true);
        second.set_airplay2_preferred(true);
        airplay.replace_link(second.link());
        second.appear(&den);
        airplay.poll();
        assert!(airplay.speakers()[0].airplay2, "Retry ended the session");
    }

    #[test]
    fn a_pair_switched_off_forgets_what_its_members_airplay1_said() {
        let fake = FakeAirPlay::new();
        let (mut pair, left, _) = stereo_pair();
        let both = den(true);
        let member = id_of(&both);
        pair.members[1] = member_of(&both);
        let mut airplay = with_target(&fake, &pair, &[&left, &both]);
        airplay.set_streaming(true);
        airplay.set_enabled(PAIR, true).expect("the pair can play");
        fake.fail(&both, ErrorKind::Refused);
        airplay.poll();
        assert!(airplay.over_airplay2(&member), "moved to AirPlay 2");

        airplay.set_enabled(PAIR, false).expect("off");
        assert!(!airplay.over_airplay2(&member), "AirPlay 1 first again");
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
        airplay.begin_attempt(&id);
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
    fn a_session_refused_before_any_pairing_step_names_no_code() {
        let fake = FakeAirPlay::new();
        fake.set_airplay2(true);
        let mut airplay = speakers(&fake);
        airplay.set_streaming(true);
        // No PIN or password in its flags and no stored credentials: a TV
        // that answers 403 to everything.
        let tv = airplay2_device(9, "Samsung", false);
        let id = id_of(&tv);
        fake.appear(&tv);
        airplay.poll();
        assert_eq!(airplay.set_enabled(&id, true), Ok(()));
        fake.take_calls();
        fake.fail(&tv, ErrorKind::PairingRequired);
        let events = airplay.poll();
        let problems: Vec<AirPlayProblem> = events
            .iter()
            .filter_map(|event| match event {
                BackendEvent::AirPlayFailed { problem, .. } => Some(*problem),
                _ => None,
            })
            .collect();
        assert_eq!(problems, [AirPlayProblem::NotAccepted], "{events:?}");
        assert_ne!(
            airplay.speakers()[0].availability,
            AirPlayAvailability::NeedsPin,
            "its row asks for no code"
        );
        assert!(airplay.pin_prompt().is_none());
        assert!(
            !fake
                .take_calls()
                .iter()
                .any(|call| matches!(call, Call::StartPairing(_)))
        );
    }

    #[test]
    fn the_same_refusal_in_the_users_attempt_pairs_with_a_pin() {
        let fake = FakeAirPlay::new();
        fake.set_airplay2(true);
        let mut airplay = speakers(&fake);
        airplay.set_streaming(true);
        let tv = airplay2_device(9, "Samsung", false);
        let id = id_of(&tv);
        fake.appear(&tv);
        airplay.poll();
        assert_eq!(airplay.set_enabled(&id, true), Ok(()));
        airplay.begin_attempt(&id);
        fake.take_calls();
        fake.fail(&tv, ErrorKind::PairingRequired);
        let events = airplay.poll();
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, BackendEvent::AirPlayFailed { .. })),
            "{events:?}"
        );
        let calls = fake.take_calls();
        assert!(calls.contains(&Call::StartPairing(id)), "{calls:?}");
        assert_eq!(
            airplay.speakers()[0].availability,
            AirPlayAvailability::NeedsPin
        );
    }

    // covers: S257
    #[test]
    fn a_pairing_forgotten_mid_attempt_leaves_a_phone_switch_on_unpaired() {
        use crate::audio::pairings::tests::homekit;

        let fake = FakeAirPlay::new();
        fake.set_airplay2(true);
        let mut airplay = speakers(&fake);
        airplay.set_streaming(true);
        let tv = airplay2_device(9, "Samsung", false);
        let id = id_of(&tv);
        airplay.store.insert_homekit(&id, homekit(0x33)).unwrap();
        fake.appear(&tv);
        airplay.poll();
        assert_eq!(airplay.set_enabled(&id, true), Ok(()));
        airplay.begin_attempt(&id);
        airplay.forget_pairing(&id);

        // The web remote switches it on without an attempt of its own.
        assert_eq!(airplay.set_enabled(&id, true), Ok(()));
        fake.take_calls();
        fake.fail(&tv, ErrorKind::PairingRequired);
        airplay.poll();
        assert!(
            !fake
                .take_calls()
                .iter()
                .any(|call| matches!(call, Call::StartPairing(_)))
        );
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
            "a wrong PIN starts the pairing again"
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
            artwork: Some(audiosync_airplay::metadata::Artwork::single(
                b"cover".to_vec(),
            )),
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

    // The receiver quit under the open prompt and came back before its
    // closed connection ended the pairing.
    // covers: S223
    #[test]
    fn a_pairing_cancelled_after_its_receiver_left_and_came_back_turns_the_speaker_off() {
        let fake = FakeAirPlay::new();
        let mut airplay = speakers(&fake);
        let tv = device(6, "Den TV", APPLE_TV_TXT);
        let id = id_of(&tv);
        pin_shown(&fake, &mut airplay, &tv);
        fake.gone(&tv);
        airplay.poll();
        fake.appear(&tv);
        airplay.poll();
        fake.pairing(&tv, pairing_failed(ErrorKind::PairingCancelled, false));
        assert!(airplay.poll().contains(&BackendEvent::AirPlayFailed {
            id,
            problem: AirPlayProblem::PairingFailed
        }));
        assert_eq!(airplay.pin_prompt(), None);
        assert!(airplay.enabled_ids().is_empty());
    }

    #[test]
    fn forgetting_a_pairing_turns_the_speaker_off_and_asks_for_a_pin_again() {
        use crate::audio::pairings::FILE_NAME;
        use crate::audio::pairings::tests::{credentials, sealer};

        let path = crate::debug_log::test_dir("airplay-forget").join(FILE_NAME);
        let fake = FakeAirPlay::new();
        let (mut store, _) = PairingStore::open(path.clone(), sealer());
        let tv = device(6, "Den TV", APPLE_TV_TXT);
        let id = id_of(&tv);
        store
            .insert(&id, credentials("0123456789ABCDEF", 2))
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
        drop(airplay);
        let (again, _) = PairingStore::open(path, sealer());
        assert!(!again.contains(&id), "gone from the file too");
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

    // A turn-down proves nothing (a reset receiver and an impostor look the
    // same), so the pairing stays in the file until a new one succeeds.
    #[test]
    fn a_legacy_pairing_turned_down_stays_in_the_file_until_a_new_pairing_replaces_it() {
        use crate::audio::pairings::FILE_NAME;
        use crate::audio::pairings::tests::{credentials, sealer};

        let path = crate::debug_log::test_dir("airplay-legacy-turned-down").join(FILE_NAME);
        let (mut store, _) = PairingStore::open(path.clone(), sealer());
        let tv = device(6, "Den TV", APPLE_TV_TXT);
        let id = id_of(&tv);
        let first = credentials("0123456789ABCDEF", 3);
        store.insert(&id, first.clone()).expect("saved");
        let fake = FakeAirPlay::new();
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
        assert!(!airplay.speakers()[0].paired, "it asks for its PIN");
        assert!(
            airplay.speakers()[0].holds_credentials,
            "Forget Pairing stays on offer"
        );
        assert_eq!(airplay.store.get(&id), Some(&first));
        let (on_disk, _) = PairingStore::open(path.clone(), sealer());
        assert_eq!(on_disk.get(&id), Some(&first), "still in the file");

        fake.take_calls();
        assert_eq!(
            airplay.set_enabled(&id, true),
            Err(AirPlayProblem::NeedsPin)
        );
        airplay.start_pairing(&id).expect("the pairing starts");
        fake.pairing(&tv, PairingEvent::PinRequested);
        airplay.poll();
        assert!(airplay.submit_pin(&id, "4821"));
        let second = credentials("FEDCBA9876543210", 4);
        fake.pairing(&tv, PairingEvent::Paired(second.clone()));
        airplay.poll();
        assert!(airplay.speakers()[0].paired);
        let (on_disk, _) = PairingStore::open(path, sealer());
        assert_eq!(on_disk.get(&id), Some(&second), "replaced");
    }

    #[test]
    fn airplay2_credentials_turned_down_stay_in_the_file_until_a_new_pairing_replaces_them() {
        use crate::audio::pairings::FILE_NAME;
        use crate::audio::pairings::tests::{homekit, sealer};

        let path = crate::debug_log::test_dir("airplay2-homekit-turned-down").join(FILE_NAME);
        let (mut store, _) = PairingStore::open(path.clone(), sealer());
        store.open_airplay2();
        let tv = airplay2_tv(9, "Loft");
        let id = id_of(&tv);
        store.insert_homekit(&id, homekit(0x33)).expect("saved");
        let fake = FakeAirPlay::new();
        fake.set_airplay2(true);
        let mut airplay = AirPlaySpeakers::new(fake.link(), 100.0, DebugLog::default(), store);
        fake.appear(&tv);
        airplay.poll();
        assert!(airplay.speakers()[0].paired);
        assert_eq!(airplay.set_enabled(&id, true), Ok(()));
        airplay.set_streaming(true);
        fake.fail(&tv, ErrorKind::PairingRequired);
        airplay.poll();
        assert!(!airplay.speakers()[0].paired, "it asks for its PIN");
        assert!(
            airplay.speakers()[0].holds_credentials,
            "Forget Pairing stays on offer"
        );
        assert_eq!(airplay.store.homekit(&id), Some(&homekit(0x33)));
        let (mut on_disk, _) = PairingStore::open(path.clone(), sealer());
        on_disk.open_airplay2();
        assert_eq!(
            on_disk.homekit(&id),
            Some(&homekit(0x33)),
            "still in the file"
        );

        fake.take_calls();
        assert_eq!(
            airplay.set_enabled(&id, true),
            Err(AirPlayProblem::NeedsPin)
        );
        airplay.start_pairing(&id).expect("the pairing starts");
        fake.pairing(&tv, PairingEvent::PinRequested);
        airplay.poll();
        assert!(airplay.submit_pin(&id, "4821"));
        fake.pairing(&tv, PairingEvent::HomeKitPaired(homekit(0x44)));
        airplay.poll();
        assert!(airplay.speakers()[0].paired);
        let (mut on_disk, _) = PairingStore::open(path, sealer());
        on_disk.open_airplay2();
        assert_eq!(on_disk.homekit(&id), Some(&homekit(0x44)), "replaced");
    }

    // Playing on them shows that whatever turned them down is gone (P154).
    #[test]
    fn turned_down_credentials_a_session_plays_on_list_as_paired_again() {
        use crate::audio::pairings::FILE_NAME;
        use crate::audio::pairings::tests::{homekit, sealer};

        let path = crate::debug_log::test_dir("airplay2-homekit-plays-again").join(FILE_NAME);
        let (mut store, _) = PairingStore::open(path, sealer());
        store.open_airplay2();
        let tv = airplay2_tv(9, "Loft");
        let id = id_of(&tv);
        store.insert_homekit(&id, homekit(0x33)).expect("saved");
        let fake = FakeAirPlay::new();
        fake.set_airplay2(true);
        let mut airplay = AirPlaySpeakers::new(fake.link(), 100.0, DebugLog::default(), store);
        fake.appear(&tv);
        airplay.poll();
        assert_eq!(airplay.set_enabled(&id, true), Ok(()));
        airplay.set_streaming(true);
        airplay.poll();
        // The switch-on gate keeps a turned-down speaker off, so the mark is
        // set on one already playing.
        airplay.credentials_turned_down.insert(id.clone());
        assert!(!airplay.speakers()[0].paired);

        fake.state(&tv, SpeakerState::Playing);
        airplay.poll();
        assert!(airplay.speakers()[0].paired);
        assert!(!airplay.credentials_turned_down.contains(&id));
    }

    // An impostor plays on legacy credentials without the receiver's key,
    // which skip the signature check (P56). AirPlay 2 credentials pin the
    // speaker to AirPlay 2, whose pair-verify proves them, so playing there
    // clears the mark.
    #[test]
    fn a_session_that_proves_nothing_leaves_turned_down_credentials_marked() {
        use crate::audio::pairings::FILE_NAME;
        use crate::audio::pairings::tests::{credentials, homekit, sealer};

        let path = crate::debug_log::test_dir("airplay-unproven-plays").join(FILE_NAME);
        let (mut store, _) = PairingStore::open(path, sealer());
        store.open_airplay2();
        let homekit_only = device(6, "Den TV", OPEN_TXT);
        let without_key = device(7, "Study", OPEN_TXT);
        let with_key = device(8, "Loft", OPEN_TXT);
        store
            .insert_homekit(&id_of(&homekit_only), homekit(0x33))
            .expect("saved");
        store
            .insert(&id_of(&without_key), credentials("0123456789ABCDEF", 3))
            .expect("saved");
        let proven = credentials("FEDCBA9876543210", 4).with_receiver_key([9; 32]);
        store.insert(&id_of(&with_key), proven).expect("saved");
        let fake = FakeAirPlay::new();
        fake.set_airplay2(true);
        let mut airplay = AirPlaySpeakers::new(fake.link(), 100.0, DebugLog::default(), store);
        let tvs = [&homekit_only, &without_key, &with_key];
        for tv in tvs {
            fake.appear(tv);
        }
        airplay.poll();
        for tv in tvs {
            assert_eq!(airplay.set_enabled(&id_of(tv), true), Ok(()));
        }
        airplay.set_streaming(true);
        airplay.poll();
        for tv in tvs {
            airplay.credentials_turned_down.insert(id_of(tv));
            fake.state(tv, SpeakerState::Playing);
        }
        airplay.poll();
        assert!(
            !airplay
                .credentials_turned_down
                .contains(&id_of(&homekit_only))
        );
        assert!(
            airplay
                .credentials_turned_down
                .contains(&id_of(&without_key))
        );
        assert!(!airplay.credentials_turned_down.contains(&id_of(&with_key)));
    }

    #[test]
    fn a_tv_that_fails_its_signature_check_goes_off_and_keeps_its_credentials() {
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
        fake.fail(&tv, ErrorKind::ReceiverUnverified);
        let events = airplay.poll();
        assert!(
            events.contains(&BackendEvent::AirPlayFailed {
                id: id.clone(),
                problem: AirPlayProblem::ReceiverUnverified
            }),
            "{events:?}"
        );
        assert!(airplay.store.contains(&id), "the pairing stays");
        assert!(airplay.speakers()[0].paired);
        assert!(airplay.enabled_ids().is_empty());
    }

    #[test]
    fn a_paired_tv_reads_as_refusing_only_when_its_pairing_failure_waits_to_retry() {
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
        // The notice comes with the second failed retry (S222).
        for _ in 0..3 {
            fake.refuse_for_state(&tv);
        }
        let events = airplay.poll();
        assert!(
            events.contains(&BackendEvent::AirPlayTrouble {
                id: id.clone(),
                problem: AirPlayProblem::Refused
            }),
            "{events:?}"
        );

        fake.fail(&tv, ErrorKind::PairingFailed);
        let events = airplay.poll();
        assert!(
            events.contains(&BackendEvent::AirPlayFailed {
                id: id.clone(),
                problem: AirPlayProblem::NeedsPin
            }),
            "a failure with no wait keeps its own problem: {events:?}"
        );
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
                "pairing started again",
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

    // covers: S79
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
        assert!(calls.contains(&Call::Volume(
            s.clone(),
            airplay_volume(100.0, NO_ZONE_POSITION, 100.0)
        )));
        assert!(
            !calls
                .iter()
                .any(|call| matches!(call, Call::Volume(_, Volume::Muted)))
        );
    }

    // covers: S276
    #[test]
    fn a_device_volume_during_a_calibration_solo_is_kept_and_stays_muted() {
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
        airplay.set_calibration_solo(Some(&k));
        fake.take_calls();
        // Study's remote asks for −6 dB while only Kitchen may be heard.
        remote(&fake, &study, RemoteCommand::DeviceVolume(Volume::Db(-6.0)));
        assert_eq!(airplay.poll(), [BackendEvent::AirPlayChanged]);
        assert_eq!(sent_levels(&fake), [(s.clone(), None)]);
        airplay.set_calibration_solo(None);
        assert!(sent_levels(&fake).contains(&(s.clone(), Some(-6.0))));
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
        fake.lose(&kitchen, ErrorKind::Lost);
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

    // A receiver powered off mid-stream: discovery withdraws it about 40 s
    // later, and the sender pauses its retries then (P215).
    #[test]
    fn a_retrying_speaker_whose_receiver_is_gone_stays_in_the_sender_until_switched_off() {
        let fake = FakeAirPlay::new();
        let mut airplay = speakers(&fake);
        let kitchen = device(1, "Kitchen", OPEN_TXT);
        let id = id_of(&kitchen);
        fake.appear(&kitchen);
        airplay.poll();
        airplay.set_enabled(&id, true).unwrap();
        airplay.set_streaming(true);
        fake.state(&kitchen, SpeakerState::Playing);
        airplay.poll();
        fake.fail(&kitchen, ErrorKind::Unreachable);
        airplay.poll();
        fake.take_calls();

        fake.gone(&kitchen);
        airplay.poll();
        assert_eq!(fake.take_calls(), [], "it left the sender");
        assert_eq!(airplay.speakers().len(), 1, "it lost its row");
        assert_eq!(airplay.enabled_ids(), std::slice::from_ref(&id));
        assert!(!airplay.chosen());

        airplay.set_enabled(&id, false).unwrap();
        assert_eq!(fake.take_calls(), [Call::Remove(id)]);
    }

    #[test]
    fn a_speaker_playing_on_after_its_announcement_lapsed_stays_in_the_sender_once_it_is_lost() {
        let fake = FakeAirPlay::new();
        let mut airplay = speakers(&fake);
        let kitchen = device(1, "Kitchen", OPEN_TXT);
        let id = id_of(&kitchen);
        fake.appear(&kitchen);
        airplay.poll();
        airplay.set_enabled(&id, true).unwrap();
        airplay.set_streaming(true);
        fake.state(&kitchen, SpeakerState::Playing);
        airplay.poll();
        fake.gone(&kitchen);
        airplay.poll();
        fake.take_calls();
        assert_eq!(airplay.speakers().len(), 1, "playing, it keeps its row");

        fake.fail(&kitchen, ErrorKind::Lost);
        let events = airplay.poll();
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, BackendEvent::AirPlayTrouble { .. })),
            "{events:?}"
        );
        assert_eq!(fake.take_calls(), []);
        assert_eq!(airplay.speakers().len(), 1);
        assert_eq!(airplay.enabled_ids(), std::slice::from_ref(&id));
    }

    // covers: S222
    // The sender restarts a session on its own (an ALAC retry, a path
    // switch) with a state change and no error.
    #[test]
    fn a_speaker_playing_on_after_its_announcement_lapsed_stays_in_the_sender_once_its_session_restarts()
     {
        let fake = FakeAirPlay::new();
        let mut airplay = speakers(&fake);
        let kitchen = device(1, "Kitchen", OPEN_TXT);
        let id = id_of(&kitchen);
        fake.appear(&kitchen);
        airplay.poll();
        airplay.set_enabled(&id, true).unwrap();
        airplay.set_streaming(true);
        fake.state(&kitchen, SpeakerState::Playing);
        airplay.poll();
        fake.gone(&kitchen);
        airplay.poll();
        fake.take_calls();

        fake.state(&kitchen, SpeakerState::Connecting);
        airplay.poll();
        assert_eq!(fake.take_calls(), []);
        assert_eq!(airplay.speakers().len(), 1);
        assert_eq!(airplay.enabled_ids(), std::slice::from_ref(&id));
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
            ErrorKind::Unsupported(UnsupportedReason::TooManyAirPlay2Sessions),
        );
        assert_eq!(
            airplay.poll(),
            [
                BackendEvent::AirPlayChanged,
                BackendEvent::AirPlayTrouble {
                    id: id.clone(),
                    problem: AirPlayProblem::TooManyAirPlay2Speakers
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

    #[test]
    fn airplay2_is_off_unless_the_setting_or_the_variable_turns_it_on() {
        // P170: with the setting off and the variable unset, nothing of the
        // AirPlay 2 path starts.
        assert!(!airplay2_on(false, None));
        assert!(!airplay2_on(false, Some("0")));
        assert!(airplay2_on(true, None));
        assert!(airplay2_on(false, Some("1")));
    }

    // covers: S323
    #[test]
    fn the_app_raises_the_airplay_driver_thread_on_windows() {
        let config = airplay2_config(false, None, false);
        assert_eq!(config.on_driver_thread.is_some(), cfg!(windows));
    }

    #[test]
    fn the_airplay2_flag_keeps_the_path_on_and_the_switch_prefers_airplay2() {
        // Spec S312: with the flag off the switch alone sets the path.
        for (saved, env, on) in [
            (false, None, false),
            (true, None, true),
            (false, Some("1"), true),
        ] {
            let config = airplay2_config(saved, env, false);
            assert_eq!(config.airplay2, on);
            assert!(!config.airplay2_preferred);
        }
        let off = airplay2_config(false, None, true);
        assert!(off.airplay2 && !off.airplay2_preferred);
        for (saved, env) in [(true, None), (false, Some("1"))] {
            let on = airplay2_config(saved, env, true);
            assert!(on.airplay2 && on.airplay2_preferred);
        }
    }

    // covers: S312
    #[test]
    fn with_nothing_switched_the_path_is_on_and_airplay1_comes_first() {
        let config = airplay2_config(false, None, airplay2_auto(None));
        assert!(config.airplay2, "AirPlay 2 only receivers play");
        assert!(
            !config.airplay2_preferred,
            "receivers that speak both take AirPlay 1"
        );
    }

    #[test]
    fn the_pcm_variable_names_speakers_by_id() {
        assert!(pcm_debug_devices(None).is_empty());
        assert!(pcm_debug_devices(Some("")).is_empty());
        assert!(pcm_debug_devices(Some(" , ")).is_empty());
        assert_eq!(
            pcm_debug_devices(Some("A1B2C3D4E5F6")),
            BTreeSet::from(["A1B2C3D4E5F6".to_owned()])
        );
        assert_eq!(
            pcm_debug_devices(Some(
                "a1:b2:c3:d4:e5:f6, raop:0011223344AA@Den ,,0011223344aa"
            )),
            BTreeSet::from([
                "A1B2C3D4E5F6".to_owned(),
                "raop:0011223344AA@Den".to_owned(),
                "0011223344AA".to_owned(),
            ])
        );
        assert!(!pcm_debug_devices(Some("A1B2C3D4E5F6")).contains("A1B2C3D4E5F7"));
    }

    #[test]
    fn the_pcm_variable_allows_pcm_for_the_named_speaker_only() {
        let fake = FakeAirPlay::new();
        let mut airplay = speakers(&fake);
        let named = device(1, "Kitchen", OPEN_TXT);
        let other = device(2, "Den", OPEN_TXT);
        airplay.pcm_devices = pcm_debug_devices(Some(&id_of(&named)));
        fake.appear(&named);
        fake.appear(&other);
        airplay.poll();
        airplay.set_streaming(true);
        airplay.set_enabled(&id_of(&named), true).unwrap();
        airplay.set_enabled(&id_of(&other), true).unwrap();
        let pcm: Vec<Call> = fake
            .take_calls()
            .into_iter()
            .filter(|call| matches!(call, Call::Pcm(_)))
            .collect();
        assert_eq!(pcm, [Call::Pcm(id_of(&named))]);
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
                volume: airplay_volume(DEFAULT_LOCAL_POSITION, NO_ZONE_POSITION, 100.0),
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
    fn forget_password_drops_the_typed_password_and_turns_the_speaker_off() {
        let fake = FakeAirPlay::new();
        let mut airplay = speakers(&fake);
        let study = password_asked(&fake, &mut airplay);
        let id = id_of(&study);
        assert!(!airplay.speakers()[0].holds_password);
        assert!(airplay.submit_password(&id, "hunter2"));
        let row = &airplay.speakers()[0];
        assert!(
            row.holds_password && !row.paired,
            "its menu offers Forget Password, not Forget Pairing"
        );
        fake.take_calls();

        airplay.forget_pairing(&id);
        assert!(!airplay.speakers()[0].holds_password);
        assert!(airplay.enabled_ids().is_empty(), "it goes off");
        assert_eq!(
            airplay.set_enabled(&id, true),
            Err(AirPlayProblem::NeedsPassword),
            "switched on again, it asks for the password"
        );
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

    // covers: S223
    #[test]
    fn a_password_required_after_the_password_typed_this_run_says_it_was_turned_down() {
        let fake = FakeAirPlay::new();
        let mut airplay = speakers(&fake);
        let study = password_asked(&fake, &mut airplay);
        let id = id_of(&study);
        assert!(airplay.submit_password(&id, "hunter1"));
        fake.take_calls();
        fake.fail(&study, ErrorKind::PasswordRequired);
        assert_eq!(airplay.poll(), [BackendEvent::AirPlayChanged], "no notice");
        assert_eq!(
            airplay.password_prompt().map(|prompt| prompt.wrong),
            Some(true)
        );
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

    /// Lets the volume set when `id` joined settle, so a read counts.
    fn settle(airplay: &mut AirPlaySpeakers, id: &str) {
        let read = &mut airplay.added.get_mut(id).expect("added").read;
        read.set_at = Instant::now()
            .checked_sub(VOLUME_SETTLE * 2)
            .expect("the clock runs past a few seconds");
    }

    /// Asks the playing speakers for their volume, as the next poll that
    /// finds the interval over does.
    fn read_now(airplay: &mut AirPlaySpeakers) {
        let now = Instant::now();
        airplay.volume_read_at = now
            .checked_sub(VOLUME_READ_INTERVAL)
            .expect("the clock runs past a few seconds");
        airplay.read_volumes(now);
    }

    fn report_volume(fake: &FakeAirPlay, device: &Device, db: f64) {
        fake.push(SenderEvent::Speaker {
            key: device.key.clone(),
            event: SpeakerEvent::VolumeReported(db),
        });
    }

    fn warn(fake: &FakeAirPlay, device: &Device, warning: Warning) {
        fake.push(SenderEvent::Speaker {
            key: device.key.clone(),
            event: SpeakerEvent::Warning(warning),
        });
    }

    /// Kitchen on, streaming and playing at 50, its set settled.
    fn kitchen_playing(fake: &FakeAirPlay, txt: &[&str]) -> (AirPlaySpeakers, Device, String) {
        let mut airplay = speakers(fake);
        let kitchen = device(1, "Kitchen", txt);
        let id = id_of(&kitchen);
        fake.appear(&kitchen);
        airplay.poll();
        airplay.set_enabled(&id, true).unwrap();
        airplay.set_streaming(true);
        airplay.set_volume(&id, 50.0);
        fake.state(&kitchen, SpeakerState::Playing);
        airplay.poll();
        settle(&mut airplay, &id);
        fake.take_calls();
        (airplay, kitchen, id)
    }

    /// The volumes sent, rounded to 0.1 dB; `None` for muted.
    fn sent_levels(fake: &FakeAirPlay) -> Vec<(String, Option<f64>)> {
        fake.take_calls()
            .into_iter()
            .filter_map(|call| match call {
                Call::Volume(id, Volume::Db(db)) => Some((id, Some((db * 10.0).round() / 10.0))),
                Call::Volume(id, Volume::Muted) => Some((id, None)),
                _ => None,
            })
            .collect()
    }

    fn remote(fake: &FakeAirPlay, device: &Device, command: RemoteCommand) {
        fake.push(SenderEvent::Remote {
            key: device.key.clone(),
            target: None,
            command,
        });
    }

    // covers: S274
    #[test]
    fn a_remote_transport_goes_to_the_now_playing_control() {
        let fake = FakeAirPlay::new();
        let (mut airplay, kitchen, _) = kitchen_playing(&fake, OPEN_TXT);
        remote(&fake, &kitchen, RemoteCommand::Next);
        airplay.poll();
        assert!(
            airplay.take_remote_controls().is_empty(),
            "no app to control"
        );
        read(
            &mut airplay,
            Some(("Spotify.exe".to_owned(), false)),
            Instant::now(),
        );
        remote(&fake, &kitchen, RemoteCommand::Pause);
        airplay.poll();
        assert!(airplay.take_remote_controls().is_empty(), "already paused");
        remote(&fake, &kitchen, RemoteCommand::Play);
        airplay.poll();
        assert_eq!(airplay.take_remote_controls(), [Transport::PlayPause]);
        assert!(fake.take_calls().is_empty(), "no volume moved");
    }

    /// The pause and resume calls since the last take.
    fn pause_calls(fake: &FakeAirPlay) -> Vec<Call> {
        fake.take_calls()
            .into_iter()
            .filter(|call| matches!(call, Call::Pause | Call::Resume))
            .collect()
    }

    /// A reading of a session that plays or is paused.
    fn read(airplay: &mut AirPlaySpeakers, playback: Option<(String, bool)>, at: Instant) {
        airplay.set_playback(playback.clone(), playback, at);
    }

    // covers: P213
    #[test]
    fn an_app_that_stops_or_changes_tracks_never_pauses_the_speakers() {
        let fake = FakeAirPlay::new();
        let (mut airplay, _, _) = kitchen_playing(&fake, OPEN_TXT);
        let t0 = Instant::now();
        read(&mut airplay, spotify(true), t0);
        airplay.set_playback(spotify(false), None, t0);
        read(&mut airplay, spotify(true), t0);
        airplay.set_playback(spotify(false), None, t0);
        read(&mut airplay, spotify(true), t0);
        airplay.poll();
        assert_eq!(pause_calls(&fake), []);
    }

    fn spotify(playing: bool) -> Option<(String, bool)> {
        Some(("Spotify.exe".to_owned(), playing))
    }

    // covers: P213
    #[test]
    fn a_remote_pause_of_a_playing_app_pauses_the_speakers_and_play_resumes() {
        let fake = FakeAirPlay::new();
        let (mut airplay, kitchen, _) = kitchen_playing(&fake, OPEN_TXT);
        let t0 = Instant::now();
        read(&mut airplay, spotify(true), t0);
        remote(&fake, &kitchen, RemoteCommand::Pause);
        airplay.poll();
        assert_eq!(airplay.take_remote_controls(), [Transport::PlayPause]);
        assert_eq!(pause_calls(&fake), [Call::Pause]);
        read(&mut airplay, spotify(false), t0);
        remote(&fake, &kitchen, RemoteCommand::Play);
        airplay.poll();
        assert_eq!(airplay.take_remote_controls(), [Transport::PlayPause]);
        assert_eq!(pause_calls(&fake), [Call::Resume]);
    }

    // covers: P213
    #[test]
    fn a_remote_play_pause_follows_the_reading_both_ways() {
        let fake = FakeAirPlay::new();
        let (mut airplay, kitchen, _) = kitchen_playing(&fake, OPEN_TXT);
        let t0 = Instant::now();
        read(&mut airplay, spotify(true), t0);
        remote(&fake, &kitchen, RemoteCommand::PlayPause);
        airplay.poll();
        assert_eq!(pause_calls(&fake), [Call::Pause]);
        read(&mut airplay, spotify(false), t0);
        std::thread::sleep(Duration::from_millis(350));
        remote(&fake, &kitchen, RemoteCommand::PlayPause);
        airplay.poll();
        assert_eq!(pause_calls(&fake), [Call::Resume]);
        assert_eq!(
            airplay.take_remote_controls(),
            [Transport::PlayPause, Transport::PlayPause]
        );
    }

    // covers: P213
    #[test]
    fn a_remote_pause_with_no_reading_or_an_app_paused_sends_no_flush() {
        let fake = FakeAirPlay::new();
        let (mut airplay, kitchen, _) = kitchen_playing(&fake, OPEN_TXT);
        let t0 = Instant::now();
        read(&mut airplay, None, t0);
        remote(&fake, &kitchen, RemoteCommand::Pause);
        airplay.poll();
        read(&mut airplay, spotify(false), t0);
        std::thread::sleep(Duration::from_millis(350));
        remote(&fake, &kitchen, RemoteCommand::PlayPause);
        airplay.poll();
        assert_eq!(pause_calls(&fake), []);
    }

    // covers: P213
    #[test]
    fn the_window_play_pause_of_the_followed_playing_app_pauses_and_resumes() {
        let fake = FakeAirPlay::new();
        let (mut airplay, _, _) = kitchen_playing(&fake, OPEN_TXT);
        let t0 = Instant::now();
        read(&mut airplay, spotify(true), t0);
        airplay.play_pause_pressed("Spotify.exe", true, t0);
        assert_eq!(pause_calls(&fake), [Call::Pause]);
        airplay.play_pause_pressed("Spotify.exe", false, t0);
        assert_eq!(pause_calls(&fake), [Call::Resume]);
    }

    // covers: P213
    #[test]
    fn a_window_play_pause_the_reading_does_not_confirm_sends_no_flush() {
        let fake = FakeAirPlay::new();
        let (mut airplay, _, _) = kitchen_playing(&fake, OPEN_TXT);
        let t0 = Instant::now();
        airplay.play_pause_pressed("Spotify.exe", true, t0);
        read(&mut airplay, spotify(true), t0);
        airplay.play_pause_pressed("chrome.exe", true, t0);
        airplay.play_pause_pressed("Spotify.exe", false, t0);
        read(&mut airplay, None, t0);
        read(&mut airplay, spotify(false), t0);
        airplay.play_pause_pressed("Spotify.exe", true, t0);
        assert_eq!(pause_calls(&fake), []);
    }

    // covers: P213
    #[test]
    fn sound_in_the_capture_resumes_a_pause_the_reading_still_holds() {
        let fake = FakeAirPlay::new();
        let (mut airplay, kitchen, _) = kitchen_playing(&fake, OPEN_TXT);
        let t0 = Instant::now();
        read(&mut airplay, spotify(true), t0);
        remote(&fake, &kitchen, RemoteCommand::Pause);
        airplay.poll();
        assert_eq!(pause_calls(&fake), [Call::Pause]);
        let later = Instant::now() + STALE_READING;
        read(&mut airplay, spotify(false), later);
        airplay.heard(0.0, later);
        airplay.heard(0.1, later);
        assert_eq!(pause_calls(&fake), [], "Spotify still reads paused");
        airplay.heard(0.3, later);
        assert_eq!(pause_calls(&fake), [Call::Resume], "another app plays");
    }

    // covers: P213
    #[test]
    fn the_feed_counts_the_sound_it_hands_over() {
        let clock = SenderClock::new(Instant::now());
        let (_stream, input) = Stream::new(StreamConfig::default(), clock, Instant::now());
        let slot = FeedSlot::new(input);
        let mut lease = slot.lease().expect("the feed is free");
        let at = Instant::now();
        lease.push(&vec![0.0_f32; 480 * 2], 480, at);
        lease.push(&vec![2e-5_f32; 480 * 2], 480, at);
        assert_eq!(slot.sound_seconds(), 0.0, "silence and dither");
        lease.push(&vec![0.25_f32; 480 * 2], 480, at);
        assert!(
            (slot.sound_seconds() - 0.01).abs() < 1e-9,
            "{}",
            slot.sound_seconds()
        );
    }

    fn chrome(playing: bool) -> Option<(String, bool)> {
        Some(("chrome.exe".to_owned(), playing))
    }

    // covers: P213
    #[test]
    fn the_app_pausing_itself_pauses_the_speakers_once_and_playing_resumes() {
        let fake = FakeAirPlay::new();
        let (mut airplay, _, _) = kitchen_playing(&fake, OPEN_TXT);
        let t0 = Instant::now();
        read(&mut airplay, spotify(true), t0);
        airplay.poll();
        assert_eq!(pause_calls(&fake), []);
        read(&mut airplay, spotify(false), t0);
        assert_eq!(pause_calls(&fake), [Call::Pause]);
        read(&mut airplay, spotify(false), t0 + STALE_READING * 2);
        airplay.poll();
        assert_eq!(pause_calls(&fake), [], "the same reading again");
        read(&mut airplay, spotify(true), t0 + STALE_READING * 3);
        assert_eq!(pause_calls(&fake), [Call::Resume]);
    }

    // covers: P213
    #[test]
    fn a_first_reading_paused_or_an_app_that_quits_never_pauses_the_speakers() {
        let fake = FakeAirPlay::new();
        let (mut airplay, _, _) = kitchen_playing(&fake, OPEN_TXT);
        let t0 = Instant::now();
        read(&mut airplay, spotify(false), t0);
        read(&mut airplay, spotify(true), t0);
        read(&mut airplay, None, t0);
        read(&mut airplay, spotify(false), t0);
        airplay.poll();
        assert_eq!(pause_calls(&fake), []);
    }

    // covers: P213
    #[test]
    fn a_player_switch_follows_the_active_player() {
        let fake = FakeAirPlay::new();
        let (mut airplay, _, _) = kitchen_playing(&fake, OPEN_TXT);
        let t0 = Instant::now();
        read(&mut airplay, spotify(true), t0);
        read(&mut airplay, chrome(false), t0);
        assert_eq!(pause_calls(&fake), [], "another app that was paused");
        read(&mut airplay, chrome(true), t0);
        read(&mut airplay, chrome(false), t0);
        assert_eq!(pause_calls(&fake), [Call::Pause]);
    }

    // covers: P213
    #[test]
    fn the_app_pausing_while_nothing_streams_never_pauses_the_speakers() {
        let fake = FakeAirPlay::new();
        let (mut airplay, _, _) = kitchen_playing(&fake, OPEN_TXT);
        airplay.set_streaming(false);
        let t0 = Instant::now();
        read(&mut airplay, spotify(true), t0);
        read(&mut airplay, spotify(false), t0);
        airplay.set_streaming(true);
        airplay.poll();
        assert_eq!(pause_calls(&fake), []);
    }

    // covers: P213
    #[test]
    fn sound_in_the_capture_resumes_a_pause_the_app_took() {
        let fake = FakeAirPlay::new();
        let (mut airplay, _, _) = kitchen_playing(&fake, OPEN_TXT);
        let t0 = Instant::now();
        read(&mut airplay, spotify(true), t0);
        read(&mut airplay, spotify(false), t0);
        assert_eq!(pause_calls(&fake), [Call::Pause]);
        let later = t0 + STALE_READING;
        airplay.heard(1.0, later);
        assert_eq!(pause_calls(&fake), []);
        airplay.heard(1.25, later);
        assert_eq!(pause_calls(&fake), [Call::Resume]);
    }

    // covers: S292
    #[test]
    fn the_debug_log_names_what_resumed_a_pause() {
        let path = crate::debug_log::test_dir("airplay-resume-cause").join("log.txt");
        let fake = FakeAirPlay::new();
        let (mut airplay, _, _) = kitchen_playing(&fake, OPEN_TXT);
        airplay.set_debug_log(DebugLog::new(Some(path.clone())));
        let t0 = Instant::now();
        read(&mut airplay, spotify(true), t0);
        read(&mut airplay, spotify(false), t0);
        let later = t0 + STALE_READING;
        airplay.heard(1.0, later);
        airplay.heard(1.25, later);
        read(&mut airplay, spotify(true), later);
        read(&mut airplay, spotify(false), later);
        read(&mut airplay, spotify(true), later);
        assert!(airplay.log.flush(crate::debug_log::TEST_FLUSH));
        let resumes: Vec<String> = crate::debug_log::read_messages(&path)
            .into_iter()
            .filter(|message| message.contains("resumed after a pause"))
            .collect();
        assert_eq!(resumes.len(), 2, "{resumes:?}");
        assert!(resumes[0].ends_with("(sound heard)"), "{resumes:?}");
        assert!(resumes[1].ends_with("(player reading)"), "{resumes:?}");
    }

    // covers: P213
    #[test]
    fn the_app_playing_again_after_a_pause_resumes_the_speakers() {
        let fake = FakeAirPlay::new();
        let (mut airplay, kitchen, _) = kitchen_playing(&fake, OPEN_TXT);
        let t0 = Instant::now();
        read(&mut airplay, spotify(true), t0);
        remote(&fake, &kitchen, RemoteCommand::Pause);
        airplay.poll();
        assert_eq!(pause_calls(&fake), [Call::Pause]);
        let at = Instant::now();
        read(&mut airplay, spotify(true), at);
        assert_eq!(
            pause_calls(&fake),
            [],
            "a reading that may predate the pause"
        );
        read(&mut airplay, spotify(false), at + STALE_READING);
        assert_eq!(pause_calls(&fake), [], "the app shows paused");
        read(&mut airplay, spotify(true), at + STALE_READING * 2);
        assert_eq!(pause_calls(&fake), [Call::Resume]);
    }

    // covers: S292
    #[test]
    fn the_debug_log_names_a_button_remote_capture_or_stop_resume() {
        let path = crate::debug_log::test_dir("airplay-resume-causes").join("log.txt");
        let fake = FakeAirPlay::new();
        let (mut airplay, kitchen, _) = kitchen_playing(&fake, OPEN_TXT);
        airplay.set_debug_log(DebugLog::new(Some(path.clone())));
        let t0 = Instant::now();
        read(&mut airplay, spotify(true), t0);
        airplay.play_pause_pressed("Spotify.exe", true, t0);
        airplay.play_pause_pressed("Spotify.exe", false, t0);

        remote(&fake, &kitchen, RemoteCommand::Pause);
        airplay.poll();
        read(&mut airplay, spotify(false), t0);
        remote(&fake, &kitchen, RemoteCommand::Play);
        airplay.poll();

        read(&mut airplay, spotify(true), t0);
        airplay.play_pause_pressed("Spotify.exe", true, t0);
        airplay.capture_started();

        airplay.play_pause_pressed("Spotify.exe", true, t0);
        airplay.set_streaming(false);
        assert_eq!(
            pause_calls(&fake),
            (0..4)
                .flat_map(|_| [Call::Pause, Call::Resume])
                .collect::<Vec<_>>(),
            "every pause resumed"
        );
        assert!(airplay.log.flush(crate::debug_log::TEST_FLUSH));
        let causes: Vec<String> = crate::debug_log::read_messages(&path)
            .into_iter()
            .filter(|message| message.contains("resumed after a pause"))
            .collect();
        let expected = [
            "(app button)",
            "(speaker remote)",
            "(new capture)",
            "(streaming stopped)",
        ];
        assert_eq!(causes.len(), expected.len(), "{causes:?}");
        for (line, cause) in causes.iter().zip(expected) {
            assert!(line.ends_with(cause), "{causes:?}");
        }
    }

    // covers: P213
    #[test]
    fn a_new_capture_or_the_stream_stopping_resumes_a_paused_stream() {
        let fake = FakeAirPlay::new();
        let (mut airplay, kitchen, _) = kitchen_playing(&fake, OPEN_TXT);
        let t0 = Instant::now();
        read(&mut airplay, spotify(true), t0);
        remote(&fake, &kitchen, RemoteCommand::Pause);
        airplay.poll();
        assert_eq!(pause_calls(&fake), [Call::Pause]);
        airplay.capture_started();
        assert_eq!(fake.take_calls(), [Call::Resume], "a resume and no restart");

        read(&mut airplay, spotify(true), t0);
        std::thread::sleep(Duration::from_millis(350));
        remote(&fake, &kitchen, RemoteCommand::Pause);
        airplay.poll();
        assert_eq!(pause_calls(&fake), [Call::Pause]);
        airplay.set_streaming(false);
        assert_eq!(pause_calls(&fake), [Call::Resume]);
    }

    // covers: S275, S271
    #[test]
    fn a_remote_volume_step_is_sent_and_starts_the_3_s_window() {
        let fake = FakeAirPlay::new();
        let (mut airplay, kitchen, id) = kitchen_playing(&fake, OPEN_TXT);
        let listed = airplay.listed.get_mut(&id).expect("listed");
        let txt = listed
            .info
            .device
            .airplay_txt
            .get_or_insert_with(Default::default);
        txt.manufacturer = Some("Samsung".to_owned());
        remote(&fake, &kitchen, RemoteCommand::VolumeUp);
        assert_eq!(airplay.poll(), [BackendEvent::AirPlayChanged]);
        assert_eq!(airplay.speakers()[0].volume, 55.0);
        assert_eq!(sent_levels(&fake), [(id.clone(), Some(-13.5))]);
        // A Samsung's echo of that send on its 0.6 dB grid, by DACP or a read.
        remote(
            &fake,
            &kitchen,
            RemoteCommand::DeviceVolume(Volume::Db(-13.2)),
        );
        read_now(&mut airplay);
        report_volume(&fake, &kitchen, -13.8);
        assert!(airplay.poll().is_empty());
        assert_eq!(airplay.speakers()[0].volume, 55.0);
    }

    // covers: S273
    #[test]
    fn a_remote_command_for_a_speaker_no_longer_played_does_nothing() {
        type End = fn(&mut AirPlaySpeakers, &FakeAirPlay, &Device, &str);
        let ends: [(&str, End); 3] = [
            ("switched off", |airplay, _, _, id| {
                airplay.set_enabled(id, false).unwrap()
            }),
            ("removed", |airplay, fake, kitchen, id| {
                airplay.set_enabled(id, false).unwrap();
                fake.gone(kitchen);
            }),
            ("session ended", |airplay, _, _, _| {
                airplay.set_streaming(false)
            }),
        ];
        for (why, end) in ends {
            let fake = FakeAirPlay::new();
            let (mut airplay, kitchen, id) = kitchen_playing(&fake, OPEN_TXT);
            read(&mut airplay, spotify(true), Instant::now());
            end(&mut airplay, &fake, &kitchen, &id);
            airplay.poll();
            airplay.take_remote_controls();
            fake.take_calls();
            let before = airplay.speakers();
            for command in [
                RemoteCommand::Pause,
                RemoteCommand::Next,
                RemoteCommand::VolumeUp,
                RemoteCommand::MuteToggle,
                RemoteCommand::DeviceVolume(Volume::Db(-6.0)),
            ] {
                remote(&fake, &kitchen, command);
                assert!(airplay.poll().is_empty(), "{why}: {command:?}");
                assert!(airplay.take_remote_controls().is_empty(), "{why}");
                assert!(fake.take_calls().is_empty(), "{why}: {command:?}");
                assert_eq!(airplay.speakers(), before, "{why}: {command:?}");
            }
        }
    }

    // covers: S276, S277
    #[test]
    fn a_device_volume_or_mute_from_the_remote_moves_the_own_volume_and_is_sent() {
        let fake = FakeAirPlay::new();
        let (mut airplay, kitchen, id) = kitchen_playing(&fake, OPEN_TXT);
        airplay.set_volume_dragging(&id, true);
        remote(
            &fake,
            &kitchen,
            RemoteCommand::DeviceVolume(Volume::Db(-6.0)),
        );
        assert!(airplay.poll().is_empty(), "the slider is held");
        airplay.set_volume_dragging(&id, false);
        remote(
            &fake,
            &kitchen,
            RemoteCommand::DeviceVolume(Volume::Db(-6.0)),
        );
        assert_eq!(airplay.poll(), [BackendEvent::AirPlayChanged]);
        assert_eq!(airplay.speakers()[0].volume, 80.0);
        assert_eq!(sent_levels(&fake), [(id.clone(), Some(-6.0))]);
        remote(&fake, &kitchen, RemoteCommand::MuteToggle);
        airplay.poll();
        assert_eq!(airplay.speakers()[0].volume, 0.0);
        assert_eq!(sent_levels(&fake), [(id.clone(), None)]);
        remote(&fake, &kitchen, RemoteCommand::MuteToggle);
        airplay.poll();
        assert_eq!(airplay.speakers()[0].volume, 80.0, "restored");
        fake.take_calls();
        settle(&mut airplay, &id);
        remote(&fake, &kitchen, RemoteCommand::DeviceVolume(Volume::Muted));
        airplay.poll();
        assert_eq!(airplay.speakers()[0].volume, 0.0);
        assert_eq!(sent_levels(&fake), [(id.clone(), None)]);
    }

    #[test]
    fn a_read_counts_only_when_asked_after_the_last_set() {
        let t0 = Instant::now();
        let at = |ms: u64| t0 + Duration::from_millis(ms);
        let mut read = VolumeRead::new(t0);
        assert!(!read.answered(), "nobody asked");
        assert!(read.ask());
        assert!(read.answered());
        assert!(!read.answered(), "one answer per read");
        // A set after the read: its answer is old.
        assert!(read.ask());
        read.set(at(2100));
        assert!(!read.answered());
        assert!(read.recent(at(5000)));
        assert!(!read.recent(at(5100)), "the echo window is 3 s");
        read.unreadable = true;
        assert!(!read.ask(), "it turned a read down");
    }

    // covers: S291
    #[test]
    fn the_first_read_near_a_sent_level_is_its_echo_and_the_baseline() {
        let fake = FakeAirPlay::new();
        let (mut airplay, kitchen, id) = kitchen_playing(&fake, OPEN_TXT);
        airplay.set_volume(&id, 10.0);
        fake.take_calls();
        // Sent −27 dB; the speaker rounds it on the same row: its echo, however late.
        settle(&mut airplay, &id);
        read_now(&mut airplay);
        report_volume(&fake, &kitchen, -26.97);
        assert!(airplay.poll().is_empty());
        read_now(&mut airplay);
        report_volume(&fake, &kitchen, -26.97);
        assert!(airplay.poll().is_empty());
        assert!(fake.take_calls().is_empty());
        // Two presses down from the baseline: level 8, so own 8.
        read_now(&mut airplay);
        report_volume(&fake, &kitchen, -27.6);
        assert_eq!(airplay.poll(), [BackendEvent::AirPlayChanged]);
        assert_eq!(own(&airplay), 8.0);
        // More than 0.5 dB from a level just sent is a change at once.
        airplay.set_volume(&id, 10.0);
        read_now(&mut airplay);
        report_volume(&fake, &kitchen, -26.4);
        assert_eq!(airplay.poll(), [BackendEvent::AirPlayChanged]);
        assert_eq!(own(&airplay), 12.0);
    }

    // covers: S276, S291
    #[test]
    fn one_step_inside_the_echo_window_is_a_change_on_the_speaker() {
        let fake = FakeAirPlay::new();
        let (mut airplay, kitchen, id) = kitchen_playing(&fake, OPEN_TXT);
        // The row moved to 60 sent −12 dB; the remote pressed down once at
        // once holds level 59, by DACP.
        airplay.set_volume(&id, 60.0);
        remote(
            &fake,
            &kitchen,
            RemoteCommand::DeviceVolume(Volume::Db(-12.3)),
        );
        assert_eq!(airplay.poll(), [BackendEvent::AirPlayChanged]);
        assert_eq!(own(&airplay), 59.0);
        // By a read before any echo: the row moved to 40, the speaker holds 39.
        airplay.set_volume(&id, 40.0);
        read_now(&mut airplay);
        report_volume(&fake, &kitchen, -18.3);
        assert_eq!(airplay.poll(), [BackendEvent::AirPlayChanged]);
        assert_eq!(own(&airplay), 39.0);
    }

    // covers: S276, S291
    #[test]
    fn one_step_from_an_odd_row_on_an_exact_receiver_is_a_change() {
        let fake = FakeAirPlay::new();
        let (mut airplay, kitchen, id) = kitchen_playing(&fake, OPEN_TXT);
        // Row 49 sent −15.3 dB; a receiver that keeps it exactly reads
        // −15 dB after one press up: level 50, by DACP.
        airplay.set_volume(&id, 49.0);
        remote(
            &fake,
            &kitchen,
            RemoteCommand::DeviceVolume(Volume::Db(-15.0)),
        );
        assert_eq!(airplay.poll(), [BackendEvent::AirPlayChanged]);
        assert_eq!(own(&airplay), 50.0);
        // Row 51 sent −14.7 dB; one press up reads −14.4 dB: level 52.
        airplay.set_volume(&id, 51.0);
        read_now(&mut airplay);
        report_volume(&fake, &kitchen, -14.4);
        assert_eq!(airplay.poll(), [BackendEvent::AirPlayChanged]);
        assert_eq!(own(&airplay), 52.0);
    }

    /// Sets the AirPlay `features` the receiver `id` announced.
    fn announce_features(airplay: &mut AirPlaySpeakers, id: &str, features: u64) {
        let listed = airplay.listed.get_mut(id).expect("listed");
        let txt = listed
            .info
            .device
            .airplay_txt
            .get_or_insert_with(Default::default);
        txt.features = Some(audiosync_airplay::discovery::Features(features));
    }

    // covers: S291
    #[test]
    fn the_recorded_television_features_take_its_grid_without_a_make() {
        let fake = FakeAirPlay::new();
        let (mut airplay, kitchen, id) = kitchen_playing(&fake, OPEN_TXT);
        // The television traced on 2026-10-25 announced these features and
        // no manufacturer; it keeps row 49 (−15.3 dB) as −15.6: the echo.
        announce_features(&mut airplay, &id, 0x038B_CB46_007F_8AD0);
        airplay.set_volume(&id, 49.0);
        airplay.poll();
        read_now(&mut airplay);
        report_volume(&fake, &kitchen, -15.6);
        airplay.poll();
        assert_eq!(own(&airplay), 49.0);
        // Other features and no make: the row rule alone, so −15.6 after
        // row 47 (−15.9 dB) is one press up, level 48.
        announce_features(&mut airplay, &id, 0x0001_0040_0000_0000);
        airplay.set_volume(&id, 47.0);
        airplay.poll();
        read_now(&mut airplay);
        report_volume(&fake, &kitchen, -15.6);
        assert_eq!(airplay.poll(), [BackendEvent::AirPlayChanged]);
        assert_eq!(own(&airplay), 48.0);
    }

    // covers: S271
    #[test]
    fn a_read_back_while_the_slider_is_dragged_is_ignored() {
        let fake = FakeAirPlay::new();
        let (mut airplay, kitchen, id) = kitchen_playing(&fake, OPEN_TXT);
        read_now(&mut airplay);
        airplay.set_volume_dragging(&id, true);
        report_volume(&fake, &kitchen, -9.0);
        assert!(airplay.poll().is_empty());
        read_now(&mut airplay);
        report_volume(&fake, &kitchen, -9.0);
        assert!(airplay.poll().is_empty());
        assert!(fake.take_calls().is_empty());
        assert_eq!(airplay.speakers()[0].volume, 50.0);
        // The answer to a read asked during the drag is old after it.
        airplay.set_volume_dragging(&id, false);
        report_volume(&fake, &kitchen, -9.0);
        assert!(airplay.poll().is_empty());
        read_now(&mut airplay);
        report_volume(&fake, &kitchen, -9.0);
        assert_eq!(airplay.poll(), [BackendEvent::AirPlayChanged]);
        assert_eq!(airplay.speakers()[0].volume, 70.0);
    }

    // covers: S271, S291
    #[test]
    fn a_slider_set_with_no_release_stops_holding_the_row_once_idle() {
        let fake = FakeAirPlay::new();
        let (mut airplay, kitchen, id) = kitchen_playing(&fake, OPEN_TXT);
        // A screen reader sets the value: the slider starts editing but
        // never sees a release.
        airplay.set_volume_dragging(&id, true);
        airplay.move_row(&id, 30.0);
        fake.take_calls();
        settle(&mut airplay, &id);
        read_now(&mut airplay);
        report_volume(&fake, &kitchen, -21.0);
        assert!(airplay.poll().is_empty(), "the echo of the level sent");
        // The television's remote then moves it to −24 dB: level 20.
        read_now(&mut airplay);
        report_volume(&fake, &kitchen, -24.0);
        assert_eq!(airplay.poll(), [BackendEvent::AirPlayChanged]);
        assert_eq!(airplay.speakers()[0].volume, 20.0);
    }

    // covers: S271
    #[test]
    fn a_slider_that_keeps_moving_past_the_idle_time_keeps_holding_the_row() {
        let fake = FakeAirPlay::new();
        let (mut airplay, kitchen, id) = kitchen_playing(&fake, OPEN_TXT);
        airplay.set_volume_dragging(&id, true);
        settle(&mut airplay, &id);
        airplay.move_row(&id, 30.0);
        fake.take_calls();
        read_now(&mut airplay);
        report_volume(&fake, &kitchen, -9.0);
        assert!(airplay.poll().is_empty());
        assert_eq!(airplay.speakers()[0].volume, 30.0);
    }

    // covers: S271, S290
    #[test]
    fn a_slider_moved_at_master_0_keeps_holding_the_row() {
        let fake = FakeAirPlay::new();
        let (mut airplay, kitchen, id) = kitchen_playing(&fake, OPEN_TXT);
        airplay.set_master(0.0);
        airplay.set_volume_dragging(&id, true);
        settle(&mut airplay, &id);
        // With k at 0 the move sends nothing, but the slider still moved.
        airplay.move_row(&id, 40.0);
        fake.take_calls();
        read_now(&mut airplay);
        report_volume(&fake, &kitchen, -9.0);
        assert!(airplay.poll().is_empty());
    }

    // covers: S250, S290, S291
    #[test]
    fn a_read_back_near_the_level_sent_or_at_master_0_keeps_the_own_volume() {
        let path = crate::debug_log::test_dir("airplay-volume-rounding-echo").join("log.txt");
        let fake = FakeAirPlay::new();
        let (mut airplay, kitchen, id) = kitchen_playing(&fake, OPEN_TXT);
        airplay.set_debug_log(DebugLog::new(Some(path.clone())));
        // 10.17 sends −26.949 dB; the speaker answers −27.0, its echo.
        airplay.set_volume(&id, 10.17);
        settle(&mut airplay, &id);
        fake.take_calls();
        read_now(&mut airplay);
        report_volume(&fake, &kitchen, -27.0);
        assert!(airplay.poll().is_empty());
        assert!(fake.take_calls().is_empty());
        assert_eq!(airplay.speakers()[0].volume, 10.17);
        assert!(airplay.log.flush(crate::debug_log::TEST_FLUSH));
        let messages = crate::debug_log::read_messages(&path);
        assert!(
            messages
                .iter()
                .any(|message| message.contains("volume: 10.17"))
        );
        assert!(
            !messages
                .iter()
                .any(|message| message.contains("changed on the speaker")),
            "{messages:?}"
        );
        // At master 0 the row shows the speaker's level and the own volume stays.
        airplay.set_master(0.0);
        settle(&mut airplay, &id);
        fake.take_calls();
        read_now(&mut airplay);
        report_volume(&fake, &kitchen, -9.0);
        assert_eq!(airplay.poll(), [BackendEvent::AirPlayChanged]);
        assert_eq!(airplay.speakers()[0].volume, 10.17);
        assert_eq!(row_of(&airplay, &id), 70.0);
    }

    // covers: S250, S290, S291
    #[test]
    fn a_read_back_one_remote_step_away_moves_only_that_speakers_volume() {
        let fake = FakeAirPlay::new();
        let (mut airplay, kitchen, id) = kitchen_playing(&fake, OPEN_TXT);
        let hall = device(2, "Hall", OPEN_TXT);
        let other = id_of(&hall);
        fake.appear(&hall);
        airplay.poll();
        airplay.set_enabled(&other, true).unwrap();
        airplay.set_volume(&other, 50.0);
        airplay.set_master(50.0);
        settle(&mut airplay, &id);
        fake.take_calls();
        // Sent −22.5 dB (level 25), echoed; one press up is −22.2 dB,
        // level 26, so 26 ÷ 0.5 = 52.
        read_now(&mut airplay);
        report_volume(&fake, &kitchen, -22.5);
        assert!(airplay.poll().is_empty());
        read_now(&mut airplay);
        report_volume(&fake, &kitchen, -22.2);
        assert_eq!(airplay.poll(), [BackendEvent::AirPlayChanged]);
        let volume_of = |airplay: &AirPlaySpeakers, id: &str| {
            airplay
                .speakers()
                .into_iter()
                .find(|speaker| speaker.id == id)
                .map(|speaker| (speaker.volume * 1e6).round() / 1e6)
        };
        assert_eq!(volume_of(&airplay, &id), Some(52.0));
        assert_eq!(volume_of(&airplay, &other), Some(50.0));
        assert_eq!(row_of(&airplay, &id), 26.0);
        assert_eq!(
            fake.take_calls(),
            [Call::KeepVolume(id.clone(), Volume::Db(-22.2))]
        );
        // With the master at 0 the own volume stays.
        airplay.set_master(0.0);
        settle(&mut airplay, &id);
        read_now(&mut airplay);
        report_volume(&fake, &kitchen, -6.0);
        airplay.poll();
        assert_eq!(volume_of(&airplay, &id), Some(52.0));
    }

    // covers: S271
    #[test]
    fn a_speaker_whose_reads_stopped_is_not_asked_again() {
        let fake = FakeAirPlay::new();
        let (mut airplay, kitchen, id) = kitchen_playing(&fake, OPEN_TXT);
        read_now(&mut airplay);
        assert_eq!(fake.take_reads(), [id.as_str()]);
        warn(&fake, &kitchen, Warning::VolumeReadsStopped);
        report_volume(&fake, &kitchen, -6.0);
        airplay.poll();
        assert_eq!(airplay.speakers()[0].volume, 50.0);
        read_now(&mut airplay);
        assert!(fake.take_reads().is_empty());
    }

    // covers: S271
    #[test]
    fn a_new_session_asks_a_speaker_whose_reads_stopped_again() {
        let refused = Warning::RequestFailed {
            method: Method::GetParameter,
            status: 501,
        };
        for (warning, states) in [
            (
                Warning::VolumeReadsStopped,
                vec![
                    SpeakerState::Retrying { at: Instant::now() },
                    SpeakerState::Connecting,
                    SpeakerState::Playing,
                ],
            ),
            (
                refused,
                vec![SpeakerState::Connecting, SpeakerState::Playing],
            ),
        ] {
            let fake = FakeAirPlay::new();
            let (mut airplay, kitchen, id) = kitchen_playing(&fake, OPEN_TXT);
            read_now(&mut airplay);
            fake.take_reads();
            warn(&fake, &kitchen, warning.clone());
            airplay.poll();
            for state in states {
                fake.state(&kitchen, state);
            }
            airplay.poll();
            read_now(&mut airplay);
            assert_eq!(fake.take_reads(), [id.as_str()], "{warning:?}");
        }
    }

    /// The first speaker's own volume to 6 decimals, which S290 keeps
    /// unrounded.
    fn own(airplay: &AirPlaySpeakers) -> f64 {
        (airplay.speakers()[0].volume * 1e6).round() / 1e6
    }

    /// The whole number speaker `id`'s row shows (S290).
    fn row_of(airplay: &AirPlaySpeakers, id: &str) -> f64 {
        let info = airplay
            .speakers()
            .into_iter()
            .find(|speaker| speaker.id == id)
            .expect("listed");
        row_level(info.device_level)
    }

    // covers: S289, S290
    #[test]
    fn the_row_shows_the_device_level_rounded_down() {
        let fake = FakeAirPlay::new();
        let (mut airplay, _, id) = kitchen_playing(&fake, OPEN_TXT);
        airplay.set_master(63.0);
        for (own, row) in [
            (0.0, 0.0),
            (1.0, 0.0),
            (50.0, 31.0),
            (90.0, 56.0),
            (100.0, 63.0),
        ] {
            airplay.set_volume(&id, own);
            assert_eq!(row_of(&airplay, &id), row, "own {own}");
        }
    }

    // covers: S289, S290
    #[test]
    fn a_row_move_sends_the_row_and_stores_the_row_over_k() {
        let fake = FakeAirPlay::new();
        let (mut airplay, _, id) = kitchen_playing(&fake, OPEN_TXT);
        airplay.set_master(63.0);
        fake.take_calls();
        airplay.move_row(&id, 20.0);
        assert_eq!(own(&airplay), 31.746032);
        assert_eq!(row_of(&airplay, &id), 20.0);
        assert_eq!(sent_levels(&fake), [(id.clone(), Some(-24.0))]);
        // A master move keeps own 20 ÷ 0.63.
        airplay.set_master(100.0);
        assert_eq!(own(&airplay), 31.746032);
        assert_eq!(row_of(&airplay, &id), 31.0);
        assert_eq!(sent_levels(&fake), [(id.clone(), Some(-20.5))]);
        // The master is no ceiling: own passes 100 and the row is the one moved to.
        airplay.set_master(50.0);
        fake.take_calls();
        airplay.move_row(&id, 80.0);
        assert_eq!(own(&airplay), 160.0);
        assert_eq!(row_of(&airplay, &id), 80.0);
        assert_eq!(sent_levels(&fake), [(id.clone(), Some(-6.0))]);
        // Up to the cap at 100 and back gives 80 again.
        airplay.set_master(100.0);
        assert_eq!(row_of(&airplay, &id), 100.0);
        airplay.set_master(50.0);
        assert_eq!(row_of(&airplay, &id), 80.0);
        // With k at 0 nothing is sent and own stays.
        airplay.set_master(0.0);
        fake.take_calls();
        airplay.move_row(&id, 40.0);
        assert!(fake.take_calls().is_empty());
        assert_eq!(own(&airplay), 160.0);
        assert_eq!(row_of(&airplay, &id), 0.0);
    }

    // covers: S290, S291
    #[test]
    fn an_echo_on_a_0_6_db_grid_never_moves_the_row() {
        let fake = FakeAirPlay::new();
        let (mut airplay, kitchen, id) = kitchen_playing(&fake, OPEN_TXT);
        airplay.set_master(63.0);
        airplay.move_row(&id, 20.0);
        fake.take_calls();
        // Sent −24 dB; the television answers on its 0.6 dB grid, however late.
        for _ in 0..3 {
            read_now(&mut airplay);
            report_volume(&fake, &kitchen, -24.0);
            assert!(airplay.poll().is_empty());
        }
        assert!(fake.take_calls().is_empty());
        assert_eq!(row_of(&airplay, &id), 20.0);
        assert_eq!(own(&airplay), 31.746032);
        // One press up on its remote: −23.7 dB, level 21, own 21 ÷ 0.63.
        read_now(&mut airplay);
        report_volume(&fake, &kitchen, -23.7);
        assert_eq!(airplay.poll(), [BackendEvent::AirPlayChanged]);
        assert_eq!(row_of(&airplay, &id), 21.0);
        assert_eq!(own(&airplay), 33.333333);
        // Sent back, −23.7 dB would land on −24 dB and undo the press.
        assert_eq!(
            fake.take_calls(),
            [Call::KeepVolume(id.clone(), Volume::Db(-23.7))]
        );
        // The same level read again changes nothing.
        read_now(&mut airplay);
        report_volume(&fake, &kitchen, -23.7);
        assert!(airplay.poll().is_empty());
        assert_eq!(row_of(&airplay, &id), 21.0);
    }

    // covers: S290
    #[test]
    fn a_master_or_zone_move_and_back_gives_the_level_read_before() {
        let fake = FakeAirPlay::new();
        let (mut airplay, kitchen, id) = kitchen_playing(&fake, OPEN_TXT);
        airplay.set_master(63.0);
        read_now(&mut airplay);
        report_volume(&fake, &kitchen, -23.7);
        assert_eq!(airplay.poll(), [BackendEvent::AirPlayChanged]);
        assert_eq!(own(&airplay), 33.333333);
        assert_eq!(row_of(&airplay, &id), 21.0);
        airplay.set_master(100.0);
        assert_eq!(row_of(&airplay, &id), 33.0);
        airplay.set_master(63.0);
        assert_eq!(own(&airplay), 33.333333);
        assert_eq!(row_of(&airplay, &id), 21.0);
        // The same for a zone volume: after the echo of 21, level 21.9 reads
        // as own 21.9 ÷ 0.63 and row 21, which it shows again after the zone.
        read_now(&mut airplay);
        report_volume(&fake, &kitchen, -23.67);
        assert!(airplay.poll().is_empty());
        read_now(&mut airplay);
        report_volume(&fake, &kitchen, -23.43);
        assert_eq!(airplay.poll(), [BackendEvent::AirPlayChanged]);
        assert_eq!(own(&airplay), 34.761905);
        assert_eq!(row_of(&airplay, &id), 21.0);
        airplay.set_zones(BTreeMap::from([(id.clone(), 50.0)]));
        assert_eq!(row_of(&airplay, &id), 10.0);
        airplay.set_zones(BTreeMap::new());
        assert_eq!(row_of(&airplay, &id), 21.0);
    }

    // covers: S291
    #[test]
    fn a_speaker_is_read_once_a_second_while_playing_and_never_while_stopped() {
        assert_eq!(VOLUME_READ_INTERVAL, Duration::from_secs(1));
        let fake = FakeAirPlay::new();
        let (mut airplay, kitchen, id) = kitchen_playing(&fake, OPEN_TXT);
        read_now(&mut airplay);
        assert_eq!(fake.take_reads(), [id.as_str()]);
        for state in [SpeakerState::Stopped, SpeakerState::Connecting] {
            fake.state(&kitchen, state);
            airplay.poll();
            read_now(&mut airplay);
            assert!(fake.take_reads().is_empty(), "{state:?}");
        }
    }

    // covers: S276, S290
    #[test]
    fn a_device_volume_reports_the_level_it_was_sent() {
        for master in [100.0, 50.0, 25.0] {
            for position in (1..=100).map(f64::from) {
                let db = airplay_volume(position, NO_ZONE_POSITION, master).wire_db();
                let level = device_level_for_db(db).expect("a finite reading");
                let expected = device_level(position, NO_ZONE_POSITION, master);
                assert!(
                    (level - expected).abs() < 1e-9,
                    "{position} at {master}: {level}"
                );
            }
        }
        assert_eq!(device_level_for_db(-144.0), Some(0.0), "muted");
        assert_eq!(device_level_for_db(3.0), Some(100.0));
        assert_eq!(held_level(40.0, NO_ZONE_POSITION, 0.0), Some(40.0));
        assert_eq!(held_level(40.0, NO_ZONE_POSITION, 50.0), None);
    }

    #[test]
    fn a_volume_changed_on_the_speaker_moves_its_slider_without_an_echo() {
        let fake = FakeAirPlay::new();
        let (mut airplay, kitchen, id) = kitchen_playing(&fake, OPEN_TXT);
        // Not asked yet: the answer isn't counted.
        report_volume(&fake, &kitchen, -6.0);
        assert!(airplay.poll().is_empty());
        read_now(&mut airplay);
        assert_eq!(fake.take_reads(), [id.as_str()]);
        // Its own level (−15 dB at 50), within the speaker's rounding.
        airplay.set_volume(&id, 49.0);
        airplay.set_volume(&id, 50.0);
        fake.take_calls();
        read_now(&mut airplay);
        report_volume(&fake, &kitchen, -14.95);
        assert!(airplay.poll().is_empty());
        assert_eq!(airplay.speakers()[0].volume, 50.0);
        read_now(&mut airplay);
        report_volume(&fake, &kitchen, -6.0);
        assert_eq!(airplay.poll(), [BackendEvent::AirPlayChanged]);
        assert_eq!(airplay.speakers()[0].volume, 80.0);
        // The sender keeps the level for a restart; nothing goes out.
        assert_eq!(
            fake.take_calls(),
            [Call::KeepVolume(id.clone(), Volume::Db(-6.0))]
        );
        read_now(&mut airplay);
        report_volume(&fake, &kitchen, -6.0);
        assert!(airplay.poll().is_empty(), "the same level is no news");
        assert!(fake.take_calls().is_empty());
    }

    #[test]
    fn a_level_set_on_the_speaker_never_goes_back_to_it() {
        // A television that keeps sets on a 0.6 dB grid turns a −26.7 dB
        // sent back into −27 dB, undoing its remote's 0.3 dB press (S291).
        let fake = FakeAirPlay::new();
        let (mut airplay, kitchen, id) = kitchen_playing(&fake, OPEN_TXT);
        airplay.set_volume(&id, 40.0);
        read_now(&mut airplay);
        report_volume(&fake, &kitchen, -18.0);
        assert!(airplay.poll().is_empty());
        fake.take_calls();
        read_now(&mut airplay);
        report_volume(&fake, &kitchen, -26.7);
        assert_eq!(airplay.poll(), [BackendEvent::AirPlayChanged]);
        assert_eq!(own(&airplay), 11.0);
        assert_eq!(
            fake.take_calls(),
            [Call::KeepVolume(id.clone(), Volume::Db(-26.7))]
        );
    }

    #[test]
    fn the_sender_keeps_the_speakers_own_level_whatever_the_master() {
        let fake = FakeAirPlay::new();
        let (mut airplay, kitchen, id) = kitchen_playing(&fake, OPEN_TXT);
        airplay.set_volume(&id, 80.0);
        airplay.set_master(50.0);
        settle(&mut airplay, &id);
        fake.take_calls();
        // −3 dB is level 90: own 90 ÷ 0.5, past 100, since the master is no
        // ceiling (S290); the speaker keeps −3 dB.
        read_now(&mut airplay);
        report_volume(&fake, &kitchen, -3.0);
        assert_eq!(airplay.poll(), [BackendEvent::AirPlayChanged]);
        assert_eq!(own(&airplay), 180.0);
        assert_eq!(row_of(&airplay, &id), 90.0);
        assert_eq!(
            fake.take_calls(),
            [Call::KeepVolume(id.clone(), Volume::Db(-3.0))]
        );
        read_now(&mut airplay);
        report_volume(&fake, &kitchen, -3.0);
        assert!(airplay.poll().is_empty(), "the level the sender has");
        assert!(fake.take_calls().is_empty());
        read_now(&mut airplay);
        report_volume(&fake, &kitchen, -1.0);
        assert_eq!(airplay.poll(), [BackendEvent::AirPlayChanged]);
        assert_eq!(own(&airplay), 193.333333);
        assert_eq!(row_of(&airplay, &id), 96.0);
        assert_eq!(
            fake.take_calls(),
            [Call::KeepVolume(id.clone(), Volume::Db(-1.0))]
        );
    }

    // covers: S249
    #[test]
    fn a_zone_volume_reaches_a_playing_speaker_and_its_read_back_goes_through_it() {
        let fake = FakeAirPlay::new();
        let (mut airplay, kitchen, id) = kitchen_playing(&fake, OPEN_TXT);
        airplay.set_volume(&id, 80.0);
        settle(&mut airplay, &id);
        fake.take_calls();
        // 80 % of a zone at 50 % is level 0.4, −18 dB, at once (S249).
        airplay.set_zones(BTreeMap::from([(id.clone(), 50.0)]));
        assert_eq!(
            fake.take_calls(),
            [Call::Volume(id.clone(), Volume::Db(-18.0))]
        );
        airplay.set_zones(BTreeMap::from([(id.clone(), 50.0)]));
        assert!(fake.take_calls().is_empty(), "unchanged: nothing goes out");
        // −21 dB is level 0.3: own 0.3 ÷ 0.5, so 60 (S250).
        settle(&mut airplay, &id);
        read_now(&mut airplay);
        report_volume(&fake, &kitchen, -21.0);
        assert_eq!(airplay.poll(), [BackendEvent::AirPlayChanged]);
        assert_eq!(airplay.speakers()[0].volume, 60.0);
        fake.take_calls();
        // A zone at 0 mutes it, and a read-back then leaves the own volume.
        airplay.set_zones(BTreeMap::from([(id.clone(), 0.0)]));
        assert_eq!(fake.take_calls(), [Call::Volume(id.clone(), Volume::Muted)]);
        settle(&mut airplay, &id);
        read_now(&mut airplay);
        report_volume(&fake, &kitchen, -6.0);
        airplay.poll();
        assert_eq!(airplay.speakers()[0].volume, 60.0);
        fake.take_calls();
        // Out of the zone: its own volume times the master again.
        airplay.set_zones(BTreeMap::new());
        assert_eq!(
            fake.take_calls(),
            [Call::Volume(id.clone(), Volume::Db(-12.0))]
        );
    }

    #[test]
    fn a_retrying_speaker_takes_new_records_with_its_own_level_and_delay() {
        let fake = FakeAirPlay::new();
        let (mut airplay, kitchen, id) = kitchen_playing(&fake, OPEN_TXT);
        airplay.set_volume(&id, 80.0);
        airplay.set_master(50.0);
        airplay.set_delay(&id, 120.0);
        settle(&mut airplay, &id);
        read_now(&mut airplay);
        report_volume(&fake, &kitchen, -3.0);
        airplay.poll();
        fake.take_calls();

        fake.state(
            &kitchen,
            SpeakerState::Retrying {
                at: Instant::now() + Duration::from_secs(1),
            },
        );
        let moved = device_at(
            1,
            "Kitchen",
            OPEN_TXT,
            IpAddr::V4(Ipv4Addr::new(192, 0, 2, 11)),
            7000,
        );
        fake.change(&moved);
        airplay.poll();
        let calls = fake.take_calls();
        let [
            Call::Add {
                id: added,
                volume,
                delay,
            },
        ] = calls.as_slice()
        else {
            panic!("one add: {calls:?}");
        };
        assert_eq!((added, *delay), (&id, Duration::from_millis(120)));
        assert!(
            (volume.wire_db() + 3.0).abs() < 1e-9,
            "the level the speaker reported, not 100 × k: {volume:?}"
        );
        assert_eq!(airplay.listed[&id].device(), &moved);
    }

    // covers: S290
    #[test]
    fn a_level_held_at_k_0_comes_back_after_a_calibration_and_goes_at_an_add() {
        let fake = FakeAirPlay::new();
        let (mut airplay, kitchen, id) = kitchen_playing(&fake, OPEN_TXT);
        airplay.set_master(0.0);
        let hold_30 = |airplay: &mut AirPlaySpeakers| {
            settle(airplay, &id);
            read_now(airplay);
            report_volume(&fake, &kitchen, -21.0);
            assert_eq!(airplay.poll(), [BackendEvent::AirPlayChanged]);
            assert_eq!(row_of(airplay, &id), 30.0);
        };
        hold_30(&mut airplay);
        // The end of a calibration gives the speaker the level it held back.
        airplay.set_calibration_solo(Some("another speaker"));
        fake.take_calls();
        airplay.set_calibration_solo(None);
        assert_eq!(sent_levels(&fake), [(id.clone(), Some(-21.0))]);
        assert_eq!(row_of(&airplay, &id), 30.0);
        // Adding it again when the stream starts again sends the level k gives.
        airplay.set_streaming(false);
        airplay.poll();
        fake.take_calls();
        airplay.set_streaming(true);
        airplay.poll();
        assert!(
            fake.take_calls().iter().any(|call| matches!(
                call,
                Call::Add {
                    volume: Volume::Muted,
                    ..
                }
            )),
            "added again at the level k gives"
        );
        assert_eq!(row_of(&airplay, &id), 0.0);
        assert_eq!(own(&airplay), 50.0);
    }

    // covers: S290
    #[test]
    fn a_level_held_at_k_0_stays_when_the_speaker_joins_or_leaves_a_zone_at_100() {
        let fake = FakeAirPlay::new();
        let (mut airplay, kitchen, id) = kitchen_playing(&fake, OPEN_TXT);
        airplay.set_master(0.0);
        settle(&mut airplay, &id);
        read_now(&mut airplay);
        report_volume(&fake, &kitchen, -21.0);
        assert_eq!(airplay.poll(), [BackendEvent::AirPlayChanged]);
        assert_eq!(row_of(&airplay, &id), 30.0);
        airplay.set_zones(BTreeMap::from([(id.clone(), 100.0)]));
        assert_eq!(row_of(&airplay, &id), 30.0);
        airplay.set_zones(BTreeMap::new());
        assert_eq!(row_of(&airplay, &id), 30.0);
    }

    // covers: S276, S290
    #[test]
    fn a_device_volume_at_k_0_holds_its_level_and_keeps_the_own_volume() {
        let fake = FakeAirPlay::new();
        let (mut airplay, kitchen, id) = kitchen_playing(&fake, OPEN_TXT);
        airplay.set_master(0.0);
        settle(&mut airplay, &id);
        fake.take_calls();
        remote(
            &fake,
            &kitchen,
            RemoteCommand::DeviceVolume(Volume::Db(-18.0)),
        );
        assert_eq!(airplay.poll(), [BackendEvent::AirPlayChanged]);
        assert_eq!(row_of(&airplay, &id), 40.0);
        assert_eq!(own(&airplay), 50.0);
        assert_eq!(sent_levels(&fake), [(id.clone(), Some(-18.0))]);
        // A master move gives the row min(100, s × k) again.
        airplay.set_master(100.0);
        assert_eq!(row_of(&airplay, &id), 50.0);
    }

    // covers: S271, S276, S290
    #[test]
    fn the_echo_of_a_level_held_at_k_0_changes_nothing() {
        let fake = FakeAirPlay::new();
        let (mut airplay, kitchen, id) = kitchen_playing(&fake, OPEN_TXT);
        airplay.set_master(0.0);
        settle(&mut airplay, &id);
        let device_volume = RemoteCommand::DeviceVolume(Volume::Db(-18.0));
        remote(&fake, &kitchen, device_volume);
        assert_eq!(airplay.poll(), [BackendEvent::AirPlayChanged]);
        fake.take_calls();
        // The speaker echoes the level sent back inside the 3 s window, as
        // sent and on the same row as it rounds.
        for echoed in [-18.0, -17.95] {
            remote(
                &fake,
                &kitchen,
                RemoteCommand::DeviceVolume(Volume::Db(echoed)),
            );
            assert!(airplay.poll().is_empty(), "{echoed} moved the row");
            assert!(sent_levels(&fake).is_empty(), "{echoed} went out again");
            assert_eq!(row_of(&airplay, &id), 40.0);
        }
    }

    #[test]
    fn a_level_near_the_bottom_moves_the_row_to_0_without_muting() {
        let fake = FakeAirPlay::new();
        let (mut airplay, kitchen, id) = kitchen_playing(&fake, OPEN_TXT);
        read_now(&mut airplay);
        report_volume(&fake, &kitchen, -29.9);
        assert_eq!(airplay.poll(), [BackendEvent::AirPlayChanged]);
        assert_eq!(row_of(&airplay, &id), 0.0);
        assert_eq!(own(&airplay), 0.333333);
        assert_eq!(
            fake.take_calls(),
            [Call::KeepVolume(id.clone(), Volume::Db(-29.9))]
        );
    }

    #[test]
    fn only_a_ptp_bind_after_the_clash_says_the_ports_are_free() {
        let fake = FakeAirPlay::new();
        let (mut airplay, kitchen, _) = kitchen_playing(&fake, OPEN_TXT);
        warn(&fake, &kitchen, Warning::PtpUnavailable);
        airplay.poll();
        fake.state(&kitchen, SpeakerState::Connecting);
        fake.state(&kitchen, SpeakerState::Playing);
        assert!(!airplay.poll().contains(&BackendEvent::AirPlayPtpFree));
        fake.state(&kitchen, SpeakerState::Connecting);
        fake.push(SenderEvent::Speaker {
            key: kitchen.key.clone(),
            event: SpeakerEvent::PtpOpened,
        });
        assert!(airplay.poll().contains(&BackendEvent::AirPlayPtpFree));
    }

    #[test]
    fn a_level_past_the_wire_range_is_kept_once_as_its_nearest() {
        let fake = FakeAirPlay::new();
        let (mut airplay, kitchen, id) = kitchen_playing(&fake, OPEN_TXT);
        read_now(&mut airplay);
        report_volume(&fake, &kitchen, -40.0);
        assert_eq!(airplay.poll(), [BackendEvent::AirPlayChanged]);
        assert_eq!(airplay.speakers()[0].volume, 0.0);
        assert_eq!(
            fake.take_calls(),
            [Call::KeepVolume(id.clone(), Volume::Db(MIN_DB))]
        );
        read_now(&mut airplay);
        report_volume(&fake, &kitchen, -40.0);
        assert!(airplay.poll().is_empty());
        assert!(fake.take_calls().is_empty(), "kept once");
    }

    #[test]
    fn a_volume_read_before_the_slider_moved_is_dropped() {
        let fake = FakeAirPlay::new();
        let (mut airplay, kitchen, id) = kitchen_playing(&fake, OPEN_TXT);
        read_now(&mut airplay);
        airplay.set_volume(&id, 70.0);
        report_volume(&fake, &kitchen, -3.0);
        assert!(airplay.poll().is_empty());
        assert_eq!(airplay.speakers()[0].volume, 70.0);
        // The master sets every speaker, so it drops a read too.
        settle(&mut airplay, &id);
        read_now(&mut airplay);
        airplay.set_master(50.0);
        report_volume(&fake, &kitchen, -3.0);
        assert!(airplay.poll().is_empty());
        assert_eq!(airplay.speakers()[0].volume, 70.0);
    }

    #[test]
    fn speakers_that_cant_answer_are_not_asked() {
        let fake = FakeAirPlay::new();
        let mut txt = OPEN_TXT.to_vec();
        txt.push("sv=true");
        let (mut airplay, _, _) = kitchen_playing(&fake, &txt);
        read_now(&mut airplay);
        assert!(fake.take_reads().is_empty(), "it scales the samples itself");

        let fake = FakeAirPlay::new();
        let (mut airplay, kitchen_open, open) = kitchen_playing(&fake, OPEN_TXT);
        read_now(&mut airplay);
        assert_eq!(fake.take_reads(), [open.as_str()]);
        warn(
            &fake,
            &kitchen_open,
            Warning::RequestFailed {
                method: Method::GetParameter,
                status: 501,
            },
        );
        airplay.poll();
        read_now(&mut airplay);
        assert!(fake.take_reads().is_empty(), "it turned a read down");
    }

    // covers: S291
    #[test]
    fn a_reading_during_a_calibration_moves_the_row_and_the_speaker_stays_silent() {
        let fake = FakeAirPlay::new();
        let (mut airplay, _, k) = kitchen_playing(&fake, OPEN_TXT);
        let study = device(2, "Study", OPEN_TXT);
        let s = id_of(&study);
        fake.appear(&study);
        airplay.poll();
        airplay.set_enabled(&s, true).unwrap();
        airplay.set_volume(&s, 50.0);
        fake.state(&study, SpeakerState::Playing);
        airplay.poll();
        airplay.set_calibration_solo(Some(&k));
        settle(&mut airplay, &s);
        fake.take_calls();
        read_now(&mut airplay);
        assert!(fake.take_reads().contains(&s));
        // Turned up on Study's remote while only Kitchen is heard.
        report_volume(&fake, &study, -6.0);
        assert_eq!(airplay.poll(), [BackendEvent::AirPlayChanged]);
        assert_eq!(row_of(&airplay, &s), 80.0);
        assert_eq!(
            fake.take_calls(),
            [Call::KeepVolume(s.clone(), Volume::Muted)]
        );
        airplay.set_calibration_solo(None);
        let sent = sent_levels(&fake);
        assert!(sent.contains(&(s.clone(), Some(-6.0))));
        assert!(
            !sent.contains(&(s.clone(), Some(-15.0))),
            "not the old level"
        );
    }

    // covers: S291
    #[test]
    fn a_pair_reads_through_its_leader_and_keeps_the_level_in_the_sender() {
        let fake = FakeAirPlay::new();
        let (pair, left, right) = stereo_pair();
        let mut airplay = with_target(&fake, &pair, &[&left, &right]);
        airplay.set_streaming(true);
        airplay.set_enabled(PAIR, true).expect("the pair can play");
        fake.state(&left, SpeakerState::Playing);
        airplay.poll();
        settle(&mut airplay, PAIR);
        fake.take_calls();
        read_now(&mut airplay);
        assert_eq!(fake.take_reads(), [format!("target {PAIR}")]);
        report_volume(&fake, &left, -144.0);
        assert_eq!(airplay.poll(), [BackendEvent::AirPlayChanged]);
        assert_eq!(airplay.speakers()[0].volume, 0.0, "muted on the speaker");
        assert_eq!(
            fake.take_calls(),
            [Call::TargetVolume(PAIR.to_owned(), Volume::Muted)]
        );
    }

    #[test]
    fn only_the_ptp_port_clash_asks_for_a_notice() {
        let fake = FakeAirPlay::new();
        let (mut airplay, kitchen, _) = kitchen_playing(&fake, OPEN_TXT);
        warn(&fake, &kitchen, Warning::PtpOutsideFamily);
        assert!(airplay.poll().is_empty());
        warn(&fake, &kitchen, Warning::PtpUnavailable);
        assert_eq!(airplay.poll(), [BackendEvent::AirPlayPtpPortsTaken]);
    }

    /// A stereo pair of two AirPlay 2 speakers, Left and Right (P173).
    pub fn stereo_pair() -> (Target, Device, Device) {
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
        let volume = airplay_volume(DEFAULT_LOCAL_POSITION, NO_ZONE_POSITION, 100.0);
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
                Call::TargetVolume(
                    PAIR.to_owned(),
                    airplay_volume(40.0, NO_ZONE_POSITION, 100.0)
                ),
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

        fake.lose(&left, ErrorKind::Lost);
        let events = airplay.poll();
        assert!(events.contains(&BackendEvent::AirPlayTrouble {
            id: PAIR.to_owned(),
            problem: AirPlayProblem::ConnectionLost,
        }));
        assert!(airplay.speakers()[0].enabled, "AudioSync keeps trying");
    }

    // covers: P177, P229
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
            airplay_volume(DEFAULT_LOCAL_POSITION, NO_ZONE_POSITION, 100.0)
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
            airplay
                .holding_target(&id_of(&left))
                .map(|target| target.name)
                .as_deref(),
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

    /// The stereo pair, switched on with Right's password typed, where Right
    /// is reached over AirPlay 2 and its flags ask for a password and no
    /// pairing.
    fn pair_on_with_typed_password(fake: &FakeAirPlay) -> (AirPlaySpeakers, Device, Device) {
        let (mut pair, left, _) = stereo_pair();
        let right = airplay2_flagged(22, "Right", 0x80);
        pair.members[1] = member_of(&right);
        let mut airplay = with_target(fake, &pair, &[&left, &right]);
        airplay.set_streaming(true);
        assert_eq!(
            airplay.set_enabled(PAIR, true),
            Err(AirPlayProblem::NeedsPassword)
        );
        airplay.ask_password(PAIR).expect("the prompt opens");
        let prompt = airplay.password_prompt().expect("a prompt");
        assert_eq!((prompt.name.as_str(), prompt.wrong), ("Right", false));
        assert!(airplay.submit_password(&id_of(&right), "hunter2"));
        assert!(matches!(&fake.take_calls()[..], [Call::AddTarget { id, .. }] if id == PAIR));
        (airplay, left, right)
    }

    // covers: P176
    #[test]
    fn a_password_only_member_over_airplay2_that_answers_pairing_required_is_asked_by_name() {
        let fake = FakeAirPlay::new();
        let (mut airplay, left, right) = pair_on_with_typed_password(&fake);
        fake.fail(&right, ErrorKind::PairingRequired);
        let events = airplay.poll();
        assert!(
            !events.iter().any(|event| matches!(
                event,
                BackendEvent::AirPlayFailed { .. }
                    | BackendEvent::AirPlayMemberNeedsPassword { .. }
            )),
            "the prompt asks instead: {events:?}"
        );
        assert_eq!(fake.take_calls(), [Call::RemoveTarget(PAIR.to_owned())]);
        let prompt = airplay.password_prompt().expect("a prompt");
        assert_eq!(
            (prompt.id.as_str(), prompt.name.as_str()),
            (id_of(&right).as_str(), "Right")
        );

        // Outside the attempt the pair goes off with a notice naming Right.
        assert!(airplay.submit_password(&id_of(&right), "hunter2"));
        fake.state(&left, SpeakerState::Playing);
        airplay.poll();
        fake.fail(&right, ErrorKind::PairingRequired);
        let events = airplay.poll();
        assert!(events.contains(&BackendEvent::AirPlayMemberNeedsPassword {
            id: PAIR.to_owned(),
            member: "Right".to_owned(),
        }));
        assert!(airplay.enabled_ids().is_empty());
        assert_eq!(airplay.password_prompt(), None);
    }

    // covers: P176
    #[test]
    fn a_member_asked_again_for_a_password_typed_this_run_gets_the_turned_down_prompt() {
        for kind in [ErrorKind::PasswordRequired, ErrorKind::PairingRequired] {
            let fake = FakeAirPlay::new();
            let (mut airplay, _, right) = pair_on_with_typed_password(&fake);
            fake.fail(&right, kind);
            airplay.poll();
            let prompt = airplay.password_prompt().expect("a prompt");
            assert_eq!(
                (prompt.name.as_str(), prompt.wrong),
                ("Right", true),
                "{kind:?}"
            );
        }
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
        assert_eq!(
            airplay_volume(100.0, NO_ZONE_POSITION, 100.0),
            Volume::Db(0.0)
        );
        assert_eq!(
            airplay_volume(50.0, NO_ZONE_POSITION, 100.0),
            Volume::Db(-15.0)
        );
        assert_eq!(
            airplay_volume(60.0, NO_ZONE_POSITION, 100.0),
            Volume::Db(-12.0)
        );
        assert_eq!(airplay_volume(100.0, NO_ZONE_POSITION, 0.0), Volume::Muted);
        assert_eq!(airplay_volume(0.0, NO_ZONE_POSITION, 100.0), Volume::Muted);
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
    /// Like the measured television, it holds the last volume set and
    /// answers a volume read with it (S289, S291).
    struct LoopbackReceiver {
        port: u16,
        methods: Arc<Mutex<Vec<String>>>,
        level: Arc<Mutex<Option<f64>>>,
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
            let level = Arc::new(Mutex::new(None));
            let stop = Arc::new(AtomicBool::new(false));
            let thread = {
                let (methods, level, stop) =
                    (Arc::clone(&methods), Arc::clone(&level), Arc::clone(&stop));
                thread::spawn(move || {
                    let _udp = udp;
                    while !stop.load(Ordering::SeqCst) {
                        match listener.accept() {
                            Ok((stream, _)) => serve(stream, &transport, &methods, &level, &stop),
                            Err(_) => thread::sleep(Duration::from_millis(10)),
                        }
                    }
                })
            };
            LoopbackReceiver {
                port,
                methods,
                level,
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

        /// The level it holds, in dB, once it has one.
        fn level(&self) -> Option<f64> {
            *self.level.lock().unwrap_or_else(PoisonError::into_inner)
        }

        /// Its own remote sets its level to `db`.
        fn press(&self, db: f64) {
            *self.level.lock().unwrap_or_else(PoisonError::into_inner) = Some(db);
        }

        /// Whether it holds `db`, to six decimals, within 5 s.
        fn reaches(&self, db: f64) -> bool {
            let until = Instant::now() + Duration::from_secs(5);
            while Instant::now() < until {
                if self.level().is_some_and(|got| (got - db).abs() < 5e-6) {
                    return true;
                }
                thread::sleep(Duration::from_millis(10));
            }
            false
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
        level: &Mutex<Option<f64>>,
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
                let mut level = level.lock().unwrap_or_else(PoisonError::into_inner);
                if matches!(request.method, Method::SetParameter) {
                    *level = parse_reported(&request.body).or(*level);
                }
                if stream
                    .write_all(&answer(&request, transport, *level).encode())
                    .is_err()
                {
                    return;
                }
            }
        }
    }

    /// Success, with what each method's answer carries; `level` answers a
    /// volume read.
    fn answer(request: &Request, transport: &str, level: Option<f64>) -> Response {
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
            Method::GetParameter => {
                if let Some(db) = level {
                    response.headers.push("Content-Type", "text/parameters");
                    response.body = format!("volume: {db:.6}\r\n").into_bytes();
                }
            }
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
        let (link, input) = SenderLink::start(false).expect("the sender starts");
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

    /// A passed dry-run cell for `scripts/fake_tv_cells.sh`, which runs
    /// this test with `--nocapture`; a plain `cargo test` swallows it.
    fn cell(id: &str, measured: &str) {
        eprintln!("CELL {id} {measured}");
    }

    /// Polls `airplay` until `done` holds, at most 15 s.
    fn poll_speakers_until(
        airplay: &mut AirPlaySpeakers,
        mut done: impl FnMut(&AirPlaySpeakers) -> bool,
    ) -> bool {
        let until = Instant::now() + Duration::from_secs(15);
        while Instant::now() < until {
            airplay.poll();
            if done(airplay) {
                return true;
            }
            thread::sleep(Duration::from_millis(20));
        }
        false
    }

    /// A television playing at `master` in a zone at `zone` (100: none),
    /// set to 30 on its own remote and given that level back after another
    /// speaker's calibration.
    fn a_tv_holding_30_after_calibrate_all(
        master: f64,
        zone: f64,
    ) -> (LoopbackReceiver, InputWriter, AirPlaySpeakers, String) {
        let receiver = LoopbackReceiver::start();
        let (link, input) = SenderLink::start(false).expect("the sender starts");
        link.sender
            .start_discovery_with(Box::new(OneService(Some(BrowseEvent::Resolved(
                receiver.record(),
            )))));
        let mut airplay = AirPlaySpeakers::new(
            Box::new(link),
            master,
            DebugLog::default(),
            PairingStore::in_memory(),
        );
        assert!(
            poll_speakers_until(&mut airplay, |airplay| !airplay.speakers().is_empty()),
            "the television is listed"
        );
        let id = airplay.speakers()[0].id.clone();
        airplay.set_zones(BTreeMap::from([(id.clone(), zone)]));
        airplay.set_enabled(&id, true).unwrap();
        airplay.set_streaming(true);
        assert!(
            poll_speakers_until(&mut airplay, |airplay| airplay.speakers()[0].playing),
            "it plays: {:?}",
            receiver.methods()
        );
        assert!(receiver.reaches(MUTED_DB), "{:?}", receiver.level());

        // Its own remote to 30 (-21 dB), read while k is 0.
        receiver.press(-21.0);
        settle(&mut airplay, &id);
        read_now(&mut airplay);
        assert!(
            poll_speakers_until(&mut airplay, |airplay| row_of(airplay, &id) == 30.0),
            "the row shows {}",
            row_of(&airplay, &id)
        );

        airplay.set_calibration_solo(Some("another speaker"));
        assert!(receiver.reaches(MUTED_DB), "{:?}", receiver.level());
        airplay.set_calibration_solo(None);
        assert!(
            receiver.reaches(-21.0),
            "after the calibration: {:?}",
            receiver.level()
        );
        assert_eq!(row_of(&airplay, &id), 30.0);
        (receiver, input, airplay, id)
    }

    // covers: S290
    #[test]
    fn the_tv_keeps_a_level_held_at_master_0_through_calibrate_all_and_a_zone_at_100() {
        let (receiver, _input, mut airplay, id) = a_tv_holding_30_after_calibrate_all(0.0, 100.0);
        for zones in [BTreeMap::from([(id.clone(), 100.0)]), BTreeMap::new()] {
            airplay.set_zones(zones);
            let until = Instant::now() + Duration::from_millis(500);
            while Instant::now() < until {
                airplay.poll();
                thread::sleep(Duration::from_millis(20));
            }
            assert_eq!(receiver.level(), Some(-21.0), "in or out of the zone");
            assert_eq!(row_of(&airplay, &id), 30.0);
        }
        cell(
            "S290.held.m0",
            "TV -21.00 dB at master 0 shows row 30; muted for another speaker's calibration, \
             back at -21.00 dB row 30 after it; zone 100 join and leave keep -21.00 dB row 30",
        );
    }

    // covers: S290
    #[test]
    fn the_tv_keeps_a_level_held_at_zone_0_through_calibrate_all() {
        let (_receiver, _input, _airplay, _id) = a_tv_holding_30_after_calibrate_all(100.0, 0.0);
        cell(
            "S290.held.z0",
            "TV -21.00 dB at master 100 in a zone at 0 shows row 30; muted for another \
             speaker's calibration, back at -21.00 dB row 30 after it",
        );
    }

    // covers: S290
    #[test]
    fn the_tv_lets_a_level_held_at_zone_0_go_once_the_zone_moves() {
        let (receiver, _input, mut airplay, id) = a_tv_holding_30_after_calibrate_all(100.0, 0.0);
        // The own volume is still the default 60, so zone 80 gives D 48.
        airplay.set_zones(BTreeMap::from([(id.clone(), 80.0)]));
        assert!(receiver.reaches(-15.6), "{:?}", receiver.level());
        assert_eq!(row_of(&airplay, &id), 48.0);
        cell(
            "S290.held.z0.release",
            "TV held at -21.00 dB row 30 at master 100 in a zone at 0 goes to -15.60 dB \
             row 48 when the zone moves to 80",
        );
    }

    // covers: S290
    #[test]
    fn the_tv_lets_a_level_held_at_master_0_go_once_the_master_moves() {
        let (receiver, _input, mut airplay, id) = a_tv_holding_30_after_calibrate_all(0.0, 100.0);
        airplay.set_master(80.0);
        assert!(receiver.reaches(-15.6), "{:?}", receiver.level());
        assert_eq!(row_of(&airplay, &id), 48.0);
        cell(
            "S290.held.m0.release",
            "TV held at -21.00 dB row 30 at master 0 in a zone at 100 goes to -15.60 dB \
             row 48 when the master moves to 80",
        );
    }

    /// A duration in whole ms.
    fn ms(duration: Duration) -> u64 {
        (duration.as_secs_f64() * 1000.0).round() as u64
    }

    /// The Lf values among `calls`, in frames.
    fn latencies(calls: &[Call]) -> Vec<u32> {
        calls
            .iter()
            .filter_map(|call| match call {
                Call::Latency(frames) => Some(*frames),
                _ => None,
            })
            .collect()
    }

    /// The delays given among `calls`, in whole ms.
    fn delays(calls: &[Call]) -> Vec<(String, u64)> {
        calls
            .iter()
            .filter_map(|call| match call {
                Call::Delay(id, delay) => Some((id.clone(), ms(*delay))),
                _ => None,
            })
            .collect()
    }

    /// Kitchen and Den listed and on, each with its own delay `delay_ms`,
    /// streaming.
    fn kitchen_and_den(fake: &FakeAirPlay, delay_ms: f64) -> (AirPlaySpeakers, Device, Device) {
        let mut airplay = speakers(fake);
        let kitchen = device(1, "Kitchen", OPEN_TXT);
        let den = device(2, "Den", OPEN_TXT);
        fake.appear(&kitchen);
        fake.appear(&den);
        airplay.poll();
        for speaker in [&kitchen, &den] {
            airplay.set_delay(&id_of(speaker), delay_ms);
            airplay.set_enabled(&id_of(speaker), true).unwrap();
        }
        airplay.set_streaming(true);
        fake.take_calls();
        (airplay, kitchen, den)
    }

    // covers: S280
    #[test]
    fn airplay_latency_is_set_from_known_reports_before_speakers_are_added() {
        let fake = FakeAirPlay::new();
        let (mut airplay, _kitchen, den) = kitchen_and_den(&fake, 0.0);
        fake.report_latency(&den, 1_300_000, LatencySources::AUDIO_LATENCIES);
        airplay.poll();
        assert_eq!(latencies(&fake.take_calls()), [79_380]);
        airplay.set_streaming(false);
        assert_eq!(
            latencies(&fake.take_calls()),
            [77_175],
            "the stream has no speaker"
        );
        airplay.set_streaming(true);
        let calls = fake.take_calls();
        let first_add = calls
            .iter()
            .position(|call| matches!(call, Call::Add { .. }))
            .expect("the speakers join again");
        assert_eq!(
            calls[..first_add],
            [Call::Latency(79_380)],
            "Den's report from this run counts before anything is added: {calls:?}"
        );
        // A receiver discovery lost is forgotten.
        airplay.set_streaming(false);
        fake.gone(&den);
        fake.appear(&den);
        airplay.poll();
        fake.take_calls();
        airplay.set_streaming(true);
        assert!(latencies(&fake.take_calls()).is_empty());
    }

    // covers: S280
    #[test]
    fn airplay_latency_follows_a_report_and_a_speaker_leaving() {
        let fake = FakeAirPlay::new();
        let (mut airplay, kitchen, den) = kitchen_and_den(&fake, 0.0);
        fake.report_latency(&kitchen, 200_000, LatencySources::AUDIO_LATENCY);
        airplay.poll();
        assert!(
            latencies(&fake.take_calls()).is_empty(),
            "200 ms fits in the 1.75 s floor"
        );
        fake.report_latency(&den, 1_300_000, LatencySources::ARRIVAL_TO_RENDER);
        airplay.poll();
        assert_eq!(latencies(&fake.take_calls()), [79_380]);
        airplay.set_enabled(&id_of(&den), false).unwrap();
        assert_eq!(latencies(&fake.take_calls()), [77_175], "Den left");
    }

    // covers: S280
    #[test]
    fn airplay_latency_covers_every_member_of_a_target() {
        let fake = FakeAirPlay::new();
        let (pair, left, right) = stereo_pair();
        let mut airplay = with_target(&fake, &pair, &[&left, &right]);
        airplay.set_streaming(true);
        airplay.set_enabled(PAIR, true).expect("the pair can play");
        fake.take_calls();
        fake.report_latency(&right, 1_600_000, LatencySources::AUDIO_LATENCIES);
        airplay.poll();
        assert_eq!(latencies(&fake.take_calls()), [88_200], "Right counts");
        airplay.set_enabled(PAIR, false).unwrap();
        airplay.set_enabled(PAIR, true).unwrap();
        let calls = fake.take_calls();
        assert_eq!(latencies(&calls), [77_175, 88_200], "{calls:?}");
        assert_eq!(airplay.speakers()[0].reported_latency_ms, 1600.0);
    }

    // covers: S280
    #[test]
    fn a_playing_speaker_that_leaves_discovery_keeps_its_rs_until_it_leaves_the_sender() {
        let fake = FakeAirPlay::new();
        let (mut airplay, kitchen, _den) = kitchen_and_den(&fake, 0.0);
        let id = id_of(&kitchen);
        fake.report_latency(&kitchen, 220_000, LatencySources::AUDIO_LATENCY);
        airplay.poll();
        fake.gone(&kitchen);
        airplay.poll();
        let rows = airplay.speakers();
        let row = rows.iter().find(|row| row.id == id).expect("Kitchen");
        assert_eq!(row.reported_latency_ms, 220.0);
        airplay.set_enabled(&id, false).unwrap();
        assert!(!airplay.reported_us.contains_key(&id));
    }

    // covers: S280
    #[test]
    fn a_speaker_out_of_discovery_that_leaves_the_sender_to_ask_for_a_password_drops_its_rs() {
        let fake = FakeAirPlay::new();
        let (mut airplay, kitchen, _den) = kitchen_and_den(&fake, 0.0);
        let id = id_of(&kitchen);
        airplay.begin_attempt(&id);
        fake.report_latency(&kitchen, 220_000, LatencySources::AUDIO_LATENCY);
        airplay.poll();
        fake.gone(&kitchen);
        airplay.poll();
        assert!(airplay.reported_us.contains_key(&id));
        fake.fail(&kitchen, ErrorKind::PasswordRequired);
        airplay.poll();
        assert!(airplay.password_prompt().is_some(), "it asks");
        assert!(!airplay.reported_us.contains_key(&id));
    }

    // covers: S280
    #[test]
    fn a_pair_member_that_leaves_discovery_keeps_its_rs_until_the_pair_leaves_the_sender() {
        let fake = FakeAirPlay::new();
        let (pair, left, right) = stereo_pair();
        let mut airplay = with_target(&fake, &pair, &[&left, &right]);
        airplay.set_streaming(true);
        airplay.set_enabled(PAIR, true).expect("the pair can play");
        fake.report_latency(&right, 1_600_000, LatencySources::AUDIO_LATENCIES);
        airplay.poll();
        fake.gone(&right);
        airplay.poll();
        let rows = airplay.speakers();
        let row = rows.iter().find(|row| row.id == PAIR).expect("the pair");
        assert_eq!(row.reported_latency_ms, 1600.0);
        airplay.set_enabled(PAIR, false).unwrap();
        assert!(!airplay.reported_us.contains_key(&id_of(&right)));
    }

    // covers: S283
    #[test]
    fn a_reported_latency_is_the_speakers_starting_estimate() {
        let fake = FakeAirPlay::new();
        let (mut airplay, kitchen, den) = kitchen_and_den(&fake, 100.0);
        fake.report_latency(&kitchen, 220_000, LatencySources::AUDIO_LATENCY);
        airplay.poll();
        assert_eq!(ms(airplay.latency()), 1750 + 220, "L_s + E");
        assert_eq!(
            delays(&fake.take_calls()),
            [(id_of(&den), 320)],
            "Kitchen keeps d_s; Den, which reported nothing, gets d_s + E"
        );
        fake.report_latency(&den, 50_000, LatencySources::AUDIO_LATENCIES);
        airplay.poll();
        assert_eq!(delays(&fake.take_calls()), [(id_of(&den), 270)]);
        assert_eq!(ms(airplay.latency()), 1970);
        let rows = airplay.speakers();
        assert!(
            rows.iter().all(|row| row.delay_ms == 100.0),
            "the stored d_s stays: {rows:?}"
        );
        let reported: Vec<f64> = rows.iter().map(|row| row.reported_latency_ms).collect();
        assert_eq!(reported, [50.0, 220.0], "Den, Kitchen");
        assert!(airplay.ahead_ms() <= 0.0);
    }

    // covers: S283, S282
    #[test]
    fn a_receiver_reporting_nothing_starts_at_zero_without_a_warning() {
        let path = crate::debug_log::test_dir("airplay-reports-nothing").join("debug.log");
        let log = DebugLog::new(Some(path.clone()));
        let fake = FakeAirPlay::new();
        let mut airplay =
            AirPlaySpeakers::new(fake.link(), 100.0, log.clone(), PairingStore::in_memory());
        let kitchen = device(1, "Kitchen", OPEN_TXT);
        fake.appear(&kitchen);
        airplay.poll();
        airplay.set_enabled(&id_of(&kitchen), true).unwrap();
        airplay.set_streaming(true);
        fake.take_calls();
        fake.report_latency(&kitchen, 0, LatencySources::NONE);
        assert!(airplay.poll().is_empty(), "no notice");
        assert_eq!(fake.take_calls(), [], "Lf and d′ stay");
        assert_eq!(ms(airplay.latency()), 1750);
        assert_eq!(airplay.speakers()[0].reported_latency_ms, 0.0);
        assert!(log.flush(Duration::from_secs(5)));
        let messages = crate::debug_log::read_messages(&path);
        assert!(
            messages
                .iter()
                .any(|line| line.contains("Kitchen") && line.contains("reports nothing")),
            "{messages:?}"
        );
        assert!(
            !messages.iter().any(|line| line.contains("warning")),
            "{messages:?}"
        );
    }

    // covers: S283
    #[test]
    fn the_estimate_waits_while_an_airplay_speaker_sounds() {
        let fake = FakeAirPlay::new();
        let mut airplay = speakers(&fake);
        let kitchen = device(1, "Kitchen", OPEN_TXT);
        let den = device(2, "Den", OPEN_TXT);
        fake.appear(&kitchen);
        fake.appear(&den);
        airplay.poll();
        airplay.set_enabled(&id_of(&kitchen), true).unwrap();
        airplay.set_streaming(true);
        fake.report_latency(&kitchen, 220_000, LatencySources::AUDIO_LATENCY);
        airplay.poll();
        assert_eq!(ms(airplay.latency()), 1970);
        fake.set_sounding(true);
        airplay.set_enabled(&id_of(&den), true).unwrap();
        let calls = fake.take_calls();
        assert!(
            calls
                .iter()
                .any(|call| matches!(call, Call::Add { id, delay, .. } if *id == id_of(&den) && ms(*delay) == 220)),
            "Den joins at d_s + E: {calls:?}"
        );
        fake.report_latency(&den, 300_000, LatencySources::AUDIO_LATENCY);
        airplay.poll();
        assert_eq!(ms(airplay.latency()), 1970, "E stays 220 ms");
        assert_eq!(
            delays(&fake.take_calls()),
            [(id_of(&den), 0)],
            "Den plays 80 ms late meanwhile"
        );
        airplay.poll();
        assert_eq!(ms(airplay.latency()), 1970);
        assert_eq!(fake.take_calls(), []);
        // A new capture silences no speaker (P143): E stays held.
        airplay.capture_started();
        assert_eq!(ms(airplay.latency()), 1970);
        assert_eq!(fake.take_calls(), []);
    }

    // covers: S283
    #[test]
    fn a_speaker_above_a_held_estimate_keeps_its_own_delay() {
        let fake = FakeAirPlay::new();
        let mut airplay = speakers(&fake);
        let kitchen = device(1, "Kitchen", OPEN_TXT);
        let den = device(2, "Den", OPEN_TXT);
        fake.appear(&kitchen);
        fake.appear(&den);
        airplay.poll();
        airplay.set_delay(&id_of(&den), 100.0);
        airplay.set_enabled(&id_of(&kitchen), true).unwrap();
        airplay.set_streaming(true);
        fake.report_latency(&kitchen, 220_000, LatencySources::AUDIO_LATENCY);
        airplay.poll();
        fake.set_sounding(true);
        airplay.set_enabled(&id_of(&den), true).unwrap();
        let calls = fake.take_calls();
        assert!(
            calls
                .iter()
                .any(|call| matches!(call, Call::Add { id, delay, .. } if *id == id_of(&den) && ms(*delay) == 320)),
            "Den joins at d_s + E: {calls:?}"
        );
        fake.report_latency(&den, 300_000, LatencySources::AUDIO_LATENCY);
        airplay.poll();
        assert_eq!(
            delays(&fake.take_calls()),
            [(id_of(&den), 100)],
            "Den keeps d_s and plays 80 ms late, not d_s - 80"
        );
    }

    /// The target delays given among `calls`, in whole ms.
    fn target_delays(calls: &[Call]) -> Vec<(String, u64)> {
        calls
            .iter()
            .filter_map(|call| match call {
                Call::TargetDelay(id, delay) => Some((id.clone(), ms(*delay))),
                _ => None,
            })
            .collect()
    }

    /// The stereo pair and a lone AirPlay 2 speaker, Den, on and streaming,
    /// with Left at 220 ms, Right at 50 ms and Den at 100 ms reported.
    fn pair_and_den(fake: &FakeAirPlay) -> (AirPlaySpeakers, Target, Device, Device) {
        let (pair, left, right) = stereo_pair();
        let den = airplay2_device(23, "Den", false);
        let mut airplay = with_target(fake, &pair, &[&left, &right]);
        fake.appear(&den);
        airplay.poll();
        airplay.set_streaming(true);
        airplay.set_enabled(PAIR, true).expect("the pair can play");
        airplay.set_enabled(&id_of(&den), true).unwrap();
        fake.report_latency(&left, 220_000, LatencySources::AUDIO_LATENCY);
        fake.report_latency(&right, 50_000, LatencySources::AUDIO_LATENCY);
        fake.report_latency(&den, 100_000, LatencySources::AUDIO_LATENCY);
        airplay.poll();
        (airplay, pair, right, den)
    }

    /// AirPlay 2 receivers listed, on and streaming, each with d_s 0.
    fn airplay2_streaming(fake: &FakeAirPlay, names: &[&str]) -> (AirPlaySpeakers, Vec<Device>) {
        fake.set_airplay2(true);
        let mut airplay = speakers(fake);
        let devices: Vec<Device> = names
            .iter()
            .zip(1u8..)
            .map(|(name, id)| airplay2_device(id, name, false))
            .collect();
        for device in &devices {
            fake.appear(device);
        }
        airplay.poll();
        for device in &devices {
            airplay.set_delay(&id_of(device), 0.0);
            airplay.set_enabled(&id_of(device), true).unwrap();
        }
        airplay.set_streaming(true);
        (airplay, devices)
    }

    // covers: S284
    #[test]
    fn several_airplay2_receivers_share_lf_and_e_and_a_target_uses_its_largest_member() {
        let fake = FakeAirPlay::new();
        let (pair, left, right) = stereo_pair();
        let mut airplay = with_target(&fake, &pair, &[&left, &right]);
        let den = airplay2_device(23, "Den", false);
        fake.appear(&den);
        airplay.poll();
        airplay.set_delay(PAIR, 0.0);
        airplay.set_delay(&id_of(&den), 0.0);
        airplay.set_streaming(true);
        airplay.set_enabled(PAIR, true).unwrap();
        airplay.set_enabled(&id_of(&den), true).unwrap();
        fake.take_calls();
        fake.report_latency(&left, 220_000, LatencySources::AUDIO_LATENCY);
        fake.report_latency(&right, 50_000, LatencySources::AUDIO_LATENCY);
        fake.report_latency(&den, 1_400_000, LatencySources::AUDIO_LATENCY);
        airplay.poll();
        let calls = fake.take_calls();
        assert_eq!(latencies(&calls), [83_790], "one Lf over all three");
        assert_eq!(ms(airplay.latency()), 1750 + 1400, "one E: Den's");
        assert_eq!(
            target_delays(&calls),
            [(PAIR.to_owned(), 1180)],
            "the pair's e_s is Left's 220 ms: {calls:?}"
        );
        assert_eq!(
            delays(&calls),
            [(id_of(&den), 220), (id_of(&den), 0)],
            "no member gets a delay of its own: {calls:?}"
        );
        let rows = airplay.speakers();
        let pair_row = rows.iter().find(|row| row.id == PAIR).expect("the pair");
        assert_eq!(pair_row.reported_latency_ms, 220.0);
    }

    // covers: S284
    #[test]
    fn several_airplay_2_receivers_share_lf_and_e_and_a_target_uses_its_largest_member() {
        let fake = FakeAirPlay::new();
        let (mut airplay, _pair, right, den) = pair_and_den(&fake);
        assert_eq!(ms(airplay.latency()), 1750 + 220, "one E over all three");
        assert_eq!(
            ms(airplay.sent_delay(PAIR)),
            0,
            "the pair's e_s is Left's 220"
        );
        assert_eq!(ms(airplay.sent_delay(&id_of(&den))), 120);
        let calls = fake.take_calls();
        assert!(
            !delays(&calls).iter().any(|(id, _)| *id == id_of(&right)),
            "the members share the pair's one delay: {calls:?}"
        );
        fake.report_latency(&right, 1_300_000, LatencySources::AUDIO_LATENCY);
        airplay.poll();
        assert_eq!(
            latencies(&fake.take_calls()),
            [79_380],
            "a member's report moves the one Lf"
        );
    }

    // covers: S284
    #[test]
    fn a_target_member_leaving_while_others_sound_changes_no_delay() {
        let fake = FakeAirPlay::new();
        let (mut airplay, pair, right, den) = pair_and_den(&fake);
        fake.take_calls();
        fake.set_sounding(true);
        let mut right_only = pair.clone();
        right_only.members = vec![member_of(&right)];
        fake.target(&right_only);
        airplay.poll();
        let calls = fake.take_calls();
        assert_eq!(delays(&calls), [], "{calls:?}");
        assert_eq!(target_delays(&calls), [], "{calls:?}");
        assert_eq!(ms(airplay.latency()), 1970, "E stays at Left's 220 ms");
        assert_eq!(ms(airplay.sent_delay(PAIR)), 0);
        airplay.set_enabled(&id_of(&den), false).unwrap();
        airplay.poll();
        let calls = fake.take_calls();
        assert_eq!(target_delays(&calls), [], "{calls:?}");
        assert_eq!(ms(airplay.latency()), 1970);
        // Once the stream is empty the next values come: only Right's 50 ms is left.
        fake.set_sounding(false);
        airplay.poll();
        assert_eq!(ms(airplay.latency()), 1800);
        assert_eq!(ms(airplay.sent_delay(PAIR)), 0);
    }

    // covers: S284
    #[test]
    fn an_airplay2_receiver_leaving_while_others_sound_changes_no_delay() {
        let fake = FakeAirPlay::new();
        let (mut airplay, devices) = airplay2_streaming(&fake, &["A1", "B2", "C3"]);
        let [a1, b2, c3] = [&devices[0], &devices[1], &devices[2]];
        fake.report_latency(a1, 220_000, LatencySources::AUDIO_LATENCY);
        fake.report_latency(b2, 50_000, LatencySources::AUDIO_LATENCY);
        fake.report_latency(c3, 1_400_000, LatencySources::AUDIO_LATENCY);
        airplay.poll();
        assert_eq!(latencies(&fake.take_calls()), [83_790]);
        assert_eq!(ms(airplay.latency()), 3150);
        fake.set_sounding(true);
        airplay.set_enabled(&id_of(c3), false).unwrap();
        airplay.poll();
        let calls = fake.take_calls();
        assert!(
            !calls
                .iter()
                .any(|call| matches!(call, Call::Pause | Call::Delay(..))),
            "nothing flushed or re-timed, every d′_s held: {calls:?}"
        );
        assert_eq!(
            latencies(&calls),
            [77_175],
            "the host sends the new Lf (S280); the sender holds it while a speaker sounds (P201)"
        );
        assert_eq!(ms(airplay.latency()), 3150, "E held at 1400 ms");
        airplay.poll();
        assert_eq!(fake.take_calls(), []);
        // A new capture silences no speaker (P143): E stays held.
        airplay.capture_started();
        assert_eq!(ms(airplay.latency()), 3150);
        assert_eq!(fake.take_calls(), []);
    }

    // covers: S285
    #[test]
    fn every_airplay2_speaker_switched_on_goes_to_the_sender_and_only_one_it_refuses_waits_switched_on_and_on_its_own_row()
     {
        let fake = FakeAirPlay::new();
        let names: Vec<String> = (1..=17).map(|n| format!("M{n}")).collect();
        let names: Vec<&str> = names.iter().map(String::as_str).collect();
        let (mut airplay, devices) = airplay2_streaming(&fake, &names);
        let calls = fake.take_calls();
        let added = calls
            .iter()
            .filter(|call| matches!(call, Call::Add { .. }))
            .count();
        assert_eq!(added, 17, "the host holds none back: {calls:?}");
        let (last, playing) = devices.split_last().expect("17 speakers");
        for device in playing {
            fake.state(device, SpeakerState::Playing);
        }
        fake.fail(
            last,
            ErrorKind::Unsupported(UnsupportedReason::TooManyAirPlay2Sessions),
        );
        let events = airplay.poll();
        let troubles: Vec<&BackendEvent> = events
            .iter()
            .filter(|event| {
                !matches!(
                    event,
                    BackendEvent::AirPlayChanged | BackendEvent::AirPlayPlaying { .. }
                )
            })
            .collect();
        assert_eq!(
            troubles,
            [&BackendEvent::AirPlayTrouble {
                id: id_of(last),
                problem: AirPlayProblem::TooManyAirPlay2Speakers
            }],
            "{events:?}"
        );
        assert_eq!(airplay.enabled_ids().len(), 17, "the 17th stays on");
        assert_eq!(fake.take_calls(), [], "nobody leaves or restarts");
        let playing_ids: Vec<String> = playing.iter().map(id_of).collect();
        let rows = airplay.speakers();
        assert!(
            rows.iter()
                .all(|row| row.playing == playing_ids.contains(&row.id)),
            "{rows:?}"
        );
    }

    // covers: S285
    #[test]
    fn one_airplay2_speaker_failing_leaves_the_others_playing() {
        let fake = FakeAirPlay::new();
        let (mut airplay, devices) = airplay2_streaming(&fake, &["A1", "B2", "C3", "D4"]);
        let [a1, b2, c3, d4] = [&devices[0], &devices[1], &devices[2], &devices[3]];
        for device in [b2, c3] {
            airplay.begin_attempt(&id_of(device));
        }
        fake.state(a1, SpeakerState::Playing);
        airplay.poll();
        fake.set_sounding(true);
        fake.take_calls();
        fake.fail(b2, ErrorKind::PasswordRequired);
        fake.fail(c3, ErrorKind::PairingRequired);
        fake.lose(d4, ErrorKind::Unreachable);
        let mut events = airplay.poll();
        assert_eq!(
            airplay.password_prompt().map(|prompt| prompt.id),
            Some(id_of(b2))
        );
        assert!(airplay.submit_password(&id_of(b2), "hunter1"));
        fake.fail(b2, ErrorKind::WrongPassword);
        events.extend(airplay.poll());
        assert_eq!(
            airplay
                .password_prompt()
                .map(|prompt| (prompt.id, prompt.wrong)),
            Some((id_of(b2), true))
        );
        let a1_id = id_of(a1);
        let calls = fake.take_calls();
        assert!(
            !calls.iter().any(|call| match call {
                Call::Pause | Call::Latency(_) => true,
                Call::Remove(id) | Call::Delay(id, _) => *id == a1_id,
                _ => false,
            }),
            "the stream and A1 play on: {calls:?}"
        );
        assert!(
            calls.contains(&Call::StartPairing(id_of(c3))),
            "C3 pairs on its own: {calls:?}"
        );
        let rows_named: Vec<&String> = events
            .iter()
            .filter_map(|event| match event {
                BackendEvent::AirPlayTrouble { id, .. }
                | BackendEvent::AirPlayFailed { id, .. } => Some(id),
                _ => None,
            })
            .collect();
        assert!(
            !rows_named.contains(&&a1_id) && rows_named.contains(&&id_of(d4)),
            "each failure on its own row: {events:?}"
        );
        let rows = airplay.speakers();
        let a1_row = rows.iter().find(|row| row.id == a1_id).expect("A1");
        assert!(a1_row.playing && a1_row.enabled, "{rows:?}");
    }
}
