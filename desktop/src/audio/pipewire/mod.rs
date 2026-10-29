//! The PipeWire driver for Linux: the system's or one app's sound, or an
//! input device, handed to the AirPlay and Google Cast senders and played
//! on local outputs in step with them.
//!
//! A graph thread follows the registry for the outputs, inputs, the
//! default devices, each output's own volume and the apps' streams
//! ([`registry`]). Each capture, output and microphone is a PipeWire stream
//! on a thread of its own, with its own main loop, context and connection.
//! A capture of the whole system or of apps is linked by AudioSync itself,
//! stream by stream, so AudioSync's own outputs are never captured
//! ([`stream`]); each output is pinned to its sink ([`playback`]).
//!
//! - [`format`]: the format asked for and the check of the one agreed.
//! - [`graph`]: the outputs, inputs, app streams and links PipeWire has.
//! - [`procfs`]: the running processes, from `/proc`.
//! - [`timing`]: where each captured buffer falls in time.

pub mod format;
pub mod graph;
#[cfg(all(test, target_os = "linux"))]
mod live_tests;
#[cfg(target_os = "linux")]
mod microphone;
#[cfg(target_os = "linux")]
mod playback;
pub mod procfs;
#[cfg(target_os = "linux")]
mod registry;
#[cfg(all(test, target_os = "linux"))]
mod restart_tests;
#[cfg(target_os = "linux")]
mod stream;
pub mod timing;

#[cfg(target_os = "linux")]
pub use driver::open;

/// The core's object ID, `PW_ID_CORE`.
#[cfg(target_os = "linux")]
const CORE_ID: u32 = 0;

/// Whether an error event on a connection's core means the connection is
/// gone, for the error `res` (a negative errno) about object `id`: a broken
/// pipe, a reset or a protocol error, after which the socket is closed.
/// Other errors come on the core too and leave it working, such as "unknown
/// resource" for a request about an object the daemon just removed.
#[cfg(target_os = "linux")]
pub fn connection_lost(id: u32, res: i32) -> bool {
    id == CORE_ID && [-libc::EPIPE, -libc::ECONNRESET, -libc::EPROTO].contains(&res)
}

#[cfg(all(test, target_os = "linux"))]
mod core_error_tests {
    use super::connection_lost;

    #[test]
    fn only_a_broken_connection_ends_it() {
        assert!(connection_lost(0, -libc::EPIPE));
        // The socket is closed after these too, so nothing more would come.
        assert!(connection_lost(0, -libc::ECONNRESET));
        assert!(connection_lost(0, -libc::EPROTO));
        // "unknown resource": a request about an object the daemon had
        // already removed, such as a client that just quit.
        assert!(!connection_lost(0, -libc::ENOENT));
        assert!(!connection_lost(0, -libc::EINVAL));
        assert!(!connection_lost(13, -libc::EPIPE));
    }
}

#[cfg(target_os = "linux")]
mod driver {
    use std::sync::Arc;

    use audiosync_core::eq::EqProcessor;
    use audiosync_core::reader::Reader;
    use audiosync_core::render::RendererControl;
    use audiosync_core::ring::RingWriter;
    use audiosync_core::volume::SharedGain;

    use super::registry::Registry;
    use super::{format, microphone, playback, procfs, stream};
    use crate::audio::backend::BackendError;
    use crate::audio::calibration::{Microphone, MicrophoneError};
    use crate::audio::capture::{CaptureTarget, Feeds};
    use crate::audio::controller::{Controller, Driver};
    use crate::audio::outputs::{Endpoint, EndpointReading, Listing};
    use crate::audio::process::ProcessTable;
    use crate::audio::routing::{EndpointAudio, EndpointVolume};

    /// The platform calls of the Linux backend.
    pub struct PipeWireDriver {
        own_pid: u32,
        registry: Registry,
    }

    impl PipeWireDriver {
        fn snapshot(&self) -> Result<ProcessTable, BackendError> {
            procfs::snapshot().map_err(|error| BackendError::System {
                call: "read_dir(/proc)",
                code: error.raw_os_error().unwrap_or(0),
            })
        }
    }

    impl Driver for PipeWireDriver {
        type Capture = stream::MonitorCapture;
        type Renderer = playback::Playback;

        fn own_pid(&self) -> u32 {
            self.own_pid
        }

        fn processes(&mut self) -> Result<ProcessTable, BackendError> {
            self.snapshot()
        }

        fn session_pids(&mut self) -> Result<Vec<u32>, BackendError> {
            let graph = self.registry.graph()?;
            Ok(graph.session_pids(&self.snapshot()?))
        }

        fn outputs(&mut self) -> Result<Listing, BackendError> {
            Ok(self.registry.graph()?.listing())
        }

        fn outputs_changed(&mut self) -> bool {
            self.registry.take_outputs_changed()
        }

        fn endpoint_audio(&mut self) -> Result<Vec<EndpointAudio>, BackendError> {
            let graph = self.registry.graph()?;
            Ok(graph.endpoint_audio(&self.snapshot()?))
        }

        fn watch_endpoint_volume(&mut self, id: &str) -> Result<(), BackendError> {
            self.registry.watch(id);
            Ok(())
        }

        fn unwatch_endpoint_volume(&mut self, id: &str) {
            self.registry.unwatch(id);
        }

        fn endpoint_volume_changes(&mut self) -> Vec<String> {
            self.registry.volume_changes()
        }

        fn endpoint_volume(&mut self, id: &str) -> Result<EndpointVolume, BackendError> {
            self.registry.volume(id)
        }

        fn set_endpoint_level(&mut self, id: &str, level: f32) -> Result<(), BackendError> {
            self.registry.set_level(id, level)
        }

        fn unmute_endpoint(&mut self, id: &str) -> Result<(), BackendError> {
            self.registry.unmute(id)
        }

        fn inputs(&mut self) -> Result<Vec<EndpointReading>, BackendError> {
            Ok(self.registry.graph()?.inputs())
        }

        fn start_capture(
            &mut self,
            target: CaptureTarget,
            writer: RingWriter,
            equalizer: EqProcessor,
            feeds: Feeds,
        ) -> Result<stream::MonitorCapture, BackendError> {
            let (plan, setup) = match &target {
                CaptureTarget::Input { endpoint_id, pair } => {
                    let graph = self.registry.graph()?;
                    let (_, node) = graph.named(endpoint_id).ok_or(BackendError::DeviceGone)?;
                    let serial = node.prop("object.serial").ok_or(BackendError::DeviceGone)?;
                    let channels = node.prop_channels().unwrap_or(2);
                    let plan = stream::Plan {
                        wiring: stream::Wiring::Input,
                        filter: None,
                        serial: Some(serial.to_owned()),
                    };
                    (plan, format::input_setup(*pair, channels))
                }
                _ => {
                    let plan = stream::Plan {
                        wiring: stream::Wiring::Links,
                        filter: stream::filter_of(&target, self.own_pid),
                        serial: None,
                    };
                    (plan, format::SETUP)
                }
            };
            stream::start_setup(plan, setup, writer, equalizer, feeds)
        }

        fn start_renderer(
            &mut self,
            endpoint: &Endpoint,
            reader: Reader,
            control: RendererControl,
            gain: Arc<SharedGain>,
        ) -> Result<playback::Playback, BackendError> {
            let target = playback::target(&self.registry.graph()?, &endpoint.id)?;
            playback::start(target, reader, control, gain)
        }

        fn open_microphone(
            &mut self,
            seconds: f64,
        ) -> Result<Box<dyn Microphone>, MicrophoneError> {
            let graph = self
                .registry
                .graph()
                .map_err(|_| MicrophoneError::Missing)?;
            let (id, node) = graph.default_input().ok_or(MicrophoneError::Missing)?;
            if graph.default_input_hands_free() {
                return Err(MicrophoneError::HandsFree);
            }
            let rate = graph
                .nodes
                .get(&id)
                .and_then(|node| node.format)
                .map(|(rate, _)| rate);
            let microphone = microphone::open(node.name().to_owned(), rate, seconds)?;
            Ok(Box::new(microphone))
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
            registry: Registry::start()?,
        }))
    }
}
