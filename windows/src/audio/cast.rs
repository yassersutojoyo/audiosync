//! Google Cast speakers, through AudioSync's own sender (`audiosync-cast`).
//!
//! Discovery lists every device the network announces; a Cast group is
//! listed as one AudioSync can't play to (spec C3). One device is on at a
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

use std::collections::BTreeMap;
use std::io;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use audiosync_cast::audio::{AudioReader, AudioWriter, audio_ring};
use audiosync_cast::discovery::mdns::MdnsBrowser;
use audiosync_cast::discovery::{
    BrowseEvent, DeviceId, DeviceKey, DeviceKind, DeviceList, Entry, ListEvent,
};
use audiosync_cast::host::{
    AttemptId, CastHost, CastTarget, DeviceMemory, HostEvent, StartRefused,
};
use audiosync_cast::media::Metadata;
use audiosync_cast::session::{SessionEnd, SessionError, SessionEvent};
use audiosync_cast::status::{ControlType, DeviceVolume};
use audiosync_core::fade::Fade;
use audiosync_core::policy::delay::MAX_HOLD_BACK_MS;

use super::SESSION_RATE;
use super::backend::{BackendEvent, CastDiscovery, CastInfo, CastProblem, CastStatus};
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
    fn hand(&mut self, writer: Option<AudioWriter>) -> bool {
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
        metadata: Metadata,
        reader: AudioReader,
    ) -> Result<AttemptId, StartRefused>;
    fn stop(&mut self);
    /// 0.0 to 1.0.
    fn set_volume(&mut self, level: f64);
    fn set_muted(&mut self, muted: bool);
    fn resync(&mut self);
    fn set_offset(&mut self, device: DeviceId, offset: Duration);
    fn memory(&self) -> DeviceMemory;
    fn poll_event(&mut self) -> Option<HostEvent>;
    fn poll_browse(&mut self) -> Option<BrowseEvent>;
    fn discovery(&self) -> CastDiscovery;
    /// Tears down whatever runs within C88's budget; the one call that
    /// waits. Returns whether it finished in time.
    fn shutdown(&mut self) -> bool;
}

/// The real sender: [`CastHost`] and a live mDNS browser.
pub struct HostLink {
    host: CastHost,
    browser: Option<MdnsBrowser>,
}

impl HostLink {
    /// Starts the sender's worker with `memory`, and discovery. A browser
    /// that can't start leaves the sender running with nothing listed; the
    /// reason comes back for the log.
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
        metadata: Metadata,
        reader: AudioReader,
    ) -> Result<AttemptId, StartRefused> {
        self.host.start(target, rate, metadata, reader)
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

/// The session of the device that is on.
#[derive(Clone, Copy, Debug)]
struct Attempt {
    id: AttemptId,
    device: DeviceId,
    status: CastStatus,
}

/// The Cast side of the backend.
pub struct CastSpeakers {
    link: Option<Box<dyn CastLink>>,
    feed: CastFeed,
    list: DeviceList,
    /// The one device the user turned on.
    enabled: Option<DeviceId>,
    /// Each device's volume as its latest status reports it.
    volumes: BTreeMap<DeviceId, DeviceVolume>,
    /// A level the user set that no status has answered yet, 0.0 to 1.0.
    requested: Option<(DeviceId, f64)>,
    attempt: Option<Attempt>,
    streaming: bool,
    log: DebugLog,
    pending: Vec<BackendEvent>,
}

impl CastSpeakers {
    pub fn new(link: Box<dyn CastLink>, log: DebugLog) -> CastSpeakers {
        CastSpeakers {
            link: Some(link),
            feed: CastFeed::new(),
            list: DeviceList::new(),
            enabled: None,
            volumes: BTreeMap::new(),
            requested: None,
            attempt: None,
            streaming: false,
            log,
            pending: Vec::new(),
        }
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
        speakers.sort_by_key(|speaker| speaker.name.to_lowercase());
        speakers
    }

    fn info(&self, entry: &Entry, memory: &DeviceMemory) -> Option<CastInfo> {
        let DeviceKey::Id(id) = entry.key else {
            return None;
        };
        let device = entry.device.as_ref()?;
        let reported = self.volumes.get(&id);
        let level = match self.requested {
            Some((device, level)) if device == id => level,
            _ => reported.map_or(1.0, |volume| volume.level),
        };
        let status = match self.attempt {
            Some(attempt) if attempt.device == id => attempt.status,
            _ => CastStatus::Idle,
        };
        Some(CastInfo {
            id: id.to_string(),
            name: device
                .name
                .clone()
                .unwrap_or_else(|| device.instance.clone()),
            group: device.kind == DeviceKind::Group,
            enabled: self.enabled == Some(id),
            volume: (level * 100.0).clamp(0.0, 100.0),
            muted: reported.is_some_and(|volume| volume.muted),
            volume_fixed: reported.is_some_and(|volume| volume.control_type == ControlType::Fixed),
            offset_ms: offset_ms(memory, &id),
            status,
        })
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
        let frames = (ms * f64::from(SESSION_RATE) / 1000.0).round() as usize;
        if self.feed.set_hold_back(frames) {
            self.log
                .log(|| format!("Cast: the feed is held back {ms:.0} ms to meet L"));
        }
    }

    fn entry(&self, id: DeviceId) -> Option<&Entry> {
        self.list.get(DeviceKey::Id(id))
    }

    fn name(&self, id: DeviceId) -> String {
        self.entry(id)
            .and_then(|entry| entry.device.as_ref())
            .and_then(|device| device.name.clone())
            .unwrap_or_else(|| id.to_string())
    }

    /// Turns a device on or off. A second device is refused while one is
    /// on (C2), and a group always (C3).
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
        if self
            .entry(device)
            .and_then(|entry| entry.device.as_ref())
            .is_some_and(|found| found.kind == DeviceKind::Group)
        {
            return Err(CastProblem::Group);
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
        if self.attempt.is_some() || !self.streaming {
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
        let target = CastTarget {
            id,
            addresses: entry.addresses.clone(),
            group: None,
        };
        let Some(link) = self.link.as_mut() else {
            return;
        };
        let (writer, reader) = audio_ring(RING_FRAMES);
        match link.start(target, SESSION_RATE, Metadata::default(), reader) {
            Ok(attempt) => {
                if !self.feed.hand(Some(writer)) {
                    self.log
                        .log(|| "Cast: the capture's handoff is full; the new session's feed waits for the next poll".to_owned());
                }
                self.attempt = Some(Attempt {
                    id: attempt,
                    device: id,
                    status: CastStatus::Connecting,
                });
                self.list.set_in_session(DeviceKey::Id(id), true);
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
        if let Some(link) = self.link.as_mut() {
            link.stop();
        }
        self.feed.hand(None);
        self.list
            .set_in_session(DeviceKey::Id(attempt.device), false);
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

    /// The device's volume, 0 to 100, sent as it is (C69); shown until the
    /// device's next status answers it.
    pub fn set_volume(&mut self, id: &str, position: f64) {
        let Some(device) = self.playing_device(id) else {
            return;
        };
        if !position.is_finite() {
            return;
        }
        let level = (position / 100.0).clamp(0.0, 1.0);
        self.requested = Some((device, level));
        if let Some(link) = self.link.as_mut() {
            link.set_volume(level);
        }
    }

    pub fn set_muted(&mut self, id: &str, muted: bool) {
        if self.playing_device(id).is_some()
            && let Some(link) = self.link.as_mut()
        {
            link.set_muted(muted);
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
        while let Some(event) = self.link.as_mut().and_then(|link| link.poll_browse()) {
            match self.list.apply(event) {
                ListEvent::Skipped(_) => {}
                change => {
                    self.log.log(|| format!("Cast discovery: {change:?}"));
                    self.pending.push(BackendEvent::CastChanged);
                }
            }
        }
        while let Some(event) = self.link.as_mut().and_then(|link| link.poll_event()) {
            self.handle(event);
        }
        self.feed.flush();
        self.sync();
        dedup_changed(std::mem::take(&mut self.pending))
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
                self.feed.hand(None);
                self.list.set_in_session(DeviceKey::Id(device), false);
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
            SessionEvent::Volume(volume) => {
                if self
                    .requested
                    .is_some_and(|(requested, _)| requested == device)
                {
                    self.requested = None;
                }
                if self.volumes.get(&device) != Some(&volume) {
                    self.volumes.insert(device, volume);
                    self.pending.push(BackendEvent::CastChanged);
                }
            }
            SessionEvent::Diagnostics(diagnostics) => {
                self.log
                    .log(|| format!("Cast diagnostics: {diagnostics:?}"));
            }
            SessionEvent::FormatFallback { from, to, reason } => {
                self.log
                    .log(|| format!("Cast: {from:?} refused ({reason}), {to:?} instead"));
            }
            SessionEvent::Rebuffering => self.log.log(|| "Cast: the device ran dry".to_owned()),
            SessionEvent::Stream(event) => self.log.log(|| format!("Cast stream: {event:?}")),
            SessionEvent::GroupMessage(kind) => self.log.log(|| format!("Cast group: {kind}")),
            SessionEvent::Members(_) | SessionEvent::Ended(_) => {}
        }
    }

    /// Starts the sender's teardown on a thread of its own, so the app's
    /// other teardowns run meanwhile (C88); join it before the process
    /// ends.
    pub fn begin_shutdown(&mut self) -> Option<JoinHandle<bool>> {
        let mut link = self.link.take()?;
        self.attempt = None;
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
        SessionEnd::Failed(SessionError::Launch(_) | SessionError::Media(_)) => {
            Some(CastProblem::Refused)
        }
        SessionEnd::Failed(SessionError::Stream(_)) => Some(CastProblem::ConnectionLost),
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
    use std::net::{IpAddr, Ipv4Addr};

    use audiosync_cast::discovery::{DEVICE_PORT, ServiceRecord};
    use audiosync_cast::error::ConnectError;

    use super::*;

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
        pub resyncs: usize,
        pub shutdowns: usize,
        pub memory: DeviceMemory,
        pub refuse: Option<StartRefused>,
        pub readers: Vec<AudioReader>,
        pub events: VecDeque<HostEvent>,
        pub browse: VecDeque<BrowseEvent>,
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
            self.state()
                .browse
                .push_back(BrowseEvent::Resolved(ServiceRecord {
                    fullname: format!("{name}-{id}._googlecast._tcp.local."),
                    port,
                    txt: vec![
                        format!("id={id}").into_bytes(),
                        format!("fn={name}").into_bytes(),
                    ],
                    addresses: vec![IpAddr::V4(Ipv4Addr::new(192, 0, 2, 10))],
                }));
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
            _metadata: Metadata,
            reader: AudioReader,
        ) -> Result<AttemptId, StartRefused> {
            let mut state = self.state();
            if let Some(refused) = state.refuse.take() {
                return Err(refused);
            }
            state.started.push(target.id);
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

        fn discovery(&self) -> CastDiscovery {
            CastDiscovery::Browsing
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

    #[test]
    fn devices_are_listed_by_name_and_a_group_says_so() {
        let (_fake, cast) = listed();
        let speakers = cast.speakers();
        let names: Vec<&str> = speakers.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["Attic", "Kitchen", "Living Room"]);
        assert!(speakers[0].group);
        assert!(!speakers[1].group);
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
    fn a_second_speaker_or_a_group_is_refused() {
        let (fake, mut cast) = listed();
        cast.set_enabled(KITCHEN, true).unwrap();
        assert_eq!(cast.set_enabled(LIVING, true), Err(CastProblem::Busy));
        assert_eq!(cast.set_enabled(ATTIC, true), Err(CastProblem::Busy));
        cast.set_enabled(KITCHEN, false).unwrap();
        assert_eq!(cast.set_enabled(ATTIC, true), Err(CastProblem::Group));
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
}
