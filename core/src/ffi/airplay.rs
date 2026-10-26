//! AudioSync's own AirPlay sender ([`audiosync_airplay`]) through the C
//! interface, for the macOS app, which links one Rust static library and so
//! reaches the sender through the core's.
//!
//! The host creates a sender, starts browsing the local network and takes
//! the receivers it finds, one change at a time, with their ID, name, model,
//! whether they can play from this sender and discovery's hints. It adds and
//! removes speakers by those IDs, sets each one's volume and the master, and
//! takes what happens to them as events: their state, the errors that end
//! their sessions with the reason to show, their latency, warnings, a
//! volume changed on the speaker itself (P122), and the base latency L
//! local outputs line up with (P113, P114). The capture thread
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
//! nothing here logs or returns a PIN, password, key or credential. The
//! AirPlay 2 path, behind its hidden switch, keeps its device ID and the
//! HomeKit credentials its pairings give in a file of their own
//! ([`asc_airplay_open_airplay2_identity`], see
//! [`crate::airplay2_identity`]), the same way.
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
use std::collections::{HashMap, HashSet, VecDeque};
use std::ffi::{CStr, CString, c_char};
use std::panic::{self, AssertUnwindSafe};
use std::path::PathBuf;
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Once, OnceLock};
use std::time::{Duration, Instant};

use audiosync_airplay::audio::SAMPLE_RATE;
use audiosync_airplay::dacp::{DacpAdvertiser, RemoteCommand};
use audiosync_airplay::discovery::mdns::{MdnsAdvertiser, MdnsBrowser};
use audiosync_airplay::discovery::{Browser, Device, DeviceKey, ProtocolPath, ServiceKind};
use audiosync_airplay::error::{ErrorKind, UnsupportedReason, UserReason, Warning};
use audiosync_airplay::homekit::HomeKitCredentials;
use audiosync_airplay::input::InputWriter;
use audiosync_airplay::latency::session_latency_frames;
use audiosync_airplay::metadata::{Artwork, TrackInfo};
use audiosync_airplay::pairing::{Credentials, PairingEvent};
use audiosync_airplay::rtsp::Method;
use audiosync_airplay::secret::Secret;
use audiosync_airplay::sender::{
    AIRPLAY2_SWITCH_ENV, NowPlaying, Progress, ReceiverEvent, ReceiverInfo, Sender, SenderConfig,
    SenderEvent, SpeakerError, SpeakerEvent, SpeakerOptions, SpeakerState, airplay2_switch,
};
use audiosync_airplay::stream::{
    DEFAULT_LATENCY_FRAMES, MAX_DELAY, MAX_LATENCY_FRAMES, MIN_LATENCY_FRAMES, SpeakerCounters,
    SpeakerLatency,
};
use audiosync_airplay::targets::{Target, TargetChange, TargetId};
use audiosync_airplay::volume::{MIN_DB, Volume};

use super::{host_slice, stereo_samples};
use crate::airplay_pairings::{Access, Loaded, PairingStore};
use crate::airplay2_identity::IdentityStore;
use crate::fade::InputFade;
use crate::policy::remote::{
    Action, App, Command, Control, Event as RemoteEvent, MUTED_DB, OwnVolume, Playback, Remote,
};
use crate::resample::Resampler;
use crate::ring::is_supported_rate;
use crate::volume::{
    NO_ZONE_POSITION, ReadBack, RowMove, device_level, device_level_for_db, level_on_speaker,
    level_set_on_speaker, row_move, sanitize_own,
};

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
/// It refused NTP timing, and PTP timing can't take its place (P157,
/// P158).
pub const AIRPLAY_REASON_TIMING: i32 = 7;
/// Another AirPlay 2 speaker plays, and only one can at a time (P169).
pub const AIRPLAY_REASON_ONE_AIRPLAY2: i32 = 8;
/// The Apple TV of a home theater group that plays, or the group of an
/// Apple TV that plays by itself (P177).
pub const AIRPLAY_REASON_PLAYS_THROUGH_SPEAKERS: i32 = 9;

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
/// The AirPlay 2 switch is on and the sender reaches it over AirPlay 2.
pub const AIRPLAY_FLAG_OVER_AIRPLAY2: u32 = 256;
/// Credentials are kept for it (a PIN pairing's, either path), and no
/// answer turned them down this run (P154).
pub const AIRPLAY_FLAG_PAIRED: u32 = 512;
/// A stereo pair or home theater group (P176): its ID is `pair:<tsid>` or
/// `theater:<group>`, and the host adds it like a speaker. Listed only with
/// the AirPlay 2 switch on.
pub const AIRPLAY_FLAG_TARGET: u32 = 1024;
/// A target with a member out of view, playing nothing, or reporting its
/// partner unreachable (P181). An added target's state and errors come
/// under its own ID, derived from its members'.
pub const AIRPLAY_FLAG_INCOMPLETE: u32 = 2048;
/// The sender reaches it over AirPlay 2 and it asks for a code every time
/// it connects (P61): auto-connect leaves it off.
pub const AIRPLAY_FLAG_CODE_EVERY_TIME: u32 = 4096;
/// Credentials are stored for it, also ones an answer turned down (P154),
/// which [`asc_airplay_forget_pairing`] drops.
pub const AIRPLAY_FLAG_CREDENTIALS_KEPT: u32 = 8192;

/// What a sender event reports ([`AscAirPlayEvent::kind`]): a speaker's
/// state changed ([`AscAirPlayEvent::state`]).
pub const AIRPLAY_EVENT_STATE: i32 = 1;
/// An error ended a speaker's session ([`AscAirPlayEvent::error`]).
pub const AIRPLAY_EVENT_ERROR: i32 = 2;
/// A speaker's latencies, when it starts playing and whenever its first
/// sound time moves: L + d_s, and the latency R_s the receiver reported
/// with its sources (P110, P113, P115, P199, P202).
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
/// pairing shows a PIN again, P52), [`AIRPLAY_ERROR_NOT_RESPONDING`] when it
/// didn't answer in time, [`AIRPLAY_ERROR_PAIRING_REQUIRED`] when it turned
/// the pairing down or cancelled it, or another reason. The detail says
/// more, for a debug log.
pub const AIRPLAY_EVENT_PAIRING_FAILED: i32 = 8;
/// A pairing succeeded and the receiver is listed as available again; its
/// credentials are in the store (P54). [`AscAirPlayEvent::detail`] is ""
/// when they were written to the file, or why they last only until the
/// handle is destroyed.
pub const AIRPLAY_EVENT_PAIRED: i32 = 9;
/// A person changed the speaker's volume on the speaker itself
/// ([`asc_airplay_read_volumes`], P122, P180): [`AscAirPlayEvent::volume`]
/// is the slider position that plays at that level with the master as it
/// is. The sender already took it as the speaker's position and level, so
/// a reconnect or a target member that starts late plays at it too.
pub const AIRPLAY_EVENT_VOLUME: i32 = 10;
/// Something about a speaker's session the host may show; the session goes
/// on. [`AscAirPlayEvent::warning`] says what, the detail says more, for a
/// debug log.
pub const AIRPLAY_EVENT_WARNING: i32 = 11;
/// A line for the debug log about a speaker's session
/// ([`AscAirPlayEvent::detail`]); sent only while `AUDIOSYNC_DEBUG` is `1`.
pub const AIRPLAY_EVENT_TRACE: i32 = 12;
/// A speaker's remote asked for a transport control
/// ([`AscAirPlayEvent::remote`]): the host sends it through its now-playing
/// control to the app that control follows (S274).
pub const AIRPLAY_EVENT_REMOTE: i32 = 13;
/// The host set an Lf while a speaker sounds: the sender holds it
/// ([`AscAirPlayEvent::latency_ms`] is the held L) until the stream has no
/// speaker or the timeline restarts, when a timeline event reports it
/// (P201).
pub const AIRPLAY_EVENT_LATENCY_HELD: i32 = 14;

/// A remote event's control ([`AscAirPlayEvent::remote`]): play-pause.
pub const AIRPLAY_REMOTE_PLAY_PAUSE: i32 = 1;
/// Next track.
pub const AIRPLAY_REMOTE_NEXT: i32 = 2;
/// Previous track.
pub const AIRPLAY_REMOTE_PREVIOUS: i32 = 3;

/// What the app the host's now-playing control follows does, for
/// [`asc_airplay_set_playback`]: no reading says.
pub const AIRPLAY_PLAYBACK_UNKNOWN: i32 = 0;
/// It plays.
pub const AIRPLAY_PLAYBACK_PLAYING: i32 = 1;
/// It is paused.
pub const AIRPLAY_PLAYBACK_PAUSED: i32 = 2;

/// [`AscAirPlayEvent::receiver_latency_sources`] bits: `Audio-Latency` of a
/// RECORD answer (P196).
pub const AIRPLAY_LATENCY_SOURCE_AUDIO_LATENCY: u32 = 1;
/// `audioLatencies` of `/info` (P197).
pub const AIRPLAY_LATENCY_SOURCE_AUDIO_LATENCIES: u32 = 2;
/// `arrivalToRenderLatencyMs` of the stream SETUP answer (P198).
pub const AIRPLAY_LATENCY_SOURCE_ARRIVAL_TO_RENDER: u32 = 4;

/// Whether the host keeps its debug log, which turns on the sender's trace
/// lines ([`SpeakerOptions::trace`]).
fn debug_trace_on() -> bool {
    std::env::var("AUDIOSYNC_DEBUG").as_deref() == Ok("1")
}

/// What a warning reports ([`AscAirPlayEvent::warning`]): the receiver says
/// nothing is plugged into its audio jack (P24).
pub const AIRPLAY_WARNING_JACK_DISCONNECTED: i32 = 1;
/// `POST /auth-setup` failed (P60).
pub const AIRPLAY_WARNING_AUTH_SETUP_FAILED: i32 = 2;
/// A volume, metadata, flush or keep-alive request failed (P127, P131,
/// P138).
pub const AIRPLAY_WARNING_REQUEST_FAILED: i32 = 3;
/// PTP port 319 or 320 can't be bound, so receivers that need PTP timing
/// can't play; reported once per sender (P158).
pub const AIRPLAY_WARNING_PTP_PORTS_TAKEN: i32 = 5;
/// The speaker has no address in the PTP service's family and plays on
/// NTP (P158, P179).
pub const AIRPLAY_WARNING_PTP_OUTSIDE_FAMILY: i32 = 6;
/// Three volume reads in a row failed on an AirPlay 2 session: it isn't
/// asked again (P184, S271).
pub const AIRPLAY_WARNING_VOLUME_READS_STOPPED: i32 = 7;
/// A receiver refused an artwork image or left it unanswered: the session
/// falls back to the small image or sends no more artwork, and plays on;
/// once per session (P185).
pub const AIRPLAY_WARNING_ARTWORK_REFUSED: i32 = 8;
/// The receiver's remote control can't reach AudioSync from this speaker:
/// no command server port could be bound on the session's local address
/// (P190).
pub const AIRPLAY_WARNING_REMOTE_CONTROL_UNAVAILABLE: i32 = 9;
/// The `_dacp._tcp` record couldn't be registered, so receivers may not
/// find the remote control (P188).
pub const AIRPLAY_WARNING_REMOTE_CONTROL_NOT_ADVERTISED: i32 = 10;
/// The receiver's reported latency needs a larger Lf than the sounding
/// stream it joined can take: it plays in step at the stream's Lf; the
/// detail names R_s and Lf (P201).
pub const AIRPLAY_WARNING_LATENCY_SHORTFALL: i32 = 11;
/// A session bound the PTP ports after [`AIRPLAY_WARNING_PTP_PORTS_TAKEN`]:
/// they are free again, and a later clash is reported anew (P158).
pub const AIRPLAY_WARNING_PTP_FREE: i32 = 12;

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
/// A paired speaker didn't prove it is the one that was paired, so the
/// sender didn't connect; its stored credentials stay (P56).
pub const AIRPLAY_ERROR_RECEIVER_UNVERIFIED: i32 = 11;

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

/// What [`asc_airplay_pairings_access`] and
/// [`asc_airplay_airplay2_identity_access`] return for a null or dead
/// handle, an identity file left unread because AirPlay 2 is off, or a
/// file that wasn't read: another account owns it, it isn't a regular
/// file, or opening it failed.
pub const AIRPLAY_FILE_UNREAD: i32 = -1;
/// Only its owner could read or write the file, or there was none.
pub const AIRPLAY_FILE_PRIVATE: i32 = 0;
/// Other accounts could read or write the file; it is owner-only now.
pub const AIRPLAY_FILE_TIGHTENED: i32 = 1;
/// Other accounts can read or write the file, and its mode couldn't be
/// changed.
pub const AIRPLAY_FILE_STILL_OPEN: i32 = 2;

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
    /// The ID of the target a receiver is a member of; "" for none, for a
    /// target, and with the AirPlay 2 switch off (P176).
    pub target: *const c_char,
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
    /// An error event's error, a warning event's warning, or a failure
    /// event's panic location (S205), for a debug log (no secrets).
    pub detail: *const c_char,
    /// A volume event's slider position, 0 to 100.
    pub volume: f64,
    /// A warning event's `AIRPLAY_WARNING_*`.
    pub warning: i32,
    /// A remote event's `AIRPLAY_REMOTE_*`.
    pub remote: i32,
    /// A latency event's R_s in microseconds, 0 when no source gave a
    /// value (P199, P202).
    pub receiver_latency_us: i64,
    /// A latency event's `AIRPLAY_LATENCY_SOURCE_*` bits, 0 when none.
    pub receiver_latency_sources: u32,
    /// A volume event's device level D, 0 to 100, which the row shows
    /// rounded down (S290).
    pub device_level: f64,
}

/// A speaker the host added.
struct Added {
    key: DeviceKey,
    /// Its slider position, 0 to 100.
    position: f64,
    /// A volume read was asked since the host last set the volume.
    reading: bool,
    /// When the host last set the volume, its own or the master.
    volume_set_at: Option<Instant>,
    /// It turned a volume read down: it isn't asked again.
    unreadable: bool,
    /// The user drags its own slider, so read-backs are ignored (S271).
    dragging: bool,
    /// The echo of the level last sent and the baseline of its reads (S291).
    read_back: ReadBack,
    /// The device level last sent, which the host's row shows (S290).
    level: f64,
    /// A single speaker's session plays: only then is it read (S291).
    playing: bool,
    /// Set when the host added a stereo pair or home theater group.
    target: Option<TargetId>,
    /// A target's member IDs.
    members: Vec<String>,
    /// Each session's counters by receiver ID: the speaker's own, or each
    /// member's of a target (P144).
    counters: Vec<(String, Arc<SpeakerCounters>)>,
    /// Its own delay on top of the hold-back
    /// ([`asc_airplay_set_speaker_offset`]).
    offset: Duration,
}

impl Added {
    /// Added now, at `position`.
    fn new(
        key: DeviceKey,
        position: f64,
        target: Option<TargetId>,
        members: Vec<String>,
        counters: Vec<(String, Arc<SpeakerCounters>)>,
    ) -> Self {
        Added {
            key,
            position,
            target,
            members,
            counters,
            reading: false,
            volume_set_at: Some(Instant::now()),
            unreadable: false,
            dragging: false,
            read_back: ReadBack::default(),
            level: 0.0,
            playing: false,
            offset: Duration::ZERO,
        }
    }

    /// The sender got `volume`: the first reading near it is its echo
    /// (S291), and its device level is the row's (S290).
    fn sent(&mut self, volume: Volume) {
        self.read_back.sent(volume.wire_db());
        self.level = device_level_for_db(volume.wire_db()).unwrap_or(0.0);
    }

    /// Sends `volume`.
    fn set_volume(&mut self, sender: &Sender, volume: Volume) {
        self.sent(volume);
        match &self.target {
            Some(id) => sender.set_target_volume(id, volume),
            None => sender.set_volume(&self.key, volume),
        }
    }

    /// Keeps `volume`, read from the speaker, without sending it back; the
    /// reading is already the baseline (S291). A target's members still get
    /// it as a set, so they stay at one level (P180).
    fn keep_volume(&mut self, sender: &Sender, volume: Volume) {
        if self.target.is_some() {
            return self.set_volume(sender, volume);
        }
        self.level = device_level_for_db(volume.wire_db()).unwrap_or(0.0);
        sender.keep_volume(&self.key, volume);
    }

    fn set_delay(&self, sender: &Sender, delay: Duration) {
        match &self.target {
            Some(id) => sender.set_target_delay(id, delay),
            None => sender.set_delay(&self.key, delay),
        }
    }

    fn remove(&self, sender: &Sender) {
        match &self.target {
            Some(id) => sender.remove_target(id),
            None => sender.remove_speaker(&self.key),
        }
    }

    fn read_volume(&self, sender: &Sender) {
        match &self.target {
            Some(id) => sender.read_target_volume(id),
            None => sender.read_volume(&self.key),
        }
    }

    /// The host set the volume: an answer to a read asked before is old.
    fn volume_set(&mut self) {
        self.reading = false;
        self.volume_set_at = Some(Instant::now());
    }
}

/// A sender event the host hasn't taken yet.
enum Pending {
    Speaker(String, SpeakerEvent),
    /// The own volume and the device level a level set on the speaker
    /// itself led to (S290).
    Volume(String, f64, f64),
    Warning(String, Warning),
    /// A transport control a speaker's remote asked for, by the row's ID.
    Remote(String, Control),
    Timeline {
        latency: Duration,
        restarted: bool,
    },
    /// An Lf held while a speaker sounds (P201).
    LatencyHeld(u32),
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
    /// The strings of the last receiver handed out: ID, name, model,
    /// target.
    receiver_strings: [CString; 4],
    /// Speaker and timeline events not taken yet, each with the number of
    /// receiver changes queued before it.
    events: VecDeque<(u64, Pending)>,
    /// The strings of the last event handed out: ID, detail.
    event_strings: [CString; 2],
    /// The receivers discovery lists now, by ID, so the host adds speakers
    /// and starts pairings by ID.
    listed: HashMap<String, ReceiverInfo>,
    /// The stereo pairs and home theater groups listed now, by ID; only
    /// with the AirPlay 2 switch on.
    targets: HashMap<String, Target>,
    /// The targets the host was told about.
    targets_reported: HashSet<String>,
    /// The speakers the host added, by ID.
    added: HashMap<String, Added>,
    /// The last state of each member of an added target, by member ID.
    member_states: HashMap<String, SpeakerState>,
    /// The state last reported for each added target, by target ID.
    target_states: HashMap<String, SpeakerState>,
    /// The members of each added target that play nothing now (P181).
    target_missing: HashMap<String, Vec<String>>,
    /// How the AirPlay 1 attempt ended of each added speaker the sender
    /// then moved to AirPlay 2 (P2), by ID: its listing follows the path.
    airplay1_outcome: HashMap<String, ErrorKind>,
    /// The keys of receivers the sender got AirPlay 2 credentials for this
    /// run, by ID: it keeps them after the receiver is gone, so forgetting
    /// must still reach it.
    homekit_keys: HashMap<String, DeviceKey>,
    /// Receivers that ask for a code every time (flags bit 3) and passed a
    /// PIN pairing that stored nothing: listed as able to play until their
    /// session ends or they leave the sender (P182).
    paired_for_session: HashSet<String>,
    /// Receivers whose stored credentials an answer turned down this run:
    /// listed as needing a PIN, with the credentials kept until a new
    /// pairing replaces them (P154).
    credentials_turned_down: HashSet<String>,
    /// The paired receivers' credentials; in memory until the host opens
    /// its file ([`asc_airplay_open_pairings`]).
    store: PairingStore,
    /// The hidden AirPlay 2 switch (P2), as the sender was started with.
    airplay2: bool,
    /// The AirPlay 2 device ID and HomeKit credentials; in memory until the
    /// host opens their file ([`asc_airplay_open_airplay2_identity`]).
    identity: IdentityStore,
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
    /// The zone position, 0 to 100, of each speaker ID in a zone
    /// ([`asc_airplay_set_zone_volume`]); one not here is in no zone.
    zones: HashMap<String, f64>,
    /// Every speaker's delay d_s ([`asc_airplay_set_hold_back`]).
    hold_back: Duration,
    /// What is playing as the sender was last given it, so new progress
    /// goes out with the track and artwork it belongs to.
    now_playing: NowPlaying,
    /// When the sender was given `now_playing`.
    now_playing_at: Instant,
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
    /// The remote control's `_dacp._tcp` record has its advertiser (P188).
    advertising: bool,
    /// What speakers' remotes asked for so far (S273 to S279).
    remote: Remote,
    /// The app the host's now-playing control follows, and what it does
    /// ([`asc_airplay_set_playback`]); `None` with no app to control.
    playback: Option<(String, Playback)>,
    /// The clock the remote decisions count on.
    created_at: Instant,
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

/// The remote control's advertiser for the app, set up with discovery. Its
/// multicast DNS daemon starts with the first session, and never in the
/// core's tests, for the same firewall reason as [`live_browser`].
fn live_advertiser() -> Option<Box<dyn DacpAdvertiser + Send>> {
    #[cfg(not(test))]
    let advertiser = Some(MdnsAdvertiser::new());
    #[cfg(test)]
    let advertiser = None::<MdnsAdvertiser>;
    Some(Box::new(advertiser?))
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

/// Whether the debug hook's value (spec P74, G10) lets the receiver `id`
/// use PCM; the app logs the speakers it names.
fn pcm_debug_allows(id: &str, value: Option<&str>) -> bool {
    audiosync_airplay::sender::pcm_debug_devices(value).contains(id)
}

/// The options for the stereo pair or home theater group added under `id`
/// at `position`; `pcm_list` is the debug hook's value, and the sender
/// gives every member the target's PCM choice.
fn target_options(
    airplay: &AscAirPlay,
    id: &str,
    position: f64,
    pcm_list: Option<&str>,
) -> SpeakerOptions {
    let zone = zone_in(&airplay.zones, id);
    let mut options = SpeakerOptions::new(speaker_volume(position, zone, airplay.master));
    options.delay = airplay.hold_back;
    options.pcm = pcm_debug_allows(id, pcm_list);
    options.trace = debug_trace_on();
    options
}

/// Fills in a target member's own password and credentials.
fn member_credentials(airplay: &AscAirPlay) -> impl FnMut(&DeviceKey, &mut SpeakerOptions) + '_ {
    |key, own| {
        let member = receiver_id(key);
        own.password = airplay.passwords.get(&member).cloned();
        own.credentials = airplay.store.get(&member).cloned();
        own.homekit_credentials = airplay.identity.get(&member).cloned();
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
        UnsupportedReason::Timing => AIRPLAY_REASON_TIMING,
        UnsupportedReason::OneAirPlay2Session => AIRPLAY_REASON_ONE_AIRPLAY2,
        UnsupportedReason::PlaysThroughSpeakers => AIRPLAY_REASON_PLAYS_THROUGH_SPEAKERS,
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
        UserReason::ReceiverUnverified => (AIRPLAY_ERROR_RECEIVER_UNVERIFIED, AIRPLAY_REASON_NONE),
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
        Ok(()) if verdict.needs_pairing && !verdict.legacy_pairing && !paired => {
            (AIRPLAY_NEEDS_PAIRING, AIRPLAY_REASON_NONE)
        }
        Ok(()) => (AIRPLAY_AVAILABLE, AIRPLAY_REASON_NONE),
    }
}

/// Whether a receiver the sender reaches over AirPlay 2 can play, as the
/// Windows app decides: once `paired` (P61) it can; otherwise one whose
/// flags ask for pairing needs a PIN, and one whose records ask for a
/// password needs that.
fn airplay2_availability(info: &ReceiverInfo, paired: bool) -> (i32, i32) {
    let verdict = &info.device.verdict;
    if paired {
        (AIRPLAY_AVAILABLE, AIRPLAY_REASON_NONE)
    } else if verdict.needs_pairing {
        (AIRPLAY_NEEDS_PIN, AIRPLAY_REASON_NONE)
    } else if verdict.needs_password {
        (AIRPLAY_NEEDS_PASSWORD, AIRPLAY_REASON_NONE)
    } else {
        (AIRPLAY_AVAILABLE, AIRPLAY_REASON_NONE)
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

/// The zone position of speaker `id`: 100 outside a zone.
fn zone_in(zones: &HashMap<String, f64>, id: &str) -> f64 {
    zones.get(id).copied().unwrap_or(NO_ZONE_POSITION)
}

/// The volume an AirPlay speaker plays at for its own volume, its zone's
/// and the master's (P118 leaves the mapping to the host; S249, S290): the
/// device level min(100, s × k) on the default line. The Windows app maps
/// its sliders the same way.
fn speaker_volume(position: f64, zone: f64, master: f64) -> Volume {
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

/// A reported level this close to the one sent is the speaker rounding it,
/// not a person's change: the first such read (S291), or a DACP volume
/// within [`VOLUME_SETTLE`] of the set (S271).
const VOLUME_TOLERANCE_DB: f64 = 0.5;

/// How long after the host sets a volume [`VOLUME_TOLERANCE_DB`] applies:
/// a receiver that rounds to 0.3 dB steps can echo the set within 0.5 dB,
/// and a remote press later can land that close too (S271).
const VOLUME_SETTLE: Duration = Duration::from_secs(3);

/// What [`AscAirPlayEvent::warning`] says for a sender warning.
fn warning_code(warning: &Warning) -> i32 {
    match warning {
        Warning::JackDisconnected => AIRPLAY_WARNING_JACK_DISCONNECTED,
        Warning::AuthSetupFailed { .. } => AIRPLAY_WARNING_AUTH_SETUP_FAILED,
        Warning::RequestFailed { .. } => AIRPLAY_WARNING_REQUEST_FAILED,
        Warning::PtpUnavailable => AIRPLAY_WARNING_PTP_PORTS_TAKEN,
        Warning::PtpOutsideFamily => AIRPLAY_WARNING_PTP_OUTSIDE_FAMILY,
        Warning::VolumeReadsStopped => AIRPLAY_WARNING_VOLUME_READS_STOPPED,
        Warning::ArtworkRefused { .. } => AIRPLAY_WARNING_ARTWORK_REFUSED,
        Warning::RemoteControlUnavailable => AIRPLAY_WARNING_REMOTE_CONTROL_UNAVAILABLE,
        Warning::RemoteControlNotAdvertised => AIRPLAY_WARNING_REMOTE_CONTROL_NOT_ADVERTISED,
        Warning::LatencyShortfall { .. } => AIRPLAY_WARNING_LATENCY_SHORTFALL,
    }
}

/// A warning's detail for the debug log.
fn warning_detail(warning: &Warning) -> String {
    match warning {
        Warning::LatencyShortfall {
            reported_us,
            latency_frames,
        } => format!(
            "reports {} ms, more than Lf {} frames ({} ms) leaves room for; it plays at that Lf",
            reported_us / 1000,
            latency_frames,
            u64::from(*latency_frames) * 1000 / 44_100
        ),
        other => format!("{other:?}"),
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
    /// can add up to date. Speaker events the host doesn't use (methods,
    /// server, audio jack) and what a cancelled pairing reports are passed
    /// over.
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
                    // The host lists targets once the AirPlay 2 switch is on.
                    ReceiverEvent::Target(_) if !self.airplay2 => return,
                    ReceiverEvent::Target(TargetChange::Updated(target)) => {
                        self.targets
                            .insert(target.id.to_string(), (**target).clone());
                    }
                    ReceiverEvent::Target(TargetChange::Gone(id)) => {
                        self.targets.remove(&id.to_string());
                    }
                }
                self.queue_receiver(event);
            }
            SenderEvent::Speaker { key, event } => {
                let id = receiver_id(&key);
                if let SpeakerEvent::Error(error) = &event {
                    self.note_path_switch(&id, error);
                    let turned_down = self.speaker_failed(&id, error.reason);
                    if self.paired_for_session.remove(&id) || turned_down {
                        self.relist(&id);
                    }
                }
                if matches!(event, SpeakerEvent::State(SpeakerState::Playing))
                    && self.playing_proves_credentials(&id)
                    && self.credentials_turned_down.remove(&id)
                {
                    self.relist(&id);
                }
                if let SpeakerEvent::HomeKitPaired(credentials) = event {
                    // When the file can't be written they last for this
                    // handle; the sender keeps them as well.
                    let _ = self.identity.insert(&id, credentials);
                    if self.credentials_turned_down.remove(&id) {
                        self.relist(&id);
                    }
                    self.homekit_keys.insert(id, key);
                    return;
                }
                if let SpeakerEvent::State(state) = &event
                    && let Some(added) = self.added.get_mut(&id)
                    && added.target.is_none()
                {
                    added.playing = *state == SpeakerState::Playing;
                }
                // P184 stops reads for one session only: the next one is
                // asked again.
                if let SpeakerEvent::State(state) = &event
                    && *state != SpeakerState::Playing
                    && let Some(owner) = self.reporting_for(&id)
                    && let Some(added) = self.added.get_mut(&owner)
                {
                    added.unreadable = false;
                }
                if let SpeakerEvent::VolumeReported(db) = event {
                    self.volume_reported(&id, db);
                    return;
                }
                if let SpeakerEvent::Warning(warning) = event {
                    self.warned(id, warning);
                    return;
                }
                if matches!(event, SpeakerEvent::Trace(_) | SpeakerEvent::PtpOpened) {
                    self.queue_event(Pending::Speaker(id, event));
                    return;
                }
                self.member_event(&id, &event);
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
            SenderEvent::LatencyHeld { frames } => self.queue_event(Pending::LatencyHeld(frames)),
            SenderEvent::Pairing { key, event } => {
                let id = receiver_id(&key);
                if matches!(event, PairingEvent::HomeKitPaired(_)) {
                    self.homekit_keys.insert(id.clone(), key);
                }
                self.pairing_event(id, event)
            }
            // Discovery's browser died (a bug in it): the sender no
            // longer lists anything new, so it counts as dead too.
            SenderEvent::DiscoveryFailed => self.death.dead.store(true, Ordering::Release),
            SenderEvent::Remote {
                key,
                target,
                command,
            } => self.remote_command(&key, target.as_ref(), command),
            // The command server's lines name their speaker themselves.
            SenderEvent::RemoteLog(line) => {
                if debug_trace_on() {
                    let event = SpeakerEvent::Trace(line);
                    self.queue_event(Pending::Speaker(String::new(), event));
                }
            }
            // The listing turns incomplete; the members' own events say
            // which ones play.
            SenderEvent::TargetMissing { id, missing } => {
                let id = id.to_string();
                let missing = missing.iter().map(receiver_id).collect();
                self.target_missing.insert(id.clone(), missing);
                if let Some(target) = self.targets.get(&id) {
                    let event = TargetChange::Updated(Box::new(target.clone()));
                    self.queue_receiver(ReceiverEvent::Target(event));
                }
            }
        }
    }

    /// A member's state and errors reach the host under the target it
    /// added as well: the target plays while any member plays, and a
    /// member's error is the target's while none does (P181).
    fn member_event(&mut self, member: &str, event: &SpeakerEvent) {
        let Some(target) = self
            .added
            .iter()
            .find(|(_, added)| added.members.iter().any(|id| id == member))
            .map(|(id, _)| id.clone())
        else {
            return;
        };
        match event {
            SpeakerEvent::State(state) => {
                self.member_states.insert(member.to_owned(), *state);
            }
            SpeakerEvent::Error(_) => {
                if self.target_states.get(&target) != Some(&SpeakerState::Playing) {
                    self.queue_event(Pending::Speaker(target, event.clone()));
                }
                return;
            }
            _ => return,
        }
        let rank = |state: &SpeakerState| match state {
            SpeakerState::Playing => (0, None),
            SpeakerState::Connecting => (1, None),
            SpeakerState::Retrying { at } => (2, Some(*at)),
            SpeakerState::Failed => (3, None),
            SpeakerState::Stopped => (4, None),
        };
        let derived = self.added[&target]
            .members
            .iter()
            .filter_map(|id| self.member_states.get(id))
            .min_by_key(|state| rank(state))
            .copied();
        if let Some(state) = derived
            && self.target_states.insert(target.clone(), state) != Some(state)
        {
            self.queue_event(Pending::Speaker(target, SpeakerEvent::State(state)));
        }
    }

    /// The added speaker or target that `id` reports for: itself, or the
    /// target it is a member of.
    fn reporting_for(&self, id: &str) -> Option<String> {
        if self.added.contains_key(id) {
            return Some(id.to_owned());
        }
        self.added
            .iter()
            .find(|(_, added)| added.members.iter().any(|member| member == id))
            .map(|(target, _)| target.clone())
    }

    /// A volume read answered `db` (S291): read after the host last set the
    /// volume and not during a drag, the first reading near a level sent is
    /// its echo and the baseline; any other reading away from the baseline
    /// is a level set on the speaker, which sets the device level and the
    /// own volume (S290), becomes the sender's level for it and goes to the
    /// host, which shows it without sending it back.
    fn volume_reported(&mut self, id: &str, db: f64) {
        let Some(owner) = self.reporting_for(id) else {
            return;
        };
        let master = self.master;
        let zone = zone_in(&self.zones, &owner);
        let Some(added) = self.added.get_mut(&owner) else {
            return;
        };
        let asked = std::mem::take(&mut added.reading);
        if !asked || added.dragging || !added.read_back.set_on_speaker(db) {
            return;
        }
        let Some(RowMove {
            own_position: position,
            device_level: level,
        }) = level_set_on_speaker(db, added.position, added.level, zone, master)
        else {
            return;
        };
        added.position = position;
        // The sender keeps the level too, for a reconnect or a target
        // member that starts late (P119, P180).
        added.keep_volume(&self.sender, device_volume(level));
        self.queue_event(Pending::Volume(owner, position, level));
    }

    /// A speaker's remote asked for `command` (P193): the core's decision
    /// (S273 to S279) acts on the speaker's row, or its target's, and its
    /// line goes to the debug log.
    fn remote_command(
        &mut self,
        key: &DeviceKey,
        target: Option<&TargetId>,
        command: RemoteCommand,
    ) {
        let id = receiver_id(key);
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
        let name = self
            .listed
            .get(&id)
            .map_or_else(|| id.clone(), |info| info.device.name.clone());
        let volume = self.own_volume(row.as_deref(), command);
        let event = RemoteEvent {
            speaker: &name,
            row: row.as_deref(),
            member: target.is_some(),
            command,
            app: self.playback.as_ref().map(|(name, playback)| App {
                name,
                playback: *playback,
            }),
            volume,
            at: self.created_at.elapsed().as_secs_f64(),
        };
        let decision = self.remote.decide(&event);
        if debug_trace_on() {
            let line = SpeakerEvent::Trace(decision.line);
            self.queue_event(Pending::Speaker(id, line));
        }
        match (decision.action, row) {
            (Action::Send(control), Some(row)) => {
                self.queue_event(Pending::Remote(row, control));
            }
            (Action::Move { to, .. }, Some(row)) => {
                let on_speaker = matches!(command, Command::DeviceVolume(_));
                self.move_level(&row, to, on_speaker);
            }
            _ => {}
        }
    }

    /// The volume of the added speaker or target `row` as a remote decision
    /// sees it: its device level (S290); for a DeviceVolume with S271's echo
    /// window and the level it asks for.
    fn own_volume(&self, row: Option<&str>, command: Command) -> OwnVolume {
        let mut own = OwnVolume {
            level: 0.0,
            dragging: false,
            echo: false,
            reported: None,
        };
        let Some((row, added)) = row.and_then(|row| Some((row, self.added.get(row)?))) else {
            return own;
        };
        let zone = zone_in(&self.zones, row);
        // With k at 0 the row shows the level set on the speaker, which
        // s × k can't give (S290).
        let level = device_level(added.position, zone, self.master);
        own.level = if level == 0.0 { added.level } else { level };
        own.dragging = added.dragging;
        if let Command::DeviceVolume(db) = command {
            let recent = added
                .volume_set_at
                .is_some_and(|at| at.elapsed() < VOLUME_SETTLE);
            let sent = speaker_volume(added.position, zone, self.master).wire_db();
            own.echo = recent && (db - sent).abs() < VOLUME_TOLERANCE_DB;
            own.reported = device_level_for_db(db);
        }
        own
    }

    /// A remote moved `row` to device level `to`: as at the end of a drag,
    /// the speaker gets it, which starts S271's window, and the host shows
    /// it without sending it back (S275 to S277). A DeviceVolume is a change
    /// made on the speaker, any other command a row move (S290).
    fn move_level(&mut self, row: &str, to: f64, on_speaker: bool) {
        let master = self.master;
        let zone = zone_in(&self.zones, row);
        let Some(added) = self.added.get_mut(row) else {
            return;
        };
        let moved = if on_speaker {
            level_on_speaker(to, added.position, zone, master)
        } else {
            let Some(moved) = row_move(to, zone, master) else {
                return;
            };
            moved
        };
        added.position = moved.own_position;
        added.volume_set();
        added.set_volume(&self.sender, device_volume(moved.device_level));
        self.queue_event(Pending::Volume(
            row.to_owned(),
            moved.own_position,
            moved.device_level,
        ));
    }

    /// A speaker's warning goes to the host. One that turned a volume read
    /// down, or whose AirPlay 2 reads stopped (P184), isn't asked again, so
    /// its log doesn't fill with the same line.
    fn warned(&mut self, id: String, warning: Warning) {
        let unreadable = matches!(
            warning,
            Warning::RequestFailed {
                method: Method::GetParameter,
                ..
            } | Warning::VolumeReadsStopped
        );
        if unreadable
            && let Some(owner) = self.reporting_for(&id)
            && let Some(added) = self.added.get_mut(&owner)
        {
            added.unreadable = true;
            added.reading = false;
        }
        self.queue_event(Pending::Warning(id, warning));
    }

    /// Asks every playing added speaker that may answer for its volume (S291).
    fn read_volumes(&mut self) {
        for (id, added) in &mut self.added {
            let playing = match added.target {
                Some(_) => self.target_states.get(id) == Some(&SpeakerState::Playing),
                None => added.playing,
            };
            if !playing {
                continue;
            }
            // A speaker that scales the samples itself reports a level of
            // its own, not the one the slider sets (P121).
            let software = self
                .listed
                .get(id)
                .and_then(|info| info.device.raop_txt.as_ref())
                .and_then(|txt| txt.software_volume)
                == Some(true);
            if software || added.unreadable {
                continue;
            }
            added.reading = true;
            added.read_volume(&self.sender);
        }
    }

    /// A speaker's session ended with `reason`: a password it turned down
    /// is forgotten. Stored credentials it turned down stay, since a reset
    /// receiver and an impostor at its address answer alike; it is listed
    /// as needing a PIN, and a new pairing replaces them (P154). Returns
    /// whether its listing changed.
    fn speaker_failed(&mut self, id: &str, reason: UserReason) -> bool {
        match reason {
            UserReason::WrongPassword => {
                self.passwords.remove(id);
                false
            }
            UserReason::PairingRequired
                if self.store.contains(id) || self.identity.get(id).is_some() =>
            {
                self.credentials_turned_down.insert(id.to_owned())
            }
            _ => false,
        }
    }

    /// Takes in what a pairing reported.
    fn pairing_event(&mut self, id: String, event: PairingEvent) {
        if self.ending.contains_key(&id) {
            self.ending_pairing_event(&id, event);
            return;
        }
        let (kind, error, unsupported, detail) = match event {
            // Why the prompt comes again: a debug log line only.
            PairingEvent::PinTurnedDown(why) => {
                if debug_trace_on() {
                    let line = SpeakerEvent::Trace(format!("pairing: {why}"));
                    self.queue_event(Pending::Speaker(id, line));
                }
                return;
            }
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
            PairingEvent::HomeKitPaired(credentials) => {
                self.pairing.remove(&id);
                let detail = self.store_homekit_paired(&id, credentials);
                (AIRPLAY_EVENT_PAIRED, 0, AIRPLAY_REASON_NONE, detail)
            }
            PairingEvent::PairedForSession => {
                self.pairing.remove(&id);
                // Stored keys stay: the flags that made this pairing one
                // for the session came unauthenticated (P61, P154).
                self.paired_for_session.insert(id.clone());
                self.relist(&id);
                (AIRPLAY_EVENT_PAIRED, 0, AIRPLAY_REASON_NONE, String::new())
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
            PairingEvent::PinRequested | PairingEvent::PinTurnedDown(_) => return,
            PairingEvent::Paired(credentials) => Some(self.store_paired(id, credentials)),
            PairingEvent::HomeKitPaired(credentials) => {
                Some(self.store_homekit_paired(id, credentials))
            }
            PairingEvent::Failed { error, .. } => {
                if error.kind.user_reason() == UserReason::WrongPassword {
                    self.passwords.remove(id);
                }
                None
            }
            // Its connection went with the cancel, and nothing was stored.
            PairingEvent::PairedForSession | PairingEvent::Stopped => None,
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
        self.credentials_turned_down.remove(id);
        self.relist(id);
        detail
    }

    /// Stores the HomeKit credentials an AirPlay 2 PIN pairing with the
    /// receiver `id` got (P152), and lists the receiver again. Returns the
    /// detail for the host, as [`Self::store_paired`] does.
    fn store_homekit_paired(&mut self, id: &str, credentials: HomeKitCredentials) -> String {
        let detail = match self.identity.insert(id, credentials) {
            Ok(()) => String::new(),
            Err(error) => format!("the AirPlay 2 identity file couldn't be written: {error}"),
        };
        self.credentials_turned_down.remove(id);
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

    /// Whether a session of `id` that plays proved the receiver holds the
    /// stored credentials, so whatever turned them down is gone (P154):
    /// AirPlay 2 verifies its signature with them, AirPlay 1 only when they
    /// carry its key (P56), and a password the user typed proves nothing.
    fn playing_proves_credentials(&self, id: &str) -> bool {
        if self.passwords.contains_key(id) {
            return false;
        }
        match self.listed.get(id) {
            Some(info) if self.over_airplay2(id, info) => self.identity.get(id).is_some(),
            Some(_) => self
                .store
                .get(id)
                .is_some_and(|credentials| credentials.receiver_key().is_some()),
            None => false,
        }
    }

    /// Whether the sender reaches the receiver `id` over AirPlay 2: only
    /// with the switch on, and then always with HomeKit credentials stored,
    /// otherwise on the path its records and its last AirPlay 1 attempt
    /// choose (P2), as the sender decides.
    fn over_airplay2(&self, id: &str, info: &ReceiverInfo) -> bool {
        self.airplay2
            && info.device.choose_path_paired(
                true,
                self.airplay1_outcome.get(id).copied(),
                self.identity.get(id).is_some(),
            ) == Ok(ProtocolPath::AirPlay2)
    }

    /// Keeps how an AirPlay 1 attempt ended when, with the switch on, the
    /// sender moves the speaker to AirPlay 2 because of it (an Unsupported
    /// or Refused it retries, P2), and lists the receiver again on that path.
    fn note_path_switch(&mut self, id: &str, error: &SpeakerError) {
        let kind = error.error.kind;
        let moved = error.retry_at.is_some()
            && self.airplay2
            && matches!(kind, ErrorKind::Unsupported(_) | ErrorKind::Refused)
            && self.listed.get(id).is_some_and(|info| {
                let device = &info.device;
                device.choose_path(true, None) == Ok(ProtocolPath::AirPlay1)
                    && device.choose_path(true, Some(kind)) == Ok(ProtocolPath::AirPlay2)
            });
        if moved && self.airplay1_outcome.insert(id.to_owned(), kind).is_none() {
            self.relist(id);
        }
    }

    /// Reports a listed receiver again, as changed, after its pairing
    /// changed: its availability follows the store.
    fn relist(&mut self, id: &str) {
        if let Some(info) = self.listed.get(id) {
            let changed = ReceiverEvent::Changed(Box::new(info.clone()));
            self.queue_receiver(changed);
        }
    }

    /// Whether the receiver `id` is paired, on AirPlay 1 or AirPlay 2,
    /// with credentials no answer turned down this run.
    fn paired(&self, id: &str) -> bool {
        (self.store.contains(id) || self.identity.get(id).is_some())
            && !self.credentials_turned_down.contains(id)
    }

    /// Forgets the receiver's AirPlay 2 credentials, here and in the
    /// sender, so its next session pairs again. Returns whether it had any.
    fn forget_homekit(&mut self, id: &str) -> bool {
        if self.identity.get(id).is_none() {
            return false;
        }
        // Forgotten for this handle even when the file can't be written.
        let _ = self.identity.remove(id);
        let key = self.homekit_key(id);
        self.homekit_keys.remove(id);
        if let Some(key) = key {
            self.sender.forget_homekit(&key);
        }
        true
    }

    /// The key the sender knows the receiver `id` by, for dropping its copy
    /// of the receiver's AirPlay 2 credentials.
    fn homekit_key(&self, id: &str) -> Option<DeviceKey> {
        self.listed
            .get(id)
            .map(|info| info.device.key.clone())
            .or_else(|| self.added.get(id).map(|added| added.key.clone()))
            .or_else(|| self.homekit_keys.get(id).cloned())
    }

    /// Keeps the strings of a receiver change and describes it with
    /// pointers to them.
    fn report(&mut self, event: ReceiverEvent) -> AscAirPlayReceiver {
        let mut target = String::new();
        let (code, id, name, model, (availability, unsupported), flags) = match &event {
            ReceiverEvent::Appeared(info) | ReceiverEvent::Changed(info) => {
                let id = receiver_id(&info.device.key);
                let mut paired = self.paired(&id);
                let (availability, mut flags) = if self.over_airplay2(&id, info) {
                    // Stored credentials never verify with a receiver that
                    // asks for a code every time (P61).
                    let every_time = info.device.verdict.code_every_time;
                    paired &= !every_time;
                    let homekit = self.identity.get(&id).is_some()
                        && !every_time
                        && !self.credentials_turned_down.contains(&id);
                    let session = self.paired_for_session.contains(&id);
                    let every_time_flag = if every_time {
                        AIRPLAY_FLAG_CODE_EVERY_TIME
                    } else {
                        0
                    };
                    (
                        airplay2_availability(info, homekit || session),
                        flags(info) | AIRPLAY_FLAG_OVER_AIRPLAY2 | every_time_flag,
                    )
                } else {
                    (availability(info, paired), flags(info))
                };
                if paired {
                    flags |= AIRPLAY_FLAG_PAIRED;
                }
                if self.store.contains(&id) || self.identity.get(&id).is_some() {
                    flags |= AIRPLAY_FLAG_CREDENTIALS_KEPT;
                }
                if self.airplay2
                    && let Some(id) = &info.target
                {
                    target = id.to_string();
                }
                (
                    if matches!(event, ReceiverEvent::Appeared(_)) {
                        AIRPLAY_APPEARED
                    } else {
                        AIRPLAY_CHANGED
                    },
                    id,
                    info.device.name.clone(),
                    model(info),
                    availability,
                    flags,
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
            ReceiverEvent::Target(TargetChange::Updated(found)) => {
                let id = found.id.to_string();
                let mut flags = AIRPLAY_FLAG_TARGET;
                let missing = self
                    .target_missing
                    .get(&id)
                    .is_some_and(|missing| !missing.is_empty());
                if !found.complete() || missing {
                    flags |= AIRPLAY_FLAG_INCOMPLETE;
                }
                let airplay2 = found.members.iter().any(|member| {
                    member
                        .device
                        .as_ref()
                        .is_some_and(|device| device.verdict.airplay2)
                });
                if airplay2 {
                    flags |= AIRPLAY_FLAG_AIRPLAY2;
                }
                let code = if self.targets_reported.insert(id.clone()) {
                    AIRPLAY_APPEARED
                } else {
                    AIRPLAY_CHANGED
                };
                (
                    code,
                    id,
                    found.name.clone(),
                    String::new(),
                    (AIRPLAY_AVAILABLE, AIRPLAY_REASON_NONE),
                    flags,
                )
            }
            ReceiverEvent::Target(TargetChange::Gone(gone)) => {
                let id = gone.to_string();
                self.targets_reported.remove(&id);
                (
                    AIRPLAY_GONE,
                    id,
                    String::new(),
                    String::new(),
                    (AIRPLAY_AVAILABLE, AIRPLAY_REASON_NONE),
                    0,
                )
            }
        };
        self.receiver_strings = [
            c_string(&id),
            c_string(&name),
            c_string(&model),
            c_string(&target),
        ];
        AscAirPlayReceiver {
            event: code,
            id: self.receiver_strings[0].as_ptr(),
            name: self.receiver_strings[1].as_ptr(),
            model: self.receiver_strings[2].as_ptr(),
            availability,
            unsupported,
            flags,
            target: self.receiver_strings[3].as_ptr(),
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
            volume: 0.0,
            warning: 0,
            remote: 0,
            receiver_latency_us: 0,
            receiver_latency_sources: 0,
            device_level: 0.0,
        };
        let (id, detail) = match pending {
            Pending::Remote(id, control) => {
                event.kind = AIRPLAY_EVENT_REMOTE;
                event.remote = match control {
                    Control::PlayPause => AIRPLAY_REMOTE_PLAY_PAUSE,
                    Control::Next => AIRPLAY_REMOTE_NEXT,
                    Control::Previous => AIRPLAY_REMOTE_PREVIOUS,
                };
                (id, String::new())
            }
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
            Pending::Volume(id, position, level) => {
                event.kind = AIRPLAY_EVENT_VOLUME;
                event.volume = position;
                event.device_level = level;
                (id, String::new())
            }
            Pending::Warning(id, warning) => {
                event.kind = AIRPLAY_EVENT_WARNING;
                event.warning = warning_code(&warning);
                (id, warning_detail(&warning))
            }
            Pending::Speaker(id, SpeakerEvent::Latency(latency)) => {
                let SpeakerLatency {
                    arrival_to_sound,
                    receiver_latency_frames,
                    receiver_latency_us,
                    receiver_latency_sources,
                    ..
                } = latency;
                event.kind = AIRPLAY_EVENT_LATENCY;
                event.latency_ms = millis(arrival_to_sound);
                event.receiver_latency_frames = receiver_latency_frames.map_or(-1, i64::from);
                event.receiver_latency_us = i64::try_from(receiver_latency_us).unwrap_or(i64::MAX);
                event.receiver_latency_sources = u32::from(receiver_latency_sources.bits());
                (id, String::new())
            }
            Pending::Speaker(id, SpeakerEvent::Trace(line)) => {
                event.kind = AIRPLAY_EVENT_TRACE;
                (id, line)
            }
            Pending::Speaker(id, SpeakerEvent::PtpOpened) => {
                event.kind = AIRPLAY_EVENT_WARNING;
                event.warning = AIRPLAY_WARNING_PTP_FREE;
                (id, "PtpOpened".to_owned())
            }
            // Never queued (see `pump`).
            Pending::Speaker(id, _) => (id, String::new()),
            Pending::Timeline { latency, restarted } => {
                event.kind = AIRPLAY_EVENT_TIMELINE;
                event.latency_ms = millis(latency);
                event.restarted = restarted;
                (String::new(), String::new())
            }
            Pending::LatencyHeld(frames) => {
                event.kind = AIRPLAY_EVENT_LATENCY_HELD;
                event.latency_ms = f64::from(frames) * 1000.0 / 44_100.0;
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
                // A capture build: its first block follows the silence the
                // stream sent meanwhile, on the same timeline (P143).
                self.writer.new_capture();
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
/// Null when the thread can't start or creating the sender failed.
#[unsafe(no_mangle)]
pub extern "C" fn asc_airplay_create() -> *mut AscAirPlay {
    create_with(live_browser)
}

/// [`asc_airplay_create`] with the app's AirPlay 2 setting: AirPlay 2 is on
/// when `airplay2` is or when the environment variable turns it on (P2);
/// off, the sender makes no AirPlay 2 traffic (P170).
#[unsafe(no_mangle)]
pub extern "C" fn asc_airplay_create_with_airplay2(airplay2: bool) -> *mut AscAirPlay {
    create_configured(live_browser, airplay2)
}

/// [`asc_airplay_create`] with the browser [`asc_airplay_start_discovery`]
/// opens: live multicast DNS for the app, one a test supplies otherwise.
pub(crate) fn create_with(open_browser: OpenBrowser) -> *mut AscAirPlay {
    create_configured(open_browser, false)
}

/// The sender's configuration for the app's AirPlay 2 setting and the
/// environment variable's value; either one turns AirPlay 2 on.
fn sender_config(setting: bool, environment: Option<&str>) -> SenderConfig {
    SenderConfig {
        airplay2: setting || airplay2_switch(environment),
        ..SenderConfig::default()
    }
}

fn create_configured(open_browser: OpenBrowser, airplay2_setting: bool) -> *mut AscAirPlay {
    note_panic_locations();
    let panics_before = PANICS.load(Ordering::SeqCst);
    let created = panic::catch_unwind(|| {
        injected_panic();
        // The environment variable still works without the app's setting,
        // for CI and debug runs.
        let config = sender_config(
            airplay2_setting,
            std::env::var(AIRPLAY2_SWITCH_ENV).ok().as_deref(),
        );
        let airplay2 = config.airplay2;
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
                targets: HashMap::new(),
                targets_reported: HashSet::new(),
                airplay1_outcome: HashMap::new(),
                homekit_keys: HashMap::new(),
                paired_for_session: HashSet::new(),
                credentials_turned_down: HashSet::new(),
                member_states: HashMap::new(),
                target_states: HashMap::new(),
                target_missing: HashMap::new(),
                added: HashMap::new(),
                store: PairingStore::in_memory(),
                airplay2,
                identity: IdentityStore::in_memory(),
                passwords: HashMap::new(),
                pairing: HashMap::new(),
                ending: HashMap::new(),
                master: 100.0,
                zones: HashMap::new(),
                hold_back: Duration::ZERO,
                now_playing: NowPlaying::default(),
                now_playing_at: Instant::now(),
                death: Arc::new(Death::new()),
                panics_before,
                failure_reported: false,
                open_browser,
                advertising: false,
                remote: Remote::default(),
                playback: None,
                created_at: Instant::now(),
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
/// the handle is null or dead or multicast DNS can't start.
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
                if !airplay.advertising
                    && let Some(advertiser) = live_advertiser()
                {
                    airplay.sender.set_dacp_advertiser(advertiser);
                    airplay.advertising = true;
                }
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
            if let Some(target) = airplay.targets.get(id) {
                let position = sanitize_own(volume);
                let options = target_options(
                    airplay,
                    id,
                    position,
                    std::env::var(audiosync_airplay::sender::AIRPLAY_PCM_ENV)
                        .ok()
                        .as_deref(),
                );
                let target_id = target.id.clone();
                let members = target
                    .members
                    .iter()
                    .map(|member| receiver_id(&member.key))
                    .collect();
                let counters =
                    airplay
                        .sender
                        .add_target(target.clone(), options, member_credentials(airplay));
                let key = DeviceKey::Instance(ServiceKind::AirPlay, id.to_owned());
                let initial = speaker_volume(position, zone_in(&airplay.zones, id), airplay.master);
                let mut added = Added::new(
                    key,
                    position,
                    Some(target_id),
                    members,
                    counters
                        .into_iter()
                        .map(|(key, counters)| (receiver_id(&key), counters))
                        .collect(),
                );
                added.sent(initial);
                airplay.added.insert(id.to_owned(), added);
                airplay.remote.forget(id);
                return true;
            }
            let Some(info) = airplay.listed.get(id) else {
                return false;
            };
            let device = info.device.clone();
            let position = sanitize_own(volume);
            let key = device.key.clone();
            let zone = zone_in(&airplay.zones, id);
            let initial = speaker_volume(position, zone, airplay.master);
            let mut options = SpeakerOptions::new(initial);
            options.delay = airplay.hold_back;
            options.password = airplay.passwords.get(id).cloned();
            options.credentials = airplay.store.get(id).cloned();
            options.homekit_credentials = airplay.identity.get(id).cloned();
            options.trace = debug_trace_on();
            options.pcm = pcm_debug_allows(
                id,
                std::env::var(audiosync_airplay::sender::AIRPLAY_PCM_ENV)
                    .ok()
                    .as_deref(),
            );
            let counters = airplay.sender.add_speaker(device, options);
            let mut added = Added::new(
                key,
                position,
                None,
                Vec::new(),
                vec![(id.to_owned(), counters)],
            );
            added.sent(initial);
            airplay.added.insert(id.to_owned(), added);
            airplay.remote.forget(id);
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
            added.remove(&airplay.sender);
            if airplay.paired_for_session.remove(id) {
                airplay.relist(id);
            }
            airplay.target_states.remove(id);
            airplay.target_missing.remove(id);
            for member in &added.members {
                airplay.member_states.remove(member);
            }
            // Added again, it starts over on AirPlay 1 first, as the sender does.
            for id in std::iter::once(id).chain(added.members.iter().map(String::as_str)) {
                if airplay.airplay1_outcome.remove(id).is_some() {
                    airplay.relist(id);
                }
            }
            true
        })
    }
}

/// Sets the own volume of the speaker added under `id`, 0 up, unrounded and
/// above 100 when S290 stored it so; it plays at min(100, own × k), live
/// (P118). False, changing
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
            let zone = zone_in(&airplay.zones, id);
            let Some(added) = airplay.added.get_mut(id) else {
                return false;
            };
            added.position = sanitize_own(volume);
            added.volume_set();
            added.set_volume(
                &airplay.sender,
                speaker_volume(added.position, zone, master),
            );
            airplay.remote.forget(id);
            true
        })
    }
}

/// An AirPlay row move ([`crate::ffi::asc_airplay_row_move`]'s results,
/// S290): stores own volume `own_position` for the speaker added under `id`
/// and sends it device level `device_level`, live. False, changing nothing,
/// when the handle or `id` is null, no speaker was added under `id`, or a
/// value isn't finite.
///
/// # Safety
///
/// `airplay` is null or a live handle, called under the threading
/// contract; `id` is null or a NUL-terminated string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn asc_airplay_set_device_level(
    airplay: *mut AscAirPlay,
    id: *const c_char,
    own_position: f64,
    device_level: f64,
) -> bool {
    // SAFETY: by the caller's contract null or a NUL-terminated string.
    let Some(id) = (unsafe { host_text(id) }) else {
        return false;
    };
    if !own_position.is_finite() || !device_level.is_finite() {
        return false;
    }
    // SAFETY: by the caller's contract null or a live handle that no other
    // call uses now.
    unsafe {
        live(airplay, false, |airplay| {
            let Some(added) = airplay.added.get_mut(id) else {
                return false;
            };
            added.position = sanitize_own(own_position);
            added.volume_set();
            added.set_volume(
                &airplay.sender,
                device_volume(device_level.clamp(0.0, 100.0)),
            );
            airplay.remote.forget(id);
            true
        })
    }
}

/// Tells whether the user is dragging the volume slider of the speaker
/// added under `id`: while it is, a level read back from the speaker is
/// ignored (S271). False when the handle or `id` is null or no speaker was
/// added under `id`.
///
/// # Safety
///
/// `airplay` is null or a live handle, called under the threading
/// contract; `id` is null or a NUL-terminated string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn asc_airplay_set_volume_dragging(
    airplay: *mut AscAirPlay,
    id: *const c_char,
    dragging: bool,
) -> bool {
    // SAFETY: by the caller's contract null or a NUL-terminated string.
    let Some(id) = (unsafe { host_text(id) }) else {
        return false;
    };
    // SAFETY: by the caller's contract null or a live handle that no other
    // call uses now.
    unsafe {
        live(airplay, false, |airplay| {
            let Some(added) = airplay.added.get_mut(id) else {
                return false;
            };
            added.dragging = dragging;
            // A read asked before this answers with a level from the drag.
            added.reading = false;
            true
        })
    }
}

/// Sets the zone position (0 to 100) of speaker `id`, 100 for no zone
/// (S249): it plays at its own position times the zone's times the master,
/// live once added, and from its start when added later. False, changing
/// nothing, when the handle or `id` is null or `zone` isn't finite.
///
/// # Safety
///
/// `airplay` is null or a live handle, called under the threading
/// contract; `id` is null or a NUL-terminated string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn asc_airplay_set_zone_volume(
    airplay: *mut AscAirPlay,
    id: *const c_char,
    zone: f64,
) -> bool {
    // SAFETY: by the caller's contract null or a NUL-terminated string.
    let Some(id) = (unsafe { host_text(id) }) else {
        return false;
    };
    if !zone.is_finite() {
        return false;
    }
    // SAFETY: by the caller's contract null or a live handle that no other
    // call uses now.
    unsafe {
        live(airplay, false, |airplay| {
            let zone = zone.clamp(0.0, 100.0);
            if zone == zone_in(&airplay.zones, id) {
                return true;
            }
            if zone == NO_ZONE_POSITION {
                airplay.zones.remove(id);
            } else {
                airplay.zones.insert(id.to_owned(), zone);
            }
            let master = airplay.master;
            if let Some(added) = airplay.added.get_mut(id) {
                added.volume_set();
                added.set_volume(
                    &airplay.sender,
                    speaker_volume(added.position, zone, master),
                );
            }
            true
        })
    }
}

/// Asks every playing speaker added for its own volume (P122, S291), which
/// a person may have changed on the speaker itself; the host calls it once
/// a second. A stereo pair or home theater group asks its leader (P180). On
/// the AirPlay 2 path the read goes out at the session's next 1 s read tick
/// (P184). A level that moves a row comes as [`AIRPLAY_EVENT_VOLUME`],
/// already the speaker's level in the sender, so a reconnect or a member
/// that starts late plays at it and the host doesn't set it again.
/// Dropped: a level read before the host last set the volume, one that
/// comes while the row is dragged, the first one within 0.5 dB of a level
/// sent (its echo), and one within 0.05 dB of the last reading kept.
/// Stopped speakers, speakers that leave volume to the sender (`sv`, P121),
/// and ones that turned a read down or whose reads stopped aren't asked.
///
/// # Safety
///
/// `airplay` is null or a live handle, called under the threading contract.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn asc_airplay_read_volumes(airplay: *mut AscAirPlay) {
    // SAFETY: by the caller's contract null or a live handle that no other
    // call uses now.
    unsafe { live(airplay, (), AscAirPlay::read_volumes) }
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
            for (id, added) in airplay.added.iter_mut() {
                added.volume_set();
                added.set_volume(
                    &airplay.sender,
                    speaker_volume(added.position, zone_in(&airplay.zones, id), airplay.master),
                );
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
                    added.set_delay(&airplay.sender, (hold_back + added.offset).min(MAX_DELAY));
                }
            }
            millis(hold_back)
        })
    }
}

/// Gives the speaker added under `id` a delay of its own on top of the
/// hold-back, `offset_ms` clamped to 0 to [`AIRPLAY_MAX_DELAY_MS`], the sum
/// capped there too: the host's E − e_s, so a receiver that reports less
/// latency than the largest waits for it ([engine-spec] S283). A value not
/// finite counts as 0. Returns the offset applied, in ms; -1 when the
/// handle or `id` is null or no speaker was added under `id`.
///
/// # Safety
///
/// `airplay` is null or a live handle, called under the threading
/// contract; `id` is null or a NUL-terminated string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn asc_airplay_set_speaker_offset(
    airplay: *mut AscAirPlay,
    id: *const c_char,
    offset_ms: f64,
) -> f64 {
    // SAFETY: by the caller's contract null or a NUL-terminated string.
    let Some(id) = (unsafe { host_text(id) }) else {
        return -1.0;
    };
    let offset = if offset_ms.is_finite() {
        Duration::from_secs_f64(offset_ms.clamp(0.0, millis(MAX_DELAY)) / 1000.0)
    } else {
        Duration::ZERO
    };
    // SAFETY: by the caller's contract null or a live handle that no other
    // call uses now.
    unsafe {
        live(airplay, -1.0, |airplay| {
            let hold_back = airplay.hold_back;
            let Some(added) = airplay.added.get_mut(id) else {
                return -1.0;
            };
            if offset != added.offset {
                added.offset = offset;
                added.set_delay(&airplay.sender, (hold_back + offset).min(MAX_DELAY));
            }
            millis(offset)
        })
    }
}

/// Sets the stream's base latency Lf, clamped to 11 025 to 88 200 frames
/// (P98, P202). While no speaker sounds it applies at once and a timeline
/// event reports the new L; later it is held ([`AIRPLAY_EVENT_LATENCY_HELD`])
/// until the stream has no speaker or the timeline restarts (P201). A null
/// or dead handle does nothing.
///
/// # Safety
///
/// `airplay` is null or a live handle, called under the threading contract.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn asc_airplay_set_latency(airplay: *mut AscAirPlay, frames: u32) {
    let frames = frames.clamp(MIN_LATENCY_FRAMES, MAX_LATENCY_FRAMES);
    // SAFETY: by the caller's contract null or a live handle that no other
    // call uses now.
    unsafe { live(airplay, (), |airplay| airplay.sender.set_latency(frames)) }
}

/// True once a speaker of the stream reached its first-sound time, so an Lf
/// set now is held (P201); false for a null or dead handle.
///
/// # Safety
///
/// `airplay` is null or a live handle, called under the threading contract.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn asc_airplay_sounding(airplay: *const AscAirPlay) -> bool {
    // SAFETY: by the caller's contract null or a live handle that no other
    // call uses now; the call only reads it.
    unsafe {
        live(airplay.cast_mut(), false, |airplay| {
            airplay.sender.sounding()
        })
    }
}

/// The stream's Lf in frames from every speaker's reported latency R_s
/// (P200, P202): `reported_us` holds `count` values in microseconds, a
/// negative one counting as 0. A null array or a count of 0 gives 77 175.
///
/// # Safety
///
/// `reported_us` is null or points to `count` readable values.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn asc_airplay_session_latency_frames(
    reported_us: *const i64,
    count: usize,
) -> u32 {
    if reported_us.is_null() || count == 0 {
        return DEFAULT_LATENCY_FRAMES;
    }
    // SAFETY: by the caller's contract `count` readable values.
    let values = unsafe { std::slice::from_raw_parts(reported_us, count) };
    session_latency_frames(values.iter().map(|&us| u64::try_from(us).unwrap_or(0)))
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

/// The user paused playback on purpose (P213): a remote's Pause, a media
/// key or the app's pause button, never silence. Every speaker gets a FLUSH
/// so its buffer drops at once and no audio goes out until
/// [`asc_airplay_resume`]; the sessions stay up.
///
/// # Safety
///
/// `airplay` is null or a live handle, called under the threading contract.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn asc_airplay_pause(airplay: *mut AscAirPlay) {
    // SAFETY: by the caller's contract null or a live handle that no other
    // call uses now.
    unsafe { live(airplay, (), |airplay| airplay.sender.pause()) }
}

/// The user resumed after [`asc_airplay_pause`] (P213): every speaker goes
/// on from its flush point, in step, with the audio written from now on.
///
/// # Safety
///
/// `airplay` is null or a live handle, called under the threading contract.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn asc_airplay_resume(airplay: *mut AscAirPlay) {
    // SAFETY: by the caller's contract null or a live handle that no other
    // call uses now.
    unsafe { live(airplay, (), |airplay| airplay.sender.resume()) }
}

/// A speaker's counters (P144), as [`asc_airplay_speaker_counters`] copies
/// them: one session's, or a stereo pair's or home theater group's summed
/// over its members. They keep counting across the speaker's reconnects.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AscAirPlayCounters {
    pub packets_sent: u64,
    pub resend_requests: u64,
    pub packets_resent: u64,
    /// Requested packets not resent: no longer held, or resent three times.
    pub not_held: u64,
    pub timing_replies: u64,
    /// Packets sent more than 5 ms after their time.
    pub late_packets: u64,
    pub silent_frames: u64,
    /// The stream's count, since every session plays the one input.
    pub input_frames_dropped: u64,
    /// Datagrams dropped on the control and timing sockets because they
    /// came from another address (P86, P91).
    pub foreign_datagrams: u64,
    /// How many sessions' counters were summed: 1 for a speaker, the member
    /// count for a target.
    pub sessions: u32,
}

/// The stream's counters (P144), as [`asc_airplay_stream_counters`] copies
/// them.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct AscAirPlayStreamCounters {
    /// The current input rate estimate R̂ (P101).
    pub rate_hz: f64,
    /// Packets cut and encoded, one per packet of the timeline.
    pub packets_encoded: u64,
    /// Silent frames inserted where the input hadn't arrived in time.
    pub silent_frames: u64,
    pub input_frames_dropped: u64,
}

impl AscAirPlayCounters {
    fn add(&mut self, counters: &SpeakerCounters) {
        self.packets_sent += counters.packets_sent();
        self.resend_requests += counters.resend_requests();
        self.packets_resent += counters.packets_resent();
        self.not_held += counters.not_held();
        self.timing_replies += counters.timing_replies();
        self.late_packets += counters.late_packets();
        self.silent_frames += counters.silent_frames();
        self.input_frames_dropped = counters.input_frames_dropped();
        self.foreign_datagrams += counters.foreign_datagrams();
        self.sessions += 1;
    }
}

impl AscAirPlay {
    /// Gives the sender what is playing, as of `now`, and keeps it.
    fn give_now_playing(&mut self, now_playing: NowPlaying, now: Instant) -> bool {
        if self.sender.set_now_playing(now_playing.clone()).is_err() {
            return false;
        }
        self.now_playing = now_playing;
        self.now_playing_at = now;
        true
    }

    /// The counters of the speaker added under `id`, or of `id` alone when
    /// it is a member of an added target; `None` when it is neither.
    fn counters(&self, id: &str) -> Option<AscAirPlayCounters> {
        let mut total = AscAirPlayCounters::default();
        if let Some(added) = self.added.get(id) {
            for (_, counters) in &added.counters {
                total.add(counters);
            }
            return Some(total);
        }
        let (_, member) = self
            .added
            .values()
            .flat_map(|added| &added.counters)
            .find(|(member, _)| member == id)?;
        total.add(member);
        Some(total)
    }
}

/// Copies into `out` the counters of the speaker added under `id`: a stereo
/// pair's or home theater group's summed over its members, or one member's
/// when `id` names a member of an added target. Reads atomics only, never
/// blocking the sender. False, with `out` zeroed, when the handle or `id` is
/// null, the handle is dead, or nothing was added under `id`; false and
/// nothing written when `out` is null.
///
/// # Safety
///
/// `airplay` is null or a live handle, called under the threading
/// contract; `id` is null or a NUL-terminated string; `out` is null or
/// points to a writable `AscAirPlayCounters`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn asc_airplay_speaker_counters(
    airplay: *const AscAirPlay,
    id: *const c_char,
    out: *mut AscAirPlayCounters,
) -> bool {
    if out.is_null() {
        return false;
    }
    // SAFETY: by the caller's contract null or a NUL-terminated string.
    let found = match unsafe { host_text(id) } {
        // SAFETY: by the caller's contract null or a live handle that no
        // other call uses now; the call only reads it.
        Some(id) => unsafe { live(airplay.cast_mut(), None, |airplay| airplay.counters(id)) },
        None => None,
    };
    // SAFETY: non-null, and by the caller's contract writable.
    unsafe { out.write(found.unwrap_or_default()) };
    found.is_some()
}

/// Copies the stream's counters into `out`. False, with `out` zeroed, for a
/// null or dead handle; false and nothing written when `out` is null.
///
/// # Safety
///
/// `airplay` is null or a live handle, called under the threading
/// contract; `out` is null or points to a writable
/// `AscAirPlayStreamCounters`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn asc_airplay_stream_counters(
    airplay: *const AscAirPlay,
    out: *mut AscAirPlayStreamCounters,
) -> bool {
    if out.is_null() {
        return false;
    }
    // SAFETY: by the caller's contract null or a live handle that no other
    // call uses now; the call only reads it.
    let found = unsafe {
        live(airplay.cast_mut(), None, |airplay| {
            let stream = airplay.sender.stream_counters();
            Some(AscAirPlayStreamCounters {
                rate_hz: stream.rate(),
                packets_encoded: stream.packets_encoded(),
                silent_frames: stream.silent_frames(),
                input_frames_dropped: stream.input_frames_dropped(),
            })
        })
    };
    // SAFETY: non-null, and by the caller's contract writable.
    unsafe { out.write(found.unwrap_or_default()) };
    found.is_some()
}

/// Sets what is playing, for the receivers that show it (P123 to P127): the
/// track's title, artist and album as UTF-8 text, and its artwork as two
/// JPEGs of at most 512 KiB each (P125): `large` for AirPlay 2 sessions and
/// `small` for AirPlay 1 sessions and after a refusal (P185). Every speaker
/// that lists the type gets it now, and every speaker that starts or
/// restarts later gets it too. A null text is an empty one, and three null
/// texts send no track information; one image alone serves both paths, and
/// none (null or a length of 0) sends no artwork. Returns false, changing
/// nothing, for a null or dead handle, a text that isn't UTF-8, a null image
/// with a length, or an image the sender refuses (not a JPEG, or too large).
///
/// # Safety
///
/// `airplay` is null or a live handle, called under the threading
/// contract; each text is null or a NUL-terminated string; `large` is null
/// or points to `large_len` bytes, and `small` to `small_len`.
#[unsafe(no_mangle)]
#[allow(clippy::too_many_arguments)]
pub unsafe extern "C" fn asc_airplay_set_now_playing(
    airplay: *mut AscAirPlay,
    title: *const c_char,
    artist: *const c_char,
    album: *const c_char,
    large: *const u8,
    large_len: usize,
    small: *const u8,
    small_len: usize,
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
    // SAFETY: by the caller's contract null or `large_len` and `small_len`
    // bytes.
    let (Some(large), Some(small)) = (unsafe { host_slice(large, large_len) }, unsafe {
        host_slice(small, small_len)
    }) else {
        return false;
    };
    let [title, artist, album] = texts;
    let track = (title.is_some() || artist.is_some() || album.is_some()).then(|| TrackInfo {
        title: title.unwrap_or_default().to_owned(),
        artist: artist.unwrap_or_default().to_owned(),
        album: album.unwrap_or_default().to_owned(),
    });
    let artwork = match (large.is_empty(), small.is_empty()) {
        (true, true) => None,
        (false, true) => Some(Artwork::single(large.to_vec())),
        (true, false) => Some(Artwork::single(small.to_vec())),
        (false, false) => Some(Artwork {
            large: large.to_vec(),
            small: small.to_vec(),
        }),
    };
    // SAFETY: by the caller's contract null or a live handle that no other
    // call uses now.
    unsafe {
        live(airplay, false, |airplay| {
            let now = Instant::now();
            let progress = if track == airplay.now_playing.track {
                let since = now.saturating_duration_since(airplay.now_playing_at);
                airplay
                    .now_playing
                    .progress
                    .map(|progress| moved_on(progress, since))
            } else {
                None
            };
            let now_playing = NowPlaying {
                track,
                artwork,
                progress,
            };
            airplay.give_now_playing(now_playing, now)
        })
    }
}

/// Sets how far into the track the audio arriving now is, and how long the
/// track is, in ms, for the receivers that show progress (P126). It goes out
/// with the track and artwork set last ([`asc_airplay_set_now_playing`]),
/// now and to every speaker that starts or restarts later. A length that
/// isn't positive and finite sends none; so does another track, until this
/// is called again, while the same track set again keeps it. An elapsed
/// time outside 0 to the length is taken as its nearest end. Returns false,
/// changing nothing, for a null or dead handle.
///
/// # Safety
///
/// `airplay` is null or a live handle, called under the threading contract.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn asc_airplay_set_progress(
    airplay: *mut AscAirPlay,
    elapsed_ms: f64,
    length_ms: f64,
) -> bool {
    let progress = host_progress(elapsed_ms, length_ms);
    // SAFETY: by the caller's contract null or a live handle that no other
    // call uses now.
    unsafe {
        live(airplay, false, |airplay| {
            let now_playing = NowPlaying {
                progress,
                ..airplay.now_playing.clone()
            };
            airplay.give_now_playing(now_playing, Instant::now())
        })
    }
}

/// Tells which app the host's now-playing control follows, by a name for
/// the debug log (UTF-8), and whether it plays (`AIRPLAY_PLAYBACK_*`), so a
/// speaker remote's Play or Pause goes as play-pause only when that changes
/// something (S274). A null or empty `app` is no app to control: transport
/// commands then do nothing. An unknown `playback` counts as no reading.
/// False, changing nothing, for a null or dead handle or an `app` that
/// isn't UTF-8.
///
/// # Safety
///
/// `airplay` is null or a live handle, called under the threading
/// contract; `app` is null or a NUL-terminated string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn asc_airplay_set_playback(
    airplay: *mut AscAirPlay,
    app: *const c_char,
    playback: i32,
) -> bool {
    let app = if app.is_null() {
        None
    } else {
        // SAFETY: by the caller's contract a NUL-terminated string.
        match unsafe { host_text(app) } {
            Some(app) => Some(app.to_owned()),
            None => return false,
        }
    };
    let playback = match playback {
        AIRPLAY_PLAYBACK_PLAYING => Playback::Playing,
        AIRPLAY_PLAYBACK_PAUSED => Playback::Paused,
        _ => Playback::Unknown,
    };
    // SAFETY: by the caller's contract null or a live handle that no other
    // call uses now.
    unsafe {
        live(airplay, false, |airplay| {
            airplay.playback = app.filter(|app| !app.is_empty()).map(|app| (app, playback));
            true
        })
    }
}

/// Progress from the host's milliseconds (P126); none without a length.
fn host_progress(elapsed_ms: f64, length_ms: f64) -> Option<Progress> {
    if !(length_ms.is_finite() && length_ms > 0.0) {
        return None;
    }
    let length = Duration::try_from_secs_f64(length_ms / 1000.0).ok()?;
    let elapsed = if elapsed_ms.is_finite() {
        Duration::from_secs_f64(elapsed_ms.clamp(0.0, length_ms) / 1000.0).min(length)
    } else {
        Duration::ZERO
    };
    Some(Progress { elapsed, length })
}

/// `progress` as it stands `since` later: the receiver's bar runs on with
/// the stream whether the player plays or not, so a resend runs on with it,
/// up to the track's end.
fn moved_on(progress: Progress, since: Duration) -> Progress {
    Progress {
        elapsed: progress.elapsed.saturating_add(since).min(progress.length),
        length: progress.length,
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

/// Opens the AirPlay 2 identity kept in the file at `path` (see
/// [`crate::airplay2_identity`]): the device ID the AirPlay 2 path sends
/// (P5, P43), made and written once when the file is missing, and the
/// HomeKit credentials of the receivers it paired with (P152), which later
/// sessions verify with; credentials a pairing gives from then on are
/// written to it. Meant for once, before discovery starts. With the hidden
/// AirPlay 2 switch off it reads and writes nothing and returns 0. Returns
/// how many receivers it holds credentials for, or
/// [`AIRPLAY_PAIRINGS_IGNORED`] or [`AIRPLAY_PAIRINGS_NEWER`] for a file
/// it ignored, or [`AIRPLAY_PAIRINGS_FAILED`] for a null or dead handle or
/// a null, empty or non-UTF-8 `path`, leaving the identity as it was.
///
/// # Safety
///
/// `airplay` is null or a live handle, called under the threading
/// contract; `path` is null or a NUL-terminated string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn asc_airplay_open_airplay2_identity(
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
            if !airplay.airplay2 {
                return 0;
            }
            airplay.pump();
            let (identity, loaded) = IdentityStore::open(PathBuf::from(path));
            airplay.sender.set_airplay2_device_id(identity.device_id());
            airplay.identity = identity;
            match loaded {
                Loaded::NoFile => 0,
                Loaded::Receivers(count) => i32::try_from(count).unwrap_or(i32::MAX),
                Loaded::Ignored(_) => AIRPLAY_PAIRINGS_IGNORED,
                Loaded::Newer(_) => AIRPLAY_PAIRINGS_NEWER,
            }
        })
    }
}

/// The `AIRPLAY_FILE_*` code for how private a file was found.
fn access_code(access: Access) -> i32 {
    match access {
        Access::Private => AIRPLAY_FILE_PRIVATE,
        Access::Tightened => AIRPLAY_FILE_TIGHTENED,
        Access::StillOpen => AIRPLAY_FILE_STILL_OPEN,
        Access::Unread => AIRPLAY_FILE_UNREAD,
    }
}

/// How private [`asc_airplay_open_pairings`] found the pairings file (S224),
/// for the host's log: [`AIRPLAY_FILE_PRIVATE`] also when there was no
/// file, or none was opened; [`AIRPLAY_FILE_UNREAD`] for a file it didn't
/// read, or a null or dead handle.
///
/// # Safety
///
/// `airplay` is null or a live handle, called under the threading contract.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn asc_airplay_pairings_access(airplay: *const AscAirPlay) -> i32 {
    // SAFETY: by the caller's contract null or a live handle that no other
    // call uses now; the call only reads it.
    unsafe {
        live(airplay.cast_mut(), AIRPLAY_FILE_UNREAD, |airplay| {
            access_code(airplay.store.access())
        })
    }
}

/// How private [`asc_airplay_open_airplay2_identity`] found the AirPlay 2
/// identity file (S224), for the host's log: [`AIRPLAY_FILE_PRIVATE`] also
/// when there was no file, or none was opened; [`AIRPLAY_FILE_UNREAD`] for a
/// file it didn't read, with the hidden AirPlay 2 switch off, or for a null
/// or dead handle.
///
/// # Safety
///
/// `airplay` is null or a live handle, called under the threading contract.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn asc_airplay_airplay2_identity_access(airplay: *const AscAirPlay) -> i32 {
    // SAFETY: by the caller's contract null or a live handle that no other
    // call uses now; the call only reads it.
    unsafe {
        live(airplay.cast_mut(), AIRPLAY_FILE_UNREAD, |airplay| {
            if airplay.airplay2 {
                access_code(airplay.identity.access())
            } else {
                AIRPLAY_FILE_UNREAD
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

/// Forgets the stored credentials of the receiver `id`, AirPlay 1 and
/// AirPlay 2, and writes their files (the pairings file is removed once
/// nothing is paired); the receiver is reported again as needing a PIN when
/// listed. A session running with them goes on
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
            let homekit = airplay.forget_homekit(id);
            airplay.credentials_turned_down.remove(id);
            if !airplay.store.contains(id) && !homekit {
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
    use crate::volume::row_level;
    use std::ffi::CStr;
    use std::net::{IpAddr, Ipv4Addr};
    use std::time::{Duration, Instant};

    use std::io::{self, Read, Write};
    use std::net::{TcpListener, TcpStream, UdpSocket};
    use std::sync::atomic::AtomicBool;
    use std::sync::{Arc, Mutex};
    use std::thread::JoinHandle;

    use audiosync_airplay::discovery::{BrowseEvent, Browser, DeviceId, ScopedAddr, ServiceRecord};
    use audiosync_airplay::homekit::HomeKitCredentials;
    use audiosync_airplay::latency::LatencySources;
    use audiosync_airplay::rtsp::{Message, Method, Parser, Response};
    use audiosync_airplay::volume::parse_reported;

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
            target: ptr::null(),
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
    fn the_pcm_debug_hook_allows_the_named_receivers_only() {
        assert!(pcm_debug_allows("kitchen", Some("den, kitchen")));
        assert!(pcm_debug_allows("den", Some("den, kitchen")));
        assert!(!pcm_debug_allows("study", Some("den, kitchen")));
        assert!(!pcm_debug_allows("kitchen", Some("")));
        assert!(!pcm_debug_allows("kitchen", None));
        assert!(pcm_debug_allows(
            "A1B2C3D4E5F6",
            Some("raop:A1B2C3D4E5F6@Kitchen, a1:b2:c3:d4:e5:f6")
        ));
        assert!(!pcm_debug_allows("A1B2C3D4E5F7", Some("a1:b2:c3:d4:e5:f6")));
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
    fn airplay2_stays_off_without_the_setting_or_the_environment_variable() {
        assert!(!sender_config(false, None).airplay2);
        assert!(!sender_config(false, Some("0")).airplay2);
    }

    #[test]
    fn the_setting_or_the_environment_variable_turns_airplay2_on() {
        assert!(sender_config(true, None).airplay2);
        assert!(sender_config(false, Some("1")).airplay2);
        assert!(sender_config(true, Some("1")).airplay2);
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
        volume: f64,
        warning: i32,
        remote: i32,
        receiver_latency_us: i64,
        receiver_latency_sources: u32,
        device_level: f64,
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
            volume: 0.0,
            warning: 0,
            remote: 0,
            receiver_latency_us: 0,
            receiver_latency_sources: 0,
            device_level: 0.0,
        }
    }

    /// Sender events, waiting at most 10 s, until `done` holds for the
    /// events taken so far.
    fn events_until(handle: *mut AscAirPlay, done: impl Fn(&[Event]) -> bool) -> Vec<Event> {
        let mut seen = Vec::new();
        let until = Instant::now() + Duration::from_secs(10);
        while !done(&seen) && Instant::now() < until {
            match take_event(handle) {
                Some(event) => seen.push(event),
                None => std::thread::sleep(Duration::from_millis(10)),
            }
        }
        seen
    }

    /// The next sender event, if one waits.
    fn take_event(handle: *mut AscAirPlay) -> Option<Event> {
        let mut raw = blank_event();
        // SAFETY: a live handle, used by this thread only; a local out struct.
        unsafe { asc_airplay_next_event(handle, &mut raw) }.then(|| Event {
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
            volume: raw.volume,
            warning: raw.warning,
            remote: raw.remote,
            receiver_latency_us: raw.receiver_latency_us,
            receiver_latency_sources: raw.receiver_latency_sources,
            device_level: raw.device_level,
        })
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

    /// The level a television [`Loopback`] holds, in dB, once it has one.
    type Level = Arc<Mutex<Option<f64>>>;

    /// A receiver on 127.0.0.1 that answers every RTSP request with 200 and
    /// the headers its method calls for, and keeps each request's method and
    /// body. Its UDP ports take the audio and are never read.
    struct Loopback {
        port: u16,
        requests: Requests,
        stop: Arc<AtomicBool>,
        hang_up: Arc<AtomicBool>,
        level: Level,
        thread: Option<JoinHandle<()>>,
    }

    impl Loopback {
        fn start() -> Loopback {
            Loopback::start_with(false)
        }

        /// A loopback that answers every request with 401 and a digest
        /// challenge, as a receiver whose password never matches.
        fn start_with(unauthorized: bool) -> Loopback {
            Loopback::serving(unauthorized, false)
        }

        /// A loopback that acts as the measured television (S289, S291): a
        /// volume SET_PARAMETER sets the level it holds, across sessions,
        /// and a volume GET_PARAMETER is answered with that level.
        fn television() -> Loopback {
            Loopback::serving(false, true)
        }

        fn serving(unauthorized: bool, television: bool) -> Loopback {
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
            let hang_up = Arc::new(AtomicBool::new(false));
            let level = Level::default();
            let thread = {
                let (requests, stop, hang_up) = (requests.clone(), stop.clone(), hang_up.clone());
                let held = television.then(|| level.clone());
                std::thread::spawn(move || {
                    let _udp = udp;
                    serve(
                        &listener,
                        &transport,
                        &requests,
                        (&stop, &hang_up),
                        unauthorized,
                        held.as_ref(),
                    );
                })
            };
            Loopback {
                port,
                requests,
                stop,
                hang_up,
                level,
                thread: Some(thread),
            }
        }

        /// A television's own remote sets its level to `db`.
        fn press(&self, db: f64) {
            *self.level.lock().unwrap() = Some(db);
        }

        /// The level a television holds now, in dB.
        fn level(&self) -> Option<f64> {
            *self.level.lock().unwrap()
        }

        /// The volume SET_PARAMETERs received, in order, as their dB.
        fn volume_sets(&self) -> Vec<f64> {
            self.bodies(Method::SetParameter)
                .iter()
                .filter_map(|body| parse_reported(body.as_bytes()))
                .collect()
        }

        /// Closes every open connection once, as a receiver that dropped
        /// the session; new ones are answered as before.
        fn hang_up(&self) {
            self.hang_up.store(true, Ordering::Relaxed);
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
        (stop, hang_up): (&AtomicBool, &AtomicBool),
        unauthorized: bool,
        level: Option<&Level>,
    ) {
        let mut connections: Vec<(TcpStream, Parser, bool)> = Vec::new();
        while !stop.load(Ordering::Relaxed) {
            if hang_up.swap(false, Ordering::Relaxed) {
                connections.clear();
            }
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
                        Method::SetParameter => {
                            if let (Some(level), Some(db)) = (level, parse_reported(&request.body))
                            {
                                *level.lock().unwrap() = Some(db);
                            }
                        }
                        Method::GetParameter => {
                            if let Some(db) = level.and_then(|level| *level.lock().unwrap()) {
                                response.headers.push("Content-Type", "text/parameters");
                                response.body = format!("volume: {db:.6}\r\n").into_bytes();
                            }
                        }
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
            asc_airplay_pause(ptr::null_mut());
            asc_airplay_resume(ptr::null_mut());
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
                volume: 0.0,
                warning: 0,
                remote: 0,
                receiver_latency_us: 0,
                receiver_latency_sources: 0,
                device_level: 0.0,
            }
        );
        // SAFETY: destroyed once, by this thread.
        unsafe { asc_airplay_destroy(&mut handle) };
    }

    // covers: S290
    #[test]
    fn an_added_speaker_keeps_an_own_volume_above_100() {
        let id = "02005E005332";
        let mut handle = listing(vec![loopback_record(id, "Den", closed_port())]);
        let c_id = c_string(id);
        let own = 80.0 / 0.63;
        // SAFETY: a live handle, used by this thread only; a C string.
        unsafe {
            asc_airplay_set_master(handle, 63.0);
            assert!(asc_airplay_add_speaker(handle, c_id.as_ptr(), own));
        }
        assert_eq!(inside(handle).added[id].position, own);
        assert!((inside(handle).added[id].level - 80.0).abs() < 1e-9);
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
        // Its zone at 25 %: 80 % of 25 % is −24 dB, live (S249).
        // SAFETY: a live handle, used by this thread only; a C string.
        unsafe { assert!(asc_airplay_set_zone_volume(handle, c_id.as_ptr(), 25.0)) };
        assert!(wait_for(|| receiver
            .bodies(Method::SetParameter)
            .contains(&"volume: -24.000000\r\n".to_owned())));

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
    fn counters_cross_the_boundary_with_the_c_layout() {
        use std::mem::{align_of, offset_of, size_of};
        assert_eq!(size_of::<AscAirPlayCounters>(), 80);
        assert_eq!(align_of::<AscAirPlayCounters>(), 8);
        let offsets = [
            offset_of!(AscAirPlayCounters, packets_sent),
            offset_of!(AscAirPlayCounters, resend_requests),
            offset_of!(AscAirPlayCounters, packets_resent),
            offset_of!(AscAirPlayCounters, not_held),
            offset_of!(AscAirPlayCounters, timing_replies),
            offset_of!(AscAirPlayCounters, late_packets),
            offset_of!(AscAirPlayCounters, silent_frames),
            offset_of!(AscAirPlayCounters, input_frames_dropped),
            offset_of!(AscAirPlayCounters, foreign_datagrams),
            offset_of!(AscAirPlayCounters, sessions),
        ];
        assert_eq!(offsets, [0, 8, 16, 24, 32, 40, 48, 56, 64, 72]);
        assert_eq!(size_of::<AscAirPlayStreamCounters>(), 32);
        assert_eq!(align_of::<AscAirPlayStreamCounters>(), 8);
        let offsets = [
            offset_of!(AscAirPlayStreamCounters, rate_hz),
            offset_of!(AscAirPlayStreamCounters, packets_encoded),
            offset_of!(AscAirPlayStreamCounters, silent_frames),
            offset_of!(AscAirPlayStreamCounters, input_frames_dropped),
        ];
        assert_eq!(offsets, [0, 8, 16, 24]);
    }

    fn stale_counters() -> AscAirPlayCounters {
        AscAirPlayCounters {
            packets_sent: 7,
            sessions: 3,
            ..AscAirPlayCounters::default()
        }
    }

    fn stale_stream() -> AscAirPlayStreamCounters {
        AscAirPlayStreamCounters {
            rate_hz: 1.0,
            packets_encoded: 7,
            ..AscAirPlayStreamCounters::default()
        }
    }

    #[test]
    fn counters_of_null_dead_or_unknown_speakers_are_zero() {
        let id = c"02005E005331";
        let mut counters = stale_counters();
        let mut stream = stale_stream();
        // SAFETY: null handles and pointers are part of the contract.
        unsafe {
            assert!(!asc_airplay_speaker_counters(
                ptr::null(),
                id.as_ptr(),
                &mut counters
            ));
            assert_eq!(counters, AscAirPlayCounters::default());
            assert!(!asc_airplay_stream_counters(ptr::null(), &mut stream));
            assert_eq!(stream, AscAirPlayStreamCounters::default());
        }

        let mut handle = listing(vec![raop("02005E005331", "Kitchen", OPEN)]);
        // SAFETY: a live handle, used by this thread only; C string literals.
        unsafe {
            assert!(!asc_airplay_speaker_counters(
                handle,
                id.as_ptr(),
                ptr::null_mut()
            ));
            assert!(!asc_airplay_stream_counters(handle, ptr::null_mut()));
            counters = stale_counters();
            assert!(!asc_airplay_speaker_counters(
                handle,
                ptr::null(),
                &mut counters
            ));
            assert_eq!(counters, AscAirPlayCounters::default());
            counters = stale_counters();
            assert!(
                !asc_airplay_speaker_counters(handle, id.as_ptr(), &mut counters),
                "listed but never added"
            );
            assert_eq!(counters, AscAirPlayCounters::default());
            assert!(asc_airplay_stream_counters(handle, &mut stream));
        }
        inside(handle).death.dead.store(true, Ordering::Release);
        stream = stale_stream();
        // SAFETY: a dead handle is still a live pointer, used by this thread
        // only.
        unsafe {
            assert!(!asc_airplay_stream_counters(handle, &mut stream));
            assert_eq!(stream, AscAirPlayStreamCounters::default());
            asc_airplay_destroy(&mut handle);
        }
    }

    fn session_fields(counters: &SpeakerCounters) -> [u64; 9] {
        [
            counters.packets_sent(),
            counters.resend_requests(),
            counters.packets_resent(),
            counters.not_held(),
            counters.timing_replies(),
            counters.late_packets(),
            counters.silent_frames(),
            counters.input_frames_dropped(),
            counters.foreign_datagrams(),
        ]
    }

    fn copied_fields(counters: &AscAirPlayCounters) -> [u64; 9] {
        [
            counters.packets_sent,
            counters.resend_requests,
            counters.packets_resent,
            counters.not_held,
            counters.timing_replies,
            counters.late_packets,
            counters.silent_frames,
            counters.input_frames_dropped,
            counters.foreign_datagrams,
        ]
    }

    #[test]
    fn a_playing_speakers_counters_are_its_sessions() {
        let receiver = Loopback::start();
        let id = "02005E005332";
        let mut handle = listing(vec![loopback_record(id, "Desk", receiver.port)]);
        let c_id = c_string(id);
        // SAFETY: a live handle, used by this thread only; a C string.
        unsafe { assert!(asc_airplay_add_speaker(handle, c_id.as_ptr(), 50.0)) };
        events_until(handle, |seen| has_state(seen, id, AIRPLAY_STATE_PLAYING));
        let mut copied = AscAirPlayCounters::default();
        assert!(wait_for(|| {
            // SAFETY: a live handle, used by this thread only; a C string.
            unsafe { asc_airplay_speaker_counters(handle, c_id.as_ptr(), &mut copied) };
            copied.packets_sent > 0
        }));
        // The sender keeps counting while the copy is made, so each field
        // lies between the session's reads just before and just after.
        let session = inside(handle).added[id].counters[0].1.clone();
        for _ in 0..3 {
            let before = session_fields(&session);
            // SAFETY: a live handle, used by this thread only; a C string.
            assert!(unsafe { asc_airplay_speaker_counters(handle, c_id.as_ptr(), &mut copied) });
            let after = session_fields(&session);
            let within = before
                .iter()
                .zip(&copied_fields(&copied))
                .zip(&after)
                .all(|((low, field), high)| low <= field && field <= high);
            assert!(within, "{before:?} {copied:?} {after:?}");
            assert_eq!(copied.sessions, 1);
        }

        let stream = inside(handle).sender.stream_counters();
        let before = stream.packets_encoded();
        let mut copied = AscAirPlayStreamCounters::default();
        // SAFETY: a live handle, used by this thread only.
        assert!(unsafe { asc_airplay_stream_counters(handle, &mut copied) });
        let after = stream.packets_encoded();
        assert!(
            before <= copied.packets_encoded && copied.packets_encoded <= after,
            "{before} {copied:?} {after}"
        );
        assert!(copied.packets_encoded > 0, "{copied:?}");
        // SAFETY: destroyed once, by this thread.
        unsafe { asc_airplay_destroy(&mut handle) };
    }

    #[test]
    fn a_targets_counters_sum_its_members_and_each_member_has_its_own() {
        let mut handle = create_with(quiet);
        let (left, right) = (pair_member(0x21, "0x2004"), pair_member(0x22, "0x4"));
        let found = audiosync_airplay::targets::find_targets([&left, &right]);
        let pair = found.first().cloned().expect("a pair");
        inside(handle).airplay2 = true;
        inside(handle).take_in(SenderEvent::Receiver(ReceiverEvent::Target(
            TargetChange::Updated(Box::new(pair.clone())),
        )));
        let pair_id = CString::new(pair.id.to_string()).expect("an ID");
        let left_id = CString::new(receiver_id(&left.key)).expect("an ID");
        let mut copied = stale_counters();
        // SAFETY: a live handle and NUL-terminated IDs.
        unsafe {
            assert!(!asc_airplay_speaker_counters(
                handle,
                left_id.as_ptr(),
                &mut copied
            ));
            assert!(asc_airplay_add_speaker(handle, pair_id.as_ptr(), 40.0));
            assert!(asc_airplay_speaker_counters(
                handle,
                pair_id.as_ptr(),
                &mut copied
            ));
            assert_eq!(copied.sessions, 2);
            assert!(asc_airplay_speaker_counters(
                handle,
                left_id.as_ptr(),
                &mut copied
            ));
            assert_eq!(copied.sessions, 1);
            assert!(asc_airplay_remove_speaker(handle, pair_id.as_ptr()));
            assert!(!asc_airplay_speaker_counters(
                handle,
                left_id.as_ptr(),
                &mut copied
            ));
            asc_airplay_destroy(&mut handle);
        }
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
            assert_eq!(
                f64::from(AIRPLAY_MAX_DELAY_MS),
                crate::policy::delay::MAX_DELAY_MS
            );
            assert_eq!(asc_airplay_set_hold_back(handle, 13_000.0), 13_000.0);
            assert_eq!(asc_airplay_set_hold_back(handle, 25_000.0), 20_000.0);
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
                0,
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
                    0,
                    ptr::null(),
                    0
                ),
                "a title that isn't UTF-8"
            );
            assert!(
                !asc_airplay_set_now_playing(
                    handle,
                    none,
                    none,
                    none,
                    ptr::null(),
                    4,
                    ptr::null(),
                    0
                ),
                "a length without artwork"
            );
            assert!(
                !asc_airplay_set_now_playing(
                    handle,
                    none,
                    none,
                    none,
                    b"GIF89a".as_ptr(),
                    6,
                    ptr::null(),
                    0
                ),
                "artwork that isn't a JPEG"
            );
            assert!(
                !asc_airplay_set_now_playing(
                    handle,
                    none,
                    none,
                    none,
                    large.as_ptr(),
                    large.len(),
                    ptr::null(),
                    0
                ),
                "artwork over 512 KiB"
            );
            assert!(
                asc_airplay_set_now_playing(
                    handle,
                    none,
                    none,
                    none,
                    ptr::null(),
                    0,
                    ptr::null(),
                    0
                ),
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
                jpeg.len(),
                ptr::null(),
                0
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

    // covers: S272
    #[test]
    fn now_playing_carries_two_images_and_an_airplay1_speaker_gets_the_small_one() {
        let jpeg = |fill: u8, len: usize| [&[0xFF, 0xD8, 0xFF][..], &vec![fill; len]].concat();
        let (large, small) = (jpeg(b'L', 300), jpeg(b'S', 100));
        let none = ptr::null::<c_char>();
        let receiver = Loopback::start();
        let id = "02005E005333";
        let mut txt = OPEN.to_vec();
        txt.push("md=0,1,2");
        let record = ServiceRecord {
            port: receiver.port,
            addresses: vec![ScopedAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST))],
            ..raop(id, "Den", &txt)
        };
        let mut handle = listing(vec![record]);
        let c_id = c_string(id);
        // SAFETY: a live handle, used by this thread only, nothing else
        // running on it; byte buffers of the lengths given.
        unsafe {
            assert!(asc_airplay_set_now_playing(
                handle,
                none,
                none,
                none,
                large.as_ptr(),
                large.len(),
                ptr::null(),
                0
            ));
            assert_eq!(
                (*handle).now_playing.artwork,
                Some(Artwork::single(large.clone())),
                "one image serves both paths"
            );
            assert!(
                !asc_airplay_set_now_playing(
                    handle,
                    none,
                    none,
                    none,
                    large.as_ptr(),
                    large.len(),
                    b"GIF89a".as_ptr(),
                    6
                ),
                "a small image that isn't a JPEG"
            );
            assert!(
                !asc_airplay_set_now_playing(
                    handle,
                    none,
                    none,
                    none,
                    large.as_ptr(),
                    large.len(),
                    ptr::null(),
                    4
                ),
                "a length without a small image"
            );
            assert!(asc_airplay_set_now_playing(
                handle,
                none,
                none,
                none,
                large.as_ptr(),
                large.len(),
                small.as_ptr(),
                small.len()
            ));
            assert_eq!(
                (*handle).now_playing.artwork,
                Some(Artwork {
                    large: large.clone(),
                    small: small.clone()
                })
            );
            assert!(asc_airplay_add_speaker(handle, c_id.as_ptr(), 50.0));
        }
        let events = events_until(handle, |seen| has_state(seen, id, AIRPLAY_STATE_PLAYING));
        assert!(has_state(&events, id, AIRPLAY_STATE_PLAYING), "{events:?}");
        let small_text = String::from_utf8_lossy(&small).into_owned();
        assert!(
            wait_for(|| receiver.bodies(Method::SetParameter).contains(&small_text)),
            "{:?}",
            receiver.bodies(Method::SetParameter)
        );
        let large_text = String::from_utf8_lossy(&large).into_owned();
        assert!(!receiver.bodies(Method::SetParameter).contains(&large_text));
        // SAFETY: destroyed once, by this thread.
        unsafe { asc_airplay_destroy(&mut handle) };
    }

    #[test]
    fn host_progress_needs_a_length_and_keeps_elapsed_within_it() {
        let progress = |elapsed: u64, length: u64| Progress {
            elapsed: Duration::from_millis(elapsed),
            length: Duration::from_millis(length),
        };
        assert_eq!(
            host_progress(30_000.0, 200_000.0),
            Some(progress(30_000, 200_000))
        );
        assert_eq!(host_progress(-5.0, 1000.0), Some(progress(0, 1000)));
        assert_eq!(host_progress(f64::NAN, 1000.0), Some(progress(0, 1000)));
        assert_eq!(host_progress(9000.0, 1000.0), Some(progress(1000, 1000)));
        for length in [0.0, -1.0, f64::NAN, f64::INFINITY, f64::MAX] {
            assert_eq!(host_progress(10.0, length), None, "{length}");
        }
        assert_eq!(
            moved_on(progress(1000, 5000), Duration::from_secs(2)),
            progress(3000, 5000)
        );
        assert_eq!(
            moved_on(progress(4000, 5000), Duration::from_secs(2)),
            progress(5000, 5000)
        );
    }

    #[test]
    fn progress_reaches_a_speaker_as_rtp_bounds_and_a_new_track_drops_it() {
        // SAFETY: a null handle is part of the contract.
        assert!(!unsafe { asc_airplay_set_progress(ptr::null_mut(), 0.0, 1000.0) });

        let receiver = Loopback::start();
        let id = "02005E005332";
        let mut txt = OPEN.to_vec();
        txt.push("md=0,1,2");
        let record = ServiceRecord {
            port: receiver.port,
            addresses: vec![ScopedAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST))],
            ..raop(id, "Den", &txt)
        };
        let mut handle = listing(vec![record]);
        let c_id = c_string(id);
        let none = ptr::null::<c_char>();
        // SAFETY: a live handle, used by this thread only; a C string.
        unsafe { assert!(asc_airplay_add_speaker(handle, c_id.as_ptr(), 50.0)) };
        let events = events_until(handle, |seen| has_state(seen, id, AIRPLAY_STATE_PLAYING));
        assert!(has_state(&events, id, AIRPLAY_STATE_PLAYING), "{events:?}");
        let progress_bodies = || -> Vec<[u32; 3]> {
            receiver
                .bodies(Method::SetParameter)
                .iter()
                .filter_map(|body| body.strip_prefix("progress: "))
                .map(|bounds| {
                    let bounds: Vec<u32> = bounds
                        .trim()
                        .split('/')
                        .map(|n| n.parse().unwrap())
                        .collect();
                    [bounds[0], bounds[1], bounds[2]]
                })
                .collect()
        };
        // SAFETY: a live handle, used by this thread only; C strings.
        unsafe {
            assert!(asc_airplay_set_now_playing(
                handle,
                c"Track One".as_ptr(),
                none,
                none,
                ptr::null(),
                0,
                ptr::null(),
                0
            ));
            assert!(asc_airplay_set_progress(handle, 30_000.0, 200_000.0));
        }
        assert!(wait_for(|| !progress_bodies().is_empty()));
        let [start, current, end] = progress_bodies()[0];
        assert_eq!(current.wrapping_sub(start), 30 * SAMPLE_RATE);
        assert_eq!(end.wrapping_sub(start), 200 * SAMPLE_RATE);

        // SAFETY: a live handle, used by this thread only, nothing else
        // running on it; C strings.
        unsafe {
            assert!(asc_airplay_set_now_playing(
                handle,
                c"Track One".as_ptr(),
                none,
                none,
                ptr::null(),
                0,
                ptr::null(),
                0
            ));
            assert!((*handle).now_playing.progress.is_some(), "the same track");
            assert!(asc_airplay_set_now_playing(
                handle,
                c"Track Two".as_ptr(),
                none,
                none,
                ptr::null(),
                0,
                ptr::null(),
                0
            ));
            assert!((*handle).now_playing.progress.is_none(), "another track");
            assert!(asc_airplay_set_progress(handle, 0.0, 0.0));
            assert!(asc_airplay_set_now_playing(
                handle,
                c"Track Three".as_ptr(),
                none,
                none,
                ptr::null(),
                0,
                ptr::null(),
                0
            ));
        }
        // Each request follows the one before on the connection, so once
        // the third track arrives, every progress sent before it has too.
        assert!(wait_for(|| receiver
            .bodies(Method::SetParameter)
            .iter()
            .any(|body| body.contains("Track Three"))));
        assert_eq!(progress_bodies().len(), 2, "the same track's, then none");
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
        let no_zone = NO_ZONE_POSITION;
        assert_eq!(speaker_volume(100.0, no_zone, 100.0), Volume::Db(0.0));
        assert_eq!(speaker_volume(50.0, no_zone, 100.0), Volume::Db(-15.0));
        assert_eq!(speaker_volume(100.0, no_zone, 50.0), Volume::Db(-15.0));
        assert_eq!(speaker_volume(0.0, no_zone, 100.0), Volume::Muted);
        assert_eq!(speaker_volume(100.0, no_zone, 0.0), Volume::Muted);
        assert_eq!(
            speaker_volume(150.0, no_zone, 200.0),
            Volume::Db(0.0),
            "clamped"
        );
        assert_eq!(
            speaker_volume(-5.0, no_zone, 100.0),
            Volume::Muted,
            "clamped"
        );
    }

    // covers: S249
    #[test]
    fn the_airplay_level_is_own_times_zone_times_master() {
        assert_eq!(speaker_volume(80.0, 50.0, 100.0), Volume::Db(-18.0));
        assert_eq!(speaker_volume(100.0, 50.0, 50.0), Volume::Db(-22.5));
        assert_eq!(speaker_volume(100.0, 0.0, 100.0), Volume::Muted, "zone 0");
        for (own, master) in [(80.0, 100.0), (37.0, 61.0), (1.0, 1.0), (100.0, 0.0)] {
            let before = (own / 100.0_f64).clamp(0.0, 1.0) * (master / 100.0_f64).clamp(0.0, 1.0);
            let expected = if before > 0.0 {
                Volume::Db(MIN_DB * (1.0 - before))
            } else {
                Volume::Muted
            };
            assert_eq!(
                speaker_volume(own, NO_ZONE_POSITION, master),
                expected,
                "no zone: as before zones"
            );
        }
    }

    // covers: S250
    #[test]
    fn a_read_back_at_zone_50_restores_the_own_volume() {
        let mut handle = create_with(quiet);
        let id = "02005E005366";
        let c_id = c_string(id);
        let key = DeviceKey::Id(DeviceId([0x02, 0, 0x5E, 0, 0x53, 0x66]));
        // SAFETY: a live handle, used by this thread only; a C string.
        unsafe { assert!(asc_airplay_set_zone_volume(handle, c_id.as_ptr(), 50.0)) };
        add_for_volume(handle, id, key.clone(), 50.0);
        // SAFETY: as above.
        unsafe { asc_airplay_read_volumes(handle) };
        // Its own level back: 50 × 50 % is −22.5 dB, nothing moved.
        report_volume(handle, &key, -22.5);
        assert!(inside(handle).events.is_empty());
        // SAFETY: as above.
        unsafe { asc_airplay_read_volumes(handle) };
        // −18 dB is level 0.4: own 0.4 ÷ 0.5, so 80.
        report_volume(handle, &key, -18.0);
        let events = events_until(handle, |events| !events.is_empty());
        assert_eq!(events.len(), 1, "{events:?}");
        assert_eq!(events[0].kind, AIRPLAY_EVENT_VOLUME);
        assert_eq!(events[0].volume, 80.0);
        assert_eq!(inside(handle).added[id].position, 80.0);
        assert_eq!(inside(handle).zones[id], 50.0, "the zone stays");
        assert_eq!(inside(handle).master, 100.0, "the master stays");
        // SAFETY: destroyed once, by this thread.
        unsafe { asc_airplay_destroy(&mut handle) };
    }

    // covers: S250, S290
    #[test]
    fn a_read_back_with_zone_0_leaves_the_own_volume() {
        let mut handle = create_with(quiet);
        let id = "02005E005367";
        let c_id = c_string(id);
        let key = DeviceKey::Id(DeviceId([0x02, 0, 0x5E, 0, 0x53, 0x67]));
        // SAFETY: a live handle, used by this thread only; a C string.
        unsafe { assert!(asc_airplay_set_zone_volume(handle, c_id.as_ptr(), 0.0)) };
        add_for_volume(handle, id, key.clone(), 50.0);
        // SAFETY: as above.
        unsafe { asc_airplay_read_volumes(handle) };
        report_volume(handle, &key, -6.0);
        // The row shows the speaker's level; the own volume stays.
        let levels: Vec<(String, f64, f64)> = inside(handle)
            .events
            .drain(..)
            .filter_map(|(_, pending)| match pending {
                Pending::Volume(id, position, level) => Some((id, position, level)),
                _ => None,
            })
            .collect();
        assert_eq!(levels, vec![(id.to_owned(), 50.0, 80.0)]);
        assert_eq!(inside(handle).added[id].position, 50.0);
        // SAFETY: destroyed once, by this thread.
        unsafe { asc_airplay_destroy(&mut handle) };
    }

    /// The host's last set of `id` was `ago` back.
    fn set_ago(handle: *mut AscAirPlay, id: &str, ago: Duration) {
        inside(handle)
            .added
            .get_mut(id)
            .expect("added")
            .volume_set_at = Instant::now().checked_sub(ago);
    }

    fn read_back(handle: *mut AscAirPlay, key: &DeviceKey, db: f64) {
        // SAFETY: a live handle, used by this thread only.
        unsafe { asc_airplay_read_volumes(handle) };
        report_volume(handle, key, db);
    }

    // covers: S291
    #[test]
    fn the_first_read_near_a_sent_level_is_its_echo_and_the_baseline() {
        let mut handle = create_with(quiet);
        let id = "02005E005370";
        let c_id = c_string(id);
        let key = DeviceKey::Id(DeviceId([0x02, 0, 0x5E, 0, 0x53, 0x70]));
        add_for_volume(handle, id, key.clone(), 50.0);
        // SAFETY: a live handle, used by this thread only; a C string.
        unsafe { assert!(asc_airplay_set_volume(handle, c_id.as_ptr(), 10.0)) };
        // Sent −27 dB; the speaker rounds it 0.3 dB off: its echo, however late.
        set_ago(handle, id, Duration::from_secs(10));
        read_back(handle, &key, -27.3);
        assert_eq!(inside(handle).added[id].position, 10.0);
        read_back(handle, &key, -27.3);
        assert_eq!(inside(handle).added[id].position, 10.0);
        assert!(volume_events(handle).is_empty());
        // One press down from the baseline: level 8, so own 8.
        read_back(handle, &key, -27.6);
        assert_eq!(own_of(handle, id), 8.0);
        assert_eq!(volume_events(handle), vec![(id.to_owned(), 8.0)]);
        // More than 0.5 dB from a level just sent is a change at once.
        // SAFETY: as above.
        unsafe { assert!(asc_airplay_set_volume(handle, c_id.as_ptr(), 10.0)) };
        read_back(handle, &key, -26.4);
        assert_eq!(own_of(handle, id), 12.0);
        // SAFETY: destroyed once, by this thread.
        unsafe { asc_airplay_destroy(&mut handle) };
    }

    // covers: S291
    #[test]
    fn only_a_playing_speaker_is_read() {
        let mut handle = create_with(quiet);
        let id = "02005E005377";
        let key = DeviceKey::Id(DeviceId([0x02, 0, 0x5E, 0, 0x53, 0x77]));
        add_for_volume(handle, id, key.clone(), 50.0);
        let state = |state| SenderEvent::Speaker {
            key: key.clone(),
            event: SpeakerEvent::State(state),
        };
        inside(handle).take_in(state(SpeakerState::Stopped));
        // SAFETY: a live handle, used by this thread only.
        unsafe { asc_airplay_read_volumes(handle) };
        assert!(!inside(handle).added[id].reading);
        inside(handle).take_in(state(SpeakerState::Playing));
        // SAFETY: as above.
        unsafe { asc_airplay_read_volumes(handle) };
        assert!(inside(handle).added[id].reading);
        // SAFETY: destroyed once, by this thread.
        unsafe { asc_airplay_destroy(&mut handle) };
    }

    // covers: S290, S291
    #[test]
    fn each_new_reading_reaches_the_host_unrounded() {
        let mut handle = create_with(quiet);
        let id = "02005E005377";
        let key = DeviceKey::Id(DeviceId([0x02, 0, 0x5E, 0, 0x53, 0x77]));
        add_for_volume(handle, id, key.clone(), 50.0);
        let rows = |handle: *mut AscAirPlay| -> Vec<(f64, f64)> {
            inside(handle)
                .events
                .drain(..)
                .filter_map(|(_, pending)| match pending {
                    Pending::Volume(_, position, level) => Some((nine(position), row_level(level))),
                    _ => None,
                })
                .collect()
        };
        // Level 20.6: own 20.6 unrounded, row 20.
        read_back(handle, &key, -23.82);
        assert_eq!(rows(handle), vec![(20.6, 20.0)]);
        read_back(handle, &key, -23.82);
        assert!(rows(handle).is_empty());
        // Level 21: own 21, row 21.
        read_back(handle, &key, -23.7);
        assert_eq!(rows(handle), vec![(21.0, 21.0)]);
        // SAFETY: destroyed once, by this thread.
        unsafe { asc_airplay_destroy(&mut handle) };
    }

    // covers: S290, S291
    #[test]
    fn a_row_move_on_a_0_6_db_grid_keeps_the_row_until_the_speaker_moves() {
        let mut handle = create_with(quiet);
        let id = "02005E005376";
        let c_id = c_string(id);
        let key = DeviceKey::Id(DeviceId([0x02, 0, 0x5E, 0, 0x53, 0x76]));
        // SAFETY: a live handle, used by this thread only.
        unsafe { asc_airplay_set_master(handle, 63.0) };
        add_for_volume(handle, id, key.clone(), 50.0);
        // The row moved to 20 with the master at 63: own 20 ÷ 0.63, level 20 sent.
        let moved = crate::volume::row_move(20.0, 100.0, 63.0).expect("k above 0");
        // SAFETY: as above; a C string.
        unsafe {
            assert!(asc_airplay_set_device_level(
                handle,
                c_id.as_ptr(),
                moved.own_position,
                moved.device_level
            ));
        }
        assert_eq!(own_of(handle, id), nine(20.0 / 0.63));
        assert_eq!(inside(handle).added[id].read_back.echo_of(), Some(-24.0));
        // The television reads −24 back on its 0.6 dB grid: no event.
        read_back(handle, &key, -24.200_001);
        read_back(handle, &key, -24.200_001);
        assert!(volume_events(handle).is_empty());
        assert_eq!(own_of(handle, id), nine(20.0 / 0.63));
        // Its remote moves it to −23.6 dB: level 21.3, row 21, own 21.3 ÷ 0.63.
        read_back(handle, &key, -23.6);
        let levels: Vec<(f64, f64)> = inside(handle)
            .events
            .drain(..)
            .filter_map(|(_, pending)| match pending {
                Pending::Volume(_, position, level) => Some((nine(position), row_level(level))),
                _ => None,
            })
            .collect();
        let own = (-23.6 + 30.0) / 0.3 / 0.63;
        assert_eq!(levels, vec![(nine(own), 21.0)]);
        // A master move keeps the own volume and sends own × 0.5.
        // SAFETY: as above.
        unsafe { asc_airplay_set_master(handle, 50.0) };
        assert_eq!(own_of(handle, id), nine(own));
        let sent = inside(handle).added[id].read_back.echo_of().expect("sent");
        assert!((sent - (-30.0 + 0.3 * own * 0.5)).abs() < 1e-9, "{sent}");
        // SAFETY: destroyed once, by this thread.
        unsafe { asc_airplay_destroy(&mut handle) };
    }

    // covers: S271
    #[test]
    fn a_read_back_while_the_slider_is_dragged_is_ignored() {
        let mut handle = create_with(quiet);
        let id = "02005E005371";
        let c_id = c_string(id);
        let key = DeviceKey::Id(DeviceId([0x02, 0, 0x5E, 0, 0x53, 0x71]));
        add_for_volume(handle, id, key.clone(), 50.0);
        // SAFETY: a live handle, used by this thread only; a C string.
        unsafe {
            assert!(asc_airplay_set_volume_dragging(handle, c_id.as_ptr(), true));
            assert!(!asc_airplay_set_volume_dragging(handle, ptr::null(), true));
        }
        read_back(handle, &key, -6.0);
        assert_eq!(inside(handle).added[id].position, 50.0);
        // A read asked during the drag and answered after it is dropped too.
        // SAFETY: as above.
        unsafe {
            asc_airplay_read_volumes(handle);
            assert!(asc_airplay_set_volume_dragging(
                handle,
                c_id.as_ptr(),
                false
            ));
        }
        report_volume(handle, &key, -6.0);
        assert_eq!(inside(handle).added[id].position, 50.0);
        read_back(handle, &key, -6.0);
        assert_eq!(inside(handle).added[id].position, 80.0);
        // A row closed mid-drag never lets go; turning the speaker off and
        // on starts it undragged.
        // SAFETY: as above.
        unsafe {
            assert!(asc_airplay_set_volume_dragging(handle, c_id.as_ptr(), true));
            assert!(asc_airplay_remove_speaker(handle, c_id.as_ptr()));
        }
        add_for_volume(handle, id, key.clone(), 50.0);
        read_back(handle, &key, -6.0);
        assert_eq!(inside(handle).added[id].position, 80.0);
        // SAFETY: destroyed once, by this thread.
        unsafe { asc_airplay_destroy(&mut handle) };
    }

    // covers: S250, S271
    #[test]
    fn a_read_back_one_remote_step_away_moves_only_that_speakers_volume() {
        let mut handle = create_with(quiet);
        let (a, b) = ("02005E005372", "02005E005373");
        let key = DeviceKey::Id(DeviceId([0x02, 0, 0x5E, 0, 0x53, 0x72]));
        let other = DeviceKey::Id(DeviceId([0x02, 0, 0x5E, 0, 0x53, 0x73]));
        add_for_volume(handle, a, key.clone(), 50.0);
        add_for_volume(handle, b, other, 50.0);
        // SAFETY: a live handle, used by this thread only.
        unsafe { asc_airplay_set_master(handle, 50.0) };
        // Sent −22.5 dB (level 0.25), read back as its echo; one press up
        // is −22.2 dB, level 0.26, so 0.26 ÷ 0.5 = 52.
        read_back(handle, &key, -22.5);
        read_back(handle, &key, -22.2);
        let events = events_until(handle, |events| {
            events.iter().any(|e| e.kind == AIRPLAY_EVENT_VOLUME)
        });
        let volumes: Vec<&Event> = events
            .iter()
            .filter(|e| e.kind == AIRPLAY_EVENT_VOLUME)
            .collect();
        assert_eq!(volumes.len(), 1, "{events:?}");
        assert_eq!((volumes[0].id.as_str(), nine(volumes[0].volume)), (a, 52.0));
        assert_eq!(own_of(handle, a), 52.0);
        assert_eq!(inside(handle).added[b].position, 50.0);
        assert_eq!(inside(handle).master, 50.0);
        // With the master at 0 every position sounds the same: the row stays.
        // SAFETY: as above.
        unsafe { asc_airplay_set_master(handle, 0.0) };
        set_ago(handle, a, Duration::from_secs(4));
        read_back(handle, &key, -144.0);
        assert_eq!(own_of(handle, a), 52.0);
        // SAFETY: destroyed once, by this thread.
        unsafe { asc_airplay_destroy(&mut handle) };
    }

    #[test]
    fn latency_event_carries_receiver_latency_us_and_sources() {
        let mut handle = create_with(quiet);
        let id = "02005E005375";
        let key = DeviceKey::Id(DeviceId([0x02, 0, 0x5E, 0, 0x53, 0x75]));
        add_for_volume(handle, id, key.clone(), 50.0);
        let latency = |frames, us, sources| SpeakerLatency {
            arrival_to_sound: Duration::from_millis(1750),
            receiver_latency_frames: frames,
            receiver_latency_us: us,
            receiver_latency_sources: sources,
            first_sound: Instant::now(),
        };
        let reported = latency(
            Some(11_025),
            400_000,
            LatencySources::AUDIO_LATENCY.union(LatencySources::AUDIO_LATENCIES),
        );
        for event in [reported, latency(None, 0, LatencySources::NONE)] {
            inside(handle).take_in(SenderEvent::Speaker {
                key: key.clone(),
                event: SpeakerEvent::Latency(event),
            });
        }
        let events = events_until(handle, |events| {
            events
                .iter()
                .filter(|e| e.kind == AIRPLAY_EVENT_LATENCY)
                .count()
                == 2
        });
        let latencies: Vec<(i64, i64, u32)> = events
            .iter()
            .filter(|e| e.kind == AIRPLAY_EVENT_LATENCY && e.id == id)
            .map(|e| {
                (
                    e.receiver_latency_frames,
                    e.receiver_latency_us,
                    e.receiver_latency_sources,
                )
            })
            .collect();
        assert_eq!(
            latencies,
            [
                (
                    11_025,
                    400_000,
                    AIRPLAY_LATENCY_SOURCE_AUDIO_LATENCY | AIRPLAY_LATENCY_SOURCE_AUDIO_LATENCIES
                ),
                (-1, 0, 0)
            ]
        );

        // A shortfall is a warning that names R_s and Lf; a held Lf is its
        // own event.
        inside(handle).take_in(SenderEvent::Speaker {
            key: key.clone(),
            event: SpeakerEvent::Warning(Warning::LatencyShortfall {
                reported_us: 1_800_000,
                latency_frames: 77_175,
            }),
        });
        inside(handle).take_in(SenderEvent::LatencyHeld { frames: 88_200 });
        let events = events_until(handle, |events| {
            events.iter().any(|e| e.kind == AIRPLAY_EVENT_LATENCY_HELD)
        });
        let warning = events
            .iter()
            .find(|e| e.kind == AIRPLAY_EVENT_WARNING)
            .expect("warning");
        assert_eq!(warning.warning, AIRPLAY_WARNING_LATENCY_SHORTFALL);
        assert!(warning.detail.contains("1800 ms") && warning.detail.contains("77175"));
        let held = events
            .iter()
            .find(|e| e.kind == AIRPLAY_EVENT_LATENCY_HELD)
            .expect("held");
        assert!((held.latency_ms - 2000.0).abs() < 1e-9);

        // The host's own delay per speaker sits on top of the hold-back.
        let c_id = c_string(id);
        // SAFETY: a live handle, used by this thread only; a C string.
        unsafe {
            assert_eq!(
                asc_airplay_set_speaker_offset(handle, c_id.as_ptr(), 170.0),
                170.0
            );
            assert_eq!(
                asc_airplay_set_speaker_offset(handle, c_id.as_ptr(), -5.0),
                0.0
            );
            assert_eq!(
                asc_airplay_set_speaker_offset(handle, c_id.as_ptr(), f64::NAN),
                0.0
            );
            assert_eq!(
                asc_airplay_set_speaker_offset(handle, c_id.as_ptr(), 30_000.0),
                20_000.0
            );
            let other = c_string("02005E0053FF");
            assert_eq!(
                asc_airplay_set_speaker_offset(handle, other.as_ptr(), 10.0),
                -1.0
            );
            assert_eq!(
                asc_airplay_set_speaker_offset(handle, ptr::null(), 10.0),
                -1.0
            );
            assert_eq!(
                asc_airplay_set_speaker_offset(ptr::null_mut(), c_id.as_ptr(), 10.0),
                -1.0
            );
            assert!(!asc_airplay_sounding(ptr::null()));
            assert!(!asc_airplay_sounding(handle));
        }
        assert_eq!(inside(handle).added[id].offset, MAX_DELAY);
        // SAFETY: destroyed once, by this thread.
        unsafe { asc_airplay_destroy(&mut handle) };
    }

    // covers: S271
    #[test]
    fn a_speaker_whose_reads_stopped_is_not_asked_again() {
        let mut handle = create_with(quiet);
        let id = "02005E005374";
        let key = DeviceKey::Id(DeviceId([0x02, 0, 0x5E, 0, 0x53, 0x74]));
        add_for_volume(handle, id, key.clone(), 50.0);
        // SAFETY: a live handle, used by this thread only.
        unsafe { asc_airplay_read_volumes(handle) };
        assert!(inside(handle).added[id].reading);
        inside(handle).take_in(SenderEvent::Speaker {
            key: key.clone(),
            event: SpeakerEvent::Warning(Warning::VolumeReadsStopped),
        });
        let events = events_until(handle, |events| {
            events.iter().any(|e| e.kind == AIRPLAY_EVENT_WARNING)
        });
        assert!(
            events.iter().any(|e| e.kind == AIRPLAY_EVENT_WARNING
                && e.warning == AIRPLAY_WARNING_VOLUME_READS_STOPPED),
            "{events:?}"
        );
        report_volume(handle, &key, -6.0);
        // SAFETY: as above.
        unsafe { asc_airplay_read_volumes(handle) };
        assert!(!inside(handle).added[id].reading);
        assert_eq!(inside(handle).added[id].position, 50.0);
        // SAFETY: destroyed once, by this thread.
        unsafe { asc_airplay_destroy(&mut handle) };
    }

    // covers: S271
    #[test]
    fn a_new_session_asks_a_speaker_whose_reads_stopped_again() {
        let refused = Warning::RequestFailed {
            method: Method::GetParameter,
            status: 501,
        };
        let retrying = SpeakerState::Retrying { at: Instant::now() };
        for (warning, states) in [
            (
                Warning::VolumeReadsStopped,
                vec![retrying, SpeakerState::Connecting, SpeakerState::Playing],
            ),
            (
                refused,
                vec![SpeakerState::Connecting, SpeakerState::Playing],
            ),
        ] {
            let mut handle = create_with(quiet);
            let id = "02005E005379";
            let key = DeviceKey::Id(DeviceId([0x02, 0, 0x5E, 0, 0x53, 0x79]));
            add_for_volume(handle, id, key.clone(), 50.0);
            inside(handle).take_in(SenderEvent::Speaker {
                key: key.clone(),
                event: SpeakerEvent::Warning(warning.clone()),
            });
            for state in states {
                inside(handle).take_in(SenderEvent::Speaker {
                    key: key.clone(),
                    event: SpeakerEvent::State(state),
                });
            }
            // SAFETY: a live handle, used by this thread only.
            unsafe { asc_airplay_read_volumes(handle) };
            assert!(inside(handle).added[id].reading, "{warning:?}");
            // SAFETY: destroyed once, by this thread.
            unsafe { asc_airplay_destroy(&mut handle) };
        }
    }

    // covers: S249
    #[test]
    fn bad_zone_volumes_change_nothing() {
        let mut handle = create_with(quiet);
        let c_id = c_string("02005E005368");
        // SAFETY: a null handle and ID are refused; a live handle, used by
        // this thread only; a C string.
        unsafe {
            assert!(!asc_airplay_set_zone_volume(
                ptr::null_mut(),
                c_id.as_ptr(),
                50.0
            ));
            assert!(!asc_airplay_set_zone_volume(handle, ptr::null(), 50.0));
            assert!(!asc_airplay_set_zone_volume(
                handle,
                c_id.as_ptr(),
                f64::NAN
            ));
            assert!(asc_airplay_set_zone_volume(handle, c_id.as_ptr(), 150.0));
        }
        assert!(inside(handle).zones.is_empty(), "150 is clamped to no zone");
        // SAFETY: as above.
        unsafe {
            assert!(asc_airplay_set_zone_volume(handle, c_id.as_ptr(), 40.0));
            assert!(asc_airplay_set_zone_volume(handle, c_id.as_ptr(), 100.0));
        }
        assert!(inside(handle).zones.is_empty(), "back to no zone");
        // SAFETY: destroyed once, by this thread.
        unsafe { asc_airplay_destroy(&mut handle) };
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
        assert_eq!(
            error_code(UserReason::ReceiverUnverified),
            (AIRPLAY_ERROR_RECEIVER_UNVERIFIED, AIRPLAY_REASON_NONE)
        );
        assert_eq!(
            error_code(UserReason::Unsupported(UnsupportedReason::Timing)),
            (AIRPLAY_ERROR_UNSUPPORTED, AIRPLAY_REASON_TIMING)
        );
        assert_eq!(
            error_code(UserReason::Unsupported(
                UnsupportedReason::OneAirPlay2Session
            )),
            (AIRPLAY_ERROR_UNSUPPORTED, AIRPLAY_REASON_ONE_AIRPLAY2)
        );
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
            volume: 0.0,
            warning: 0,
            remote: 0,
            receiver_latency_us: 0,
            receiver_latency_sources: 0,
            device_level: 0.0,
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
            asc_airplay_pause(handle);
            asc_airplay_resume(handle);
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

    #[test]
    fn a_receiver_reached_over_airplay2_is_listed_on_that_path_only_with_the_switch_on() {
        let mut handle = create_with(quiet);
        let mut record = airplay2_only();
        record.txt.push(b"flags=0x200".to_vec());
        let device =
            Device::from_records(record.device_key(), None, Some(&record)).expect("a device");
        let id = receiver_id(&device.key);
        let info = ReceiverInfo::new(device);
        let report = inside(handle).report(ReceiverEvent::Changed(Box::new(info.clone())));
        assert_eq!(
            report.availability, AIRPLAY_AIRPLAY2_ONLY,
            "the switch is off"
        );
        assert_eq!(report.flags & AIRPLAY_FLAG_OVER_AIRPLAY2, 0);

        inside(handle).airplay2 = true;
        let report = inside(handle).report(ReceiverEvent::Changed(Box::new(info.clone())));
        assert_eq!(
            report.availability, AIRPLAY_NEEDS_PIN,
            "it asks for pairing: a PIN"
        );
        assert_ne!(report.flags & AIRPLAY_FLAG_OVER_AIRPLAY2, 0);
        assert_eq!(report.flags & AIRPLAY_FLAG_PAIRED, 0, "nothing kept yet");

        let mut blob = vec![1u8];
        blob.extend_from_slice(b"01234567-89AB-CDEF-0123-456789ABCDEF");
        blob.extend_from_slice(&[5; 64]);
        blob.extend_from_slice(b"receiver");
        let credentials = HomeKitCredentials::from_bytes(&blob).expect("a credential blob");
        let _ = inside(handle).identity.insert(&id, credentials);
        let report = inside(handle).report(ReceiverEvent::Changed(Box::new(info)));
        assert_eq!(
            report.availability, AIRPLAY_AVAILABLE,
            "paired on AirPlay 2"
        );
        assert_ne!(
            report.flags & AIRPLAY_FLAG_PAIRED,
            0,
            "Forget Pairing applies"
        );

        let kitchen = raop("02005E005306", "Kitchen", OPEN);
        let device =
            Device::from_records(kitchen.device_key(), Some(&kitchen), None).expect("a device");
        let report =
            inside(handle).report(ReceiverEvent::Changed(Box::new(ReceiverInfo::new(device))));
        assert_eq!(
            report.flags & AIRPLAY_FLAG_OVER_AIRPLAY2,
            0,
            "AirPlay 1 drives it"
        );
        assert_eq!(report.availability, AIRPLAY_AVAILABLE);
        // SAFETY: destroyed once, by this thread.
        unsafe { asc_airplay_destroy(&mut handle) };
    }

    // Any host on the LAN can publish a RAOP record under the speaker's
    // device id, so stored HomeKit credentials pin it to AirPlay 2 (P2).
    #[test]
    fn a_forged_raop_record_never_lists_a_homekit_paired_receiver_on_airplay1() {
        let mut handle = create_with(quiet);
        inside(handle).airplay2 = true;
        let airplay = airplay2_only();
        let forged = raop("02005E005305", "Living Room", PASSWORD);
        let both = Device::from_records(airplay.device_key(), Some(&forged), Some(&airplay))
            .expect("a device");
        let id = receiver_id(&both.key);
        let forged_only =
            Device::from_records(airplay.device_key(), Some(&forged), None).expect("a device");
        let _ = inside(handle).identity.insert(&id, homekit_blob(3));

        for (device, what) in [(both, "next to its own record"), (forged_only, "alone")] {
            let mut info = ReceiverInfo::new(device);
            info.plan = Ok(());
            let report = inside(handle).report(ReceiverEvent::Changed(Box::new(info)));
            assert_ne!(
                report.flags & AIRPLAY_FLAG_OVER_AIRPLAY2,
                0,
                "forged record {what}"
            );
            assert_eq!(
                report.availability, AIRPLAY_AVAILABLE,
                "forged record {what}: no password asked"
            );
            assert_ne!(
                report.flags & AIRPLAY_FLAG_PAIRED,
                0,
                "forged record {what}"
            );
        }
        // SAFETY: destroyed once, by this thread.
        unsafe { asc_airplay_destroy(&mut handle) };
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

    #[test]
    fn the_airplay2_identity_is_kept_only_with_the_switch_on() {
        let folder = scratch("airplay2-identity");
        let path = folder.join("identity.dat");
        let c_path = CString::new(path.to_str().expect("UTF-8")).expect("no NUL");
        let mut handle = create_with(quiet);
        // SAFETY: a live handle, used by this thread only; the path is a
        // NUL-terminated string.
        unsafe {
            assert_eq!(
                asc_airplay_open_airplay2_identity(handle, ptr::null()),
                AIRPLAY_PAIRINGS_FAILED
            );
            inside(handle).airplay2 = false;
            assert_eq!(
                asc_airplay_open_airplay2_identity(handle, c_path.as_ptr()),
                0
            );
            assert!(!path.exists(), "nothing is written with the switch off");
            inside(handle).airplay2 = true;
            assert_eq!(
                asc_airplay_open_airplay2_identity(handle, c_path.as_ptr()),
                0
            );
            assert!(path.exists(), "the new device ID is written at once");
        }
        let device_id = inside(handle).identity.device_id();
        let mut blob = vec![1u8];
        blob.extend_from_slice(b"01234567-89AB-CDEF-0123-456789ABCDEF");
        blob.extend_from_slice(&[9; 64]);
        blob.extend_from_slice(b"receiver");
        let credentials = HomeKitCredentials::from_bytes(&blob).expect("a credential blob");
        let key = DeviceKey::Id(DeviceId([0x02, 0, 0x5E, 0, 0x53, 0x50]));
        let queued = inside(handle).events.len();
        inside(handle).take_in(SenderEvent::Speaker {
            key,
            event: SpeakerEvent::HomeKitPaired(credentials.clone()),
        });
        assert_eq!(
            inside(handle).events.len(),
            queued,
            "credentials never cross to the host"
        );
        // SAFETY: destroyed once, by this thread.
        unsafe { asc_airplay_destroy(&mut handle) };
        let (again, loaded) = IdentityStore::open(path);
        assert_eq!(loaded, Loaded::Receivers(1));
        assert_eq!(again.device_id(), device_id);
        assert_eq!(again.get("02005E005350"), Some(&credentials));
    }

    // covers: S224
    #[test]
    fn the_host_is_told_how_private_the_files_were_found() {
        assert_eq!(access_code(Access::Private), AIRPLAY_FILE_PRIVATE);
        assert_eq!(access_code(Access::Tightened), AIRPLAY_FILE_TIGHTENED);
        assert_eq!(access_code(Access::StillOpen), AIRPLAY_FILE_STILL_OPEN);
        assert_eq!(access_code(Access::Unread), AIRPLAY_FILE_UNREAD);
        let folder = scratch("file-access");
        let pairings = folder.join("pairings.dat");
        let identity = folder.join("identity.dat");
        let c_pairings = CString::new(pairings.to_str().expect("UTF-8")).expect("no NUL");
        let c_identity = CString::new(identity.to_str().expect("UTF-8")).expect("no NUL");
        let mut handle = create_with(quiet);
        // SAFETY: a live handle, or null, used by this thread only; the
        // paths are NUL-terminated strings.
        unsafe {
            assert_eq!(
                asc_airplay_pairings_access(ptr::null()),
                AIRPLAY_FILE_UNREAD
            );
            assert_eq!(
                asc_airplay_airplay2_identity_access(ptr::null()),
                AIRPLAY_FILE_UNREAD
            );
            inside(handle).airplay2 = false;
            assert_eq!(
                asc_airplay_airplay2_identity_access(handle),
                AIRPLAY_FILE_UNREAD,
                "the identity file isn't read with the switch off"
            );
            inside(handle).airplay2 = true;
            assert_eq!(asc_airplay_open_pairings(handle, c_pairings.as_ptr()), 0);
            assert_eq!(
                asc_airplay_open_airplay2_identity(handle, c_identity.as_ptr()),
                0
            );
            assert_eq!(asc_airplay_pairings_access(handle), AIRPLAY_FILE_PRIVATE);
            assert_eq!(
                asc_airplay_airplay2_identity_access(handle),
                AIRPLAY_FILE_PRIVATE
            );
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let credentials = audiosync_airplay::pairing::Credentials::new(
                "0123456789ABCDEF",
                [7; audiosync_airplay::pairing::SECRET_BYTES],
            )
            .expect("valid credentials");
            inside(handle)
                .store
                .insert("02005E005377", credentials)
                .expect("written");
            let open = std::fs::Permissions::from_mode(0o644);
            std::fs::set_permissions(&pairings, open.clone()).expect("mode set");
            std::fs::set_permissions(&identity, open).expect("mode set");
            // SAFETY: as above.
            unsafe {
                assert_eq!(asc_airplay_open_pairings(handle, c_pairings.as_ptr()), 1);
                assert_eq!(
                    asc_airplay_open_airplay2_identity(handle, c_identity.as_ptr()),
                    0
                );
                assert_eq!(asc_airplay_pairings_access(handle), AIRPLAY_FILE_TIGHTENED);
                assert_eq!(
                    asc_airplay_airplay2_identity_access(handle),
                    AIRPLAY_FILE_TIGHTENED
                );
            }
        }
        // SAFETY: destroyed once, by this thread.
        unsafe { asc_airplay_destroy(&mut handle) };
    }

    /// HomeKit credentials made from `fill`, for telling two pairings apart.
    fn homekit_blob(fill: u8) -> HomeKitCredentials {
        let mut blob = vec![1u8];
        blob.extend_from_slice(b"01234567-89AB-CDEF-0123-456789ABCDEF");
        blob.extend_from_slice(&[fill; 64]);
        blob.extend_from_slice(b"receiver");
        HomeKitCredentials::from_bytes(&blob).expect("a credential blob")
    }

    /// The error a session that needed a pairing ends with.
    fn pairing_required() -> SpeakerEvent {
        SpeakerEvent::Error(SpeakerError {
            error: audiosync_airplay::error::SessionError::new(ErrorKind::PairingRequired),
            reason: UserReason::PairingRequired,
            retry_at: None,
        })
    }

    // A turn-down proves nothing (a reset receiver and an impostor look the
    // same), so the credentials stay on disk until a new pairing succeeds.
    #[test]
    fn airplay2_credentials_turned_down_stay_on_disk_until_a_new_pairing_replaces_them() {
        let folder = scratch("airplay2-identity-turned-down");
        let path = folder.join("identity.dat");
        let c_path = CString::new(path.to_str().expect("UTF-8")).expect("no NUL");
        let mut handle = create_with(quiet);
        inside(handle).airplay2 = true;
        // SAFETY: a live handle, used by this thread only; the path is a
        // NUL-terminated string.
        unsafe {
            assert_eq!(
                asc_airplay_open_airplay2_identity(handle, c_path.as_ptr()),
                0
            );
        }
        let mut record = airplay2_only();
        record.txt.push(b"flags=0x200".to_vec());
        let device =
            Device::from_records(record.device_key(), None, Some(&record)).expect("a device");
        let id = receiver_id(&device.key);
        let key = device.key.clone();
        let mut info = ReceiverInfo::new(device);
        info.plan = Ok(());
        inside(handle).listed.insert(id.clone(), info.clone());
        let first = homekit_blob(7);
        let second = homekit_blob(8);
        inside(handle).pairing.insert(id.clone(), key.clone());
        inside(handle).take_in(SenderEvent::Pairing {
            key: key.clone(),
            event: PairingEvent::HomeKitPaired(first.clone()),
        });
        let report = inside(handle).report(ReceiverEvent::Changed(Box::new(info.clone())));
        assert_eq!(report.availability, AIRPLAY_AVAILABLE);

        inside(handle).take_in(SenderEvent::Speaker {
            key: key.clone(),
            event: pairing_required(),
        });
        let report = inside(handle).report(ReceiverEvent::Changed(Box::new(info.clone())));
        assert_eq!(
            report.availability, AIRPLAY_NEEDS_PIN,
            "it asks for its PIN"
        );
        assert_eq!(report.flags & AIRPLAY_FLAG_PAIRED, 0);
        assert_ne!(
            report.flags & AIRPLAY_FLAG_CREDENTIALS_KEPT,
            0,
            "Forget Pairing stays on offer"
        );
        assert_eq!(inside(handle).identity.get(&id), Some(&first));
        let (on_disk, _) = IdentityStore::open(path.clone());
        assert_eq!(on_disk.get(&id), Some(&first), "still on disk");

        inside(handle).pairing.insert(id.clone(), key.clone());
        inside(handle).take_in(SenderEvent::Pairing {
            key,
            event: PairingEvent::HomeKitPaired(second.clone()),
        });
        let report = inside(handle).report(ReceiverEvent::Changed(Box::new(info)));
        assert_eq!(report.availability, AIRPLAY_AVAILABLE);
        let (on_disk, _) = IdentityStore::open(path);
        assert_eq!(on_disk.get(&id), Some(&second), "replaced");
        // SAFETY: destroyed once, by this thread.
        unsafe { asc_airplay_destroy(&mut handle) };
    }

    // Playing on them shows that whatever turned them down is gone (P154).
    #[test]
    fn turned_down_credentials_a_session_plays_on_list_as_paired_again() {
        let folder = scratch("airplay2-identity-plays-again");
        let path = folder.join("identity.dat");
        let c_path = CString::new(path.to_str().expect("UTF-8")).expect("no NUL");
        let mut handle = create_with(quiet);
        inside(handle).airplay2 = true;
        // SAFETY: a live handle, used by this thread only; the path is a
        // NUL-terminated string.
        unsafe {
            assert_eq!(
                asc_airplay_open_airplay2_identity(handle, c_path.as_ptr()),
                0
            );
        }
        let mut record = airplay2_only();
        record.txt.push(b"flags=0x200".to_vec());
        let device =
            Device::from_records(record.device_key(), None, Some(&record)).expect("a device");
        let id = receiver_id(&device.key);
        let key = device.key.clone();
        let mut info = ReceiverInfo::new(device);
        info.plan = Ok(());
        inside(handle).listed.insert(id.clone(), info.clone());
        inside(handle).pairing.insert(id.clone(), key.clone());
        inside(handle).take_in(SenderEvent::Pairing {
            key: key.clone(),
            event: PairingEvent::HomeKitPaired(homekit_blob(7)),
        });
        inside(handle).take_in(SenderEvent::Speaker {
            key: key.clone(),
            event: pairing_required(),
        });
        let report = inside(handle).report(ReceiverEvent::Changed(Box::new(info.clone())));
        assert_eq!(report.availability, AIRPLAY_NEEDS_PIN);

        inside(handle).take_in(SenderEvent::Speaker {
            key,
            event: SpeakerEvent::State(SpeakerState::Playing),
        });
        let report = inside(handle).report(ReceiverEvent::Changed(Box::new(info)));
        assert_eq!(report.availability, AIRPLAY_AVAILABLE);
        assert_ne!(report.flags & AIRPLAY_FLAG_PAIRED, 0);
        // SAFETY: destroyed once, by this thread.
        unsafe { asc_airplay_destroy(&mut handle) };
    }

    #[test]
    fn a_turned_down_legacy_pairing_stays_on_disk_until_a_new_pairing_replaces_it() {
        let folder = scratch("airplay-pairings-turned-down");
        let path = folder.join("pairings.dat");
        let c_path = CString::new(path.to_str().expect("UTF-8")).expect("no NUL");
        let id = "02005E005376";
        let mut handle = listing(vec![raop(id, "Den TV", APPLE_TV)]);
        // SAFETY: a live handle, used by this thread only; the path is a
        // NUL-terminated string.
        unsafe {
            assert_eq!(asc_airplay_open_pairings(handle, c_path.as_ptr()), 0);
        }
        let _ = next(handle, 1);
        let key = inside(handle).listed[id].device.key.clone();
        let credentials = |client: &str, fill: u8| {
            audiosync_airplay::pairing::Credentials::new(
                client,
                [fill; audiosync_airplay::pairing::SECRET_BYTES],
            )
            .expect("valid credentials")
        };
        let first = credentials("0123456789ABCDEF", 7);
        let second = credentials("FEDCBA9876543210", 8);
        inside(handle).pairing.insert(id.to_owned(), key.clone());
        inside(handle).take_in(SenderEvent::Pairing {
            key: key.clone(),
            event: PairingEvent::Paired(first.clone()),
        });
        assert_eq!(next(handle, 1)[0].availability, AIRPLAY_AVAILABLE);

        inside(handle).take_in(SenderEvent::Speaker {
            key: key.clone(),
            event: pairing_required(),
        });
        assert_eq!(
            next(handle, 1)[0].availability,
            AIRPLAY_NEEDS_PIN,
            "it asks for its PIN"
        );
        let (on_disk, _) = PairingStore::open(path.clone());
        assert_eq!(on_disk.get(id), Some(&first), "still on disk");

        inside(handle).pairing.insert(id.to_owned(), key.clone());
        inside(handle).take_in(SenderEvent::Pairing {
            key,
            event: PairingEvent::Paired(second.clone()),
        });
        assert_eq!(next(handle, 1)[0].availability, AIRPLAY_AVAILABLE);
        let (on_disk, _) = PairingStore::open(path);
        assert_eq!(on_disk.get(id), Some(&second), "replaced");
        // SAFETY: destroyed once, by this thread.
        unsafe { asc_airplay_destroy(&mut handle) };
    }

    /// A speaker the host added, straight into the handle, at `position`
    /// and set long enough ago for its volume reads to count.
    fn add_for_volume(handle: *mut AscAirPlay, id: &str, key: DeviceKey, position: f64) {
        let mut added = Added::new(key, position, None, Vec::new(), Vec::new());
        added.volume_set_at = None;
        added.playing = true;
        // As a real add sends it: the first read near it is its echo (S291).
        let zone = zone_in(&inside(handle).zones, id);
        added.sent(speaker_volume(position, zone, inside(handle).master));
        inside(handle).added.insert(id.to_owned(), added);
    }

    fn report_volume(handle: *mut AscAirPlay, key: &DeviceKey, db: f64) {
        inside(handle).take_in(SenderEvent::Speaker {
            key: key.clone(),
            event: SpeakerEvent::VolumeReported(db),
        });
    }

    #[test]
    #[cfg(target_pointer_width = "64")]
    fn the_event_layout_matches_the_header() {
        // The Swift smoke test checks the same size against the header.
        assert_eq!(std::mem::size_of::<AscAirPlayEvent>(), 112);
        assert_eq!(std::mem::offset_of!(AscAirPlayEvent, device_level), 104);
        assert_eq!(std::mem::offset_of!(AscAirPlayEvent, volume), 72);
        assert_eq!(std::mem::offset_of!(AscAirPlayEvent, warning), 80);
        assert_eq!(
            std::mem::offset_of!(AscAirPlayEvent, receiver_latency_us),
            88
        );
        assert_eq!(
            std::mem::offset_of!(AscAirPlayEvent, receiver_latency_sources),
            96
        );
        assert_eq!(std::mem::offset_of!(AscAirPlayEvent, remote), 84);
    }

    fn remote(
        handle: *mut AscAirPlay,
        key: &DeviceKey,
        target: Option<&str>,
        command: RemoteCommand,
    ) {
        inside(handle).take_in(SenderEvent::Remote {
            key: key.clone(),
            target: target.map(|id| TargetId::parse(id).expect("a target ID")),
            command,
        });
    }

    /// The volume events waiting, as row and position to 1e-9; every
    /// waiting event is taken.
    fn volume_events(handle: *mut AscAirPlay) -> Vec<(String, f64)> {
        inside(handle)
            .events
            .drain(..)
            .filter_map(|(_, pending)| match pending {
                Pending::Volume(id, position, _) => Some((id, nine(position))),
                _ => None,
            })
            .collect()
    }

    /// `value` to 9 decimals: the unrounded own volume of S290 without its
    /// float error.
    fn nine(value: f64) -> f64 {
        (value * 1e9).round() / 1e9
    }

    /// The own volume of the speaker added under `id`, to 9 decimals.
    fn own_of(handle: *mut AscAirPlay, id: &str) -> f64 {
        nine(inside(handle).added[id].position)
    }

    /// Whether a remote control or volume event waits; other events, such
    /// as the stream's timeline, are passed over.
    fn acted(handle: *mut AscAirPlay) -> bool {
        inside(handle)
            .events
            .iter()
            .any(|(_, pending)| matches!(pending, Pending::Remote(..) | Pending::Volume(..)))
    }

    /// Whether the host's last set of `id`, its own or a remote's, was just
    /// now: the speaker got a level and S271's window runs.
    fn just_sent(handle: *mut AscAirPlay, id: &str) -> bool {
        inside(handle).added[id]
            .volume_set_at
            .is_some_and(|at| at.elapsed() < Duration::from_secs(1))
    }

    fn speaker_key(last: u8) -> DeviceKey {
        DeviceKey::Id(DeviceId([0x02, 0, 0x5E, 0, 0x54, last]))
    }

    // covers: S274
    #[test]
    fn a_remote_transport_reaches_the_host_as_a_remote_event() {
        let mut handle = create_with(quiet);
        let key = speaker_key(0x01);
        add_for_volume(handle, "02005E005401", key.clone(), 50.0);
        remote(handle, &key, None, RemoteCommand::Next);
        assert!(!acted(handle), "no app to control");
        let app = c_string("Music");
        // SAFETY: a live handle, used by this thread only; a C string.
        unsafe {
            assert!(asc_airplay_set_playback(
                handle,
                app.as_ptr(),
                AIRPLAY_PLAYBACK_PAUSED
            ));
        }
        remote(handle, &key, None, RemoteCommand::Pause);
        assert!(!acted(handle), "already paused");
        remote(handle, &key, None, RemoteCommand::Play);
        let events = events_until(handle, |events| !events.is_empty());
        assert_eq!(events.len(), 1, "{events:?}");
        assert_eq!(events[0].kind, AIRPLAY_EVENT_REMOTE);
        assert_eq!(events[0].id, "02005E005401");
        assert_eq!(events[0].remote, AIRPLAY_REMOTE_PLAY_PAUSE);
        // SAFETY: as above; null is no app.
        unsafe {
            assert!(asc_airplay_set_playback(handle, ptr::null(), 0));
            assert!(!asc_airplay_set_playback(ptr::null_mut(), ptr::null(), 0));
        }
        remote(handle, &key, None, RemoteCommand::Previous);
        assert!(!acted(handle));
        // SAFETY: destroyed once, by this thread.
        unsafe { asc_airplay_destroy(&mut handle) };
    }

    // The Mac stops reading the player when nothing shows the track: the
    // paused state it read last no longer holds.
    // covers: S274
    #[test]
    fn a_remote_pause_after_the_reading_stopped_goes_as_play_pause() {
        let mut handle = create_with(quiet);
        let key = speaker_key(0x07);
        add_for_volume(handle, "02005E005407", key.clone(), 50.0);
        let app = c_string("Music");
        // SAFETY: a live handle, used by this thread only; a C string.
        unsafe {
            assert!(asc_airplay_set_playback(
                handle,
                app.as_ptr(),
                AIRPLAY_PLAYBACK_PAUSED
            ));
            assert!(asc_airplay_set_playback(
                handle,
                app.as_ptr(),
                AIRPLAY_PLAYBACK_UNKNOWN
            ));
        }
        remote(handle, &key, None, RemoteCommand::Pause);
        let events = events_until(handle, |events| !events.is_empty());
        assert_eq!(events.len(), 1, "{events:?}");
        assert_eq!(events[0].kind, AIRPLAY_EVENT_REMOTE);
        assert_eq!(events[0].remote, AIRPLAY_REMOTE_PLAY_PAUSE);
        // SAFETY: destroyed once, by this thread.
        unsafe { asc_airplay_destroy(&mut handle) };
    }

    // covers: S275, S250, S290
    #[test]
    fn a_remote_volume_up_moves_only_that_speakers_row_by_5() {
        let mut handle = create_with(quiet);
        let kitchen = speaker_key(0x02);
        add_for_volume(handle, "02005E005402", kitchen.clone(), 40.0);
        add_for_volume(handle, "02005E005403", speaker_key(0x03), 60.0);
        inside(handle).master = 80.0;
        // Own 40 at master 80 is row 32; up 5 is row 37, own 37 ÷ 0.8.
        remote(handle, &kitchen, None, RemoteCommand::VolumeUp);
        assert_eq!(
            volume_events(handle),
            vec![("02005E005402".to_owned(), 46.25)]
        );
        assert_eq!(own_of(handle, "02005E005402"), 46.25);
        let inner = inside(handle);
        assert_eq!(inner.added["02005E005403"].position, 60.0);
        assert_eq!(inner.master, 80.0);
        assert!(!just_sent(handle, "02005E005403"));
        // SAFETY: destroyed once, by this thread.
        unsafe { asc_airplay_destroy(&mut handle) };
    }

    // covers: S275, S271
    #[test]
    fn a_remote_volume_step_is_sent_and_starts_the_3_s_window() {
        let mut handle = create_with(quiet);
        let key = speaker_key(0x04);
        let id = "02005E005404";
        add_for_volume(handle, id, key.clone(), 50.0);
        remote(handle, &key, None, RemoteCommand::VolumeDown);
        assert_eq!(volume_events(handle), vec![(id.to_owned(), 45.0)]);
        assert!(just_sent(handle, id));
        // Sent −16.5 dB; the receiver echoes it 0.3 dB off, by DACP or a read.
        remote(
            handle,
            &key,
            None,
            RemoteCommand::DeviceVolume(Volume::Db(-16.2)),
        );
        read_back(handle, &key, -16.8);
        assert!(volume_events(handle).is_empty());
        assert_eq!(inside(handle).added[id].position, 45.0);
        // SAFETY: destroyed once, by this thread.
        unsafe { asc_airplay_destroy(&mut handle) };
    }

    // covers: S276, S271
    #[test]
    fn a_device_volume_near_the_sent_level_is_ignored_for_3_s_only() {
        let mut handle = create_with(quiet);
        let id = "02005E005405";
        let c_id = c_string(id);
        let key = speaker_key(0x05);
        add_for_volume(handle, id, key.clone(), 50.0);
        // SAFETY: a live handle, used by this thread only; a C string.
        unsafe { assert!(asc_airplay_set_volume(handle, c_id.as_ptr(), 10.0)) };
        // Sent −27 dB; within 0.5 dB of it is the echo.
        remote(
            handle,
            &key,
            None,
            RemoteCommand::DeviceVolume(Volume::Db(-27.3)),
        );
        set_ago(handle, id, Duration::from_millis(2_900));
        remote(
            handle,
            &key,
            None,
            RemoteCommand::DeviceVolume(Volume::Db(-26.7)),
        );
        assert!(volume_events(handle).is_empty());
        assert_eq!(inside(handle).added[id].position, 10.0);
        // After 3 s the same level is the receiver's own: level 0.09, so 9.
        set_ago(handle, id, Duration::from_secs(3));
        remote(
            handle,
            &key,
            None,
            RemoteCommand::DeviceVolume(Volume::Db(-27.3)),
        );
        assert_eq!(volume_events(handle), vec![(id.to_owned(), 9.0)]);
        // Within 3 s, a level further than 0.5 dB off counts at once.
        remote(
            handle,
            &key,
            None,
            RemoteCommand::DeviceVolume(Volume::Db(-26.4)),
        );
        assert_eq!(volume_events(handle), vec![(id.to_owned(), 12.0)]);
        // SAFETY: destroyed once, by this thread.
        unsafe { asc_airplay_destroy(&mut handle) };
    }

    // covers: S276
    #[test]
    fn a_device_volume_while_the_slider_is_dragged_is_ignored() {
        let mut handle = create_with(quiet);
        let id = "02005E005406";
        let c_id = c_string(id);
        let key = speaker_key(0x06);
        add_for_volume(handle, id, key.clone(), 50.0);
        // SAFETY: a live handle, used by this thread only; a C string.
        unsafe { assert!(asc_airplay_set_volume_dragging(handle, c_id.as_ptr(), true)) };
        remote(
            handle,
            &key,
            None,
            RemoteCommand::DeviceVolume(Volume::Db(-6.0)),
        );
        assert!(volume_events(handle).is_empty());
        assert_eq!(inside(handle).added[id].position, 50.0);
        // SAFETY: as above.
        unsafe {
            assert!(asc_airplay_set_volume_dragging(
                handle,
                c_id.as_ptr(),
                false
            ))
        };
        remote(
            handle,
            &key,
            None,
            RemoteCommand::DeviceVolume(Volume::Db(-6.0)),
        );
        assert_eq!(volume_events(handle), vec![(id.to_owned(), 80.0)]);
        // SAFETY: destroyed once, by this thread.
        unsafe { asc_airplay_destroy(&mut handle) };
    }

    // covers: S276, S250
    #[test]
    fn a_device_volume_moves_the_own_volume_and_is_sent_back() {
        let mut handle = create_with(quiet);
        let id = "02005E005407";
        let key = speaker_key(0x07);
        add_for_volume(handle, id, key.clone(), 50.0);
        add_for_volume(handle, "02005E005408", speaker_key(0x08), 50.0);
        remote(
            handle,
            &key,
            None,
            RemoteCommand::DeviceVolume(Volume::Db(-6.0)),
        );
        assert_eq!(volume_events(handle), vec![(id.to_owned(), 80.0)]);
        assert!(just_sent(handle, id), "sent back as after a drag");
        let inner = inside(handle);
        assert_eq!(inner.added["02005E005408"].position, 50.0);
        assert_eq!(inner.master, 100.0);
        // The level the row already has changes nothing.
        set_ago(handle, id, Duration::from_secs(3));
        remote(
            handle,
            &key,
            None,
            RemoteCommand::DeviceVolume(Volume::Db(-6.0)),
        );
        assert!(volume_events(handle).is_empty());
        assert!(!just_sent(handle, id));
        // SAFETY: destroyed once, by this thread.
        unsafe { asc_airplay_destroy(&mut handle) };
    }

    // covers: S276, S290
    #[test]
    fn a_device_volume_at_k_0_reports_its_level_and_keeps_the_own_volume() {
        let mut handle = create_with(quiet);
        let id = "02005E005410";
        let key = speaker_key(0x10);
        add_for_volume(handle, id, key.clone(), 50.0);
        // SAFETY: a live handle, used by this thread only.
        unsafe { asc_airplay_set_master(handle, 0.0) };
        inside(handle).events.clear();
        remote(
            handle,
            &key,
            None,
            RemoteCommand::DeviceVolume(Volume::Db(-18.0)),
        );
        let moved: Vec<(String, f64, f64)> = inside(handle)
            .events
            .drain(..)
            .filter_map(|(_, pending)| match pending {
                Pending::Volume(id, position, level) => Some((id, nine(position), nine(level))),
                _ => None,
            })
            .collect();
        assert_eq!(moved, [(id.to_owned(), 50.0, 40.0)]);
        assert!(just_sent(handle, id), "sent back as after a drag");
        // SAFETY: destroyed once, by this thread.
        unsafe { asc_airplay_destroy(&mut handle) };
    }

    // covers: S276
    #[test]
    fn a_muted_device_volume_sets_the_own_volume_to_0() {
        let mut handle = create_with(quiet);
        let id = "02005E005409";
        let key = speaker_key(0x09);
        add_for_volume(handle, id, key.clone(), 50.0);
        remote(
            handle,
            &key,
            None,
            RemoteCommand::DeviceVolume(Volume::Muted),
        );
        assert_eq!(volume_events(handle), vec![(id.to_owned(), 0.0)]);
        add_for_volume(handle, id, key.clone(), 50.0);
        remote(
            handle,
            &key,
            None,
            RemoteCommand::DeviceVolume(Volume::Db(-30.0)),
        );
        assert_eq!(volume_events(handle), vec![(id.to_owned(), 0.0)]);
        // SAFETY: destroyed once, by this thread.
        unsafe { asc_airplay_destroy(&mut handle) };
    }

    // covers: S277
    #[test]
    fn a_users_volume_change_forgets_the_muted_position() {
        let mut handle = create_with(quiet);
        let id = "02005E00540A";
        let c_id = c_string(id);
        let key = speaker_key(0x0A);
        add_for_volume(handle, id, key.clone(), 50.0);
        remote(handle, &key, None, RemoteCommand::MuteToggle);
        assert_eq!(volume_events(handle), vec![(id.to_owned(), 0.0)]);
        // SAFETY: a live handle, used by this thread only; a C string.
        unsafe { assert!(asc_airplay_set_volume(handle, c_id.as_ptr(), 0.0)) };
        remote(handle, &key, None, RemoteCommand::MuteToggle);
        assert!(volume_events(handle).is_empty());
        // SAFETY: destroyed once, by this thread.
        unsafe { asc_airplay_destroy(&mut handle) };
    }

    // covers: S278
    #[test]
    fn a_members_command_moves_the_targets_row() {
        let mut handle = create_with(quiet);
        let member = speaker_key(0x0B);
        let pair = "pair:remote-pair";
        let added = Added::new(
            DeviceKey::Instance(ServiceKind::AirPlay, pair.to_owned()),
            40.0,
            Some(TargetId::parse(pair).expect("a target ID")),
            vec!["02005E00540B".to_owned()],
            Vec::new(),
        );
        inside(handle).added.insert(pair.to_owned(), added);
        remote(handle, &member, Some(pair), RemoteCommand::VolumeUp);
        assert_eq!(volume_events(handle), vec![(pair.to_owned(), 45.0)]);
        let app = c_string("Music");
        // SAFETY: a live handle, used by this thread only; a C string.
        unsafe { assert!(asc_airplay_set_playback(handle, app.as_ptr(), 1)) };
        remote(handle, &member, Some(pair), RemoteCommand::Next);
        let events = events_until(handle, |events| !events.is_empty());
        assert_eq!(events[0].kind, AIRPLAY_EVENT_REMOTE);
        assert_eq!(events[0].id, pair);
        // A target that is no longer added does nothing.
        inside(handle).added.remove(pair);
        remote(handle, &member, Some(pair), RemoteCommand::VolumeUp);
        assert!(!acted(handle));
        // SAFETY: destroyed once, by this thread.
        unsafe { asc_airplay_destroy(&mut handle) };
    }

    // covers: S276, S290
    #[test]
    fn a_device_volume_reports_the_level_it_was_sent() {
        let no_zone = NO_ZONE_POSITION;
        for master in [100.0, 50.0, 25.0] {
            for position in (1..=100).map(f64::from) {
                let db = speaker_volume(position, no_zone, master).wire_db();
                let level = device_level_for_db(db).expect("a finite reading");
                assert!(
                    (level - device_level(position, no_zone, master)).abs() < 1e-9,
                    "{position} at {master}: {level}"
                );
            }
        }
        assert_eq!(device_level_for_db(-144.0), Some(0.0), "muted");
        assert_eq!(device_level_for_db(-40.0), Some(0.0));
        assert_eq!(device_level_for_db(3.0), Some(100.0));
        assert_eq!(device_level_for_db(f64::NAN), None);
    }

    #[test]
    fn a_volume_changed_on_the_speaker_moves_its_slider_once() {
        let mut handle = create_with(quiet);
        let key = DeviceKey::Id(DeviceId([0x02, 0, 0x5E, 0, 0x53, 0x60]));
        add_for_volume(handle, "02005E005360", key.clone(), 50.0);
        // The live sender's timeline can post at any moment; only the
        // speaker's own events are under test.
        let speaker_events = |handle: *mut AscAirPlay| {
            inside(handle)
                .events
                .iter()
                .filter(|(_, p)| !matches!(p, Pending::Timeline { .. }))
                .count()
        };
        // Nobody asked: an answer that isn't the host's is dropped.
        report_volume(handle, &key, -6.0);
        assert_eq!(speaker_events(handle), 0);
        // SAFETY: a live handle, used by this thread only.
        unsafe { asc_airplay_read_volumes(handle) };
        assert!(inside(handle).added["02005E005360"].reading);
        // Its own level back (−15 dB at 50 with the master at 100), within
        // the speaker's rounding: nothing moved.
        report_volume(handle, &key, -15.1);
        assert_eq!(speaker_events(handle), 0);
        // SAFETY: as above.
        unsafe { asc_airplay_read_volumes(handle) };
        report_volume(handle, &key, -6.0);
        let events: Vec<Event> = events_until(handle, |events| {
            events.iter().any(|e| e.kind != AIRPLAY_EVENT_TIMELINE)
        })
        .into_iter()
        .filter(|e| e.kind != AIRPLAY_EVENT_TIMELINE)
        .collect();
        assert_eq!(events.len(), 1, "{events:?}");
        assert_eq!(events[0].kind, AIRPLAY_EVENT_VOLUME);
        assert_eq!(events[0].id, "02005E005360");
        assert_eq!(events[0].volume, 80.0);
        assert_eq!(events[0].detail, "");
        assert_eq!(inside(handle).added["02005E005360"].position, 80.0);
        // The same level again is no news, and nothing went back to the
        // speaker: its position only follows.
        // SAFETY: as above.
        unsafe { asc_airplay_read_volumes(handle) };
        report_volume(handle, &key, -6.0);
        assert_eq!(speaker_events(handle), 0);
        // SAFETY: destroyed once, by this thread.
        unsafe { asc_airplay_destroy(&mut handle) };
    }

    #[test]
    fn a_speaker_reconnects_at_the_volume_changed_on_it() {
        let receiver = Loopback::start();
        let id = "02005E005364";
        let mut handle = listing(vec![loopback_record(id, "Shelf", receiver.port)]);
        let c_id = c_string(id);
        // SAFETY: a live handle, used by this thread only; a C string.
        unsafe { assert!(asc_airplay_add_speaker(handle, c_id.as_ptr(), 50.0)) };
        let volumes = || -> Vec<String> {
            receiver
                .bodies(Method::SetParameter)
                .into_iter()
                .filter(|body| body.starts_with("volume:"))
                .collect()
        };
        let first = "volume: -15.000000\r\n".to_owned();
        let reported = "volume: -6.000000\r\n".to_owned();
        assert!(wait_for(|| volumes().contains(&first)), "{:?}", volumes());
        let key = {
            let added = inside(handle).added.get_mut(id).expect("added");
            added.volume_set_at = None;
            added.reading = true;
            added.key.clone()
        };
        // Turned up to −6 dB on the speaker: slider 80 with the master at 100.
        report_volume(handle, &key, -6.0);
        assert_eq!(inside(handle).added[id].position, 80.0);
        // Never sent back: a receiver that rounds sets more coarsely than
        // its remote steps would undo the press (S291).
        std::thread::sleep(Duration::from_millis(500));
        assert!(!volumes().contains(&reported), "{:?}", volumes());
        let before = volumes().len();
        receiver.hang_up();
        assert!(
            wait_for(|| volumes().len() > before),
            "no reconnect: {:?}",
            volumes()
        );
        assert_eq!(volumes()[before], reported, "{:?}", volumes());
        // SAFETY: destroyed once, by this thread.
        unsafe { asc_airplay_destroy(&mut handle) };
    }

    /// What the host's row shows for an own volume at `master`, outside a
    /// zone (S290).
    fn shown_row(own: f64, master: f64) -> f64 {
        row_level(device_level(own, NO_ZONE_POSITION, master))
    }

    /// Whether the television got `db` on the wire, to its six decimals.
    fn same_db(got: f64, db: f64) -> bool {
        (got - db).abs() < 5e-6
    }

    /// The speaker `id` on a television [`Loopback`], with the master at
    /// `master`, added at own volume `own` and playing with its first level
    /// on the television.
    fn on_television(receiver: &Loopback, id: &str, master: f64, own: f64) -> *mut AscAirPlay {
        let handle = listing(vec![loopback_record(id, "Television", receiver.port)]);
        let c_id = c_string(id);
        let sets = receiver.volume_sets().len();
        // SAFETY: a live handle, used by this thread only; a C string.
        unsafe {
            asc_airplay_set_master(handle, master);
            assert!(asc_airplay_add_speaker(handle, c_id.as_ptr(), own));
        }
        let events = events_until(handle, |seen| has_state(seen, id, AIRPLAY_STATE_PLAYING));
        assert!(has_state(&events, id, AIRPLAY_STATE_PLAYING), "{events:?}");
        assert!(
            wait_for(|| receiver.volume_sets().len() > sets),
            "no volume set: {:?}",
            receiver.volume_sets()
        );
        handle
    }

    /// One read of the speaker `id` (S291): the host asks, the television
    /// answers with the level it holds, and the volume events that answer
    /// led to come back.
    fn read_once(handle: *mut AscAirPlay, id: &str) -> Vec<Event> {
        // SAFETY: a live handle, used by this thread only.
        unsafe { asc_airplay_read_volumes(handle) };
        assert!(inside(handle).added[id].reading, "not asked");
        let mut events = events_until(handle, |_| !inside(handle).added[id].reading);
        assert!(!inside(handle).added[id].reading, "no answer");
        events.extend(std::iter::from_fn(|| take_event(handle)));
        events
            .into_iter()
            .filter(|event| event.kind == AIRPLAY_EVENT_VOLUME && event.id == id)
            .collect()
    }

    /// A row move to `row` as the Mac host makes it (S290): the core's
    /// result goes to the speaker. Returns the own volume to store.
    fn move_row(handle: *mut AscAirPlay, id: &str, row: f64, master: f64) -> f64 {
        let (mut own, mut level) = (f64::NAN, f64::NAN);
        // SAFETY: both point to live doubles.
        let moved = unsafe {
            crate::ffi::asc_airplay_row_move(row, NO_ZONE_POSITION, master, &mut own, &mut level)
        };
        assert!(moved, "row {row} at master {master} sends nothing");
        let c_id = c_string(id);
        // SAFETY: a live handle, used by this thread only; a C string.
        assert!(unsafe { asc_airplay_set_device_level(handle, c_id.as_ptr(), own, level) });
        own
    }

    /// Waits for the television to get `db` as its latest volume set.
    fn lands(receiver: &Loopback, db: f64) {
        let landed = || {
            receiver
                .volume_sets()
                .last()
                .is_some_and(|&got| same_db(got, db))
        };
        assert!(
            wait_for(landed),
            "{db} dB never landed: {:?}",
            receiver.volume_sets()
        );
        assert!(receiver.level().is_some_and(|got| same_db(got, db)));
    }

    // covers: S290, S291
    #[test]
    fn k1_at_master_63_the_row_and_the_television_agree_both_ways() {
        let receiver = Loopback::television();
        let id = "02005E005391";
        let master = 63.0;
        let mut handle = on_television(&receiver, id, master, 50.0);
        // Own 50 at master 63 is device level 31.5 (-30 + 0.3 x 31.5 dB), the row 31.
        let sets = receiver.volume_sets();
        assert!(sets.iter().all(|&db| same_db(db, -20.55)), "{sets:?}");
        assert!(read_once(handle, id).is_empty(), "the echo moved the row");

        // The television's remote to 20 (−24 dB): the row shows 20.
        receiver.press(-24.0);
        let events = read_once(handle, id);
        assert_eq!(events.len(), 1, "{events:?}");
        assert_eq!(row_level(events[0].device_level), 20.0, "{events:?}");
        assert!((events[0].volume - 20.0 / 0.63).abs() < 1e-6, "{events:?}");
        assert_eq!(shown_row(events[0].volume, master), 20.0);
        assert_eq!(row_level(inside(handle).added[id].level), 20.0);

        // Its remote to 50, then the row moved to 20: 20 lands on it.
        receiver.press(-15.0);
        let events = read_once(handle, id);
        assert_eq!(events.len(), 1, "{events:?}");
        assert_eq!(row_level(events[0].device_level), 50.0, "{events:?}");
        let own = move_row(handle, id, 20.0, master);
        lands(&receiver, -24.0);
        assert!(
            receiver
                .bodies(Method::SetParameter)
                .contains(&"volume: -24.000000\r\n".to_owned())
        );
        assert_eq!(shown_row(own, master), 20.0);
        assert_eq!(row_level(inside(handle).added[id].level), 20.0);
        let tv_row = row_level(device_level_for_db(receiver.level().unwrap()).unwrap());
        assert_eq!(tv_row, 20.0, "the television shows another number");
        assert!(read_once(handle, id).is_empty(), "the echo moved the row");
        // SAFETY: destroyed once, by this thread.
        unsafe { asc_airplay_destroy(&mut handle) };
    }

    // covers: S290, S291, S276, S271
    #[test]
    fn the_s290_vector_cases_land_on_the_television() {
        use serde_json::Value;
        let doc: Value = serde_json::from_str(include_str!("../../tests/vectors/s290_volume.json"))
            .expect("valid JSON");
        let number = |value: &Value, key: &str| value.get(key).and_then(Value::as_f64);
        // The dB each step leaves on the television, worked out by hand
        // from S289 (-30 + 0.3 x D, -144 at D = 0) rather than through the
        // code under test; every case starts with an add at own volume 0.
        let wire: [(&str, &[f64]); 6] = [
            ("row 100 at master 63 sends 100", &[0.0]),
            (
                "TV at 80 at master 63, master to 64 gives 81",
                &[-6.0, -5.619048],
            ),
            (
                "row 100 at master 63, master to 50 gives 79",
                &[0.0, -6.190476],
            ),
            (
                "TV at 80 at master 63, master to 100 caps at 100, back to 63 gives 80",
                &[-6.0, 0.0, -6.0],
            ),
            (
                "DeviceVolume 100 at master 63, master to 100 and back keeps 100",
                &[0.0, 0.0, 0.0],
            ),
            (
                "DeviceVolume 40 at master 0 keeps own and shows 40 until the master moves",
                &[-144.0, -18.0, -15.0],
            ),
        ];
        let cases = doc["cases"].as_array().expect("cases");

        let receiver = Loopback::television();
        for (index, (name, dbs)) in wire.into_iter().enumerate() {
            let case = cases
                .iter()
                .find(|case| case["name"].as_str() == Some(name))
                .unwrap_or_else(|| panic!("{name} missing from the vectors"));
            assert_eq!(number(case, "zone"), Some(NO_ZONE_POSITION), "{name}");
            let steps = case["steps"].as_array().expect("steps");
            assert_eq!(steps.len(), dbs.len(), "{name}");
            let id = format!("02005E0054{index:02X}");
            let c_id = c_string(&id);
            let mut master = number(case, "master").expect("master");
            let mut own = 0.0;
            let mut handle = on_television(&receiver, &id, master, own);
            lands(&receiver, -144.0);
            for (step, &db) in steps.iter().zip(dbs) {
                let level = number(step, "level").expect("level");
                let sets = receiver.volume_sets().len();
                // What the host's row shows: a level from the speaker as it
                // came, any other change through the core's level.
                let shown;
                if let Some(value) = number(step, "own") {
                    own = value;
                    // SAFETY: a live handle, used by this thread only; a C string.
                    assert!(unsafe { asc_airplay_set_volume(handle, c_id.as_ptr(), own) });
                    lands(&receiver, db);
                    shown = device_level(own, NO_ZONE_POSITION, master);
                } else if number(step, "tv_level").is_some() {
                    receiver.press(db);
                    let events = read_once(handle, &id);
                    assert_eq!(events.len(), 1, "{name}: {step}: {events:?}");
                    (own, shown) = (events[0].volume, events[0].device_level);
                    assert_eq!(receiver.volume_sets().len(), sets, "{name}: sent back");
                } else if number(step, "device_volume").is_some() {
                    receiver.press(db);
                    let key = inside(handle).added[&id].key.clone();
                    let command = RemoteCommand::DeviceVolume(Volume::Db(db));
                    remote(handle, &key, None, command);
                    let events = volume_events_of(handle, &id);
                    assert_eq!(events.len(), 1, "{name}: {step}: {events:?}");
                    (own, shown) = (events[0].volume, events[0].device_level);
                    // Sent back as after a drag, which starts S271's window.
                    lands(&receiver, db);
                    assert_eq!(receiver.volume_sets().len(), sets + 1, "{name}");
                    // The speaker echoes that set inside the window, as sent
                    // and 0.2 dB off as it rounds: nothing.
                    for echoed in [db, db - 0.2] {
                        let near = RemoteCommand::DeviceVolume(Volume::Db(echoed));
                        remote(handle, &key, None, near);
                        let echo = volume_events_of(handle, &id);
                        assert!(echo.is_empty(), "{name}: {echoed} moved: {echo:?}");
                    }
                    std::thread::sleep(Duration::from_millis(200));
                    let after = receiver.volume_sets();
                    assert_eq!(after.len(), sets + 1, "{name}: echo sent: {after:?}");
                } else if let Some(value) = number(step, "master") {
                    master = value;
                    // SAFETY: a live handle, used by this thread only.
                    unsafe { asc_airplay_set_master(handle, master) };
                    lands(&receiver, db);
                    shown = device_level(own, NO_ZONE_POSITION, master);
                } else if let Some(row) = number(step, "row_move") {
                    own = move_row(handle, &id, row, master);
                    lands(&receiver, db);
                    shown = device_level(own, NO_ZONE_POSITION, master);
                } else {
                    panic!("{name}: no fake for step {step}");
                }
                let row = number(step, "row").expect("row");
                let held = receiver.level().expect("a level on the television");
                assert!(same_db(held, db), "{name}: {step}: television at {held}");
                assert!(
                    (shown - level).abs() < 1e-6,
                    "{name}: {step}: level {shown}"
                );
                assert_eq!(row_level(shown), row, "{name}: {step}: row");
                assert_eq!(row_level(inside(handle).added[&id].level), row, "{name}");
                let own_after = number(step, "own_after").expect("own_after");
                assert!((own - own_after).abs() < 1e-6, "{name}: {step}: own {own}");
            }
            assert!(read_once(handle, &id).is_empty(), "{name}: the echo moved");
            // SAFETY: destroyed once, by this thread.
            unsafe { asc_airplay_destroy(&mut handle) };
        }
    }

    /// The volume events waiting for the speaker `id`, all taken.
    fn volume_events_of(handle: *mut AscAirPlay, id: &str) -> Vec<Event> {
        std::iter::from_fn(|| take_event(handle))
            .filter(|event| event.kind == AIRPLAY_EVENT_VOLUME && event.id == id)
            .collect()
    }

    // covers: S290, S291
    #[test]
    fn k1_keep_a_row_at_40_comes_back_at_40_after_a_restart() {
        let receiver = Loopback::television();
        let id = "02005E005392";
        let c_id = c_string(id);
        let master = 63.0;
        let forty = -18.0;
        let mut handle = on_television(&receiver, id, master, 50.0);
        let own = move_row(handle, id, 40.0, master);
        lands(&receiver, forty);
        let mut rows = read_once(handle, id);

        // Paused and played again: the session stays.
        // SAFETY: a live handle, used by this thread only.
        unsafe {
            asc_airplay_pause(handle);
        }
        assert!(wait_for(|| receiver.methods().contains(&Method::Flush)));
        // SAFETY: as above.
        unsafe { asc_airplay_resume(handle) };
        rows.extend(read_once(handle, id));

        // The speaker turned off and on again, as the host stored it.
        let sets = receiver.volume_sets().len();
        // SAFETY: a live handle, used by this thread only; a C string.
        assert!(unsafe { asc_airplay_remove_speaker(handle, c_id.as_ptr()) });
        let stopped = events_until(handle, |seen| has_state(seen, id, AIRPLAY_STATE_STOPPED));
        assert!(
            has_state(&stopped, id, AIRPLAY_STATE_STOPPED),
            "{stopped:?}"
        );
        // SAFETY: as above.
        assert!(unsafe { asc_airplay_add_speaker(handle, c_id.as_ptr(), own) });
        let playing = events_until(handle, |seen| has_state(seen, id, AIRPLAY_STATE_PLAYING));
        assert!(
            has_state(&playing, id, AIRPLAY_STATE_PLAYING),
            "{playing:?}"
        );
        assert!(wait_for(|| receiver.volume_sets().len() > sets));
        let first = receiver.volume_sets()[sets];
        assert!(
            same_db(first, forty),
            "the first set after the add: {first}"
        );
        rows.extend(read_once(handle, id));

        // The engine stopped and started: a new handle, as the host makes it.
        // SAFETY: destroyed once, by this thread.
        unsafe { asc_airplay_destroy(&mut handle) };
        let sets = receiver.volume_sets().len();
        let mut handle = on_television(&receiver, id, master, own);
        let first = receiver.volume_sets()[sets];
        assert!(
            same_db(first, forty),
            "the first set after a start: {first}"
        );
        rows.extend(read_once(handle, id));

        let after_move = receiver.volume_sets();
        let index = after_move
            .iter()
            .position(|&db| same_db(db, forty))
            .unwrap();
        assert!(
            after_move[index..].iter().all(|&db| same_db(db, forty)),
            "{after_move:?}"
        );
        assert!(
            rows.iter().all(|e| row_level(e.device_level) == 40.0),
            "the row reported another value: {rows:?}"
        );
        assert_eq!(shown_row(own, master), 40.0);
        assert_eq!(row_level(inside(handle).added[id].level), 40.0);
        // SAFETY: destroyed once, by this thread.
        unsafe { asc_airplay_destroy(&mut handle) };
    }

    #[test]
    fn a_volume_read_before_the_host_set_one_is_dropped() {
        let mut handle = create_with(quiet);
        let key = DeviceKey::Id(DeviceId([0x02, 0, 0x5E, 0, 0x53, 0x61]));
        add_for_volume(handle, "02005E005361", key.clone(), 50.0);
        let id = CString::new("02005E005361").expect("no NUL");
        // SAFETY: a live handle, used by this thread only; a NUL-terminated ID.
        unsafe {
            asc_airplay_read_volumes(handle);
            assert!(asc_airplay_set_volume(handle, id.as_ptr(), 70.0));
        }
        report_volume(handle, &key, -15.0);
        assert_eq!(inside(handle).added["02005E005361"].position, 70.0);
        // The master moves every speaker, so a read asked before it is old
        // too.
        // SAFETY: as above.
        unsafe {
            asc_airplay_read_volumes(handle);
            asc_airplay_set_master(handle, 50.0);
        }
        report_volume(handle, &key, -3.0);
        assert!(inside(handle).events.is_empty());
        assert_eq!(inside(handle).added["02005E005361"].position, 70.0);
        // SAFETY: destroyed once, by this thread.
        unsafe { asc_airplay_destroy(&mut handle) };
    }

    #[test]
    fn a_target_reads_its_volume_through_a_member() {
        let mut handle = create_with(quiet);
        let member = DeviceKey::Id(DeviceId([0x02, 0, 0x5E, 0, 0x53, 0x62]));
        let target = TargetId::parse("pair:volume-pair").expect("a target ID");
        let mut added = Added::new(
            DeviceKey::Instance(ServiceKind::AirPlay, "pair:volume-pair".to_owned()),
            40.0,
            Some(target),
            vec!["02005E005362".to_owned()],
            Vec::new(),
        );
        added.volume_set_at = None;
        inside(handle)
            .added
            .insert("pair:volume-pair".to_owned(), added);
        // A stopped target isn't asked (S291).
        // SAFETY: a live handle, used by this thread only.
        unsafe { asc_airplay_read_volumes(handle) };
        assert!(!inside(handle).added["pair:volume-pair"].reading);
        inside(handle)
            .target_states
            .insert("pair:volume-pair".to_owned(), SpeakerState::Playing);
        // SAFETY: as above.
        unsafe { asc_airplay_read_volumes(handle) };
        report_volume(handle, &member, -144.0);
        let events = events_until(handle, |events| !events.is_empty());
        assert_eq!(events.len(), 1, "{events:?}");
        assert_eq!(events[0].kind, AIRPLAY_EVENT_VOLUME);
        assert_eq!(events[0].id, "pair:volume-pair");
        assert_eq!(events[0].volume, 0.0, "muted on the speaker");
        // SAFETY: destroyed once, by this thread.
        unsafe { asc_airplay_destroy(&mut handle) };
    }

    #[test]
    fn warnings_reach_the_host_in_order_with_their_strings() {
        let mut handle = create_with(quiet);
        let key = DeviceKey::Id(DeviceId([0x02, 0, 0x5E, 0, 0x53, 0x63]));
        add_for_volume(handle, "02005E005363", key.clone(), 50.0);
        let warn = |warning: Warning| {
            inside(handle).take_in(SenderEvent::Speaker {
                key: key.clone(),
                event: SpeakerEvent::Warning(warning),
            });
        };
        warn(Warning::PtpUnavailable);
        inside(handle).take_in(SenderEvent::Speaker {
            key: key.clone(),
            event: SpeakerEvent::State(SpeakerState::Connecting),
        });
        warn(Warning::RequestFailed {
            method: Method::GetParameter,
            status: 501,
        });
        warn(Warning::PtpOutsideFamily);
        warn(Warning::JackDisconnected);
        inside(handle).take_in(SenderEvent::Speaker {
            key: key.clone(),
            event: SpeakerEvent::PtpOpened,
        });
        let mut kinds = Vec::new();
        let mut raw = blank_event();
        // SAFETY: a live handle, used by this thread only; a local out
        // struct, whose strings are read before the next call.
        while unsafe { asc_airplay_next_event(handle, &mut raw) } {
            // The live sender's timeline can post at any moment; only the
            // speaker's own events are under test.
            if raw.kind != AIRPLAY_EVENT_TIMELINE {
                kinds.push((raw.kind, raw.warning, text(raw.id), text(raw.detail)));
            }
        }
        let id = "02005E005363".to_owned();
        assert_eq!(
            kinds,
            [
                (
                    AIRPLAY_EVENT_WARNING,
                    AIRPLAY_WARNING_PTP_PORTS_TAKEN,
                    id.clone(),
                    "PtpUnavailable".to_owned()
                ),
                (AIRPLAY_EVENT_STATE, 0, id.clone(), String::new()),
                (
                    AIRPLAY_EVENT_WARNING,
                    AIRPLAY_WARNING_REQUEST_FAILED,
                    id.clone(),
                    format!(
                        "{:?}",
                        Warning::RequestFailed {
                            method: Method::GetParameter,
                            status: 501
                        }
                    )
                ),
                (
                    AIRPLAY_EVENT_WARNING,
                    AIRPLAY_WARNING_PTP_OUTSIDE_FAMILY,
                    id.clone(),
                    "PtpOutsideFamily".to_owned()
                ),
                (
                    AIRPLAY_EVENT_WARNING,
                    AIRPLAY_WARNING_JACK_DISCONNECTED,
                    id.clone(),
                    "JackDisconnected".to_owned()
                ),
                (
                    AIRPLAY_EVENT_WARNING,
                    AIRPLAY_WARNING_PTP_FREE,
                    id.clone(),
                    "PtpOpened".to_owned()
                ),
            ]
        );
        // It turned a volume read down: it isn't asked again.
        // SAFETY: as above.
        unsafe { asc_airplay_read_volumes(handle) };
        assert!(!inside(handle).added["02005E005363"].reading);
        // SAFETY: destroyed once, by this thread.
        unsafe { asc_airplay_destroy(&mut handle) };
    }

    #[test]
    fn an_airplay2_pin_pairing_stores_its_credentials_and_reports_paired() {
        let folder = scratch("airplay2-identity-pin");
        let path = folder.join("identity.dat");
        let c_path = CString::new(path.to_str().expect("UTF-8")).expect("no NUL");
        let mut handle = create_with(quiet);
        inside(handle).airplay2 = true;
        // SAFETY: a live handle, used by this thread only; the path is a
        // NUL-terminated string.
        unsafe {
            assert_eq!(
                asc_airplay_open_airplay2_identity(handle, c_path.as_ptr()),
                0
            );
        }
        let mut blob = vec![1u8];
        blob.extend_from_slice(b"01234567-89AB-CDEF-0123-456789ABCDEF");
        blob.extend_from_slice(&[9; 64]);
        blob.extend_from_slice(b"receiver");
        let credentials = HomeKitCredentials::from_bytes(&blob).expect("a credential blob");
        let id = "02005E005352";
        let key = DeviceKey::Id(DeviceId([0x02, 0, 0x5E, 0, 0x53, 0x52]));
        inside(handle).pairing.insert(id.to_owned(), key.clone());
        inside(handle).take_in(SenderEvent::Pairing {
            key,
            event: PairingEvent::HomeKitPaired(credentials.clone()),
        });
        let events = events_until(handle, |events| {
            events.iter().any(|e| e.kind == AIRPLAY_EVENT_PAIRED)
        });
        assert_eq!(
            pairing_events(&events),
            vec![(AIRPLAY_EVENT_PAIRED, id.to_owned(), 0)]
        );
        assert!(inside(handle).pairing.is_empty());
        assert_eq!(inside(handle).identity.get(id), Some(&credentials));
        // SAFETY: destroyed once, by this thread.
        unsafe { asc_airplay_destroy(&mut handle) };
        let (again, _) = IdentityStore::open(path);
        assert_eq!(again.get(id), Some(&credentials));
    }

    #[test]
    fn an_airplay2_pin_pairing_reads_as_paired_and_forget_removes_it() {
        let mut handle = create_with(quiet);
        inside(handle).airplay2 = true;
        let mut record = airplay2_only();
        record.txt.push(b"flags=0x200".to_vec());
        let device =
            Device::from_records(record.device_key(), None, Some(&record)).expect("a device");
        let id = receiver_id(&device.key);
        let mut info = ReceiverInfo::new(device);
        // Planned as the sender plans it with the AirPlay 2 switch on.
        info.plan = Ok(());
        inside(handle).listed.insert(id.clone(), info.clone());
        let report = inside(handle).report(ReceiverEvent::Changed(Box::new(info.clone())));
        assert_eq!(
            report.availability, AIRPLAY_NEEDS_PIN,
            "reached over AirPlay 2, it pairs with a PIN, as on Windows"
        );
        assert_eq!(
            report.flags & AIRPLAY_FLAG_CODE_EVERY_TIME,
            0,
            "it asks once, so auto-connect may turn it on"
        );

        let mut blob = vec![1u8];
        blob.extend_from_slice(b"01234567-89AB-CDEF-0123-456789ABCDEF");
        blob.extend_from_slice(&[7; 64]);
        blob.extend_from_slice(b"receiver");
        let credentials = HomeKitCredentials::from_bytes(&blob).expect("a credential blob");
        let _ = inside(handle).identity.insert(&id, credentials);
        let report = inside(handle).report(ReceiverEvent::Changed(Box::new(info.clone())));
        assert_eq!(
            report.availability, AIRPLAY_AVAILABLE,
            "paired on AirPlay 2"
        );

        let c_id = CString::new(id.clone()).expect("no NUL");
        // SAFETY: a live handle, used by this thread only; a local C string.
        unsafe {
            assert!(asc_airplay_forget_pairing(handle, c_id.as_ptr()));
            assert!(!asc_airplay_forget_pairing(handle, c_id.as_ptr()));
        }
        assert_eq!(inside(handle).identity.get(&id), None);
        let report = inside(handle).report(ReceiverEvent::Changed(Box::new(info)));
        assert_eq!(report.availability, AIRPLAY_NEEDS_PIN);
        assert_eq!(report.flags & AIRPLAY_FLAG_PAIRED, 0);
        // SAFETY: destroyed once, by this thread.
        unsafe { asc_airplay_destroy(&mut handle) };
    }

    #[test]
    fn a_receiver_that_asks_every_time_never_reads_as_paired() {
        let mut handle = create_with(quiet);
        inside(handle).airplay2 = true;
        let mut record = airplay2_only();
        record.txt.push(b"flags=0x4c".to_vec());
        let device =
            Device::from_records(record.device_key(), None, Some(&record)).expect("a device");
        let id = receiver_id(&device.key);
        let key = device.key.clone();
        let mut info = ReceiverInfo::new(device);
        info.plan = Ok(());
        inside(handle).listed.insert(id.clone(), info.clone());
        // Credentials kept from before the TV was set to ask every time.
        let mut blob = vec![1u8];
        blob.extend_from_slice(b"01234567-89AB-CDEF-0123-456789ABCDEF");
        blob.extend_from_slice(&[7; 64]);
        blob.extend_from_slice(b"receiver");
        let credentials = HomeKitCredentials::from_bytes(&blob).expect("a credential blob");
        let _ = inside(handle).identity.insert(&id, credentials.clone());
        let report = inside(handle).report(ReceiverEvent::Changed(Box::new(info.clone())));
        assert_eq!(report.availability, AIRPLAY_NEEDS_PIN);
        assert_eq!(report.flags & AIRPLAY_FLAG_PAIRED, 0);
        assert_ne!(report.flags & AIRPLAY_FLAG_CODE_EVERY_TIME, 0);

        // A PIN pairing stores nothing and keeps the stored keys; the
        // receiver can play until its session ends.
        inside(handle).pairing.insert(id.clone(), key.clone());
        inside(handle).take_in(SenderEvent::Pairing {
            key: key.clone(),
            event: PairingEvent::PairedForSession,
        });
        let relisted = next(handle, 1);
        assert_eq!(relisted.len(), 1);
        assert_eq!(relisted[0].availability, AIRPLAY_AVAILABLE);
        assert_eq!(relisted[0].flags & AIRPLAY_FLAG_PAIRED, 0);
        assert_ne!(
            relisted[0].flags & AIRPLAY_FLAG_CODE_EVERY_TIME,
            0,
            "paired for this session, it still asks next time"
        );
        let events = events_until(handle, |events| {
            events.iter().any(|e| e.kind == AIRPLAY_EVENT_PAIRED)
        });
        assert_eq!(
            pairing_events(&events),
            vec![(AIRPLAY_EVENT_PAIRED, id.clone(), 0)]
        );
        assert_eq!(
            inside(handle).identity.get(&id),
            Some(&credentials),
            "an answer during the pairing proves nothing about them"
        );
        inside(handle).take_in(SenderEvent::Speaker {
            key,
            event: SpeakerEvent::Error(SpeakerError {
                error: audiosync_airplay::error::SessionError::new(ErrorKind::PairingRequired),
                reason: UserReason::PairingRequired,
                retry_at: None,
            }),
        });
        let relisted = next(handle, 1);
        assert_eq!(relisted.len(), 1, "the session ended");
        assert_eq!(relisted[0].availability, AIRPLAY_NEEDS_PIN);
        // SAFETY: destroyed once, by this thread.
        unsafe { asc_airplay_destroy(&mut handle) };
    }

    #[test]
    fn forget_reaches_the_senders_airplay2_credentials_of_a_receiver_no_longer_listed() {
        let mut handle = create_with(quiet);
        inside(handle).airplay2 = true;
        let mut blob = vec![1u8];
        blob.extend_from_slice(b"01234567-89AB-CDEF-0123-456789ABCDEF");
        blob.extend_from_slice(&[5; 64]);
        blob.extend_from_slice(b"receiver");
        let credentials = HomeKitCredentials::from_bytes(&blob).expect("a credential blob");
        let id = "02005E005353";
        let key = DeviceKey::Id(DeviceId([0x02, 0, 0x5E, 0, 0x53, 0x53]));
        inside(handle).pairing.insert(id.to_owned(), key.clone());
        inside(handle).take_in(SenderEvent::Pairing {
            key: key.clone(),
            event: PairingEvent::HomeKitPaired(credentials),
        });
        inside(handle).take_in(SenderEvent::Receiver(ReceiverEvent::Gone(key.clone())));
        assert!(!inside(handle).listed.contains_key(id) && !inside(handle).added.contains_key(id));
        assert_eq!(
            inside(handle).homekit_key(id),
            Some(key),
            "the sender keeps the credentials it paired with after the receiver is gone"
        );
        let c_id = CString::new(id).expect("no NUL");
        // SAFETY: a live handle, used by this thread only; a local C string.
        unsafe {
            assert!(asc_airplay_forget_pairing(handle, c_id.as_ptr()));
        }
        assert_eq!(inside(handle).identity.get(id), None);
        // SAFETY: destroyed once, by this thread.
        unsafe { asc_airplay_destroy(&mut handle) };
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
        }
        // Turned down, it says why before asking again, in the debug log only.
        let key = inside(handle).listed[id].device.key.clone();
        for event in [
            PairingEvent::PinTurnedDown("error 2 (authentication) in answer to M3".to_owned()),
            PairingEvent::PinRequested,
        ] {
            inside(handle).take_in(SenderEvent::Pairing {
                key: key.clone(),
                event,
            });
        }
        let events = events_until(handle, |events| {
            events.iter().any(|e| e.kind == AIRPLAY_EVENT_PIN_NEEDED)
        });
        let lines: Vec<_> = events
            .iter()
            .map(|e| (e.kind, e.id.as_str(), e.detail.as_str()))
            .collect();
        let why = (
            AIRPLAY_EVENT_TRACE,
            id,
            "pairing: error 2 (authentication) in answer to M3",
        );
        let prompt = (AIRPLAY_EVENT_PIN_NEEDED, id, "");
        if debug_trace_on() {
            assert_eq!(lines, [why, prompt]);
        } else {
            assert_eq!(lines, [prompt]);
        }
        // SAFETY: as above.
        unsafe {
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

        // A device at its address that can't sign as it: the user is told,
        // and the credentials stay, so it can't wipe the pairing.
        inside(handle).take_in(SenderEvent::Speaker {
            key: key.clone(),
            event: SpeakerEvent::Error(SpeakerError {
                error: audiosync_airplay::error::SessionError::new(ErrorKind::ReceiverUnverified),
                reason: UserReason::ReceiverUnverified,
                retry_at: None,
            }),
        });
        let events = events_until(handle, |events| {
            events.iter().any(|e| e.kind == AIRPLAY_EVENT_ERROR)
        });
        assert!(
            events
                .iter()
                .any(|e| e.kind == AIRPLAY_EVENT_ERROR
                    && e.error == AIRPLAY_ERROR_RECEIVER_UNVERIFIED)
        );
        assert!(inside(handle).store.contains(id), "kept");

        // The Apple TV was reset, or something at its address answers in
        // its place: it turns the credentials down, and they stay.
        inside(handle).take_in(SenderEvent::Speaker {
            key,
            event: SpeakerEvent::Error(SpeakerError {
                error: audiosync_airplay::error::SessionError::new(ErrorKind::PairingRequired),
                reason: UserReason::PairingRequired,
                retry_at: None,
            }),
        });
        let seen = next(handle, 1);
        assert_eq!(seen[0].availability, AIRPLAY_NEEDS_PIN, "asks for its PIN");
        assert!(inside(handle).store.contains(id), "kept");
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
                volume: raw.volume,
                warning: raw.warning,
                remote: raw.remote,
                receiver_latency_us: raw.receiver_latency_us,
                receiver_latency_sources: raw.receiver_latency_sources,
                device_level: raw.device_level,
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
    fn pair_member(last: u8, flags: &str) -> Device {
        let mut record = airplay2_only();
        record.instance = format!("HomePod {last}");
        record.txt = vec![
            format!("deviceid=02:00:5E:00:53:{last:02X}").into_bytes(),
            b"features=0x0,0x40".to_vec(),
            b"model=AudioAccessory5,1".to_vec(),
            format!("flags={flags}").into_bytes(),
            b"tsid=A1B2C3D4-0000-4000-8000-00000000AAAA".to_vec(),
            b"gpn=Kitchen".to_vec(),
        ];
        Device::from_records(record.device_key(), None, Some(&record)).expect("a device")
    }

    #[test]
    fn a_target_named_in_the_pcm_variable_gives_every_member_pcm() {
        let mut handle = create_with(quiet);
        let (left, right) = (pair_member(0x31, "0x2004"), pair_member(0x32, "0x4"));
        let found = audiosync_airplay::targets::find_targets([&left, &right]);
        let pair = found.first().cloned().expect("a pair");
        let pair_id = pair.id.to_string();
        let member_ids = format!("{}, {}", receiver_id(&left.key), receiver_id(&right.key));
        for (list, pcm) in [
            (format!("{pair_id}, 0011223344AA"), true),
            (member_ids, false),
            (String::new(), false),
        ] {
            let airplay = inside(handle);
            let options = target_options(airplay, &pair_id, 40.0, Some(&list));
            let members = audiosync_airplay::sender::member_options(
                &pair,
                &options,
                member_credentials(airplay),
            );
            let keys: Vec<_> = members.iter().map(|(key, _)| key.clone()).collect();
            assert_eq!(keys, [left.key.clone(), right.key.clone()], "{list}");
            assert!(members.iter().all(|(_, own)| own.pcm == pcm), "{list}");
        }
        // SAFETY: destroyed once, by this thread.
        unsafe { asc_airplay_destroy(&mut handle) };
    }

    #[test]
    fn a_stereo_pair_is_listed_and_added_as_one_target_only_with_the_switch_on() {
        let mut handle = create_with(quiet);
        let (left, right) = (pair_member(0x11, "0x2004"), pair_member(0x12, "0x4"));
        let found = audiosync_airplay::targets::find_targets([&left, &right]);
        let pair = found.first().cloned().expect("a pair");
        let pair_id = pair.id.to_string();
        let mut member = ReceiverInfo::new(left.clone());
        member.target = Some(pair.id.clone());
        let updated = || {
            SenderEvent::Receiver(ReceiverEvent::Target(TargetChange::Updated(Box::new(
                pair.clone(),
            ))))
        };

        inside(handle).take_in(updated());
        assert!(inside(handle).targets.is_empty(), "the switch is off");
        let report = inside(handle).report(ReceiverEvent::Changed(Box::new(member.clone())));
        assert_eq!(text(report.target), "");

        inside(handle).airplay2 = true;
        inside(handle).take_in(updated());
        let queued = inside(handle).receivers.pop_back().expect("queued");
        let report = inside(handle).report(queued);
        assert_eq!(report.event, AIRPLAY_APPEARED);
        assert_eq!(text(report.id), pair_id);
        assert_eq!(text(report.name), "Kitchen");
        assert_ne!(report.flags & AIRPLAY_FLAG_TARGET, 0);
        assert_eq!(report.flags & AIRPLAY_FLAG_INCOMPLETE, 0);
        assert_eq!(text(report.target), "");
        let report = inside(handle).report(ReceiverEvent::Target(TargetChange::Updated(Box::new(
            pair.clone(),
        ))));
        assert_eq!(report.event, AIRPLAY_CHANGED);
        let report = inside(handle).report(ReceiverEvent::Changed(Box::new(member)));
        assert_eq!(text(report.target), pair_id);

        let c_id = CString::new(pair_id.clone()).expect("an ID");
        // SAFETY: a live handle and a NUL-terminated ID.
        unsafe {
            assert!(asc_airplay_add_speaker(handle, c_id.as_ptr(), 40.0));
        }
        for (key, state) in [
            (&left.key, SpeakerState::Connecting),
            (&right.key, SpeakerState::Playing),
            (&left.key, SpeakerState::Playing),
        ] {
            let event = SpeakerEvent::State(state);
            inside(handle).take_in(SenderEvent::Speaker {
                key: key.clone(),
                event,
            });
        }
        let states: Vec<_> = inside(handle)
            .events
            .iter()
            .filter_map(|(_, pending)| match pending {
                Pending::Speaker(id, SpeakerEvent::State(state)) if *id == pair_id => Some(*state),
                _ => None,
            })
            .collect();
        assert_eq!(states, [SpeakerState::Connecting, SpeakerState::Playing]);
        inside(handle).take_in(SenderEvent::TargetMissing {
            id: pair.id.clone(),
            missing: vec![left.key.clone()],
        });
        let queued = inside(handle).receivers.pop_back().expect("queued");
        let report = inside(handle).report(queued);
        assert_ne!(report.flags & AIRPLAY_FLAG_INCOMPLETE, 0);
        // SAFETY: a live handle and a NUL-terminated ID.
        unsafe {
            assert!(asc_airplay_set_volume(handle, c_id.as_ptr(), 60.0));
            assert!(asc_airplay_remove_speaker(handle, c_id.as_ptr()));
        }
        inside(handle).take_in(SenderEvent::Receiver(ReceiverEvent::Target(
            TargetChange::Gone(pair.id.clone()),
        )));
        assert!(inside(handle).targets.is_empty());
        let queued = inside(handle).receivers.pop_back().expect("queued");
        let report = inside(handle).report(queued);
        assert_eq!(report.event, AIRPLAY_GONE);
        assert_eq!(text(report.id), pair_id);
        // SAFETY: the handle is live and used by no other call.
        unsafe { asc_airplay_destroy(&mut handle) };
    }
}
