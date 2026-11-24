//! The resend-rate step to a 0.3 s margin, next to the late-request step
//! it leaves unchanged.

mod common;

use std::time::Duration;

use audiosync_airplay::audio::FRAMES_PER_PACKET;
use audiosync_airplay::rtp::{ResendRequest, SyncPacket};
use audiosync_airplay::sdp::Codec;
use audiosync_airplay::stream::{SpeakerId, StreamConfig, StreamEvent};
use common::rig::{Host, Rig, decode, host_frame, speaker};

/// R_max + 4410 with R_max = 0, where these streams start.
const LOW: u32 = 4_410;
/// R_max + 17 640: the rate step's level and the ceiling of every step.
const CEILING: u32 = 17_640;

fn rig_at(latency: u32, quiet: bool) -> Rig {
    let mut rig = Rig::new(StreamConfig {
        latency_frames: latency,
        input_frames: 4 * 44_100,
        ..StreamConfig::default()
    });
    let mut host = Host::new(0.0);
    if quiet {
        // 4 s of programme, the last 0.5 s of each under -60 dBFS.
        host.quiet = Some((176_400, 22_050));
    }
    rig.host = Some(host);
    rig
}

/// The receiver asks again for the newest packet, still in time.
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

/// The receiver asks for a packet whose play time has passed.
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

/// One in-time request every `every` seconds from `from` to `to`.
fn ask(rig: &mut Rig, id: SpeakerId, from: f64, to: f64, every: f64) {
    let mut second = from;
    while second < to {
        run_to(rig, second);
        lose_one(rig, id);
        second += every;
    }
}

/// The Lf, in frames, of every step the speakers took.
fn steps(rig: &Rig) -> Vec<u32> {
    rig.events
        .iter()
        .filter_map(|event| match event {
            StreamEvent::Timeline {
                latency,
                restarted: false,
                ..
            } => Some((latency.as_secs_f64() * 44_100.0).round() as u32),
            _ => None,
        })
        .collect()
}

// covers: P254
#[test]
fn a_resend_rate_steps_lf_to_r_max_plus_17640_in_one_step() {
    for (start, stepped) in [(LOW, CEILING), (8_820, 22_050), (80_000, 88_200)] {
        let mut rig = rig_at(start, false);
        let id = rig.add(speaker(1, Codec::Pcm));
        ask(&mut rig, id, 1.0, 60.0, 2.0);
        assert_eq!(rig.stream.latency_frames(), stepped, "from {start}");
        assert_eq!(steps(&rig), [stepped], "one step from {start}");
        let moved = rig.writer.take_move().expect("the step is announced");
        assert_eq!(moved.frames, i64::from(stepped - start));
    }
}

// covers: P254
#[test]
fn after_a_late_request_step_the_rate_steps_to_the_same_ceiling() {
    let mut rig = rig_at(LOW, true);
    let id = rig.add(speaker(1, Codec::Pcm));
    run_to(&mut rig, 1.0);
    gap(&mut rig, id);
    run_to(&mut rig, 5.0);
    assert_eq!(
        rig.stream.latency_frames(),
        8_820,
        "a late request steps 4410"
    );
    ask(&mut rig, id, 6.0, 60.0, 2.0);
    assert_eq!(rig.stream.latency_frames(), CEILING);
    assert_eq!(steps(&rig), [8_820, CEILING]);
}

// covers: P254
#[test]
fn at_the_ceiling_neither_the_rate_nor_a_late_request_steps_again() {
    let mut rig = rig_at(LOW, false);
    let id = rig.add(speaker(1, Codec::Pcm));
    ask(&mut rig, id, 1.0, 100.0, 2.0);
    gap(&mut rig, id);
    ask(&mut rig, id, 101.0, 240.0, 0.5);
    gap(&mut rig, id);
    run_to(&mut rig, 250.0);
    assert_eq!(rig.stream.latency_frames(), CEILING);
    assert_eq!(steps(&rig), [CEILING]);
}

// covers: P254
#[test]
fn a_stream_that_never_crosses_the_rate_line_keeps_p200s_levels() {
    let mut rig = rig_at(LOW, true);
    let id = rig.add(speaker(1, Codec::Pcm));
    // At most 20 requests in any minute, the late one included.
    ask(&mut rig, id, 1.0, 180.0, 3.1);
    assert_eq!(rig.stream.latency_frames(), LOW);
    assert_eq!(steps(&rig), []);
    run_to(&mut rig, 240.0);
    gap(&mut rig, id);
    ask(&mut rig, id, 244.0, 420.0, 3.5);
    assert_eq!(rig.stream.latency_frames(), 8_820);
    assert_eq!(steps(&rig), [8_820]);
}

/// A sync's time on the sender clock, in seconds.
fn sync_seconds(sync: &SyncPacket) -> f64 {
    (sync.time.0 >> 32) as f64 + (sync.time.0 & 0xffff_ffff) as f64 / 4_294_967_296.0
}

/// When a sync says RTP time `rtp` sounds.
fn heard_by(sync: &SyncPacket, rtp: u32) -> f64 {
    sync_seconds(sync) + f64::from(rtp.wrapping_sub(sync.rtp_heard) as i32) / 44_100.0
}

// covers: P254
#[test]
fn the_rate_step_fades_out_before_its_silence_and_in_after_it() {
    let mut rig = rig_at(LOW, false);
    let id = rig.add(speaker(1, Codec::Pcm));
    ask(&mut rig, id, 1.0, 60.0, 2.0);
    assert_eq!(rig.stream.latency_frames(), CEILING);
    let packets: Vec<(i64, u32, Vec<[i16; 2]>)> = rig
        .audio(id)
        .iter()
        .map(|packet| {
            (
                rig.frame_of(id, packet.header.timestamp),
                packet.header.timestamp,
                decode(Codec::Pcm, &packet.payload),
            )
        })
        .collect();
    let out = packets
        .iter()
        .position(|(_, _, frames)| frames[FRAMES_PER_PACKET - 1] == [0, 0])
        .expect("a packet fades out");
    let back = out
        + 1
        + packets[out + 1..]
            .iter()
            .position(|(_, _, frames)| frames.iter().any(|frame| *frame != [0, 0]))
            .expect("the sound comes back");

    // Out: from the programme at full gain down to 0, never above it.
    let (n, _, faded) = &packets[out];
    let first = host_frame(*n as u64)[0];
    assert!((faded[0][0] - first).abs() <= first.abs() / 300 + 1);
    for (i, frame) in faded.iter().enumerate() {
        assert!(frame[0].abs() <= host_frame((*n + i as i64) as u64)[0].abs());
    }
    // In: from 0 up to the programme at full gain.
    let (m, rtp, rising) = &packets[back];
    assert!(rising[0][0].abs() <= host_frame(*m as u64)[0].abs() / 300 + 1);
    assert_eq!(
        rising[FRAMES_PER_PACKET - 1],
        host_frame((*m + FRAMES_PER_PACKET as i64 - 1) as u64)
    );

    // The speaker pauses 0.3 s between the two fades.
    let lead = |sync: &SyncPacket| sync.next_rtp.wrapping_sub(sync.rtp_heard);
    let syncs = rig.syncs(id);
    let new = &syncs
        .iter()
        .find(|(_, sync)| lead(sync) == CEILING)
        .expect("a sync stating the step")
        .1;
    let old = &syncs
        .iter()
        .rfind(|(_, sync)| lead(sync) == LOW)
        .expect("a sync at 0.1 s")
        .1;
    let pause = heard_by(new, *rtp) - heard_by(old, *rtp);
    assert!(
        (pause - 0.3).abs() < 0.002,
        "the pause is the step: {pause}"
    );
}

// covers: P254
#[test]
fn a_rate_step_holds_as_a_floor_by_address_and_steps_down_after_it_leaves() {
    let mut rig = rig_at(LOW, true);
    let id = rig.add(speaker(1, Codec::Pcm));
    ask(&mut rig, id, 1.0, 60.0, 2.0);
    assert_eq!(rig.stream.latency_frames(), CEILING, "stepped up");
    run_to(&mut rig, 120.0);
    assert_eq!(rig.stream.latency_frames(), CEILING, "held while it plays");

    // Streaming off and on: it joins again at its level.
    rig.stream.remove_speaker(id);
    rig.drain();
    rig.stream.set_latency(LOW, rig.now);
    rig.drain();
    let again = rig.add(speaker(1, Codec::Pcm));
    rig.stream.set_latency(LOW, rig.now);
    rig.drain();
    run_to(&mut rig, 125.0);
    assert_eq!(rig.stream.latency_frames(), CEILING, "its floor");

    // It leaves while another speaker plays: a clean minute steps down.
    rig.add(speaker(2, Codec::Pcm));
    run_to(&mut rig, 130.0);
    rig.stream.remove_speaker(again);
    rig.drain();
    run_to(&mut rig, 179.0);
    assert_eq!(
        rig.stream.latency_frames(),
        CEILING,
        "not before 60 s clean"
    );
    run_to(&mut rig, 200.0);
    assert_eq!(rig.stream.latency_frames(), LOW, "back to P200's level");
}

/// The receiver asks for a packet past the midpoint between its send and
/// its play time, still before it plays.
fn ask_tight(rig: &mut Rig, id: SpeakerId) {
    let now = rig.t();
    let latency = rig.latency();
    let packet = 352.0 / 44_100.0;
    let tight = rig
        .audio(id)
        .into_iter()
        .rfind(|sent| {
            let play = sent.at + latency - sent.margin - packet;
            now >= (sent.at + play) / 2.0 + 0.005 && now < play - 0.005
        })
        .expect("a packet past its midpoint");
    let request = ResendRequest {
        seq: 1,
        first: tight.header.seq,
        count: 1,
    };
    rig.stream
        .handle_control_packet(id, &request.encode(), rig.now);
    rig.drain();
}

// covers: P260
#[test]
fn three_requests_too_tight_to_answer_in_a_minute_step_to_the_rate_ceiling() {
    let mut rig = rig_at(LOW, false);
    let id = rig.add(speaker(1, Codec::Pcm));
    for at in [1.0, 16.0] {
        run_to(&mut rig, at);
        ask_tight(&mut rig, id);
    }
    run_to(&mut rig, 30.0);
    assert_eq!(rig.stream.latency_frames(), LOW, "two are not starved");
    ask_tight(&mut rig, id);
    run_to(&mut rig, 31.0);
    assert_eq!(rig.stream.latency_frames(), CEILING);
    assert_eq!(steps(&rig), [CEILING], "one step");

    // Three in one second are one burst, not a starved receiver.
    let mut rig = rig_at(LOW, false);
    let id = rig.add(speaker(1, Codec::Pcm));
    run_to(&mut rig, 1.0);
    for _ in 0..3 {
        ask_tight(&mut rig, id);
    }
    run_to(&mut rig, 10.0);
    assert_eq!(rig.stream.latency_frames(), LOW, "one burst");
}

/// Runs until Lf leaves `from` or `end` passes, by 10 ms: when it left.
fn until_lf_leaves(rig: &mut Rig, from: u32, end: f64) -> Option<f64> {
    while rig.t() < end {
        let next = rig.t() + 0.01;
        run_to(rig, next);
        if rig.stream.latency_frames() != from {
            return Some(rig.t());
        }
    }
    None
}

/// One tight request at each of `at`.
fn starve(rig: &mut Rig, id: SpeakerId, at: &[f64]) {
    for &second in at {
        run_to(rig, second);
        ask_tight(rig, id);
    }
}

// covers: P260
#[test]
fn a_starved_step_is_a_floor_that_a_try_brings_down_and_starving_again_fails_the_try() {
    let mut rig = rig_at(LOW, true);
    rig.stream.set_step_down_times(
        Duration::from_secs(30),
        Duration::from_secs(120),
        Duration::from_secs(20),
    );
    let id = rig.add(speaker(1, Codec::Pcm));
    starve(&mut rig, id, &[1.0, 16.0, 31.0]);
    run_to(&mut rig, 32.0);
    assert_eq!(rig.stream.latency_frames(), CEILING, "starved");

    // Streaming off and on: it joins again at its floor.
    rig.stream.remove_speaker(id);
    rig.drain();
    rig.stream.set_latency(LOW, rig.now);
    rig.drain();
    let id = rig.add(speaker(1, Codec::Pcm));
    rig.stream.set_latency(LOW, rig.now);
    rig.drain();
    run_to(&mut rig, 34.0);
    assert_eq!(rig.stream.latency_frames(), CEILING, "its floor");

    // A try brings it down after the wait.
    let at = until_lf_leaves(&mut rig, CEILING, 140.0).expect("a try");
    assert_eq!(rig.stream.latency_frames(), LOW);

    // Starved again inside the watch: the try fails and Lf goes back.
    starve(&mut rig, id, &[at + 1.0, at + 2.0]);
    run_to(&mut rig, at + 3.0);
    assert_eq!(rig.stream.latency_frames(), LOW, "two are not starved");
    starve(&mut rig, id, &[at + 3.5]);
    let back = until_lf_leaves(&mut rig, LOW, at + 5.0).expect("back up");
    assert!(back - at < 5.0);
    assert_eq!(rig.stream.latency_frames(), CEILING);
    assert!(
        rig.events
            .iter()
            .any(|event| matches!(event, StreamEvent::MarginTryFailed { .. })),
        "the try failed"
    );
}

// covers: P254
// covers: P260
#[test]
fn a_rate_step_while_another_step_is_on_its_way_fades_in_as_that_one_applies() {
    let mut rig = rig_at(LOW, true);
    let id = rig.add(speaker(1, Codec::Pcm));
    // 19 in-time requests, the line not crossed.
    ask(&mut rig, id, 2.0, 39.0, 2.0);
    run_to(&mut rig, 40.0);
    gap(&mut rig, id);
    rig.writer.take_move();
    // The late request's step waits for a quiet run, then is on its way.
    while rig.writer.take_move().is_none() {
        assert!(rig.t() < 50.0, "the late-request step starts");
        let next = rig.t() + 0.01;
        run_to(&mut rig, next);
    }
    assert_eq!(rig.stream.latency_frames(), LOW, "on its way");
    // The 21st request crosses the line.
    lose_one(&mut rig, id);
    let pending = rig.t();
    let first = until_lf_leaves(&mut rig, LOW, pending + 1.0).expect("the pending step");
    assert_eq!(rig.stream.latency_frames(), 8_820);
    let second = until_lf_leaves(&mut rig, 8_820, first + 3.0).expect("the rate step");
    assert!(
        second - first < 1.0,
        "the rate step after {} s",
        second - first
    );
    assert_eq!(rig.stream.latency_frames(), CEILING);
}

// covers: P260
#[test]
fn a_receiver_starved_while_another_ones_step_down_is_on_its_way_still_steps_up() {
    let mut rig = rig_at(LOW, true);
    rig.stream.set_step_down_times(
        Duration::from_secs(30),
        Duration::from_secs(120),
        Duration::from_secs(20),
    );
    let a = rig.add(speaker(1, Codec::Pcm));
    let b = rig.add(speaker(2, Codec::Pcm));
    starve(&mut rig, a, &[1.0, 16.0, 31.0]);
    run_to(&mut rig, 32.0);
    assert_eq!(rig.stream.latency_frames(), CEILING, "a starved");
    starve(&mut rig, b, &[33.0, 34.0]);
    rig.writer.take_move();
    // A's try starts its step down; it is on its way.
    while rig.writer.take_move().is_none() {
        assert!(rig.t() < 90.0, "a's try starts");
        let next = rig.t() + 0.01;
        run_to(&mut rig, next);
    }
    assert_eq!(rig.stream.latency_frames(), CEILING, "on its way");
    // B's third tight request: B is starved.
    ask_tight(&mut rig, b);
    let starved = rig.t();
    let down = until_lf_leaves(&mut rig, CEILING, starved + 1.0).expect("a's step down");
    assert_eq!(rig.stream.latency_frames(), LOW);
    let up = until_lf_leaves(&mut rig, LOW, down + 3.0).expect("b's step up");
    assert!(up - down < 3.0);
    assert_eq!(rig.stream.latency_frames(), CEILING);
}

/// The receiver asks for the packet whose send-to-play midpoint comes
/// next, just before that midpoint.
fn ask_before_midpoint(rig: &mut Rig, id: SpeakerId) {
    let now = rig.t();
    let latency = rig.latency();
    let packet = 352.0 / 44_100.0;
    let early = rig
        .audio(id)
        .into_iter()
        .find(|sent| {
            let play = sent.at + latency - sent.margin - packet;
            now < (sent.at + play) / 2.0 - 0.005
        })
        .expect("a packet before its midpoint");
    let request = ResendRequest {
        seq: 1,
        first: early.header.seq,
        count: 1,
    };
    rig.stream
        .handle_control_packet(id, &request.encode(), rig.now);
    rig.drain();
}

// covers: P260
#[test]
fn requests_just_before_the_midpoint_or_past_the_play_time_are_not_tight() {
    let mut rig = rig_at(LOW, false);
    let id = rig.add(speaker(1, Codec::Pcm));
    for at in [1.0, 2.0, 3.0] {
        run_to(&mut rig, at);
        ask_before_midpoint(&mut rig, id);
    }
    run_to(&mut rig, 10.0);
    assert_eq!(rig.stream.latency_frames(), LOW, "in time");
    assert!(steps(&rig).is_empty());

    // Late requests are P200's, never P260's.
    let mut rig = rig_at(LOW, false);
    let id = rig.add(speaker(1, Codec::Pcm));
    for at in [2.0, 3.0, 4.0] {
        run_to(&mut rig, at);
        gap(&mut rig, id);
    }
    // P200's step waits for a quiet run, which this programme never has.
    run_to(&mut rig, 10.0);
    assert_eq!(rig.stream.latency_frames(), LOW, "late");
    assert!(steps(&rig).is_empty());
}
