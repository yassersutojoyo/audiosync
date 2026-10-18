//! The Windows audio backend: what AudioSync captures and where it plays it.
//!
//! Capture uses the Application Loopback API (WASAPI process loopback): one
//! app's process tree, or every app except AudioSync's own tree. Each enabled
//! render endpoint (speakers, headphones, HDMI, Bluetooth) gets its own render
//! thread, which reads the shared ring of `audiosync-core` at that output's
//! delay and volume. The core does the signal work (ring, delay reader,
//! resampler, volume curve, channel mapping); this module decides what to
//! capture and which outputs play, and talks to Windows.
//!
//! - [`backend`]: the [`backend::AudioBackend`] trait the UI uses, and its types.
//! - [`controller`]: the backend logic, over a [`controller::Driver`] that owns
//!   the platform calls.
//! - [`mock`]: an in-memory driver and backend for UI work and tests.
//! - [`process`]: process trees, from a snapshot of the running processes.
//! - [`capture`]: what process loopback captures, the source list, the capture
//!   formats and the conversion of captured packets into the ring.
//! - [`outputs`]: which outputs play, at what delay and gain, and their format.
//! - [`render`]: an output's render pipeline, from the ring to the device.
//! - `wasapi` (Windows only): the WASAPI driver.
//!
//! Everything except `wasapi` is plain Rust and tested on any host. The
//! real-time paths (the capture and render loops, [`capture::CaptureConverter`]
//! and [`render::RenderPipeline`]) never allocate, lock, block or log.

// Until the UI drives the backend, nothing outside this module and its tests
// calls it.
#![allow(dead_code, reason = "the UI is wired to the backend in a later change")]

pub mod backend;
pub mod capture;
pub mod controller;
pub mod mock;
pub mod outputs;
pub mod process;
pub mod render;
#[cfg(windows)]
pub mod wasapi;

use backend::{AudioBackend, BackendError};

/// The rate of the shared ring and of every reader, in Hz (Rc in the engine
/// specification): capture converts to it when Windows delivers another rate,
/// and each output converts from it to its device's rate.
pub const SESSION_RATE: u32 = 48_000;

/// The backend for this platform: WASAPI on Windows; elsewhere the demo mock,
/// so the UI can be worked on without Windows.
pub fn platform_backend() -> Result<Box<dyn AudioBackend>, BackendError> {
    #[cfg(windows)]
    {
        Ok(Box::new(wasapi::open()?))
    }
    #[cfg(not(windows))]
    {
        Ok(Box::new(mock::MockBackend::demo()))
    }
}
