//! The AirPlay 2 session control path (sections 4 and 6 of the spec)
//! against the sans-I/O fake receiver of `common::ap2`: framing, the
//! request order and bodies, pairing on the session connection, the event
//! connection, `/feedback`, the NTP-first timing rule and TEARDOWN.

mod common;

use std::net::SocketAddr;
use std::time::{Duration, Instant};

use audiosync_airplay::ap2::framing::{Framing, MAX_FRAME_PLAINTEXT, frame_nonce};
use audiosync_airplay::ap2::{
    Ap2Event, Ap2Output, Ap2Session, Ap2State, PtpPeer, StreamPorts, Timing,
};
use audiosync_airplay::error::{ErrorKind, UnsupportedReason};
use audiosync_airplay::rtsp::{Method, Request};
use audiosync_airplay::secret::Secret;
use audiosync_airplay::volume::Volume;
use plist::Value;

use common::ap2::{
    Ap2Rig, DATA_PORT, EVENT_PORT, FakeAp2, FakeFraming, RECEIVER_CONTROL_PORT, STREAM_ID,
    TestEntropy, ap2_config, dict, int, plist, read_dict, read_value, request_key,
};
use common::hap::Answer;
use common::{CONTROL_PORT, RECEIVER, SENDER, TIMING_PORT, connection, ids};

const SECRET: [u8; 32] = [7; 32];

fn transient() -> Ap2Rig {
    Ap2Rig::new(ap2_config(), FakeAp2::new())
}

fn kind(rig: &Ap2Rig) -> Option<ErrorKind> {
    rig.failure().map(|error| error.kind)
}

fn text(value: &str) -> Value {
    Value::String(value.to_owned())
}

// covers: P67, P68, P69
#[test]
fn framing_matches_the_spec() {
    assert_eq!(frame_nonce(0x0102), [0, 0, 0, 0, 2, 1, 0, 0, 0, 0, 0, 0]);

    // The sender seals with the Write key and writes at most 1024
    // plaintext bytes a frame, the length little-endian in front.
    let mut sender = Framing::control(&SECRET);
    let mut receiver = FakeFraming::control(&SECRET);
    let plain: Vec<u8> = (0..3000u32).map(|i| i as u8).collect();
    let sealed = sender.seal(&plain);
    assert_eq!(&sealed[..2], &1024u16.to_le_bytes());
    assert_eq!(sealed.len(), plain.len() + 3 * (2 + 16));
    let (opened, sizes) = receiver.open(&sealed);
    assert_eq!(opened, plain);
    assert_eq!(sizes, vec![MAX_FRAME_PLAINTEXT, MAX_FRAME_PLAINTEXT, 952]);
    assert_eq!(sender.counters(), (3, 0));
    assert_eq!(receiver.received, 3);

    // It opens frames up to 65 535 bytes, however the reads split them.
    receiver.frame_size = 65_535;
    let big = vec![0x5A; 70_000];
    let sealed = receiver.seal(&big);
    assert_eq!(&sealed[..2], &65_535u16.to_le_bytes());
    let mut got = Vec::new();
    for chunk in sealed.chunks(7) {
        got.extend(sender.open(chunk).expect("frames open"));
    }
    assert_eq!(got, big);
    assert_eq!(sender.counters(), (3, 2));

    // A tampered frame fails, and so does everything after it.
    let mut bad = receiver.seal(b"x");
    bad[2] ^= 1;
    assert!(sender.open(&bad).is_err());
    assert!(sender.open(&receiver.seal(b"y")).is_err());

    // A frame sealed under the other direction's key, or with another
    // counter, doesn't open.
    let mut own = Framing::control(&SECRET);
    let mut mirror = Framing::control(&SECRET);
    assert!(own.open(&mirror.seal(b"same key both ways")).is_err());
    let mut skipped = FakeFraming::control(&SECRET);
    let _ = skipped.seal(b"frame 0");
    assert!(
        Framing::control(&SECRET)
            .open(&skipped.seal(b"frame 1"))
            .is_err()
    );
}

// covers: P70
#[test]
fn event_framing_swaps_the_roles() {
    let mut sender = Framing::events(&SECRET);
    let mut receiver = FakeFraming::events(&SECRET);
    let (opened, _) = receiver.open(&sender.seal(b"answer"));
    assert_eq!(opened, b"answer");
    assert_eq!(
        sender.open(&receiver.seal(b"request")).expect("opens"),
        b"request"
    );
    // The control keys don't open event frames.
    assert!(
        Framing::control(&SECRET)
            .open(&FakeFraming::events(&SECRET).seal(b"x"))
            .is_err()
    );
}

// covers: P41, P42, P43, P44, P104, P168
// Part of P153: the order and framing after a transient pair-setup.
#[test]
fn ap2_session_runs_in_order() {
    let mut rig = transient();
    assert!(rig.ready(), "{:?} {:?}", rig.keys(), rig.events);
    assert_eq!(
        rig.keys(),
        [
            "GET /info",
            "POST /pair-setup",
            "POST /pair-setup",
            "SETUP session",
            "EVENTS",
            "GET /info",
            "RECORD",
            "SETUP stream",
            "FLUSH",
            "SET_PARAMETER",
        ]
    );
    // Plain text up to the M4 answer, framed from the session SETUP on.
    let framed: Vec<bool> = rig.fake.requests.iter().map(|(_, f)| *f).collect();
    assert_eq!(
        framed,
        [false, false, false, true, true, true, true, true, true]
    );
    assert!(rig.session.framed());
    assert!(
        rig.fake
            .frame_sizes
            .iter()
            .all(|&n| n <= MAX_FRAME_PLAINTEXT)
    );
    for (request, _) in &rig.fake.requests {
        assert!(!request.headers.contains("Session"), "{}", request.uri);
        assert!(request.headers.get("CSeq").is_some());
    }

    // The first `/info` asks for the TXT values in a binary plist, the
    // second has no body.
    let first = &rig.fake.requests[0].0;
    assert_eq!(first.method, Method::Get);
    assert_eq!(first.uri, "/info");
    assert_eq!(first.headers.get("X-Apple-ProtocolVersion"), Some("1"));
    assert_eq!(
        first.headers.get("Content-Type"),
        Some("application/x-apple-binary-plist")
    );
    assert!(first.body.starts_with(b"bplist00"));
    assert_eq!(
        read_value(&first.body),
        Some(dict(&[(
            "qualifier",
            Value::Array(vec![text("txtAirPlay")])
        )]))
    );
    let second = &rig.fake.requests[4].0;
    assert_eq!(second.uri, "/info");
    assert!(second.body.is_empty());
    assert_eq!(second.headers.get("X-Apple-ProtocolVersion"), Some("1"));

    // The session SETUP: AudioSync, the device ID twice, et 0 without ekey
    // or eiv, NTP with the sender's timing port.
    let setup = rig.fake.last("SETUP session").expect("session SETUP");
    let body = read_dict(&setup.body).expect("a binary plist");
    assert_eq!(body.get("name"), Some(&text("AudioSync")));
    assert_eq!(body.get("model"), Some(&text("AudioSync")));
    assert_eq!(body.get("deviceID"), Some(&text("12:20:30:40:50:60")));
    assert_eq!(body.get("macAddress"), body.get("deviceID"));
    assert_eq!(body.get("et"), Some(&int(0)));
    assert!(!body.contains_key("ekey") && !body.contains_key("eiv"));
    assert_eq!(body.get("timingProtocol"), Some(&text("NTP")));
    assert_eq!(body.get("timingPort"), Some(&int(i64::from(TIMING_PORT))));
    assert!(!body.contains_key("timingPeerInfo"));
    assert_eq!(
        body.get("groupContainsGroupLeader"),
        Some(&Value::Boolean(false))
    );
    assert_eq!(
        body.get("isMultiSelectAirPlay"),
        Some(&Value::Boolean(false))
    );
    for key in ["sessionUUID", "groupUUID"] {
        let uuid = body.get(key).and_then(Value::as_string).expect(key);
        assert_eq!(uuid.len(), 36, "{key}");
        assert_eq!(&uuid[14..15], "4", "{key}");
    }
    assert_ne!(body.get("sessionUUID"), body.get("groupUUID"));
    assert!(
        body.get("sourceVersion")
            .and_then(Value::as_string)
            .is_some()
    );

    // The event connection goes to the answer's eventPort at the address
    // the session connected to.
    assert_eq!(rig.event_connects.len(), 1);
    assert_eq!(
        rig.event_connects[0].1,
        std::net::SocketAddr::new(RECEIVER, EVENT_PORT)
    );

    // The stream SETUP: one realtime ALAC stream, never buffered audio.
    let setup = rig.fake.last("SETUP stream").expect("stream SETUP");
    let body = read_dict(&setup.body).expect("a binary plist");
    let streams = body
        .get("streams")
        .and_then(Value::as_array)
        .expect("streams");
    assert_eq!(streams.len(), 1);
    let stream = streams[0].as_dictionary().expect("a dictionary");
    assert_eq!(stream.get("type"), Some(&int(96)));
    assert_eq!(stream.get("ct"), Some(&int(2)));
    assert_eq!(stream.get("audioFormat"), Some(&int(0x40000)));
    assert_eq!(stream.get("spf"), Some(&int(352)));
    assert_eq!(stream.get("sr"), Some(&int(44_100)));
    assert_eq!(stream.get("latencyMin"), Some(&int(11_025)));
    assert_eq!(stream.get("latencyMax"), Some(&int(88_200)));
    assert_eq!(
        stream.get("controlPort"),
        Some(&int(i64::from(CONTROL_PORT)))
    );
    assert_eq!(stream.get("isMedia"), Some(&Value::Boolean(true)));
    assert_eq!(stream.get("audioMode"), Some(&text("default")));
    assert_eq!(
        stream.get("supportsDynamicStreamID"),
        Some(&Value::Boolean(false))
    );
    assert_eq!(
        stream.get("streamConnectionID"),
        Some(&int(i64::from(ids().session_id)))
    );
    let shk = stream
        .get("shk")
        .and_then(Value::as_data)
        .expect("shk data");
    assert_eq!(shk.len(), 32);
    assert!(shk.iter().any(|&byte| byte != 0));
    for (request, _) in &rig.fake.requests {
        assert!(!request.uri.contains("SETRATEANCHORTIME"));
        assert_ne!(request.method.as_str(), "FLUSHBUFFERED");
    }

    let volume = rig.fake.last("SET_PARAMETER").expect("volume");
    assert_eq!(volume.body, b"volume: -15.000000\r\n");
    let flush = rig.fake.last("FLUSH").expect("FLUSH");
    assert_eq!(
        flush.headers.get("RTP-Info"),
        Some("seq=17000;rtptime=2000000000")
    );
    assert_eq!(
        rig.session.stream(),
        Some(StreamPorts {
            data_port: DATA_PORT,
            control_port: RECEIVER_CONTROL_PORT,
            stream_id: STREAM_ID,
        })
    );
    assert!(rig.events.contains(&Ap2Event::TimingChosen(Timing::Ntp)));
    assert!(
        rig.events
            .contains(&Ap2Event::Server("AirTunes/fake".to_owned()))
    );

    // TEARDOWN of the stream, then of the session.
    let now = rig.now;
    rig.session.teardown(now);
    rig.pump();
    let keys = rig.keys();
    assert_eq!(keys[keys.len() - 2..], ["TEARDOWN", "TEARDOWN"]);
    let n = rig.fake.requests.len();
    assert_eq!(
        read_value(&rig.fake.requests[n - 2].0.body),
        Some(dict(&[(
            "streams",
            Value::Array(vec![dict(&[
                ("type", int(96)),
                ("streamID", int(STREAM_ID))
            ])])
        )]))
    );
    assert_eq!(
        read_value(&rig.fake.requests[n - 1].0.body),
        Some(dict(&[]))
    );
    assert_eq!(rig.events.last(), Some(&Ap2Event::Closed));
}

// covers: P41
#[test]
fn info_answer_may_be_only_the_txt_record() {
    // DNS wire form: each string a length byte, then `key=value`.
    let mut wire = Vec::new();
    for string in ["features=0x0,0x10040", "flags=0x80"] {
        wire.push(string.len() as u8);
        wire.extend_from_slice(string.as_bytes());
    }
    let mut fake = FakeAp2::new();
    fake.info_body = plist(dict(&[
        ("statusFlags", text("not a number")),
        ("txtAirPlay", Value::Data(wire)),
    ]));
    let rig = Ap2Rig::new(ap2_config(), fake);
    let info = rig
        .events
        .iter()
        .find_map(|event| match event {
            Ap2Event::Info(info) => Some(info.clone()),
            _ => None,
        })
        .expect("an Info event");
    assert_eq!(info.status_flags, Some(0x80));
    assert!(info.features.is_some_and(|features| features.has(38)));
    // Flags bit 7 asks for a password, which this config lacks.
    assert_eq!(kind(&rig), Some(ErrorKind::PasswordRequired));
    assert_eq!(rig.keys(), ["GET /info"]);
}

// Part of P149 and P153: transient and password pairing on the session
// connection; tests/ap2_pin.rs covers the PIN pairing.
#[test]
fn pairing_goes_on_the_session_connection() {
    let rig = transient();
    let setups: Vec<&Request> = rig
        .fake
        .requests
        .iter()
        .map(|(request, _)| request)
        .filter(|request| request.uri == "/pair-setup")
        .collect();
    assert_eq!(setups.len(), 2);
    for request in setups {
        assert_eq!(request.method, Method::Post);
        assert_eq!(
            request.headers.get("Content-Type"),
            Some("application/octet-stream")
        );
        assert_eq!(request.headers.get("X-Apple-HKP"), Some("4"));
        assert!(
            request
                .headers
                .get("User-Agent")
                .is_some_and(|ua| ua.starts_with("AudioSync/"))
        );
        for name in ["CSeq", "Client-Instance", "DACP-ID", "Active-Remote"] {
            assert!(request.headers.contains(name), "{name}");
        }
    }
    assert_eq!(rig.connects.len(), 1);

    // A password pairing: M1 to M6, then pair-verify on the same
    // connection, framed from the request after the verify M4.
    let mut fake = FakeAp2::new();
    fake.info_body = plist(dict(&[("statusFlags", int(0x80))]));
    let mut config = ap2_config();
    config.password = Some(Secret::new("3939".to_owned()));
    let rig = Ap2Rig::new(config, fake);
    assert!(rig.ready(), "{:?} {:?}", rig.keys(), rig.events);
    assert_eq!(
        rig.keys()[..7],
        [
            "GET /info",
            "POST /pair-setup",
            "POST /pair-setup",
            "POST /pair-setup",
            "POST /pair-verify",
            "POST /pair-verify",
            "SETUP session",
        ]
    );
    let framed: Vec<bool> = rig.fake.requests.iter().map(|(_, f)| *f).collect();
    assert_eq!(
        framed[..7],
        [false, false, false, false, false, false, true]
    );
    for (request, _) in &rig.fake.requests[1..6] {
        assert_eq!(
            request.headers.get("X-Apple-HKP"),
            Some("3"),
            "{}",
            request.uri
        );
    }
    assert!(
        rig.events
            .iter()
            .any(|event| matches!(event, Ap2Event::Credentials(_)))
    );

    // A digest challenge on the pairing: the same request again with
    // Authorization, then every later one carries it.
    let mut fake = FakeAp2::new();
    fake.challenge = Some((
        "POST /pair-setup".to_owned(),
        "Digest realm=\"raop\", nonce=\"abc\"".to_owned(),
    ));
    let mut config = ap2_config();
    config.password = Some(Secret::new("hunter2".to_owned()));
    let rig = Ap2Rig::new(config, fake);
    assert!(rig.ready(), "{:?} {:?}", rig.keys(), rig.events);
    let keys = rig.keys();
    assert_eq!(
        keys[..3],
        ["GET /info", "POST /pair-setup", "POST /pair-setup"]
    );
    assert!(!rig.fake.requests[1].0.headers.contains("Authorization"));
    for (request, _) in &rig.fake.requests[2..] {
        let value = request.headers.get("Authorization").expect("Authorization");
        assert!(value.starts_with("Digest "), "{value}");
    }

    // Without a password the challenge ends the session.
    let mut fake = FakeAp2::new();
    fake.challenge = Some((
        "POST /pair-setup".to_owned(),
        "Digest realm=\"raop\", nonce=\"abc\"".to_owned(),
    ));
    let rig = Ap2Rig::new(ap2_config(), fake);
    assert_eq!(kind(&rig), Some(ErrorKind::PasswordRequired));
}

// Outside a PIN pairing a back-off ends the pairing; its wait still
// reaches the host's event and debug log, as a PIN pairing's does.
// covers: P150
#[test]
fn a_back_off_outside_a_pin_pairing_tells_the_host_its_wait() {
    let mut fake = FakeAp2::new();
    fake.hap
        .answers
        .push((("/pair-setup", 1), Answer::Error(3)));
    fake.hap.back_off = Some(30);
    let rig = Ap2Rig::new(ap2_config(), fake);
    let error = rig.failure().expect("the pairing fails");
    assert_eq!(error.kind, ErrorKind::PairingFailed);
    assert!(error.to_string().contains("30 s"), "{error}");
}

// covers: P70
#[test]
fn event_connection_answers_every_request() {
    let mut rig = transient();
    assert!(rig.ready());
    let mut request = Request::new(Method::Post, "/command");
    request.headers.push("CSeq", "5");
    request.headers.push("Server", "AirTunes/fake");
    request
        .headers
        .push("Content-Type", "application/x-apple-binary-plist");
    request.body = plist(dict(&[("type", text("updateInfo"))]));
    rig.send_event(&request);
    let mut plain = Request::new(Method::Post, "/command");
    plain.headers.push("CSeq", "6");
    rig.send_event(&plain);

    assert_eq!(rig.event_answers.len(), 2);
    let first = &rig.event_answers[0];
    assert_eq!(first.code, 200);
    assert_eq!(first.headers.get("CSeq"), Some("5"));
    assert_eq!(first.headers.get("Content-Length"), Some("0"));
    assert_eq!(first.headers.get("Audio-Latency"), Some("0"));
    assert_eq!(first.headers.get("Server"), Some("AirTunes/fake"));
    let second = &rig.event_answers[1];
    assert_eq!(second.headers.get("CSeq"), Some("6"));
    assert!(!second.headers.contains("Server"));
    // Each direction counts its own frames from 0.
    let events = rig.fake.events.as_ref().expect("event framing");
    assert_eq!((events.sent, events.received), (2, 2));
    assert!(rig.failure().is_none());

    // A refused event connect is retried 1 s apart, and the stream SETUP
    // waits for the connection.
    let mut rig = Ap2Rig::new_with(ap2_config(), FakeAp2::new(), 3);
    assert!(!rig.ready());
    rig.advance(Duration::from_secs(5));
    assert!(rig.ready(), "{:?}", rig.events);
    let times: Vec<_> = rig.event_connects.iter().map(|(at, _)| *at).collect();
    assert_eq!(times.len(), 4);
    for pair in times.windows(2) {
        assert_eq!(pair[1] - pair[0], Duration::from_secs(1));
    }
    assert_eq!(rig.times_of("SETUP stream"), vec![times[3]]);

    // After the first connect and five retries, all refused, the speaker
    // is unreachable.
    let mut rig = Ap2Rig::new_with(ap2_config(), FakeAp2::new(), 6);
    rig.advance(Duration::from_secs(8));
    assert_eq!(rig.event_connects.len(), 6);
    assert_eq!(kind(&rig), Some(ErrorKind::Unreachable));
    assert!(rig.times_of("SETUP stream").is_empty());
}

// covers: P132
#[test]
fn feedback_every_two_seconds() {
    let mut rig = transient();
    let start = rig.now;
    rig.advance(Duration::from_millis(9_000));
    let times = rig.times_of("POST /feedback");
    assert_eq!(times.len(), 4, "{:?}", rig.keys());
    for (n, at) in times.iter().enumerate() {
        assert_eq!(*at - start, Duration::from_secs(2 * (n as u64 + 1)));
    }
    let feedback = rig.fake.last("POST /feedback").expect("feedback");
    assert!(feedback.body.is_empty());
    assert!(rig.failure().is_none());

    // An answer with a plist body is fine too.
    let mut fake = FakeAp2::new();
    fake.bodies.insert(
        "POST /feedback".to_owned(),
        plist(dict(&[("streams", Value::Array(Vec::new()))])),
    );
    let mut rig = Ap2Rig::new(ap2_config(), fake);
    rig.advance(Duration::from_secs(9));
    assert!(rig.failure().is_none());

    // Unanswered: the one in flight counts once per 2 s, and the third miss
    // means lost.
    let mut fake = FakeAp2::new();
    fake.silent.push("POST /feedback".to_owned());
    let mut rig = Ap2Rig::new(ap2_config(), fake);
    rig.advance(Duration::from_millis(7_900));
    assert!(rig.failure().is_none());
    rig.advance(Duration::from_millis(200));
    let error = rig.failure().expect("lost");
    assert_eq!(error.kind, ErrorKind::Lost);
    assert_eq!(error.method, Some(Method::Post));
    assert_eq!(rig.times_of("POST /feedback").len(), 1);

    // Failed answers count the same way.
    let mut fake = FakeAp2::new();
    fake.status.insert("POST /feedback".to_owned(), 500);
    let mut rig = Ap2Rig::new(ap2_config(), fake);
    rig.advance(Duration::from_millis(5_900));
    assert!(rig.failure().is_none());
    rig.advance(Duration::from_millis(200));
    assert_eq!(kind(&rig), Some(ErrorKind::Lost));
    assert_eq!(rig.times_of("POST /feedback").len(), 3);
}

fn ptp_peer() -> PtpPeer {
    PtpPeer {
        clock_id: 0xFFFF_0000_0000_0001,
        id: "6f1e7c9a-3b2d-4c5e-8f90-a1b2c3d4e5f6".to_owned(),
    }
}

/// `/info` features with bit 41 (PTP) as well as 38 and 48.
fn ptp_fake() -> FakeAp2 {
    let mut fake = FakeAp2::new();
    fake.info_body = plist(dict(&[
        ("features", int((1 << 38) | (1 << 41) | (1 << 48))),
        ("statusFlags", int(0x4)),
    ]));
    fake
}

// covers: P157, P164, P165, P106
#[test]
fn ptp_only_after_ntp_is_refused() {
    let mut fake = ptp_fake();
    fake.status_once.insert("SETUP session".to_owned(), 501);
    let mut config = ap2_config();
    config.ptp = Some(ptp_peer());
    let rig = Ap2Rig::new(config, fake);
    assert!(rig.ready(), "{:?} {:?}", rig.keys(), rig.events);
    assert_eq!(rig.connects.len(), 2);
    assert_eq!(rig.fake.connections, 2);
    assert_eq!(
        rig.keys(),
        [
            "GET /info",
            "POST /pair-setup",
            "POST /pair-setup",
            "SETUP session",
            "GET /info",
            "POST /pair-setup",
            "POST /pair-setup",
            "SETUP session",
            "EVENTS",
            "GET /info",
            "RECORD",
            "SETPEERS",
            "SETUP stream",
            "FLUSH",
            "SET_PARAMETER",
        ]
    );
    let first = read_dict(&rig.fake.requests[3].0.body).expect("plist");
    assert_eq!(first.get("timingProtocol"), Some(&text("NTP")));

    let second = read_dict(&rig.fake.requests[7].0.body).expect("plist");
    assert_eq!(second.get("timingProtocol"), Some(&text("PTP")));
    assert!(!second.contains_key("timingPort"));
    assert_eq!(
        second.get("groupContainsGroupLeader"),
        Some(&Value::Boolean(false))
    );
    let info = second
        .get("timingPeerInfo")
        .and_then(Value::as_dictionary)
        .expect("timingPeerInfo");
    assert_eq!(
        info.get("Addresses"),
        Some(&Value::Array(vec![text(&SENDER.to_string())]))
    );
    assert_eq!(info.get("ID"), Some(&text(&ptp_peer().id)));
    assert_eq!(info.get("DeviceType"), Some(&int(0)));
    // The clock identity's 64 bits, read as signed.
    assert_eq!(
        info.get("ClockID").and_then(Value::as_signed_integer),
        Some(0xFFFF_0000_0000_0001_u64 as i64)
    );
    assert_eq!(
        info.get("SupportsClockPortMatchingOverride"),
        Some(&Value::Boolean(false))
    );
    assert_eq!(
        second.get("timingPeerList"),
        Some(&Value::Array(vec![Value::Dictionary(info.clone())]))
    );
    assert!(
        rig.session
            .receiver_timing_peer()
            .is_some_and(|peer| peer.contains_key("ID"))
    );
    assert!(rig.events.contains(&Ap2Event::TimingChosen(Timing::Ptp)));
    assert_eq!(rig.session.timing(), Timing::Ptp);

    // SETPEERS: the receiver's address, then the sender's.
    let peers = rig.fake.last("SETPEERS").expect("SETPEERS");
    assert_eq!(
        peers.headers.get("Content-Type"),
        Some("/peer-list-changed")
    );
    assert_eq!(
        read_value(&peers.body),
        Some(Value::Array(vec![
            text(&RECEIVER.to_string()),
            text(&SENDER.to_string())
        ]))
    );

    // Refused under NTP without a PTP clock, or by a receiver without bit
    // 41: Unsupported (timing) on the first connection.
    let mut fake = ptp_fake();
    fake.status_once.insert("SETUP session".to_owned(), 501);
    let rig = Ap2Rig::new(ap2_config(), fake);
    assert_eq!(
        kind(&rig),
        Some(ErrorKind::Unsupported(UnsupportedReason::Timing))
    );
    assert_eq!(rig.connects.len(), 1);

    let mut fake = FakeAp2::new();
    fake.status_once.insert("SETUP session".to_owned(), 501);
    let mut config = ap2_config();
    config.ptp = Some(ptp_peer());
    let rig = Ap2Rig::new(config, fake);
    assert_eq!(
        kind(&rig),
        Some(ErrorKind::Unsupported(UnsupportedReason::Timing))
    );
    assert_eq!(rig.connects.len(), 1);

    // One PTP retry only.
    let mut fake = ptp_fake();
    fake.status.insert("SETUP session".to_owned(), 501);
    let mut config = ap2_config();
    config.ptp = Some(ptp_peer());
    let rig = Ap2Rig::new(config, fake);
    assert_eq!(
        kind(&rig),
        Some(ErrorKind::Unsupported(UnsupportedReason::Timing))
    );
    assert_eq!(rig.connects.len(), 2);
    assert!(!rig.keys().contains(&"SETPEERS"));

    // A remembered PTP choice starts with PTP.
    let mut config = ap2_config();
    config.ptp = Some(ptp_peer());
    config.timing = Timing::Ptp;
    let rig = Ap2Rig::new(config, ptp_fake());
    assert!(rig.ready());
    assert_eq!(rig.connects.len(), 1);
    let body = read_dict(&rig.fake.last("SETUP session").expect("SETUP").body).expect("plist");
    assert_eq!(body.get("timingProtocol"), Some(&text("PTP")));
}

// covers: P165
#[test]
fn no_setpeers_under_ntp() {
    let rig = transient();
    assert!(rig.ready());
    assert!(!rig.keys().contains(&"SETPEERS"));
}

// covers: P43
#[test]
fn stream_setup_refused_without_a_key_is_unsupported() {
    let mut fake = FakeAp2::new();
    fake.no_data_port = true;
    let rig = Ap2Rig::new(ap2_config(), fake);
    assert_eq!(
        kind(&rig),
        Some(ErrorKind::Unsupported(UnsupportedReason::Encryption))
    );

    let mut fake = FakeAp2::new();
    fake.silent.push("SETUP stream".to_owned());
    let mut rig = Ap2Rig::new(ap2_config(), fake);
    assert!(rig.failure().is_none());
    let now = rig.now;
    rig.session.handle_connection_closed(now);
    rig.pump();
    assert_eq!(
        kind(&rig),
        Some(ErrorKind::Unsupported(UnsupportedReason::Encryption))
    );

    // A close anywhere else is a lost connection.
    let mut rig = transient();
    let now = rig.now;
    rig.session.handle_connection_closed(now);
    rig.pump();
    assert_eq!(kind(&rig), Some(ErrorKind::Lost));
}

// covers: P42
#[test]
fn volume_set_while_the_initial_volume_is_out_goes_after_it() {
    let mut fake = FakeAp2::new();
    fake.hold = Some("SET_PARAMETER".to_owned());
    let mut rig = Ap2Rig::new(ap2_config(), fake);
    assert_eq!(rig.keys().last(), Some(&"SET_PARAMETER"));
    let now = rig.now;
    rig.session.set_volume(Volume::Db(-30.0), now);
    rig.pump();
    rig.release();
    assert!(rig.ready(), "{:?} {:?}", rig.keys(), rig.events);
    assert_eq!(
        rig.keys()[rig.keys().len() - 3..],
        ["FLUSH", "SET_PARAMETER", "SET_PARAMETER"]
    );
    let volume = rig.fake.last("SET_PARAMETER").expect("volume");
    assert_eq!(volume.body, b"volume: -30.000000\r\n");
}

// covers: P157, P153
#[test]
fn ptp_retry_after_a_password_pairing_verifies_instead_of_pairing_again() {
    let mut fake = ptp_fake();
    fake.info_body = plist(dict(&[
        ("features", int((1 << 38) | (1 << 41) | (1 << 48))),
        ("statusFlags", int(0x80)),
    ]));
    fake.status_once.insert("SETUP session".to_owned(), 501);
    let mut config = ap2_config();
    config.ptp = Some(ptp_peer());
    config.password = Some(Secret::new("3939".to_owned()));
    let rig = Ap2Rig::new(config, fake);
    assert!(rig.ready(), "{:?} {:?}", rig.keys(), rig.events);
    assert_eq!(rig.connects.len(), 2);
    let credentials = rig
        .events
        .iter()
        .filter(|event| matches!(event, Ap2Event::Credentials(_)))
        .count();
    assert_eq!(credentials, 1);
    let keys = rig.keys();
    let second = keys
        .iter()
        .rposition(|key| *key == "SETUP session")
        .expect("second session SETUP");
    assert_eq!(
        keys[second - 3..=second],
        [
            "GET /info",
            "POST /pair-verify",
            "POST /pair-verify",
            "SETUP session"
        ]
    );
}

// covers: P42
#[test]
fn teardown_during_the_stream_setup_still_tears_the_stream_down() {
    let mut fake = FakeAp2::new();
    fake.hold = Some("SETUP stream".to_owned());
    let mut rig = Ap2Rig::new(ap2_config(), fake);
    assert_eq!(rig.keys().last(), Some(&"SETUP stream"));
    let now = rig.now;
    rig.session.teardown(now);
    rig.pump();
    rig.release();
    let keys = rig.keys();
    assert_eq!(
        keys[keys.len() - 3..],
        ["SETUP stream", "TEARDOWN", "TEARDOWN"]
    );
    let n = rig.fake.requests.len();
    assert_eq!(
        read_value(&rig.fake.requests[n - 2].0.body),
        Some(dict(&[(
            "streams",
            Value::Array(vec![dict(&[
                ("type", int(96)),
                ("streamID", int(STREAM_ID))
            ])])
        )]))
    );
    assert_eq!(rig.events.last(), Some(&Ap2Event::Closed));
}

// covers: P129
#[test]
fn teardown_during_the_session_setup_tears_down_the_session_it_made() {
    let mut fake = FakeAp2::new();
    fake.hold = Some("SETUP session".to_owned());
    let mut rig = Ap2Rig::new(ap2_config(), fake);
    assert_eq!(rig.keys().last(), Some(&"SETUP session"));
    let now = rig.now;
    rig.session.teardown(now);
    rig.pump();
    assert_eq!(rig.closes, 0, "closed with the session SETUP unanswered");
    rig.release();
    let keys = rig.keys();
    assert_eq!(keys[keys.len() - 2..], ["SETUP session", "TEARDOWN"]);
    let n = rig.fake.requests.len();
    assert_eq!(
        read_value(&rig.fake.requests[n - 1].0.body),
        Some(dict(&[]))
    );
    assert_eq!(rig.events.last(), Some(&Ap2Event::Closed));
    assert!(rig.failure().is_none());
    assert!(rig.event_connects.is_empty());
}

// covers: P25, P129
#[test]
fn teardown_during_the_session_setup_carries_the_answers_session() {
    let mut fake = FakeAp2::new();
    fake.session = Some("5A1B2C3D".to_owned());
    fake.hold = Some("SETUP session".to_owned());
    let mut rig = Ap2Rig::new(ap2_config(), fake);
    rig.advance(Duration::from_millis(100));
    let now = rig.now;
    rig.session.teardown(now);
    rig.advance(Duration::from_millis(200));
    rig.release();
    let teardown = rig.fake.last("TEARDOWN").expect("TEARDOWN");
    assert_eq!(teardown.headers.get("Session"), Some("5A1B2C3D"));
}

// covers: P129
#[test]
fn teardown_during_an_unanswered_session_setup_closes_after_two_seconds() {
    let mut fake = FakeAp2::new();
    fake.hold = Some("SETUP session".to_owned());
    let mut rig = Ap2Rig::new(ap2_config(), fake);
    let now = rig.now;
    rig.session.teardown(now);
    rig.advance(Duration::from_millis(1900));
    assert_eq!(rig.closes, 0);
    rig.advance(Duration::from_millis(200));
    assert_eq!(rig.closes, 1);
    assert_eq!(rig.keys().last(), Some(&"SETUP session"));
    assert_eq!(rig.events.last(), Some(&Ap2Event::Closed));
}

// covers: P129
#[test]
fn teardown_during_a_refused_session_setup_just_closes() {
    let mut fake = FakeAp2::new();
    fake.hold = Some("SETUP session".to_owned());
    fake.status.insert("SETUP session".to_owned(), 500);
    let mut rig = Ap2Rig::new(ap2_config(), fake);
    let now = rig.now;
    rig.session.teardown(now);
    rig.pump();
    rig.release();
    assert_eq!(rig.keys().last(), Some(&"SETUP session"));
    assert_eq!(rig.closes, 1);
    assert_eq!(rig.events.last(), Some(&Ap2Event::Closed));
    assert!(rig.failure().is_none());
}

// covers: P42, P106
#[test]
fn volume_set_before_the_startup_volume_goes_out_once() {
    let mut fake = ptp_fake();
    fake.status_once.insert("SETUP session".to_owned(), 501);
    fake.hold = Some("SETPEERS".to_owned());
    let mut config = ap2_config();
    config.ptp = Some(ptp_peer());
    let mut rig = Ap2Rig::new(config, fake);
    assert_eq!(rig.keys().last(), Some(&"SETPEERS"));
    let now = rig.now;
    rig.session.set_volume(Volume::Db(-30.0), now);
    rig.pump();
    rig.release();
    assert!(rig.ready(), "{:?} {:?}", rig.keys(), rig.events);
    let volumes: Vec<&[u8]> = rig
        .fake
        .requests
        .iter()
        .filter(|(request, _)| request.method.as_str() == "SET_PARAMETER")
        .map(|(request, _)| request.body.as_slice())
        .collect();
    assert_eq!(volumes, [b"volume: -30.000000\r\n".as_slice()]);
}

fn traces(rig: &Ap2Rig) -> Vec<&str> {
    rig.events
        .iter()
        .filter_map(|event| match event {
            Ap2Event::Trace(line) => Some(line.as_str()),
            _ => None,
        })
        .collect()
}

fn traced_config() -> audiosync_airplay::ap2::Ap2Config {
    let mut config = ap2_config();
    config.trace = true;
    config
}

#[test]
fn traced_session_logs_a_steady_read_answer_once() {
    let mut fake = FakeAp2::new();
    fake.status.insert("GET_PARAMETER".to_owned(), 404);
    let mut config = traced_config();
    config.read_volume = true;
    let mut rig = Ap2Rig::new(config, fake);
    assert!(rig.ready(), "{:?} {:?}", rig.keys(), rig.events);
    for _ in 0..8 {
        let now = rig.now;
        rig.session.read_volume(now);
        rig.advance(Duration::from_secs(2));
    }
    assert!(rig.failure().is_none(), "{:?}", rig.events);
    let reads: Vec<&Request> = rig
        .fake
        .requests
        .iter()
        .map(|(request, _)| request)
        .filter(|request| request.method.as_str() == "GET_PARAMETER")
        .collect();
    assert_eq!(reads.len(), 3, "{:?}", rig.keys());
    let keys = rig.keys();
    let read_at = keys.iter().position(|key| *key == "GET_PARAMETER");
    let volume_at = keys.iter().position(|key| *key == "SET_PARAMETER");
    assert!(volume_at < read_at, "{keys:?}");
    let lines = traces(&rig);
    assert!(
        lines
            .iter()
            .any(|line| line.starts_with("/info initialVolume")),
        "{lines:?}"
    );
    let refusals = lines
        .iter()
        .filter(|line| line.starts_with("GET_PARAMETER volume: status 404"))
        .count();
    assert_eq!(refusals, 1, "{lines:?}");
}

#[test]
fn traced_session_shows_a_refused_set_parameter_with_its_body() {
    let mut fake = FakeAp2::new();
    fake.status.insert("SET_PARAMETER".to_owned(), 500);
    let rig = Ap2Rig::new(traced_config(), fake);
    assert!(rig.ready(), "{:?} {:?}", rig.keys(), rig.events);
    let lines = traces(&rig);
    assert!(
        lines.iter().any(|line| line
            .starts_with("SET_PARAMETER text/parameters: status 500, body text \"volume: -")),
        "{lines:?}"
    );
}

#[test]
fn traced_session_shows_the_volume_request_as_sent_and_the_refusal_as_answered() {
    let mut fake = FakeAp2::new();
    fake.status.insert("SET_PARAMETER".to_owned(), 500);
    fake.session = Some("5A1B2C3D".to_owned());
    let rig = Ap2Rig::new(traced_config(), fake);
    assert!(rig.ready(), "{:?} {:?}", rig.keys(), rig.events);
    let lines = traces(&rig);
    assert!(
        lines
            .iter()
            .any(|line| line.starts_with("SETUP session: status 200 OK [")
                && line.contains("CSeq: ")
                && line.contains("Session: 5A1B2C3D;timeout=60")),
        "{lines:?}"
    );
    let sent = lines
        .iter()
        .find(|line| line.starts_with("SET_PARAMETER rtsp://"))
        .unwrap_or_else(|| panic!("{lines:?}"));
    assert!(sent.contains("CSeq: "), "{sent}");
    assert!(sent.contains("Content-Type: text/parameters"), "{sent}");
    assert!(sent.contains("body text \"volume: -"), "{sent}");
    assert!(!sent.contains("Authorization"), "{sent}");
    assert!(
        lines.iter().any(
            |line| line.starts_with("SET_PARAMETER text/parameters: status 500")
                && line.contains(", reply ")
                && line.contains("CSeq: ")
        ),
        "{lines:?}"
    );
}

#[test]
fn an_untraced_session_logs_no_set_parameter_lines() {
    let mut fake = FakeAp2::new();
    fake.status.insert("SET_PARAMETER".to_owned(), 500);
    let rig = Ap2Rig::new(ap2_config(), fake);
    assert!(rig.ready(), "{:?} {:?}", rig.keys(), rig.events);
    assert!(traces(&rig).is_empty(), "{:?}", traces(&rig));
}

/// Every request's key and `Session` header, in order.
fn session_headers(rig: &Ap2Rig) -> Vec<(String, Option<String>)> {
    rig.fake
        .requests
        .iter()
        .map(|(request, _)| {
            let session = request.headers.get("Session").map(str::to_owned);
            (request_key(request), session)
        })
        .collect()
}

const CARRYING: [&str; 5] = [
    "RECORD",
    "SET_PARAMETER",
    "GET_PARAMETER",
    "FLUSH",
    "TEARDOWN",
];

/// Plays 4 s with reads on, sets a volume, then tears down; every one of
/// the five methods went out and nothing warned.
fn play_and_tear_down(fake: FakeAp2, ptp: bool) -> Ap2Rig {
    let mut config = ap2_config();
    config.read_volume = true;
    if ptp {
        config.ptp = Some(ptp_peer());
    }
    let mut rig = Ap2Rig::new(config, fake);
    assert!(rig.ready(), "{:?} {:?}", rig.keys(), rig.events);
    let now = rig.now;
    rig.session.set_volume(Volume::Db(-20.0), now);
    rig.pump();
    rig.advance(Duration::from_secs(4));
    let now = rig.now;
    rig.session.teardown(now);
    rig.pump();
    let headers = session_headers(&rig);
    for key in CARRYING {
        assert!(headers.iter().any(|(k, _)| k == key), "{headers:?}");
    }
    let warned = rig
        .events
        .iter()
        .any(|event| matches!(event, Ap2Event::Warning(_)));
    assert!(!warned, "{:?}", rig.events);
    rig
}

/// Nothing up to the last session SETUP carries `Session`; after it the
/// five methods carry `id` and the stream SETUP none.
fn assert_session_after_setup(rig: &Ap2Rig, id: &str) {
    let headers = session_headers(rig);
    let setup = headers
        .iter()
        .rposition(|(key, _)| key == "SETUP session")
        .expect("session SETUP");
    for (key, session) in &headers[..=setup] {
        assert_eq!(session.as_deref(), None, "{key}: {headers:?}");
    }
    for (key, session) in &headers[setup + 1..] {
        let wanted = CARRYING.contains(&key.as_str()).then_some(id);
        assert_eq!(session.as_deref(), wanted, "{key}: {headers:?}");
    }
    assert!(headers.iter().any(|(key, _)| key == "SETUP stream"));
}

// covers: P25
#[test]
fn the_session_setup_answers_session_goes_on_every_later_rtsp_request() {
    let mut fake = FakeAp2::new();
    fake.session = Some("5A1B2C3D".to_owned());
    let rig = play_and_tear_down(fake, false);
    assert_session_after_setup(&rig, "5A1B2C3D");
}

// covers: P25
#[test]
fn a_session_setup_answer_without_session_leaves_the_header_out() {
    let rig = play_and_tear_down(FakeAp2::new(), false);
    let headers = session_headers(&rig);
    assert!(headers.iter().all(|(_, s)| s.is_none()), "{headers:?}");
}

// covers: P25, P157
#[test]
fn a_session_setup_retried_on_a_new_connection_uses_only_the_new_answers_session() {
    let mut fake = ptp_fake();
    fake.session = Some("5A1B2C3D".to_owned());
    fake.status_once.insert("SETUP session".to_owned(), 501);
    let rig = play_and_tear_down(fake, true);
    assert_eq!(rig.fake.connections, 2);
    assert_session_after_setup(&rig, "5A1B2C3D-2");
}

// covers: P223
#[test]
fn a_pair_setup_step_that_never_comes_back_fails_the_session() {
    let mut config = ap2_config();
    config.offload_srp = true;
    let mut rig = Ap2Rig::new(config, FakeAp2::new());
    // The thread that took the step is gone and never answers.
    assert!(rig.session.take_work().is_some(), "{:?}", rig.keys());
    rig.advance(Duration::from_secs(9));
    assert_eq!(kind(&rig), None);
    rig.advance(Duration::from_secs(2));
    assert_eq!(kind(&rig), Some(ErrorKind::Timeout));

    // One that comes back in time goes on to the session.
    let mut config = ap2_config();
    config.offload_srp = true;
    let mut rig = Ap2Rig::new(config, FakeAp2::new());
    let work = rig.session.take_work().expect("the SRP step");
    rig.advance(Duration::from_secs(1));
    rig.session.handle_computed(work.run(), rig.now);
    rig.pump();
    rig.advance(Duration::from_secs(11));
    assert_eq!(kind(&rig), None);
    assert!(
        rig.keys().iter().any(|key| key.starts_with("SETUP")),
        "{:?}",
        rig.keys()
    );
}

// covers: P217
#[test]
fn a_refused_remembered_ptp_runs_the_ntp_first_order_once() {
    let mut fake = ptp_fake();
    fake.status.insert("SETUP session".to_owned(), 501);
    let mut config = ap2_config();
    config.ptp = Some(ptp_peer());
    config.timing = Timing::Ptp;
    let rig = Ap2Rig::new(config, fake);
    assert_eq!(
        kind(&rig),
        Some(ErrorKind::Unsupported(UnsupportedReason::Timing))
    );
    assert_eq!(rig.connects.len(), 3);
    assert!(rig.events.contains(&Ap2Event::TimingRefused));
    let timings: Vec<Option<Value>> = rig
        .fake
        .requests
        .iter()
        .filter(|(request, _)| request_key(request) == "SETUP session")
        .map(|(request, _)| {
            read_dict(&request.body).and_then(|body| body.get("timingProtocol").cloned())
        })
        .collect();
    assert_eq!(
        timings,
        [Some(text("PTP")), Some(text("NTP")), Some(text("PTP"))]
    );
}

// A pair-setup retry takes a session that connected back to Connecting: an
// address discovery adds then leaves it in place.
// covers: P228
#[test]
fn a_session_back_to_connecting_for_a_pairing_retry_doesnt_start_again() {
    let mut fake = FakeAp2::new();
    fake.hap.hash_padded = true;
    fake.hap.zero_led_b = 1;
    let config = ap2_config();
    let first = config.addresses[0];
    let added = SocketAddr::new("2001:db8::55".parse().expect("an address"), first.port());
    let now = Instant::now();
    let session = Ap2Session::new(config, ids(), Box::new(TestEntropy(1)), now);
    let mut rig = Ap2Rig::with_session(session, fake, now);
    assert!(rig.session.restarts_on(&[first, added]));
    assert!(!rig.session.restarts_on(&[first]));

    let mut connects = 0;
    while let Some(output) = rig.session.poll_output() {
        match output {
            Ap2Output::Connect(_) if connects == 1 => break,
            Ap2Output::Connect(_) => {
                connects += 1;
                rig.fake.connect();
                rig.session.handle_connected(connection(SENDER), rig.now);
            }
            Ap2Output::Send(bytes) => {
                let answer = rig.fake.feed(&bytes);
                if !answer.is_empty() {
                    rig.session.handle_input(&answer, rig.now);
                }
            }
            _ => {}
        }
    }
    assert_eq!(rig.session.state(), Ap2State::Connecting);
    assert!(!rig.session.restarts_on(&[first, added]));
}
