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
//! listed (S68). An output that can't start is turned off and reported (S69).

use std::collections::BTreeMap;
use std::sync::Arc;

use audiosync_core::eq::{self, EqControl, EqProcessor, EqSettings};
use audiosync_core::reader::{Reader, ReaderControl};
use audiosync_core::ring::{Ring, RingWriter};
use audiosync_core::volume::SharedGain;

use super::SESSION_RATE;
use super::backend::{
    AudioBackend, BackendError, BackendEvent, OutputInfo, SourceId, SourceInfo, StopReason,
};
use super::capture::{Activation, decide_activation, source_list};
#[cfg(test)]
use super::outputs::OutputParams;
use super::outputs::{AirPlay, Endpoint, OutputPolicy};
use super::process::ProcessTable;

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
}

/// A running output. Dropping it stops the output and waits for its thread.
pub trait RendererHandle {
    /// Asks the output to fade out and stop, without waiting for it, so that
    /// several outputs can fade out together before they are dropped.
    fn request_stop(&mut self);

    /// Whether it stopped by itself, because its device went away or failed.
    fn has_ended(&self) -> bool;
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

    /// The connected render endpoints.
    fn outputs(&mut self) -> Result<Vec<Endpoint>, BackendError>;

    /// Whether outputs may have changed since the last call.
    fn outputs_changed(&mut self) -> bool;

    /// Starts capturing as `activation` says, writing to the ring through
    /// `equalizer` and `writer`. Returns once the capture runs, or with why
    /// it couldn't start.
    fn start_capture(
        &mut self,
        activation: Activation,
        writer: RingWriter,
        equalizer: EqProcessor,
    ) -> Result<Self::Capture, BackendError>;

    /// Starts rendering to `endpoint` through `reader`, at the gain in
    /// `gain`. Returns once it plays, or with why it couldn't start:
    /// [`BackendError::DeviceGone`] when the device went away.
    fn start_renderer(
        &mut self,
        endpoint: &Endpoint,
        reader: Reader,
        gain: Arc<SharedGain>,
    ) -> Result<Self::Renderer, BackendError>;
}

/// A running session.
struct Session<D: Driver> {
    source: SourceId,
    ring: Arc<Ring>,
    capture: D::Capture,
    /// The control half of the capture's equalizer.
    equalizer: EqControl,
    /// The playing outputs, by endpoint ID.
    outputs: BTreeMap<String, Playing<D::Renderer>>,
}

/// A playing output and the handles its delay and gain are changed through.
struct Playing<R> {
    control: ReaderControl,
    gain: Arc<SharedGain>,
    handle: R,
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
}

impl<D: Driver> Controller<D> {
    /// A stopped backend over `driver`, with no output set and the equalizer
    /// off.
    pub fn with_driver(driver: D) -> Controller<D> {
        Controller {
            driver,
            policy: OutputPolicy::new(AirPlay::Unavailable),
            present: Vec::new(),
            listing_ok: false,
            session: None,
            equalizer: EqSettings::default(),
            pending: Vec::new(),
        }
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
        let mut listed = match self.driver.outputs() {
            Ok(listed) => listed,
            Err(error) => {
                self.listing_ok = false;
                return (false, Err(error));
            }
        };
        listed.sort_by(|a, b| a.id.cmp(&b.id));
        listed.dedup_by(|a, b| a.id == b.id);
        let changed = listed != self.present;
        self.present = listed;
        self.listing_ok = true;
        (changed, Ok(()))
    }

    /// Lists the endpoints again when the driver says they may have changed,
    /// when `force` is set, or when the last listing failed or never ran, and
    /// queues [`BackendEvent::OutputsChanged`] when the list differs.
    fn refresh_if_needed(&mut self, force: bool) {
        let notified = self.driver.outputs_changed();
        if force || notified || !self.listing_ok {
            let (changed, _) = self.refresh_outputs();
            if changed && !self.pending.contains(&BackendEvent::OutputsChanged) {
                self.pending.push(BackendEvent::OutputsChanged);
            }
        }
    }

    /// Adds and removes renderers so the playing outputs are the enabled,
    /// connected ones. Returns the outputs that couldn't start; they are
    /// turned off (S69).
    fn sync_outputs(&mut self) -> Vec<(String, BackendError)> {
        let Some(session) = self.session.as_mut() else {
            return Vec::new();
        };
        let active: Vec<&str> = session.outputs.keys().map(String::as_str).collect();
        let plan = self.policy.plan(true, &self.present, &active);

        let mut removed: Vec<Playing<D::Renderer>> = plan
            .detach
            .iter()
            .filter_map(|id| session.outputs.remove(id))
            .collect();
        for playing in &mut removed {
            playing.handle.request_stop();
        }
        drop(removed);

        let mut failures = Vec::new();
        for endpoint in plan.attach {
            let params = self.policy.params(&endpoint.id);
            let reader = Reader::new(Arc::clone(&session.ring), SESSION_RATE);
            let control = reader.control();
            control.set_delay_frames(params.delay_frames);
            let gain = Arc::new(SharedGain::new(params.gain));
            match self
                .driver
                .start_renderer(&endpoint, reader, Arc::clone(&gain))
            {
                Ok(handle) => {
                    session.outputs.insert(
                        endpoint.id,
                        Playing {
                            control,
                            gain,
                            handle,
                        },
                    );
                }
                // Listed a moment ago and gone now: it stays on and plays
                // once it is listed again (S68).
                Err(BackendError::DeviceGone) => {}
                Err(error) => {
                    self.policy.set_enabled(&endpoint.id, false);
                    failures.push((endpoint.id, error));
                }
            }
        }
        failures
    }

    /// Stops the playing outputs whose device the last successful listing
    /// doesn't show, and returns their IDs. A stream may not notice that its
    /// device went away (Windows can stop signalling it without a word), but
    /// the device list does (S68).
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
        let mut stopped: Vec<Playing<D::Renderer>> = unlisted
            .iter()
            .filter_map(|id| session.outputs.remove(id))
            .collect();
        for playing in &mut stopped {
            playing.handle.request_stop();
        }
        drop(stopped);
        unlisted
    }

    fn report_failures(&mut self, failures: Vec<(String, BackendError)>) {
        self.pending.extend(
            failures
                .into_iter()
                .map(|(id, error)| BackendEvent::OutputFailed { id, error }),
        );
    }

    /// Stops every output, fading them out together, then the capture.
    fn end_session(&mut self) {
        let Some(session) = self.session.take() else {
            return;
        };
        let Session {
            mut outputs,
            capture,
            ring,
            ..
        } = session;
        for playing in outputs.values_mut() {
            playing.handle.request_stop();
        }
        // Each renderer waits for its thread, and the capture for its own,
        // before the ring they use is freed (S134).
        drop(outputs);
        drop(capture);
        drop(ring);
    }

    /// Gives a playing output its current delay; a delay that didn't change
    /// doesn't re-anchor its reader.
    fn apply_delay(&self, id: &str) {
        let Some(playing) = self.session.as_ref().and_then(|s| s.outputs.get(id)) else {
            return;
        };
        let frames = self.policy.params(id).delay_frames;
        if playing.control.delay_frames() != frames {
            playing.control.set_delay_frames(frames);
        }
    }

    /// Gives a playing output its current gain.
    fn apply_gain(&self, id: &str) {
        if let Some(playing) = self.session.as_ref().and_then(|s| s.outputs.get(id)) {
            playing.gain.set(self.policy.params(id).gain);
        }
    }
}

impl<D: Driver> Drop for Controller<D> {
    fn drop(&mut self) {
        self.end_session();
    }
}

impl<D: Driver> AudioBackend for Controller<D> {
    fn list_sources(&mut self) -> Result<Vec<SourceInfo>, BackendError> {
        let processes = self.driver.processes()?;
        let session_pids = self.driver.session_pids()?;
        Ok(source_list(
            &processes,
            &session_pids,
            self.driver.own_pid(),
        ))
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
                    is_default: endpoint.is_default,
                    enabled: settings.enabled,
                    delay_ms: settings.delay_ms,
                    volume: settings.volume,
                    playing: self.is_playing(&endpoint.id),
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
        self.end_session();
        let processes = match source {
            SourceId::WholeSystem => ProcessTable::default(),
            SourceId::App { .. } => self.driver.processes()?,
        };
        let activation = decide_activation(source, self.driver.own_pid(), &processes)?;
        let ring = Ring::for_rate(SESSION_RATE).map_err(|_| BackendError::UnsupportedFormat)?;
        let writer = ring.writer().ok_or(BackendError::UnsupportedFormat)?;
        // The capture equalizes at the session rate, after any resampling
        // (a positive rate is always accepted).
        let (mut equalizer, processor) = eq::equalizer();
        equalizer.set_sample_rate(f64::from(SESSION_RATE));
        equalizer.set_settings(self.equalizer);
        let capture = self.driver.start_capture(activation, writer, processor)?;
        self.session = Some(Session {
            source: source.clone(),
            ring,
            capture,
            equalizer,
            outputs: BTreeMap::new(),
        });
        self.refresh_if_needed(false);
        let failures = self.sync_outputs();
        self.report_failures(failures);
        Ok(())
    }

    fn stop(&mut self) {
        self.end_session();
    }

    fn running_source(&self) -> Option<SourceId> {
        self.session.as_ref().map(|session| session.source.clone())
    }

    fn set_output_enabled(&mut self, id: &str, enabled: bool) -> Result<(), BackendError> {
        self.policy.set_enabled(id, enabled);
        if self.session.is_none() {
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
        result
    }

    fn set_output_delay(&mut self, id: &str, delay_ms: f64) {
        if self.policy.set_delay_ms(id, delay_ms) {
            self.apply_delay(id);
        }
    }

    fn set_output_volume(&mut self, id: &str, position: f64) {
        if self.policy.set_volume(id, position) {
            self.apply_gain(id);
        }
    }

    fn set_master_volume(&mut self, position: f64) {
        if self.policy.set_master(position) {
            for id in self.playing_ids() {
                self.apply_gain(&id);
            }
        }
    }

    fn master_volume(&self) -> f64 {
        self.policy.master()
    }

    fn set_equalizer(&mut self, settings: EqSettings) {
        self.equalizer = settings;
        if let Some(session) = self.session.as_mut() {
            session.equalizer.set_settings(settings);
        }
    }

    fn poll(&mut self) -> Vec<BackendEvent> {
        // A session whose capture stopped ends first.
        let stop_reason = self
            .session
            .as_ref()
            .and_then(|session| match session.capture.state() {
                CaptureState::Running => None,
                CaptureState::SourceQuit => Some(StopReason::SourceQuit),
                CaptureState::Failed(error) => Some(StopReason::CaptureFailed(error)),
            });
        if let Some(reason) = stop_reason {
            self.end_session();
            self.pending.push(BackendEvent::Stopped { reason });
        }

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
                ended.push(id);
            }
        }

        // A stopped output may mean a device went away before its
        // notification arrived, so the list is read again then too.
        self.refresh_if_needed(!ended.is_empty());
        // An output whose device is no longer listed is lost too, whether or
        // not its stream noticed.
        ended.extend(self.stop_unlisted_outputs());
        let failures = self.sync_outputs();
        for id in ended {
            // An output whose device is still there plays again at once
            // (after a format change, say); one that isn't is lost.
            if !self.is_playing(&id) && !failures.iter().any(|(failed, _)| *failed == id) {
                self.pending.push(BackendEvent::OutputLost { id });
            }
        }
        self.report_failures(failures);
        std::mem::take(&mut self.pending)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::capture::LoopbackMode;
    use crate::audio::mock::{MOCK_OWN_PID, MockBackend};

    const SPEAKERS: &str = "speakers";
    const HEADPHONES: &str = "headphones";
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
            [Activation {
                mode: LoopbackMode::ExcludeTree,
                target_pid: MOCK_OWN_PID,
            }]
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
            [Activation {
                mode: LoopbackMode::IncludeTree,
                target_pid: PLAYER_PID,
            }]
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
            backend.driver().renderers_faded(),
            1,
            "its renderer stopped"
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
        assert!(backend.poll().is_empty());
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
    fn an_output_that_cant_start_is_turned_off_and_reported() {
        let mut backend = backend();
        let error = BackendError::System {
            call: "IAudioClient::Initialize",
            code: 0x8889_000A_u32 as i32,
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
        backend.set_output_enabled("later", true).unwrap();
        backend.set_output_delay("later", 40.0);
        backend.start(&SourceId::WholeSystem).unwrap();
        assert!(playing(&backend).is_empty(), "not connected yet");
        backend.connect_output("later", "Later");
        backend.poll();
        assert_eq!(playing(&backend), ["later"]);
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
}
