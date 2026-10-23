//! Parts of docs/AIRPLAY_SPEC.md clauses that the per-section suites name
//! but don't reach: the keep-alive's idle clock, digest edge cases, header
//! case, teardown behind an unanswered request, the `/feedback` miss count
//! and the sync sequence across a flush.

mod common;

use std::time::Duration;

use audiosync_airplay::ap2::bodies::{InfoAnswer, read_info};
use audiosync_airplay::error::ErrorKind;
use audiosync_airplay::metadata::{MAX_ARTWORK_BYTES, Metadata, MetadataSupport, TrackInfo};
use audiosync_airplay::rtp::SyncPacket;
use audiosync_airplay::sdp::Codec;
use audiosync_airplay::secret::Secret;
use audiosync_airplay::session::{SessionConfig, SessionEvent, State};
use audiosync_airplay::stream::PacketKind;
use audiosync_airplay::volume::Volume;
use common::ap2::{Ap2Rig, FakeAp2, ap2_config, dict, int, plist};
use common::rig::{Rig, speaker};
use common::{FakeReceiver, Link, PasswordGate, START, config, digest_params};
use plist::Value;

fn playing() -> Link {
    let mut link = Link::new(config(), FakeReceiver::new());
    link.connect();
    assert_eq!(link.session.state(), State::Playing, "{:?}", link.events);
    link
}

fn password_config(password: &str) -> SessionConfig {
    let mut config = config();
    config.password = Some(Secret::from(password));
    config
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

fn text(value: &str) -> Value {
    Value::String(value.to_owned())
}

fn count(link: &Link, key: &str) -> usize {
    link.keys().iter().filter(|k| *k == key).count()
}

/// The held answer with every header name rewritten by `name`.
fn rename_headers(held: &[u8], name: fn(&str) -> String) -> Vec<u8> {
    let text = String::from_utf8(held.to_vec()).expect("text answer");
    let (head, body) = text.split_once("\r\n\r\n").expect("whole answer");
    let mut lines = head.split("\r\n");
    let mut out = lines.next().expect("status line").to_owned();
    for line in lines {
        let (key, value) = line.split_once(':').expect("header line");
        out.push_str(&format!("\r\n{}:{value}", name(key)));
    }
    out.push_str("\r\n\r\n");
    out.push_str(body);
    out.into_bytes()
}

// covers: P131
#[test]
fn p131_any_request_restarts_the_keepalive_idle_time() {
    let mut link = playing();
    link.advance(Duration::from_secs(20));
    link.session.set_volume(Volume::Db(-6.0), link.now);
    link.pump();
    // 40 s since the start, 20 s since the volume request: no keep-alive.
    link.advance(Duration::from_secs(20));
    assert_eq!(count(&link, "OPTIONS"), 1, "{:?}", link.keys());
    // 30 s after the volume request it goes.
    link.advance(Duration::from_secs(10));
    assert_eq!(count(&link, "OPTIONS"), 2, "{:?}", link.keys());
    assert_eq!(link.last("OPTIONS").uri, "*");
}

// covers: P131, P134
#[test]
fn p131_an_unanswered_keepalive_is_a_timeout() {
    let mut link = playing();
    link.receiver.silent = Some("OPTIONS".to_owned());
    link.advance(Duration::from_secs(30));
    assert_eq!(count(&link, "OPTIONS"), 2);
    link.advance(Duration::from_millis(9_999));
    assert!(link.failure().is_none());
    link.advance(Duration::from_millis(1));
    let error = link.failure().expect("timed out");
    assert_eq!(error.kind, ErrorKind::Timeout);
    assert_eq!(error.method.map(|m| m.as_str()), Some("OPTIONS"));
    assert!(link.closed);
}

// covers: P122
#[test]
fn p122_a_volume_answer_after_an_empty_line_is_reported() {
    let mut receiver = FakeReceiver::new();
    receiver.volume_answer = "\r\nvolume: -12.500000\r\n".to_owned();
    let mut link = Link::new(config(), receiver);
    link.connect();
    assert!(link.session.read_volume(link.now));
    link.pump();
    assert!(link.events.contains(&SessionEvent::VolumeReported(-12.5)));
    assert_eq!(link.session.state(), State::Playing);
}

// covers: P24
#[test]
fn p24_answer_header_names_match_in_any_case() {
    for name in [str::to_ascii_lowercase, str::to_ascii_uppercase] {
        let mut receiver = FakeReceiver::new();
        receiver.hold = Some("SETUP".to_owned());
        let mut link = Link::new(config(), receiver);
        link.connect();
        let held = std::mem::take(&mut link.receiver.held);
        link.receiver.held = rename_headers(&held, name);
        link.receiver.hold = None;
        link.release();
        assert_eq!(link.session.state(), State::Playing, "{:?}", link.events);
        assert_eq!(link.keys(), START);
        assert_eq!(link.last("RECORD").headers.get("Session"), Some("1"));
        let ports = link.session.receiver_ports().expect("ports");
        assert_ne!(ports.server_port, 0);
        assert_ne!(ports.control_port, 0);
    }
}

// covers: P35, P129
#[test]
fn p35_teardown_behind_an_unanswered_request_closes_after_2_s() {
    let mut link = playing();
    link.receiver.hold = Some("SET_PARAMETER".to_owned());
    link.advance(Duration::from_millis(200));
    link.session.set_volume(Volume::Db(-5.0), link.now);
    link.pump();
    link.session.teardown(link.now);
    link.pump();
    link.advance(Duration::from_millis(1_999));
    assert!(!link.closed);
    link.advance(Duration::from_millis(1));
    assert!(link.closed);
    assert_eq!(link.events.last(), Some(&SessionEvent::Closed));
    assert!(link.failure().is_none(), "{:?}", link.events);
}

// covers: P37
#[test]
fn p37_the_nonce_count_starts_over_with_a_new_nonce() {
    let mut gate = PasswordGate::new("hunter2");
    gate.qop = true;
    gate.stale_on = Some(3);
    let mut receiver = FakeReceiver::new();
    receiver.gate = Some(gate);
    let mut link = Link::new(password_config("hunter2"), receiver);
    link.connect();
    assert_eq!(link.session.state(), State::Playing, "{:?}", link.events);
    let params: Vec<_> = link.receiver.requests[1..]
        .iter()
        .map(|request| digest_params(request.headers.get("Authorization").expect("auth")))
        .collect();
    let counts: Vec<&str> = params.iter().map(|p| p["nc"].as_str()).collect();
    // OPTIONS, ANNOUNCE, the stale SETUP; then SETUP, RECORD and the volume
    // with the new nonce (RFC 2617 §3.2.2: nc counts per nonce).
    assert_eq!(
        counts,
        [
            "00000001", "00000002", "00000003", "00000001", "00000002", "00000003"
        ]
    );
    for p in &params {
        assert_eq!(p["qop"], "auth");
        assert!(!p["cnonce"].is_empty());
    }
}

/// A receiver that lets the first five requests through and challenges
/// from the sixth on.
fn late_gate_link() -> Link {
    let mut gate = PasswordGate::new("hunter2");
    gate.from_request = 5;
    let mut receiver = FakeReceiver::new();
    receiver.gate = Some(gate);
    let mut link = Link::new(password_config("hunter2"), receiver);
    link.connect();
    assert_eq!(link.keys(), START);
    assert_eq!(link.session.state(), State::Playing);
    link
}

// covers: P21, P36, P38
#[test]
fn p36_a_first_challenge_while_playing_is_answered_and_kept() {
    let mut link = late_gate_link();
    link.advance(Duration::from_millis(200));
    link.session.set_volume(Volume::Db(-9.0), link.now);
    link.pump();
    let requests = &link.receiver.requests;
    let [challenged, resent] = &requests[5..] else {
        panic!("{:?}", link.keys());
    };
    assert_eq!(challenged.body, resent.body);
    assert!(challenged.headers.get("Authorization").is_none());
    assert_eq!(challenged.headers.get("CSeq"), Some("5"));
    assert_eq!(resent.headers.get("CSeq"), Some("6"));
    assert!(resent.headers.get("Authorization").is_some());
    assert_eq!(link.receiver.authorized, 1);
    assert!(link.failure().is_none(), "{:?}", link.events);
    // The keep-alive carries Authorization too.
    link.advance(Duration::from_secs(30));
    let keepalive = link.last("OPTIONS");
    let params = digest_params(keepalive.headers.get("Authorization").expect("auth"));
    assert_eq!(params["uri"], "*");
    assert_eq!(link.receiver.authorized, 2);
    assert_eq!(link.session.state(), State::Playing);
}

// covers: P35, P36
#[test]
fn p35_a_challenged_teardown_still_closes_without_an_error() {
    let mut link = late_gate_link();
    link.session.teardown(link.now);
    link.pump();
    link.advance(Duration::from_secs(2));
    assert!(link.closed);
    assert_eq!(link.events.last(), Some(&SessionEvent::Closed));
    assert!(kinds(&link).is_empty(), "{:?}", link.events);
}

// covers: P132
#[test]
fn p132_an_answered_feedback_starts_the_miss_count_over() {
    let mut fake = FakeAp2::new();
    fake.status.insert("POST /feedback".to_owned(), 500);
    let mut rig = Ap2Rig::new(ap2_config(), fake);
    // Misses at 2 s and 4 s, an answer at 6 s.
    rig.advance(Duration::from_millis(4_500));
    rig.fake.status.remove("POST /feedback");
    rig.advance(Duration::from_millis(2_000));
    // Misses at 8 s and 10 s: two in a row, still playing.
    rig.fake.status.insert("POST /feedback".to_owned(), 500);
    rig.advance(Duration::from_millis(4_000));
    assert!(rig.failure().is_none(), "{:?}", rig.keys());
    assert_eq!(rig.times_of("POST /feedback").len(), 5);
    // The third in a row, at 12 s, loses it.
    rig.advance(Duration::from_millis(2_000));
    assert_eq!(rig.failure().map(|e| e.kind), Some(ErrorKind::Lost));
}

// covers: P93
#[test]
fn p93_the_sync_sequence_counts_on_across_a_flush() {
    let mut rig = Rig::with_host();
    let id = rig.add(speaker(1, Codec::Alac));
    rig.run_for(Duration::from_secs(2));
    rig.stream
        .set_delay(id, Duration::from_millis(200), rig.now);
    rig.run_for(Duration::from_secs(3));
    let syncs: Vec<SyncPacket> = rig
        .of(id)
        .iter()
        .filter(|sent| sent.transmit.kind == PacketKind::Sync)
        .map(|sent| SyncPacket::parse(&sent.transmit.bytes).expect("sync"))
        .collect();
    let seqs: Vec<u16> = syncs.iter().map(|sync| sync.seq).collect();
    let expected: Vec<u16> = (0..).take(seqs.len()).collect();
    assert_eq!(seqs, expected);
    let firsts = syncs.iter().filter(|sync| sync.first).count();
    assert!(syncs[0].first);
    assert_eq!(firsts, 2, "after RECORD and after the flush");
}

// covers: P41
#[test]
fn p41_info_values_of_an_unexpected_type_are_ignored() {
    let answer = read_info(&plist(dict(&[
        ("features", text("0x40000")),
        ("statusFlags", Value::Real(4.0)),
        ("name", int(7)),
        ("model", Value::Boolean(true)),
        ("sourceVersion", Value::Array(Vec::new())),
        ("initialVolume", text("-20")),
    ])));
    assert_eq!(answer, InfoAnswer::default());

    // A value of the wrong type is absent, so the TXT record fills it.
    let mut wire = Vec::new();
    for string in ["features=0x0,0x10040", "model=AudioAccessory5,1"] {
        wire.push(u8::try_from(string.len()).expect("short"));
        wire.extend_from_slice(string.as_bytes());
    }
    let answer = read_info(&plist(dict(&[
        ("features", text("not a number")),
        ("model", int(1)),
        ("initialVolume", int(-30)),
        ("txtAirPlay", Value::Data(wire)),
    ])));
    assert!(answer.features.is_some_and(|features| features.has(38)));
    assert_eq!(answer.model.as_deref(), Some("AudioAccessory5,1"));
    assert_eq!(answer.initial_volume, Some(-30.0));

    // A TXT entry that isn't data is ignored too.
    let answer = read_info(&plist(dict(&[("txtAirPlay", text("features=0x1"))])));
    assert_eq!(answer, InfoAnswer::default());
}

// covers: P33, P125, P126
#[test]
fn p125_artwork_of_exactly_512_kib_goes_and_progress_names_its_rtptime() {
    let mut config = config();
    config.metadata = MetadataSupport {
        text: false,
        artwork: true,
        progress: true,
    };
    let mut link = Link::new(config, FakeReceiver::new());
    link.connect();
    let mut jpeg = vec![0u8; MAX_ARTWORK_BYTES];
    jpeg[..3].copy_from_slice(&[0xFF, 0xD8, 0xFF]);
    link.session
        .send_metadata(
            Metadata::Artwork {
                jpeg: jpeg.clone(),
                rtptime: 9,
            },
            link.now,
        )
        .expect("512 KiB is allowed");
    link.pump();
    assert_eq!(link.last("SET_PARAMETER").body, jpeg);
    link.session
        .send_metadata(
            Metadata::Progress {
                start: 100,
                current: 200,
                end: 300,
            },
            link.now,
        )
        .expect("listed");
    link.pump();
    let progress = link.last("SET_PARAMETER");
    assert_eq!(progress.headers.get("RTP-Info"), Some("rtptime=200"));
    assert_eq!(progress.body, b"progress: 100/200/300\r\n");
}

// covers: P26, P127
#[test]
fn p127_metadata_queued_before_record_follows_the_initial_volume() {
    let mut config = config();
    config.metadata.text = true;
    let mut receiver = FakeReceiver::new();
    receiver.hold = Some("SETUP".to_owned());
    let mut link = Link::new(config, receiver);
    link.connect();
    let track = Metadata::Track {
        info: TrackInfo {
            title: "Song".to_owned(),
            artist: "Band".to_owned(),
            album: "Record".to_owned(),
        },
        rtptime: 5,
    };
    link.session
        .send_metadata(track.clone(), link.now)
        .expect("listed");
    link.pump();
    assert_eq!(link.keys(), ["OPTIONS", "ANNOUNCE", "SETUP"]);
    link.receiver.hold = None;
    link.release();
    let keys = link.keys();
    assert_eq!(keys[..5], START);
    assert_eq!(keys[5..], ["SET_PARAMETER"]);
    let bodies: Vec<&[u8]> = link.receiver.requests[4..]
        .iter()
        .map(|request| request.body.as_slice())
        .collect();
    assert!(bodies[0].starts_with(b"volume: "));
    assert_eq!(bodies[1], track.body());
}
