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
//! - [`airplay`]: AirPlay speakers through AudioSync's own sender
//!   (`audiosync-airplay`), and the capture's feed into it.
//! - [`backend`]: the [`backend::AudioBackend`] trait the UI's adapter
//!   (`crate::adapter`) uses, and its types.
//! - [`calibration`]: measuring a local output's offset against the
//!   reference with the microphone.
//! - [`controller`]: the backend logic, over a [`controller::Driver`] that owns
//!   the platform calls.
//! - `media_sessions`: the one thread that reads the system's media
//!   sessions for the receivers and the Now Playing card, over Windows'
//!   sessions (`wasapi`) or the MPRIS players on Linux (`mpris`).
//! - `mock` (tests only): an in-memory driver and the backend over it.
//! - [`now_playing`]: the playing track's title, artist, album and artwork
//!   for the AirPlay receivers that show it, from the media sessions.
//! - [`process`]: process trees, from a snapshot of the running processes.
//! - [`capture`]: what process loopback captures, the source list, the capture
//!   formats and the conversion of captured packets into the ring, through
//!   the equalizer.
//! - [`outputs`]: which outputs play, at what delay and gain, and their format.
//! - [`pairings`]: the credentials of paired Apple TVs, kept per speaker in a
//!   file encrypted for the current user (DPAPI on Windows).
//! - `pipewire` (Linux; its plain parts are tested on any host): the
//!   PipeWire driver, which captures the whole system for the network
//!   speakers.
//! - [`render`]: an output's render pipeline, from the ring to the device.
//! - [`routing`]: whether the source is heard twice, straight from Windows
//!   as well as through AudioSync's delayed copies.
//! - `wasapi` (Windows only): the WASAPI driver; `wasapi::open` gives the
//!   backend the app runs on.
//!
//! Everything except `wasapi` is plain Rust and tested on any host. The
//! real-time paths (the capture and render loops, [`capture::CaptureConverter`]
//! with the AirPlay feed, and [`render::RenderPipeline`]) never allocate,
//! lock, block or log.

pub mod airplay;
pub mod backend;
pub mod calibrate_all;
pub mod calibration;
pub mod capture;
pub mod cast;
pub mod cast_address;
pub mod controller;
#[cfg(any(windows, target_os = "linux", test))]
pub mod media_sessions;
pub mod mix;
#[cfg(test)]
pub mod mock;
#[cfg(any(target_os = "linux", test))]
pub mod mpris;
pub mod now_playing;
pub mod outputs;
pub mod pairings;
#[cfg(any(target_os = "linux", test))]
pub mod pipewire;
pub mod process;
pub mod render;
pub mod routing;
#[cfg(windows)]
pub mod wasapi;

/// The rate of the shared ring and of every reader, in Hz (Rc in the engine
/// specification): capture converts to it when Windows delivers another rate,
/// and each output converts from it to its device's rate.
pub const SESSION_RATE: u32 = 48_000;
