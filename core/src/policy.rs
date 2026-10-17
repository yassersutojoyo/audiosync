//! Platform-independent host policies: the decisions the host application
//! makes around the audio engine (spec sections 2, 6 to 11 and 17).
//!
//! Everything here is a pure decision or a small state machine driven by the
//! host: no clock, thread, timer, network or OS call. Times are [`Seconds`] on a
//! monotonic clock the host chooses. Speakers are identified by engine output
//! ID (AirPlay) or device UID (local), never by display name (S147). None of
//! this runs on an audio thread, so it may allocate.
//!
//! - [`delay`]: local output delays and the AirPlay sync rule (S3, S77 to S81).
//! - [`source`]: capture sources and the microphone permission (S12, S15).
//! - [`capture`]: whether captured audio flows (S17 to S19).
//! - [`session`]: starting, stopping and output sync (S20, S21, S68, S69, S116 to S122, S136).
//! - [`engine`]: the AirPlay engine process lifecycle and pipe playback (S98, S99).
//! - [`reconcile`]: the engine's speaker list, reachability and selection (S100, S102 to S105).
//! - [`pairing`]: speakers that need a PIN or password (S106 to S109).
//! - [`speakers`]: speaker keys, groups, device lists, layout and notices (S149 to S153).

#![forbid(unsafe_code)]

/// Seconds on a monotonic clock chosen by the host.
pub type Seconds = f64;

pub mod capture;
pub mod delay;
pub mod engine;
pub mod pairing;
pub mod reconcile;
pub mod session;
pub mod source;
pub mod speakers;
