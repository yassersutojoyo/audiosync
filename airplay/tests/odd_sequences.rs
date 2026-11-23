//! Odd user sequences over loopback: a speaker switched off and on fast, a
//! level or a new track while a speaker reconnects or the stream is paused,
//! and a pause and resume pressed in a burst.

mod common;

use std::time::{Duration, Instant};

use audiosync_airplay::metadata::TrackInfo;
use audiosync_airplay::rtsp::Method;
use audiosync_airplay::sender::{NowPlaying, Sender, SenderConfig, SpeakerOptions, SpeakerState};
use audiosync_airplay::volume::Volume;
use common::net::{Behavior, Events, HostAudio, NetReceiver, serial, wait_until};

const PCM_TXT: &[&str] = &[
    "txtvers=1",
    "ch=2",
    "cn=0",
    "et=0",
    "sr=44100",
    "ss=16",
    "tp=UDP",
    "md=0,1,2",
];

const LONG: Duration = Duration::from_secs(15);

fn audio_after(receiver: &NetReceiver, at: Instant) -> usize {
    receiver
        .log()
        .audio
        .iter()
        .filter(|packet| packet.at > at)
        .count()
}

fn volumes(receiver: &NetReceiver) -> Vec<Vec<u8>> {
    receiver
        .log()
        .requests
        .iter()
        .filter(|(_, request)| {
            request.method == Method::SetParameter && request.body.starts_with(b"volume:")
        })
        .map(|(_, request)| request.body.clone())
        .collect()
}

fn names_title(receiver: &NetReceiver, title: &str, after: usize) -> bool {
    receiver
        .log()
        .requests
        .iter()
        .skip(after)
        .any(|(_, request)| {
            request.headers.get("Content-Type") == Some("application/x-dmap-tagged")
                && request
                    .body
                    .windows(title.len())
                    .any(|window| window == title.as_bytes())
        })
}

fn track(title: &str) -> NowPlaying {
    NowPlaying {
        track: Some(TrackInfo {
            title: title.to_owned(),
            artist: "Artist".to_owned(),
            album: "Album".to_owned(),
        }),
        ..NowPlaying::default()
    }
}

#[test]
fn a_speaker_switched_off_and_on_fast_plays_again() {
    let _serial = serial();
    let receiver = NetReceiver::start(Behavior::default());
    let (sender, writer) = Sender::start(SenderConfig::default()).expect("the sender starts");
    let _host = HostAudio::start(writer);
    let mut events = Events::default();
    let device = receiver.device(1, PCM_TXT);
    let key = device.key.clone();
    sender.add_speaker(device.clone(), SpeakerOptions::new(Volume::Db(-20.0)));
    assert!(
        events
            .wait_state(&sender, &key, 1, LONG, |s| *s == SpeakerState::Playing)
            .is_some()
    );
    for _ in 0..3 {
        sender.remove_speaker(&key);
        sender.add_speaker(device.clone(), SpeakerOptions::new(Volume::Db(-20.0)));
    }
    let toggled = Instant::now();
    assert!(
        wait_until(LONG, || audio_after(&receiver, toggled) >= 100),
        "{:?}",
        events.of(&key)
    );
    events.drain(&sender);
    assert_eq!(
        events.of(&key).iter().rev().find_map(|event| match event {
            audiosync_airplay::sender::SpeakerEvent::State(state) => Some(*state),
            _ => None,
        }),
        Some(SpeakerState::Playing),
        "{:?}",
        events.of(&key)
    );
    sender.shutdown();
}

#[test]
fn a_level_set_while_a_speaker_reconnects_is_the_level_it_plays_at() {
    let _serial = serial();
    let receiver = NetReceiver::start(Behavior::default());
    let (sender, writer) = Sender::start(SenderConfig::default()).expect("the sender starts");
    let _host = HostAudio::start(writer);
    let mut events = Events::default();
    let device = receiver.device(1, PCM_TXT);
    let key = device.key.clone();
    sender.add_speaker(device, SpeakerOptions::new(Volume::Db(-20.0)));
    assert!(
        events
            .wait_state(&sender, &key, 1, LONG, |s| *s == SpeakerState::Playing)
            .is_some()
    );
    receiver.kill();
    sender.set_volume(&key, Volume::Db(-7.0));
    assert!(
        events
            .wait_state(&sender, &key, 2, LONG, |s| *s == SpeakerState::Playing)
            .is_some(),
        "{:?}",
        events.of(&key)
    );
    assert!(
        wait_until(LONG, || volumes(&receiver).last()
            == Some(&b"volume: -7.000000\r\n".to_vec())),
        "{:?}",
        volumes(&receiver)
            .iter()
            .map(|body| String::from_utf8_lossy(body).into_owned())
            .collect::<Vec<_>>()
    );
    sender.shutdown();
}

#[test]
fn a_track_changed_while_a_speaker_reconnects_reaches_it_once_it_plays() {
    let _serial = serial();
    let receiver = NetReceiver::start(Behavior::default());
    let (sender, writer) = Sender::start(SenderConfig::default()).expect("the sender starts");
    let _host = HostAudio::start(writer);
    let mut events = Events::default();
    let device = receiver.device(1, PCM_TXT);
    let key = device.key.clone();
    sender.add_speaker(device, SpeakerOptions::new(Volume::Db(-20.0)));
    assert!(
        events
            .wait_state(&sender, &key, 1, LONG, |s| *s == SpeakerState::Playing)
            .is_some()
    );
    sender.set_now_playing(track("First")).expect("valid");
    assert!(wait_until(LONG, || names_title(&receiver, "First", 0)));
    receiver.kill();
    let before = receiver.log().requests.len();
    sender.set_now_playing(track("Second")).expect("valid");
    assert!(
        events
            .wait_state(&sender, &key, 2, LONG, |s| *s == SpeakerState::Playing)
            .is_some()
    );
    assert!(
        wait_until(LONG, || names_title(&receiver, "Second", before)),
        "the new track never reached the reconnected speaker"
    );
    sender.shutdown();
}

#[test]
fn a_burst_of_pause_and_resume_ends_playing() {
    let _serial = serial();
    let receiver = NetReceiver::start(Behavior::default());
    let (sender, writer) = Sender::start(SenderConfig::default()).expect("the sender starts");
    let _host = HostAudio::start(writer);
    let mut events = Events::default();
    let device = receiver.device(1, PCM_TXT);
    let key = device.key.clone();
    sender.add_speaker(device, SpeakerOptions::new(Volume::Db(-20.0)));
    assert!(
        events
            .wait_state(&sender, &key, 1, LONG, |s| *s == SpeakerState::Playing)
            .is_some()
    );
    assert!(wait_until(LONG, || receiver.log().audio.len() >= 50));
    for _ in 0..5 {
        sender.pause();
        std::thread::sleep(Duration::from_millis(20));
        sender.resume();
    }
    let resumed = Instant::now();
    assert!(
        wait_until(LONG, || audio_after(&receiver, resumed) >= 100),
        "no audio after the burst"
    );
    sender.shutdown();
}

#[test]
fn a_speaker_switched_off_and_on_while_paused_plays_on_resume() {
    let _serial = serial();
    let receiver = NetReceiver::start(Behavior::default());
    let (sender, writer) = Sender::start(SenderConfig::default()).expect("the sender starts");
    // At an Lf of 100 ms a loaded runner's stall restarts the timeline (P143),
    // and that FLUSH would pass for the pause's; 2 s rules it out.
    sender.set_latency(88_200);
    let _host = HostAudio::start(writer);
    let mut events = Events::default();
    let device = receiver.device(1, PCM_TXT);
    let key = device.key.clone();
    sender.add_speaker(device.clone(), SpeakerOptions::new(Volume::Db(-20.0)));
    assert!(
        events
            .wait_state(&sender, &key, 1, LONG, |s| *s == SpeakerState::Playing)
            .is_some()
    );
    // Playing comes with RECORD's answer; a pause before the first packet has
    // nothing to flush (P213), so the pause waits for audio.
    assert!(wait_until(LONG, || receiver.log().audio.len() >= 50));
    sender.pause();
    assert!(wait_until(LONG, || receiver.log().count("FLUSH") >= 1));
    sender.remove_speaker(&key);
    sender.add_speaker(device, SpeakerOptions::new(Volume::Db(-15.0)));
    assert!(
        events
            .wait_state(&sender, &key, 2, LONG, |s| *s == SpeakerState::Playing)
            .is_some(),
        "{:?}",
        events.of(&key)
    );
    let paused = Instant::now();
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(audio_after(&receiver, paused), 0, "no audio while paused");
    sender.resume();
    let resumed = Instant::now();
    assert!(
        wait_until(LONG, || audio_after(&receiver, resumed) >= 100),
        "no audio after the resume"
    );
    assert_eq!(
        volumes(&receiver).last(),
        Some(&b"volume: -15.000000\r\n".to_vec())
    );
    sender.shutdown();
}

#[test]
fn a_new_sender_right_after_the_last_one_shut_down_plays() {
    let _serial = serial();
    let receiver = NetReceiver::start(Behavior::default());
    for round in 0..2 {
        let (sender, writer) = Sender::start(SenderConfig::default()).expect("the sender starts");
        let _host = HostAudio::start(writer);
        let mut events = Events::default();
        let device = receiver.device(1, PCM_TXT);
        let key = device.key.clone();
        sender.add_speaker(device, SpeakerOptions::new(Volume::Db(-20.0)));
        assert!(
            events
                .wait_state(&sender, &key, 1, LONG, |s| *s == SpeakerState::Playing)
                .is_some(),
            "round {round}: {:?}",
            events.of(&key)
        );
        let started = Instant::now();
        assert!(wait_until(LONG, || audio_after(&receiver, started) >= 30));
        sender.pause();
        sender.shutdown();
    }
}
