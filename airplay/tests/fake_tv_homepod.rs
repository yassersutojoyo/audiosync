//! `fake_tv --homepod --dump`: a sender session against the HomePod profile
//! leaves a numbered file per request, the stream SETUP's latency keys
//! among them, and a request method the crate's parser does not know is
//! answered instead of ending the fake.

mod common;
#[path = "../examples/fake_tv/dump.rs"]
mod dump;
#[path = "../examples/fake_tv/television.rs"]
mod television;

use std::fs;
use std::net::{IpAddr, Ipv4Addr};
use std::path::Path;
use std::time::Duration;

use audiosync_airplay::discovery::{
    Device, DeviceId, DeviceKey, ScopedAddr, ServiceKind, ServiceRecord,
};
use audiosync_airplay::sender::{Sender, SenderConfig, SpeakerOptions, SpeakerState};
use audiosync_airplay::volume::Volume;

use common::ap2::{Ap2Server, FakeAp2, read_dict};
use common::net::{Events, HostAudio, serial};
use dump::Dump;
use television::{HOMEPOD_MODEL, Pin};

const LONG: Duration = Duration::from_secs(15);

fn device(server: &Ap2Server) -> Device {
    let record = ServiceRecord {
        kind: ServiceKind::AirPlay,
        instance: television::HOMEPOD_NAME.to_owned(),
        host: format!("{}.local", television::HOST_LABEL),
        port: server.address.port(),
        txt: television::homepod_txt(Pin::None, television::DEVICE_ID)
            .into_iter()
            .map(|(key, value)| format!("{key}={value}").into_bytes())
            .collect(),
        addresses: vec![ScopedAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST))],
    };
    let key = DeviceKey::Id(DeviceId(television::DEVICE_ID));
    Device::from_records(key, None, Some(&record)).expect("the HomePod's record")
}

fn file_ending(dir: &Path, suffix: &str) -> Option<String> {
    let mut names: Vec<_> = fs::read_dir(dir)
        .ok()?
        .filter_map(|entry| entry.ok()?.file_name().into_string().ok())
        .collect();
    names.sort();
    let name = names.into_iter().find(|name| name.ends_with(suffix))?;
    fs::read_to_string(dir.join(name)).ok()
}

#[test]
fn a_session_against_the_homepod_profile_dumps_its_stream_setup() {
    let _serial = serial();
    let fake = television::homepod(
        television::HOMEPOD_NAME,
        Pin::None,
        "4821",
        television::DEVICE_ID,
        0,
    );
    let info = read_dict(&fake.info_body).expect("an /info plist");
    assert_eq!(
        info.get("model").and_then(|m| m.as_string()),
        Some(HOMEPOD_MODEL)
    );
    let server = Ap2Server::start(fake);
    let (sender, writer) = Sender::start(SenderConfig {
        airplay2: true,
        airplay2_device_id: Some([0x10, 0x20, 0x30, 0x40, 0x50, 0x61]),
        ..SenderConfig::default()
    })
    .expect("the sender starts");
    let _host = HostAudio::start(writer);
    let mut events = Events::default();
    let device = device(&server);
    let key = device.key.clone();
    sender.add_speaker(device, SpeakerOptions::new(Volume::Db(-20.0)));
    let playing = events.wait_state(&sender, &key, 1, LONG, |s| *s == SpeakerState::Playing);
    sender.shutdown();
    assert!(playing.is_some(), "{:?}", events.of(&key));

    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("fake_tv_homepod_dump");
    let _ = fs::remove_dir_all(&dir);
    let mut dump = Dump::create(&dir).expect("the dump directory");
    let mut lines = Vec::new();
    let fake = server.fake();
    for ((request, _), answered) in fake.requests.iter().zip(&fake.answered) {
        let answered = answered.as_ref().expect("every request answered");
        lines.extend(dump.record(request, answered).expect("a dump file"));
    }

    let info = file_ending(&dir, "-GET-info.txt").expect("the /info file");
    assert!(info.contains(HOMEPOD_MODEL), "{info}");
    let stream = file_ending(&dir, "-SETUP-stream.txt").expect("the stream SETUP file");
    for key in [
        "<?xml",
        "<key>type</key>",
        "<key>ct</key>",
        "<key>spf</key>",
        "<key>sr</key>",
    ] {
        assert!(stream.contains(key), "{key} missing:\n{stream}");
    }
    let summary = lines
        .iter()
        .find(|line| line.starts_with("dump: stream SETUP"))
        .expect("a stream SETUP line");
    assert!(summary.contains("type 96 realtime"), "{summary}");
    assert!(summary.contains("timing NTP"), "{summary}");
}

#[test]
fn a_method_the_parser_does_not_know_is_answered_under_its_own_name() {
    let mut fake = FakeAp2::new();
    let answer = fake.feed(
        b"SETRATEANCHORTIME rtsp://127.0.0.1/1 RTSP/1.0\r\nCSeq: 9\r\nContent-Length: 2\r\n\r\nab\
          GET_PARAMETER rtsp://127.0.0.1/1 RTSP/1.0\r\nCSeq: 10\r\n\r\n",
    );
    let text = String::from_utf8_lossy(&answer);
    assert_eq!(text.matches("RTSP/1.0 200").count(), 2, "{text}");
    assert_eq!(fake.keys(), ["SETRATEANCHORTIME", "GET_PARAMETER"]);
    assert_eq!(fake.requests[0].0.body, b"ab");
}
