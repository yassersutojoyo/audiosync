//! AudioSync's own Google Cast sender.
//!
//! Written from `docs/CAST_SPEC.md`; clause IDs (C1, C2, ...) in comments
//! and test names refer to that document. The protocol parts are plain
//! state machines that take messages and the time in and give messages and
//! events out, so they are tested in-process; [`client`] runs them over a
//! real TLS connection.
//!
//! What is here so far:
//!
//! - [`discovery`]: the DNS-SD records of `_googlecast._tcp`, their TXT
//!   keys and one device list keyed by `id`, fed live by
//!   [`discovery::mdns::MdnsBrowser`].
//! - [`message`], [`framing`]: the `CastMessage` protocol buffer, its
//!   endpoints and namespaces, and the length-prefixed frames on the wire.
//! - [`channel`]: `CONNECT`, the heartbeat, `RECEIVER_STATUS`, the launch of
//!   the Default Media Receiver and the teardown, with request ids
//!   ([`ids`]) and the platform status ([`status`]).
//! - [`client`]: TLS to the device, accepting any certificate and
//!   recording its fingerprint.
//! - [`error`]: the typed errors and the connect backoff.
//!
//! The stream server, `LOAD`, volume and timing come in later stages.

pub mod channel;
pub mod client;
pub mod discovery;
pub mod error;
pub mod framing;
pub mod ids;
pub mod message;
pub mod status;
