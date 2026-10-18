//! An in-memory driver, and the backend over it: for tests of the backend
//! logic and of the UI's adapter over it.
//!
//! [`MockBackend`] is the real [`Controller`] over a [`MockDriver`] that
//! keeps a pretend list of processes, audio sessions and outputs. Nothing
//! plays: a started output only records that it started. Methods on
//! [`MockBackend`] change the pretend world the way Windows would (an output
//! is unplugged, an app quits, a device refuses to start), and the next
//! [`AudioBackend::poll`](super::backend::AudioBackend::poll) reports it.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use audiosync_core::eq::EqProcessor;
use audiosync_core::reader::Reader;
use audiosync_core::ring::RingWriter;
use audiosync_core::volume::SharedGain;

use super::backend::{BackendError, OutputKind};
use super::capture::{Activation, CAPTURE_ATTEMPTS, CaptureAttempt, LoopbackMode};
use super::controller::{CaptureHandle, CaptureState, Controller, Driver, RendererHandle};
use super::outputs::{Endpoint, RenderFormat, StreamSetup};
use super::process::{ProcessEntry, ProcessTable};

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

/// What a mock capture shares with the driver.
struct CaptureLink {
    activation: Activation,
    state: CaptureState,
    /// The ring writer, until the capture stops.
    writer: Option<RingWriter>,
    /// The processing half of the capture's equalizer.
    equalizer: EqProcessor,
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

    /// The first way a real capture tries.
    fn setup(&self) -> Option<CaptureAttempt> {
        Some(CAPTURE_ATTEMPTS[0])
    }
}

impl Drop for MockCapture {
    fn drop(&mut self) {
        lock(&self.link).writer = None;
    }
}

/// A mock output.
pub struct MockRenderer {
    ended: Arc<AtomicBool>,
    faded: Arc<AtomicUsize>,
    stop_requested: bool,
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

    fn setup(&self) -> Option<StreamSetup> {
        Some(MOCK_STREAM)
    }
}

/// The pretend world of the mock.
pub struct MockDriver {
    processes: Vec<ProcessEntry>,
    session_pids: Vec<u32>,
    outputs: Vec<Endpoint>,
    changed: bool,
    listing_error: Option<BackendError>,
    output_errors: BTreeMap<String, BackendError>,
    capture_error: Option<BackendError>,
    capture: Option<Arc<Mutex<CaptureLink>>>,
    /// The ended flag of every output started, by endpoint ID.
    renderers: Vec<(String, Arc<AtomicBool>)>,
    faded: Arc<AtomicUsize>,
    captures_started: Vec<Activation>,
    renderers_started: Vec<String>,
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
            changed: false,
            listing_error: None,
            output_errors: BTreeMap::new(),
            capture_error: None,
            capture: None,
            renderers: Vec::new(),
            faded: Arc::new(AtomicUsize::new(0)),
            captures_started: Vec::new(),
            renderers_started: Vec::new(),
        }
    }

    /// Every capture started, in order.
    pub fn captures_started(&self) -> &[Activation] {
        &self.captures_started
    }

    /// The endpoint ID of every output started, in order.
    pub fn renderers_started(&self) -> &[String] {
        &self.renderers_started
    }

    /// How many outputs were asked to fade out and stop.
    pub fn renderers_faded(&self) -> usize {
        self.faded.load(Ordering::Relaxed)
    }

    fn end_renderers_of(&mut self, id: &str) {
        for (renderer_id, ended) in &self.renderers {
            if renderer_id == id {
                ended.store(true, Ordering::Relaxed);
            }
        }
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

    fn outputs(&mut self) -> Result<Vec<Endpoint>, BackendError> {
        match &self.listing_error {
            Some(error) => Err(error.clone()),
            None => Ok(self.outputs.clone()),
        }
    }

    fn outputs_changed(&mut self) -> bool {
        std::mem::take(&mut self.changed)
    }

    fn start_capture(
        &mut self,
        activation: Activation,
        writer: RingWriter,
        equalizer: EqProcessor,
    ) -> Result<MockCapture, BackendError> {
        if let Some(error) = &self.capture_error {
            return Err(error.clone());
        }
        self.captures_started.push(activation);
        let link = Arc::new(Mutex::new(CaptureLink {
            activation,
            state: CaptureState::Running,
            writer: Some(writer),
            equalizer,
        }));
        self.capture = Some(Arc::clone(&link));
        Ok(MockCapture { link })
    }

    fn start_renderer(
        &mut self,
        endpoint: &Endpoint,
        _reader: Reader,
        _gain: Arc<SharedGain>,
    ) -> Result<MockRenderer, BackendError> {
        if !self.outputs.iter().any(|output| output.id == endpoint.id) {
            return Err(BackendError::DeviceGone);
        }
        if let Some(error) = self.output_errors.get(&endpoint.id) {
            return Err(error.clone());
        }
        self.renderers
            .retain(|(_, stopped)| !stopped.load(Ordering::Relaxed));
        let ended = Arc::new(AtomicBool::new(false));
        self.renderers
            .push((endpoint.id.clone(), Arc::clone(&ended)));
        self.renderers_started.push(endpoint.id.clone());
        Ok(MockRenderer {
            ended,
            faded: Arc::clone(&self.faded),
            stop_requested: false,
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
        let created = driver
            .processes
            .iter()
            .filter_map(|process| process.created)
            .max()
            .unwrap_or(0)
            + 1;
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

    /// Quits a process. A capture of its tree reports that its source quit.
    pub fn quit_app(&mut self, pid: u32) {
        let driver = self.driver_mut();
        driver.processes.retain(|process| process.pid != pid);
        driver
            .session_pids
            .retain(|&session_pid| session_pid != pid);
        if let Some(link) = &driver.capture {
            let mut link = lock(link);
            if link.activation.mode == LoopbackMode::IncludeTree
                && link.activation.target_pid == pid
                && link.state == CaptureState::Running
            {
                link.state = CaptureState::SourceQuit;
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

    /// Makes every later capture start fail with `error`, or succeed again
    /// with `None`.
    pub fn refuse_captures(&mut self, error: Option<BackendError>) {
        self.driver_mut().capture_error = error;
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
