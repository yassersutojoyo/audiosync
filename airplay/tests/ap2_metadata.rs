//! Now-playing metadata and the volume read-back on the AirPlay 2 path
//! (P122, P123, P128, P180): the session against the sans-I/O fake receiver,
//! then the sender's driver over loopback sockets.

mod common;

use std::net::{IpAddr, Ipv4Addr};
use std::time::Duration;

use audiosync_airplay::discovery::{
    Device, DeviceId, DeviceKey, ScopedAddr, ServiceKind, ServiceRecord,
};
use audiosync_airplay::metadata::{Metadata, MetadataError, TrackInfo};
use audiosync_airplay::rtsp::{Method, Request, Response};
use audiosync_airplay::sender::{
    NowPlaying, Progress, Sender, SenderConfig, SenderEvent, SpeakerEvent, SpeakerOptions,
    SpeakerState,
};
use audiosync_airplay::volume::Volume;
use plist::Value;

use common::ap2::{Ap2Rig, Ap2Server, FakeAp2, ap2_config, dict, int, plist};
use common::net::{Events, HostAudio, serial, wait_until};

const LONG: Duration = Duration::from_secs(15);

/// Features bits 34 and 48, which the fake receiver always sets.
const BASE: i64 = 0x4_0000_0000 | 0x1_0000_0000_0000;
const TEXT: i64 = 1 << 17;
const ARTWORK: i64 = 1 << 15;
const PROGRESS: i64 = 1 << 16;
const BINARY_PLIST: i64 = 1 << 50;

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
    let cseq = rig
        .fake
        .requests
        .last()
        .and_then(|(request, _)| request.headers.get("CSeq"))
        .expect("a request with a CSeq")
        .to_owned();
    let mut response = Response::new(200, "OK");
    response.headers.push("CSeq", cseq);
    let framing = rig.fake.framing.as_mut().expect("framing has started");
    let bytes = framing.seal(&response.encode());
    let now = rig.now;
    rig.session.handle_input(&bytes, now);
    rig.pump();
}

// covers: P123
#[test]
fn ap2_metadata_waits_for_the_stream_setup_and_goes_framed() {
    // The initial volume goes unanswered, so the stream SETUP isn't out yet.
    let mut fake = fake_with(TEXT | ARTWORK | PROGRESS);
    fake.silent.push("SET_PARAMETER".to_owned());
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
    rig.session.set_volume(Volume::Db(-5.0), now);
    rig.pump();
    assert!(rig.failure().is_none());
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
            artwork: Some(vec![0xFF, 0xD8, 0xFF, 0xE0, 0, 0, 0xFF, 0xD9]),
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

// covers: P122
#[test]
fn an_airplay2_read_back_reports_the_last_volume_set_without_asking() {
    let _serial = serial();
    let server = Ap2Server::start(FakeAp2::new());
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
    sender.read_volume(&key);
    assert!(wait_until(LONG, || {
        events.drain(&sender);
        reported(&events, &key) == vec![-20.0]
    }));
    sender.set_volume(&key, Volume::Db(-7.5));
    sender.read_volume(&key);
    sender.set_volume(&key, Volume::Muted);
    sender.read_volume(&key);
    assert!(
        wait_until(LONG, || {
            events.drain(&sender);
            reported(&events, &key) == vec![-20.0, -7.5, -144.0]
        }),
        "{:?}",
        reported(&events, &key)
    );
    assert!(!server.fake().keys().contains(&"GET_PARAMETER".to_owned()));
    assert!(!events.seen.iter().any(|(_, event)| matches!(
        event,
        SenderEvent::Speaker {
            event: SpeakerEvent::Error(_),
            ..
        }
    )));
    sender.shutdown();
}
