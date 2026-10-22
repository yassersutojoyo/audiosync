//! The sender's typed errors and end reasons (C80, C83).

use std::fmt;
use std::io;
use std::net::SocketAddr;
use std::time::Duration;

use crate::framing::FrameError;

/// Why a connection attempt failed (C80). Each says its cause.
#[derive(Debug)]
pub enum ConnectError {
    /// No address to try.
    NoAddress,
    /// No TCP connection to any address; the last error.
    Tcp {
        address: SocketAddr,
        error: io::Error,
    },
    /// The TLS handshake failed, with what the TLS stack reported, which
    /// names the alert when there was one (C20).
    Tls {
        address: SocketAddr,
        message: String,
    },
    /// No `RECEIVER_STATUS` in time after `CONNECT` (C33).
    NoStatus,
    /// The channel ended before it was ready.
    Ended(EndReason),
}

impl fmt::Display for ConnectError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConnectError::NoAddress => f.write_str("the device has no address"),
            ConnectError::Tcp { address, error } => {
                write!(f, "no connection to {address}: {error}")
            }
            ConnectError::Tls { address, message } => {
                write!(f, "TLS handshake with {address} failed: {message}")
            }
            ConnectError::NoStatus => f.write_str("the device did not report its status"),
            ConnectError::Ended(reason) => write!(f, "the connection ended: {reason}"),
        }
    }
}

impl std::error::Error for ConnectError {}

/// Why a launch of the Default Media Receiver failed (C41, C83).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LaunchError {
    /// The channel is not ready for a launch (C33).
    NotReady,
    /// `LAUNCH_ERROR` or another error reply, with its `reason`.
    Refused { reason: String },
    /// No status listing the app in time.
    Timeout,
    /// The channel ended during the launch.
    Ended(EndReason),
}

impl fmt::Display for LaunchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LaunchError::NotReady => f.write_str("the device is not ready"),
            LaunchError::Refused { reason } => {
                write!(f, "the device refused to start the media player ({reason})")
            }
            LaunchError::Timeout => f.write_str("the media player did not start in time"),
            LaunchError::Ended(reason) => write!(f, "the connection ended: {reason}"),
        }
    }
}

impl std::error::Error for LaunchError {}

/// Why a channel ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EndReason {
    /// No `RECEIVER_STATUS` after the first `GET_STATUS` (C33).
    NoStatus,
    /// The app was stopped or replaced elsewhere (C43).
    TakenOver,
    /// Nothing arrived for the dead-channel time (C36).
    Silent,
    /// The device sent `CLOSE` from the platform, or closed the stream.
    ClosedByDevice,
    /// The app's virtual connection closed again after its one reconnect
    /// (C82).
    AppClosed,
    /// A framing or decoding fault (C25).
    Fault(FrameError),
    /// The socket failed.
    Io(String),
    /// The sender's own teardown finished (C87).
    Stopped,
}

impl fmt::Display for EndReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            EndReason::NoStatus => f.write_str("the device did not report its status"),
            EndReason::TakenOver => f.write_str("taken over"),
            EndReason::Silent => f.write_str("the device stopped answering"),
            EndReason::ClosedByDevice => f.write_str("the device closed the connection"),
            EndReason::AppClosed => f.write_str("the media player closed its connection"),
            EndReason::Fault(fault) => write!(f, "protocol fault: {fault}"),
            EndReason::Io(error) => write!(f, "connection error: {error}"),
            EndReason::Stopped => f.write_str("stopped"),
        }
    }
}

/// The wait before each connect retry (C80): 1 s, doubling up to 30 s.
#[derive(Clone, Debug)]
pub struct Backoff {
    next: Duration,
}

impl Backoff {
    pub const FIRST: Duration = Duration::from_secs(1);
    pub const MAX: Duration = Duration::from_secs(30);

    pub fn new() -> Backoff {
        Backoff { next: Self::FIRST }
    }

    /// The wait before the next attempt; each call doubles the one after.
    pub fn next_wait(&mut self) -> Duration {
        let wait = self.next;
        self.next = (self.next * 2).min(Self::MAX);
        wait
    }

    /// After a connection that worked.
    pub fn reset(&mut self) {
        self.next = Self::FIRST;
    }
}

impl Default for Backoff {
    fn default() -> Backoff {
        Backoff::new()
    }
}
