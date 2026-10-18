//! The WASAPI driver: process loopback capture, one render stream per
//! output, endpoint listing and notifications, and the process snapshot.
//!
//! Each audio thread initializes COM in the multithreaded apartment and makes
//! its own COM objects; only endpoint IDs, the ring's reader and writer, and
//! atomics cross threads. Calls from the UI thread (listing endpoints,
//! sessions and processes) use COM in whatever apartment that thread has.

mod com;
mod devices;
mod endpoint;
mod loopback;
mod processes;

use std::sync::Arc;

use audiosync_core::eq::EqProcessor;
use audiosync_core::reader::Reader;
use audiosync_core::ring::RingWriter;
use audiosync_core::volume::SharedGain;

use super::backend::BackendError;
use super::capture::Activation;
use super::controller::{Controller, Driver};
use super::outputs::{Endpoint, Listing};
use super::process::ProcessTable;

/// The platform calls of the Windows backend.
pub struct WasapiDriver {
    watcher: devices::DeviceWatcher,
    own_pid: u32,
}

impl WasapiDriver {
    /// Starts watching the render endpoints.
    pub fn new() -> Result<WasapiDriver, BackendError> {
        Ok(WasapiDriver {
            watcher: devices::DeviceWatcher::start()?,
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

    fn start_capture(
        &mut self,
        activation: Activation,
        writer: RingWriter,
        equalizer: EqProcessor,
    ) -> Result<loopback::LoopbackCapture, BackendError> {
        loopback::start(activation, writer, equalizer)
    }

    fn start_renderer(
        &mut self,
        endpoint: &Endpoint,
        reader: Reader,
        gain: Arc<SharedGain>,
    ) -> Result<endpoint::EndpointRenderer, BackendError> {
        endpoint::start(&endpoint.id, reader, gain)
    }
}

/// The Windows audio backend.
pub type WasapiBackend = Controller<WasapiDriver>;

/// Opens the Windows audio backend, stopped.
pub fn open() -> Result<WasapiBackend, BackendError> {
    Ok(Controller::with_driver(WasapiDriver::new()?))
}
