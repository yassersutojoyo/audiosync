//! The Apple TV legacy pairing and pair-verify against the fake Apple TV
//! (section 4 of docs/AIRPLAY_SPEC.md, P46 to P58, P60, P136): message
//! order on one connection, the PIN wait, the credentials handed to the
//! host, a wrong PIN, failures at each step, the verification on every
//! later connection and a credential the Apple TV no longer accepts.

mod common;

use std::io::Cursor;
use std::time::Duration;

use audiosync_airplay::error::{ErrorKind, Warning};
use audiosync_airplay::pairing::{
    self, Credentials, LegacyVerifier, PairingEvent, PairingState, PinError,
};
use audiosync_airplay::rtsp::{Method, Request};
use audiosync_airplay::session::{SessionEvent, State, Verifier};
use common::atv::{CLIENT, FakeAppleTv, PairLink, TestEntropy, public_of};
use common::{FakeReceiver, Link, key};
use plist::Value;

const PIN: &str = "4821";

fn apple_tv_receiver(atv: &FakeAppleTv) -> FakeReceiver {
    let mut receiver = FakeReceiver::new();
    receiver.apple_tv = Some(atv.clone());
    receiver
}

fn plist(body: &[u8]) -> plist::Dictionary {
    Value::from_reader(Cursor::new(body))
        .expect("a plist")
        .into_dictionary()
        .expect("a dictionary")
}

/// Pairs with the fake Apple TV and returns the credentials.
fn pair(atv: &FakeAppleTv) -> Credentials {
    let mut link = PairLink::new(apple_tv_receiver(atv));
    link.connect();
    link.submit(PIN, &mut TestEntropy::new());
    link.paired()
        .unwrap_or_else(|| panic!("paired: {:?}", link.events))
}

fn session_kinds(link: &Link) -> Vec<ErrorKind> {
    link.events
        .iter()
        .filter_map(|event| match event {
            SessionEvent::Failed(error) => Some(error.kind),
            _ => None,
        })
        .collect()
}

// covers: P47, P48, P49, P51, P54
#[test]
fn pairing_runs_on_one_connection_waits_for_the_pin_and_hands_over_credentials() {
    let atv = FakeAppleTv::new(PIN);
    let mut link = PairLink::new(apple_tv_receiver(&atv));
    link.connect();

    // P48, P49: pin-start without a body, then the client ID in a plist.
    let keys: Vec<String> = link.receiver.requests.iter().map(key).collect();
    assert_eq!(keys, ["POST /pair-pin-start", "POST /pair-setup-pin"]);
    let start: &Request = &link.receiver.requests[0];
    assert_eq!(start.method, Method::Post);
    assert!(start.body.is_empty());
    assert!(start.headers.get("Content-Type").is_none());
    for request in &link.receiver.requests {
        for header in [
            "CSeq",
            "User-Agent",
            "Client-Instance",
            "DACP-ID",
            "Active-Remote",
        ] {
            assert!(request.headers.get(header).is_some(), "{header}");
        }
    }
    let first = &link.receiver.requests[1];
    assert_eq!(
        first.headers.get("Content-Type"),
        Some("application/x-apple-binary-plist")
    );
    assert!(first.body.starts_with(b"bplist00"), "a binary plist (P47)");
    let body = plist(&first.body);
    assert_eq!(body.get("method").and_then(Value::as_string), Some("pin"));
    let client_id = format!("{CLIENT:016X}");
    assert_eq!(client_id.len(), 16);
    assert_eq!(
        body.get("user").and_then(Value::as_string),
        Some(client_id.as_str())
    );
    assert_eq!(link.pairing.client_id(), client_id);

    // P51: the PIN is awaited without a timeout of the sender's own.
    assert_eq!(link.events, [PairingEvent::PinRequested]);
    assert_eq!(link.pairing.state(), PairingState::WaitingForPin);
    assert_eq!(link.pairing.poll_timeout(), None);
    link.advance(Duration::from_secs(3600));
    assert_eq!(link.events, [PairingEvent::PinRequested]);
    assert!(!link.closed);

    let mut entropy = TestEntropy::new();
    link.submit(PIN, &mut entropy);
    let keys: Vec<String> = link.receiver.requests.iter().map(key).collect();
    assert_eq!(
        keys,
        [
            "POST /pair-pin-start",
            "POST /pair-setup-pin",
            "POST /pair-setup-pin",
            "POST /pair-setup-pin"
        ]
    );
    // P47: all on the one connection, which closes once paired.
    assert_eq!(link.connects.len(), 1);
    assert!(link.closed);
    assert_eq!(link.pairing.state(), PairingState::Closed);

    // P54: the client ID and the secret a, which the Apple TV now knows by
    // its Ed25519 public key.
    let credentials = link.paired().expect("paired");
    assert_eq!(credentials.client_id(), client_id);
    assert_eq!(atv.paired(), [credentials.public_key()]);
    let proof = plist(&link.receiver.requests[2].body);
    let sent_a = proof.get("pk").and_then(Value::as_data).expect("pk");
    assert_eq!(sent_a, public_of(credentials.secret()).as_slice());

    // The blob a host stores round-trips, and nothing prints the secret.
    let blob = credentials.to_bytes();
    assert_eq!(blob.len(), 49);
    assert_eq!(Credentials::from_bytes(&blob), Ok(credentials.clone()));
    assert!(Credentials::from_bytes(&blob[..48]).is_err());
    let shown = format!("{credentials:?}");
    let secret_hex: String = credentials.secret()[..4]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    assert!(!shown.contains(&secret_hex), "{shown}");
    assert!(shown.contains(&client_id));
}

// covers: P50, P52, P53
#[test]
fn the_exchange_carries_a_padded_a_m1_and_the_sealed_key_and_checks_m2() {
    let atv = FakeAppleTv::new(PIN);
    let credentials = pair(&atv);
    let bodies = atv.bodies("/pair-setup-pin");
    assert_eq!(bodies.len(), 3);
    // P52: {pk: A (256 bytes), proof: M1 (20 bytes)}; the fake Apple TV
    // checked M1 with its own arithmetic before answering.
    let proof = plist(&bodies[1]);
    assert_eq!(
        proof.get("pk").and_then(Value::as_data).map(<[u8]>::len),
        Some(256)
    );
    assert_eq!(
        proof.get("proof").and_then(Value::as_data).map(<[u8]>::len),
        Some(20)
    );
    // P53: {epk: 32 bytes, authTag: 16 bytes}, opened by the fake with its
    // own key and nonce to the client's public key.
    let sealed = plist(&bodies[2]);
    assert_eq!(
        sealed.get("epk").and_then(Value::as_data).map(<[u8]>::len),
        Some(32)
    );
    assert_eq!(
        sealed
            .get("authTag")
            .and_then(Value::as_data)
            .map(<[u8]>::len),
        Some(16)
    );
    assert_eq!(atv.paired(), [credentials.public_key()]);

    // A wrong M2 ends the pairing before the key goes out (P52).
    let atv = FakeAppleTv::new(PIN);
    atv.send_wrong_server_proof();
    let mut link = PairLink::new(apple_tv_receiver(&atv));
    link.connect();
    link.submit(PIN, &mut TestEntropy::new());
    let (error, wrong_pin) = link.failure().expect("failed");
    assert_eq!(error.kind, ErrorKind::PairingFailed);
    assert!(!wrong_pin);
    assert_eq!(atv.bodies("/pair-setup-pin").len(), 2);
    assert!(atv.paired().is_empty());
    assert!(link.closed);
}

// covers: P50
#[test]
fn a_is_drawn_again_when_s_would_start_with_a_zero_byte() {
    // An a whose premaster secret S starts with a zero byte, found the
    // receiver's way, then one whose S doesn't (P50's required redraw).
    let atv = FakeAppleTv::new(PIN);
    let user = format!("{CLIENT:016X}");
    let mut entropy = TestEntropy::new();
    let mut zero_led = None;
    for _ in 0..4096 {
        let mut a = [0u8; 32];
        audiosync_airplay::ids::Entropy::fill(&mut entropy, &mut a).expect("bytes");
        if atv.premaster_of(&a, &user)[0] == 0 && public_of(&a)[0] != 0 {
            zero_led = Some(a);
            break;
        }
    }
    let zero_led = zero_led.expect("an a with a zero-led S within 4096 draws");
    let good = [0x6Bu8; 32];
    assert_ne!(atv.premaster_of(&good, &user)[0], 0);

    let mut link = PairLink::new(apple_tv_receiver(&atv));
    link.connect();
    let mut entropy = TestEntropy::scripted(vec![zero_led.to_vec(), good.to_vec()]);
    link.submit(PIN, &mut entropy);
    assert_eq!(entropy.fills, 2);
    let credentials = link.paired().expect("paired");
    assert_eq!(credentials.secret(), &good);
    let proof = plist(&atv.bodies("/pair-setup-pin")[1]);
    assert_eq!(
        proof.get("pk").and_then(Value::as_data),
        Some(public_of(&good).as_slice())
    );
}

// Part of P50: the same redraw when A would start with a zero byte, so
// PAD(A) and A are the same bytes in M1 and M2.
#[test]
fn a_is_drawn_again_when_a_would_start_with_a_zero_byte() {
    // An a whose A = g^a mod N starts with a zero byte, found the
    // receiver's way, then one that doesn't.
    let mut entropy = TestEntropy::new();
    let mut zero_led = None;
    for _ in 0..4096 {
        let mut a = [0u8; 32];
        audiosync_airplay::ids::Entropy::fill(&mut entropy, &mut a).expect("bytes");
        if public_of(&a)[0] == 0 {
            zero_led = Some(a);
            break;
        }
    }
    let zero_led = zero_led.expect("an a with a zero-led A within 4096 draws");
    let good = [0x6Bu8; 32];
    assert_ne!(public_of(&good)[0], 0);

    let atv = FakeAppleTv::new(PIN);
    let mut link = PairLink::new(apple_tv_receiver(&atv));
    link.connect();
    let mut entropy = TestEntropy::scripted(vec![zero_led.to_vec(), good.to_vec()]);
    link.submit(PIN, &mut entropy);
    assert_eq!(entropy.fills, 2);
    let credentials = link.paired().expect("paired");
    assert_eq!(credentials.secret(), &good);
    let proof = plist(&atv.bodies("/pair-setup-pin")[1]);
    assert_eq!(
        proof.get("pk").and_then(Value::as_data),
        Some(public_of(&good).as_slice())
    );
}

// covers: P58, P136
#[test]
fn a_wrong_pin_or_a_470_at_any_step_ends_with_pairing_failed() {
    // A wrong PIN: 470 to the proof (P52).
    let atv = FakeAppleTv::new(PIN);
    let mut link = PairLink::new(apple_tv_receiver(&atv));
    link.connect();
    link.submit("0000", &mut TestEntropy::new());
    let (error, wrong_pin) = link.failure().expect("failed");
    assert_eq!(error.kind, ErrorKind::PairingFailed);
    assert_eq!(error.status, Some(470));
    assert_eq!(error.method, Some(Method::Post));
    assert!(wrong_pin);
    assert!(link.closed);
    assert!(atv.paired().is_empty());

    // 470 to pin-start, to the first and to the last pair-setup step.
    let steps: [(Option<&str>, Vec<Option<u16>>); 3] = [
        (Some("POST /pair-pin-start"), vec![]),
        (None, vec![Some(470)]),
        (None, vec![None, None, Some(470)]),
    ];
    for (index, (status_key, setup)) in steps.into_iter().enumerate() {
        let atv = FakeAppleTv::new(PIN);
        let mut receiver = apple_tv_receiver(&atv);
        if let Some(status_key) = status_key {
            receiver.status.insert(status_key.to_owned(), 470);
        }
        for status in setup {
            atv.queue_setup_status(status);
        }
        let mut link = PairLink::new(receiver);
        link.connect();
        if link.pairing.state() == PairingState::WaitingForPin {
            link.submit(PIN, &mut TestEntropy::new());
        }
        let (error, wrong_pin) = link.failure().unwrap_or_else(|| panic!("step {index}"));
        assert_eq!(error.kind, ErrorKind::PairingFailed, "step {index}");
        assert_eq!(error.status, Some(470), "step {index}");
        assert!(!wrong_pin, "step {index}");
        assert!(link.paired().is_none(), "step {index}");
        assert!(link.closed, "step {index}");
    }

    // P136: the Apple TV closing the connection while the PIN is awaited.
    let atv = FakeAppleTv::new(PIN);
    let mut link = PairLink::new(apple_tv_receiver(&atv));
    link.connect();
    assert_eq!(link.pairing.state(), PairingState::WaitingForPin);
    let now = link.now;
    link.pairing.handle_connection_closed(now);
    link.pump();
    let (error, _) = link.failure().expect("cancelled");
    assert_eq!(error.kind, ErrorKind::PairingCancelled);
    assert_eq!(
        link.pairing
            .submit_pin(PIN, &mut TestEntropy::new(), link.now),
        Err(PinError::NotWaiting)
    );
}

#[test]
fn pins_must_be_digits_and_the_host_can_cancel() {
    let atv = FakeAppleTv::new(PIN);
    let mut link = PairLink::new(apple_tv_receiver(&atv));
    link.connect();
    let now = link.now;
    for pin in ["", "12a4", " 1234", "１２３４"] {
        assert_eq!(
            link.pairing.submit_pin(pin, &mut TestEntropy::new(), now),
            Err(PinError::NotDigits),
            "{pin:?}"
        );
    }
    assert_eq!(link.pairing.state(), PairingState::WaitingForPin);
    link.pairing.cancel();
    link.pump();
    assert!(link.closed);
    assert_eq!(link.events.last(), Some(&PairingEvent::Stopped));
    assert_eq!(atv.bodies("/pair-setup-pin").len(), 1);
}

#[test]
fn a_silent_apple_tv_times_out_but_the_pin_wait_does_not() {
    let atv = FakeAppleTv::new(PIN);
    let mut receiver = apple_tv_receiver(&atv);
    receiver.silent = Some("POST /pair-pin-start".to_owned());
    let mut link = PairLink::new(receiver);
    link.connect();
    link.advance(Duration::from_secs(9));
    assert!(link.failure().is_none());
    link.advance(Duration::from_secs(1));
    let (error, _) = link.failure().expect("timed out");
    assert_eq!(error.kind, ErrorKind::Timeout);
}

// covers: P55, P56
#[test]
fn stored_credentials_verify_before_announce_on_every_connection() {
    let atv = FakeAppleTv::new(PIN);
    let credentials = pair(&atv);
    let mut entropy = TestEntropy::new();
    let mut verify_keys = Vec::new();
    for round in 0..2 {
        let mut config = common::config();
        let verifier = LegacyVerifier::new(&credentials, &mut entropy).expect("a verifier");
        verify_keys.push(verifier.verify_public());
        config.verifier = Some(Box::new(verifier));
        let mut link = Link::new(config, apple_tv_receiver(&atv));
        link.connect();
        assert_eq!(
            link.keys(),
            [
                "OPTIONS",
                "POST /pair-verify",
                "POST /pair-verify",
                "ANNOUNCE",
                "SETUP",
                "RECORD",
                "SET_PARAMETER"
            ],
            "round {round}: {:?}",
            link.events
        );
        assert_eq!(link.session.state(), State::Playing);
        // P55: 01 00 00 00 | v_pub | a_pub, raw bytes.
        let first = &link.receiver.requests[1];
        assert_eq!(first.body.len(), 68);
        assert_eq!(&first.body[..4], &[1, 0, 0, 0]);
        assert_eq!(&first.body[4..36], &verify_keys[round]);
        assert_eq!(&first.body[36..], &credentials.public_key());
        // P56: 00 00 00 00 | the encrypted signature, which the fake
        // decrypted and checked against a_pub before its 200.
        let second = &link.receiver.requests[2];
        assert_eq!(second.body.len(), 68);
        assert_eq!(&second.body[..4], &[0, 0, 0, 0]);
        link.session.teardown(link.now);
        link.pump();
        assert!(link.closed);
    }
    // A fresh X25519 key on each connection (P55).
    assert_ne!(verify_keys[0], verify_keys[1]);

    // A malformed first answer is a protocol error, not a verification.
    let mut verifier = LegacyVerifier::new(&credentials, &mut entropy).expect("a verifier");
    assert_eq!(verifier.second_request(&[0x5A; 95]), None);
    assert_eq!(verifier.second_request(&[0; 96]), None);
}

// covers: P58, P136
#[test]
fn a_credential_the_apple_tv_no_longer_accepts_means_pairing_required() {
    let atv = FakeAppleTv::new(PIN);
    let credentials = pair(&atv);
    atv.revoke_all();
    let mut config = common::config();
    let verifier = LegacyVerifier::new(&credentials, &mut TestEntropy::new()).expect("verifier");
    config.verifier = Some(Box::new(verifier));
    let mut link = Link::new(config, apple_tv_receiver(&atv));
    link.connect();
    assert_eq!(session_kinds(&link), [ErrorKind::PairingRequired]);
    let error = link.failure().expect("failed");
    assert_eq!(error.status, Some(470));
    assert!(error.kind.needs_user());
    assert!(!link.keys().contains(&"ANNOUNCE".to_owned()));

    // Credentials made by another pairing aren't known either.
    let other = Credentials::new("FEDCBA9876543210", [0x24; 32]).expect("credentials");
    let mut config = common::config();
    config.verifier = Some(Box::new(
        LegacyVerifier::new(&other, &mut TestEntropy::new()).expect("verifier"),
    ));
    let mut link = Link::new(config, apple_tv_receiver(&atv));
    link.connect();
    assert_eq!(session_kinds(&link), [ErrorKind::PairingRequired]);
}

// covers: P60
#[test]
fn auth_setup_sends_a_fresh_x25519_public_key_and_any_answer_goes_on() {
    let secret = [0x31u8; 32];
    let mut entropy = TestEntropy::scripted(vec![secret.to_vec()]);
    let key = pairing::auth_setup_key(&mut entropy).expect("a key");
    let expected = x25519_dalek::PublicKey::from(&x25519_dalek::StaticSecret::from(secret));
    assert_eq!(&key, expected.as_bytes());
    // Fresh each time.
    let mut entropy = TestEntropy::new();
    let one = pairing::auth_setup_key(&mut entropy).expect("a key");
    let two = pairing::auth_setup_key(&mut entropy).expect("a key");
    assert_ne!(one, two);

    for status in [200, 403, 500] {
        let mut config = common::config();
        config.auth_setup_key = Some(key);
        let mut receiver = FakeReceiver::new();
        if status != 200 {
            receiver
                .status
                .insert("POST /auth-setup".to_owned(), status);
        }
        let mut link = Link::new(config, receiver);
        link.connect();
        // Right after OPTIONS, 0x01 then the key, raw bytes.
        assert_eq!(link.keys()[..2], ["OPTIONS", "POST /auth-setup"]);
        let request = link.last("POST /auth-setup");
        assert_eq!(
            request.headers.get("Content-Type"),
            Some("application/octet-stream")
        );
        assert_eq!(request.body[0], 0x01);
        assert_eq!(&request.body[1..], &key);
        assert_eq!(link.session.state(), State::Playing, "status {status}");
        let warned = link.events.iter().any(|event| {
            matches!(
                event,
                SessionEvent::Warning(Warning::AuthSetupFailed { .. })
            )
        });
        assert_eq!(warned, status != 200);
    }
}
