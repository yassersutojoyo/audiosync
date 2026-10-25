//! A PIN pairing on the AirPlay 2 path that the host asked for (P61, P153):
//! an [`Ap2Session`] that runs `GET /info`, `/pair-pin-start`, pair-setup M1
//! to M6 and a pair-verify on one connection, reported with the legacy
//! pairing's states and events so a host drives both the same way (P46).
//! A wrong PIN asks for the code again on that connection. Once paired, the
//! connection stays open for the speaker's session ([`PinPairing::into_session`]):
//! a receiver set to ask for a code every time takes no pair-verify on
//! another connection.

use std::collections::VecDeque;
use std::fmt;
use std::net::{IpAddr, Ipv4Addr};
use std::time::Instant;

use crate::error::{ErrorKind, SessionError};
use crate::homekit::{self, HomeKitCredentials};
use crate::ids::{Entropy, SessionIds};
use crate::pairing::{self, Output, PairingEvent, PairingState, PinError};
use crate::session::Connection;

use super::session::{Ap2Config, Ap2Event, Ap2Output, Ap2Session, Ap2State};

/// See the module documentation.
pub struct PinPairing {
    session: Ap2Session,
    state: PairingState,
    credentials: Option<HomeKitCredentials>,
    txt_flags: Option<u64>,
    /// Flags bit 3 in the `/info` answer: nothing to store (P182).
    every_time: bool,
    verified: bool,
    cancelled: bool,
    outputs: VecDeque<Output>,
}

impl fmt::Debug for PinPairing {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PinPairing")
            .field("state", &self.state)
            .field("verified", &self.verified)
            .finish_non_exhaustive()
    }
}

impl PinPairing {
    /// A pairing that asks for its first connect at `now`. Stored
    /// credentials and PTP in `config` are ignored: the pairing replaces the
    /// one, and no session SETUP needs the other.
    pub fn new(
        mut config: Ap2Config,
        ids: SessionIds,
        entropy: Box<dyn Entropy + Send>,
        now: Instant,
    ) -> PinPairing {
        config.pin_pairing = true;
        config.credentials = None;
        config.ptp = None;
        let txt_flags = config.txt_flags;
        let mut pairing = PinPairing {
            session: Ap2Session::new(config, ids, entropy, now),
            state: PairingState::Connecting,
            credentials: None,
            txt_flags,
            every_time: false,
            verified: false,
            cancelled: false,
            outputs: VecDeque::new(),
        };
        pairing.pump();
        pairing
    }

    pub fn state(&self) -> PairingState {
        self.state
    }

    /// Whether the session takes the host's PIN now: not while the next M1
    /// is held for a back-off (P150).
    pub fn waiting_for_pin(&self) -> bool {
        self.session.waiting_for_pin()
    }

    pub fn poll_output(&mut self) -> Option<Output> {
        self.pump();
        self.outputs.pop_front()
    }

    pub fn poll_timeout(&self) -> Option<Instant> {
        self.session.poll_timeout()
    }

    pub fn handle_timeout(&mut self, now: Instant) {
        self.session.handle_timeout(now);
        self.pump();
    }

    /// The connection is open. No UDP socket is needed: the pairing ends
    /// before a session SETUP would name one.
    pub fn handle_connected(&mut self, now: Instant) {
        self.state = PairingState::Starting;
        let connection = Connection {
            local: IpAddr::V4(Ipv4Addr::UNSPECIFIED),
            control_port: 0,
            timing_port: 0,
        };
        self.session.handle_connected(connection, now);
        self.pump();
    }

    pub fn handle_connect_failed(&mut self, now: Instant) {
        self.session.handle_connect_failed(now);
        self.pump();
    }

    pub fn handle_input(&mut self, bytes: &[u8], now: Instant) {
        self.session.handle_input(bytes, now);
        self.pump();
    }

    pub fn handle_connection_closed(&mut self, now: Instant) {
        self.session.handle_connection_closed(now);
        self.pump();
    }

    /// The PIN the user read off the receiver (P151: ASCII digits).
    pub fn submit_pin(&mut self, pin: &str, now: Instant) -> Result<(), PinError> {
        pairing::check_pin(pin)?;
        if self.state != PairingState::WaitingForPin || !self.session.waiting_for_pin() {
            return Err(PinError::NotWaiting);
        }
        self.state = PairingState::Proving;
        self.session.submit_pin(pin, now);
        self.pump();
        Ok(())
    }

    /// The session of a pairing that paired, its connection still open and
    /// framed, for [`Ap2Session::proceed`] (P153).
    pub fn into_session(self) -> Option<Ap2Session> {
        (self.verified && self.session.state() == Ap2State::Paired).then_some(self.session)
    }

    /// Stops the pairing; [`PairingEvent::Stopped`] follows.
    pub fn cancel(&mut self, now: Instant) {
        if self.state == PairingState::Closed {
            return;
        }
        self.cancelled = true;
        self.session.teardown(now);
        self.pump();
    }

    fn event(&mut self, event: PairingEvent) {
        self.outputs.push_back(Output::Event(event));
    }

    fn pump(&mut self) {
        while let Some(output) = self.session.poll_output() {
            match output {
                Ap2Output::Connect(address) => {
                    self.state = PairingState::Connecting;
                    self.outputs.push_back(Output::Connect(address));
                }
                Ap2Output::Send(bytes) => self.outputs.push_back(Output::Send(bytes)),
                Ap2Output::Close => self.outputs.push_back(Output::Close),
                Ap2Output::ConnectEvents(_) | Ap2Output::SendEvents(_) => {}
                Ap2Output::Event(event) => self.session_event(event),
            }
        }
    }

    fn session_event(&mut self, event: Ap2Event) {
        if self.state == PairingState::Closed {
            return;
        }
        match event {
            Ap2Event::PinRequested => {
                self.state = PairingState::WaitingForPin;
                self.event(PairingEvent::PinRequested);
            }
            // The receiver's code stays up, so the prompt comes back (P150).
            Ap2Event::WrongPin(why) => {
                self.state = PairingState::WaitingForPin;
                self.event(PairingEvent::PinTurnedDown(why));
                self.event(PairingEvent::PinRequested);
            }
            Ap2Event::PinStartAgain(why) => self.event(PairingEvent::PinTurnedDown(why)),
            Ap2Event::Credentials(credentials) => self.credentials = Some(credentials),
            Ap2Event::Info(info) => {
                let flags = homekit::status_flags(info.status_flags, self.txt_flags);
                self.every_time = homekit::asks_code_every_time(flags);
            }
            Ap2Event::Verified => {
                self.state = PairingState::Closed;
                let event = match self.credentials.clone() {
                    Some(_) if self.every_time => {
                        self.verified = true;
                        PairingEvent::PairedForSession
                    }
                    Some(credentials) => {
                        self.verified = true;
                        PairingEvent::HomeKitPaired(credentials)
                    }
                    None => PairingEvent::Failed {
                        error: unfinished(),
                        wrong_pin: false,
                    },
                };
                self.event(event);
            }
            Ap2Event::Failed(error) => {
                self.state = PairingState::Closed;
                let event = if self.cancelled {
                    PairingEvent::Stopped
                } else {
                    PairingEvent::Failed {
                        error,
                        wrong_pin: false,
                    }
                };
                self.event(event);
            }
            Ap2Event::Closed => {
                self.state = PairingState::Closed;
                let event = if self.cancelled {
                    PairingEvent::Stopped
                } else {
                    PairingEvent::Failed {
                        error: unfinished(),
                        wrong_pin: false,
                    }
                };
                self.event(event);
            }
            Ap2Event::Server(_)
            | Ap2Event::TimingChosen(_)
            | Ap2Event::Ready(_)
            | Ap2Event::Flushed
            | Ap2Event::Warning(_)
            | Ap2Event::Trace(_)
            | Ap2Event::VolumeReported(_) => {}
        }
    }
}

fn unfinished() -> SessionError {
    let mut error = SessionError::new(ErrorKind::PairingFailed);
    error.detail = Some("the connection ended before the pairing did");
    error
}
