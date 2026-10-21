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
//! to 44.1 kHz (P73) without allocating, locking or logging (P6).
//!
//! Pairing and passwords: the host opens the pairing store
//! ([`asc_airplay_open_pairings`], see [`crate::airplay_pairings`]) before
//! discovery starts. An Apple TV it holds credentials for is listed as
//! available, and every session to it verifies with them (P55); one it
//! doesn't is listed as needing a PIN. The host starts a PIN pairing
//! ([`asc_airplay_start_pairing`]) only when the user turns such a receiver
//! on, asks the user for the PIN when [`AIRPLAY_EVENT_PIN_NEEDED`] comes,
//! and submits it ([`asc_airplay_submit_pin`]); [`AIRPLAY_EVENT_PAIRED`]
//! follows once the credentials are stored, or
//! [`AIRPLAY_EVENT_PAIRING_FAILED`] with why, a wrong PIN included. A
//! receiver that turns its stored credentials down (it was reset) has them
//! forgotten and is listed as needing a PIN again. A password the host
//! hands over ([`asc_airplay_set_password`]) serves that receiver's
//! sessions and pairings until the handle is destroyed; it is never
//! stored, and forgotten when the receiver turns it down. PINs and
//! passwords cross this interface only inward, and credentials never do:
//! nothing here logs or returns a PIN, password, key or credential.
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
//!
//! Panics: a bug anywhere in the sender must not end the host. Every call
//! here runs its work behind [`std::panic::catch_unwind`] (the core's
//! release build unwinds): a panic inside one returns that call's failure
//! value (null, false or 0) and marks its handle dead, and a panic that
//! ends the sender's driver thread does the same, as does discovery's
//! browser dying (its mDNS daemon ended). A dead handle's calls do
//! nothing and return their failure values, except that
//! [`asc_airplay_next_event`] reports [`AIRPLAY_EVENT_FAILED`] once; the
//! host then destroys it and may create a new sender. A panic in an input
//! write marks the input and its sender dead. Destroying a dead handle
//! still frees it. The failure's detail says where the panic happened, as
//! a source file and line, never what it said (S205): a panic hook,
//! installed once by the first [`asc_airplay_create`] in front of the one
//! that was there before, notes each panic's location.

use std::cell::Cell;
use std::collections::{HashMap, VecDeque};
use std::ffi::{CStr, CString, c_char};
use std::panic::{self, AssertUnwindSafe};
use std::path::PathBuf;
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Once, OnceLock};
use std::time::{Duration, Instant};

use audiosync_airplay::audio::SAMPLE_RATE;
use audiosync_airplay::discovery::mdns::MdnsBrowser;
use audiosync_airplay::discovery::{Browser, Device, DeviceKey, ServiceKind};
use audiosync_airplay::error::{ErrorKind, UnsupportedReason, UserReason};
use audiosync_airplay::input::InputWriter;
use audiosync_airplay::metadata::TrackInfo;
use audiosync_airplay::pairing::{Credentials, PairingEvent};
use audiosync_airplay::secret::Secret;
use audiosync_airplay::sender::{
    AIRPLAY2_SWITCH_ENV, NowPlaying, ReceiverEvent, ReceiverInfo, Sender, SenderConfig,
    SenderEvent, SpeakerError, SpeakerEvent, SpeakerOptions, SpeakerState, airplay2_switch,
};
use audiosync_airplay::stream::{MAX_DELAY, SpeakerLatency};
use audiosync_airplay::volume::{MIN_DB, Volume};

use super::{host_slice, stereo_samples};
use crate::airplay_pairings::{Loaded, PairingStore};
use crate::fade::InputFade;
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
/// The sender stopped working: a bug ended one of its calls, its driver
/// thread or its discovery. It lists, plays and reports nothing more; the
/// host destroys it and may create a new one. Reported once, with no ID;
/// the detail says where the panic happened, or is "" (S205).
pub const AIRPLAY_EVENT_FAILED: i32 = 5;
/// The receiver of a PIN pairing shows a PIN (P48, P51): the host asks the
/// user for it and hands it to [`asc_airplay_submit_pin`]. The pairing
/// waits for it without a timeout.
pub const AIRPLAY_EVENT_PIN_NEEDED: i32 = 6;
/// A pairing ended because the receiver asks for a password
/// ([`AscAirPlayEvent::error`] [`AIRPLAY_ERROR_PASSWORD_REQUIRED`]) or turned
/// down the one given, which is then forgotten
/// ([`AIRPLAY_ERROR_WRONG_PASSWORD`]): the host asks the user, hands the
/// password over ([`asc_airplay_set_password`]) and starts the pairing again.
pub const AIRPLAY_EVENT_PASSWORD_NEEDED: i32 = 7;
/// A pairing ended without credentials ([`AscAirPlayEvent::error`]):
/// [`AIRPLAY_ERROR_WRONG_PIN`] when the receiver turned the PIN down (a new
/// pairing shows a new PIN, P52), [`AIRPLAY_ERROR_NOT_RESPONDING`] when it
/// didn't answer in time, [`AIRPLAY_ERROR_PAIRING_REQUIRED`] when it turned
/// the pairing down or cancelled it, or another reason. The detail says
/// more, for a debug log.
pub const AIRPLAY_EVENT_PAIRING_FAILED: i32 = 8;
/// A pairing succeeded and the receiver is listed as available again; its
/// credentials are in the store (P54). [`AscAirPlayEvent::detail`] is ""
/// when they were written to the file, or why they last only until the
/// handle is destroyed.
pub const AIRPLAY_EVENT_PAIRED: i32 = 9;

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
/// A pairing's receiver turned the PIN down (P52).
pub const AIRPLAY_ERROR_WRONG_PIN: i32 = 10;

/// What [`asc_airplay_open_pairings`] returns when it can't open a store:
/// a null or dead handle, or a path that isn't UTF-8.
pub const AIRPLAY_PAIRINGS_FAILED: i32 = -1;
/// The pairings file can't be read or isn't in the expected format: it is
/// ignored, and the next pairing replaces it.
pub const AIRPLAY_PAIRINGS_IGNORED: i32 = -2;

/// The largest hold-back [`asc_airplay_set_hold_back`] applies, in ms: the
/// sender's limit on a speaker's delay (P99).
pub const AIRPLAY_MAX_DELAY_MS: i32 = MAX_DELAY.as_millis() as i32;
/// A newer AudioSync wrote the pairings file: it is ignored and left as it
/// is, and pairings made now last until the handle is destroyed.
pub const AIRPLAY_PAIRINGS_NEWER: i32 = -3;

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
    /// An error event's error, or a failure event's panic location
    /// (S205), for a debug log (no secrets).
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
    Timeline {
        latency: Duration,
        restarted: bool,
    },
    /// The sender is dead ([`AIRPLAY_EVENT_FAILED`]).
    Failed,
    /// A pairing event for the host: its kind (`AIRPLAY_EVENT_PIN_NEEDED`
    /// to `AIRPLAY_EVENT_PAIRED`), the receiver ID, the error and
    /// unsupported codes and the detail. Never a secret.
    Pairing {
        kind: i32,
        id: String,
        error: i32,
        unsupported: i32,
        detail: String,
    },
}

/// An AirPlay sender behind an opaque C pointer.
pub struct AscAirPlay {
    sender: Sender,
    /// The stream's audio input, until [`asc_airplay_take_input`] hands it
    /// to the host.
    input: Option<InputWriter>,
    /// Receiver changes not taken yet.
    receivers: VecDeque<ReceiverEvent>,
    /// How many receiver changes were queued, and how many the host took:
    /// an event waits until the host took every change queued before it
    /// ([`asc_airplay_next_event`]).
    receivers_queued: u64,
    receivers_taken: u64,
    /// The strings of the last receiver handed out: ID, name, model.
    receiver_strings: [CString; 3],
    /// Speaker and timeline events not taken yet, each with the number of
    /// receiver changes queued before it.
    events: VecDeque<(u64, Pending)>,
    /// The strings of the last event handed out: ID, detail.
    event_strings: [CString; 2],
    /// The receivers discovery lists now, by ID, so the host adds speakers
    /// and starts pairings by ID.
    listed: HashMap<String, ReceiverInfo>,
    /// The speakers the host added, by ID.
    added: HashMap<String, Added>,
    /// The paired receivers' credentials; in memory until the host opens
    /// its file ([`asc_airplay_open_pairings`]).
    store: PairingStore,
    /// The passwords the host handed over, by receiver ID, for this
    /// handle's life only; wiped from memory when dropped.
    passwords: HashMap<String, Secret>,
    /// The receivers with a pairing running, by ID, with the key the
    /// sender knows each by: a receiver that leaves discovery while the
    /// user reads its PIN (the wait has no timeout, P51) still takes the
    /// PIN and is still cancelled.
    pairing: HashMap<String, DeviceKey>,
    /// The receivers whose cancelled pairing hasn't reported its end yet,
    /// by ID, each with the receiver of the pairing the host started since,
    /// if any, which starts once that end comes (or, when the cancelled
    /// pairing paired after all, ends there as paired). The sender's pairing
    /// events name the receiver only, so until then every one for it is the
    /// cancelled pairing's: it never reaches the host or ends the pairing
    /// the host has now, and the two pairings never run at once.
    ending: HashMap<String, Option<Device>>,
    /// The master position, 0 to 100.
    master: f64,
    /// Every speaker's delay d_s ([`asc_airplay_set_hold_back`]).
    hold_back: Duration,
    /// Whether a call on this handle or its input panicked, or discovery
    /// died, and where the panic happened; shared with the input.
    death: Arc<Death>,
    /// How many panics the process had noted when this handle was created
    /// ([`PANICS`]).
    panics_before: u64,
    /// [`AIRPLAY_EVENT_FAILED`] was handed out.
    failure_reported: bool,
    /// Opens the browser [`asc_airplay_start_discovery`] starts discovery
    /// with.
    open_browser: OpenBrowser,
}

/// Opens the browser discovery runs on; `None` when it can't start.
pub(crate) type OpenBrowser = fn() -> Option<Box<dyn Browser + Send>>;

/// Discovery's browser for the app: live multicast DNS (P7).
///
/// The core's tests never browse the real network: its multicast sockets on
/// every interface make the system firewall ask the user to allow the test
/// program. In them a handle [`asc_airplay_create`] makes fails to start
/// discovery, as on a host without multicast; a test that needs discovery
/// running picks its browser with [`create_with`].
fn live_browser() -> Option<Box<dyn Browser + Send>> {
    #[cfg(not(test))]
    let browser = MdnsBrowser::new().ok();
    #[cfg(test)]
    let browser = None::<MdnsBrowser>;
    Some(Box::new(browser?))
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
        // Only the AirPlay 2 path, still behind its switch, gives these; the
        // header has no codes for them yet.
        UnsupportedReason::Timing | UnsupportedReason::OneAirPlay2Session => AIRPLAY_REASON_NONE,
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
/// the Windows app decides. An Apple TV that asks for a PIN needs one
/// until it is `paired`; the legacy pairing's flags stay set once paired,
/// so they can't tell.
fn availability(info: &ReceiverInfo, paired: bool) -> (i32, i32) {
    let verdict = &info.device.verdict;
    match info.plan {
        Err(ErrorKind::Unsupported(UnsupportedReason::AirPlay2Only)) => {
            (AIRPLAY_AIRPLAY2_ONLY, AIRPLAY_REASON_AIRPLAY2_ONLY)
        }
        Err(ErrorKind::Unsupported(reason)) => (AIRPLAY_UNSUPPORTED, reason_code(reason)),
        Err(_) => (AIRPLAY_UNSUPPORTED, AIRPLAY_REASON_NONE),
        Ok(()) if verdict.legacy_pairing && !paired => (AIRPLAY_NEEDS_PIN, AIRPLAY_REASON_NONE),
        Ok(()) if verdict.needs_password => (AIRPLAY_NEEDS_PASSWORD, AIRPLAY_REASON_NONE),
        Ok(()) if verdict.needs_pairing && !verdict.legacy_pairing => {
            (AIRPLAY_NEEDS_PAIRING, AIRPLAY_REASON_NONE)
        }
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
    /// Moves every event the sender reported into the host's queues
    /// ([`AscAirPlay::take_in`]).
    fn pump(&mut self) {
        while let Some(event) = self.sender.poll_event() {
            self.take_in(event);
        }
    }

    /// Takes one sender event in, and keeps the list of receivers the host
    /// can add up to date. Speaker events the host doesn't use (warnings,
    /// methods, server, audio jack, volume reports) and what a cancelled
    /// pairing reports are passed over.
    fn take_in(&mut self, event: SenderEvent) {
        match event {
            SenderEvent::Receiver(event) => {
                match &event {
                    ReceiverEvent::Appeared(info) | ReceiverEvent::Changed(info) => {
                        self.listed
                            .insert(receiver_id(&info.device.key), (**info).clone());
                    }
                    ReceiverEvent::Gone(key) => {
                        self.listed.remove(&receiver_id(key));
                    }
                }
                self.queue_receiver(event);
            }
            SenderEvent::Speaker { key, event } => {
                let id = receiver_id(&key);
                if let SpeakerEvent::Error(SpeakerError { reason, .. }) = &event {
                    self.speaker_failed(&id, *reason);
                }
                if matches!(
                    event,
                    SpeakerEvent::State(_) | SpeakerEvent::Error(_) | SpeakerEvent::Latency(_)
                ) {
                    self.queue_event(Pending::Speaker(id, event));
                }
            }
            SenderEvent::Timeline {
                latency, restarted, ..
            } => self.queue_event(Pending::Timeline { latency, restarted }),
            SenderEvent::Pairing { key, event } => self.pairing_event(receiver_id(&key), event),
            // Discovery's browser died (a bug in it): the sender no
            // longer lists anything new, so it counts as dead too.
            SenderEvent::DiscoveryFailed => self.death.dead.store(true, Ordering::Release),
        }
    }

    /// A speaker's session ended with `reason`: a password it turned down
    /// is forgotten, and so are stored credentials it turned down (it was
    /// reset, or the pairing was removed on it), so it is listed as needing
    /// a PIN again and pairs anew.
    fn speaker_failed(&mut self, id: &str, reason: UserReason) {
        match reason {
            UserReason::WrongPassword => {
                self.passwords.remove(id);
            }
            UserReason::PairingRequired if self.store.contains(id) => {
                // When the file can't be written they are forgotten for
                // this handle all the same; the next pairing writes it.
                let _ = self.store.remove(id);
                self.relist(id);
            }
            _ => {}
        }
    }

    /// Takes in what a pairing reported.
    fn pairing_event(&mut self, id: String, event: PairingEvent) {
        if self.ending.contains_key(&id) {
            self.ending_pairing_event(&id, event);
            return;
        }
        let (kind, error, unsupported, detail) = match event {
            PairingEvent::PinRequested => (
                AIRPLAY_EVENT_PIN_NEEDED,
                0,
                AIRPLAY_REASON_NONE,
                String::new(),
            ),
            PairingEvent::Paired(credentials) => {
                self.pairing.remove(&id);
                let detail = self.store_paired(&id, credentials);
                (AIRPLAY_EVENT_PAIRED, 0, AIRPLAY_REASON_NONE, detail)
            }
            PairingEvent::Failed { error, wrong_pin } => {
                self.pairing.remove(&id);
                let reason = error.kind.user_reason();
                let detail = error.to_string();
                match reason {
                    _ if wrong_pin => (
                        AIRPLAY_EVENT_PAIRING_FAILED,
                        AIRPLAY_ERROR_WRONG_PIN,
                        AIRPLAY_REASON_NONE,
                        detail,
                    ),
                    UserReason::PasswordRequired | UserReason::WrongPassword => {
                        if reason == UserReason::WrongPassword {
                            self.passwords.remove(&id);
                        }
                        let (error, unsupported) = error_code(reason);
                        (AIRPLAY_EVENT_PASSWORD_NEEDED, error, unsupported, detail)
                    }
                    _ => {
                        let (error, unsupported) = error_code(reason);
                        (AIRPLAY_EVENT_PAIRING_FAILED, error, unsupported, detail)
                    }
                }
            }
            // The sender shut down: nothing to tell. (The end of a
            // pairing the host cancelled comes while it is `ending`.)
            PairingEvent::Stopped => return,
        };
        self.queue_event(Pending::Pairing {
            kind,
            id,
            error,
            unsupported,
            detail,
        });
    }

    /// Takes in what a pairing the host cancelled reported before its end
    /// (see `ending`). What it found out still counts: credentials it got
    /// are real, so they are stored and the receiver is listed again, and a
    /// password the receiver turned down is forgotten. Nothing reaches the
    /// host, and the pairing the host has now keeps its mark. Its end (it
    /// stopped, failed or paired: a pairing ends once) lets the pairing the
    /// host started since, if any, start; but when it paired, the receiver
    /// asks for no PIN any more, so that pairing never starts and ends there
    /// as paired: the host gets [`AIRPLAY_EVENT_PAIRED`], as from a pairing
    /// that paired. With no pairing started since, its success is stored
    /// only.
    fn ending_pairing_event(&mut self, id: &str, event: PairingEvent) {
        let paired = match event {
            PairingEvent::PinRequested => return,
            PairingEvent::Paired(credentials) => Some(self.store_paired(id, credentials)),
            PairingEvent::Failed { error, .. } => {
                if error.kind.user_reason() == UserReason::WrongPassword {
                    self.passwords.remove(id);
                }
                None
            }
            PairingEvent::Stopped => None,
        };
        let Some(Some(device)) = self.ending.remove(id) else {
            return;
        };
        match paired {
            Some(detail) => {
                self.pairing.remove(id);
                self.queue_event(Pending::Pairing {
                    kind: AIRPLAY_EVENT_PAIRED,
                    id: id.to_owned(),
                    error: 0,
                    unsupported: AIRPLAY_REASON_NONE,
                    detail,
                });
            }
            None => self.start_pairing_now(id, device),
        }
    }

    /// Stores the credentials a pairing with the receiver `id` got and
    /// lists the receiver again. Returns the detail for the host: empty, or
    /// why the pairings file couldn't be written (they last for this handle
    /// all the same).
    fn store_paired(&mut self, id: &str, credentials: Credentials) -> String {
        let detail = match self.store.insert(id, credentials) {
            Ok(()) => String::new(),
            Err(error) => format!("the pairings file couldn't be written: {error}"),
        };
        self.relist(id);
        detail
    }

    /// Has the sender start a pairing with `device`, the receiver `id`,
    /// with the password handed over for it, if any.
    fn start_pairing_now(&mut self, id: &str, device: Device) {
        let password = self.passwords.get(id).cloned();
        self.sender.start_pairing_with_password(device, password);
    }

    /// Queues a receiver change for the host.
    fn queue_receiver(&mut self, event: ReceiverEvent) {
        self.receivers.push_back(event);
        self.receivers_queued += 1;
    }

    /// Queues an event for the host, behind the receiver changes queued so
    /// far.
    fn queue_event(&mut self, pending: Pending) {
        self.events.push_back((self.receivers_queued, pending));
    }

    /// Reports a listed receiver again, as changed, after its pairing
    /// changed: its availability follows the store.
    fn relist(&mut self, id: &str) {
        if let Some(info) = self.listed.get(id) {
            let changed = ReceiverEvent::Changed(Box::new(info.clone()));
            self.queue_receiver(changed);
        }
    }

    /// Keeps the strings of a receiver change and describes it with
    /// pointers to them.
    fn report(&mut self, event: ReceiverEvent) -> AscAirPlayReceiver {
        let (code, id, name, model, (availability, unsupported), flags) = match &event {
            ReceiverEvent::Appeared(info) | ReceiverEvent::Changed(info) => {
                let id = receiver_id(&info.device.key);
                let paired = self.store.contains(&id);
                (
                    if matches!(event, ReceiverEvent::Appeared(_)) {
                        AIRPLAY_APPEARED
                    } else {
                        AIRPLAY_CHANGED
                    },
                    id,
                    info.device.name.clone(),
                    model(info),
                    availability(info, paired),
                    flags(info),
                )
            }
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
            Pending::Failed => {
                event.kind = AIRPLAY_EVENT_FAILED;
                (String::new(), self.failure_detail())
            }
            Pending::Pairing {
                kind,
                id,
                error,
                unsupported,
                detail,
            } => {
                event.kind = kind;
                event.error = error;
                event.unsupported = unsupported;
                (id, detail)
            }
        };
        self.event_strings = [c_string(&id), c_string(&detail)];
        event.id = self.event_strings[0].as_ptr();
        event.detail = self.event_strings[1].as_ptr();
        event
    }
}

impl AscAirPlay {
    /// A call panicked, or the driver thread ended (see the module's
    /// Panics).
    fn is_dead(&self) -> bool {
        self.death.dead.load(Ordering::Acquire) || !self.sender.is_running()
    }

    /// The detail of [`AIRPLAY_EVENT_FAILED`] (S205): `panic at
    /// <file>:<line>` for the panic a call on this handle or its input
    /// caught; when none did (the driver thread or discovery ended),
    /// `latest panic at <file>:<line>` for the latest panic the process
    /// noted since this handle was created, which, with one sender, is
    /// usually the one that ended it; "" when there was none.
    fn failure_detail(&self) -> String {
        if let Some(at) = self.death.at.get() {
            return format!("panic at {at}");
        }
        if PANICS.load(Ordering::SeqCst) == self.panics_before {
            return String::new();
        }
        LATEST_PANIC
            .lock()
            .ok()
            .and_then(|latest| latest.clone())
            .map_or_else(String::new, |at| format!("latest panic at {at}"))
    }
}

/// A sender's death (see the module's Panics), shared by its handle and its
/// input.
struct Death {
    /// Set by a panic in a call on the handle or its input, or by
    /// discovery's browser dying.
    dead: AtomicBool,
    /// Where the first panic a call caught happened (`<file>:<line>`).
    at: OnceLock<String>,
}

impl Death {
    fn new() -> Self {
        Death {
            dead: AtomicBool::new(false),
            at: OnceLock::new(),
        }
    }

    /// A call on this thread caught a panic: the sender is dead, and the
    /// location the panic hook noted for it is kept for the failure's
    /// detail. Runs only after a panic, which has allocated already; the
    /// location was allocated with it, and keeping it allocates nothing.
    fn panicked(&self) {
        if let Some(at) = caught_panic_location() {
            // Only the first panic's: a later one may follow from it.
            let _ = self.at.set(at);
        }
        self.dead.store(true, Ordering::Release);
    }
}

/// How many panics the hook noted in this process, on any thread.
pub(super) static PANICS: AtomicU64 = AtomicU64::new(0);

/// Where the latest panic the hook noted happened, on any thread.
pub(super) static LATEST_PANIC: Mutex<Option<String>> = Mutex::new(None);

thread_local! {
    /// Where the latest panic on this thread happened, until a call here
    /// that caught it takes it. Touched only while a panic is handled, so
    /// a thread without one (the capture thread) never sets it up.
    static PANIC_AT: Cell<Option<String>> = const { Cell::new(None) };
}

/// A panic's source location as the failure's detail gives it:
/// `<file>:<line>`, the file from its crate's folder on (the folder before
/// the last `src` in its path, or just its name without one), with forward
/// slashes; a build machine's home folder never shows (S205).
fn panic_location(file: &str, line: u32) -> String {
    let parts: Vec<&str> = file.split(['/', '\\']).collect();
    let mut from = match parts.iter().rposition(|part| *part == "src") {
        Some(src) => src.saturating_sub(1),
        None => parts.len() - 1,
    };
    // A `src` right below a home folder (a clone in ~/src) would keep the
    // account name: the file name alone then.
    let home = parts
        .iter()
        .position(|part| part.eq_ignore_ascii_case("users") || *part == "home");
    if home.is_some_and(|home| from <= home + 1) {
        from = parts.len() - 1;
    }
    format!("{}:{line}", parts[from..].join("/"))
}

/// Installs, once, the panic hook that notes where each panic happened, for
/// the failure's detail; it then runs the hook that was there before (the
/// default one prints the panic to standard error). It notes the location
/// only: a panic's message may hold anything (S205).
pub(super) fn note_panic_locations() {
    static INSTALL: Once = Once::new();
    INSTALL.call_once(|| {
        let previous = panic::take_hook();
        panic::set_hook(Box::new(move |info| {
            if let Some(location) = info.location() {
                let at = panic_location(location.file(), location.line());
                // Never waits: with another panic noting its own, this
                // one's location is only on its thread.
                if let Ok(mut latest) = LATEST_PANIC.try_lock() {
                    *latest = Some(at.clone());
                }
                // Not while the thread's locals are going away.
                let _ = PANIC_AT.try_with(|slot| slot.set(Some(at)));
                PANICS.fetch_add(1, Ordering::SeqCst);
            }
            previous(info);
        }));
    });
}

/// The location the hook noted for the latest panic on this thread, which a
/// call here just caught; taken, so it is never reported twice.
pub(super) fn caught_panic_location() -> Option<String> {
    PANIC_AT.try_with(Cell::take).ok().flatten()
}

/// Tests: the panic a test asked for (`tests::panic_in_next_call`);
/// nothing otherwise.
#[inline(always)]
fn injected_panic() {
    #[cfg(test)]
    if tests::PANIC_NEXT.with(|next| next.replace(false)) {
        panic!("a panic injected by a test");
    }
}

/// Runs `body` on the handle behind `airplay`, stopping a panic at this
/// boundary: `failed` for a null handle, and after a panic, which also
/// marks the handle dead. Catching costs nothing until a panic happens.
///
/// # Safety
///
/// `airplay` is null or a live handle, called under the threading contract.
unsafe fn catching<T: Copy>(
    airplay: *mut AscAirPlay,
    failed: T,
    body: impl FnOnce(&mut AscAirPlay) -> T,
) -> T {
    if airplay.is_null() {
        return failed;
    }
    let outcome = panic::catch_unwind(AssertUnwindSafe(|| {
        injected_panic();
        // SAFETY: non-null, and by the caller's contract a live handle that
        // no other call uses now.
        body(unsafe { &mut *airplay })
    }));
    outcome.unwrap_or_else(|_| {
        // SAFETY: still live; the closure's borrow ended with it.
        unsafe { &*airplay }.death.panicked();
        failed
    })
}

/// [`catching`] for a call that does nothing on a dead handle.
///
/// # Safety
///
/// As [`catching`].
pub(crate) unsafe fn live<T: Copy>(
    airplay: *mut AscAirPlay,
    failed: T,
    body: impl FnOnce(&mut AscAirPlay) -> T,
) -> T {
    // SAFETY: as the caller's.
    unsafe {
        catching(airplay, failed, |airplay| {
            if airplay.is_dead() {
                failed
            } else {
                body(airplay)
            }
        })
    }
}

/// The capture side of an [`AscAirPlayInput`]: the sender's input, the
/// resampler from the capture rate to 44.1 kHz and its output buffer, and
/// the input's fade at the capture rate (S189).
struct Feed {
    writer: InputWriter,
    resampler: Resampler,
    /// Resampler output, stereo.
    resampled: Box<[f32]>,
    /// Fades in after each rate set and out after a stop request (S189).
    fade: InputFade,
    /// The last rate request taken in (see [`AscAirPlayInput::requested`]).
    applied: u64,
    /// The resampler runs at a supported rate the host asked for.
    ready: bool,
}

impl Feed {
    /// Resamples `block` and hands it to the sender as audio that arrived
    /// now; takes in a new rate request first, which starts a fade-in, and
    /// fades out while `closing` (S189). Returns the 44.1 kHz frames handed
    /// over. Never allocates, locks, blocks or logs (P6).
    fn write(&mut self, block: &[f32], requested: u64, closing: bool) -> usize {
        if requested != self.applied {
            self.applied = requested;
            // Low half: the rate; a new request, even for the same rate,
            // starts the resampler afresh.
            self.ready = self.resampler.set_input_rate(requested as u32).is_ok();
            if self.ready {
                self.fade.restart(requested as u32);
            }
        }
        if !self.ready {
            return 0;
        }
        if closing {
            self.fade.fade_out();
        }
        let at = Instant::now();
        let mut handed = 0;
        let (resampler, resampled, writer) =
            (&mut self.resampler, &mut self.resampled, &mut self.writer);
        self.fade.process(block, |piece| {
            handed += resample_into(resampler, resampled, writer, piece, at);
        });
        handed
    }
}

/// Resamples `block` into `resampled` and hands it to `writer` as audio that
/// arrived `at`; returns the 44.1 kHz frames handed over.
fn resample_into(
    resampler: &mut Resampler,
    resampled: &mut [f32],
    writer: &mut InputWriter,
    block: &[f32],
    at: Instant,
) -> usize {
    let frames = block.len() / 2;
    let (mut consumed, mut handed) = (0, 0);
    // Until the input is used up and nothing more comes out: the resampler
    // may take in the whole block while the buffer holds only part of its
    // output, and keeps the rest for the next call.
    loop {
        let (used, produced) = resampler.process(&block[consumed * 2..frames * 2], resampled);
        if produced > 0 {
            // Frames that come too late or don't fit are dropped there and
            // counted (P112, P144).
            writer.write_f32(&resampled[..produced * 2], at);
            handed += produced;
        }
        consumed += used;
        if used == 0 && produced == 0 {
            break;
        }
    }
    handed
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
    /// A stop asked the input to fade out (S189); cleared by the next rate
    /// request.
    closing: AtomicBool,
    /// Its sender's death: set by a panic in a write, which then writes
    /// nothing more.
    death: Arc<Death>,
}

/// Starts an AirPlay sender: its stream, silent until audio comes through
/// its input ([`asc_airplay_take_input`]), and its driver thread. Discovery
/// waits for [`asc_airplay_start_discovery`]. The master starts at 100.
/// Null when the thread can't start.
#[unsafe(no_mangle)]
pub extern "C" fn asc_airplay_create() -> *mut AscAirPlay {
    create_with(live_browser)
}

/// [`asc_airplay_create`] with the browser [`asc_airplay_start_discovery`]
/// opens: live multicast DNS for the app, one a test supplies otherwise.
pub(crate) fn create_with(open_browser: OpenBrowser) -> *mut AscAirPlay {
    note_panic_locations();
    let panics_before = PANICS.load(Ordering::SeqCst);
    let created = panic::catch_unwind(|| {
        injected_panic();
        // The hidden AirPlay 2 switch (spec P2): an environment variable, so
        // it flips without a rebuild and no control shows it.
        let config = SenderConfig {
            airplay2: airplay2_switch(std::env::var(AIRPLAY2_SWITCH_ENV).ok().as_deref()),
            ..SenderConfig::default()
        };
        match Sender::start(config) {
            Ok((sender, input)) => Box::into_raw(Box::new(AscAirPlay {
                sender,
                input: Some(input),
                receivers: VecDeque::new(),
                receivers_queued: 0,
                receivers_taken: 0,
                receiver_strings: Default::default(),
                events: VecDeque::new(),
                event_strings: Default::default(),
                listed: HashMap::new(),
                added: HashMap::new(),
                store: PairingStore::in_memory(),
                passwords: HashMap::new(),
                pairing: HashMap::new(),
                ending: HashMap::new(),
                master: 100.0,
                hold_back: Duration::ZERO,
                death: Arc::new(Death::new()),
                panics_before,
                failure_reported: false,
                open_browser,
            })),
            Err(_) => ptr::null_mut(),
        }
    });
    created.unwrap_or_else(|_| {
        // No handle to report it: the location goes.
        drop(caught_panic_location());
        ptr::null_mut()
    })
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
    let freed = panic::catch_unwind(AssertUnwindSafe(|| drop(unsafe { Box::from_raw(handle) })));
    // A panic while freeing leaves what wasn't freed yet behind; the host's
    // pointer is null either way.
    drop(freed);
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
    unsafe {
        live(airplay, false, |airplay| match (airplay.open_browser)() {
            Some(browser) => {
                airplay.sender.start_discovery_with(browser);
                true
            }
            None => false,
        })
    }
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
    unsafe { live(airplay, (), |airplay| airplay.sender.stop_discovery()) }
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
    let take = |airplay: &mut AscAirPlay| {
        airplay.pump();
        let Some(event) = airplay.receivers.pop_front() else {
            return false;
        };
        airplay.receivers_taken += 1;
        let report = airplay.report(event);
        // SAFETY: non-null, and by the caller's contract writable.
        unsafe { receiver.write(report) };
        true
    };
    // SAFETY: by the caller's contract null or a live handle that no other
    // call uses now.
    unsafe { live(airplay, false, take) }
}

/// Takes the next speaker or timeline event, if one is waiting, into
/// `*event` and returns true. Returns false, leaving `*event` as it was,
/// when none is waiting or either pointer is null. Never waits. Receiver
/// changes wait for [`asc_airplay_next_receiver`], and an event waits until
/// every receiver change queued before it was taken there, so a host that
/// takes the changes first never sees an event ahead of the change it
/// follows (a speaker that turned its stored pairing down is listed as
/// needing a PIN before its error comes). A dead sender (see the module's
/// Panics) reports [`AIRPLAY_EVENT_FAILED`] once, in place of whatever
/// still waited, and nothing after it.
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
    let take = |airplay: &mut AscAirPlay| {
        if !airplay.is_dead() {
            airplay.pump();
        }
        let pending = if airplay.is_dead() {
            if airplay.failure_reported {
                return false;
            }
            airplay.failure_reported = true;
            airplay.events.clear();
            Pending::Failed
        } else {
            // An event waits for the receiver changes queued before it.
            let taken = airplay.receivers_taken;
            if !airplay
                .events
                .front()
                .is_some_and(|(after, _)| *after <= taken)
            {
                return false;
            }
            let Some((_, pending)) = airplay.events.pop_front() else {
                return false;
            };
            pending
        };
        let described = airplay.describe(pending);
        // SAFETY: non-null, and by the caller's contract writable.
        unsafe { event.write(described) };
        true
    };
    // SAFETY: by the caller's contract null or a live handle that no other
    // call uses now.
    match unsafe { catching(airplay, None, |airplay| Some(take(airplay))) } {
        Some(taken) => taken,
        // A null handle, or that call panicked and made the sender dead,
        // whose failure then comes at once.
        // SAFETY: as above.
        None => unsafe { catching(airplay, false, take) },
    }
}

/// Adds the receiver discovery lists under `id` as a speaker, at slider
/// position `volume` (0 to 100) times the master, held back by the
/// hold-back set last ([`asc_airplay_set_hold_back`]): a session starts at
/// once, and its state, errors and latency come as events. Its sessions use the password handed over for it
/// ([`asc_airplay_set_password`]) and verify with the stored credentials,
/// if any. Adding a speaker already added applies the volume, the password
/// and the credentials and, if it failed or waits for a retry, starts a new
/// session now. False, adding nothing, when the handle or `id` is null,
/// `id` isn't a listed receiver, or `volume` isn't finite.
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
    // SAFETY: by the caller's contract null or a NUL-terminated string.
    let Some(id) = (unsafe { host_text(id) }) else {
        return false;
    };
    if !volume.is_finite() {
        return false;
    }
    // SAFETY: by the caller's contract null or a live handle that no other
    // call uses now.
    unsafe {
        live(airplay, false, |airplay| {
            airplay.pump();
            let Some(info) = airplay.listed.get(id) else {
                return false;
            };
            let device = info.device.clone();
            let position = volume.clamp(0.0, 100.0);
            let key = device.key.clone();
            let mut options = SpeakerOptions::new(speaker_volume(position, airplay.master));
            options.delay = airplay.hold_back;
            options.password = airplay.passwords.get(id).cloned();
            options.credentials = airplay.store.get(id).cloned();
            airplay.sender.add_speaker(device, options);
            airplay.added.insert(id.to_owned(), Added { key, position });
            true
        })
    }
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
    // SAFETY: by the caller's contract null or a NUL-terminated string.
    let Some(id) = (unsafe { host_text(id) }) else {
        return false;
    };
    // SAFETY: by the caller's contract null or a live handle that no other
    // call uses now.
    unsafe {
        live(airplay, false, |airplay| {
            let Some(added) = airplay.added.remove(id) else {
                return false;
            };
            airplay.sender.remove_speaker(&added.key);
            true
        })
    }
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
    // SAFETY: by the caller's contract null or a NUL-terminated string.
    let Some(id) = (unsafe { host_text(id) }) else {
        return false;
    };
    if !volume.is_finite() {
        return false;
    }
    // SAFETY: by the caller's contract null or a live handle that no other
    // call uses now.
    unsafe {
        live(airplay, false, |airplay| {
            let master = airplay.master;
            let Some(added) = airplay.added.get_mut(id) else {
                return false;
            };
            added.position = volume.clamp(0.0, 100.0);
            airplay
                .sender
                .set_volume(&added.key, speaker_volume(added.position, master));
            true
        })
    }
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
    if !master.is_finite() {
        return;
    }
    // SAFETY: by the caller's contract null or a live handle that no other
    // call uses now.
    unsafe {
        live(airplay, (), |airplay| {
            airplay.master = master.clamp(0.0, 100.0);
            for added in airplay.added.values() {
                airplay
                    .sender
                    .set_volume(&added.key, speaker_volume(added.position, airplay.master));
            }
        })
    }
}

/// Holds every speaker back by `hold_back_ms` on top of L, live, and every
/// speaker added later, so they meet an output that plays later than L,
/// such as a Google Cast speaker with a longer offset (C77). The sender caps
/// a speaker's delay at [`AIRPLAY_MAX_DELAY_MS`] (P99); past it the speakers
/// play early by the rest. A value below 0 or not finite counts as 0.
/// Returns the hold-back applied, in ms; 0 for a null or dead handle.
///
/// # Safety
///
/// `airplay` is null or a live handle, called under the threading contract.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn asc_airplay_set_hold_back(
    airplay: *mut AscAirPlay,
    hold_back_ms: f64,
) -> f64 {
    let hold_back = if hold_back_ms.is_finite() {
        Duration::from_secs_f64(hold_back_ms.clamp(0.0, millis(MAX_DELAY)) / 1000.0)
    } else {
        Duration::ZERO
    };
    // SAFETY: by the caller's contract null or a live handle that no other
    // call uses now.
    unsafe {
        live(airplay, 0.0, |airplay| {
            if hold_back != airplay.hold_back {
                airplay.hold_back = hold_back;
                for added in airplay.added.values() {
                    airplay.sender.set_delay(&added.key, hold_back);
                }
            }
            millis(hold_back)
        })
    }
}

/// The base latency L in milliseconds (P98, P113): a local output lines up
/// with the speakers at L plus its own delay (P114). 0 for a null or dead
/// handle.
///
/// # Safety
///
/// `airplay` is null or a live handle, called under the threading contract.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn asc_airplay_latency_ms(airplay: *const AscAirPlay) -> f64 {
    // SAFETY: by the caller's contract null or a live handle that no other
    // call uses now; the call only reads it.
    unsafe {
        live(airplay.cast_mut(), 0.0, |airplay| {
            millis(airplay.sender.latency())
        })
    }
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
    unsafe { live(airplay, (), |airplay| airplay.sender.restart_timeline()) }
}

/// Sets what is playing, for the receivers that show it (P123 to P127): the
/// track's title, artist and album as UTF-8 text, and its artwork as a JPEG
/// of at most 512 KiB (P125). Every speaker that lists the type gets it now,
/// and every speaker that starts or restarts later gets it too. A null text
/// is an empty one, and three null texts send no track information; null
/// artwork, or a length of 0, sends none. Returns false, changing nothing,
/// for a null or dead handle, a text that isn't UTF-8, a null `artwork`
/// with a length, or artwork the sender refuses (not a JPEG, or too large).
///
/// # Safety
///
/// `airplay` is null or a live handle, called under the threading
/// contract; each text is null or a NUL-terminated string; `artwork` is
/// null or points to `artwork_len` bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn asc_airplay_set_now_playing(
    airplay: *mut AscAirPlay,
    title: *const c_char,
    artist: *const c_char,
    album: *const c_char,
    artwork: *const u8,
    artwork_len: usize,
) -> bool {
    let mut texts = [None; 3];
    for (text, pointer) in texts.iter_mut().zip([title, artist, album]) {
        if pointer.is_null() {
            continue;
        }
        // SAFETY: non-null, and by the caller's contract NUL-terminated.
        let Some(value) = (unsafe { host_text(pointer) }) else {
            return false;
        };
        *text = Some(value);
    }
    // SAFETY: by the caller's contract null or `artwork_len` bytes.
    let Some(jpeg) = (unsafe { host_slice(artwork, artwork_len) }) else {
        return false;
    };
    let [title, artist, album] = texts;
    let track = (title.is_some() || artist.is_some() || album.is_some()).then(|| TrackInfo {
        title: title.unwrap_or_default().to_owned(),
        artist: artist.unwrap_or_default().to_owned(),
        album: album.unwrap_or_default().to_owned(),
    });
    let now_playing = NowPlaying {
        track,
        artwork: (!jpeg.is_empty()).then(|| jpeg.to_vec()),
        progress: None,
    };
    // SAFETY: by the caller's contract null or a live handle that no other
    // call uses now.
    unsafe {
        live(airplay, false, |airplay| {
            airplay.sender.set_now_playing(now_playing).is_ok()
        })
    }
}

/// Opens the pairing store kept in the file at `path` (see
/// [`crate::airplay_pairings`]) in place of the one in memory, whose
/// pairings it drops; meant for once, before discovery starts. Listed
/// receivers are reported again, as changed, with their availability under
/// the new store. Returns how many receivers it holds credentials for, or
/// [`AIRPLAY_PAIRINGS_IGNORED`] or [`AIRPLAY_PAIRINGS_NEWER`] for a file it
/// ignored (the store is then empty), or [`AIRPLAY_PAIRINGS_FAILED`] for a
/// null or dead handle or a null or non-UTF-8 `path`, leaving the store
/// as it was.
///
/// # Safety
///
/// `airplay` is null or a live handle, called under the threading
/// contract; `path` is null or a NUL-terminated string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn asc_airplay_open_pairings(
    airplay: *mut AscAirPlay,
    path: *const c_char,
) -> i32 {
    // SAFETY: by the caller's contract null or a NUL-terminated string.
    let Some(path) = (unsafe { host_text(path) }) else {
        return AIRPLAY_PAIRINGS_FAILED;
    };
    if path.is_empty() {
        return AIRPLAY_PAIRINGS_FAILED;
    }
    // SAFETY: by the caller's contract null or a live handle that no other
    // call uses now.
    unsafe {
        live(airplay, AIRPLAY_PAIRINGS_FAILED, |airplay| {
            airplay.pump();
            let (store, loaded) = PairingStore::open(PathBuf::from(path));
            airplay.store = store;
            let mut ids: Vec<String> = airplay.listed.keys().cloned().collect();
            ids.sort();
            for id in ids {
                airplay.relist(&id);
            }
            match loaded {
                Loaded::NoFile => 0,
                Loaded::Receivers(count) => i32::try_from(count).unwrap_or(i32::MAX),
                Loaded::Ignored(_) => AIRPLAY_PAIRINGS_IGNORED,
                Loaded::Newer(_) => AIRPLAY_PAIRINGS_NEWER,
            }
        })
    }
}

/// Starts a PIN pairing with the receiver discovery lists under `id`,
/// meant for one listed as [`AIRPLAY_NEEDS_PIN`] that the user turned on
/// (P46 to P54): it is asked to show a PIN, and
/// [`AIRPLAY_EVENT_PIN_NEEDED`] follows, or the pairing's failure. The
/// pairing uses the password handed over for the receiver, if any. A
/// pairing already running for it is replaced: that one is cancelled. A
/// new pairing starts only once the one cancelled before it has ended, and
/// nothing that one reports reaches the host or ends the new one; when that
/// one ended paired after all, the new one never starts and
/// [`AIRPLAY_EVENT_PAIRED`] follows instead, with no PIN asked. Playing
/// speakers are not touched. False, starting nothing, when the handle or
/// `id` is null or `id` isn't a listed receiver.
///
/// # Safety
///
/// `airplay` is null or a live handle, called under the threading
/// contract; `id` is null or a NUL-terminated string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn asc_airplay_start_pairing(
    airplay: *mut AscAirPlay,
    id: *const c_char,
) -> bool {
    // SAFETY: by the caller's contract null or a NUL-terminated string.
    let Some(id) = (unsafe { host_text(id) }) else {
        return false;
    };
    // SAFETY: by the caller's contract null or a live handle that no other
    // call uses now.
    unsafe {
        live(airplay, false, |airplay| {
            airplay.pump();
            let Some(info) = airplay.listed.get(id) else {
                return false;
            };
            let device = info.device.clone();
            let key = device.key.clone();
            if let Some(next) = airplay.ending.get_mut(id) {
                // A pairing cancelled before hasn't ended yet: this one
                // starts once it has.
                *next = Some(device);
            } else if let Some(running) = airplay.pairing.get(id).cloned() {
                // Replaced: the one running is cancelled, and this one
                // starts once it has ended.
                airplay.sender.cancel_pairing(&running);
                airplay.ending.insert(id.to_owned(), Some(device));
            } else {
                airplay.start_pairing_now(id, device);
            }
            airplay.pairing.insert(id.to_owned(), key);
            true
        })
    }
}

/// Hands the PIN the user typed to the pairing with the receiver `id`
/// (P50, P51); its outcome comes as an event. False, sending nothing, when
/// the handle, `id` or `pin` is null, no pairing runs for `id`, or `pin`
/// isn't ASCII digits only. A PIN while the pairing doesn't wait for one is
/// ignored. A receiver that left discovery since its pairing started still
/// takes it. The PIN is never kept or logged.
///
/// # Safety
///
/// `airplay` is null or a live handle, called under the threading
/// contract; `id` and `pin` are null or NUL-terminated strings.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn asc_airplay_submit_pin(
    airplay: *mut AscAirPlay,
    id: *const c_char,
    pin: *const c_char,
) -> bool {
    // SAFETY: by the caller's contract null or NUL-terminated strings.
    let (Some(id), Some(pin)) = (unsafe { host_text(id) }, unsafe { host_text(pin) }) else {
        return false;
    };
    // SAFETY: by the caller's contract null or a live handle that no other
    // call uses now.
    unsafe {
        live(airplay, false, |airplay| {
            airplay.pump();
            let Some(key) = airplay.pairing.get(id) else {
                return false;
            };
            airplay.sender.submit_pin(key, pin).is_ok()
        })
    }
}

/// Stops the pairing with the receiver `id`, if one runs, also when the
/// receiver left discovery since; nothing is reported for it any more.
/// Credentials it still gets before it stops are stored all the same.
///
/// # Safety
///
/// `airplay` is null or a live handle, called under the threading
/// contract; `id` is null or a NUL-terminated string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn asc_airplay_cancel_pairing(airplay: *mut AscAirPlay, id: *const c_char) {
    // SAFETY: by the caller's contract null or a NUL-terminated string.
    let Some(id) = (unsafe { host_text(id) }) else {
        return;
    };
    // SAFETY: by the caller's contract null or a live handle that no other
    // call uses now.
    unsafe {
        live(airplay, (), |airplay| {
            airplay.pump();
            let Some(key) = airplay.pairing.remove(id) else {
                return;
            };
            match airplay.ending.get_mut(id) {
                // It still waits for the pairing cancelled before it, so
                // it never started and now never will.
                Some(next) => *next = None,
                None => {
                    airplay.sender.cancel_pairing(&key);
                    airplay.ending.insert(id.to_owned(), None);
                }
            }
        })
    }
}

/// Hands over the password of the receiver `id` (P39), for its next
/// sessions ([`asc_airplay_add_speaker`] adds or restarts one) and pairings
/// until the handle is destroyed; null or "" forgets it. It is kept in
/// memory only, wiped when replaced, forgotten or dropped, never stored or
/// logged, and forgotten when the receiver turns it down. False when the
/// handle or `id` is null.
///
/// # Safety
///
/// `airplay` is null or a live handle, called under the threading
/// contract; `id` and `password` are null or NUL-terminated strings.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn asc_airplay_set_password(
    airplay: *mut AscAirPlay,
    id: *const c_char,
    password: *const c_char,
) -> bool {
    // SAFETY: by the caller's contract null or NUL-terminated strings.
    let (Some(id), password) = (unsafe { host_text(id) }, unsafe { host_text(password) }) else {
        return false;
    };
    // SAFETY: by the caller's contract null or a live handle that no other
    // call uses now.
    unsafe {
        live(airplay, false, |airplay| {
            match password.filter(|password| !password.is_empty()) {
                Some(password) => {
                    airplay
                        .passwords
                        .insert(id.to_owned(), Secret::from(password));
                }
                None => {
                    airplay.passwords.remove(id);
                }
            }
            true
        })
    }
}

/// Forgets the stored credentials of the receiver `id` and writes the
/// pairings file (removed once nothing is paired); the receiver is reported
/// again as needing a PIN when listed. A session running with them goes on
/// until it ends. Returns whether it was paired; false too for a null or
/// dead handle or a null `id`.
///
/// # Safety
///
/// `airplay` is null or a live handle, called under the threading
/// contract; `id` is null or a NUL-terminated string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn asc_airplay_forget_pairing(
    airplay: *mut AscAirPlay,
    id: *const c_char,
) -> bool {
    // SAFETY: by the caller's contract null or a NUL-terminated string.
    let Some(id) = (unsafe { host_text(id) }) else {
        return false;
    };
    // SAFETY: by the caller's contract null or a live handle that no other
    // call uses now.
    unsafe {
        live(airplay, false, |airplay| {
            airplay.pump();
            if !airplay.store.contains(id) {
                return false;
            }
            // Forgotten for this handle even when the file can't be written.
            let _ = airplay.store.remove(id);
            airplay.relist(id);
            true
        })
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
    unsafe {
        live(airplay, ptr::null_mut(), |airplay| {
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
                    fade: InputFade::new(SAMPLE_RATE, INPUT_CHUNK_FRAMES),
                    applied: 0,
                    ready: false,
                },
                requested: AtomicU64::new(0),
                closing: AtomicBool::new(false),
                death: airplay.death.clone(),
            }))
        })
    }
}

/// Any thread: the rate in Hz of the audio the capture writes from now on,
/// rounded to whole hertz. The next write takes it in and starts the
/// resampler afresh, with a fade-in from silence over F capture-rate frames
/// (S189), so a host calls it at each capture start, even at an
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
    // SAFETY: the handle is live; this call borrows only the atomics, which
    // writes read but never borrow mutably.
    let (requested, closing, death) =
        unsafe { (&(*input).requested, &(*input).closing, &(*input).death) };
    let set = panic::catch_unwind(|| {
        injected_panic();
        // A new capture: open again after a stop's fade-out (S189).
        closing.store(false, Ordering::Release);
        let mut old = requested.load(Ordering::Relaxed);
        loop {
            let new = ((old >> 32).wrapping_add(1) << 32) | u64::from(rate as u32);
            match requested.compare_exchange_weak(old, new, Ordering::Release, Ordering::Relaxed) {
                Ok(_) => return true,
                Err(current) => old = current,
            }
        }
    });
    set.unwrap_or_else(|_| {
        death.panicked();
        false
    })
}

/// Capture thread: hands `frames` interleaved stereo frames at the rate
/// last set to the sender, resampled to 44.1 kHz, as audio that arrived
/// now (P73, P96). Returns the 44.1 kHz frames handed over, 0 before a
/// rate is set; the sender drops those that come too late or don't fit
/// (P112). Never allocates, locks, blocks or logs (P6); it reads the
/// monotonic clock once. Real-time safe. 0 once a write panicked (only a
/// panic itself allocates; see the module's Panics).
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
    let (requested, closing, death) =
        unsafe { (&(*input).requested, &(*input).closing, &(*input).death) };
    if death.dead.load(Ordering::Acquire) {
        return 0;
    }
    let requested = requested.load(Ordering::Acquire);
    let closing = closing.load(Ordering::Acquire);
    // SAFETY: as above.
    let feed = unsafe { &mut (*input).feed };
    let written = panic::catch_unwind(AssertUnwindSafe(|| {
        injected_panic();
        feed.write(block, requested, closing)
    }));
    written.unwrap_or_else(|_| {
        death.panicked();
        0
    })
}

/// Any thread, at a session stop: asks the input to fade out over the next
/// F capture-rate frames written and to hand zeros after them, until the
/// next [`asc_airplay_input_set_rate`] fades it in again (S186, S189). Does
/// nothing for a null handle. Real-time safe.
///
/// # Safety
///
/// `input` is null or a live input handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn asc_airplay_input_request_fade_out(input: *mut AscAirPlayInput) {
    if input.is_null() {
        return;
    }
    // SAFETY: the handle is live; this call borrows only an atomic, which
    // writes read but never borrow mutably.
    let closing = unsafe { &(*input).closing };
    closing.store(true, Ordering::Release);
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
    let freed = panic::catch_unwind(AssertUnwindSafe(|| drop(unsafe { Box::from_raw(handle) })));
    // As in asc_airplay_destroy: the host's pointer is null either way.
    drop(freed);
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

    /// A browser that finds nothing and never dies.
    struct Quiet;

    impl Browser for Quiet {
        fn poll_event(&mut self) -> Option<BrowseEvent> {
            None
        }
    }

    fn quiet() -> Option<Box<dyn Browser + Send>> {
        Some(Box::new(Quiet))
    }

    /// No test browses the real network: a live `mdns-sd` daemon binds
    /// multicast DNS on every interface, and the system firewall then asks
    /// the user to allow the test program.
    fn assert_no_live_discovery() {
        assert_eq!(
            audiosync_airplay::discovery::mdns::daemons_started(),
            0,
            "a test started a live mDNS daemon"
        );
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
    fn discovery_starts_on_the_handles_browser_and_destroy_is_prompt() {
        let mut handle = create_with(quiet);
        assert!(!handle.is_null());
        // SAFETY: a live handle, used by this thread only.
        assert!(unsafe { asc_airplay_start_discovery(handle) });
        // SAFETY: as above.
        assert!(unsafe { asc_airplay_latency_ms(handle) } > 0.0, "alive");
        let started = Instant::now();
        // SAFETY: destroyed once, by this thread.
        unsafe { asc_airplay_destroy(&mut handle) };
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "{:?}",
            started.elapsed()
        );
        assert!(handle.is_null());
        assert_no_live_discovery();
    }

    #[test]
    fn discovery_that_cannot_start_fails_cleanly() {
        // In the core's tests the C constructor's browser refuses, as live
        // multicast DNS does on a host without multicast: the call says so
        // and the handle stays usable, with a prompt destroy.
        let mut handle = asc_airplay_create();
        assert!(!handle.is_null());
        // SAFETY: a live handle, used by this thread only.
        assert!(!unsafe { asc_airplay_start_discovery(handle) });
        // SAFETY: as above.
        assert!(unsafe { asc_airplay_latency_ms(handle) } > 0.0, "alive");
        assert!(next(handle, 0).is_empty());
        let started = Instant::now();
        // SAFETY: destroyed once, by this thread.
        unsafe { asc_airplay_destroy(&mut handle) };
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "{:?}",
            started.elapsed()
        );
        assert!(handle.is_null());
        assert_no_live_discovery();
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
            Loopback::start_with(false)
        }

        /// A loopback that answers every request with 401 and a digest
        /// challenge, as a receiver whose password never matches.
        fn start_with(unauthorized: bool) -> Loopback {
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
                    serve(&listener, &transport, &requests, &stop, unauthorized);
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

    fn serve(
        listener: &TcpListener,
        transport: &str,
        requests: &Requests,
        stop: &AtomicBool,
        unauthorized: bool,
    ) {
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
                    if unauthorized {
                        response = Response::new(401, "Unauthorized");
                        response
                            .headers
                            .push("WWW-Authenticate", "Digest realm=\"raop\", nonce=\"0123\"");
                    }
                    if let Some(cseq) = request.headers.get("CSeq") {
                        response.headers.push("CSeq", cseq);
                    }
                    if unauthorized {
                        let _ = stream.write_all(&response.encode());
                        continue;
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
    fn the_hold_back_delays_every_speaker_up_to_the_sender_limit() {
        // SAFETY: a null handle is allowed.
        let null = unsafe { asc_airplay_set_hold_back(ptr::null_mut(), 500.0) };
        assert_eq!(null, 0.0);
        let receiver = Loopback::start();
        let id = "02005E005322";
        let mut handle = listing(vec![loopback_record(id, "Den", receiver.port)]);
        let c_id = c_string(id);
        // SAFETY: a live handle, used by this thread only; a C string.
        unsafe {
            assert_eq!(asc_airplay_set_hold_back(handle, 5000.0), 3000.0);
            assert_eq!(asc_airplay_set_hold_back(handle, f64::NAN), 0.0);
            assert_eq!(asc_airplay_set_hold_back(handle, -20.0), 0.0);
            assert_eq!(asc_airplay_set_hold_back(handle, 1000.0), 1000.0);
            assert!(asc_airplay_add_speaker(handle, c_id.as_ptr(), 100.0));
        }
        let events = events_until(handle, |seen| {
            seen.iter()
                .any(|e| e.kind == AIRPLAY_EVENT_LATENCY && e.id == id)
        });
        // A speaker added later starts at L + the hold-back.
        let latency = events.last().unwrap();
        assert!((latency.latency_ms - 2750.0).abs() < 0.01, "{latency:?}");
        // SAFETY: a live handle, used by this thread only; a C string.
        unsafe {
            assert_eq!(asc_airplay_set_hold_back(handle, 1250.0), 1250.0);
            assert!(asc_airplay_remove_speaker(handle, c_id.as_ptr()));
        }
        // SAFETY: destroyed once, by this thread.
        unsafe { asc_airplay_destroy(&mut handle) };
    }

    #[test]
    fn now_playing_reaches_a_speaker_that_shows_it_and_bad_input_is_refused() {
        let jpeg: &[u8] = &[0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10];
        let none = ptr::null::<c_char>();
        // SAFETY: null handles and pointers are part of the contract.
        unsafe {
            assert!(!asc_airplay_set_now_playing(
                ptr::null_mut(),
                c"Track One".as_ptr(),
                none,
                none,
                ptr::null(),
                0
            ));
        }

        let receiver = Loopback::start();
        let id = "02005E005331";
        let mut txt = OPEN.to_vec();
        txt.push("md=0,1,2");
        let record = ServiceRecord {
            port: receiver.port,
            addresses: vec![ScopedAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST))],
            ..raop(id, "Den", &txt)
        };
        let mut handle = listing(vec![record]);
        let c_id = c_string(id);
        let not_utf8: [u8; 3] = [0xC3, 0x28, 0x00];
        let mut large = vec![0u8; audiosync_airplay::metadata::MAX_ARTWORK_BYTES + 1];
        large[..3].copy_from_slice(&[0xFF, 0xD8, 0xFF]);
        // SAFETY: a live handle, used by this thread only; C strings and
        // byte buffers of the lengths given.
        unsafe {
            assert!(
                !asc_airplay_set_now_playing(
                    handle,
                    not_utf8.as_ptr().cast(),
                    none,
                    none,
                    ptr::null(),
                    0
                ),
                "a title that isn't UTF-8"
            );
            assert!(
                !asc_airplay_set_now_playing(handle, none, none, none, ptr::null(), 4),
                "a length without artwork"
            );
            assert!(
                !asc_airplay_set_now_playing(handle, none, none, none, b"GIF89a".as_ptr(), 6),
                "artwork that isn't a JPEG"
            );
            assert!(
                !asc_airplay_set_now_playing(handle, none, none, none, large.as_ptr(), large.len()),
                "artwork over 512 KiB"
            );
            assert!(
                asc_airplay_set_now_playing(handle, none, none, none, ptr::null(), 0),
                "nothing playing"
            );
            assert!(asc_airplay_add_speaker(handle, c_id.as_ptr(), 50.0));
        }
        let events = events_until(handle, |seen| has_state(seen, id, AIRPLAY_STATE_PLAYING));
        assert!(has_state(&events, id, AIRPLAY_STATE_PLAYING), "{events:?}");
        // SAFETY: a live handle, used by this thread only; C strings and a
        // byte buffer of the length given.
        unsafe {
            assert!(asc_airplay_set_now_playing(
                handle,
                c"Track One".as_ptr(),
                c"Some Artist".as_ptr(),
                c"Some Album".as_ptr(),
                jpeg.as_ptr(),
                jpeg.len()
            ));
        }
        assert!(
            wait_for(|| receiver
                .bodies(Method::SetParameter)
                .iter()
                .any(|body| body.contains("Track One")
                    && body.contains("Some Artist")
                    && body.contains("Some Album"))),
            "{:?}",
            receiver.bodies(Method::SetParameter)
        );
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

    /// A browser whose first poll panics on the driver thread, as a bug in
    /// discovery would.
    struct Panicking;

    impl Browser for Panicking {
        fn poll_event(&mut self) -> Option<BrowseEvent> {
            panic!("a bug in discovery (test)");
        }
    }

    thread_local! {
        /// The next guarded call on this thread panics inside its guard.
        pub(super) static PANIC_NEXT: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    }

    /// The next guarded call on this thread panics inside its guard.
    fn panic_in_next_call() {
        PANIC_NEXT.with(|next| next.set(true));
    }

    fn failure_event() -> Event {
        Event {
            kind: AIRPLAY_EVENT_FAILED,
            id: String::new(),
            state: 0,
            error: 0,
            unsupported: AIRPLAY_REASON_NONE,
            retry_ms: -1,
            latency_ms: 0.0,
            receiver_latency_frames: -1,
            restarted: false,
            detail: String::new(),
        }
    }

    /// `event` with an empty detail: a failure's detail is checked on its
    /// own (S205).
    fn without_detail(event: &Event) -> Event {
        Event {
            detail: String::new(),
            ..event.clone()
        }
    }

    fn failed(events: &[Event]) -> bool {
        events
            .iter()
            .any(|event| event.kind == AIRPLAY_EVENT_FAILED)
    }

    #[test]
    fn a_panic_inside_a_call_returns_its_failure_value_and_kills_the_handle() {
        panic_in_next_call();
        assert!(asc_airplay_create().is_null(), "a create that panics");
        let mut handle = asc_airplay_create();
        assert!(!handle.is_null());
        let first = events_until(handle, |seen| !seen.is_empty());
        assert_eq!(first.len(), 1, "{first:?}");
        assert_eq!(first[0].kind, AIRPLAY_EVENT_TIMELINE);
        panic_in_next_call();
        // SAFETY: a live handle, used by this thread only.
        let latency = unsafe { asc_airplay_latency_ms(handle) };
        assert_eq!(latency, 0.0, "the call that panicked returns 0");
        // SAFETY: as above; the handle is dead, not freed.
        unsafe {
            assert_eq!(asc_airplay_latency_ms(handle), 0.0);
            assert!(!asc_airplay_start_discovery(handle));
            assert!(!asc_airplay_add_speaker(
                handle,
                c"02005E005301".as_ptr(),
                50.0
            ));
            assert!(asc_airplay_take_input(handle).is_null());
            asc_airplay_set_master(handle, 50.0);
            asc_airplay_restart_timeline(handle);
            let mut receiver = empty();
            assert!(!asc_airplay_next_receiver(handle, &mut receiver));
        }
        let events = events_until(handle, |seen| !seen.is_empty());
        assert_eq!(
            events.iter().map(without_detail).collect::<Vec<_>>(),
            [failure_event()],
            "the failure, once, and only it"
        );
        let mut raw = blank_event();
        // SAFETY: as above; a local out struct.
        let more = unsafe { asc_airplay_next_event(handle, &mut raw) };
        assert!(!more, "nothing after it");
        // SAFETY: destroyed once, by this thread.
        unsafe { asc_airplay_destroy(&mut handle) };
        assert!(handle.is_null());
    }

    #[test]
    fn a_panic_while_taking_an_event_reports_the_failure_at_once() {
        let mut handle = asc_airplay_create();
        panic_in_next_call();
        let mut raw = blank_event();
        // SAFETY: a live handle, used by this thread only; a local out struct.
        unsafe {
            assert!(asc_airplay_next_event(handle, &mut raw));
            assert_eq!(raw.kind, AIRPLAY_EVENT_FAILED);
            assert!(!asc_airplay_next_event(handle, &mut raw));
            asc_airplay_destroy(&mut handle);
        }
    }

    #[test]
    fn a_panic_in_an_input_write_stops_the_input_and_fails_its_sender() {
        let mut handle = asc_airplay_create();
        // SAFETY: a live handle, used by this thread only.
        let mut input = unsafe { asc_airplay_take_input(handle) };
        assert!(!input.is_null());
        let block = vec![0.0f32; 480 * 2];
        // SAFETY: a live input written by this thread only; a block of 480
        // stereo frames.
        unsafe {
            assert!(asc_airplay_input_set_rate(input, 48_000.0));
            assert_eq!(asc_airplay_input_write(input, block.as_ptr(), 480), 441);
            panic_in_next_call();
            assert_eq!(asc_airplay_input_write(input, block.as_ptr(), 480), 0);
            assert_eq!(
                asc_airplay_input_write(input, block.as_ptr(), 480),
                0,
                "a dead input writes nothing more"
            );
        }
        let events = events_until(handle, failed);
        assert_eq!(
            events.last().map(without_detail),
            Some(failure_event()),
            "{events:?}"
        );
        // SAFETY: each destroyed once, by this thread.
        unsafe {
            asc_airplay_input_destroy(&mut input);
            asc_airplay_destroy(&mut handle);
        }
    }

    #[test]
    fn a_driver_ended_by_a_panic_reports_the_failure_once() {
        let mut handle = asc_airplay_create();
        // SAFETY: a live handle, used by this thread only.
        unsafe { &*handle }
            .sender
            .start_discovery_with(Box::new(Panicking));
        let events = events_until(handle, failed);
        assert_eq!(
            events.last().map(without_detail),
            Some(failure_event()),
            "{events:?}"
        );
        assert_eq!(
            events
                .iter()
                .filter(|e| e.kind == AIRPLAY_EVENT_FAILED)
                .count(),
            1
        );
        let mut raw = blank_event();
        // SAFETY: as above; a local out struct.
        unsafe {
            assert!(
                !asc_airplay_next_event(handle, &mut raw),
                "nothing after it"
            );
            assert_eq!(
                asc_airplay_latency_ms(handle),
                0.0,
                "a dead handle's calls fail"
            );
        }
        let started = Instant::now();
        // SAFETY: destroyed once, by this thread.
        unsafe { asc_airplay_destroy(&mut handle) };
        assert!(started.elapsed() < Duration::from_secs(1), "destroy waited");
    }

    /// Whether a failure's detail names where a panic happened, as
    /// `panic at <path>:<line>` with forward slashes, in a file whose path
    /// ends with `file`.
    fn is_panic_location(detail: &str, file: &str) -> bool {
        detail
            .strip_prefix("panic at ")
            .and_then(|at| at.rsplit_once(':'))
            .is_some_and(|(path, line)| {
                path.ends_with(file)
                    && !path.contains('\\')
                    && !line.is_empty()
                    && line.chars().all(|c| c.is_ascii_digit())
            })
    }

    // The debug log learns where the bug is, and nothing the panic said
    // (its message may hold anything, a receiver's reply included).
    // covers: S205
    #[test]
    fn a_failure_after_a_caught_panic_says_where_it_happened_and_never_what_it_said() {
        let mut handle = asc_airplay_create();
        assert!(!handle.is_null());
        panic_in_next_call();
        // SAFETY: a live handle, used by this thread only.
        assert_eq!(unsafe { asc_airplay_latency_ms(handle) }, 0.0);
        let events = events_until(handle, failed);
        let failure = events.last().expect("the failure");
        assert_eq!(failure.kind, AIRPLAY_EVENT_FAILED, "{events:?}");
        assert!(
            is_panic_location(&failure.detail, "src/ffi/airplay.rs"),
            "{failure:?}"
        );
        assert!(!failure.detail.contains("injected"), "{failure:?}");
        // SAFETY: destroyed once, by this thread.
        unsafe { asc_airplay_destroy(&mut handle) };
    }

    // covers: S205
    #[test]
    fn a_failure_after_a_panic_in_an_input_write_says_where_it_happened() {
        let mut handle = asc_airplay_create();
        // SAFETY: a live handle, used by this thread only.
        let mut input = unsafe { asc_airplay_take_input(handle) };
        let block = vec![0.0f32; 480 * 2];
        // SAFETY: a live input written by this thread only; a block of 480
        // stereo frames.
        unsafe {
            assert!(asc_airplay_input_set_rate(input, 48_000.0));
            panic_in_next_call();
            assert_eq!(asc_airplay_input_write(input, block.as_ptr(), 480), 0);
        }
        let events = events_until(handle, failed);
        let failure = events.last().expect("the failure");
        assert_eq!(failure.kind, AIRPLAY_EVENT_FAILED, "{events:?}");
        assert!(
            is_panic_location(&failure.detail, "src/ffi/airplay.rs"),
            "{failure:?}"
        );
        assert!(!failure.detail.contains("injected"), "{failure:?}");
        // SAFETY: each destroyed once, by this thread.
        unsafe {
            asc_airplay_input_destroy(&mut input);
            asc_airplay_destroy(&mut handle);
        }
    }

    // A panic on the sender's own driver thread: no call caught it, so the
    // detail is the latest panic the process saw since the sender started,
    // and says so, since a session thread may have recovered from that one
    // (other tests panic in parallel here, so only its form is checked).
    // covers: S205
    #[test]
    fn a_failure_after_the_driver_panicked_says_where_the_latest_panic_happened() {
        let mut handle = asc_airplay_create();
        // SAFETY: a live handle, used by this thread only.
        unsafe { &*handle }
            .sender
            .start_discovery_with(Box::new(Panicking));
        let events = events_until(handle, failed);
        let failure = events.last().expect("the failure");
        assert_eq!(failure.kind, AIRPLAY_EVENT_FAILED, "{events:?}");
        let detail = failure
            .detail
            .strip_prefix("latest ")
            .unwrap_or_else(|| panic!("not marked as the latest panic: {failure:?}"));
        assert!(is_panic_location(detail, ".rs"), "{failure:?}");
        assert!(!failure.detail.contains("a bug in"), "{failure:?}");
        // SAFETY: destroyed once, by this thread.
        unsafe { asc_airplay_destroy(&mut handle) };
    }

    #[test]
    fn panic_locations_keep_the_crate_and_drop_the_home_folder() {
        for (file, short) in [
            ("src/ffi/airplay.rs", "src/ffi/airplay.rs"),
            (r"src\ffi\airplay.rs", "src/ffi/airplay.rs"),
            (
                "/Users/<you>/Projects/audiosync/airplay/src/sender/driver.rs",
                "airplay/src/sender/driver.rs",
            ),
            (
                r"C:\Users\<you>\.cargo\registry\src\index.crates.io-0000\mdns-sd-0.21.5\src\service_daemon.rs",
                "mdns-sd-0.21.5/src/service_daemon.rs",
            ),
            (
                "/rustc/0123abcd/library/core/src/option.rs",
                "core/src/option.rs",
            ),
            ("/home/<you>/generated/out/bindings.rs", "bindings.rs"),
            (
                "/Users/<you>/src/audiosync/core/target/release/build/out/gen.rs",
                "gen.rs",
            ),
            (r"C:\Users\<you>\src\gen.rs", "gen.rs"),
            ("lib.rs", "lib.rs"),
        ] {
            assert_eq!(panic_location(file, 42), format!("{short}:42"), "{file}");
        }
    }

    /// A browser whose daemon is already gone, as one a bug ended leaves it.
    struct Dead;

    impl Browser for Dead {
        fn poll_event(&mut self) -> Option<BrowseEvent> {
            None
        }

        fn is_dead(&self) -> bool {
            true
        }
    }

    // Discovery that died is the sender's failure too: the host gets the
    // same failed state and Retry, not a list that silently stops changing.
    #[test]
    fn dead_discovery_reports_the_failure_once() {
        let mut handle = asc_airplay_create();
        // SAFETY: a live handle, used by this thread only.
        unsafe { &*handle }
            .sender
            .start_discovery_with(Box::new(Dead));
        let events = events_until(handle, failed);
        assert_eq!(
            events.last().map(without_detail),
            Some(failure_event()),
            "{events:?}"
        );
        let mut raw = blank_event();
        // SAFETY: as above; a local out struct.
        unsafe {
            assert!(
                !asc_airplay_next_event(handle, &mut raw),
                "nothing after it"
            );
            asc_airplay_destroy(&mut handle);
        }
    }

    /// An Apple TV record that asks for the PIN pairing, on 127.0.0.1 at
    /// `port`.
    fn apple_tv_at(id: &str, name: &str, port: u16) -> ServiceRecord {
        ServiceRecord {
            port,
            addresses: vec![ScopedAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST))],
            ..raop(id, name, APPLE_TV)
        }
    }

    /// The handle behind `handle`, for a test that looks inside.
    fn inside<'a>(handle: *mut AscAirPlay) -> &'a mut AscAirPlay {
        // SAFETY: a live handle, used by this thread only.
        unsafe { &mut *handle }
    }

    /// A fresh folder for one test under the crate's own `target`.
    fn scratch(name: &str) -> PathBuf {
        let folder = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("test-scratch")
            .join(name);
        let _ = std::fs::remove_dir_all(&folder);
        folder
    }

    fn pairing_events(events: &[Event]) -> Vec<(i32, String, i32)> {
        events
            .iter()
            .filter(|e| e.kind >= AIRPLAY_EVENT_PIN_NEEDED)
            .map(|e| (e.kind, e.id.clone(), e.error))
            .collect()
    }

    #[test]
    fn pairing_calls_with_null_or_unknown_arguments_do_nothing() {
        let id = c"02005E005301";
        let path = c"pairings.dat";
        // SAFETY: null handles and pointers are part of the contract.
        unsafe {
            assert_eq!(
                asc_airplay_open_pairings(ptr::null_mut(), path.as_ptr()),
                AIRPLAY_PAIRINGS_FAILED
            );
            assert!(!asc_airplay_start_pairing(ptr::null_mut(), id.as_ptr()));
            assert!(!asc_airplay_submit_pin(
                ptr::null_mut(),
                id.as_ptr(),
                c"1234".as_ptr()
            ));
            asc_airplay_cancel_pairing(ptr::null_mut(), id.as_ptr());
            assert!(!asc_airplay_set_password(
                ptr::null_mut(),
                id.as_ptr(),
                c"secret".as_ptr()
            ));
            assert!(!asc_airplay_forget_pairing(ptr::null_mut(), id.as_ptr()));
        }
        let mut handle = asc_airplay_create();
        assert!(!handle.is_null());
        // SAFETY: a live handle, used by this thread only; null strings are
        // part of the contract.
        unsafe {
            assert_eq!(
                asc_airplay_open_pairings(handle, ptr::null()),
                AIRPLAY_PAIRINGS_FAILED
            );
            assert_eq!(
                asc_airplay_open_pairings(handle, c"".as_ptr()),
                AIRPLAY_PAIRINGS_FAILED
            );
            assert!(!asc_airplay_start_pairing(handle, ptr::null()));
            assert!(
                !asc_airplay_start_pairing(handle, id.as_ptr()),
                "only a listed receiver pairs"
            );
            assert!(
                !asc_airplay_submit_pin(handle, id.as_ptr(), c"1234".as_ptr()),
                "no pairing runs"
            );
            assert!(!asc_airplay_submit_pin(handle, id.as_ptr(), ptr::null()));
            asc_airplay_cancel_pairing(handle, id.as_ptr());
            asc_airplay_cancel_pairing(handle, ptr::null());
            assert!(!asc_airplay_set_password(
                handle,
                ptr::null(),
                c"secret".as_ptr()
            ));
            assert!(asc_airplay_set_password(
                handle,
                id.as_ptr(),
                c"secret".as_ptr()
            ));
            assert_eq!(inside(handle).passwords.len(), 1);
            assert!(asc_airplay_set_password(handle, id.as_ptr(), ptr::null()));
            assert!(inside(handle).passwords.is_empty(), "null forgets it");
            assert!(!asc_airplay_forget_pairing(handle, id.as_ptr()));
            assert!(!asc_airplay_forget_pairing(handle, ptr::null()));
            asc_airplay_destroy(&mut handle);
        }
    }

    #[test]
    fn a_pin_pairing_asks_for_the_pin_and_reports_how_it_ended() {
        let receiver = Loopback::start();
        let id = "02005E005308";
        let mut handle = listing(vec![apple_tv_at(id, "Den TV", receiver.port)]);
        let c_id = c"02005E005308";
        // SAFETY: a live handle, used by this thread only; local C strings.
        unsafe {
            assert!(asc_airplay_start_pairing(handle, c_id.as_ptr()));
        }
        // The loopback answers the pairing's first step with an empty 200,
        // which no Apple TV sends: the pairing fails before a PIN, without
        // credentials.
        let events = events_until(handle, |events| !pairing_events(events).is_empty());
        assert_eq!(
            pairing_events(&events),
            vec![(
                AIRPLAY_EVENT_PAIRING_FAILED,
                id.to_owned(),
                AIRPLAY_ERROR_INCOMPATIBLE
            )]
        );
        assert!(
            receiver.methods().contains(&Method::Post),
            "the Apple TV was asked to show a PIN"
        );
        assert!(inside(handle).store.is_empty() && inside(handle).pairing.is_empty());
        // SAFETY: as above.
        unsafe {
            assert!(
                !asc_airplay_submit_pin(handle, c_id.as_ptr(), c"1234".as_ptr()),
                "the pairing is over"
            );
        }

        // A pairing whose Apple TV shows a PIN (its SRP answer is the
        // crate's to test) waits for it.
        let key = inside(handle).listed[id].device.key.clone();
        inside(handle).pairing.insert(id.to_owned(), key.clone());
        inside(handle).take_in(SenderEvent::Pairing {
            key,
            event: PairingEvent::PinRequested,
        });
        let events = events_until(handle, |events| !pairing_events(events).is_empty());
        assert_eq!(
            pairing_events(&events),
            vec![(AIRPLAY_EVENT_PIN_NEEDED, id.to_owned(), 0)]
        );
        // SAFETY: as above.
        unsafe {
            assert!(
                !asc_airplay_submit_pin(handle, c_id.as_ptr(), c"12a4".as_ptr()),
                "digits only"
            );
            assert!(!asc_airplay_submit_pin(handle, c_id.as_ptr(), c"".as_ptr()));
            assert!(asc_airplay_submit_pin(
                handle,
                c_id.as_ptr(),
                c"1234".as_ptr()
            ));
            asc_airplay_cancel_pairing(handle, c_id.as_ptr());
            assert!(inside(handle).pairing.is_empty(), "cancelled");
            assert!(
                !asc_airplay_submit_pin(handle, c_id.as_ptr(), c"1234".as_ptr()),
                "a cancelled pairing takes no PIN"
            );
            asc_airplay_destroy(&mut handle);
        }
    }

    #[test]
    fn a_pairing_asks_for_the_password_and_forgets_one_turned_down() {
        let receiver = Loopback::start_with(true);
        let id = "02005E005309";
        let mut handle = listing(vec![apple_tv_at(id, "Den TV", receiver.port)]);
        let c_id = c"02005E005309";
        // SAFETY: a live handle, used by this thread only; local C strings.
        unsafe {
            assert!(asc_airplay_start_pairing(handle, c_id.as_ptr()));
        }
        let events = events_until(handle, |events| {
            events
                .iter()
                .any(|e| e.kind == AIRPLAY_EVENT_PASSWORD_NEEDED)
        });
        assert_eq!(
            pairing_events(&events),
            vec![(
                AIRPLAY_EVENT_PASSWORD_NEEDED,
                id.to_owned(),
                AIRPLAY_ERROR_PASSWORD_REQUIRED
            )]
        );
        // SAFETY: as above.
        unsafe {
            assert!(asc_airplay_set_password(
                handle,
                c_id.as_ptr(),
                c"not it".as_ptr()
            ));
            assert!(asc_airplay_start_pairing(handle, c_id.as_ptr()));
        }
        let events = events_until(handle, |events| {
            events
                .iter()
                .any(|e| e.kind == AIRPLAY_EVENT_PASSWORD_NEEDED)
        });
        let needed = pairing_events(&events);
        assert_eq!(
            needed,
            vec![(
                AIRPLAY_EVENT_PASSWORD_NEEDED,
                id.to_owned(),
                AIRPLAY_ERROR_WRONG_PASSWORD
            )]
        );
        assert!(
            inside(handle).passwords.is_empty(),
            "a password turned down is forgotten"
        );
        assert!(events.iter().all(|e| !e.detail.contains("not it")));
        // SAFETY: as above.
        unsafe { asc_airplay_destroy(&mut handle) };
    }

    #[test]
    fn credentials_make_an_apple_tv_available_until_it_turns_them_down() {
        let id = "02005E005310";
        let mut handle = listing(vec![raop(id, "Den TV", APPLE_TV)]);
        let key = inside(handle).listed[id].device.key.clone();
        let credentials = audiosync_airplay::pairing::Credentials::new(
            "0123456789ABCDEF",
            [7; audiosync_airplay::pairing::SECRET_BYTES],
        )
        .expect("valid credentials");
        inside(handle).pairing.insert(id.to_owned(), key.clone());
        inside(handle).take_in(SenderEvent::Pairing {
            key: key.clone(),
            event: PairingEvent::Paired(credentials),
        });
        let seen = next(handle, 1);
        assert_eq!(seen[0].event, AIRPLAY_CHANGED);
        assert_eq!(seen[0].availability, AIRPLAY_AVAILABLE, "paired");
        let events = events_until(handle, |events| {
            events.iter().any(|e| e.kind == AIRPLAY_EVENT_PAIRED)
        });
        let paired = events
            .iter()
            .find(|e| e.kind == AIRPLAY_EVENT_PAIRED)
            .expect("paired");
        assert_eq!(
            (paired.id.as_str(), paired.detail.as_str()),
            (id, ""),
            "stored (in memory: no file opened)"
        );
        assert!(!paired.detail.contains("0123456789ABCDEF"));
        assert!(inside(handle).pairing.is_empty());

        // The Apple TV was reset: it turns the credentials down.
        inside(handle).take_in(SenderEvent::Speaker {
            key,
            event: SpeakerEvent::Error(SpeakerError {
                error: audiosync_airplay::error::SessionError::new(ErrorKind::PairingRequired),
                reason: UserReason::PairingRequired,
                retry_at: None,
            }),
        });
        let seen = next(handle, 1);
        assert_eq!(seen[0].availability, AIRPLAY_NEEDS_PIN, "forgotten");
        assert!(inside(handle).store.is_empty());
        let events = events_until(handle, |events| {
            events.iter().any(|e| e.kind == AIRPLAY_EVENT_ERROR)
        });
        assert!(
            events
                .iter()
                .any(|e| e.kind == AIRPLAY_EVENT_ERROR && e.error == AIRPLAY_ERROR_PAIRING_REQUIRED)
        );
        // SAFETY: a live handle, used by this thread only.
        unsafe { asc_airplay_destroy(&mut handle) };
    }

    #[test]
    fn a_wrong_pin_and_a_cancelled_pairing_are_told_apart() {
        let id = "02005E005311";
        let mut handle = listing(vec![raop(id, "Den TV", APPLE_TV)]);
        let key = inside(handle).listed[id].device.key.clone();
        let mut error = audiosync_airplay::error::SessionError::new(ErrorKind::PairingFailed);
        error.status = Some(470);
        inside(handle).take_in(SenderEvent::Pairing {
            key: key.clone(),
            event: PairingEvent::Failed {
                error: error.clone(),
                wrong_pin: true,
            },
        });
        inside(handle).take_in(SenderEvent::Pairing {
            key: key.clone(),
            event: PairingEvent::Failed {
                error: audiosync_airplay::error::SessionError::new(ErrorKind::Timeout),
                wrong_pin: false,
            },
        });
        inside(handle).take_in(SenderEvent::Pairing {
            key,
            event: PairingEvent::Stopped,
        });
        let events = events_until(handle, |events| pairing_events(events).len() >= 2);
        assert_eq!(
            pairing_events(&events),
            vec![
                (
                    AIRPLAY_EVENT_PAIRING_FAILED,
                    id.to_owned(),
                    AIRPLAY_ERROR_WRONG_PIN
                ),
                (
                    AIRPLAY_EVENT_PAIRING_FAILED,
                    id.to_owned(),
                    AIRPLAY_ERROR_NOT_RESPONDING
                ),
            ],
            "a stopped pairing reports nothing"
        );
        // SAFETY: a live handle, used by this thread only.
        unsafe { asc_airplay_destroy(&mut handle) };
    }

    /// A receiver on 127.0.0.1 whose port takes connections and never
    /// answers: a pairing with it stays open, waiting, until it is
    /// cancelled.
    fn silent_receiver() -> (TcpListener, u16) {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("a TCP port");
        let port = listener.local_addr().expect("its address").port();
        (listener, port)
    }

    /// Waits at most 10 s for the sender itself, not the handle's queues,
    /// to report that its pairing with `id` stopped: it does once a cancel
    /// reached it.
    fn sender_stopped_pairing(handle: *mut AscAirPlay, id: &str) -> bool {
        wait_for(|| {
            while let Some(event) = inside(handle).sender.poll_event() {
                if let SenderEvent::Pairing {
                    key,
                    event: PairingEvent::Stopped,
                } = &event
                    && receiver_id(key) == id
                {
                    return true;
                }
            }
            false
        })
    }

    // The PIN wait has no timeout (P51), so the Apple TV may drop out of
    // discovery while the user reads its PIN: the pairing still takes the
    // PIN, and turning the speaker off still cancels it at the sender
    // instead of leaving it waiting on an open connection.
    #[test]
    fn a_receiver_that_goes_away_mid_pairing_still_takes_the_pin_and_the_cancel() {
        let (_receiver, port) = silent_receiver();
        let id = "02005E005313";
        let c_id = c"02005E005313";
        let mut handle = listing(vec![apple_tv_at(id, "Den TV", port)]);
        let key = inside(handle).listed[id].device.key.clone();
        // SAFETY: a live handle, used by this thread only; a local C string.
        unsafe {
            assert!(asc_airplay_start_pairing(handle, c_id.as_ptr()));
        }
        inside(handle).take_in(SenderEvent::Pairing {
            key: key.clone(),
            event: PairingEvent::PinRequested,
        });
        inside(handle).take_in(SenderEvent::Receiver(ReceiverEvent::Gone(key)));
        assert_eq!(next(handle, 1)[0].event, AIRPLAY_GONE);
        // SAFETY: as above; local C strings.
        unsafe {
            assert!(
                asc_airplay_submit_pin(handle, c_id.as_ptr(), c"1234".as_ptr()),
                "the PIN still goes to the pairing"
            );
            asc_airplay_cancel_pairing(handle, c_id.as_ptr());
        }
        assert!(
            sender_stopped_pairing(handle, id),
            "the cancel reached the sender"
        );
        // SAFETY: as above; local C strings.
        unsafe {
            assert!(
                !asc_airplay_submit_pin(handle, c_id.as_ptr(), c"1234".as_ptr()),
                "a cancelled pairing takes no PIN"
            );
            asc_airplay_destroy(&mut handle);
        }
    }

    // A group click can cancel a member's pairing and start it again back
    // to back: the cancelled pairing's Stopped, reported after the new one
    // started, doesn't end the new one, whose PIN would then be refused.
    #[test]
    fn a_late_stopped_from_a_cancelled_pairing_does_not_end_the_next_one() {
        let (_receiver, port) = silent_receiver();
        let id = "02005E005314";
        let c_id = c"02005E005314";
        let mut handle = listing(vec![apple_tv_at(id, "Den TV", port)]);
        let key = inside(handle).listed[id].device.key.clone();
        // Marked as running without a pairing on the sender, so its real
        // end can't arrive before the one below (see the next test).
        inside(handle).pairing.insert(id.to_owned(), key.clone());
        // SAFETY: a live handle, used by this thread only; a local C string.
        unsafe {
            asc_airplay_cancel_pairing(handle, c_id.as_ptr());
            assert!(asc_airplay_start_pairing(handle, c_id.as_ptr()));
        }
        assert!(
            inside(handle).ending.contains_key(id),
            "the new pairing waits for the cancelled one's end"
        );
        inside(handle).take_in(SenderEvent::Pairing {
            key,
            event: PairingEvent::Stopped,
        });
        // SAFETY: as above; local C strings.
        unsafe {
            assert!(
                asc_airplay_submit_pin(handle, c_id.as_ptr(), c"1234".as_ptr()),
                "the new pairing takes its PIN"
            );
            asc_airplay_destroy(&mut handle);
        }
    }

    /// The events the handle has ready now, taken without waiting.
    fn waiting_events(handle: *mut AscAirPlay) -> Vec<Event> {
        let mut seen = Vec::new();
        loop {
            let mut raw = blank_event();
            // SAFETY: a live handle, used by this thread only; a local out struct.
            if !unsafe { asc_airplay_next_event(handle, &mut raw) } {
                return seen;
            }
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
        }
    }

    // A cancelled pairing may have failed before the cancel reached the
    // sender: its failure, taken after a new pairing started back to back,
    // belongs to the cancelled one. It neither reaches the host nor ends
    // the new pairing, which still takes its PIN once it asks for one.
    #[test]
    fn a_late_failure_from_a_cancelled_pairing_does_not_end_the_next_one() {
        let (_receiver, port) = silent_receiver();
        let id = "02005E005315";
        let c_id = c"02005E005315";
        let mut handle = listing(vec![apple_tv_at(id, "Den TV", port)]);
        let key = inside(handle).listed[id].device.key.clone();
        waiting_events(handle);
        // The first pairing is marked as running without a pairing on the
        // sender, so no real end of it can arrive before the one below and
        // let the next pairing start at once (the test would then race the
        // sender's thread).
        inside(handle).pairing.insert(id.to_owned(), key.clone());
        // SAFETY: a live handle, used by this thread only; a local C string.
        unsafe {
            asc_airplay_cancel_pairing(handle, c_id.as_ptr());
            assert!(asc_airplay_start_pairing(handle, c_id.as_ptr()));
        }
        assert!(
            inside(handle).ending.contains_key(id),
            "the new pairing waits for the cancelled one's end"
        );
        inside(handle).take_in(SenderEvent::Pairing {
            key: key.clone(),
            event: PairingEvent::Failed {
                error: audiosync_airplay::error::SessionError::new(ErrorKind::PairingCancelled),
                wrong_pin: false,
            },
        });
        assert_eq!(
            pairing_events(&waiting_events(handle)),
            vec![],
            "the cancelled pairing's end isn't the host's news"
        );
        inside(handle).take_in(SenderEvent::Pairing {
            key,
            event: PairingEvent::PinRequested,
        });
        assert_eq!(
            pairing_events(&waiting_events(handle)),
            vec![(AIRPLAY_EVENT_PIN_NEEDED, id.to_owned(), 0)],
            "the new pairing asks for its PIN"
        );
        // SAFETY: as above; local C strings.
        unsafe {
            assert!(
                asc_airplay_submit_pin(handle, c_id.as_ptr(), c"1234".as_ptr()),
                "the new pairing takes its PIN"
            );
            asc_airplay_destroy(&mut handle);
        }
    }

    /// A late success from a cancelled pairing, as the sender reports it.
    fn late_success(key: DeviceKey) -> SenderEvent {
        SenderEvent::Pairing {
            key,
            event: PairingEvent::Paired(
                audiosync_airplay::pairing::Credentials::new(
                    "0123456789ABCDEF",
                    [5; audiosync_airplay::pairing::SECRET_BYTES],
                )
                .expect("valid credentials"),
            ),
        }
    }

    /// After a late success: the credentials are stored, the receiver is
    /// listed as able to play, and the host was told `told` of pairings.
    fn stored_and_told(handle: *mut AscAirPlay, id: &str, told: Vec<(i32, String, i32)>) {
        assert_eq!(
            pairing_events(&waiting_events(handle)),
            vec![],
            "nothing is told before the receiver change it follows"
        );
        assert_eq!(next(handle, 1)[0].availability, AIRPLAY_AVAILABLE, "stored");
        assert!(inside(handle).store.contains(id));
        assert_eq!(pairing_events(&waiting_events(handle)), told);
    }

    // A pairing replaced by a new one may have succeeded just before. Its
    // credentials are real, so they are stored and the receiver is listed
    // as able to play; and the Apple TV is paired now, so the new pairing,
    // still waiting for that end, never starts (no second PIN): the host
    // hears of it as a pairing that paired.
    #[test]
    fn a_late_success_from_a_replaced_pairing_finishes_the_new_one_as_paired() {
        let (_receiver, port) = silent_receiver();
        let id = "02005E005316";
        let c_id = c"02005E005316";
        let mut handle = listing(vec![apple_tv_at(id, "Den TV", port)]);
        let key = inside(handle).listed[id].device.key.clone();
        waiting_events(handle);
        // The first pairing is marked as running without a pairing on the
        // sender, so no real end of it can arrive before the one below.
        inside(handle).pairing.insert(id.to_owned(), key.clone());
        // SAFETY: a live handle, used by this thread only; a local C string.
        unsafe { assert!(asc_airplay_start_pairing(handle, c_id.as_ptr())) };
        assert!(
            matches!(inside(handle).ending.get(id), Some(Some(_))),
            "the new pairing waits for the replaced one's end"
        );
        inside(handle).take_in(late_success(key));
        stored_and_told(handle, id, vec![(AIRPLAY_EVENT_PAIRED, id.to_owned(), 0)]);
        assert!(
            !inside(handle).ending.contains_key(id),
            "nothing waits any more"
        );
        // SAFETY: as above; local C strings.
        unsafe {
            assert!(
                !asc_airplay_submit_pin(handle, c_id.as_ptr(), c"1234".as_ptr()),
                "no pairing runs to take a PIN"
            );
            asc_airplay_destroy(&mut handle);
        }
    }

    // The user cancels a pairing and starts it again at once, but the
    // cancelled pairing had already succeeded. The pairing started since
    // waits for that end and then never starts, so no second PIN shows for
    // an Apple TV that is paired: the host hears of it as a pairing that
    // paired, and the credentials are stored.
    #[test]
    fn a_late_success_from_a_cancelled_pairing_finishes_the_next_one_as_paired() {
        let (_receiver, port) = silent_receiver();
        let id = "02005E005318";
        let c_id = c"02005E005318";
        let mut handle = listing(vec![apple_tv_at(id, "Den TV", port)]);
        let key = inside(handle).listed[id].device.key.clone();
        waiting_events(handle);
        // Marked as running without a pairing on the sender (see above).
        inside(handle).pairing.insert(id.to_owned(), key.clone());
        // SAFETY: a live handle, used by this thread only; a local C string.
        unsafe {
            asc_airplay_cancel_pairing(handle, c_id.as_ptr());
            assert!(asc_airplay_start_pairing(handle, c_id.as_ptr()));
        }
        assert!(
            matches!(inside(handle).ending.get(id), Some(Some(_))),
            "the new pairing waits for the cancelled one's end"
        );
        inside(handle).take_in(late_success(key));
        stored_and_told(handle, id, vec![(AIRPLAY_EVENT_PAIRED, id.to_owned(), 0)]);
        assert!(
            !inside(handle).ending.contains_key(id),
            "nothing waits any more"
        );
        // SAFETY: as above; local C strings.
        unsafe {
            assert!(
                !asc_airplay_submit_pin(handle, c_id.as_ptr(), c"1234".as_ptr()),
                "no pairing runs to take a PIN"
            );
            asc_airplay_destroy(&mut handle);
        }
    }

    // With no pairing waiting for it (the one started since was cancelled
    // too), a cancelled pairing's success is stored and the receiver listed
    // again, but the host hears nothing of it.
    #[test]
    fn a_late_success_with_no_pairing_waiting_is_only_stored() {
        let (_receiver, port) = silent_receiver();
        let id = "02005E005319";
        let c_id = c"02005E005319";
        let mut handle = listing(vec![apple_tv_at(id, "Den TV", port)]);
        let key = inside(handle).listed[id].device.key.clone();
        waiting_events(handle);
        inside(handle).pairing.insert(id.to_owned(), key.clone());
        // SAFETY: a live handle, used by this thread only; a local C string.
        unsafe {
            asc_airplay_cancel_pairing(handle, c_id.as_ptr());
            assert!(asc_airplay_start_pairing(handle, c_id.as_ptr()));
            asc_airplay_cancel_pairing(handle, c_id.as_ptr());
        }
        assert!(
            matches!(inside(handle).ending.get(id), Some(None)),
            "nothing waits for the cancelled pairing's end"
        );
        inside(handle).take_in(late_success(key));
        stored_and_told(handle, id, vec![]);
        assert!(!inside(handle).ending.contains_key(id));
        assert!(!inside(handle).pairing.contains_key(id), "none started");
        // SAFETY: a live handle, used by this thread only.
        unsafe { asc_airplay_destroy(&mut handle) };
    }

    // The host takes the receiver changes first, then the events. An Apple
    // TV that turns its stored credentials down is listed as needing a PIN
    // again before its error is queued; when that happens while the host
    // takes events, the error waits for the next round, after the change,
    // so the host pairs it again instead of switching it off.
    #[test]
    fn an_event_waits_for_the_receiver_change_queued_before_it() {
        let id = "02005E005317";
        let mut handle = listing(vec![raop(id, "Den TV", APPLE_TV)]);
        let key = inside(handle).listed[id].device.key.clone();
        let credentials = audiosync_airplay::pairing::Credentials::new(
            "0123456789ABCDEF",
            [3; audiosync_airplay::pairing::SECRET_BYTES],
        )
        .expect("valid credentials");
        inside(handle)
            .store
            .insert(id, credentials)
            .expect("in memory");
        waiting_events(handle);

        // The host's round: no receiver change waits, so it takes events.
        let mut receiver = empty();
        // SAFETY: a live handle, used by this thread only; a local out struct.
        assert!(!unsafe { asc_airplay_next_receiver(handle, &mut receiver) });
        inside(handle).take_in(SenderEvent::Pairing {
            key: key.clone(),
            event: PairingEvent::PinRequested,
        });
        inside(handle).take_in(SenderEvent::Speaker {
            key,
            event: SpeakerEvent::Error(SpeakerError {
                error: audiosync_airplay::error::SessionError::new(ErrorKind::PairingRequired),
                reason: UserReason::PairingRequired,
                retry_at: None,
            }),
        });
        let first = waiting_events(handle);
        assert_eq!(
            first.iter().map(|e| e.kind).collect::<Vec<_>>(),
            vec![AIRPLAY_EVENT_PIN_NEEDED],
            "an event queued before the change goes out; the error waits: {first:?}"
        );

        // The next round takes the change, then the error.
        let seen = next(handle, 1);
        assert_eq!(seen[0].availability, AIRPLAY_NEEDS_PIN, "forgotten");
        let second = waiting_events(handle);
        assert_eq!(
            second.iter().map(|e| (e.kind, e.error)).collect::<Vec<_>>(),
            vec![(AIRPLAY_EVENT_ERROR, AIRPLAY_ERROR_PAIRING_REQUIRED)]
        );
        // SAFETY: a live handle, used by this thread only.
        unsafe { asc_airplay_destroy(&mut handle) };
    }

    #[test]
    fn pairings_last_across_handles_through_the_store_file() {
        let folder = scratch("ffi-pairings");
        let path = folder.join("AudioSync").join("airplay-pairings.dat");
        let c_path = CString::new(path.to_str().expect("UTF-8")).expect("no NUL");
        let id = "02005E005312";

        let mut first = asc_airplay_create();
        // SAFETY: a live handle, used by this thread only; a local C string.
        unsafe {
            assert_eq!(asc_airplay_open_pairings(first, c_path.as_ptr()), 0);
        }
        start_with(
            first,
            vec![BrowseEvent::Resolved(raop(id, "Den TV", APPLE_TV))],
        );
        assert_eq!(next(first, 1)[0].availability, AIRPLAY_NEEDS_PIN);
        let key = inside(first).listed[id].device.key.clone();
        inside(first).take_in(SenderEvent::Pairing {
            key,
            event: PairingEvent::Paired(
                audiosync_airplay::pairing::Credentials::new(
                    "0123456789ABCDEF",
                    [9; audiosync_airplay::pairing::SECRET_BYTES],
                )
                .expect("valid credentials"),
            ),
        });
        assert_eq!(next(first, 1)[0].availability, AIRPLAY_AVAILABLE);
        assert!(path.exists(), "written at once");
        // SAFETY: as above.
        unsafe { asc_airplay_destroy(&mut first) };

        let mut second = asc_airplay_create();
        // SAFETY: a live handle, used by this thread only; a local C string.
        unsafe {
            assert_eq!(asc_airplay_open_pairings(second, c_path.as_ptr()), 1);
        }
        start_with(
            second,
            vec![BrowseEvent::Resolved(raop(id, "Den TV", APPLE_TV))],
        );
        assert_eq!(
            next(second, 1)[0].availability,
            AIRPLAY_AVAILABLE,
            "paired at the last run"
        );
        let c_id = CString::new(id).expect("no NUL");
        // SAFETY: as above.
        unsafe {
            assert!(asc_airplay_forget_pairing(second, c_id.as_ptr()));
            assert!(!asc_airplay_forget_pairing(second, c_id.as_ptr()));
        }
        assert_eq!(next(second, 1)[0].availability, AIRPLAY_NEEDS_PIN);
        assert!(!path.exists(), "nothing paired, no file");
        // SAFETY: as above.
        unsafe { asc_airplay_destroy(&mut second) };

        // A file in no known format is ignored.
        std::fs::create_dir_all(path.parent().expect("its folder")).expect("folder");
        std::fs::write(&path, b"something else").expect("written");
        let mut third = asc_airplay_create();
        // SAFETY: a live handle, used by this thread only; a local C string.
        unsafe {
            assert_eq!(
                asc_airplay_open_pairings(third, c_path.as_ptr()),
                AIRPLAY_PAIRINGS_IGNORED
            );
            asc_airplay_destroy(&mut third);
        }
        let _ = std::fs::remove_dir_all(&folder);
    }
}
