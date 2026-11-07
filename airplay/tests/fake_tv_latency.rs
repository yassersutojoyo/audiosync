//! The output latency `fake_tv` reports when `FAKE_TV_LATENCY_US` is set:
//! `outputLatencyMicros` in its `/info` answer, which the sender reads as
//! the receiver's R_s (P197, P199).

mod common;
#[path = "../examples/fake_tv/television.rs"]
mod television;

use std::net::{IpAddr, Ipv4Addr, UdpSocket};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use audiosync_airplay::discovery::{
    Device, DeviceId, DeviceKey, ScopedAddr, ServiceKind, ServiceRecord,
};
use audiosync_airplay::latency::LatencySources;
use audiosync_airplay::rtp::{ResendRequest, RtpHeader, SyncPacket};
use audiosync_airplay::sender::{
    Sender, SenderConfig, SenderEvent, SpeakerEvent, SpeakerOptions, SpeakerState,
};
use audiosync_airplay::volume::Volume;

use common::ap2::{Ap2Server, read_dict, request_key};
use common::net::{Events, HostAudio, serial};
use television::{LATENCY_VAR, Pin, latency_from, report_latency, television};

const LONG: Duration = Duration::from_secs(15);

fn plain_tv() -> common::ap2::FakeAp2 {
    television(
        "AudioSync Test TV",
        Pin::None,
        "4821",
        0,
        television::DEVICE_ID,
    )
}

fn device(server: &Ap2Server) -> Device {
    let record = ServiceRecord {
        kind: ServiceKind::AirPlay,
        instance: "AudioSync Test TV".to_owned(),
        host: format!("{}.local", television::HOST_LABEL),
        port: server.address.port(),
        txt: television::txt(Pin::None, television::DEVICE_ID)
            .into_iter()
            .map(|(key, value)| format!("{key}={value}").into_bytes())
            .collect(),
        addresses: vec![ScopedAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST))],
    };
    let key = DeviceKey::Id(DeviceId(television::DEVICE_ID));
    Device::from_records(key, None, Some(&record)).expect("the television's record")
}

#[test]
fn fake_tv_latency_unset_or_empty_reports_nothing() {
    assert_eq!(LATENCY_VAR, "FAKE_TV_LATENCY_US");
    assert_eq!(latency_from(None), Ok(None));
    assert_eq!(latency_from(Some("")), Ok(None));
    let info = read_dict(&plain_tv().info_body).expect("an /info plist");
    assert!(!info.contains_key("audioLatencies"), "{info:?}");
}

#[test]
fn fake_tv_latency_reads_whole_microseconds_and_names_the_variable_otherwise() {
    assert_eq!(latency_from(Some("0")), Ok(Some(0)));
    assert_eq!(latency_from(Some("250000")), Ok(Some(250_000)));
    for bad in ["-5", "1.5", "250 ms", " 250000", "abc"] {
        let error = latency_from(Some(bad)).expect_err(bad);
        assert!(error.contains(LATENCY_VAR), "{bad:?}: {error}");
    }
}

// covers: P203
#[test]
fn fake_tv_latency_reaches_the_sender_as_r_s() {
    const REPORTED_US: u64 = 320_000;
    let _serial = serial();
    let mut fake = plain_tv();
    report_latency(&mut fake, REPORTED_US);
    let server = Ap2Server::start(fake);
    let (sender, writer) = Sender::start(SenderConfig {
        airplay2: true,
        airplay2_device_id: Some([0x10, 0x20, 0x30, 0x40, 0x50, 0x60]),
        ..SenderConfig::default()
    })
    .expect("the sender starts");
    let _host = HostAudio::start(writer);
    let mut events = Events::default();
    let device = device(&server);
    let key = device.key.clone();
    sender.add_speaker(device, SpeakerOptions::new(Volume::Db(-20.0)));
    let playing = events.wait_state(&sender, &key, 1, LONG, |s| *s == SpeakerState::Playing);
    let latency = events.wait_for(&sender, LONG, |event| {
        matches!(
            event,
            SenderEvent::Speaker {
                event: SpeakerEvent::Latency(_),
                ..
            }
        )
    });
    sender.shutdown();
    assert!(playing.is_some(), "{:?}", events.of(&key));
    let Some(SenderEvent::Speaker {
        event: SpeakerEvent::Latency(latency),
        ..
    }) = latency
    else {
        panic!("no latency event: {:?}", events.of(&key));
    };
    assert_eq!(latency.receiver_latency_us, REPORTED_US);
    assert_eq!(
        latency.receiver_latency_sources,
        LatencySources::AUDIO_LATENCIES
    );
}

// covers: P44, P98
#[test]
fn fake_tv_stream_setup_asks_the_lf_in_use_when_it_is_below_the_capture_minimum() {
    let _serial = serial();
    for (lf, latency_min) in [(None, 4_410), (Some(17_640), 11_025)] {
        let server = Ap2Server::start(plain_tv());
        let (sender, writer) = Sender::start(SenderConfig {
            airplay2: true,
            airplay2_device_id: Some([0x10, 0x20, 0x30, 0x40, 0x50, 0x60]),
            ..SenderConfig::default()
        })
        .expect("the sender starts");
        let _host = HostAudio::start(writer);
        let mut events = Events::default();
        if let Some(frames) = lf {
            sender.set_latency(frames);
            let applied = events.wait_for(&sender, LONG, |event| {
                matches!(event, SenderEvent::Timeline { latency, .. }
                    if *latency == Duration::from_millis(400))
            });
            assert!(applied.is_some(), "Lf {frames} applies before any speaker");
        }
        let device = device(&server);
        let key = device.key.clone();
        sender.add_speaker(device, SpeakerOptions::new(Volume::Db(-20.0)));
        let playing = events.wait_state(&sender, &key, 1, LONG, |s| *s == SpeakerState::Playing);
        sender.shutdown();
        assert!(playing.is_some(), "{:?}", events.of(&key));
        assert_eq!(
            server.fake().stream_latency_min(),
            Some(latency_min),
            "Lf {lf:?}"
        );
    }
}

/// Every datagram reaching `socket` for `span`, with its arrival time.
fn read_for(socket: &UdpSocket, span: Duration) -> Vec<(Instant, Vec<u8>)> {
    socket
        .set_read_timeout(Some(Duration::from_millis(50)))
        .expect("a read timeout");
    let end = Instant::now() + span;
    let mut read = Vec::new();
    let mut buffer = [0u8; 2048];
    while Instant::now() < end {
        if let Ok(length) = socket.recv(&mut buffer) {
            read.push((Instant::now(), buffer[..length].to_vec()));
        }
    }
    read
}

// covers: P200
#[test]
fn fake_tv_a_burst_of_resend_requests_leaves_lf_at_a_tenth_of_a_second() {
    let _serial = serial();
    let server = Ap2Server::start(plain_tv());
    let [data, control] = <[UdpSocket; 2]>::try_from(server.udp()).expect("two sockets");
    let (sender, writer) = Sender::start(SenderConfig {
        airplay2: true,
        airplay2_device_id: Some([0x10, 0x20, 0x30, 0x40, 0x50, 0x60]),
        ..SenderConfig::default()
    })
    .expect("the sender starts");
    let _host = HostAudio::start(writer);
    let mut events = Events::default();
    sender.set_latency(4_410);
    let started = events.wait_for(&sender, LONG, |event| {
        matches!(event, SenderEvent::Timeline { latency, .. }
            if *latency == Duration::from_millis(100))
    });
    assert!(started.is_some(), "Lf 4410 applies before any speaker");
    let stop = Arc::new(AtomicBool::new(false));
    let reading = Arc::clone(&stop);
    let watcher = control.try_clone().expect("a control reader");
    let control_reads = std::thread::spawn(move || {
        watcher
            .set_read_timeout(Some(Duration::from_millis(50)))
            .expect("a read timeout");
        let mut read = Vec::new();
        let mut buffer = [0u8; 2048];
        while !reading.load(Ordering::Relaxed) {
            if let Ok(length) = watcher.recv(&mut buffer) {
                read.push(buffer[..length].to_vec());
            }
        }
        read
    });
    let device = device(&server);
    let key = device.key.clone();
    sender.add_speaker(device, SpeakerOptions::new(Volume::Db(-20.0)));
    let playing = events.wait_state(&sender, &key, 1, LONG, |s| *s == SpeakerState::Playing);
    assert!(playing.is_some(), "{:?}", events.of(&key));

    let before = read_for(&data, Duration::from_secs(2));
    let flushes = || {
        server
            .fake()
            .requests
            .iter()
            .filter(|(request, _)| request_key(request) == "FLUSH")
            .count()
    };
    let flushed = flushes();
    let last = RtpHeader::parse(&before.last().expect("audio").1).expect("RTP");
    let setup = server
        .fake()
        .last("SETUP stream")
        .cloned()
        .expect("a stream SETUP");
    let port = read_dict(&setup.body)
        .and_then(|body| {
            body.get("streams")?
                .as_array()?
                .first()?
                .as_dictionary()?
                .get("controlPort")?
                .as_unsigned_integer()
        })
        .expect("the sender's control port");
    for seq in 1..=3u16 {
        let request = ResendRequest {
            seq,
            first: last.seq.wrapping_sub(3 - seq),
            count: 1,
        };
        control
            .send_to(&request.encode(), (Ipv4Addr::LOCALHOST, port as u16))
            .expect("the request goes out");
    }
    let stepped = events.wait_for(&sender, Duration::from_secs(3), |event| {
        matches!(event, SenderEvent::Timeline { latency, .. }
            if *latency != Duration::from_millis(100))
    });
    stop.store(true, Ordering::Relaxed);
    let control_reads = control_reads.join().expect("the control reader");
    let stated: Vec<u32> = control_reads
        .iter()
        .filter_map(|bytes| SyncPacket::parse(bytes))
        .map(|sync| sync.next_rtp.wrapping_sub(sync.rtp_heard))
        .collect();
    let flushed_after = flushes();
    sender.shutdown();
    assert!(stepped.is_none(), "{stepped:?}");
    assert_eq!(flushed_after, flushed, "no FLUSH");
    assert!(
        !stated.is_empty() && stated.iter().all(|&lf| lf == 4_410),
        "{stated:?}"
    );
}
