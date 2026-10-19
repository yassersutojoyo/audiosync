//! Typed errors and warnings, one set per speaker (P133).
//!
//! An error ends the session it belongs to and nothing else: no other
//! session, and never the audio input. A warning leaves the session running.
//! Nothing here panics.

use std::fmt;

use crate::rtsp::Method;
use crate::sdp::Codec;

/// Why a receiver can't be driven (P139).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum UnsupportedReason {
    /// No encryption mode the sender supports: `et` lacks 0 and the device
    /// isn't an Apple TV with legacy pairing (P59, P1).
    Encryption,
    /// No codec the sender may use (P74).
    Codec,
    /// No UDP transport (P12, G14).
    Transport,
    /// The OPTIONS answer lacks ANNOUNCE, SETUP or RECORD (P27).
    Methods,
    /// A format other than 44 100 Hz, 16-bit stereo (P16, P72).
    Format,
    /// Only the AirPlay 2 path could serve the device, and it is off (P2).
    AirPlay2Only,
}

impl fmt::Display for UnsupportedReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            UnsupportedReason::Encryption => "encryption",
            UnsupportedReason::Codec => "codec",
            UnsupportedReason::Transport => "transport",
            UnsupportedReason::Methods => "methods",
            UnsupportedReason::Format => "format",
            UnsupportedReason::AirPlay2Only => "AirPlay 2 only",
        })
    }
}

/// The kinds of P133.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ErrorKind {
    /// The TCP connect was refused, unreachable or unanswered for 5 s (P134).
    Unreachable,
    /// A response didn't arrive within 10 s (P134).
    Timeout,
    /// A digest challenge arrived and the host gave no password (P39, P135).
    PasswordRequired,
    /// The receiver rejected the password (P38, P135).
    WrongPassword,
    /// The receiver needs a legacy pairing, or rejected the stored one
    /// (P46, P58).
    PairingRequired,
    /// A pairing step was answered with 470 or another non-200 status, except
    /// a Busy answer (P58, P136).
    PairingFailed,
    /// The receiver closed the connection while the sender waited for the PIN
    /// (P51, P136).
    PairingCancelled,
    /// 453 to ANNOUNCE, SETUP or RECORD, or 409 to any request before RECORD
    /// is answered, pairing and verification included (P137).
    Busy,
    /// Any other failure status to OPTIONS, ANNOUNCE, SETUP or RECORD (P138).
    Refused,
    /// The receiver can't be driven (P139).
    Unsupported(UnsupportedReason),
    /// A malformed or unexpected message (P140).
    ProtocolError,
    /// The connection died or the receiver went silent (P141).
    Lost,
}

impl ErrorKind {
    /// Kinds that need the user, so the host doesn't retry them on its own
    /// (P135, P142).
    pub fn needs_user(self) -> bool {
        matches!(
            self,
            ErrorKind::PasswordRequired
                | ErrorKind::WrongPassword
                | ErrorKind::PairingRequired
                | ErrorKind::PairingFailed
                | ErrorKind::Unsupported(_)
        )
    }
}

impl ErrorKind {
    /// What a host tells the user about this kind (P133).
    pub fn user_reason(self) -> UserReason {
        match self {
            ErrorKind::Unreachable | ErrorKind::Timeout => UserReason::NotResponding,
            ErrorKind::PasswordRequired => UserReason::PasswordRequired,
            ErrorKind::WrongPassword => UserReason::WrongPassword,
            ErrorKind::PairingRequired | ErrorKind::PairingFailed | ErrorKind::PairingCancelled => {
                UserReason::PairingRequired
            }
            ErrorKind::Busy => UserReason::Busy,
            ErrorKind::Refused => UserReason::Refused,
            ErrorKind::Unsupported(reason) => UserReason::Unsupported(reason),
            ErrorKind::ProtocolError => UserReason::Incompatible,
            ErrorKind::Lost => UserReason::ConnectionLost,
        }
    }
}

/// The few kinds of message a host shows for an error, grouped from the
/// kinds of P133. Whether the user must act is
/// [`ErrorKind::needs_user`]; everything else is retried (P142).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum UserReason {
    /// The speaker didn't answer: no connection, or no response in time
    /// (Unreachable, Timeout).
    NotResponding,
    /// The speaker needs a password and none was given.
    PasswordRequired,
    /// The speaker rejected the password.
    WrongPassword,
    /// The speaker needs pairing, or rejected or cancelled it.
    PairingRequired,
    /// Another sender seems to be playing to it.
    Busy,
    /// The speaker turned the session down.
    Refused,
    /// The sender can't drive this speaker, for this reason.
    Unsupported(UnsupportedReason),
    /// The speaker answered in a way the sender doesn't understand.
    Incompatible,
    /// The connection died or the speaker went silent while playing.
    ConnectionLost,
}

/// An error that ended a session, with the request and status that caused it
/// when there is one (P133).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionError {
    pub kind: ErrorKind,
    /// The RTSP method of the request it answers, if any.
    pub method: Option<Method>,
    /// The RTSP status code, if a response caused it.
    pub status: Option<u16>,
    /// The codec to retry with on a new connection: set when a receiver
    /// refused an ANNOUNCE offering PCM and also lists ALAC (P74).
    pub fallback: Option<Codec>,
    /// A short diagnostic for protocol errors. Never contains the password.
    pub detail: Option<&'static str>,
}

impl SessionError {
    pub fn new(kind: ErrorKind) -> SessionError {
        SessionError {
            kind,
            method: None,
            status: None,
            fallback: None,
            detail: None,
        }
    }
}

impl fmt::Display for SessionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}", self.kind)?;
        if let Some(method) = self.method {
            write!(f, " ({}", method.as_str())?;
            if let Some(status) = self.status {
                write!(f, " {status}")?;
            }
            f.write_str(")")?;
        }
        if let Some(detail) = self.detail {
            write!(f, ": {detail}")?;
        }
        Ok(())
    }
}

impl std::error::Error for SessionError {}

/// Something the host may want to show; the session goes on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Warning {
    /// `Audio-Jack-Status: disconnected` (P24).
    JackDisconnected,
    /// `POST /auth-setup` failed; the session continues (P60).
    AuthSetupFailed { status: u16 },
    /// A failure status to SET_PARAMETER, GET_PARAMETER or FLUSH, or to the
    /// keep-alive OPTIONS of a running session other than a 401 (P131,
    /// P138); a failed metadata request lands here too (P127).
    RequestFailed { method: Method, status: u16 },
    /// The receiver's signature in its first pair-verify answer didn't
    /// check out against the Ed25519 key it gave at pairing; the
    /// verification goes on (P56).
    ReceiverSignatureInvalid,
}
