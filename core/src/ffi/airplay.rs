//! AudioSync's own AirPlay sender ([`audiosync_airplay`]) through the C
//! interface, for the macOS app, which links one Rust static library and so
//! reaches the sender through the core's.
//!
//! The host creates a sender, starts browsing the local network and takes
//! the receivers it finds, one change at a time, with their ID, name, model,
//! whether they can play from this sender and discovery's hints. It adds and
//! removes speakers by those IDs, sets each one's volume and the master, and
//! takes what happens to them as events: their state, the errors that end
//! their sessions with the reason to show, their latency, and the base
//! latency L local outputs line up with (P113, P114). The capture thread
//! hands the sender its audio through an input handle
//! ([`asc_airplay_take_input`]) at the capture rate; the input resamples it
//! to 44.1 kHz (P73) without allocating, locking or logging (P6). Nothing
//! secret ever crosses this interface: no password, PIN, key or credential.
//! PIN pairing comes in a later stage.
//!
//! Threads: every `asc_airplay_*` call on a sender handle runs on one control
//! thread at a time (never an audio thread: they allocate, and
//! `asc_airplay_destroy` waits for the sender's threads), and
//! `asc_airplay_destroy` runs once no other call on that handle can run. An
//! input handle's `asc_airplay_input_write` runs on one capture thread at a
//! time, `asc_airplay_input_set_rate` on any thread, and its destroy once no
//! write can run; it may outlive its sender, whose audio then goes nowhere.
//! The sender itself runs its own threads and sockets (P6), which this
//! module never touches.

use std::collections::{HashMap, VecDeque};
use std::ffi::{CStr, CString, c_char};
use std::ptr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use audiosync_airplay::audio::SAMPLE_RATE;
use audiosync_airplay::discovery::{Device, DeviceKey, ServiceKind};
use audiosync_airplay::error::{ErrorKind, UnsupportedReason, UserReason};
use audiosync_airplay::input::InputWriter;
use audiosync_airplay::sender::{
    ReceiverEvent, ReceiverInfo, Sender, SenderConfig, SenderEvent, SpeakerError, SpeakerEvent,
    SpeakerOptions, SpeakerState,
};
use audiosync_airplay::stream::SpeakerLatency;
use audiosync_airplay::volume::{MIN_DB, Volume};

use super::{host_slice, stereo_samples};
use crate::resample::Resampler;
use crate::ring::is_supported_rate;

/// What a receiver event reports ([`AscAirPlayReceiver::event`]): discovery
/// found a receiver.
pub const AIRPLAY_APPEARED: i32 = 1;
/// Its records or its verdict changed.
pub const AIRPLAY_CHANGED: i32 = 2;
/// It left the network, or discovery stopped.
pub const AIRPLAY_GONE: i32 = 3;

/// Whether a receiver can play from this sender
/// ([`AscAirPlayReceiver::availability`]): it can.
pub const AIRPLAY_AVAILABLE: i32 = 0;
/// An Apple TV that asks for a PIN once (the legacy pairing, P46).
pub const AIRPLAY_NEEDS_PIN: i32 = 1;
/// It asks for a password (P39).
pub const AIRPLAY_NEEDS_PASSWORD: i32 = 2;
/// It likely needs another kind of pairing (P16).
pub const AIRPLAY_NEEDS_PAIRING: i32 = 3;
/// Only AirPlay 2 could serve it, and that path is off (P2).
pub const AIRPLAY_AIRPLAY2_ONLY: i32 = 4;
/// The sender can't drive it (P139); [`AscAirPlayReceiver::unsupported`]
/// says why.
pub const AIRPLAY_UNSUPPORTED: i32 = 5;

/// Why a receiver can't play ([`AscAirPlayReceiver::unsupported`]): no
/// reason, it can (or the reason is not one of these).
pub const AIRPLAY_REASON_NONE: i32 = 0;
/// No encryption mode the sender supports.
pub const AIRPLAY_REASON_ENCRYPTION: i32 = 1;
/// No codec the sender may use.
pub const AIRPLAY_REASON_CODEC: i32 = 2;
/// No UDP transport.
pub const AIRPLAY_REASON_TRANSPORT: i32 = 3;
/// Its methods lack ANNOUNCE, SETUP or RECORD.
pub const AIRPLAY_REASON_METHODS: i32 = 4;
/// A format other than 44 100 Hz, 16-bit stereo.
pub const AIRPLAY_REASON_FORMAT: i32 = 5;
/// Only AirPlay 2 could serve it.
pub const AIRPLAY_REASON_AIRPLAY2_ONLY: i32 = 6;

/// What discovery saw ([`AscAirPlayReceiver::flags`]), bit by bit: it
/// announces `_raop._tcp` (AirPlay 1 audio).
pub const AIRPLAY_FLAG_RAOP: u32 = 1;
/// It announces `_airplay._tcp`.
pub const AIRPLAY_FLAG_AIRPLAY: u32 = 2;
/// Its model is an Apple TV.
pub const AIRPLAY_FLAG_APPLE_TV: u32 = 4;
/// Its records say it needs a password.
pub const AIRPLAY_FLAG_PASSWORD: u32 = 8;
/// Its flags hint that it needs pairing (only a hint, P46).
pub const AIRPLAY_FLAG_PAIRING: u32 = 16;
/// An Apple TV that needs the legacy PIN pairing.
pub const AIRPLAY_FLAG_LEGACY_PAIRING: u32 = 32;
/// Its flags hint that another sender may be playing (only a hint, P137).
pub const AIRPLAY_FLAG_BUSY: u32 = 64;
/// It lists AirPlay 2 features.
pub const AIRPLAY_FLAG_AIRPLAY2: u32 = 128;

/// What a sender event reports ([`AscAirPlayEvent::kind`]): a speaker's
/// state changed ([`AscAirPlayEvent::state`]).
pub const AIRPLAY_EVENT_STATE: i32 = 1;
/// An error ended a speaker's session ([`AscAirPlayEvent::error`]).
pub const AIRPLAY_EVENT_ERROR: i32 = 2;
/// A speaker's latencies, when it starts playing and whenever its first
/// sound time moves (P110, P113, P115).
pub const AIRPLAY_EVENT_LATENCY: i32 = 3;
/// The base latency L or the timeline changed; local outputs line up with
/// the new L (P98, P114, P143).
pub const AIRPLAY_EVENT_TIMELINE: i32 = 4;

/// A speaker's state ([`AscAirPlayEvent::state`]): a session is starting.
pub const AIRPLAY_STATE_CONNECTING: i32 = 1;
/// RECORD succeeded: it plays the shared timeline.
pub const AIRPLAY_STATE_PLAYING: i32 = 2;
/// Its last session failed; the next starts in
/// [`AscAirPlayEvent::retry_ms`] (P142).
pub const AIRPLAY_STATE_RETRYING: i32 = 3;
/// Its last session failed with an error that needs the user; no retry
/// until the host adds it again (P142).
pub const AIRPLAY_STATE_FAILED: i32 = 4;
/// It was removed, or the sender shut down, and its session is closed.
pub const AIRPLAY_STATE_STOPPED: i32 = 5;

/// What to tell the user about an error that ended a session
/// ([`AscAirPlayEvent::error`], P133): the speaker didn't answer.
pub const AIRPLAY_ERROR_NOT_RESPONDING: i32 = 1;
/// The connection died or the speaker went silent while playing.
pub const AIRPLAY_ERROR_CONNECTION_LOST: i32 = 2;
/// Another sender seems to be playing to it.
pub const AIRPLAY_ERROR_BUSY: i32 = 3;
/// The speaker turned the session down.
pub const AIRPLAY_ERROR_REFUSED: i32 = 4;
/// The speaker answered in a way the sender doesn't understand.
pub const AIRPLAY_ERROR_INCOMPATIBLE: i32 = 5;
/// The speaker needs a password and none was given.
pub const AIRPLAY_ERROR_PASSWORD_REQUIRED: i32 = 6;
/// The speaker rejected the password.
pub const AIRPLAY_ERROR_WRONG_PASSWORD: i32 = 7;
/// The speaker needs pairing, or rejected or cancelled it.
pub const AIRPLAY_ERROR_PAIRING_REQUIRED: i32 = 8;
/// The sender can't drive it; [`AscAirPlayEvent::unsupported`] says why.
pub const AIRPLAY_ERROR_UNSUPPORTED: i32 = 9;

/// Output frames the input resamples into per step; bounds its buffer.
const INPUT_CHUNK_FRAMES: usize = 4096;

/// One change discovery saw. The strings are NUL-terminated UTF-8 owned by
/// the sender handle, valid until the next `asc_airplay_next_receiver` call
/// on it or its destroy; a gone receiver has only its ID, its name and
/// model are empty and its availability is `AIRPLAY_AVAILABLE`.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct AscAirPlayReceiver {
    pub event: i32,
    pub id: *const c_char,
    pub name: *const c_char,
    pub model: *const c_char,
    pub availability: i32,
    pub unsupported: i32,
    pub flags: u32,
}

/// One thing that happened to a speaker, or to the stream. The strings are
/// NUL-terminated UTF-8 owned by the sender handle, valid until the next
/// `asc_airplay_next_event` call on it or its destroy. Fields a kind
/// doesn't use are 0, except `retry_ms` and `receiver_latency_frames`,
/// which are then -1, and the strings, which are then "".
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct AscAirPlayEvent {
    /// `AIRPLAY_EVENT_*`.
    pub kind: i32,
    /// The speaker's receiver ID; "" for a timeline event.
    pub id: *const c_char,
    /// A state event's `AIRPLAY_STATE_*`.
    pub state: i32,
    /// An error event's `AIRPLAY_ERROR_*`.
    pub error: i32,
    /// With `AIRPLAY_ERROR_UNSUPPORTED`, its `AIRPLAY_REASON_*`.
    pub unsupported: i32,
    /// Milliseconds until the next session of a retrying speaker, or of the
    /// speaker an error event names; -1 when the user must act (P142).
    pub retry_ms: i64,
    /// A latency event's L + d_s, or a timeline event's L, in ms (P113).
    pub latency_ms: f64,
    /// A latency event's `Audio-Latency` in frames; -1 when it sent none.
    pub receiver_latency_frames: i64,
    /// A timeline event after a restart (P143).
    pub restarted: bool,
    /// An error event's error, for a debug log (no secrets).
    pub detail: *const c_char,
}

/// A speaker the host added.
struct Added {
    key: DeviceKey,
    /// Its slider position, 0 to 100.
    position: f64,
}

/// A sender event the host hasn't taken yet.
enum Pending {
    Speaker(String, SpeakerEvent),
    Timeline { latency: Duration, restarted: bool },
}

/// An AirPlay sender behind an opaque C pointer.
pub struct AscAirPlay {
    sender: Sender,
    /// The stream's audio input, until [`asc_airplay_take_input`] hands it
    /// to the host.
    input: Option<InputWriter>,
    /// Receiver changes not taken yet.
    receivers: VecDeque<ReceiverEvent>,
    /// The strings of the last receiver handed out: ID, name, model.
    receiver_strings: [CString; 3],
    /// Speaker and timeline events not taken yet.
    events: VecDeque<Pending>,
    /// The strings of the last event handed out: ID, detail.
    event_strings: [CString; 2],
    /// The receivers discovery lists now, by ID, so the host adds speakers
    /// by ID.
    listed: HashMap<String, Device>,
    /// The speakers the host added, by ID.
    added: HashMap<String, Added>,
    /// The master position, 0 to 100.
    master: f64,
}

/// The ID a receiver goes by, the one the Windows app uses too: its device
/// ID (12 uppercase hex digits), or, for a service without one, its kind
/// and instance name (`raop:<instance>`, `airplay:<instance>`).
fn receiver_id(key: &DeviceKey) -> String {
    match key {
        DeviceKey::Id(id) => id.to_string(),
        DeviceKey::Instance(ServiceKind::Raop, instance) => format!("raop:{instance}"),
        DeviceKey::Instance(ServiceKind::AirPlay, instance) => format!("airplay:{instance}"),
    }
}

/// The reason code of an unsupported receiver.
fn reason_code(reason: UnsupportedReason) -> i32 {
    match reason {
        UnsupportedReason::Encryption => AIRPLAY_REASON_ENCRYPTION,
        UnsupportedReason::Codec => AIRPLAY_REASON_CODEC,
        UnsupportedReason::Transport => AIRPLAY_REASON_TRANSPORT,
        UnsupportedReason::Methods => AIRPLAY_REASON_METHODS,
        UnsupportedReason::Format => AIRPLAY_REASON_FORMAT,
        UnsupportedReason::AirPlay2Only => AIRPLAY_REASON_AIRPLAY2_ONLY,
    }
}

/// The error code of what to tell the user, with the unsupported reason.
fn error_code(reason: UserReason) -> (i32, i32) {
    match reason {
        UserReason::NotResponding => (AIRPLAY_ERROR_NOT_RESPONDING, AIRPLAY_REASON_NONE),
        UserReason::ConnectionLost => (AIRPLAY_ERROR_CONNECTION_LOST, AIRPLAY_REASON_NONE),
        UserReason::Busy => (AIRPLAY_ERROR_BUSY, AIRPLAY_REASON_NONE),
        UserReason::Refused => (AIRPLAY_ERROR_REFUSED, AIRPLAY_REASON_NONE),
        UserReason::Incompatible => (AIRPLAY_ERROR_INCOMPATIBLE, AIRPLAY_REASON_NONE),
        UserReason::PasswordRequired => (AIRPLAY_ERROR_PASSWORD_REQUIRED, AIRPLAY_REASON_NONE),
        UserReason::WrongPassword => (AIRPLAY_ERROR_WRONG_PASSWORD, AIRPLAY_REASON_NONE),
        UserReason::PairingRequired => (AIRPLAY_ERROR_PAIRING_REQUIRED, AIRPLAY_REASON_NONE),
        UserReason::Unsupported(reason) => (AIRPLAY_ERROR_UNSUPPORTED, reason_code(reason)),
    }
}

/// Whether a receiver can play, and if not, why: its plan first (P139,
/// P2), then discovery's hints, the legacy pairing before a password, as
/// the Windows app decides. There is no pairing store behind this
/// interface yet, so an Apple TV that asks for a PIN always needs one.
fn availability(info: &ReceiverInfo) -> (i32, i32) {
    let verdict = &info.device.verdict;
    match info.plan {
        Err(ErrorKind::Unsupported(UnsupportedReason::AirPlay2Only)) => {
            (AIRPLAY_AIRPLAY2_ONLY, AIRPLAY_REASON_AIRPLAY2_ONLY)
        }
        Err(ErrorKind::Unsupported(reason)) => (AIRPLAY_UNSUPPORTED, reason_code(reason)),
        Err(_) => (AIRPLAY_UNSUPPORTED, AIRPLAY_REASON_NONE),
        Ok(()) if verdict.legacy_pairing => (AIRPLAY_NEEDS_PIN, AIRPLAY_REASON_NONE),
        Ok(()) if verdict.needs_password => (AIRPLAY_NEEDS_PASSWORD, AIRPLAY_REASON_NONE),
        Ok(()) if verdict.needs_pairing => (AIRPLAY_NEEDS_PAIRING, AIRPLAY_REASON_NONE),
        Ok(()) => (AIRPLAY_AVAILABLE, AIRPLAY_REASON_NONE),
    }
}

/// Discovery's view of a receiver as flag bits.
fn flags(info: &ReceiverInfo) -> u32 {
    let device = &info.device;
    let verdict = &device.verdict;
    [
        (device.raop.is_some(), AIRPLAY_FLAG_RAOP),
        (device.airplay.is_some(), AIRPLAY_FLAG_AIRPLAY),
        (device.is_apple_tv(), AIRPLAY_FLAG_APPLE_TV),
        (verdict.needs_password, AIRPLAY_FLAG_PASSWORD),
        (verdict.needs_pairing, AIRPLAY_FLAG_PAIRING),
        (verdict.legacy_pairing, AIRPLAY_FLAG_LEGACY_PAIRING),
        (verdict.busy_hint, AIRPLAY_FLAG_BUSY),
        (verdict.airplay2, AIRPLAY_FLAG_AIRPLAY2),
    ]
    .into_iter()
    .filter(|(set, _)| *set)
    .fold(0, |bits, (_, bit)| bits | bit)
}

/// The receiver's model: AirPlay `model` when present, otherwise RAOP `am`.
fn model(info: &ReceiverInfo) -> String {
    let device = &info.device;
    device
        .airplay_txt
        .as_ref()
        .and_then(|txt| txt.model.clone())
        .or_else(|| device.raop_txt.as_ref().and_then(|txt| txt.model.clone()))
        .unwrap_or_default()
}

/// `text` as a C string; a NUL inside (which no DNS-SD name should carry)
/// becomes U+FFFD.
fn c_string(text: &str) -> CString {
    CString::new(text.replace('\0', "\u{FFFD}")).unwrap_or_default()
}

/// The volume an AirPlay speaker plays at for its slider position and the
/// master position, both 0 to 100 (P118 leaves the mapping to the host):
/// their product, 0 to 1, spread evenly over −30 to 0 dB, and muted at 0.
/// The Windows app maps its sliders the same way.
fn speaker_volume(position: f64, master: f64) -> Volume {
    let level = (position / 100.0).clamp(0.0, 1.0) * (master / 100.0).clamp(0.0, 1.0);
    if level > 0.0 {
        Volume::Db(MIN_DB * (1.0 - level))
    } else {
        Volume::Muted
    }
}

/// Milliseconds from now until `at`, 0 when it has passed.
fn millis_until(at: Instant) -> i64 {
    i64::try_from(at.saturating_duration_since(Instant::now()).as_millis()).unwrap_or(i64::MAX)
}

/// A duration in milliseconds.
fn millis(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1000.0
}

/// The host's C string as UTF-8, if it is one.
///
/// # Safety
///
/// `text` is null or points to a NUL-terminated string that stays valid and
/// unchanged during the call.
unsafe fn host_text<'a>(text: *const c_char) -> Option<&'a str> {
    if text.is_null() {
        return None;
    }
    // SAFETY: non-null, and by the caller's contract NUL-terminated and
    // unchanged during the call.
    unsafe { CStr::from_ptr(text) }.to_str().ok()
}

impl AscAirPlay {
    /// Moves every event the sender reported into the host's queues, and
    /// keeps the list of receivers the host can add up to date. Speaker
    /// events the host doesn't use (warnings, methods, server, audio jack,
    /// volume reports) and pairing events are passed over.
    fn pump(&mut self) {
        while let Some(event) = self.sender.poll_event() {
            match event {
                SenderEvent::Receiver(event) => {
                    match &event {
                        ReceiverEvent::Appeared(info) | ReceiverEvent::Changed(info) => {
                            self.listed
                                .insert(receiver_id(&info.device.key), info.device.clone());
                        }
                        ReceiverEvent::Gone(key) => {
                            self.listed.remove(&receiver_id(key));
                        }
                    }
                    self.receivers.push_back(event);
                }
                SenderEvent::Speaker { key, event } => {
                    if matches!(
                        event,
                        SpeakerEvent::State(_) | SpeakerEvent::Error(_) | SpeakerEvent::Latency(_)
                    ) {
                        self.events
                            .push_back(Pending::Speaker(receiver_id(&key), event));
                    }
                }
                SenderEvent::Timeline {
                    latency, restarted, ..
                } => self
                    .events
                    .push_back(Pending::Timeline { latency, restarted }),
                SenderEvent::Pairing { .. } => {}
            }
        }
    }

    /// Keeps the strings of a receiver change and describes it with
    /// pointers to them.
    fn report(&mut self, event: ReceiverEvent) -> AscAirPlayReceiver {
        let (code, id, name, model, (availability, unsupported), flags) = match &event {
            ReceiverEvent::Appeared(info) | ReceiverEvent::Changed(info) => (
                if matches!(event, ReceiverEvent::Appeared(_)) {
                    AIRPLAY_APPEARED
                } else {
                    AIRPLAY_CHANGED
                },
                receiver_id(&info.device.key),
                info.device.name.clone(),
                model(info),
                availability(info),
                flags(info),
            ),
            ReceiverEvent::Gone(key) => (
                AIRPLAY_GONE,
                receiver_id(key),
                String::new(),
                String::new(),
                (AIRPLAY_AVAILABLE, AIRPLAY_REASON_NONE),
                0,
            ),
        };
        self.receiver_strings = [c_string(&id), c_string(&name), c_string(&model)];
        AscAirPlayReceiver {
            event: code,
            id: self.receiver_strings[0].as_ptr(),
            name: self.receiver_strings[1].as_ptr(),
            model: self.receiver_strings[2].as_ptr(),
            availability,
            unsupported,
            flags,
        }
    }

    /// Keeps the strings of a sender event and describes it with pointers
    /// to them.
    fn describe(&mut self, pending: Pending) -> AscAirPlayEvent {
        let mut event = AscAirPlayEvent {
            kind: 0,
            id: ptr::null(),
            state: 0,
            error: 0,
            unsupported: AIRPLAY_REASON_NONE,
            retry_ms: -1,
            latency_ms: 0.0,
            receiver_latency_frames: -1,
            restarted: false,
            detail: ptr::null(),
        };
        let (id, detail) = match pending {
            Pending::Speaker(id, SpeakerEvent::State(state)) => {
                event.kind = AIRPLAY_EVENT_STATE;
                event.state = match state {
                    SpeakerState::Connecting => AIRPLAY_STATE_CONNECTING,
                    SpeakerState::Playing => AIRPLAY_STATE_PLAYING,
                    SpeakerState::Retrying { at } => {
                        event.retry_ms = millis_until(at);
                        AIRPLAY_STATE_RETRYING
                    }
                    SpeakerState::Failed => AIRPLAY_STATE_FAILED,
                    SpeakerState::Stopped => AIRPLAY_STATE_STOPPED,
                };
                (id, String::new())
            }
            Pending::Speaker(
                id,
                SpeakerEvent::Error(SpeakerError {
                    error,
                    reason,
                    retry_at,
                }),
            ) => {
                event.kind = AIRPLAY_EVENT_ERROR;
                (event.error, event.unsupported) = error_code(reason);
                event.retry_ms = retry_at.map_or(-1, millis_until);
                (id, error.to_string())
            }
            Pending::Speaker(id, SpeakerEvent::Latency(latency)) => {
                let SpeakerLatency {
                    arrival_to_sound,
                    receiver_latency_frames,
                    ..
                } = latency;
                event.kind = AIRPLAY_EVENT_LATENCY;
                event.latency_ms = millis(arrival_to_sound);
                event.receiver_latency_frames = receiver_latency_frames.map_or(-1, i64::from);
                (id, String::new())
            }
            // Never queued (see `pump`).
            Pending::Speaker(id, _) => (id, String::new()),
            Pending::Timeline { latency, restarted } => {
                event.kind = AIRPLAY_EVENT_TIMELINE;
                event.latency_ms = millis(latency);
                event.restarted = restarted;
                (String::new(), String::new())
            }
        };
        self.event_strings = [c_string(&id), c_string(&detail)];
        event.id = self.event_strings[0].as_ptr();
        event.detail = self.event_strings[1].as_ptr();
        event
    }
}

/// The capture side of an [`AscAirPlayInput`]: the sender's input, the
/// resampler from the capture rate to 44.1 kHz and its output buffer.
struct Feed {
    writer: InputWriter,
    resampler: Resampler,
    /// Resampler output, stereo.
    resampled: Box<[f32]>,
    /// The last rate request taken in (see [`AscAirPlayInput::requested`]).
    applied: u64,
    /// The resampler runs at a supported rate the host asked for.
    ready: bool,
}

impl Feed {
    /// Resamples `block` and hands it to the sender as audio that arrived
    /// now; takes in a new rate request first. Returns the 44.1 kHz frames
    /// handed over. Never allocates, locks, blocks or logs (P6).
    fn write(&mut self, block: &[f32], requested: u64) -> usize {
        if requested != self.applied {
            self.applied = requested;
            // Low half: the rate; a new request, even for the same rate,
            // starts the resampler afresh.
            self.ready = self.resampler.set_input_rate(requested as u32).is_ok();
        }
        if !self.ready {
            return 0;
        }
        let at = Instant::now();
        let frames = block.len() / 2;
        let (mut consumed, mut handed) = (0, 0);
        // Until the input is used up and nothing more comes out: the
        // resampler may take in the whole block while the buffer holds only
        // part of its output, and keeps the rest for the next call.
        loop {
            let (used, produced) = self
                .resampler
                .process(&block[consumed * 2..frames * 2], &mut self.resampled);
            if produced > 0 {
                // Frames that come too late or don't fit are dropped there
                // and counted (P112, P144).
                self.writer.write_f32(&self.resampled[..produced * 2], at);
                handed += produced;
            }
            consumed += used;
            if used == 0 && produced == 0 {
                break;
            }
        }
        handed
    }
}

/// The capture thread's way into a sender behind an opaque C pointer. The
/// capture side and the rate request are separate fields that share only
/// an atomic, so a rate request never borrows what a write borrows.
pub struct AscAirPlayInput {
    feed: Feed,
    /// The latest rate request: a count in the high 32 bits, bumped by
    /// every request, and the rate in Hz in the low 32 bits; 0 before the
    /// first.
    requested: AtomicU64,
}

/// Starts an AirPlay sender: its stream, silent until audio comes through
/// its input ([`asc_airplay_take_input`]), and its driver thread. Discovery
/// waits for [`asc_airplay_start_discovery`]. The master starts at 100.
/// Null when the thread can't start.
#[unsafe(no_mangle)]
pub extern "C" fn asc_airplay_create() -> *mut AscAirPlay {
    match Sender::start(SenderConfig::default()) {
        Ok((sender, input)) => Box::into_raw(Box::new(AscAirPlay {
            sender,
            input: Some(input),
            receivers: VecDeque::new(),
            receiver_strings: Default::default(),
            events: VecDeque::new(),
            event_strings: Default::default(),
            listed: HashMap::new(),
            added: HashMap::new(),
            master: 100.0,
        })),
        Err(_) => ptr::null_mut(),
    }
}

/// Stops the sender and frees it: every speaker is torn down, discovery
/// stops and its threads end within about 2 s, which this call waits for
/// (P129), so a host calls it off its main thread. Sets `*airplay` to null,
/// so destroying through the same pointer again does nothing. An input
/// taken from it stays valid until its own destroy.
///
/// # Safety
///
/// `airplay` is null or points to a pointer that is null or was returned by
/// [`asc_airplay_create`] and not destroyed yet; no other call on that
/// handle runs now or later.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn asc_airplay_destroy(airplay: *mut *mut AscAirPlay) {
    if airplay.is_null() {
        return;
    }
    // SAFETY: by the caller's contract `airplay` points to a readable and
    // writable handle pointer.
    let handle = unsafe { airplay.replace(ptr::null_mut()) };
    if handle.is_null() {
        return;
    }
    // SAFETY: the pointer came from Box::into_raw in asc_airplay_create, and
    // by the caller's contract it is destroyed once (the host's pointer is
    // now null), with no call in progress.
    drop(unsafe { Box::from_raw(handle) });
}

/// Starts browsing the local network for AirPlay receivers over multicast
/// DNS (P7); they arrive through [`asc_airplay_next_receiver`]. False when
/// the handle is null or multicast DNS can't start.
///
/// # Safety
///
/// `airplay` is null or a live handle, called under the threading contract.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn asc_airplay_start_discovery(airplay: *mut AscAirPlay) -> bool {
    // SAFETY: by the caller's contract null or a live handle that no other
    // call uses now.
    let Some(airplay) = (unsafe { airplay.as_mut() }) else {
        return false;
    };
    airplay.sender.start_discovery().is_ok()
}

/// Stops browsing; every receiver discovery listed is then reported gone.
/// Speakers keep playing.
///
/// # Safety
///
/// `airplay` is null or a live handle, called under the threading contract.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn asc_airplay_stop_discovery(airplay: *mut AscAirPlay) {
    // SAFETY: by the caller's contract null or a live handle that no other
    // call uses now.
    if let Some(airplay) = unsafe { airplay.as_mut() } {
        airplay.sender.stop_discovery();
    }
}

/// Takes the next change discovery saw, if one is waiting, into
/// `*receiver` and returns true. Returns false, leaving `*receiver` as it
/// was, when none is waiting or either pointer is null. Never waits.
/// Speaker and timeline events wait for [`asc_airplay_next_event`].
///
/// # Safety
///
/// `airplay` is null or a live handle, called under the threading
/// contract; `receiver` is null or points to a writable
/// `AscAirPlayReceiver`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn asc_airplay_next_receiver(
    airplay: *mut AscAirPlay,
    receiver: *mut AscAirPlayReceiver,
) -> bool {
    if receiver.is_null() {
        return false;
    }
    // SAFETY: by the caller's contract null or a live handle that no other
    // call uses now.
    let Some(airplay) = (unsafe { airplay.as_mut() }) else {
        return false;
    };
    airplay.pump();
    let Some(event) = airplay.receivers.pop_front() else {
        return false;
    };
    let report = airplay.report(event);
    // SAFETY: non-null, and by the caller's contract writable.
    unsafe { receiver.write(report) };
    true
}

/// Takes the next speaker or timeline event, if one is waiting, into
/// `*event` and returns true. Returns false, leaving `*event` as it was,
/// when none is waiting or either pointer is null. Never waits. Receiver
/// changes wait for [`asc_airplay_next_receiver`].
///
/// # Safety
///
/// `airplay` is null or a live handle, called under the threading
/// contract; `event` is null or points to a writable `AscAirPlayEvent`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn asc_airplay_next_event(
    airplay: *mut AscAirPlay,
    event: *mut AscAirPlayEvent,
) -> bool {
    if event.is_null() {
        return false;
    }
    // SAFETY: by the caller's contract null or a live handle that no other
    // call uses now.
    let Some(airplay) = (unsafe { airplay.as_mut() }) else {
        return false;
    };
    airplay.pump();
    let Some(pending) = airplay.events.pop_front() else {
        return false;
    };
    let described = airplay.describe(pending);
    // SAFETY: non-null, and by the caller's contract writable.
    unsafe { event.write(described) };
    true
}

/// Adds the receiver discovery lists under `id` as a speaker, at slider
/// position `volume` (0 to 100) times the master, with no delay of its own:
/// a session starts at once, and its state, errors and latency come as
/// events. Adding a speaker already added applies the volume and, if it
/// failed or waits for a retry, starts a new session now. False, adding
/// nothing, when the handle or `id` is null, `id` isn't a listed receiver,
/// or `volume` isn't finite.
///
/// # Safety
///
/// `airplay` is null or a live handle, called under the threading
/// contract; `id` is null or a NUL-terminated string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn asc_airplay_add_speaker(
    airplay: *mut AscAirPlay,
    id: *const c_char,
    volume: f64,
) -> bool {
    // SAFETY: by the caller's contract null or a live handle that no other
    // call uses now.
    let Some(airplay) = (unsafe { airplay.as_mut() }) else {
        return false;
    };
    // SAFETY: by the caller's contract null or a NUL-terminated string.
    let Some(id) = (unsafe { host_text(id) }) else {
        return false;
    };
    if !volume.is_finite() {
        return false;
    }
    airplay.pump();
    let Some(device) = airplay.listed.get(id).cloned() else {
        return false;
    };
    let position = volume.clamp(0.0, 100.0);
    let key = device.key.clone();
    let options = SpeakerOptions::new(speaker_volume(position, airplay.master));
    airplay.sender.add_speaker(device, options);
    airplay.added.insert(id.to_owned(), Added { key, position });
    true
}

/// Removes the speaker added under `id`: its packets stop, TEARDOWN goes
/// out and its connection closes after the answer or 2 s (P129); a stopped
/// state event follows. False when the handle or `id` is null or no
/// speaker was added under `id`.
///
/// # Safety
///
/// `airplay` is null or a live handle, called under the threading
/// contract; `id` is null or a NUL-terminated string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn asc_airplay_remove_speaker(
    airplay: *mut AscAirPlay,
    id: *const c_char,
) -> bool {
    // SAFETY: by the caller's contract null or a live handle that no other
    // call uses now.
    let Some(airplay) = (unsafe { airplay.as_mut() }) else {
        return false;
    };
    // SAFETY: by the caller's contract null or a NUL-terminated string.
    let Some(id) = (unsafe { host_text(id) }) else {
        return false;
    };
    let Some(added) = airplay.added.remove(id) else {
        return false;
    };
    airplay.sender.remove_speaker(&added.key);
    true
}

/// Sets the slider position (0 to 100) of the speaker added under `id`; it
/// plays at that position times the master, live (P118). False, changing
/// nothing, when the handle or `id` is null, no speaker was added under
/// `id`, or `volume` isn't finite.
///
/// # Safety
///
/// `airplay` is null or a live handle, called under the threading
/// contract; `id` is null or a NUL-terminated string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn asc_airplay_set_volume(
    airplay: *mut AscAirPlay,
    id: *const c_char,
    volume: f64,
) -> bool {
    // SAFETY: by the caller's contract null or a live handle that no other
    // call uses now.
    let Some(airplay) = (unsafe { airplay.as_mut() }) else {
        return false;
    };
    // SAFETY: by the caller's contract null or a NUL-terminated string.
    let Some(id) = (unsafe { host_text(id) }) else {
        return false;
    };
    if !volume.is_finite() {
        return false;
    }
    let master = airplay.master;
    let Some(added) = airplay.added.get_mut(id) else {
        return false;
    };
    added.position = volume.clamp(0.0, 100.0);
    airplay
        .sender
        .set_volume(&added.key, speaker_volume(added.position, master));
    true
}

/// Sets the master position (0 to 100), which scales every speaker's
/// volume, live, and the volume of every speaker added later. A master
/// that isn't finite is ignored.
///
/// # Safety
///
/// `airplay` is null or a live handle, called under the threading contract.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn asc_airplay_set_master(airplay: *mut AscAirPlay, master: f64) {
    // SAFETY: by the caller's contract null or a live handle that no other
    // call uses now.
    let Some(airplay) = (unsafe { airplay.as_mut() }) else {
        return;
    };
    if !master.is_finite() {
        return;
    }
    airplay.master = master.clamp(0.0, 100.0);
    for added in airplay.added.values() {
        airplay
            .sender
            .set_volume(&added.key, speaker_volume(added.position, airplay.master));
    }
}

/// The base latency L in milliseconds (P98, P113): a local output lines up
/// with the speakers at L plus its own delay (P114). 0 for a null handle.
///
/// # Safety
///
/// `airplay` is null or a live handle, called under the threading contract.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn asc_airplay_latency_ms(airplay: *const AscAirPlay) -> f64 {
    // SAFETY: by the caller's contract null or a live handle that no other
    // call uses now.
    unsafe { airplay.as_ref() }.map_or(0.0, |airplay| millis(airplay.sender.latency()))
}

/// The host restarted its capture (P143): every speaker is flushed and the
/// timeline starts over; a timeline event follows.
///
/// # Safety
///
/// `airplay` is null or a live handle, called under the threading contract.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn asc_airplay_restart_timeline(airplay: *mut AscAirPlay) {
    // SAFETY: by the caller's contract null or a live handle that no other
    // call uses now.
    if let Some(airplay) = unsafe { airplay.as_mut() } {
        airplay.sender.restart_timeline();
    }
}

/// The sender's audio input, for the capture thread: the first call hands
/// it over, every later one returns null, as does a null handle. Its
/// memory is allocated here; it starts without a rate, so its writes are
/// dropped until [`asc_airplay_input_set_rate`] gives it one.
///
/// # Safety
///
/// `airplay` is null or a live handle, called under the threading contract.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn asc_airplay_take_input(airplay: *mut AscAirPlay) -> *mut AscAirPlayInput {
    // SAFETY: by the caller's contract null or a live handle that no other
    // call uses now.
    let Some(airplay) = (unsafe { airplay.as_mut() }) else {
        return ptr::null_mut();
    };
    let Ok(resampler) = Resampler::new(SAMPLE_RATE, SAMPLE_RATE) else {
        return ptr::null_mut();
    };
    let Some(writer) = airplay.input.take() else {
        return ptr::null_mut();
    };
    Box::into_raw(Box::new(AscAirPlayInput {
        feed: Feed {
            writer,
            resampler,
            resampled: vec![0.0; INPUT_CHUNK_FRAMES * 2].into_boxed_slice(),
            applied: 0,
            ready: false,
        },
        requested: AtomicU64::new(0),
    }))
}

/// Any thread: the rate in Hz of the audio the capture writes from now on,
/// rounded to whole hertz. The next write takes it in and starts the
/// resampler afresh, so a host calls it at each capture start, even at an
/// unchanged rate. False, changing nothing, for a null handle or a rate
/// outside 8000 to 384 000 Hz. Real-time safe.
///
/// # Safety
///
/// `input` is null or a live input handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn asc_airplay_input_set_rate(
    input: *mut AscAirPlayInput,
    rate: f64,
) -> bool {
    if input.is_null() || !rate.is_finite() {
        return false;
    }
    let rate = rate.round();
    if !(0.0..=f64::from(u32::MAX)).contains(&rate) || !is_supported_rate(rate as u32) {
        return false;
    }
    // SAFETY: the handle is live; this call borrows only the atomic, which
    // writes read but never borrow mutably.
    let requested = unsafe { &(*input).requested };
    let mut old = requested.load(Ordering::Relaxed);
    loop {
        let new = ((old >> 32).wrapping_add(1) << 32) | u64::from(rate as u32);
        match requested.compare_exchange_weak(old, new, Ordering::Release, Ordering::Relaxed) {
            Ok(_) => return true,
            Err(current) => old = current,
        }
    }
}

/// Capture thread: hands `frames` interleaved stereo frames at the rate
/// last set to the sender, resampled to 44.1 kHz, as audio that arrived
/// now (P73, P96). Returns the 44.1 kHz frames handed over, 0 before a
/// rate is set; the sender drops those that come too late or don't fit
/// (P112). Never allocates, locks, blocks or logs (P6); it reads the
/// monotonic clock once. Real-time safe.
///
/// # Safety
///
/// `input` is null or a live input handle, written by one thread at a
/// time; `samples` is null or points to `frames` × 2 aligned floats that
/// nothing writes during the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn asc_airplay_input_write(
    input: *mut AscAirPlayInput,
    samples: *const f32,
    frames: usize,
) -> usize {
    if input.is_null() {
        return 0;
    }
    let Some(count) = stereo_samples(frames) else {
        return 0;
    };
    // SAFETY: by the caller's contract `samples` is null or holds `count`
    // floats that nothing writes during the call.
    let Some(block) = (unsafe { host_slice(samples, count) }) else {
        return 0;
    };
    // SAFETY: the handle is live; writes run on one thread at a time and
    // borrow only the `feed` field, which rate requests never touch.
    let requested = unsafe { &(*input).requested }.load(Ordering::Acquire);
    // SAFETY: as above.
    let feed = unsafe { &mut (*input).feed };
    feed.write(block, requested)
}

/// Frees an input handle. Sets `*input` to null, so destroying through the
/// same pointer again does nothing.
///
/// # Safety
///
/// `input` is null or points to a pointer that is null or was returned by
/// [`asc_airplay_take_input`] and not destroyed yet; no other call on that
/// handle runs now or later.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn asc_airplay_input_destroy(input: *mut *mut AscAirPlayInput) {
    if input.is_null() {
        return;
    }
    // SAFETY: by the caller's contract `input` points to a readable and
    // writable handle pointer.
    let handle = unsafe { input.replace(ptr::null_mut()) };
    if handle.is_null() {
        return;
    }
    // SAFETY: the pointer came from Box::into_raw in asc_airplay_take_input,
    // and by the caller's contract it is destroyed once, with no call in
    // progress.
    drop(unsafe { Box::from_raw(handle) });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::CStr;
    use std::net::{IpAddr, Ipv4Addr};
    use std::time::{Duration, Instant};

    use std::io::{self, Read, Write};
    use std::net::{TcpListener, TcpStream, UdpSocket};
    use std::sync::atomic::AtomicBool;
    use std::sync::{Arc, Mutex};
    use std::thread::JoinHandle;

    use audiosync_airplay::discovery::{BrowseEvent, Browser, ScopedAddr, ServiceRecord};
    use audiosync_airplay::rtsp::{Message, Method, Parser, Response};

    /// A browser that hands over a fixed list of events, one per poll.
    struct Script(Vec<BrowseEvent>);

    impl Browser for Script {
        fn poll_event(&mut self) -> Option<BrowseEvent> {
            (!self.0.is_empty()).then(|| self.0.remove(0))
        }
    }

    /// A RAOP record at a documentation address.
    fn raop(id: &str, name: &str, txt: &[&str]) -> ServiceRecord {
        ServiceRecord {
            kind: ServiceKind::Raop,
            instance: format!("{id}@{name}"),
            host: format!("{}.local", name.replace(' ', "-")),
            port: 7000,
            txt: txt.iter().map(|s| s.as_bytes().to_vec()).collect(),
            addresses: vec![ScopedAddr::new(IpAddr::V4(Ipv4Addr::new(192, 0, 2, 20)))],
        }
    }

    const OPEN: &[&str] = &[
        "txtvers=1",
        "ch=2",
        "cn=0,1",
        "et=0",
        "sr=44100",
        "ss=16",
        "tp=UDP",
        "am=Speaker1,1",
    ];

    const APPLE_TV: &[&str] = &[
        "txtvers=1",
        "ch=2",
        "cn=0,1",
        "et=0,3,5",
        "sr=44100",
        "ss=16",
        "tp=UDP",
        "am=AppleTV3,2",
        "sf=0x8",
    ];

    const PASSWORD: &[&str] = &[
        "txtvers=1",
        "ch=2",
        "cn=0,1",
        "et=0",
        "sr=44100",
        "ss=16",
        "tp=UDP",
        "pw=true",
    ];

    const RSA_ONLY: &[&str] = &["txtvers=1", "cn=0,1", "et=1", "tp=UDP"];

    /// An `_airplay._tcp`-only device with an AirPlay 2 features bit.
    fn airplay2_only() -> ServiceRecord {
        ServiceRecord {
            kind: ServiceKind::AirPlay,
            instance: "Living Room".to_owned(),
            host: "living.local".to_owned(),
            port: 7000,
            txt: vec![
                b"deviceid=02:00:5E:00:53:05".to_vec(),
                b"features=0x0,0x40".to_vec(),
                b"model=AudioAccessory5,1".to_vec(),
            ],
            addresses: vec![ScopedAddr::new(IpAddr::V4(Ipv4Addr::new(192, 0, 2, 21)))],
        }
    }

    /// A receiver event with its strings copied out.
    #[derive(Debug, Clone, PartialEq, Eq)]
    struct Seen {
        event: i32,
        id: String,
        name: String,
        model: String,
        availability: i32,
        unsupported: i32,
        flags: u32,
    }

    fn text(pointer: *const c_char) -> String {
        assert!(!pointer.is_null());
        // SAFETY: a string the handle owns until the next call on it.
        unsafe { CStr::from_ptr(pointer) }
            .to_str()
            .expect("UTF-8")
            .to_owned()
    }

    fn empty() -> AscAirPlayReceiver {
        AscAirPlayReceiver {
            event: 0,
            id: ptr::null(),
            name: ptr::null(),
            model: ptr::null(),
            availability: -1,
            unsupported: -1,
            flags: 0,
        }
    }

    /// The next `count` receiver events, waiting at most 10 s for them.
    fn next(handle: *mut AscAirPlay, count: usize) -> Vec<Seen> {
        let mut seen = Vec::new();
        let until = Instant::now() + Duration::from_secs(10);
        while seen.len() < count && Instant::now() < until {
            let mut receiver = empty();
            // SAFETY: a live handle, used by this thread only; a local out struct.
            if unsafe { asc_airplay_next_receiver(handle, &mut receiver) } {
                seen.push(Seen {
                    event: receiver.event,
                    id: text(receiver.id),
                    name: text(receiver.name),
                    model: text(receiver.model),
                    availability: receiver.availability,
                    unsupported: receiver.unsupported,
                    flags: receiver.flags,
                });
            } else {
                std::thread::sleep(Duration::from_millis(10));
            }
        }
        seen
    }

    fn start_with(handle: *mut AscAirPlay, events: Vec<BrowseEvent>) {
        // SAFETY: a live handle, used by this thread only.
        let airplay = unsafe { &*handle };
        airplay
            .sender
            .start_discovery_with(Box::new(Script(events)));
    }

    #[test]
    fn null_handles_and_null_outputs_are_harmless() {
        let mut receiver = empty();
        // SAFETY: null handles and pointers are part of the contract.
        unsafe {
            asc_airplay_destroy(ptr::null_mut());
            let mut none: *mut AscAirPlay = ptr::null_mut();
            asc_airplay_destroy(&mut none);
            assert!(none.is_null());
            assert!(!asc_airplay_start_discovery(ptr::null_mut()));
            asc_airplay_stop_discovery(ptr::null_mut());
            assert!(!asc_airplay_next_receiver(ptr::null_mut(), &mut receiver));
        }
        assert_eq!(receiver.event, 0, "untouched");

        let mut handle = asc_airplay_create();
        assert!(!handle.is_null());
        // SAFETY: a live handle and a null out pointer.
        assert!(!unsafe { asc_airplay_next_receiver(handle, ptr::null_mut()) });
        // Nothing found without discovery.
        // SAFETY: a live handle and a local out struct.
        assert!(!unsafe { asc_airplay_next_receiver(handle, &mut receiver) });
        assert_eq!(receiver.event, 0, "untouched");
        // SAFETY: destroyed once; the second call sees the nulled pointer.
        unsafe {
            asc_airplay_destroy(&mut handle);
            assert!(handle.is_null());
            asc_airplay_destroy(&mut handle);
        }
    }

    #[test]
    fn discovered_receivers_cross_the_boundary_with_their_verdicts() {
        let mut handle = asc_airplay_create();
        assert!(!handle.is_null());
        start_with(
            handle,
            vec![
                BrowseEvent::Resolved(raop("02005E005301", "Kitchen", OPEN)),
                BrowseEvent::Resolved(raop("02005E005302", "Den TV", APPLE_TV)),
                BrowseEvent::Resolved(raop("02005E005303", "Office", PASSWORD)),
                BrowseEvent::Resolved(raop("02005E005304", "Old", RSA_ONLY)),
                BrowseEvent::Resolved(airplay2_only()),
            ],
        );
        let mut seen = next(handle, 5);
        seen.sort_by(|a, b| a.id.cmp(&b.id));
        let raop_only = AIRPLAY_FLAG_RAOP;
        assert_eq!(
            seen,
            [
                Seen {
                    event: AIRPLAY_APPEARED,
                    id: "02005E005301".to_owned(),
                    name: "Kitchen".to_owned(),
                    model: "Speaker1,1".to_owned(),
                    availability: AIRPLAY_AVAILABLE,
                    unsupported: AIRPLAY_REASON_NONE,
                    flags: raop_only,
                },
                Seen {
                    event: AIRPLAY_APPEARED,
                    id: "02005E005302".to_owned(),
                    name: "Den TV".to_owned(),
                    model: "AppleTV3,2".to_owned(),
                    availability: AIRPLAY_NEEDS_PIN,
                    unsupported: AIRPLAY_REASON_NONE,
                    flags: raop_only
                        | AIRPLAY_FLAG_APPLE_TV
                        | AIRPLAY_FLAG_PAIRING
                        | AIRPLAY_FLAG_LEGACY_PAIRING,
                },
                Seen {
                    event: AIRPLAY_APPEARED,
                    id: "02005E005303".to_owned(),
                    name: "Office".to_owned(),
                    model: String::new(),
                    availability: AIRPLAY_NEEDS_PASSWORD,
                    unsupported: AIRPLAY_REASON_NONE,
                    flags: raop_only | AIRPLAY_FLAG_PASSWORD,
                },
                Seen {
                    event: AIRPLAY_APPEARED,
                    id: "02005E005304".to_owned(),
                    name: "Old".to_owned(),
                    model: String::new(),
                    availability: AIRPLAY_UNSUPPORTED,
                    unsupported: AIRPLAY_REASON_ENCRYPTION,
                    flags: raop_only,
                },
                Seen {
                    event: AIRPLAY_APPEARED,
                    id: "02005E005305".to_owned(),
                    name: "Living Room".to_owned(),
                    model: "AudioAccessory5,1".to_owned(),
                    availability: AIRPLAY_AIRPLAY2_ONLY,
                    unsupported: AIRPLAY_REASON_AIRPLAY2_ONLY,
                    flags: AIRPLAY_FLAG_AIRPLAY | AIRPLAY_FLAG_AIRPLAY2,
                },
            ]
        );
        // SAFETY: destroyed once, by this thread.
        unsafe { asc_airplay_destroy(&mut handle) };
    }

    #[test]
    fn a_receiver_that_leaves_and_every_one_after_a_stop_is_reported_gone() {
        let mut handle = asc_airplay_create();
        start_with(
            handle,
            vec![
                BrowseEvent::Resolved(raop("02005E005311", "Kitchen", OPEN)),
                BrowseEvent::Resolved(raop("02005E005312", "Bedroom", OPEN)),
                BrowseEvent::Removed {
                    kind: ServiceKind::Raop,
                    instance: "02005E005311@Kitchen".to_owned(),
                },
            ],
        );
        let seen = next(handle, 3);
        let events: Vec<(i32, &str, &str)> = seen
            .iter()
            .map(|s| (s.event, s.id.as_str(), s.name.as_str()))
            .collect();
        assert!(
            events.contains(&(AIRPLAY_GONE, "02005E005311", "")),
            "{events:?}"
        );
        let gone = seen.iter().find(|s| s.event == AIRPLAY_GONE).unwrap();
        assert_eq!(
            (
                gone.model.as_str(),
                gone.availability,
                gone.unsupported,
                gone.flags
            ),
            ("", AIRPLAY_AVAILABLE, AIRPLAY_REASON_NONE, 0)
        );

        // SAFETY: a live handle, used by this thread only.
        unsafe { asc_airplay_stop_discovery(handle) };
        let after = next(handle, 1);
        assert_eq!(
            after
                .iter()
                .map(|s| (s.event, s.id.as_str()))
                .collect::<Vec<_>>(),
            [(AIRPLAY_GONE, "02005E005312")]
        );
        // SAFETY: destroyed once, by this thread.
        unsafe { asc_airplay_destroy(&mut handle) };
    }

    #[test]
    fn ids_names_and_strings_follow_the_hosts_conventions() {
        assert_eq!(
            receiver_id(&DeviceKey::Instance(ServiceKind::Raop, "odd".to_owned())),
            "raop:odd"
        );
        assert_eq!(
            receiver_id(&DeviceKey::Instance(
                ServiceKind::AirPlay,
                "Living Room".to_owned()
            )),
            "airplay:Living Room"
        );
        assert_eq!(c_string("a\0b").to_str(), Ok("a\u{FFFD}b"));
    }

    #[test]
    fn live_discovery_starts_or_fails_cleanly_and_destroy_is_prompt() {
        let mut handle = asc_airplay_create();
        assert!(!handle.is_null());
        // A runner without multicast may refuse; either way the handle stays
        // usable and its destroy returns within the sender's 2 s bound.
        // SAFETY: a live handle, used by this thread only.
        let _ = unsafe { asc_airplay_start_discovery(handle) };
        let _ = next(handle, 0);
        let started = Instant::now();
        // SAFETY: destroyed once, by this thread.
        unsafe { asc_airplay_destroy(&mut handle) };
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "{:?}",
            started.elapsed()
        );
        assert!(handle.is_null());
    }
    // ---- Speakers, their events and the audio input ----

    /// A sender event with its strings copied out.
    #[derive(Debug, Clone, PartialEq)]
    struct Event {
        kind: i32,
        id: String,
        state: i32,
        error: i32,
        unsupported: i32,
        retry_ms: i64,
        latency_ms: f64,
        receiver_latency_frames: i64,
        restarted: bool,
        detail: String,
    }

    fn blank_event() -> AscAirPlayEvent {
        AscAirPlayEvent {
            kind: 0,
            id: ptr::null(),
            state: 0,
            error: 0,
            unsupported: 0,
            retry_ms: 0,
            latency_ms: 0.0,
            receiver_latency_frames: 0,
            restarted: false,
            detail: ptr::null(),
        }
    }

    /// Sender events, waiting at most 10 s, until `done` holds for the
    /// events taken so far.
    fn events_until(handle: *mut AscAirPlay, done: impl Fn(&[Event]) -> bool) -> Vec<Event> {
        let mut seen = Vec::new();
        let until = Instant::now() + Duration::from_secs(10);
        while !done(&seen) && Instant::now() < until {
            let mut raw = blank_event();
            // SAFETY: a live handle, used by this thread only; a local out struct.
            if unsafe { asc_airplay_next_event(handle, &mut raw) } {
                seen.push(Event {
                    kind: raw.kind,
                    id: text(raw.id),
                    state: raw.state,
                    error: raw.error,
                    unsupported: raw.unsupported,
                    retry_ms: raw.retry_ms,
                    latency_ms: raw.latency_ms,
                    receiver_latency_frames: raw.receiver_latency_frames,
                    restarted: raw.restarted,
                    detail: text(raw.detail),
                });
            } else {
                std::thread::sleep(Duration::from_millis(10));
            }
        }
        seen
    }

    fn has_state(events: &[Event], id: &str, state: i32) -> bool {
        events
            .iter()
            .any(|e| e.kind == AIRPLAY_EVENT_STATE && e.id == id && e.state == state)
    }

    /// Waits at most 10 s for `condition`.
    fn wait_for(mut condition: impl FnMut() -> bool) -> bool {
        let until = Instant::now() + Duration::from_secs(10);
        while Instant::now() < until {
            if condition() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        condition()
    }

    /// A RAOP record of a receiver that can play, on 127.0.0.1 at `port`.
    fn loopback_record(id: &str, name: &str, port: u16) -> ServiceRecord {
        ServiceRecord {
            port,
            addresses: vec![ScopedAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST))],
            ..raop(id, name, OPEN)
        }
    }

    /// Requests a [`Loopback`] received: method and body.
    type Requests = Arc<Mutex<Vec<(Method, String)>>>;

    /// A receiver on 127.0.0.1 that answers every RTSP request with 200 and
    /// the headers its method calls for, and keeps each request's method and
    /// body. Its UDP ports take the audio and are never read.
    struct Loopback {
        port: u16,
        requests: Requests,
        stop: Arc<AtomicBool>,
        thread: Option<JoinHandle<()>>,
    }

    impl Loopback {
        fn start() -> Loopback {
            let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("a TCP port");
            listener.set_nonblocking(true).expect("non-blocking");
            let port = listener.local_addr().expect("its address").port();
            let udp: Vec<UdpSocket> = (0..3)
                .map(|_| UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).expect("a UDP port"))
                .collect();
            let ports: Vec<u16> = udp
                .iter()
                .map(|socket| socket.local_addr().expect("its address").port())
                .collect();
            let transport = format!(
                "RTP/AVP/UDP;unicast;mode=record;server_port={};control_port={};timing_port={}",
                ports[0], ports[1], ports[2]
            );
            let requests = Requests::default();
            let stop = Arc::new(AtomicBool::new(false));
            let thread = {
                let (requests, stop) = (requests.clone(), stop.clone());
                std::thread::spawn(move || {
                    let _udp = udp;
                    serve(&listener, &transport, &requests, &stop);
                })
            };
            Loopback {
                port,
                requests,
                stop,
                thread: Some(thread),
            }
        }

        fn methods(&self) -> Vec<Method> {
            let requests = self.requests.lock().unwrap();
            requests.iter().map(|(method, _)| *method).collect()
        }

        fn bodies(&self, wanted: Method) -> Vec<String> {
            let requests = self.requests.lock().unwrap();
            requests
                .iter()
                .filter(|(method, _)| *method == wanted)
                .map(|(_, body)| body.clone())
                .collect()
        }
    }

    impl Drop for Loopback {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Relaxed);
            if let Some(thread) = self.thread.take() {
                let _ = thread.join();
            }
        }
    }

    fn serve(listener: &TcpListener, transport: &str, requests: &Requests, stop: &AtomicBool) {
        let mut connections: Vec<(TcpStream, Parser, bool)> = Vec::new();
        while !stop.load(Ordering::Relaxed) {
            if let Ok((stream, _)) = listener.accept() {
                stream.set_nonblocking(true).expect("non-blocking");
                connections.push((stream, Parser::new(), true));
            }
            for (stream, parser, open) in connections.iter_mut().filter(|c| c.2) {
                let mut buffer = [0u8; 4096];
                match stream.read(&mut buffer) {
                    Ok(0) => *open = false,
                    Ok(read) => parser.push(&buffer[..read]),
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
                    Err(_) => *open = false,
                }
                while let Ok(Some(Message::Request(request))) = parser.next_message() {
                    let body = String::from_utf8_lossy(&request.body).into_owned();
                    requests.lock().unwrap().push((request.method, body));
                    let mut response = Response::new(200, "OK");
                    if let Some(cseq) = request.headers.get("CSeq") {
                        response.headers.push("CSeq", cseq);
                    }
                    match request.method {
                        Method::Options => response.headers.push(
                            "Public",
                            "ANNOUNCE, SETUP, RECORD, FLUSH, TEARDOWN, OPTIONS, SET_PARAMETER",
                        ),
                        Method::Setup => {
                            response.headers.push("Transport", transport);
                            response.headers.push("Session", "1");
                        }
                        Method::Record => response.headers.push("Audio-Latency", "2205"),
                        _ => {}
                    }
                    let _ = stream.write_all(&response.encode());
                }
            }
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    /// A port nothing listens on: one the system just handed out and took
    /// back.
    fn closed_port() -> u16 {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("a TCP port");
        listener.local_addr().expect("its address").port()
    }

    /// A handle whose discovery listed `records`, each taken through
    /// `asc_airplay_next_receiver`.
    fn listing(records: Vec<ServiceRecord>) -> *mut AscAirPlay {
        let handle = asc_airplay_create();
        assert!(!handle.is_null());
        let count = records.len();
        start_with(
            handle,
            records.into_iter().map(BrowseEvent::Resolved).collect(),
        );
        assert_eq!(next(handle, count).len(), count);
        handle
    }

    #[test]
    fn speaker_calls_with_null_or_unknown_arguments_do_nothing() {
        let mut event = blank_event();
        let id = c"02005E005301";
        // SAFETY: null handles and pointers are part of the contract.
        unsafe {
            assert!(!asc_airplay_next_event(ptr::null_mut(), &mut event));
            assert!(!asc_airplay_add_speaker(ptr::null_mut(), id.as_ptr(), 50.0));
            assert!(!asc_airplay_remove_speaker(ptr::null_mut(), id.as_ptr()));
            assert!(!asc_airplay_set_volume(ptr::null_mut(), id.as_ptr(), 50.0));
            asc_airplay_set_master(ptr::null_mut(), 50.0);
            asc_airplay_restart_timeline(ptr::null_mut());
            assert_eq!(asc_airplay_latency_ms(ptr::null()), 0.0);
            assert!(asc_airplay_take_input(ptr::null_mut()).is_null());
            assert!(!asc_airplay_input_set_rate(ptr::null_mut(), 48_000.0));
            assert_eq!(asc_airplay_input_write(ptr::null_mut(), ptr::null(), 0), 0);
            asc_airplay_input_destroy(ptr::null_mut());
            let mut none: *mut AscAirPlayInput = ptr::null_mut();
            asc_airplay_input_destroy(&mut none);
        }
        assert_eq!(event.kind, 0, "untouched");

        let mut handle = listing(vec![raop("02005E005301", "Kitchen", OPEN)]);
        // SAFETY: a live handle, used by this thread only; C string literals.
        unsafe {
            assert!(!asc_airplay_next_event(handle, ptr::null_mut()));
            assert!(!asc_airplay_add_speaker(handle, ptr::null(), 50.0));
            assert!(
                !asc_airplay_add_speaker(handle, c"02005E005399".as_ptr(), 50.0),
                "not listed"
            );
            assert!(
                !asc_airplay_add_speaker(handle, id.as_ptr(), f64::NAN),
                "no volume"
            );
            assert!(
                !asc_airplay_remove_speaker(handle, id.as_ptr()),
                "never added"
            );
            assert!(
                !asc_airplay_set_volume(handle, id.as_ptr(), 50.0),
                "never added"
            );
            asc_airplay_set_master(handle, f64::NAN);
            asc_airplay_destroy(&mut handle);
        }
    }

    #[test]
    fn the_first_event_is_the_timeline_with_the_base_latency() {
        let mut handle = asc_airplay_create();
        let events = events_until(handle, |seen| !seen.is_empty());
        // SAFETY: a live handle, used by this thread only.
        let latency = unsafe { asc_airplay_latency_ms(handle) };
        assert!(
            (latency - 1750.0).abs() < 0.01,
            "Lf 77 175 frames at 44.1 kHz (P98): {latency}"
        );
        assert_eq!(events.len(), 1, "{events:?}");
        assert!((events[0].latency_ms - 1750.0).abs() < 0.01, "{events:?}");
        assert_eq!(
            events[0],
            Event {
                kind: AIRPLAY_EVENT_TIMELINE,
                id: String::new(),
                state: 0,
                error: 0,
                unsupported: AIRPLAY_REASON_NONE,
                retry_ms: -1,
                latency_ms: events[0].latency_ms,
                receiver_latency_frames: -1,
                restarted: false,
                detail: String::new(),
            }
        );
        // SAFETY: a live handle, used by this thread only.
        unsafe { asc_airplay_restart_timeline(handle) };
        let restarted = events_until(handle, |seen| !seen.is_empty());
        assert_eq!(
            restarted
                .iter()
                .map(|e| (e.kind, e.restarted))
                .collect::<Vec<_>>(),
            [(AIRPLAY_EVENT_TIMELINE, true)],
            "P143"
        );
        assert!((restarted[0].latency_ms - 1750.0).abs() < 0.01);
        // SAFETY: destroyed once, by this thread.
        unsafe { asc_airplay_destroy(&mut handle) };
    }

    #[test]
    fn a_speaker_that_does_not_answer_reports_why_and_when_it_retries() {
        let id = "02005E005331";
        let mut handle = listing(vec![loopback_record(id, "Attic", closed_port())]);
        let c_id = c_string(id);
        // SAFETY: a live handle, used by this thread only; a C string.
        assert!(unsafe { asc_airplay_add_speaker(handle, c_id.as_ptr(), 40.0) });
        let events = events_until(handle, |seen| has_state(seen, id, AIRPLAY_STATE_RETRYING));
        assert!(
            has_state(&events, id, AIRPLAY_STATE_CONNECTING),
            "{events:?}"
        );
        let error = events
            .iter()
            .find(|e| e.kind == AIRPLAY_EVENT_ERROR)
            .unwrap_or_else(|| panic!("no error in {events:?}"));
        assert_eq!(
            (error.id.as_str(), error.error, error.unsupported),
            (id, AIRPLAY_ERROR_NOT_RESPONDING, AIRPLAY_REASON_NONE)
        );
        assert!(error.retry_ms >= 0, "retried by the sender (P142)");
        assert!(
            ["Unreachable", "Timeout"]
                .iter()
                .any(|kind| error.detail.starts_with(kind)),
            "{}",
            error.detail
        );
        let retrying = events
            .iter()
            .find(|e| e.kind == AIRPLAY_EVENT_STATE && e.state == AIRPLAY_STATE_RETRYING)
            .unwrap();
        assert!(retrying.retry_ms >= 0);

        // SAFETY: a live handle, used by this thread only; a C string.
        assert!(unsafe { asc_airplay_remove_speaker(handle, c_id.as_ptr()) });
        let stopped = events_until(handle, |seen| has_state(seen, id, AIRPLAY_STATE_STOPPED));
        assert!(
            has_state(&stopped, id, AIRPLAY_STATE_STOPPED),
            "{stopped:?}"
        );
        // SAFETY: destroyed once, by this thread.
        unsafe { asc_airplay_destroy(&mut handle) };
    }

    #[test]
    fn a_speaker_plays_reports_its_latency_and_takes_volume_and_master() {
        let receiver = Loopback::start();
        let id = "02005E005321";
        let mut handle = listing(vec![loopback_record(id, "Desk", receiver.port)]);
        let c_id = c_string(id);
        // SAFETY: a live handle, used by this thread only; a C string.
        unsafe {
            asc_airplay_set_master(handle, 50.0);
            assert!(asc_airplay_add_speaker(handle, c_id.as_ptr(), 100.0));
        }
        let events = events_until(handle, |seen| {
            seen.iter()
                .any(|e| e.kind == AIRPLAY_EVENT_LATENCY && e.id == id)
        });
        assert_eq!(events[0].kind, AIRPLAY_EVENT_TIMELINE, "{events:?}");
        let states: Vec<i32> = events
            .iter()
            .filter(|e| e.kind == AIRPLAY_EVENT_STATE && e.id == id)
            .map(|e| e.state)
            .collect();
        assert_eq!(
            states,
            [AIRPLAY_STATE_CONNECTING, AIRPLAY_STATE_PLAYING],
            "{events:?}"
        );
        // L + d_s with no delay of its own, and the receiver's Audio-Latency
        // (P113).
        let latency = events.last().unwrap();
        assert!((latency.latency_ms - 1750.0).abs() < 0.01, "{latency:?}");
        assert_eq!(latency.receiver_latency_frames, 2205);
        // Its own position times the master: 100 % of 50 % is half way
        // down the −30 to 0 dB range (P119).
        assert!(
            wait_for(|| receiver
                .bodies(Method::SetParameter)
                .contains(&"volume: -15.000000\r\n".to_owned())),
            "{:?}",
            receiver.bodies(Method::SetParameter)
        );
        // SAFETY: a live handle, used by this thread only; a C string.
        unsafe { assert!(asc_airplay_set_volume(handle, c_id.as_ptr(), 80.0)) };
        assert!(wait_for(|| receiver
            .bodies(Method::SetParameter)
            .contains(&"volume: -18.000000\r\n".to_owned())));
        // SAFETY: a live handle, used by this thread only.
        unsafe { asc_airplay_set_master(handle, 100.0) };
        assert!(wait_for(|| receiver
            .bodies(Method::SetParameter)
            .contains(&"volume: -6.000000\r\n".to_owned())));

        // SAFETY: a live handle, used by this thread only; a C string.
        unsafe {
            assert!(asc_airplay_remove_speaker(handle, c_id.as_ptr()));
            assert!(
                !asc_airplay_remove_speaker(handle, c_id.as_ptr()),
                "removed once"
            );
        }
        let stopped = events_until(handle, |seen| has_state(seen, id, AIRPLAY_STATE_STOPPED));
        assert!(
            has_state(&stopped, id, AIRPLAY_STATE_STOPPED),
            "{stopped:?}"
        );
        assert!(receiver.methods().contains(&Method::Teardown));
        // SAFETY: destroyed once, by this thread.
        unsafe { asc_airplay_destroy(&mut handle) };
    }

    #[test]
    fn receiver_and_event_strings_stay_valid_independently() {
        let mut handle = asc_airplay_create();
        start_with(
            handle,
            vec![BrowseEvent::Resolved(raop("02005E005341", "Hall", OPEN))],
        );
        let mut event = blank_event();
        let until = Instant::now() + Duration::from_secs(10);
        // SAFETY: a live handle, used by this thread only; a local out struct.
        while !unsafe { asc_airplay_next_event(handle, &mut event) } && Instant::now() < until {
            std::thread::sleep(Duration::from_millis(10));
        }
        let mut receiver = empty();
        let until = Instant::now() + Duration::from_secs(10);
        // SAFETY: as above.
        while !unsafe { asc_airplay_next_receiver(handle, &mut receiver) } && Instant::now() < until
        {
            std::thread::sleep(Duration::from_millis(10));
        }
        // Taking a receiver leaves the last event's strings alone.
        assert_eq!(event.kind, AIRPLAY_EVENT_TIMELINE);
        assert_eq!(
            (text(event.id), text(event.detail)),
            (String::new(), String::new())
        );
        assert_eq!(text(receiver.name), "Hall");
        // SAFETY: destroyed once, by this thread.
        unsafe { asc_airplay_destroy(&mut handle) };
    }

    #[test]
    fn the_input_is_handed_over_once_and_resamples_to_44_1_khz() {
        let mut handle = asc_airplay_create();
        // SAFETY: a live handle, used by this thread only.
        let mut input = unsafe { asc_airplay_take_input(handle) };
        assert!(!input.is_null());
        // SAFETY: as above.
        assert!(unsafe { asc_airplay_take_input(handle) }.is_null(), "once");
        let block = vec![0.25f32; 4800 * 2];
        // SAFETY: a live input, written by this thread only; `block` holds
        // 4800 stereo frames.
        unsafe {
            assert_eq!(
                asc_airplay_input_write(input, block.as_ptr(), 4800),
                0,
                "dropped until a rate is set"
            );
            for rate in [f64::NAN, 0.0, 7_999.0, 384_001.0, 1e12, -48_000.0] {
                assert!(!asc_airplay_input_set_rate(input, rate), "{rate}");
            }
            assert!(asc_airplay_input_set_rate(input, 48_000.0));
            // 100 ms at 48 kHz become 100 ms at 44.1 kHz: ceil(4800 × 44 100
            // / 48 000) frames (S85).
            assert_eq!(asc_airplay_input_write(input, block.as_ptr(), 4800), 4410);
            assert_eq!(asc_airplay_input_write(input, block.as_ptr(), 4800), 4410);
            // A null buffer with frames, or an odd count of no frames, is
            // nothing.
            assert_eq!(asc_airplay_input_write(input, ptr::null(), 10), 0);
            assert_eq!(asc_airplay_input_write(input, block.as_ptr(), 0), 0);
            assert!(asc_airplay_input_set_rate(input, 44_100.4));
            assert_eq!(asc_airplay_input_write(input, block.as_ptr(), 441), 441);
        }
        // The input outlives its sender; its audio then goes nowhere.
        // SAFETY: each handle destroyed once, by this thread.
        unsafe {
            asc_airplay_destroy(&mut handle);
            assert_eq!(asc_airplay_input_write(input, block.as_ptr(), 441), 441);
            asc_airplay_input_destroy(&mut input);
            assert!(input.is_null());
            asc_airplay_input_destroy(&mut input);
        }
    }

    #[test]
    fn speaker_volume_spreads_the_product_of_both_sliders_over_30_db() {
        assert_eq!(speaker_volume(100.0, 100.0), Volume::Db(0.0));
        assert_eq!(speaker_volume(50.0, 100.0), Volume::Db(-15.0));
        assert_eq!(speaker_volume(100.0, 50.0), Volume::Db(-15.0));
        assert_eq!(speaker_volume(0.0, 100.0), Volume::Muted);
        assert_eq!(speaker_volume(100.0, 0.0), Volume::Muted);
        assert_eq!(speaker_volume(150.0, 200.0), Volume::Db(0.0), "clamped");
        assert_eq!(speaker_volume(-5.0, 100.0), Volume::Muted, "clamped");
    }

    #[test]
    fn user_reasons_have_their_codes() {
        assert_eq!(
            error_code(UserReason::Unsupported(UnsupportedReason::Codec)),
            (AIRPLAY_ERROR_UNSUPPORTED, AIRPLAY_REASON_CODEC)
        );
        let codes: Vec<i32> = [
            UserReason::NotResponding,
            UserReason::ConnectionLost,
            UserReason::Busy,
            UserReason::Refused,
            UserReason::Incompatible,
            UserReason::PasswordRequired,
            UserReason::WrongPassword,
            UserReason::PairingRequired,
            UserReason::Unsupported(UnsupportedReason::AirPlay2Only),
        ]
        .into_iter()
        .map(|reason| error_code(reason).0)
        .collect();
        assert_eq!(codes, (1..=9).collect::<Vec<i32>>());
    }
}
