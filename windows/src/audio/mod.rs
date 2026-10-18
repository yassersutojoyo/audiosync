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
//! - [`backend`]: the [`backend::AudioBackend`] trait the UI's adapter
//!   (`crate::adapter`) uses, and its types.
//! - [`controller`]: the backend logic, over a [`controller::Driver`] that owns
//!   the platform calls.
//! - `mock` (tests only): an in-memory driver and the backend over it.
//! - [`process`]: process trees, from a snapshot of the running processes.
//! - [`capture`]: what process loopback captures, the source list, the capture
//!   formats and the conversion of captured packets into the ring, through
//!   the equalizer.
//! - [`outputs`]: which outputs play, at what delay and gain, and their format.
//! - [`render`]: an output's render pipeline, from the ring to the device.
//! - `wasapi` (Windows only): the WASAPI driver; `wasapi::open` gives the
//!   backend the app runs on.
//!
//! Everything except `wasapi` is plain Rust and tested on any host. The
//! real-time paths (the capture and render loops, [`capture::CaptureConverter`]
//! and [`render::RenderPipeline`]) never allocate, lock, block or log.

pub mod backend;
pub mod capture;
pub mod controller;
#[cfg(test)]
pub mod mock;
pub mod outputs;
pub mod process;
pub mod render;
#[cfg(windows)]
pub mod wasapi;

/// The rate of the shared ring and of every reader, in Hz (Rc in the engine
/// specification): capture converts to it when Windows delivers another rate,
/// and each output converts from it to its device's rate.
pub const SESSION_RATE: u32 = 48_000;
