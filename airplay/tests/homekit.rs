//! The HomeKit pairing of the AirPlay 2 path against a fake receiver that
//! computes its side on its own (tests/common/hap.rs): the choice of
//! pairing, TLV8, transient, PIN and password pair-setup with the SRP retry,
//! pair-verify, the error mapping and the credentials.

mod common;

use std::time::Duration;

use audiosync_airplay::error::{ErrorKind, UnsupportedReason};
use audiosync_airplay::homekit::tlv8::{self, Tlv8, Truncated};
use audiosync_airplay::homekit::{
    self, HomeKitCredentials, Identity, Mode, PairSetup, PairVerify, SetupStep, SrpWork, VerifyStep,
};
use audiosync_airplay::rtsp::{Method, Request, Response};
use audiosync_airplay::secret::Secret;

use common::atv::TestEntropy;
use common::hap::{self, Answer, FakeHomeKit};

const PIN: &str = "4821";
const PASSWORD: &str = "hunter2";

/// Runs a pair-setup against the fake until it ends, answering the PIN
/// request with `pin` and taking the P151 retry on a new connection.
fn run_setup(
    setup: &mut PairSetup,
    fake: &mut FakeHomeKit,
    entropy: &mut TestEntropy,
    pin: Option<&str>,
) -> (SetupStep, usize) {
    let mut request = setup.start();
    let mut retries = 0;
    loop {
        let response = fake.answer(&request);
        match setup.handle_response(&response, entropy) {
            SetupStep::Send(next) => request = next,
            SetupStep::PinRequested => {
                request = setup.submit_pin(pin.expect("a PIN")).expect("M1");
            }
            SetupStep::Retry => {
                retries += 1;
                fake.new_connection();
                request = setup.restart();
            }
            end => return (end, retries),
        }
    }
}

fn identity(entropy: &mut TestEntropy) -> Identity {
    Identity::generate(entropy).expect("an identity")
}

/// Pairs with a PIN and returns the credentials.
fn paired(fake: &mut FakeHomeKit, entropy: &mut TestEntropy) -> HomeKitCredentials {
    let mut setup = PairSetup::pin(identity(entropy));
    match run_setup(&mut setup, fake, entropy, Some(PIN)).0 {
        SetupStep::Paired(credentials) => credentials,
        other => panic!("{other:?}"),
    }
}

fn run_verify(verify: &mut PairVerify, fake: &mut FakeHomeKit) -> VerifyStep {
    let mut request = verify.start();
    loop {
        let response = fake.answer(&request);
        match verify.handle_response(&response) {
            VerifyStep::Send(next) => request = next,
            end => return end,
        }
    }
}

fn failed_kind(step: &SetupStep) -> ErrorKind {
    match step {
        SetupStep::Failed(failure) => failure.error.kind,
        other => panic!("{other:?}"),
    }
}

fn paths(fake: &FakeHomeKit) -> Vec<(String, u8)> {
    fake.requests
        .iter()
        .map(|request| {
            let items = hap::untlv(&request.body);
            let state = hap::item(&items, 6).map_or(0, |v| v[0]);
            (request.uri.clone(), state)
        })
        .collect()
}

// covers: P62
#[test]
fn tlv8_encoding() {
    let long: Vec<u8> = (0..600u32).map(|i| i as u8).collect();
    let body = Tlv8::new()
        .with(tlv8::STATE, &[3])
        .with(tlv8::PUBLIC_KEY, &long)
        .with(tlv8::PROOF, &[])
        .with(tlv8::SIGNATURE, &[9; 64]);
    let bytes = body.encode();
    // 600 bytes go in items of 255, 255 and 90; an empty value is one item.
    assert_eq!(&bytes[..3], &[6, 1, 3]);
    assert_eq!(&bytes[3..5], &[3, 255]);
    assert_eq!(&bytes[260..262], &[3, 255]);
    assert_eq!(&bytes[517..519], &[3, 90]);
    assert_eq!(&bytes[609..611], &[4, 0]);
    assert_eq!(bytes.len(), 611 + 2 + 64);
    let decoded = Tlv8::decode(&bytes).expect("decodes");
    assert_eq!(decoded, body);
    assert_eq!(decoded.get(tlv8::PUBLIC_KEY), Some(long.as_slice()));
    assert_eq!(decoded.get(tlv8::PROOF), Some(&[][..]));
    let kinds: Vec<u8> = decoded.items().map(|(kind, _)| kind).collect();
    assert_eq!(kinds, vec![6, 3, 4, 10]);
    // The fake's own codec reads the same items.
    assert_eq!(hap::untlv(&bytes).len(), 4);
    // Two short items of one type stay apart; a cut-off item is refused.
    let two = Tlv8::decode(&[1, 1, b'a', 1, 1, b'b']).expect("decodes");
    assert_eq!(two.items().count(), 2);
    assert_eq!(Tlv8::decode(&[6, 2, 1]), Err(Truncated));
    assert_eq!(Tlv8::decode(&[6]), Err(Truncated));
    // The types of P62 and P150; type 27 of a pair-setup reply is kept as
    // an unknown item.
    assert_eq!(
        [
            tlv8::METHOD,
            tlv8::IDENTIFIER,
            tlv8::SALT,
            tlv8::PUBLIC_KEY,
            tlv8::PROOF,
            tlv8::ENCRYPTED_DATA,
            tlv8::STATE,
            tlv8::ERROR,
            tlv8::BACK_OFF,
            tlv8::SIGNATURE,
            tlv8::NAME,
            tlv8::FLAGS
        ],
        [0, 1, 2, 3, 4, 5, 6, 7, 8, 10, 17, 19]
    );
    let odd = Tlv8::decode(&[6, 1, 2, 27, 1, 0]).expect("decodes");
    assert_eq!(odd.get(27), Some(&[0][..]));
    // The sizes of the published messages: M2 salt 16 and B 384 bytes,
    // pair-verify encrypted data 120 bytes.
    let m2 = hap::tlv(&[(6, &[2]), (2, &[1; 16]), (3, &[2; 384])]);
    let m2 = Tlv8::decode(&m2).expect("decodes");
    assert_eq!(m2.get(tlv8::SALT).map(<[u8]>::len), Some(16));
    assert_eq!(m2.get(tlv8::PUBLIC_KEY).map(<[u8]>::len), Some(384));
    let verify = hap::tlv(&[(6, &[2]), (3, &[1; 32]), (5, &[3; 120])]);
    let verify = Tlv8::decode(&verify).expect("decodes");
    assert_eq!(verify.get(tlv8::ENCRYPTED_DATA).map(<[u8]>::len), Some(120));
}

// covers: P61, P154
#[test]
fn the_pairing_is_chosen_from_credentials_then_status_flags() {
    use homekit::{Stored, check_mode, choose_mode, status_flags};
    // statusFlags of /info first, the TXT flags only without it.
    assert_eq!(status_flags(Some(0x80), Some(0x8)), 0x80);
    assert_eq!(status_flags(None, Some(0x8)), 0x8);
    assert_eq!(status_flags(None, None), 0);
    // Bit 3 asks for a code every time, stored credentials or not; they
    // win over every other flag. The two values come from one TV set to
    // ask every time (0x4c) and only the first time (0x244).
    assert_eq!(choose_mode(0x8 | 0x80 | 0x200, Stored::Kept), Mode::Pin);
    assert_eq!(choose_mode(0x4c, Stored::Kept), Mode::Pin);
    assert_eq!(choose_mode(0x80 | 0x200, Stored::Kept), Mode::Verify);
    assert_eq!(choose_mode(0x244, Stored::Kept), Mode::Verify);
    assert!(homekit::asks_code_every_time(0x4c));
    assert!(!homekit::asks_code_every_time(0x244));
    assert_eq!(choose_mode(1 << 9, Stored::None), Mode::Pin);
    assert_eq!(choose_mode(1 << 3, Stored::None), Mode::Pin);
    assert_eq!(choose_mode((1 << 3) | (1 << 7), Stored::None), Mode::Pin);
    assert_eq!(choose_mode(1 << 7, Stored::None), Mode::Password);
    assert_eq!(choose_mode(0x4 | (1 << 17), Stored::None), Mode::Transient);
    // Turned-down credentials verify again, unless the password can pair
    // anew; they never fall back to a transient pairing (P154).
    assert_eq!(choose_mode(0x80, Stored::TurnedDown), Mode::Password);
    assert_eq!(choose_mode(0x244, Stored::TurnedDown), Mode::Verify);
    assert_eq!(choose_mode(0x4, Stored::TurnedDown), Mode::Verify);
    assert_eq!(choose_mode(0x4c, Stored::TurnedDown), Mode::Pin);
    // A PIN pairing waits for the host; a password pairing for the password.
    assert_eq!(
        check_mode(Mode::Pin, false, true),
        Err(ErrorKind::PairingRequired)
    );
    assert_eq!(check_mode(Mode::Pin, true, false), Ok(()));
    assert_eq!(
        check_mode(Mode::Password, false, false),
        Err(ErrorKind::PasswordRequired)
    );
    assert_eq!(check_mode(Mode::Password, false, true), Ok(()));
    assert_eq!(check_mode(Mode::Transient, false, false), Ok(()));
    assert_eq!(check_mode(Mode::Verify, false, false), Ok(()));

    // A 470 to a transient pair-setup means the receiver wants a PIN
    // pairing: PairingRequired.
    let mut fake = FakeHomeKit::new(homekit::TRANSIENT_CODE);
    fake.answers.push((("/pair-setup", 1), Answer::Status(470)));
    let mut entropy = TestEntropy::new();
    let (end, _) = run_setup(&mut PairSetup::transient(), &mut fake, &mut entropy, None);
    assert_eq!(failed_kind(&end), ErrorKind::PairingRequired);
    // The same status in a PIN pairing fails it (P58).
    let mut fake = FakeHomeKit::new(PIN);
    fake.answers.push((("/pair-setup", 3), Answer::Status(470)));
    let mut setup = PairSetup::pin(identity(&mut entropy));
    let (end, _) = run_setup(&mut setup, &mut fake, &mut entropy, Some(PIN));
    assert_eq!(failed_kind(&end), ErrorKind::PairingFailed);
}

fn header<'a>(request: &'a Request, name: &str) -> Option<&'a str> {
    request.headers.get(name)
}

// Part of P149: the request lines and headers; the AirPlay 2 session that
// sends them on its connection comes in a later stage.
#[test]
fn pairing_requests_carry_their_paths_and_headers() {
    let mut entropy = TestEntropy::new();
    let mut fake = FakeHomeKit::new(homekit::TRANSIENT_CODE);
    run_setup(&mut PairSetup::transient(), &mut fake, &mut entropy, None);
    let mut pin_fake = FakeHomeKit::new(PIN);
    let credentials = paired(&mut pin_fake, &mut entropy);
    let mut verify = PairVerify::new(credentials, &mut entropy).expect("entropy");
    run_verify(&mut verify, &mut pin_fake);

    for request in &fake.requests {
        assert_eq!(request.method, Method::Post);
        assert_eq!(request.uri, "/pair-setup");
        assert_eq!(header(request, "X-Apple-HKP"), Some("4"));
        assert_eq!(
            header(request, "Content-Type"),
            Some("application/octet-stream")
        );
        assert!(!request.headers.contains("Session"));
    }
    let encoded = String::from_utf8_lossy(&fake.requests[0].encode()).into_owned();
    assert!(
        encoded.starts_with("POST /pair-setup RTSP/1.0\r\n"),
        "{encoded}"
    );
    for request in &pin_fake.requests {
        assert_eq!(request.method, Method::Post);
        assert_eq!(header(request, "X-Apple-HKP"), Some("3"), "{}", request.uri);
        assert_eq!(
            header(request, "Content-Type"),
            Some("application/octet-stream")
        );
    }
    let pin_start = &pin_fake.requests[0];
    assert_eq!(pin_start.uri, "/pair-pin-start");
    assert!(pin_start.body.is_empty());
    let uris: Vec<&str> = pin_fake.requests.iter().map(|r| r.uri.as_str()).collect();
    assert_eq!(
        uris,
        vec![
            "/pair-pin-start",
            "/pair-setup",
            "/pair-setup",
            "/pair-setup",
            "/pair-verify",
            "/pair-verify"
        ]
    );
}

// covers: P63, P64
#[test]
fn a_transient_pairing_ends_after_m4_with_k_as_the_shared_secret() {
    let mut fake = FakeHomeKit::new("3939");
    let mut entropy = TestEntropy::new();
    let (end, retries) = run_setup(&mut PairSetup::transient(), &mut fake, &mut entropy, None);
    assert_eq!(retries, 0);
    let SetupStep::Transient(shared) = end else {
        panic!("{end:?}");
    };
    assert_eq!(shared.expose().len(), 64);
    assert_eq!(Some(shared.expose().to_vec()), fake.transient_key);
    assert!(fake.paired.is_empty(), "nothing is stored");
    assert_eq!(
        paths(&fake),
        vec![("/pair-setup".to_owned(), 1), ("/pair-setup".to_owned(), 3)]
    );
    // M1 {method 0, state 1, flags 0x10}; M3 {state 3, A (384), proof (64)}.
    let m1 = hap::untlv(&fake.requests[0].body);
    assert_eq!(
        m1,
        vec![(0, vec![0]), (6, vec![1]), (19, vec![0x10])],
        "transient M1"
    );
    let m3 = hap::untlv(&fake.requests[1].body);
    let kinds: Vec<u8> = m3.iter().map(|(kind, _)| *kind).collect();
    assert_eq!(kinds, vec![6, 3, 4]);
    assert_eq!(hap::item(&m3, 3).map(<[u8]>::len), Some(384));
    assert_eq!(hap::item(&m3, 4).map(<[u8]>::len), Some(64));
    assert_eq!(format!("{shared:?}"), "SharedSecret(64 bytes)");

    // M1 of a PIN pairing has no flags item.
    let mut fake = FakeHomeKit::new(PIN);
    paired(&mut fake, &mut entropy);
    let m1 = hap::untlv(&fake.requests[1].body);
    assert_eq!(m1, vec![(0, vec![0]), (6, vec![1])]);

    // A wrong M4 proof is an authentication error.
    let mut fake = FakeHomeKit::new("3939");
    fake.wrong_m4_proof = true;
    let (end, _) = run_setup(&mut PairSetup::transient(), &mut fake, &mut entropy, None);
    assert_eq!(failed_kind(&end), ErrorKind::PairingRequired);

    // An empty 200 to M1, or one without a state 2 item: no HomeKit pairing.
    for answer in [Answer::Empty, Answer::Error(0)] {
        let mut fake = FakeHomeKit::new("3939");
        fake.answers.push((("/pair-setup", 1), answer));
        let (end, _) = run_setup(&mut PairSetup::transient(), &mut fake, &mut entropy, None);
        let expected = match answer {
            Answer::Empty => ErrorKind::Unsupported(UnsupportedReason::Encryption),
            _ => ErrorKind::PairingFailed,
        };
        assert_eq!(failed_kind(&end), expected, "{answer:?}");
        assert_eq!(fake.requests.len(), 1);
    }
    let mut setup = PairSetup::transient();
    setup.start();
    let mut legacy = Response::new(200, "OK");
    legacy.body = vec![0x41; 32];
    assert_eq!(
        failed_kind(&setup.handle_response(&legacy, &mut entropy)),
        ErrorKind::Unsupported(UnsupportedReason::Encryption)
    );
}

// covers: P151
#[test]
fn srp_draws_a_until_a_is_full_and_retries_once_after_a_short_b() {
    // An a whose A has a leading zero byte is drawn again.
    let mut short_a = None;
    for i in 0u32..4096 {
        let a = hap::sha512(&[b"a", &i.to_be_bytes()])[..32].to_vec();
        if hap::public_of(&a)[0] == 0 {
            short_a = Some(a);
            break;
        }
    }
    let short_a = short_a.expect("an a with a short A");
    let good_a = vec![0x11; 32];
    assert_ne!(hap::public_of(&good_a)[0], 0);
    let mut entropy = TestEntropy::scripted(vec![short_a, good_a.clone()]);
    let mut fake = FakeHomeKit::new("3939");
    let (end, _) = run_setup(&mut PairSetup::transient(), &mut fake, &mut entropy, None);
    assert!(matches!(end, SetupStep::Transient(_)), "{end:?}");
    let m3 = hap::untlv(&fake.requests[1].body);
    assert_eq!(hap::item(&m3, 3), Some(hap::public_of(&good_a).as_slice()));
    assert_eq!(entropy.fills, 2);

    // A receiver that pads B and S computes another proof when B starts
    // with a zero byte: one retry on a new connection, then success.
    let mut fake = FakeHomeKit::new("3939");
    fake.hash_padded = true;
    fake.zero_led_b = 1;
    let mut entropy = TestEntropy::new();
    let (end, retries) = run_setup(&mut PairSetup::transient(), &mut fake, &mut entropy, None);
    assert!(matches!(end, SetupStep::Transient(_)), "{end:?}");
    assert_eq!((retries, fake.connections), (1, 2));
    assert_eq!(fake.requests.len(), 4);

    // A password pairing retries with the same password.
    let mut fake = FakeHomeKit::new(PASSWORD);
    fake.hash_padded = true;
    fake.zero_led_b = 1;
    let mut setup = PairSetup::password(Secret::from(PASSWORD), identity(&mut entropy));
    let (end, retries) = run_setup(&mut setup, &mut fake, &mut entropy, None);
    assert!(matches!(end, SetupStep::Paired(_)), "{end:?}");
    assert_eq!(retries, 1);

    // A full-length B but an S with a leading zero byte: the padding
    // receiver answers M3 with error 2, and the sender retries once from M1.
    let mut fake = FakeHomeKit::new("3939");
    fake.hash_padded = true;
    let mut setup = PairSetup::transient();
    let m1 = setup.start();
    let m2 = fake.answer(&m1);
    assert_ne!(fake.b_pub()[0], 0, "B is full length");
    let short_s = (0u32..8192)
        .map(|i| hap::sha512(&[b"s", &i.to_be_bytes()])[..32].to_vec())
        .find(|a| hap::public_of(a)[0] != 0 && fake.premaster_of(a)[0] == 0)
        .expect("an a with a short S");
    let mut entropy = TestEntropy::scripted(vec![short_s]);
    let SetupStep::Send(m3) = setup.handle_response(&m2, &mut entropy) else {
        panic!("M3");
    };
    let answer = fake.answer(&m3);
    assert_eq!(hap::item(&hap::untlv(&answer.body), 7), Some(&[2][..]));
    assert_eq!(
        setup.handle_response(&answer, &mut entropy),
        SetupStep::Retry
    );
    fake.new_connection();
    let mut request = setup.restart();
    assert_eq!(hap::untlv(&request.body), hap::untlv(&m1.body), "M1 again");
    let end = loop {
        let response = fake.answer(&request);
        match setup.handle_response(&response, &mut entropy) {
            SetupStep::Send(next) => request = next,
            end => break end,
        }
    };
    assert!(matches!(end, SetupStep::Transient(_)), "{end:?}");
    assert_eq!(fake.connections, 2);

    // A retry that fails the same way is mapped as P150 says.
    let mut fake = FakeHomeKit::new("3939");
    fake.hash_padded = true;
    fake.zero_led_b = 2;
    let (end, retries) = run_setup(&mut PairSetup::transient(), &mut fake, &mut entropy, None);
    assert_eq!(retries, 1);
    assert_eq!(failed_kind(&end), ErrorKind::PairingRequired);

    // A PIN pairing doesn't retry by itself: it asks for the PIN again.
    let mut fake = FakeHomeKit::new(PIN);
    fake.hash_padded = true;
    fake.zero_led_b = 1;
    let mut setup = PairSetup::pin(identity(&mut entropy));
    let (end, retries) = run_setup(&mut setup, &mut fake, &mut entropy, Some(PIN));
    assert_eq!(retries, 0);
    assert_eq!(end, SetupStep::WrongPin);

    // Without a short B or S, error 2 is mapped at once: no retry.
    let mut fake = FakeHomeKit::new("3939");
    fake.answers.push((("/pair-setup", 3), Answer::Error(2)));
    let (end, retries) = run_setup(&mut PairSetup::transient(), &mut fake, &mut entropy, None);
    assert_eq!(retries, 0);
    assert_eq!(failed_kind(&end), ErrorKind::PairingRequired);
}

// covers: P65, P152
#[test]
fn pin_and_password_pairings_seal_m5_and_open_m6() {
    let mut entropy = TestEntropy::new();
    let mut fake = FakeHomeKit::new(PIN);
    let me = identity(&mut entropy);
    let id = me.id().to_owned();
    let public = me.public_key();
    let mut setup = PairSetup::pin(me);
    let (end, _) = run_setup(&mut setup, &mut fake, &mut entropy, Some(PIN));
    let SetupStep::Paired(credentials) = end else {
        panic!("{end:?}");
    };
    // The fake opened M5 with its own keys and checked the signature.
    assert_eq!(fake.paired, vec![(id.as_bytes().to_vec(), public)]);
    assert_eq!(credentials.receiver_id(), fake.id.as_slice());
    assert_eq!(credentials.receiver_key(), fake.public_key());
    assert_eq!(credentials.identity().id(), id);
    let m5 = hap::untlv(&fake.requests[3].body);
    let kinds: Vec<u8> = m5.iter().map(|(kind, _)| *kind).collect();
    assert_eq!(kinds, vec![6, 5]);
    assert_eq!(hap::item(&m5, 6), Some(&[5][..]));
    // identifier (2 + 36), key (2 + 32), signature (2 + 64), tag 16.
    assert_eq!(hap::item(&m5, 5).map(<[u8]>::len), Some(38 + 34 + 66 + 16));

    // The identifier is a random version 4 UUID in its 36-character form.
    assert_eq!(id.len(), homekit::IDENTIFIER_CHARS);
    let groups: Vec<usize> = id.split('-').map(str::len).collect();
    assert_eq!(groups, vec![8, 4, 4, 4, 12]);
    assert_eq!(id.as_bytes()[14], b'4');
    assert!(matches!(id.as_bytes()[19], b'8' | b'9' | b'a' | b'b'));
    assert_ne!(identity(&mut entropy).id(), id);

    // The credentials round-trip through the host's storage, and their
    // Debug form shows no key.
    let blob = credentials.to_bytes();
    assert_eq!(
        HomeKitCredentials::from_bytes(&blob),
        Ok(credentials.clone())
    );
    assert!(HomeKitCredentials::from_bytes(&blob[..blob.len() - 18]).is_err());
    assert!(HomeKitCredentials::from_bytes(&[2]).is_err());
    let debug = format!("{credentials:?}");
    assert!(debug.contains(&id), "{debug}");
    assert!(!debug.contains("seed"), "{debug}");

    // A bad M6 tag or signature ends the pairing with PairingFailed.
    for tamper in [0, 1] {
        let mut fake = FakeHomeKit::new(PIN);
        if tamper == 0 {
            fake.wrong_m6_tag = true;
        } else {
            fake.wrong_m6_signature = true;
        }
        let mut setup = PairSetup::pin(identity(&mut entropy));
        let (end, _) = run_setup(&mut setup, &mut fake, &mut entropy, Some(PIN));
        assert_eq!(failed_kind(&end), ErrorKind::PairingFailed, "{tamper}");
    }

    // A password pairing: no /pair-pin-start, the password as P.
    let mut fake = FakeHomeKit::new(PASSWORD);
    let mut setup = PairSetup::password(Secret::from(PASSWORD), identity(&mut entropy));
    let (end, _) = run_setup(&mut setup, &mut fake, &mut entropy, None);
    assert!(matches!(end, SetupStep::Paired(_)), "{end:?}");
    assert!(fake.requests.iter().all(|r| r.uri == "/pair-setup"));
    // A wrong password: the receiver answers M3 with error 2.
    let mut fake = FakeHomeKit::new(PASSWORD);
    let mut setup = PairSetup::password(Secret::from("wrong"), identity(&mut entropy));
    let (end, _) = run_setup(&mut setup, &mut fake, &mut entropy, None);
    assert_eq!(failed_kind(&end), ErrorKind::WrongPassword);
    // A wrong PIN is asked for again; the right one then pairs from M1,
    // with the code still shown (no second /pair-pin-start).
    let mut fake = FakeHomeKit::new(PIN);
    let mut setup = PairSetup::pin(identity(&mut entropy));
    let (end, _) = run_setup(&mut setup, &mut fake, &mut entropy, Some("0000"));
    assert_eq!(end, SetupStep::WrongPin);
    let mut request = setup.submit_pin(PIN).expect("M1 again");
    let end = loop {
        match setup.handle_response(&fake.answer(&request), &mut entropy) {
            SetupStep::Send(next) => request = next,
            end => break end,
        }
    };
    assert!(matches!(end, SetupStep::Paired(_)), "{end:?}");
    let starts = fake
        .requests
        .iter()
        .filter(|r| r.uri == "/pair-pin-start")
        .count();
    assert_eq!(starts, 1);
    // A second wrong PIN in a row asks the receiver for a new code.
    let mut fake = FakeHomeKit::new(PIN);
    let mut setup = PairSetup::pin(identity(&mut entropy));
    let (end, _) = run_setup(&mut setup, &mut fake, &mut entropy, Some("0000"));
    assert_eq!(end, SetupStep::WrongPin);
    let mut request = setup.submit_pin("1111").expect("M1 again");
    let end = loop {
        match setup.handle_response(&fake.answer(&request), &mut entropy) {
            SetupStep::Send(next) if next.uri == "/pair-pin-start" => break SetupStep::Send(next),
            SetupStep::Send(next) => request = next,
            end => break end,
        }
    };
    let SetupStep::Send(start) = end else {
        panic!("{end:?}");
    };
    assert_eq!(
        setup.handle_response(&fake.answer(&start), &mut entropy),
        SetupStep::PinRequested
    );
    // A PIN can only be given after /pair-pin-start was answered.
    let mut setup = PairSetup::pin(identity(&mut entropy));
    assert!(setup.submit_pin(PIN).is_none());
}

// covers: P66, P154
#[test]
fn pair_verify_checks_m2_and_ends_with_the_x25519_secret() {
    let mut entropy = TestEntropy::new();
    let mut fake = FakeHomeKit::new(PIN);
    let credentials = paired(&mut fake, &mut entropy);
    let before = fake.requests.len();
    let mut verify = PairVerify::new(credentials.clone(), &mut entropy).expect("entropy");
    let end = run_verify(&mut verify, &mut fake);
    let VerifyStep::Verified(shared) = end else {
        panic!("{end:?}");
    };
    assert_eq!(
        Some(shared.expose().to_vec()),
        fake.verify_shared.map(|s| s.to_vec())
    );
    let sent = &fake.requests[before..];
    assert_eq!(sent.len(), 2);
    let m1 = hap::untlv(&sent[0].body);
    assert_eq!(hap::item(&m1, 6), Some(&[1][..]));
    assert_eq!(hap::item(&m1, 3).map(<[u8]>::len), Some(32));
    let m3 = hap::untlv(&sent[1].body);
    assert_eq!(hap::item(&m3, 6), Some(&[3][..]));
    // identifier (2 + 36), signature (2 + 64) and the tag: the 120 bytes of
    // the published capture.
    assert_eq!(hap::item(&m3, 5).map(<[u8]>::len), Some(120));

    // A fresh X25519 key for each verification.
    let mut again = PairVerify::new(credentials.clone(), &mut entropy).expect("entropy");
    let first = again.start();
    assert_ne!(
        hap::item(&hap::untlv(&first.body), 3),
        hap::item(&m1, 3),
        "a new key"
    );

    // A bad tag, signature or identifier in M2: rejected credentials, and
    // no M3 goes out.
    for tamper in 0..3 {
        let mut fake = FakeHomeKit::new(PIN);
        let credentials = paired(&mut fake, &mut entropy);
        match tamper {
            0 => fake.wrong_m2_tag = true,
            1 => fake.wrong_m2_signature = true,
            _ => fake.wrong_m2_identifier = true,
        }
        let before = fake.requests.len();
        let mut verify = PairVerify::new(credentials.clone(), &mut entropy).expect("entropy");
        let end = run_verify(&mut verify, &mut fake);
        let VerifyStep::Failed(failure) = end else {
            panic!("{end:?}");
        };
        assert_eq!(failure.error.kind, ErrorKind::PairingRequired, "{tamper}");
        assert_eq!(fake.requests.len() - before, 1, "no M3");
        // The credentials stay as they were; deleting them is the host's.
        assert_eq!(
            HomeKitCredentials::from_bytes(&credentials.to_bytes()),
            Ok(credentials)
        );
    }

    // Credentials from another pairing, or a receiver that forgot them.
    let mut fake = FakeHomeKit::new(PIN);
    let credentials = paired(&mut fake, &mut entropy);
    fake.paired.clear();
    let mut verify = PairVerify::new(credentials.clone(), &mut entropy).expect("entropy");
    let end = run_verify(&mut verify, &mut fake);
    assert!(
        matches!(&end, VerifyStep::Failed(f) if f.error.kind == ErrorKind::PairingRequired),
        "{end:?}"
    );

    // Error 2, a 470 or another 4xx means rejected credentials; error 7 and
    // 409 are Busy; any other error item describes the receiver's state
    // (P150); 401 is a challenge (P58), and a 5xx is a passing fault that
    // keeps them (P154).
    for (answer, expected) in [
        (Answer::Error(7), Some(ErrorKind::Busy)),
        (Answer::Error(2), Some(ErrorKind::PairingRequired)),
        (Answer::Error(1), Some(ErrorKind::PairingFailed)),
        (Answer::Error(3), Some(ErrorKind::PairingFailed)),
        (Answer::Error(4), Some(ErrorKind::PairingFailed)),
        (Answer::Error(5), Some(ErrorKind::PairingFailed)),
        (Answer::Error(6), Some(ErrorKind::PairingFailed)),
        (Answer::Error(9), Some(ErrorKind::PairingFailed)),
        (Answer::EmptyError, Some(ErrorKind::PairingFailed)),
        (Answer::Status(470), Some(ErrorKind::PairingRequired)),
        (Answer::Status(403), Some(ErrorKind::PairingRequired)),
        (Answer::Status(500), Some(ErrorKind::Refused)),
        (Answer::Status(503), Some(ErrorKind::Refused)),
        (Answer::Status(409), Some(ErrorKind::Busy)),
        (Answer::Status(401), None),
    ] {
        for state in [1, 3] {
            let mut fake = FakeHomeKit::new(PIN);
            let credentials = paired(&mut fake, &mut entropy);
            fake.answers.push((("/pair-verify", state), answer));
            let mut verify = PairVerify::new(credentials, &mut entropy).expect("entropy");
            let end = run_verify(&mut verify, &mut fake);
            match (expected, &end) {
                (Some(kind), VerifyStep::Failed(failure)) => {
                    assert_eq!(failure.error.kind, kind, "{answer:?} at {state}");
                }
                (None, VerifyStep::Challenge) => {
                    let last = verify.last_request().expect("the request");
                    assert_eq!(last, fake.requests.last().expect("sent"));
                }
                _ => panic!("{answer:?} at {state}: {end:?}"),
            }
        }
    }
}

// A malformed M2 fails P66's checks and rejects the credentials; a malformed
// M4 comes after the receiver proved itself, so it is a protocol error that
// keeps them.
// covers: P66, P154
#[test]
fn a_malformed_verify_answer_rejects_the_credentials_only_at_m2() {
    let mut entropy = TestEntropy::new();
    for (state, expected) in [
        (1, ErrorKind::PairingRequired),
        (3, ErrorKind::ProtocolError),
    ] {
        let mut fake = FakeHomeKit::new(PIN);
        let credentials = paired(&mut fake, &mut entropy);
        fake.answers
            .push((("/pair-verify", state), Answer::Malformed));
        let mut verify = PairVerify::new(credentials, &mut entropy).expect("entropy");
        let VerifyStep::Failed(failure) = run_verify(&mut verify, &mut fake) else {
            panic!("a failure at {state}");
        };
        assert_eq!(failure.error.kind, expected, "{state}");
    }
}

// Only error 2 speaks about the credentials; a back-off, a full peer list,
// too many tries or an unavailable receiver is retried with them kept.
// covers: P150, P154
#[test]
fn pair_verify_errors_about_the_receivers_state_are_retried() {
    let mut entropy = TestEntropy::new();
    let mut fake = FakeHomeKit::new(PIN);
    let credentials = paired(&mut fake, &mut entropy);
    fake.answers.push((("/pair-verify", 1), Answer::Error(3)));
    fake.back_off = Some(30);
    let mut verify = PairVerify::new(credentials, &mut entropy).expect("entropy");
    let VerifyStep::Failed(failure) = run_verify(&mut verify, &mut fake) else {
        panic!("a failure");
    };
    assert_eq!(failure.error.kind, ErrorKind::PairingFailed);
    assert_eq!(failure.back_off, Some(Duration::from_secs(30)));
    assert_eq!(failure.error.retry_after, Some(Duration::from_secs(30)));
    for code in [1, 4, 5, 6] {
        let mut fake = FakeHomeKit::new(PIN);
        let credentials = paired(&mut fake, &mut entropy);
        fake.answers
            .push((("/pair-verify", 3), Answer::Error(code)));
        let mut verify = PairVerify::new(credentials, &mut entropy).expect("entropy");
        let VerifyStep::Failed(failure) = run_verify(&mut verify, &mut fake) else {
            panic!("a failure for {code}");
        };
        assert_eq!(failure.error.kind, ErrorKind::PairingFailed, "{code}");
        assert_eq!(failure.error.retry_after, Some(Duration::ZERO), "{code}");
    }
    let mut fake = FakeHomeKit::new(PIN);
    let credentials = paired(&mut fake, &mut entropy);
    fake.answers.push((("/pair-verify", 1), Answer::Error(2)));
    fake.back_off = Some(30);
    let mut verify = PairVerify::new(credentials, &mut entropy).expect("entropy");
    let VerifyStep::Failed(failure) = run_verify(&mut verify, &mut fake) else {
        panic!("a failure");
    };
    assert_eq!(failure.error.kind, ErrorKind::PairingRequired);
    assert_eq!(failure.error.retry_after, None);
}

// covers: P150
#[test]
fn pair_setup_error_items_map_to_session_errors() {
    let mut entropy = TestEntropy::new();
    let cases: [(u8, ErrorKind, ErrorKind, ErrorKind, &str); 7] = [
        (
            2,
            ErrorKind::PairingFailed,
            ErrorKind::WrongPassword,
            ErrorKind::PairingRequired,
            "authentication",
        ),
        (
            3,
            ErrorKind::PairingFailed,
            ErrorKind::PairingFailed,
            ErrorKind::PairingFailed,
            "back-off",
        ),
        (
            4,
            ErrorKind::PairingFailed,
            ErrorKind::PairingFailed,
            ErrorKind::PairingFailed,
            "too many paired senders",
        ),
        (
            5,
            ErrorKind::PairingFailed,
            ErrorKind::PairingFailed,
            ErrorKind::PairingFailed,
            "too many attempts",
        ),
        (
            6,
            ErrorKind::PairingFailed,
            ErrorKind::PairingFailed,
            ErrorKind::PairingFailed,
            "unavailable",
        ),
        (7, ErrorKind::Busy, ErrorKind::Busy, ErrorKind::Busy, "busy"),
        (
            1,
            ErrorKind::PairingFailed,
            ErrorKind::PairingFailed,
            ErrorKind::PairingFailed,
            "unknown pairing error",
        ),
    ];
    for (code, pin, password, transient, detail) in cases {
        for (state, kind) in [
            (1, 0),
            (3, 0),
            (1, 1),
            (3, 1),
            (1, 2),
            (3, 2),
            (5, 0),
            (5, 1),
        ] {
            let mut fake = FakeHomeKit::new(match kind {
                0 => PIN,
                1 => PASSWORD,
                _ => "3939",
            });
            fake.answers
                .push((("/pair-setup", state), Answer::Error(code)));
            fake.back_off = Some(30);
            let mut setup = match kind {
                0 => PairSetup::pin(identity(&mut entropy)),
                1 => PairSetup::password(Secret::from(PASSWORD), identity(&mut entropy)),
                _ => PairSetup::transient(),
            };
            let (end, retries) = run_setup(&mut setup, &mut fake, &mut entropy, Some(PIN));
            assert_eq!(retries, 0);
            // A PIN turned down at M4 is asked for again (P150).
            if (code, state, kind) == (2, 3, 0) {
                assert_eq!(end, SetupStep::WrongPin);
                continue;
            }
            // A back-off in a PIN pairing waits, then asks again (P150).
            if (code, kind) == (3, 0) {
                assert_eq!(end, SetupStep::BackOff(Duration::from_secs(30)));
                continue;
            }
            let SetupStep::Failed(failure) = end else {
                panic!("{code} at {state}: {end:?}");
            };
            let expected = [pin, password, transient][kind];
            assert_eq!(
                failure.error.kind, expected,
                "error {code} at state {state}"
            );
            assert_eq!(failure.error.detail, Some(detail));
            let wait = (code == 3).then_some(Duration::from_secs(30));
            assert_eq!(failure.back_off, wait);
        }
    }
    // An error item refuses the step whatever its state item says.
    let mut setup = PairSetup::transient();
    setup.start();
    let mut response = Response::new(200, "OK");
    response.body = hap::tlv(&[(6, &[2]), (7, &[6]), (2, &[1; 16]), (3, &[2; 384])]);
    assert_eq!(
        failed_kind(&setup.handle_response(&response, &mut entropy)),
        ErrorKind::PairingFailed
    );
    // A non-200 answer to /pair-pin-start or pair-setup: PairingFailed,
    // except 409 (Busy) and 401 (a challenge with the same request kept).
    for (path, state) in [
        ("/pair-pin-start", 0),
        ("/pair-setup", 1),
        ("/pair-setup", 5),
    ] {
        for (status, expected) in [
            (500, Some(ErrorKind::PairingFailed)),
            (470, Some(ErrorKind::PairingFailed)),
            (409, Some(ErrorKind::Busy)),
            (401, None),
        ] {
            let mut fake = FakeHomeKit::new(PIN);
            fake.answers.push(((path, state), Answer::Status(status)));
            let mut setup = PairSetup::pin(identity(&mut entropy));
            let (end, _) = run_setup(&mut setup, &mut fake, &mut entropy, Some(PIN));
            match expected {
                Some(kind) => {
                    let SetupStep::Failed(failure) = &end else {
                        panic!("{path} {status}: {end:?}");
                    };
                    assert_eq!(failure.error.kind, kind, "{path} {status}");
                    assert_eq!(failure.error.status, Some(status));
                }
                None => {
                    assert_eq!(end, SetupStep::Challenge, "{path}");
                    let last = setup.last_request().expect("the request");
                    assert_eq!(last, fake.requests.last().expect("sent"));
                }
            }
        }
    }
}

/// A fixed-seed xorshift, so a failure shows up the same way on every run.
struct Xorshift(u64);

impl Xorshift {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn bytes(&mut self, len: usize) -> Vec<u8> {
        (0..len).map(|_| self.next() as u8).collect()
    }
}

/// Variants of a real answer: cut at many lengths, single bits flipped,
/// and random bodies.
fn variants(real: &[u8], rng: &mut Xorshift) -> Vec<Vec<u8>> {
    let mut out: Vec<Vec<u8>> = (0..real.len())
        .step_by(7)
        .map(|n| real[..n].to_vec())
        .collect();
    for _ in 0..24 {
        let mut mutated = real.to_vec();
        if !mutated.is_empty() {
            let at = rng.next() as usize % mutated.len();
            mutated[at] ^= 1 << (rng.next() % 8);
        }
        out.push(mutated);
        let len = rng.next() as usize % 600;
        out.push(rng.bytes(len));
    }
    out
}

/// A PIN pairing driven through its first `steps` answers from the fake,
/// with the request for the next one.
fn pin_setup_at(steps: usize, entropy: &mut TestEntropy) -> (PairSetup, FakeHomeKit, Request) {
    let mut fake = FakeHomeKit::new(PIN);
    let mut setup = PairSetup::pin(identity(entropy));
    let mut request = setup.start();
    for _ in 0..steps {
        let response = fake.answer(&request);
        request = match setup.handle_response(&response, entropy) {
            SetupStep::Send(next) => next,
            SetupStep::PinRequested => setup.submit_pin(PIN).expect("M1"),
            other => panic!("{other:?}"),
        };
    }
    (setup, fake, request)
}

// covers: P140
#[test]
fn no_pairing_answer_makes_the_pairing_panic() {
    let mut rng = Xorshift(0x9E37_79B9_7F4A_7C15);
    let mut entropy = TestEntropy::new();
    // Pair-setup: the real answer at each step of a PIN pairing, replaced.
    for steps in 0..4 {
        let (_, mut fake, request) = pin_setup_at(steps, &mut entropy);
        let real = fake.answer(&request).body;
        for body in variants(&real, &mut rng) {
            let (mut setup, _, _) = pin_setup_at(steps, &mut entropy);
            let mut response = Response::new(200, "OK");
            response.body = body;
            let _ = setup.handle_response(&response, &mut entropy);
            // An answer after the end is refused, not read.
            let _ = setup.handle_response(&response, &mut entropy);
        }
    }
    // Pair-verify: M2 and M4 replaced.
    let mut fake = FakeHomeKit::new(PIN);
    let credentials = paired(&mut fake, &mut entropy);
    for step in 0..2 {
        let mut verify = PairVerify::new(credentials.clone(), &mut entropy).expect("entropy");
        let mut request = verify.start();
        if step == 1 {
            let response = fake.answer(&request);
            let VerifyStep::Send(next) = verify.handle_response(&response) else {
                panic!("M3");
            };
            request = next;
        }
        let real = fake.answer(&request).body;
        for body in variants(&real, &mut rng) {
            let mut copy = PairVerify::new(credentials.clone(), &mut entropy).expect("entropy");
            let first = copy.start();
            if step == 1 {
                let m2 = fake.answer(&first);
                assert!(matches!(copy.handle_response(&m2), VerifyStep::Send(_)));
            }
            let mut response = Response::new(200, "OK");
            response.body = body;
            let _ = copy.handle_response(&response);
            let _ = copy.handle_response(&response);
        }
    }
    for len in 0..64 {
        let _ = Tlv8::decode(&rng.bytes(len));
        let _ = HomeKitCredentials::from_bytes(&rng.bytes(len * 2));
    }
}

/// Like `run_setup`, with M2's arithmetic handed out and run apart, as the
/// sender does on another thread; counts the handed-out steps.
fn run_offloaded(
    setup: &mut PairSetup,
    fake: &mut FakeHomeKit,
    entropy: &mut TestEntropy,
    pin: Option<&str>,
) -> (SetupStep, usize) {
    setup.set_offload(true);
    let mut request = setup.start();
    let mut computed = 0;
    loop {
        let response = fake.answer(&request);
        let mut step = setup.handle_response(&response, entropy);
        if let SetupStep::Compute(work) = step {
            computed += 1;
            step = setup.computed(work.clone().run()).expect("M3 or a failure");
            assert_eq!(setup.computed(work.run()), None);
        }
        match step {
            SetupStep::Send(next) => request = next,
            SetupStep::PinRequested => {
                request = setup.submit_pin(pin.expect("a PIN")).expect("M1");
            }
            end => return (end, computed),
        }
    }
}

// covers: P223
#[test]
fn the_srp_step_can_run_apart_from_the_session() {
    let mut entropy = TestEntropy::new();
    let mut fake = FakeHomeKit::new(homekit::TRANSIENT_CODE);
    let (end, computed) = run_offloaded(&mut PairSetup::transient(), &mut fake, &mut entropy, None);
    assert!(matches!(end, SetupStep::Transient(_)), "{end:?}");
    assert_eq!(computed, 1);

    let mut fake = FakeHomeKit::new(PIN);
    let mut setup = PairSetup::pin(identity(&mut entropy));
    let (end, computed) = run_offloaded(&mut setup, &mut fake, &mut entropy, Some(PIN));
    assert!(matches!(end, SetupStep::Paired(_)), "{end:?}");
    assert_eq!(computed, 1);

    // A wrong PIN is still turned down at M4, after the step ran apart.
    let mut fake = FakeHomeKit::new(PIN);
    let mut setup = PairSetup::pin(identity(&mut entropy));
    let (end, _) = run_offloaded(&mut setup, &mut fake, &mut entropy, Some("1111"));
    assert!(matches!(end, SetupStep::WrongPin), "{end:?}");
}

fn compute(step: SetupStep) -> SrpWork {
    match step {
        SetupStep::Compute(work) => work,
        other => panic!("{other:?}"),
    }
}

// covers: P223
#[test]
fn a_result_computed_before_a_reconnect_is_dropped() {
    let mut entropy = TestEntropy::new();
    let mut fake = FakeHomeKit::new(homekit::TRANSIENT_CODE);
    let mut setup = PairSetup::transient();
    setup.set_offload(true);
    let m1 = setup.start();
    let old = compute(setup.handle_response(&fake.answer(&m1), &mut entropy));
    // The connection drops while the old step runs; the new M2 comes first.
    fake.new_connection();
    let m1 = setup.restart();
    let new = compute(setup.handle_response(&fake.answer(&m1), &mut entropy));
    assert_eq!(setup.computed(old.run()), None);
    let m3 = match setup.computed(new.run()) {
        Some(SetupStep::Send(m3)) => m3,
        other => panic!("{other:?}"),
    };
    let end = setup.handle_response(&fake.answer(&m3), &mut entropy);
    assert!(matches!(end, SetupStep::Transient(_)), "{end:?}");
}
