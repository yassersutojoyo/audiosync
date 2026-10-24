//! The PipeWire driver for Linux: the whole system's sound, captured from
//! the default output's monitor and handed to the AirPlay and Google Cast
//! senders.
//!
//! The capture is a PipeWire stream that records what the default output
//! plays (`stream.capture.sink`), left untargeted so the session manager
//! links it to the default output and moves it when that output goes. It
//! runs on its own thread with its own main loop, context and connection
//! ([`stream`]). Local outputs, one app's sound and input devices aren't
//! offered yet: no output plays, so the capture never hears AudioSync.
//!
//! - [`format`]: the format asked for and the check of the one agreed.
//! - [`procfs`]: the running processes, from `/proc`.
//! - [`timing`]: where each captured buffer falls in time.

pub mod format;
pub mod procfs;
#[cfg(target_os = "linux")]
mod stream;
pub mod timing;

#[cfg(target_os = "linux")]
pub use driver::{PipeWireBackend, open};

#[cfg(target_os = "linux")]
mod driver {
    use std::sync::Arc;

    use audiosync_core::eq::EqProcessor;
    use audiosync_core::reader::Reader;
    use audiosync_core::render::RendererControl;
    use audiosync_core::ring::RingWriter;
    use audiosync_core::volume::SharedGain;

    use super::{procfs, stream};
    use crate::audio::backend::BackendError;
    use crate::audio::capture::{Activation, CaptureTarget, Feeds, LoopbackMode};
    use crate::audio::controller::{Controller, Driver, RendererHandle};
    use crate::audio::outputs::{Endpoint, EndpointReading, Listing, StreamSetup};
    use crate::audio::process::ProcessTable;
    use crate::audio::routing::{EndpointAudio, EndpointVolume};

    /// The platform calls of the Linux backend.
    pub struct PipeWireDriver {
        own_pid: u32,
    }

    /// No local output plays on Linux yet, so none ever runs.
    pub enum NoRenderer {}

    impl RendererHandle for NoRenderer {
        fn request_stop(&mut self) {
            match *self {}
        }

        fn has_ended(&self) -> bool {
            match *self {}
        }

        fn is_faded(&self) -> bool {
            match *self {}
        }

        fn setup(&self) -> Option<StreamSetup> {
            match *self {}
        }
    }

    impl Driver for PipeWireDriver {
        type Capture = stream::MonitorCapture;
        type Renderer = NoRenderer;

        fn own_pid(&self) -> u32 {
            self.own_pid
        }

        fn processes(&mut self) -> Result<ProcessTable, BackendError> {
            procfs::snapshot().map_err(|error| BackendError::System {
                call: "read_dir(/proc)",
                code: error.raw_os_error().unwrap_or(0),
            })
        }

        fn session_pids(&mut self) -> Result<Vec<u32>, BackendError> {
            Ok(Vec::new())
        }

        fn outputs(&mut self) -> Result<Listing, BackendError> {
            Ok(Listing::default())
        }

        fn outputs_changed(&mut self) -> bool {
            false
        }

        fn endpoint_audio(&mut self) -> Result<Vec<EndpointAudio>, BackendError> {
            Ok(Vec::new())
        }

        fn watch_endpoint_volume(&mut self, _id: &str) -> Result<(), BackendError> {
            Ok(())
        }

        fn unwatch_endpoint_volume(&mut self, _id: &str) {}

        fn endpoint_volume_changes(&mut self) -> Vec<String> {
            Vec::new()
        }

        fn endpoint_volume(&mut self, _id: &str) -> Result<EndpointVolume, BackendError> {
            Err(BackendError::NotOffered)
        }

        fn set_endpoint_level(&mut self, _id: &str, _level: f32) -> Result<(), BackendError> {
            Err(BackendError::NotOffered)
        }

        fn unmute_endpoint(&mut self, _id: &str) -> Result<(), BackendError> {
            Err(BackendError::NotOffered)
        }

        fn inputs(&mut self) -> Result<Vec<EndpointReading>, BackendError> {
            Ok(Vec::new())
        }

        fn start_capture(
            &mut self,
            target: CaptureTarget,
            writer: RingWriter,
            equalizer: EqProcessor,
            feeds: Feeds,
        ) -> Result<stream::MonitorCapture, BackendError> {
            match target {
                // The whole system: every app except AudioSync's own tree.
                CaptureTarget::Loopback(Activation {
                    mode: LoopbackMode::ExcludeTree,
                    ..
                }) => stream::start(writer, equalizer, feeds),
                _ => Err(BackendError::NotOffered),
            }
        }

        fn start_renderer(
            &mut self,
            _endpoint: &Endpoint,
            _reader: Reader,
            _control: RendererControl,
            _gain: Arc<SharedGain>,
        ) -> Result<NoRenderer, BackendError> {
            Err(BackendError::NotOffered)
        }
    }

    /// The Linux audio backend.
    pub type PipeWireBackend = Controller<PipeWireDriver>;

    /// The Linux audio backend, stopped; [`BackendError::ServiceNotRunning`]
    /// when no PipeWire daemon answers.
    pub fn open() -> Result<PipeWireBackend, BackendError> {
        stream::probe()?;
        Ok(Controller::with_driver(PipeWireDriver {
            own_pid: std::process::id(),
        }))
    }
}
