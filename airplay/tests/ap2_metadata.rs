//! Now-playing metadata and the volume read-back on the AirPlay 2 path
//! (P122, P123, P128, P180): the session against the sans-I/O fake receiver,
//! then the sender's driver over loopback sockets.

mod common;

use std::net::{IpAddr, Ipv4Addr};
use std::time::Duration;

use audiosync_airplay::ap2::{Ap2Event, PtpPeer};
use audiosync_airplay::discovery::{
    Device, DeviceId, DeviceKey, ScopedAddr, ServiceKind, ServiceRecord,
};
use audiosync_airplay::error::Warning;
use audiosync_airplay::metadata::{Artwork, Metadata, MetadataError, TrackInfo};
use audiosync_airplay::rtsp::{Method, Request, Response};
use audiosync_airplay::sender::{
    NowPlaying, Progress, Sender, SenderConfig, SenderEvent, SpeakerEvent, SpeakerOptions,
    SpeakerState,
};
use audiosync_airplay::session::RESPONSE_TIMEOUT;
use audiosync_airplay::volume::Volume;
use plist::Value;

use common::ap2::{Ap2Rig, Ap2Server, FakeAp2, ap2_config, dict, int, plist};
use common::net::{Behavior, Events, HostAudio, NetReceiver, serial, wait_until};

const LONG: Duration = Duration::from_secs(15);

/// Features bits 34 and 48, which the fake receiver always sets.
const BASE: i64 = 0x4_0000_0000 | 0x1_0000_0000_0000;
const TEXT: i64 = 1 << 17;
const ARTWORK: i64 = 1 << 15;
const PROGRESS: i64 = 1 << 16;
const BINARY_PLIST: i64 = 1 << 50;
const PTP: i64 = 1 << 41;

fn fake_with(features: i64) -> FakeAp2 {
    let mut fake = FakeAp2::new();
    fake.info_body = plist(dict(&[
        ("features", int(BASE | features)),
        ("statusFlags", int(0x4)),
        ("name", Value::String("Fake".to_owned())),
        ("model", Value::String("Speaker1,1".to_owned())),
    ]));
    fake
}

fn track() -> TrackInfo {
    TrackInfo {
        title: "Title".to_owned(),
        artist: "Artist".to_owned(),
        album: "Album".to_owned(),
    }
}

fn all_three() -> [Metadata; 3] {
    [
        Metadata::Track {
            info: track(),
            rtptime: 1000,
        },
        Metadata::Artwork {
            jpeg: vec![0xFF, 0xD8, 0xFF, 0xE0, 0, 0, 0xFF, 0xD9],
            rtptime: 1000,
        },
        Metadata::Progress {
            start: 500,
            current: 1000,
            end: 9000,
        },
    ]
}

fn is_metadata(request: &Request) -> bool {
    request.method == Method::SetParameter
        && request.headers.get("Content-Type").is_some_and(|value| {
            value == "application/x-dmap-tagged"
                || value == "image/jpeg"
                || (value == "text/parameters"
                    && String::from_utf8_lossy(&request.body).starts_with("progress:"))
        })
}

fn metadata_sent(fake: &FakeAp2) -> Vec<(Request, bool)> {
    fake.requests
        .iter()
        .filter(|(request, _)| is_metadata(request))
        .cloned()
        .collect()
}

/// Answers the last request the fake read and left unanswered, with 200.
fn answer_last(rig: &mut Ap2Rig) {
    answer_last_with(rig, 200, "OK");
}

fn answer_last_with(rig: &mut Ap2Rig, code: u16, reason: &str) {
    let cseq = rig
        .fake
        .requests
        .last()
        .and_then(|(request, _)| request.headers.get("CSeq"))
        .expect("a request with a CSeq")
        .to_owned();
    answer_cseq(rig, &cseq, code, reason);
}

fn answer_cseq(rig: &mut Ap2Rig, cseq: &str, code: u16, reason: &str) {
    let mut response = Response::new(code, reason);
    response.headers.push("CSeq", cseq.to_owned());
    let framing = rig.fake.framing.as_mut().expect("framing has started");
    let bytes = framing.seal(&response.encode());
    let now = rig.now;
    rig.session.handle_input(&bytes, now);
    rig.pump();
}

// covers: P123
#[test]
fn ap2_metadata_waits_for_the_stream_setup_and_goes_framed() {
    // The FLUSH goes unanswered, so the stream isn't ready yet.
    let mut fake = fake_with(TEXT | ARTWORK | PROGRESS);
    fake.silent.push("FLUSH".to_owned());
    let mut rig = Ap2Rig::new(ap2_config(), fake);
    assert!(!rig.ready());
    let now = rig.now;
    for metadata in all_three() {
        assert_eq!(rig.session.send_metadata(metadata, now), Ok(()));
    }
    rig.pump();
    assert!(metadata_sent(&rig.fake).is_empty(), "{:?}", rig.keys());

    rig.fake.silent.clear();
    answer_last(&mut rig);
    assert!(rig.ready(), "{:?}", rig.keys());
    let keys = rig.keys();
    let stream_setup = keys.iter().position(|key| *key == "SETUP stream");
    let flush = keys.iter().position(|key| *key == "FLUSH");
    assert!(stream_setup.is_some() && stream_setup < flush, "{keys:?}");

    let sent = metadata_sent(&rig.fake);
    assert_eq!(sent.len(), 3, "{keys:?}");
    let first = rig
        .fake
        .requests
        .iter()
        .position(|(request, _)| is_metadata(request));
    let flush_at = rig
        .fake
        .requests
        .iter()
        .position(|(request, _)| request.method == Method::Flush);
    assert!(flush_at < first);
    // The initial volume goes ahead of the metadata (P183).
    assert_eq!(
        set_parameters_after_flush(&rig.fake),
        ["volume", "track", "artwork", "progress"]
    );
    for ((request, framed), metadata) in sent.iter().zip(all_three()) {
        assert!(framed, "metadata goes framed (P67)");
        assert_eq!(
            request.headers.get("Content-Type"),
            Some(metadata.content_type())
        );
        assert_eq!(
            request.headers.get("RTP-Info"),
            Some(format!("rtptime={}", metadata.rtptime()).as_str())
        );
        assert_eq!(request.body, metadata.body());
        assert!(request.headers.get("Session").is_none());
    }
    assert!(rig.failure().is_none());
}

// covers: P123, P128
#[test]
fn ap2_metadata_follows_features_bits_17_15_16_and_bit_50_changes_nothing() {
    let kinds = |features: i64| {
        let mut rig = Ap2Rig::new(ap2_config(), fake_with(features));
        assert!(rig.ready());
        let now = rig.now;
        let results: Vec<Result<(), MetadataError>> = all_three()
            .into_iter()
            .map(|metadata| rig.session.send_metadata(metadata, now))
            .collect();
        rig.pump();
        let types: Vec<String> = metadata_sent(&rig.fake)
            .iter()
            .filter_map(|(request, _)| request.headers.get("Content-Type").map(str::to_owned))
            .collect();
        (results, types)
    };
    let refused = Err(MetadataError::NotListed);

    // Bit 50 alone: nothing (P128).
    let (results, types) = kinds(BINARY_PLIST);
    assert_eq!(results, vec![refused, refused, refused]);
    assert!(types.is_empty());

    // Bit 50 beside bit 17: the text goes, as without bit 50.
    let (results, types) = kinds(BINARY_PLIST | TEXT);
    assert_eq!(results, vec![Ok(()), refused, refused]);
    assert_eq!(types, vec!["application/x-dmap-tagged"]);

    let (results, types) = kinds(BINARY_PLIST | ARTWORK | PROGRESS);
    assert_eq!(results, vec![refused, Ok(()), Ok(())]);
    assert_eq!(types, vec!["image/jpeg", "text/parameters"]);

    let (results, types) = kinds(TEXT | ARTWORK | PROGRESS);
    assert_eq!(results, vec![Ok(()), Ok(()), Ok(())]);
    assert_eq!(types.len(), 3);
}

// covers: P127
#[test]
fn a_failed_ap2_metadata_request_is_a_warning() {
    let mut fake = fake_with(TEXT);
    fake.status.insert("SET_PARAMETER".to_owned(), 500);
    let mut rig = Ap2Rig::new(ap2_config(), fake);
    assert!(rig.ready());
    let now = rig.now;
    let [track, ..] = all_three();
    assert_eq!(rig.session.send_metadata(track, now), Ok(()));
    rig.pump();
    assert_eq!(metadata_sent(&rig.fake).len(), 1);
    assert!(rig.failure().is_none());
    assert!(
        rig.events
            .contains(&Ap2Event::Warning(Warning::RequestFailed {
                method: Method::SetParameter,
                status: 500,
            })),
        "{:?}",
        rig.events
    );
    rig.session.set_volume(Volume::Db(-5.0), now);
    rig.pump();
    assert!(rig.failure().is_none());
}

/// The SET_PARAMETER requests after the FLUSH, by what they carry.
fn set_parameters_after_flush(fake: &FakeAp2) -> Vec<&'static str> {
    let flush = fake
        .requests
        .iter()
        .position(|(request, _)| request.method == Method::Flush)
        .expect("a FLUSH");
    fake.requests[flush..]
        .iter()
        .filter(|(request, _)| request.method == Method::SetParameter)
        .map(|(request, _)| {
            let body = String::from_utf8_lossy(&request.body);
            match request.headers.get("Content-Type") {
                Some("application/x-dmap-tagged") => "track",
                Some("image/jpeg") => "artwork",
                _ if body.starts_with("progress:") => "progress",
                _ if body.starts_with("volume:") => "volume",
                _ => "other",
            }
        })
        .collect()
}

// covers: P123, P127
#[test]
fn a_new_ap2_volume_goes_before_queued_metadata() {
    let mut rig = Ap2Rig::new(ap2_config(), fake_with(TEXT | PROGRESS));
    assert!(rig.ready());
    let now = rig.now;
    let [track, _, progress] = all_three();
    // The track goes out at once, so the progress waits behind it.
    assert_eq!(rig.session.send_metadata(track, now), Ok(()));
    assert_eq!(rig.session.send_metadata(progress, now), Ok(()));
    rig.session.set_volume(Volume::Db(-5.0), now);
    rig.pump();
    assert_eq!(
        set_parameters_after_flush(&rig.fake),
        ["volume", "track", "volume", "progress"],
        "{:?}",
        rig.keys()
    );
    assert!(rig.failure().is_none());
}

// covers: P123
#[test]
fn ap2_metadata_queued_before_ready_is_dropped_by_teardown() {
    let mut fake = fake_with(TEXT | ARTWORK | PROGRESS);
    fake.silent.push("FLUSH".to_owned());
    let mut rig = Ap2Rig::new(ap2_config(), fake);
    assert!(!rig.ready());
    let now = rig.now;
    for metadata in all_three() {
        assert_eq!(rig.session.send_metadata(metadata, now), Ok(()));
    }
    rig.pump();
    rig.session.teardown(now);
    rig.pump();

    // The FLUSH's late answer lets the TEARDOWN go out.
    rig.fake.silent.clear();
    answer_last(&mut rig);
    rig.advance(Duration::from_secs(10));
    let keys = rig.keys();
    assert!(metadata_sent(&rig.fake).is_empty(), "{keys:?}");
    assert!(keys.contains(&"TEARDOWN"), "{keys:?}");
    let [track, ..] = all_three();
    assert_eq!(
        rig.session.send_metadata(track, rig.now),
        Err(MetadataError::SessionEnded)
    );
}

// covers: P123, P157
#[test]
fn ap2_metadata_queued_before_a_ptp_reconnect_is_dropped() {
    let mut fake = fake_with(TEXT | PTP);
    fake.silent.push("SETUP session".to_owned());
    let mut config = ap2_config();
    config.ptp = Some(PtpPeer {
        clock_id: 0xFFFF_0000_0000_0001,
        id: "6f1e7c9a-3b2d-4c5e-8f90-a1b2c3d4e5f6".to_owned(),
    });
    let mut rig = Ap2Rig::new(config, fake);
    assert!(!rig.ready());
    let now = rig.now;
    let [track, ..] = all_three();
    assert_eq!(rig.session.send_metadata(track, now), Ok(()));
    rig.pump();

    // A refused NTP session SETUP: the session reconnects for PTP.
    rig.fake.silent.clear();
    answer_last_with(&mut rig, 501, "Not Implemented");
    assert!(rig.ready(), "{:?} {:?}", rig.keys(), rig.events);
    assert_eq!(rig.connects.len(), 2);
    assert!(metadata_sent(&rig.fake).is_empty(), "{:?}", rig.keys());
}

fn airplay_record(port: u16, id: u8, features: &str) -> ServiceRecord {
    let device_id = DeviceId([0x02, 0x00, 0x5E, 0x00, 0x53, id]);
    ServiceRecord {
        kind: ServiceKind::AirPlay,
        instance: format!("Speaker {id}"),
        host: format!("speaker-{id}.local"),
        port,
        txt: [
            format!("features={features}"),
            "flags=0x4".to_owned(),
            "model=Speaker1,1".to_owned(),
            format!("deviceid={device_id}"),
        ]
        .into_iter()
        .map(String::into_bytes)
        .collect(),
        addresses: vec![ScopedAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST))],
    }
}

/// An AirPlay 2 device without a RAOP record, so without `md`.
fn airplay2_only(server: &Ap2Server, id: u8, features: &str) -> Device {
    let device_id = DeviceId([0x02, 0x00, 0x5E, 0x00, 0x53, id]);
    let record = airplay_record(server.address.port(), id, features);
    Device::from_records(DeviceKey::Id(device_id), None, Some(&record)).expect("device")
}

fn start() -> (Sender, HostAudio) {
    let config = SenderConfig {
        airplay2: true,
        airplay2_device_id: Some([0x10, 0x20, 0x30, 0x40, 0x50, 0x60]),
        ..SenderConfig::default()
    };
    let (sender, writer) = Sender::start(config).expect("the sender starts");
    (sender, HostAudio::start(writer))
}

fn reported(events: &Events, key: &DeviceKey) -> Vec<f64> {
    events
        .of(key)
        .into_iter()
        .filter_map(|event| match event {
            SpeakerEvent::VolumeReported(db) => Some(db),
            _ => None,
        })
        .collect()
}

// covers: P123
#[test]
fn now_playing_reaches_an_airplay2_speaker_by_its_features() {
    let _serial = serial();
    let server = Ap2Server::start(fake_with(TEXT | ARTWORK | PROGRESS));
    let (sender, _host) = start();
    let mut events = Events::default();
    let device = airplay2_only(&server, 61, "0x38000,0x10040");
    let key = device.key.clone();
    sender
        .set_now_playing(NowPlaying {
            track: Some(track()),
            artwork: Some(Artwork::single(vec![
                0xFF, 0xD8, 0xFF, 0xE0, 0, 0, 0xFF, 0xD9,
            ])),
            progress: Some(Progress {
                elapsed: Duration::from_secs(10),
                length: Duration::from_secs(200),
            }),
        })
        .expect("valid metadata");
    sender.add_speaker(device, SpeakerOptions::new(Volume::Db(-20.0)));
    assert!(
        events
            .wait_state(&sender, &key, 1, LONG, |s| *s == SpeakerState::Playing)
            .is_some(),
        "{:?}",
        events.of(&key)
    );
    assert!(
        wait_until(LONG, || metadata_sent(&server.fake()).len() == 3),
        "{:?}",
        server.fake().keys()
    );
    let fake = server.fake();
    let keys = fake.keys();
    let stream_setup = keys.iter().position(|key| key == "SETUP stream");
    let first = fake
        .requests
        .iter()
        .position(|(request, _)| is_metadata(request));
    assert!(stream_setup.is_some() && stream_setup < first, "{keys:?}");
    for (request, framed) in metadata_sent(&fake) {
        assert!(framed);
        assert!(
            request
                .headers
                .get("RTP-Info")
                .is_some_and(|value| value.starts_with("rtptime="))
        );
    }
    drop(fake);

    // A new now-playing goes to the playing speaker too.
    sender
        .set_now_playing(NowPlaying {
            track: Some(track()),
            ..NowPlaying::default()
        })
        .expect("valid metadata");
    assert!(wait_until(LONG, || metadata_sent(&server.fake()).len() == 4));
    sender.shutdown();
}

// covers: P123, P128
#[test]
fn an_airplay2_speaker_with_bit_50_alone_gets_no_metadata() {
    let _serial = serial();
    let server = Ap2Server::start(fake_with(BINARY_PLIST));
    let (sender, _host) = start();
    let mut events = Events::default();
    let device = airplay2_only(&server, 62, "0x0,0x50040");
    let key = device.key.clone();
    sender
        .set_now_playing(NowPlaying {
            track: Some(track()),
            ..NowPlaying::default()
        })
        .expect("valid metadata");
    sender.add_speaker(device, SpeakerOptions::new(Volume::Db(-20.0)));
    assert!(
        events
            .wait_state(&sender, &key, 1, LONG, |s| *s == SpeakerState::Playing)
            .is_some()
    );
    assert!(wait_until(LONG, || server
        .fake()
        .keys()
        .contains(&"FLUSH".to_owned())));
    assert!(!wait_until(Duration::from_secs(1), || !metadata_sent(
        &server.fake()
    )
    .is_empty()));
    sender.shutdown();
}

// covers: P122, P184
#[test]
fn an_airplay2_read_back_asks_the_receiver_with_the_next_feedback() {
    let _serial = serial();
    let mut fake = FakeAp2::new();
    fake.bodies.insert(
        "GET_PARAMETER".to_owned(),
        b"volume: -27.000000\r\n".to_vec(),
    );
    let server = Ap2Server::start(fake);
    let (sender, _host) = start();
    let mut events = Events::default();
    let device = airplay2_only(&server, 63, "0x0,0x10040");
    let key = device.key.clone();
    sender.add_speaker(device, SpeakerOptions::new(Volume::Db(-20.0)));
    assert!(
        events
            .wait_state(&sender, &key, 1, LONG, |s| *s == SpeakerState::Playing)
            .is_some()
    );
    assert!(!server.fake().keys().contains(&"GET_PARAMETER".to_owned()));
    sender.read_volume(&key);
    assert!(wait_until(LONG, || {
        events.drain(&sender);
        reported(&events, &key) == vec![-27.0]
    }));
    server.fake().bodies.insert(
        "GET_PARAMETER".to_owned(),
        b"volume: -26.700001\r\n".to_vec(),
    );
    sender.read_volume(&key);
    assert!(
        wait_until(LONG, || {
            events.drain(&sender);
            reported(&events, &key) == vec![-27.0, -26.700001]
        }),
        "{:?}",
        reported(&events, &key)
    );
    assert!(!events.seen.iter().any(|(_, event)| matches!(
        event,
        SenderEvent::Speaker {
            event: SpeakerEvent::Error(_),
            ..
        }
    )));
    sender.shutdown();
}

fn jpeg(fill: u8, len: usize) -> Vec<u8> {
    let mut jpeg = vec![fill; len];
    jpeg[..3].copy_from_slice(&[0xFF, 0xD8, 0xFF]);
    jpeg
}

/// A picture too large for the small limits: two different encodings.
fn two_sizes() -> Artwork {
    Artwork {
        large: jpeg(0x11, 300),
        small: jpeg(0x22, 100),
    }
}

/// The artwork requests the fake read, in order.
fn images_sent(fake: &FakeAp2) -> Vec<(Request, bool)> {
    metadata_sent(fake)
        .into_iter()
        .filter(|(request, _)| request.headers.get("Content-Type") == Some("image/jpeg"))
        .collect()
}

fn image_bodies(fake: &FakeAp2) -> Vec<Vec<u8>> {
    images_sent(fake)
        .into_iter()
        .map(|(request, _)| request.body)
        .collect()
}

fn artwork_warnings(events: &[Ap2Event]) -> Vec<Warning> {
    events
        .iter()
        .filter_map(|event| match event {
            Ap2Event::Warning(warning @ Warning::ArtworkRefused { .. }) => Some(warning.clone()),
            _ => None,
        })
        .collect()
}

fn artwork_rig() -> Ap2Rig {
    let rig = Ap2Rig::new(ap2_config(), fake_with(TEXT | ARTWORK | PROGRESS));
    assert!(rig.ready(), "{:?}", rig.keys());
    rig
}

// covers: P185
#[test]
fn an_airplay2_session_sends_the_large_image_while_it_is_taken() {
    let mut rig = artwork_rig();
    let now = rig.now;
    assert_eq!(rig.session.send_artwork(two_sizes(), 1000, now), Ok(()));
    rig.pump();
    assert_eq!(rig.session.send_artwork(two_sizes(), 2000, now), Ok(()));
    rig.pump();
    assert_eq!(
        image_bodies(&rig.fake),
        [two_sizes().large, two_sizes().large]
    );
    let times: Vec<Option<String>> = images_sent(&rig.fake)
        .iter()
        .map(|(request, _)| request.headers.get("RTP-Info").map(str::to_owned))
        .collect();
    assert_eq!(
        times,
        [
            Some("rtptime=1000".to_owned()),
            Some("rtptime=2000".to_owned())
        ]
    );
    assert!(artwork_warnings(&rig.events).is_empty());
}

// covers: P185, P127, P138, P141
#[test]
fn a_refused_large_image_goes_again_small_and_the_session_keeps_the_small_one() {
    let mut rig = artwork_rig();
    rig.fake.status_once.insert("SET_PARAMETER".to_owned(), 413);
    let now = rig.now;
    assert_eq!(rig.session.send_artwork(two_sizes(), 1000, now), Ok(()));
    rig.pump();
    assert_eq!(
        image_bodies(&rig.fake),
        [two_sizes().large, two_sizes().small]
    );
    for (request, _) in images_sent(&rig.fake) {
        assert_eq!(request.headers.get("RTP-Info"), Some("rtptime=1000"));
    }
    assert_eq!(
        artwork_warnings(&rig.events),
        [Warning::ArtworkRefused { status: Some(413) }]
    );

    // Later tracks on this session send only the small image.
    let [track, ..] = all_three();
    assert_eq!(rig.session.send_metadata(track, now), Ok(()));
    assert_eq!(rig.session.send_artwork(two_sizes(), 2000, now), Ok(()));
    rig.pump();
    assert_eq!(
        image_bodies(&rig.fake),
        [two_sizes().large, two_sizes().small, two_sizes().small]
    );
    assert_eq!(artwork_warnings(&rig.events).len(), 1);
    assert!(rig.failure().is_none(), "{:?}", rig.events);
    assert!(rig.ready());
}

// covers: P185, P127
#[test]
fn a_refused_small_image_ends_artwork_while_text_and_progress_still_go() {
    let mut rig = artwork_rig();
    rig.fake.status.insert("SET_PARAMETER".to_owned(), 500);
    let now = rig.now;
    assert_eq!(rig.session.send_artwork(two_sizes(), 1000, now), Ok(()));
    rig.pump();
    assert_eq!(
        image_bodies(&rig.fake),
        [two_sizes().large, two_sizes().small]
    );
    rig.fake.status.clear();
    assert_eq!(rig.session.send_artwork(two_sizes(), 2000, now), Ok(()));
    for metadata in all_three() {
        assert_eq!(rig.session.send_metadata(metadata, now), Ok(()));
    }
    rig.pump();
    assert_eq!(image_bodies(&rig.fake).len(), 2, "{:?}", rig.keys());
    let types: Vec<String> = metadata_sent(&rig.fake)
        .iter()
        .skip(2)
        .filter_map(|(request, _)| request.headers.get("Content-Type").map(str::to_owned))
        .collect();
    assert_eq!(types, ["application/x-dmap-tagged", "text/parameters"]);
    assert_eq!(
        artwork_warnings(&rig.events),
        [Warning::ArtworkRefused { status: Some(500) }]
    );
    assert!(rig.failure().is_none(), "{:?}", rig.events);
    assert!(rig.ready());

    // A new session starts again from its path's image.
    let mut rig = artwork_rig();
    let now = rig.now;
    assert_eq!(rig.session.send_artwork(two_sizes(), 3000, now), Ok(()));
    rig.pump();
    assert_eq!(image_bodies(&rig.fake), [two_sizes().large]);
}

// covers: P185
#[test]
fn one_picture_as_the_same_bytes_for_both_goes_once_per_session() {
    let mut rig = artwork_rig();
    rig.fake.status_once.insert("SET_PARAMETER".to_owned(), 413);
    let now = rig.now;
    let one = Artwork::single(jpeg(0x33, 80));
    assert_eq!(rig.session.send_artwork(one.clone(), 1000, now), Ok(()));
    rig.pump();
    assert_eq!(rig.session.send_artwork(one.clone(), 2000, now), Ok(()));
    rig.pump();
    assert_eq!(image_bodies(&rig.fake), [one.large]);
    assert_eq!(artwork_warnings(&rig.events).len(), 1);
    assert!(rig.failure().is_none(), "{:?}", rig.events);
}

// covers: P185, P134
#[test]
fn an_unanswered_image_counts_as_refused_and_never_ends_the_session() {
    let mut rig = artwork_rig();
    rig.fake.silent.push("SET_PARAMETER".to_owned());
    rig.fake.silent.push("POST /feedback".to_owned());
    let now = rig.now;
    assert_eq!(rig.session.send_artwork(two_sizes(), 1000, now), Ok(()));
    rig.pump();
    let large_cseq = images_sent(&rig.fake)[0]
        .0
        .headers
        .get("CSeq")
        .expect("a CSeq")
        .to_owned();
    // Given up on, it lets the feedback that waited behind it go first.
    rig.advance(RESPONSE_TIMEOUT + Duration::from_millis(200));
    assert_eq!(
        artwork_warnings(&rig.events),
        [Warning::ArtworkRefused { status: None }]
    );
    assert_eq!(rig.keys().last(), Some(&"POST /feedback"));
    // A receiver that wakes up answers in order: the late answer is dropped.
    answer_cseq(&mut rig, &large_cseq, 200, "OK");
    assert!(rig.failure().is_none(), "{:?}", rig.events);
    answer_last(&mut rig);
    assert_eq!(
        image_bodies(&rig.fake),
        [two_sizes().large, two_sizes().small]
    );

    rig.advance(RESPONSE_TIMEOUT + Duration::from_millis(200));
    rig.fake.silent.clear();
    assert_eq!(rig.keys().last(), Some(&"POST /feedback"));
    answer_last(&mut rig);
    let [track, ..] = all_three();
    let now = rig.now;
    assert_eq!(rig.session.send_artwork(two_sizes(), 2000, now), Ok(()));
    assert_eq!(rig.session.send_metadata(track, now), Ok(()));
    rig.pump();
    assert_eq!(image_bodies(&rig.fake).len(), 2, "{:?}", rig.keys());
    assert_eq!(
        metadata_sent(&rig.fake)
            .last()
            .and_then(|(request, _)| request.headers.get("Content-Type").map(str::to_owned)),
        Some("application/x-dmap-tagged".to_owned())
    );
    assert_eq!(artwork_warnings(&rig.events).len(), 1);
    assert!(rig.failure().is_none(), "{:?}", rig.events);
    assert!(rig.ready());
}

/// An AirPlay 1 receiver that takes PCM and lists every metadata type.
const AP1_TXT: &[&str] = &[
    "txtvers=1",
    "ch=2",
    "cn=0",
    "et=0",
    "sr=44100",
    "ss=16",
    "tp=UDP",
    "md=0,1,2",
];

// covers: P185, P125
#[test]
fn each_speaker_gets_its_own_paths_image() {
    let _serial = serial();
    let server = Ap2Server::start(fake_with(TEXT | ARTWORK | PROGRESS));
    let receiver = NetReceiver::start(Behavior::default());
    let (sender, _host) = start();
    let mut events = Events::default();
    let airplay2 = airplay2_only(&server, 63, "0x38000,0x10040");
    let airplay1 = receiver.device(64, AP1_TXT);
    let keys = [airplay2.key.clone(), airplay1.key.clone()];
    sender
        .set_now_playing(NowPlaying {
            artwork: Some(two_sizes()),
            ..NowPlaying::default()
        })
        .expect("valid artwork");
    sender.add_speaker(airplay2, SpeakerOptions::new(Volume::Db(-20.0)));
    sender.add_speaker(airplay1, SpeakerOptions::new(Volume::Db(-20.0)));
    for key in &keys {
        assert!(
            events
                .wait_state(&sender, key, 1, LONG, |s| *s == SpeakerState::Playing)
                .is_some(),
            "{:?}",
            events.of(key)
        );
    }
    assert!(
        wait_until(LONG, || images_sent(&server.fake()).len() == 1),
        "{:?}",
        server.fake().keys()
    );
    let ap1_images = || {
        receiver
            .log()
            .requests
            .iter()
            .filter(|(_, request)| request.headers.get("Content-Type") == Some("image/jpeg"))
            .map(|(_, request)| request.body.clone())
            .collect::<Vec<_>>()
    };
    assert!(
        wait_until(LONG, || ap1_images().len() == 1),
        "{:?}",
        receiver.log().keys()
    );
    assert_eq!(image_bodies(&server.fake()), [two_sizes().large]);
    assert_eq!(ap1_images(), [two_sizes().small]);
    sender.shutdown();
}
