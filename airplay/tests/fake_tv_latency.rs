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
    for (lf, latency_min) in [(None, 11_025), (Some(4_410), 4_410)] {
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
                    if *latency == Duration::from_millis(100))
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
fn fake_tv_a_burst_of_resend_requests_raises_lf_by_a_pause_the_size_of_the_step() {
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
    // Syncs are read from before the speaker joins and until the step is
    // seen: on a slow runner a late packet can step Lf before the burst does.
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
    let stepped = events.wait_for(&sender, LONG, |event| {
        matches!(event, SenderEvent::Timeline { latency, restarted: false, .. }
            if *latency == Duration::from_millis(200))
    });
    let after = read_for(&data, Duration::from_secs(2));
    stop.store(true, Ordering::Relaxed);
    let control_reads = control_reads.join().expect("the control reader");
    let syncs: Vec<SyncPacket> = control_reads
        .iter()
        .filter_map(|bytes| SyncPacket::parse(bytes))
        .collect();
    let stated: Vec<u32> = syncs
        .iter()
        .map(|sync| sync.next_rtp.wrapping_sub(sync.rtp_heard))
        .collect();
    let flushed_after = flushes();
    let cause = sender.stream_counters().last_step();
    sender.shutdown();
    assert!(stepped.is_some(), "Lf steps to 200 ms on the burst");
    assert_eq!(flushed_after, flushed, "no FLUSH for the step");
    let audio: Vec<(Instant, RtpHeader)> = before
        .iter()
        .chain(&after)
        .filter_map(|(at, bytes)| Some((*at, RtpHeader::parse(bytes)?)))
        .collect();
    let mut longest = Duration::ZERO;
    for pair in audio.windows(2) {
        assert_eq!(
            pair[1].1.seq,
            pair[0].1.seq.wrapping_add(1),
            "numbering runs on"
        );
        longest = longest.max(pair[1].0 - pair[0].0);
    }
    eprintln!(
        "fake_tv step: {} packets, longest arrival gap {} ms, syncs state {stated:?}, last step on {cause:?}",
        audio.len(),
        longest.as_millis()
    );
    assert!(
        longest < Duration::from_millis(150),
        "sending pauses only for the 100 ms step: {longest:?}"
    );
    let step = stated
        .iter()
        .position(|&lf| lf != 4_410)
        .expect("a sync after the step");
    assert!(step > 0, "a sync before the step: {stated:?}");
    assert!(
        stated[step..].iter().all(|&lf| lf == 8_820),
        "one step, from 4410 to 8820: {stated:?}"
    );
    let earlier = &syncs[step - 1];
    let sync = syncs.last().expect("a sync after the step");
    let silence = heard_by(sync, sync.next_rtp) - heard_by(earlier, sync.next_rtp);
    eprintln!("fake_tv step: silence {:.1} ms", silence * 1_000.0);
    assert!(
        (silence - 0.1).abs() < 0.002,
        "the pause is the step: {silence}"
    );
}

/// When a sync says RTP time `rtp` sounds.
fn heard_by(sync: &SyncPacket, rtp: u32) -> f64 {
    let at = (sync.time.0 >> 32) as f64 + (sync.time.0 & 0xffff_ffff) as f64 / 4_294_967_296.0;
    at + f64::from(rtp.wrapping_sub(sync.rtp_heard) as i32) / 44_100.0
}
