//! The PIN pairing the host asks for on the AirPlay 2 path (P61, P149,
//! P153), against the sans-I/O fake receiver of `common::ap2`: the code
//! shown after `/pair-pin-start`, pair-setup M1 to M6 and a pair-verify on
//! one connection that the session then goes on with, a wrong PIN asked
//! for again, and how a 470, a cancel and a close end it.

mod common;

use std::time::{Duration, Instant};

use audiosync_airplay::ap2::{Ap2Event, PinPairing};
use audiosync_airplay::error::ErrorKind;
use audiosync_airplay::homekit::HomeKitCredentials;
use audiosync_airplay::pairing::{Output, PairingEvent, PairingState, PinError};
use audiosync_airplay::rtsp::Method;

use common::ap2::{Ap2Rig, FakeAp2, TestEntropy, ap2_config, dict, int, plist};
use common::hap::Answer;
use common::{SENDER, connection, ids};

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

/// A receiver that shows `PIN` and asks for a pairing the first time only
/// (flags bit 9, P61), as one TV does with 0x244.
fn pin_receiver() -> FakeAp2 {
    let mut fake = FakeAp2::new();
    fake.hap.code = PIN.to_owned();
    fake.info_body = plist(dict(&[("statusFlags", int(0x244))]));
    fake
}

/// A receiver that shows `PIN` and asks for a code every time (flags bit
/// 3, 0x4c as the same TV sets it): a pairing lasts as long as its
/// connection.
fn every_time_receiver() -> FakeAp2 {
    let mut fake = pin_receiver();
    fake.info_body = plist(dict(&[("statusFlags", int(0x4c))]));
    fake.hap.pairing_per_connection = true;
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
    // The same connection throughout, left open after the pair-verify for
    // the session, with no session SETUP yet.
    assert_eq!(rig.connects, 1);
    assert_eq!(rig.closes, 0);
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

// covers: P153, P182
#[test]
fn the_session_goes_on_over_the_pairing_connection() {
    let mut rig = PinRig::new(every_time_receiver());
    assert_eq!(rig.submit(PIN), Ok(()));
    // Nothing to store: the receiver turns every later pair-verify down.
    assert_eq!(rig.credentials(), None);
    assert_eq!(
        rig.events,
        [PairingEvent::PinRequested, PairingEvent::PairedForSession]
    );
    let PinRig {
        pairing, fake, now, ..
    } = rig;
    let session = pairing.into_session().expect("the connection waits");
    let mut ap2 = Ap2Rig::with_session(session, fake, now);
    ap2.session.proceed(ap2_config(), connection(SENDER), now);
    ap2.pump();
    assert!(ap2.ready(), "{:?} {:?}", ap2.keys(), ap2.events);
    assert!(ap2.connects.is_empty(), "no new connection");
    assert_eq!(ap2.fake.connections, 1);
    assert_eq!(ap2.keys().first(), Some(&"SETUP session"));
    assert!(!ap2.keys().contains(&"POST /pair-verify"));
    let (setup, framed) = ap2
        .fake
        .requests
        .iter()
        .find(|(request, _)| request.method == Method::Setup)
        .expect("SETUP");
    assert!(framed, "{}", setup.uri);
}

// covers: P61
#[test]
fn stored_credentials_never_verify_with_a_receiver_that_asks_every_time() {
    let mut rig = PinRig::new(pin_receiver());
    assert_eq!(rig.submit(PIN), Ok(()));
    let credentials = rig.credentials().expect("paired");
    let mut fake = every_time_receiver();
    fake.hap.paired = rig.fake.hap.paired.clone();
    let mut config = ap2_config();
    config.credentials = Some(credentials);
    let session = Ap2Rig::new(config, fake);
    let keys = session.keys();
    assert!(!keys.contains(&"POST /pair-verify"), "{keys:?}");
    assert!(!keys.contains(&"POST /pair-pin-start"), "{keys:?}");
    let failed = session.events.iter().find_map(|event| match event {
        Ap2Event::Failed(error) => Some(error.kind),
        _ => None,
    });
    assert_eq!(failed, Some(ErrorKind::PairingRequired));
}

// covers: P58, P150
#[test]
fn a_wrong_pin_is_asked_for_again_on_the_same_connection() {
    let mut rig = PinRig::new(pin_receiver());
    assert_eq!(rig.submit("1111"), Ok(()));
    assert_eq!(rig.failure(), None);
    assert!(
        matches!(
            rig.events.as_slice(),
            [
                PairingEvent::PinRequested,
                PairingEvent::PinTurnedDown(_),
                PairingEvent::PinRequested
            ]
        ),
        "{:?}",
        rig.events
    );
    assert_eq!(rig.pairing.state(), PairingState::WaitingForPin);
    // No M5 after the refusal, and nothing more until the next PIN (P151).
    assert_eq!(
        rig.fake.keys(),
        [
            "GET /info",
            "POST /pair-pin-start",
            "POST /pair-setup",
            "POST /pair-setup",
        ]
    );
    // The right PIN pairs from M1, with the code the receiver still shows.
    assert_eq!(rig.submit(PIN), Ok(()));
    assert!(rig.credentials().is_some(), "{:?}", rig.events);
    let keys = rig.fake.keys();
    let starts = keys.iter().filter(|k| *k == "POST /pair-pin-start").count();
    assert_eq!(starts, 1, "{keys:?}");
    assert_eq!(rig.connects, 1);
    assert_eq!(rig.closes, 0);

    // Wrong twice in a row: the receiver is asked for a new code, then the
    // prompt comes back.
    let mut rig = PinRig::new(pin_receiver());
    assert_eq!(rig.submit("1111"), Ok(()));
    assert_eq!(rig.submit("2222"), Ok(()));
    let keys = rig.fake.keys();
    let starts = keys.iter().filter(|k| *k == "POST /pair-pin-start").count();
    assert_eq!(starts, 2, "{keys:?}");
    assert_eq!(
        keys.last().map(String::as_str),
        Some("POST /pair-pin-start")
    );
    assert_eq!(rig.pairing.state(), PairingState::WaitingForPin);
    assert_eq!(rig.failure(), None);
    assert_eq!(rig.submit(PIN), Ok(()));
    assert!(rig.credentials().is_some(), "{:?}", rig.events);
    assert_eq!(rig.connects, 1);

    // A back-off answer asks again and holds M1 back for its wait.
    let mut fake = pin_receiver();
    fake.hap.back_off = Some(5);
    fake.hap
        .answers
        .push((("/pair-setup", 1), Answer::Error(3)));
    let mut rig = PinRig::new(fake);
    assert_eq!(rig.submit(PIN), Ok(()));
    assert_eq!(rig.failure(), None);
    assert_eq!(rig.pairing.state(), PairingState::WaitingForPin);
    rig.fake.hap.answers.clear();
    rig.fake.requests.clear();
    assert!(rig.pairing.waiting_for_pin());
    assert_eq!(rig.submit(PIN), Ok(()));
    assert!(!rig.pairing.waiting_for_pin(), "held, it takes no PIN");
    assert_eq!(rig.submit(PIN), Err(PinError::NotWaiting));
    rig.advance(Duration::from_secs(4));
    assert!(rig.fake.requests.is_empty(), "{:?}", rig.fake.keys());
    rig.advance(Duration::from_secs(2));
    assert!(rig.credentials().is_some(), "{:?}", rig.fake.keys());
    assert!(!rig.fake.keys().contains(&"POST /pair-pin-start".to_owned()));

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

impl PinRig {
    /// The reasons the pairing gave for asking for the PIN again.
    fn turned_down(&self) -> Vec<String> {
        self.events
            .iter()
            .filter_map(|event| match event {
                PairingEvent::PinTurnedDown(why) => Some(why.clone()),
                _ => None,
            })
            .collect()
    }
}

// covers: P150
#[test]
fn every_pin_asked_for_again_says_why() {
    let mut rig = PinRig::new(pin_receiver());
    assert_eq!(rig.submit("1111"), Ok(()));
    assert_eq!(
        rig.turned_down(),
        [
            "the receiver turned the PIN down: error 2 (authentication) in answer to M3; the PIN is asked for again"
        ]
    );
    assert_eq!(rig.submit("2222"), Ok(()));
    assert_eq!(
        rig.turned_down().last().map(String::as_str),
        Some(
            "the receiver turned the PIN down again: error 2 (authentication) in answer to M3; /pair-pin-start goes out again"
        )
    );
    let reasons = rig.turned_down().join(" ");
    assert!(!reasons.contains("1111") && !reasons.contains("2222"));
    assert_eq!(rig.events.last(), Some(&PairingEvent::PinRequested));

    let mut fake = pin_receiver();
    fake.hap.back_off = Some(5);
    fake.hap
        .answers
        .push((("/pair-setup", 1), Answer::Error(3)));
    let mut rig = PinRig::new(fake);
    assert_eq!(rig.submit(PIN), Ok(()));
    assert_eq!(
        rig.turned_down(),
        [
            "the receiver asked for a wait: error 3 (back-off, 5 s) in answer to M1; the PIN is asked for again"
        ]
    );

    // An M4 whose proof doesn't verify is a refusal too.
    let mut fake = pin_receiver();
    fake.hap.wrong_m4_proof = true;
    let mut rig = PinRig::new(fake);
    assert_eq!(rig.submit(PIN), Ok(()));
    assert_eq!(
        rig.turned_down(),
        [
            "the receiver turned the PIN down: an M4 with a proof that didn't check; the PIN is asked for again"
        ]
    );

    // A receiver that reads B padded turns down a B that starts with a zero byte.
    let mut fake = pin_receiver();
    fake.hap.hash_padded = true;
    fake.hap.zero_led_b = 1;
    let mut rig = PinRig::new(fake);
    assert_eq!(rig.submit(PIN), Ok(()));
    assert_eq!(
        rig.turned_down(),
        [
            "the receiver turned the PIN down: error 2 (authentication) in answer to M3, with B or S shorter than 384 bytes (P151); the PIN is asked for again"
        ]
    );

    // A right PIN that pairs gives no reason.
    let mut rig = PinRig::new(pin_receiver());
    assert_eq!(rig.submit(PIN), Ok(()));
    assert!(rig.credentials().is_some());
    assert!(rig.turned_down().is_empty(), "{:?}", rig.events);
}

// covers: P149
#[test]
fn a_pair_verify_that_fails_after_m6_pairs_nothing() {
    let mut fake = pin_receiver();
    fake.hap.wrong_m2_signature = true;
    let mut rig = PinRig::new(fake);
    assert_eq!(rig.submit(PIN), Ok(()));
    let keys = rig.fake.keys();
    assert_eq!(
        keys.iter().filter(|key| *key == "POST /pair-setup").count(),
        3,
        "{keys:?}"
    );
    assert!(keys.contains(&"POST /pair-verify".to_owned()), "{keys:?}");
    let (_, wrong_pin) = rig.failure().expect("the pairing fails");
    assert!(!wrong_pin);
    assert_eq!(rig.credentials(), None, "no credentials without a verify");
    assert_eq!(rig.pairing.state(), PairingState::Closed);
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
