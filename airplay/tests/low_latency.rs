//! The stream at the smallest base latency, 4410 frames (0.1 s), where an
//! unseen speaker starts (P200): the steps up on lost or late packets, the
//! timing of the first packets against the sync anchor on a fake clock, and
//! resends over loopback.

mod common;

use std::time::Duration;

use audiosync_airplay::audio::FRAMES_PER_PACKET;
use audiosync_airplay::rtp::{HEADER_BYTES, RESEND_PREFIX_BYTES, ResendRequest, SyncPacket};
use audiosync_airplay::sdp::Codec;
use audiosync_airplay::sender::{Sender, SenderConfig, SenderEvent, SpeakerOptions, SpeakerState};
use audiosync_airplay::stream::{PacketKind, SpeakerId, StepCause, StreamConfig, StreamEvent};
use audiosync_airplay::volume::Volume;
use common::net::{Behavior, Events, HostAudio, NetReceiver, serial, wait_until};
use common::rig::{Host, Rig, decode, host_frame, speaker};

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

/// The receiver asks again for the newest packet, as one that lost it does.
fn lose_one(rig: &mut Rig, id: SpeakerId) {
    let lost = rig.audio(id).last().cloned().expect("audio");
    let request = ResendRequest {
        seq: 1,
        first: lost.header.seq,
        count: 1,
    };
    rig.stream
        .handle_control_packet(id, &request.encode(), rig.now);
    rig.drain();
}

/// Three requests within 2 s, as a receiver on a lossy network sends.
fn burst(rig: &mut Rig, id: SpeakerId) {
    for _ in 0..3 {
        lose_one(rig, id);
        rig.run_for(Duration::from_millis(100));
    }
}

/// A sync's time on the sender clock, in seconds.
fn sync_seconds(sync: &SyncPacket) -> f64 {
    (sync.time.0 >> 32) as f64 + (sync.time.0 & 0xffff_ffff) as f64 / 4_294_967_296.0
}

/// When a sync says RTP time `rtp` sounds.
fn heard_by(sync: &SyncPacket, rtp: u32) -> f64 {
    sync_seconds(sync) + f64::from(rtp.wrapping_sub(sync.rtp_heard) as i32) / 44_100.0
}

/// A speaker's RTP time for timeline frame `n`.
fn rtp_of(rig: &Rig, id: SpeakerId, n: i64) -> u32 {
    let (packet, rtptime, _) = *rig.first_sounds(id).last().expect("first sound");
    rtptime.wrapping_add((n - (packet * FRAMES_PER_PACKET as u64) as i64) as u32)
}

/// (L in ms, restarted) of every timeline event.
fn timelines(rig: &Rig) -> Vec<(u128, bool)> {
    rig.events
        .iter()
        .filter_map(|event| match event {
            StreamEvent::Timeline {
                latency, restarted, ..
            } => Some((latency.as_millis(), *restarted)),
            _ => None,
        })
        .collect()
}

// covers: P200, P201
#[test]
fn a_resend_request_doubles_lf_at_most_once_per_ten_seconds_without_a_flush() {
    let mut rig = low_rig();
    let id = rig.add(speaker(1, Codec::Alac));
    rig.run_for(Duration::from_secs(30));
    assert_eq!(rig.stream.latency_frames(), LOW, "no loss, no step");
    let mut steps = Vec::new();
    for _ in 0..6 {
        burst(&mut rig, id);
        rig.run_for(Duration::from_secs(1));
        burst(&mut rig, id);
        steps.push(rig.stream.latency_frames());
        rig.run_for(Duration::from_millis(8_500));
    }
    assert_eq!(steps, [8_820, 17_640, 35_280, 77_175, 77_175, 77_175]);
    assert_eq!(
        timelines(&rig),
        [(200, false), (400, false), (800, false), (1750, false)]
    );

    // Seamless: no flush, one first sound, numbering and RTP time run on.
    assert!(
        !rig.events
            .iter()
            .any(|event| matches!(event, StreamEvent::Flush { .. }))
    );
    assert_eq!(rig.first_sounds(id).len(), 1);
    let audio = rig.audio(id);
    for pair in audio.windows(2) {
        assert_eq!(pair[1].header.seq, pair[0].header.seq.wrapping_add(1));
        assert_eq!(
            pair[1].header.timestamp,
            pair[0]
                .header
                .timestamp
                .wrapping_add(FRAMES_PER_PACKET as u32)
        );
        assert!(!pair[1].header.marker);
    }
    let (_, sync) = *rig.syncs(id).last().expect("a sync");
    assert_eq!(sync.next_rtp.wrapping_sub(sync.rtp_heard), 77_175);

    // Never down within the session: a lower Lf from the host is held.
    rig.stream.set_latency(LOW, rig.now);
    rig.run_for(Duration::from_secs(5));
    assert_eq!(rig.stream.latency_frames(), 77_175);
}

// covers: P200
#[test]
fn a_stray_resend_request_steps_nothing_and_three_within_2_s_do() {
    let mut rig = low_rig();
    let id = rig.add(speaker(1, Codec::Alac));
    rig.run_for(Duration::from_secs(2));
    for _ in 0..3 {
        lose_one(&mut rig, id);
        rig.run_for(Duration::from_millis(2_500));
    }
    assert_eq!(rig.stream.latency_frames(), LOW, "one at a time");
    burst(&mut rig, id);
    rig.run_for(Duration::from_secs(1));
    assert_eq!(rig.stream.latency_frames(), 8_820);
    assert_eq!(rig.stream.counters().last_step(), Some(StepCause::Resends));
}

// covers: P200, P143
#[test]
fn a_stall_longer_than_lf_restarts_the_timeline_and_steps_lf() {
    let mut rig = low_rig();
    rig.add(speaker(1, Codec::Alac));
    rig.run_for(Duration::from_secs(2));
    let mut steps = Vec::new();
    for _ in 0..3 {
        let stall = rig.now + Duration::from_millis(1_000);
        rig.jump_to(stall);
        rig.answer_flushes();
        rig.run_for(Duration::from_secs(5));
        steps.push(rig.stream.latency_frames());
    }
    assert_eq!(steps, [8_820, 8_820, 17_640], "at most once per 10 s");
    assert_eq!(rig.stream.counters().last_step(), Some(StepCause::Stall));
    assert!(
        timelines(&rig).contains(&(200, true)),
        "{:?}",
        timelines(&rig)
    );
}

// covers: P200, P201, P213
#[test]
fn a_lower_lf_held_while_sounding_never_applies_below_a_step() {
    let mut rig = low_rig();
    let id = rig.add(speaker(1, Codec::Alac));
    rig.run_for(Duration::from_secs(2));
    burst(&mut rig, id);
    rig.run_for(Duration::from_secs(1));
    assert_eq!(rig.stream.latency_frames(), 8_820);
    rig.stream.set_latency(LOW, rig.now);
    rig.stream.pause(rig.now);
    rig.run_for(Duration::from_secs(1));
    rig.stream.resume(rig.now);
    rig.answer_flushes();
    rig.run_for(Duration::from_secs(2));
    assert_eq!(rig.stream.latency_frames(), 8_820, "not at a resume");
    rig.stream.set_latency(LOW, rig.now);
    rig.stream.restart(rig.now);
    rig.answer_flushes();
    rig.run_for(Duration::from_secs(2));
    assert_eq!(rig.stream.latency_frames(), 8_820, "not at a restart");
}

// covers: P200, P80
#[test]
fn a_step_pauses_each_speaker_inside_silence_whatever_its_delay() {
    let mut rig = low_rig();
    let one = rig.add(speaker(1, Codec::Pcm));
    let mut later = speaker(2, Codec::Pcm);
    later.delay = Duration::from_millis(250);
    let two = rig.add(later);
    rig.run_for(Duration::from_secs(2));
    burst(&mut rig, one);
    rig.run_for(Duration::from_secs(1));
    assert_eq!(rig.stream.latency_frames(), 8_820);
    let lead = |sync: &SyncPacket| sync.next_rtp.wrapping_sub(sync.rtp_heard);
    for id in [one, two] {
        let packets: Vec<(i64, Vec<[i16; 2]>)> = rig
            .audio(id)
            .iter()
            .map(|packet| {
                (
                    rig.frame_of(id, packet.header.timestamp),
                    decode(Codec::Pcm, &packet.payload),
                )
            })
            .collect();
        let out = packets
            .iter()
            .position(|(_, frames)| frames[FRAMES_PER_PACKET - 1] == [0, 0])
            .expect("a packet fades out");
        let back = out
            + 1
            + packets[out + 1..]
                .iter()
                .position(|(_, frames)| frames.iter().any(|frame| *frame != [0, 0]))
                .expect("the sound comes back");
        let (n, faded) = &packets[out];
        for (i, frame) in faded.iter().enumerate() {
            let full = host_frame((*n + i as i64) as u64);
            assert!(frame[0].abs() <= full[0].abs(), "fading out at {i}");
        }
        let (m, rising) = &packets[back];
        assert!(
            rising[0][0].abs() <= host_frame(*m as u64)[0].abs() / 300 + 1,
            "fades in from 0: {:?}",
            rising[0]
        );
        assert_eq!(
            rising[FRAMES_PER_PACKET - 1],
            host_frame((*m + FRAMES_PER_PACKET as i64 - 1) as u64)
        );

        // On the old Lf the receiver plays the fade's end, then silence until
        // the fade-in; the sync stating the step must land in that silence,
        // with room for the network, so the pause cuts no sound.
        let syncs = rig.syncs(id);
        let (sent, new) = syncs
            .iter()
            .find(|(_, sync)| lead(sync) == 8_820)
            .expect("a sync stating the step");
        let old = &syncs
            .iter()
            .rfind(|(at, sync)| at < sent && lead(sync) == LOW)
            .expect("a sync at 0.1 s")
            .1;
        let time = rig.clock.ntp_at(*sent);
        let sent = (time.0 >> 32) as f64 + (time.0 & 0xffff_ffff) as f64 / 4_294_967_296.0;
        let silent_from = heard_by(old, rtp_of(&rig, id, *n + FRAMES_PER_PACKET as i64));
        let sound_from = heard_by(old, rtp_of(&rig, id, *m));
        let pause = heard_by(new, rtp_of(&rig, id, *m)) - sound_from;
        eprintln!(
            "speaker {id:?}: sync {:.1} ms into {:.1} ms of silence, pause {:.1} ms",
            (sent - silent_from) * 1000.0,
            (sound_from - silent_from) * 1000.0,
            pause * 1000.0
        );
        // A tenth of a millisecond covers the NTP fraction's rounding.
        assert!(
            sent >= silent_from - 1e-4,
            "the sync comes after the fade's end"
        );
        assert!(
            sent + 0.016 <= sound_from,
            "the sync lands 16 ms or more before the fade-in"
        );
        assert!(
            (pause - 0.1).abs() < 0.002,
            "the pause is the step: {pause}"
        );
    }
}

// covers: P200
#[test]
fn a_packet_sent_late_steps_lf_as_a_resend_request_does() {
    let mut rig = low_rig();
    rig.add(speaker(1, Codec::Alac));
    rig.run_for(Duration::from_secs(2));
    assert_eq!(rig.stream.latency_frames(), LOW);
    let late = rig.now + Duration::from_millis(50);
    rig.jump_to(late);
    rig.run_for(Duration::from_millis(100));
    assert_eq!(rig.stream.latency_frames(), 8_820);
    assert_eq!(timelines(&rig), [(200, false)]);
}

// covers: P200
#[test]
fn late_sends_in_a_speakers_first_second_step_nothing_and_a_late_send_after_it_does() {
    let mut rig = low_rig();
    rig.run_for(Duration::from_secs(1));
    rig.add(speaker(1, Codec::Alac));
    // A loaded host wakes the sender late as a speaker starts, as a CI
    // runner does: the first turn ends past the speaker's first sound,
    // short of a stall, and a wake 0.2 s on is late again.
    let slow = rig.now + Duration::from_millis(90);
    rig.jump_to(slow);
    rig.run_for(Duration::from_millis(200));
    let again = rig.now + Duration::from_millis(30);
    rig.jump_to(again);
    rig.run_for(Duration::from_secs(2));
    assert_eq!(rig.stream.latency_frames(), LOW, "{:?}", timelines(&rig));
    assert_eq!(rig.stream.counters().last_step(), None);
    let late = rig.now + Duration::from_millis(50);
    rig.jump_to(late);
    rig.run_for(Duration::from_millis(100));
    assert_eq!(rig.stream.latency_frames(), 8_820);
    assert_eq!(rig.stream.counters().last_step(), Some(StepCause::LateSend));
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
