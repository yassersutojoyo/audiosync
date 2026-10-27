//! A speaker with both services that AirPlay 1 turns down (P218 to P221):
//! the outcome that moved it to the AirPlay 2 path stays with the device,
//! so the pairing the host starts afterwards is the AirPlay 2 PIN pairing
//! and the user gets its prompt. The fake advertises `_raop._tcp` on a
//! `NetReceiver` and `_airplay._tcp` on an `Ap2Server` under one device id.

mod common;

use std::collections::VecDeque;
use std::net::{IpAddr, Ipv4Addr};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use audiosync_airplay::discovery::{
    BrowseEvent, Browser, Device, DeviceId, DeviceKey, ScopedAddr, ServiceKind, ServiceRecord,
};
use audiosync_airplay::error::{ErrorKind, UnsupportedReason};
use audiosync_airplay::pairing::PairingEvent;
use audiosync_airplay::secret::Secret;
use audiosync_airplay::sender::{
    ReceiverEvent, Sender, SenderConfig, SenderEvent, SpeakerEvent, SpeakerOptions, SpeakerState,
};
use audiosync_airplay::volume::Volume;

use common::ap2::{Ap2Server, FakeAp2, dict, int, plist};
use common::net::{Behavior, Events, HostAudio, NetReceiver, serial};

const LONG: Duration = Duration::from_secs(15);
const PIN: &str = "4821";

const PCM_TXT: &[&str] = &[
    "txtvers=1",
    "ch=2",
    "cn=0",
    "et=0",
    "sr=44100",
    "ss=16",
    "tp=UDP",
    "md=0,1,2",
    "am=Speaker1,1",
];

/// Flags bit 9: a PIN pairing the first time only (P61).
const ONE_TIME: u32 = 0x244;
/// Flags bit 3: a code at every connection (P182).
const EVERY_TIME: u32 = 0x4c;
/// Flags bit 7 alone: a password pairing (P61).
const PASSWORD: u32 = 0x84;

/// How the fake turns AirPlay 1 audio down (P221).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Variant {
    /// OPTIONS answered with a `Public` that lacks ANNOUNCE (P27).
    Unsupported,
    /// ANNOUNCE answered with 406 (P138); `/pair-pin-start` with 404.
    Announce406,
    /// OPTIONS answered with 404, Refused (P138).
    Options404,
}

impl Variant {
    fn outcome(self) -> ErrorKind {
        match self {
            Variant::Unsupported => ErrorKind::Unsupported(UnsupportedReason::Methods),
            Variant::Announce406 | Variant::Options404 => ErrorKind::Refused,
        }
    }
}

struct Rig {
    raop: NetReceiver,
    ap2: Ap2Server,
    device: Device,
    key: DeviceKey,
    sender: Sender,
    _host: HostAudio,
    events: Events,
}

fn airplay_record(port: u16, id: u8, flags: u32) -> ServiceRecord {
    ServiceRecord {
        kind: ServiceKind::AirPlay,
        instance: format!("Speaker {id}"),
        host: format!("speaker-{id}.local"),
        port,
        txt: [
            "features=0x0,0x10040".to_owned(),
            format!("flags={flags:#x}"),
            "model=Speaker1,1".to_owned(),
            format!("deviceid=02:00:5E:00:53:{id:02X}"),
        ]
        .into_iter()
        .map(String::into_bytes)
        .collect(),
        addresses: vec![ScopedAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST))],
    }
}

fn raop_behavior(variant: Variant) -> Behavior {
    let mut behavior = Behavior::default();
    match variant {
        Variant::Unsupported => {
            behavior.public = Some("OPTIONS, GET_PARAMETER, SET_PARAMETER, POST, GET".to_owned());
        }
        Variant::Announce406 => {
            behavior.status.insert("ANNOUNCE".to_owned(), 406);
            behavior
                .status
                .insert("POST /pair-pin-start".to_owned(), 404);
        }
        Variant::Options404 => {
            behavior.status.insert("OPTIONS".to_owned(), 404);
        }
    }
    behavior
}

fn ap2_fake(flags: u32) -> FakeAp2 {
    let mut fake = FakeAp2::new();
    fake.hap.code = PIN.to_owned();
    fake.info_body = plist(dict(&[("statusFlags", int(i64::from(flags)))]));
    fake.hap.pairing_per_connection = flags & 0x8 != 0;
    fake
}

impl Rig {
    fn new(variant: Variant, flags: u32, id: u8, airplay2: bool) -> Rig {
        let raop = NetReceiver::start(raop_behavior(variant));
        let ap2 = Ap2Server::start(ap2_fake(flags));
        let device_id = DeviceId([0x02, 0x00, 0x5E, 0x00, 0x53, id]);
        let device = Device::from_records(
            DeviceKey::Id(device_id),
            Some(&raop.record(id, PCM_TXT)),
            Some(&airplay_record(ap2.address.port(), id, flags)),
        )
        .expect("device");
        assert!(device.verdict.airplay2);
        assert!(!device.is_apple_tv());
        let key = device.key.clone();
        let config = SenderConfig {
            airplay2,
            airplay2_device_id: Some([0x10, 0x20, 0x30, 0x40, 0x50, 0x60]),
            ..SenderConfig::default()
        };
        let (sender, writer) = Sender::start(config).expect("the sender starts");
        Rig {
            raop,
            ap2,
            device,
            key,
            sender,
            _host: HostAudio::start(writer),
            events: Events::default(),
        }
    }

    fn options() -> SpeakerOptions {
        SpeakerOptions::new(Volume::Db(-20.0))
    }

    fn add(&self) {
        self.sender.add_speaker(self.device.clone(), Rig::options());
    }

    fn errors(&self) -> Vec<ErrorKind> {
        self.events
            .of(&self.key)
            .into_iter()
            .filter_map(|event| match event {
                SpeakerEvent::Error(error) => Some(error.error.kind),
                _ => None,
            })
            .collect()
    }

    fn pairing_events(&self) -> Vec<PairingEvent> {
        self.events
            .seen
            .iter()
            .filter_map(|(_, event)| match event {
                SenderEvent::Pairing { key, event } if *key == self.key => Some(event.clone()),
                _ => None,
            })
            .collect()
    }

    /// Waits for the `nth` (from 1) error of the speaker of this kind.
    fn wait_error(&mut self, nth: usize, kind: ErrorKind) -> bool {
        let deadline = std::time::Instant::now() + LONG;
        loop {
            self.events.drain(&self.sender);
            if self.errors().iter().filter(|k| **k == kind).count() >= nth {
                return true;
            }
            if std::time::Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// Waits for the `nth` (from 1) pairing event that `wanted` accepts.
    fn wait_pairing(&mut self, nth: usize, wanted: impl Fn(&PairingEvent) -> bool) -> bool {
        let deadline = std::time::Instant::now() + LONG;
        loop {
            self.events.drain(&self.sender);
            if self.pairing_events().iter().filter(|e| wanted(e)).count() >= nth {
                return true;
            }
            if std::time::Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn wait_state(&mut self, nth: usize, state: SpeakerState) -> bool {
        self.events
            .wait_state(&self.sender, &self.key, nth, LONG, |s| *s == state)
            .is_some()
    }

    fn remove(&mut self, nth: usize) {
        self.sender.remove_speaker(&self.key);
        assert!(
            self.wait_state(nth, SpeakerState::Stopped),
            "{:?}",
            self.events.of(&self.key)
        );
    }

    fn raop_keys(&self) -> Vec<String> {
        self.raop.log().keys()
    }

    fn ap2_count(&self, wanted: &str) -> usize {
        self.ap2
            .fake()
            .keys()
            .iter()
            .filter(|key| *key == wanted)
            .count()
    }

    fn pin_requests(&self) -> usize {
        self.pairing_events()
            .iter()
            .filter(|event| **event == PairingEvent::PinRequested)
            .count()
    }

    fn failures(&self) -> Vec<ErrorKind> {
        self.pairing_events()
            .iter()
            .filter_map(|event| match event {
                PairingEvent::Failed { error, .. } => Some(error.kind),
                _ => None,
            })
            .collect()
    }

    /// The debug-log lines on the remembered outcome (P218).
    fn path_logs(&self) -> Vec<String> {
        self.events
            .seen
            .iter()
            .filter_map(|(_, event)| match event {
                SenderEvent::PathLog { key, line } if *key == self.key => Some(line.clone()),
                _ => None,
            })
            .collect()
    }

    /// (a): one AirPlay 1 attempt, then the AirPlay 2 session asks for
    /// pairing, with no `/pair-pin-start` sent.
    fn first_start(&mut self, variant: Variant) {
        self.add();
        assert!(
            self.wait_error(1, ErrorKind::PairingRequired),
            "{:?} {:?}",
            self.events.of(&self.key),
            self.ap2.fake().keys()
        );
        assert_eq!(
            self.errors(),
            [variant.outcome(), ErrorKind::PairingRequired]
        );
        assert_eq!(
            self.raop.log().connections.len(),
            1,
            "{:?}",
            self.raop_keys()
        );
        assert_eq!(self.ap2_count("POST /pair-pin-start"), 0);
    }

    /// The PIN pairing on the AirPlay 2 port, asked for once, whose
    /// connection the speaker's start goes on over (P153, P219). The RAOP
    /// port gets `raop_options` OPTIONS from the pairing and nothing else.
    fn pair_and_play(&mut self, pairings: usize, stored: bool, raop_options: usize) {
        let mut raop_before = self.raop_keys();
        raop_before.extend(std::iter::repeat_n("OPTIONS".to_owned(), raop_options));
        self.sender.start_pairing(self.device.clone());
        assert!(
            self.wait_pairing(pairings, |event| *event == PairingEvent::PinRequested),
            "{:?} {:?} {:?}",
            self.pairing_events(),
            self.raop_keys(),
            self.ap2.fake().keys()
        );
        assert_eq!(self.pin_requests(), pairings);
        assert_eq!(self.ap2_count("POST /pair-pin-start"), pairings);
        self.sender.submit_pin(&self.key, PIN).expect("digits");
        let done = |event: &PairingEvent| {
            matches!(
                event,
                PairingEvent::HomeKitPaired(_) | PairingEvent::PairedForSession
            )
        };
        assert!(
            self.wait_pairing(pairings, done),
            "{:?}",
            self.pairing_events()
        );
        let homekit = self
            .pairing_events()
            .iter()
            .any(|event| matches!(event, PairingEvent::HomeKitPaired(_)));
        assert_eq!(homekit, stored, "{:?}", self.pairing_events());
        assert!(self.failures().is_empty(), "{:?}", self.pairing_events());

        let verifies = self.ap2_count("POST /pair-verify");
        let connections = self.ap2.fake().connections;
        let playing = self
            .events
            .of(&self.key)
            .iter()
            .filter(|event| **event == SpeakerEvent::State(SpeakerState::Playing))
            .count();
        self.add();
        assert!(
            self.wait_state(playing + 1, SpeakerState::Playing),
            "{:?} {:?}",
            self.events.of(&self.key),
            self.ap2.fake().keys()
        );
        assert_eq!(self.ap2_count("POST /pair-verify"), verifies);
        assert_eq!(self.ap2.fake().connections, connections);
        assert_eq!(
            self.raop_keys(),
            raop_before,
            "nothing reaches the RAOP port"
        );
    }
}

fn after_a_refusal_the_pairing_takes_the_airplay2_path(variant: Variant, id: u8) {
    let _serial = serial();
    let mut rig = Rig::new(variant, ONE_TIME, id, true);
    rig.first_start(variant);
    // (b): the host lets the speaker go before it starts the pairing.
    rig.remove(1);
    rig.pair_and_play(1, true, 0);
    rig.sender.shutdown();
}

// covers: P218, P219, P221
#[test]
fn a_speaker_unsupported_on_airplay1_and_let_go_pairs_on_airplay2() {
    after_a_refusal_the_pairing_takes_the_airplay2_path(Variant::Unsupported, 81);
}

// covers: P218, P219, P221
#[test]
fn a_speaker_refused_at_announce_and_let_go_pairs_on_airplay2() {
    after_a_refusal_the_pairing_takes_the_airplay2_path(Variant::Announce406, 82);
}

// covers: P182, P219, P221
#[test]
fn an_every_time_speaker_pairs_on_airplay2_again_after_its_session_ended() {
    let _serial = serial();
    let mut rig = Rig::new(Variant::Unsupported, EVERY_TIME, 83, true);
    rig.first_start(Variant::Unsupported);
    rig.remove(1);
    rig.pair_and_play(1, false, 0);
    rig.remove(2);
    // (c): nothing was stored, so the next pairing asks for a code again,
    // on the AirPlay 2 port.
    rig.pair_and_play(2, false, 0);
    rig.sender.shutdown();
}

// covers: P220, P221
#[test]
fn a_pairing_from_the_hint_switches_to_airplay2_after_an_unsupported_options() {
    let _serial = serial();
    let mut rig = Rig::new(Variant::Unsupported, ONE_TIME, 84, true);
    // (d): no session before; the pairing asks OPTIONS once on the RAOP
    // port and goes on to the AirPlay 2 PIN pairing with no outcome shown.
    rig.pair_and_play(1, true, 1);
    assert_eq!(rig.raop_keys(), ["OPTIONS"]);
    assert!(rig.errors().is_empty(), "{:?}", rig.events.of(&rig.key));
    let logs = rig.path_logs();
    assert_eq!(logs.len(), 1, "{logs:?}");
    assert!(
        logs[0].contains("Unsupported(Methods) remembered"),
        "{logs:?}"
    );
    rig.sender.shutdown();
}

// covers: P220
#[test]
fn a_pairing_from_the_hint_switches_to_airplay2_after_a_refused_options() {
    let _serial = serial();
    let mut rig = Rig::new(Variant::Options404, ONE_TIME, 89, true);
    rig.pair_and_play(1, true, 1);
    assert_eq!(rig.raop_keys(), ["OPTIONS"]);
    assert!(rig.errors().is_empty(), "{:?}", rig.events.of(&rig.key));
    let logs = rig.path_logs();
    assert_eq!(logs.len(), 1, "{logs:?}");
    assert!(logs[0].contains("Refused remembered"), "{logs:?}");
    rig.sender.shutdown();
}

// covers: P219
#[test]
fn a_remembered_outcome_asks_for_the_password_of_a_password_speaker() {
    let _serial = serial();
    let mut rig = Rig::new(Variant::Unsupported, PASSWORD, 85, true);
    rig.add();
    assert!(
        rig.wait_error(1, ErrorKind::PasswordRequired),
        "{:?}",
        rig.events.of(&rig.key)
    );
    assert_eq!(
        rig.errors(),
        [
            ErrorKind::Unsupported(UnsupportedReason::Methods),
            ErrorKind::PasswordRequired
        ]
    );
    rig.remove(1);
    let raop_before = rig.raop_keys();
    rig.sender.start_pairing(rig.device.clone());
    assert!(
        rig.wait_pairing(1, |event| matches!(event, PairingEvent::Failed { .. })),
        "{:?}",
        rig.pairing_events()
    );
    assert_eq!(rig.failures(), [ErrorKind::PasswordRequired]);
    assert_eq!(rig.pin_requests(), 0);
    assert_eq!(rig.ap2_count("POST /pair-pin-start"), 0);
    assert_eq!(rig.raop_keys(), raop_before);

    // With the password the receiver uses for its pairing code, it pairs.
    rig.sender
        .start_pairing_with_password(rig.device.clone(), Some(Secret::new(PIN.to_owned())));
    assert!(
        rig.wait_pairing(1, |event| matches!(event, PairingEvent::HomeKitPaired(_))),
        "{:?} {:?}",
        rig.pairing_events(),
        rig.ap2.fake().keys()
    );
    assert_eq!(rig.pin_requests(), 0);
    assert_eq!(rig.ap2_count("POST /pair-pin-start"), 0);
    assert_eq!(rig.raop_keys(), raop_before);
    rig.sender.shutdown();
}

/// A browser that hands the sender scripted discovery events.
#[derive(Clone, Default)]
struct Script(Arc<Mutex<VecDeque<BrowseEvent>>>);

impl Script {
    fn push(&self, event: BrowseEvent) {
        self.0.lock().expect("script").push_back(event);
    }
}

impl Browser for Script {
    fn poll_event(&mut self) -> Option<BrowseEvent> {
        self.0.lock().expect("script").pop_front()
    }
}

fn wait_listed(rig: &mut Rig, nth: usize, raop: bool) -> bool {
    let key = rig.key.clone();
    let listed = |rig: &Rig| {
        rig.events
            .seen
            .iter()
            .filter(|(_, event)| match event {
                SenderEvent::Receiver(
                    ReceiverEvent::Appeared(info) | ReceiverEvent::Changed(info),
                ) => {
                    info.device.key == key
                        && info.device.airplay.is_some()
                        && info.device.raop.is_some() == raop
                }
                _ => false,
            })
            .count()
    };
    let deadline = std::time::Instant::now() + LONG;
    loop {
        rig.events.drain(&rig.sender);
        if listed(rig) >= nth {
            return true;
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

// covers: P218, P221
#[test]
fn a_goodbye_for_the_raop_record_forgets_the_outcome() {
    let _serial = serial();
    let mut rig = Rig::new(Variant::Unsupported, ONE_TIME, 86, true);
    let raop_record = rig.raop.record(86, PCM_TXT);
    let script = Script::default();
    script.push(BrowseEvent::Resolved(raop_record.clone()));
    script.push(BrowseEvent::Resolved(airplay_record(
        rig.ap2.address.port(),
        86,
        ONE_TIME,
    )));
    rig.sender.start_discovery_with(Box::new(script.clone()));
    assert!(wait_listed(&mut rig, 1, true), "{:?}", rig.events.seen);

    rig.first_start(Variant::Unsupported);
    rig.remove(1);
    // Remembered: the host's next start goes straight to AirPlay 2.
    rig.add();
    assert!(rig.wait_error(2, ErrorKind::PairingRequired));
    assert_eq!(rig.raop_keys(), ["OPTIONS"]);
    rig.remove(2);

    // (e): the RAOP record says goodbye and comes back.
    script.push(BrowseEvent::Removed {
        kind: ServiceKind::Raop,
        instance: raop_record.instance.clone(),
    });
    assert!(wait_listed(&mut rig, 1, false), "{:?}", rig.events.seen);
    script.push(BrowseEvent::Resolved(raop_record));
    assert!(wait_listed(&mut rig, 2, true), "{:?}", rig.events.seen);
    rig.add();
    assert!(
        rig.wait_error(3, ErrorKind::PairingRequired),
        "{:?}",
        rig.events.of(&rig.key)
    );
    assert_eq!(rig.raop_keys(), ["OPTIONS", "OPTIONS"]);
    assert_eq!(
        rig.errors(),
        [
            ErrorKind::Unsupported(UnsupportedReason::Methods),
            ErrorKind::PairingRequired,
            ErrorKind::PairingRequired,
            ErrorKind::Unsupported(UnsupportedReason::Methods),
            ErrorKind::PairingRequired,
        ]
    );
    let logs = rig.path_logs();
    assert_eq!(logs.len(), 3, "{logs:?}");
    assert!(logs[0].contains("remembered"), "{logs:?}");
    assert!(
        logs[1].contains("forgotten") && logs[1].contains("_raop._tcp"),
        "{logs:?}"
    );
    assert!(logs[2].contains("remembered"), "{logs:?}");
    rig.sender.shutdown();
}

// covers: P220, P221
#[test]
fn a_refusal_at_announce_takes_the_known_path_to_the_airplay2_pin() {
    let _serial = serial();
    let mut rig = Rig::new(Variant::Announce406, ONE_TIME, 87, true);
    // (f): a fresh run; the pairing from the hint goes the legacy way and
    // the fake has no legacy pairing on its RAOP port.
    rig.sender.start_pairing(rig.device.clone());
    assert!(
        rig.wait_pairing(1, |event| matches!(event, PairingEvent::Failed { .. })),
        "{:?}",
        rig.pairing_events()
    );
    assert_eq!(rig.failures(), [ErrorKind::PairingFailed]);
    assert_eq!(rig.pin_requests(), 0);
    assert_eq!(rig.raop_keys(), ["OPTIONS", "POST /pair-pin-start"]);
    assert!(
        rig.ap2.fake().keys().is_empty(),
        "{:?}",
        rig.ap2.fake().keys()
    );

    // The stream attempt meets the 406 and moves to AirPlay 2.
    rig.add();
    assert!(rig.wait_error(1, ErrorKind::PairingRequired));
    assert_eq!(
        rig.errors(),
        [ErrorKind::Refused, ErrorKind::PairingRequired]
    );
    let raop_before = rig.raop_keys();
    assert_eq!(raop_before.iter().filter(|k| *k == "ANNOUNCE").count(), 1);

    // The next pairing: nothing to the RAOP port, one PIN prompt.
    rig.sender.start_pairing(rig.device.clone());
    assert!(rig.wait_pairing(1, |event| *event == PairingEvent::PinRequested));
    assert_eq!(rig.pin_requests(), 1);
    assert_eq!(rig.ap2_count("POST /pair-pin-start"), 1);
    assert_eq!(rig.raop_keys(), raop_before);
    rig.sender.shutdown();
}

// covers: P170, P218, P221
#[test]
fn with_the_switch_off_the_pairing_stays_legacy_and_the_outcome_is_reported() {
    let _serial = serial();
    let mut rig = Rig::new(Variant::Unsupported, ONE_TIME, 88, false);
    rig.add();
    assert!(
        rig.wait_error(1, ErrorKind::Unsupported(UnsupportedReason::Methods)),
        "{:?}",
        rig.events.of(&rig.key)
    );
    rig.remove(1);
    rig.sender.start_pairing(rig.device.clone());
    assert!(
        rig.wait_pairing(1, |event| matches!(event, PairingEvent::Failed { .. })),
        "{:?}",
        rig.pairing_events()
    );
    assert_eq!(
        rig.failures(),
        [ErrorKind::Unsupported(UnsupportedReason::Methods)]
    );
    assert_eq!(rig.raop_keys(), ["OPTIONS", "OPTIONS"]);
    assert!(
        rig.ap2.fake().keys().is_empty(),
        "{:?}",
        rig.ap2.fake().keys()
    );
    assert_eq!(rig.ap2.fake().connections, 0);
    rig.sender.shutdown();
}
