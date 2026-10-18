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

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use audiosync_core::eq::{self, EqControl, EqProcessor, EqSettings};
use audiosync_core::reader::{Reader, ReaderControl};
use audiosync_core::ring::{Ring, RingWriter};
use audiosync_core::volume::SharedGain;

use super::SESSION_RATE;
use super::backend::{
    AudioBackend, BackendError, BackendEvent, OutputInfo, SourceId, SourceInfo, StopReason,
};
use super::capture::{
    Activation, CaptureAttempt, LoopbackMode, SampleType, decide_activation, running_app,
    source_list,
};
#[cfg(test)]
use super::outputs::OutputParams;
use super::outputs::{AirPlay, Endpoint, Listing, ListingNote, OutputPolicy, StreamSetup};
use super::process::ProcessTable;
use super::routing::{
    self, Captured, DirectPath, DirectReason, EndpointAudio, EndpointVolume, PlayedOn,
};
use crate::debug_log::DebugLog;

/// How often the debug log gets a status line while a session runs.
const STATUS_INTERVAL: Duration = Duration::from_secs(5);

/// How often a running session checks whether the source is heard twice
/// ([`routing`](super::routing)).
const ROUTING_INTERVAL: Duration = Duration::from_secs(3);

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

/// A running output. Dropping it stops the output and waits for its thread.
pub trait RendererHandle {
    /// Asks the output to fade out and stop, without waiting for it, so that
    /// several outputs can fade out together before they are dropped.
    fn request_stop(&mut self);

    /// Whether it stopped by itself, because its device went away or failed.
    fn has_ended(&self) -> bool;

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
    /// What the capture takes in.
    activation: Activation,
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
    /// The notes of the last listing: notes that repeat at every listing
    /// are logged once.
    listing_notes: Vec<ListingNote>,
    /// When the log last got a status line, and the frames captured then.
    last_status: Option<(Instant, u64)>,
    /// The double-audio check of the running session.
    routing: RoutingState,
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
            listing_notes: Vec::new(),
            last_status: None,
            routing: RoutingState::default(),
        }
    }

    /// Sends what the backend does to `log`.
    pub fn set_debug_log(&mut self, log: DebugLog) {
        self.log = log;
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
        self.listing_ok = true;
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
        for id in &plan.detach {
            let why = if self.policy.settings(id).enabled {
                "its device isn't listed"
            } else {
                "switched off"
            };
            self.log
                .log(|| format!("output {} stopped: {why}", label(&self.names, id)));
        }

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
                    let setup = handle.setup();
                    if let Some(setup) = setup {
                        self.setups.insert(endpoint.id.clone(), setup);
                    }
                    self.log.log(|| {
                        let settings = self.policy.settings(&endpoint.id);
                        format!(
                            "output {} started: {}; delay {} ms, volume {} (gain {:.3})",
                            label(&self.names, &endpoint.id),
                            describe_stream(setup),
                            settings.delay_ms,
                            settings.volume,
                            params.gain,
                        )
                    });
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
                Err(BackendError::DeviceGone) => {
                    self.log.log(|| {
                        format!(
                            "output {} went away while starting; it stays on",
                            label(&self.names, &endpoint.id)
                        )
                    });
                }
                Err(error) => {
                    self.log.log(|| {
                        format!(
                            "output {} could not start: {error} It is turned off.",
                            label(&self.names, &endpoint.id)
                        )
                    });
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
        let started = Instant::now();
        let Session {
            mut outputs,
            capture,
            ring,
            ..
        } = session;
        let stopped = outputs.len();
        for playing in outputs.values_mut() {
            playing.handle.request_stop();
        }
        // Each renderer waits for its thread, and the capture for its own,
        // before the ring they use is freed (S134).
        drop(outputs);
        drop(capture);
        let captured = ring.frames_written();
        drop(ring);
        self.last_status = None;
        self.log.log(|| {
            format!(
                "session ended: the capture and {stopped} output(s) stopped in {} ms; \
                 {captured} frames captured",
                started.elapsed().as_millis()
            )
        });
        // Without a session, nothing is heard twice.
        let routing = std::mem::take(&mut self.routing);
        if !routing.reported.is_empty() {
            self.log
                .log(|| "double audio: none, streaming stopped".to_owned());
            self.pending
                .push(BackendEvent::HeardTwice { paths: Vec::new() });
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
        let Some((since, frames_then)) = self.last_status else {
            self.last_status = Some((now, frames));
            return;
        };
        let elapsed = now.saturating_duration_since(since);
        if elapsed < STATUS_INTERVAL {
            return;
        }
        self.last_status = Some((now, frames));
        self.log.log(|| {
            let seconds = elapsed.as_secs_f64();
            let mut text = format!(
                "status: capturing {}; {frames} frames captured, {:.0} frames/s over {seconds:.1} s",
                describe_source(&session.source),
                frames.saturating_sub(frames_then) as f64 / seconds,
            );
            for (id, playing) in &session.outputs {
                let settings = self.policy.settings(id);
                let counters = playing.control.counters();
                text.push_str(&format!(
                    "\n  output {}: delay {} ms ({} frames), volume {} (gain {:.3}), lag {} frames; \
                     primes {}, resyncs {}, starved reads {} ({} frames), drift skips {} ({} frames), \
                     drift silences {} ({} frames), rejected copies {}",
                    label(&self.names, id),
                    settings.delay_ms,
                    playing.control.delay_frames(),
                    settings.volume,
                    playing.gain.get(),
                    playing.control.lag_frames(),
                    counters.primes,
                    counters.resyncs,
                    counters.starved_renders,
                    counters.starved_frames,
                    counters.drift_skips,
                    counters.drift_skip_frames,
                    counters.drift_silences,
                    counters.drift_silence_frames,
                    counters.rejected_copies,
                ));
            }
            if session.outputs.is_empty() {
                text.push_str("\n  no output plays");
            }
            text
        });
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

    /// Starts a session capturing `source`, after the running one ended.
    fn start_session(&mut self, source: &SourceId) -> Result<(), BackendError> {
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
        self.log.log(|| {
            format!(
                "capture started: {}",
                describe_capture(activation, capture.setup())
            )
        });
        self.session = Some(Session {
            source: source.clone(),
            activation,
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
        self.routing.last_check = Some(now);
        let activation = session.activation;
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
        self.routing.played_on = answer.played_on;
        if answer.paths != self.routing.reported {
            self.log.log(|| {
                describe_routing(
                    &answer.paths,
                    &self.routing.reported,
                    &endpoints,
                    &captured,
                    &rendered,
                )
            });
            self.routing.reported.clone_from(&answer.paths);
            self.pending.push(BackendEvent::HeardTwice {
                paths: answer.paths,
            });
        }
    }

    /// The sources to offer, from the processes and audio sessions now.
    fn read_sources(&mut self) -> Result<Vec<SourceInfo>, BackendError> {
        let processes = self.driver.processes()?;
        let session_pids = self.driver.session_pids()?;
        Ok(source_list(
            &processes,
            &session_pids,
            self.driver.own_pid(),
        ))
    }
}

impl<D: Driver> Drop for Controller<D> {
    fn drop(&mut self) {
        self.end_session();
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
        self.log
            .log(|| format!("starting: capturing {}", describe_source(source)));
        self.start_session(source).inspect_err(|error| {
            self.log.log(|| format!("the start failed: {error}"));
        })
    }

    fn stop(&mut self) {
        if self.session.is_some() {
            self.log.log(|| "stopping".to_owned());
        }
        self.end_session();
    }

    fn running_source(&self) -> Option<SourceId> {
        self.session.as_ref().map(|session| session.source.clone())
    }

    fn set_output_enabled(&mut self, id: &str, enabled: bool) -> Result<(), BackendError> {
        self.log.log(|| {
            let state = if enabled { "on" } else { "off" };
            format!("output {} switched {state}", label(&self.names, id))
        });
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
        let before = self.policy.settings(id).delay_ms;
        if self.policy.set_delay_ms(id, delay_ms) {
            self.apply_delay(id);
            let after = self.policy.settings(id).delay_ms;
            if after != before {
                self.log.log(|| {
                    format!(
                        "output {} delay: {after} ms ({} frames)",
                        label(&self.names, id),
                        self.policy.params(id).delay_frames,
                    )
                });
            }
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

    fn set_master_volume(&mut self, position: f64) {
        let before = self.policy.master();
        if self.policy.set_master(position) {
            for id in self.playing_ids() {
                self.apply_gain(&id);
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
            self.log
                .log(|| format!("the capture ended: {}", describe_stop(&reason)));
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
        let now = Instant::now();
        self.check_routing_if_due(now);
        self.log_status_if_due(now);
        let events = std::mem::take(&mut self.pending);
        for event in &events {
            self.log
                .log(|| format!("event: {}", describe_event(event, &self.names)));
        }
        events
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
) -> String {
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
                match pids.as_slice() {
                    [] => "the source played there last and is silent now".to_owned(),
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
    }
}

/// What a capture captures, and the format Windows delivers it in.
fn describe_capture(activation: Activation, setup: Option<CaptureAttempt>) -> String {
    let pid = activation.target_pid;
    let what = match activation.mode {
        LoopbackMode::ExcludeTree => format!("everything except process {pid} and its children"),
        LoopbackMode::IncludeTree => format!("process {pid} and its children"),
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
        BackendEvent::Stopped { reason } => format!("stopped: {}", describe_stop(reason)),
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
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use super::*;
    use crate::audio::backend::OutputKind;
    use crate::audio::mock::{MOCK_OWN_PID, MockBackend};
    use crate::audio::outputs::{EndpointProperties, EndpointReading};
    use crate::audio::routing::{DirectPath, DirectReason, HOLD_CHECKS};
    use crate::debug_log::{FILE_NAME, TEST_FLUSH, assert_in_order, read_messages, test_dir};

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
                "output \"Speakers\" (speakers) delay: 120 ms (5760 frames)".to_owned(),
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
                        drift skips 0 (0 frames), drift silences 0 (0 frames), rejected copies 0";
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
}
