//! An in-memory driver, and the backend over it: for tests of the backend
//! logic and of the UI's adapter over it.
//!
//! [`MockBackend`] is the real [`Controller`] over a [`MockDriver`] that
//! keeps a pretend list of processes, audio sessions and outputs. Nothing
//! plays: a started output only records that it started. Methods on
//! [`MockBackend`] change the pretend world the way Windows would (an output
//! or input device is unplugged, an app quits, a device refuses to start),
//! and the next
//! [`AudioBackend::poll`](super::backend::AudioBackend::poll) reports it.
//!
//! For the double-audio check, each mock output has a mute, a volume and the
//! processes with an active session on it; an output AudioSync plays to has
//! an active session of AudioSync's own, as on Windows.
//!
//! The same mute and volume are each output's own volume in Windows, which
//! the backend follows for the outputs that are on (spec S192 to S202): the
//! mock records which outputs have a change listener and every write, and a
//! change made through [`MockBackend`] notifies a followed output, as Windows
//! does.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, SystemTime};

use audiosync_core::eq::EqProcessor;
use audiosync_core::reader::Reader;
use audiosync_core::render::RendererControl;
use audiosync_core::ring::{Ring, RingWriter};
use audiosync_core::volume::SharedGain;

use super::backend::{BackendError, OutputKind};
use super::capture::{CAPTURE_ATTEMPTS, CaptureAttempt, CaptureTarget, Feeds, LoopbackMode};
use super::controller::{
    CaptureHandle, CaptureState, Controller, Driver, PowerNotice, RendererHandle,
};
use super::outputs::{
    DeviceTraits, Endpoint, EndpointProperties, EndpointReading, Listing, RenderFormat,
    StreamSetup, list_endpoints,
};
use super::process::{ProcessEntry, ProcessTable};
use super::render::{Stall, StallSlot};
use super::routing::{EndpointAudio, EndpointVolume};

/// AudioSync's process ID in the mock.
pub const MOCK_OWN_PID: u32 = 1000;

/// How every mock output says its stream is set up: stereo float at 48 kHz,
/// with a 20 ms buffer.
pub const MOCK_STREAM: StreamSetup = StreamSetup {
    format: RenderFormat {
        rate: 48_000,
        channels: 2,
        channel_mask: 0x3,
        convert: false,
    },
    buffer_frames: 960,
};

/// The mock's Explorer, which started AudioSync and every mock app.
const EXPLORER_PID: u32 = 900;

/// `E_ACCESSDENIED`: what Windows answers when a process running as
/// administrator is opened for a wait.
const ACCESS_DENIED: i32 = 0x8007_0005_u32 as i32;

/// How a mock output starts: unmuted, at full volume.
const FULL_VOLUME: EndpointVolume = EndpointVolume {
    muted: false,
    level: 1.0,
};

/// A write to an output's own volume, as the mock driver records it.
#[derive(Clone, Debug, PartialEq)]
pub enum VolumeWrite {
    /// `SetMasterVolumeLevelScalar`, 0 to 1.
    Level { id: String, level: f32 },
    /// `SetMute(false)`.
    Unmute { id: String },
}

/// What the mock driver records of the outputs' own volumes: the outputs
/// with a change listener, and every write. Shared, so a test can still read
/// it once the backend is gone.
#[derive(Clone, Debug, Default)]
pub struct VolumeRecord(Arc<Mutex<Recorded>>);

#[derive(Debug, Default)]
struct Recorded {
    watched: BTreeSet<String>,
    writes: Vec<VolumeWrite>,
    /// The output of every read of an own volume, in order.
    reads: Vec<String>,
}

impl VolumeRecord {
    /// The outputs with a change listener now, sorted.
    pub fn watched(&self) -> Vec<String> {
        self.lock().watched.iter().cloned().collect()
    }

    /// Every write so far, in order.
    pub fn writes(&self) -> Vec<VolumeWrite> {
        self.lock().writes.clone()
    }

    /// The output of every read so far, in order.
    pub fn reads(&self) -> Vec<String> {
        self.lock().reads.clone()
    }

    fn lock(&self) -> MutexGuard<'_, Recorded> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// The outputs' own volumes, besides their mute and volume themselves.
#[derive(Default)]
struct DeviceVolumes {
    record: VolumeRecord,
    /// The followed outputs whose mute or volume changed since the backend
    /// last asked.
    changed: BTreeSet<String>,
    /// Writes work but change nothing, as on a device that ignores them.
    ignore_writes: bool,
    /// Writes fail with this error.
    write_error: Option<BackendError>,
    /// Calls on the outputs' own volumes (listener registrations, reads and
    /// writes) don't answer in time, as while Windows audio hangs.
    stall: bool,
    /// Notifications wait in `held` until a test lets them through, as
    /// Windows' come some time after the change.
    hold: bool,
    held: BTreeSet<String>,
}

/// What a mock capture shares with the driver.
struct CaptureLink {
    target: CaptureTarget,
    state: CaptureState,
    /// The ring writer, until the capture stops.
    writer: Option<RingWriter>,
    /// The processing half of the capture's equalizer.
    equalizer: EqProcessor,
    /// The network senders' feeds, until the capture stops.
    feeds: Feeds,
    /// A mix lacks the stream of one of its apps.
    missing: bool,
    /// How often a mix was moved onto new apps.
    retargets: u32,
    /// The format it captures in.
    setup: CaptureAttempt,
    /// The error code its app's process refused a wait with, as an app
    /// running as administrator does (S327).
    watch_refused: Option<i32>,
}

fn lock(link: &Mutex<CaptureLink>) -> std::sync::MutexGuard<'_, CaptureLink> {
    link.lock().unwrap_or_else(PoisonError::into_inner)
}

/// A mock capture.
pub struct MockCapture {
    link: Arc<Mutex<CaptureLink>>,
}

impl CaptureHandle for MockCapture {
    fn state(&self) -> CaptureState {
        lock(&self.link).state.clone()
    }

    /// The first way a real capture tries, unless a test chose another.
    fn setup(&self) -> Option<CaptureAttempt> {
        Some(lock(&self.link).setup)
    }

    /// A mix follows its apps, as a real one does.
    fn retarget(&mut self, target: &CaptureTarget) -> bool {
        let mut link = lock(&self.link);
        if !matches!(
            (&link.target, target),
            (CaptureTarget::Mix(_), CaptureTarget::Mix(_))
        ) {
            return false;
        }
        link.target = target.clone();
        link.retargets += 1;
        link.missing = false;
        true
    }

    fn holds_all(&self) -> bool {
        !lock(&self.link).missing
    }

    fn watch_refused(&self) -> Option<i32> {
        lock(&self.link).watch_refused
    }
}

impl Drop for MockCapture {
    fn drop(&mut self) {
        let mut link = lock(&self.link);
        link.writer = None;
        link.feeds = Feeds::default();
    }
}

/// A mock output. Its fade-out has played as soon as it was asked for,
/// unless the driver holds fades.
pub struct MockRenderer {
    ended: Arc<AtomicBool>,
    /// Set once the backend dropped it.
    dropped: Arc<AtomicBool>,
    faded: Arc<AtomicUsize>,
    /// While set, no fade-out has played yet.
    hold_fades: Arc<AtomicBool>,
    stop_requested: bool,
    stall: Arc<StallSlot>,
    /// How its stream was opened: in its device's format then.
    setup: StreamSetup,
    invalidated: Arc<AtomicBool>,
}

impl Drop for MockRenderer {
    fn drop(&mut self) {
        self.dropped.store(true, Ordering::Relaxed);
    }
}

/// What the driver keeps of each output started.
struct RendererLink {
    id: String,
    /// The ring its reader reads.
    ring: Arc<Ring>,
    ended: Arc<AtomicBool>,
    dropped: Arc<AtomicBool>,
    stall: Arc<StallSlot>,
    /// The format its stream was opened in.
    format: RenderFormat,
    /// Set when its audio session says it was disconnected.
    invalidated: Arc<AtomicBool>,
}

impl RendererLink {
    /// Whether it still plays: neither ended by itself nor dropped.
    fn plays(&self) -> bool {
        !self.ended.load(Ordering::Relaxed) && !self.dropped.load(Ordering::Relaxed)
    }
}

impl RendererHandle for MockRenderer {
    fn request_stop(&mut self) {
        if !self.stop_requested {
            self.stop_requested = true;
            self.faded.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn has_ended(&self) -> bool {
        self.ended.load(Ordering::Relaxed)
    }

    fn is_faded(&self) -> bool {
        self.has_ended() || (self.stop_requested && !self.hold_fades.load(Ordering::Relaxed))
    }

    fn setup(&self) -> Option<StreamSetup> {
        Some(self.setup)
    }

    fn stall(&self) -> Option<Stall> {
        self.stall.get()
    }

    fn invalidated(&self) -> bool {
        self.invalidated.load(Ordering::Relaxed)
    }
}

/// The pretend world of the mock.
pub struct MockDriver {
    processes: Vec<ProcessEntry>,
    session_pids: Vec<u32>,
    outputs: Vec<Endpoint>,
    /// The active capture endpoints, as Windows would describe them.
    inputs: Vec<EndpointReading>,
    inputs_error: Option<BackendError>,
    /// Endpoints as Windows would describe them, which go through the
    /// listing's own rules.
    readings: Vec<EndpointReading>,
    changed: bool,
    listing_error: Option<BackendError>,
    output_errors: BTreeMap<String, BackendError>,
    /// The mix formats of the outputs whose device isn't in
    /// [`MOCK_STREAM`]'s format.
    output_formats: BTreeMap<String, RenderFormat>,
    /// The outputs whose device format Windows said changed, not yet taken.
    format_notices: Vec<String>,
    capture_error: Option<BackendError>,
    /// The error only the next capture start fails with.
    next_capture_error: Option<BackendError>,
    /// The format the next captures start in.
    capture_setup: CaptureAttempt,
    /// The processes that can't be opened to wait for their exit.
    unwatchable: BTreeSet<u32>,
    /// The creation time of the process started last.
    last_created: u64,
    capture: Option<Arc<Mutex<CaptureLink>>>,
    /// Every output started that hasn't ended by itself.
    renderers: Vec<RendererLink>,
    /// The processes with an active audio session on each output, besides
    /// AudioSync's own, by endpoint ID.
    active_sessions: BTreeMap<String, Vec<u32>>,
    /// The mute and volume of the outputs set otherwise than unmuted at full
    /// volume; `None` when they can't be read.
    volumes: BTreeMap<String, Option<EndpointVolume>>,
    /// The listeners, notifications and writes of the outputs' own volumes.
    device_volumes: DeviceVolumes,
    endpoint_audio_error: Option<BackendError>,
    faded: Arc<AtomicUsize>,
    hold_fades: Arc<AtomicBool>,
    captures_started: Vec<CaptureTarget>,
    renderers_started: Vec<String>,
    /// Every capture start asked for, refused ones included.
    capture_attempts: usize,
    /// Windows' suspend and resume notices not yet taken.
    power_notices: Vec<PowerNotice>,
    /// The system clock: it stands still between polls unless a test moves
    /// it, as across a sleep.
    wall_clock: SystemTime,
    /// The endpoint ID of every output start asked for, refused ones
    /// included.
    renderer_attempts: Vec<String>,
}

impl Default for MockDriver {
    fn default() -> Self {
        MockDriver::new()
    }
}

impl MockDriver {
    /// No app and no output; AudioSync runs as [`MOCK_OWN_PID`], started by
    /// Explorer.
    pub fn new() -> MockDriver {
        MockDriver {
            processes: vec![
                ProcessEntry {
                    pid: EXPLORER_PID,
                    parent_pid: 4,
                    created: Some(1),
                    executable: "explorer.exe".to_owned(),
                },
                ProcessEntry {
                    pid: MOCK_OWN_PID,
                    parent_pid: EXPLORER_PID,
                    created: Some(2),
                    executable: "audiosync.exe".to_owned(),
                },
            ],
            session_pids: Vec::new(),
            outputs: Vec::new(),
            inputs: Vec::new(),
            inputs_error: None,
            readings: Vec::new(),
            changed: false,
            listing_error: None,
            output_errors: BTreeMap::new(),
            output_formats: BTreeMap::new(),
            format_notices: Vec::new(),
            capture_error: None,
            next_capture_error: None,
            capture_setup: CAPTURE_ATTEMPTS[0],
            unwatchable: BTreeSet::new(),
            last_created: 0,
            capture: None,
            renderers: Vec::new(),
            active_sessions: BTreeMap::new(),
            volumes: BTreeMap::new(),
            device_volumes: DeviceVolumes::default(),
            endpoint_audio_error: None,
            faded: Arc::new(AtomicUsize::new(0)),
            hold_fades: Arc::new(AtomicBool::new(false)),
            captures_started: Vec::new(),
            renderers_started: Vec::new(),
            capture_attempts: 0,
            renderer_attempts: Vec::new(),
            power_notices: Vec::new(),
            wall_clock: SystemTime::UNIX_EPOCH + Duration::from_secs(1_800_000_000),
        }
    }

    /// Every capture started, in order.
    pub fn captures_started(&self) -> &[CaptureTarget] {
        &self.captures_started
    }

    /// What the running capture takes in now, a mix's apps as it follows
    /// them.
    pub fn capture_target(&self) -> Option<CaptureTarget> {
        self.capture.as_ref().map(|link| lock(link).target.clone())
    }

    /// The running mix loses the stream of one of its apps, as when its
    /// loopback fails to open.
    pub fn lose_mix_stream(&self) {
        if let Some(link) = &self.capture {
            lock(link).missing = true;
        }
    }

    /// How often the running mix was moved onto new apps.
    pub fn capture_retargets(&self) -> u32 {
        self.capture.as_ref().map_or(0, |link| lock(link).retargets)
    }

    /// Whether the running capture feeds the AirPlay sender.
    pub fn capture_feeds_airplay(&self) -> bool {
        self.capture
            .as_ref()
            .is_some_and(|link| lock(link).feeds.airplay.is_some())
    }

    /// Whether the running capture feeds the Google Cast sender.
    pub fn capture_feeds_cast(&self) -> bool {
        self.capture
            .as_ref()
            .is_some_and(|link| lock(link).feeds.cast.is_some())
    }

    /// The endpoint ID of every output started, in order.
    pub fn renderers_started(&self) -> &[String] {
        &self.renderers_started
    }

    /// How many capture starts were asked for, refused ones included.
    pub fn capture_attempts(&self) -> usize {
        self.capture_attempts
    }

    /// How many starts of output `id` were asked for, refused ones included.
    pub fn renderer_attempts(&self, id: &str) -> usize {
        self.renderer_attempts
            .iter()
            .filter(|attempt| *attempt == id)
            .count()
    }

    /// How many outputs were asked to fade out and stop.
    pub fn renderers_faded(&self) -> usize {
        self.faded.load(Ordering::Relaxed)
    }

    /// While `hold` is set, the outputs asked to fade out haven't played
    /// their fades yet, so the backend keeps them running.
    pub fn hold_fades(&self, hold: bool) {
        self.hold_fades.store(hold, Ordering::Relaxed);
    }

    /// How many outputs started are still running: neither ended by
    /// themselves nor dropped by the backend.
    pub fn renderers_running(&self) -> usize {
        self.renderers
            .iter()
            .filter(|renderer| renderer.plays())
            .count()
    }

    /// The ring the running capture writes.
    pub fn capture_ring(&self) -> Option<Arc<Ring>> {
        self.capture.as_ref().and_then(|link| {
            lock(link)
                .writer
                .as_ref()
                .map(|writer| Arc::clone(writer.ring()))
        })
    }

    /// The endpoint ID and the ring read of every output still running, in
    /// start order.
    pub fn renderer_rings(&self) -> Vec<(String, Arc<Ring>)> {
        self.renderers
            .iter()
            .filter(|renderer| renderer.plays())
            .map(|renderer| (renderer.id.clone(), Arc::clone(&renderer.ring)))
            .collect()
    }

    /// Whether the last capture started still runs.
    pub fn capture_running(&self) -> bool {
        self.capture
            .as_ref()
            .is_some_and(|link| lock(link).writer.is_some())
    }

    fn end_renderers_of(&mut self, id: &str) {
        for renderer in &self.renderers {
            if renderer.id == id {
                renderer.ended.store(true, Ordering::Relaxed);
            }
        }
    }

    /// The mute and volume of output `id`.
    fn volume_of(&self, id: &str) -> Option<EndpointVolume> {
        self.volumes.get(id).copied().unwrap_or(Some(FULL_VOLUME))
    }

    /// The mix format of output `id`'s device.
    fn device_format(&self, id: &str) -> RenderFormat {
        self.output_formats
            .get(id)
            .copied()
            .unwrap_or(MOCK_STREAM.format)
    }

    fn is_connected(&self, id: &str) -> bool {
        self.outputs.iter().any(|output| output.id == id)
    }

    /// Output `id`'s mute or volume changed: Windows notifies its listener,
    /// now or once a test lets held notifications through.
    fn notify_volume(&mut self, id: &str) {
        if self.device_volumes.record.lock().watched.contains(id) {
            let volumes = &mut self.device_volumes;
            let queue = if volumes.hold {
                &mut volumes.held
            } else {
                &mut volumes.changed
            };
            queue.insert(id.to_owned());
        }
    }

    /// Records a write to output `id`'s own volume, and makes it unless the
    /// writes fail or are ignored.
    fn write_volume(
        &mut self,
        write: VolumeWrite,
        change: impl FnOnce(Option<EndpointVolume>) -> EndpointVolume,
    ) -> Result<(), BackendError> {
        let id = match &write {
            VolumeWrite::Level { id, .. } | VolumeWrite::Unmute { id } => id.clone(),
        };
        self.device_volumes.record.lock().writes.push(write);
        if self.device_volumes.stall {
            return Err(BackendError::NotResponding);
        }
        if let Some(error) = &self.device_volumes.write_error {
            return Err(error.clone());
        }
        if !self.is_connected(&id) {
            return Err(BackendError::DeviceGone);
        }
        if !self.device_volumes.ignore_writes {
            let volume = change(self.volume_of(&id));
            self.volumes.insert(id.clone(), Some(volume));
            self.notify_volume(&id);
        }
        Ok(())
    }
}

impl Driver for MockDriver {
    type Capture = MockCapture;
    type Renderer = MockRenderer;

    fn own_pid(&self) -> u32 {
        MOCK_OWN_PID
    }

    fn processes(&mut self) -> Result<ProcessTable, BackendError> {
        Ok(ProcessTable::new(self.processes.iter().cloned()))
    }

    fn session_pids(&mut self) -> Result<Vec<u32>, BackendError> {
        Ok(self.session_pids.clone())
    }

    fn outputs(&mut self) -> Result<Listing, BackendError> {
        if let Some(error) = &self.listing_error {
            return Err(error.clone());
        }
        let mut listing = list_endpoints(self.readings.clone(), None);
        listing.endpoints.extend(self.outputs.iter().cloned());
        Ok(listing)
    }

    fn outputs_changed(&mut self) -> bool {
        std::mem::take(&mut self.changed)
    }

    fn format_notices(&mut self) -> Vec<String> {
        std::mem::take(&mut self.format_notices)
    }

    fn power_notices(&mut self) -> Vec<PowerNotice> {
        std::mem::take(&mut self.power_notices)
    }

    fn wall_clock(&self) -> SystemTime {
        self.wall_clock
    }

    fn output_format(&mut self, id: &str) -> Result<RenderFormat, BackendError> {
        if !self.is_connected(id) {
            return Err(BackendError::DeviceGone);
        }
        Ok(self.device_format(id))
    }

    fn inputs(&mut self) -> Result<Vec<EndpointReading>, BackendError> {
        if let Some(error) = &self.inputs_error {
            return Err(error.clone());
        }
        Ok(self.inputs.clone())
    }

    fn endpoint_audio(&mut self) -> Result<Vec<EndpointAudio>, BackendError> {
        if let Some(error) = &self.endpoint_audio_error {
            return Err(error.clone());
        }
        Ok(self
            .outputs
            .iter()
            .map(|output| {
                let mut active_pids = self
                    .active_sessions
                    .get(&output.id)
                    .cloned()
                    .unwrap_or_default();
                if self
                    .renderers
                    .iter()
                    .any(|renderer| renderer.id == output.id && renderer.plays())
                {
                    active_pids.push(MOCK_OWN_PID);
                }
                EndpointAudio {
                    id: output.id.clone(),
                    name: output.name.clone(),
                    is_default: output.is_default,
                    virtual_device: output.kind == OutputKind::Virtual,
                    volume: self.volume_of(&output.id),
                    active_pids,
                }
            })
            .collect())
    }

    fn watch_endpoint_volume(&mut self, id: &str) -> Result<(), BackendError> {
        if !self.is_connected(id) {
            return Err(BackendError::DeviceGone);
        }
        if self.device_volumes.stall {
            return Err(BackendError::NotResponding);
        }
        self.device_volumes
            .record
            .lock()
            .watched
            .insert(id.to_owned());
        Ok(())
    }

    fn unwatch_endpoint_volume(&mut self, id: &str) {
        self.device_volumes.record.lock().watched.remove(id);
        self.device_volumes.changed.remove(id);
        self.device_volumes.held.remove(id);
    }

    fn endpoint_volume_changes(&mut self) -> Vec<String> {
        std::mem::take(&mut self.device_volumes.changed)
            .into_iter()
            .collect()
    }

    fn endpoint_volume(&mut self, id: &str) -> Result<EndpointVolume, BackendError> {
        self.device_volumes.record.lock().reads.push(id.to_owned());
        if self.device_volumes.stall {
            return Err(BackendError::NotResponding);
        }
        if !self.is_connected(id) {
            return Err(BackendError::DeviceGone);
        }
        self.volume_of(id).ok_or(BackendError::System {
            call: "IAudioEndpointVolume::GetMute",
            code: 0x8000_4005_u32 as i32,
        })
    }

    fn set_endpoint_level(&mut self, id: &str, level: f32) -> Result<(), BackendError> {
        let write = VolumeWrite::Level {
            id: id.to_owned(),
            level,
        };
        self.write_volume(write, |volume| EndpointVolume {
            muted: volume.is_some_and(|volume| volume.muted),
            level,
        })
    }

    fn unmute_endpoint(&mut self, id: &str) -> Result<(), BackendError> {
        let write = VolumeWrite::Unmute { id: id.to_owned() };
        self.write_volume(write, |volume| EndpointVolume {
            muted: false,
            level: volume.map_or(1.0, |volume| volume.level),
        })
    }

    fn start_capture(
        &mut self,
        target: CaptureTarget,
        writer: RingWriter,
        equalizer: EqProcessor,
        feeds: Feeds,
    ) -> Result<MockCapture, BackendError> {
        self.capture_attempts += 1;
        if let Some(error) = self.next_capture_error.take() {
            return Err(error);
        }
        if let Some(error) = &self.capture_error {
            return Err(error.clone());
        }
        if let CaptureTarget::Input { endpoint_id, .. } = &target
            && !self
                .inputs
                .iter()
                .any(|input| input.id.as_deref() == Ok(endpoint_id.as_str()))
        {
            return Err(BackendError::InputGone);
        }
        self.captures_started.push(target.clone());
        let watch_refused = match &target {
            CaptureTarget::Loopback(activation)
                if activation.mode == LoopbackMode::IncludeTree
                    && self.unwatchable.contains(&activation.target_pid) =>
            {
                Some(ACCESS_DENIED)
            }
            _ => None,
        };
        let link = Arc::new(Mutex::new(CaptureLink {
            target,
            state: CaptureState::Running,
            writer: Some(writer),
            equalizer,
            feeds,
            missing: false,
            retargets: 0,
            setup: self.capture_setup,
            watch_refused,
        }));
        self.capture = Some(Arc::clone(&link));
        Ok(MockCapture { link })
    }

    fn start_renderer(
        &mut self,
        endpoint: &Endpoint,
        reader: Reader,
        _control: RendererControl,
        _gain: Arc<SharedGain>,
    ) -> Result<MockRenderer, BackendError> {
        self.renderer_attempts.push(endpoint.id.clone());
        if !self.outputs.iter().any(|output| output.id == endpoint.id) {
            return Err(BackendError::DeviceGone);
        }
        if let Some(error) = self.output_errors.get(&endpoint.id) {
            return Err(error.clone());
        }
        self.renderers
            .retain(|renderer| !renderer.ended.load(Ordering::Relaxed));
        let ended = Arc::new(AtomicBool::new(false));
        let dropped = Arc::new(AtomicBool::new(false));
        let stall = Arc::new(StallSlot::default());
        let invalidated = Arc::new(AtomicBool::new(false));
        let format = self.device_format(&endpoint.id);
        self.renderers.push(RendererLink {
            id: endpoint.id.clone(),
            ring: Arc::clone(reader.ring()),
            ended: Arc::clone(&ended),
            dropped: Arc::clone(&dropped),
            stall: Arc::clone(&stall),
            format,
            invalidated: Arc::clone(&invalidated),
        });
        self.renderers_started.push(endpoint.id.clone());
        Ok(MockRenderer {
            ended,
            dropped,
            faded: Arc::clone(&self.faded),
            hold_fades: Arc::clone(&self.hold_fades),
            stop_requested: false,
            stall,
            setup: StreamSetup {
                format,
                ..MOCK_STREAM
            },
            invalidated,
        })
    }
}

/// The backend over the mock driver.
pub type MockBackend = Controller<MockDriver>;

impl Controller<MockDriver> {
    /// A mock with no app and no output.
    pub fn empty() -> MockBackend {
        Controller::with_driver(MockDriver::new())
    }

    /// A mock with a few apps and outputs. Every output is off; the speakers
    /// are the default output.
    pub fn demo() -> MockBackend {
        let mut backend = MockBackend::empty();
        backend.add_app(2000, "music.exe");
        backend.add_app(3000, "browser.exe");
        backend.add_helper(3001, 3000, "browser.exe");
        backend.add_app(4000, "video.exe");
        backend.connect_output_as("speakers", "Speakers", OutputKind::BuiltIn);
        backend.connect_output_as("headphones", "Headphones", OutputKind::Headphones);
        backend.connect_output_as("tv", "TV (HDMI)", OutputKind::Hdmi);
        backend.connect_output_as("bluetooth", "Bluetooth Speaker", OutputKind::Bluetooth);
        backend.set_default_output("speakers");
        backend
    }

    /// Starts an app with an audio session, started by Explorer.
    pub fn add_app(&mut self, pid: u32, executable: &str) {
        self.add_process(pid, EXPLORER_PID, executable, true);
    }

    /// Starts a helper process of `parent_pid` that holds the app's audio
    /// session (as a browser's audio process does).
    pub fn add_helper(&mut self, pid: u32, parent_pid: u32, executable: &str) {
        self.add_process(pid, parent_pid, executable, true);
    }

    /// Starts an app, started by Explorer, that hasn't opened an audio
    /// stream yet.
    pub fn add_silent_app(&mut self, pid: u32, executable: &str) {
        self.add_process(pid, EXPLORER_PID, executable, false);
    }

    /// Ends the audio session of process `pid`, which keeps running (as an
    /// app may do while it is paused).
    pub fn close_audio_session(&mut self, pid: u32) {
        self.driver_mut()
            .session_pids
            .retain(|&session_pid| session_pid != pid);
    }

    fn add_process(&mut self, pid: u32, parent_pid: u32, executable: &str, audio: bool) {
        let driver = self.driver_mut();
        // A process started after one that quit is still a later one.
        let created = driver
            .processes
            .iter()
            .filter_map(|process| process.created)
            .chain([driver.last_created])
            .max()
            .unwrap_or(0)
            + 1;
        driver.last_created = created;
        driver.processes.retain(|process| process.pid != pid);
        driver.processes.push(ProcessEntry {
            pid,
            parent_pid,
            created: Some(created),
            executable: executable.to_owned(),
        });
        if audio && !driver.session_pids.contains(&pid) {
            driver.session_pids.push(pid);
        }
    }

    /// Makes process `pid` one AudioSync can't open to wait for its exit,
    /// as an app running as administrator: a capture of its tree then
    /// doesn't notice when it quits.
    pub fn refuse_watch(&mut self, pid: u32) {
        self.driver_mut().unwatchable.insert(pid);
    }

    /// Makes the creation time of process `pid` one that can't be read.
    pub fn hide_start_time(&mut self, pid: u32) {
        for process in &mut self.driver_mut().processes {
            if process.pid == pid {
                process.created = None;
            }
        }
    }

    /// Quits a process. A capture of its tree reports that its source quit,
    /// unless it couldn't watch it.
    pub fn quit_app(&mut self, pid: u32) {
        let driver = self.driver_mut();
        driver.processes.retain(|process| process.pid != pid);
        driver
            .session_pids
            .retain(|&session_pid| session_pid != pid);
        if let Some(link) = &driver.capture {
            let mut link = lock(link);
            let processes = &driver.processes;
            let captures_app = match &link.target {
                CaptureTarget::Loopback(activation) => {
                    activation.mode == LoopbackMode::IncludeTree && activation.target_pid == pid
                }
                // A mix reports a quit once all of its apps quit.
                CaptureTarget::Mix(activations) => {
                    !activations.is_empty()
                        && activations.iter().all(|activation| {
                            !processes
                                .iter()
                                .any(|process| process.pid == activation.target_pid)
                        })
                }
                CaptureTarget::Input { .. } => false,
            };
            if captures_app && link.watch_refused.is_none() && link.state == CaptureState::Running {
                link.state = CaptureState::SourceQuit;
            }
        }
    }

    /// Connects an input device, or renames a connected one. `enumerator`
    /// is its device's enumerator name, as Windows reports it.
    pub fn connect_input(&mut self, id: &str, name: &str, enumerator: &str) {
        let driver = self.driver_mut();
        driver.inputs.retain(|input| input.id.as_deref() != Ok(id));
        let index = driver.inputs.len() as u32;
        driver.inputs.push(EndpointReading {
            index,
            id: Ok(id.to_owned()),
            properties: Ok(EndpointProperties {
                name: Some(name.to_owned()),
                enumerator: Some(enumerator.to_owned()),
                ..EndpointProperties::default()
            }),
        });
        driver.changed = true;
    }

    /// Gives a connected input device `channels` channels.
    pub fn set_input_channels(&mut self, id: &str, channels: u16) {
        let driver = self.driver_mut();
        for input in &mut driver.inputs {
            if input.id.as_deref() == Ok(id)
                && let Ok(properties) = &mut input.properties
            {
                properties.channels = Some(channels);
            }
        }
        driver.changed = true;
    }

    /// Makes listing the input devices fail with `error`, or work again.
    pub fn fail_input_listing(&mut self, error: Option<BackendError>) {
        self.driver_mut().inputs_error = error;
    }

    /// Unplugs an input device. A capture from it fails as Windows fails it,
    /// with its stream invalidated.
    pub fn disconnect_input(&mut self, id: &str) {
        let driver = self.driver_mut();
        driver.inputs.retain(|input| input.id.as_deref() != Ok(id));
        driver.changed = true;
        if let Some(link) = &driver.capture {
            let mut link = lock(link);
            let captures_it = matches!(
                &link.target,
                CaptureTarget::Input { endpoint_id, .. } if endpoint_id == id
            );
            if captures_it && link.state == CaptureState::Running {
                link.state = CaptureState::Failed(BackendError::DeviceGone);
            }
        }
    }

    /// Connects an output of an unknown kind, or renames a connected one.
    pub fn connect_output(&mut self, id: &str, name: &str) {
        let driver = self.driver_mut();
        match driver.outputs.iter_mut().find(|output| output.id == id) {
            Some(output) => output.name = name.to_owned(),
            None => driver.outputs.push(Endpoint {
                id: id.to_owned(),
                name: name.to_owned(),
                is_default: false,
                kind: OutputKind::Other,
                traits: DeviceTraits::default(),
            }),
        }
        driver.changed = true;
    }

    /// Connects an output of this kind, or changes a connected one.
    pub fn connect_output_as(&mut self, id: &str, name: &str, kind: OutputKind) {
        self.connect_output(id, name);
        let driver = self.driver_mut();
        if let Some(output) = driver.outputs.iter_mut().find(|output| output.id == id) {
            output.kind = kind;
        }
    }

    /// Sets what Windows says about a connected output's device.
    pub fn set_output_traits(&mut self, id: &str, traits: DeviceTraits) {
        let driver = self.driver_mut();
        if let Some(output) = driver.outputs.iter_mut().find(|output| output.id == id) {
            output.traits = traits;
        }
        driver.changed = true;
    }

    /// Disconnects an output; an output playing on it stops by itself.
    pub fn disconnect_output(&mut self, id: &str) {
        self.disconnect_output_unnoticed(id);
        self.driver_mut().end_renderers_of(id);
    }

    /// Disconnects an output without telling the output playing on it, which
    /// plays on until the backend stops it, as a Windows stream whose device
    /// went away may.
    pub fn disconnect_output_unnoticed(&mut self, id: &str) {
        let driver = self.driver_mut();
        driver.outputs.retain(|output| output.id != id);
        driver.changed = true;
        // Its volume control goes with it, and its listener is never called
        // again, even once the device is back.
        let volumes = &mut driver.device_volumes;
        volumes.record.lock().watched.remove(id);
        volumes.changed.remove(id);
        volumes.held.remove(id);
    }

    /// Adds an endpoint as Windows would describe it, which the listing
    /// lists or leaves out by its own rules.
    pub fn enumerate_endpoint(&mut self, reading: EndpointReading) {
        let driver = self.driver_mut();
        driver.readings.push(reading);
        driver.changed = true;
    }

    /// Makes every later listing of the outputs fail with `error`, or work
    /// again with `None`.
    pub fn fail_listing(&mut self, error: Option<BackendError>) {
        self.driver_mut().listing_error = error;
    }

    /// Makes `id` Windows' default output.
    pub fn set_default_output(&mut self, id: &str) {
        let driver = self.driver_mut();
        for output in &mut driver.outputs {
            output.is_default = output.id == id;
        }
        driver.changed = true;
    }

    /// Makes every later start of output `id` fail with `error`, or succeed
    /// again with `None`.
    pub fn fail_output(&mut self, id: &str, error: Option<BackendError>) {
        let errors = &mut self.driver_mut().output_errors;
        match error {
            Some(error) => errors.insert(id.to_owned(), error),
            None => errors.remove(id),
        };
    }

    /// Stops the output playing on `id` as if its stream failed, while the
    /// device stays connected (as after a change of its format).
    pub fn end_renderer(&mut self, id: &str) {
        self.driver_mut().end_renderers_of(id);
    }

    /// Ends the output playing on `id` as a render thread ends a stream
    /// that stopped taking frames (S326), while the device stays listed.
    pub fn stall_renderer(&mut self, id: &str, stall: Stall) {
        for renderer in &self.driver().renderers {
            if renderer.id == id && renderer.plays() {
                renderer.stall.record(stall);
                renderer.ended.store(true, Ordering::Relaxed);
            }
        }
    }

    /// Changes the mix format of output `id`'s device, and Windows says so
    /// (S330).
    pub fn change_output_format(&mut self, id: &str, format: RenderFormat) {
        let driver = self.driver_mut();
        driver.output_formats.insert(id.to_owned(), format);
        driver.format_notices.push(id.to_owned());
    }

    /// Windows says output `id`'s device format changed while it didn't.
    pub fn notify_output_format(&mut self, id: &str) {
        self.driver_mut().format_notices.push(id.to_owned());
    }

    /// The audio session of the output playing on `id` says it was
    /// disconnected, before its render thread notices (S330).
    pub fn disconnect_session(&mut self, id: &str) {
        for renderer in &self.driver().renderers {
            if renderer.id == id && renderer.plays() {
                renderer.invalidated.store(true, Ordering::Relaxed);
            }
        }
    }

    /// The format the output playing on `id` opened its stream in.
    pub fn playing_format(&self, id: &str) -> Option<RenderFormat> {
        self.driver()
            .renderers
            .iter()
            .rev()
            .find(|renderer| renderer.id == id && renderer.plays())
            .map(|renderer| renderer.format)
    }

    /// Makes the captures started from now on deliver `setup`'s format.
    pub fn set_capture_setup(&mut self, setup: CaptureAttempt) {
        self.driver_mut().capture_setup = setup;
    }

    /// Makes every later capture start fail with `error`, or succeed again
    /// with `None`.
    pub fn refuse_captures(&mut self, error: Option<BackendError>) {
        self.driver_mut().capture_error = error;
    }

    /// Makes only the next capture start fail with `error`.
    pub fn refuse_next_capture(&mut self, error: BackendError) {
        self.driver_mut().next_capture_error = Some(error);
    }

    /// Windows tells of a suspend or a resume, as the next poll finds out.
    pub fn notify_power(&mut self, notice: PowerNotice) {
        self.driver_mut().power_notices.push(notice);
    }

    /// The system clock moves on by `span` before the next poll, as when the
    /// PC sleeps that long.
    pub fn pass_wall_time(&mut self, span: Duration) {
        self.driver_mut().wall_clock += span;
    }

    /// Makes the running capture fail with `error`.
    pub fn fail_capture(&mut self, error: BackendError) {
        if let Some(link) = &self.driver().capture {
            let mut link = lock(link);
            if link.writer.is_some() {
                link.state = CaptureState::Failed(error);
            }
        }
    }

    /// Gives process `pid` an active audio session on output `id`, as when
    /// it plays there.
    pub fn play_on(&mut self, pid: u32, id: &str) {
        let pids = self
            .driver_mut()
            .active_sessions
            .entry(id.to_owned())
            .or_default();
        if !pids.contains(&pid) {
            pids.push(pid);
        }
    }

    /// Ends the active session of process `pid` on output `id`, as when it
    /// pauses: its session there turns inactive.
    pub fn pause_on(&mut self, pid: u32, id: &str) {
        if let Some(pids) = self.driver_mut().active_sessions.get_mut(id) {
            pids.retain(|&active| active != pid);
        }
    }

    /// Moves the active session of process `pid` from output `from` to
    /// output `to`, as the volume mixer's per-app output does.
    pub fn move_session(&mut self, pid: u32, from: &str, to: &str) {
        self.pause_on(pid, from);
        self.play_on(pid, to);
    }

    /// Mutes or unmutes output `id`, as Windows' Sound settings do: a
    /// followed output is notified.
    pub fn mute_output(&mut self, id: &str, muted: bool) {
        let driver = self.driver_mut();
        let level = driver.volume_of(id).map_or(1.0, |volume| volume.level);
        driver
            .volumes
            .insert(id.to_owned(), Some(EndpointVolume { muted, level }));
        driver.notify_volume(id);
    }

    /// Sets the master volume of output `id`, 0 to 1, as Windows' Sound
    /// settings do: a followed output is notified.
    pub fn set_output_level(&mut self, id: &str, level: f32) {
        let driver = self.driver_mut();
        let muted = driver.volume_of(id).is_some_and(|volume| volume.muted);
        driver
            .volumes
            .insert(id.to_owned(), Some(EndpointVolume { muted, level }));
        driver.notify_volume(id);
    }

    /// Notifies output `id`'s volume listener without changing anything, as
    /// Windows does for a change AudioSync doesn't show (a channel's volume).
    pub fn notify_volume(&mut self, id: &str) {
        self.driver_mut().notify_volume(id);
    }

    /// Makes later writes to the outputs' own volumes work without changing
    /// anything (`true`), as on a device that ignores them, or work again.
    pub fn ignore_volume_writes(&mut self, ignore: bool) {
        self.driver_mut().device_volumes.ignore_writes = ignore;
    }

    /// Holds the volume notifications back (`true`) until this is called
    /// with `false`, which lets them through, as Windows sends them some
    /// time after a change.
    pub fn hold_volume_notifications(&mut self, hold: bool) {
        let volumes = &mut self.driver_mut().device_volumes;
        volumes.hold = hold;
        if !hold {
            let held = std::mem::take(&mut volumes.held);
            volumes.changed.extend(held);
        }
    }

    /// Makes later calls on the outputs' own volumes (listener
    /// registrations, reads and writes) not answer in time (`true`), as
    /// while Windows audio hangs, or work again.
    pub fn stall_device_volumes(&mut self, stall: bool) {
        self.driver_mut().device_volumes.stall = stall;
    }

    /// Makes later writes to the outputs' own volumes fail with `error`, or
    /// work again with `None`.
    pub fn refuse_volume_writes(&mut self, error: Option<BackendError>) {
        self.driver_mut().device_volumes.write_error = error;
    }

    /// The outputs with a volume listener and the writes to their volumes,
    /// readable after the backend is gone.
    pub fn volume_record(&self) -> VolumeRecord {
        self.driver().device_volumes.record.clone()
    }

    /// Makes the mute and volume of output `id` unreadable.
    pub fn hide_output_volume(&mut self, id: &str) {
        self.driver_mut().volumes.insert(id.to_owned(), None);
    }

    /// Makes every later double-audio snapshot fail with `error`, or work
    /// again with `None`.
    pub fn fail_endpoint_audio(&mut self, error: Option<BackendError>) {
        self.driver_mut().endpoint_audio_error = error;
    }

    /// Writes `frames` frames of silence to the running capture's ring, as if
    /// the source played.
    pub fn feed_capture(&mut self, frames: usize) {
        self.feed_capture_level(frames, 0.0);
    }

    /// Writes `frames` stereo frames whose every sample is `level` to the
    /// running capture's ring, through its equalizer as a real capture does,
    /// and returns what was written; nothing without a running capture.
    pub fn feed_capture_level(&mut self, frames: usize, level: f32) -> Vec<f32> {
        let Some(link) = &self.driver().capture else {
            return Vec::new();
        };
        let mut link = lock(link);
        let CaptureLink {
            writer, equalizer, ..
        } = &mut *link;
        let Some(writer) = writer.as_mut() else {
            return Vec::new();
        };
        let mut block = vec![level; frames * 2];
        equalizer.process(&mut block);
        writer.write(&block, frames);
        block
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::backend::{AudioBackend, SourceId};

    #[test]
    fn the_demo_offers_apps_and_outputs() {
        let mut backend = MockBackend::demo();
        let sources = backend.list_sources().unwrap();
        let names: Vec<&str> = sources.iter().map(|source| source.name.as_str()).collect();
        assert_eq!(names, ["All System Audio", "Browser", "Music", "Video"]);
        let browser = &sources[1];
        assert_eq!(browser.id, SourceId::App { root_pid: 3000 });
        let outputs = backend.list_outputs().unwrap();
        let names: Vec<&str> = outputs.iter().map(|output| output.name.as_str()).collect();
        assert_eq!(
            names,
            ["Bluetooth Speaker", "Headphones", "Speakers", "TV (HDMI)"]
        );
        assert!(
            outputs
                .iter()
                .all(|output| !output.enabled && !output.playing)
        );
        assert_eq!(outputs.iter().filter(|output| output.is_default).count(), 1);
    }

    #[test]
    fn the_demo_plays_what_is_turned_on() {
        let mut backend = MockBackend::demo();
        backend.set_output_enabled("tv", true).unwrap();
        backend.start(&SourceId::App { root_pid: 3000 }).unwrap();
        assert_eq!(backend.playing_ids(), ["tv"]);
        backend.stop();
        assert!(backend.playing_ids().is_empty());
    }

    #[test]
    fn a_renamed_output_is_announced() {
        let mut backend = MockBackend::demo();
        backend.list_outputs().unwrap();
        backend.poll();
        backend.connect_output("tv", "Living Room TV");
        assert_eq!(
            backend.poll(),
            [crate::audio::backend::BackendEvent::OutputsChanged]
        );
        let outputs = backend.list_outputs().unwrap();
        assert!(outputs.iter().any(|output| output.name == "Living Room TV"));
    }
}
