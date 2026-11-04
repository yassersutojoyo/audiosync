//! The stream at the smallest base latency, 4410 frames (0.1 s), which a
//! user may choose through the hidden override of P200: the clamp and the
//! parse of the override, the timing of the first packets against the
//! sync anchor on a fake clock, and resends over loopback.

mod common;

use std::time::Duration;

use audiosync_airplay::audio::FRAMES_PER_PACKET;
use audiosync_airplay::latency::{AIRPLAY_LATENCY_ENV, latency_override, override_latency_frames};
use audiosync_airplay::rtp::{HEADER_BYTES, RESEND_PREFIX_BYTES, ResendRequest};
use audiosync_airplay::sdp::Codec;
use audiosync_airplay::sender::{Sender, SenderConfig, SenderEvent, SpeakerOptions, SpeakerState};
use audiosync_airplay::stream::{PacketKind, StreamConfig};
use audiosync_airplay::volume::Volume;
use common::net::{Behavior, Events, HostAudio, NetReceiver, serial, wait_until};
use common::rig::{Host, Rig, speaker};

const LOW: u32 = 4_410;
const LONG: Duration = Duration::from_secs(15);

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

fn low_rig() -> Rig {
    let mut rig = Rig::new(StreamConfig {
        latency_frames: LOW,
        input_frames: 88_200,
    });
    rig.host = Some(Host::new(0.0));
    rig
}

// covers: P200
#[test]
fn the_override_reads_whole_milliseconds_and_clamps_to_the_range() {
    assert_eq!(AIRPLAY_LATENCY_ENV, "AUDIOSYNC_AIRPLAY_LATENCY_MS");
    assert_eq!(override_latency_frames(100), LOW);
    assert_eq!(override_latency_frames(150), 6_615);
    assert_eq!(override_latency_frames(0), LOW);
    assert_eq!(override_latency_frames(50), LOW);
    assert_eq!(override_latency_frames(5_000), 88_200);
    assert_eq!(override_latency_frames(u64::MAX), 88_200);
    assert_eq!(latency_override(None), Ok(None));
    assert_eq!(latency_override(Some("100")), Ok(Some(LOW)));
    assert_eq!(latency_override(Some("2000")), Ok(Some(88_200)));
    assert_eq!(latency_override(Some("10")), Ok(Some(LOW)));
    for junk in ["", "-5", "1.5", "100 ms", " 100", "fast"] {
        let why = latency_override(Some(junk)).expect_err(junk);
        assert!(why.contains(AIRPLAY_LATENCY_ENV), "{junk:?}: {why}");
    }
}

// covers: P98
#[test]
fn at_a_tenth_of_a_second_every_packet_leaves_before_its_play_time() {
    let mut rig = low_rig();
    assert_eq!(rig.stream.latency_frames(), LOW);
    let id = rig.add(speaker(1, Codec::Alac));
    rig.run_for(Duration::from_secs(3));
    assert!((rig.latency() - 0.1).abs() < 1e-9);

    // The anchor: the first packet is heard at c(n) + 0.1 s.
    let (packet, _, first_sound) = *rig.first_sounds(id).first().expect("first sound");
    let n = (packet * FRAMES_PER_PACKET as u64) as i64;
    assert!((first_sound - (rig.c(n) + 0.1)).abs() < 1e-6);

    let syncs = rig.syncs(id);
    assert!(syncs.len() >= 3);
    for (_, sync) in &syncs {
        assert_eq!(sync.next_rtp.wrapping_sub(sync.rtp_heard), LOW);
    }

    // Each audio packet leaves before its first frame's play time, with at
    // least the 0.1 s less the packet's own length and the margin M.
    let audio = rig.audio(id);
    assert!(audio.len() > 300);
    let mut least = f64::MAX;
    for packet in &audio {
        let n = rig.frame_of(id, packet.header.timestamp);
        let play = rig.c(n) + rig.latency();
        least = least.min(play - packet.at);
    }
    let packet_length = FRAMES_PER_PACKET as f64 / 44_100.0;
    let margin = rig.stream.margin().as_secs_f64();
    assert!(least > 0.0, "a packet left after its play time: {least}");
    assert!(
        least >= 0.1 - packet_length - margin - 0.005,
        "lead {least} with M {margin}"
    );
}

// covers: P88
#[test]
fn at_a_tenth_of_a_second_a_lost_packet_is_resent_before_its_play_time() {
    let mut rig = low_rig();
    let id = rig.add(speaker(1, Codec::Alac));
    rig.run_for(Duration::from_secs(2));
    let lost = rig.audio(id).last().cloned().expect("audio");
    let play = rig.c(rig.frame_of(id, lost.header.timestamp)) + rig.latency();

    // The receiver notices the gap at the next packet and asks; 20 ms of
    // round trip later the request reaches the sender.
    rig.run_for(Duration::from_millis(20));
    let start = rig.sent.len();
    rig.stream.handle_control_packet(
        id,
        &ResendRequest {
            seq: 1,
            first: lost.header.seq,
            count: 1,
        }
        .encode(),
        rig.now,
    );
    rig.drain();
    let resent: Vec<_> = rig.sent[start..]
        .iter()
        .filter(|sent| sent.transmit.kind == PacketKind::Resend)
        .collect();
    assert_eq!(resent.len(), 1);
    assert_eq!(
        &resent[0].transmit.bytes[RESEND_PREFIX_BYTES + HEADER_BYTES..],
        &lost.payload[..]
    );
    assert!(
        resent[0].at < play,
        "resent at {} for play at {play}",
        resent[0].at
    );
}

// covers: P98, P87
#[test]
fn a_sender_set_to_a_tenth_of_a_second_states_it_and_serves_resends_over_loopback() {
    let _serial = serial();
    let receiver = NetReceiver::start(Behavior::default());
    let (sender, writer) = Sender::start(SenderConfig::default()).expect("the sender starts");
    let _host = HostAudio::start(writer);
    let mut events = Events::default();
    sender.set_latency(LOW);
    let changed = events.wait_for(&sender, LONG, |e| {
        matches!(e, SenderEvent::Timeline { latency, .. } if *latency == Duration::from_millis(100))
    });
    assert!(changed.is_some(), "Lf 4410 applies before any speaker");
    let device = receiver.device(21, PCM_TXT);
    let key = device.key.clone();
    let counters = sender.add_speaker(device, SpeakerOptions::new(Volume::Db(-20.0)));
    let playing = events.wait_state(&sender, &key, 1, LONG, |s| *s == SpeakerState::Playing);
    assert!(playing.is_some(), "never played: {:?}", events.of(&key));
    assert!(wait_until(LONG, || receiver.log().audio.len() >= 100
        && receiver.log().syncs.len() >= 2));
    for (_, _, sync) in &receiver.log().syncs {
        assert_eq!(sync.next_rtp.wrapping_sub(sync.rtp_heard), LOW);
    }

    // Asked again for the newest packet each time, as the loopback resend
    // of the transport tests is, since a loaded runner can lose a datagram.
    let mut resent = false;
    for _ in 0..5 {
        let original = receiver.log().audio.last().cloned().expect("a packet");
        receiver.request_resend(original.header.seq, 1);
        resent = wait_until(Duration::from_secs(3), || {
            receiver
                .log()
                .resends
                .iter()
                .any(|(_, bytes)| bytes[16..] == original.payload[..])
        });
        if resent {
            break;
        }
    }
    assert!(resent, "resent {}", counters.packets_resent());
    sender.shutdown();
}
