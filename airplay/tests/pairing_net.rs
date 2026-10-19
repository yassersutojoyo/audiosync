//! Apple TV pairing through the host API, over real sockets against a fake
//! Apple TV on loopback (P46 to P58, P136): the host starts the pairing,
//! hands in the PIN, gets the credentials, and plays with them; a wrong
//! PIN, a cancelled wait and a revoked credential reach the host as
//! events. One test at a time, generous timeouts.

mod common;

use std::time::Duration;

use audiosync_airplay::discovery::DeviceKey;
use audiosync_airplay::error::ErrorKind;
use audiosync_airplay::pairing::{Credentials, PairingEvent, PinError};
use audiosync_airplay::sender::{
    ReceiverInfo, Sender, SenderConfig, SenderEvent, SpeakerEvent, SpeakerOptions, SpeakerState,
};
use audiosync_airplay::volume::Volume;

use common::atv::FakeAppleTv;
use common::net::{Behavior, Events, HostAudio, NetReceiver, serial};

/// An Apple TV that takes PCM and asks for the PIN pairing (flags bit 3).
const ATV_TXT: &[&str] = &[
    "txtvers=1",
    "ch=2",
    "cn=0",
    "et=0",
    "sr=44100",
    "ss=16",
    "tp=UDP",
    "am=AppleTV9,2",
    "sf=0x8",
];

const PIN: &str = "4821";
const LONG: Duration = Duration::from_secs(15);

fn start() -> (Sender, HostAudio) {
    let (sender, writer) = Sender::start(SenderConfig::default()).expect("the sender starts");
    (sender, HostAudio::start(writer))
}

/// Waits for the `nth` (from 1) pairing event of `key` that matches.
fn wait_pairing(
    events: &mut Events,
    sender: &Sender,
    key: &DeviceKey,
    nth: usize,
    wanted: impl Fn(&PairingEvent) -> bool,
) -> Option<PairingEvent> {
    let deadline = std::time::Instant::now() + LONG;
    loop {
        events.drain(sender);
        let found = events
            .seen
            .iter()
            .filter_map(|(_, event)| match event {
                SenderEvent::Pairing { key: k, event } if k == key && wanted(event) => {
                    Some(event.clone())
                }
                _ => None,
            })
            .nth(nth - 1);
        if found.is_some() {
            return found;
        }
        if std::time::Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn speaker_errors(events: &Events, key: &DeviceKey) -> Vec<ErrorKind> {
    events
        .of(key)
        .into_iter()
        .filter_map(|event| match event {
            SpeakerEvent::Error(error) => Some(error.error.kind),
            _ => None,
        })
        .collect()
}

/// Pairs through the host API and returns the credentials.
fn pair(
    events: &mut Events,
    sender: &Sender,
    key: &DeviceKey,
    device: &audiosync_airplay::discovery::Device,
    nth: usize,
) -> Credentials {
    sender.start_pairing(device.clone());
    assert!(
        wait_pairing(events, sender, key, nth, |e| *e
            == PairingEvent::PinRequested)
        .is_some(),
        "the Apple TV shows a PIN"
    );
    sender.submit_pin(key, PIN).expect("digits");
    match wait_pairing(events, sender, key, 1, |e| {
        matches!(e, PairingEvent::Paired(_))
    }) {
        Some(PairingEvent::Paired(credentials)) => credentials,
        other => panic!("not paired: {other:?}"),
    }
}

// covers: P46, P54, P55
#[test]
fn the_host_pairs_an_apple_tv_and_plays_to_it_with_the_stored_credentials() {
    let _serial = serial();
    let atv = FakeAppleTv::new(PIN);
    let receiver = NetReceiver::start(Behavior {
        apple_tv: Some(atv.clone()),
        ..Behavior::default()
    });
    let (sender, _host) = start();
    let mut events = Events::default();
    let device = receiver.device(21, ATV_TXT);
    let key = device.key.clone();

    // Without credentials discovery's verdict says the Apple TV needs
    // pairing; added as a speaker, it is asked OPTIONS first, and only then
    // reported as PairingRequired (P46).
    assert_eq!(
        ReceiverInfo::new(device.clone()).plan,
        Err(ErrorKind::PairingRequired)
    );
    sender.add_speaker(device.clone(), SpeakerOptions::new(Volume::Db(-20.0)));
    assert!(
        events
            .wait_state(&sender, &key, 1, LONG, |s| *s == SpeakerState::Failed)
            .is_some()
    );
    assert_eq!(speaker_errors(&events, &key), [ErrorKind::PairingRequired]);
    assert_eq!(receiver.log().connections.len(), 1);
    assert_eq!(receiver.log().keys(), ["OPTIONS"]);

    // The host pairs: a PIN prompt, digits only, then the credentials.
    sender.start_pairing(device.clone());
    assert!(
        wait_pairing(&mut events, &sender, &key, 1, |e| *e
            == PairingEvent::PinRequested)
        .is_some()
    );
    assert_eq!(sender.submit_pin(&key, "48 21"), Err(PinError::NotDigits));
    sender.submit_pin(&key, PIN).expect("digits");
    let credentials = match wait_pairing(&mut events, &sender, &key, 1, |e| {
        matches!(e, PairingEvent::Paired(_))
    }) {
        Some(PairingEvent::Paired(credentials)) => credentials,
        other => panic!("not paired: {other:?}"),
    };
    assert_eq!(atv.paired(), [credentials.public_key()]);
    assert_eq!(credentials.receiver_key(), Some(atv.receiver_key()));
    let pairing_requests: Vec<String> = receiver.log().keys()[1..].to_vec();
    assert_eq!(
        pairing_requests,
        [
            "OPTIONS",
            "POST /pair-pin-start",
            "POST /pair-setup-pin",
            "POST /pair-setup-pin",
            "POST /pair-setup-pin"
        ]
    );
    assert_eq!(receiver.log().connections.len(), 2, "one connection (P47)");

    // What the host stores comes back as the same credentials, and the
    // speaker plays with them, verifying first (P54, P55).
    let stored = Credentials::from_bytes(&credentials.to_bytes()).expect("a blob");
    let mut options = SpeakerOptions::new(Volume::Db(-20.0));
    options.credentials = Some(stored);
    sender.add_speaker(device.clone(), options);
    assert!(
        events
            .wait_state(&sender, &key, 1, LONG, |s| *s == SpeakerState::Playing)
            .is_some(),
        "plays with the credentials: {:?}",
        events.of(&key)
    );
    let session_requests: Vec<String> = receiver.log().keys()[6..].to_vec();
    assert_eq!(
        session_requests[..4],
        [
            "POST /pair-verify",
            "POST /pair-verify",
            "OPTIONS",
            "ANNOUNCE"
        ]
    );
    sender.shutdown();
}

// covers: P58
#[test]
fn a_close_right_after_verification_gets_one_reconnect_over_loopback() {
    let _serial = serial();
    let atv = FakeAppleTv::new(PIN);
    let receiver = NetReceiver::start(Behavior {
        apple_tv: Some(atv.clone()),
        ..Behavior::default()
    });
    let (sender, _host) = start();
    let mut events = Events::default();
    let device = receiver.device(23, ATV_TXT);
    let key = device.key.clone();
    let credentials = pair(&mut events, &sender, &key, &device, 1);
    let paired_at = receiver.log().connections.len();

    // Once: the sender connects again at once, verifies and plays.
    receiver.behavior().close_after_verify = 1;
    let mut options = SpeakerOptions::new(Volume::Db(-20.0));
    options.credentials = Some(credentials.clone());
    sender.add_speaker(device.clone(), options);
    assert!(
        events
            .wait_state(&sender, &key, 1, LONG, |s| *s == SpeakerState::Playing)
            .is_some(),
        "{:?}",
        events.of(&key)
    );
    assert!(speaker_errors(&events, &key).is_empty());
    assert_eq!(receiver.log().connections.len(), paired_at + 2);
    let keys = receiver.log().keys();
    let verify_at = keys
        .iter()
        .position(|key| key == "POST /pair-verify")
        .expect("a verify");
    assert_eq!(
        keys[verify_at..verify_at + 5],
        [
            "POST /pair-verify",
            "POST /pair-verify",
            "POST /pair-verify",
            "POST /pair-verify",
            "OPTIONS"
        ]
    );
    sender.remove_speaker(&key);
    assert!(
        events
            .wait_state(&sender, &key, 1, LONG, |s| *s == SpeakerState::Stopped)
            .is_some()
    );

    // Twice in a row: PairingRequired, and no retry.
    receiver.behavior().close_after_verify = 2;
    let mut options = SpeakerOptions::new(Volume::Db(-20.0));
    options.credentials = Some(credentials);
    sender.add_speaker(device.clone(), options);
    assert!(
        events
            .wait_state(&sender, &key, 1, LONG, |s| *s == SpeakerState::Failed)
            .is_some(),
        "{:?}",
        events.of(&key)
    );
    assert_eq!(speaker_errors(&events, &key), [ErrorKind::PairingRequired]);
    let connections = receiver.log().connections.len();
    std::thread::sleep(Duration::from_millis(1500));
    assert_eq!(receiver.log().connections.len(), connections, "no retry");
    sender.shutdown();
}

// covers: P58, P136
#[test]
fn a_wrong_pin_a_cancelled_wait_and_a_revoked_credential_reach_the_host() {
    let _serial = serial();
    let atv = FakeAppleTv::new(PIN);
    let receiver = NetReceiver::start(Behavior {
        apple_tv: Some(atv.clone()),
        ..Behavior::default()
    });
    let (sender, _host) = start();
    let mut events = Events::default();
    let device = receiver.device(22, ATV_TXT);
    let key = device.key.clone();

    // A wrong PIN.
    sender.start_pairing(device.clone());
    assert!(
        wait_pairing(&mut events, &sender, &key, 1, |e| *e
            == PairingEvent::PinRequested)
        .is_some()
    );
    sender.submit_pin(&key, "0000").expect("digits");
    match wait_pairing(&mut events, &sender, &key, 1, |e| {
        matches!(e, PairingEvent::Failed { .. })
    }) {
        Some(PairingEvent::Failed { error, wrong_pin }) => {
            assert_eq!(error.kind, ErrorKind::PairingFailed);
            assert_eq!(error.status, Some(470));
            assert!(wrong_pin);
        }
        other => panic!("{other:?}"),
    }

    // The host cancels while the PIN is awaited.
    sender.start_pairing(device.clone());
    assert!(
        wait_pairing(&mut events, &sender, &key, 2, |e| *e
            == PairingEvent::PinRequested)
        .is_some()
    );
    sender.cancel_pairing(&key);
    assert!(
        wait_pairing(&mut events, &sender, &key, 1, |e| *e
            == PairingEvent::Stopped)
        .is_some()
    );

    // The Apple TV closes the connection while the PIN is awaited.
    receiver.behavior().close_after = Some("POST /pair-setup-pin".to_owned());
    sender.start_pairing(device.clone());
    match wait_pairing(&mut events, &sender, &key, 2, |e| {
        matches!(e, PairingEvent::Failed { .. })
    }) {
        Some(PairingEvent::Failed { error, wrong_pin }) => {
            assert_eq!(error.kind, ErrorKind::PairingCancelled);
            assert!(!wrong_pin);
        }
        other => panic!("{other:?}"),
    }
    receiver.behavior().close_after = None;

    // A credential the Apple TV forgot: PairingRequired, and no retry.
    let credentials = pair(&mut events, &sender, &key, &device, 4);
    atv.revoke_all();
    let mut options = SpeakerOptions::new(Volume::Db(-20.0));
    options.credentials = Some(credentials);
    sender.add_speaker(device.clone(), options);
    assert!(
        events
            .wait_state(&sender, &key, 1, LONG, |s| *s == SpeakerState::Failed)
            .is_some(),
        "{:?}",
        events.of(&key)
    );
    assert_eq!(speaker_errors(&events, &key), [ErrorKind::PairingRequired]);
    let connections = receiver.log().connections.len();
    std::thread::sleep(Duration::from_millis(1500));
    assert_eq!(receiver.log().connections.len(), connections, "no retry");
    sender.shutdown();
}
