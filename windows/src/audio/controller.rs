//! The backend logic, the same on every platform: sessions, the output
//! policy, and what happens when devices come and go. The platform calls go
//! through a [`Driver`]: WASAPI on Windows, an in-memory mock in tests.
//!
//! A session owns one ring at [`SESSION_RATE`], one capture writing to it
//! through the equalizer, and one renderer per playing output, each reading
//! the ring through its own delay reader. Each session gets a new equalizer:
//! the capture thread owns its processing half, the session its control half,
//! which takes new settings live. Outputs are added and removed by the core's
//! output sync (spec S117, S161), keyed by endpoint ID. An output whose
//! device goes away stops, whether its stream notices or the device list no
//! longer shows the device, and stays on; it plays again once the device is
//! listed (S68). One whose stream ended while the device was still listed,
//! and that can't start again, is lost once the list drops the device. An
//! output that can't start is turned off and reported (S69), except one
//! whose device another app holds in exclusive mode: that one stays on, as
//! the user chose, is reported once as busy, and is tried again every few
//! seconds until it plays.
//!
//! A capture lost because Windows audio restarted, or the device it captures
//! through went away, ends its session and starts it again about a second
//! later, with streaming on all along, trying once a second for about five
//! seconds while Windows audio comes back; if it can't start by then, or is
//! lost again within a few seconds, streaming stops and the loss is reported.
//! A capture whose app quit, or that failed otherwise, ends streaming at once.
//!
//! Everything the controller does and reports goes to the debug log
//! ([`DebugLog`]), from the thread that calls it, never from an audio
//! thread; while a session runs, [`AudioBackend::poll`] adds a status line
//! every few seconds from the counters the audio threads keep.
//!
//! While a session runs, [`AudioBackend::poll`] also checks every few
//! seconds whether the source is heard twice, straight from an output as
//! well as through AudioSync ([`routing`](super::routing)), from a read-only
//! snapshot of the endpoints and the processes, and reports a change as
//! [`BackendEvent::HeardTwice`].
//!
//! Local outputs play at the hold-backs of the local delay model (S172 to
//! S178, see [`outputs`](super::outputs)): offsets relative to the reference
//! output, with the AirPlay latency L on top while an AirPlay speaker is
//! chosen. Whenever an output joins or leaves, an offset or the reference
//! changes, or the AirPlay condition or L changes, every playing output
//! whose hold-back changed is re-timed (S177).
//!
//! With an AirPlay sender ([`Controller::set_airplay`]), the capture also
//! feeds the sender and AirPlay speakers that are on join the stream while a
//! session runs ([`airplay`](super::airplay)). L is the sender's base
//! latency L_s plus the user's adjustment (S178). A new capture restarts the
//! sender's timeline (P143); a new L_s from the sender re-times every
//! playing output.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use audiosync_core::eq::{self, EqControl, EqProcessor, EqSettings};
use audiosync_core::policy::delay::native_airplay_latency_ms;
use audiosync_core::policy::source::AudioSource;
use audiosync_core::policy::source_quit::{
    SourceQuitAction, SourceQuitWatch, Streaming, WhenSourceQuits,
};
use audiosync_core::reader::{Reader, ReaderCounters};
use audiosync_core::render::{RendererControl, STOP_FADE_CAP};
use audiosync_core::ring::{Ring, RingWriter};
use audiosync_core::volume::SharedGain;

use super::SESSION_RATE;
use super::airplay::{AirPlayLink, AirPlaySpeakers, FeedSlot, SenderStarter, StartedSender};
use super::backend::{
    AirPlayInfo, AirPlayPasswordPrompt, AirPlayPinPrompt, AirPlayProblem, AudioBackend,
    BackendError, BackendEvent, CastDiscovery, CastInfo, CastProblem, OutputInfo, OutputKind,
    SourceId, SourceInfo, StopReason, TIMED_OUT,
};
use super::calibration::{CalibrationTap, Engine, Microphone, MicrophoneError};
use super::capture::{
    CaptureAttempt, CaptureTarget, Feeds, LoopbackMode, SampleType, decide_capture, display_name,
    input_sources, running_app, source_list,
};
use super::cast::{CastLink, CastSpeakers, CastTrack};
use super::cast_address::CastAddress;
use super::now_playing::{Follow, SharedLabels, TrackInfo, TrackReader, TrackSource};
#[cfg(test)]
use super::outputs::OutputParams;
use super::outputs::{
    AirPlay, Endpoint, EndpointReading, Listing, ListingNote, OutputPolicy, StreamSetup,
};
use super::pairings::PairingStore;
use super::process::ProcessTable;
use super::routing::{
    self, Captured, DirectPath, DirectReason, EndpointAudio, EndpointVolume, PlayedOn,
};
use crate::debug_log::DebugLog;

mod device_volume;

#[cfg(test)]
mod log_lines_tests;

/// How often the debug log gets a status line while a session runs.
const STATUS_INTERVAL: Duration = Duration::from_secs(5);

/// How often a running session checks whether the source is heard twice
/// ([`routing`](super::routing)).
const ROUTING_INTERVAL: Duration = Duration::from_secs(3);

/// A capture lost because Windows audio restarted, or its device went away
/// ([`BackendError::DeviceGone`]), starts again at this poll after the one
/// that noticed: about a second later at the UI's two polls a second, so
/// that the audio service is back.
const RESTART_DELAY_POLLS: u32 = 2;

/// A capture lost that way is tried this many times, [`RESTART_DELAY_POLLS`]
/// apart (about five seconds in all), before streaming stops: Windows audio
/// can take a few seconds to come back.
const RESTART_TRIES: u32 = 5;

/// An output whose device another app holds ([`BackendError::DeviceBusy`])
/// is tried again this many polls after its last try: about every three
/// seconds at the UI's two polls a second. Each try starts an output thread
/// and opens the device while the UI thread waits, too much for every poll,
/// and the user hears it within three seconds once the device is free.
const BUSY_RETRY_POLLS: u32 = 6;

/// An output whose device didn't answer in time while it started
/// ([`BackendError::NotResponding`]) is tried again this many polls later:
/// about every ten seconds at the UI's two polls a second. Each such try
/// holds the UI thread for up to five seconds, so most of the time the
/// window stays responsive while Windows audio comes back.
const STALL_RETRY_POLLS: u32 = 20;

/// An output whose device Windows still lists but refuses to open as if it
/// were gone ([`BackendError::DeviceGone`]: `AUDCLNT_E_ENDPOINT_CREATE_FAILED`
/// while it rebuilds the endpoint after a wake, say) is tried at every poll
/// for this many polls, about thirty seconds at the UI's two polls a second;
/// still refused then, it counts as a start that failed (S69).
const REFUSED_GIVE_UP_POLLS: u32 = 60;

/// A capture that started again counts as working once it has run this many
/// polls (about five seconds); a loss before then means the restart failed,
/// and streaming stops. After it, the next loss is restarted again.
const RESTART_TRIAL_POLLS: u32 = 10;

/// How a running capture is doing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CaptureState {
    /// It captures.
    Running,
    /// The app it captures quit.
    SourceQuit,
    /// It failed and stopped.
    Failed(BackendError),
}

/// A running capture. Dropping it stops the capture and waits for its thread.
pub trait CaptureHandle {
    /// How the capture is doing.
    fn state(&self) -> CaptureState;

    /// The format Windows delivers the capture in, for the debug log, when
    /// known.
    fn setup(&self) -> Option<CaptureAttempt>;
}

/// A running output. Dropping it stops the output at once and waits for its
/// thread.
pub trait RendererHandle {
    /// Asks the output to fade out and stop once its device has played the
    /// fade (S185), without waiting for it, so that several outputs can fade
    /// out together before they are dropped.
    fn request_stop(&mut self);

    /// Whether it stopped by itself, because its device went away or failed.
    fn has_ended(&self) -> bool;

    /// Whether its device has played the fade-out a stop asked for, or it
    /// can play nothing more because its stream ended (S185).
    fn is_faded(&self) -> bool;

    /// How its stream is set up, for the debug log, when known.
    fn setup(&self) -> Option<StreamSetup>;
}

/// The platform calls the backend makes.
pub trait Driver {
    /// A running capture.
    type Capture: CaptureHandle;
    /// A running output.
    type Renderer: RendererHandle;

    /// AudioSync's own process ID.
    fn own_pid(&self) -> u32;

    /// A snapshot of the running processes.
    fn processes(&mut self) -> Result<ProcessTable, BackendError>;

    /// The IDs of the processes that own an audio session on a connected
    /// output.
    fn session_pids(&mut self) -> Result<Vec<u32>, BackendError>;

    /// The connected render endpoints, and notes for the log on the ones
    /// left out or listed without their properties.
    fn outputs(&mut self) -> Result<Listing, BackendError>;

    /// Whether outputs may have changed since the last call.
    fn outputs_changed(&mut self) -> bool;

    /// A snapshot of the active render endpoints for the double-audio check
    /// ([`routing`](super::routing)): each one's name, whether it is the
    /// default output, its mute and master volume, and the processes with an
    /// active audio session on it. It only reads: no mute, volume or default
    /// output changes.
    fn endpoint_audio(&mut self) -> Result<Vec<EndpointAudio>, BackendError>;

    /// Starts following the endpoint `id`'s own mute and master volume
    /// (spec S195): a change notification from Windows marks it for
    /// [`Driver::endpoint_volume_changes`]. The notification runs on a system
    /// thread and only marks; nothing is read there.
    /// [`BackendError::NotResponding`] when Windows audio didn't answer in
    /// time: the endpoint isn't followed then.
    fn watch_endpoint_volume(&mut self, id: &str) -> Result<(), BackendError>;

    /// Stops following the endpoint `id`'s volume. Does nothing when it isn't
    /// followed.
    fn unwatch_endpoint_volume(&mut self, id: &str);

    /// The endpoints followed whose mute or volume Windows reported changed
    /// since the last call, each once however many notifications came.
    fn endpoint_volume_changes(&mut self) -> Vec<String>;

    /// The endpoint `id`'s mute and master volume, read now
    /// (`IAudioEndpointVolume::GetMute` and `GetMasterVolumeLevelScalar`):
    /// an error when its volume control can't be activated or read (S194),
    /// and [`BackendError::NotResponding`] when Windows audio didn't answer
    /// in time.
    fn endpoint_volume(&mut self, id: &str) -> Result<EndpointVolume, BackendError>;

    /// Sets the endpoint `id`'s master volume, 0 to 1
    /// (`SetMasterVolumeLevelScalar`): only the fix the user clicks (S197).
    /// [`BackendError::NotResponding`] when Windows audio didn't answer in
    /// time.
    fn set_endpoint_level(&mut self, id: &str, level: f32) -> Result<(), BackendError>;

    /// Unmutes the endpoint `id` (`SetMute(false)`): only the fix the user
    /// clicks (S197). [`BackendError::NotResponding`] when Windows audio
    /// didn't answer in time.
    fn unmute_endpoint(&mut self, id: &str) -> Result<(), BackendError>;

    /// The active capture endpoints (input devices), as Windows describes
    /// them; [`input_sources`] decides which to offer.
    fn inputs(&mut self) -> Result<Vec<EndpointReading>, BackendError>;

    /// Starts capturing `target`, writing to the ring through
    /// `equalizer` and `writer`, and handing the same audio to the AirPlay and
    /// Google Cast senders' feeds in `feeds`. Returns once the capture runs, or
    /// with why it couldn't start. Each feed goes back to its slot when the
    /// capture stops.
    fn start_capture(
        &mut self,
        target: CaptureTarget,
        writer: RingWriter,
        equalizer: EqProcessor,
        feeds: Feeds,
    ) -> Result<Self::Capture, BackendError>;

    /// Starts rendering to `endpoint` through `reader`, controlled through
    /// `control` (made for that reader), at the gain in `gain`. Returns once
    /// it plays, or with why it couldn't start: [`BackendError::DeviceGone`]
    /// when the device went away.
    fn start_renderer(
        &mut self,
        endpoint: &Endpoint,
        reader: Reader,
        control: RendererControl,
        gain: Arc<SharedGain>,
    ) -> Result<Self::Renderer, BackendError>;

    /// Starts recording the default input device, mono at its own rate,
    /// with room for `seconds`.
    fn open_microphone(&mut self, _seconds: f64) -> Result<Box<dyn Microphone>, MicrophoneError> {
        Err(MicrophoneError::Missing)
    }
}

/// Output `id`'s gain: 0 while a calibration lets only `solo` be heard.
fn solo_gain(solo: Option<&str>, id: &str, gain: f32) -> f32 {
    match solo {
        Some(solo) if solo != id => 0.0,
        _ => gain,
    }
}

/// A running session.
struct Session<D: Driver> {
    source: SourceId,
    /// What the capture takes in.
    target: CaptureTarget,
    ring: Arc<Ring>,
    capture: D::Capture,
    /// The control half of the capture's equalizer.
    equalizer: EqControl,
    /// The playing outputs, by endpoint ID.
    outputs: BTreeMap<String, Playing<D::Renderer>>,
    /// Polls left in the trial of a capture that started again after a loss
    /// ([`RESTART_TRIAL_POLLS`]); 0 for one that runs as it should.
    trial_polls: u32,
    /// The senders' feeds the capture started without because an earlier
    /// capture, let go while Windows audio hung, still held them: the
    /// session starts again once that one gives one back.
    waits_for_feed: MissingFeeds,
}

/// Which of the senders' feeds a capture started without.
#[derive(Clone, Copy)]
struct MissingFeeds {
    airplay: bool,
    cast: bool,
}

/// A playing output and the handles its delay and gain are changed through.
struct Playing<R> {
    /// Its delay, faded out and in when it changes (S187), and its reader's
    /// lag and counters.
    control: RendererControl,
    gain: Arc<SharedGain>,
    handle: R,
}

/// Outputs asked to fade out, waiting for their devices to play the fade
/// before they are dropped (S185, S186): those of a session that ended, with
/// its capture and ring, which they still read until then, or those switched
/// off during a session. Nothing waits for them on the UI thread: each poll
/// drops what has faded, and what hasn't after [`STOP_FADE_CAP`].
struct Retiring<D: Driver> {
    outputs: Vec<Playing<D::Renderer>>,
    /// The ended session's capture and ring.
    session: Option<(D::Capture, Arc<Ring>)>,
    /// When the fade-outs were asked for.
    since: Instant,
}

/// The double-audio check of the running session.
#[derive(Default)]
struct RoutingState {
    /// When the check last ran; `None` until the session's first check.
    last_check: Option<Instant>,
    /// The endpoints the source played on at the last check.
    played_on: Vec<PlayedOn>,
    /// What was last reported: empty while the source is heard only through
    /// AudioSync.
    reported: Vec<DirectPath>,
    /// The last error of the check: an error that repeats is logged once.
    error: Option<BackendError>,
}

/// The audio backend over a driver. See the module documentation.
pub struct Controller<D: Driver> {
    driver: D,
    policy: OutputPolicy,
    /// The connected endpoints, sorted by ID, as last listed successfully.
    present: Vec<Endpoint>,
    /// Whether the last listing worked: false before the first one and after
    /// a failed one, so the next refresh lists again.
    listing_ok: bool,
    session: Option<Session<D>>,
    /// The equalizer settings, for the running session and the next.
    equalizer: EqSettings,
    /// Events for the next poll.
    pending: Vec<BackendEvent>,
    /// The debug log; it writes nothing unless `AUDIOSYNC_DEBUG=1`.
    log: DebugLog,
    /// The last name of every output listed, so the log names outputs
    /// that are gone.
    names: BTreeMap<String, String>,
    /// How each output's stream was set up the last time it played.
    setups: BTreeMap<String, StreamSetup>,
    /// The last error listing the outputs, and listing the sources: an
    /// error that repeats at every poll is logged once.
    listing_error: Option<BackendError>,
    sources_error: Option<BackendError>,
    /// Why the input devices couldn't be listed last time, if they couldn't:
    /// the log tells once per failure, and the list goes on without them.
    inputs_error: Option<BackendError>,
    /// The notes of the last listing: notes that repeat at every listing
    /// are logged once.
    listing_notes: Vec<ListingNote>,
    /// When the log last got a status line, and the frames captured then.
    last_status: Option<(Instant, u64)>,
    /// The double-audio check of the running session.
    routing: RoutingState,
    /// The AirPlay side, once there is a sender.
    airplay: Option<AirPlaySpeakers>,
    /// The sender's feed, which each capture borrows.
    feed: Option<FeedSlot>,
    /// The Google Cast side, once its host runs; it holds its own feed.
    cast: Option<CastSpeakers>,
    /// Starts a new sender when the user retries after the sender stopped
    /// or couldn't start.
    starter: Option<SenderStarter>,
    /// The paired Apple TVs' credentials while no sender ever started: the
    /// first one that does gets them.
    waiting_store: Option<PairingStore>,
    /// Outputs fading out before they stop.
    retiring: Vec<Retiring<D>>,
    /// A start that waits for the last session's outputs to fade out: the
    /// first poll after they have faded starts it (S186).
    next_start: Option<SourceId>,
    /// Polls left before `next_start` restarts a capture that was lost
    /// ([`RESTART_DELAY_POLLS`]); `None` when it isn't a restart.
    restart_wait: Option<u32>,
    /// The tries a restart has left after the one `restart_wait` waits for
    /// ([`RESTART_TRIES`]).
    restart_tries_left: u32,
    /// The own volume and mute of each output that is on and connected
    /// (spec S192 to S202), followed through Windows' notifications.
    device_volumes: device_volume::Followed,
    /// The outputs reported busy ([`BackendEvent::OutputBusy`]) that
    /// haven't played since or been switched off, each with the polls left
    /// before its next try ([`BUSY_RETRY_POLLS`]).
    busy: BTreeMap<String, u32>,
    /// The outputs on that don't play while their device is still listed:
    /// starting them failed as if the device were gone, or didn't answer in
    /// time, or waited behind a start that didn't (their stream may have
    /// just ended). Once the listing drops one,
    /// it is lost ([`BackendEvent::OutputLost`]), as an output that played
    /// is.
    unplayable: BTreeSet<String>,
    /// The outputs whose device didn't answer in time at their last start,
    /// each with the polls left before its next try ([`STALL_RETRY_POLLS`]).
    stalled: BTreeMap<String, u32>,
    /// The stalled outputs whose own start didn't answer in time, not just
    /// one waiting behind another's ([`Controller::sync_outputs`]): they
    /// start last in a pass, so the others aren't kept waiting behind them.
    hung: BTreeSet<String>,
    /// The outputs on whose listed device refuses to open as if it were
    /// gone, each with the polls left before that counts as a start that
    /// failed ([`REFUSED_GIVE_UP_POLLS`]).
    refused: BTreeMap<String, u32>,
    /// What AirPlay receivers with a screen show of the playing track.
    track_info: TrackInfo,
    /// The source track info last followed, as it looks to track info.
    track_source: Option<(SourceId, TrackSource)>,
    /// Whether a quit of the captured app stops the session or moves it to
    /// the whole system (spec S227).
    quit_watch: SourceQuitWatch,
    /// The microphone calibration's way into every capture.
    calibration_tap: Arc<CalibrationTap>,
    /// The one local output a calibration lets be heard.
    calibration_solo: Option<String>,
    /// Sessions started so far, captures started again included: a
    /// calibration tells by it that its session went on.
    sessions_started: u64,
}

impl<D: Driver> Controller<D> {
    /// A stopped backend over `driver`, with no output set, the equalizer
    /// off and the debug log off.
    pub fn with_driver(driver: D) -> Controller<D> {
        Controller {
            driver,
            policy: OutputPolicy::new(AirPlay::Unavailable),
            present: Vec::new(),
            listing_ok: false,
            session: None,
            equalizer: EqSettings::default(),
            pending: Vec::new(),
            log: DebugLog::default(),
            names: BTreeMap::new(),
            setups: BTreeMap::new(),
            listing_error: None,
            sources_error: None,
            inputs_error: None,
            listing_notes: Vec::new(),
            last_status: None,
            routing: RoutingState::default(),
            airplay: None,
            feed: None,
            cast: None,
            starter: None,
            waiting_store: None,
            retiring: Vec::new(),
            next_start: None,
            restart_wait: None,
            restart_tries_left: 0,
            quit_watch: SourceQuitWatch::default(),
            device_volumes: device_volume::Followed::default(),
            busy: BTreeMap::new(),
            unplayable: BTreeSet::new(),
            stalled: BTreeMap::new(),
            hung: BTreeSet::new(),
            refused: BTreeMap::new(),
            track_info: TrackInfo::default(),
            track_source: None,
            calibration_tap: Arc::new(CalibrationTap::new()),
            calibration_solo: None,
            sessions_started: 0,
        }
    }

    /// Reads the playing track through `reader` for the AirPlay receivers
    /// that show it, while they play.
    pub fn set_track_reader(&mut self, reader: Box<dyn TrackReader>) {
        self.track_info.set_reader(reader);
    }

    /// What receivers show while no track counts, as the UI words it.
    pub fn set_receiver_labels(&mut self, labels: SharedLabels) {
        self.track_info.set_labels(labels);
    }

    /// Starts a new AirPlay sender through `starter` when the user retries
    /// after the one running stopped.
    #[cfg(test)]
    pub fn set_airplay_starter(&mut self, starter: SenderStarter) {
        self.starter = Some(starter);
    }

    /// Starts the AirPlay sender through `starter`, with the paired Apple
    /// TVs' credentials in `store`, and keeps `starter` to start a new one
    /// when the user retries. A sender that can't start, or whose discovery
    /// can't, is reported as [`BackendEvent::AirPlayStopped`], whose Retry
    /// tries again.
    pub fn start_airplay(&mut self, mut starter: SenderStarter, store: PairingStore) {
        let started = starter();
        self.starter = Some(starter);
        self.waiting_store = Some(store);
        match started {
            Some(started) => self.use_sender(started),
            None => {
                self.log
                    .log(|| "AirPlay: no sender runs; AirPlay stays off until Retry".to_owned());
                self.pending.push(BackendEvent::AirPlayStopped);
            }
        }
    }

    /// Plays to AirPlay through `started`: the first sender, or one that
    /// replaces a sender that stopped. One whose discovery couldn't start
    /// lists nothing, so it counts as stopped and is reported.
    fn use_sender(&mut self, started: StartedSender) {
        let StartedSender {
            link,
            feed,
            browsing,
        } = started;
        match self.airplay.as_mut() {
            Some(airplay) => {
                airplay.replace_link(link);
                self.feed = feed;
            }
            None => {
                let store = self
                    .waiting_store
                    .take()
                    .unwrap_or_else(PairingStore::in_memory);
                self.set_airplay_with_pairings(link, feed, store);
            }
        }
        if !browsing && let Some(airplay) = self.airplay.as_mut() {
            airplay.discovery_unavailable(&mut self.pending);
        }
    }

    /// Sends what the backend does to `log`.
    pub fn set_debug_log(&mut self, log: DebugLog) {
        if let Some(airplay) = self.airplay.as_mut() {
            airplay.set_debug_log(log.clone());
        }
        if let Some(cast) = self.cast.as_mut() {
            cast.set_debug_log(log.clone());
        }
        self.track_info.set_debug_log(log.clone());
        self.log = log;
    }

    /// Plays to AirPlay speakers through `link`, fed by the capture through
    /// `feed`: local outputs follow the AirPlay sync rule from now on. Apple
    /// TV pairings are kept for this run only.
    #[cfg_attr(not(test), expect(dead_code, reason = "the app keeps its pairings"))]
    pub fn set_airplay(&mut self, link: Box<dyn AirPlayLink>, feed: Option<FeedSlot>) {
        self.set_airplay_with_pairings(link, feed, PairingStore::in_memory());
    }

    /// [`Controller::set_airplay`], with the paired Apple TVs' credentials
    /// in `store`.
    pub fn set_airplay_with_pairings(
        &mut self,
        link: Box<dyn AirPlayLink>,
        feed: Option<FeedSlot>,
        store: PairingStore,
    ) {
        let airplay = AirPlaySpeakers::new(link, self.policy.master(), self.log.clone(), store);
        self.log.log(|| {
            format!(
                "AirPlay: the sender runs, L = {} ms; local outputs line up with each other \
                 until an AirPlay speaker is chosen",
                airplay.latency().as_millis()
            )
        });
        self.policy.set_airplay(AirPlay::Available {
            chosen: false,
            latency: airplay.latency(),
        });
        self.airplay = Some(airplay);
        self.feed = feed;
        self.hold_back_network();
        if self.session.is_some() {
            self.airplay_streaming(true);
        }
    }

    /// Plays to Google Cast speakers through `link`, fed by the capture.
    pub fn set_cast(&mut self, link: Box<dyn CastLink>) {
        let mut cast = CastSpeakers::new(link, self.log.clone());
        cast.set_streaming(self.session.is_some());
        self.cast = Some(cast);
        self.sync_cast_rule();
    }

    /// AirPlay and Google Cast speakers that are on join the stream, or
    /// leave it.
    fn airplay_streaming(&mut self, streaming: bool) {
        if let Some(airplay) = self.airplay.as_mut() {
            airplay.set_streaming(streaming);
        }
        if let Some(cast) = self.cast.as_mut() {
            cast.set_streaming(streaming);
        }
        self.sync_airplay_rule();
        self.sync_cast_rule();
    }

    /// Holds AirPlay or Google Cast back, live, so that with both on they
    /// play at the longer of L and the Cast offset, as local outputs do (C77).
    fn hold_back_network(&mut self) {
        let airplay = self.policy.airplay_hold_back_ms();
        let cast = self.policy.cast_hold_back_ms();
        if let Some(speakers) = self.airplay.as_mut() {
            speakers.set_hold_back(airplay);
        }
        if let Some(speakers) = self.cast.as_mut() {
            speakers.set_hold_back(cast);
        }
    }

    /// Re-times the other outputs, live, when a Google Cast speaker is
    /// switched on or off or its offset changes: they wait the larger of L
    /// and that offset.
    fn sync_cast_rule(&mut self) {
        let anchor = self.cast.as_ref().and_then(CastSpeakers::anchor);
        let changed = self.policy.set_cast_offset(anchor);
        self.hold_back_network();
        if !changed {
            return;
        }
        self.log.log(|| match anchor {
            Some(offset) => format!(
                "the other outputs wait for the Google Cast speaker: its offset is {} ms",
                offset.as_millis()
            ),
            None => "no Google Cast speaker is on: the outputs no longer wait for one".to_owned(),
        });
        self.retime();
    }

    /// Re-times local outputs, live, when whether an AirPlay speaker is
    /// chosen, or L_s while one is, changed (S177, S178).
    fn sync_airplay_rule(&mut self) {
        let Some(airplay) = &self.airplay else {
            return;
        };
        let rule = AirPlay::Available {
            chosen: airplay.chosen(),
            latency: airplay.latency(),
        };
        let changed = self.policy.set_airplay(rule);
        self.hold_back_network();
        if !changed {
            return;
        }
        self.log.log(|| match rule {
            AirPlay::Available {
                chosen: true,
                latency,
            } => format!(
                "local outputs play at L + their offset (L = {} ms: the sender's {} ms and the \
                 adjustment {} ms): an AirPlay speaker is chosen",
                native_airplay_latency_ms(
                    latency.as_secs_f64() * 1000.0,
                    self.policy.airplay_adjustment_ms()
                ),
                latency.as_millis(),
                self.policy.airplay_adjustment_ms()
            ),
            _ => "local outputs line up with each other: no AirPlay speaker is chosen".to_owned(),
        });
        self.retime();
    }

    /// Gives the senders what their receivers should show of the playing
    /// track, while an AirPlay or Cast speaker plays the capture.
    fn sync_track_info(&mut self) {
        let sending = self.session.is_some()
            && (self
                .airplay
                .as_ref()
                .is_some_and(|airplay| airplay.chosen())
                || self.cast.as_ref().is_some_and(CastSpeakers::chosen));
        if self.session.is_none() {
            self.track_source = None;
        }
        let source = if sending { self.track_source() } else { None };
        let now = Instant::now();
        let now_playing = self.track_info.sync(source.as_ref(), now);
        if let Some(cast) = self.cast.as_mut() {
            let track = if self.track_info.reads() {
                self.track_info.cast_track(now)
            } else {
                Some(CastTrack::default())
            };
            cast.set_track(track);
        }
        if let (Some(now_playing), Some(airplay)) = (now_playing, self.airplay.as_mut()) {
            airplay.set_now_playing(now_playing);
        }
    }

    /// The session's source as track info sees it: which media sessions
    /// count, and the name the source list shows. An app or input device is
    /// looked up once per session; when the process snapshot fails, no media
    /// session counts until a later poll finds the app.
    fn track_source(&mut self) -> Option<TrackSource> {
        let id = self.session.as_ref()?.source.clone();
        if let Some((known, source)) = &self.track_source
            && *known == id
        {
            return Some(source.clone());
        }
        let source = match &id {
            SourceId::WholeSystem => TrackSource {
                follow: Follow::AnyApp,
                name: None,
            },
            SourceId::App { root_pid } => {
                let Ok(processes) = self.driver.processes() else {
                    return Some(TrackSource {
                        follow: Follow::App {
                            executable: String::new(),
                        },
                        name: None,
                    });
                };
                let executable = processes
                    .get(*root_pid)
                    .map(|entry| entry.executable.clone())
                    .unwrap_or_default();
                TrackSource {
                    name: (!executable.is_empty()).then(|| display_name(&executable)),
                    follow: Follow::App { executable },
                }
            }
            // An input device has no media session; receivers get the
            // labels with its name, or without one when it isn't listed.
            SourceId::Input { endpoint_id } => {
                let unnamed = TrackSource {
                    follow: Follow::App {
                        executable: String::new(),
                    },
                    name: None,
                };
                // A failed listing isn't kept, so a later poll can still name it.
                let Ok(readings) = self.driver.inputs() else {
                    return Some(unnamed);
                };
                TrackSource {
                    name: input_sources(readings)
                        .into_iter()
                        .find(|input| {
                            matches!(&input.id, SourceId::Input { endpoint_id: listed } if listed == endpoint_id)
                        })
                        .map(|input| input.name),
                    ..unnamed
                }
            }
        };
        self.track_source = Some((id, source.clone()));
        Some(source)
    }

    /// The driver.
    #[cfg(test)]
    pub fn driver(&self) -> &D {
        &self.driver
    }

    /// The driver, to change.
    #[cfg(test)]
    pub fn driver_mut(&mut self) -> &mut D {
        &mut self.driver
    }

    /// The output settings.
    #[cfg(test)]
    pub fn policy(&self) -> &OutputPolicy {
        &self.policy
    }

    /// The IDs of the playing outputs, sorted.
    pub fn playing_ids(&self) -> Vec<String> {
        self.session
            .as_ref()
            .map(|session| session.outputs.keys().cloned().collect())
            .unwrap_or_default()
    }

    /// The delay and gain a playing output applies right now.
    #[cfg(test)]
    pub fn playing_params(&self, id: &str) -> Option<OutputParams> {
        let playing = self.session.as_ref()?.outputs.get(id)?;
        Some(OutputParams {
            delay_frames: playing.control.delay_frames(),
            gain: playing.gain.get(),
        })
    }

    /// Frames captured since the start: 0 until the source plays something.
    #[cfg(test)]
    pub fn frames_captured(&self) -> u64 {
        self.session
            .as_ref()
            .map_or(0, |session| session.ring.frames_written())
    }

    fn is_playing(&self, id: &str) -> bool {
        self.session
            .as_ref()
            .is_some_and(|session| session.outputs.contains_key(id))
    }

    /// Lists the connected endpoints again. Returns whether the list changed,
    /// and the error when listing failed. A failed listing keeps the last
    /// list, and the next refresh lists again.
    fn refresh_outputs(&mut self) -> (bool, Result<(), BackendError>) {
        let Listing {
            endpoints: mut listed,
            notes,
        } = match self.driver.outputs() {
            Ok(listing) => listing,
            Err(error) => {
                self.listing_ok = false;
                if self.listing_error.as_ref() != Some(&error) {
                    self.log
                        .log(|| format!("listing the outputs failed: {error}"));
                    self.listing_error = Some(error.clone());
                }
                return (false, Err(error));
            }
        };
        listed.sort_by(|a, b| a.id.cmp(&b.id));
        listed.dedup_by(|a, b| a.id == b.id);
        let changed = listed != self.present;
        // The first listing, or the first since one failed, shows every
        // output as it is; later ones mark what changed.
        let previous = self.listing_ok.then_some(self.present.as_slice());
        self.log.log(|| match previous {
            Some(_) if !changed => format!("outputs listed again, no change ({})", listed.len()),
            _ => describe_listing(previous, &listed, &self.setups),
        });
        // An endpoint left out, or listed without its properties, is
        // logged with the full listing and whenever the notes change.
        if previous.is_none() || notes != self.listing_notes {
            for note in &notes {
                self.log.log(|| note.to_string());
            }
        }
        self.listing_notes = notes;
        for endpoint in &listed {
            self.names
                .insert(endpoint.id.clone(), endpoint.name.clone());
        }
        self.listing_error = None;
        self.present = listed;
        self.policy.set_listed(&self.present);
        self.listing_ok = true;
        // A busy output whose device is gone no longer waits for it: it
        // stays on and is tried again once it is listed (S68).
        let present = &self.present;
        let pending = &mut self.pending;
        self.busy.retain(|id, _| {
            let listed = present.iter().any(|endpoint| endpoint.id == *id);
            if !listed {
                pending.push(BackendEvent::OutputFree { id: id.clone() });
            }
            listed
        });
        // A device that came back is read afresh; one gone is let go.
        self.follow_device_volumes();
        (changed, Ok(()))
    }

    /// Lists the endpoints again when the driver says they may have changed,
    /// when `force` is set, or when the last listing failed or never ran, and
    /// queues [`BackendEvent::OutputsChanged`] when the list differs.
    fn refresh_if_needed(&mut self, force: bool) {
        let notified = self.driver.outputs_changed();
        if notified {
            self.log
                .log(|| "Windows reported a change of the outputs".to_owned());
        }
        if force || notified || !self.listing_ok {
            let (changed, listed) = self.refresh_outputs();
            if changed && !self.pending.contains(&BackendEvent::OutputsChanged) {
                self.pending.push(BackendEvent::OutputsChanged);
            }
            // A device unplugged and plugged in again (or whose stream
            // ended) may be listed as before, without its volume listener.
            if (force || notified) && listed.is_ok() {
                self.renew_device_volumes();
            }
        }
    }

    /// Adds and removes renderers so the playing outputs are the enabled,
    /// connected ones. Returns the outputs that couldn't start; they are
    /// turned off (S69). One whose device is gone stays on and is tried
    /// again at the next poll, for about thirty seconds while its device is
    /// still listed ([`REFUSED_GIVE_UP_POLLS`]); one whose device another
    /// app holds stays on and is tried again every few seconds
    /// ([`BUSY_RETRY_POLLS`]). Once a start doesn't answer in time, which
    /// held the UI thread for the whole wait, the starts left in the pass
    /// wait as that one does ([`STALL_RETRY_POLLS`]) without a try, so a
    /// pass holds the window that long once at most; the outputs whose own
    /// start hung go last in the next pass.
    fn sync_outputs(&mut self) -> Vec<(String, BackendError)> {
        let Some(session) = self.session.as_mut() else {
            return Vec::new();
        };
        let active: Vec<&str> = session.outputs.keys().map(String::as_str).collect();
        let plan = self.policy.plan(true, &self.present, &active);

        // A switched-off output fades out first, together with any others
        // switched off now; one whose device isn't listed has nothing to
        // fade (S185, S186).
        let mut switched_off = Vec::new();
        for id in &plan.detach {
            let Some(mut playing) = session.outputs.remove(id) else {
                continue;
            };
            let why = if self.policy.settings(id).enabled {
                drop(playing);
                "its device isn't listed"
            } else {
                playing.handle.request_stop();
                switched_off.push(playing);
                "switched off"
            };
            self.log
                .log(|| format!("output {} stopped: {why}", label(&self.names, id)));
        }
        if !switched_off.is_empty() {
            self.retiring.push(Retiring {
                outputs: switched_off,
                session: None,
                since: Instant::now(),
            });
        }

        let mut failures = Vec::new();
        // The outputs whose own start hung last time go last: one that
        // waited behind them doesn't wait behind them again.
        let mut attach = plan.attach;
        attach.sort_by_key(|endpoint| self.hung.contains(&endpoint.id));
        // Whether a start in this pass didn't answer in time.
        let mut held = false;
        for endpoint in attach {
            // A busy or stalled output waits for its next try.
            let waits = |tries: &BTreeMap<String, u32>| {
                tries.get(&endpoint.id).is_some_and(|&wait| wait > 0)
            };
            if waits(&self.busy) || waits(&self.stalled) {
                continue;
            }
            // A start before it hung, which most likely means Windows audio
            // isn't answering at all: it waits as that one does instead of
            // holding the window as long again.
            if held {
                // It waited behind a hang this time, so next time it goes
                // before the one that hung, whatever happened before.
                self.hung.remove(&endpoint.id);
                self.unplayable.insert(endpoint.id.clone());
                self.stalled.insert(endpoint.id.clone(), STALL_RETRY_POLLS);
                self.log.log(|| {
                    format!(
                        "output {} isn't started yet: a start before it didn't answer in time; \
                         it stays on and is tried again in about ten seconds",
                        label(&self.names, &endpoint.id)
                    )
                });
                continue;
            }
            let params = self.policy.params(&endpoint.id);
            let reader = Reader::new(Arc::clone(&session.ring), SESSION_RATE);
            reader.control().set_delay_frames(params.delay_frames);
            let control = RendererControl::new(&reader);
            control.set_bluetooth(endpoint.kind == OutputKind::Bluetooth);
            let gain = Arc::new(SharedGain::new(solo_gain(
                self.calibration_solo.as_deref(),
                &endpoint.id,
                params.gain,
            )));
            let failed = match self.driver.start_renderer(
                &endpoint,
                reader,
                control.clone(),
                Arc::clone(&gain),
            ) {
                Ok(handle) => {
                    self.unplayable.remove(&endpoint.id);
                    self.stalled.remove(&endpoint.id);
                    self.hung.remove(&endpoint.id);
                    self.refused.remove(&endpoint.id);
                    if self.busy.remove(&endpoint.id).is_some() {
                        self.log.log(|| {
                            format!(
                                "output {}: the other app let go of its device",
                                label(&self.names, &endpoint.id)
                            )
                        });
                        self.pending.push(BackendEvent::OutputFree {
                            id: endpoint.id.clone(),
                        });
                    }
                    let setup = handle.setup();
                    if let Some(setup) = setup {
                        self.setups.insert(endpoint.id.clone(), setup);
                    }
                    self.log.log(|| {
                        describe_output_start(
                            &label(&self.names, &endpoint.id),
                            setup,
                            self.policy.hold_back_ms(&endpoint.id),
                            self.policy.settings(&endpoint.id).volume,
                            params.gain,
                            control.extra_lead_frames(),
                        )
                    });
                    session.outputs.insert(
                        endpoint.id.clone(),
                        Playing {
                            control,
                            gain,
                            handle,
                        },
                    );
                    None
                }
                // Listed a moment ago and gone now, or rebuilt by Windows
                // (after a wake, or a change of its format): it stays on and
                // plays once it can (S68), and is lost once the listing drops
                // it. Told once; the next tries, at every poll, are quiet.
                // Still listed and refused about thirty seconds on, it is a
                // start that failed (S69).
                Err(BackendError::DeviceGone) => {
                    let left = *self
                        .refused
                        .entry(endpoint.id.clone())
                        .or_insert(REFUSED_GIVE_UP_POLLS);
                    if left == 0 {
                        self.log.log(|| {
                            format!(
                                "output {} is still listed, but its device has refused to open \
                                 for about thirty seconds",
                                label(&self.names, &endpoint.id)
                            )
                        });
                        Some(BackendError::DeviceGone)
                    } else {
                        if self.unplayable.insert(endpoint.id.clone()) {
                            self.log.log(|| {
                                format!(
                                    "output {} went away while starting; it stays on",
                                    label(&self.names, &endpoint.id)
                                )
                            });
                        }
                        None
                    }
                }
                // Windows audio didn't answer in time (after a wake, say): it
                // stays on, and is tried again a few seconds later, since
                // each try may hold the UI thread that long.
                Err(BackendError::NotResponding) => {
                    held = true;
                    self.unplayable.insert(endpoint.id.clone());
                    self.stalled.insert(endpoint.id.clone(), STALL_RETRY_POLLS);
                    self.hung.insert(endpoint.id.clone());
                    self.log.log(|| {
                        format!(
                            "output {} didn't answer in time while starting; it stays on and is \
                             tried again in about ten seconds",
                            label(&self.names, &endpoint.id)
                        )
                    });
                    None
                }
                // Another app holds the device in exclusive mode: it stays
                // on, as the user chose, and plays once that app lets go.
                // Told once; the next tries, every few seconds, are quiet.
                Err(BackendError::DeviceBusy) => {
                    let first = self
                        .busy
                        .insert(endpoint.id.clone(), BUSY_RETRY_POLLS)
                        .is_none();
                    if first {
                        self.log.log(|| {
                            format!(
                                "output {} is busy: another app holds its device in exclusive \
                                 mode; it stays on and is tried again every few seconds",
                                label(&self.names, &endpoint.id)
                            )
                        });
                        self.pending.push(BackendEvent::OutputBusy {
                            id: endpoint.id.clone(),
                        });
                    }
                    None
                }
                Err(error) => Some(error),
            };
            if let Some(error) = failed {
                self.log.log(|| {
                    format!(
                        "output {} could not start: {error} It is turned off.",
                        label(&self.names, &endpoint.id)
                    )
                });
                self.policy.set_enabled(&endpoint.id, false);
                self.unplayable.remove(&endpoint.id);
                self.stalled.remove(&endpoint.id);
                self.hung.remove(&endpoint.id);
                self.refused.remove(&endpoint.id);
                // Off, it no longer waits for its device, so a later
                // busy device is told again.
                if self.busy.remove(&endpoint.id).is_some() {
                    self.pending.push(BackendEvent::OutputFree {
                        id: endpoint.id.clone(),
                    });
                }
                failures.push((endpoint.id, error));
            }
        }
        failures
    }

    /// Stops the playing outputs whose device the last successful listing
    /// doesn't show, and returns their IDs. A stream may not notice that its
    /// device went away (Windows can stop signalling it without a word), but
    /// the device list does (S68). There is nothing to fade (S185).
    fn stop_unlisted_outputs(&mut self) -> Vec<String> {
        let Some(session) = self.session.as_mut() else {
            return Vec::new();
        };
        let present = &self.present;
        let unlisted: Vec<String> = session
            .outputs
            .keys()
            .filter(|id| !present.iter().any(|endpoint| endpoint.id == **id))
            .cloned()
            .collect();
        let stopped: Vec<Playing<D::Renderer>> = unlisted
            .iter()
            .filter_map(|id| session.outputs.remove(id))
            .collect();
        drop(stopped);
        for id in &unlisted {
            self.log.log(|| {
                format!(
                    "output {} stopped: its device is no longer listed",
                    label(&self.names, id)
                )
            });
        }
        unlisted
    }

    /// Forgets the outputs that couldn't play while their device was listed
    /// ([`Controller::unplayable`]) whose device the last successful listing
    /// no longer shows, or that were switched off, and returns the IDs of
    /// the first: they are lost now.
    fn forget_unlisted_unplayable(&mut self) -> Vec<String> {
        let present = &self.present;
        let policy = &self.policy;
        let mut gone = Vec::new();
        self.unplayable.retain(|id| {
            let listed = present.iter().any(|endpoint| endpoint.id == *id);
            if !listed && policy.settings(id).enabled {
                gone.push(id.clone());
            }
            listed && policy.settings(id).enabled
        });
        // Listed again later, a device's refusals count afresh.
        let unplayable = &self.unplayable;
        self.refused.retain(|id, _| unplayable.contains(id));
        for id in &gone {
            self.log.log(|| {
                format!(
                    "output {}, which couldn't play, is no longer listed",
                    label(&self.names, id)
                )
            });
        }
        gone
    }

    fn report_failures(&mut self, failures: Vec<(String, BackendError)>) {
        self.pending.extend(
            failures
                .into_iter()
                .map(|(id, error)| BackendEvent::OutputFailed { id, error }),
        );
    }

    /// Ends the session (S186): every output and the AirPlay sender's input
    /// fade out at once while the capture runs on, and once the outputs'
    /// devices have played their fades (or [`STOP_FADE_CAP`] passed), the
    /// outputs stop, then the capture. The UI thread doesn't wait: a poll, a
    /// new start or the drop finishes it.
    fn end_session(&mut self) {
        let Some(session) = self.session.take() else {
            return;
        };
        let Session {
            outputs,
            capture,
            ring,
            ..
        } = session;
        let mut outputs: Vec<Playing<D::Renderer>> = outputs.into_values().collect();
        for playing in &mut outputs {
            playing.handle.request_stop();
        }
        if let Some(feed) = &self.feed {
            feed.request_fade_out();
        }
        self.retiring.push(Retiring {
            outputs,
            session: Some((capture, ring)),
            since: Instant::now(),
        });
        self.last_status = None;
        self.reap(false);
        // Without a session, nothing is heard twice.
        let routing = std::mem::take(&mut self.routing);
        if !routing.reported.is_empty() {
            self.log
                .log(|| "double audio: none, streaming stopped".to_owned());
            self.pending
                .push(BackendEvent::HeardTwice { paths: Vec::new() });
        }
    }

    /// Stops the retiring outputs whose devices have played their fade-outs,
    /// or that are still fading after [`STOP_FADE_CAP`], and then the capture
    /// and ring of an ended session (S186). With `wait`, returns only once
    /// none is left, which takes at most that cap.
    fn reap(&mut self, wait: bool) {
        while !self.retiring.is_empty() {
            let now = Instant::now();
            let mut index = 0;
            while index < self.retiring.len() {
                let entry = &self.retiring[index];
                let faded = entry
                    .outputs
                    .iter()
                    .all(|playing| playing.handle.is_faded() || playing.handle.has_ended());
                let waited = now.saturating_duration_since(entry.since);
                if faded || waited >= STOP_FADE_CAP {
                    let entry = self.retiring.remove(index);
                    self.retire(entry, faded, waited);
                } else {
                    index += 1;
                }
            }
            if !wait || self.retiring.is_empty() {
                return;
            }
            thread::sleep(Duration::from_millis(2));
        }
    }

    /// Drops what `entry` holds: an ended session's capture first, then the
    /// outputs (S115, S186), then the ring. The capture and each renderer
    /// wait for their threads before the ring they use is freed (S134); a
    /// thread let go because Windows audio doesn't answer holds its share
    /// of the ring until it ends.
    fn retire(&mut self, entry: Retiring<D>, faded: bool, waited: Duration) {
        let Retiring {
            outputs, session, ..
        } = entry;
        let stopped = outputs.len();
        let (capture, ring) = session.unzip();
        drop(capture);
        drop(outputs);
        if !faded {
            self.log.log(|| {
                format!(
                    "{stopped} output(s) still fading after {} ms: stopped anyway",
                    waited.as_millis()
                )
            });
        }
        if let Some(ring) = ring {
            let captured = ring.frames_written();
            drop(ring);
            self.log.log(|| {
                format!(
                    "session ended: the capture and {stopped} output(s) stopped in {} ms; \
                     {captured} frames captured",
                    waited.as_millis()
                )
            });
        }
    }

    /// Logs how the session is doing, at most every [`STATUS_INTERVAL`]:
    /// the frames captured, and each playing output's delay, volume and
    /// reader counters, all read from the atomics the audio threads keep.
    fn log_status_if_due(&mut self, now: Instant) {
        if !self.log.enabled() {
            return;
        }
        let Some(session) = &self.session else {
            return;
        };
        let frames = session.ring.frames_written();
        let Some(span) = status_due(&mut self.last_status, now, frames) else {
            return;
        };
        self.log.log(|| {
            let outputs: Vec<OutputStatus> = session
                .outputs
                .iter()
                .map(|(id, playing)| OutputStatus {
                    label: label(&self.names, id),
                    hold_back_ms: self.policy.hold_back_ms(id),
                    delay_frames: playing.control.delay_frames(),
                    volume: self.policy.settings(id).volume,
                    gain: playing.gain.get(),
                    lag_frames: playing.control.lag_frames(),
                    counters: playing.control.counters(),
                    slowed_frames: playing.control.slowed_frames(),
                    sped_frames: playing.control.sped_frames(),
                    shortfall_frames: playing.control.shortfall_frames(),
                    shortage_frames: playing.control.shortage_frames(),
                    least_slack_ns: playing.control.least_slack_ns(),
                })
                .collect();
            let airplay = self.airplay.as_ref().and_then(AirPlaySpeakers::status);
            describe_status(&session.source, span, &outputs, airplay.as_deref())
        });
    }

    /// Gives a playing output its current hold-back; one that didn't change
    /// doesn't re-anchor its reader. Returns whether it changed.
    fn apply_delay(&self, id: &str) -> bool {
        let Some(playing) = self.session.as_ref().and_then(|s| s.outputs.get(id)) else {
            return false;
        };
        let frames = self.policy.params(id).delay_frames;
        if playing.control.delay_frames() == frames {
            return false;
        }
        playing.control.set_delay_frames(frames);
        true
    }

    /// Gives every playing output its current hold-back, after the outputs
    /// taking part, an offset, the reference, the AirPlay condition or L
    /// changed (S177): an increase inserts that much silence, a decrease
    /// skips forward, and an output whose hold-back is unchanged gets no
    /// call.
    fn retime(&self) {
        for id in self.playing_ids() {
            if self.apply_delay(&id) {
                self.log.log(|| {
                    format!(
                        "output {} delay now {} ms ({} frames)",
                        label(&self.names, &id),
                        self.policy.hold_back_ms(&id),
                        self.policy.params(&id).delay_frames
                    )
                });
            }
        }
    }

    /// Gives a playing output its current gain.
    fn apply_gain(&self, id: &str) {
        if let Some(playing) = self.session.as_ref().and_then(|s| s.outputs.get(id)) {
            playing.gain.set(self.gain_of(id));
        }
    }

    fn gain_of(&self, id: &str) -> f32 {
        solo_gain(
            self.calibration_solo.as_deref(),
            id,
            self.policy.params(id).gain,
        )
    }

    /// Starts a session capturing `source`, or, while the last session's
    /// outputs still fade out (its capture holds the AirPlay feed and its
    /// outputs their endpoints), leaves it to the first poll after they have
    /// faded, at most [`STOP_FADE_CAP`] later (S186), so the UI thread never
    /// waits for a fade. A source that can't be captured is refused at once
    /// either way.
    fn begin_session(&mut self, source: &SourceId) -> Result<(), BackendError> {
        self.next_start = None;
        self.restart_wait = None;
        self.reap(false);
        if !self.last_session_fading() {
            return self.start_session(source);
        }
        let processes = match source {
            SourceId::WholeSystem | SourceId::Input { .. } => ProcessTable::default(),
            SourceId::App { .. } => self.driver.processes()?,
        };
        decide_capture(source, self.driver.own_pid(), &processes)?;
        self.log
            .log(|| "the capture starts once the last session's outputs have faded out".to_owned());
        self.next_start = Some(source.clone());
        Ok(())
    }

    /// Starts the session again once a sender's feed (AirPlay or Google
    /// Cast), which an earlier capture let go while Windows audio hung still
    /// held when it started, is back in its slot: the capture can't take a
    /// feed while it runs, as for Retry. Streaming stays on meanwhile.
    fn take_back_feed(&mut self) {
        let Some((source, missing)) = self
            .session
            .as_ref()
            .map(|session| (session.source.clone(), session.waits_for_feed))
        else {
            return;
        };
        let airplay_back = missing.airplay && self.feed.as_ref().is_some_and(FeedSlot::is_free);
        let cast_back = missing.cast && self.cast.as_ref().is_some_and(CastSpeakers::is_free);
        if !airplay_back && !cast_back {
            return;
        }
        let sender = if airplay_back {
            "AirPlay"
        } else {
            "Google Cast"
        };
        self.log.log(|| {
            format!(
                "the {sender} feed is back from the capture that hung: the capture starts again \
                 to feed the speakers"
            )
        });
        self.end_session();
        if let Err(error) = self.begin_session(&source) {
            self.log
                .log(|| format!("the capture couldn't start again with the feed: {error}"));
            self.airplay_streaming(false);
            self.release_busy();
            self.pending.push(BackendEvent::Stopped {
                reason: StopReason::CaptureFailed(error),
            });
        }
    }

    /// Ends the session whose capture was lost because Windows audio
    /// restarted or its device went away, and starts it again
    /// [`RESTART_DELAY_POLLS`] polls later (`start_when_faded`), up to
    /// [`RESTART_TRIES`] times. Streaming stays on meanwhile, the AirPlay
    /// speakers in the stream included, as for a change of source.
    fn restart_capture(&mut self) {
        let Some(source) = self.session.as_ref().map(|session| session.source.clone()) else {
            return;
        };
        self.log.log(|| {
            "the capture was lost: Windows audio restarted or its device went away; it starts \
             again in a moment"
                .to_owned()
        });
        self.end_session();
        self.next_start = Some(source);
        self.restart_wait = Some(RESTART_DELAY_POLLS);
        self.restart_tries_left = RESTART_TRIES - 1;
    }

    /// Ends the wait of every output reported busy, once streaming stopped:
    /// nothing plays to them now. A later session tells again of one whose
    /// device is still held. The outputs that couldn't play are forgotten
    /// too: a device that goes away while nothing streams isn't lost. So is
    /// the wait of one that didn't answer in time: a new session tries it
    /// at once.
    fn release_busy(&mut self) {
        self.unplayable.clear();
        self.stalled.clear();
        self.hung.clear();
        self.refused.clear();
        for id in std::mem::take(&mut self.busy).into_keys() {
            self.pending.push(BackendEvent::OutputFree { id });
        }
    }

    /// Whether an ended session's outputs are still fading out.
    fn last_session_fading(&self) -> bool {
        self.retiring.iter().any(|entry| entry.session.is_some())
    }

    /// Starts the session that waits for the last one's outputs to fade, once
    /// they have, and for a restart its delay too; a capture that can't start
    /// then ends it with that reason, unless it is a restart with tries left
    /// that may work later ([`may_start_later`]): that one waits again.
    fn start_when_faded(&mut self) {
        if let Some(wait) = self.restart_wait.as_mut() {
            *wait = wait.saturating_sub(1);
            if *wait > 0 {
                return;
            }
        }
        if self.last_session_fading() {
            return;
        }
        let Some(source) = self.next_start.take() else {
            return;
        };
        let restart = self.restart_wait.take().is_some();
        match self.start_rebuilt(&source) {
            Ok(()) => {
                if restart {
                    self.log.log(|| "the capture runs again".to_owned());
                    if let Some(session) = self.session.as_mut() {
                        session.trial_polls = RESTART_TRIAL_POLLS;
                    }
                }
                self.follow_device_volumes();
            }
            Err(error) if restart && self.restart_tries_left > 0 && may_start_later(&error) => {
                self.restart_tries_left -= 1;
                self.log.log(|| {
                    format!(
                        "the capture couldn't start again yet: {error} It tries again in a moment."
                    )
                });
                self.next_start = Some(source);
                self.restart_wait = Some(RESTART_DELAY_POLLS);
            }
            Err(error) => {
                self.log.log(|| {
                    if restart {
                        format!("the capture couldn't start again: {error}")
                    } else {
                        format!("the start failed: {error}")
                    }
                });
                self.airplay_streaming(false);
                self.release_busy();
                self.pending.push(BackendEvent::Stopped {
                    reason: StopReason::CaptureFailed(error),
                });
            }
        }
    }

    /// Starts the session that waited for the last one to fade, or a capture
    /// lost a moment ago. Its app quitting meanwhile counts as a quit while
    /// streaming (spec S227 e): set to switch, the whole system starts instead.
    fn start_rebuilt(&mut self, source: &SourceId) -> Result<(), BackendError> {
        match self.start_session(source) {
            Err(BackendError::SourceGone)
                if self.decide_quit(source, Streaming::Rebuilding)
                    == SourceQuitAction::SwitchToSystem =>
            {
                self.log.log(|| {
                    "the captured app quit before its capture started: capturing the whole \
                     system instead, as set"
                        .to_owned()
                });
                self.start_session(&SourceId::WholeSystem)?;
                self.pending.push(BackendEvent::SwitchedToSystem);
                Ok(())
            }
            result => result,
        }
    }

    /// What the setting makes of the quit of `source`'s app (spec S227). The
    /// capture itself reports that its app ended, so the source's own
    /// process ID names the app that quit.
    fn decide_quit(&mut self, source: &SourceId, streaming: Streaming) -> SourceQuitAction {
        let SourceId::App { root_pid } = source else {
            return SourceQuitAction::Keep;
        };
        let app = root_pid.to_string();
        let source = AudioSource::App {
            bundle_id: app.clone(),
        };
        self.quit_watch.app_exited(&source, Some(&app), streaming)
    }

    /// Ends the session whose app quit and goes on with the whole system, as
    /// a change of source does: the outputs fade over and the AirPlay
    /// speakers stay in the stream (spec S186, S227).
    fn switch_to_system(&mut self) {
        self.log
            .log(|| "the captured app quit: capturing the whole system instead, as set".to_owned());
        self.end_session();
        let started = self.begin_session(&SourceId::WholeSystem);
        self.follow_device_volumes();
        match started {
            Ok(()) => self.pending.push(BackendEvent::SwitchedToSystem),
            Err(error) => {
                self.log
                    .log(|| format!("the whole system couldn't be captured: {error}"));
                self.airplay_streaming(false);
                self.release_busy();
                self.pending.push(BackendEvent::Stopped {
                    reason: StopReason::CaptureFailed(error),
                });
            }
        }
    }

    /// Starts a session capturing `source` now, after the running one ended.
    fn start_session(&mut self, source: &SourceId) -> Result<(), BackendError> {
        let processes = match source {
            SourceId::WholeSystem | SourceId::Input { .. } => ProcessTable::default(),
            SourceId::App { .. } => self.driver.processes()?,
        };
        let target = decide_capture(source, self.driver.own_pid(), &processes)?;
        let ring = Ring::for_rate(SESSION_RATE).map_err(|_| BackendError::UnsupportedFormat)?;
        let writer = ring.writer().ok_or(BackendError::UnsupportedFormat)?;
        // The capture equalizes at the session rate, after any resampling
        // (a positive rate is always accepted).
        let (mut equalizer, processor) = eq::equalizer();
        equalizer.set_sample_rate(f64::from(SESSION_RATE));
        equalizer.set_settings(self.equalizer);
        let feed = self.feed.as_ref().and_then(FeedSlot::lease);
        let feeds_airplay = feed.is_some();
        let cast = self.cast.as_ref().and_then(CastSpeakers::lease);
        let waits_for_feed = MissingFeeds {
            airplay: self.feed.is_some() && !feeds_airplay,
            cast: self.cast.is_some() && cast.is_none(),
        };
        let feeds = Feeds {
            airplay: feed,
            cast,
            calibration: Some(Arc::clone(&self.calibration_tap)),
        };
        let capture = self
            .driver
            .start_capture(target.clone(), writer, processor, feeds)?;
        self.log
            .log(|| describe_capture_start(&target, capture.setup(), feeds_airplay));
        for (missing, sender) in [
            (waits_for_feed.airplay, "AirPlay"),
            (waits_for_feed.cast, "Google Cast"),
        ] {
            if missing {
                self.log.log(|| {
                    format!(
                        "the {sender} feed is still held by an earlier capture that hung: the \
                         capture starts again once it's back"
                    )
                });
            }
        }
        if let Some(airplay) = self.airplay.as_mut() {
            airplay.restart_timeline();
        }
        self.sessions_started += 1;
        self.session = Some(Session {
            source: source.clone(),
            target,
            ring,
            capture,
            equalizer,
            outputs: BTreeMap::new(),
            trial_polls: 0,
            waits_for_feed,
        });
        self.refresh_if_needed(false);
        let failures = self.sync_outputs();
        self.report_failures(failures);
        // An output that couldn't start was turned off: the others no
        // longer wait for it (S177).
        self.retime();
        self.airplay_streaming(true);
        Ok(())
    }

    /// Checks whether the source is heard twice, at the first poll of a
    /// session and then every [`ROUTING_INTERVAL`].
    fn check_routing_if_due(&mut self, now: Instant) {
        if self.session.is_none() {
            return;
        }
        let due = self
            .routing
            .last_check
            .is_none_or(|last| now.saturating_duration_since(last) >= ROUTING_INTERVAL);
        if due {
            self.check_routing(now);
        }
    }

    /// Checks whether the source is heard twice now, whenever the last check
    /// ran; the next poll reports a change.
    #[cfg(test)]
    pub fn check_routing_now(&mut self) {
        if self.session.is_some() {
            self.check_routing(Instant::now());
        }
    }

    /// Checks whether the source of the running session is heard twice, and
    /// queues [`BackendEvent::HeardTwice`] when the answer changed. A
    /// snapshot that fails keeps the last answer.
    fn check_routing(&mut self, now: Instant) {
        let Some(session) = &self.session else {
            return;
        };
        // An input device is no render stream Windows could play straight.
        let &CaptureTarget::Loopback(activation) = &session.target else {
            return;
        };
        self.routing.last_check = Some(now);
        let rendered: Vec<String> = session.outputs.keys().cloned().collect();
        let snapshot = self
            .driver
            .endpoint_audio()
            .and_then(|endpoints| Ok((endpoints, self.driver.processes()?)));
        let (mut endpoints, processes) = match snapshot {
            Ok(snapshot) => snapshot,
            Err(error) => {
                if self.routing.error.as_ref() != Some(&error) {
                    self.log
                        .log(|| format!("the double-audio check failed: {error}"));
                    self.routing.error = Some(error);
                }
                return;
            }
        };
        if self.routing.error.take().is_some() {
            self.log
                .log(|| "the double-audio check works again".to_owned());
        }
        for endpoint in &mut endpoints {
            if endpoint.name.is_empty() {
                endpoint.name = self
                    .names
                    .get(&endpoint.id)
                    .cloned()
                    .unwrap_or_else(|| endpoint.id.clone());
            }
        }
        let tree = processes.tree(activation.target_pid);
        let captured = match activation.mode {
            LoopbackMode::IncludeTree => Captured::Only(tree),
            LoopbackMode::ExcludeTree => Captured::AllExcept(tree),
        };
        let answer = routing::check(&captured, &endpoints, &rendered, &self.routing.played_on);
        if answer.paths != self.routing.reported {
            self.log.log(|| {
                describe_routing(
                    &answer.paths,
                    &self.routing.reported,
                    &endpoints,
                    &captured,
                    &rendered,
                    &answer.played_on,
                )
            });
            self.routing.reported.clone_from(&answer.paths);
            self.pending.push(BackendEvent::HeardTwice {
                paths: answer.paths,
            });
        }
        self.routing.played_on = answer.played_on;
    }

    /// The sources to offer, from the processes and audio sessions now.
    fn read_sources(&mut self) -> Result<Vec<SourceInfo>, BackendError> {
        let processes = self.driver.processes()?;
        let session_pids = self.driver.session_pids()?;
        let mut sources = source_list(&processes, &session_pids, self.driver.own_pid());
        match self.driver.inputs() {
            Ok(readings) => {
                if self.inputs_error.take().is_some() {
                    self.log
                        .log(|| "listing the input devices works again".to_owned());
                }
                sources.extend(input_sources(readings));
            }
            Err(error) => {
                if self.inputs_error.as_ref() != Some(&error) {
                    self.log.log(|| {
                        format!("listing the input devices failed, so none are offered: {error}")
                    });
                    self.inputs_error = Some(error);
                }
            }
        }
        Ok(sources)
    }
}

impl<D: Driver> Drop for Controller<D> {
    fn drop(&mut self) {
        // The Cast host's quit runs beside the fade and the AirPlay
        // teardown, so the quit stays within its budget.
        let cast = self.cast.as_mut().and_then(CastSpeakers::begin_shutdown);
        self.next_start = None;
        self.end_session();
        self.reap(true);
        self.stop_following_device_volumes();
        self.airplay = None;
        if let Some(cast) = cast {
            let clean = cast.join().unwrap_or(false);
            if !clean {
                self.log
                    .log(|| "Google Cast: the host didn't stop in time".to_owned());
            }
        }
    }
}

impl<D: Driver> AudioBackend for Controller<D> {
    fn list_sources(&mut self) -> Result<Vec<SourceInfo>, BackendError> {
        let sources = self.read_sources();
        match &sources {
            Err(error) if self.sources_error.as_ref() != Some(error) => {
                self.log
                    .log(|| format!("listing the sources failed: {error}"));
                self.sources_error = Some(error.clone());
            }
            Ok(_) if self.sources_error.is_some() => {
                self.log
                    .log(|| "listing the sources works again".to_owned());
                self.sources_error = None;
            }
            _ => {}
        }
        sources
    }

    fn find_app(
        &mut self,
        root_pid: u32,
        executable: &str,
    ) -> Result<Option<SourceInfo>, BackendError> {
        let processes = self.driver.processes().inspect_err(|error| {
            self.log
                .log(|| format!("looking for {executable} failed: {error}"));
        })?;
        Ok(running_app(&processes, root_pid, executable))
    }

    fn list_outputs(&mut self) -> Result<Vec<OutputInfo>, BackendError> {
        self.refresh_outputs().1?;
        Ok(self.outputs())
    }

    fn outputs(&self) -> Vec<OutputInfo> {
        let mut outputs: Vec<OutputInfo> = self
            .present
            .iter()
            .map(|endpoint| {
                let settings = self.policy.settings(&endpoint.id);
                OutputInfo {
                    id: endpoint.id.clone(),
                    name: endpoint.name.clone(),
                    kind: endpoint.kind,
                    traits: endpoint.traits.clone(),
                    is_default: endpoint.is_default,
                    is_reference: self.policy.reference() == Some(endpoint.id.as_str()),
                    enabled: settings.enabled,
                    delay_ms: settings.delay_ms,
                    volume: settings.volume,
                    playing: self.is_playing(&endpoint.id),
                    device_volume: self.device_volumes.reading(&endpoint.id),
                }
            })
            .collect();
        outputs.sort_by(|a, b| {
            a.name
                .to_lowercase()
                .cmp(&b.name.to_lowercase())
                .then_with(|| a.id.cmp(&b.id))
        });
        outputs
    }

    fn enabled_outputs(&self) -> Vec<String> {
        self.policy
            .enabled_ids()
            .into_iter()
            .map(str::to_owned)
            .collect()
    }

    fn start(&mut self, source: &SourceId) -> Result<(), BackendError> {
        self.quit_watch.source_picked();
        self.end_session();
        self.log
            .log(|| format!("starting: capturing {}", describe_source(source)));
        let started = self.begin_session(source).inspect_err(|error| {
            self.log.log(|| format!("the start failed: {error}"));
        });
        // Outputs that couldn't start were turned off.
        self.follow_device_volumes();
        started
    }

    fn stop(&mut self) {
        if self.session.is_some() || self.next_start.is_some() {
            self.log.log(|| "stopping".to_owned());
        }
        self.next_start = None;
        self.restart_wait = None;
        self.end_session();
        self.airplay_streaming(false);
        self.release_busy();
    }

    fn running_source(&self) -> Option<SourceId> {
        self.session
            .as_ref()
            .map(|session| session.source.clone())
            .or_else(|| self.next_start.clone())
    }

    fn set_output_enabled(&mut self, id: &str, enabled: bool) -> Result<(), BackendError> {
        self.log.log(|| {
            let state = if enabled { "on" } else { "off" };
            format!("output {} switched {state}", label(&self.names, id))
        });
        self.policy.set_enabled(id, enabled);
        // Switched off, a busy or stalled output no longer waits for its
        // device: switched on again, it is tried at once.
        if !enabled {
            self.stalled.remove(id);
            self.hung.remove(id);
            self.refused.remove(id);
        }
        if !enabled && self.busy.remove(id).is_some() {
            self.pending
                .push(BackendEvent::OutputFree { id: id.to_owned() });
        }
        if self.session.is_none() {
            self.follow_device_volumes();
            return Ok(());
        }
        if enabled {
            self.refresh_if_needed(false);
        }
        let mut result = Ok(());
        for (failed, error) in self.sync_outputs() {
            if failed == id {
                result = Err(error);
            } else {
                self.pending
                    .push(BackendEvent::OutputFailed { id: failed, error });
            }
        }
        self.retime();
        // An output that couldn't start was turned off again.
        self.follow_device_volumes();
        result
    }

    fn set_output_delay(&mut self, id: &str, delay_ms: f64) {
        let before = self.policy.settings(id).delay_ms;
        if self.policy.set_delay_ms(id, delay_ms) {
            let after = self.policy.settings(id).delay_ms;
            if after != before {
                self.log.log(|| {
                    let reference = if self.policy.reference() == Some(id) {
                        ", the reference: not used"
                    } else {
                        ""
                    };
                    format!(
                        "output {} offset: {after} ms{reference}",
                        label(&self.names, id)
                    )
                });
            }
            self.retime();
        }
    }

    fn set_output_volume(&mut self, id: &str, position: f64) {
        let before = self.policy.settings(id).volume;
        if self.policy.set_volume(id, position) {
            self.apply_gain(id);
            let after = self.policy.settings(id).volume;
            if after != before {
                self.log.log(|| {
                    format!(
                        "output {} volume: {after} (gain {:.3})",
                        label(&self.names, id),
                        self.policy.params(id).gain,
                    )
                });
            }
        }
    }

    fn has_output_settings(&self, id: &str) -> bool {
        self.policy.has_settings(id)
    }

    fn move_output_settings(&mut self, from: &str, to: &str) -> Result<(), BackendError> {
        let settings = self.policy.take_settings(from);
        self.log.log(|| {
            format!(
                "output {} takes over the settings of {}, which is forgotten",
                label(&self.names, to),
                label(&self.names, from)
            )
        });
        self.policy.set_delay_ms(to, settings.delay_ms);
        self.policy.set_volume(to, settings.volume);
        if settings.enabled {
            self.set_output_enabled(to, true)
        } else {
            self.policy.set_enabled(to, false);
            Ok(())
        }
    }

    fn fix_device_volume(&mut self, id: &str) {
        self.fix_followed_device_volume(id);
    }

    fn set_master_volume(&mut self, position: f64) {
        let before = self.policy.master();
        if self.policy.set_master(position) {
            for id in self.playing_ids() {
                self.apply_gain(&id);
            }
            let master = self.policy.master();
            if let Some(airplay) = self.airplay.as_mut()
                && master != before
            {
                airplay.set_master(master);
            }
            let after = self.policy.master();
            if after != before {
                self.log.log(|| format!("master volume: {after}"));
            }
        }
    }

    fn master_volume(&self) -> f64 {
        self.policy.master()
    }

    fn set_equalizer(&mut self, settings: EqSettings) {
        if settings != self.equalizer {
            self.log
                .log(|| format!("equalizer: {}", describe_equalizer(&settings)));
        }
        self.equalizer = settings;
        if let Some(session) = self.session.as_mut() {
            session.equalizer.set_settings(settings);
        }
    }

    fn airplay_speakers(&self) -> Vec<AirPlayInfo> {
        self.airplay
            .as_ref()
            .map(AirPlaySpeakers::speakers)
            .unwrap_or_default()
    }

    fn airplay_browsing(&self) -> bool {
        self.airplay
            .as_ref()
            .is_some_and(|airplay| !airplay.is_stopped())
    }

    fn enabled_airplay(&self) -> Vec<String> {
        self.airplay
            .as_ref()
            .map(AirPlaySpeakers::enabled_ids)
            .unwrap_or_default()
    }

    fn set_airplay_enabled(&mut self, id: &str, enabled: bool) -> Result<(), AirPlayProblem> {
        let Some(airplay) = self.airplay.as_mut() else {
            // Without a sender nothing is listed, so nothing can play.
            return if enabled {
                Err(AirPlayProblem::Unsupported)
            } else {
                Ok(())
            };
        };
        let result = airplay.set_enabled(id, enabled);
        self.sync_airplay_rule();
        result
    }

    fn start_airplay_pairing(&mut self, id: &str) -> Result<(), AirPlayProblem> {
        match self.airplay.as_mut() {
            Some(airplay) => airplay.start_pairing(id),
            None => Err(AirPlayProblem::Unsupported),
        }
    }

    fn submit_airplay_pin(&mut self, id: &str, pin: &str) -> bool {
        self.airplay
            .as_mut()
            .is_some_and(|airplay| airplay.submit_pin(id, pin))
    }

    fn cancel_airplay_pairing(&mut self, id: &str) {
        if let Some(airplay) = self.airplay.as_mut() {
            airplay.cancel_pairing(id);
        }
    }

    fn forget_airplay_pairing(&mut self, id: &str) {
        if let Some(airplay) = self.airplay.as_mut() {
            airplay.forget_pairing(id);
        }
        self.sync_airplay_rule();
    }

    fn airplay_pin_prompt(&self) -> Option<AirPlayPinPrompt> {
        self.airplay.as_ref().and_then(AirPlaySpeakers::pin_prompt)
    }

    fn begin_airplay_attempt(&mut self, id: &str) {
        if let Some(airplay) = self.airplay.as_mut() {
            airplay.begin_attempt(id);
        }
    }

    fn ask_airplay_password(&mut self, id: &str) -> Result<(), AirPlayProblem> {
        match self.airplay.as_mut() {
            Some(airplay) => airplay.ask_password(id),
            None => Err(AirPlayProblem::Unsupported),
        }
    }

    fn submit_airplay_password(&mut self, id: &str, password: &str) -> bool {
        let taken = self
            .airplay
            .as_mut()
            .is_some_and(|airplay| airplay.submit_password(id, password));
        self.sync_airplay_rule();
        taken
    }

    fn cancel_airplay_password(&mut self, id: &str) {
        if let Some(airplay) = self.airplay.as_mut() {
            airplay.cancel_password(id);
        }
        self.sync_airplay_rule();
    }

    fn airplay_password_prompt(&self) -> Option<AirPlayPasswordPrompt> {
        self.airplay
            .as_ref()
            .and_then(AirPlaySpeakers::password_prompt)
    }

    fn airplay_holding_target(&self, id: &str) -> Option<String> {
        self.airplay.as_ref()?.holding_target(id)
    }

    fn set_airplay_volume(&mut self, id: &str, position: f64) {
        if let Some(airplay) = self.airplay.as_mut() {
            airplay.set_volume(id, position);
        }
    }

    fn set_airplay_delay(&mut self, id: &str, delay_ms: f64) {
        if let Some(airplay) = self.airplay.as_mut() {
            airplay.set_delay(id, delay_ms);
        }
    }

    fn airplay_sender_latency_ms(&self) -> Option<f64> {
        self.policy.sender_latency_ms()
    }

    fn airplay_adjustment(&self) -> f64 {
        self.policy.airplay_adjustment_ms()
    }

    fn set_airplay_adjustment(&mut self, adjustment_ms: f64) {
        let before = self.policy.airplay_adjustment_ms();
        if self.policy.set_airplay_adjustment_ms(adjustment_ms) {
            let after = self.policy.airplay_adjustment_ms();
            if after != before {
                self.log
                    .log(|| format!("AirPlay delay adjustment: {after} ms"));
            }
            self.hold_back_network();
            self.retime();
        }
    }

    fn cast_speakers(&self) -> Vec<CastInfo> {
        self.cast
            .as_ref()
            .map(CastSpeakers::speakers)
            .unwrap_or_default()
    }

    fn cast_discovery(&self) -> CastDiscovery {
        self.cast
            .as_ref()
            .map_or(CastDiscovery::Off, CastSpeakers::discovery)
    }

    fn set_cast_enabled(&mut self, id: &str, enabled: bool) -> Result<(), CastProblem> {
        let Some(cast) = self.cast.as_mut() else {
            return if enabled {
                Err(CastProblem::NotResponding)
            } else {
                Ok(())
            };
        };
        let result = cast.set_enabled(id, enabled);
        self.sync_cast_rule();
        result
    }

    fn set_cast_volume(&mut self, id: &str, position: f64) {
        if let Some(cast) = self.cast.as_mut() {
            cast.set_volume(id, position);
        }
    }

    fn set_cast_muted(&mut self, id: &str, muted: bool) {
        if let Some(cast) = self.cast.as_mut() {
            cast.set_muted(id, muted);
        }
    }

    fn set_cast_offset(&mut self, id: &str, offset_ms: f64) {
        if let Some(cast) = self.cast.as_mut()
            && cast.set_offset(id, offset_ms)
        {
            self.sync_cast_rule();
        }
    }

    fn airplay_ahead_ms(&self) -> f64 {
        self.airplay.as_ref().map_or(0.0, AirPlaySpeakers::ahead_ms)
    }

    fn resync_cast(&mut self, id: &str) {
        if let Some(cast) = self.cast.as_mut() {
            cast.resync(id);
        }
    }

    fn add_cast_address(&mut self, address: CastAddress) {
        if let Some(cast) = self.cast.as_mut() {
            cast.add_address(address);
        }
    }

    fn remove_cast_address(&mut self, address: &CastAddress) {
        if let Some(cast) = self.cast.as_mut() {
            cast.remove_address(address);
            self.sync_cast_rule();
        }
    }

    fn cast_memory_json(&self) -> Option<String> {
        self.cast.as_ref().and_then(CastSpeakers::memory_json)
    }

    fn calibration_engine(&self) -> Engine {
        let playing = self.playing_ids();
        Engine {
            session: self.session.as_ref().map(|_| self.sessions_started),
            hold_backs_ms: playing
                .iter()
                .map(|id| (id.clone(), self.policy.hold_back_ms(id)))
                .collect(),
            playing,
            reference: self.policy.reference().map(str::to_owned),
        }
    }

    fn calibration_tap(&self) -> Option<Arc<CalibrationTap>> {
        Some(Arc::clone(&self.calibration_tap))
    }

    fn set_calibration_solo(&mut self, id: Option<&str>) {
        if self.calibration_solo.as_deref() == id {
            return;
        }
        self.calibration_solo = id.map(str::to_owned);
        self.log.log(|| match id {
            Some(id) => format!("calibration: only {} is heard", label(&self.names, id)),
            None => "calibration: every output is heard again".to_owned(),
        });
        for id in self.playing_ids() {
            self.apply_gain(&id);
        }
    }

    fn set_calibration_airplay_solo(&mut self, id: Option<&str>) {
        if let Some(airplay) = self.airplay.as_mut() {
            airplay.set_calibration_solo(id);
        }
    }

    fn open_microphone(&mut self, seconds: f64) -> Result<Box<dyn Microphone>, MicrophoneError> {
        self.driver.open_microphone(seconds)
    }

    fn set_when_source_quits(&mut self, setting: WhenSourceQuits) {
        self.quit_watch.set_setting(setting);
    }

    fn retry_airplay(&mut self) {
        if self
            .airplay
            .as_ref()
            .is_some_and(|airplay| !airplay.is_stopped())
        {
            // A late click (the other window hadn't refreshed yet): the
            // sender runs, so there is nothing to replace.
            self.log
                .log(|| "AirPlay: Retry, but the sender runs".to_owned());
            return;
        }
        let Some(starter) = self.starter.as_mut() else {
            self.log
                .log(|| "AirPlay: Retry, but there is no way to start a sender".to_owned());
            return;
        };
        self.log
            .log(|| "AirPlay: Retry: starting a new sender".to_owned());
        let Some(started) = starter() else {
            self.pending.push(BackendEvent::AirPlayStopped);
            return;
        };
        // A running capture feeds the old sender, or none: it starts again
        // with the new sender's feed (the capture can't swap it while it
        // runs).
        let source = self.session.as_ref().map(|session| session.source.clone());
        self.end_session();
        self.use_sender(started);
        self.sync_airplay_rule();
        if let Some(source) = source
            && let Err(error) = self.begin_session(&source)
        {
            self.log
                .log(|| format!("the capture couldn't start again after Retry: {error}"));
            self.airplay_streaming(false);
            self.pending.push(BackendEvent::Stopped {
                reason: StopReason::CaptureFailed(error),
            });
        }
        self.pending.push(BackendEvent::AirPlayChanged);
    }

    fn poll(&mut self) -> Vec<BackendEvent> {
        // A busy or stalled output's next try comes a poll closer.
        // So does the end of a listed device's refusals.
        for wait in self
            .busy
            .values_mut()
            .chain(self.stalled.values_mut())
            .chain(self.refused.values_mut())
        {
            *wait = wait.saturating_sub(1);
        }
        self.device_volumes.tick();
        // Outputs that faded out stop, and then an ended session's capture;
        // a start that waited for them goes ahead.
        self.reap(false);
        self.start_when_faded();
        // A session whose capture stopped ends first. One lost because
        // Windows audio restarted, or its device went away, starts again a
        // moment later; streaming stays on meanwhile.
        let mut lost = false;
        let stop_reason = match self.session.as_mut() {
            None => None,
            Some(session) => match session.capture.state() {
                CaptureState::Running => {
                    session.trial_polls = session.trial_polls.saturating_sub(1);
                    None
                }
                CaptureState::Failed(BackendError::DeviceGone) if session.trial_polls == 0 => {
                    lost = true;
                    None
                }
                CaptureState::SourceQuit => Some(StopReason::SourceQuit),
                CaptureState::Failed(error) => Some(StopReason::CaptureFailed(error)),
            },
        };
        let quit_source = match stop_reason {
            Some(StopReason::SourceQuit) => self.session.as_ref().map(|s| s.source.clone()),
            _ => None,
        };
        let stop_reason = match quit_source {
            Some(source)
                if self.decide_quit(&source, Streaming::Running)
                    == SourceQuitAction::SwitchToSystem =>
            {
                self.switch_to_system();
                None
            }
            _ => stop_reason,
        };
        if lost {
            self.restart_capture();
        }
        if let Some(reason) = stop_reason {
            self.log
                .log(|| format!("the capture ended: {}", describe_stop(&reason)));
            self.end_session();
            self.airplay_streaming(false);
            self.release_busy();
            self.pending.push(BackendEvent::Stopped { reason });
        }
        self.take_back_feed();

        // What the AirPlay sender reported.
        if let Some(airplay) = self.airplay.as_mut() {
            let events = airplay.poll();
            self.pending.extend(events);
            self.sync_airplay_rule();
        }
        if let Some(cast) = self.cast.as_mut() {
            let events = cast.poll();
            self.pending.extend(events);
            self.sync_cast_rule();
        }
        self.sync_track_info();

        // Outputs that stopped by themselves.
        let mut ended = Vec::new();
        if let Some(session) = self.session.as_mut() {
            let stopped: Vec<String> = session
                .outputs
                .iter()
                .filter(|(_, playing)| playing.handle.has_ended())
                .map(|(id, _)| id.clone())
                .collect();
            for id in stopped {
                session.outputs.remove(&id);
                self.log.log(|| {
                    format!(
                        "output {} stopped by itself: its stream failed or its device went away",
                        label(&self.names, &id)
                    )
                });
                ended.push(id);
            }
        }

        // A stopped output may mean a device went away before its
        // notification arrived, so the list is read again then too.
        self.refresh_if_needed(!ended.is_empty());
        // An output whose device is no longer listed is lost too, whether or
        // not its stream noticed, and so is one that couldn't play while its
        // device was still listed.
        ended.extend(self.stop_unlisted_outputs());
        ended.extend(self.forget_unlisted_unplayable());
        let failures = self.sync_outputs();
        // An output that left or joined moves the others (S177).
        self.retime();
        for id in ended {
            // An output whose device is still there plays again at once
            // (after a format change, say); one that isn't is lost.
            if !self.is_playing(&id) && !failures.iter().any(|(failed, _)| *failed == id) {
                self.pending.push(BackendEvent::OutputLost { id });
            }
        }
        self.report_failures(failures);
        // Outputs that couldn't start were turned off; then what Windows
        // reported of the followed outputs' own volumes.
        self.follow_device_volumes();
        self.read_device_volume_changes();
        let now = Instant::now();
        self.check_routing_if_due(now);
        self.log_status_if_due(now);
        let events = std::mem::take(&mut self.pending);
        // A device volume that changed is logged when its decision changes
        // (S201), so its event isn't.
        for event in events
            .iter()
            .filter(|event| !matches!(event, BackendEvent::DeviceVolumeChanged { .. }))
        {
            self.log
                .log(|| format!("event: {}", describe_event(event, &self.names)));
        }
        events
    }
}

/// Whether a capture that couldn't start again after a loss may start at a
/// later try: Windows audio isn't back yet, the device is held or failed for
/// now, or an input device is inactive for a moment (a USB microphone after a
/// wake, or while Windows lists it again). An app that quit, or a source that
/// can't be captured, stays so.
/// An activation Windows never completed, or a capture thread that didn't
/// answer in time ([`BackendError::NotResponding`]), is the last try: each
/// one held the UI thread for the whole wait.
fn may_start_later(error: &BackendError) -> bool {
    match error {
        BackendError::DeviceGone | BackendError::DeviceBusy | BackendError::InputGone => true,
        BackendError::System { code, .. } => *code != TIMED_OUT,
        _ => false,
    }
}

/// How the log names an output: its last listed name and its endpoint ID.
fn label(names: &BTreeMap<String, String>, id: &str) -> String {
    match names.get(id) {
        Some(name) => format!("\"{name}\" ({id})"),
        None => format!("({id})"),
    }
}

/// The log lines for a listing of `listed` after `previous`: a heading, then
/// each output, and each output gone. Against an earlier listing, a new
/// output is marked `+`, a changed one (a new name, kind or default output)
/// `*`, and one gone `-`.
fn describe_listing(
    previous: Option<&[Endpoint]>,
    listed: &[Endpoint],
    setups: &BTreeMap<String, StreamSetup>,
) -> String {
    let mut text = format!("outputs listed: {}", listed.len());
    for endpoint in listed {
        let mark = match previous.map(|previous| previous.iter().find(|e| e.id == endpoint.id)) {
            None => ' ',
            Some(None) => '+',
            Some(Some(before)) if before != endpoint => '*',
            Some(Some(_)) => ' ',
        };
        let default = if endpoint.is_default {
            ", Windows default"
        } else {
            ""
        };
        let played = setups.get(&endpoint.id).map_or_else(String::new, |setup| {
            format!(
                ", last played at {} Hz, {} ch",
                setup.format.rate, setup.format.channels
            )
        });
        text.push_str(&format!(
            "\n  {mark} \"{}\" ({}): {:?}{default}{played}",
            endpoint.name, endpoint.id, endpoint.kind
        ));
    }
    for gone in previous
        .unwrap_or_default()
        .iter()
        .filter(|before| !listed.iter().any(|endpoint| endpoint.id == before.id))
    {
        text.push_str(&format!("\n  - \"{}\" ({}): gone", gone.name, gone.id));
    }
    text
}

/// The log lines for a double-audio answer of `paths` after `previous`: a
/// heading, each endpoint the source is heard from straight and why, with
/// its mute and volume, then each endpoint dropped since and why.
fn describe_routing(
    paths: &[DirectPath],
    previous: &[DirectPath],
    endpoints: &[EndpointAudio],
    captured: &Captured,
    rendered: &[String],
    played_on: &[PlayedOn],
) -> String {
    let silent = !endpoints.iter().any(|endpoint| {
        endpoint
            .active_pids
            .iter()
            .any(|&pid| captured.captures(pid))
    });
    let mut text = if paths.is_empty() {
        "double audio: none, the source is heard only through AudioSync".to_owned()
    } else {
        format!(
            "double audio: the source plays straight from {} output(s) too",
            paths.len()
        )
    };
    for path in paths {
        let endpoint = endpoints.iter().find(|e| e.id == path.endpoint_id);
        let why = match path.reason {
            DirectReason::DefaultEndpoint => {
                "Windows' default output, and the whole system is captured".to_owned()
            }
            DirectReason::Session => {
                let pids: Vec<String> = endpoint
                    .map(|endpoint| {
                        endpoint
                            .active_pids
                            .iter()
                            .filter(|&&pid| captured.captures(pid))
                            .map(u32::to_string)
                            .collect()
                    })
                    .unwrap_or_default();
                let idle = played_on
                    .iter()
                    .find(|played| played.endpoint_id == path.endpoint_id)
                    .map_or(0, |played| played.idle_checks);
                match pids.as_slice() {
                    [] if silent => "the source played there last and is silent now".to_owned(),
                    [] => format!("no active session of the source there for {idle} check(s)"),
                    [pid] => format!("an active session of process {pid}"),
                    _ => format!("active sessions of processes {}", pids.join(", ")),
                }
            }
        };
        let volume = describe_volume(endpoint.and_then(|endpoint| endpoint.volume));
        let plays = if path.audiosync_plays_here {
            "; AudioSync plays there too"
        } else {
            ""
        };
        text.push_str(&format!(
            "\n  \"{}\" ({}): {why}; {volume}{plays}",
            path.endpoint_name, path.endpoint_id
        ));
    }
    for gone in previous.iter().filter(|gone| {
        !paths
            .iter()
            .any(|path| path.endpoint_id == gone.endpoint_id)
    }) {
        let why = match endpoints.iter().find(|e| e.id == gone.endpoint_id) {
            None => "not connected anymore",
            Some(_) if rendered.is_empty() => "AudioSync plays to no output",
            Some(endpoint) => match endpoint.volume {
                Some(volume) if volume.muted => "muted",
                Some(volume) if volume.level <= 0.0 => "volume 0",
                _ if endpoint.virtual_device && !rendered.contains(&endpoint.id) => {
                    "a software device AudioSync doesn't play to"
                }
                _ if gone.reason == DirectReason::DefaultEndpoint && !endpoint.is_default => {
                    "not Windows' default output anymore"
                }
                _ => "the source doesn't play there anymore",
            },
        };
        text.push_str(&format!(
            "\n  no longer \"{}\" ({}): {why}",
            gone.endpoint_name, gone.endpoint_id
        ));
    }
    text
}

/// An endpoint's mute and volume, as the log shows them.
fn describe_volume(volume: Option<EndpointVolume>) -> String {
    match volume {
        Some(volume) => format!(
            "{}, volume {:.2}",
            if volume.muted { "muted" } else { "not muted" },
            volume.level
        ),
        None => "mute and volume unknown".to_owned(),
    }
}

/// A source as the log names it.
fn describe_source(source: &SourceId) -> String {
    match source {
        SourceId::WholeSystem => "the whole system".to_owned(),
        SourceId::App { root_pid } => format!("the app of process {root_pid}"),
        SourceId::Input { endpoint_id } => format!("the input device {endpoint_id}"),
    }
}

/// What a capture captures, and the format Windows delivers it in.
fn describe_capture(target: &CaptureTarget, setup: Option<CaptureAttempt>) -> String {
    let what = match target {
        CaptureTarget::Loopback(activation) => {
            let pid = activation.target_pid;
            match activation.mode {
                LoopbackMode::ExcludeTree => {
                    format!("everything except process {pid} and its children")
                }
                LoopbackMode::IncludeTree => format!("process {pid} and its children"),
            }
        }
        CaptureTarget::Input { endpoint_id } => format!("input device {endpoint_id}"),
    };
    let format = match setup {
        Some(attempt) => {
            let sample = match attempt.format.sample {
                SampleType::F32 => "32-bit float",
                SampleType::I16 => "16-bit integer",
            };
            let converted = if attempt.autoconvert {
                ", converted by Windows"
            } else {
                ""
            };
            format!(
                "{} Hz, {} ch, {sample}{converted}",
                attempt.format.rate, attempt.format.channels
            )
        }
        None => "format not known".to_owned(),
    };
    format!("{what}; {format}")
}

/// How an output's stream is set up, as the log shows it.
fn describe_stream(setup: Option<StreamSetup>) -> String {
    let Some(setup) = setup else {
        return "stream format not known".to_owned();
    };
    let format = setup.format;
    let samples = if format.convert {
        "float, converted by Windows to the device's format"
    } else {
        "float, the device's own format"
    };
    let buffer_ms = f64::from(setup.buffer_frames) * 1_000.0 / f64::from(format.rate.max(1));
    format!(
        "{} Hz, {} ch (mask 0x{:x}), {samples}; buffer {} frames ({buffer_ms:.1} ms)",
        format.rate, format.channels, format.channel_mask, setup.buffer_frames
    )
}

/// The log line of a capture that started: what it captures, the format
/// Windows delivers it in, and whether it feeds the AirPlay sender.
fn describe_capture_start(
    target: &CaptureTarget,
    setup: Option<CaptureAttempt>,
    feeds_airplay: bool,
) -> String {
    let airplay = if feeds_airplay {
        "; it feeds the AirPlay sender"
    } else {
        ""
    };
    format!(
        "capture started: {}{airplay}",
        describe_capture(target, setup)
    )
}

/// The log line of an output that started: the output as [`label`] names
/// it, how its stream is set up, its hold-back in ms, its volume slider
/// position, the gain it plays at and, for a Bluetooth output, what its
/// start lead adds in capture frames (S230).
fn describe_output_start(
    label: &str,
    setup: Option<StreamSetup>,
    hold_back_ms: f64,
    volume: f64,
    gain: f32,
    extra_lead_frames: i64,
) -> String {
    let bluetooth = if extra_lead_frames > 0 {
        let ms = extra_lead_frames as f64 * 1_000.0 / f64::from(SESSION_RATE);
        format!("; Bluetooth, extra lead {ms:.1} ms")
    } else {
        String::new()
    };
    format!(
        "output {label} started: {}; delay {hold_back_ms} ms, volume {volume} (gain {gain:.3}){bluetooth}",
        describe_stream(setup),
    )
}

/// What a status line covers: the time since the last one, and the frames
/// the capture had written by then and by now.
#[derive(Clone, Copy, Debug, PartialEq)]
struct StatusSpan {
    elapsed: Duration,
    frames_then: u64,
    frames: u64,
}

/// Whether a status check at `now`, with `frames` captured so far, owes the
/// log a status line. `last` is the mark of the last line: when it came and
/// the frames captured then. The first check of a session only sets the
/// mark; a later one owes a line once [`STATUS_INTERVAL`] has passed since
/// the mark, and moves the mark to `now`.
fn status_due(last: &mut Option<(Instant, u64)>, now: Instant, frames: u64) -> Option<StatusSpan> {
    let Some((since, frames_then)) = *last else {
        *last = Some((now, frames));
        return None;
    };
    let elapsed = now.saturating_duration_since(since);
    if elapsed < STATUS_INTERVAL {
        return None;
    }
    *last = Some((now, frames));
    Some(StatusSpan {
        elapsed,
        frames_then,
        frames,
    })
}

/// One playing output's part of a status line, as read from the atomics its
/// audio thread keeps.
#[derive(Clone, Debug, PartialEq)]
struct OutputStatus {
    /// The output as [`label`] names it.
    label: String,
    /// Its hold-back, in ms.
    hold_back_ms: f64,
    /// The delay its renderer was asked for, in session-rate frames.
    delay_frames: i64,
    /// Its volume slider position.
    volume: f64,
    /// The gain it plays at.
    gain: f32,
    /// Its reader's last published lag.
    lag_frames: i64,
    /// Its reader's counters.
    counters: ReaderCounters,
    /// Capture frames its converter was slowed down by to keep the reader
    /// off W or follow the capture's clock (S206, S207), sped up by to
    /// follow it (S207), and device frames it couldn't produce.
    slowed_frames: u64,
    sped_frames: u64,
    shortfall_frames: u64,
    /// The shortages its slow-down made up (S206) and its least slack, `None`
    /// before the observer's warm-up (S229).
    shortage_frames: u64,
    least_slack_ns: Option<i64>,
}

/// A status line: what the session captures from `source` and how fast
/// over `span`, then a line per output in `outputs` (or that none plays),
/// then the AirPlay sender's status when it has one.
fn describe_status(
    source: &SourceId,
    span: StatusSpan,
    outputs: &[OutputStatus],
    airplay: Option<&str>,
) -> String {
    let seconds = span.elapsed.as_secs_f64();
    let mut text = format!(
        "status: capturing {}; {} frames captured, {:.0} frames/s over {seconds:.1} s",
        describe_source(source),
        span.frames,
        span.frames.saturating_sub(span.frames_then) as f64 / seconds,
    );
    for output in outputs {
        let counters = output.counters;
        text.push_str(&format!(
            "\n  output {}: delay {} ms ({} frames), volume {} (gain {:.3}), lag {} frames; \
             primes {}, resyncs {}, starved reads {} ({} frames), drift skips {} ({} frames), \
             drift silences {} ({} frames), rejected copies {}, slowed {} frames, \
             sped {} frames, shortfall {} frames, shortage {} frames, least slack {}",
            output.label,
            output.hold_back_ms,
            output.delay_frames,
            output.volume,
            output.gain,
            output.lag_frames,
            counters.primes,
            counters.resyncs,
            counters.starved_renders,
            counters.starved_frames,
            counters.drift_skips,
            counters.drift_skip_frames,
            counters.drift_silences,
            counters.drift_silence_frames,
            counters.rejected_copies,
            output.slowed_frames,
            output.sped_frames,
            output.shortfall_frames,
            output.shortage_frames,
            output
                .least_slack_ns
                .map_or("n/a".to_owned(), |ns| format!("{:.1} ms", ns as f64 / 1e6)),
        ));
    }
    if outputs.is_empty() {
        text.push_str("\n  no output plays");
    }
    if let Some(airplay) = airplay {
        text.push('\n');
        text.push_str(airplay);
    }
    text
}

/// The equalizer settings as the log shows them.
fn describe_equalizer(settings: &EqSettings) -> String {
    let bands: Vec<String> = settings
        .gains_db
        .iter()
        .map(|gain| format!("{gain:+.1}"))
        .collect();
    format!(
        "{}, preamp {:+.1} dB, bands {} dB",
        if settings.enabled { "on" } else { "off" },
        settings.preamp_db,
        bands.join(" ")
    )
}

/// Why playback stopped on its own, as the log says it.
fn describe_stop(reason: &StopReason) -> String {
    match reason {
        StopReason::SourceQuit => "the captured app quit".to_owned(),
        StopReason::CaptureFailed(error) => format!("the capture failed: {error}"),
    }
}

/// An event the backend reports, as the log says it.
fn describe_event(event: &BackendEvent, names: &BTreeMap<String, String>) -> String {
    match event {
        BackendEvent::OutputsChanged => "the outputs changed".to_owned(),
        BackendEvent::OutputLost { id } => format!("output {} lost", label(names, id)),
        BackendEvent::OutputFailed { id, error } => {
            format!("output {} failed: {error}", label(names, id))
        }
        BackendEvent::OutputBusy { id } => {
            format!(
                "output {} busy: another app holds its device",
                label(names, id)
            )
        }
        BackendEvent::OutputFree { id } => {
            format!("output {} no longer waits for its device", label(names, id))
        }
        BackendEvent::Stopped { reason } => format!("stopped: {}", describe_stop(reason)),
        BackendEvent::SwitchedToSystem => {
            "the captured app quit: the whole system is the source now".to_owned()
        }
        BackendEvent::HeardTwice { paths } if paths.is_empty() => {
            "the source is heard only through AudioSync again".to_owned()
        }
        BackendEvent::HeardTwice { paths } => {
            let endpoints: Vec<String> = paths
                .iter()
                .map(|path| label(names, &path.endpoint_id))
                .collect();
            format!(
                "the source is heard twice, straight from {}",
                endpoints.join(", ")
            )
        }
        BackendEvent::AirPlayChanged => "the AirPlay speakers changed".to_owned(),
        BackendEvent::AirPlayPlaying { id } => format!("AirPlay speaker ({id}) plays"),
        BackendEvent::AirPlayTrouble { id, problem } => {
            format!("AirPlay speaker ({id}) stopped ({problem:?}); the sender keeps trying")
        }
        BackendEvent::AirPlayFailed { id, problem } => {
            format!("AirPlay speaker ({id}) can't play ({problem:?}); it is turned off")
        }
        BackendEvent::AirPlayMemberNeedsPassword { id, member } => {
            format!("AirPlay target ({id}) turned off: its member {member} asks for a password")
        }
        BackendEvent::AirPlayPaired { id } => format!("AirPlay speaker ({id}) paired and on"),
        BackendEvent::AirPlayStopped => {
            "the AirPlay sender stopped; it waits for the user's Retry".to_owned()
        }
        BackendEvent::AirPlayPtpFree => {
            "an AirPlay session bound the PTP ports; they are free again".to_owned()
        }
        BackendEvent::AirPlayPtpPortsTaken => {
            "PTP port 319 or 320 is taken; AirPlay speakers that need PTP can't play".to_owned()
        }
        BackendEvent::DeviceVolumeChanged { id } => {
            format!("output {}: its own volume changed", label(names, id))
        }
        BackendEvent::CastChanged => "the Google Cast speakers changed".to_owned(),
        BackendEvent::CastPlaying { id } => format!("Google Cast speaker ({id}) plays"),
        BackendEvent::CastFailed { id, problem } => {
            format!("Google Cast speaker ({id}) can't play ({problem:?}); it is turned off")
        }
        BackendEvent::CastMemoryChanged => "the Google Cast settings changed".to_owned(),
        BackendEvent::PairingStore { saved: true } => {
            "the AirPlay pairings' file can be written again".to_owned()
        }
        BackendEvent::PairingStore { saved: false } => {
            "the AirPlay pairings' file couldn't be written; pairings added or forgotten \
             since last until AudioSync quits"
                .to_owned()
        }
        BackendEvent::PairingsKeptAside { file } => {
            format!("the AirPlay pairings' file ignored at launch was kept as {file}")
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::thread;

    use audiosync_airplay::clock::SenderClock;
    use audiosync_airplay::discovery::Device;
    use audiosync_airplay::error::ErrorKind;
    use audiosync_airplay::sender::{NowPlaying, SpeakerState};
    use audiosync_airplay::stream::{Stream, StreamConfig};
    use audiosync_airplay::volume::Volume;
    use audiosync_core::render::STOP_FADE_CAP;

    use super::*;
    use crate::audio::airplay::tests::{Call, FakeAirPlay, OPEN_TXT, device, id_of};
    use crate::audio::backend::OutputKind;
    use crate::audio::capture::Activation;
    use crate::audio::mock::{MOCK_OWN_PID, MockBackend};
    use crate::audio::now_playing::fake::{FakeReader, shared_labels, track};
    use crate::audio::outputs::{EndpointProperties, EndpointReading};
    use crate::audio::routing::{DirectPath, DirectReason, HOLD_CHECKS};
    use crate::debug_log::{FILE_NAME, TEST_FLUSH, assert_in_order, read_messages, test_dir};

    const SPEAKERS: &str = "speakers";
    const HEADPHONES: &str = "headphones";
    const USB_OUTPUT: &str = "usb-dac";
    const PLAYER_PID: u32 = 2000;

    /// A stopped mock with two outputs, both on and listed once (as the UI
    /// does), and one app.
    fn backend() -> MockBackend {
        let mut backend = MockBackend::empty();
        backend.connect_output(SPEAKERS, "Speakers");
        backend.connect_output(HEADPHONES, "Headphones");
        backend.add_app(PLAYER_PID, "player.exe");
        backend.set_output_enabled(SPEAKERS, true).unwrap();
        backend.set_output_enabled(HEADPHONES, true).unwrap();
        backend.list_outputs().unwrap();
        backend
    }

    fn playing(backend: &MockBackend) -> Vec<String> {
        backend.playing_ids()
    }

    #[test]
    fn the_whole_system_is_captured_without_audiosync() {
        let mut backend = backend();
        backend.start(&SourceId::WholeSystem).unwrap();
        assert_eq!(
            backend.driver().captures_started(),
            [CaptureTarget::Loopback(Activation {
                mode: LoopbackMode::ExcludeTree,
                target_pid: MOCK_OWN_PID,
            })]
        );
        assert_eq!(backend.running_source(), Some(SourceId::WholeSystem));
    }

    #[test]
    fn an_app_is_captured_with_its_tree() {
        let mut backend = backend();
        let source = SourceId::App {
            root_pid: PLAYER_PID,
        };
        backend.start(&source).unwrap();
        assert_eq!(
            backend.driver().captures_started(),
            [CaptureTarget::Loopback(Activation {
                mode: LoopbackMode::IncludeTree,
                target_pid: PLAYER_PID,
            })]
        );
        assert_eq!(backend.running_source(), Some(source));
    }

    #[test]
    fn audiosync_itself_is_never_captured() {
        let mut backend = backend();
        let result = backend.start(&SourceId::App {
            root_pid: MOCK_OWN_PID,
        });
        assert_eq!(result, Err(BackendError::OwnProcess));
        assert!(backend.driver().captures_started().is_empty());
        assert_eq!(backend.running_source(), None);
        assert!(playing(&backend).is_empty());
    }

    #[test]
    fn an_app_that_quit_before_the_start_is_gone() {
        let mut backend = backend();
        backend.quit_app(PLAYER_PID);
        let result = backend.start(&SourceId::App {
            root_pid: PLAYER_PID,
        });
        assert_eq!(result, Err(BackendError::SourceGone));
        assert_eq!(backend.running_source(), None);
    }

    #[test]
    fn a_start_plays_every_enabled_connected_output() {
        let mut backend = backend();
        backend.connect_output("tv", "TV");
        backend.start(&SourceId::WholeSystem).unwrap();
        assert_eq!(playing(&backend), [HEADPHONES, SPEAKERS]);
        assert_eq!(backend.driver().renderers_started(), [HEADPHONES, SPEAKERS]);
    }

    #[test]
    fn a_capture_that_cant_start_starts_nothing() {
        let mut backend = backend();
        backend.refuse_captures(Some(BackendError::System {
            call: "IAudioClient::Initialize",
            code: -1,
        }));
        assert!(backend.start(&SourceId::WholeSystem).is_err());
        assert_eq!(backend.running_source(), None);
        assert!(backend.driver().renderers_started().is_empty());
    }

    #[test]
    fn outputs_turned_on_and_off_while_playing_follow_at_once() {
        let mut backend = backend();
        backend.set_output_enabled(HEADPHONES, false).unwrap();
        backend.start(&SourceId::WholeSystem).unwrap();
        assert_eq!(playing(&backend), [SPEAKERS]);
        backend.set_output_enabled(HEADPHONES, true).unwrap();
        assert_eq!(playing(&backend), [HEADPHONES, SPEAKERS]);
        backend.set_output_enabled(SPEAKERS, false).unwrap();
        assert_eq!(playing(&backend), [HEADPHONES]);
    }

    #[test]
    fn a_stop_ends_every_output_after_asking_them_to_fade_out() {
        let mut backend = backend();
        backend.start(&SourceId::WholeSystem).unwrap();
        backend.stop();
        assert!(playing(&backend).is_empty());
        assert_eq!(backend.running_source(), None);
        assert_eq!(backend.driver().renderers_faded(), 2);
    }

    /// How long a call on the UI thread may take without counting as a wait.
    const NO_WAIT: Duration = Duration::from_millis(100);

    #[test]
    fn a_speaker_switched_off_fades_out_before_its_stream_stops() {
        let mut backend = backend();
        backend.start(&SourceId::WholeSystem).unwrap();
        backend.driver().hold_fades(true);
        let asked = Instant::now();
        backend.set_output_enabled(HEADPHONES, false).unwrap();
        assert!(asked.elapsed() < NO_WAIT, "the UI thread doesn't wait");
        assert_eq!(playing(&backend), [SPEAKERS]);
        assert_eq!(backend.driver().renderers_faded(), 1, "asked to fade out");
        assert_eq!(
            backend.driver().renderers_running(),
            2,
            "its stream runs while it fades"
        );
        backend.poll();
        assert_eq!(backend.driver().renderers_running(), 2);
        backend.driver().hold_fades(false);
        backend.poll();
        assert_eq!(
            backend.driver().renderers_running(),
            1,
            "stopped once it faded"
        );
    }

    #[test]
    fn a_stop_waits_for_every_output_to_fade_and_never_more_than_500_ms() {
        let mut backend = backend();
        backend.start(&SourceId::WholeSystem).unwrap();
        backend.driver().hold_fades(true);
        let asked = Instant::now();
        backend.stop();
        assert!(asked.elapsed() < NO_WAIT, "the UI thread doesn't wait");
        assert_eq!(backend.running_source(), None);
        assert_eq!(
            backend.driver().renderers_faded(),
            2,
            "every output is asked at once"
        );
        assert!(
            backend.driver().capture_running(),
            "the capture runs while they fade"
        );
        assert_eq!(backend.driver().renderers_running(), 2);
        backend.poll();
        assert!(backend.driver().capture_running());
        // Once every output has faded, the next poll stops them and then
        // the capture.
        backend.driver().hold_fades(false);
        backend.poll();
        assert!(!backend.driver().capture_running());
        assert_eq!(backend.driver().renderers_running(), 0);

        // Outputs that never fade are stopped anyway once 500 ms have passed.
        backend.start(&SourceId::WholeSystem).unwrap();
        backend.driver().hold_fades(true);
        backend.stop();
        backend.poll();
        assert!(backend.driver().capture_running());
        thread::sleep(STOP_FADE_CAP + Duration::from_millis(20));
        backend.poll();
        assert!(!backend.driver().capture_running());
        assert_eq!(backend.driver().renderers_running(), 0);

        // A source change is a stop and a start: the new capture waits for
        // the old outputs' fades, never more than 500 ms, and the UI thread
        // doesn't wait for it.
        backend.start(&SourceId::WholeSystem).unwrap();
        let asked = Instant::now();
        backend
            .start(&SourceId::App {
                root_pid: PLAYER_PID,
            })
            .unwrap();
        assert!(asked.elapsed() < NO_WAIT, "the UI thread doesn't wait");
        assert_eq!(backend.driver().captures_started().len(), 3);
        thread::sleep(STOP_FADE_CAP + Duration::from_millis(20));
        backend.poll();
        assert_eq!(backend.driver().captures_started().len(), 4);
        assert_eq!(
            backend.driver().renderers_running(),
            2,
            "only the new outputs"
        );
        backend.driver().hold_fades(false);
    }

    #[test]
    fn a_source_change_starts_the_new_capture_at_a_poll_once_the_old_outputs_faded() {
        let mut backend = backend();
        backend.start(&SourceId::WholeSystem).unwrap();
        backend.driver().hold_fades(true);
        let app = SourceId::App {
            root_pid: PLAYER_PID,
        };
        let asked = Instant::now();
        backend.start(&app).unwrap();
        assert!(asked.elapsed() < NO_WAIT, "the UI thread doesn't wait");
        assert_eq!(backend.running_source(), Some(app.clone()));
        assert_eq!(
            backend.driver().captures_started().len(),
            1,
            "the new capture waits"
        );
        assert!(
            backend.driver().capture_running(),
            "the old capture runs while its outputs fade"
        );
        backend.poll();
        assert_eq!(backend.driver().captures_started().len(), 1);
        backend.driver().hold_fades(false);
        backend.poll();
        assert_eq!(backend.driver().captures_started().len(), 2);
        assert_eq!(backend.driver().renderers_running(), 2);
        assert_eq!(playing(&backend), [HEADPHONES, SPEAKERS]);
        assert_eq!(backend.running_source(), Some(app.clone()));

        // A source that can't be captured is refused at once; one whose
        // capture fails when it starts at the poll stops with that reason.
        backend.driver().hold_fades(true);
        assert_eq!(
            backend.start(&SourceId::App { root_pid: 4321 }),
            Err(BackendError::SourceGone)
        );
        backend.start(&SourceId::WholeSystem).unwrap();
        backend.refuse_captures(Some(BackendError::DeviceGone));
        backend.driver().hold_fades(false);
        let events = backend.poll();
        assert!(events.contains(&BackendEvent::Stopped {
            reason: StopReason::CaptureFailed(BackendError::DeviceGone),
        }));
        assert_eq!(backend.running_source(), None);

        // A stop while the start waits forgets it.
        backend.refuse_captures(None);
        backend.start(&app).unwrap();
        backend.driver().hold_fades(true);
        backend.start(&SourceId::WholeSystem).unwrap();
        backend.stop();
        assert_eq!(backend.running_source(), None);
        backend.driver().hold_fades(false);
        backend.poll();
        assert_eq!(backend.running_source(), None);
        assert_eq!(backend.driver().renderers_running(), 0);
    }

    #[test]
    fn removing_several_outputs_asks_all_to_fade_then_waits_once() {
        let mut backend = backend();
        backend.connect_output(USB_OUTPUT, "USB DAC");
        backend.set_output_enabled(USB_OUTPUT, true).unwrap();
        backend.list_outputs().unwrap();
        backend.start(&SourceId::WholeSystem).unwrap();
        assert_eq!(backend.driver().renderers_running(), 3);
        backend.driver().hold_fades(true);
        let asked = Instant::now();
        backend.set_output_enabled(SPEAKERS, false).unwrap();
        backend.set_output_enabled(HEADPHONES, false).unwrap();
        assert!(asked.elapsed() < NO_WAIT, "the UI thread doesn't wait");
        assert_eq!(
            backend.driver().renderers_faded(),
            2,
            "both asked before either stopped"
        );
        assert_eq!(backend.driver().renderers_running(), 3);
        assert_eq!(playing(&backend), [USB_OUTPUT]);
        backend.driver().hold_fades(false);
        backend.poll();
        assert_eq!(backend.driver().renderers_running(), 1);
    }

    #[test]
    fn each_output_plays_at_its_own_delay_and_volume() {
        let mut backend = backend();
        backend.set_output_delay(SPEAKERS, 120.0);
        backend.set_output_volume(SPEAKERS, 50.0);
        backend.set_output_delay(HEADPHONES, 0.0);
        backend.set_output_volume(HEADPHONES, 100.0);
        backend.start(&SourceId::WholeSystem).unwrap();
        assert_eq!(
            backend.playing_params(SPEAKERS),
            Some(OutputParams {
                delay_frames: 5_760,
                gain: 0.125,
            })
        );
        assert_eq!(
            backend.playing_params(HEADPHONES),
            Some(OutputParams {
                delay_frames: 0,
                gain: 1.0,
            })
        );
    }

    #[test]
    fn a_calibration_lets_one_output_be_heard_and_then_all_again() {
        let mut backend = backend();
        backend.set_output_volume(SPEAKERS, 50.0);
        backend.start(&SourceId::WholeSystem).unwrap();
        let gain = |backend: &MockBackend, id| backend.playing_params(id).unwrap().gain;
        backend.set_calibration_solo(Some(SPEAKERS));
        assert_eq!(gain(&backend, SPEAKERS), 0.125);
        assert_eq!(gain(&backend, HEADPHONES), 0.0);
        backend.set_output_volume(HEADPHONES, 80.0);
        assert_eq!(
            gain(&backend, HEADPHONES),
            0.0,
            "a volume change keeps it silent"
        );
        backend.set_calibration_solo(None);
        assert_eq!(gain(&backend, SPEAKERS), 0.125);
        assert!(gain(&backend, HEADPHONES) > 0.0);
    }

    #[test]
    fn a_calibration_sees_a_restarted_session_as_another() {
        let mut backend = backend();
        assert_eq!(backend.calibration_engine().session, None);
        backend.start(&SourceId::WholeSystem).unwrap();
        let first = backend.calibration_engine();
        assert!(first.session.is_some());
        assert_eq!(first.playing.len(), 2);
        assert_eq!(first.hold_backs_ms.len(), 2);
        backend.stop();
        poll_quietly(&mut backend, 3);
        backend.start(&SourceId::WholeSystem).unwrap();
        poll_quietly(&mut backend, 3);
        assert_ne!(backend.calibration_engine().session, first.session);
    }

    #[test]
    fn delay_and_volume_changes_apply_live() {
        let mut backend = backend();
        backend.start(&SourceId::WholeSystem).unwrap();
        backend.set_output_delay(SPEAKERS, 250.0);
        backend.set_output_volume(SPEAKERS, 50.0);
        backend.set_master_volume(50.0);
        assert_eq!(
            backend.playing_params(SPEAKERS),
            Some(OutputParams {
                delay_frames: 12_000,
                gain: 0.015_625,
            })
        );
        backend.set_output_delay(SPEAKERS, f64::NAN);
        assert_eq!(
            backend.playing_params(SPEAKERS).unwrap().delay_frames,
            12_000
        );
        assert_eq!(backend.master_volume(), 50.0);
    }

    #[test]
    fn a_device_removed_and_added_again_plays_again_with_its_settings() {
        let mut backend = backend();
        backend.start(&SourceId::WholeSystem).unwrap();
        assert!(backend.poll().is_empty());

        backend.disconnect_output(HEADPHONES);
        let events = backend.poll();
        assert!(events.contains(&BackendEvent::OutputLost {
            id: HEADPHONES.to_owned(),
        }));
        assert!(events.contains(&BackendEvent::OutputsChanged));
        assert_eq!(playing(&backend), [SPEAKERS]);
        assert!(backend.policy().settings(HEADPHONES).enabled, "it stays on");

        // Changed while away: applies when it's back.
        backend.set_output_delay(HEADPHONES, 300.0);
        assert!(backend.poll().is_empty());

        backend.connect_output(HEADPHONES, "Headphones");
        assert_eq!(backend.poll(), [BackendEvent::OutputsChanged]);
        assert_eq!(playing(&backend), [HEADPHONES, SPEAKERS]);
        assert_eq!(
            backend.playing_params(HEADPHONES).unwrap().delay_frames,
            14_400
        );
    }

    #[test]
    fn an_unplugged_output_whose_stream_never_notices_is_lost_and_comes_back() {
        let mut backend = backend();
        backend.start(&SourceId::WholeSystem).unwrap();

        // Its renderer never ends by itself: the device list tells.
        backend.disconnect_output_unnoticed(HEADPHONES);
        let events = backend.poll();
        assert!(events.contains(&BackendEvent::OutputLost {
            id: HEADPHONES.to_owned(),
        }));
        assert!(events.contains(&BackendEvent::OutputsChanged));
        assert_eq!(playing(&backend), [SPEAKERS]);
        assert_eq!(
            backend.driver().renderers_running(),
            1,
            "its renderer stopped"
        );
        assert_eq!(
            backend.driver().renderers_faded(),
            0,
            "at once: a device that is gone has nothing to fade (S185)"
        );
        assert!(backend.policy().settings(HEADPHONES).enabled, "it stays on");
        assert!(backend.poll().is_empty(), "reported once");

        backend.connect_output(HEADPHONES, "Headphones");
        assert_eq!(backend.poll(), [BackendEvent::OutputsChanged]);
        assert_eq!(playing(&backend), [HEADPHONES, SPEAKERS]);
        assert_eq!(
            backend.driver().renderers_started(),
            [HEADPHONES, SPEAKERS, HEADPHONES],
            "started again"
        );
    }

    #[test]
    fn an_output_the_ui_saw_disappear_is_lost_at_the_next_poll() {
        let mut backend = backend();
        backend.start(&SourceId::WholeSystem).unwrap();
        backend.disconnect_output_unnoticed(HEADPHONES);
        let outputs = backend.list_outputs().unwrap();
        assert!(outputs.iter().all(|output| output.id != HEADPHONES));
        assert_eq!(
            backend.poll(),
            [BackendEvent::OutputLost {
                id: HEADPHONES.to_owned(),
            }]
        );
        assert_eq!(playing(&backend), [SPEAKERS]);
    }

    #[test]
    fn an_output_lost_while_still_listed_is_lost_again_once_the_listing_drops_it() {
        let mut backend = backend();
        backend.start(&SourceId::WholeSystem).unwrap();
        // Its stream dies first, and opening it again fails as if the device
        // were gone, while Windows still lists it.
        backend.end_renderer(HEADPHONES);
        backend.fail_output(HEADPHONES, Some(BackendError::DeviceGone));
        let lost = BackendEvent::OutputLost {
            id: HEADPHONES.to_owned(),
        };
        assert_eq!(backend.poll(), std::slice::from_ref(&lost));
        assert_eq!(playing(&backend), [SPEAKERS]);
        assert!(backend.poll().is_empty(), "still listed: nothing new");

        // Then the listing drops it: now it is gone for the user too.
        backend.disconnect_output_unnoticed(HEADPHONES);
        let events = backend.poll();
        assert!(events.contains(&lost), "{events:?}");
        assert!(events.contains(&BackendEvent::OutputsChanged));
        assert!(backend.policy().settings(HEADPHONES).enabled, "it stays on");
        assert!(backend.poll().is_empty(), "reported once");

        backend.fail_output(HEADPHONES, None);
        backend.connect_output(HEADPHONES, "Headphones");
        assert_eq!(backend.poll(), [BackendEvent::OutputsChanged]);
        assert_eq!(playing(&backend), [HEADPHONES, SPEAKERS]);
    }

    #[test]
    fn an_output_that_cant_start_for_a_vanishing_device_is_lost_once_the_listing_drops_it() {
        let mut backend = backend();
        backend.fail_output(HEADPHONES, Some(BackendError::DeviceGone));
        backend.start(&SourceId::WholeSystem).unwrap();
        assert!(backend.poll().is_empty());
        backend.disconnect_output_unnoticed(HEADPHONES);
        let events = backend.poll();
        assert!(
            events.contains(&BackendEvent::OutputLost {
                id: HEADPHONES.to_owned(),
            }),
            "{events:?}"
        );
        assert!(backend.poll().is_empty(), "reported once");
    }

    #[test]
    fn an_output_that_plays_again_after_a_reset_is_lost_once_when_unplugged() {
        let mut backend = backend();
        backend.start(&SourceId::WholeSystem).unwrap();
        // A format change: the stream ends and plays again a poll later.
        backend.end_renderer(HEADPHONES);
        backend.fail_output(HEADPHONES, Some(BackendError::DeviceGone));
        backend.poll();
        backend.fail_output(HEADPHONES, None);
        backend.poll();
        assert_eq!(playing(&backend), [HEADPHONES, SPEAKERS]);
        // Unplugged now, it is lost as any playing output is: once.
        backend.disconnect_output_unnoticed(HEADPHONES);
        let lost: Vec<BackendEvent> = backend
            .poll()
            .into_iter()
            .filter(|event| matches!(event, BackendEvent::OutputLost { .. }))
            .collect();
        assert_eq!(
            lost,
            [BackendEvent::OutputLost {
                id: HEADPHONES.to_owned(),
            }]
        );
    }

    fn listing_error() -> BackendError {
        BackendError::System {
            call: "IMMDeviceEnumerator::EnumAudioEndpoints",
            code: 0x8000_4005_u32 as i32,
        }
    }

    #[test]
    fn a_failed_listing_is_tried_again_without_a_device_change() {
        let mut backend = MockBackend::empty();
        backend.connect_output(SPEAKERS, "Speakers");
        backend.set_output_enabled(SPEAKERS, true).unwrap();
        backend.fail_listing(Some(listing_error()));
        assert_eq!(backend.list_outputs(), Err(listing_error()));
        backend.start(&SourceId::WholeSystem).unwrap();
        assert!(playing(&backend).is_empty(), "nothing listed yet");

        // Listing works again, and no device changed since.
        backend.fail_listing(None);
        assert_eq!(backend.poll(), [BackendEvent::OutputsChanged]);
        assert_eq!(playing(&backend), [SPEAKERS]);
    }

    #[test]
    fn a_failed_listing_keeps_the_last_list_and_the_outputs_that_play() {
        let mut backend = backend();
        backend.start(&SourceId::WholeSystem).unwrap();
        backend.fail_listing(Some(listing_error()));
        backend.set_default_output(SPEAKERS);
        // The speakers, now the default output, play the whole system
        // straight too; the double-audio tests cover that report.
        let mut events = backend.poll();
        events.retain(|event| !matches!(event, BackendEvent::HeardTwice { .. }));
        assert!(events.is_empty(), "{events:?}");
        assert_eq!(playing(&backend), [HEADPHONES, SPEAKERS]);

        backend.fail_listing(None);
        assert_eq!(backend.poll(), [BackendEvent::OutputsChanged]);
        let outputs = backend.list_outputs().unwrap();
        assert!(
            outputs
                .iter()
                .any(|output| output.id == SPEAKERS && output.is_default)
        );
        assert_eq!(playing(&backend), [HEADPHONES, SPEAKERS]);
    }

    #[test]
    fn a_device_that_vanishes_while_starting_stays_on() {
        let mut backend = backend();
        backend.fail_output(HEADPHONES, Some(BackendError::DeviceGone));
        backend.start(&SourceId::WholeSystem).unwrap();
        assert!(backend.poll().is_empty());
        assert_eq!(playing(&backend), [SPEAKERS]);
        assert!(backend.policy().settings(HEADPHONES).enabled);
        backend.fail_output(HEADPHONES, None);
        backend.poll();
        assert_eq!(playing(&backend), [HEADPHONES, SPEAKERS]);
    }

    #[test]
    fn an_output_whose_device_another_app_holds_stays_on_and_plays_once_free() {
        let mut backend = backend();
        backend.fail_output(HEADPHONES, Some(BackendError::DeviceBusy));
        backend.start(&SourceId::WholeSystem).unwrap();
        assert_eq!(
            backend.poll(),
            [BackendEvent::OutputBusy {
                id: HEADPHONES.to_owned(),
            }]
        );
        assert!(
            backend.policy().settings(HEADPHONES).enabled,
            "it stays on, as the user chose"
        );
        assert_eq!(playing(&backend), [SPEAKERS]);
        // Tried again every few seconds, and told once.
        poll_quietly(&mut backend, 2 * BUSY_RETRY_POLLS);
        assert_eq!(playing(&backend), [SPEAKERS]);
        backend.fail_output(HEADPHONES, None);
        // It plays at its next try.
        let events: Vec<BackendEvent> =
            (0..BUSY_RETRY_POLLS).flat_map(|_| backend.poll()).collect();
        assert_eq!(
            events,
            [BackendEvent::OutputFree {
                id: HEADPHONES.to_owned(),
            }]
        );
        assert_eq!(playing(&backend), [HEADPHONES, SPEAKERS]);
        assert!(backend.poll().is_empty());
    }

    #[test]
    fn a_busy_output_is_tried_again_every_few_seconds_not_at_every_poll() {
        let mut backend = backend();
        backend.fail_output(HEADPHONES, Some(BackendError::DeviceBusy));
        backend.start(&SourceId::WholeSystem).unwrap();
        assert_eq!(backend.driver().renderer_attempts(HEADPHONES), 1);
        // Thirty seconds at the UI's two polls a second: each try starts
        // an output thread and opens the device, so one about every three
        // seconds, not one at every poll.
        for _ in 0..60 {
            backend.poll();
        }
        let attempts = backend.driver().renderer_attempts(HEADPHONES);
        assert!((10..=11).contains(&attempts), "{attempts} tries");
        assert!(backend.policy().settings(HEADPHONES).enabled);
        assert_eq!(playing(&backend), [SPEAKERS]);
        // Once free, it plays within about three seconds.
        backend.fail_output(HEADPHONES, None);
        let mut polls = 0;
        while !playing(&backend).iter().any(|id| id == HEADPHONES) {
            assert!(polls < 6, "not playing after {polls} polls");
            backend.poll();
            polls += 1;
        }
    }

    #[test]
    fn an_output_taken_over_by_another_app_while_playing_waits_for_it() {
        let mut backend = backend();
        backend.start(&SourceId::WholeSystem).unwrap();
        // Windows ends a shared stream when another app takes the device
        // in exclusive mode; opening it again then finds it in use.
        backend.fail_output(HEADPHONES, Some(BackendError::DeviceBusy));
        backend.end_renderer(HEADPHONES);
        let events = backend.poll();
        assert!(
            events.contains(&BackendEvent::OutputBusy {
                id: HEADPHONES.to_owned(),
            }),
            "{events:?}"
        );
        assert!(backend.policy().settings(HEADPHONES).enabled);
        assert_eq!(playing(&backend), [SPEAKERS]);
        backend.fail_output(HEADPHONES, None);
        // It plays at its next try.
        for _ in 0..BUSY_RETRY_POLLS {
            backend.poll();
        }
        assert_eq!(playing(&backend), [HEADPHONES, SPEAKERS]);
    }

    #[test]
    fn turning_on_an_output_whose_device_is_busy_keeps_it_on() {
        let mut backend = backend();
        backend.set_output_enabled(HEADPHONES, false).unwrap();
        backend.start(&SourceId::WholeSystem).unwrap();
        backend.fail_output(HEADPHONES, Some(BackendError::DeviceBusy));
        assert_eq!(backend.set_output_enabled(HEADPHONES, true), Ok(()));
        assert!(backend.policy().settings(HEADPHONES).enabled);
        assert_eq!(
            backend.poll(),
            [BackendEvent::OutputBusy {
                id: HEADPHONES.to_owned(),
            }]
        );
        // Switched off while it waits, it no longer does.
        backend.set_output_enabled(HEADPHONES, false).unwrap();
        assert_eq!(
            backend.poll(),
            [BackendEvent::OutputFree {
                id: HEADPHONES.to_owned(),
            }]
        );
        backend.fail_output(HEADPHONES, None);
        backend.poll();
        assert_eq!(playing(&backend), [SPEAKERS]);
    }

    #[test]
    fn stopping_ends_the_wait_for_a_busy_output() {
        let mut backend = backend();
        backend.fail_output(HEADPHONES, Some(BackendError::DeviceBusy));
        backend.start(&SourceId::WholeSystem).unwrap();
        let busy = BackendEvent::OutputBusy {
            id: HEADPHONES.to_owned(),
        };
        assert_eq!(backend.poll(), std::slice::from_ref(&busy));
        backend.stop();
        assert_eq!(
            backend.poll(),
            [BackendEvent::OutputFree {
                id: HEADPHONES.to_owned(),
            }]
        );
        assert!(backend.policy().settings(HEADPHONES).enabled, "it stays on");
        // Started again while the other app still holds it: told again.
        backend.start(&SourceId::WholeSystem).unwrap();
        assert_eq!(backend.poll(), [busy]);
    }

    #[test]
    fn a_session_that_ends_by_itself_ends_the_wait_for_a_busy_output() {
        let mut backend = backend();
        backend.fail_output(HEADPHONES, Some(BackendError::DeviceBusy));
        backend
            .start(&SourceId::App {
                root_pid: PLAYER_PID,
            })
            .unwrap();
        backend.poll();
        backend.quit_app(PLAYER_PID);
        let events = backend.poll();
        assert!(
            events.contains(&BackendEvent::Stopped {
                reason: StopReason::SourceQuit,
            }),
            "{events:?}"
        );
        assert!(
            events.contains(&BackendEvent::OutputFree {
                id: HEADPHONES.to_owned(),
            }),
            "{events:?}"
        );
    }

    #[test]
    fn an_output_whose_device_doesnt_answer_stays_on_and_is_tried_again_seconds_later() {
        let mut backend = backend();
        // The last output of the pass: no other start waits behind it.
        backend.fail_output(SPEAKERS, Some(BackendError::NotResponding));
        backend.start(&SourceId::WholeSystem).unwrap();
        assert!(backend.poll().is_empty(), "no notice: it is tried again");
        assert!(backend.policy().settings(SPEAKERS).enabled, "it stays on");
        assert_eq!(playing(&backend), [HEADPHONES]);

        backend.fail_output(SPEAKERS, None);
        backend.poll();
        assert_eq!(
            backend.driver().renderer_attempts(SPEAKERS),
            1,
            "not at the next poll: a try that hangs holds the window for seconds"
        );
        for _ in 0..40 {
            backend.poll();
        }
        assert_eq!(playing(&backend), [HEADPHONES, SPEAKERS]);
        assert_eq!(backend.driver().renderer_attempts(SPEAKERS), 2);
    }

    #[test]
    fn once_a_start_doesnt_answer_the_other_starts_of_that_pass_wait_their_turn() {
        let mut backend = backend();
        backend.connect_output(USB_OUTPUT, "USB DAC");
        backend.set_output_enabled(USB_OUTPUT, true).unwrap();
        backend.list_outputs().unwrap();
        // The headphones start first, and hang: Windows audio isn't back
        // after a wake. The others would most likely hang as well, each
        // holding the window as long, so they aren't tried in that pass.
        backend.fail_output(HEADPHONES, Some(BackendError::NotResponding));
        backend.fail_output(SPEAKERS, Some(BackendError::NotResponding));
        backend.start(&SourceId::WholeSystem).unwrap();
        let attempts = |backend: &MockBackend| {
            [HEADPHONES, SPEAKERS, USB_OUTPUT].map(|id| backend.driver().renderer_attempts(id))
        };
        assert_eq!(attempts(&backend), [1, 0, 0]);
        poll_quietly(&mut backend, STALL_RETRY_POLLS - 1);
        assert_eq!(attempts(&backend), [1, 0, 0], "they wait like it");
        for id in [HEADPHONES, SPEAKERS, USB_OUTPUT] {
            assert!(backend.policy().settings(id).enabled, "{id} stays on");
        }

        // At the next pass the outputs not tried yet go first: the speakers
        // hang now, so the USB DAC waits again, and so do the headphones.
        backend.poll();
        assert_eq!(attempts(&backend), [1, 1, 0]);
        // Windows audio answers again: at the next pass the USB DAC, never
        // tried, goes first, and every output plays.
        backend.fail_output(HEADPHONES, None);
        backend.fail_output(SPEAKERS, None);
        poll_quietly(&mut backend, STALL_RETRY_POLLS);
        assert_eq!(attempts(&backend), [2, 2, 1]);
        assert_eq!(playing(&backend), [HEADPHONES, SPEAKERS, USB_OUTPUT]);
    }

    #[test]
    fn a_healthy_output_isnt_kept_waiting_behind_one_that_keeps_hanging() {
        let mut backend = backend();
        backend.fail_output(HEADPHONES, Some(BackendError::NotResponding));
        backend.start(&SourceId::WholeSystem).unwrap();
        assert_eq!(backend.driver().renderer_attempts(SPEAKERS), 0);
        // At the next pass the speakers, which didn't hang, go before the
        // headphones, which still do.
        poll_quietly(&mut backend, STALL_RETRY_POLLS);
        assert_eq!(playing(&backend), [SPEAKERS]);
        assert_eq!(backend.driver().renderer_attempts(HEADPHONES), 2);
    }

    #[test]
    fn an_output_that_answers_again_isnt_starved_behind_one_that_still_hangs() {
        let mut backend = backend();
        // After a wake both hang: the headphones at the start, the speakers
        // at the next pass, so both have hung once.
        backend.fail_output(HEADPHONES, Some(BackendError::NotResponding));
        backend.fail_output(SPEAKERS, Some(BackendError::NotResponding));
        backend.start(&SourceId::WholeSystem).unwrap();
        poll_quietly(&mut backend, STALL_RETRY_POLLS);
        // Windows answers for the speakers again; the headphones still hang.
        backend.fail_output(SPEAKERS, None);
        poll_quietly(&mut backend, 3 * STALL_RETRY_POLLS);
        assert_eq!(playing(&backend), [SPEAKERS]);
    }

    #[test]
    fn stopping_ends_the_wait_for_an_output_that_didnt_answer() {
        let mut backend = backend();
        backend.fail_output(SPEAKERS, Some(BackendError::NotResponding));
        backend.start(&SourceId::WholeSystem).unwrap();
        backend.poll();
        backend.stop();
        backend.poll();
        // Windows audio answers again: started again, it is tried at once.
        backend.fail_output(SPEAKERS, None);
        backend.start(&SourceId::WholeSystem).unwrap();
        backend.poll();
        assert_eq!(backend.driver().renderer_attempts(SPEAKERS), 2);
        assert_eq!(playing(&backend), [HEADPHONES, SPEAKERS]);
    }

    #[test]
    fn an_output_whose_listed_device_keeps_refusing_to_open_is_turned_off_after_a_while() {
        let mut backend = backend();
        // Windows still lists it but keeps refusing to open it, as after a
        // wake while it rebuilds the endpoint, only for good.
        backend.fail_output(HEADPHONES, Some(BackendError::DeviceGone));
        backend.start(&SourceId::WholeSystem).unwrap();
        poll_quietly(&mut backend, REFUSED_GIVE_UP_POLLS - 1);
        assert!(backend.policy().settings(HEADPHONES).enabled);
        let tries = backend.driver().renderer_attempts(HEADPHONES);
        assert_eq!(tries, REFUSED_GIVE_UP_POLLS as usize, "a try at every poll");
        // About thirty seconds on, it is a start that failed (S69): turned
        // off, with the notice, and no longer tried.
        assert_eq!(
            backend.poll(),
            [BackendEvent::OutputFailed {
                id: HEADPHONES.to_owned(),
                error: BackendError::DeviceGone,
            }]
        );
        assert!(!backend.policy().settings(HEADPHONES).enabled);
        poll_quietly(&mut backend, STALL_RETRY_POLLS);
        assert_eq!(backend.driver().renderer_attempts(HEADPHONES), tries + 1);
        assert_eq!(playing(&backend), [SPEAKERS]);
    }

    #[test]
    fn an_output_refused_for_a_moment_gets_the_whole_wait_again_next_time() {
        let mut backend = backend();
        backend.fail_output(HEADPHONES, Some(BackendError::DeviceGone));
        backend.start(&SourceId::WholeSystem).unwrap();
        poll_quietly(&mut backend, REFUSED_GIVE_UP_POLLS - 2);
        // It opens after all, and plays.
        backend.fail_output(HEADPHONES, None);
        poll_quietly(&mut backend, 1);
        assert_eq!(playing(&backend), [HEADPHONES, SPEAKERS]);
        // Its stream restarts later (a format change), refused again for a
        // moment: as long a wait as the first time.
        backend.fail_output(HEADPHONES, Some(BackendError::DeviceGone));
        backend.end_renderer(HEADPHONES);
        let lost = BackendEvent::OutputLost {
            id: HEADPHONES.to_owned(),
        };
        assert_eq!(backend.poll(), [lost]);
        poll_quietly(&mut backend, REFUSED_GIVE_UP_POLLS - 1);
        assert!(backend.policy().settings(HEADPHONES).enabled);
    }

    #[test]
    fn a_busy_output_that_then_fails_another_way_is_told_busy_again_later() {
        let mut backend = backend();
        backend.fail_output(HEADPHONES, Some(BackendError::DeviceBusy));
        backend.start(&SourceId::WholeSystem).unwrap();
        let busy = BackendEvent::OutputBusy {
            id: HEADPHONES.to_owned(),
        };
        assert_eq!(backend.poll(), std::slice::from_ref(&busy));
        // At its next try the device fails another way: turned off, it no
        // longer waits for the other app.
        backend.fail_output(HEADPHONES, Some(BackendError::UnsupportedFormat));
        let events: Vec<BackendEvent> =
            (0..BUSY_RETRY_POLLS).flat_map(|_| backend.poll()).collect();
        assert_eq!(
            events,
            [
                BackendEvent::OutputFree {
                    id: HEADPHONES.to_owned(),
                },
                BackendEvent::OutputFailed {
                    id: HEADPHONES.to_owned(),
                    error: BackendError::UnsupportedFormat,
                },
            ]
        );
        assert!(!backend.policy().settings(HEADPHONES).enabled);
        // Turned on again while an app holds it once more: told again.
        backend.fail_output(HEADPHONES, Some(BackendError::DeviceBusy));
        assert_eq!(backend.set_output_enabled(HEADPHONES, true), Ok(()));
        assert_eq!(backend.poll(), [busy]);
    }

    #[test]
    fn a_busy_output_whose_device_goes_away_no_longer_waits() {
        let mut backend = backend();
        backend.fail_output(HEADPHONES, Some(BackendError::DeviceBusy));
        backend.start(&SourceId::WholeSystem).unwrap();
        backend.poll();
        backend.disconnect_output(HEADPHONES);
        let events = backend.poll();
        assert!(
            events.contains(&BackendEvent::OutputFree {
                id: HEADPHONES.to_owned(),
            }),
            "{events:?}"
        );
        assert!(
            backend.policy().settings(HEADPHONES).enabled,
            "it stays on (S68)"
        );
        // Back and still held by the other app: told again.
        backend.connect_output(HEADPHONES, "Headphones");
        let events = backend.poll();
        assert!(
            events.contains(&BackendEvent::OutputBusy {
                id: HEADPHONES.to_owned(),
            }),
            "{events:?}"
        );
    }

    #[test]
    fn an_output_that_cant_start_is_turned_off_and_reported() {
        let mut backend = backend();
        let error = BackendError::System {
            call: "IAudioClient::Initialize",
            code: 0x8889_0008_u32 as i32,
        };
        backend.fail_output(HEADPHONES, Some(error.clone()));
        backend.start(&SourceId::WholeSystem).unwrap();
        assert_eq!(
            backend.poll(),
            [BackendEvent::OutputFailed {
                id: HEADPHONES.to_owned(),
                error: error.clone(),
            }]
        );
        assert!(!backend.policy().settings(HEADPHONES).enabled);
        assert_eq!(playing(&backend), [SPEAKERS]);
        assert!(backend.poll().is_empty(), "not retried");
    }

    #[test]
    fn turning_on_an_output_that_cant_start_returns_the_error() {
        let mut backend = backend();
        backend.set_output_enabled(HEADPHONES, false).unwrap();
        backend.start(&SourceId::WholeSystem).unwrap();
        backend.fail_output(HEADPHONES, Some(BackendError::UnsupportedFormat));
        assert_eq!(
            backend.set_output_enabled(HEADPHONES, true),
            Err(BackendError::UnsupportedFormat)
        );
        assert!(!backend.policy().settings(HEADPHONES).enabled);
        assert!(
            backend.poll().is_empty(),
            "the caller already has the error"
        );
    }

    #[test]
    fn an_output_that_stops_while_its_device_stays_plays_again_unreported() {
        let mut backend = backend();
        backend.start(&SourceId::WholeSystem).unwrap();
        backend.end_renderer(SPEAKERS);
        assert!(backend.poll().is_empty());
        assert_eq!(playing(&backend), [HEADPHONES, SPEAKERS]);
        assert_eq!(
            backend.driver().renderers_started(),
            [HEADPHONES, SPEAKERS, SPEAKERS]
        );
    }

    #[test]
    fn the_session_ends_when_the_captured_app_quits() {
        let mut backend = backend();
        backend
            .start(&SourceId::App {
                root_pid: PLAYER_PID,
            })
            .unwrap();
        backend.quit_app(PLAYER_PID);
        assert_eq!(
            backend.poll(),
            [BackendEvent::Stopped {
                reason: StopReason::SourceQuit,
            }]
        );
        assert_eq!(backend.running_source(), None);
        assert!(playing(&backend).is_empty());
    }

    #[test]
    fn the_session_ends_when_capture_fails() {
        let mut backend = backend();
        backend.start(&SourceId::WholeSystem).unwrap();
        let error = BackendError::System {
            call: "IAudioCaptureClient::GetBuffer",
            code: 0x8000_4005_u32 as i32,
        };
        backend.fail_capture(error.clone());
        assert_eq!(
            backend.poll(),
            [BackendEvent::Stopped {
                reason: StopReason::CaptureFailed(error),
            }]
        );
        assert_eq!(backend.running_source(), None);
        backend.poll();
        assert_eq!(backend.driver().captures_started().len(), 1, "no restart");
    }

    /// Polls `count` times, expecting no event.
    fn poll_quietly(backend: &mut MockBackend, count: u32) {
        for _ in 0..count {
            let events = backend.poll();
            assert!(events.is_empty(), "{events:?}");
        }
    }

    #[test]
    fn a_capture_lost_to_a_windows_audio_restart_starts_again_a_moment_later() {
        let mut backend = backend();
        backend.start(&SourceId::WholeSystem).unwrap();
        backend.fail_capture(BackendError::DeviceGone);
        poll_quietly(&mut backend, 1);
        assert_eq!(
            backend.running_source(),
            Some(SourceId::WholeSystem),
            "streaming stays on"
        );
        assert!(playing(&backend).is_empty());
        poll_quietly(&mut backend, RESTART_DELAY_POLLS - 1);
        assert_eq!(backend.driver().captures_started().len(), 1, "not yet");
        poll_quietly(&mut backend, 1);
        assert_eq!(backend.driver().captures_started().len(), 2);
        assert!(backend.driver().capture_running());
        assert_eq!(playing(&backend), [HEADPHONES, SPEAKERS]);
        assert_eq!(backend.running_source(), Some(SourceId::WholeSystem));
    }

    #[test]
    fn input_devices_are_listed_after_the_apps() {
        let mut backend = backend();
        backend.connect_input("mic", "Microphone", "USB");
        let ids: Vec<SourceId> = backend
            .list_sources()
            .unwrap()
            .into_iter()
            .map(|source| source.id)
            .collect();
        assert_eq!(
            ids,
            [
                SourceId::WholeSystem,
                SourceId::App {
                    root_pid: PLAYER_PID
                },
                SourceId::Input {
                    endpoint_id: "mic".to_owned()
                },
            ]
        );
    }

    #[test]
    fn an_input_device_unplugged_while_streaming_stops_streaming_once_the_restart_tries_run_out() {
        let mut backend = backend();
        backend.connect_input("mic", "Microphone", "USB");
        let source = SourceId::Input {
            endpoint_id: "mic".to_owned(),
        };
        backend.start(&source).unwrap();
        assert_eq!(
            backend.driver().captures_started(),
            [CaptureTarget::Input {
                endpoint_id: "mic".to_owned()
            }]
        );
        assert_eq!(playing(&backend), [HEADPHONES, SPEAKERS]);
        backend.disconnect_input("mic");
        // The stream fails as one lost to a Windows audio restart does, and
        // is tried again as that one is: a USB device can be inactive for a
        // moment after a wake.
        poll_quietly(&mut backend, 5 * RESTART_DELAY_POLLS);
        assert_eq!(
            backend.poll(),
            [BackendEvent::Stopped {
                reason: StopReason::CaptureFailed(BackendError::InputGone),
            }]
        );
        assert_eq!(backend.driver().capture_attempts(), 1 + 5);
        assert_eq!(backend.running_source(), None);
        assert!(playing(&backend).is_empty());
    }

    #[test]
    fn an_input_device_back_within_the_restart_tries_is_captured_again() {
        let mut backend = backend();
        backend.connect_input("mic", "Microphone", "USB");
        let source = SourceId::Input {
            endpoint_id: "mic".to_owned(),
        };
        backend.start(&source).unwrap();
        backend.disconnect_input("mic");
        poll_quietly(&mut backend, 2 * RESTART_DELAY_POLLS);
        assert_eq!(backend.driver().capture_attempts(), 1 + 1, "one try failed");
        backend.connect_input("mic", "Microphone", "USB");
        poll_quietly(&mut backend, RESTART_DELAY_POLLS);
        assert_eq!(backend.driver().capture_attempts(), 1 + 2);
        assert!(backend.driver().capture_running());
        assert_eq!(backend.running_source(), Some(source));
        assert_eq!(playing(&backend), [HEADPHONES, SPEAKERS]);
    }

    #[test]
    fn a_failed_input_listing_leaves_the_inputs_out_and_keeps_the_apps() {
        let mut backend = backend();
        backend.connect_input("mic", "Microphone", "USB");
        backend.fail_input_listing(Some(BackendError::System {
            call: "IMMDeviceEnumerator::EnumAudioEndpoints",
            code: 1,
        }));
        let ids = |backend: &mut MockBackend| -> Vec<SourceId> {
            backend
                .list_sources()
                .expect("the apps are listed")
                .into_iter()
                .map(|source| source.id)
                .collect()
        };
        let apps = [
            SourceId::WholeSystem,
            SourceId::App {
                root_pid: PLAYER_PID,
            },
        ];
        assert_eq!(ids(&mut backend), apps);
        assert_eq!(ids(&mut backend), apps, "every refresh");
        backend.fail_input_listing(None);
        assert_eq!(ids(&mut backend).len(), 3, "the input is back");
    }

    #[test]
    fn a_microphone_blocked_by_windows_privacy_settings_isnt_tried_again() {
        assert!(!may_start_later(&BackendError::MicrophoneBlocked));
        assert!(may_start_later(&BackendError::InputGone));
    }

    #[test]
    fn a_capture_that_cant_start_again_after_windows_audio_restarted_stops_streaming() {
        let mut backend = backend();
        backend.start(&SourceId::WholeSystem).unwrap();
        backend.fail_capture(BackendError::DeviceGone);
        backend.refuse_captures(Some(BackendError::DeviceGone));
        // Five tries a second apart, the first a second after the loss.
        poll_quietly(&mut backend, 5 * RESTART_DELAY_POLLS);
        assert_eq!(
            backend.poll(),
            [BackendEvent::Stopped {
                reason: StopReason::CaptureFailed(BackendError::DeviceGone),
            }]
        );
        assert_eq!(backend.driver().capture_attempts(), 1 + 5);
        assert_eq!(backend.running_source(), None);
        assert!(playing(&backend).is_empty());
        backend.refuse_captures(None);
        poll_quietly(&mut backend, RESTART_DELAY_POLLS + 1);
        assert_eq!(backend.running_source(), None, "no try once it stopped");
    }

    #[test]
    fn a_capture_restart_that_times_out_is_the_last_try() {
        let mut backend = backend();
        backend.start(&SourceId::WholeSystem).unwrap();
        backend.fail_capture(BackendError::DeviceGone);
        // Windows never completed the activation: each try held the UI
        // thread for the whole wait, so there is no other.
        let timed_out = BackendError::System {
            call: "ActivateAudioInterfaceAsync",
            code: crate::audio::backend::TIMED_OUT,
        };
        backend.refuse_captures(Some(timed_out.clone()));
        poll_quietly(&mut backend, RESTART_DELAY_POLLS);
        assert_eq!(
            backend.poll(),
            [BackendEvent::Stopped {
                reason: StopReason::CaptureFailed(timed_out),
            }]
        );
        assert_eq!(backend.driver().capture_attempts(), 1 + 1);
        assert_eq!(backend.running_source(), None);
        // Another Windows error is still tried again.
        backend.refuse_captures(None);
        backend.start(&SourceId::WholeSystem).unwrap();
        backend.fail_capture(BackendError::DeviceGone);
        backend.refuse_captures(Some(BackendError::System {
            call: "IAudioClient::Initialize",
            code: -1,
        }));
        poll_quietly(&mut backend, 1 + 2 * RESTART_DELAY_POLLS);
        assert_eq!(backend.running_source(), Some(SourceId::WholeSystem));
    }

    #[test]
    fn a_capture_restart_whose_thread_doesnt_answer_is_the_last_try() {
        let mut backend = backend();
        backend.start(&SourceId::WholeSystem).unwrap();
        backend.fail_capture(BackendError::DeviceGone);
        // The capture thread didn't answer within its wait: that try held
        // the UI thread for the whole wait, so there is no other.
        backend.refuse_captures(Some(BackendError::NotResponding));
        poll_quietly(&mut backend, RESTART_DELAY_POLLS);
        assert_eq!(
            backend.poll(),
            [BackendEvent::Stopped {
                reason: StopReason::CaptureFailed(BackendError::NotResponding),
            }]
        );
        assert_eq!(backend.driver().capture_attempts(), 1 + 1);
        assert_eq!(backend.running_source(), None);
    }

    #[test]
    fn a_capture_that_cant_start_again_at_first_is_tried_again_for_a_few_seconds() {
        let mut backend = backend();
        backend.start(&SourceId::WholeSystem).unwrap();
        backend.fail_capture(BackendError::DeviceGone);
        backend.refuse_captures(Some(BackendError::DeviceGone));
        // Windows audio takes a few seconds to come back: two tries fail.
        poll_quietly(&mut backend, 1 + 2 * RESTART_DELAY_POLLS);
        assert_eq!(backend.driver().capture_attempts(), 1 + 2);
        assert_eq!(
            backend.running_source(),
            Some(SourceId::WholeSystem),
            "streaming stays on"
        );
        backend.refuse_captures(None);
        poll_quietly(&mut backend, RESTART_DELAY_POLLS);
        assert_eq!(backend.driver().captures_started().len(), 2);
        assert!(backend.driver().capture_running());
        assert_eq!(playing(&backend), [HEADPHONES, SPEAKERS]);
        // It has the trial of any capture that started again.
        poll_quietly(&mut backend, 3);
        backend.fail_capture(BackendError::DeviceGone);
        assert_eq!(
            backend.poll(),
            [BackendEvent::Stopped {
                reason: StopReason::CaptureFailed(BackendError::DeviceGone),
            }]
        );
    }

    #[test]
    fn an_app_that_quit_while_its_capture_waits_to_start_again_stops_streaming_at_once() {
        let mut backend = backend();
        backend
            .start(&SourceId::App {
                root_pid: PLAYER_PID,
            })
            .unwrap();
        backend.fail_capture(BackendError::DeviceGone);
        poll_quietly(&mut backend, 1);
        backend.quit_app(PLAYER_PID);
        poll_quietly(&mut backend, RESTART_DELAY_POLLS - 1);
        assert_eq!(
            backend.poll(),
            [BackendEvent::Stopped {
                reason: StopReason::CaptureFailed(BackendError::SourceGone),
            }]
        );
        assert_eq!(backend.running_source(), None);
    }

    #[test]
    fn a_restarted_capture_that_fails_again_soon_stops_streaming() {
        let mut backend = backend();
        backend.start(&SourceId::WholeSystem).unwrap();
        backend.fail_capture(BackendError::DeviceGone);
        poll_quietly(&mut backend, RESTART_DELAY_POLLS + 1);
        assert_eq!(backend.driver().captures_started().len(), 2);
        // A second or two later: within its trial.
        poll_quietly(&mut backend, 3);
        backend.fail_capture(BackendError::DeviceGone);
        assert_eq!(
            backend.poll(),
            [BackendEvent::Stopped {
                reason: StopReason::CaptureFailed(BackendError::DeviceGone),
            }]
        );
        assert_eq!(backend.running_source(), None);
    }

    #[test]
    fn a_restarted_capture_that_ran_a_while_is_restarted_again_after_the_next_loss() {
        let mut backend = backend();
        backend.start(&SourceId::WholeSystem).unwrap();
        backend.fail_capture(BackendError::DeviceGone);
        poll_quietly(&mut backend, RESTART_DELAY_POLLS + 1);
        poll_quietly(&mut backend, RESTART_TRIAL_POLLS);
        backend.fail_capture(BackendError::DeviceGone);
        poll_quietly(&mut backend, RESTART_DELAY_POLLS + 1);
        assert_eq!(backend.driver().captures_started().len(), 3);
        assert_eq!(playing(&backend), [HEADPHONES, SPEAKERS]);
    }

    #[test]
    fn a_stop_while_the_capture_waits_to_restart_cancels_the_restart() {
        let mut backend = backend();
        backend
            .start(&SourceId::App {
                root_pid: PLAYER_PID,
            })
            .unwrap();
        backend.fail_capture(BackendError::DeviceGone);
        poll_quietly(&mut backend, 1);
        backend.stop();
        poll_quietly(&mut backend, RESTART_DELAY_POLLS + 1);
        assert_eq!(backend.driver().captures_started().len(), 1);
        assert_eq!(backend.running_source(), None);
    }

    #[test]
    fn an_app_that_quits_ends_the_session_without_a_restart() {
        let mut backend = backend();
        backend
            .start(&SourceId::App {
                root_pid: PLAYER_PID,
            })
            .unwrap();
        // A capture whose app quit reports that first, whatever its stream
        // says (EC6).
        backend.quit_app(PLAYER_PID);
        assert_eq!(
            backend.poll(),
            [BackendEvent::Stopped {
                reason: StopReason::SourceQuit,
            }]
        );
        poll_quietly(&mut backend, RESTART_DELAY_POLLS + 1);
        assert_eq!(backend.driver().captures_started().len(), 1);
    }

    #[test]
    fn an_app_that_quits_gives_way_to_the_whole_system_when_set_and_airplay_plays_on() {
        let (mut backend, fake, kitchen) = with_airplay();
        let id = id_of(&kitchen);
        backend.set_airplay_enabled(&id, true).unwrap();
        backend.set_when_source_quits(WhenSourceQuits::SwitchToSystem);
        backend
            .start(&SourceId::App {
                root_pid: PLAYER_PID,
            })
            .unwrap();
        fake.take_calls();

        backend.quit_app(PLAYER_PID);
        assert_eq!(backend.poll(), [BackendEvent::SwitchedToSystem]);
        poll_quietly(&mut backend, 1);
        assert_eq!(backend.running_source(), Some(SourceId::WholeSystem));
        assert_eq!(backend.driver().captures_started().len(), 2);
        assert!(backend.driver().capture_running());
        assert_eq!(playing(&backend), [HEADPHONES, SPEAKERS]);
        assert!(
            !fake
                .take_calls()
                .iter()
                .any(|call| matches!(call, Call::Remove(_))),
            "the AirPlay speaker stays in the stream"
        );
        assert_eq!(backend.enabled_airplay(), [id]);

        // The app starting again changes nothing (S227 c).
        backend.add_app(PLAYER_PID + 1, "player.exe");
        poll_quietly(&mut backend, 2);
        assert_eq!(backend.running_source(), Some(SourceId::WholeSystem));
    }

    #[test]
    fn an_app_that_quit_while_its_capture_waits_to_start_again_gives_way_when_set() {
        let mut backend = backend();
        backend.set_when_source_quits(WhenSourceQuits::SwitchToSystem);
        backend
            .start(&SourceId::App {
                root_pid: PLAYER_PID,
            })
            .unwrap();
        backend.fail_capture(BackendError::DeviceGone);
        poll_quietly(&mut backend, 1);
        backend.quit_app(PLAYER_PID);
        poll_quietly(&mut backend, RESTART_DELAY_POLLS - 1);
        assert_eq!(backend.poll(), [BackendEvent::SwitchedToSystem]);
        assert_eq!(backend.running_source(), Some(SourceId::WholeSystem));
        assert_eq!(backend.driver().captures_started().len(), 2, "one rebuild");
        assert_eq!(playing(&backend), [HEADPHONES, SPEAKERS]);
    }

    #[test]
    fn an_app_picked_while_the_last_session_fades_that_quits_before_its_start_gives_way_when_set() {
        let (mut backend, fake, kitchen) = with_airplay();
        let id = id_of(&kitchen);
        backend.set_airplay_enabled(&id, true).unwrap();
        backend.set_when_source_quits(WhenSourceQuits::SwitchToSystem);
        backend.start(&SourceId::WholeSystem).unwrap();
        backend.driver().hold_fades(true);
        backend
            .start(&SourceId::App {
                root_pid: PLAYER_PID,
            })
            .unwrap();
        assert_eq!(
            backend.driver().captures_started().len(),
            1,
            "the start waits for the fade"
        );
        fake.take_calls();

        backend.quit_app(PLAYER_PID);
        backend.driver().hold_fades(false);
        assert!(backend.poll().contains(&BackendEvent::SwitchedToSystem));
        assert_eq!(backend.running_source(), Some(SourceId::WholeSystem));
        assert_eq!(backend.driver().captures_started().len(), 2);
        assert!(backend.driver().capture_running());
        assert_eq!(playing(&backend), [HEADPHONES, SPEAKERS]);
        assert!(
            !fake
                .take_calls()
                .iter()
                .any(|call| matches!(call, Call::Remove(_))),
            "the AirPlay speaker stays in the stream"
        );
        assert_eq!(backend.enabled_airplay(), [id]);
    }

    #[test]
    fn a_new_start_replaces_the_running_session() {
        let mut backend = backend();
        backend.start(&SourceId::WholeSystem).unwrap();
        backend
            .start(&SourceId::App {
                root_pid: PLAYER_PID,
            })
            .unwrap();
        assert_eq!(backend.driver().captures_started().len(), 2);
        assert_eq!(playing(&backend), [HEADPHONES, SPEAKERS]);
        assert_eq!(backend.driver().renderers_faded(), 2);
    }

    #[test]
    fn frames_captured_count_from_each_start() {
        let mut backend = backend();
        assert_eq!(backend.frames_captured(), 0);
        backend.start(&SourceId::WholeSystem).unwrap();
        assert_eq!(backend.frames_captured(), 0, "nothing played yet");
        backend.feed_capture(480);
        assert_eq!(backend.frames_captured(), 480);
        backend.start(&SourceId::WholeSystem).unwrap();
        assert_eq!(backend.frames_captured(), 0);
    }

    #[test]
    fn outputs_are_listed_by_name_with_their_settings() {
        let mut backend = backend();
        backend.set_default_output(SPEAKERS);
        backend.set_output_volume(SPEAKERS, 80.0);
        backend.start(&SourceId::WholeSystem).unwrap();
        let outputs = backend.list_outputs().unwrap();
        assert_eq!(outputs.len(), 2);
        assert_eq!(outputs[0].name, "Headphones");
        let speakers = &outputs[1];
        assert_eq!(speakers.id, SPEAKERS);
        assert!(speakers.is_default);
        assert!(speakers.enabled);
        assert!(speakers.playing);
        assert_eq!(speakers.volume, 80.0);
        assert_eq!(speakers.delay_ms, 0.0);
    }

    #[test]
    fn a_new_default_output_is_announced() {
        let mut backend = backend();
        backend.list_outputs().unwrap();
        backend.set_default_output(HEADPHONES);
        assert_eq!(backend.poll(), [BackendEvent::OutputsChanged]);
        assert!(backend.poll().is_empty());
    }

    #[test]
    fn sources_list_the_whole_system_then_apps() {
        let mut backend = backend();
        let sources = backend.list_sources().unwrap();
        let names: Vec<&str> = sources.iter().map(|source| source.name.as_str()).collect();
        assert_eq!(names, ["All System Audio", "Player"]);
    }

    #[test]
    fn settings_made_while_stopped_are_kept_for_the_start() {
        let mut backend = MockBackend::empty();
        backend.connect_output("first", "First");
        backend.set_output_enabled("first", true).unwrap();
        backend.set_output_enabled("later", true).unwrap();
        backend.set_output_delay("later", 40.0);
        backend.start(&SourceId::WholeSystem).unwrap();
        assert_eq!(playing(&backend), ["first"], "later isn't connected yet");
        backend.connect_output("later", "Later");
        backend.poll();
        assert_eq!(playing(&backend), ["first", "later"]);
        // 40 ms after the output at 0.
        assert_eq!(backend.playing_params("later").unwrap().delay_frames, 1_920);
    }

    #[test]
    fn the_equalizer_set_while_stopped_applies_at_the_start_then_live() {
        let mut backend = backend();
        backend.set_equalizer(EqSettings {
            enabled: true,
            preamp_db: 6.0,
            ..EqSettings::default()
        });
        backend.start(&SourceId::WholeSystem).unwrap();
        let raised = 0.25 * 10f32.powf(6.0 / 20.0);
        let out = backend.feed_capture_level(4, 0.25);
        assert_eq!(out.len(), 8);
        assert!(out.iter().all(|s| (s - raised).abs() < 1e-4), "{out:?}");

        backend.set_equalizer(EqSettings::default());
        assert_eq!(backend.feed_capture_level(4, 0.25), [0.25; 8], "off");
    }

    /// EC10: eight local outputs switched on together, while streaming
    /// runs and before it starts.
    #[test]
    fn ec10_eight_local_outputs_switched_on_together_all_play_on_their_own_renderers() {
        let mut backend = MockBackend::empty();
        let ids: Vec<String> = (1..=8).map(|n| format!("output-{n}")).collect();
        for (n, id) in ids.iter().enumerate() {
            backend.connect_output(id, &format!("Output {}", n + 1));
        }
        backend.list_outputs().unwrap();
        backend.start(&SourceId::WholeSystem).unwrap();
        for (n, id) in ids.iter().enumerate() {
            backend.set_output_volume(id, 20.0 + 10.0 * n as f64);
            backend.set_output_enabled(id, true).unwrap();
        }
        assert!(backend.poll().is_empty());
        assert_eq!(playing(&backend), ids);
        assert_eq!(backend.driver().renderers_running(), 8, "one renderer each");
        let mut started = backend.driver().renderers_started().to_vec();
        started.sort();
        assert_eq!(started, ids, "each started once");
        // Each reads through its own controls: eight different gains.
        let mut gains: Vec<f32> = ids
            .iter()
            .map(|id| backend.playing_params(id).unwrap().gain)
            .collect();
        gains.dedup();
        assert_eq!(gains.len(), 8, "{gains:?}");

        // All eight start together with the next session too.
        backend.stop();
        backend.poll();
        assert_eq!(backend.driver().renderers_running(), 0);
        backend.start(&SourceId::WholeSystem).unwrap();
        assert!(backend.poll().is_empty());
        assert_eq!(playing(&backend), ids);
        assert_eq!(backend.driver().renderers_running(), 8);
    }

    #[test]
    fn enabled_outputs_count_the_ones_not_connected() {
        let mut backend = backend();
        backend.set_output_enabled("away", true).unwrap();
        assert_eq!(backend.enabled_outputs(), ["away", HEADPHONES, SPEAKERS]);
        let listed: Vec<String> = backend.outputs().into_iter().map(|o| o.id).collect();
        assert_eq!(
            listed,
            [HEADPHONES, SPEAKERS],
            "only connected ones are listed"
        );
    }

    /// The backend of [`backend`], logging to a debug log of its own in a
    /// folder for the test `name`; returns the log and the folder too.
    fn logged_backend(name: &str) -> (MockBackend, DebugLog, PathBuf) {
        let dir = test_dir(name);
        let log = DebugLog::new(Some(dir.join(FILE_NAME)));
        let mut backend = backend();
        backend.set_debug_log(log.clone());
        (backend, log, dir)
    }

    /// What `log`, kept in `dir`, holds so far, without the time stamps.
    fn logged(log: &DebugLog, dir: &Path) -> Vec<String> {
        assert!(log.flush(TEST_FLUSH), "the log is written");
        read_messages(&dir.join(FILE_NAME))
    }

    #[test]
    fn a_start_and_an_output_switch_are_logged() {
        let (mut backend, log, dir) = logged_backend("controller-start");
        backend.start(&SourceId::WholeSystem).unwrap();
        backend.set_output_enabled(HEADPHONES, false).unwrap();
        backend.set_output_enabled(HEADPHONES, true).unwrap();
        backend.stop();
        let messages = logged(&log, &dir);
        assert_in_order(
            &messages,
            &[
                "starting: capturing the whole system",
                "capture started: everything except process 1000 and its children; \
                 48000 Hz, 2 ch, 32-bit float, converted by Windows",
                "output \"Headphones\" (headphones) started: 48000 Hz, 2 ch (mask 0x3), \
                 float, the device's own format; buffer 960 frames (20.0 ms); delay 0 ms, volume ",
                "output \"Speakers\" (speakers) started: 48000 Hz",
                "output \"Headphones\" (headphones) switched off",
                "output \"Headphones\" (headphones) stopped: switched off",
                "output \"Headphones\" (headphones) switched on",
                "output \"Headphones\" (headphones) started: 48000 Hz",
                "stopping",
                "session ended: the capture and 2 output(s) stopped in ",
            ],
        );
        assert!(
            !messages.iter().any(|m| m.starts_with("event:")),
            "nothing to report: {messages:?}"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_bluetooth_outputs_start_names_its_extra_lead() {
        let (mut backend, log, dir) = logged_backend("controller-bluetooth");
        backend.connect_output_as("buds", "Buds", OutputKind::Bluetooth);
        backend.set_output_enabled("buds", true).unwrap();
        backend.start(&SourceId::WholeSystem).unwrap();
        backend.stop();
        let messages = logged(&log, &dir);
        let started = |prefix: &str| {
            messages
                .iter()
                .find(|m| m.starts_with(prefix))
                .unwrap_or_else(|| panic!("no {prefix}: {messages:?}"))
                .clone()
        };
        assert!(
            started("output \"Buds\" (buds) started: ")
                .ends_with("; Bluetooth, extra lead 10.7 ms")
        );
        assert!(!started("output \"Speakers\" (speakers) started: ").contains("Bluetooth"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_start_that_fails_and_an_output_that_cant_start_are_logged() {
        let (mut backend, log, dir) = logged_backend("controller-failures");
        backend.refuse_captures(Some(BackendError::SourceGone));
        assert!(backend.start(&SourceId::WholeSystem).is_err());
        backend.refuse_captures(None);
        backend.fail_output(SPEAKERS, Some(BackendError::UnsupportedFormat));
        backend.start(&SourceId::WholeSystem).unwrap();
        backend.poll();
        let messages = logged(&log, &dir);
        assert_in_order(
            &messages,
            &[
                "starting: capturing the whole system",
                "the start failed: The app isn't running anymore.",
                "starting: capturing the whole system",
                "capture started:",
                "output \"Headphones\" (headphones) started:",
                "output \"Speakers\" (speakers) could not start: \
                 The device's audio format isn't supported. It is turned off.",
                "event: output \"Speakers\" (speakers) failed: \
                 The device's audio format isn't supported.",
            ],
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn device_changes_and_the_events_they_bring_are_logged() {
        let (mut backend, log, dir) = logged_backend("controller-devices");
        backend.start(&SourceId::WholeSystem).unwrap();
        backend.disconnect_output(HEADPHONES);
        backend.poll();
        backend.connect_output_as("tv", "TV", OutputKind::Hdmi);
        backend.set_default_output("tv");
        backend.poll();
        // A listing that keeps failing is logged once, until it works again.
        backend.fail_listing(Some(listing_error()));
        backend.disconnect_output("tv");
        backend.poll();
        backend.poll();
        backend.fail_listing(None);
        backend.poll();
        backend.poll();
        let messages = logged(&log, &dir);
        assert_in_order(
            &messages,
            &[
                "output \"Headphones\" (headphones) stopped by itself",
                "Windows reported a change of the outputs",
                "outputs listed: 1",
                "    \"Speakers\" (speakers): Other, last played at 48000 Hz, 2 ch",
                "  - \"Headphones\" (headphones): gone",
                "event: the outputs changed",
                "event: output \"Headphones\" (headphones) lost",
                "Windows reported a change of the outputs",
                "outputs listed: 2",
                "    \"Speakers\" (speakers): Other",
                "  + \"TV\" (tv): Hdmi, Windows default",
                "event: the outputs changed",
                "Windows reported a change of the outputs",
                "listing the outputs failed: \
                 IMMDeviceEnumerator::EnumAudioEndpoints failed (0x80004005).",
                "outputs listed: 1",
                "    \"Speakers\" (speakers): Other",
                "event: the outputs changed",
            ],
        );
        let failures = messages
            .iter()
            .filter(|m| m.starts_with("listing the outputs failed"))
            .count();
        assert_eq!(failures, 1, "{messages:?}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn endpoints_left_out_of_the_listing_are_logged_once() {
        let (mut backend, log, dir) = logged_backend("controller-left-out");
        backend.enumerate_endpoint(EndpointReading {
            index: 2,
            id: Ok("buds-call".to_owned()),
            properties: Ok(EndpointProperties {
                name: Some("Headset (Example Buds Hands-Free)".to_owned()),
                form_factor: Some(5),
                enumerator: Some("BTHHFENUM".to_owned()),
                display_port: false,
            }),
        });
        backend.enumerate_endpoint(EndpointReading {
            index: 3,
            id: Err("IMMDevice::GetId failed (0x80070490)".to_owned()),
            properties: Ok(EndpointProperties::default()),
        });
        backend.poll();
        let listed = backend.list_outputs().unwrap();
        assert_eq!(
            listed.iter().map(|o| o.id.as_str()).collect::<Vec<_>>(),
            [HEADPHONES, SPEAKERS],
            "neither endpoint is listed"
        );
        let messages = logged(&log, &dir);
        let hands_free = "output \"Headset (Example Buds Hands-Free)\" (buds-call) left out: \
                          a Bluetooth hands-free endpoint, for calls only";
        let no_id = "render endpoint 3 left out: it couldn't be read \
                     (IMMDevice::GetId failed (0x80070490))";
        assert_in_order(
            &messages,
            &[
                "Windows reported a change of the outputs",
                "outputs listed again, no change (2)",
                hands_free,
                no_id,
                "outputs listed again, no change (2)",
            ],
        );
        for note in [hands_free, no_id] {
            let count = messages.iter().filter(|m| m.as_str() == note).count();
            assert_eq!(count, 1, "logged once, not at every listing: {messages:?}");
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn setting_changes_are_logged_when_they_change_something() {
        let (mut backend, log, dir) = logged_backend("controller-settings");
        backend.set_output_delay(SPEAKERS, 120.0);
        backend.set_output_delay(SPEAKERS, 120.0);
        backend.set_output_volume(HEADPHONES, 50.0);
        let gain = backend.policy().params(HEADPHONES).gain;
        backend.set_master_volume(80.0);
        let mut settings = EqSettings {
            enabled: true,
            preamp_db: -3.0,
            ..EqSettings::default()
        };
        settings.gains_db[0] = 6.0;
        backend.set_equalizer(settings);
        backend.set_equalizer(settings);
        let messages = logged(&log, &dir);
        assert_eq!(
            messages[..3],
            [
                "output \"Speakers\" (speakers) offset: 120 ms".to_owned(),
                format!("output \"Headphones\" (headphones) volume: 50 (gain {gain:.3})"),
                "master volume: 80".to_owned(),
            ]
        );
        assert_eq!(
            messages[3..],
            ["equalizer: on, preamp -3.0 dB, bands \
              +6.0 +0.0 +0.0 +0.0 +0.0 +0.0 +0.0 +0.0 +0.0 +0.0 dB"]
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_status_line_comes_every_few_seconds_while_streaming() {
        let (mut backend, log, dir) = logged_backend("controller-status");
        backend.set_output_delay(SPEAKERS, 120.0);
        backend.start(&SourceId::WholeSystem).unwrap();
        let start = Instant::now();
        // The first check only notes where the count starts.
        backend.log_status_if_due(start);
        backend.feed_capture(48_000);
        backend.log_status_if_due(start + Duration::from_secs(1));
        backend.log_status_if_due(start + STATUS_INTERVAL);
        backend.log_status_if_due(start + STATUS_INTERVAL + Duration::from_secs(1));
        backend.stop();
        backend.log_status_if_due(start + STATUS_INTERVAL * 3);
        let messages = logged(&log, &dir);
        let status = messages.iter().filter(|m| m.starts_with("status:")).count();
        assert_eq!(status, 1, "{messages:?}");
        assert_in_order(
            &messages,
            &[
                "status: capturing the whole system; 48000 frames captured, \
                 9600 frames/s over 5.0 s",
                "  output \"Headphones\" (headphones): delay 0 ms (0 frames), volume ",
                "  output \"Speakers\" (speakers): delay 120 ms (5760 frames), volume ",
                "session ended:",
            ],
        );
        // Nothing reads the mock's ring, so every reader counter is 0.
        let counters = "lag 0 frames; primes 0, resyncs 0, starved reads 0 (0 frames), \
                        drift skips 0 (0 frames), drift silences 0 (0 frames), rejected copies 0, \
                        slowed 0 frames, sped 0 frames, shortfall 0 frames, shortage 0 frames, \
                        least slack n/a";
        let outputs = messages.iter().filter(|m| m.starts_with("  output "));
        assert!(outputs.clone().count() == 2 && outputs.clone().all(|m| m.ends_with(counters)));
        let _ = std::fs::remove_dir_all(dir);
    }

    /// The double-audio reports among `events`: the endpoint IDs of each.
    fn heard_twice(events: &[BackendEvent]) -> Vec<Vec<String>> {
        events
            .iter()
            .filter_map(|event| match event {
                BackendEvent::HeardTwice { paths } => {
                    Some(paths.iter().map(|path| path.endpoint_id.clone()).collect())
                }
                _ => None,
            })
            .collect()
    }

    /// Runs the double-audio check now, and returns what the next poll
    /// reports about it.
    fn check_routing(backend: &mut MockBackend) -> Vec<Vec<String>> {
        backend.check_routing_now();
        heard_twice(&backend.poll())
    }

    const TV: &str = "tv";
    const CABLE: &str = "cable";

    /// The backend of [`backend`] with a TV and a virtual cable connected
    /// and off, streaming the player.
    fn streaming_player() -> MockBackend {
        let mut backend = backend();
        backend.connect_output_as(TV, "TV", OutputKind::Hdmi);
        backend.connect_output_as(CABLE, "Cable Input", OutputKind::Virtual);
        backend.list_outputs().unwrap();
        backend
            .start(&SourceId::App {
                root_pid: PLAYER_PID,
            })
            .unwrap();
        assert!(
            heard_twice(&backend.poll()).is_empty(),
            "the player is silent"
        );
        backend
    }

    #[test]
    fn an_app_heard_straight_from_an_output_is_reported_when_that_changes() {
        let mut backend = streaming_player();
        backend.play_on(PLAYER_PID, TV);
        let events = {
            backend.check_routing_now();
            backend.poll()
        };
        let paths: Vec<&DirectPath> = events
            .iter()
            .flat_map(|event| match event {
                BackendEvent::HeardTwice { paths } => paths.iter().collect(),
                _ => Vec::new(),
            })
            .collect();
        assert_eq!(
            paths,
            [&DirectPath {
                endpoint_id: TV.to_owned(),
                endpoint_name: "TV".to_owned(),
                reason: DirectReason::Session,
                audiosync_plays_here: false,
            }]
        );
        assert!(
            check_routing(&mut backend).is_empty(),
            "no change, no report"
        );
        // It plays on an output AudioSync plays to as well.
        backend.play_on(PLAYER_PID, SPEAKERS);
        assert_eq!(check_routing(&mut backend), [[SPEAKERS, TV]]);
        // A helper process of the app counts as the app.
        backend.add_helper(PLAYER_PID + 1, PLAYER_PID, "player.exe");
        backend.pause_on(PLAYER_PID, SPEAKERS);
        backend.play_on(PLAYER_PID + 1, SPEAKERS);
        assert!(check_routing(&mut backend).is_empty(), "the same outputs");
    }

    #[test]
    fn an_app_moved_to_a_virtual_cable_is_heard_once_again() {
        let mut backend = streaming_player();
        backend.play_on(PLAYER_PID, TV);
        assert_eq!(check_routing(&mut backend), [[TV]]);
        backend.move_session(PLAYER_PID, TV, CABLE);
        for _ in 0..HOLD_CHECKS {
            assert!(check_routing(&mut backend).is_empty(), "the TV is held");
        }
        assert_eq!(check_routing(&mut backend), [Vec::<String>::new()]);
        // A virtual device AudioSync plays to passes the sound on to
        // something the user listens to.
        backend.set_output_enabled(CABLE, true).unwrap();
        assert_eq!(check_routing(&mut backend), [[CABLE]]);
    }

    #[test]
    fn muting_the_output_or_turning_it_down_to_0_clears_the_report() {
        let mut backend = streaming_player();
        backend.play_on(PLAYER_PID, TV);
        assert_eq!(check_routing(&mut backend), [[TV]]);
        backend.mute_output(TV, true);
        assert_eq!(check_routing(&mut backend), [Vec::<String>::new()]);
        backend.mute_output(TV, false);
        assert_eq!(check_routing(&mut backend), [[TV]]);
        backend.set_output_level(TV, 0.0);
        assert_eq!(check_routing(&mut backend), [Vec::<String>::new()]);
        // An output whose volume can't be read counts as heard.
        backend.hide_output_volume(TV);
        assert_eq!(check_routing(&mut backend), [[TV]]);
    }

    #[test]
    fn a_paused_app_stays_reported_where_it_played() {
        let mut backend = streaming_player();
        backend.play_on(PLAYER_PID, TV);
        assert_eq!(check_routing(&mut backend), [[TV]]);
        backend.pause_on(PLAYER_PID, TV);
        for _ in 0..HOLD_CHECKS * 2 {
            assert!(check_routing(&mut backend).is_empty(), "still reported");
        }
        backend.play_on(PLAYER_PID, CABLE);
        for _ in 0..HOLD_CHECKS {
            assert!(check_routing(&mut backend).is_empty(), "held a few checks");
        }
        assert_eq!(check_routing(&mut backend), [Vec::<String>::new()]);
    }

    #[test]
    fn stopping_or_changing_the_source_clears_the_report() {
        let mut backend = streaming_player();
        backend.play_on(PLAYER_PID, TV);
        assert_eq!(check_routing(&mut backend), [[TV]]);
        backend.stop();
        assert_eq!(heard_twice(&backend.poll()), [Vec::<String>::new()]);
        assert!(check_routing(&mut backend).is_empty(), "nothing streams");

        backend
            .start(&SourceId::App {
                root_pid: PLAYER_PID,
            })
            .unwrap();
        assert_eq!(heard_twice(&backend.poll()), [[TV]]);
        backend.add_app(3000, "video.exe");
        backend.start(&SourceId::App { root_pid: 3000 }).unwrap();
        assert_eq!(
            heard_twice(&backend.poll()),
            [Vec::<String>::new()],
            "the new source plays nowhere yet"
        );
    }

    #[test]
    fn nothing_is_heard_twice_while_no_output_plays() {
        let mut backend = streaming_player();
        backend.play_on(PLAYER_PID, TV);
        assert_eq!(check_routing(&mut backend), [[TV]]);
        backend.set_output_enabled(SPEAKERS, false).unwrap();
        backend.set_output_enabled(HEADPHONES, false).unwrap();
        assert_eq!(check_routing(&mut backend), [Vec::<String>::new()]);
    }

    #[test]
    fn the_whole_system_is_heard_twice_from_the_default_output() {
        let mut backend = backend();
        backend.connect_output_as(TV, "TV", OutputKind::Hdmi);
        backend.set_default_output(TV);
        backend.list_outputs().unwrap();
        backend.start(&SourceId::WholeSystem).unwrap();
        let events = backend.poll();
        let reasons: Vec<(String, DirectReason, bool)> = events
            .iter()
            .flat_map(|event| match event {
                BackendEvent::HeardTwice { paths } => paths
                    .iter()
                    .map(|path| {
                        (
                            path.endpoint_id.clone(),
                            path.reason,
                            path.audiosync_plays_here,
                        )
                    })
                    .collect(),
                _ => Vec::new(),
            })
            .collect();
        // AudioSync's own sessions on the speakers and headphones don't
        // count.
        assert_eq!(
            reasons,
            [(TV.to_owned(), DirectReason::DefaultEndpoint, false)]
        );
        backend.play_on(PLAYER_PID, HEADPHONES);
        assert_eq!(check_routing(&mut backend), [[HEADPHONES, TV]]);
        backend.set_default_output(SPEAKERS);
        backend.pause_on(PLAYER_PID, HEADPHONES);
        backend.play_on(PLAYER_PID, SPEAKERS);
        assert_eq!(
            check_routing(&mut backend),
            [[HEADPHONES, SPEAKERS]],
            "the headphones are held a few checks"
        );
        for _ in 1..HOLD_CHECKS {
            assert!(check_routing(&mut backend).is_empty());
        }
        assert_eq!(check_routing(&mut backend), [[SPEAKERS]]);
    }

    #[test]
    fn another_app_pausing_now_and_then_doesnt_flap_the_whole_system_report() {
        let mut backend = backend();
        backend.connect_output_as(TV, "TV", OutputKind::Hdmi);
        backend.set_default_output(TV);
        backend.list_outputs().unwrap();
        backend.start(&SourceId::WholeSystem).unwrap();
        assert_eq!(heard_twice(&backend.poll()), [[TV]]);
        // One app plays on the TV all along, another on the headphones now
        // and then.
        backend.play_on(PLAYER_PID, TV);
        backend.add_app(3000, "video.exe");
        backend.play_on(3000, HEADPHONES);
        assert_eq!(check_routing(&mut backend), [[HEADPHONES, TV]]);
        for _ in 0..3 {
            backend.pause_on(3000, HEADPHONES);
            for _ in 0..HOLD_CHECKS {
                assert!(check_routing(&mut backend).is_empty(), "held");
            }
            backend.play_on(3000, HEADPHONES);
            assert!(check_routing(&mut backend).is_empty());
        }
    }

    #[test]
    fn the_check_runs_at_the_first_poll_then_every_few_seconds() {
        let mut backend = backend();
        backend.connect_output_as(TV, "TV", OutputKind::Hdmi);
        backend.list_outputs().unwrap();
        backend
            .start(&SourceId::App {
                root_pid: PLAYER_PID,
            })
            .unwrap();
        backend.play_on(PLAYER_PID, TV);
        let start = Instant::now();
        backend.check_routing_if_due(start);
        assert_eq!(heard_twice(&backend.poll()), [[TV]], "the first check");
        backend.mute_output(TV, true);
        backend.check_routing_if_due(start + ROUTING_INTERVAL / 2);
        assert!(heard_twice(&backend.poll()).is_empty(), "not due yet");
        backend.check_routing_if_due(start + ROUTING_INTERVAL);
        assert_eq!(heard_twice(&backend.poll()), [Vec::<String>::new()]);
    }

    #[test]
    fn a_snapshot_that_fails_keeps_the_last_answer() {
        let mut backend = streaming_player();
        backend.play_on(PLAYER_PID, TV);
        assert_eq!(check_routing(&mut backend), [[TV]]);
        backend.fail_endpoint_audio(Some(listing_error()));
        backend.mute_output(TV, true);
        assert!(check_routing(&mut backend).is_empty());
        backend.fail_endpoint_audio(None);
        assert_eq!(check_routing(&mut backend), [Vec::<String>::new()]);
    }

    #[test]
    fn double_audio_is_logged_when_it_changes() {
        let (mut backend, log, dir) = logged_backend("controller-double-audio");
        backend.connect_output_as(TV, "TV", OutputKind::Hdmi);
        backend.list_outputs().unwrap();
        backend
            .start(&SourceId::App {
                root_pid: PLAYER_PID,
            })
            .unwrap();
        backend.poll();
        backend.play_on(PLAYER_PID, TV);
        backend.play_on(PLAYER_PID, SPEAKERS);
        backend.set_output_level(SPEAKERS, 0.25);
        check_routing(&mut backend);
        check_routing(&mut backend);
        backend.mute_output(SPEAKERS, true);
        check_routing(&mut backend);
        backend.fail_endpoint_audio(Some(listing_error()));
        check_routing(&mut backend);
        check_routing(&mut backend);
        backend.fail_endpoint_audio(None);
        backend.pause_on(PLAYER_PID, TV);
        backend.pause_on(PLAYER_PID, SPEAKERS);
        backend.mute_output(SPEAKERS, false);
        check_routing(&mut backend);
        backend.stop();
        backend.poll();
        let messages = logged(&log, &dir);
        assert_in_order(
            &messages,
            &[
                "double audio: the source plays straight from 2 output(s) too",
                "  \"Speakers\" (speakers): an active session of process 2000; \
                 not muted, volume 0.25; AudioSync plays there too",
                "  \"TV\" (tv): an active session of process 2000; \
                 not muted, volume 1.00",
                "event: the source is heard twice, straight from \"Speakers\" (speakers), \
                 \"TV\" (tv)",
                "double audio: the source plays straight from 1 output(s) too",
                "  \"TV\" (tv): an active session of process 2000; not muted, volume 1.00",
                "  no longer \"Speakers\" (speakers): muted",
                "event: the source is heard twice, straight from \"TV\" (tv)",
                "the double-audio check failed: \
                 IMMDeviceEnumerator::EnumAudioEndpoints failed (0x80004005).",
                "double audio: the source plays straight from 2 output(s) too",
                "  \"Speakers\" (speakers): the source played there last and is silent now; \
                 not muted, volume 0.25; AudioSync plays there too",
                "  \"TV\" (tv): the source played there last and is silent now; \
                 not muted, volume 1.00",
                "double audio: none, streaming stopped",
                "event: the source is heard only through AudioSync again",
            ],
        );
        let failures = messages
            .iter()
            .filter(|m| m.starts_with("the double-audio check failed"))
            .count();
        assert_eq!(failures, 1, "logged once: {messages:?}");
        let changes = messages
            .iter()
            .filter(|m| m.starts_with("double audio:"))
            .count();
        assert_eq!(changes, 4, "only changes are logged: {messages:?}");
        let _ = std::fs::remove_dir_all(dir);
    }

    /// `backend()` with an AirPlay sender that lists one speaker it can
    /// play, Kitchen.
    fn with_airplay() -> (MockBackend, FakeAirPlay, Device) {
        let mut backend = backend();
        let fake = FakeAirPlay::new();
        backend.set_airplay(fake.link(), None);
        let kitchen = device(1, "Kitchen", OPEN_TXT);
        fake.appear(&kitchen);
        assert_eq!(backend.poll(), [BackendEvent::AirPlayChanged]);
        (backend, fake, kitchen)
    }

    fn delay_frames(backend: &MockBackend, id: &str) -> i64 {
        backend.playing_params(id).expect("it plays").delay_frames
    }

    const BUILT_IN: &str = "builtin";
    const USB: &str = "usb";
    const BLUETOOTH: &str = "bluetooth";

    /// A stopped mock with the PC's own speakers (the reference), a USB
    /// speaker at +150 ms and a Bluetooth speaker at −250 ms, all on and
    /// listed, and one app.
    fn local_delay_backend() -> MockBackend {
        let mut backend = MockBackend::empty();
        backend.connect_output_as(BUILT_IN, "Speakers", OutputKind::BuiltIn);
        backend.connect_output_as(USB, "USB Speaker", OutputKind::Usb);
        backend.connect_output_as(BLUETOOTH, "Earbuds", OutputKind::Bluetooth);
        backend.add_app(PLAYER_PID, "player.exe");
        for id in [BUILT_IN, USB, BLUETOOTH] {
            backend.set_output_enabled(id, true).unwrap();
        }
        backend.set_output_delay(USB, 150.0);
        backend.set_output_delay(BLUETOOTH, -250.0);
        backend.list_outputs().unwrap();
        backend
    }

    /// The hold-backs the three outputs of [`local_delay_backend`] play
    /// at, in whole ms: the PC's speakers, USB, Bluetooth; `None` for one
    /// that doesn't play.
    fn held(backend: &MockBackend) -> [Option<i64>; 3] {
        [BUILT_IN, USB, BLUETOOTH].map(|id| {
            backend
                .playing_params(id)
                .map(|params| params.delay_frames / 48)
        })
    }

    #[test]
    fn only_local_outputs_a_speakers_offset_is_applied() {
        let mut backend = local_delay_backend();
        backend.start(&SourceId::WholeSystem).unwrap();
        assert_eq!(held(&backend), [Some(250), Some(400), Some(0)]);
    }

    #[test]
    fn a_late_speaker_switched_on_or_off_retimes_the_running_others() {
        let mut backend = local_delay_backend();
        backend.set_output_enabled(BLUETOOTH, false).unwrap();
        backend.start(&SourceId::WholeSystem).unwrap();
        assert_eq!(held(&backend), [Some(0), Some(150), None]);
        backend.set_output_enabled(BLUETOOTH, true).unwrap();
        assert_eq!(held(&backend), [Some(250), Some(400), Some(0)]);
        backend.set_output_enabled(BLUETOOTH, false).unwrap();
        assert_eq!(held(&backend), [Some(0), Some(150), None]);
        // A device that goes away leaves too, and joins again when it's back.
        backend.set_output_enabled(BLUETOOTH, true).unwrap();
        backend.disconnect_output(BLUETOOTH);
        backend.poll();
        assert_eq!(held(&backend), [Some(0), Some(150), None]);
        backend.connect_output_as(BLUETOOTH, "Earbuds", OutputKind::Bluetooth);
        backend.poll();
        assert_eq!(held(&backend), [Some(250), Some(400), Some(0)]);
    }

    #[test]
    fn editing_the_lowest_offset_retimes_the_others() {
        let mut backend = local_delay_backend();
        backend.start(&SourceId::WholeSystem).unwrap();
        backend.set_output_delay(BLUETOOTH, -300.0);
        assert_eq!(held(&backend), [Some(300), Some(450), Some(0)]);
        backend.set_output_delay(USB, 200.0);
        assert_eq!(held(&backend), [Some(300), Some(500), Some(0)]);
    }

    #[test]
    fn a_late_speaker_that_cant_start_doesnt_hold_the_others_back() {
        let mut backend = local_delay_backend();
        backend.set_output_enabled(BLUETOOTH, false).unwrap();
        // Started after the others (IDs in order), at −250 ms, and failing.
        let late = "zz-late";
        backend.connect_output_as(late, "Late Earbuds", OutputKind::Bluetooth);
        backend.set_output_enabled(late, true).unwrap();
        backend.set_output_delay(late, -250.0);
        backend.list_outputs().unwrap();
        backend.fail_output(
            late,
            Some(BackendError::System {
                call: "IAudioClient::Initialize",
                code: 0x8889_0008_u32 as i32,
            }),
        );
        backend.start(&SourceId::WholeSystem).unwrap();
        assert!(backend.playing_params(late).is_none(), "it is turned off");
        assert_eq!(held(&backend), [Some(0), Some(150), None]);
    }

    /// [`local_delay_backend`] with an AirPlay sender (L = 1750 ms) that
    /// lists Kitchen.
    fn local_delay_with_airplay() -> (MockBackend, FakeAirPlay, Device) {
        let mut backend = local_delay_backend();
        let fake = FakeAirPlay::new();
        backend.set_airplay(fake.link(), None);
        let kitchen = device(1, "Kitchen", OPEN_TXT);
        fake.appear(&kitchen);
        backend.poll();
        (backend, fake, kitchen)
    }

    #[test]
    fn an_airplay_speaker_chosen_adds_the_sender_latency_on_top() {
        let (mut backend, _fake, kitchen) = local_delay_with_airplay();
        backend.start(&SourceId::WholeSystem).unwrap();
        assert_eq!(held(&backend), [Some(250), Some(400), Some(0)]);
        backend.set_airplay_enabled(&id_of(&kitchen), true).unwrap();
        assert_eq!(held(&backend), [Some(1750), Some(1900), Some(1500)]);
        backend
            .set_airplay_enabled(&id_of(&kitchen), false)
            .unwrap();
        assert_eq!(held(&backend), [Some(250), Some(400), Some(0)]);
    }

    #[test]
    fn airplay_speakers_wait_for_a_cast_speaker_whose_offset_is_longer_than_l() {
        use crate::audio::cast::tests::{FakeCast, KITCHEN};
        let (mut backend, fake, kitchen) = local_delay_with_airplay();
        let id = id_of(&kitchen);
        let cast = FakeCast::default();
        cast.announce(KITCHEN, "Kitchen", audiosync_cast::discovery::DEVICE_PORT);
        backend.set_cast(cast.link());
        backend.poll();
        backend.set_airplay_enabled(&id, true).unwrap();
        backend.start(&SourceId::WholeSystem).unwrap();
        backend.set_cast_enabled(KITCHEN, true).unwrap();
        fake.take_calls();
        backend.set_cast_offset(KITCHEN, 3000.0);
        assert_eq!(held(&backend), [Some(3000), Some(3150), Some(2750)]);
        assert_eq!(
            fake.take_calls(),
            [Call::Delay(id.clone(), Duration::from_millis(1250))]
        );
        backend.set_cast_offset(KITCHEN, 1000.0);
        assert_eq!(held(&backend), [Some(1750), Some(1900), Some(1500)]);
        assert_eq!(fake.take_calls(), [Call::Delay(id, Duration::ZERO)]);
    }

    #[test]
    fn calibrate_all_waits_for_the_cast_feeds_hold_back_and_a_guess_by_format() {
        use audiosync_cast::audio::{Codec, StreamFormat};
        use audiosync_cast::discovery::DeviceId;
        use audiosync_cast::host::{AttemptId, HostEvent};
        use audiosync_cast::media::MediaError;
        use audiosync_cast::session::SessionEvent;
        use audiosync_core::calibrate_all::Class;

        use crate::audio::calibrate_all::lineup;
        use crate::audio::cast::tests::{FakeCast, KITCHEN};
        let (mut backend, _fake, kitchen) = local_delay_with_airplay();
        let cast = FakeCast::default();
        cast.announce(KITCHEN, "Kitchen", audiosync_cast::discovery::DEVICE_PORT);
        backend.set_cast(cast.link());
        backend.poll();
        backend.set_airplay_enabled(&id_of(&kitchen), true).unwrap();
        backend.start(&SourceId::WholeSystem).unwrap();
        backend.set_cast_enabled(KITCHEN, true).unwrap();
        let device = DeviceId::parse(KITCHEN).unwrap();
        cast.report(HostEvent::Started {
            attempt: AttemptId(1),
            device,
            format: StreamFormat::wav(48_000).unwrap(),
        });
        backend.poll();
        let wait = |backend: &MockBackend| {
            lineup(backend)
                .members
                .into_iter()
                .find(|member| member.class == Class::Cast)
                .map(|member| member.latency_ms)
        };
        // No offset: the feed is held back to L = 1750 ms, then the WAV guess.
        assert_eq!(wait(&backend), Some(1750.0 + 3000.0));
        backend.set_cast_offset(KITCHEN, 1000.0);
        assert_eq!(wait(&backend), Some(1000.0 + 750.0 + 500.0), "measured");
        backend.set_cast_offset(KITCHEN, 0.0);
        assert_eq!(wait(&backend), Some(1750.0 + 3000.0), "0 ms is no offset");
        cast.report(HostEvent::Session {
            attempt: AttemptId(1),
            device,
            event: SessionEvent::FormatFallback {
                from: Codec::Wav,
                to: Codec::Flac,
                reason: MediaError::LoadFailed {
                    detailed_error_code: None,
                },
            },
        });
        backend.poll();
        assert_eq!(wait(&backend), Some(1750.0 + 6000.0), "FLAC");
    }

    #[test]
    fn the_airplay_delay_is_an_adjustment_on_top_of_the_sender_latency() {
        let (mut backend, fake, kitchen) = local_delay_with_airplay();
        backend.set_airplay_enabled(&id_of(&kitchen), true).unwrap();
        backend.start(&SourceId::WholeSystem).unwrap();
        assert_eq!(backend.airplay_sender_latency_ms(), Some(1750.0));
        backend.set_airplay_adjustment(250.0);
        assert_eq!(backend.airplay_adjustment(), 250.0);
        assert_eq!(held(&backend), [Some(2000), Some(2150), Some(1750)]);
        // A is relative: a new L_s moves L with it.
        fake.set_latency(Duration::from_secs(1));
        backend.poll();
        assert_eq!(backend.airplay_sender_latency_ms(), Some(1000.0));
        assert_eq!(held(&backend), [Some(1250), Some(1400), Some(1000)]);
        backend.set_airplay_adjustment(f64::NAN);
        assert_eq!(backend.airplay_adjustment(), 250.0, "ignored");
        backend.set_airplay_adjustment(-9000.0);
        assert_eq!(backend.airplay_adjustment(), -2000.0, "clamped");
        assert_eq!(held(&backend), [Some(250), Some(400), Some(0)], "L = 0");
    }

    #[test]
    fn an_airplay_speaker_on_moves_local_outputs_to_l_plus_their_delay_and_off_back() {
        let (mut backend, fake, kitchen) = with_airplay();
        let id = id_of(&kitchen);
        backend.set_output_delay(SPEAKERS, 150.0);
        backend.start(&SourceId::WholeSystem).unwrap();
        assert_eq!(delay_frames(&backend, SPEAKERS), 7_200, "its own delay");
        assert_eq!(delay_frames(&backend, HEADPHONES), 0);
        fake.take_calls();

        backend.set_airplay_enabled(&id, true).unwrap();
        // L + offset (S176): 1750 + 150 ms and 1750 + 0 ms at 48 kHz.
        assert_eq!(delay_frames(&backend, SPEAKERS), 91_200);
        assert_eq!(delay_frames(&backend, HEADPHONES), 84_000);
        assert_eq!(
            fake.take_calls(),
            [Call::Add {
                id: id.clone(),
                volume: Volume::Db(-12.0),
                delay: Duration::ZERO,
            }]
        );
        assert_eq!(backend.enabled_airplay(), std::slice::from_ref(&id));

        backend.set_airplay_enabled(&id, false).unwrap();
        assert_eq!(delay_frames(&backend, SPEAKERS), 7_200, "back to its own");
        assert_eq!(delay_frames(&backend, HEADPHONES), 0);
        assert_eq!(fake.take_calls(), [Call::Remove(id)]);
    }

    #[test]
    fn airplay_speakers_join_with_the_capture_and_leave_when_streaming_stops() {
        let mut backend = backend();
        let fake = FakeAirPlay::new();
        let clock = SenderClock::new(Instant::now());
        let (_stream, input) = Stream::new(StreamConfig::default(), clock, Instant::now());
        let feed = FeedSlot::new(input);
        backend.set_airplay(fake.link(), Some(feed.clone()));
        let kitchen = device(1, "Kitchen", OPEN_TXT);
        let id = id_of(&kitchen);
        fake.appear(&kitchen);
        backend.poll();
        backend.set_airplay_enabled(&id, true).unwrap();
        assert!(fake.take_calls().is_empty(), "nothing streams yet");

        backend.start(&SourceId::WholeSystem).unwrap();
        assert!(backend.driver().capture_feeds_airplay());
        assert!(!feed.is_free(), "the capture holds the feed");
        assert_eq!(
            fake.take_calls(),
            [
                Call::RestartTimeline,
                Call::Add {
                    id: id.clone(),
                    volume: Volume::Db(-12.0),
                    delay: Duration::ZERO,
                }
            ]
        );
        assert_eq!(delay_frames(&backend, SPEAKERS), 84_000);

        backend.stop();
        assert_eq!(fake.take_calls(), [Call::Remove(id.clone())]);
        assert!(feed.is_free(), "the feed is back for the next capture");
        assert_eq!(backend.enabled_airplay(), [id], "it stays on");
    }

    fn now_playing_sent(fake: &FakeAirPlay) -> Vec<NowPlaying> {
        fake.take_calls()
            .into_iter()
            .filter_map(|call| match call {
                Call::NowPlaying(now_playing) => Some(now_playing),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn the_playing_track_reaches_airplay_receivers_while_they_play() {
        let (mut backend, fake, kitchen) = with_airplay();
        let reader = FakeReader::with_icon(b"icon");
        backend.set_track_reader(reader.reader());
        backend.set_receiver_labels(shared_labels());
        backend.set_airplay_enabled(&id_of(&kitchen), true).unwrap();
        backend.poll();
        assert_eq!(reader.following(), None, "nothing streams yet");

        backend
            .start(&SourceId::App {
                root_pid: PLAYER_PID,
            })
            .unwrap();
        backend.poll();
        assert_eq!(
            reader.following(),
            Some(Follow::App {
                executable: "player.exe".to_owned()
            })
        );
        reader.read(Some(track("player.exe", "Song", false, Some(b"cover"))));
        backend.poll();
        let sent = now_playing_sent(&fake);
        assert_eq!(sent.len(), 1, "{sent:?}");
        let track_sent = sent[0].track.as_ref().expect("a track");
        assert_eq!(
            track_sent.title, "Song",
            "a paused track of the source app counts"
        );
        assert_eq!(sent[0].artwork.as_deref(), Some(&b"cover"[..]));

        reader.read(Some(track("player.exe", "Song", false, Some(b"cover"))));
        backend.poll();
        assert!(now_playing_sent(&fake).is_empty(), "nothing changed");

        reader.read(None);
        backend.poll();
        let sent = now_playing_sent(&fake);
        let track_sent = sent[0].track.as_ref().expect("labels");
        assert_eq!(
            (track_sent.artist.as_str(), sent[0].artwork.as_deref()),
            ("Player", Some(&b"icon"[..])),
            "with no session, the source's name and AudioSync's icon"
        );

        backend.stop();
        backend.poll();
        assert_eq!(reader.following(), None, "reading stops with the stream");
        assert_eq!(
            now_playing_sent(&fake),
            [NowPlaying::default()],
            "a receiver that starts later gets no track from before"
        );
    }

    #[test]
    fn an_input_device_source_names_the_device_to_airplay_receivers() {
        let (mut backend, fake, kitchen) = with_airplay();
        backend.connect_input("mic", "USB Microphone", "USB");
        let reader = FakeReader::with_icon(b"icon");
        backend.set_track_reader(reader.reader());
        backend.set_receiver_labels(shared_labels());
        backend.set_airplay_enabled(&id_of(&kitchen), true).unwrap();
        backend
            .start(&SourceId::Input {
                endpoint_id: "mic".to_owned(),
            })
            .unwrap();
        backend.poll();
        reader.read(None);
        backend.poll();
        let sent = now_playing_sent(&fake);
        let track_sent = sent.last().and_then(|sent| sent.track.as_ref());
        assert_eq!(
            track_sent.map(|track| (track.title.as_str(), track.artist.as_str())),
            Some(("Title label", "USB Microphone")),
            "{sent:?}"
        );
    }

    #[test]
    fn an_input_device_that_cant_be_named_gets_the_unnamed_source_label() {
        let (mut backend, fake, kitchen) = with_airplay();
        backend.connect_input("mic", "USB Microphone", "USB");
        let reader = FakeReader::with_icon(b"icon");
        backend.set_track_reader(reader.reader());
        backend.set_receiver_labels(shared_labels());
        backend.set_airplay_enabled(&id_of(&kitchen), true).unwrap();
        backend
            .start(&SourceId::Input {
                endpoint_id: "mic".to_owned(),
            })
            .unwrap();
        backend.fail_input_listing(Some(BackendError::NotResponding));
        backend.poll();
        reader.read(None);
        backend.poll();
        let sent = now_playing_sent(&fake);
        let track_sent = sent.last().and_then(|sent| sent.track.as_ref());
        assert_eq!(
            track_sent.map(|track| track.artist.as_str()),
            Some("Unnamed label"),
            "{sent:?}"
        );

        backend.fail_input_listing(None);
        reader.read(None);
        backend.poll();
        let sent = now_playing_sent(&fake);
        assert_eq!(
            sent.last()
                .and_then(|sent| sent.track.as_ref())
                .map(|track| track.artist.as_str()),
            Some("USB Microphone"),
            "a listing that works again names the device: {sent:?}"
        );
    }

    #[test]
    fn a_capture_started_while_a_hung_one_holds_the_airplay_feed_takes_it_once_its_back() {
        let mut backend = backend();
        let fake = FakeAirPlay::new();
        let clock = SenderClock::new(Instant::now());
        let (_stream, input) = Stream::new(StreamConfig::default(), clock, Instant::now());
        let feed = FeedSlot::new(input);
        backend.set_airplay(fake.link(), Some(feed.clone()));
        // The last capture, let go while Windows audio hung, still holds the
        // feed: the next one starts without it.
        let held = feed.lease().expect("the feed is free");
        backend.start(&SourceId::WholeSystem).unwrap();
        assert!(!backend.driver().capture_feeds_airplay());
        backend.poll();
        assert!(!backend.driver().capture_feeds_airplay(), "still held");

        // Windows audio answers: that capture ends and gives the feed back,
        // and the session starts again to feed the speakers, streaming on.
        drop(held);
        let events = backend.poll();
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, BackendEvent::Stopped { .. })),
            "{events:?}"
        );
        assert!(
            backend.driver().capture_feeds_airplay(),
            "the capture feeds the speakers"
        );
        assert!(!feed.is_free(), "the new capture holds the feed");
        assert_eq!(playing(&backend), [HEADPHONES, SPEAKERS]);
        backend.poll();
        assert!(backend.driver().capture_feeds_airplay(), "it starts once");
    }

    #[test]
    fn a_capture_started_while_a_hung_one_holds_the_cast_tap_takes_it_once_its_back() {
        use crate::audio::cast::tests::FakeCast;
        let mut backend = backend();
        backend.set_cast(FakeCast::default().link());
        let held = backend
            .cast
            .as_ref()
            .and_then(CastSpeakers::lease)
            .expect("the tap is free");
        backend.start(&SourceId::WholeSystem).unwrap();
        assert!(!backend.driver().capture_feeds_cast());
        backend.poll();
        assert!(!backend.driver().capture_feeds_cast(), "still held");

        drop(held);
        let events = backend.poll();
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, BackendEvent::Stopped { .. })),
            "{events:?}"
        );
        assert!(
            backend.driver().capture_feeds_cast(),
            "the capture feeds the Cast speaker"
        );
        assert_eq!(playing(&backend), [HEADPHONES, SPEAKERS]);
        backend.poll();
        assert_eq!(
            backend.driver().captures_started().len(),
            2,
            "it starts once"
        );
    }

    #[test]
    fn a_new_l_from_the_sender_re_anchors_the_playing_outputs() {
        let (mut backend, fake, kitchen) = with_airplay();
        backend.set_airplay_enabled(&id_of(&kitchen), true).unwrap();
        backend.start(&SourceId::WholeSystem).unwrap();
        assert_eq!(delay_frames(&backend, SPEAKERS), 84_000);
        fake.set_latency(Duration::from_secs(1));
        backend.poll();
        assert_eq!(delay_frames(&backend, SPEAKERS), 48_000);
    }

    #[test]
    fn an_airplay_speaker_that_needs_a_password_is_turned_off_and_outputs_follow() {
        let (mut backend, fake, kitchen) = with_airplay();
        let id = id_of(&kitchen);
        backend.set_output_delay(SPEAKERS, 150.0);
        backend.set_airplay_enabled(&id, true).unwrap();
        backend.start(&SourceId::WholeSystem).unwrap();
        fake.state(&kitchen, SpeakerState::Playing);
        assert_eq!(
            backend.poll(),
            [
                BackendEvent::AirPlayChanged,
                BackendEvent::AirPlayPlaying { id: id.clone() }
            ]
        );
        fake.fail(&kitchen, ErrorKind::PasswordRequired);
        assert_eq!(
            backend.poll(),
            [
                BackendEvent::AirPlayChanged,
                BackendEvent::AirPlayFailed {
                    id: id.clone(),
                    problem: AirPlayProblem::NeedsPassword,
                }
            ]
        );
        assert!(backend.enabled_airplay().is_empty());
        assert_eq!(delay_frames(&backend, SPEAKERS), 7_200);
    }

    #[test]
    fn airplay_is_logged() {
        let (mut backend, log, dir) = logged_backend("airplay_is_logged");
        let fake = FakeAirPlay::new();
        backend.set_airplay(fake.link(), None);
        let kitchen = device(1, "Kitchen", OPEN_TXT);
        let id = id_of(&kitchen);
        fake.appear(&kitchen);
        backend.poll();
        backend.set_airplay_enabled(&id, true).unwrap();
        backend.start(&SourceId::WholeSystem).unwrap();
        fake.state(&kitchen, SpeakerState::Playing);
        fake.fail(&kitchen, ErrorKind::Lost);
        backend.poll();
        backend.stop();
        let messages = logged(&log, &dir);
        assert_in_order(
            &messages,
            &[
                "AirPlay: the sender runs, L = 1750 ms",
                "AirPlay receiver found: \"Kitchen\" (02005E005301), can play",
                "AirPlay speaker \"Kitchen\" (02005E005301) switched on",
                "local outputs play at L + their offset (L = 1750 ms",
                "capture started:",
                "AirPlay speaker \"Kitchen\" (02005E005301) added: volume 60 (-12.0 dB), delay 0 ms",
                "AirPlay speaker \"Kitchen\" (02005E005301): playing",
                "AirPlay speaker \"Kitchen\" (02005E005301) error: Lost (ConnectionLost)",
                "AirPlay speaker \"Kitchen\" (02005E005301) removed",
            ],
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    /// A starter that hands out `senders` in turn, then fails.
    fn starter_of(senders: Vec<Option<StartedSender>>) -> SenderStarter {
        let mut senders = senders.into_iter();
        Box::new(move || senders.next().flatten())
    }

    fn not_browsing(fake: &FakeAirPlay) -> Option<StartedSender> {
        Some(StartedSender {
            link: fake.link(),
            feed: None,
            browsing: false,
        })
    }

    #[test]
    fn an_airplay_sender_that_cant_start_at_launch_is_reported_and_retry_starts_one() {
        let mut backend = backend();
        let fake = FakeAirPlay::new();
        let clock = SenderClock::new(Instant::now());
        let (_stream, input) = Stream::new(StreamConfig::default(), clock, Instant::now());
        let feed = FeedSlot::new(input);
        backend.start_airplay(
            starter_of(vec![
                None,
                Some(StartedSender::browsing(fake.link(), Some(feed))),
            ]),
            PairingStore::in_memory(),
        );
        assert_eq!(backend.poll(), [BackendEvent::AirPlayStopped]);
        assert!(backend.poll().is_empty(), "told once");
        backend.start(&SourceId::WholeSystem).unwrap();
        assert!(!backend.driver().capture_feeds_airplay());

        backend.retry_airplay();
        assert_eq!(backend.poll(), [BackendEvent::AirPlayChanged]);
        assert!(
            backend.driver().capture_feeds_airplay(),
            "the capture feeds the new sender"
        );
        assert_eq!(playing(&backend), [HEADPHONES, SPEAKERS]);
        let kitchen = device(1, "Kitchen", OPEN_TXT);
        fake.appear(&kitchen);
        backend.poll();
        assert_eq!(backend.airplay_speakers().len(), 1);
    }

    #[test]
    fn an_airplay_sender_whose_discovery_cant_start_is_reported_and_retry_starts_a_new_one() {
        let mut backend = backend();
        let first = FakeAirPlay::new();
        let second = FakeAirPlay::new();
        backend.start_airplay(
            starter_of(vec![
                not_browsing(&first),
                Some(StartedSender::browsing(second.link(), None)),
            ]),
            PairingStore::in_memory(),
        );
        assert_eq!(backend.poll(), [BackendEvent::AirPlayStopped]);
        assert!(backend.poll().is_empty(), "told once");

        backend.retry_airplay();
        backend.poll();
        let kitchen = device(1, "Kitchen", OPEN_TXT);
        second.appear(&kitchen);
        backend.poll();
        assert_eq!(backend.airplay_speakers().len(), 1);
    }

    #[test]
    fn a_retry_whose_discovery_still_cant_start_is_reported_again() {
        let mut backend = backend();
        let first = FakeAirPlay::new();
        let second = FakeAirPlay::new();
        let third = FakeAirPlay::new();
        backend.start_airplay(
            starter_of(vec![
                not_browsing(&first),
                not_browsing(&second),
                Some(StartedSender::browsing(third.link(), None)),
            ]),
            PairingStore::in_memory(),
        );
        assert_eq!(backend.poll(), [BackendEvent::AirPlayStopped]);
        backend.retry_airplay();
        let events = backend.poll();
        assert!(events.contains(&BackendEvent::AirPlayStopped), "{events:?}");
        // And Retry still works after that.
        backend.retry_airplay();
        assert!(!backend.poll().contains(&BackendEvent::AirPlayStopped));
    }

    #[test]
    fn a_retry_whose_sender_still_cant_start_is_reported_again() {
        let mut backend = backend();
        backend.start_airplay(starter_of(vec![None, None]), PairingStore::in_memory());
        assert_eq!(backend.poll(), [BackendEvent::AirPlayStopped]);
        backend.retry_airplay();
        assert_eq!(backend.poll(), [BackendEvent::AirPlayStopped]);
        assert!(backend.airplay_speakers().is_empty());
    }
}
