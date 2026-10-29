//! The whole system's capture into the network senders, heard by the mock
//! receivers of the airplay and cast crates' own tests (compiled in from
//! their tests/common folders): a fake AirPlay 1 receiver on loopback
//! sockets that takes PCM, and a fake Cast device that fetches the WAV
//! stream. Runs with the other live tests, on the default sink `as-a`.

use std::time::{Duration, Instant};

use audiosync_airplay::audio::{FRAMES_PER_PACKET, read_pcm_payload};
use audiosync_airplay::sender::{Sender, SenderConfig, SpeakerOptions, SpeakerState};
use audiosync_airplay::volume::Volume;
use audiosync_cast::audio::audio_ring;
use audiosync_cast::discovery::DeviceId;
use audiosync_cast::host::{CastHost, CastTarget, DeviceMemory, HostEvent};
use audiosync_cast::media::Metadata;
use audiosync_core::eq::equalizer;
use audiosync_core::ring::Ring;

use super::{detached, kill, set_default, tone};
use crate::audio::SESSION_RATE;
use crate::audio::airplay::FeedSlot;
use crate::audio::capture::Feeds;
use crate::audio::cast::CastFeed;
use crate::audio::controller::{CaptureHandle, CaptureState};
use crate::audio::pipewire::{graph, stream};

#[path = "../../../../../airplay/tests/common/mod.rs"]
mod airplay_mock;
#[path = "../../../../../cast/tests/common/mod.rs"]
mod cast_mock;

use airplay_mock::net::{Events, NetReceiver};
use cast_mock::{FakeDevice, http};

/// A receiver that takes PCM only, so what it got can be read back.
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

const LONG: Duration = Duration::from_secs(20);

/// A tenth of full scale: well above dither, well below the tone.
const AUDIBLE: i16 = 3277;

fn wait_until(limit: Duration, mut done: impl FnMut() -> bool) -> bool {
    let until = Instant::now() + limit;
    while Instant::now() < until {
        if done() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    done()
}

/// The loudest sample in the PCM packets the AirPlay receiver got.
fn airplay_peak(receiver: &NetReceiver) -> i16 {
    let log = receiver.log();
    let mut frames = [[0_i16; 2]; FRAMES_PER_PACKET];
    log.audio
        .iter()
        .filter(|packet| packet.payload.len() == FRAMES_PER_PACKET * 4)
        .map(|packet| {
            read_pcm_payload(&packet.payload, &mut frames);
            frames
                .as_flattened()
                .iter()
                .map(|s| s.saturating_abs())
                .max()
                .unwrap_or(0)
        })
        .max()
        .unwrap_or(0)
}

/// The loudest sample in the WAV streams the Cast device fetched.
fn cast_peak(device: &FakeDevice) -> i16 {
    device
        .fetches()
        .iter()
        .map(|fetch| {
            let body = http::chunks(&fetch.body).concat();
            let Some(data) = body.windows(4).position(|tag| tag == b"data") else {
                return 0;
            };
            body.get(data + 8..)
                .unwrap_or_default()
                .as_chunks::<2>()
                .0
                .iter()
                .map(|bytes| i16::from_le_bytes(*bytes).saturating_abs())
                .max()
                .unwrap_or(0)
        })
        .max()
        .unwrap_or(0)
}

#[test]
#[ignore = "needs the CI job's PipeWire daemon and sink as-a"]
fn the_whole_system_reaches_mock_airplay_and_cast_receivers() {
    set_default("as-a");
    let (sender, input) = Sender::start(SenderConfig::default()).expect("the AirPlay sender");
    let airplay_feed = FeedSlot::new(input);
    let mut cast_feed = CastFeed::new();
    let (writer, reader) = audio_ring(48_000);
    assert!(
        cast_feed.hand(Some(writer)),
        "the Cast writer is handed over"
    );

    let ring = Ring::for_rate(SESSION_RATE).unwrap();
    let (_control, processor) = equalizer();
    let capture = stream::start_plan(
        stream::Plan {
            wiring: stream::Wiring::Links,
            filter: Some(graph::Filter::Except(std::process::id())),
            serial: None,
        },
        ring.writer().unwrap(),
        processor,
        Feeds {
            airplay: airplay_feed.lease(),
            cast: cast_feed.slot().lease(),
            calibration: None,
        },
    )
    .unwrap();

    let airplay = NetReceiver::start(airplay_mock::net::Behavior::default());
    let device = airplay.device(1, PCM_TXT);
    let key = device.key.clone();
    let _counters = sender.add_speaker(device, SpeakerOptions::new(Volume::Db(0.0)));
    let mut events = Events::default();
    let playing = events.wait_state(&sender, &key, 1, LONG, |s| *s == SpeakerState::Playing);
    assert!(
        playing.is_some(),
        "AirPlay never played: {:?}",
        events.of(&key)
    );

    let cast = FakeDevice::start(cast_mock::Behavior::default());
    let mut host = CastHost::new(DeviceMemory::default()).expect("the Cast worker");
    let target = CastTarget {
        id: DeviceId([0xC5; 16]),
        addresses: vec![cast.address],
        group: None,
        records: None,
    };
    host.start(target, SESSION_RATE, Metadata::default(), reader)
        .expect("the Cast session starts");
    let mut cast_events = Vec::new();
    let started = wait_until(LONG, || {
        while let Some(event) = host.poll_event() {
            cast_events.push(event);
        }
        cast_events
            .iter()
            .any(|event| matches!(event, HostEvent::Started { .. }))
    });
    assert!(started, "Cast never started: {cast_events:?}");

    let quiet_airplay = airplay_peak(&airplay);
    let quiet_cast = cast_peak(&cast);
    assert!(
        quiet_airplay <= AUDIBLE && quiet_cast <= AUDIBLE,
        "audible before the tone: AirPlay peak {quiet_airplay}, Cast peak {quiet_cast}"
    );
    let path = tone("receivers", 440.0, 0.5, 30.0);
    let app = detached(&format!("pw-play --latency 1024 {}", path.display()));
    let airplay_heard = wait_until(LONG, || {
        events.drain(&sender);
        airplay_peak(&airplay) > AUDIBLE
    });
    let cast_heard = wait_until(LONG, || {
        while host.poll_event().is_some() {}
        cast_peak(&cast) > AUDIBLE
    });
    let state = capture.state();
    kill(app);
    drop(capture);
    host.stop();
    assert!(
        airplay_heard,
        "AirPlay peak {} (before the tone {quiet_airplay}), {} packets",
        airplay_peak(&airplay),
        airplay.log().audio.len()
    );
    assert!(
        cast_heard,
        "Cast peak {} (before the tone {quiet_cast}), fetches {}",
        cast_peak(&cast),
        cast.fetches().len()
    );
    assert_eq!(state, CaptureState::Running);
}
