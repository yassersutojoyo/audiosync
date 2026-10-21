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
//! - [`device_volume`]: an output device's own volume and mute, and the fix (S192 to S202).
//! - [`source`]: capture sources, the microphone permission and app quits (S12, S15, S164).
//! - [`source_quit`]: stopping or going on with whole-system audio when the source app quits (S226).
//! - [`capture`]: whether captured audio flows, and capture rate changes (S17 to S19, S157).
//! - [`session`]: starting, stopping, output sync and source switches (S20, S21, S68, S69, S116 to S122, S136, S161 to S163).
//! - [`speakers`]: speaker keys, groups, device lists, layout and notices (S149 to S153).

#![forbid(unsafe_code)]

/// Seconds on a monotonic clock chosen by the host.
pub type Seconds = f64;

pub mod capture;
pub mod delay;
pub mod device_volume;
pub mod session;
pub mod source;
pub mod source_quit;
pub mod speakers;
