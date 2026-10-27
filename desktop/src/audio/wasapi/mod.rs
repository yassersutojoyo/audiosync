//! The WASAPI driver: process loopback and input device capture, one render
//! stream per output, endpoint listing and notifications, the process
//! snapshot, the read-only endpoint snapshot of the double-audio check,
//! each output's own volume and mute (read, followed, and set only by the
//! fix the user clicks), and the calibration's recording of the default
//! input device.
//!
//! Each audio thread initializes COM in the multithreaded apartment and makes
//! its own COM objects; only endpoint IDs, the ring's reader and writer, and
//! atomics cross threads. Calls from the UI thread (listing endpoints,
//! sessions and processes) use COM in whatever apartment that thread has.

mod com;
mod devices;
mod endpoint;
mod loopback;
mod media;
mod media_sessions;
mod microphone;
mod processes;
mod routing;
mod volume;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use audiosync_core::eq::EqProcessor;
use audiosync_core::reader::Reader;
use audiosync_core::render::RendererControl;
use audiosync_core::ring::RingWriter;
use audiosync_core::volume::SharedGain;

use super::backend::BackendError;
use super::calibration::{Microphone, MicrophoneError};
use super::capture::{CaptureTarget, Feeds};
use super::controller::{Controller, Driver};
use super::outputs::{Endpoint, EndpointReading, Listing};
use super::process::ProcessTable;
use super::routing::{EndpointAudio, EndpointVolume};
use crate::debug_log::DebugLog;

pub use media_sessions::read_media_sessions;

/// The platform calls of the Windows backend.
pub struct WasapiDriver {
    watcher: devices::DeviceWatcher,
    /// The thread that reads, follows and sets the outputs' own volumes,
    /// from the first call on.
    volumes: volume::Volumes,
    own_pid: u32,
}

impl WasapiDriver {
    /// Starts watching the render endpoints.
    pub fn new() -> Result<WasapiDriver, BackendError> {
        Ok(WasapiDriver {
            watcher: devices::DeviceWatcher::start()?,
            volumes: volume::Volumes::default(),
            own_pid: std::process::id(),
        })
    }
}

impl Driver for WasapiDriver {
    type Capture = loopback::LoopbackCapture;
    type Renderer = endpoint::EndpointRenderer;

    fn own_pid(&self) -> u32 {
        self.own_pid
    }

    fn processes(&mut self) -> Result<ProcessTable, BackendError> {
        processes::snapshot()
    }

    fn session_pids(&mut self) -> Result<Vec<u32>, BackendError> {
        processes::audio_session_pids()
    }

    fn outputs(&mut self) -> Result<Listing, BackendError> {
        devices::render_endpoints()
    }

    fn outputs_changed(&mut self) -> bool {
        self.watcher.take_changed()
    }

    fn inputs(&mut self) -> Result<Vec<EndpointReading>, BackendError> {
        devices::capture_endpoints()
    }

    fn endpoint_audio(&mut self) -> Result<Vec<EndpointAudio>, BackendError> {
        routing::endpoint_audio()
    }

    fn watch_endpoint_volume(&mut self, id: &str) -> Result<(), BackendError> {
        self.volumes.watcher()?.watch(id)
    }

    fn unwatch_endpoint_volume(&mut self, id: &str) {
        if let Some(watcher) = self.volumes.running() {
            watcher.unwatch(id);
        }
    }

    fn endpoint_volume_changes(&mut self) -> Vec<String> {
        self.volumes
            .running()
            .map(|watcher| watcher.take_changed())
            .unwrap_or_default()
    }

    fn endpoint_volume(&mut self, id: &str) -> Result<EndpointVolume, BackendError> {
        self.volumes.watcher()?.read(id)
    }

    fn set_endpoint_level(&mut self, id: &str, level: f32) -> Result<(), BackendError> {
        self.volumes.watcher()?.set_level(id, level)
    }

    fn unmute_endpoint(&mut self, id: &str) -> Result<(), BackendError> {
        self.volumes.watcher()?.unmute(id)
    }

    fn start_capture(
        &mut self,
        target: CaptureTarget,
        writer: RingWriter,
        equalizer: EqProcessor,
        feeds: Feeds,
    ) -> Result<loopback::LoopbackCapture, BackendError> {
        loopback::start(target, writer, equalizer, feeds)
    }

    fn start_renderer(
        &mut self,
        endpoint: &Endpoint,
        reader: Reader,
        control: RendererControl,
        gain: Arc<SharedGain>,
    ) -> Result<endpoint::EndpointRenderer, BackendError> {
        endpoint::start(&endpoint.id, reader, control, gain)
    }

    fn open_microphone(&mut self, seconds: f64) -> Result<Box<dyn Microphone>, MicrophoneError> {
        Ok(Box::new(microphone::open(seconds)?))
    }
}

/// The Windows audio backend.
pub type WasapiBackend = Controller<WasapiDriver>;

/// Opens the Windows audio backend, stopped.
pub fn open() -> Result<WasapiBackend, BackendError> {
    Ok(Controller::with_driver(WasapiDriver::new()?))
}

/// Registers the calling thread as a Pro Audio task for as long as the
/// returned value lives (engine spec S323): the AirPlay sender's driver
/// thread, which at normal priority woke more than 5 ms late on about 0.1% of
/// packets on a busy PC. A refusal is logged once per run.
pub fn pro_audio_thread() -> Box<dyn std::any::Any> {
    static NOTED: AtomicBool = AtomicBool::new(false);
    let task = com::ProAudioTask::join();
    if let Some(line) = refusal_note(task.is_some(), &NOTED) {
        // The setup runs on the driver thread, which has no handle on the
        // app's log, so it appends through one of its own.
        let log = DebugLog::appending_from_env();
        log.log(|| line.to_owned());
        log.flush(Duration::from_secs(1));
    }
    Box::new(task)
}

/// The debug line for a refused Pro Audio registration, the first time only.
fn refusal_note(joined: bool, noted: &AtomicBool) -> Option<&'static str> {
    if joined || noted.swap(true, Ordering::Relaxed) {
        return None;
    }
    Some("AirPlay driver thread: MMCSS refused the Pro Audio task; it runs at normal priority")
}

#[cfg(test)]
mod tests {
    use super::*;

    // covers: S323
    #[test]
    fn a_refused_pro_audio_registration_is_noted_once() {
        let noted = AtomicBool::new(false);
        assert_eq!(refusal_note(true, &noted), None);
        assert!(refusal_note(false, &noted).is_some());
        assert_eq!(refusal_note(false, &noted), None);
    }
}
