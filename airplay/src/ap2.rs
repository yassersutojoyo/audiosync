//! The AirPlay 2 session (sections 4 to 6 of the spec), behind the hidden
//! switch (P2): [`framing`] encrypts the RTSP and event connections after
//! the HomeKit pairing (P67 to P70), [`bodies`] writes and reads the binary
//! property lists (P41 to P44, P106, P164), [`session`] runs one session
//! from `GET /info` to TEARDOWN, [`pin`] runs the PIN pairing the host asks
//! for on a connection of its own (P61, P153), and [`audio`] seals its
//! realtime audio packets (P155, P156). Only the driver creates a session or
//! a PIN pairing, and only while the switch is on (P170); the PTP clock a
//! session may need is in [`crate::ptp`].

pub mod audio;
pub mod bodies;
pub mod framing;
pub mod pin;
pub mod session;

pub use audio::{PacketSealer, StreamKey};
pub use bodies::{PtpPeer, Timing};
pub use pin::PinPairing;
pub use session::{Ap2Config, Ap2Event, Ap2Output, Ap2Session, Ap2State, StreamPorts};
