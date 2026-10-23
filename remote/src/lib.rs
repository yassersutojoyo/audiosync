//! AudioSync's web remote.
//!
//! When the user turns it on, the app runs one small HTTP/1.1 server on the
//! home network. It serves one page, built into the binary, and a JSON API
//! under `/api` that lets a paired phone switch speakers on and off, set
//! their volume and delay and the master volume, and pick the capture
//! source: nothing the app's own window can't do.
//!
//! The app stays the source of truth. It publishes a full [`State`] snapshot
//! whenever something the page shows changes ([`RemoteServer::publish`]);
//! the server numbers the snapshots and streams each new one to the open
//! pages as a Server-Sent Event. The page's changes come back as
//! [`Command`]s through the one callback given to
//! [`RemoteServer::start`], on a server thread; the app applies them like a
//! click in its window and publishes the result. The server never reads the
//! app's state on its own.
//!
//! The connection is plain HTTP, so the server guards the control channel
//! itself: only private, link-local and loopback peers, and IPv6 peers in
//! this machine's own /64, get an answer, the
//! `Host` header must name this machine ([`net`]), every `POST` needs a
//! matching `Origin` and a JSON body, and every API route but pairing needs
//! the device token that a single-use 6-digit pairing code buys
//! ([`auth`]). Tokens are 128 random bits; only their SHA-256 is kept.
//!
//! - [`server`]: the listener, the routes and the event stream.
//! - [`state`]: the snapshot and the commands, to and from JSON.
//! - [`auth`]: the pairing code, its rate limit and lockout, and the
//!   paired-device store.
//! - [`net`]: which peers, `Host` and `Origin` values are accepted.
//! - [`http`]: reading one request and writing one response.
//! - [`page`]: the page, its manifest and icons, and the
//!   Content-Security-Policy hashes of its inline script and style.
//! - [`text`]: the error codes and messages the API answers with.

pub mod auth;
pub mod http;
pub mod net;
pub mod page;
pub mod server;
pub mod state;
pub mod text;

pub use server::{Capacity, Config, DEFAULT_PORT, RemoteError, RemoteServer, ServerEvent, Timing};
pub use state::{
    Command, NowPlaying, Preset, Source, SourceKind, Speaker, SpeakerChange, State, StateError,
    Zone, ZoneChange, ZoneCounts,
};
