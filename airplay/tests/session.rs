//! The AirPlay 1 session against the fake receiver: request order, headers,
//! digest authentication, volume, metadata, teardown, keep-alive and the
//! error classification (sections 3, 8, 9 and 10 of docs/AIRPLAY_SPEC.md).

mod common;

use std::time::Duration;

use audiosync_airplay::error::{ErrorKind, UnsupportedReason, Warning};
use audiosync_airplay::ids::{Entropy, EntropyUnavailable, OsEntropy, SessionIds};
use audiosync_airplay::metadata::{Metadata, MetadataError, MetadataSupport, TrackInfo};
use audiosync_airplay::rtsp::{JackKind, JackStatus};
use audiosync_airplay::sdp::{self, Codec};
use audiosync_airplay::session::{
    Output, ReceiverPorts, SessionEvent, State, UdpSendResult, Verifier,
};
use audiosync_airplay::volume::Volume;
use common::{
    CONTROL_PORT, FakeReceiver, Link, PasswordGate, RECEIVER, SENDER, START, TIMING_PORT, config,
    digest_params,
};

const URI: &str = "rtsp://192.0.2.10/3141592653";

fn playing() -> Link {
    let mut link = Link::new(config(), FakeReceiver::new());
    link.connect();
    assert_eq!(link.session.state(), State::Playing, "{:?}", link.events);
    link
}

fn kinds(link: &Link) -> Vec<ErrorKind> {
    link.events
        .iter()
        .filter_map(|event| match event {
            SessionEvent::Failed(error) => Some(error.kind),
            _ => None,
        })
        .collect()
}

fn warnings(link: &Link) -> Vec<Warning> {
    link.events
        .iter()
        .filter_map(|event| match event {
            SessionEvent::Warning(warning) => Some(warning.clone()),
            _ => None,
        })
        .collect()
}

// covers: P20, P26, P25, P32, P33, P119
#[test]
fn rtsp_happy_path_runs_options_to_record_then_the_initial_volume() {
    let mut link = Link::new(config(), FakeReceiver::new());
    assert!(!link.session.media_allowed());
    link.connect();
    assert_eq!(link.keys(), START);
    assert!(link.session.media_allowed());

    assert_eq!(link.last("OPTIONS").uri, "*");
    for key in ["ANNOUNCE", "SETUP", "RECORD", "SET_PARAMETER"] {
        assert_eq!(link.last(key).uri, URI, "{key}");
    }
    // The SETUP answer's Session goes on every later request (P25).
    for key in ["RECORD", "SET_PARAMETER"] {
        assert_eq!(link.last(key).headers.get("Session"), Some("1"), "{key}");
    }
    for key in ["OPTIONS", "ANNOUNCE", "SETUP"] {
        assert_eq!(link.last(key).headers.get("Session"), None, "{key}");
    }

    let record = link.last("RECORD");
    assert_eq!(record.headers.get("Range"), Some("npt=0-"));
    assert_eq!(
        record.headers.get("RTP-Info"),
        Some("seq=17000;rtptime=2000000000")
    );
    assert!(link.events.contains(&SessionEvent::Recording {
        first_seq: 17_000,
        first_rtptime: 2_000_000_000,
        audio_latency: Some(2205),
        receiver_ports: ReceiverPorts {
            server_port: 50_000,
            control_port: 50_001,
            timing_port: 50_002,
        },
    }));

    // The initial volume goes out right after RECORD (P119, P33).
    let volume = link.last("SET_PARAMETER");
    assert_eq!(volume.headers.get("Content-Type"), Some("text/parameters"));
    assert_eq!(volume.body, b"volume: -15.000000\r\n");

    // RTSP/1.0 text with CRLF line ends and a Content-Length body (P20).
    for bytes in &link.sent {
        let head_end = bytes
            .windows(4)
            .position(|window| window == b"\r\n\r\n")
            .expect("blank line");
        let head = &bytes[..head_end];
        for (index, &byte) in head.iter().enumerate() {
            if byte == b'\n' {
                assert_eq!(head[index - 1], b'\r', "bare LF in a request head");
            }
        }
        let text = String::from_utf8_lossy(head);
        let first = text.lines().next().expect("request line");
        assert!(first.ends_with(" RTSP/1.0"), "{first}");
        let body_len = bytes.len() - head_end - 4;
        if body_len > 0 {
            assert!(
                text.contains(&format!("\r\nContent-Length: {body_len}")),
                "{text}"
            );
        } else {
            assert!(!text.contains("Content-Length"), "{text}");
        }
    }
    assert!(!link.closed);
}

// covers: P26
#[test]
fn media_waits_for_the_record_answer_and_stops_at_teardown() {
    let mut receiver = FakeReceiver::new();
    receiver.hold = Some("RECORD".to_owned());
    let mut link = Link::new(config(), receiver);
    link.connect();
    assert_eq!(link.session.state(), State::Record);
    assert!(
        !link.session.media_allowed(),
        "no packet before RECORD's 200"
    );
    link.release();
    assert!(link.session.media_allowed());
    link.session.teardown(link.now);
    assert!(!link.session.media_allowed());

    // A refused RECORD never allows media.
    let mut receiver = FakeReceiver::new();
    receiver.status.insert("RECORD".to_owned(), 500);
    let mut link = Link::new(config(), receiver);
    link.connect();
    assert!(!link.session.media_allowed());
}

struct FakeVerifier {
    seen: Vec<Vec<u8>>,
}

impl Verifier for FakeVerifier {
    fn first_request(&mut self) -> Vec<u8> {
        vec![1, 0, 0, 0]
    }

    fn second_request(&mut self, first_response: &[u8]) -> Option<Vec<u8>> {
        self.seen.push(first_response.to_vec());
        (first_response.len() == 96).then(|| vec![0, 0, 0, 0])
    }
}

// covers: P26
#[test]
fn auth_setup_and_pair_verify_sit_between_options_and_announce() {
    let mut config = config();
    config.auth_setup_key = Some([7; 32]);
    config.verifier = Some(Box::new(FakeVerifier { seen: Vec::new() }));
    let mut link = Link::new(config, FakeReceiver::new());
    link.connect();
    assert_eq!(
        link.keys(),
        [
            "OPTIONS",
            "POST /auth-setup",
            "POST /pair-verify",
            "POST /pair-verify",
            "ANNOUNCE",
            "SETUP",
            "RECORD",
            "SET_PARAMETER"
        ]
    );
    let auth_setup = link.last("POST /auth-setup");
    assert_eq!(
        auth_setup.headers.get("Content-Type"),
        Some("application/octet-stream")
    );
    assert_eq!(auth_setup.body.len(), 33);
    assert_eq!(auth_setup.body[0], 0x01);
    assert_eq!(&auth_setup.body[1..], &[7; 32]);
    let verify = &link.receiver.requests[2];
    assert_eq!(
        verify.headers.get("Content-Type"),
        Some("application/octet-stream")
    );
    assert_eq!(verify.body, [1, 0, 0, 0]);
    assert_eq!(link.receiver.requests[3].body, [0, 0, 0, 0]);
}

#[test]
fn a_failed_auth_setup_is_a_warning_and_the_session_goes_on() {
    let mut config = config();
    config.auth_setup_key = Some([7; 32]);
    let mut receiver = FakeReceiver::new();
    receiver.status.insert("POST /auth-setup".to_owned(), 403);
    let mut link = Link::new(config, receiver);
    link.connect();
    assert_eq!(link.session.state(), State::Playing);
    assert_eq!(warnings(&link), [Warning::AuthSetupFailed { status: 403 }]);
}

#[test]
fn a_rejected_verification_reports_pairing_required() {
    let mut config = config();
    config.verifier = Some(Box::new(FakeVerifier { seen: Vec::new() }));
    let mut receiver = FakeReceiver::new();
    receiver.status.insert("POST /pair-verify".to_owned(), 470);
    let mut link = Link::new(config, receiver);
    link.connect();
    assert_eq!(kinds(&link), [ErrorKind::PairingRequired]);
    assert!(link.closed);
}

// covers: P21, P40, P140
#[test]
fn rtsp_cseq_starts_at_zero_and_any_stray_message_is_a_protocol_error() {
    let link = playing();
    let cseqs: Vec<&str> = link
        .receiver
        .requests
        .iter()
        .map(|request| request.headers.get("CSeq").expect("CSeq"))
        .collect();
    assert_eq!(cseqs, ["0", "1", "2", "3", "4"]);

    // A response with another CSeq.
    let mut receiver = FakeReceiver::new();
    receiver.cseq_offset = 1;
    let mut link = Link::new(config(), receiver);
    link.connect();
    let error = link.failure().expect("failed");
    assert_eq!(error.kind, ErrorKind::ProtocolError);
    assert_eq!(error.method.map(|m| m.as_str()), Some("OPTIONS"));
    assert!(link.closed);

    // A request from the receiver (P40).
    let mut link = playing();
    link.session
        .handle_input(b"POST /command RTSP/1.0\r\nCSeq: 1\r\n\r\n", link.now);
    link.pump();
    assert_eq!(kinds(&link), [ErrorKind::ProtocolError]);
    assert!(link.closed);

    // A response while nothing is outstanding (P40).
    let mut link = playing();
    link.session
        .handle_input(b"RTSP/1.0 200 OK\r\nCSeq: 9\r\n\r\n", link.now);
    link.pump();
    assert_eq!(kinds(&link), [ErrorKind::ProtocolError]);

    // A malformed status line and a missing CSeq (P140).
    for answer in [
        &b"RTSP/1.0 2OO OK\r\nCSeq: 0\r\n\r\n"[..],
        b"RTSP/1.0 200 OK\r\n\r\n",
    ] {
        let mut receiver = FakeReceiver::new();
        receiver.silent = Some("OPTIONS".to_owned());
        let mut link = Link::new(config(), receiver);
        link.connect();
        link.session.handle_input(answer, link.now);
        link.pump();
        assert_eq!(kinds(&link), [ErrorKind::ProtocolError]);
        assert!(link.closed);
    }
}

/// Bytes from a counter, so two sources give different identifiers.
struct CountingEntropy(u8);

impl Entropy for CountingEntropy {
    fn fill(&mut self, buf: &mut [u8]) -> Result<(), EntropyUnavailable> {
        for byte in buf {
            *byte = self.0;
            self.0 = self.0.wrapping_add(1);
        }
        Ok(())
    }
}

// covers: P22, P5, P23
#[test]
fn rtsp_common_headers_carry_random_per_session_identifiers() {
    let link = playing();
    for request in &link.receiver.requests {
        let headers = &request.headers;
        assert_eq!(
            headers.get("User-Agent"),
            Some(audiosync_airplay::user_agent().as_str())
        );
        assert_eq!(headers.get("Client-Instance"), Some("0123456789ABCDEF"));
        assert_eq!(headers.get("DACP-ID"), Some("0123456789ABCDEF"));
        assert_eq!(headers.get("Active-Remote"), Some("1234567890"));
    }
    assert_eq!(
        audiosync_airplay::user_agent(),
        format!("AudioSync/{}", env!("CARGO_PKG_VERSION"))
    );
    // The URI carries the sender's own address and the session ID (P23).
    assert_eq!(link.session.uri(), URI);
    // The SDP names AudioSync (P5).
    let sdp = String::from_utf8(link.last("ANNOUNCE").body.clone()).expect("UTF-8");
    assert!(sdp.contains("\r\no=AudioSync 3141592653 0 IN IP4 192.0.2.10\r\n"));
    assert!(sdp.contains("\r\ns=AudioSync\r\n"));

    // Identifiers come from the random source alone: different bytes give
    // different identifiers, and the same bytes the same ones.
    let a = SessionIds::generate(&mut CountingEntropy(0)).expect("ids");
    let b = SessionIds::generate(&mut CountingEntropy(100)).expect("ids");
    let a_again = SessionIds::generate(&mut CountingEntropy(0)).expect("ids");
    assert_eq!(a, a_again);
    assert_ne!(a.session_id, b.session_id);
    assert_ne!(a.client_instance, b.client_instance);
    assert_ne!(a.active_remote, b.active_remote);
    assert_eq!(a.client_instance_hex().len(), 16);
    assert_eq!(
        a.client_instance_hex(),
        a.client_instance_hex().to_uppercase()
    );

    // Two sessions from the OS source differ.
    let first = SessionIds::generate(&mut OsEntropy).expect("OS random source");
    let second = SessionIds::generate(&mut OsEntropy).expect("OS random source");
    assert_ne!(first, second);
    let mut link = Link::with_ids(config(), FakeReceiver::new(), first);
    link.connect();
    let mut other = Link::with_ids(config(), FakeReceiver::new(), second);
    other.connect();
    assert_ne!(
        link.last("OPTIONS").headers.get("Client-Instance"),
        other.last("OPTIONS").headers.get("Client-Instance")
    );
    assert_ne!(link.session.uri(), other.session.uri());
}

// covers: P27
#[test]
fn options_records_the_method_list_and_needs_announce_setup_and_record() {
    let link = playing();
    let options = link.last("OPTIONS");
    assert!(!options.headers.contains("Apple-Challenge"));
    assert!(link.events.iter().any(|event| matches!(
        event,
        SessionEvent::Methods(methods) if methods.contains(&"SET_PARAMETER".to_owned())
    )));
    assert_eq!(link.session.methods().map(<[String]>::len), Some(7));

    // A missing Public header is accepted.
    let mut receiver = FakeReceiver::new();
    receiver.public = None;
    let mut link = Link::new(config(), receiver);
    link.connect();
    assert_eq!(link.session.state(), State::Playing);

    // A list without RECORD makes the receiver unsupported.
    let mut receiver = FakeReceiver::new();
    receiver.public = Some("ANNOUNCE, SETUP, TEARDOWN, OPTIONS".to_owned());
    let mut link = Link::new(config(), receiver);
    link.connect();
    assert_eq!(
        kinds(&link),
        [ErrorKind::Unsupported(UnsupportedReason::Methods)]
    );
    assert_eq!(link.keys(), ["OPTIONS"]);
}

// covers: P28, P30
#[test]
fn announce_carries_the_sdp_for_the_chosen_codec() {
    let link = playing();
    let announce = link.last("ANNOUNCE");
    assert_eq!(
        announce.headers.get("Content-Type"),
        Some("application/sdp")
    );
    assert_eq!(
        announce.body,
        sdp::announce_body(3_141_592_653, SENDER, RECEIVER, Codec::Alac).into_bytes()
    );

    let mut config = config();
    config.codec = Codec::Pcm;
    let mut link = Link::new(config, FakeReceiver::new());
    link.connect();
    let body = String::from_utf8(link.last("ANNOUNCE").body.clone()).expect("UTF-8");
    assert!(body.contains("\r\na=rtpmap:96 L16/44100/2\r\n"));
    assert!(!body.contains("fmtp"));
}

// Part of P31 (sockets open and answering before SETUP need P82, transport stage).
#[test]
fn rtsp_setup_transport_announces_the_local_ports_and_reads_the_receivers() {
    let link = playing();
    assert_eq!(
        link.last("SETUP").headers.get("Transport"),
        Some(
            format!(
                "RTP/AVP/UDP;unicast;interleaved=0-1;mode=record;control_port={CONTROL_PORT};timing_port={TIMING_PORT}"
            )
            .as_str()
        )
    );
    assert_eq!(
        link.session.receiver_ports(),
        Some(ReceiverPorts {
            server_port: 50_000,
            control_port: 50_001,
            timing_port: 50_002
        })
    );

    // timing_port=0 is accepted.
    let mut receiver = FakeReceiver::new();
    receiver.transport =
        "RTP/AVP/UDP;unicast;mode=record;server_port=50000;control_port=50001;timing_port=0"
            .to_owned();
    let mut link = Link::new(config(), receiver);
    link.connect();
    assert_eq!(link.session.state(), State::Playing);
    assert_eq!(
        link.session.receiver_ports().map(|p| p.timing_port),
        Some(0)
    );

    // Without server_port, control_port or Session: a protocol error.
    for (transport, session) in [
        (
            "RTP/AVP/UDP;unicast;mode=record;control_port=50001;timing_port=50002",
            Some("1"),
        ),
        (
            "RTP/AVP/UDP;unicast;mode=record;server_port=50000;timing_port=50002",
            Some("1"),
        ),
        (
            "RTP/AVP/UDP;unicast;mode=record;server_port=50000;control_port=50001",
            None,
        ),
    ] {
        let mut receiver = FakeReceiver::new();
        receiver.transport = transport.to_owned();
        receiver.session = session.map(str::to_owned);
        let mut link = Link::new(config(), receiver);
        link.connect();
        let error = link.failure().expect("failed");
        assert_eq!(error.kind, ErrorKind::ProtocolError, "{transport}");
        assert_eq!(error.method.map(|m| m.as_str()), Some("SETUP"));
        assert!(!link.keys().contains(&"RECORD".to_owned()));
    }
}

// covers: P24
#[test]
fn server_and_audio_jack_status_are_reported_and_a_disconnected_jack_warns() {
    let link = playing();
    assert_eq!(link.session.server(), Some("FakeReceiver/1.0"));
    let reported: Vec<&SessionEvent> = link
        .events
        .iter()
        .filter(|event| matches!(event, SessionEvent::Server(_) | SessionEvent::AudioJack(_)))
        .collect();
    assert_eq!(
        reported,
        [
            &SessionEvent::Server("FakeReceiver/1.0".to_owned()),
            &SessionEvent::AudioJack(JackStatus {
                connected: true,
                kind: Some(JackKind::Analog)
            }),
        ],
        "reported once, not on every answer"
    );
    assert!(warnings(&link).is_empty());

    let mut receiver = FakeReceiver::new();
    receiver.jack = Some("disconnected".to_owned());
    let mut link = Link::new(config(), receiver);
    link.connect();
    assert_eq!(
        link.session.state(),
        State::Playing,
        "a warning, not an error"
    );
    assert_eq!(warnings(&link), [Warning::JackDisconnected]);
}

#[test]
fn a_session_header_with_a_timeout_is_sent_back_without_it() {
    let mut receiver = FakeReceiver::new();
    receiver.session = Some("DEADBEEF;timeout=60".to_owned());
    let mut link = Link::new(config(), receiver);
    link.connect();
    assert_eq!(link.last("RECORD").headers.get("Session"), Some("DEADBEEF"));
}

// covers: P25
#[test]
fn every_request_after_setup_carries_the_session() {
    let mut link = playing();
    link.session.flush(17_352, 2_000_000_352, link.now);
    link.pump();
    assert!(link.session.read_volume(link.now));
    link.pump();
    link.session.teardown(link.now);
    link.pump();
    for key in [
        "RECORD",
        "SET_PARAMETER",
        "FLUSH",
        "GET_PARAMETER",
        "TEARDOWN",
    ] {
        assert_eq!(link.last(key).headers.get("Session"), Some("1"), "{key}");
    }
}

#[test]
fn flush_names_the_next_packet_and_reports_the_answer() {
    let mut link = playing();
    assert!(link.session.flush(17_352, 2_000_000_352, link.now));
    link.pump();
    let flush = link.last("FLUSH");
    assert_eq!(flush.uri, URI);
    assert_eq!(
        flush.headers.get("RTP-Info"),
        Some("seq=17352;rtptime=2000000352")
    );
    assert!(link.events.contains(&SessionEvent::Flushed {
        seq: 17_352,
        rtptime: 2_000_000_352
    }));
    // Before RECORD there is nothing to flush.
    let mut receiver = FakeReceiver::new();
    receiver.silent = Some("SETUP".to_owned());
    let mut link = Link::new(config(), receiver);
    link.connect();
    assert!(!link.session.flush(1, 1, link.now));
}

// covers: P35
#[test]
fn rtsp_teardown_sends_the_session_and_closes_after_the_answer_or_2_s() {
    let mut link = playing();
    link.session.teardown(link.now);
    assert_eq!(link.session.state(), State::TearingDown);
    link.pump();
    let teardown = link.last("TEARDOWN");
    assert_eq!(teardown.uri, URI);
    assert_eq!(teardown.headers.get("Session"), Some("1"));
    assert!(teardown.body.is_empty());
    assert!(link.closed);
    assert_eq!(link.events.last(), Some(&SessionEvent::Closed));
    assert_eq!(link.session.state(), State::Closed);

    // A silent receiver: closed 2 s after the call.
    let mut link = playing();
    link.receiver.silent = Some("TEARDOWN".to_owned());
    link.session.teardown(link.now);
    link.pump();
    assert!(!link.closed);
    link.advance(Duration::from_millis(1999));
    assert!(!link.closed);
    link.advance(Duration::from_millis(1));
    assert!(link.closed);
    assert_eq!(link.events.last(), Some(&SessionEvent::Closed));
    assert!(link.failure().is_none(), "a slow teardown is not an error");

    // A request in flight is answered first, still within the 2 s.
    let mut link = playing();
    link.receiver.hold = Some("SET_PARAMETER".to_owned());
    link.advance(Duration::from_millis(200));
    link.session.set_volume(Volume::Db(-5.0), link.now);
    link.pump();
    link.session.teardown(link.now);
    link.pump();
    assert!(!link.keys().contains(&"TEARDOWN".to_owned()));
    link.release();
    assert_eq!(link.keys().last().map(String::as_str), Some("TEARDOWN"));
    assert!(link.closed);

    // Before SETUP there is no receiver session: the connection just closes.
    let mut receiver = FakeReceiver::new();
    receiver.silent = Some("ANNOUNCE".to_owned());
    let mut link = Link::new(config(), receiver);
    link.connect();
    link.session.teardown(link.now);
    link.pump();
    assert!(link.closed);
    assert!(!link.keys().contains(&"TEARDOWN".to_owned()));
}

fn password_config(password: &str) -> audiosync_airplay::session::SessionConfig {
    let mut config = config();
    config.password = Some(password.to_owned());
    config
}

// covers: P36, P38
#[test]
fn digest_retry_and_reuse() {
    let mut receiver = FakeReceiver::new();
    receiver.gate = Some(PasswordGate::new("hunter2"));
    let mut link = Link::new(password_config("hunter2"), receiver);
    link.connect();
    assert_eq!(link.session.state(), State::Playing, "{:?}", link.events);
    // The challenged OPTIONS goes again with the next CSeq and Authorization.
    let requests = &link.receiver.requests;
    assert_eq!(requests[0].headers.get("CSeq"), Some("0"));
    assert!(requests[0].headers.get("Authorization").is_none());
    assert_eq!(requests[1].headers.get("CSeq"), Some("1"));
    assert_eq!(link.keys()[..2], ["OPTIONS", "OPTIONS"]);
    // Every later request carries Authorization for its own method and URI.
    for request in &requests[1..] {
        let params = digest_params(request.headers.get("Authorization").expect("Authorization"));
        assert_eq!(params["username"], "iTunes");
        assert_eq!(params["realm"], "raop");
        assert_eq!(params["nonce"], "a1b2c3d4e5f60718293a4b5c6d7e8f90");
        assert_eq!(params["uri"], request.uri);
        assert!(!params.contains_key("qop"));
    }
    assert_eq!(link.receiver.authorized, 5);

    // A stale nonce: one retry with the new nonce, no WrongPassword.
    let mut receiver = FakeReceiver::new();
    let mut gate = PasswordGate::new("hunter2");
    gate.stale_on = Some(3);
    receiver.gate = Some(gate);
    let mut link = Link::new(password_config("hunter2"), receiver);
    link.connect();
    assert_eq!(link.session.state(), State::Playing, "{:?}", link.events);
    assert_eq!(
        link.keys(),
        [
            "OPTIONS",
            "OPTIONS",
            "ANNOUNCE",
            "SETUP",
            "SETUP",
            "RECORD",
            "SET_PARAMETER"
        ]
    );
    let new_nonce = link.receiver.gate.as_ref().expect("gate").nonce.clone();
    assert_ne!(new_nonce, "a1b2c3d4e5f60718293a4b5c6d7e8f90");
    for request in &link.receiver.requests[4..] {
        let params = digest_params(request.headers.get("Authorization").expect("Authorization"));
        assert_eq!(params["nonce"], new_nonce);
    }

    // With qop=auth: nc counts the requests sent with the nonce.
    let mut receiver = FakeReceiver::new();
    let mut gate = PasswordGate::new("hunter2");
    gate.qop = true;
    receiver.gate = Some(gate);
    let mut link = Link::new(password_config("hunter2"), receiver);
    link.connect();
    assert_eq!(link.session.state(), State::Playing, "{:?}", link.events);
    let counts: Vec<String> = link.receiver.requests[1..]
        .iter()
        .map(|request| {
            digest_params(request.headers.get("Authorization").expect("auth"))["nc"].clone()
        })
        .collect();
    assert_eq!(
        counts,
        ["00000001", "00000002", "00000003", "00000004", "00000005"]
    );
}

// covers: P135
#[test]
fn digest_wrong_or_missing_password() {
    // No password from the host.
    let mut receiver = FakeReceiver::new();
    receiver.gate = Some(PasswordGate::new("hunter2"));
    let mut link = Link::new(config(), receiver);
    link.connect();
    let error = link.failure().expect("failed");
    assert_eq!(error.kind, ErrorKind::PasswordRequired);
    assert_eq!(error.status, Some(401));
    assert_eq!(link.keys(), ["OPTIONS"], "no retry");
    assert!(link.closed);

    // A wrong password: the second 401 ends it.
    let mut receiver = FakeReceiver::new();
    receiver.gate = Some(PasswordGate::new("hunter2"));
    let mut link = Link::new(password_config("letmein"), receiver);
    link.connect();
    assert_eq!(kinds(&link), [ErrorKind::WrongPassword]);
    assert_eq!(link.keys(), ["OPTIONS", "OPTIONS"], "one retry, then stop");
    assert!(link.closed);
    // Neither is retried by the host on its own (P142's stop list).
    assert!(ErrorKind::PasswordRequired.needs_user());
    assert!(ErrorKind::WrongPassword.needs_user());
}

// covers: P39
#[test]
fn the_password_never_shows_in_diagnostics_or_on_the_wire() {
    let mut receiver = FakeReceiver::new();
    receiver.gate = Some(PasswordGate::new("hunter2"));
    let mut link = Link::new(password_config("hunter2"), receiver);
    link.connect();
    assert_eq!(link.session.state(), State::Playing);
    let debug = format!("{:?} {:?}", link.session, password_config("hunter2"));
    assert!(!debug.contains("hunter2"), "{debug}");
    for bytes in &link.sent {
        assert!(!String::from_utf8_lossy(bytes).contains("hunter2"));
    }
    for event in &link.events {
        assert!(!format!("{event:?}").contains("hunter2"));
    }
}

// covers: P46
#[test]
fn an_apple_tv_without_credentials_that_refuses_announce_needs_pairing() {
    let mut config = config();
    config.apple_tv_without_credentials = true;
    let mut receiver = FakeReceiver::new();
    receiver.status.insert("ANNOUNCE".to_owned(), 403);
    let mut link = Link::new(config, receiver);
    link.connect();
    let error = link.failure().expect("failed");
    assert_eq!(error.kind, ErrorKind::PairingRequired);
    assert_eq!(error.status, Some(403));

    // 453 stays Busy, and other receivers stay Refused.
    for (apple_tv, status, kind) in [
        (true, 453, ErrorKind::Busy),
        (true, 500, ErrorKind::Refused),
        (false, 403, ErrorKind::Refused),
    ] {
        let mut config = common::config();
        config.apple_tv_without_credentials = apple_tv;
        let mut receiver = FakeReceiver::new();
        receiver.status.insert("ANNOUNCE".to_owned(), status);
        let mut link = Link::new(config, receiver);
        link.connect();
        assert_eq!(kinds(&link), [kind], "{apple_tv} {status}");
    }
}

// covers: P74
#[test]
fn a_refused_pcm_announce_asks_for_one_retry_with_alac() {
    let mut config = config();
    config.codec = Codec::Pcm;
    config.alac_fallback = true;
    let mut receiver = FakeReceiver::new();
    receiver.refuse_pcm = Some(406);
    let mut link = Link::new(config, receiver);
    link.connect();
    let error = link.failure().expect("failed");
    assert_eq!(error.kind, ErrorKind::Refused);
    assert_eq!(error.status, Some(406));
    assert_eq!(error.fallback, Some(Codec::Alac));
    assert!(link.closed);

    // The retry is a new session with ALAC, which the same receiver takes.
    let mut receiver = FakeReceiver::new();
    receiver.refuse_pcm = Some(406);
    let mut retry = Link::new(common::config(), receiver);
    retry.connect();
    assert_eq!(retry.session.state(), State::Playing);

    // Without ALAC in `cn` there is no fallback.
    let mut config = common::config();
    config.codec = Codec::Pcm;
    let mut receiver = FakeReceiver::new();
    receiver.refuse_pcm = Some(406);
    let mut link = Link::new(config, receiver);
    link.connect();
    assert_eq!(link.failure().expect("failed").fallback, None);
}

// covers: P20, P134
#[test]
fn errors_connect_and_response_timeouts() {
    // A refused or unreachable connect.
    let mut link = Link::new(config(), FakeReceiver::new());
    link.session.handle_connect_failed(link.now);
    link.pump();
    assert_eq!(kinds(&link), [ErrorKind::Unreachable]);
    assert!(link.closed);

    // No connect within 5 s.
    let mut link = Link::new(config(), FakeReceiver::new());
    assert_eq!(
        link.session.poll_timeout(),
        Some(link.start + Duration::from_secs(5))
    );
    link.advance(Duration::from_millis(4999));
    assert!(kinds(&link).is_empty());
    link.advance(Duration::from_millis(1));
    assert_eq!(kinds(&link), [ErrorKind::Unreachable]);

    // No answer within 10 s.
    let mut receiver = FakeReceiver::new();
    receiver.silent = Some("SETUP".to_owned());
    let mut link = Link::new(config(), receiver);
    link.connect();
    link.advance(Duration::from_millis(9999));
    assert!(kinds(&link).is_empty());
    link.advance(Duration::from_millis(1));
    let error = link.failure().expect("timed out");
    assert_eq!(error.kind, ErrorKind::Timeout);
    assert_eq!(error.method.map(|m| m.as_str()), Some("SETUP"));
    assert!(link.closed);
    assert_eq!(link.session.poll_timeout(), None);
}

// covers: P20
#[test]
fn one_request_is_in_flight_at_a_time() {
    let mut link = playing();
    link.receiver.hold = Some("FLUSH".to_owned());
    link.advance(Duration::from_millis(500));
    link.session.flush(17_352, 2_000_000_352, link.now);
    link.session.set_volume(Volume::Db(-3.0), link.now);
    link.session.read_volume(link.now);
    link.pump();
    assert_eq!(link.keys().last().map(String::as_str), Some("FLUSH"));
    let before = link.keys().len();
    link.advance(Duration::from_secs(1));
    assert_eq!(
        link.keys().len(),
        before,
        "nothing else until FLUSH is answered"
    );
    link.release();
    assert_eq!(
        link.keys()[before..],
        ["SET_PARAMETER", "GET_PARAMETER"],
        "then the queue drains, volume first"
    );
}

// covers: P133, P138
#[test]
fn errors_refused_per_method_and_warnings_after_record() {
    for (key, method) in [
        ("OPTIONS", "OPTIONS"),
        ("ANNOUNCE", "ANNOUNCE"),
        ("SETUP", "SETUP"),
        ("RECORD", "RECORD"),
    ] {
        for status in [400, 403, 404, 500, 503] {
            let mut receiver = FakeReceiver::new();
            receiver.status.insert(key.to_owned(), status);
            let mut link = Link::new(config(), receiver);
            link.connect();
            let error = link.failure().expect("failed");
            assert_eq!(error.kind, ErrorKind::Refused, "{key} {status}");
            assert_eq!(error.method.map(|m| m.as_str()), Some(method));
            assert_eq!(error.status, Some(status));
            assert!(link.closed);
            assert!(!error.kind.needs_user());
        }
    }

    // After RECORD, a failure status is only a warning.
    let mut link = playing();
    for key in ["SET_PARAMETER", "GET_PARAMETER", "FLUSH"] {
        link.receiver.status.insert(key.to_owned(), 500);
    }
    link.advance(Duration::from_millis(200));
    link.session.set_volume(Volume::Db(-6.0), link.now);
    link.session.read_volume(link.now);
    link.session.flush(1, 1, link.now);
    link.pump();
    assert_eq!(link.session.state(), State::Playing);
    assert_eq!(warnings(&link).len(), 3);
    assert!(
        warnings(&link)
            .iter()
            .all(|warning| matches!(warning, Warning::RequestFailed { status: 500, .. }))
    );
    assert!(!link.closed);
}

// covers: P137
#[test]
fn errors_busy_on_453_to_announce_setup_or_record() {
    for key in ["ANNOUNCE", "SETUP", "RECORD"] {
        let mut receiver = FakeReceiver::new();
        receiver.status.insert(key.to_owned(), 453);
        let mut link = Link::new(config(), receiver);
        link.connect();
        let error = link.failure().expect("failed");
        assert_eq!(error.kind, ErrorKind::Busy, "{key}");
        assert_eq!(error.status, Some(453));
    }
    // OPTIONS isn't one of them.
    let mut receiver = FakeReceiver::new();
    receiver.status.insert("OPTIONS".to_owned(), 453);
    let mut link = Link::new(config(), receiver);
    link.connect();
    assert_eq!(kinds(&link), [ErrorKind::Refused]);
}

// covers: P116, P118
#[test]
fn volume_requests_carry_the_level_and_mute() {
    let mut link = playing();
    link.advance(Duration::from_millis(200));
    link.session.set_volume(Volume::Muted, link.now);
    link.pump();
    assert_eq!(link.last("SET_PARAMETER").body, b"volume: -144.000000\r\n");
    link.advance(Duration::from_millis(200));
    link.session
        .set_volume(Volume::from_db(-45.0).expect("clamped"), link.now);
    link.pump();
    assert_eq!(link.last("SET_PARAMETER").body, b"volume: -30.000000\r\n");
    // NaN is ignored.
    let before = link.keys().len();
    link.advance(Duration::from_millis(200));
    link.session.set_volume(Volume::Db(f64::NAN), link.now);
    link.pump();
    assert_eq!(link.keys().len(), before);

    // A volume set before RECORD becomes the initial volume.
    let mut receiver = FakeReceiver::new();
    receiver.hold = Some("SETUP".to_owned());
    let mut link = Link::new(config(), receiver);
    link.connect();
    link.session.set_volume(Volume::Db(-7.5), link.now);
    link.release();
    assert_eq!(link.last("SET_PARAMETER").body, b"volume: -7.500000\r\n");
    assert_eq!(
        link.keys().iter().filter(|k| *k == "SET_PARAMETER").count(),
        1
    );
}

// covers: P120, P127
#[test]
fn volume_coalescing_and_priority_over_metadata() {
    let mut config = config();
    config.metadata = MetadataSupport {
        text: true,
        artwork: true,
        progress: true,
    };
    let mut link = Link::new(config, FakeReceiver::new());
    link.connect();
    link.receiver.hold = Some("SET_PARAMETER".to_owned());
    link.advance(Duration::from_millis(200));
    link.session.set_volume(Volume::Db(-10.0), link.now);
    link.pump();
    // Newer values replace the queued one; metadata waits behind volume.
    link.session.set_volume(Volume::Db(-11.0), link.now);
    link.session.set_volume(Volume::Db(-12.0), link.now);
    let track = Metadata::Track {
        info: TrackInfo {
            title: "Song".to_owned(),
            artist: "Band".to_owned(),
            album: "Record".to_owned(),
        },
        rtptime: 2_000_000_000,
    };
    link.session
        .send_metadata(track.clone(), link.now)
        .expect("listed");
    link.pump();
    link.release();
    link.advance(Duration::from_millis(100));
    link.release();
    let bodies: Vec<Vec<u8>> = link
        .receiver
        .requests
        .iter()
        .filter(|request| common::key(request) == "SET_PARAMETER")
        .map(|request| request.body.clone())
        .collect();
    assert_eq!(
        bodies,
        [
            b"volume: -15.000000\r\n".to_vec(),
            b"volume: -10.000000\r\n".to_vec(),
            b"volume: -12.000000\r\n".to_vec(),
            track.body(),
        ]
    );

    // At most 10 volume requests per second, however fast the host moves.
    let mut link = playing();
    let volume_count = |link: &Link| link.keys().iter().filter(|k| *k == "SET_PARAMETER").count();
    let before = volume_count(&link);
    let start = link.now;
    for step in 0..100 {
        link.advance(Duration::from_millis(10));
        link.session
            .set_volume(Volume::Db(-20.0 + f64::from(step) * 0.1), link.now);
        link.pump();
    }
    assert_eq!(link.now - start, Duration::from_secs(1));
    assert_eq!(volume_count(&link) - before, 10, "one every 100 ms");
    // The latest value arrives once the interval allows.
    link.advance(Duration::from_millis(100));
    assert_eq!(link.last("SET_PARAMETER").body, b"volume: -10.100000\r\n");

    // A failed metadata request is a warning only.
    let mut config = common::config();
    config.metadata.text = true;
    let mut link = Link::new(config, FakeReceiver::new());
    link.connect();
    link.receiver.status.insert("SET_PARAMETER".to_owned(), 500);
    link.session.send_metadata(track, link.now).expect("listed");
    link.pump();
    assert_eq!(link.session.state(), State::Playing);
    assert_eq!(warnings(&link).len(), 1);
}

// Part of P121: the sample scaling itself comes with the audio stage.
#[test]
fn software_volume_receivers_get_no_volume_requests() {
    let mut config = config();
    config.software_volume = true;
    let mut link = Link::new(config, FakeReceiver::new());
    link.connect();
    link.advance(Duration::from_millis(200));
    link.session.set_volume(Volume::Db(-3.0), link.now);
    link.pump();
    assert_eq!(link.keys(), ["OPTIONS", "ANNOUNCE", "SETUP", "RECORD"]);
}

// covers: P122
#[test]
fn volume_read_back_reports_without_changing_the_senders_volume() {
    let mut link = playing();
    assert!(link.session.read_volume(link.now));
    link.pump();
    let read = link.last("GET_PARAMETER");
    assert_eq!(read.headers.get("Content-Type"), Some("text/parameters"));
    assert_eq!(read.body, b"volume\r\n");
    assert!(link.events.contains(&SessionEvent::VolumeReported(-20.0)));
    let volume_requests = link.keys().iter().filter(|k| *k == "SET_PARAMETER").count();
    link.advance(Duration::from_secs(1));
    assert_eq!(
        link.keys().iter().filter(|k| *k == "SET_PARAMETER").count(),
        volume_requests
    );
}

// covers: P123, P124, P125, P126
#[test]
fn metadata_requests_follow_the_listed_types() {
    let mut config = config();
    config.metadata = MetadataSupport {
        text: true,
        artwork: true,
        progress: true,
    };
    let mut link = Link::new(config, FakeReceiver::new());
    link.connect();
    let info = TrackInfo {
        title: "Song".to_owned(),
        artist: "Band".to_owned(),
        album: "Record".to_owned(),
    };
    link.session
        .send_metadata(
            Metadata::Track {
                info,
                rtptime: 2_000_000_352,
            },
            link.now,
        )
        .expect("text listed");
    link.pump();
    let track = link.last("SET_PARAMETER");
    assert_eq!(
        track.headers.get("Content-Type"),
        Some("application/x-dmap-tagged")
    );
    assert_eq!(track.headers.get("RTP-Info"), Some("rtptime=2000000352"));
    let mut expected = b"mlit\x00\x00\x00\x26".to_vec();
    expected.extend(b"minm\x00\x00\x00\x04Song");
    expected.extend(b"asar\x00\x00\x00\x04Band");
    expected.extend(b"asal\x00\x00\x00\x06Record");
    assert_eq!(track.body, expected);

    let jpeg = [&[0xFF, 0xD8, 0xFF, 0xE0][..], &[0u8; 60]].concat();
    link.session
        .send_metadata(
            Metadata::Artwork {
                jpeg: jpeg.clone(),
                rtptime: 7,
            },
            link.now,
        )
        .expect("artwork listed");
    link.pump();
    let artwork = link.last("SET_PARAMETER");
    assert_eq!(artwork.headers.get("Content-Type"), Some("image/jpeg"));
    assert_eq!(artwork.headers.get("RTP-Info"), Some("rtptime=7"));
    assert_eq!(artwork.body, jpeg);

    link.session
        .send_metadata(
            Metadata::Progress {
                start: 100,
                current: 200,
                end: 300,
            },
            link.now,
        )
        .expect("progress listed");
    link.pump();
    let progress = link.last("SET_PARAMETER");
    assert_eq!(
        progress.headers.get("Content-Type"),
        Some("text/parameters")
    );
    assert_eq!(progress.body, b"progress: 100/200/300\r\n");

    // Artwork must be a JPEG of at most 512 KiB (P125).
    let too_big = [&[0xFF, 0xD8, 0xFF][..], &vec![0u8; 512 * 1024]].concat();
    assert_eq!(
        link.session.send_metadata(
            Metadata::Artwork {
                jpeg: too_big,
                rtptime: 0
            },
            link.now
        ),
        Err(MetadataError::ArtworkTooLarge)
    );
    assert_eq!(
        link.session.send_metadata(
            Metadata::Artwork {
                jpeg: b"\x89PNG".to_vec(),
                rtptime: 0
            },
            link.now
        ),
        Err(MetadataError::NotJpeg)
    );

    // A receiver that lists nothing gets nothing (P123).
    let mut link = playing();
    let before = link.keys().len();
    assert_eq!(
        link.session.send_metadata(
            Metadata::Progress {
                start: 1,
                current: 2,
                end: 3
            },
            link.now
        ),
        Err(MetadataError::NotListed)
    );
    link.pump();
    assert_eq!(link.keys().len(), before);
}

// covers: P130, P131
// Part of P141 (other speakers keep playing: stream stage).
#[test]
fn keepalive_ap1() {
    // OPTIONS after 30 s without a request; a silent stream never ends it.
    let mut link = playing();
    let idle_from = link.now;
    for _ in 0..20 {
        link.session.handle_timing_request(link.now);
        link.advance(Duration::from_secs(3));
    }
    assert_eq!(link.session.state(), State::Playing, "60 s without audio");
    let keepalives: Vec<&audiosync_airplay::rtsp::Request> = link.receiver.requests[START.len()..]
        .iter()
        .filter(|request| request.method.as_str() == "OPTIONS")
        .collect();
    assert_eq!(keepalives.len(), 2, "at 30 s and 60 s");
    assert!(keepalives.iter().all(|request| request.uri == "*"));
    assert!(link.now - idle_from >= Duration::from_secs(60));

    // 10 s without timing requests after the first: lost.
    let mut link = playing();
    link.session.handle_timing_request(link.now);
    link.advance(Duration::from_millis(9999));
    assert_eq!(link.session.state(), State::Playing);
    link.advance(Duration::from_millis(1));
    assert_eq!(kinds(&link), [ErrorKind::Lost]);
    assert!(link.closed);

    // No timing requests at all: judged by the connection alone.
    let mut link = playing();
    link.advance_in_steps(Duration::from_secs(60), Duration::from_secs(1));
    assert_eq!(link.session.state(), State::Playing);

    // The connection closing, or two unreachable UDP sends in a row: lost.
    let mut link = playing();
    link.session.handle_connection_closed(link.now);
    link.pump();
    assert_eq!(kinds(&link), [ErrorKind::Lost]);

    let mut link = playing();
    link.session
        .handle_udp_send(UdpSendResult::Unreachable, link.now);
    link.session.handle_udp_send(UdpSendResult::Sent, link.now);
    link.session
        .handle_udp_send(UdpSendResult::Unreachable, link.now);
    link.pump();
    assert_eq!(link.session.state(), State::Playing, "not in a row");
    link.session
        .handle_udp_send(UdpSendResult::Unreachable, link.now);
    link.pump();
    assert_eq!(kinds(&link), [ErrorKind::Lost]);
    assert!(link.closed);
    assert!(!ErrorKind::Lost.needs_user());
}

#[test]
fn outputs_end_with_close_and_nothing_follows_it() {
    let mut link = playing();
    link.session.handle_connection_closed(link.now);
    let outputs: Vec<Output> = std::iter::from_fn(|| link.session.poll_output()).collect();
    assert_eq!(outputs[0], Output::Close);
    assert!(matches!(outputs[1], Output::Event(SessionEvent::Failed(_))));
    link.session
        .handle_input(b"RTSP/1.0 200 OK\r\nCSeq: 5\r\n\r\n", link.now);
    link.session
        .handle_timeout(link.now + Duration::from_secs(60));
    assert_eq!(link.session.poll_output(), None);
}
