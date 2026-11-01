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
use audiosync_airplay::secret::Secret;
use audiosync_airplay::session::{SessionEvent, State, Verifier, VerifyFailure};
use common::atv::{CLIENT, FakeAppleTv, PairLink, SealAnswer, TestEntropy, public_of};
use common::{FakeReceiver, Link, PasswordGate, digest_params, key};
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

    // P46, P48, P49: OPTIONS, pin-start without a body, then the client ID
    // in a plist.
    let keys: Vec<String> = link.receiver.requests.iter().map(key).collect();
    assert_eq!(
        keys,
        ["OPTIONS", "POST /pair-pin-start", "POST /pair-setup-pin"]
    );
    let start: &Request = &link.receiver.requests[1];
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
    let first = &link.receiver.requests[2];
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
            "OPTIONS",
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
    let proof = plist(&link.receiver.requests[3].body);
    let sent_a = proof.get("pk").and_then(Value::as_data).expect("pk");
    assert_eq!(sent_a, public_of(credentials.secret()).as_slice());
    // P53, P54: the receiver's key from its sealed answer.
    assert_eq!(credentials.receiver_key(), Some(atv.receiver_key()));

    // The blob a host stores round-trips, and nothing prints the secret:
    // version 2 with the receiver's key, 81 bytes, version 1 without, 49.
    let blob = credentials.to_bytes();
    assert_eq!(blob.len(), 81);
    assert_eq!(blob[0], 2);
    assert_eq!(Credentials::from_bytes(&blob), Ok(credentials.clone()));
    assert!(Credentials::from_bytes(&blob[..80]).is_err());
    let without_key = Credentials::new(&client_id, *credentials.secret()).expect("credentials");
    assert_ne!(without_key, credentials);
    let blob = without_key.to_bytes();
    assert_eq!(blob.len(), 49);
    assert_eq!(blob[0], 1);
    assert_eq!(Credentials::from_bytes(&blob), Ok(without_key.clone()));
    assert!(Credentials::from_bytes(&blob[..48]).is_err());
    assert_eq!(without_key.receiver_key(), None);
    let mut other = *credentials.secret();
    other[31] ^= 1;
    assert_ne!(
        Credentials::new(&client_id, other).expect("credentials"),
        credentials
    );
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
    // The detail names the check, so a log tells it apart from a wrong PIN.
    assert!(
        error
            .detail
            .is_some_and(|detail| detail.starts_with("M2 check failed")),
        "{error}"
    );
    assert_eq!(error.status, Some(200));
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

// Part of P50: the same redraw when A would start with a zero byte, which
// P50 asks for along with S's.
// covers: P50
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

// covers: P50
#[test]
fn a_b_with_a_leading_zero_byte_is_hashed_in_its_shortest_form() {
    // The fake hashes B without its leading zero byte in M1, as P50 says,
    // so a sender that padded B would get a 470 here.
    let user = format!("{CLIENT:016X}");
    let atv = FakeAppleTv::with_zero_led_b(PIN, &user);
    let b_pub = atv.b_of(&user);
    assert_eq!(b_pub.len(), 256, "B goes out as 256 bytes (P49)");
    assert_eq!(b_pub[0], 0, "B starts with a zero byte");
    let credentials = pair(&atv);
    assert_eq!(atv.paired(), [credentials.public_key()]);
}

// covers: P52
#[test]
fn after_a_470_to_the_proof_no_proof_is_sent_again() {
    let atv = FakeAppleTv::new(PIN);
    let mut link = PairLink::new(apple_tv_receiver(&atv));
    link.connect();
    link.submit("0000", &mut TestEntropy::new());
    let (error, wrong_pin) = link.failure().expect("failed");
    assert_eq!(error.kind, ErrorKind::PairingFailed);
    assert!(wrong_pin);
    assert!(link.closed);
    assert_eq!(link.pairing.state(), PairingState::Closed);
    // A corrected PIN can't go after it on this pairing.
    assert_eq!(
        link.pairing
            .submit_pin(PIN, &mut TestEntropy::new(), link.now),
        Err(PinError::NotWaiting)
    );
    link.pump();
    assert_eq!(atv.bodies("/pair-setup-pin").len(), 2, "one proof only");
    // The host starts again from /pair-pin-start.
    let mut link = PairLink::new(apple_tv_receiver(&atv));
    link.connect();
    let keys: Vec<String> = link.receiver.requests.iter().map(key).collect();
    assert_eq!(
        keys,
        ["OPTIONS", "POST /pair-pin-start", "POST /pair-setup-pin"]
    );
    link.submit(PIN, &mut TestEntropy::new());
    assert!(link.paired().is_some());
}

// covers: P46, P53, P54, P148
#[test]
fn options_goes_first_and_the_receivers_sealed_key_is_kept_when_it_opens() {
    // A 200 whose `Public` lacks ANNOUNCE: Unsupported, no PIN asked.
    let atv = FakeAppleTv::new(PIN);
    let mut receiver = apple_tv_receiver(&atv);
    receiver.public = Some("OPTIONS, SETUP, RECORD, GET_PARAMETER, POST, GET".to_owned());
    let mut link = PairLink::new(receiver);
    link.connect();
    let (error, _) = link.failure().expect("failed");
    assert_eq!(
        error.kind,
        ErrorKind::Unsupported(audiosync_airplay::error::UnsupportedReason::Methods)
    );
    assert_eq!(error.method, Some(Method::Options));
    assert!(!link.events.contains(&PairingEvent::PinRequested));
    assert!(
        !atv.paths().iter().any(|path| path == "/pair-pin-start"),
        "no pin-start"
    );
    assert!(link.closed);

    // A 403, a 501 or no `Public` goes on to pin-start.
    for (status, public) in [(Some(403), None), (Some(501), None), (None, None)] {
        let atv = FakeAppleTv::new(PIN);
        let mut receiver = apple_tv_receiver(&atv);
        receiver.public = public;
        if let Some(status) = status {
            receiver.status.insert("OPTIONS".to_owned(), status);
        }
        let mut link = PairLink::new(receiver);
        link.connect();
        assert_eq!(link.events, [PairingEvent::PinRequested], "{status:?}");
    }
    // A 403 that means access restricted: pin-start fails, PairingFailed.
    let atv = FakeAppleTv::new(PIN);
    let mut receiver = apple_tv_receiver(&atv);
    receiver.status.insert("OPTIONS".to_owned(), 403);
    receiver
        .status
        .insert("POST /pair-pin-start".to_owned(), 403);
    let mut link = PairLink::new(receiver);
    link.connect();
    let (error, _) = link.failure().expect("failed");
    assert_eq!(error.kind, ErrorKind::PairingFailed);
    assert_eq!(error.status, Some(403));

    // The sealed answer: kept when it opens, ignored when it is missing,
    // doesn't open or is too large to read; pairing completes either way.
    for (answer, kept) in [
        (SealAnswer::Sealed, true),
        (SealAnswer::Empty, false),
        (SealAnswer::Tampered, false),
        (SealAnswer::Oversized, false),
    ] {
        let atv = FakeAppleTv::new(PIN);
        atv.answer_seal_with(answer);
        let credentials = pair(&atv);
        assert_eq!(atv.paired(), [credentials.public_key()], "{answer:?}");
        assert_eq!(
            credentials.receiver_key(),
            kept.then(|| atv.receiver_key()),
            "{answer:?}"
        );
    }
}

// covers: P46, P38, P39
#[test]
fn a_401_to_the_pairing_options_is_a_password_challenge_not_a_pairing_answer() {
    let gated = |atv: &FakeAppleTv| {
        let mut receiver = apple_tv_receiver(atv);
        receiver.gate = Some(PasswordGate::new("secret"));
        receiver
    };

    // Without the speaker's password: PasswordRequired, and the pairing
    // stops before `/pair-pin-start`, so the Apple TV shows no PIN.
    let atv = FakeAppleTv::new(PIN);
    let mut link = PairLink::new(gated(&atv));
    link.connect();
    let (error, wrong_pin) = link.failure().expect("failed");
    assert_eq!(error.kind, ErrorKind::PasswordRequired);
    assert_eq!(error.method, Some(Method::Options));
    assert_eq!(error.status, Some(401));
    assert!(!wrong_pin);
    assert_eq!(link.receiver.keys(), ["OPTIONS"]);
    assert!(atv.paths().is_empty(), "no pin-start: {:?}", atv.paths());
    assert!(!link.events.contains(&PairingEvent::PinRequested));
    assert!(link.closed);

    // With it: the challenge is answered, and every later request on the
    // connection, pairing requests included, carries `Authorization` with
    // the same nonce, made for its own method and URI (P36, P37, P38).
    let atv = FakeAppleTv::new(PIN);
    let mut link = PairLink::with_password(gated(&atv), Some("secret"));
    link.connect();
    assert_eq!(link.events, [PairingEvent::PinRequested]);
    link.submit(PIN, &mut TestEntropy::new());
    let credentials = link
        .paired()
        .unwrap_or_else(|| panic!("paired: {:?}", link.events));
    assert_eq!(atv.paired(), [credentials.public_key()]);
    assert_eq!(
        link.receiver.keys(),
        [
            "OPTIONS",
            "OPTIONS",
            "POST /pair-pin-start",
            "POST /pair-setup-pin",
            "POST /pair-setup-pin",
            "POST /pair-setup-pin"
        ]
    );
    let requests = &link.receiver.requests;
    assert!(requests[0].headers.get("Authorization").is_none());
    let nonce = PasswordGate::new("secret").nonce;
    for request in &requests[1..] {
        let authorization = request
            .headers
            .get("Authorization")
            .unwrap_or_else(|| panic!("{} {} without Authorization", request.method, request.uri));
        let params = digest_params(authorization);
        assert_eq!(params["username"], "iTunes");
        assert_eq!(params["uri"], request.uri);
        assert_eq!(params["nonce"], nonce);
    }
    assert_eq!(
        link.receiver.authorized, 5,
        "every resent and later request"
    );
    assert!(!format!("{:?}", link.pairing).contains("secret"));

    // A 403 to the OPTIONS resent with `Authorization` is the Apple TV
    // asking for verification: the pairing goes on to pin-start (P46).
    let atv = FakeAppleTv::new(PIN);
    let mut receiver = gated(&atv);
    receiver.status.insert("OPTIONS".to_owned(), 403);
    let mut link = PairLink::with_password(receiver, Some("secret"));
    link.connect();
    assert_eq!(link.events, [PairingEvent::PinRequested]);
    assert_eq!(
        link.receiver.keys(),
        [
            "OPTIONS",
            "OPTIONS",
            "POST /pair-pin-start",
            "POST /pair-setup-pin"
        ]
    );

    // A password the receiver turns down: WrongPassword, no resend, no
    // pin-start (P135).
    let atv = FakeAppleTv::new(PIN);
    let mut link = PairLink::with_password(gated(&atv), Some("wrong"));
    link.connect();
    let (error, wrong_pin) = link.failure().expect("failed");
    assert_eq!(error.kind, ErrorKind::WrongPassword);
    assert_eq!(error.status, Some(401));
    assert!(!wrong_pin, "the PIN was never asked for");
    assert_eq!(link.receiver.keys(), ["OPTIONS", "OPTIONS"]);
    assert!(atv.paths().is_empty());

    // A stale nonce on a pairing request: one resend with the new nonce,
    // and the pairing goes on (P38). The pin-start is the second request
    // with a valid `Authorization`.
    let atv = FakeAppleTv::new(PIN);
    let mut receiver = gated(&atv);
    receiver.gate.as_mut().expect("gate").stale_on = Some(2);
    let mut link = PairLink::with_password(receiver, Some("secret"));
    link.connect();
    assert_eq!(link.events, [PairingEvent::PinRequested]);
    assert_eq!(
        link.receiver.keys(),
        [
            "OPTIONS",
            "OPTIONS",
            "POST /pair-pin-start",
            "POST /pair-pin-start",
            "POST /pair-setup-pin"
        ]
    );
    assert_eq!(atv.paths(), ["*", "/pair-pin-start", "/pair-setup-pin"]);
    let last = link.receiver.requests.last().expect("a request");
    let params = digest_params(last.headers.get("Authorization").expect("Authorization"));
    assert_ne!(params["nonce"], nonce, "the new nonce");
}

/// The request keys of a whole pairing on one connection: OPTIONS,
/// pin-start and the three pair-setup steps.
const PAIRING_KEYS: [&str; 5] = [
    "OPTIONS",
    "POST /pair-pin-start",
    "POST /pair-setup-pin",
    "POST /pair-setup-pin",
    "POST /pair-setup-pin",
];

/// The pairing's request keys with the one at `index` sent `times` times,
/// cut after it when `stop` is set.
fn keys_with_repeat(index: usize, times: usize, stop: bool) -> Vec<String> {
    let mut keys: Vec<String> = PAIRING_KEYS[..index]
        .iter()
        .map(|key| (*key).to_owned())
        .collect();
    keys.extend(std::iter::repeat_n(PAIRING_KEYS[index].to_owned(), times));
    if !stop {
        keys.extend(
            PAIRING_KEYS[index + 1..]
                .iter()
                .map(|key| (*key).to_owned()),
        );
    }
    keys
}

/// A fake Apple TV behind a password that it first asks for at the
/// request with this index on the connection, with nothing challenged
/// before it.
fn challenging_from(atv: &FakeAppleTv, index: usize) -> FakeReceiver {
    let mut receiver = apple_tv_receiver(atv);
    let mut gate = PasswordGate::new("secret");
    gate.from_request = index;
    receiver.gate = Some(gate);
    receiver
}

/// Runs a pairing as far as it goes, typing the right PIN when asked.
fn run_pairing(link: &mut PairLink) {
    link.connect();
    if link.pairing.state() == PairingState::WaitingForPin {
        link.submit(PIN, &mut TestEntropy::new());
    }
}

// covers: P36, P58, P136
#[test]
fn a_401_to_pin_start_or_a_pair_setup_step_is_a_password_challenge() {
    // pin-start, then the client ID, the proof and the sealed key.
    for (index, step) in [
        (1, "pin-start"),
        (2, "setup 1"),
        (3, "setup 2"),
        (4, "setup 3"),
    ] {
        // Without the speaker's password: PasswordRequired, not
        // PairingFailed, with no resend and the PIN not counted as wrong.
        let atv = FakeAppleTv::new(PIN);
        let mut link = PairLink::new(challenging_from(&atv, index));
        run_pairing(&mut link);
        let (error, wrong_pin) = link.failure().unwrap_or_else(|| panic!("{step}"));
        assert_eq!(error.kind, ErrorKind::PasswordRequired, "{step}");
        assert_eq!(error.method, Some(Method::Post), "{step}");
        assert_eq!(error.status, Some(401), "{step}");
        assert!(!wrong_pin, "{step}");
        assert_eq!(
            link.receiver.keys(),
            keys_with_repeat(index, 1, true),
            "{step}"
        );
        assert!(link.closed, "{step}");
        assert_eq!(
            atv.paths().len(),
            index,
            "{step}: the 401 is no pairing answer"
        );
        assert!(link.paired().is_none(), "{step}");

        // With it: the same request again, same body, with `Authorization`,
        // and the pairing goes on to the credentials; every later request
        // carries it too (P38). For the proof this is not the resend after
        // a verdict that P52 rules out: the Apple TV judged it once.
        let atv = FakeAppleTv::new(PIN);
        let mut link = PairLink::with_password(challenging_from(&atv, index), Some("secret"));
        run_pairing(&mut link);
        let credentials = link
            .paired()
            .unwrap_or_else(|| panic!("{step}: {:?}", link.events));
        assert_eq!(atv.paired(), [credentials.public_key()], "{step}");
        assert_eq!(
            link.receiver.keys(),
            keys_with_repeat(index, 2, false),
            "{step}"
        );
        let requests = &link.receiver.requests;
        assert_eq!(requests[index].body, requests[index + 1].body, "{step}");
        for request in &requests[..=index] {
            assert!(request.headers.get("Authorization").is_none(), "{step}");
        }
        let nonce = PasswordGate::new("secret").nonce;
        for request in &requests[index + 1..] {
            let authorization = request
                .headers
                .get("Authorization")
                .unwrap_or_else(|| panic!("{step}: {} without Authorization", request.uri));
            let params = digest_params(authorization);
            assert_eq!(params["uri"], request.uri, "{step}");
            assert_eq!(params["nonce"], nonce, "{step}");
        }
        assert_eq!(atv.bodies("/pair-setup-pin").len(), 3, "{step}");
        assert!(
            !link
                .events
                .iter()
                .any(|event| matches!(event, PairingEvent::Failed { .. })),
            "{step}"
        );

        // A second 401 to the answered request: WrongPassword (P135), no
        // third send, and the PIN not counted as wrong.
        let atv = FakeAppleTv::new(PIN);
        let mut link = PairLink::with_password(challenging_from(&atv, index), Some("wrong"));
        run_pairing(&mut link);
        let (error, wrong_pin) = link.failure().unwrap_or_else(|| panic!("{step}"));
        assert_eq!(error.kind, ErrorKind::WrongPassword, "{step}");
        assert_eq!(error.status, Some(401), "{step}");
        assert!(!wrong_pin, "{step}");
        assert_eq!(
            link.receiver.keys(),
            keys_with_repeat(index, 2, true),
            "{step}"
        );
        assert!(link.closed, "{step}");

        // A `stale=true` answer to the resend: exactly one more resend with
        // the new nonce, and the pairing goes on (P38).
        let atv = FakeAppleTv::new(PIN);
        let mut receiver = challenging_from(&atv, index);
        receiver.gate.as_mut().expect("gate").stale_on = Some(1);
        let mut link = PairLink::with_password(receiver, Some("secret"));
        run_pairing(&mut link);
        assert!(link.paired().is_some(), "{step}: {:?}", link.events);
        assert_eq!(
            link.receiver.keys(),
            keys_with_repeat(index, 3, false),
            "{step}"
        );
        let requests = &link.receiver.requests;
        assert_eq!(requests[index + 1].body, requests[index + 2].body, "{step}");
        let new_nonce = link.receiver.gate.as_ref().expect("gate").nonce.clone();
        assert_ne!(new_nonce, nonce, "{step}");
        let params = digest_params(
            requests[index + 2]
                .headers
                .get("Authorization")
                .expect("Authorization"),
        );
        assert_eq!(params["nonce"], new_nonce, "{step}");

        // A second `stale=true` answer to the same request: WrongPassword,
        // with no fourth send.
        let atv = FakeAppleTv::new(PIN);
        let mut receiver = challenging_from(&atv, index);
        let gate = receiver.gate.as_mut().expect("gate");
        gate.stale_on = Some(1);
        gate.stale_times = 2;
        let mut link = PairLink::with_password(receiver, Some("secret"));
        run_pairing(&mut link);
        let (error, wrong_pin) = link.failure().unwrap_or_else(|| panic!("{step}"));
        assert_eq!(error.kind, ErrorKind::WrongPassword, "{step}");
        assert!(!wrong_pin, "{step}");
        assert_eq!(
            link.receiver.keys(),
            keys_with_repeat(index, 3, true),
            "{step}"
        );
    }
}

// The 409 and other non-200 parts of P58 and P136; their 401 part, a
// password challenge, is in the 401 tests above and below.
// covers: P58, P136, P137
#[test]
fn a_409_at_a_pairing_step_is_busy_and_other_failures_are_pairing_failed() {
    for (status_key, setup) in [
        (Some("OPTIONS"), vec![]),
        (Some("POST /pair-pin-start"), vec![]),
        (None, vec![Some(409)]),
        (None, vec![None, Some(409)]),
    ] {
        let atv = FakeAppleTv::new(PIN);
        let mut receiver = apple_tv_receiver(&atv);
        if let Some(status_key) = status_key {
            receiver.status.insert(status_key.to_owned(), 409);
        }
        for status in setup.iter().copied() {
            atv.queue_setup_status(status);
        }
        let mut link = PairLink::new(receiver);
        link.connect();
        if link.pairing.state() == PairingState::WaitingForPin {
            link.submit(PIN, &mut TestEntropy::new());
        }
        let (error, wrong_pin) = link.failure().expect("failed");
        assert_eq!(error.kind, ErrorKind::Busy, "{status_key:?} {setup:?}");
        assert_eq!(error.status, Some(409));
        assert!(!wrong_pin);
    }
    // Any other non-200 to a pairing request but a 401: PairingFailed
    // (P58).
    for status in [400, 403, 500, 503] {
        let atv = FakeAppleTv::new(PIN);
        let mut receiver = apple_tv_receiver(&atv);
        receiver
            .status
            .insert("POST /pair-pin-start".to_owned(), status);
        let mut link = PairLink::new(receiver);
        link.connect();
        let (error, _) = link.failure().expect("failed");
        assert_eq!(error.kind, ErrorKind::PairingFailed, "{status}");
        let atv = FakeAppleTv::new(PIN);
        atv.queue_setup_status(Some(status));
        let mut link = PairLink::new(apple_tv_receiver(&atv));
        link.connect();
        let (error, _) = link.failure().expect("failed");
        assert_eq!(error.kind, ErrorKind::PairingFailed, "{status}");
    }
    // A 401 to pin-start or to a pair-setup step, with no challenge before
    // it, is a password challenge instead: PasswordRequired without the
    // speaker's password, never PairingFailed (P36, P58).
    for index in 1..PAIRING_KEYS.len() {
        let atv = FakeAppleTv::new(PIN);
        let mut link = PairLink::new(challenging_from(&atv, index));
        run_pairing(&mut link);
        let (error, wrong_pin) = link.failure().expect("failed");
        assert_eq!(error.kind, ErrorKind::PasswordRequired, "{index}");
        assert!(!wrong_pin, "{index}");
    }
}

// covers: P138, P220
#[test]
fn another_4xx_or_5xx_to_the_pairing_options_is_refused_before_any_pin() {
    for status in [400, 404, 406, 500, 503] {
        let atv = FakeAppleTv::new(PIN);
        let mut receiver = apple_tv_receiver(&atv);
        receiver.status.insert("OPTIONS".to_owned(), status);
        let mut link = PairLink::new(receiver);
        link.connect();
        let (error, wrong_pin) = link.failure().expect("failed");
        assert_eq!(error.kind, ErrorKind::Refused, "{status}");
        assert_eq!(error.method, Some(Method::Options), "{status}");
        assert_eq!(error.status, Some(status));
        assert!(!wrong_pin);
        assert_eq!(link.receiver.keys(), ["OPTIONS"], "{status}");
    }
    // A 403 and a 501 are no refusal: the pairing goes on (P46, P27).
    for status in [403, 501] {
        let atv = FakeAppleTv::new(PIN);
        let mut receiver = apple_tv_receiver(&atv);
        receiver.status.insert("OPTIONS".to_owned(), status);
        let mut link = PairLink::new(receiver);
        link.connect();
        assert!(link.failure().is_none(), "{status} {:?}", link.events);
        assert_eq!(
            link.pairing.state(),
            PairingState::WaitingForPin,
            "{status}"
        );
    }
}

// The 470 and PIN-wait parts of P58 and P136; a 401 at any step is a
// password challenge, tested in the 401 tests.
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

// covers: P55, P56, P26
#[test]
fn stored_credentials_verify_first_on_every_connection() {
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
                "POST /pair-verify",
                "POST /pair-verify",
                "OPTIONS",
                "ANNOUNCE",
                "SETUP",
                "RECORD",
                "SET_PARAMETER"
            ],
            "round {round}: {:?}",
            link.events
        );
        assert_eq!(link.session.state(), State::Playing);
        // The receiver's signature checked out against its key (P56).
        assert!(
            !link
                .events
                .iter()
                .any(|event| matches!(event, SessionEvent::Warning(_))),
            "{:?}",
            link.events
        );
        // P55: 01 00 00 00 | v_pub | a_pub, raw bytes.
        let first = &link.receiver.requests[0];
        assert_eq!(first.body.len(), 68);
        assert_eq!(&first.body[..4], &[1, 0, 0, 0]);
        assert_eq!(&first.body[4..36], &verify_keys[round]);
        assert_eq!(&first.body[36..], &credentials.public_key());
        // P56: 00 00 00 00 | the encrypted signature, which the fake
        // decrypted and checked against a_pub before its 200.
        let second = &link.receiver.requests[1];
        assert_eq!(second.body.len(), 68);
        assert_eq!(&second.body[..4], &[0, 0, 0, 0]);
        link.session.teardown(link.now);
        link.pump();
        assert!(link.closed);
    }
    // A fresh X25519 key on each connection (P55).
    assert_ne!(verify_keys[0], verify_keys[1]);

    // The verifier refuses a first answer of another length (which the
    // session reports as a failed verification, P55) and an atv_pub that
    // gives an all-zero shared secret.
    let mut verifier = LegacyVerifier::new(&credentials, &mut entropy).expect("a verifier");
    assert_eq!(
        verifier.second_request(&[0x5A; 95]),
        Err(VerifyFailure::Malformed)
    );
    assert_eq!(
        verifier.second_request(&[0; 96]),
        Err(VerifyFailure::Malformed)
    );

    // A spare key pair for the one reconnect of P58, and only one.
    let before = verifier.verify_public();
    assert!(verifier.renew());
    assert_ne!(verifier.verify_public(), before);
    assert!(!verifier.renew());
}

// covers: P56
#[test]
fn a_receiver_signature_that_fails_the_check_ends_the_session_before_the_sender_signs() {
    let atv = FakeAppleTv::new(PIN);
    let credentials = pair(&atv);
    assert!(credentials.receiver_key().is_some());
    atv.sign_with_another_key();
    let mut config = common::config();
    config.verifier = Some(Box::new(
        LegacyVerifier::new(&credentials, &mut TestEntropy::new()).expect("verifier"),
    ));
    let mut link = Link::new(config, apple_tv_receiver(&atv));
    link.connect();
    assert_ne!(link.session.state(), State::Playing, "{:?}", link.events);
    let error = link.failure().expect("failed");
    // Not PairingRequired: a host drops stored credentials on that, and a
    // device that can't sign must not be able to wipe them.
    assert_eq!(error.kind, ErrorKind::ReceiverUnverified);
    assert!(error.kind.needs_user());
    // The sender's signature never leaves.
    assert_eq!(link.keys(), ["POST /pair-verify"]);
    assert!(link.closed);

    // Without the receiver's key there is nothing to check.
    let without_key =
        Credentials::new(credentials.client_id(), *credentials.secret()).expect("credentials");
    let mut config = common::config();
    config.verifier = Some(Box::new(
        LegacyVerifier::new(&without_key, &mut TestEntropy::new()).expect("verifier"),
    ));
    let mut link = Link::new(config, apple_tv_receiver(&atv));
    link.connect();
    assert_eq!(link.session.state(), State::Playing);
    assert!(
        !link
            .events
            .iter()
            .any(|event| matches!(event, SessionEvent::Warning(_)))
    );
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

/// A session that verifies `credentials` with the fake Apple TV, which
/// first asks for its password at the request with this index on the
/// connection (0: the first pair-verify step, 1: the second).
fn verify_link(
    atv: &FakeAppleTv,
    credentials: &Credentials,
    index: usize,
    password: Option<&str>,
    stale: Option<usize>,
) -> Link {
    let mut config = common::config();
    let verifier = LegacyVerifier::new(credentials, &mut TestEntropy::new()).expect("verifier");
    config.verifier = Some(Box::new(verifier));
    config.password = password.map(Secret::from);
    let mut receiver = apple_tv_receiver(atv);
    let mut gate = PasswordGate::new("secret");
    gate.from_request = index;
    if let Some(times) = stale {
        gate.stale_on = Some(1);
        gate.stale_times = times;
    }
    receiver.gate = Some(gate);
    let mut link = Link::new(config, receiver);
    link.connect();
    link
}

/// The session's request keys: `verifies` pair-verify requests, then the
/// rest of the start (P26).
fn verified_start(verifies: usize) -> Vec<String> {
    let mut keys = vec!["POST /pair-verify".to_owned(); verifies];
    keys.extend(
        ["OPTIONS", "ANNOUNCE", "SETUP", "RECORD", "SET_PARAMETER"]
            .iter()
            .map(|key| (*key).to_owned()),
    );
    keys
}

// covers: P36, P58, P136
#[test]
fn a_401_to_either_pair_verify_step_is_a_password_challenge_and_the_credentials_stay() {
    let atv = FakeAppleTv::new(PIN);
    let credentials = pair(&atv);
    let verify = "POST /pair-verify".to_owned();
    for (index, step) in [(0, "first step"), (1, "second step")] {
        // Without the speaker's password: PasswordRequired, not
        // PairingRequired, so the host keeps the credentials, and the
        // Apple TV still knows them.
        let link = verify_link(&atv, &credentials, index, None, None);
        assert_eq!(
            session_kinds(&link),
            [ErrorKind::PasswordRequired],
            "{step}"
        );
        let error = link.failure().expect("failed");
        assert_eq!(error.method, Some(Method::Post), "{step}");
        assert_eq!(error.status, Some(401), "{step}");
        assert_eq!(link.keys(), vec![verify.clone(); index + 1], "{step}");
        assert!(link.closed, "{step}");
        assert_eq!(atv.paired(), [credentials.public_key()], "{step}");

        // With it: the same step again, same body, with `Authorization`;
        // the verification and the session go on, every later request
        // carrying `Authorization` (P38).
        let link = verify_link(&atv, &credentials, index, Some("secret"), None);
        assert_eq!(
            link.session.state(),
            State::Playing,
            "{step}: {:?}",
            link.events
        );
        assert_eq!(link.keys(), verified_start(3), "{step}");
        let requests = &link.receiver.requests;
        assert_eq!(requests[index].body, requests[index + 1].body, "{step}");
        assert!(requests[index].headers.get("Authorization").is_none());
        for request in &requests[index + 1..] {
            let authorization = request
                .headers
                .get("Authorization")
                .unwrap_or_else(|| panic!("{step}: {} without Authorization", request.uri));
            assert_eq!(digest_params(authorization)["uri"], request.uri, "{step}");
        }

        // A second 401 to the answered step: WrongPassword, not
        // PairingRequired, and no third send.
        let link = verify_link(&atv, &credentials, index, Some("wrong"), None);
        assert_eq!(session_kinds(&link), [ErrorKind::WrongPassword], "{step}");
        assert_eq!(link.keys(), vec![verify.clone(); index + 2], "{step}");
        assert!(link.closed, "{step}");

        // A `stale=true` answer to the resend: exactly one more resend, and
        // the session goes on (P38).
        let link = verify_link(&atv, &credentials, index, Some("secret"), Some(1));
        assert_eq!(
            link.session.state(),
            State::Playing,
            "{step}: {:?}",
            link.events
        );
        assert_eq!(link.keys(), verified_start(4), "{step}");
        let requests = &link.receiver.requests;
        assert_eq!(requests[index + 1].body, requests[index + 2].body, "{step}");

        // A second `stale=true` answer to the same step: WrongPassword,
        // with no fourth send.
        let link = verify_link(&atv, &credentials, index, Some("secret"), Some(2));
        assert_eq!(session_kinds(&link), [ErrorKind::WrongPassword], "{step}");
        assert_eq!(link.keys(), vec![verify.clone(); index + 3], "{step}");
    }
    // The credentials still verify where no password is asked for.
    let mut config = common::config();
    let verifier = LegacyVerifier::new(&credentials, &mut TestEntropy::new()).expect("verifier");
    config.verifier = Some(Box::new(verifier));
    let mut link = Link::new(config, apple_tv_receiver(&atv));
    link.connect();
    assert_eq!(link.session.state(), State::Playing, "{:?}", link.events);
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
