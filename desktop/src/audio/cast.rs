//! Google Cast speakers, through AudioSync's own sender (`audiosync-cast`).
//!
//! Discovery lists every device the network announces. A Cast group plays
//! as one device, with its own volume and offset (C3, C100, C101), while
//! each member stays a device of its own (C99). One device is on at a
//! time (C2): turning a second one on is refused while one is on, and the
//! host's own refusal of a second session reads the same. A device that is
//! on joins while streaming is on, through [`CastHost`], whose one worker
//! thread makes every network call; the UI thread only sends commands and
//! drains events. Every event carries the attempt that started the session,
//! and events of an attempt that isn't the current one are dropped, so a
//! late `Ended` never turns off a speaker the user switched on again.
//!
//! Cast has no shared clock (C73), so it is kept in step only by its
//! measured offset (C77): while one is on, local outputs wait that long for
//! it ([`CastSpeakers::anchor`]). With an AirPlay speaker chosen as well, the
//! one with the shorter latency is held back to the longer: AirPlay through
//! its speakers' delay, Cast through a delay line in front of its ring
//! ([`CastSpeakers::set_hold_back`]). Local outputs never wait for it to
//! start. Only the user sets the offset, and it is kept per device for the
//! next session; Resync is the user's too (C79).
//!
//! The capture feeds the device through a [`CastLease`] of the one
//! [`CastTap`]: equalized session-rate audio goes through the session's delay
//! line into its ring. Each session has its own ring and delay line, which
//! reach the capture through a bounded channel and come back the same way,
//! so the capture never allocates, frees, locks or blocks (C8).
//!
//! The sender sits behind [`CastLink`], so this logic is tested with a fake;
//! [`HostLink`] is the real one.
//!
//! A speaker the user adds by address is listed under that address until a
//! browse finds its `id` (C18). Until then its session and offset are kept
//! under an ID that follows from the address ([`CastAddress::device_id`]),
//! and a host name is looked up on a thread of its own.

use std::collections::BTreeMap;
use std::io;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use audiosync_cast::audio::{AudioReader, AudioWriter, Codec, audio_ring};
use audiosync_cast::discovery::mdns::MdnsBrowser;
use audiosync_cast::discovery::reach::Reachability;
use audiosync_cast::discovery::{
    Announcements, BrowseEvent, DeviceId, DeviceKey, DeviceList, Entry, ListEvent,
};
use audiosync_cast::group::{GroupMember, GroupRecords};
use audiosync_cast::host::{
    AttemptId, CastHost, CastTarget, DeviceMemory, HostEvent, StartRefused,
};
use audiosync_cast::media::{MediaError, Metadata};
use audiosync_cast::session::{SessionEnd, SessionError, SessionEvent};
use audiosync_cast::status::{
    ControlType, DeviceVolume, NO_ZONE_LEVEL, answers_volume, device_level, own_level,
    start_own_level,
};
use audiosync_core::fade::Fade;
use audiosync_core::policy::delay::MAX_HOLD_BACK_MS;

use super::SESSION_RATE;
use super::backend::{
    BackendEvent, CastDiscovery, CastGroup, CastInfo, CastMembers, CastProblem, CastStatus,
};
use super::cast_address::{self, CastAddress};
use crate::debug_log::DebugLog;

/// The ring between the capture and a Cast session: 2 s at the session
/// rate, beyond the sender's own 2 s queue (C64).
const RING_FRAMES: usize = SESSION_RATE as usize * 2;

/// Writers in flight between the UI thread and the capture, each way.
const HANDOFF: usize = 8;

/// The longest hold-back of the Cast feed, in frames: the ceiling of L.
const MAX_HOLD_BACK_FRAMES: usize = MAX_HOLD_BACK_MS as usize * SESSION_RATE as usize / 1000;

/// The smallest page size Windows maps memory in.
const PAGE_BYTES: usize = 4096;

/// The volume step of a device that reports none, 0 to 100.
const DEFAULT_VOLUME_STEP: f64 = 1.0;

/// Discovery answers handled per UI tick, so a flood of mDNS answers
/// cannot stall the UI thread; the rest wait for the next tick (C106).
const MAX_BROWSE_PER_POLL: usize = 256;

/// What a Cast device shows of the playing track (C53): only what the
/// track itself has, never a label or the app's icon in place of a missing
/// field.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CastTrack {
    /// Title, artist and album; `images` stays empty, the sender fills it.
    pub metadata: Metadata,
    /// PNG or JPEG of at most 1280 by 720.
    pub artwork: Option<Arc<[u8]>>,
}

/// The step the volume keys move a device by, 0 to 100: the device's own
/// `stepInterval` (C69), or [`DEFAULT_VOLUME_STEP`] without a usable one.
/// Rounded to a whole level and at least 1, so a tiny step still moves the
/// slider and an odd one like 0.07 lands on whole percents.
pub fn volume_step(volume: Option<&DeviceVolume>) -> f64 {
    volume
        .and_then(|volume| volume.step_interval)
        .filter(|step| step.is_finite() && *step > 0.0 && *step <= 1.0)
        .map_or(DEFAULT_VOLUME_STEP, |step| (step * 100.0).round().max(1.0))
}

/// Interleaved stereo frames held back by a delay that may change while it
/// runs: a change fades out, moves the read point and fades in, so it
/// never clicks.
struct DelayLine {
    buffer: Box<[f32]>,
    /// The frame written next.
    write: usize,
    /// The delay in frames; `None` until the first frame, which takes the
    /// target at once since only silence is behind it.
    delay: Option<usize>,
    fade: Fade,
}

impl DelayLine {
    /// Allocates; on the UI thread only.
    fn new(max_frames: usize) -> DelayLine {
        let mut buffer = vec![0.0_f32; (max_frames + 1) * 2].into_boxed_slice();
        // Zeroed memory is mapped on first touch: touch each page here so the
        // capture thread never takes the page fault.
        let last = buffer.len() - 1;
        let stride = PAGE_BYTES / size_of::<f32>();
        for index in (0..last).step_by(stride).chain([last]) {
            // SAFETY: `index` is in bounds, so the pointer is valid and aligned.
            unsafe { std::ptr::write_volatile(&raw mut buffer[index], 0.0) };
        }
        DelayLine {
            buffer,
            write: 0,
            delay: None,
            fade: Fade::opened(SESSION_RATE),
        }
    }

    /// Takes in the whole frames of `input` and hands as many frames, held
    /// back by `target` frames, to `out`. Never allocates.
    fn process(&mut self, input: &[f32], target: usize, mut out: impl FnMut(&[f32])) {
        let capacity = self.buffer.len() / 2;
        let target = target.min(capacity - 1);
        let mut chunk = [0.0_f32; 512];
        for piece in input.chunks(chunk.len()) {
            let frames = piece.len() / 2;
            for (frame, held) in piece
                .as_chunks::<2>()
                .0
                .iter()
                .zip(chunk.as_chunks_mut::<2>().0)
            {
                let delay = *self.delay.get_or_insert(target);
                if delay == target {
                    self.fade.fade_in();
                } else if self.fade.is_closed() {
                    self.delay = Some(target);
                    self.fade.fade_in();
                } else {
                    self.fade.fade_out();
                }
                let delay = self.delay.unwrap_or(target);
                let write = self.write * 2;
                self.buffer[write..write + 2].copy_from_slice(frame.as_slice());
                let read = (self.write + capacity - delay) % capacity * 2;
                let gain = self.fade.next_gain();
                held[0] = self.buffer[read] * gain;
                held[1] = self.buffer[read + 1] * gain;
                self.write = (self.write + 1) % capacity;
            }
            out(&chunk[..frames * 2]);
        }
    }
}

/// A Cast session's writer behind its delay line; built and freed on the
/// UI thread only.
struct CastWriter {
    audio: AudioWriter,
    delay: DelayLine,
}

impl CastWriter {
    fn new(audio: AudioWriter) -> CastWriter {
        CastWriter {
            audio,
            delay: DelayLine::new(MAX_HOLD_BACK_FRAMES),
        }
    }
}

/// The capture's end of the Cast feed.
pub struct CastTap {
    inbox: Receiver<Option<CastWriter>>,
    outbox: SyncSender<CastWriter>,
    writer: Option<CastWriter>,
    /// A writer the full outbox couldn't take yet; kept, never freed here.
    retired: Option<CastWriter>,
    /// How many frames the feed is held back (C77), set by the UI thread.
    hold_back: Arc<AtomicUsize>,
}

impl CastTap {
    /// Takes the newest writer the UI thread handed over, then writes the
    /// first `frames` frames of interleaved stereo `block` through it, held
    /// back as the UI thread asks. Never allocates, frees, locks or blocks
    /// (C8).
    pub fn push(&mut self, block: &[f32], frames: usize) {
        self.take_handoffs();
        let target = self.hold_back.load(Ordering::Relaxed);
        if let Some(CastWriter { audio, delay }) = self.writer.as_mut() {
            delay.process(&block[..(frames * 2).min(block.len())], target, |held| {
                audio.write_f32(held);
            });
        }
    }

    /// Every old writer goes back to the UI thread to be freed there; while
    /// the outbox is full the tap stops taking handoffs instead.
    fn take_handoffs(&mut self) {
        loop {
            if let Some(old) = self.retired.take()
                && let Err(TrySendError::Full(old) | TrySendError::Disconnected(old)) =
                    self.outbox.try_send(old)
            {
                self.retired = Some(old);
                return;
            }
            let Ok(next) = self.inbox.try_recv() else {
                return;
            };
            self.retired = std::mem::replace(&mut self.writer, next);
        }
    }

    /// On the UI thread, with the tap in its slot: drops every writer it
    /// holds or waits for, and takes `writer`.
    fn replace_all(&mut self, writer: Option<CastWriter>) {
        while self.inbox.try_recv().is_ok() {}
        self.retired = None;
        self.writer = writer;
    }
}

/// Where the one [`CastTap`] waits between capture sessions.
#[derive(Clone)]
pub struct CastFeedSlot {
    tap: Arc<Mutex<Option<CastTap>>>,
}

impl CastFeedSlot {
    /// Lends the tap to a capture; `None` while another capture holds it.
    pub fn lease(&self) -> Option<CastLease> {
        let tap = self
            .tap
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()?;
        Some(CastLease {
            tap: Some(tap),
            slot: Arc::clone(&self.tap),
        })
    }

    /// Whether the tap is in the slot, lent to no capture.
    pub fn is_free(&self) -> bool {
        self.tap
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .is_some()
    }
}

/// The tap, lent to one capture; dropping it puts the tap back.
pub struct CastLease {
    tap: Option<CastTap>,
    slot: Arc<Mutex<Option<CastTap>>>,
}

impl CastLease {
    /// See [`CastTap::push`].
    pub fn push(&mut self, block: &[f32], frames: usize) {
        if let Some(tap) = self.tap.as_mut() {
            tap.push(block, frames);
        }
    }
}

impl Drop for CastLease {
    fn drop(&mut self) {
        if let Some(tap) = self.tap.take() {
            *self.slot.lock().unwrap_or_else(PoisonError::into_inner) = Some(tap);
        }
    }
}

/// The UI thread's end of the Cast feed.
pub struct CastFeed {
    to_tap: SyncSender<Option<CastWriter>>,
    from_tap: Receiver<CastWriter>,
    slot: CastFeedSlot,
    /// The newest writer (or none) the full handoff couldn't take yet.
    waiting: Option<Option<CastWriter>>,
    /// The tap's hold-back, in frames.
    hold_back: Arc<AtomicUsize>,
}

impl CastFeed {
    pub fn new() -> CastFeed {
        CastFeed::with_handoff(HANDOFF)
    }

    fn with_handoff(handoff: usize) -> CastFeed {
        let (to_tap, inbox) = mpsc::sync_channel(handoff);
        let (outbox, from_tap) = mpsc::sync_channel(handoff);
        let hold_back = Arc::new(AtomicUsize::new(0));
        let tap = CastTap {
            inbox,
            outbox,
            writer: None,
            retired: None,
            hold_back: Arc::clone(&hold_back),
        };
        CastFeed {
            to_tap,
            from_tap,
            slot: CastFeedSlot {
                tap: Arc::new(Mutex::new(Some(tap))),
            },
            waiting: None,
            hold_back,
        }
    }

    /// Holds the feed back by `frames` from now on, up to the ceiling of L;
    /// returns whether that changed.
    fn set_hold_back(&self, frames: usize) -> bool {
        let frames = frames.min(MAX_HOLD_BACK_FRAMES);
        self.hold_back.swap(frames, Ordering::Relaxed) != frames
    }

    pub fn slot(&self) -> &CastFeedSlot {
        &self.slot
    }

    /// Gives the capture `writer` (or none) for the audio from now on.
    /// Returns false when it has to wait for a later [`CastFeed::flush`]:
    /// the handoff is full and the tap is lent to a capture that hasn't
    /// pushed since.
    pub(crate) fn hand(&mut self, writer: Option<AudioWriter>) -> bool {
        self.waiting = Some(writer.map(CastWriter::new));
        self.flush()
    }

    /// Frees the writers the capture gave back, and hands over the one
    /// waiting; returns whether none waits anymore.
    fn flush(&mut self) -> bool {
        while self.from_tap.try_recv().is_ok() {}
        let Some(writer) = self.waiting.take() else {
            return true;
        };
        let Err(TrySendError::Full(writer) | TrySendError::Disconnected(writer)) =
            self.to_tap.try_send(writer)
        else {
            return true;
        };
        let mut slot = self.slot.tap.lock().unwrap_or_else(PoisonError::into_inner);
        let Some(tap) = slot.as_mut() else {
            drop(slot);
            self.waiting = Some(writer);
            return false;
        };
        tap.replace_all(writer);
        drop(slot);
        while self.from_tap.try_recv().is_ok() {}
        true
    }
}

/// The Cast sender, as [`CastSpeakers`] uses it.
pub trait CastLink: Send {
    fn start(
        &mut self,
        target: CastTarget,
        rate: u32,
        track: CastTrack,
        reader: AudioReader,
    ) -> Result<AttemptId, StartRefused>;
    fn stop(&mut self);
    /// 0.0 to 1.0.
    fn set_volume(&mut self, level: f64);
    fn set_muted(&mut self, muted: bool);
    /// Kept for the next `LOAD`; the playing stream is not reloaded (C54).
    fn set_track(&mut self, track: CastTrack);
    fn resync(&mut self);
    fn set_offset(&mut self, device: DeviceId, offset: Duration);
    fn memory(&self) -> DeviceMemory;
    fn poll_event(&mut self) -> Option<HostEvent>;
    fn poll_browse(&mut self) -> Option<BrowseEvent>;
    /// The device announced itself: a reconnect to it goes at once (C81).
    fn announced(&mut self, device: DeviceId);
    fn discovery(&self) -> CastDiscovery;
    /// Tears down whatever runs within C88's budget; the one call that
    /// waits. Returns whether it finished in time.
    fn shutdown(&mut self) -> bool;
}

/// The real sender: [`CastHost`] and a live mDNS browser.
#[cfg_attr(
    not(os_backend),
    expect(dead_code, reason = "the OS backend's startup starts it")
)]
pub struct HostLink {
    host: CastHost,
    browser: Option<MdnsBrowser>,
}

impl HostLink {
    /// Starts the sender's worker with `memory`, and discovery. A browser
    /// that can't start leaves the sender running with nothing listed; the
    /// reason comes back for the log.
    #[cfg_attr(
        not(os_backend),
        expect(dead_code, reason = "the OS backend's startup starts it")
    )]
    pub fn start(memory: DeviceMemory) -> io::Result<(HostLink, Option<String>)> {
        let host = CastHost::new(memory)?;
        let (browser, error) = match MdnsBrowser::new() {
            Ok(browser) => (Some(browser), None),
            Err(error) => (None, Some(error.to_string())),
        };
        Ok((HostLink { host, browser }, error))
    }
}

impl CastLink for HostLink {
    fn start(
        &mut self,
        target: CastTarget,
        rate: u32,
        track: CastTrack,
        reader: AudioReader,
    ) -> Result<AttemptId, StartRefused> {
        let artwork = track.artwork.map(|bytes| bytes.to_vec());
        self.host
            .start_with_artwork(target, rate, track.metadata, artwork, reader)
    }

    fn stop(&mut self) {
        self.host.stop();
    }

    fn set_volume(&mut self, level: f64) {
        self.host.set_volume(level);
    }

    fn set_muted(&mut self, muted: bool) {
        self.host.set_muted(muted);
    }

    fn set_track(&mut self, track: CastTrack) {
        self.host.set_metadata(track.metadata);
        if let Some(artwork) = track.artwork {
            self.host.set_artwork(artwork.to_vec());
        }
    }

    fn resync(&mut self) {
        self.host.resync();
    }

    fn set_offset(&mut self, device: DeviceId, offset: Duration) {
        self.host.set_offset(device, offset);
    }

    fn memory(&self) -> DeviceMemory {
        self.host.memory()
    }

    fn poll_event(&mut self) -> Option<HostEvent> {
        self.host.poll_event()
    }

    fn poll_browse(&mut self) -> Option<BrowseEvent> {
        self.browser.as_mut()?.poll_event()
    }

    fn announced(&mut self, device: DeviceId) {
        self.host.announced(device);
    }

    fn discovery(&self) -> CastDiscovery {
        match &self.browser {
            Some(browser) if !browser.is_dead() => CastDiscovery::Browsing,
            _ => CastDiscovery::Failed,
        }
    }

    fn shutdown(&mut self) -> bool {
        self.browser = None;
        self.host.shutdown()
    }
}

/// How long a host name that didn't resolve waits for the next lookup.
const LOOKUP_RETRY: Duration = Duration::from_secs(30);

/// Looks a host name up, IPv4 only; it may block.
type Resolver = fn(&str, u16) -> io::Result<SocketAddr>;

/// A finished lookup: the address's text, and where it is.
type Lookup = (String, io::Result<SocketAddr>);

/// A speaker the user added by address (C18).
struct Manual {
    address: CastAddress,
    id: DeviceId,
    /// Where it is: known at once for an IPv4 address, after a lookup for a
    /// host name.
    resolved: Option<SocketAddr>,
    looking_up: bool,
    /// When a lookup that failed is tried again.
    retry_at: Option<Instant>,
}

/// The session of the device that is on.
#[derive(Clone, Copy, Debug)]
struct Attempt {
    id: AttemptId,
    device: DeviceId,
    status: CastStatus,
    /// The format the device plays, once the session reports it.
    codec: Option<Codec>,
    /// Whether the session reported the device's volume yet (C104).
    heard_volume: bool,
}

/// The Cast side of the backend.
pub struct CastSpeakers {
    link: Option<Box<dyn CastLink>>,
    feed: CastFeed,
    list: DeviceList,
    announcements: Announcements,
    /// The one device the user turned on.
    enabled: Option<DeviceId>,
    /// Each device's volume as its latest status reports it.
    volumes: BTreeMap<DeviceId, DeviceVolume>,
    /// An own level the user set that no status has answered yet, 0.0 to
    /// 1.0.
    requested: Option<(DeviceId, f64)>,
    /// Each device's own level, 0.0 to 1.0, which the master scales (C104).
    levels: BTreeMap<DeviceId, f64>,
    /// The master, 0.0 to 1.0.
    master: f64,
    /// The zone level, 0.0 to 1.0, of each device in a zone (S249); one not
    /// here is in no zone.
    zones: BTreeMap<DeviceId, f64>,
    /// The levels sent in this session that no status has answered yet,
    /// oldest first; all of them, since the sender paces a drag later (C70)
    /// and may send any of them.
    unanswered: Vec<f64>,
    attempt: Option<Attempt>,
    /// How long the feed waits for L, in ms (C77).
    hold_back_ms: f64,
    /// The addresses of the group in a session, kept current from
    /// discovery (C90, C103).
    records: Option<GroupRecords>,
    /// Each group's members as its latest session listed them (C94).
    members: BTreeMap<DeviceId, Vec<GroupMember>>,
    streaming: bool,
    /// The playing track; `None` while it is being read, which holds a
    /// start back so the first `LOAD` names it (C54).
    track: Option<CastTrack>,
    /// What the session in progress was given last.
    sent: Option<CastTrack>,
    log: DebugLog,
    pending: Vec<BackendEvent>,
    manual: Vec<Manual>,
    lookups: (mpsc::Sender<Lookup>, Receiver<Lookup>),
    resolver: Resolver,
    /// Whether each speaker added by address answers: it is listed offline
    /// until a check reaches it.
    reach: Reachability,
}

impl CastSpeakers {
    pub fn new(link: Box<dyn CastLink>, log: DebugLog) -> CastSpeakers {
        CastSpeakers {
            link: Some(link),
            feed: CastFeed::new(),
            list: DeviceList::new(),
            announcements: Announcements::default(),
            enabled: None,
            volumes: BTreeMap::new(),
            requested: None,
            levels: BTreeMap::new(),
            master: 1.0,
            zones: BTreeMap::new(),
            unanswered: Vec::new(),
            attempt: None,
            hold_back_ms: 0.0,
            records: None,
            members: BTreeMap::new(),
            streaming: false,
            track: Some(CastTrack::default()),
            sent: None,
            log,
            pending: Vec::new(),
            manual: Vec::new(),
            lookups: mpsc::channel(),
            resolver: cast_address::resolve,
            #[cfg(not(test))]
            reach: Reachability::default(),
            // Unit tests never reach the network; one that needs an answer sets a probe.
            #[cfg(test)]
            reach: Reachability::new(|_| false),
        }
    }

    #[cfg(test)]
    pub fn set_resolver(&mut self, resolver: Resolver) {
        self.resolver = resolver;
    }

    #[cfg(test)]
    pub fn set_probe(&mut self, probe: audiosync_cast::discovery::reach::Probe) {
        self.reach = Reachability::new(probe);
    }

    /// Adds a speaker by address (C18); one added before stays as it is.
    pub fn add_address(&mut self, address: CastAddress) {
        if self.manual.iter().any(|manual| manual.address == address) {
            return;
        }
        self.log
            .log(|| format!("Cast: {} added by address", address.text()));
        let socket = address.socket_address();
        self.manual.push(Manual {
            id: address.device_id(),
            address,
            resolved: socket,
            looking_up: false,
            retry_at: None,
        });
        if socket.is_none() {
            self.look_up(self.manual.len() - 1);
        }
        self.keep_manual_listed();
        self.check_manual();
        self.pending.push(BackendEvent::CastChanged);
    }

    /// Takes in what the checks of the speakers added by address found,
    /// and starts the ones due.
    fn check_manual(&mut self) {
        for change in self.reach.poll(&mut self.list, Instant::now()) {
            if let ListEvent::Updated(key @ DeviceKey::Manual(socket)) = change {
                let online = self.list.get(key).is_some_and(|entry| entry.online);
                self.log.log(|| {
                    let state = if online { "answers" } else { "doesn't answer" };
                    format!("Cast: {socket} {state}")
                });
            }
            self.pending.push(BackendEvent::CastChanged);
        }
    }

    /// Takes back a speaker added by address, off first if it is on. One a
    /// browse found stays listed as the device it is.
    pub fn remove_address(&mut self, address: &CastAddress) {
        let Some(index) = self
            .manual
            .iter()
            .position(|manual| manual.address == *address)
        else {
            return;
        };
        let id = self.manual[index].id;
        if self.enabled == Some(id) {
            self.enabled = None;
            self.sync();
        }
        let manual = self.manual.remove(index);
        if let Some(socket) = manual.resolved
            && !self
                .manual
                .iter()
                .any(|other| other.resolved == Some(socket))
        {
            self.list.remove_manual(socket);
        }
        self.log
            .log(|| format!("Cast: {} removed", manual.address.text()));
        self.pending.push(BackendEvent::CastChanged);
    }

    /// Looks the host name of `self.manual[index]` up on a thread of its own.
    fn look_up(&mut self, index: usize) {
        let manual = &mut self.manual[index];
        manual.looking_up = true;
        let text = manual.address.text();
        let host = manual.address.host().to_owned();
        let port = manual.address.port();
        let sender = self.lookups.0.clone();
        let resolve = self.resolver;
        let spawned = thread::Builder::new()
            .name("cast-lookup".to_owned())
            .spawn(move || {
                let result = resolve(&host, port);
                let _ = sender.send((text, result));
            });
        if let Err(error) = spawned {
            manual.looking_up = false;
            manual.retry_at = Some(Instant::now() + LOOKUP_RETRY);
            self.log
                .log(|| format!("Cast: the lookup thread couldn't start: {error}"));
        }
    }

    /// Takes in finished lookups, and starts again the ones due.
    fn poll_lookups(&mut self) {
        while let Ok((text, result)) = self.lookups.1.try_recv() {
            let Some(manual) = self
                .manual
                .iter_mut()
                .find(|manual| manual.looking_up && manual.address.text() == text)
            else {
                continue;
            };
            manual.looking_up = false;
            match result {
                Ok(socket) => {
                    manual.resolved = Some(socket);
                    self.log.log(|| format!("Cast: {text} is at {socket}"));
                    self.pending.push(BackendEvent::CastChanged);
                    let browsed = self
                        .list
                        .entries()
                        .iter()
                        .find_map(|entry| match entry.key {
                            DeviceKey::Id(id) if entry.addresses.contains(&socket) => Some(id),
                            _ => None,
                        });
                    if let Some(id) = browsed {
                        self.identified(socket, id);
                    }
                }
                Err(error) => {
                    manual.retry_at = Some(Instant::now() + LOOKUP_RETRY);
                    self.log.log(|| {
                        format!(
                            "Cast: {text} couldn't be looked up ({error}); trying again in 30 s"
                        )
                    });
                }
            }
        }
        let now = Instant::now();
        let due: Vec<usize> = (0..self.manual.len())
            .filter(|&index| {
                let manual = &self.manual[index];
                manual.resolved.is_none()
                    && !manual.looking_up
                    && manual.retry_at.is_some_and(|at| at <= now)
            })
            .collect();
        for index in due {
            self.look_up(index);
        }
    }

    /// Lists each speaker added by address whose address no entry has, so
    /// it shows again after the device a browse found is gone.
    fn keep_manual_listed(&mut self) {
        let missing: Vec<SocketAddr> = self
            .manual
            .iter()
            .filter_map(|manual| manual.resolved)
            .filter(|socket| {
                !self
                    .list
                    .entries()
                    .iter()
                    .any(|entry| entry.addresses.contains(socket))
            })
            .collect();
        for socket in missing {
            if let ListEvent::Added(_) = self.list.add_manual(socket) {
                self.pending.push(BackendEvent::CastChanged);
            }
        }
    }

    /// A browse, or a lookup that lands on a browsed address, found the
    /// device a speaker added by address is (C18): one that is on stays on
    /// as that device, and its offset goes with it.
    fn identified(&mut self, from: SocketAddr, id: DeviceId) {
        let Some(manual) = self
            .manual
            .iter()
            .find(|manual| manual.resolved == Some(from))
            .map(|manual| manual.id)
        else {
            return;
        };
        // 0 ms counts as none: the sender can't forget an offset, so Undo leaves 0.
        let memory = self.memory();
        let stored = |device| memory.offset(device).filter(|offset| !offset.is_zero());
        if let (Some(offset), None, Some(link)) = (stored(&manual), stored(&id), self.link.as_mut())
        {
            link.set_offset(id, offset);
            self.pending.push(BackendEvent::CastMemoryChanged);
        }
        if self.enabled == Some(manual) {
            if self.attempt.is_some() {
                self.stop_session();
                self.list.set_in_session(DeviceKey::Id(id), false);
            }
            self.enabled = Some(id);
            self.log
                .log(|| format!("Cast: {from} stays on as {}", self.name(id)));
        }
    }

    /// How the list keys device `id`: by its address while it is a speaker
    /// added by address that no browse has found.
    fn key_of(&self, id: DeviceId) -> DeviceKey {
        self.manual
            .iter()
            .find(|manual| manual.id == id)
            .and_then(|manual| manual.resolved)
            .map(DeviceKey::Manual)
            .filter(|&key| self.list.get(key).is_some())
            .unwrap_or(DeviceKey::Id(id))
    }

    /// What the user typed for the speaker added by address that `entry`
    /// is, if it is one.
    fn added_as(&self, entry: &Entry) -> Option<String> {
        self.manual
            .iter()
            .find(|manual| {
                manual
                    .resolved
                    .is_some_and(|socket| entry.addresses.contains(&socket))
            })
            .map(|manual| manual.address.text())
    }

    pub fn set_debug_log(&mut self, log: DebugLog) {
        self.log = log;
    }

    /// The tap for a capture that starts; `None` while one holds it.
    pub fn lease(&self) -> Option<CastLease> {
        self.feed.slot().lease()
    }

    /// Whether the tap is free for a capture that starts.
    pub fn is_free(&self) -> bool {
        self.feed.slot().is_free()
    }

    pub fn discovery(&self) -> CastDiscovery {
        self.link
            .as_ref()
            .map_or(CastDiscovery::Off, |link| link.discovery())
    }

    fn memory(&self) -> DeviceMemory {
        self.link
            .as_ref()
            .map(|link| link.memory())
            .unwrap_or_default()
    }

    pub fn memory_json(&self) -> Option<String> {
        self.link.as_ref().map(|link| link.memory().to_json())
    }

    /// The devices listed, by name, with their settings.
    pub fn speakers(&self) -> Vec<CastInfo> {
        let memory = self.memory();
        let mut speakers: Vec<CastInfo> = self
            .list
            .entries()
            .iter()
            .filter_map(|entry| self.info(entry, &memory))
            .collect();
        speakers.extend(
            self.manual
                .iter()
                .filter(|manual| manual.resolved.is_none())
                .map(|manual| {
                    let text = manual.address.text();
                    self.row(manual.id, text.clone(), None, Some(text), &memory)
                }),
        );
        speakers.sort_by_key(|speaker| speaker.name.to_lowercase());
        speakers
    }

    fn info(&self, entry: &Entry, memory: &DeviceMemory) -> Option<CastInfo> {
        let (id, name, group) = match entry.key {
            DeviceKey::Id(id) => {
                let device = entry.device.as_ref()?;
                let name = device
                    .name
                    .clone()
                    .unwrap_or_else(|| device.instance.clone());
                let group = entry.is_group().then(|| CastGroup {
                    members: self.group_members(id),
                });
                (id, name, group)
            }
            DeviceKey::Manual(socket) => {
                let manual = self
                    .manual
                    .iter()
                    .find(|manual| manual.resolved == Some(socket))?;
                (manual.id, manual.address.text(), None)
            }
        };
        Some(self.row(id, name, group, self.added_as(entry), memory))
    }

    fn row(
        &self,
        id: DeviceId,
        name: String,
        group: Option<CastGroup>,
        address: Option<String>,
        memory: &DeviceMemory,
    ) -> CastInfo {
        let reported = self.volumes.get(&id);
        let level = self
            .own_of(id)
            .or_else(|| reported.map(|volume| volume.level));
        let session = self.attempt.filter(|attempt| attempt.device == id);
        let status = session.map_or(CastStatus::Idle, |attempt| attempt.status);
        CastInfo {
            id: id.to_string(),
            name,
            group,
            address,
            offline: !self.entry(id).is_some_and(|entry| entry.online),
            enabled: self.enabled == Some(id),
            volume: (level.unwrap_or(1.0) * 100.0).clamp(0.0, 100.0),
            volume_known: level.is_some(),
            muted: reported.is_some_and(|volume| volume.muted),
            volume_fixed: reported.is_some_and(|volume| volume.control_type == ControlType::Fixed),
            volume_step: volume_step(reported),
            offset_ms: offset_ms(memory, &id),
            status,
            format: session.and_then(|attempt| attempt.codec),
            hold_back_ms: if session.is_some() {
                self.hold_back_ms
            } else {
                0.0
            },
        }
    }

    /// Whether a device is on, listed or not: one that left the network
    /// still refuses another (spec C2).
    pub fn is_on(&self) -> bool {
        self.enabled.is_some()
    }

    /// The ID of the device that is on, listed or not.
    pub fn enabled_id(&self) -> Option<String> {
        self.enabled.map(|id| id.to_string())
    }

    /// Whether a device is on and either listed or in a session: then local
    /// outputs wait for it by [`CastSpeakers::anchor`].
    pub fn chosen(&self) -> bool {
        self.enabled.is_some_and(|id| {
            self.attempt.is_some_and(|attempt| attempt.device == id)
                || self.entry(id).is_some_and(|entry| entry.online)
        })
    }

    /// How long local outputs wait for the device that is on: its measured
    /// offset (C77); `None` while none is chosen.
    pub fn anchor(&self) -> Option<Duration> {
        if !self.chosen() {
            return None;
        }
        let id = self.enabled?;
        Some(self.memory().offset(&id).unwrap_or_default())
    }

    /// Holds the feed back by `hold_back_ms`, live, so a device whose offset
    /// is shorter than L plays at L (C77). A change fades over 20 ms.
    pub fn set_hold_back(&mut self, hold_back_ms: f64) {
        let ms = if hold_back_ms.is_finite() {
            hold_back_ms.clamp(0.0, MAX_HOLD_BACK_MS)
        } else {
            0.0
        };
        self.hold_back_ms = ms;
        let frames = (ms * f64::from(SESSION_RATE) / 1000.0).round() as usize;
        if self.feed.set_hold_back(frames) {
            self.log
                .log(|| format!("Cast: the feed is held back {ms:.0} ms to meet L"));
        }
    }

    /// What the row of the group `id` says about its members. A list
    /// arrives before the stream plays (C94), so one still missing once it
    /// plays is unknown; members without a name are left out.
    fn group_members(&self, id: DeviceId) -> CastMembers {
        let Some(attempt) = self.attempt.filter(|attempt| attempt.device == id) else {
            return CastMembers::NotReported;
        };
        let names: Vec<String> = self
            .members
            .get(&id)
            .map(|members| self.member_names(members).into_iter().flatten().collect())
            .unwrap_or_default();
        if !names.is_empty() {
            CastMembers::Known(names)
        } else if attempt.status == CastStatus::Playing {
            CastMembers::Unknown
        } else {
            CastMembers::NotReported
        }
    }

    /// A group's members by name: the name of the device a member is, when
    /// one is listed, else the name the group gives it (C95), else `None`.
    fn member_names(&self, members: &[GroupMember]) -> Vec<Option<String>> {
        members
            .iter()
            .map(|member| {
                self.list
                    .member(member)
                    .and_then(|entry| entry.device.as_ref())
                    .and_then(|device| device.name.clone())
                    .filter(|name| !name.is_empty())
                    .or_else(|| member.name.clone())
                    .filter(|name| !name.is_empty())
            })
            .collect()
    }

    /// Hands the device or group in a session its latest records, and a
    /// group its members' addresses (C90, C103, C111).
    fn follow_group(&self) {
        let (Some(records), Some(attempt)) = (&self.records, self.attempt) else {
            return;
        };
        if let Some(entry) = self.entry(attempt.device) {
            records.set_group(entry.addresses.clone());
        }
        let members = self
            .members
            .get(&attempt.device)
            .map_or(Vec::new(), |members| {
                members
                    .iter()
                    .filter_map(|member| self.list.member(member))
                    .flat_map(|entry| entry.addresses.iter().map(|address| address.ip()))
                    .collect()
            });
        records.set_members(members);
    }

    fn entry(&self, id: DeviceId) -> Option<&Entry> {
        self.list.get(self.key_of(id))
    }

    fn name(&self, id: DeviceId) -> String {
        if let Some(manual) = self.manual.iter().find(|manual| manual.id == id) {
            return manual.address.text();
        }
        self.entry(id)
            .and_then(|entry| entry.device.as_ref())
            .and_then(|device| device.name.clone())
            .unwrap_or_else(|| id.to_string())
    }

    /// Turns a device or group on or off. A second one is refused while one
    /// is on (C2), a member of the group that plays included (C99).
    pub fn set_enabled(&mut self, id: &str, on: bool) -> Result<(), CastProblem> {
        let Some(device) = DeviceId::parse(id) else {
            return Ok(());
        };
        if !on {
            if self.enabled == Some(device) {
                self.log
                    .log(|| format!("Cast: {} switched off", self.name(device)));
                self.enabled = None;
                self.sync();
            }
            return Ok(());
        }
        if let Some(other) = self.enabled.filter(|&other| other != device) {
            self.log.log(|| {
                format!(
                    "Cast: {} refused, {} is on (one Cast speaker at a time)",
                    self.name(device),
                    self.name(other)
                )
            });
            return Err(CastProblem::Busy);
        }
        self.enabled = Some(device);
        self.log
            .log(|| format!("Cast: {} switched on", self.name(device)));
        self.sync();
        Ok(())
    }

    /// Streaming started or stopped: the device that is on joins or leaves.
    pub fn set_streaming(&mut self, streaming: bool) {
        self.streaming = streaming;
        self.sync();
    }

    /// A session runs while its device is on and streaming is on, whatever
    /// discovery says meanwhile (C17); a new one starts once the device that
    /// is on is listed.
    fn sync(&mut self) {
        if let Some(attempt) = self.attempt
            && (self.enabled != Some(attempt.device) || !self.streaming)
        {
            self.stop_session();
        }
        if self.attempt.is_some() || !self.streaming || self.track.is_none() {
            return;
        }
        let listed = self.enabled.filter(|&id| {
            self.entry(id)
                .is_some_and(|entry| entry.online && !entry.addresses.is_empty())
        });
        if let Some(id) = listed {
            self.start_session(id);
        }
    }

    fn start_session(&mut self, id: DeviceId) {
        let Some(entry) = self.entry(id) else {
            return;
        };
        let records = Some(GroupRecords::new(entry.addresses.clone()));
        let group = entry.is_group();
        let target = CastTarget {
            id,
            addresses: entry.addresses.clone(),
            group: records.clone().filter(|_| group),
            records: records.clone().filter(|_| !group),
        };
        let Some(link) = self.link.as_mut() else {
            return;
        };
        let (writer, reader) = audio_ring(RING_FRAMES);
        let track = self.track.clone().unwrap_or_default();
        match link.start(target, SESSION_RATE, track.clone(), reader) {
            Ok(attempt) => {
                self.sent = Some(track);
                if !self.feed.hand(Some(writer)) {
                    self.log
                        .log(|| "Cast: the capture's handoff is full; the new session's feed waits for the next poll".to_owned());
                }
                self.attempt = Some(Attempt {
                    id: attempt,
                    device: id,
                    status: CastStatus::Connecting,
                    codec: None,
                    heard_volume: false,
                });
                if group {
                    self.members.remove(&id);
                }
                self.records = records;
                let key = self.key_of(id);
                self.list.set_in_session(key, true);
                self.log
                    .log(|| format!("Cast: {} connects (attempt {})", self.name(id), attempt.0));
                self.pending.push(BackendEvent::CastChanged);
            }
            Err(refused) => {
                self.log
                    .log(|| format!("Cast: {} couldn't start: {refused}", self.name(id)));
                let problem = match refused {
                    StartRefused::Busy { .. } => CastProblem::Busy,
                    StartRefused::Rate(_) | StartRefused::Closed => CastProblem::NotResponding,
                };
                self.enabled = None;
                self.fail(id, problem);
            }
        }
    }

    fn stop_session(&mut self) {
        let Some(attempt) = self.attempt.take() else {
            return;
        };
        self.records = None;
        self.requested = None;
        self.unanswered.clear();
        if let Some(link) = self.link.as_mut() {
            link.stop();
        }
        self.feed.hand(None);
        let key = self.key_of(attempt.device);
        self.list.set_in_session(key, false);
        self.log.log(|| {
            format!(
                "Cast: {} stops (attempt {})",
                self.name(attempt.device),
                attempt.id.0
            )
        });
        self.pending.push(BackendEvent::CastChanged);
    }

    fn fail(&mut self, id: DeviceId, problem: CastProblem) {
        self.pending.push(BackendEvent::CastFailed {
            id: id.to_string(),
            problem,
        });
        self.pending.push(BackendEvent::CastChanged);
    }

    /// The device's own level, 0 to 100, sent times the master (C69,
    /// C104); shown until the device's next status answers it. Without a
    /// session it is kept, and the next session's first status sets it (C105).
    pub fn set_volume(&mut self, id: &str, position: f64) {
        let Some(device) = DeviceId::parse(id) else {
            return;
        };
        if !position.is_finite() {
            return;
        }
        let own = (position / 100.0).clamp(0.0, 1.0);
        if self.playing_device(id).is_none() {
            self.levels.insert(device, own);
            return;
        }
        self.requested = Some((device, own));
        self.send_level(device, own);
    }

    /// The master position, 0 to 100 (C104): a change reaches the device in
    /// a session at once.
    pub fn set_master(&mut self, position: f64) {
        if !position.is_finite() {
            return;
        }
        let master = (position / 100.0).clamp(0.0, 1.0);
        if master == self.master {
            return;
        }
        self.master = master;
        if let Some(attempt) = self.attempt
            && let Some(own) = self.own_of(attempt.device)
        {
            self.send_level(attempt.device, own);
        }
    }

    /// The zone volume, 0 to 100, of every Cast device ID in a zone (S249),
    /// the rest being in no zone: a change reaches the device in a session
    /// at once, as a master change does (C104).
    pub fn set_zones(&mut self, zones: BTreeMap<String, f64>) {
        let zones: BTreeMap<DeviceId, f64> = zones
            .into_iter()
            .filter(|(_, zone)| zone.is_finite())
            .filter_map(|(id, zone)| Some((DeviceId::parse(&id)?, (zone / 100.0).clamp(0.0, 1.0))))
            .filter(|(_, zone)| *zone != NO_ZONE_LEVEL)
            .collect();
        let live = self.attempt.map(|attempt| attempt.device);
        let before = live.map(|device| self.zone_of(device));
        self.zones = zones;
        if let Some(device) = live
            && before != Some(self.zone_of(device))
            && let Some(own) = self.own_of(device)
        {
            self.send_level(device, own);
        }
    }

    /// The zone level of `device`: [`NO_ZONE_LEVEL`] when it is in no zone.
    fn zone_of(&self, device: DeviceId) -> f64 {
        self.zones.get(&device).copied().unwrap_or(NO_ZONE_LEVEL)
    }

    /// The own level the slider shows: one the user set that no status
    /// answered yet, else the one kept.
    fn own_of(&self, device: DeviceId) -> Option<f64> {
        match self.requested {
            Some((requested, own)) if requested == device => Some(own),
            _ => self.levels.get(&device).copied(),
        }
    }

    /// Sends master × zone × `own`, unless the device's volume is fixed
    /// (C68).
    fn send_level(&mut self, device: DeviceId, own: f64) {
        let level = device_level(self.master, self.zone_of(device), own);
        let reported = self.volumes.get(&device);
        if reported.is_some_and(|volume| volume.control_type == ControlType::Fixed) {
            return;
        }
        if reported.is_some_and(|volume| answers_volume(level, volume)) {
            // The device reports only changes, so this level gets no answer,
            // and the latest level sent replaces whatever was still in flight.
            self.unanswered.clear();
            self.levels.insert(device, own);
            if self
                .requested
                .is_some_and(|(requested, _)| requested == device)
            {
                self.requested = None;
            }
        } else {
            self.unanswered.push(level);
        }
        if let Some(link) = self.link.as_mut() {
            link.set_volume(level);
        }
    }

    /// A status's volume (C70, C104, C105): the answer to a level sent keeps
    /// the own level and any other level moves it by master × zone, except
    /// the session's first, which is set to master × zone × own level
    /// instead. A status without a level is not the first.
    fn volume_reported(&mut self, device: DeviceId, volume: DeviceVolume) {
        if !volume.has_level {
            return;
        }
        let shown = self.own_of(device);
        let zone = self.zone_of(device);
        let level = volume.level;
        let first = self
            .attempt
            .as_mut()
            .is_some_and(|attempt| !std::mem::replace(&mut attempt.heard_volume, true));
        if first {
            // The sender sends no level before the device's first status.
            self.unanswered.clear();
        }
        // Answers come back in send order, so the oldest copy of a level a
        // drag passed twice is the one this answers.
        let answered = self
            .unanswered
            .iter()
            .position(|&sent| answers_volume(sent, &volume));
        if let Some(index) = answered {
            self.unanswered.drain(..=index);
        }
        let own = match (first, answered) {
            (true, _) => start_own_level(self.master, zone, shown, level),
            (false, Some(_)) => shown.unwrap_or_else(|| own_level(level, self.master, zone, level)),
            (false, None) => own_level(level, self.master, zone, shown.unwrap_or(level)),
        };
        if self
            .requested
            .is_some_and(|(requested, _)| requested == device)
            && (answered.is_none() || self.unanswered.is_empty())
        {
            self.requested = None;
        }
        self.levels.insert(device, own);
        if self.volumes.get(&device) != Some(&volume) {
            self.volumes.insert(device, volume);
            self.pending.push(BackendEvent::CastChanged);
        } else if self.own_of(device) != shown {
            self.pending.push(BackendEvent::CastChanged);
        }
        let allowed = device_level(self.master, zone, own);
        let at_level = self
            .volumes
            .get(&device)
            .is_some_and(|volume| answers_volume(allowed, volume));
        if first && !at_level {
            self.log.log(|| {
                format!(
                    "Cast: {} reports level {level:.2} at the start, set to {allowed:.2}",
                    self.name(device)
                )
            });
            self.send_level(device, own);
        }
    }

    pub fn set_muted(&mut self, id: &str, muted: bool) {
        if self.playing_device(id).is_some()
            && let Some(link) = self.link.as_mut()
        {
            link.set_muted(muted);
        }
    }

    /// The playing track, or `None` while it is being read. A session in
    /// progress keeps a new one for its next `LOAD` (C54); a start waits
    /// for it.
    pub fn set_track(&mut self, track: Option<CastTrack>) {
        if self.track == track {
            return;
        }
        let known = self.track.is_none() && track.is_some();
        self.track = track;
        if let Some(track) = &self.track
            && self.attempt.is_some()
            && self.sent.as_ref() != Some(track)
        {
            self.log.log(|| {
                format!(
                    "Cast: track kept for the next load: \"{}\" by \"{}\", {}",
                    track.metadata.title.as_deref().unwrap_or(""),
                    track.metadata.artist.as_deref().unwrap_or(""),
                    if track.artwork.is_some() {
                        "with artwork"
                    } else {
                        "no artwork"
                    }
                )
            });
            if let Some(link) = self.link.as_mut() {
                link.set_track(track.clone());
            }
            self.sent = Some(track.clone());
        }
        if known {
            self.sync();
        }
    }

    /// The user's Resync (C79), for the device in a session.
    pub fn resync(&mut self, id: &str) {
        let Some(device) = self.playing_device(id) else {
            return;
        };
        self.log
            .log(|| format!("Cast: the user resyncs {}", self.name(device)));
        if let Some(link) = self.link.as_mut() {
            link.resync();
        }
    }

    fn playing_device(&self, id: &str) -> Option<DeviceId> {
        let device = DeviceId::parse(id)?;
        self.attempt
            .is_some_and(|attempt| attempt.device == device)
            .then_some(device)
    }

    /// The user's measured offset for the device, clamped to 0 to 20 000 ms
    /// and kept per device (C77). Returns whether it changed.
    pub fn set_offset(&mut self, id: &str, offset_ms: f64) -> bool {
        let Some(device) = DeviceId::parse(id) else {
            return false;
        };
        if !offset_ms.is_finite() {
            return false;
        }
        let ms = offset_ms.clamp(0.0, MAX_HOLD_BACK_MS).round();
        if self.memory().offset(&device) == Some(Duration::from_millis(ms as u64)) {
            return false;
        }
        if let Some(link) = self.link.as_mut() {
            link.set_offset(device, Duration::from_millis(ms as u64));
        }
        self.log
            .log(|| format!("Cast: offset of {} set to {ms} ms", self.name(device)));
        self.pending.push(BackendEvent::CastMemoryChanged);
        self.pending.push(BackendEvent::CastChanged);
        true
    }

    /// Handles what discovery and the sender reported since the last poll.
    pub fn poll(&mut self) -> Vec<BackendEvent> {
        let selected = self.enabled.map(|id| self.key_of(id));
        let members = self
            .attempt
            .as_ref()
            .and_then(|attempt| self.members.get(&attempt.device))
            .into_iter()
            .flatten()
            .filter_map(|member| member.id.map(DeviceKey::Id));
        let kept: Vec<DeviceKey> = selected.into_iter().chain(members).collect();
        self.list.set_kept(kept);
        for _ in 0..MAX_BROWSE_PER_POLL {
            let Some(event) = self.link.as_mut().and_then(|link| link.poll_browse()) else {
                break;
            };
            match self.list.apply(event) {
                ListEvent::Skipped(_) => {}
                change => {
                    self.log.log(|| format!("Cast discovery: {change:?}"));
                    if let Some(id) = self.announcements.note(&self.list, &change)
                        && let Some(link) = self.link.as_mut()
                    {
                        link.announced(id);
                    }
                    if let ListEvent::Added(DeviceKey::Id(id))
                    | ListEvent::Displaced {
                        added: DeviceKey::Id(id),
                        ..
                    }
                    | ListEvent::Identified { id, .. } = change
                    {
                        self.log_group(id);
                    }
                    if let ListEvent::Identified { from, id } = change {
                        self.identified(from, id);
                    }
                    self.pending.push(BackendEvent::CastChanged);
                }
            }
        }
        self.poll_lookups();
        self.keep_manual_listed();
        self.check_manual();
        self.follow_group();
        while let Some(event) = self.link.as_mut().and_then(|link| link.poll_event()) {
            self.handle(event);
        }
        self.off_when_discovery_stopped();
        self.feed.flush();
        self.sync();
        dedup_changed(std::mem::take(&mut self.pending))
    }

    /// A browsed device that is on with no session and left the list can't
    /// come back once discovery stopped, so it goes off and no longer
    /// refuses another (S339).
    fn off_when_discovery_stopped(&mut self) {
        let Some(id) = self.enabled else { return };
        if self.discovery() != CastDiscovery::Failed
            || self.entry(id).is_some()
            || self.attempt.is_some_and(|attempt| attempt.device == id)
            || self.manual.iter().any(|manual| manual.id == id)
        {
            return;
        }
        self.log
            .log(|| format!("Cast: {id} switched off, discovery stopped"));
        self.enabled = None;
        self.fail(id, CastProblem::SearchStopped);
    }

    /// What a group announces, when it is first listed (C89).
    fn log_group(&self, id: DeviceId) {
        if let Some(line) = self
            .entry(id)
            .and_then(|entry| entry.device.as_ref())
            .and_then(|device| device.group_log_line())
        {
            self.log.log(|| line);
        }
    }

    fn is_current(&self, attempt: AttemptId) -> bool {
        self.attempt.is_some_and(|current| current.id == attempt)
    }

    fn set_status(&mut self, status: CastStatus) {
        if let Some(attempt) = self.attempt.as_mut()
            && attempt.status != status
        {
            attempt.status = status;
            self.pending.push(BackendEvent::CastChanged);
            if status == CastStatus::Playing {
                self.pending.push(BackendEvent::CastPlaying {
                    id: attempt.device.to_string(),
                });
            }
        }
    }

    fn set_codec(&mut self, codec: Codec) {
        if let Some(attempt) = self.attempt.as_mut() {
            attempt.codec = Some(codec);
        }
    }

    fn handle(&mut self, event: HostEvent) {
        match event {
            HostEvent::Session {
                attempt,
                device,
                event,
            } => {
                if !self.is_current(attempt) {
                    self.log.log(|| {
                        format!(
                            "Cast: an event of attempt {} ignored, no longer current",
                            attempt.0
                        )
                    });
                    return;
                }
                self.session_event(device, event);
            }
            HostEvent::Started {
                attempt, format, ..
            } => {
                if self.is_current(attempt) {
                    self.log.log(|| format!("Cast: plays {format:?}"));
                    self.set_codec(format.codec());
                    self.set_status(CastStatus::Playing);
                }
            }
            HostEvent::Ended {
                attempt,
                device,
                end,
            } => {
                if !self.is_current(attempt) {
                    self.log.log(|| {
                        format!(
                            "Cast: the end of attempt {} ignored, no longer current",
                            attempt.0
                        )
                    });
                    return;
                }
                self.attempt = None;
                self.records = None;
                self.sent = None;
                self.requested = None;
                self.unanswered.clear();
                self.feed.hand(None);
                let key = self.key_of(device);
                self.list.set_in_session(key, false);
                self.log
                    .log(|| format!("Cast: {} ended: {}", self.name(device), describe_end(&end)));
                self.enabled = None;
                match problem_of(&end) {
                    Some(problem) => self.fail(device, problem),
                    None => self.pending.push(BackendEvent::CastChanged),
                }
            }
            HostEvent::CommandFailed { error, .. } => {
                self.log.log(|| format!("Cast: a request failed: {error}"));
            }
            HostEvent::MemoryChanged => self.pending.push(BackendEvent::CastMemoryChanged),
        }
    }

    fn session_event(&mut self, device: DeviceId, event: SessionEvent) {
        match event {
            SessionEvent::Playing | SessionEvent::Recovered { .. } | SessionEvent::Reloaded => {
                self.set_status(CastStatus::Playing);
            }
            SessionEvent::Recovering(reason) => {
                self.log.log(|| format!("Cast: reconnecting ({reason:?})"));
                self.set_status(CastStatus::Reconnecting);
            }
            SessionEvent::ConnectFailed { error, retry_in } => {
                self.log.log(|| {
                    format!(
                        "Cast: connecting failed ({error}), again in {} ms",
                        retry_in.as_millis()
                    )
                });
                let playing = self
                    .attempt
                    .is_some_and(|attempt| attempt.status != CastStatus::Connecting);
                if playing {
                    self.set_status(CastStatus::Reconnecting);
                }
            }
            SessionEvent::Volume(volume) => self.volume_reported(device, volume),
            SessionEvent::Diagnostics(diagnostics) => {
                self.log
                    .log(|| format!("Cast diagnostics: {diagnostics:?}"));
            }
            SessionEvent::FormatFallback { from, to, reason } => {
                self.log
                    .log(|| format!("Cast: {from:?} refused ({reason}), {to:?} instead"));
                self.set_codec(to);
            }
            SessionEvent::Rebuffering => self.log.log(|| "Cast: the device ran dry".to_owned()),
            SessionEvent::Stream(event) => self.log.log(|| format!("Cast stream: {event:?}")),
            SessionEvent::GroupMessage(kind) => self.log.log(|| format!("Cast group: {kind}")),
            SessionEvent::CertificateChanged { before, after } => self.log.log(|| {
                format!(
                    "Cast: {}'s certificate changed from {before} to {after}",
                    self.name(device)
                )
            }),
            SessionEvent::Members(members) => {
                self.log.log(|| {
                    let names: Vec<String> = self
                        .member_names(&members)
                        .into_iter()
                        .zip(&members)
                        .map(|(name, member)| {
                            name.unwrap_or_else(|| {
                                format!("unnamed, deviceId {}", member.device_id)
                            })
                        })
                        .collect();
                    format!(
                        "Cast group {} members: {} ({})",
                        self.name(device),
                        members.len(),
                        names.join(", ")
                    )
                });
                self.members.insert(device, members);
                self.follow_group();
                self.pending.push(BackendEvent::CastChanged);
            }
            SessionEvent::Ended(_) => {}
        }
    }

    /// Starts the sender's teardown on a thread of its own, so the app's
    /// other teardowns run meanwhile (C88); join it before the process
    /// ends.
    pub fn begin_shutdown(&mut self) -> Option<JoinHandle<bool>> {
        let mut link = self.link.take()?;
        self.attempt = None;
        self.records = None;
        self.feed.hand(None);
        thread::Builder::new()
            .name("cast-shutdown".to_owned())
            .spawn(move || link.shutdown())
            .ok()
    }
}

fn offset_ms(memory: &DeviceMemory, id: &DeviceId) -> f64 {
    memory
        .offset(id)
        .map_or(0.0, |offset| offset.as_millis() as f64)
}

/// One `CastChanged` is enough per poll.
fn dedup_changed(events: Vec<BackendEvent>) -> Vec<BackendEvent> {
    let mut seen_changed = false;
    events
        .into_iter()
        .filter(|event| {
            if *event != BackendEvent::CastChanged {
                return true;
            }
            !std::mem::replace(&mut seen_changed, true)
        })
        .collect()
}

/// What the user is told about a session that ended on its own; `None` for
/// a stop the user or AudioSync asked for.
fn problem_of(end: &SessionEnd) -> Option<CastProblem> {
    match end {
        SessionEnd::Stopped | SessionEnd::Deselected => None,
        SessionEnd::TakenOver => Some(CastProblem::TakenOver),
        SessionEnd::Failed(SessionError::Connect(_)) => Some(CastProblem::NotResponding),
        SessionEnd::Failed(SessionError::Media(MediaError::NoFetch)) => Some(CastProblem::Blocked),
        SessionEnd::Failed(SessionError::Launch(_) | SessionError::Media(_)) => {
            Some(CastProblem::Refused)
        }
        SessionEnd::Failed(SessionError::Stream(_) | SessionError::Internal) => {
            Some(CastProblem::ConnectionLost)
        }
    }
}

fn describe_end(end: &SessionEnd) -> String {
    match end {
        SessionEnd::Stopped => "stopped".to_owned(),
        SessionEnd::TakenOver => "taken over by another sender".to_owned(),
        SessionEnd::Deselected => "deselected".to_owned(),
        SessionEnd::Failed(error) => format!("failed: {error}"),
    }
}

#[cfg(test)]
pub mod tests {
    use std::collections::VecDeque;
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};

    use audiosync_cast::client::Fingerprint;
    use audiosync_cast::discovery::{DEVICE_PORT, MAX_DEVICES, ServiceRecord};
    use audiosync_cast::error::ConnectError;

    use super::*;
    use crate::debug_log::{FILE_NAME, TEST_FLUSH, assert_in_order, read_messages, test_dir};

    pub const KITCHEN: &str = "0123456789abcdef0123456789abcdef";
    pub const LIVING: &str = "fedcba9876543210fedcba9876543210";
    pub const ATTIC: &str = "00112233445566778899aabbccddeeff";

    /// What the fake sender was asked, and what it reports next.
    #[derive(Default)]
    pub struct FakeState {
        pub started: Vec<DeviceId>,
        pub stops: usize,
        pub volumes: Vec<f64>,
        pub mutes: Vec<bool>,
        /// The track each start was given.
        pub start_tracks: Vec<CastTrack>,
        /// Tracks given to a session in progress.
        pub tracks: Vec<CastTrack>,
        pub resyncs: usize,
        pub shutdowns: usize,
        pub announced: Vec<DeviceId>,
        pub memory: DeviceMemory,
        pub refuse: Option<StartRefused>,
        pub readers: Vec<AudioReader>,
        /// The group records each start was handed.
        pub groups: Vec<Option<GroupRecords>>,
        /// The single-device records each start was handed.
        pub records: Vec<Option<GroupRecords>>,
        /// The addresses each start was handed.
        pub targets: Vec<Vec<SocketAddr>>,
        pub events: VecDeque<HostEvent>,
        pub browse: VecDeque<BrowseEvent>,
        /// The mDNS browser ended: discovery reads as failed.
        pub discovery_stopped: bool,
        next: u64,
    }

    /// A sender that answers at once; the test drives what it reports.
    #[derive(Clone, Default)]
    pub struct FakeCast(pub Arc<Mutex<FakeState>>);

    impl FakeCast {
        pub fn state(&self) -> std::sync::MutexGuard<'_, FakeState> {
            self.0.lock().unwrap_or_else(PoisonError::into_inner)
        }

        pub fn link(&self) -> Box<dyn CastLink> {
            Box::new(self.clone())
        }

        /// A device announces itself; a port other than 8009 is a group.
        pub fn announce(&self, id: &str, name: &str, port: u16) {
            self.announce_at(id, name, port, Ipv4Addr::new(192, 0, 2, 10));
        }

        pub fn announce_at(&self, id: &str, name: &str, port: u16, address: Ipv4Addr) {
            self.state()
                .browse
                .push_back(BrowseEvent::Resolved(ServiceRecord {
                    fullname: format!("{name}-{id}._googlecast._tcp.local."),
                    port,
                    txt: vec![
                        format!("id={id}").into_bytes(),
                        format!("fn={name}").into_bytes(),
                    ],
                    addresses: vec![IpAddr::V4(address)],
                }));
        }

        /// The goodbye of a speaker announced with [`FakeCast::announce`].
        pub fn withdraw(&self, id: &str, name: &str) {
            self.state().browse.push_back(BrowseEvent::Removed {
                fullname: format!("{name}-{id}._googlecast._tcp.local."),
            });
        }

        pub fn report(&self, event: HostEvent) {
            self.state().events.push_back(event);
        }
    }

    impl CastLink for FakeCast {
        fn start(
            &mut self,
            target: CastTarget,
            _rate: u32,
            track: CastTrack,
            reader: AudioReader,
        ) -> Result<AttemptId, StartRefused> {
            let mut state = self.state();
            if let Some(refused) = state.refuse.take() {
                return Err(refused);
            }
            state.start_tracks.push(track);
            state.started.push(target.id);
            state.targets.push(target.addresses);
            state.groups.push(target.group);
            state.records.push(target.records);
            state.readers.push(reader);
            state.next += 1;
            Ok(AttemptId(state.next))
        }

        fn stop(&mut self) {
            self.state().stops += 1;
        }

        fn set_volume(&mut self, level: f64) {
            self.state().volumes.push(level);
        }

        fn set_muted(&mut self, muted: bool) {
            self.state().mutes.push(muted);
        }

        fn set_track(&mut self, track: CastTrack) {
            self.state().tracks.push(track);
        }

        fn resync(&mut self) {
            self.state().resyncs += 1;
        }

        fn set_offset(&mut self, device: DeviceId, offset: Duration) {
            self.state().memory.set_offset(device, offset);
        }

        fn memory(&self) -> DeviceMemory {
            self.state().memory.clone()
        }

        fn poll_event(&mut self) -> Option<HostEvent> {
            self.state().events.pop_front()
        }

        fn poll_browse(&mut self) -> Option<BrowseEvent> {
            self.state().browse.pop_front()
        }

        fn announced(&mut self, device: DeviceId) {
            self.state().announced.push(device);
        }

        fn discovery(&self) -> CastDiscovery {
            if self.state().discovery_stopped {
                CastDiscovery::Failed
            } else {
                CastDiscovery::Browsing
            }
        }

        fn shutdown(&mut self) -> bool {
            self.state().shutdowns += 1;
            true
        }
    }

    pub fn id(text: &str) -> DeviceId {
        DeviceId::parse(text).expect("a test ID")
    }

    /// Kitchen and Living Room as single devices, Attic as a group, all
    /// listed.
    fn listed() -> (FakeCast, CastSpeakers) {
        let fake = FakeCast::default();
        fake.announce(KITCHEN, "Kitchen", DEVICE_PORT);
        fake.announce(LIVING, "Living Room", DEVICE_PORT);
        fake.announce(ATTIC, "Attic", 32_123);
        let mut cast = CastSpeakers::new(fake.link(), DebugLog::default());
        cast.poll();
        (fake, cast)
    }

    fn playing(device: &str) -> HostEvent {
        HostEvent::Session {
            attempt: AttemptId(1),
            device: id(device),
            event: SessionEvent::Playing,
        }
    }

    fn status(cast: &CastSpeakers, id: &str) -> CastStatus {
        cast.speakers()
            .into_iter()
            .find(|speaker| speaker.id == id)
            .expect("listed")
            .status
    }

    // covers: C106
    #[test]
    fn a_discovery_burst_beyond_the_cap_waits_for_the_next_tick() {
        let fake = FakeCast::default();
        let extra = 10;
        for n in 0..MAX_BROWSE_PER_POLL + extra {
            let id = format!("{n:032x}");
            fake.announce(&id, &format!("Speaker {n}"), DEVICE_PORT);
        }
        let mut cast = CastSpeakers::new(fake.link(), DebugLog::default());
        cast.poll();
        assert_eq!(fake.state().browse.len(), extra);
        cast.poll();
        assert!(fake.state().browse.is_empty());
    }

    #[test]
    fn the_selected_speaker_stays_listed_through_a_flood_of_devices() {
        let fake = FakeCast::default();
        let selected = format!("{:032x}", 1);
        fake.announce(&selected, "Kitchen", DEVICE_PORT);
        let mut cast = CastSpeakers::new(fake.link(), DebugLog::default());
        cast.poll();
        cast.set_enabled(&selected, true).expect("switched on");
        for n in 2..=MAX_DEVICES + 10 {
            fake.announce(&format!("{n:032x}"), &format!("Speaker {n}"), DEVICE_PORT);
        }
        cast.poll();
        let speakers = cast.speakers();
        assert_eq!(speakers.len(), MAX_DEVICES);
        assert!(speakers.iter().any(|speaker| speaker.id == selected));
    }

    #[test]
    fn a_stream_the_speaker_couldnt_fetch_reads_as_blocked() {
        let blocked = SessionEnd::Failed(SessionError::Media(MediaError::NoFetch));
        assert_eq!(problem_of(&blocked), Some(CastProblem::Blocked));
        let refused = SessionEnd::Failed(SessionError::Media(MediaError::LoadCancelled));
        assert_eq!(problem_of(&refused), Some(CastProblem::Refused));
    }

    #[test]
    fn a_fault_inside_the_sender_reads_as_a_lost_connection() {
        let internal = SessionEnd::Failed(SessionError::Internal);
        assert_eq!(problem_of(&internal), Some(CastProblem::ConnectionLost));
    }

    #[test]
    fn devices_are_listed_by_name_and_a_group_says_so() {
        let (_fake, cast) = listed();
        let speakers = cast.speakers();
        let names: Vec<&str> = speakers.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["Attic", "Kitchen", "Living Room"]);
        assert_eq!(
            speakers[0].group,
            Some(CastGroup {
                members: CastMembers::NotReported
            })
        );
        assert_eq!(speakers[1].group, None);
        assert_eq!(speakers[1].id, KITCHEN);
        assert_eq!(cast.discovery(), CastDiscovery::Browsing);
    }

    #[test]
    fn a_session_runs_only_while_on_and_streaming() {
        let (fake, mut cast) = listed();
        assert_eq!(cast.set_enabled(KITCHEN, true), Ok(()));
        assert!(fake.state().started.is_empty());
        cast.set_streaming(true);
        assert_eq!(fake.state().started, [id(KITCHEN)]);
        assert_eq!(status(&cast, KITCHEN), CastStatus::Connecting);

        fake.report(playing(KITCHEN));
        let events = cast.poll();
        assert!(events.contains(&BackendEvent::CastPlaying {
            id: KITCHEN.to_owned()
        }));
        assert_eq!(status(&cast, KITCHEN), CastStatus::Playing);

        cast.set_streaming(false);
        assert_eq!(fake.state().stops, 1);
        assert_eq!(status(&cast, KITCHEN), CastStatus::Idle);
    }

    #[test]
    fn events_of_an_older_attempt_are_ignored() {
        let (fake, mut cast) = listed();
        cast.set_streaming(true);
        cast.set_enabled(KITCHEN, true).unwrap();
        cast.set_enabled(KITCHEN, false).unwrap();
        cast.set_enabled(KITCHEN, true).unwrap();
        assert_eq!(fake.state().started.len(), 2);

        // Attempt 1's late events reach the host after attempt 2 started.
        fake.report(playing(KITCHEN));
        fake.report(HostEvent::Ended {
            attempt: AttemptId(1),
            device: id(KITCHEN),
            end: SessionEnd::TakenOver,
        });
        let events = cast.poll();
        assert!(!events.iter().any(|event| matches!(
            event,
            BackendEvent::CastFailed { .. } | BackendEvent::CastPlaying { .. }
        )));
        let kitchen = &cast.speakers()[1];
        assert!(kitchen.enabled);
        assert_eq!(kitchen.status, CastStatus::Connecting);

        fake.report(HostEvent::Ended {
            attempt: AttemptId(2),
            device: id(KITCHEN),
            end: SessionEnd::TakenOver,
        });
        let events = cast.poll();
        assert!(events.contains(&BackendEvent::CastFailed {
            id: KITCHEN.to_owned(),
            problem: CastProblem::TakenOver,
        }));
        let kitchen = &cast.speakers()[1];
        assert!(!kitchen.enabled);
        assert_eq!(kitchen.status, CastStatus::Idle);
    }

    // covers: C81
    #[test]
    fn only_a_device_coming_back_after_its_records_went_away_is_an_announcement() {
        let (fake, mut cast) = listed();
        cast.set_streaming(true);
        cast.set_enabled(KITCHEN, true).unwrap();
        cast.poll();
        assert_eq!(fake.state().announced.len(), 3, "each one found");
        fake.state().announced.clear();
        fake.announce(KITCHEN, "Kitchen", DEVICE_PORT);
        cast.poll();
        assert!(fake.state().announced.is_empty(), "a re-resolve");
        fake.withdraw(KITCHEN, "Kitchen");
        cast.poll();
        fake.announce(KITCHEN, "Kitchen", DEVICE_PORT);
        cast.poll();
        assert_eq!(fake.state().announced, [id(KITCHEN)]);
    }

    // covers: C81, C90
    #[test]
    fn a_group_whose_leader_handed_over_counts_as_announced() {
        let (fake, mut cast) = listed();
        cast.set_streaming(true);
        cast.set_enabled(ATTIC, true).unwrap();
        cast.poll();
        fake.state().announced.clear();
        fake.announce_at(ATTIC, "Attic", 32_123, Ipv4Addr::new(192, 0, 2, 11));
        cast.poll();
        assert_eq!(fake.state().announced, [id(ATTIC)]);
    }

    #[test]
    fn a_failed_session_turns_its_speaker_off_with_the_reason() {
        let (fake, mut cast) = listed();
        cast.set_streaming(true);
        cast.set_enabled(KITCHEN, true).unwrap();
        fake.report(HostEvent::Ended {
            attempt: AttemptId(1),
            device: id(KITCHEN),
            end: SessionEnd::Failed(SessionError::Connect(ConnectError::NoAddress)),
        });
        let events = cast.poll();
        assert!(events.contains(&BackendEvent::CastFailed {
            id: KITCHEN.to_owned(),
            problem: CastProblem::NotResponding,
        }));
        assert!(!cast.speakers()[1].enabled);
        // Off now, it doesn't start again on its own.
        cast.poll();
        assert_eq!(fake.state().started.len(), 1);
    }

    #[test]
    fn a_second_speaker_is_refused_and_a_group_starts_like_one() {
        let (fake, mut cast) = listed();
        cast.set_enabled(KITCHEN, true).unwrap();
        assert_eq!(cast.set_enabled(LIVING, true), Err(CastProblem::Busy));
        assert_eq!(cast.set_enabled(ATTIC, true), Err(CastProblem::Busy));
        cast.set_enabled(KITCHEN, false).unwrap();
        assert_eq!(cast.set_enabled(ATTIC, true), Ok(()));
        assert!(cast.speakers()[0].enabled);
        cast.set_enabled(ATTIC, false).unwrap();
        assert!(cast.speakers().iter().all(|speaker| !speaker.enabled));

        // The host's own refusal reads the same.
        fake.state().refuse = Some(StartRefused::Busy {
            playing: id(KITCHEN),
        });
        cast.set_streaming(true);
        cast.set_enabled(LIVING, true).unwrap();
        let events = cast.poll();
        assert!(events.contains(&BackendEvent::CastFailed {
            id: LIVING.to_owned(),
            problem: CastProblem::Busy,
        }));
        assert!(!cast.speakers()[2].enabled);
    }

    // covers: S339
    #[test]
    fn a_speaker_that_left_is_turned_off_with_a_notice_when_discovery_stops_and_another_can_go_on()
    {
        let (fake, mut cast) = listed();
        cast.set_enabled(KITCHEN, true).unwrap();
        fake.withdraw(KITCHEN, "Kitchen");
        cast.poll();
        assert!(cast.is_on());

        fake.state().discovery_stopped = true;
        let events = cast.poll();
        assert!(
            events.iter().any(|event| matches!(
                event,
                BackendEvent::CastFailed {
                    id,
                    problem: CastProblem::SearchStopped
                } if id == KITCHEN
            )),
            "{events:?}"
        );
        assert!(!cast.is_on());
        assert_eq!(cast.set_enabled(LIVING, true), Ok(()));
    }

    fn members(device: &str, members: Vec<GroupMember>) -> HostEvent {
        HostEvent::Session {
            attempt: AttemptId(1),
            device: id(device),
            event: SessionEvent::Members(members),
        }
    }

    fn member(device_id: &str, name: &str) -> GroupMember {
        GroupMember {
            device_id: device_id.to_owned(),
            id: DeviceId::parse(device_id),
            name: Some(name.to_owned()),
        }
    }

    fn group_members(cast: &CastSpeakers) -> CastMembers {
        cast.speakers()[0]
            .group
            .clone()
            .expect("Attic is a group")
            .members
    }

    #[test]
    fn a_single_device_session_follows_its_announced_address() {
        let (fake, mut cast) = listed();
        cast.set_streaming(true);
        cast.set_enabled(KITCHEN, true).unwrap();
        assert_eq!(fake.state().groups[0], None);
        let records = fake.state().records[0]
            .clone()
            .expect("a single device's records");
        let moved = Ipv4Addr::new(192, 0, 2, 30);
        fake.announce_at(KITCHEN, "Kitchen", DEVICE_PORT, moved);
        cast.poll();
        assert_eq!(
            records.get().group,
            [SocketAddr::new(IpAddr::V4(moved), DEVICE_PORT)]
        );
    }

    #[test]
    fn a_group_plays_as_one_speaker_and_lists_its_members_once_named() {
        let (fake, mut cast) = listed();
        cast.set_streaming(true);
        cast.set_enabled(ATTIC, true).unwrap();
        assert_eq!(fake.state().started, [id(ATTIC)]);
        let records = fake.state().groups[0].clone().expect("a group's records");
        let attic: SocketAddr = "192.0.2.10:32123".parse().unwrap();
        assert_eq!(records.get().group, [attic]);
        assert_eq!(
            group_members(&cast),
            CastMembers::NotReported,
            "not reported before it plays"
        );

        // Kitchen by its UUID in another case and with dashes, then one no
        // listed device matches.
        fake.report(playing(ATTIC));
        fake.report(members(
            ATTIC,
            vec![
                member("01234567-89AB-CDEF-0123-456789ABCDEF", "Kitchen speaker"),
                member("garage", "Garage"),
            ],
        ));
        let events = cast.poll();
        assert!(events.contains(&BackendEvent::CastChanged));
        assert_eq!(
            group_members(&cast),
            CastMembers::Known(vec!["Kitchen".to_owned(), "Garage".to_owned()])
        );
        assert_eq!(records.get().members, [attic.ip()]);

        // A leader handover moves the group's records; its session follows.
        let leader = Ipv4Addr::new(192, 0, 2, 20);
        fake.announce_at(ATTIC, "Attic", 32_123, leader);
        cast.poll();
        assert_eq!(
            records.get().group,
            [SocketAddr::new(IpAddr::V4(leader), 32_123)]
        );

        // Its members stay speakers of their own, off while the group plays.
        let kitchen = &cast.speakers()[1];
        assert_eq!(kitchen.name, "Kitchen");
        assert!(!kitchen.enabled && kitchen.group.is_none());
        assert_eq!(cast.set_enabled(KITCHEN, true), Err(CastProblem::Busy));

        // One volume, sent to the group's session.
        cast.set_volume(ATTIC, 30.0);
        assert_eq!(fake.state().volumes, [0.3]);
    }

    #[test]
    fn a_member_of_the_playing_group_stays_listed_through_a_flood_of_devices() {
        let (fake, mut cast) = listed();
        cast.set_streaming(true);
        cast.set_enabled(ATTIC, true).unwrap();
        fake.report(members(ATTIC, vec![member(KITCHEN, "Kitchen")]));
        cast.poll();
        for n in 1..=MAX_DEVICES {
            fake.announce(&format!("{n:032x}"), &format!("Speaker {n}"), DEVICE_PORT);
        }
        cast.poll();
        let listed: Vec<String> = cast.speakers().into_iter().map(|s| s.id).collect();
        assert_eq!(listed.len(), MAX_DEVICES);
        assert!(listed.iter().any(|id| id == KITCHEN));
        assert!(
            !listed.iter().any(|id| id == LIVING),
            "the oldest other one went"
        );
    }

    #[test]
    fn a_new_group_session_starts_with_its_members_unknown() {
        let (fake, mut cast) = listed();
        cast.set_streaming(true);
        cast.set_enabled(ATTIC, true).unwrap();
        fake.report(members(ATTIC, vec![member(KITCHEN, "Kitchen")]));
        cast.poll();
        cast.set_enabled(ATTIC, false).unwrap();
        assert_eq!(
            group_members(&cast),
            CastMembers::NotReported,
            "an off group shows no list"
        );
        cast.set_enabled(ATTIC, true).unwrap();
        assert_eq!(fake.state().started.len(), 2);
        assert_eq!(group_members(&cast), CastMembers::NotReported);
    }

    #[test]
    fn a_group_that_plays_without_a_member_list_has_its_members_unknown() {
        let (fake, mut cast) = listed();
        cast.set_streaming(true);
        cast.set_enabled(ATTIC, true).unwrap();
        cast.poll();
        assert_eq!(group_members(&cast), CastMembers::NotReported);
        fake.report(playing(ATTIC));
        cast.poll();
        assert_eq!(group_members(&cast), CastMembers::Unknown);
        cast.set_enabled(ATTIC, false).unwrap();
        assert_eq!(group_members(&cast), CastMembers::NotReported, "off");
    }

    #[test]
    fn members_nobody_names_are_left_out_and_none_named_is_unknown() {
        let (fake, mut cast) = listed();
        cast.set_streaming(true);
        cast.set_enabled(ATTIC, true).unwrap();
        let nameless = GroupMember {
            device_id: "porch-7".to_owned(),
            id: None,
            name: None,
        };
        fake.report(members(
            ATTIC,
            vec![member(KITCHEN, "Kitchen"), nameless.clone()],
        ));
        cast.poll();
        assert_eq!(
            group_members(&cast),
            CastMembers::Known(vec!["Kitchen".to_owned()])
        );

        fake.report(members(ATTIC, vec![nameless]));
        cast.poll();
        assert_eq!(
            group_members(&cast),
            CastMembers::NotReported,
            "not yet playing"
        );
        fake.report(playing(ATTIC));
        cast.poll();
        assert_eq!(group_members(&cast), CastMembers::Unknown);
    }

    #[test]
    fn a_group_keeps_its_own_offset_under_its_id() {
        let (fake, mut cast) = listed();
        assert!(cast.set_offset(KITCHEN, 80.0));
        assert!(cast.set_offset(ATTIC, 240.0));
        let memory = fake.state().memory.clone();
        assert_eq!(memory.offset(&id(ATTIC)), Some(Duration::from_millis(240)));
        assert_eq!(memory.offset(&id(KITCHEN)), Some(Duration::from_millis(80)));
        assert_eq!(cast.speakers()[0].offset_ms, 240.0);
        cast.set_enabled(ATTIC, true).unwrap();
        assert_eq!(cast.anchor(), Some(Duration::from_millis(240)));
    }

    #[test]
    fn group_lines_reach_the_debug_log() {
        let dir = test_dir("cast-group-log");
        let log = DebugLog::new(Some(dir.join(FILE_NAME)));
        let fake = FakeCast::default();
        fake.announce(ATTIC, "Attic", 32_123);
        let mut cast = CastSpeakers::new(fake.link(), log.clone());
        cast.poll();
        cast.set_streaming(true);
        cast.set_enabled(ATTIC, true).unwrap();
        let nameless = GroupMember {
            device_id: "porch-7".to_owned(),
            id: None,
            name: None,
        };
        fake.report(members(ATTIC, vec![member("garage", "Garage"), nameless]));
        fake.report(HostEvent::Session {
            attempt: AttemptId(1),
            device: id(ATTIC),
            event: SessionEvent::GroupMessage("CASTING_GROUPS".to_owned()),
        });
        cast.poll();
        assert!(log.flush(TEST_FLUSH));
        assert_in_order(
            &read_messages(&dir.join(FILE_NAME)),
            &[
                "Cast group 00112233445566778899aabbccddeeff on port 32123: ca=- md=-",
                "Cast: Attic switched on",
                "Cast group Attic members: 2 (Garage, unnamed, deviceId porch-7)",
                "Cast group: CASTING_GROUPS",
            ],
        );
    }

    // covers: C21
    #[test]
    fn a_changed_certificate_reaches_the_debug_log() {
        let dir = test_dir("cast-certificate-log");
        let log = DebugLog::new(Some(dir.join(FILE_NAME)));
        let fake = FakeCast::default();
        fake.announce(ATTIC, "Attic", 32_123);
        let mut cast = CastSpeakers::new(fake.link(), log.clone());
        cast.poll();
        cast.set_streaming(true);
        cast.set_enabled(ATTIC, true).unwrap();
        let before = Fingerprint([0xAB; 32]);
        let after = Fingerprint([0xCD; 32]);
        fake.report(HostEvent::Session {
            attempt: AttemptId(1),
            device: id(ATTIC),
            event: SessionEvent::CertificateChanged { before, after },
        });
        cast.poll();
        assert!(log.flush(TEST_FLUSH));
        assert_in_order(
            &read_messages(&dir.join(FILE_NAME)),
            &[&format!(
                "Cast: Attic's certificate changed from {before} to {after}"
            )],
        );
    }

    #[test]
    fn the_offset_is_clamped_kept_and_held_by_local_outputs() {
        let (fake, mut cast) = listed();
        assert!(cast.set_offset(KITCHEN, 25_000.0));
        assert_eq!(
            fake.state().memory.offset(&id(KITCHEN)),
            Some(Duration::from_secs(20))
        );
        let events = cast.poll();
        assert!(events.contains(&BackendEvent::CastMemoryChanged));
        assert!(!cast.set_offset(KITCHEN, 20_000.0));
        assert!(cast.set_offset(KITCHEN, -50.0));
        assert_eq!(cast.speakers()[1].offset_ms, 0.0);
        assert!(cast.set_offset(KITCHEN, 350.4));
        assert_eq!(cast.speakers()[1].offset_ms, 350.0);
        assert!(!cast.set_offset(KITCHEN, f64::NAN));

        // Only the speaker that is on holds the others back.
        assert_eq!(cast.anchor(), None);
        cast.set_enabled(KITCHEN, true).unwrap();
        assert_eq!(cast.anchor(), Some(Duration::from_millis(350)));
        cast.set_enabled(KITCHEN, false).unwrap();
        assert_eq!(cast.anchor(), None);
    }

    #[test]
    fn volume_mute_and_resync_reach_only_the_speaker_in_a_session() {
        let (fake, mut cast) = listed();
        cast.set_volume(KITCHEN, 40.0);
        cast.set_muted(KITCHEN, true);
        cast.resync(KITCHEN);
        assert!(fake.state().volumes.is_empty());
        assert!(fake.state().mutes.is_empty());
        assert_eq!(fake.state().resyncs, 0);

        cast.set_streaming(true);
        cast.set_enabled(KITCHEN, true).unwrap();
        cast.set_volume(LIVING, 40.0);
        cast.set_volume(KITCHEN, 40.0);
        cast.set_muted(KITCHEN, true);
        cast.resync(KITCHEN);
        assert_eq!(fake.state().volumes, [0.4]);
        assert_eq!(fake.state().mutes, [true]);
        assert_eq!(fake.state().resyncs, 1);
        assert_eq!(cast.speakers()[1].volume, 40.0);
    }

    #[test]
    fn an_unanswered_volume_is_forgotten_when_the_session_ends() {
        let (fake, mut cast) = listed();
        cast.set_streaming(true);
        cast.set_enabled(KITCHEN, true).unwrap();
        cast.set_volume(KITCHEN, 40.0);
        cast.set_enabled(KITCHEN, false).unwrap();
        assert_eq!(cast.speakers()[1].volume, 100.0, "stopped");

        cast.set_enabled(KITCHEN, true).unwrap();
        cast.set_volume(KITCHEN, 40.0);
        fake.report(HostEvent::Ended {
            attempt: AttemptId(2),
            device: id(KITCHEN),
            end: SessionEnd::TakenOver,
        });
        cast.poll();
        assert_eq!(cast.speakers()[1].volume, 100.0, "ended");
    }

    // covers: C105, S252 (a preset sets the volume before streaming starts)
    #[test]
    fn a_volume_set_before_the_session_is_kept_and_set_at_its_first_status() {
        let (fake, mut cast) = listed();
        cast.set_enabled(KITCHEN, true).unwrap();
        cast.set_volume(KITCHEN, 30.0);
        assert!(sent(&fake).is_empty(), "no session to send it to");
        assert_eq!(shown(&cast, KITCHEN), 30.0, "the slider stays");

        cast.set_streaming(true);
        report(&fake, &mut cast, 0.8);
        assert_eq!(sent(&fake), [0.3]);
        assert_eq!(shown(&cast, KITCHEN), 30.0);
    }

    // covers: C70, C104 (the sender paces a drag after the host recorded it)
    #[test]
    fn the_answer_to_the_first_level_of_a_fast_drag_is_not_read_as_a_change() {
        let (fake, mut cast) = listed();
        cast.set_streaming(true);
        cast.set_enabled(KITCHEN, true).unwrap();
        report(&fake, &mut cast, 1.0);
        for position in (40..100).rev() {
            cast.set_volume(KITCHEN, f64::from(position));
        }
        report(&fake, &mut cast, 0.99);
        report(&fake, &mut cast, 0.4);
        assert_eq!(shown(&cast, KITCHEN), 40.0);
    }

    // covers: C70, C104 (answers come back in send order)
    #[test]
    fn a_late_answer_to_a_level_the_drag_passed_again_matches_the_one_sent_first() {
        let (fake, mut cast) = listed();
        cast.set_streaming(true);
        cast.set_enabled(KITCHEN, true).unwrap();
        report(&fake, &mut cast, 0.5);
        for position in (51..=60).chain((52..60).rev()) {
            cast.set_volume(KITCHEN, f64::from(position));
        }
        // What 100 ms pacing put on the wire, answered in that order.
        for answer in [0.51, 0.56, 0.6, 0.55, 0.52] {
            report(&fake, &mut cast, answer);
        }
        assert_eq!(shown(&cast, KITCHEN), 52.0);
    }

    // covers: C70, C104 (a drag of 60 levels, 100 ms pacing, 400 ms echoes)
    #[test]
    fn a_long_fast_drag_ends_on_the_last_level_once_its_echoes_arrive() {
        let (fake, mut cast) = listed();
        cast.set_streaming(true);
        cast.set_enabled(KITCHEN, true).unwrap();
        report(&fake, &mut cast, 0.2);
        let moves: Vec<(u32, f64)> = (0..60)
            .map(|i| (i * 1000 / 59, f64::from(21 + i)))
            .collect();
        let mut position = 20.0;
        let mut on_wire = 20.0;
        let mut echoes = Vec::new();
        for now in 0..=1400 {
            if let Some(&(_, next)) = moves.iter().find(|(at, _)| *at == now) {
                cast.set_volume(KITCHEN, next);
                position = next;
            }
            if now % 100 == 0 && position != on_wire {
                on_wire = position;
                echoes.push((now + 400, position / 100.0));
            }
            for &(_, level) in echoes.iter().filter(|(at, _)| *at == now) {
                report(&fake, &mut cast, level);
            }
        }
        assert_eq!(shown(&cast, KITCHEN), 80.0);
    }

    // covers: C70, C104 (a device that rounds to its 0.05 step)
    #[test]
    fn a_device_that_rounds_to_its_step_answers_within_half_a_step() {
        let (fake, mut cast) = listed();
        cast.set_streaming(true);
        cast.set_enabled(KITCHEN, true).unwrap();
        let stepped = |level| {
            fake.report(HostEvent::Session {
                attempt: AttemptId(1),
                device: id(KITCHEN),
                event: SessionEvent::Volume(DeviceVolume {
                    level,
                    step_interval: Some(0.05),
                    has_level: true,
                    ..DeviceVolume::default()
                }),
            });
        };
        stepped(0.5);
        cast.poll();
        cast.set_volume(KITCHEN, 33.0);
        stepped(0.35);
        cast.poll();
        assert_eq!(shown(&cast, KITCHEN), 33.0, "the answer to 0.33");

        // 0.36 rounds to the 0.35 it has, so no answer comes.
        cast.set_volume(KITCHEN, 36.0);
        stepped(0.4);
        cast.poll();
        stepped(0.35);
        cast.poll();
        assert_eq!(shown(&cast, KITCHEN), 35.0, "a change from elsewhere");
    }

    // covers: C105 (a device that rounds to its 0.05 step)
    #[test]
    fn a_first_status_within_half_a_step_of_the_slider_is_not_set_again() {
        let (fake, mut cast) = listed();
        cast.set_enabled(KITCHEN, true).unwrap();
        cast.set_volume(KITCHEN, 36.0);
        cast.set_streaming(true);
        fake.report(HostEvent::Session {
            attempt: AttemptId(1),
            device: id(KITCHEN),
            event: SessionEvent::Volume(DeviceVolume {
                level: 0.35,
                step_interval: Some(0.05),
                has_level: true,
                ..DeviceVolume::default()
            }),
        });
        cast.poll();
        assert!(sent(&fake).is_empty(), "{:?}", sent(&fake));
        assert_eq!(shown(&cast, KITCHEN), 36.0);
    }

    // covers: C70, C104, C105 (nothing is sent before the first status)
    #[test]
    fn a_level_set_before_the_first_status_never_swallows_a_later_change_from_elsewhere() {
        let (fake, mut cast) = listed();
        cast.set_streaming(true);
        cast.set_enabled(KITCHEN, true).unwrap();
        cast.set_volume(KITCHEN, 30.0);
        report(&fake, &mut cast, 0.8);
        report(&fake, &mut cast, 0.3);
        report(&fake, &mut cast, 0.7);
        assert_eq!(shown(&cast, KITCHEN), 70.0);
        report(&fake, &mut cast, 0.3);
        assert_eq!(shown(&cast, KITCHEN), 30.0);
    }

    #[test]
    fn the_capture_feeds_the_current_session() {
        let (fake, mut cast) = listed();
        let mut lease = cast.lease().expect("the tap");
        assert!(cast.lease().is_none());
        cast.set_streaming(true);
        cast.set_enabled(KITCHEN, true).unwrap();
        lease.push(&[0.25; 64], 32);
        assert_eq!(fake.state().readers[0].available(), 32);
        drop(lease);
        assert!(cast.lease().is_some());
    }

    #[test]
    fn the_session_hears_the_capture_held_back() {
        let (fake, mut cast) = listed();
        let mut lease = cast.lease().expect("the tap");
        cast.set_streaming(true);
        cast.set_enabled(KITCHEN, true).unwrap();
        cast.set_hold_back(1.0);
        lease.push(&[0.25; 128], 64);
        let mut bytes = Vec::new();
        fake.state().readers[0].read_into(&mut bytes, 64);
        let (held, heard) = bytes.split_at(48 * 4);
        assert!(held.iter().all(|byte| *byte == 0), "1 ms of silence first");
        assert!(heard.iter().any(|byte| *byte != 0));
    }

    fn delayed(line: &mut DelayLine, input: &[f32], target: usize) -> Vec<f32> {
        let mut out = Vec::new();
        line.process(input, target, |held| out.extend_from_slice(held));
        out
    }

    #[test]
    fn the_delay_line_passes_audio_on_unchanged_or_late_by_its_hold_back() {
        let ramp: Vec<f32> = (0..2000).map(|i| i as f32 / 4000.0).collect();
        assert_eq!(delayed(&mut DelayLine::new(4800), &ramp, 0), ramp);
        let out = delayed(&mut DelayLine::new(4800), &ramp, 100);
        assert!(out[..200].iter().all(|sample| *sample == 0.0));
        assert_eq!(out[200..], ramp[..1800]);
        let out = delayed(&mut DelayLine::new(50), &ramp, 100);
        assert_eq!(out[100..], ramp[..1900], "held back at most its length");

        let long: Vec<f32> = (0..(MAX_HOLD_BACK_FRAMES + 1000) * 2)
            .map(|i| (i % 4000) as f32 / 4000.0)
            .collect();
        let out = delayed(
            &mut DelayLine::new(MAX_HOLD_BACK_FRAMES),
            &long,
            MAX_HOLD_BACK_FRAMES,
        );
        let (held, heard) = out.split_at(MAX_HOLD_BACK_FRAMES * 2);
        assert!(held.iter().all(|sample| *sample == 0.0));
        assert_eq!(heard, &long[..2000], "the full 20 s");
        let (_fake, mut cast) = listed();
        cast.set_hold_back(MAX_HOLD_BACK_MS);
        assert_eq!(
            cast.feed.hold_back.load(Ordering::Relaxed),
            MAX_HOLD_BACK_FRAMES
        );
    }

    #[test]
    fn a_new_hold_back_fades_out_and_in_without_a_jump() {
        let step = std::f32::consts::TAU * 440.0 / SESSION_RATE as f32;
        let sine: Vec<f32> = (0..24_000)
            .flat_map(|i| {
                let sample = 0.5 * (i as f32 * step).sin();
                [sample, sample]
            })
            .collect();
        let mut line = DelayLine::new(48_000);
        let mut out = delayed(&mut line, &sine[..9600], 0);
        out.extend(delayed(&mut line, &sine[9600..], 12_000));
        let largest = out
            .as_chunks::<2>()
            .0
            .windows(2)
            .map(|pair| (pair[1][0] - pair[0][0]).abs())
            .fold(0.0, f32::max);
        assert!(largest < 0.5 * step * 1.5, "largest step {largest}");
        assert_eq!(
            out[40_000..],
            sine[16_000..24_000],
            "then 12 000 frames late"
        );
    }

    #[test]
    fn a_writer_the_full_handoff_refused_reaches_the_capture_after_the_next_poll() {
        let mut feed = CastFeed::new();
        let mut lease = feed.slot().lease().expect("the tap");
        let mut readers = Vec::new();
        for n in 0..=HANDOFF {
            let (writer, reader) = audio_ring(64);
            readers.push(reader);
            assert_eq!(feed.hand(Some(writer)), n < HANDOFF, "handoff {n}");
        }
        lease.push(&[0.5; 8], 4);
        assert_eq!(readers[HANDOFF - 1].available(), 4);
        assert!(feed.flush(), "the newest writer goes over now");
        lease.push(&[0.5; 8], 4);
        assert_eq!(readers[HANDOFF].available(), 4);
        assert_eq!(readers[HANDOFF - 1].available(), 4);
    }

    #[test]
    fn old_writers_go_back_to_the_ui_thread_even_while_its_outbox_is_full() {
        let feed = CastFeed::with_handoff(1);
        let mut lease = feed.slot().lease().expect("the tap");
        let (first, _) = audio_ring(64);
        let (second, _) = audio_ring(64);
        let (third, reader) = audio_ring(64);
        for writer in [first, second, third] {
            feed.to_tap.try_send(Some(CastWriter::new(writer))).unwrap();
            lease.push(&[0.5; 2], 1);
        }
        assert_eq!(reader.available(), 1, "the newest writer plays");
        assert_eq!(feed.from_tap.try_iter().count(), 1);
        // The tap kept the second writer until the outbox had room.
        lease.push(&[], 0);
        assert_eq!(feed.from_tap.try_iter().count(), 1);
    }

    #[test]
    fn shutting_down_tears_the_sender_down_once() {
        let (fake, mut cast) = listed();
        let thread = cast.begin_shutdown().expect("a teardown");
        assert!(thread.join().unwrap());
        assert_eq!(fake.state().shutdowns, 1);
        assert!(cast.begin_shutdown().is_none());
        assert_eq!(cast.discovery(), CastDiscovery::Off);
    }

    fn address(text: &str) -> CastAddress {
        CastAddress::parse(text).expect("an address")
    }

    /// Polls until the fake was asked to start a session, for a lookup
    /// that answers on its own thread.
    fn poll_until_started(fake: &FakeCast, cast: &mut CastSpeakers) {
        for _ in 0..400 {
            cast.poll();
            if !fake.state().started.is_empty() {
                return;
            }
            thread::sleep(Duration::from_millis(5));
        }
        panic!("no session started");
    }

    // covers: C18
    #[test]
    fn a_speaker_added_by_address_plays_under_its_address_until_removed() {
        let (fake, mut cast) = listed();
        cast.set_probe(|_| true);
        cast.add_address(address("192.0.2.20"));
        cast.add_address(address("192.0.2.20:8009"));
        let speakers = cast.speakers();
        assert_eq!(speakers.len(), 4, "listed once");
        let row = speakers
            .iter()
            .find(|speaker| speaker.address.is_some())
            .expect("the added one");
        assert_eq!(row.name, "192.0.2.20");
        assert_eq!(row.address.as_deref(), Some("192.0.2.20"));
        assert_eq!(row.id, address("192.0.2.20").device_id().to_string());
        let row_id = row.id.clone();

        assert_eq!(cast.set_enabled(&row_id, true), Ok(()));
        cast.set_streaming(true);
        poll_until_started(&fake, &mut cast);
        assert_eq!(fake.state().started, [address("192.0.2.20").device_id()]);
        assert_eq!(
            fake.state().targets,
            [vec![SocketAddr::from(([192, 0, 2, 20], 8009))]]
        );
        assert_eq!(status(&cast, &row_id), CastStatus::Connecting);

        cast.remove_address(&address("192.0.2.20"));
        assert_eq!(fake.state().stops, 1, "switched off first");
        assert_eq!(cast.speakers().len(), 3);
        assert!(!cast.chosen());
    }

    // covers: C18
    // No record vouches for a speaker added by address, so it stays
    // offline, and nothing waits for it, until a check reaches it.
    #[test]
    fn a_speaker_added_by_address_that_doesnt_answer_stays_offline_and_off_the_air() {
        let fake = FakeCast::default();
        let mut cast = CastSpeakers::new(fake.link(), DebugLog::default());
        cast.set_probe(|_| false);
        cast.add_address(address("192.0.2.21"));
        let row_id = address("192.0.2.21").device_id().to_string();
        assert_eq!(cast.set_enabled(&row_id, true), Ok(()));
        cast.set_streaming(true);
        for _ in 0..40 {
            cast.poll();
            thread::sleep(Duration::from_millis(5));
        }
        assert!(
            fake.state().started.is_empty(),
            "no session to a silent address"
        );
        assert!(!cast.chosen(), "local outputs don't wait for it");
        assert_eq!(cast.speakers().len(), 1, "still listed");

        cast.set_probe(|_| true);
        poll_until_started(&fake, &mut cast);
        assert!(cast.chosen());
    }

    // covers: C16, C18
    #[test]
    fn the_address_of_a_listed_device_marks_that_device_instead_of_a_second_row() {
        let fake = FakeCast::default();
        fake.announce_at(
            KITCHEN,
            "Kitchen",
            DEVICE_PORT,
            Ipv4Addr::new(192, 0, 2, 30),
        );
        let mut cast = CastSpeakers::new(fake.link(), DebugLog::default());
        cast.poll();
        cast.add_address(address("192.0.2.30"));
        let speakers = cast.speakers();
        assert_eq!(speakers.len(), 1, "no second row");
        assert_eq!(speakers[0].id, KITCHEN);
        assert_eq!(speakers[0].address.as_deref(), Some("192.0.2.30"));

        cast.remove_address(&address("192.0.2.30"));
        let speakers = cast.speakers();
        assert_eq!(speakers.len(), 1, "a found device stays");
        assert_eq!(speakers[0].address, None);
    }

    // covers: C18
    #[test]
    fn a_browse_that_finds_a_speaker_added_by_address_keeps_it_on_with_its_offset() {
        let fake = FakeCast::default();
        let mut cast = CastSpeakers::new(fake.link(), DebugLog::default());
        cast.set_probe(|_| true);
        cast.add_address(address("192.0.2.30"));
        let manual = address("192.0.2.30").device_id();
        assert!(cast.set_offset(&manual.to_string(), 400.0));
        assert_eq!(cast.set_enabled(&manual.to_string(), true), Ok(()));
        cast.set_streaming(true);
        poll_until_started(&fake, &mut cast);

        fake.announce_at(
            KITCHEN,
            "Kitchen",
            DEVICE_PORT,
            Ipv4Addr::new(192, 0, 2, 30),
        );
        cast.poll();
        let speakers = cast.speakers();
        assert_eq!(speakers.len(), 1);
        assert_eq!(
            (speakers[0].id.as_str(), speakers[0].name.as_str()),
            (KITCHEN, "Kitchen")
        );
        assert_eq!(
            speakers[0].address.as_deref(),
            Some("192.0.2.30"),
            "still added by address, so it can be removed"
        );
        assert!(speakers[0].enabled);
        assert_eq!(speakers[0].offset_ms, 400.0);
        assert_eq!(fake.state().started, [manual, id(KITCHEN)]);
        assert_eq!(fake.state().stops, 1);
    }

    // covers: C18, C77
    #[test]
    fn a_browse_moves_the_offset_over_a_zero_but_not_over_an_offset_the_id_has() {
        const LIVING: &str = "fedcba9876543210fedcba9876543210";
        let fake = FakeCast::default();
        let mut cast = CastSpeakers::new(fake.link(), DebugLog::default());
        for (text, id, own) in [("192.0.2.31", KITCHEN, 0.0), ("192.0.2.32", LIVING, 100.0)] {
            cast.add_address(address(text));
            assert!(cast.set_offset(&address(text).device_id().to_string(), 400.0));
            assert!(cast.set_offset(id, own));
        }
        fake.announce_at(
            KITCHEN,
            "Kitchen",
            DEVICE_PORT,
            Ipv4Addr::new(192, 0, 2, 31),
        );
        fake.announce_at(LIVING, "Living", DEVICE_PORT, Ipv4Addr::new(192, 0, 2, 32));
        cast.poll();
        let offset = |id: &str| {
            cast.speakers()
                .into_iter()
                .find(|speaker| speaker.id == id)
                .map(|speaker| speaker.offset_ms)
        };
        assert_eq!(
            offset(KITCHEN),
            Some(400.0),
            "0 ms, left by an Undo, counts as none"
        );
        assert_eq!(offset(LIVING), Some(100.0), "the ID's own offset stays");
    }

    // covers: C18
    #[test]
    fn a_host_name_is_listed_at_once_and_plays_once_looked_up() {
        let fake = FakeCast::default();
        let mut cast = CastSpeakers::new(fake.link(), DebugLog::default());
        cast.set_resolver(|host, port| {
            assert_eq!(host, "speaker.local");
            Ok(SocketAddr::from(([192, 0, 2, 40], port)))
        });
        cast.set_probe(|_| true);
        cast.add_address(address("Speaker.local:8010"));
        let speakers = cast.speakers();
        assert_eq!(speakers.len(), 1);
        assert_eq!(speakers[0].name, "speaker.local:8010");
        let row_id = speakers[0].id.clone();
        assert_eq!(cast.set_enabled(&row_id, true), Ok(()));
        cast.set_streaming(true);
        poll_until_started(&fake, &mut cast);
        assert_eq!(
            fake.state().targets,
            [vec![SocketAddr::from(([192, 0, 2, 40], 8010))]]
        );
        assert_eq!(cast.speakers().len(), 1);
    }

    #[test]
    fn a_host_name_that_doesnt_resolve_stays_listed_until_removed() {
        let fake = FakeCast::default();
        let mut cast = CastSpeakers::new(fake.link(), DebugLog::default());
        cast.set_resolver(|_, _| Err(io::Error::new(io::ErrorKind::NotFound, "unknown")));
        cast.add_address(address("speaker.local"));
        for _ in 0..20 {
            cast.poll();
            thread::sleep(Duration::from_millis(5));
        }
        let speakers = cast.speakers();
        assert_eq!(speakers.len(), 1);
        assert_eq!(speakers[0].address.as_deref(), Some("speaker.local"));
        cast.remove_address(&address("speaker.local"));
        assert!(cast.speakers().is_empty());
    }

    // covers: C2, C18
    #[test]
    fn a_host_name_that_looks_up_to_a_browsed_device_plays_as_that_device() {
        let fake = FakeCast::default();
        fake.announce_at(
            KITCHEN,
            "Kitchen",
            DEVICE_PORT,
            Ipv4Addr::new(192, 0, 2, 50),
        );
        let mut cast = CastSpeakers::new(fake.link(), DebugLog::default());
        cast.poll();
        cast.set_resolver(|_, port| Ok(SocketAddr::from(([192, 0, 2, 50], port))));
        cast.add_address(address("kitchen.local"));
        let manual = address("kitchen.local").device_id().to_string();
        assert_eq!(cast.set_enabled(&manual, true), Ok(()));
        cast.set_streaming(true);
        poll_until_started(&fake, &mut cast);
        assert_eq!(fake.state().started, [id(KITCHEN)]);
        assert_eq!(cast.set_enabled(KITCHEN, true), Ok(()), "not busy");
    }

    fn track(title: &str, artwork: Option<&[u8]>) -> CastTrack {
        CastTrack {
            metadata: Metadata {
                title: Some(title.to_owned()),
                ..Metadata::default()
            },
            artwork: artwork.map(Arc::from),
        }
    }

    #[test]
    fn a_start_waits_for_the_track_and_gives_it_to_the_first_load() {
        let (fake, mut cast) = listed();
        cast.set_track(None);
        cast.set_streaming(true);
        assert_eq!(cast.set_enabled(KITCHEN, true), Ok(()));
        assert!(fake.state().started.is_empty(), "waits while it is read");

        let first = track("First", Some(&[0xff, 0xd8]));
        cast.set_track(Some(first.clone()));
        assert_eq!(fake.state().started, [id(KITCHEN)]);
        assert_eq!(fake.state().start_tracks, std::slice::from_ref(&first));
        cast.set_track(Some(first.clone()));
        assert!(fake.state().tracks.is_empty(), "the start already had it");
    }

    #[test]
    fn a_new_track_goes_to_the_session_once_and_an_unchanged_one_never() {
        let (fake, mut cast) = listed();
        cast.set_streaming(true);
        assert_eq!(cast.set_enabled(KITCHEN, true), Ok(()));
        assert_eq!(fake.state().start_tracks, [CastTrack::default()]);
        fake.report(playing(KITCHEN));
        cast.poll();

        let second = track("Second", None);
        cast.set_track(Some(second.clone()));
        cast.set_track(Some(second.clone()));
        assert_eq!(fake.state().tracks, std::slice::from_ref(&second));
        assert_eq!(fake.state().started.len(), 1, "no restart for a track");

        // A reading that stops and comes back with the same track sends nothing.
        cast.set_track(None);
        cast.set_track(Some(second.clone()));
        assert_eq!(fake.state().tracks.len(), 1);
        let third = track("Third", Some(&[0x89, b'P']));
        cast.set_track(Some(third.clone()));
        assert_eq!(fake.state().tracks, [second, third]);
    }

    #[test]
    fn the_volume_keys_move_by_the_device_step_or_1_without_one() {
        assert_eq!(volume_step(None), 1.0);
        let reported = |step| DeviceVolume {
            step_interval: step,
            has_level: true,
            ..DeviceVolume::default()
        };
        assert_eq!(volume_step(Some(&reported(None))), 1.0);
        assert_eq!(volume_step(Some(&reported(Some(0.05)))), 5.0);
        assert_eq!(volume_step(Some(&reported(Some(0.07)))), 7.0);
        assert_eq!(volume_step(Some(&reported(Some(0.125)))), 13.0);
        assert_eq!(volume_step(Some(&reported(Some(0.004)))), 1.0);
        assert_eq!(volume_step(Some(&reported(Some(1.0)))), 100.0);
        for bad in [0.0, -0.1, 1.5, f64::NAN] {
            assert_eq!(volume_step(Some(&reported(Some(bad)))), 1.0, "{bad}");
        }

        let (fake, mut cast) = listed();
        cast.set_streaming(true);
        assert_eq!(cast.set_enabled(KITCHEN, true), Ok(()));
        let step_of = |cast: &CastSpeakers| {
            cast.speakers()
                .into_iter()
                .find(|info| info.id == KITCHEN)
                .map(|info| info.volume_step)
        };
        assert_eq!(step_of(&cast), Some(1.0));
        fake.report(HostEvent::Session {
            attempt: AttemptId(1),
            device: id(KITCHEN),
            event: SessionEvent::Volume(reported(Some(0.02))),
        });
        cast.poll();
        assert_eq!(step_of(&cast), Some(2.0));
    }

    fn reported(device: &str, level: f64, control_type: ControlType) -> HostEvent {
        HostEvent::Session {
            attempt: AttemptId(1),
            device: id(device),
            event: SessionEvent::Volume(DeviceVolume {
                level,
                control_type,
                has_level: true,
                ..DeviceVolume::default()
            }),
        }
    }

    fn shown(cast: &CastSpeakers, device: &str) -> f64 {
        cast.speakers()
            .into_iter()
            .find(|info| info.id == device)
            .map(|info| info.volume)
            .expect("listed")
    }

    fn report(fake: &FakeCast, cast: &mut CastSpeakers, level: f64) {
        fake.report(reported(KITCHEN, level, ControlType::Attenuation));
        cast.poll();
    }

    fn sent(fake: &FakeCast) -> Vec<f64> {
        fake.state().volumes.clone()
    }

    #[test]
    fn the_master_scales_the_level_sent_and_a_master_change_resends_it() {
        let (fake, mut cast) = listed();
        cast.set_master(50.0);
        cast.set_streaming(true);
        cast.set_enabled(KITCHEN, true).unwrap();
        cast.set_volume(KITCHEN, 80.0);
        assert_eq!(sent(&fake), [0.4]);
        assert_eq!(shown(&cast, KITCHEN), 80.0);

        cast.set_master(0.0);
        assert_eq!(sent(&fake), [0.4, 0.0], "master 0 sends level 0");
        assert!(fake.state().mutes.is_empty(), "never the muted flag");
        assert_eq!(shown(&cast, KITCHEN), 80.0);

        report(&fake, &mut cast, 0.0);
        cast.set_master(100.0);
        assert_eq!(sent(&fake), [0.4, 0.0, 0.8]);
        assert_eq!(shown(&cast, KITCHEN), 80.0);
    }

    #[test]
    fn a_zone_scales_the_level_sent_and_a_zone_change_resends_it() {
        let (fake, mut cast) = listed();
        cast.set_master(50.0);
        cast.set_zones(BTreeMap::from([(KITCHEN.to_owned(), 50.0)]));
        cast.set_streaming(true);
        cast.set_enabled(KITCHEN, true).unwrap();
        cast.set_volume(KITCHEN, 80.0);
        assert_eq!(sent(&fake), [0.2], "master × zone × own");
        cast.set_zones(BTreeMap::from([(KITCHEN.to_owned(), 50.0)]));
        assert_eq!(sent(&fake), [0.2], "unchanged: nothing goes out");
        cast.set_zones(BTreeMap::from([(KITCHEN.to_owned(), 0.0)]));
        assert_eq!(sent(&fake), [0.2, 0.0], "zone 0 sends level 0");
        assert!(fake.state().mutes.is_empty(), "never the muted flag");
        cast.set_zones(BTreeMap::new());
        assert_eq!(sent(&fake), [0.2, 0.0, 0.4], "out of the zone");
        assert_eq!(shown(&cast, KITCHEN), 80.0);
    }

    #[test]
    fn a_cast_report_in_a_zone_moves_the_slider_by_master_times_zone() {
        let (fake, mut cast) = listed();
        cast.set_master(50.0);
        cast.set_zones(BTreeMap::from([(KITCHEN.to_owned(), 50.0)]));
        cast.set_streaming(true);
        cast.set_enabled(KITCHEN, true).unwrap();
        // No own level yet: the first status's 0.2 ÷ (0.5 × 0.5) (C105).
        report(&fake, &mut cast, 0.2);
        assert!((shown(&cast, KITCHEN) - 80.0).abs() < 1e-9);
        assert!(sent(&fake).is_empty(), "it plays what the sliders show");
        report(&fake, &mut cast, 0.1);
        assert!((shown(&cast, KITCHEN) - 40.0).abs() < 1e-9);
        cast.set_zones(BTreeMap::from([(KITCHEN.to_owned(), 0.0)]));
        assert_eq!(sent(&fake), [0.0]);
        report(&fake, &mut cast, 0.0);
        report(&fake, &mut cast, 0.3);
        assert!(
            (shown(&cast, KITCHEN) - 40.0).abs() < 1e-9,
            "zone 0 leaves it"
        );
    }

    #[test]
    fn a_level_from_elsewhere_moves_the_slider_by_the_master() {
        let (fake, mut cast) = listed();
        cast.set_master(50.0);
        cast.set_streaming(true);
        cast.set_enabled(KITCHEN, true).unwrap();
        report(&fake, &mut cast, 0.2);
        assert!((shown(&cast, KITCHEN) - 40.0).abs() < 1e-9);
        assert!(sent(&fake).is_empty(), "nothing sent back");

        cast.set_master(0.0);
        assert_eq!(sent(&fake), [0.0]);
        report(&fake, &mut cast, 0.0);
        report(&fake, &mut cast, 0.3);
        assert!(
            (shown(&cast, KITCHEN) - 40.0).abs() < 1e-9,
            "master 0 leaves it"
        );
        assert_eq!(sent(&fake), [0.0], "a change from elsewhere isn't fought");
    }

    #[test]
    fn a_session_starts_no_louder_than_the_master_allows() {
        let (fake, mut cast) = listed();
        cast.set_master(0.0);
        cast.set_streaming(true);
        cast.set_enabled(KITCHEN, true).unwrap();
        report(&fake, &mut cast, 0.7);
        assert_eq!(sent(&fake), [0.0], "turned on at master 0");
        assert!((shown(&cast, KITCHEN) - 70.0).abs() < 1e-9);
        report(&fake, &mut cast, 0.0);
        report(&fake, &mut cast, 0.5);
        assert_eq!(sent(&fake), [0.0], "only the first status is answered");

        cast.set_enabled(KITCHEN, false).unwrap();
        cast.set_master(50.0);
        cast.set_enabled(KITCHEN, true).unwrap();
        fake.report(HostEvent::Session {
            attempt: AttemptId(2),
            device: id(KITCHEN),
            event: SessionEvent::Volume(DeviceVolume {
                level: 0.8,
                has_level: true,
                ..DeviceVolume::default()
            }),
        });
        cast.poll();
        assert_eq!(sent(&fake), [0.0, 0.35], "louder than master × own: set");
        assert!((shown(&cast, KITCHEN) - 70.0).abs() < 1e-9, "not read back");
    }

    #[test]
    fn a_first_status_without_a_level_keeps_the_own_level() {
        let (fake, mut cast) = listed();
        cast.set_master(50.0);
        cast.set_streaming(true);
        cast.set_enabled(KITCHEN, true).unwrap();
        let before = shown(&cast, KITCHEN);
        let payload = serde_json::json!({"status": {"volume": {"muted": false}}});
        fake.report(HostEvent::Session {
            attempt: AttemptId(1),
            device: id(KITCHEN),
            event: SessionEvent::Volume(
                audiosync_cast::status::ReceiverStatus::from_payload(
                    payload.as_object().expect("object"),
                )
                .volume,
            ),
        });
        cast.poll();
        assert!(sent(&fake).is_empty(), "{:?}", sent(&fake));
        assert_eq!(shown(&cast, KITCHEN), before);
        report(&fake, &mut cast, 0.2);
        assert!(sent(&fake).is_empty(), "{:?}", sent(&fake));
        assert!((shown(&cast, KITCHEN) - 40.0).abs() < 1e-9);
    }

    #[test]
    fn a_level_the_device_already_has_is_not_waited_for() {
        let (fake, mut cast) = listed();
        cast.set_streaming(true);
        cast.set_enabled(KITCHEN, true).unwrap();
        report(&fake, &mut cast, 0.5);
        cast.set_volume(KITCHEN, 0.0);
        report(&fake, &mut cast, 0.0);
        for master in [60.0, 50.0, 40.0] {
            cast.set_master(master);
        }
        report(&fake, &mut cast, 0.3);
        report(&fake, &mut cast, 0.0);
        assert_eq!(shown(&cast, KITCHEN), 0.0, "the buttons' 0 is read back");
        cast.set_master(100.0);
        assert_eq!(
            sent(&fake).last(),
            Some(&0.0),
            "a muted speaker stays silent"
        );
    }

    #[test]
    fn a_session_starts_at_master_times_the_own_level_even_when_the_device_is_quieter() {
        let (fake, mut cast) = listed();
        cast.set_streaming(true);
        cast.set_enabled(KITCHEN, true).unwrap();
        report(&fake, &mut cast, 0.5);
        cast.set_volume(KITCHEN, 80.0);
        report(&fake, &mut cast, 0.8);
        assert_eq!(sent(&fake), [0.8]);

        cast.set_enabled(KITCHEN, false).unwrap();
        cast.set_enabled(KITCHEN, true).unwrap();
        fake.report(HostEvent::Session {
            attempt: AttemptId(2),
            device: id(KITCHEN),
            event: SessionEvent::Volume(DeviceVolume {
                level: 0.1,
                has_level: true,
                ..DeviceVolume::default()
            }),
        });
        cast.poll();
        assert_eq!(sent(&fake), [0.8, 0.8], "quieter than master × own: set");
        assert_eq!(shown(&cast, KITCHEN), 80.0, "not read back");
    }

    #[test]
    fn a_group_follows_the_master_and_a_fixed_volume_gets_nothing() {
        let (fake, mut cast) = listed();
        cast.set_master(50.0);
        cast.set_streaming(true);
        cast.set_enabled(ATTIC, true).unwrap();
        cast.set_volume(ATTIC, 80.0);
        assert_eq!(sent(&fake), [0.4]);
        cast.set_master(100.0);
        assert_eq!(sent(&fake), [0.4, 0.8]);

        cast.set_enabled(ATTIC, false).unwrap();
        cast.set_enabled(KITCHEN, true).unwrap();
        fake.report(HostEvent::Session {
            attempt: AttemptId(2),
            device: id(KITCHEN),
            event: SessionEvent::Volume(DeviceVolume {
                level: 0.6,
                control_type: ControlType::Fixed,
                has_level: true,
                ..DeviceVolume::default()
            }),
        });
        cast.poll();
        cast.set_master(20.0);
        assert_eq!(
            sent(&fake),
            [0.4, 0.8],
            "a fixed volume sends nothing (C68)"
        );
    }

    fn volume_fixed(cast: &CastSpeakers, id: &str) -> bool {
        cast.speakers()
            .into_iter()
            .find(|speaker| speaker.id == id)
            .expect("listed")
            .volume_fixed
    }

    // covers: C68
    #[test]
    fn a_fixed_volume_report_marks_only_that_speaker_fixed() {
        let (fake, mut cast) = listed();
        cast.set_streaming(true);
        cast.set_enabled(KITCHEN, true).unwrap();
        assert!(!volume_fixed(&cast, KITCHEN), "unknown until reported");
        let report = |control_type| HostEvent::Session {
            attempt: AttemptId(1),
            device: id(KITCHEN),
            event: SessionEvent::Volume(DeviceVolume {
                level: 0.6,
                control_type,
                has_level: true,
                ..DeviceVolume::default()
            }),
        };
        fake.report(report(ControlType::Fixed));
        cast.poll();
        assert!(volume_fixed(&cast, KITCHEN));
        assert!(!volume_fixed(&cast, ATTIC));

        fake.report(report(ControlType::Attenuation));
        cast.poll();
        assert!(!volume_fixed(&cast, KITCHEN));
    }
}
