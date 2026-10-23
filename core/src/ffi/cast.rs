//! AudioSync's own Google Cast sender ([`audiosync_cast`]) through the C
//! interface, for the macOS app, which reaches it through the core's
//! static library the way it reaches the AirPlay sender.
//!
//! The host creates a handle with the device memory it stored last time,
//! starts browsing, and takes the devices discovery finds, one change at a
//! time, by their ID: the `id` TXT value in hex, or `manual:<address>` for a
//! device the user added by address (C18). It starts one device at a time
//! (C2) by that ID and gets an attempt number back; everything that happens
//! to the attempt arrives as events carrying that number, and every attempt
//! ends with exactly one [`CAST_EVENT_ENDED`]. A Cast device is a
//! measured-offset output (C73, C77): the host delays its other outputs by
//! the offset it stores here per device, and Resync is a user action (C79).
//!
//! Threads: every `asc_cast_*` call on a handle runs on one control thread at
//! a time, never an audio thread. None of them waits on the network: the
//! sender's worker thread makes every network call (C8). The one call that
//! waits is [`asc_cast_destroy`], for the teardown of C88, at most about
//! 3.5 s. An input handle's write runs on one capture thread at a time, its
//! rate and fade-out requests on any thread, and its destroy once no write
//! can run; it may outlive its sender, whose audio then goes nowhere.
//!
//! Panics: as in the AirPlay interface, a panic inside a call returns that
//! call's failure value and makes the handle dead, as does a panic that ends
//! the sender's worker thread. A dead handle's calls do nothing, except that
//! [`asc_cast_next_event`] reports [`CAST_EVENT_FAILED`] once, with where
//! the panic happened; the host then destroys it and may create another.

use std::collections::VecDeque;
use std::ffi::{CStr, CString, c_char};
use std::net::{IpAddr, SocketAddr};
use std::panic::{self, AssertUnwindSafe};
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError};
use std::time::Duration;

use audiosync_cast::audio::{AudioWriter, Codec, audio_ring};
use audiosync_cast::discovery::mdns::MdnsBrowser;
use audiosync_cast::discovery::{
    BrowseEvent, DEVICE_PORT, DeviceId, DeviceKey, DeviceKind, DeviceList, ListEvent,
};
use audiosync_cast::error::ConnectError;
use audiosync_cast::group::{GroupMember, GroupRecords};
use audiosync_cast::host::{
    AttemptId, CastHost, CastTarget, CommandError, DeviceMemory, HostEvent, StartRefused,
};
use audiosync_cast::media::{MediaError, Metadata, VolumeError};
use audiosync_cast::session::{SessionEnd, SessionError, SessionEvent};
use audiosync_cast::status::{self, ControlType};
use audiosync_cast::stream::StreamError;

use super::airplay::{LATEST_PANIC, PANICS, caught_panic_location, note_panic_locations};
use super::{host_slice, stereo_samples};
use crate::delay_line::DelayLine;
use crate::fade::InputFade;
use crate::policy::delay::MAX_HOLD_BACK_MS;
use crate::resample::Resampler;
use crate::ring::is_supported_rate;

/// The rate of every Cast stream from this interface; the input resamples
/// the capture to it (C63 allows 44 100 and 48 000 Hz).
pub const CAST_RATE: u32 = 48_000;

/// What a device change reports ([`AscCastDevice::event`]): discovery found
/// a device, or the host added one by address.
pub const CAST_APPEARED: i32 = 1;
/// Its records changed, or it went offline during a session (C17).
pub const CAST_CHANGED: i32 = 2;
/// It left the list.
pub const CAST_GONE: i32 = 3;

/// What a device is ([`AscCastDevice::flags`]), bit by bit: a Cast group,
/// which plays as one device with its own volume and offset (C3, C14).
pub const CAST_FLAG_GROUP: u32 = 1;
/// Its records went away while a session with it runs (C17).
pub const CAST_FLAG_OFFLINE: u32 = 2;
/// Added by address; no browse has found its `id` yet (C18).
pub const CAST_FLAG_MANUAL: u32 = 4;

/// Why [`asc_cast_start`] refused: it didn't.
pub const CAST_REFUSED_NONE: i32 = 0;
/// Another Cast device plays or starts (C2).
pub const CAST_REFUSED_BUSY: i32 = 1;
/// The ID is not a listed device.
pub const CAST_REFUSED_UNKNOWN: i32 = 2;
/// No longer reported: a Cast group starts like a device (C3). Kept so
/// hosts built against it still compile.
pub const CAST_REFUSED_GROUP: i32 = 3;
/// The handle is dead or its worker gone.
pub const CAST_REFUSED_CLOSED: i32 = 4;
/// A null or non-UTF-8 ID.
pub const CAST_REFUSED_INVALID: i32 = 5;

/// An event's kind ([`AscCastEvent::kind`]): the device plays the stream in
/// `format`.
pub const CAST_EVENT_STARTED: i32 = 1;
/// The attempt is over, its last event; `reason` says how, and with
/// [`CAST_END_FAILED`] `error` says why.
pub const CAST_EVENT_ENDED: i32 = 2;
/// The device reports `PLAYING` (C49).
pub const CAST_EVENT_PLAYING: i32 = 3;
/// A connect attempt failed; the next one follows in `retry_ms` (C80).
pub const CAST_EVENT_CONNECT_FAILED: i32 = 4;
/// The channel failed and the sender reconnects (C81).
pub const CAST_EVENT_RECOVERING: i32 = 5;
/// The session carries on; `relaunched` when the app was loaded again.
pub const CAST_EVENT_RECOVERED: i32 = 6;
/// The device saw the stream end and the sender loaded it again (C86).
pub const CAST_EVENT_RELOADED: i32 = 7;
/// The receiver ran dry (C51).
pub const CAST_EVENT_REBUFFERING: i32 = 8;
/// The device refused WAV; the sender loads `format` instead (C84).
pub const CAST_EVENT_FORMAT_FALLBACK: i32 = 9;
/// The device's volume as it reports it (C69, C70).
pub const CAST_EVENT_VOLUME: i32 = 10;
/// Once a second while playing, for the debug log (C78).
pub const CAST_EVENT_DIAGNOSTICS: i32 = 11;
/// The stream server's log line, in `detail`.
pub const CAST_EVENT_STREAM: i32 = 12;
/// A volume, artwork or Resync request failed; `error` says why.
pub const CAST_EVENT_COMMAND_FAILED: i32 = 13;
/// The device memory changed: store [`asc_cast_memory_json`].
pub const CAST_EVENT_MEMORY_CHANGED: i32 = 14;
/// Discovery's mDNS daemon ended; its devices were reported gone.
pub const CAST_EVENT_DISCOVERY_STOPPED: i32 = 15;
/// The handle is dead (see the module's Panics).
pub const CAST_EVENT_FAILED: i32 = 16;
/// The playing group's member list changed: read it with
/// [`asc_cast_member_count`] and [`asc_cast_member`] (C94, C95).
pub const CAST_EVENT_MEMBERS: i32 = 17;
/// A line about a Cast group for the debug log, in `detail`: what a group
/// announces (C89), or a multizone message the sender doesn't act on (C96).
pub const CAST_EVENT_GROUP_LOG: i32 = 18;

/// How an attempt ended ([`AscCastEvent::reason`]): the sender's own
/// teardown (C87).
pub const CAST_END_STOPPED: i32 = 1;
/// Someone else stopped or replaced the app or the stream (C43, C86).
pub const CAST_END_TAKEN_OVER: i32 = 2;
/// The host stopped it before it started.
pub const CAST_END_DESELECTED: i32 = 3;
/// A failure the session could not recover from.
pub const CAST_END_FAILED: i32 = 4;

/// Why something failed ([`AscCastEvent::error`]): it didn't.
pub const CAST_ERROR_NONE: i32 = 0;
/// No TCP connection, a TLS failure, or no status in time (C80).
pub const CAST_ERROR_CONNECT: i32 = 1;
/// The Default Media Receiver did not launch (C41).
pub const CAST_ERROR_LAUNCH: i32 = 2;
/// The device refused or stopped the stream (C83, C86).
pub const CAST_ERROR_MEDIA: i32 = 3;
/// The device never fetched the stream; a firewall may block it (C56, C85).
pub const CAST_ERROR_NO_FETCH: i32 = 4;
/// The stream server could not start (C56, C57).
pub const CAST_ERROR_STREAM: i32 = 5;
/// No session runs.
pub const CAST_ERROR_NO_SESSION: i32 = 6;
/// The device's volume is locked (C68).
pub const CAST_ERROR_VOLUME_FIXED: i32 = 7;
/// The device was not ready for the volume change, or has no step (C69).
pub const CAST_ERROR_VOLUME: i32 = 8;
/// Artwork that is not PNG or JPEG, or over 1280 by 720 (C53).
pub const CAST_ERROR_ARTWORK: i32 = 9;
/// No longer reported: a Cast group plays like a device (C3). Kept so
/// hosts built against it still compile.
pub const CAST_ERROR_GROUP: i32 = 10;

/// A stream's format ([`AscCastEvent::format`]): live WAV.
pub const CAST_FORMAT_WAV: i32 = 1;
/// FLAC, the fallback (C84).
pub const CAST_FORMAT_FLAC: i32 = 2;

/// Frames each session's ring holds between the capture and the stream
/// server: one second at [`CAST_RATE`].
const RING_FRAMES: usize = CAST_RATE as usize;
/// Output frames the input resamples into per step; bounds its buffer.
const INPUT_CHUNK_FRAMES: usize = 4096;
/// The longest hold-back of the feed, in frames at [`CAST_RATE`]: the
/// ceiling of L.
const MAX_HOLD_BACK_FRAMES: usize = MAX_HOLD_BACK_MS as usize * CAST_RATE as usize / 1000;
/// The largest artwork [`asc_cast_set_artwork`] keeps.
const MAX_ARTWORK_BYTES: usize = 8 << 20;
/// The largest offset [`asc_cast_set_offset`] stores: C74 reports latencies
/// beyond 30 s.
const MAX_OFFSET_MS: f64 = 120_000.0;

/// One device change. The strings are NUL-terminated UTF-8 owned by the
/// handle, valid until the next `asc_cast_next_device` call on it or its
/// destroy; a gone device has only its ID.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct AscCastDevice {
    pub event: i32,
    /// 32 hex digits, or `manual:<address>`.
    pub id: *const c_char,
    /// The `fn` TXT value; "" when it has none.
    pub name: *const c_char,
    /// The `md` TXT value; "" when it has none.
    pub model: *const c_char,
    pub flags: u32,
    /// On the [`CAST_APPEARED`] of a device a browse found that was added
    /// by address, the `manual:<address>` ID it was listed under, so the
    /// host moves what it kept for it (C18); "" otherwise.
    pub former: *const c_char,
}

/// One member of the playing group (C94, C95). The strings are
/// NUL-terminated UTF-8 owned by the handle, valid until the next
/// `asc_cast_member` call on it or its destroy.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct AscCastMember {
    /// The listed device it is, as [`AscCastDevice::id`] names it; "" when
    /// no discovered device matches, and the host shows the name alone.
    pub id: *const c_char,
    /// The name the group gives it; "" when none.
    pub name: *const c_char,
}

/// One event. The strings are NUL-terminated UTF-8 owned by the handle,
/// valid until the next `asc_cast_next_event` call on it or its destroy.
/// Fields a kind doesn't use are 0, "" or, for `retry_ms`, -1.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct AscCastEvent {
    pub kind: i32,
    /// What [`asc_cast_start`] returned for the attempt; 0 for none.
    pub attempt: u64,
    /// The attempt's device ID; "" for none.
    pub id: *const c_char,
    /// An ended event's `CAST_END_*`.
    pub reason: i32,
    /// A `CAST_ERROR_*`, for ended, connect-failed and command-failed events.
    pub error: i32,
    /// A started or fallback event's `CAST_FORMAT_*`.
    pub format: i32,
    pub retry_ms: i64,
    /// A volume event's level, 0 to 1.
    pub level: f64,
    pub muted: bool,
    /// The device's volume can't be changed (C68).
    pub volume_fixed: bool,
    /// The device's volume step; 0 when it reports none (C69).
    pub step: f64,
    /// Diagnostics: dropped audio plus rebuffering since the `LOAD` (C79).
    pub drift_ms: f64,
    /// Diagnostics: audio waiting in the sender's queue (C64).
    pub queued_ms: f64,
    /// Diagnostics: the drift is over 250 ms; the host may offer Resync.
    pub resync_offered: bool,
    pub relaunched: bool,
    /// For the debug log: an error's text, a log line, a panic location.
    pub detail: *const c_char,
}

/// Where discovery's events come from; a test supplies its own.
trait Browse {
    fn poll_event(&mut self) -> Option<BrowseEvent>;
    fn is_dead(&self) -> bool;
}

impl Browse for MdnsBrowser {
    fn poll_event(&mut self) -> Option<BrowseEvent> {
        MdnsBrowser::poll_event(self)
    }

    fn is_dead(&self) -> bool {
        MdnsBrowser::is_dead(self)
    }
}

type OpenBrowser = fn() -> Option<Box<dyn Browse>>;

/// Live multicast DNS (C10). The core's tests never browse the real
/// network, whose sockets make the firewall ask the user; there it fails.
fn live_browser() -> Option<Box<dyn Browse>> {
    #[cfg(not(test))]
    let browser = MdnsBrowser::new().ok();
    #[cfg(test)]
    let browser = None::<MdnsBrowser>;
    Some(Box::new(browser?))
}

/// An event waiting for the host, with its strings.
#[derive(Clone, Debug, PartialEq)]
struct Pending {
    kind: i32,
    attempt: u64,
    id: String,
    reason: i32,
    error: i32,
    format: i32,
    retry_ms: i64,
    level: f64,
    muted: bool,
    volume_fixed: bool,
    step: f64,
    drift_ms: f64,
    queued_ms: f64,
    resync_offered: bool,
    relaunched: bool,
    detail: String,
}

impl Pending {
    fn new(kind: i32) -> Pending {
        Pending {
            kind,
            attempt: 0,
            id: String::new(),
            reason: 0,
            error: CAST_ERROR_NONE,
            format: 0,
            retry_ms: -1,
            level: 0.0,
            muted: false,
            volume_fixed: false,
            step: 0.0,
            drift_ms: 0.0,
            queued_ms: 0.0,
            resync_offered: false,
            relaunched: false,
            detail: String::new(),
        }
    }
}

/// A device change waiting for the host.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Change {
    event: i32,
    id: String,
    name: String,
    model: String,
    flags: u32,
    former: String,
}

/// An attempt whose `Ended` hasn't come yet.
struct Attempt {
    attempt: AttemptId,
    key: DeviceKey,
    id: String,
    /// A group's addresses, kept current from discovery (C90, C103).
    group: Option<GroupRecords>,
}

/// What the handle and its input share: the writing end of the running
/// session's ring, and the handle's death.
struct Shared {
    /// The ring the input writes into; null when no session takes audio.
    writer: AtomicPtr<AudioWriter>,
    /// A write runs; a writer swapped out is freed only once this was seen
    /// false after the swap.
    writing: AtomicBool,
    /// Writers swapped out that a write may still use.
    retired: Mutex<Vec<Retired>>,
    dead: AtomicBool,
    /// Where the first panic a call caught happened (`<file>:<line>`).
    at: OnceLock<String>,
    /// How many frames the input holds its audio back
    /// ([`asc_cast_set_hold_back`]).
    hold_back: AtomicUsize,
}

/// A writer from `Box::into_raw` that [`Shared::attach`] swapped out.
struct Retired(*mut AudioWriter);

// SAFETY: it owns its writer, whose ring is all atomics, and is touched by
// one control thread at a time.
unsafe impl Send for Retired {}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Shared {
    fn new() -> Shared {
        Shared {
            writer: AtomicPtr::new(ptr::null_mut()),
            writing: AtomicBool::new(false),
            retired: Mutex::new(Vec::new()),
            dead: AtomicBool::new(false),
            at: OnceLock::new(),
            hold_back: AtomicUsize::new(0),
        }
    }

    /// Control thread: the input writes into `writer` from its next write
    /// on, or nowhere.
    fn attach(&self, writer: Option<AudioWriter>) {
        let new = writer.map_or(ptr::null_mut(), |writer| Box::into_raw(Box::new(writer)));
        let old = self.writer.swap(new, Ordering::SeqCst);
        if !old.is_null() {
            lock(&self.retired).push(Retired(old));
        }
        self.reclaim();
    }

    /// Control thread: frees the retired writers once no write runs. A
    /// write that starts after this load reads the pointer after the swap
    /// (all four operations are SeqCst), so it can't hold a retired one.
    fn reclaim(&self) {
        if self.writing.load(Ordering::SeqCst) {
            return;
        }
        for Retired(writer) in lock(&self.retired).drain(..) {
            // SAFETY: from Box::into_raw in attach, swapped out before the
            // load above found no write running, so nothing uses it.
            drop(unsafe { Box::from_raw(writer) });
        }
    }

    /// Capture thread, inside a write: hands `samples` at [`CAST_RATE`] to
    /// the running session's ring; returns the frames it took. Atomics only.
    fn write(&self, samples: &[f32]) -> usize {
        let writer = self.writer.load(Ordering::SeqCst);
        if writer.is_null() {
            return 0;
        }
        // SAFETY: a write runs (`writing` is set), so attach and reclaim
        // free no writer this load can return.
        unsafe { &*writer }.write_f32(samples)
    }

    /// A call on this thread caught a panic: the handle is dead, and the
    /// location the panic hook noted for it is kept.
    fn panicked(&self) {
        if let Some(at) = caught_panic_location() {
            let _ = self.at.set(at);
        }
        self.dead.store(true, Ordering::Release);
    }
}

impl Drop for Shared {
    fn drop(&mut self) {
        // The handle and the input are both gone: no write can run.
        let writer = *self.writer.get_mut();
        let retired = self
            .retired
            .get_mut()
            .unwrap_or_else(PoisonError::into_inner);
        for writer in retired
            .drain(..)
            .map(|Retired(writer)| writer)
            .chain((!writer.is_null()).then_some(writer))
        {
            // SAFETY: each came from Box::into_raw in attach and is freed
            // once: it is in exactly one of the two places.
            drop(unsafe { Box::from_raw(writer) });
        }
    }
}

/// A Google Cast sender behind an opaque C pointer.
pub struct AscCast {
    host: CastHost,
    shared: Arc<Shared>,
    input_taken: bool,
    list: DeviceList,
    browser: Option<Box<dyn Browse>>,
    open_browser: OpenBrowser,
    changes: VecDeque<Change>,
    /// The strings of the last device change handed out: ID, name, model.
    change_strings: [CString; 4],
    events: VecDeque<Pending>,
    /// The strings of the last event handed out: ID, detail.
    event_strings: [CString; 2],
    memory_json: CString,
    /// Sent with the next `LOAD` (C54).
    metadata: Metadata,
    /// Served again for each new session, for its next `LOAD` (C53).
    artwork: Option<Vec<u8>>,
    attempts: Vec<Attempt>,
    /// The attempt whose ring the input writes into.
    feeding: Option<AttemptId>,
    panics_before: u64,
    failure_reported: bool,
    /// The playing group's members, as its session last reported them.
    members: Vec<GroupMember>,
    /// The strings of the last member handed out: ID, name.
    member_strings: [CString; 2],
}

/// `text` as a C string; a NUL inside becomes U+FFFD.
fn c_string(text: &str) -> CString {
    CString::new(text.replace('\0', "\u{FFFD}")).unwrap_or_default()
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

/// An optional metadata text: NULL or "" is unknown, left out (C53).
///
/// # Safety
///
/// As [`host_text`].
unsafe fn metadata_text(text: *const c_char) -> Result<Option<String>, ()> {
    if text.is_null() {
        return Ok(None);
    }
    // SAFETY: as the caller's.
    let text = unsafe { host_text(text) }.ok_or(())?;
    Ok((!text.is_empty()).then(|| text.to_owned()))
}

/// The ID the host knows a list entry by.
fn device_id_text(key: DeviceKey) -> String {
    match key {
        DeviceKey::Id(id) => id.to_string(),
        DeviceKey::Manual(address) => format!("manual:{address}"),
    }
}

fn device_key(id: &str) -> Option<DeviceKey> {
    match id.strip_prefix("manual:") {
        Some(address) => address.parse().ok().map(DeviceKey::Manual),
        None => DeviceId::parse(id).map(DeviceKey::Id),
    }
}

/// The key a device's memory is stored under. A manual entry has no `id`
/// yet (C18), so its address text is hashed into one (FNV-1a, 128 bits).
fn memory_id(key: DeviceKey) -> DeviceId {
    match key {
        DeviceKey::Id(id) => id,
        DeviceKey::Manual(_) => {
            const OFFSET: u128 = 0x6c62_272e_07bb_0142_62b8_2175_6295_c58d;
            const PRIME: u128 = 0x0000_0000_0100_0000_0000_0000_0000_013b;
            let hash = device_id_text(key).bytes().fold(OFFSET, |hash, byte| {
                (hash ^ u128::from(byte)).wrapping_mul(PRIME)
            });
            DeviceId(hash.to_be_bytes())
        }
    }
}

/// A manual address: `ip:port`, `[ipv6]:port`, or an address alone, on the
/// device port.
fn manual_address(text: &str) -> Option<SocketAddr> {
    text.parse::<SocketAddr>().ok().or_else(|| {
        text.parse::<IpAddr>()
            .ok()
            .map(|ip| SocketAddr::new(ip, DEVICE_PORT))
    })
}

fn format_code(codec: Codec) -> i32 {
    match codec {
        Codec::Wav => CAST_FORMAT_WAV,
        Codec::Flac => CAST_FORMAT_FLAC,
    }
}

fn media_code(error: &MediaError) -> i32 {
    match error {
        MediaError::NoFetch => CAST_ERROR_NO_FETCH,
        _ => CAST_ERROR_MEDIA,
    }
}

fn stream_code(error: &StreamError) -> i32 {
    match error {
        StreamError::ArtworkFormat | StreamError::ArtworkTooLarge { .. } => CAST_ERROR_ARTWORK,
        _ => CAST_ERROR_STREAM,
    }
}

fn connect_code(_: &ConnectError) -> i32 {
    CAST_ERROR_CONNECT
}

fn session_code(error: &SessionError) -> i32 {
    match error {
        SessionError::Connect(error) => connect_code(error),
        SessionError::Launch(_) => CAST_ERROR_LAUNCH,
        SessionError::Media(error) => media_code(error),
        SessionError::Stream(error) => stream_code(error),
    }
}

fn command_code(error: &CommandError) -> i32 {
    match error {
        CommandError::NoSession => CAST_ERROR_NO_SESSION,
        CommandError::Volume(VolumeError::Fixed) => CAST_ERROR_VOLUME_FIXED,
        CommandError::Volume(_) => CAST_ERROR_VOLUME,
        CommandError::Media(error) => media_code(error),
        CommandError::Stream(error) => stream_code(error),
    }
}

fn millis(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1000.0
}

/// The C number of an attempt: one more than the host's, so 0 is none.
fn attempt_number(attempt: AttemptId) -> u64 {
    attempt.0.saturating_add(1)
}

impl AscCast {
    /// A call panicked, or the worker thread ended.
    fn is_dead(&self) -> bool {
        self.shared.dead.load(Ordering::Acquire) || !self.host.is_running()
    }

    /// The detail of [`CAST_EVENT_FAILED`], as the AirPlay interface gives
    /// it (S205): where the caught panic happened, else the latest panic
    /// since the handle was created, else "".
    fn failure_detail(&self) -> String {
        if let Some(at) = self.shared.at.get() {
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

    fn change(&mut self, event: i32, key: DeviceKey) {
        let id = device_id_text(key);
        let entry = if event == CAST_GONE {
            None
        } else {
            self.list.get(key)
        };
        let mut change = Change {
            event,
            id,
            name: String::new(),
            model: String::new(),
            flags: 0,
            former: String::new(),
        };
        if let Some(entry) = entry {
            if let Some(device) = &entry.device {
                change.name = device.name.clone().unwrap_or_default();
                change.model = device.model.clone().unwrap_or_default();
                if device.kind == DeviceKind::Group {
                    change.flags |= CAST_FLAG_GROUP;
                }
            }
            if !entry.online {
                change.flags |= CAST_FLAG_OFFLINE;
            }
            if matches!(entry.key, DeviceKey::Manual(_)) {
                change.flags |= CAST_FLAG_MANUAL;
            }
        }
        self.changes.push_back(change);
    }

    /// Logs what a group announces when it is first listed (C89).
    fn log_group(&mut self, key: DeviceKey) {
        let line = self
            .list
            .get(key)
            .and_then(|entry| entry.device.as_ref())
            .and_then(|device| device.group_log_line());
        if let Some(line) = line {
            let mut pending = Pending::new(CAST_EVENT_GROUP_LOG);
            pending.detail = line;
            self.events.push_back(pending);
        }
    }

    /// Hands the playing group's latest records and its members' addresses
    /// to its session (C90, C103).
    fn follow_groups(&mut self) {
        let member_ips: Vec<IpAddr> = self
            .members
            .iter()
            .filter_map(|member| self.list.member(member))
            .flat_map(|entry| entry.addresses.iter().map(SocketAddr::ip))
            .collect();
        for attempt in &self.attempts {
            let Some(records) = &attempt.group else {
                continue;
            };
            if let Some(entry) = self.list.get(attempt.key) {
                records.set_group(entry.addresses.clone());
            }
            records.set_members(member_ips.clone());
        }
    }

    fn listed(&mut self, event: ListEvent) {
        match event {
            ListEvent::Added(key) => {
                self.change(CAST_APPEARED, key);
                self.log_group(key);
            }
            ListEvent::Updated(key) | ListEvent::Offline(key) => self.change(CAST_CHANGED, key),
            ListEvent::Removed(key) => self.change(CAST_GONE, key),
            ListEvent::Identified { from, id } => {
                for attempt in &mut self.attempts {
                    if attempt.key == DeviceKey::Manual(from) {
                        attempt.key = DeviceKey::Id(id);
                    }
                }
                // Before CAST_APPEARED, which the host answers by reading the ID's offset.
                // 0 ms counts as none: the sender can't forget an offset, so Undo leaves 0.
                let memory = self.host.memory();
                let stored = |device| memory.offset(device).filter(|offset| !offset.is_zero());
                if let (Some(offset), None) =
                    (stored(&memory_id(DeviceKey::Manual(from))), stored(&id))
                {
                    self.host.set_offset(id, offset);
                    self.events
                        .push_back(Pending::new(CAST_EVENT_MEMORY_CHANGED));
                }
                self.change(CAST_GONE, DeviceKey::Manual(from));
                self.change(CAST_APPEARED, DeviceKey::Id(id));
                if let Some(appeared) = self.changes.back_mut() {
                    appeared.former = device_id_text(DeviceKey::Manual(from));
                }
                self.log_group(DeviceKey::Id(id));
            }
            ListEvent::Skipped(_) => {}
        }
    }

    fn pump_discovery(&mut self) {
        let Some(browser) = self.browser.as_mut() else {
            return;
        };
        let mut found = Vec::new();
        while let Some(event) = browser.poll_event() {
            found.push(event);
        }
        let dead = browser.is_dead();
        for event in found {
            let event = self.list.apply(event);
            self.listed(event);
        }
        self.follow_groups();
        if dead {
            self.end_discovery();
            self.events
                .push_back(Pending::new(CAST_EVENT_DISCOVERY_STOPPED));
        }
    }

    /// Stops browsing: every device discovery found leaves the list, except
    /// one in a session, which stays as offline (C17).
    fn end_discovery(&mut self) {
        self.browser = None;
        let found: Vec<String> = self
            .list
            .entries()
            .iter()
            .filter_map(|entry| entry.device.as_ref().map(|device| device.instance.clone()))
            .collect();
        for fullname in found {
            let event = self.list.apply(BrowseEvent::Removed { fullname });
            self.listed(event);
        }
    }

    fn attempt_id(&self, attempt: AttemptId, device: DeviceId) -> String {
        self.attempts
            .iter()
            .find(|known| known.attempt == attempt)
            .map_or_else(|| device.to_string(), |known| known.id.clone())
    }

    fn pump_host(&mut self) {
        while let Some(event) = self.host.poll_event() {
            let pending = self.take_in(event);
            self.events.push_back(pending);
        }
        self.shared.reclaim();
    }

    fn take_in(&mut self, event: HostEvent) -> Pending {
        match event {
            HostEvent::Session {
                attempt,
                device,
                event,
            } => {
                let id = self.attempt_id(attempt, device);
                let mut members = None;
                let mut pending = session_event(event, |list| members = Some(list));
                if let Some(members) = members {
                    self.members = members;
                    self.follow_groups();
                }
                if pending.kind == CAST_EVENT_ENDED {
                    self.ended(attempt);
                }
                pending.attempt = attempt_number(attempt);
                pending.id = id;
                pending
            }
            HostEvent::Started {
                attempt,
                device,
                format,
            } => {
                let mut pending = Pending::new(CAST_EVENT_STARTED);
                pending.attempt = attempt_number(attempt);
                pending.id = self.attempt_id(attempt, device);
                pending.format = format_code(format.codec());
                pending
            }
            HostEvent::Ended {
                attempt,
                device,
                end,
            } => {
                let id = self.attempt_id(attempt, device);
                self.ended(attempt);
                let mut pending = ended_event(end);
                pending.attempt = attempt_number(attempt);
                pending.id = id;
                pending
            }
            HostEvent::CommandFailed { attempt, error } => {
                let mut pending = Pending::new(CAST_EVENT_COMMAND_FAILED);
                if let Some(attempt) = attempt {
                    pending.attempt = attempt_number(attempt);
                    pending.id = self
                        .attempts
                        .iter()
                        .find(|known| known.attempt == attempt)
                        .map(|known| known.id.clone())
                        .unwrap_or_default();
                }
                pending.error = command_code(&error);
                pending.detail = error.to_string();
                pending
            }
            HostEvent::MemoryChanged => Pending::new(CAST_EVENT_MEMORY_CHANGED),
        }
    }

    /// An attempt's `Ended` came: its device is no longer in a session
    /// (C17), and its ring takes no more audio.
    fn ended(&mut self, attempt: AttemptId) {
        let Some(index) = self
            .attempts
            .iter()
            .position(|known| known.attempt == attempt)
        else {
            return;
        };
        let removed = self.attempts.remove(index);
        let key = removed.key;
        if removed.group.is_some() {
            self.members.clear();
        }
        if self.feeding == Some(attempt) {
            self.feeding = None;
            self.shared.attach(None);
        }
        if self.attempts.iter().any(|known| known.key == key) {
            return;
        }
        let listed = self.list.get(key).is_some();
        self.list.set_in_session(key, false);
        if listed && self.list.get(key).is_none() {
            self.change(CAST_GONE, key);
        }
    }

    fn start(&mut self, id: &str) -> Result<AttemptId, i32> {
        self.pump_discovery();
        let key = device_key(id).ok_or(CAST_REFUSED_UNKNOWN)?;
        let entry = self.list.get(key).ok_or(CAST_REFUSED_UNKNOWN)?;
        let group = entry
            .is_group()
            .then(|| GroupRecords::new(entry.addresses.clone()));
        let target = CastTarget {
            id: memory_id(key),
            addresses: entry.addresses.clone(),
            group: group.clone(),
        };
        let (writer, reader) = audio_ring(RING_FRAMES);
        let attempt = self
            .host
            .start_with_artwork(
                target,
                CAST_RATE,
                self.metadata.clone(),
                self.artwork.clone(),
                reader,
            )
            .map_err(|refused| match refused {
                StartRefused::Busy { .. } => CAST_REFUSED_BUSY,
                StartRefused::Rate(_) | StartRefused::Closed => CAST_REFUSED_CLOSED,
            })?;
        self.shared.attach(Some(writer));
        self.feeding = Some(attempt);
        self.list.set_in_session(key, true);
        if group.is_some() {
            self.members.clear();
        }
        self.attempts.push(Attempt {
            attempt,
            key,
            id: device_id_text(key),
            group,
        });
        Ok(attempt)
    }

    fn describe_change(&mut self, change: Change) -> AscCastDevice {
        self.change_strings = [
            c_string(&change.id),
            c_string(&change.name),
            c_string(&change.model),
            c_string(&change.former),
        ];
        AscCastDevice {
            event: change.event,
            id: self.change_strings[0].as_ptr(),
            name: self.change_strings[1].as_ptr(),
            model: self.change_strings[2].as_ptr(),
            flags: change.flags,
            former: self.change_strings[3].as_ptr(),
        }
    }

    fn describe_event(&mut self, pending: Pending) -> AscCastEvent {
        self.event_strings = [c_string(&pending.id), c_string(&pending.detail)];
        AscCastEvent {
            kind: pending.kind,
            attempt: pending.attempt,
            id: self.event_strings[0].as_ptr(),
            reason: pending.reason,
            error: pending.error,
            format: pending.format,
            retry_ms: pending.retry_ms,
            level: pending.level,
            muted: pending.muted,
            volume_fixed: pending.volume_fixed,
            step: pending.step,
            drift_ms: pending.drift_ms,
            queued_ms: pending.queued_ms,
            resync_offered: pending.resync_offered,
            relaunched: pending.relaunched,
            detail: self.event_strings[1].as_ptr(),
        }
    }
}

fn session_event(event: SessionEvent, on_members: impl FnOnce(Vec<GroupMember>)) -> Pending {
    match event {
        SessionEvent::Playing => Pending::new(CAST_EVENT_PLAYING),
        SessionEvent::Diagnostics(diagnostics) => {
            let mut pending = Pending::new(CAST_EVENT_DIAGNOSTICS);
            pending.drift_ms = millis(diagnostics.drift);
            pending.queued_ms = millis(diagnostics.queued);
            pending.resync_offered = diagnostics.resync_offered;
            pending.detail = format!("{diagnostics:?}");
            pending
        }
        SessionEvent::Rebuffering => Pending::new(CAST_EVENT_REBUFFERING),
        SessionEvent::Volume(volume) => {
            let mut pending = Pending::new(CAST_EVENT_VOLUME);
            pending.level = volume.level;
            pending.muted = volume.muted;
            pending.volume_fixed = volume.control_type == ControlType::Fixed;
            pending.step = volume.step_interval.unwrap_or(0.0);
            pending
        }
        SessionEvent::ConnectFailed { error, retry_in } => {
            let mut pending = Pending::new(CAST_EVENT_CONNECT_FAILED);
            pending.error = connect_code(&error);
            pending.retry_ms = i64::try_from(retry_in.as_millis()).unwrap_or(i64::MAX);
            pending.detail = error.to_string();
            pending
        }
        SessionEvent::Recovering(reason) => {
            let mut pending = Pending::new(CAST_EVENT_RECOVERING);
            pending.detail = reason.to_string();
            pending
        }
        SessionEvent::Recovered { relaunched } => {
            let mut pending = Pending::new(CAST_EVENT_RECOVERED);
            pending.relaunched = relaunched;
            pending
        }
        SessionEvent::Reloaded => Pending::new(CAST_EVENT_RELOADED),
        SessionEvent::FormatFallback { to, reason, .. } => {
            let mut pending = Pending::new(CAST_EVENT_FORMAT_FALLBACK);
            pending.format = format_code(to);
            pending.detail = reason.to_string();
            pending
        }
        SessionEvent::Stream(event) => {
            let mut pending = Pending::new(CAST_EVENT_STREAM);
            pending.detail = format!("{event:?}");
            pending
        }
        SessionEvent::Members(members) => {
            on_members(members);
            Pending::new(CAST_EVENT_MEMBERS)
        }
        SessionEvent::GroupMessage(kind) => {
            let mut pending = Pending::new(CAST_EVENT_GROUP_LOG);
            pending.detail = format!("Cast group message {kind}");
            pending
        }
        SessionEvent::Ended(end) => ended_event(end),
    }
}

fn ended_event(end: SessionEnd) -> Pending {
    let mut pending = Pending::new(CAST_EVENT_ENDED);
    match end {
        SessionEnd::Stopped => pending.reason = CAST_END_STOPPED,
        SessionEnd::TakenOver => pending.reason = CAST_END_TAKEN_OVER,
        SessionEnd::Deselected => pending.reason = CAST_END_DESELECTED,
        SessionEnd::Failed(error) => {
            pending.reason = CAST_END_FAILED;
            pending.error = session_code(&error);
            pending.detail = error.to_string();
        }
    }
    pending
}

/// Tests: the panic a test asked for; nothing otherwise.
#[inline(always)]
fn injected_panic() {
    #[cfg(test)]
    if tests::PANIC_NEXT.with(|next| next.replace(false)) {
        panic!("a panic injected by a test");
    }
}

/// Runs `body` on the live handle behind `cast`, stopping a panic at this
/// boundary: `failed` for a null or dead handle, and after a panic, which
/// also makes the handle dead.
///
/// # Safety
///
/// `cast` is null or a live handle, called under the threading contract.
unsafe fn live<T>(cast: *mut AscCast, failed: T, body: impl FnOnce(&mut AscCast) -> T) -> T {
    if cast.is_null() {
        return failed;
    }
    let outcome = panic::catch_unwind(AssertUnwindSafe(|| {
        injected_panic();
        // SAFETY: non-null, and by the caller's contract a live handle that
        // no other call uses now.
        let cast = unsafe { &mut *cast };
        if cast.is_dead() {
            None
        } else {
            Some(body(cast))
        }
    }));
    match outcome {
        Ok(Some(value)) => value,
        Ok(None) => failed,
        Err(_) => {
            // SAFETY: still live; the closure's borrow ended with it.
            unsafe { &*cast }.shared.panicked();
            failed
        }
    }
}

/// Starts a Cast sender's worker thread with the device memory the host
/// stored from [`asc_cast_memory_json`]: null or text that doesn't read
/// starts empty, and damaged entries are skipped. Discovery waits for
/// [`asc_cast_start_discovery`]. Null when the thread can't start.
///
/// # Safety
///
/// `memory_json` is null or a NUL-terminated string, unchanged during the
/// call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn asc_cast_create(memory_json: *const c_char) -> *mut AscCast {
    // SAFETY: as the caller's.
    let text = unsafe { host_text(memory_json) }.unwrap_or_default();
    create_with(text, live_browser)
}

fn create_with(memory_json: &str, open_browser: OpenBrowser) -> *mut AscCast {
    note_panic_locations();
    let panics_before = PANICS.load(Ordering::SeqCst);
    let created = panic::catch_unwind(|| {
        injected_panic();
        let Ok(host) = CastHost::new(DeviceMemory::from_json(memory_json)) else {
            return ptr::null_mut();
        };
        Box::into_raw(Box::new(AscCast {
            host,
            shared: Arc::new(Shared::new()),
            input_taken: false,
            list: DeviceList::new(),
            browser: None,
            open_browser,
            changes: VecDeque::new(),
            change_strings: Default::default(),
            events: VecDeque::new(),
            event_strings: Default::default(),
            memory_json: CString::default(),
            metadata: Metadata::default(),
            artwork: None,
            attempts: Vec::new(),
            feeding: None,
            panics_before,
            failure_reported: false,
            members: Vec::new(),
            member_strings: Default::default(),
        }))
    });
    created.unwrap_or_else(|_| {
        drop(caught_panic_location());
        ptr::null_mut()
    })
}

/// Stops the sender and frees it: a running or starting session gets the
/// teardown of C88 within 3 s, which this call waits for (about 3.5 s at
/// most), so a host calls it from its quit path, off the main thread. Sets
/// `*cast` to null. An input taken from it stays valid until its own
/// destroy.
///
/// # Safety
///
/// `cast` is null or points to a pointer that is null or was returned by
/// [`asc_cast_create`] and not destroyed yet; no other call on that handle
/// runs now or later.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn asc_cast_destroy(cast: *mut *mut AscCast) {
    if cast.is_null() {
        return;
    }
    // SAFETY: by the caller's contract `cast` points to a readable and
    // writable handle pointer.
    let handle = unsafe { cast.replace(ptr::null_mut()) };
    if handle.is_null() {
        return;
    }
    // SAFETY: from Box::into_raw in create_with, destroyed once (the host's
    // pointer is now null), with no call in progress.
    let mut owned = unsafe { Box::from_raw(handle) };
    let freed = panic::catch_unwind(AssertUnwindSafe(move || {
        owned.shared.attach(None);
        owned.host.shutdown();
        drop(owned);
    }));
    // What a panic left unfreed stays behind; the host's pointer is null.
    drop(freed);
}

/// Starts browsing for Cast devices over multicast DNS (C10); they arrive
/// through [`asc_cast_next_device`]. True when it runs already. False when
/// the handle is null or dead or multicast DNS can't start.
///
/// # Safety
///
/// `cast` is null or a live handle, called under the threading contract.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn asc_cast_start_discovery(cast: *mut AscCast) -> bool {
    // SAFETY: as the caller's.
    unsafe {
        live(cast, false, |cast| {
            if cast.browser.is_none() {
                cast.browser = (cast.open_browser)();
            }
            cast.browser.is_some()
        })
    }
}

/// Stops browsing: every device discovery found is reported gone, except
/// one in a session, which stays listed as offline (C17). Devices added by
/// address stay.
///
/// # Safety
///
/// `cast` is null or a live handle, called under the threading contract.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn asc_cast_stop_discovery(cast: *mut AscCast) {
    // SAFETY: as the caller's.
    unsafe { live(cast, (), AscCast::end_discovery) }
}

/// Adds a device by address while multicast DNS is blocked (C18):
/// `ip:port`, `[ipv6]:port` or an address alone (port 8009). It is listed
/// as `manual:<ip:port>` until a browse finds its `id`. False for a null or
/// dead handle or an address that doesn't read.
///
/// # Safety
///
/// `cast` is null or a live handle, called under the threading contract;
/// `address` is null or a NUL-terminated string, unchanged during the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn asc_cast_add_device(cast: *mut AscCast, address: *const c_char) -> bool {
    // SAFETY: as the caller's.
    let Some(address) = unsafe { host_text(address) }.and_then(manual_address) else {
        return false;
    };
    // SAFETY: as the caller's.
    unsafe {
        live(cast, false, |cast| {
            let event = cast.list.add_manual(address);
            cast.listed(event);
            true
        })
    }
}

/// Takes back a device added by address that no browse has found yet, by
/// its `manual:<ip:port>` ID; its gone change follows (C18). The host turns
/// it off first. False for a null or dead handle, or an ID that names no
/// such device: one a browse found stays listed under its `id`.
///
/// # Safety
///
/// `cast` is null or a live handle, called under the threading contract;
/// `id` is null or a NUL-terminated string, unchanged during the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn asc_cast_remove_device(cast: *mut AscCast, id: *const c_char) -> bool {
    // SAFETY: as the caller's.
    let Some(DeviceKey::Manual(address)) = unsafe { host_text(id) }.and_then(device_key) else {
        return false;
    };
    // SAFETY: as the caller's.
    unsafe {
        live(cast, false, |cast| {
            let Some(event) = cast.list.remove_manual(address) else {
                return false;
            };
            cast.listed(event);
            true
        })
    }
}

/// Takes the next device change into `*device` and returns true; false,
/// leaving it as it was, when none waits or a pointer is null. Never waits.
///
/// # Safety
///
/// `cast` is null or a live handle, called under the threading contract;
/// `device` is null or writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn asc_cast_next_device(
    cast: *mut AscCast,
    device: *mut AscCastDevice,
) -> bool {
    if device.is_null() {
        return false;
    }
    // SAFETY: as the caller's.
    let next = unsafe {
        live(cast, None, |cast| {
            if cast.changes.is_empty() {
                cast.pump_discovery();
            }
            let change = cast.changes.pop_front()?;
            Some(cast.describe_change(change))
        })
    };
    let Some(next) = next else {
        return false;
    };
    // SAFETY: non-null and writable by the caller's contract.
    unsafe { device.write(next) };
    true
}

/// Takes the next event into `*event` and returns true; false, leaving it
/// as it was, when none waits or a pointer is null. Never waits. A dead
/// handle reports [`CAST_EVENT_FAILED`] once, in place of whatever still
/// waited, and nothing after it.
///
/// # Safety
///
/// `cast` is null or a live handle, called under the threading contract;
/// `event` is null or writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn asc_cast_next_event(cast: *mut AscCast, event: *mut AscCastEvent) -> bool {
    if cast.is_null() || event.is_null() {
        return false;
    }
    let outcome = panic::catch_unwind(AssertUnwindSafe(|| {
        injected_panic();
        // SAFETY: non-null, and by the caller's contract a live handle that
        // no other call uses now.
        let cast = unsafe { &mut *cast };
        if cast.is_dead() {
            return failure(cast);
        }
        if cast.events.is_empty() {
            cast.pump_discovery();
            cast.pump_host();
        }
        let pending = cast.events.pop_front()?;
        Some(cast.describe_event(pending))
    }));
    let next = outcome.unwrap_or_else(|_| {
        // SAFETY: still live; the closure's borrow ended with it.
        let cast = unsafe { &mut *cast };
        cast.shared.panicked();
        panic::catch_unwind(AssertUnwindSafe(|| failure(cast))).unwrap_or(None)
    });
    let Some(next) = next else {
        return false;
    };
    // SAFETY: non-null and writable by the caller's contract.
    unsafe { event.write(next) };
    true
}

/// [`CAST_EVENT_FAILED`], the first time a dead handle is asked.
fn failure(cast: &mut AscCast) -> Option<AscCastEvent> {
    if cast.failure_reported {
        return None;
    }
    cast.failure_reported = true;
    cast.events.clear();
    let mut pending = Pending::new(CAST_EVENT_FAILED);
    pending.detail = cast.failure_detail();
    Some(cast.describe_event(pending))
}

/// Starts a session on the listed device `id` in the background and
/// returns its attempt number (1 or more); its events carry it. The stream
/// plays what the input writes from now on, with the metadata and artwork
/// set last. Returns 0, starting nothing, for a null or dead handle, an ID
/// that isn't listed, or while another device or group plays or starts
/// (C2); a group plays as one device (C3, C93). `*refused` (when not null) gets the `CAST_REFUSED_*` code, 0 on
/// success. The format that worked last on the device is tried first (C84).
///
/// # Safety
///
/// `cast` is null or a live handle, called under the threading contract;
/// `id` is null or a NUL-terminated string, unchanged during the call;
/// `refused` is null or writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn asc_cast_start(
    cast: *mut AscCast,
    id: *const c_char,
    refused: *mut i32,
) -> u64 {
    // SAFETY: as the caller's.
    let result = match unsafe { host_text(id) } {
        None => Err(CAST_REFUSED_INVALID),
        // SAFETY: as the caller's.
        Some(id) => unsafe { live(cast, Err(CAST_REFUSED_CLOSED), |cast| cast.start(id)) },
    };
    let (attempt, code) = match result {
        Ok(attempt) => (attempt_number(attempt), CAST_REFUSED_NONE),
        Err(code) => (0, code),
    };
    if !refused.is_null() {
        // SAFETY: non-null and writable by the caller's contract.
        unsafe { refused.write(code) };
    }
    attempt
}

/// Stops the session, or the attempt to start one: connect retries and the
/// waits for the launch and `PLAYING` end at once, and the teardown of C87
/// runs on the worker. Another device may start right away; this attempt's
/// ended event follows.
///
/// # Safety
///
/// `cast` is null or a live handle, called under the threading contract.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn asc_cast_stop(cast: *mut AscCast) {
    // SAFETY: as the caller's.
    unsafe { live(cast, (), |cast| cast.host.stop()) }
}

/// Runs `send` when a session runs or starts; false otherwise.
///
/// # Safety
///
/// As [`live`].
unsafe fn with_session(cast: *mut AscCast, send: impl FnOnce(&CastHost)) -> bool {
    // SAFETY: as the caller's.
    unsafe {
        live(cast, false, |cast| {
            if cast.host.playing().is_none() {
                return false;
            }
            send(&cast.host);
            true
        })
    }
}

/// Sets the device's volume, 0 to 1, clamped (C65, C67). The level shown is
/// the one volume events report (C69, C70); a host dragging a slider sends
/// at most one every 100 ms. False, sending nothing, for a null or dead
/// handle, a level that isn't finite, or when no session runs; a refusal
/// (a locked volume, C68) comes as a command-failed event.
///
/// # Safety
///
/// `cast` is null or a live handle, called under the threading contract.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn asc_cast_set_volume(cast: *mut AscCast, level: f64) -> bool {
    if !level.is_finite() {
        return false;
    }
    // SAFETY: as the caller's.
    unsafe { with_session(cast, |host| host.set_volume(level.clamp(0.0, 1.0))) }
}

/// Mutes or unmutes the device, apart from its level (C65). False, sending
/// nothing, for a null or dead handle or when no session runs.
///
/// # Safety
///
/// `cast` is null or a live handle, called under the threading contract.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn asc_cast_set_muted(cast: *mut AscCast, muted: bool) -> bool {
    // SAFETY: as the caller's.
    unsafe { with_session(cast, |host| host.set_muted(muted)) }
}

/// Moves the device's volume one of its own steps up or down (C69). False,
/// sending nothing, for a null or dead handle or when no session runs.
///
/// # Safety
///
/// `cast` is null or a live handle, called under the threading contract.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn asc_cast_step_volume(cast: *mut AscCast, up: bool) -> bool {
    // SAFETY: as the caller's.
    unsafe { with_session(cast, |host| host.step_volume(up)) }
}

/// The level to send for the host's master and the speaker's own level, all
/// 0 to 1: their product, clamped; anything not finite counts as 0 (C104).
#[unsafe(no_mangle)]
pub extern "C" fn asc_cast_device_level(master: f64, own: f64) -> f64 {
    status::device_level(master, status::NO_ZONE_LEVEL, own)
}

/// The level to send for the host's master, the speaker's zone (1 outside a
/// zone) and its own level, all 0 to 1: master × zone × own, clamped;
/// anything not finite counts as 0 (C104, S249).
#[unsafe(no_mangle)]
pub extern "C" fn asc_cast_zone_level(master: f64, zone: f64, own: f64) -> f64 {
    status::device_level(master, zone, own)
}

/// The own level a reported device level means at `master` (C104):
/// level ÷ master, clamped to 0 to 1; `own` as it is at master 0 or when a
/// value isn't finite.
#[unsafe(no_mangle)]
pub extern "C" fn asc_cast_own_level(reported: f64, master: f64, own: f64) -> f64 {
    status::own_level(reported, master, status::NO_ZONE_LEVEL, own)
}

/// The own level a reported device level means at `master` and `zone`
/// (C104, S250): level ÷ (master × zone), clamped to 0 to 1; `own` as it is
/// when master × zone is 0 or a value isn't finite.
#[unsafe(no_mangle)]
pub extern "C" fn asc_cast_zone_own_level(reported: f64, master: f64, zone: f64, own: f64) -> f64 {
    status::own_level(reported, master, zone, own)
}

/// The own level a session starts from (C105): `own` when `has_own`, whatever
/// the first status reports; else that status's level ÷ master, clamped, or
/// the level itself at master 0.
#[unsafe(no_mangle)]
pub extern "C" fn asc_cast_start_own_level(
    master: f64,
    has_own: bool,
    own: f64,
    reported: f64,
) -> f64 {
    status::start_own_level(
        master,
        status::NO_ZONE_LEVEL,
        has_own.then_some(own),
        reported,
    )
}

/// The own level a session starts from (C105) for a speaker in a zone:
/// `own` when `has_own`, whatever the first status reports; else that
/// status's level ÷ (master × zone), clamped, or the level itself when
/// master × zone is 0.
#[unsafe(no_mangle)]
pub extern "C" fn asc_cast_zone_start_own_level(
    master: f64,
    zone: f64,
    has_own: bool,
    own: f64,
    reported: f64,
) -> f64 {
    status::start_own_level(master, zone, has_own.then_some(own), reported)
}

/// Whether a reported level is the device's answer to the level `sent`
/// (C104): within half a percent.
#[unsafe(no_mangle)]
pub extern "C" fn asc_cast_level_answers(sent: f64, reported: f64) -> bool {
    status::answers_level(sent, reported)
}

/// Sets the track's title, artist and album (UTF-8) for the next `LOAD`:
/// the next start, or a Resync; a playing stream is not reloaded (C54).
/// NULL or "" leaves a field out (C53). False, changing nothing, for a null
/// or dead handle or a text that isn't UTF-8.
///
/// # Safety
///
/// `cast` is null or a live handle, called under the threading contract;
/// each text is null or a NUL-terminated string, unchanged during the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn asc_cast_set_metadata(
    cast: *mut AscCast,
    title: *const c_char,
    artist: *const c_char,
    album: *const c_char,
) -> bool {
    // SAFETY: as the caller's.
    let texts = unsafe {
        (
            metadata_text(title),
            metadata_text(artist),
            metadata_text(album),
        )
    };
    let (Ok(title), Ok(artist), Ok(album_name)) = texts else {
        return false;
    };
    let metadata = Metadata {
        title,
        artist,
        album_name,
        images: Vec::new(),
    };
    // SAFETY: as the caller's.
    unsafe {
        live(cast, false, |cast| {
            cast.metadata = metadata.clone();
            if cast.host.playing().is_some() {
                cast.host.set_metadata(metadata);
                if let Some(artwork) = &cast.artwork {
                    cast.host.set_artwork(artwork.clone());
                }
            }
            true
        })
    }
}

/// Sets the track's artwork, PNG or JPEG of at most 1280 by 720 (C53), for
/// the next `LOAD` of this and every later session; NULL or a length of 0
/// forgets it. The device's refusal of the image comes as a command-failed
/// event. False, changing nothing, for a null or dead handle, NULL with a
/// length, or more than 8 MiB.
///
/// # Safety
///
/// `cast` is null or a live handle, called under the threading contract;
/// `bytes` is null or points to `len` bytes, unchanged during the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn asc_cast_set_artwork(
    cast: *mut AscCast,
    bytes: *const u8,
    len: usize,
) -> bool {
    if len > MAX_ARTWORK_BYTES {
        return false;
    }
    // SAFETY: as the caller's.
    let Some(bytes) = (unsafe { host_slice(bytes, len) }) else {
        return false;
    };
    // SAFETY: as the caller's.
    unsafe {
        live(cast, false, |cast| {
            cast.artwork = (!bytes.is_empty()).then(|| bytes.to_vec());
            if cast.host.playing().is_some() {
                // New metadata drops the session's artwork; it is then served again if kept.
                cast.host.set_metadata(cast.metadata.clone());
                if let Some(artwork) = &cast.artwork {
                    cast.host.set_artwork(artwork.clone());
                }
            }
            true
        })
    }
}

/// Holds the audio the input hands the session back by `hold_back_ms`,
/// live, so a device whose offset is shorter than L plays at L (C77). It
/// stops at 20 000 ms; a value below 0 or not finite counts as 0. A change
/// fades out and in over 20 ms; a capture start (a rate request) begins
/// with silence for the hold-back, never with the end of the last capture.
/// Returns the hold-back applied, in ms; 0 for a null or dead handle.
///
/// # Safety
///
/// `cast` is null or a live handle, called under the threading contract.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn asc_cast_set_hold_back(cast: *mut AscCast, hold_back_ms: f64) -> f64 {
    let ms = if hold_back_ms.is_finite() {
        hold_back_ms.clamp(0.0, MAX_HOLD_BACK_MS)
    } else {
        0.0
    };
    let frames = (ms * f64::from(CAST_RATE) / 1000.0).round() as usize;
    // SAFETY: as the caller's.
    unsafe {
        live(cast, 0.0, |cast| {
            cast.shared.hold_back.store(frames, Ordering::Relaxed);
            frames as f64 * 1000.0 / f64::from(CAST_RATE)
        })
    }
}

/// The user's Resync (C79): a new `LOAD` on the same app, which brings the
/// latency back toward its start at the cost of a gap. Never sent on the
/// core's own. False, sending nothing, for a null or dead handle or when no
/// session runs.
///
/// # Safety
///
/// `cast` is null or a live handle, called under the threading contract.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn asc_cast_resync(cast: *mut AscCast) -> bool {
    // SAFETY: as the caller's.
    unsafe { with_session(cast, CastHost::resync) }
}

/// Stores the user's measured offset for device `id`, in ms, rounded to
/// whole ms (C77): the host delays its other outputs by it. The device
/// needn't be listed. Store [`asc_cast_memory_json`] afterwards. False for
/// a null or dead handle, an ID that doesn't read, or an offset outside 0
/// to 120 000 ms.
///
/// # Safety
///
/// `cast` is null or a live handle, called under the threading contract;
/// `id` is null or a NUL-terminated string, unchanged during the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn asc_cast_set_offset(
    cast: *mut AscCast,
    id: *const c_char,
    offset_ms: f64,
) -> bool {
    if !(0.0..=MAX_OFFSET_MS).contains(&offset_ms) {
        return false;
    }
    // SAFETY: as the caller's.
    let Some(key) = unsafe { host_text(id) }.and_then(device_key) else {
        return false;
    };
    let offset = Duration::from_millis(offset_ms.round() as u64);
    // SAFETY: as the caller's.
    unsafe {
        live(cast, false, |cast| {
            cast.host.set_offset(memory_id(key), offset);
            true
        })
    }
}

/// How many members the playing group has, as its latest
/// [`CAST_EVENT_MEMBERS`] reported; 0 while none are known, when no group
/// plays, and for a null or dead handle.
///
/// # Safety
///
/// `cast` is null or a live handle, called under the threading contract.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn asc_cast_member_count(cast: *mut AscCast) -> usize {
    // SAFETY: as the caller's.
    unsafe { live(cast, 0, |cast| cast.members.len()) }
}

/// Writes member `index` of the playing group into `*member` and returns
/// true; false, leaving it as it was, for an index past the count or a null
/// pointer or handle. A member matches the listed single device whose `id`
/// equals its `deviceId` read as a UUID (C95).
///
/// # Safety
///
/// `cast` is null or a live handle, called under the threading contract;
/// `member` is null or writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn asc_cast_member(
    cast: *mut AscCast,
    index: usize,
    member: *mut AscCastMember,
) -> bool {
    if member.is_null() {
        return false;
    }
    // SAFETY: as the caller's.
    let found = unsafe {
        live(cast, None, |cast| {
            let found = cast.members.get(index)?;
            let id = cast
                .list
                .member(found)
                .map(|entry| device_id_text(entry.key))
                .unwrap_or_default();
            let name = found.name.clone().unwrap_or_default();
            cast.member_strings = [c_string(&id), c_string(&name)];
            Some(AscCastMember {
                id: cast.member_strings[0].as_ptr(),
                name: cast.member_strings[1].as_ptr(),
            })
        })
    };
    let Some(found) = found else {
        return false;
    };
    // SAFETY: non-null and writable by the caller's contract.
    unsafe { member.write(found) };
    true
}

/// The offset stored for device `id`, in ms; -1 when none is, and for a
/// null or dead handle or an ID that doesn't read.
///
/// # Safety
///
/// `cast` is null or a live handle, called under the threading contract;
/// `id` is null or a NUL-terminated string, unchanged during the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn asc_cast_offset_ms(cast: *mut AscCast, id: *const c_char) -> f64 {
    // SAFETY: as the caller's.
    let Some(key) = unsafe { host_text(id) }.and_then(device_key) else {
        return -1.0;
    };
    // SAFETY: as the caller's.
    unsafe {
        live(cast, -1.0, |cast| {
            cast.host
                .memory()
                .offset(&memory_id(key))
                .map_or(-1.0, millis)
        })
    }
}

/// The device memory to store between runs, as JSON: per device, the
/// format that last worked (C84) and the offset (C77). The string is
/// NUL-terminated UTF-8 owned by the handle, valid until the next call of
/// this on it or its destroy. Null for a null or dead handle.
///
/// # Safety
///
/// `cast` is null or a live handle, called under the threading contract.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn asc_cast_memory_json(cast: *mut AscCast) -> *const c_char {
    // SAFETY: as the caller's.
    unsafe {
        live(cast, ptr::null(), |cast| {
            cast.memory_json = c_string(&cast.host.memory().to_json());
            cast.memory_json.as_ptr()
        })
    }
}

/// The capture side of an [`AscCastInput`]: the resampler from the capture
/// rate to [`CAST_RATE`], its output buffer and the input's fade (S189).
struct Feed {
    resampler: Resampler,
    resampled: Box<[f32]>,
    fade: InputFade,
    /// Holds the resampled audio back by [`Shared::hold_back`].
    delay: DelayLine,
    /// The last rate request taken in (see [`AscCastInput::requested`]).
    applied: u64,
    ready: bool,
}

impl Feed {
    /// Resamples `block` into the running session's ring. Never allocates,
    /// locks, blocks or logs (C8).
    fn write(&mut self, block: &[f32], requested: u64, closing: bool, shared: &Shared) -> usize {
        if requested != self.applied {
            self.applied = requested;
            self.ready = self.resampler.set_input_rate(requested as u32).is_ok();
            if self.ready {
                self.fade.restart(requested as u32);
                self.delay.reset();
            }
        }
        if !self.ready {
            return 0;
        }
        if closing {
            self.fade.fade_out();
            self.delay.close();
        }
        let mut taken = 0;
        let target = shared.hold_back.load(Ordering::Relaxed);
        let (resampler, resampled, delay) =
            (&mut self.resampler, &mut self.resampled, &mut self.delay);
        self.fade.process(block, |piece| {
            let frames = piece.len() / 2;
            let mut consumed = 0;
            // The resampler may take the whole piece while the buffer holds
            // only part of its output; it keeps the rest for the next call.
            loop {
                let (used, produced) =
                    resampler.process(&piece[consumed * 2..frames * 2], resampled);
                if produced > 0 {
                    delay.process(&resampled[..produced * 2], target, |held| {
                        taken += shared.write(held);
                    });
                }
                consumed += used;
                if used == 0 && produced == 0 {
                    break;
                }
            }
        });
        taken
    }
}

/// The capture thread's way into a Cast sender behind an opaque C pointer.
pub struct AscCastInput {
    feed: Feed,
    /// The latest rate request: a count in the high 32 bits, bumped by
    /// every request, and the rate in Hz in the low 32 bits; 0 before the
    /// first.
    requested: AtomicU64,
    closing: AtomicBool,
    shared: Arc<Shared>,
}

/// The sender's audio input, for the capture thread: the first call hands
/// it over, every later one returns null, as does a null or dead handle. It
/// starts without a rate, so its writes are dropped until
/// [`asc_cast_input_set_rate`] gives it one.
///
/// # Safety
///
/// `cast` is null or a live handle, called under the threading contract.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn asc_cast_take_input(cast: *mut AscCast) -> *mut AscCastInput {
    // SAFETY: as the caller's.
    unsafe {
        live(cast, ptr::null_mut(), |cast| {
            if cast.input_taken {
                return ptr::null_mut();
            }
            let Ok(resampler) = Resampler::new(CAST_RATE, CAST_RATE) else {
                return ptr::null_mut();
            };
            cast.input_taken = true;
            Box::into_raw(Box::new(AscCastInput {
                feed: Feed {
                    resampler,
                    resampled: vec![0.0; INPUT_CHUNK_FRAMES * 2].into_boxed_slice(),
                    fade: InputFade::new(CAST_RATE, INPUT_CHUNK_FRAMES),
                    delay: DelayLine::new(MAX_HOLD_BACK_FRAMES, CAST_RATE),
                    applied: 0,
                    ready: false,
                },
                requested: AtomicU64::new(0),
                closing: AtomicBool::new(false),
                shared: Arc::clone(&cast.shared),
            }))
        })
    }
}

/// Any thread: the rate in Hz of the audio the capture writes from now on,
/// rounded to whole hertz. The next write takes it in and starts the
/// resampler afresh with a fade-in from silence (S189), so a host calls it
/// at each capture start, even at an unchanged rate. False, changing
/// nothing, for a null handle or a rate outside 8000 to 384 000 Hz.
/// Real-time safe.
///
/// # Safety
///
/// `input` is null or a live input handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn asc_cast_input_set_rate(input: *mut AscCastInput, rate: f64) -> bool {
    if input.is_null() || !rate.is_finite() {
        return false;
    }
    let rate = rate.round();
    if !(0.0..=f64::from(u32::MAX)).contains(&rate) || !is_supported_rate(rate as u32) {
        return false;
    }
    // SAFETY: the handle is live; this call borrows only atomics, which
    // writes read but never borrow mutably.
    let (requested, closing) = unsafe { (&(*input).requested, &(*input).closing) };
    closing.store(false, Ordering::Release);
    let mut old = requested.load(Ordering::Relaxed);
    loop {
        let new = ((old >> 32).wrapping_add(1) << 32) | u64::from(rate as u32);
        match requested.compare_exchange_weak(old, new, Ordering::Release, Ordering::Relaxed) {
            Ok(_) => return true,
            Err(current) => old = current,
        }
    }
}

/// Any thread, at a capture stop: the input fades out over the next 20 ms
/// of frames written and hands zeros after them, until the next
/// [`asc_cast_input_set_rate`] fades it in again. Does nothing for a null
/// handle. Real-time safe.
///
/// # Safety
///
/// `input` is null or a live input handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn asc_cast_input_request_fade_out(input: *mut AscCastInput) {
    if input.is_null() {
        return;
    }
    // SAFETY: the handle is live; this call borrows only an atomic.
    let closing = unsafe { &(*input).closing };
    closing.store(true, Ordering::Release);
}

/// Capture thread: hands `frames` interleaved stereo frames at the rate
/// last set to the running session, resampled to 48 kHz (C8). Returns the
/// 48 kHz frames its ring took: 0 before a rate is set and while no
/// session runs; frames that don't fit are dropped and counted there.
/// Real-time safe. Returns 0 once a write panicked.
///
/// # Safety
///
/// `input` is null or a live input handle, written by one thread at a
/// time; `samples` is null or points to `frames` × 2 aligned floats that
/// nothing writes during the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn asc_cast_input_write(
    input: *mut AscCastInput,
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
    // borrow only `feed`, which no other call touches.
    let (requested, closing, shared, feed) = unsafe {
        (
            &(*input).requested,
            &(*input).closing,
            &*(*input).shared,
            &mut (*input).feed,
        )
    };
    if shared.dead.load(Ordering::Acquire) {
        return 0;
    }
    let requested = requested.load(Ordering::Acquire);
    let closing = closing.load(Ordering::Acquire);
    shared.writing.store(true, Ordering::SeqCst);
    let written = panic::catch_unwind(AssertUnwindSafe(|| {
        injected_panic();
        feed.write(block, requested, closing, shared)
    }));
    shared.writing.store(false, Ordering::SeqCst);
    written.unwrap_or_else(|_| {
        shared.panicked();
        0
    })
}

/// Frees an input handle. Sets `*input` to null.
///
/// # Safety
///
/// `input` is null or points to a pointer that is null or was returned by
/// [`asc_cast_take_input`] and not destroyed yet; no other call on that
/// handle runs now or later.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn asc_cast_input_destroy(input: *mut *mut AscCastInput) {
    if input.is_null() {
        return;
    }
    // SAFETY: by the caller's contract `input` points to a readable and
    // writable handle pointer.
    let handle = unsafe { input.replace(ptr::null_mut()) };
    if handle.is_null() {
        return;
    }
    // SAFETY: from Box::into_raw in asc_cast_take_input, destroyed once,
    // with no call in progress.
    let freed = panic::catch_unwind(AssertUnwindSafe(|| drop(unsafe { Box::from_raw(handle) })));
    drop(freed);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, TcpListener};
    use std::time::Instant;

    use audiosync_cast::discovery::ServiceRecord;

    thread_local! {
        /// The next guarded call on this thread panics inside its guard.
        pub(super) static PANIC_NEXT: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    }

    fn panic_in_next_call() {
        PANIC_NEXT.with(|next| next.set(true));
    }

    // covers: S249, S250
    #[test]
    fn the_zoned_cast_levels_cross_the_boundary_as_the_sender_makes_them() {
        assert!((asc_cast_zone_level(0.5, 0.5, 0.8) - 0.2).abs() < 1e-12);
        assert!((asc_cast_zone_own_level(0.2, 0.5, 0.5, 0.9) - 0.8).abs() < 1e-12);
        assert_eq!(asc_cast_zone_own_level(0.2, 1.0, 0.0, 0.7), 0.7);
        assert!((asc_cast_zone_start_own_level(0.5, 0.5, false, 0.0, 0.2) - 0.8).abs() < 1e-12);
        for (master, own, reported) in [(0.5, 0.8, 0.2), (0.37, 0.91, 0.3)] {
            assert_eq!(
                asc_cast_zone_level(master, 1.0, own).to_bits(),
                asc_cast_device_level(master, own).to_bits()
            );
            assert_eq!(
                asc_cast_zone_own_level(reported, master, 1.0, own).to_bits(),
                asc_cast_own_level(reported, master, own).to_bits()
            );
            assert_eq!(
                asc_cast_zone_start_own_level(master, 1.0, false, 0.0, reported).to_bits(),
                asc_cast_start_own_level(master, false, 0.0, reported).to_bits()
            );
        }
    }

    /// A browser that hands over a fixed list of events, then nothing; one
    /// that `dies` reports its daemon gone once they are taken.
    struct Script(VecDeque<BrowseEvent>, bool);

    impl Browse for Script {
        fn poll_event(&mut self) -> Option<BrowseEvent> {
            self.0.pop_front()
        }

        fn is_dead(&self) -> bool {
            self.1 && self.0.is_empty()
        }
    }

    fn quiet() -> Option<Box<dyn Browse>> {
        Some(Box::new(Script(VecDeque::new(), false)))
    }

    fn create() -> *mut AscCast {
        let handle = create_with("", quiet);
        assert!(!handle.is_null());
        handle
    }

    fn inside<'a>(handle: *mut AscCast) -> &'a mut AscCast {
        // SAFETY: a live handle, used by this thread only.
        unsafe { &mut *handle }
    }

    fn browse(handle: *mut AscCast, events: Vec<BrowseEvent>, dies: bool) {
        inside(handle).browser = Some(Box::new(Script(events.into(), dies)));
    }

    const KITCHEN: &str = "0123456789abcdef0123456789abcdef";

    fn record(id: &str, name: &str, port: u16, ip: Ipv4Addr) -> ServiceRecord {
        ServiceRecord {
            fullname: format!("{name}-{id}._googlecast._tcp.local."),
            port,
            txt: vec![
                format!("id={id}").into_bytes(),
                format!("fn={name}").into_bytes(),
                b"md=Chromecast Audio".to_vec(),
            ],
            addresses: vec![IpAddr::V4(ip)],
        }
    }

    fn text(pointer: *const c_char) -> String {
        assert!(!pointer.is_null());
        // SAFETY: a string the handle owns, read before the next call.
        unsafe { CStr::from_ptr(pointer) }
            .to_str()
            .unwrap()
            .to_owned()
    }

    fn blank_device() -> AscCastDevice {
        AscCastDevice {
            event: -1,
            id: ptr::null(),
            name: ptr::null(),
            model: ptr::null(),
            flags: 0,
            former: ptr::null(),
        }
    }

    fn devices(handle: *mut AscCast) -> Vec<Change> {
        let mut found = Vec::new();
        let mut device = blank_device();
        // SAFETY: a live handle, used by this thread only.
        while unsafe { asc_cast_next_device(handle, &mut device) } {
            found.push(Change {
                event: device.event,
                id: text(device.id),
                name: text(device.name),
                model: text(device.model),
                flags: device.flags,
                former: text(device.former),
            });
        }
        found
    }

    fn blank_event() -> AscCastEvent {
        AscCastEvent {
            kind: -1,
            attempt: 0,
            id: ptr::null(),
            reason: 0,
            error: 0,
            format: 0,
            retry_ms: 0,
            level: 0.0,
            muted: false,
            volume_fixed: false,
            step: 0.0,
            drift_ms: 0.0,
            queued_ms: 0.0,
            resync_offered: false,
            relaunched: false,
            detail: ptr::null(),
        }
    }

    fn waiting(handle: *mut AscCast) -> Vec<Pending> {
        let mut found = Vec::new();
        let mut event = blank_event();
        // SAFETY: a live handle, used by this thread only.
        while unsafe { asc_cast_next_event(handle, &mut event) } {
            let mut pending = Pending::new(event.kind);
            pending.attempt = event.attempt;
            pending.id = text(event.id);
            pending.reason = event.reason;
            pending.error = event.error;
            pending.format = event.format;
            pending.retry_ms = event.retry_ms;
            pending.detail = text(event.detail);
            found.push(pending);
        }
        found
    }

    /// Every event until one matches `done`, within 10 s.
    fn events_until(handle: *mut AscCast, done: impl Fn(&Pending) -> bool) -> Vec<Pending> {
        let until = Instant::now() + Duration::from_secs(10);
        let mut seen = Vec::new();
        while Instant::now() < until {
            let found = waiting(handle);
            let matched = found.iter().any(&done);
            seen.extend(found);
            if matched {
                return seen;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("no matching event in 10 s: {seen:?}");
    }

    /// A loopback port nothing listens on.
    fn closed_port() -> u16 {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        listener.local_addr().unwrap().port()
    }

    /// A device added by address at `address`, as its ID.
    fn add(handle: *mut AscCast, address: &str) -> String {
        let text = CString::new(address).unwrap();
        // SAFETY: a live handle, used by this thread only.
        assert!(unsafe { asc_cast_add_device(handle, text.as_ptr()) });
        format!("manual:{address}")
    }

    fn start(handle: *mut AscCast, id: &str) -> (u64, i32) {
        let id = CString::new(id).unwrap();
        let mut refused = -1;
        // SAFETY: a live handle, used by this thread only.
        let attempt = unsafe { asc_cast_start(handle, id.as_ptr(), &mut refused) };
        (attempt, refused)
    }

    fn destroy(mut handle: *mut AscCast) {
        // SAFETY: destroyed once, by this thread.
        unsafe { asc_cast_destroy(&mut handle) };
        assert!(handle.is_null());
    }

    #[test]
    fn null_handles_and_bad_arguments_are_harmless() {
        let mut device = blank_device();
        let mut event = blank_event();
        let mut refused = -1;
        let mut no_handle: *mut AscCast = ptr::null_mut();
        let mut no_input: *mut AscCastInput = ptr::null_mut();
        let samples = [0.0f32; 4];
        let kitchen = c"0123456789abcdef0123456789abcdef";
        // SAFETY: every call takes a null handle, which it must ignore.
        unsafe {
            asc_cast_destroy(ptr::null_mut());
            asc_cast_destroy(&mut no_handle);
            assert!(!asc_cast_start_discovery(no_handle));
            asc_cast_stop_discovery(no_handle);
            assert!(!asc_cast_add_device(no_handle, c"127.0.0.1".as_ptr()));
            assert!(!asc_cast_remove_device(
                no_handle,
                c"manual:127.0.0.1:8009".as_ptr()
            ));
            assert!(!asc_cast_next_device(no_handle, &mut device));
            assert!(!asc_cast_next_event(no_handle, &mut event));
            let id = c"manual:127.0.0.1:8009".as_ptr();
            assert_eq!(asc_cast_start(no_handle, id, &mut refused), 0);
            assert_eq!(refused, CAST_REFUSED_CLOSED);
            asc_cast_stop(no_handle);
            assert!(!asc_cast_set_volume(no_handle, 0.5));
            assert!(!asc_cast_set_muted(no_handle, true));
            assert!(!asc_cast_step_volume(no_handle, true));
            assert!(!asc_cast_set_metadata(
                no_handle,
                ptr::null(),
                ptr::null(),
                ptr::null()
            ));
            assert!(!asc_cast_set_artwork(no_handle, ptr::null(), 0));
            assert!(!asc_cast_resync(no_handle));
            assert!(!asc_cast_set_offset(no_handle, kitchen.as_ptr(), 10.0));
            assert_eq!(asc_cast_offset_ms(no_handle, kitchen.as_ptr()), -1.0);
            assert!(asc_cast_memory_json(no_handle).is_null());
            assert!(asc_cast_take_input(no_handle).is_null());
            assert!(!asc_cast_input_set_rate(no_input, 48_000.0));
            asc_cast_input_request_fade_out(no_input);
            assert_eq!(asc_cast_input_write(no_input, samples.as_ptr(), 2), 0);
            asc_cast_input_destroy(ptr::null_mut());
            asc_cast_input_destroy(&mut no_input);
        }
        assert_eq!(device.event, -1);
        assert_eq!(event.kind, -1);

        let handle = create();
        // SAFETY: a live handle, used by this thread only; NULL outputs and
        // bad texts must be refused.
        unsafe {
            assert!(!asc_cast_next_device(handle, ptr::null_mut()));
            assert!(!asc_cast_next_event(handle, ptr::null_mut()));
            assert!(!asc_cast_add_device(handle, ptr::null()));
            assert!(!asc_cast_add_device(handle, c"not an address".as_ptr()));
            assert!(!asc_cast_remove_device(handle, ptr::null()));
            assert!(!asc_cast_remove_device(handle, c"\xff".as_ptr()));
            assert!(!asc_cast_set_volume(handle, f64::NAN));
            assert!(!asc_cast_set_artwork(handle, ptr::null(), 3));
            assert!(!asc_cast_set_metadata(
                handle,
                c"\xff".as_ptr(),
                ptr::null(),
                ptr::null()
            ));
            assert!(asc_cast_set_metadata(
                handle,
                c"Song".as_ptr(),
                c"".as_ptr(),
                ptr::null()
            ));
            assert!(asc_cast_set_artwork(handle, [0xffu8, 0xd8].as_ptr(), 2));
            assert!(asc_cast_set_artwork(handle, ptr::null(), 0));
            assert!(!asc_cast_set_offset(handle, ptr::null(), 10.0));
            assert_eq!(asc_cast_offset_ms(handle, ptr::null()), -1.0);
            assert_eq!(asc_cast_start(handle, ptr::null(), &mut refused), 0);
            assert_eq!(refused, CAST_REFUSED_INVALID);
            assert_eq!(asc_cast_start(handle, ptr::null(), ptr::null_mut()), 0);
        }
        assert_eq!(inside(handle).metadata.title.as_deref(), Some("Song"));
        assert_eq!(inside(handle).metadata.artist, None);
        assert_eq!(inside(handle).artwork, None);
        // Nothing plays: volume, mute, step and Resync send nothing.
        // SAFETY: as above.
        unsafe {
            assert!(!asc_cast_set_volume(handle, 0.5));
            assert!(!asc_cast_set_muted(handle, false));
            assert!(!asc_cast_step_volume(handle, false));
            assert!(!asc_cast_resync(handle));
        }
        assert!(waiting(handle).is_empty());
        destroy(handle);
    }

    #[test]
    fn a_start_on_an_unknown_or_unlisted_device_is_refused_at_once() {
        let handle = create();
        for id in [
            KITCHEN,
            "manual:127.0.0.1:8009",
            "kitchen",
            "manual:nowhere",
            "",
        ] {
            assert_eq!(start(handle, id), (0, CAST_REFUSED_UNKNOWN), "{id}");
        }
        assert!(waiting(handle).is_empty());
        destroy(handle);
    }

    // CAST_SPEC: C3, C14
    #[test]
    fn discovered_devices_and_groups_cross_the_boundary() {
        let handle = create();
        let group = "fedcba9876543210fedcba9876543210";
        let kitchen = record(KITCHEN, "Kitchen", DEVICE_PORT, Ipv4Addr::new(192, 0, 2, 5));
        let downstairs = record(group, "Downstairs", 32_000, Ipv4Addr::new(192, 0, 2, 6));
        browse(
            handle,
            vec![
                BrowseEvent::Resolved(kitchen.clone()),
                BrowseEvent::Resolved(downstairs),
                BrowseEvent::Removed {
                    fullname: kitchen.fullname.clone(),
                },
            ],
            false,
        );
        let appeared = |id: &str, name: &str, flags: u32| Change {
            event: CAST_APPEARED,
            id: id.to_owned(),
            name: name.to_owned(),
            model: "Chromecast Audio".to_owned(),
            flags,
            former: String::new(),
        };
        let gone = Change {
            event: CAST_GONE,
            id: KITCHEN.to_owned(),
            name: String::new(),
            model: String::new(),
            flags: 0,
            former: String::new(),
        };
        assert_eq!(
            devices(handle),
            [
                appeared(KITCHEN, "Kitchen", 0),
                appeared(group, "Downstairs", CAST_FLAG_GROUP),
                gone
            ]
        );
        let logged: Vec<Pending> = waiting(handle)
            .into_iter()
            .filter(|pending| pending.kind == CAST_EVENT_GROUP_LOG)
            .collect();
        assert_eq!(logged.len(), 1);
        assert!(logged[0].detail.contains(group), "{}", logged[0].detail);
        assert_eq!(start(handle, KITCHEN), (0, CAST_REFUSED_UNKNOWN));
        let (attempt, refused) = start(handle, group);
        assert!(attempt > 0);
        assert_eq!(refused, CAST_REFUSED_NONE);
        // SAFETY: a live handle.
        unsafe { asc_cast_stop(handle) };
        destroy(handle);
    }

    // CAST_SPEC: C95
    #[test]
    fn the_playing_groups_members_cross_the_boundary_matched_to_listed_devices() {
        let handle = create();
        let group = "fedcba9876543210fedcba9876543210";
        let kitchen = record(KITCHEN, "Kitchen", DEVICE_PORT, Ipv4Addr::new(192, 0, 2, 5));
        let downstairs = record(group, "Downstairs", 32_000, Ipv4Addr::new(192, 0, 2, 6));
        browse(
            handle,
            vec![
                BrowseEvent::Resolved(kitchen),
                BrowseEvent::Resolved(downstairs),
            ],
            false,
        );
        devices(handle);
        // SAFETY: a live handle.
        assert_eq!(unsafe { asc_cast_member_count(handle) }, 0);
        let (attempt, _) = start(handle, group);
        assert!(attempt > 0);
        let member = |device_id: &str, name: &str| GroupMember {
            device_id: device_id.to_owned(),
            id: DeviceId::parse(device_id),
            name: Some(name.to_owned()),
        };
        let dashed = "01234567-89AB-CDEF-0123-456789ABCDEF";
        assert_eq!(dashed.replace('-', "").to_ascii_lowercase(), KITCHEN);
        inside(handle).take_in(HostEvent::Session {
            attempt: AttemptId(attempt - 1),
            device: DeviceId::parse(group).unwrap(),
            event: SessionEvent::Members(vec![
                member(dashed, "Kitchen speaker"),
                member("not-a-uuid", "Porch"),
            ]),
        });
        // SAFETY: a live handle.
        assert_eq!(unsafe { asc_cast_member_count(handle) }, 2);
        let mut found = AscCastMember {
            id: ptr::null(),
            name: ptr::null(),
        };
        // SAFETY: a live handle and a writable member.
        assert!(unsafe { asc_cast_member(handle, 0, &mut found) });
        assert_eq!(
            (text(found.id), text(found.name)),
            (KITCHEN.to_owned(), "Kitchen speaker".to_owned())
        );
        // SAFETY: as above.
        assert!(unsafe { asc_cast_member(handle, 1, &mut found) });
        assert_eq!(
            (text(found.id), text(found.name)),
            (String::new(), "Porch".to_owned())
        );
        // SAFETY: as above.
        assert!(!unsafe { asc_cast_member(handle, 2, &mut found) });
        // SAFETY: a null member pointer is refused.
        assert!(!unsafe { asc_cast_member(handle, 0, ptr::null_mut()) });
        let records = inside(handle).attempts[0].group.clone().expect("a group");
        assert_eq!(records.get().members, [IpAddr::from([192, 0, 2, 5])]);
        assert_eq!(
            records.get().group,
            [SocketAddr::new(IpAddr::from([192, 0, 2, 6]), 32_000)]
        );
        // SAFETY: a live handle.
        unsafe { asc_cast_stop(handle) };
        destroy(handle);
    }

    // CAST_SPEC: C18
    #[test]
    fn a_device_added_by_address_is_listed_until_a_browse_finds_its_id() {
        let handle = create();
        add(handle, "192.0.2.5");
        add(handle, "[fe80::1]:8010");
        let manual = |event: i32, id: &str, flags: u32| Change {
            event,
            id: id.to_owned(),
            name: String::new(),
            model: String::new(),
            flags,
            former: String::new(),
        };
        assert_eq!(
            devices(handle),
            [
                manual(CAST_APPEARED, "manual:192.0.2.5:8009", CAST_FLAG_MANUAL),
                manual(CAST_APPEARED, "manual:[fe80::1]:8010", CAST_FLAG_MANUAL),
            ]
        );
        let found = record(KITCHEN, "Kitchen", DEVICE_PORT, Ipv4Addr::new(192, 0, 2, 5));
        browse(handle, vec![BrowseEvent::Resolved(found)], false);
        let seen = devices(handle);
        assert_eq!(seen.len(), 2, "{seen:?}");
        assert_eq!(seen[0], manual(CAST_GONE, "manual:192.0.2.5:8009", 0));
        assert_eq!(
            (seen[1].event, seen[1].id.as_str()),
            (CAST_APPEARED, KITCHEN)
        );
        assert_eq!(seen[1].former, "manual:192.0.2.5:8009", "what it was");
        destroy(handle);
    }

    // CAST_SPEC: C18, C77
    #[test]
    fn a_browse_that_identifies_a_device_added_by_address_keeps_its_offset() {
        const LIVING: &str = "fedcba9876543210fedcba9876543210";
        const PORCH: &str = "00112233445566778899aabbccddeeff";
        let handle = create();
        let kitchen_manual = add(handle, "192.0.2.5:8009");
        let living_manual = add(handle, "192.0.2.6:8009");
        let porch_manual = add(handle, "192.0.2.7:8009");
        let offset = |id: &str| {
            let id = CString::new(id).unwrap();
            // SAFETY: a live handle, used by this thread only.
            unsafe { asc_cast_offset_ms(handle, id.as_ptr()) }
        };
        let set = |id: &str, ms: f64| {
            let id = CString::new(id).unwrap();
            // SAFETY: a live handle, used by this thread only.
            assert!(unsafe { asc_cast_set_offset(handle, id.as_ptr(), ms) });
        };
        set(&kitchen_manual, 240.0);
        set(&living_manual, 300.0);
        set(LIVING, 100.0);
        set(&porch_manual, 240.0);
        set(PORCH, 0.0);
        devices(handle);
        waiting(handle);
        browse(
            handle,
            vec![
                BrowseEvent::Resolved(record(
                    KITCHEN,
                    "Kitchen",
                    DEVICE_PORT,
                    Ipv4Addr::new(192, 0, 2, 5),
                )),
                BrowseEvent::Resolved(record(
                    LIVING,
                    "Living",
                    DEVICE_PORT,
                    Ipv4Addr::new(192, 0, 2, 6),
                )),
                BrowseEvent::Resolved(record(
                    PORCH,
                    "Porch",
                    DEVICE_PORT,
                    Ipv4Addr::new(192, 0, 2, 7),
                )),
            ],
            false,
        );
        let seen = devices(handle);
        assert!(
            seen.iter()
                .any(|change| change.id == KITCHEN && change.former == kitchen_manual),
            "{seen:?}"
        );
        assert_eq!(offset(KITCHEN), 240.0, "moved to the ID");
        assert_eq!(offset(LIVING), 100.0, "the ID's own offset stays");
        assert_eq!(
            offset(PORCH),
            240.0,
            "0 ms, left by an Undo, counts as none"
        );
        assert!(
            waiting(handle)
                .iter()
                .any(|pending| pending.kind == CAST_EVENT_MEMORY_CHANGED),
            "the host stores the moved offset"
        );
        destroy(handle);
    }

    // CAST_SPEC: C18
    #[test]
    fn a_device_added_by_address_can_be_taken_back_until_a_browse_finds_it() {
        let handle = create();
        let id = add(handle, "192.0.2.5:8009");
        add(handle, "192.0.2.6");
        assert_eq!(devices(handle).len(), 2);
        let remove = |id: &str| {
            let id = CString::new(id).unwrap();
            // SAFETY: a live handle, used by this thread only.
            unsafe { asc_cast_remove_device(handle, id.as_ptr()) }
        };
        assert!(remove(&id));
        let seen = devices(handle);
        assert_eq!(seen.len(), 1, "{seen:?}");
        assert_eq!(
            (seen[0].event, seen[0].id.as_str()),
            (CAST_GONE, id.as_str())
        );
        assert!(!remove(&id), "already gone");
        assert!(!remove("manual:192.0.2.9:8009"), "never added");
        assert!(!remove("192.0.2.6"), "an address, not an ID");
        let found = record(KITCHEN, "Kitchen", DEVICE_PORT, Ipv4Addr::new(192, 0, 2, 6));
        browse(handle, vec![BrowseEvent::Resolved(found)], false);
        assert_eq!(devices(handle).len(), 2);
        assert!(!remove("manual:192.0.2.6:8009"), "identified");
        assert!(!remove(KITCHEN), "found by a browse");
        assert!(devices(handle).is_empty());
        let (attempt, refused) = start(handle, &id);
        assert_eq!((attempt, refused), (0, CAST_REFUSED_UNKNOWN));
        destroy(handle);
    }

    // A device whose records go away during a session stays listed as
    // offline, and leaves once its session ends.
    // CAST_SPEC: C17
    #[test]
    fn a_device_in_a_session_stays_listed_offline_until_the_session_ends() {
        let handle = create();
        let kitchen = record(KITCHEN, "Kitchen", DEVICE_PORT, Ipv4Addr::LOCALHOST);
        browse(handle, vec![BrowseEvent::Resolved(kitchen.clone())], false);
        assert_eq!(devices(handle).len(), 1);
        let (attempt, _) = start(handle, KITCHEN);
        assert!(attempt > 0);
        let removed = BrowseEvent::Removed {
            fullname: kitchen.fullname,
        };
        browse(handle, vec![removed], false);
        let seen = devices(handle);
        assert_eq!(seen.len(), 1, "{seen:?}");
        assert_eq!(
            (seen[0].event, seen[0].flags),
            (CAST_CHANGED, CAST_FLAG_OFFLINE)
        );
        // SAFETY: a live handle, used by this thread only.
        unsafe { asc_cast_stop(handle) };
        events_until(handle, |event| event.kind == CAST_EVENT_ENDED);
        let seen = devices(handle);
        assert_eq!(seen.len(), 1, "{seen:?}");
        assert_eq!((seen[0].event, seen[0].id.as_str()), (CAST_GONE, KITCHEN));
        destroy(handle);
    }

    // CAST_SPEC: C2, C80
    #[test]
    fn stop_cancels_a_pending_start_and_every_event_carries_its_attempt() {
        let handle = create();
        let id = add(handle, &format!("127.0.0.1:{}", closed_port()));
        let (first, refused) = start(handle, &id);
        assert_eq!((first, refused), (1, CAST_REFUSED_NONE));
        assert_eq!(start(handle, &id), (0, CAST_REFUSED_BUSY));
        let seen = events_until(handle, |event| event.kind == CAST_EVENT_CONNECT_FAILED);
        let failed = seen.last().unwrap();
        assert_eq!((failed.attempt, failed.id.as_str()), (first, id.as_str()));
        assert_eq!((failed.error, failed.retry_ms), (CAST_ERROR_CONNECT, 1_000));
        assert!(!failed.detail.is_empty());

        // SAFETY: a live handle, used by this thread only.
        unsafe { asc_cast_stop(handle) };
        let (second, _) = start(handle, &id);
        assert_eq!(second, first + 1, "the slot is free right after a stop");
        // SAFETY: as above.
        unsafe { asc_cast_stop(handle) };
        let mut seen = seen;
        seen.extend(events_until(handle, |event| {
            event.kind == CAST_EVENT_ENDED && event.attempt == second
        }));
        let ends: Vec<(u64, i32)> = seen
            .iter()
            .filter(|event| event.kind == CAST_EVENT_ENDED)
            .map(|event| (event.attempt, event.reason))
            .collect();
        assert_eq!(
            ends,
            [(first, CAST_END_DESELECTED), (second, CAST_END_DESELECTED)]
        );
        assert!(
            seen.iter()
                .all(|event| (event.attempt == first || event.attempt == second) && event.id == id),
            "{seen:?}"
        );
        destroy(handle);
    }

    // CAST_SPEC: C77, C84
    #[test]
    fn the_memory_json_round_trips_offsets_and_formats() {
        let flac = "a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1";
        let stored = CString::new(format!(r#"{{"{flac}": {{"format": "flac"}}}}"#)).unwrap();
        // SAFETY: a NUL-terminated string.
        let first = unsafe { asc_cast_create(stored.as_ptr()) };
        assert!(!first.is_null());
        let kitchen = CString::new(KITCHEN).unwrap();
        let manual = c"manual:192.0.2.5:8009";
        // SAFETY: a live handle, used by this thread only.
        let json = unsafe {
            assert_eq!(asc_cast_offset_ms(first, kitchen.as_ptr()), -1.0);
            assert!(asc_cast_set_offset(first, kitchen.as_ptr(), 1_849.6));
            assert!(asc_cast_set_offset(first, manual.as_ptr(), 240.0));
            for refused in [-1.0, 120_001.0, f64::NAN, f64::INFINITY] {
                assert!(
                    !asc_cast_set_offset(first, kitchen.as_ptr(), refused),
                    "{refused}"
                );
            }
            assert!(!asc_cast_set_offset(first, c"kitchen".as_ptr(), 10.0));
            text(asc_cast_memory_json(first))
        };
        destroy(first);
        assert!(json.contains(r#""format":"flac""#), "{json}");

        let json = CString::new(json).unwrap();
        // SAFETY: a NUL-terminated string.
        let second = unsafe { asc_cast_create(json.as_ptr()) };
        assert!(!second.is_null());
        // SAFETY: a live handle, used by this thread only.
        unsafe {
            assert_eq!(asc_cast_offset_ms(second, kitchen.as_ptr()), 1_850.0);
            assert_eq!(asc_cast_offset_ms(second, manual.as_ptr()), 240.0);
            assert_eq!(text(asc_cast_memory_json(second)), json.to_str().unwrap());
        }
        destroy(second);

        for damaged in [c"not json", c"\xff"] {
            // SAFETY: a NUL-terminated string.
            let handle = unsafe { asc_cast_create(damaged.as_ptr()) };
            assert!(!handle.is_null());
            // SAFETY: a live handle, used by this thread only.
            assert_eq!(text(unsafe { asc_cast_memory_json(handle) }), "{}");
            destroy(handle);
        }
    }

    #[test]
    fn manual_devices_have_stable_memory_ids_apart_from_each_other() {
        let a = DeviceKey::Manual("192.0.2.5:8009".parse().unwrap());
        let b = DeviceKey::Manual("192.0.2.6:8009".parse().unwrap());
        assert_eq!(memory_id(a), memory_id(a));
        assert_ne!(memory_id(a), memory_id(b));
        let id = DeviceId::parse(KITCHEN).unwrap();
        assert_eq!(memory_id(DeviceKey::Id(id)), id);
        assert_eq!(device_key(&device_id_text(a)), Some(a));
        assert_eq!(device_key(KITCHEN), Some(DeviceKey::Id(id)));
    }

    // CAST_SPEC: C8
    #[test]
    fn the_input_writes_into_the_running_session_only() {
        let handle = create();
        let id = add(handle, &format!("127.0.0.1:{}", closed_port()));
        // SAFETY: a live handle, used by this thread only.
        let mut input = unsafe { asc_cast_take_input(handle) };
        assert!(!input.is_null());
        // SAFETY: as above.
        let again = unsafe { asc_cast_take_input(handle) };
        assert!(again.is_null(), "handed over once");
        let block = vec![0.25f32; 480 * 2];
        let write = |input: *mut AscCastInput| {
            // SAFETY: a live input, written by this thread only; `block`
            // holds 480 stereo frames.
            unsafe { asc_cast_input_write(input, block.as_ptr(), 480) }
        };
        assert_eq!(write(input), 0, "no rate yet");
        // SAFETY: a live input.
        unsafe {
            assert!(!asc_cast_input_set_rate(input, 1_000.0));
            assert!(asc_cast_input_set_rate(input, 48_000.0));
        }
        assert_eq!(write(input), 0, "no session yet");
        assert!(start(handle, &id).0 > 0);
        let taken: usize = (0..20).map(|_| write(input)).sum();
        assert!(taken > 0);
        // A ring of one second: what doesn't fit is dropped, never waited for.
        for _ in 0..200 {
            write(input);
        }
        assert_eq!(write(input), 0);
        // SAFETY: a live handle, used by this thread only.
        unsafe { asc_cast_stop(handle) };
        events_until(handle, |event| event.kind == CAST_EVENT_ENDED);
        assert!(start(handle, &id).0 > 0);
        assert!(write(input) > 0, "a new session takes audio again");
        // SAFETY: as above.
        unsafe { asc_cast_stop(handle) };
        events_until(handle, |event| event.kind == CAST_EVENT_ENDED);
        assert_eq!(write(input), 0, "an ended session's ring is gone");
        destroy(handle);
        assert_eq!(write(input), 0, "an input outlives its sender");
        // SAFETY: destroyed once, by this thread.
        unsafe { asc_cast_input_destroy(&mut input) };
        assert!(input.is_null());
    }

    // CAST_SPEC: C77
    #[test]
    fn the_hold_back_delays_the_feed_and_a_capture_start_begins_with_silence() {
        // SAFETY: a null handle is allowed.
        let null = unsafe { asc_cast_set_hold_back(ptr::null_mut(), 5.0) };
        assert_eq!(null, 0.0);
        let handle = create();
        // SAFETY: a live handle, used by this thread only.
        unsafe {
            assert_eq!(asc_cast_set_hold_back(handle, 30_000.0), 20_000.0);
            assert_eq!(asc_cast_set_hold_back(handle, f64::NAN), 0.0);
            assert_eq!(asc_cast_set_hold_back(handle, -1.0), 0.0);
        }
        // SAFETY: as above.
        let mut input = unsafe { asc_cast_take_input(handle) };
        assert!(!input.is_null());
        let block = vec![0.25f32; 480 * 2];
        // The first frame the ring gets that isn't silent, after a capture
        // start with the feed held back by `hold_back_ms`.
        let first_sound = |hold_back_ms: f64| {
            let (writer, mut reader) = audio_ring(RING_FRAMES);
            inside(handle).shared.attach(Some(writer));
            // SAFETY: a live handle and input, used by this thread only;
            // `block` holds 480 stereo frames.
            unsafe {
                asc_cast_set_hold_back(handle, hold_back_ms);
                assert!(asc_cast_input_set_rate(input, 48_000.0));
                for _ in 0..10 {
                    asc_cast_input_write(input, block.as_ptr(), 480);
                }
            }
            let mut bytes = Vec::new();
            reader.read_into(&mut bytes, 4800);
            bytes
                .chunks_exact(4)
                .position(|frame| frame.iter().any(|byte| *byte != 0))
                .expect("sound within 100 ms")
        };
        let at_once = first_sound(0.0);
        assert_eq!(first_sound(50.0), at_once + 2400, "50 ms later");
        assert_eq!(
            first_sound(50.0),
            at_once + 2400,
            "a capture start never replays the end of the last one"
        );
        inside(handle).shared.attach(None);
        destroy(handle);
        // SAFETY: destroyed once, by this thread.
        unsafe { asc_cast_input_destroy(&mut input) };
    }

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

    // covers: S205
    #[test]
    fn a_panic_inside_a_call_stays_at_the_boundary_and_kills_the_handle() {
        let handle = create();
        let id = add(handle, "127.0.0.1:9");
        devices(handle);
        panic_in_next_call();
        assert_eq!(start(handle, &id), (0, CAST_REFUSED_CLOSED));
        let seen = waiting(handle);
        assert_eq!(seen.len(), 1, "{seen:?}");
        assert_eq!(seen[0].kind, CAST_EVENT_FAILED);
        assert!(
            is_panic_location(&seen[0].detail, "src/ffi/cast.rs"),
            "{seen:?}"
        );
        assert!(!seen[0].detail.contains("injected"), "{seen:?}");
        // Dead: every call fails, and the failure is told once.
        assert_eq!(start(handle, &id), (0, CAST_REFUSED_CLOSED));
        // SAFETY: a live handle, used by this thread only.
        unsafe {
            assert!(!asc_cast_start_discovery(handle));
            assert!(!asc_cast_add_device(handle, c"192.0.2.7".as_ptr()));
            assert!(asc_cast_memory_json(handle).is_null());
            assert!(asc_cast_take_input(handle).is_null());
        }
        assert!(devices(handle).is_empty());
        assert!(waiting(handle).is_empty());
        destroy(handle);
    }

    #[test]
    fn a_panic_while_taking_an_event_reports_the_failure_at_once() {
        let handle = create();
        panic_in_next_call();
        let seen = waiting(handle);
        assert_eq!(seen.len(), 1, "{seen:?}");
        assert_eq!(seen[0].kind, CAST_EVENT_FAILED);
        destroy(handle);
    }

    #[test]
    fn a_panic_in_an_input_write_stops_the_input_and_fails_its_sender() {
        let handle = create();
        // SAFETY: a live handle, used by this thread only.
        let mut input = unsafe { asc_cast_take_input(handle) };
        let block = [0.0f32; 64];
        // SAFETY: a live input, written by this thread only.
        unsafe {
            assert!(asc_cast_input_set_rate(input, 48_000.0));
            panic_in_next_call();
            assert_eq!(asc_cast_input_write(input, block.as_ptr(), 32), 0);
            assert!(!inside(handle).shared.writing.load(Ordering::SeqCst));
            assert_eq!(asc_cast_input_write(input, block.as_ptr(), 32), 0);
        }
        let seen = waiting(handle);
        assert_eq!(seen.len(), 1, "{seen:?}");
        assert!(
            is_panic_location(&seen[0].detail, "src/ffi/cast.rs"),
            "{seen:?}"
        );
        destroy(handle);
        // SAFETY: destroyed once, by this thread.
        unsafe { asc_cast_input_destroy(&mut input) };
    }

    #[test]
    fn a_worker_that_ended_reports_the_failure_once() {
        let handle = create();
        assert!(inside(handle).host.shutdown());
        let seen = waiting(handle);
        assert_eq!(seen.len(), 1, "{seen:?}");
        assert_eq!(seen[0].kind, CAST_EVENT_FAILED);
        // The panic count is process-wide, so a test panicking in parallel
        // may lend its location as the best guess.
        let detail = seen[0].detail.as_str();
        assert!(
            detail.is_empty() || detail.starts_with("latest panic at "),
            "{detail}"
        );
        assert_eq!(start(handle, KITCHEN), (0, CAST_REFUSED_CLOSED));
        assert!(waiting(handle).is_empty());
        destroy(handle);
    }

    #[test]
    fn dead_discovery_reports_its_devices_gone_and_can_start_again() {
        let handle = create();
        let found = record(KITCHEN, "Kitchen", DEVICE_PORT, Ipv4Addr::new(192, 0, 2, 5));
        browse(handle, vec![BrowseEvent::Resolved(found)], true);
        let seen: Vec<i32> = devices(handle).iter().map(|change| change.event).collect();
        assert_eq!(seen, [CAST_APPEARED, CAST_GONE]);
        let events: Vec<i32> = waiting(handle).iter().map(|event| event.kind).collect();
        assert_eq!(events, [CAST_EVENT_DISCOVERY_STOPPED]);
        // SAFETY: a live handle, used by this thread only.
        unsafe {
            assert!(asc_cast_start_discovery(handle));
            asc_cast_stop_discovery(handle);
        }
        destroy(handle);
    }

    // CAST_SPEC: C88
    #[test]
    fn destroy_returns_within_3_s_while_a_start_retries() {
        let handle = create();
        let id = add(handle, &format!("127.0.0.1:{}", closed_port()));
        assert!(start(handle, &id).0 > 0);
        std::thread::sleep(Duration::from_millis(300));
        let began = Instant::now();
        destroy(handle);
        assert!(
            began.elapsed() < Duration::from_secs(3),
            "{:?}",
            began.elapsed()
        );
    }

    // CAST_SPEC: C88
    #[test]
    fn destroy_stays_within_the_shutdown_bound_while_a_device_never_answers() {
        // Its backlog accepts the connection and nothing answers the TLS
        // handshake, which no quit flag interrupts: the host's 3 s plus 0.5 s.
        let silent = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let handle = create();
        let id = add(handle, &silent.local_addr().unwrap().to_string());
        assert!(start(handle, &id).0 > 0);
        std::thread::sleep(Duration::from_millis(300));
        let began = Instant::now();
        destroy(handle);
        assert!(
            began.elapsed() < Duration::from_millis(3700),
            "{:?}",
            began.elapsed()
        );
    }

    #[test]
    fn calls_return_at_once_while_a_start_waits_on_the_network() {
        let silent = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let handle = create();
        let id = add(handle, &silent.local_addr().unwrap().to_string());
        let began = Instant::now();
        assert!(start(handle, &id).0 > 0);
        // SAFETY: a live handle, used by this thread only.
        unsafe {
            assert!(asc_cast_set_volume(handle, 0.4));
            assert!(asc_cast_set_muted(handle, true));
            assert!(asc_cast_step_volume(handle, true));
            assert!(asc_cast_resync(handle));
            assert!(asc_cast_set_metadata(
                handle,
                c"Song".as_ptr(),
                ptr::null(),
                ptr::null()
            ));
            asc_cast_stop(handle);
        }
        waiting(handle);
        assert!(
            began.elapsed() < Duration::from_millis(500),
            "{:?}",
            began.elapsed()
        );
        destroy(handle);
    }
}
