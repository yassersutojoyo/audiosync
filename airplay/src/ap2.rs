//! The AirPlay 2 session (sections 4 and 6 of the spec), behind the hidden
//! switch (P2): [`framing`] encrypts the RTSP and event connections after
//! the HomeKit pairing (P67 to P70), [`bodies`] writes and reads the binary
//! property lists (P41 to P44, P106, P164), and [`session`] runs one session
//! from `GET /info` to TEARDOWN. Only the driver creates a session, and only
//! while the switch is on (P170). Audio packets and the PTP clock come in a
//! later stage.

pub mod bodies;
pub mod framing;
pub mod session;

pub use bodies::{PtpPeer, Timing};
pub use session::{Ap2Config, Ap2Event, Ap2Output, Ap2Session, Ap2State, StreamPorts};
