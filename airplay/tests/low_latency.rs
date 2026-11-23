//! The stream at the smallest base latency, 4410 frames (0.1 s), where
//! every stream starts (P200): lost or late packets that never step Lf, the
//! step itself, the timing of the first packets against the sync anchor on a fake clock, and
//! resends over loopback.

mod common;

use std::time::Duration;

use audiosync_airplay::audio::FRAMES_PER_PACKET;
use audiosync_airplay::latency::session_latency_frames;
use audiosync_airplay::rtp::{HEADER_BYTES, RESEND_PREFIX_BYTES, ResendRequest, SyncPacket};
use audiosync_airplay::sdp::Codec;
use audiosync_airplay::sender::{Sender, SenderConfig, SenderEvent, SpeakerOptions, SpeakerState};
use audiosync_airplay::stream::{PacketKind, SpeakerId, StreamConfig, StreamEvent};
use audiosync_airplay::volume::Volume;
use common::net::{Behavior, Events, HostAudio, NetReceiver, serial, wait_until};
use common::rig::{Host, Rig, Rng, decode, host_frame, speaker};

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
        ..StreamConfig::default()
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
fn a_forced_step_steps_lf_up_once_to_8820_without_a_flush() {
    let mut rig = low_rig();
    let id = rig.add(speaker(1, Codec::Alac));
    rig.run_for(Duration::from_secs(30));
    assert_eq!(rig.stream.latency_frames(), LOW, "no step asked");
    let mut steps = Vec::new();
    for _ in 0..3 {
        rig.stream.step_up(rig.now);
        rig.run_for(Duration::from_secs(1));
        rig.stream.step_up(rig.now);
        steps.push(rig.stream.latency_frames());
        rig.run_for(Duration::from_millis(9_500));
    }
    assert_eq!(steps, [8_820, 8_820, 8_820]);
    assert_eq!(timelines(&rig), [(200, false)]);

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
    assert_eq!(sync.next_rtp.wrapping_sub(sync.rtp_heard), 8_820);

    // Never down within the session: a lower Lf from the host is held.
    rig.stream.set_latency(LOW, rig.now);
    rig.run_for(Duration::from_secs(5));
    assert_eq!(rig.stream.latency_frames(), 8_820);
}

// covers: P200, P201, P213
#[test]
fn a_lower_lf_held_while_sounding_never_applies_below_a_step() {
    let mut rig = low_rig();
    rig.add(speaker(1, Codec::Alac));
    rig.run_for(Duration::from_secs(2));
    rig.stream.step_up(rig.now);
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
    rig.stream.step_up(rig.now);
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
fn a_step_tells_the_host_where_the_speakers_move_before_they_do() {
    let mut rig = low_rig();
    let id = rig.add(speaker(1, Codec::Pcm));
    rig.run_for(Duration::from_secs(2));
    assert_eq!(rig.writer.take_move(), None, "no step, no move");
    rig.stream.step_up(rig.now);
    rig.run_for(Duration::from_millis(10));
    let moved = rig.writer.take_move().expect("the step is announced");
    assert_eq!(rig.stream.latency_frames(), LOW, "before the speakers move");
    assert_eq!(moved.frames, 4_410, "by the step");
    rig.run_for(Duration::from_secs(1));
    assert_eq!(rig.stream.latency_frames(), 8_820);
    assert_eq!(rig.writer.take_move(), None, "once");

    // From the first frame of the packet that fades out.
    let faded = rig
        .audio(id)
        .iter()
        .find(|packet| {
            let frames = decode(Codec::Pcm, &packet.payload);
            frames[0] != [0, 0] && frames[FRAMES_PER_PACKET - 1] == [0, 0]
        })
        .map(|packet| rig.frame_of(id, packet.header.timestamp))
        .expect("a packet fades out");
    let origin = rig
        .events
        .iter()
        .filter_map(|event| match event {
            StreamEvent::Timeline { origin, .. } => Some(*origin),
            _ => None,
        })
        .next_back()
        .unwrap_or(0);
    assert_eq!(moved.position as i64, faded + origin);
}

// covers: P200, P144
#[test]
fn late_sends_never_raise_lf_and_are_counted() {
    let mut rig = low_rig();
    let id = rig.add(speaker(1, Codec::Alac));
    rig.run_for(Duration::from_secs(2));
    for _ in 0..4 {
        let late = rig.now + Duration::from_millis(50);
        rig.jump_to(late);
        rig.run_for(Duration::from_secs(11));
    }
    assert_eq!(rig.stream.latency_frames(), LOW);
    assert_eq!(timelines(&rig), []);
    let counters = rig.stream.speaker_counters(id).expect("counters");
    assert!(counters.late_packets() >= 4, "{}", counters.late_packets());
}

// covers: P200, P143
#[test]
fn resend_bursts_and_stalls_never_raise_lf() {
    let mut rig = low_rig();
    let id = rig.add(speaker(1, Codec::Alac));
    rig.run_for(Duration::from_secs(2));
    for _ in 0..4 {
        burst(&mut rig, id);
        rig.run_for(Duration::from_secs(11));
    }
    assert_eq!(rig.stream.latency_frames(), LOW, "{:?}", timelines(&rig));
    for _ in 0..2 {
        let stall = rig.now + Duration::from_millis(1_000);
        rig.jump_to(stall);
        rig.answer_flushes();
        rig.run_for(Duration::from_secs(11));
    }
    assert_eq!(rig.stream.latency_frames(), LOW);
    assert_eq!(timelines(&rig), [(100, true), (100, true)]);
}

// covers: P98
#[test]
fn a_stream_starts_at_a_tenth_of_a_second_by_default() {
    assert_eq!(StreamConfig::default().latency_frames, LOW);
    assert_eq!(SenderConfig::default().latency_frames, LOW);
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
    assert!(wait_until(LONG, || receiver.log().audio.len() >= 100));
    // A burst of resend requests leaves Lf where it is (P200).
    let newest = receiver.log().audio.last().expect("a packet").header.seq;
    for _ in 0..5 {
        receiver.request_resend(newest, 1);
    }
    std::thread::sleep(Duration::from_secs(1));
    let lead = |sync: &SyncPacket| sync.next_rtp.wrapping_sub(sync.rtp_heard);
    let stated: Vec<u32> = receiver
        .log()
        .syncs
        .iter()
        .map(|(_, _, s)| lead(s))
        .collect();
    assert!(
        !stated.is_empty() && stated.iter().all(|&lf| lf == LOW),
        "Lf 4410 throughout: {stated:?}"
    );

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

/// Four seconds of programme, the last half second of each silent.
const PERIOD: u64 = 176_400;
const PAUSE: u64 = 22_050;
/// The quiet run a step waits for: 26 packets, about 207 ms (P200).
const QUIET_FRAMES: u64 = 26 * FRAMES_PER_PACKET as u64;

fn quiet_rig() -> Rig {
    let mut rig = low_rig();
    if let Some(host) = rig.host.as_mut() {
        host.quiet = Some((PERIOD, PAUSE));
    }
    rig
}

/// The receiver asks for a packet whose play time has passed: it played
/// without it.
fn gap(rig: &mut Rig, id: SpeakerId) {
    let now = rig.t();
    let latency = rig.latency();
    let missed = rig
        .audio(id)
        .into_iter()
        .rfind(|packet| packet.at < now - latency - 0.3)
        .expect("a packet played");
    let request = ResendRequest {
        seq: 1,
        first: missed.header.seq,
        count: 1,
    };
    rig.stream
        .handle_control_packet(id, &request.encode(), rig.now);
    rig.drain();
}

fn run_to(rig: &mut Rig, seconds: f64) {
    let end = rig.instant(seconds);
    rig.run_for(end.saturating_duration_since(rig.now));
}

/// Whether the programme is silent over the quiet run before `position`
/// and the step's packet and the 5 after it.
fn quiet_around(rig: &Rig, position: u64) -> bool {
    let host = rig.host.as_ref().expect("host");
    let after = 6 * FRAMES_PER_PACKET as u64;
    (position - QUIET_FRAMES..position + after)
        .all(|f| host.frame(f).iter().all(|s| s.unsigned_abs() < 33))
}

/// Whether every packet the speaker got carries the programme unchanged:
/// a step mutes or fades nothing.
fn programme_unchanged(rig: &Rig, id: SpeakerId) -> bool {
    let host = rig.host.as_ref().expect("host");
    rig.audio(id).iter().all(|packet| {
        let n = rig.frame_of(id, packet.header.timestamp) as u64;
        decode(Codec::Pcm, &packet.payload)
            .iter()
            .enumerate()
            .all(|(i, frame)| *frame == host.frame(n + i as u64))
    })
}

// covers: P200
#[test]
fn a_gap_steps_up_once_inside_a_quiet_window() {
    let mut rig = quiet_rig();
    let id = rig.add(speaker(1, Codec::Pcm));
    run_to(&mut rig, 1.0);
    gap(&mut rig, id);
    run_to(&mut rig, 3.5);
    assert_eq!(rig.stream.latency_frames(), LOW, "waits for a pause");
    assert_eq!(rig.writer.take_move(), None);
    run_to(&mut rig, 4.5);
    assert_eq!(rig.stream.latency_frames(), 8_820);
    let moved = rig.writer.take_move().expect("the step is announced");
    assert_eq!(moved.frames, 4_410);
    assert!(quiet_around(&rig, moved.position), "at {}", moved.position);
    assert!(moved.position < PERIOD, "in the first pause");
    assert_eq!(timelines(&rig), [(200, false)]);
    assert!(programme_unchanged(&rig, id));
}

// covers: P200
#[test]
fn a_step_waits_while_the_programme_is_loud() {
    let mut rig = low_rig();
    let id = rig.add(speaker(1, Codec::Pcm));
    run_to(&mut rig, 1.0);
    gap(&mut rig, id);
    run_to(&mut rig, 31.0);
    assert_eq!(rig.stream.latency_frames(), LOW);
    assert_eq!(rig.writer.take_move(), None);
    assert_eq!(timelines(&rig), []);
}

// covers: P200
#[test]
fn a_session_steps_up_once_and_back_down_once() {
    let mut rig = quiet_rig();
    let id = rig.add(speaker(1, Codec::Pcm));
    let other = rig.add(speaker(2, Codec::Pcm));
    run_to(&mut rig, 1.0);
    gap(&mut rig, id);
    run_to(&mut rig, 5.0);
    assert_eq!(rig.stream.latency_frames(), 8_820);
    for second in [6.0, 12.0, 20.0] {
        run_to(&mut rig, second);
        gap(&mut rig, id);
    }
    // The step holds while the receiver whose gap asked for it plays.
    rig.stream.remove_speaker(id);
    rig.drain();
    // The last gap starts the clean minute again.
    run_to(&mut rig, 79.9);
    assert_eq!(rig.stream.latency_frames(), 8_820, "not before 60 s clean");
    assert_eq!(timelines(&rig), [(200, false)], "never a second step up");
    let _ = rig.writer.take_move();
    run_to(&mut rig, 85.0);
    assert_eq!(rig.stream.latency_frames(), LOW, "back down");
    let moved = rig.writer.take_move().expect("the step down is announced");
    assert_eq!(moved.frames, -4_410);
    assert!(quiet_around(&rig, moved.position), "at {}", moved.position);
    assert!(
        moved.position > 80 * 44_100 - PAUSE,
        "in the pause the clean minute ends in"
    );
    assert!(programme_unchanged(&rig, other));

    run_to(&mut rig, 86.0);
    gap(&mut rig, other);
    run_to(&mut rig, 160.0);
    assert_eq!(rig.stream.latency_frames(), LOW, "no step after the cycle");
    assert_eq!(timelines(&rig), [(200, false), (100, false)]);
}

// covers: P200, P144
#[test]
fn in_time_resends_and_late_sends_never_step() {
    let mut rig = quiet_rig();
    let id = rig.add(speaker(1, Codec::Pcm));
    run_to(&mut rig, 1.0);
    for _ in 0..4 {
        burst(&mut rig, id);
        let late = rig.now + Duration::from_millis(50);
        rig.jump_to(late);
        rig.run_for(Duration::from_secs(5));
    }
    assert_eq!(rig.stream.latency_frames(), LOW);
    assert_eq!(rig.writer.take_move(), None);
    assert_eq!(timelines(&rig), []);
}

// covers: P200, P96, P102
#[test]
fn received_timestamps_run_on_across_both_steps() {
    let mut rig = quiet_rig();
    let one = rig.add(speaker(1, Codec::Pcm));
    let mut later = speaker(2, Codec::Pcm);
    later.delay = Duration::from_millis(250);
    let two = rig.add(later);
    let gapping = rig.add(speaker(3, Codec::Pcm));
    run_to(&mut rig, 1.0);
    gap(&mut rig, gapping);
    run_to(&mut rig, 5.0);
    // The step holds while the receiver whose gap asked for it plays.
    rig.stream.remove_speaker(gapping);
    rig.drain();
    run_to(&mut rig, 72.0);
    assert_eq!(timelines(&rig), [(200, false), (100, false)]);
    assert!(
        !rig.events
            .iter()
            .any(|event| matches!(event, StreamEvent::Flush { .. }))
    );
    let lead = |sync: &SyncPacket| sync.next_rtp.wrapping_sub(sync.rtp_heard);
    for id in [one, two] {
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
        }
        let first = rig.frame_of(id, audio[0].header.timestamp);
        let samples: Vec<[i16; 2]> = audio
            .iter()
            .flat_map(|packet| decode(Codec::Pcm, &packet.payload))
            .collect();
        let quiet_at = |n: i64| {
            (n - 441..n + 441).all(|m| {
                let frame = samples[(m - first) as usize];
                frame[0].unsigned_abs() < 33 && frame[1].unsigned_abs() < 33
            })
        };
        // Where frame 0 sounds, by each sync: it moves twice, by the step.
        let reference = rtp_of(&rig, id, 0);
        let syncs = rig.syncs(id);
        let mut moves = Vec::new();
        for pair in syncs.windows(2) {
            let (before, after) = (&pair[0].1, &pair[1].1);
            let shift = heard_by(after, reference) - heard_by(before, reference);
            if lead(before) == lead(after) {
                assert!(shift.abs() < 1e-3, "a jump of {shift} s");
                continue;
            }
            moves.push((shift * 1000.0).round() as i64);
            // The receiver hears silence on both sides of the jump.
            let time = rig.clock.ntp_at(pair[1].0);
            let sent = (time.0 >> 32) as f64 + (time.0 & 0xffff_ffff) as f64 / 4_294_967_296.0;
            for sync in [before, after] {
                let rtp = sync
                    .rtp_heard
                    .wrapping_add(((sent - sync_seconds(sync)) * 44_100.0).round() as i64 as u32);
                let n = rig.frame_of(id, rtp);
                assert!(quiet_at(n), "speaker {id:?} hears sound at frame {n}");
            }
        }
        assert_eq!(moves, [100, -100], "speaker {id:?}");
    }
}

// covers: P200
#[test]
fn a_step_down_returns_to_the_lf_a_clamped_step_up_left() {
    let mut rig = quiet_rig();
    rig.stream.set_latency(87_000, rig.now);
    let id = rig.add(speaker(1, Codec::Pcm));
    rig.add(speaker(2, Codec::Pcm));
    run_to(&mut rig, 3.0);
    gap(&mut rig, id);
    run_to(&mut rig, 7.0);
    assert_eq!(rig.stream.latency_frames(), 88_200, "clamped");
    rig.stream.remove_speaker(id);
    rig.drain();
    run_to(&mut rig, 75.0);
    assert_eq!(rig.stream.latency_frames(), 87_000);
}

// covers: P200, P201, P143
#[test]
fn a_step_down_never_goes_below_a_host_value_a_restart_applied() {
    let mut rig = quiet_rig();
    let id = rig.add(speaker(1, Codec::Pcm));
    run_to(&mut rig, 1.0);
    gap(&mut rig, id);
    run_to(&mut rig, 5.0);
    assert_eq!(rig.stream.latency_frames(), 8_820);
    rig.stream.set_latency(13_230, rig.now);
    rig.stream.restart(rig.now);
    rig.answer_flushes();
    run_to(&mut rig, 6.0);
    assert_eq!(rig.stream.latency_frames(), 13_230);
    run_to(&mut rig, 75.0);
    assert_eq!(rig.stream.latency_frames(), 13_230);
}

// covers: P200, P201
#[test]
fn a_step_down_returns_to_a_host_value_the_step_up_dropped() {
    let mut rig = quiet_rig();
    let id = rig.add(speaker(1, Codec::Pcm));
    rig.add(speaker(2, Codec::Pcm));
    run_to(&mut rig, 1.0);
    rig.stream.set_latency(6_000, rig.now);
    assert_eq!(rig.stream.pending_latency_frames(), Some(6_000));
    gap(&mut rig, id);
    run_to(&mut rig, 5.0);
    assert_eq!(rig.stream.latency_frames(), 8_820);
    rig.stream.remove_speaker(id);
    rig.drain();
    run_to(&mut rig, 75.0);
    assert_eq!(rig.stream.latency_frames(), 6_000);
}

// covers: P200
#[test]
fn a_step_waits_and_says_so_once_while_m_leaves_no_room() {
    let mut rig = quiet_rig();
    if let Some(host) = rig.host.as_mut() {
        // A 90 ms capture stall every half second holds M near 0.1 s.
        host.stalls = (1..60).map(|k| (k * 50, 0.09)).collect();
    }
    let id = rig.add(speaker(1, Codec::Pcm));
    run_to(&mut rig, 10.0);
    let margin = rig.stream.margin().as_secs_f64();
    assert!(margin > 0.07, "M {margin}");
    gap(&mut rig, id);
    run_to(&mut rig, 30.0);
    assert_eq!(rig.stream.latency_frames(), LOW);
    let told = rig
        .events
        .iter()
        .filter(|event| matches!(event, StreamEvent::StepWaiting { .. }))
        .count();
    assert_eq!(told, 1);
}

/// Audio packets that reach the receiver after their play time, by the
/// latest sync sent before each, over a network adding up to `jitter`
/// seconds one way: the receiver drops them without asking for a resend.
fn dropped_late(rig: &Rig, id: SpeakerId, jitter: f64, rng: &mut Rng) -> usize {
    let syncs = rig.syncs(id);
    rig.audio(id)
        .iter()
        .filter(|packet| {
            let arrival = packet.at + rng.uniform(0.0, jitter);
            // Syncs carry NTP-era seconds; the packets, seconds on the clock.
            syncs
                .iter()
                .rfind(|(at, _)| *at <= packet.at)
                .is_some_and(|(_, sync)| {
                    let skew = rig.clock.seconds_of(sync.time) - sync_seconds(sync);
                    arrival > heard_by(sync, packet.header.timestamp) + skew
                })
        })
        .count()
}

// covers: P200
#[test]
fn a_receiver_that_reports_nothing_plays_with_room_for_a_jittery_network() {
    let mut rig = Rig::new(StreamConfig {
        latency_frames: LOW,
        input_frames: 88_200,
        host_answers_latency: true,
    });
    rig.host = Some(Host::new(0.0));
    let mut rng = Rng::new(1121);
    // Up to 120 ms one way, as a busy 2.4 GHz network gives.
    let jitter = 0.12;
    let id = rig.add(speaker(1, Codec::Alac));
    rig.stream.set_latency(session_latency_frames([0]), rig.now);
    run_to(&mut rig, 20.0);
    assert!(rig.stream.latency_frames() >= 8_820);
    assert_eq!(dropped_late(&rig, id, jitter, &mut rng), 0, "fresh");

    rig.stream.pause(rig.now);
    rig.run_for(Duration::from_secs(5));
    rig.stream.resume(rig.now);
    rig.answer_flushes();
    run_to(&mut rig, 40.0);
    assert!(rig.stream.latency_frames() >= 8_820, "after a pause");
    assert_eq!(dropped_late(&rig, id, jitter, &mut rng), 0, "after a pause");

    rig.stream.remove_speaker(id);
    rig.drain();
    let again = rig.add(speaker(2, Codec::Alac));
    rig.stream.set_latency(session_latency_frames([0]), rig.now);
    run_to(&mut rig, 60.0);
    assert!(rig.stream.latency_frames() >= 8_820, "after a reconnect");
    assert_eq!(
        dropped_late(&rig, again, jitter, &mut rng),
        0,
        "after a reconnect"
    );
}

// covers: P200
#[test]
fn a_steady_run_of_in_time_resend_requests_steps_up_and_keeps_it() {
    let mut rig = quiet_rig();
    let id = rig.add(speaker(1, Codec::Pcm));
    // 30 requests a minute, each still in time, as the television on a
    // jittery network sent while it cut out.
    let mut second = 1.0;
    while second < 70.0 {
        run_to(&mut rig, second);
        lose_one(&mut rig, id);
        second += 2.0;
    }
    assert_eq!(rig.stream.latency_frames(), 8_820, "{:?}", timelines(&rig));

    rig.stream.pause(rig.now);
    rig.run_for(Duration::from_secs(5));
    rig.stream.resume(rig.now);
    rig.answer_flushes();
    rig.stream.restart(rig.now);
    rig.answer_flushes();
    let resumed = rig.t();
    let mut second = resumed + 1.0;
    while second < resumed + 90.0 {
        run_to(&mut rig, second);
        lose_one(&mut rig, id);
        second += 2.0;
    }
    assert_eq!(
        rig.stream.latency_frames(),
        8_820,
        "kept across a pause and a restart while the requests go on"
    );
}

// covers: P200
#[test]
fn one_burst_of_resend_requests_after_a_resume_never_steps() {
    let mut rig = quiet_rig();
    let id = rig.add(speaker(1, Codec::Pcm));
    run_to(&mut rig, 5.0);
    rig.stream.pause(rig.now);
    rig.run_for(Duration::from_secs(2));
    rig.stream.resume(rig.now);
    rig.answer_flushes();
    rig.run_for(Duration::from_secs(1));
    // A receiver asking again and again for one lost run, 25 times in a
    // second.
    for _ in 0..25 {
        lose_one(&mut rig, id);
        rig.run_for(Duration::from_millis(40));
    }
    // Well inside the clean minute a step up would wait out.
    rig.run_for(Duration::from_secs(30));
    assert_eq!(rig.stream.latency_frames(), LOW, "{:?}", timelines(&rig));
}

// covers: P200
#[test]
fn a_steady_run_of_228_resend_requests_a_minute_steps_up_within_11_seconds() {
    let mut rig = quiet_rig();
    let id = rig.add(speaker(1, Codec::Pcm));
    // 3% loss at about 125 packets a second, as fake_tv asked on 2026-11-21.
    let mut second = 1.0;
    while second < 12.0 {
        run_to(&mut rig, second);
        lose_one(&mut rig, id);
        second += 60.0 / 228.0;
    }
    run_to(&mut rig, 12.5);
    assert_eq!(rig.stream.latency_frames(), 8_820, "{:?}", timelines(&rig));
}

// covers: P200
#[test]
fn a_burst_and_one_request_10_seconds_later_never_step() {
    let mut rig = quiet_rig();
    let id = rig.add(speaker(1, Codec::Pcm));
    run_to(&mut rig, 2.0);
    for _ in 0..25 {
        lose_one(&mut rig, id);
        rig.run_for(Duration::from_millis(40));
    }
    rig.run_for(Duration::from_secs(10));
    lose_one(&mut rig, id);
    rig.run_for(Duration::from_secs(30));
    assert_eq!(rig.stream.latency_frames(), LOW, "{:?}", timelines(&rig));
}

// covers: P200
#[test]
fn a_rate_step_comes_back_with_its_receiver_and_comes_again_after_a_step_down() {
    let mut rig = quiet_rig();
    let id = rig.add(speaker(1, Codec::Pcm));
    let mut second = 1.0;
    while second < 50.0 {
        run_to(&mut rig, second);
        lose_one(&mut rig, id);
        second += 2.0;
    }
    assert_eq!(rig.stream.latency_frames(), 8_820, "stepped up");

    rig.stream.remove_speaker(id);
    rig.drain();
    let again = rig.add(speaker(1, Codec::Pcm));
    rig.stream.set_latency(LOW, rig.now);
    run_to(&mut rig, 55.0);
    assert_eq!(
        rig.stream.latency_frames(),
        8_820,
        "the same receiver joins again at its level"
    );

    // It leaves while another speaker plays: a clean minute steps down.
    rig.add(speaker(2, Codec::Pcm));
    run_to(&mut rig, 60.0);
    rig.stream.remove_speaker(again);
    rig.drain();
    run_to(&mut rig, 130.0);
    assert_eq!(
        rig.stream.latency_frames(),
        LOW,
        "a clean minute steps down"
    );
    let again = rig.add(speaker(1, Codec::Pcm));
    let mut second = 131.0;
    while second < 180.0 {
        run_to(&mut rig, second);
        lose_one(&mut rig, again);
        second += 2.0;
    }
    assert_eq!(rig.stream.latency_frames(), 8_820, "stepped up again");
}

// covers: P200
#[test]
fn a_new_stream_starts_at_the_level_its_receivers_rate_reached() {
    let mut rig = quiet_rig();
    let id = rig.add(speaker(1, Codec::Pcm));
    let mut second = 1.0;
    while second < 50.0 {
        run_to(&mut rig, second);
        lose_one(&mut rig, id);
        second += 2.0;
    }
    assert_eq!(rig.stream.latency_frames(), 8_820, "stepped up");
    rig.stream.remove_speaker(id);
    rig.drain();
    // The host's value over no speaker, as at Streaming off.
    rig.stream.set_latency(LOW, rig.now);
    rig.drain();
    run_to(&mut rig, 55.0);
    let before = timelines(&rig).len();

    // Streaming switched off and on: the host sets its own value only after
    // the receiver joined, as both hosts do.
    let again = rig.add(speaker(1, Codec::Pcm));
    assert_eq!(rig.stream.latency_frames(), 8_820, "starts at the level");
    rig.stream.set_latency(LOW, rig.now);
    rig.drain();
    run_to(&mut rig, 60.0);
    assert!(
        timelines(&rig)[before..].iter().all(|&(ms, _)| ms == 200),
        "{:?}",
        &timelines(&rig)[before..]
    );
    for (_, sync) in rig.syncs(again) {
        assert_eq!(sync.next_rtp.wrapping_sub(sync.rtp_heard), 8_820);
    }
}

// covers: P200
#[test]
fn a_receiver_that_rejoins_next_to_a_sounding_speaker_can_still_step_on_its_rate() {
    let mut rig = quiet_rig();
    let one = rig.add(speaker(1, Codec::Pcm));
    rig.add(speaker(2, Codec::Pcm));
    let mut second = 1.0;
    while second < 50.0 {
        run_to(&mut rig, second);
        lose_one(&mut rig, one);
        second += 2.0;
    }
    assert_eq!(rig.stream.latency_frames(), 8_820, "stepped up");
    rig.stream.remove_speaker(one);
    rig.drain();
    run_to(&mut rig, 130.0);
    assert_eq!(
        rig.stream.latency_frames(),
        LOW,
        "a clean minute steps down"
    );

    let again = rig.add(speaker(1, Codec::Pcm));
    let mut second = 131.0;
    while second < 180.0 {
        run_to(&mut rig, second);
        lose_one(&mut rig, again);
        second += 2.0;
    }
    assert_eq!(
        rig.stream.latency_frames(),
        8_820,
        "its rate steps up again"
    );
}
