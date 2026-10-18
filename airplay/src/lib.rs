//! AudioSync's own AirPlay sender.
//!
//! Written from `docs/AIRPLAY_SPEC.md`; clause IDs (P1, P2, ...) in comments
//! and test names refer to that document. The design is sans-I/O: every
//! protocol part is a plain state machine that takes bytes, events and the
//! time in and gives bytes, actions and events out. Sockets, threads, the
//! mDNS browser and the audio clock belong to the layer that drives it, so
//! every step can be tested in-process without a network.
//!
//! What is here so far:
//!
//! - [`discovery`]: DNS-SD records of `_raop._tcp` and `_airplay._tcp`, their
//!   TXT keys, one merged description per device with its capabilities and
//!   verdicts, and the choice of protocol path, codec and address.
//! - [`rtsp`], [`sdp`], [`digest`]: RTSP/1.0 messages, the ANNOUNCE body and
//!   RFC 2617 digest authentication.
//! - [`session`]: the AirPlay 1 session from OPTIONS to TEARDOWN, with volume,
//!   metadata and the error classification of section 10.
//!
//! Audio encoding, RTP, timing, retransmission, pairing crypto, AirPlay 2 and
//! real sockets come later; the seams they plug into are named where they
//! sit (for example [`session::Verifier`] and [`discovery::Browser`]).

pub mod digest;
pub mod discovery;
pub mod error;
pub mod ids;
pub mod metadata;
pub mod rtsp;
pub mod sdp;
pub mod session;
pub mod volume;

/// The crate version, which the sender names itself with (P5).
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// The `User-Agent` value every request carries: `AudioSync/<version>` (P5,
/// P22). The sender is this crate, so the version is the crate's.
pub fn user_agent() -> String {
    format!("AudioSync/{VERSION}")
}
