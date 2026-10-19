//! AudioSync's own AirPlay sender.
//!
//! Written from `docs/AIRPLAY_SPEC.md`; clause IDs (P1, P2, ...) in comments
//! and test names refer to that document. Every protocol part is a plain
//! state machine that takes bytes, events and the time in and gives bytes,
//! actions and events out, so every step can be tested in-process without a
//! network. Sockets, threads and the mDNS browser live in a thin layer on
//! top ([`sender`]), which a host app drives.
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
//! - [`stream`]: one audio stream to many speakers: the host's real-time
//!   input ([`input`]), the format and payloads ([`audio`], [`alac`]), RTP,
//!   sync, resend and timing packets ([`rtp`]) on the sender clock
//!   ([`clock`]), the arrival model and send margin ([`rate`]) and the shared
//!   payload stores ([`store`]), with per-speaker delay.
//! - [`sender`]: what a host app drives. It runs the state machines on the
//!   crate's own threads with real sockets (TCP for RTSP, UDP for audio,
//!   control and timing), browses the network with
//!   [`discovery::mdns::MdnsBrowser`], reconnects lost speakers
//!   ([`reconnect`]) and reports events and counters.
//!
//! Pairing crypto and AirPlay 2 come later; the seams they plug into are
//! named where they sit (for example [`session::Verifier`] and
//! [`session::SessionConfig::auth_setup_key`]).

pub mod alac;
pub mod audio;
pub mod clock;
pub mod digest;
pub mod discovery;
pub mod error;
pub mod ids;
pub mod input;
pub mod metadata;
pub mod rate;
pub mod reconnect;
pub mod rtp;
pub mod rtsp;
pub mod sdp;
pub mod sender;
pub mod session;
pub mod store;
pub mod stream;
mod transport;
pub mod volume;

/// The crate version, which the sender names itself with (P5).
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// The `User-Agent` value every request carries: `AudioSync/<version>` (P5,
/// P22). The sender is this crate, so the version is the crate's.
pub fn user_agent() -> String {
    format!("AudioSync/{VERSION}")
}
