//! The PIN pairing the host asks for on the AirPlay 2 path (P61, P149,
//! P153), against the sans-I/O fake receiver of `common::ap2`: the code
//! shown after `/pair-pin-start`, pair-setup M1 to M6 and a pair-verify on
//! one connection, and how a wrong PIN, a 470, a cancel and a close end it.

mod common;

use std::time::{Duration, Instant};

use audiosync_airplay::ap2::PinPairing;
use audiosync_airplay::error::ErrorKind;
use audiosync_airplay::homekit::HomeKitCredentials;
use audiosync_airplay::pairing::{Output, PairingEvent, PairingState, PinError};
use audiosync_airplay::rtsp::Method;

use common::ap2::{Ap2Rig, FakeAp2, TestEntropy, ap2_config, dict, int, plist};
use common::hap::Answer;
use common::ids;

const PIN: &str = "4821";

/// A [`PinPairing`] wired to a [`FakeAp2`].
struct PinRig {
    pairing: PinPairing,
    fake: FakeAp2,
    now: Instant,
    events: Vec<PairingEvent>,
    connects: usize,
    closes: usize,
}

impl PinRig {
    fn new(fake: FakeAp2) -> PinRig {
        let now = Instant::now();
        let pairing = PinPairing::new(
            ap2_config(),
            ids(),
            Box::new(TestEntropy(0x2545_F491_4F6C_DD1D)),
            now,
        );
        let mut rig = PinRig {
            pairing,
            fake,
            now,
            events: Vec::new(),
            connects: 0,
            closes: 0,
        };
        rig.pump();
        rig
    }

    fn pump(&mut self) {
        while let Some(output) = self.pairing.poll_output() {
            match output {
                Output::Connect(_) => {
                    self.connects += 1;
                    self.fake.connect();
                    self.pairing.handle_connected(self.now);
                }
                Output::Send(bytes) => {
                    let answer = self.fake.feed(&bytes);
                    if !answer.is_empty() {
                        self.pairing.handle_input(&answer, self.now);
                    }
                }
                Output::Event(event) => self.events.push(event),
                Output::Close => self.closes += 1,
            }
        }
    }

    fn submit(&mut self, pin: &str) -> Result<(), PinError> {
        let result = self.pairing.submit_pin(pin, self.now);
        self.pump();
        result
    }

    fn advance(&mut self, by: Duration) {
        let end = self.now + by;
        while self.now < end {
            self.now = (self.now + Duration::from_millis(500)).min(end);
            if self.pairing.poll_timeout().is_some_and(|at| at <= self.now) {
                self.pairing.handle_timeout(self.now);
            }
            self.pump();
        }
    }

    fn failure(&self) -> Option<(ErrorKind, bool)> {
        self.events.iter().find_map(|event| match event {
            PairingEvent::Failed { error, wrong_pin } => Some((error.kind, *wrong_pin)),
            _ => None,
        })
    }

    fn credentials(&self) -> Option<HomeKitCredentials> {
        self.events.iter().find_map(|event| match event {
            PairingEvent::HomeKitPaired(credentials) => Some(credentials.clone()),
            _ => None,
        })
    }
}

/// A receiver that shows `PIN` and sets the PIN flag (P61).
fn pin_receiver() -> FakeAp2 {
    let mut fake = FakeAp2::new();
    fake.hap.code = PIN.to_owned();
    fake.info_body = plist(dict(&[("statusFlags", int(0x8))]));
    fake
}

// covers: P149, P153
#[test]
fn pin_pairing_runs_on_one_connection() {
    let mut rig = PinRig::new(pin_receiver());
    assert_eq!(rig.fake.keys(), ["GET /info", "POST /pair-pin-start"]);
    let (start, framed) = &rig.fake.requests[1];
    assert!(!framed);
    assert_eq!(start.method, Method::Post);
    assert!(start.body.is_empty(), "no body on /pair-pin-start");
    assert_eq!(start.headers.get("X-Apple-HKP"), Some("3"));
    assert_eq!(
        start.headers.get("Content-Type"),
        Some("application/octet-stream")
    );
    assert_eq!(rig.events, [PairingEvent::PinRequested]);
    assert_eq!(rig.pairing.state(), PairingState::WaitingForPin);

    // The code stays up as long as the user takes (P51).
    rig.advance(Duration::from_secs(120));
    assert_eq!(rig.events, [PairingEvent::PinRequested]);
    assert_eq!(rig.fake.requests.len(), 2);

    assert_eq!(rig.submit("12a4"), Err(PinError::NotDigits));
    assert_eq!(rig.submit(PIN), Ok(()));
    assert_eq!(
        rig.fake.keys(),
        [
            "GET /info",
            "POST /pair-pin-start",
            "POST /pair-setup",
            "POST /pair-setup",
            "POST /pair-setup",
            "POST /pair-verify",
            "POST /pair-verify",
        ]
    );
    for (request, framed) in &rig.fake.requests[1..] {
        assert!(!framed, "{} goes in plain text", request.uri);
        assert_eq!(
            request.headers.get("X-Apple-HKP"),
            Some("3"),
            "{}",
            request.uri
        );
        for name in ["CSeq", "User-Agent", "Client-Instance", "DACP-ID"] {
            assert!(request.headers.contains(name), "{name} on {}", request.uri);
        }
    }
    // The same connection throughout, closed after the pair-verify with no
    // session SETUP.
    assert_eq!(rig.connects, 1);
    assert_eq!(rig.closes, 1);
    assert_eq!(rig.pairing.state(), PairingState::Closed);
    let credentials = rig.credentials().expect("paired");
    assert_eq!(rig.events.len(), 2, "{:?}", rig.events);
    assert_eq!(rig.fake.hap.paired.len(), 1);
    assert_eq!(rig.submit(PIN), Err(PinError::NotWaiting));

    // The credentials verify a later session, with no pair-setup (P61).
    let mut fake = pin_receiver();
    fake.hap.paired = rig.fake.hap.paired.clone();
    let mut config = ap2_config();
    config.credentials = Some(credentials);
    let session = Ap2Rig::new(config, fake);
    assert!(session.ready(), "{:?} {:?}", session.keys(), session.events);
    let keys = session.keys();
    assert!(keys.contains(&"POST /pair-verify"), "{keys:?}");
    assert!(!keys.contains(&"POST /pair-setup"), "{keys:?}");
    assert!(!keys.contains(&"POST /pair-pin-start"), "{keys:?}");
}

// covers: P58, P150
#[test]
fn a_wrong_pin_ends_the_pairing_as_a_wrong_pin() {
    let mut rig = PinRig::new(pin_receiver());
    assert_eq!(rig.submit("1111"), Ok(()));
    assert_eq!(rig.failure(), Some((ErrorKind::PairingFailed, true)));
    // No M5 after the refusal, and no retry by itself (P151).
    assert_eq!(
        rig.fake.keys(),
        [
            "GET /info",
            "POST /pair-pin-start",
            "POST /pair-setup",
            "POST /pair-setup",
        ]
    );
    assert_eq!(rig.connects, 1);
    assert_eq!(rig.credentials(), None);
    assert_eq!(rig.pairing.state(), PairingState::Closed);

    // A 470 at any step is PairingFailed, not a wrong PIN (P58).
    let mut fake = pin_receiver();
    fake.hap
        .answers
        .push((("/pair-pin-start", 0), Answer::Status(470)));
    let rig = PinRig::new(fake);
    assert_eq!(rig.failure(), Some((ErrorKind::PairingFailed, false)));
    let mut fake = pin_receiver();
    fake.hap
        .answers
        .push((("/pair-setup", 3), Answer::Status(470)));
    let mut rig = PinRig::new(fake);
    assert_eq!(rig.submit(PIN), Ok(()));
    assert_eq!(rig.failure(), Some((ErrorKind::PairingFailed, false)));

    // Busy is Busy (P137).
    let mut fake = pin_receiver();
    fake.hap
        .answers
        .push((("/pair-setup", 1), Answer::Error(7)));
    let mut rig = PinRig::new(fake);
    assert_eq!(rig.submit(PIN), Ok(()));
    assert_eq!(rig.failure(), Some((ErrorKind::Busy, false)));
}

// covers: P136
#[test]
fn a_cancel_or_a_close_while_waiting_ends_the_pairing() {
    let mut rig = PinRig::new(pin_receiver());
    rig.pairing.cancel(rig.now);
    rig.pump();
    assert_eq!(
        rig.events,
        [PairingEvent::PinRequested, PairingEvent::Stopped]
    );
    assert_eq!(rig.closes, 1);
    assert_eq!(rig.submit(PIN), Err(PinError::NotWaiting));

    let mut rig = PinRig::new(pin_receiver());
    rig.pairing.handle_connection_closed(rig.now);
    rig.pump();
    assert_eq!(rig.failure(), Some((ErrorKind::PairingCancelled, false)));
    assert_eq!(rig.pairing.state(), PairingState::Closed);
}
