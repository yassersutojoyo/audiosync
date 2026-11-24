//! A margin a receiver stepped up tries to come back down after a wait
//! under the resend line that doubles on each failed try, only where
//! nothing is heard, and goes straight back up when the try fails.

mod common;

use std::net::{IpAddr, Ipv4Addr};
use std::time::Duration;

use audiosync_airplay::rtp::ResendRequest;
use audiosync_airplay::sdp::Codec;
use audiosync_airplay::stream::{SpeakerId, StreamConfig, StreamEvent};
use common::rig::{Host, Rig, speaker};

/// R_max + 4410 with R_max = 0: the start level of these streams.
const LOW: u32 = 4_410;
/// R_max + 17 640: the rate floor.
const CEILING: u32 = 17_640;
/// The test's shortened first wait, its ceiling and the watch, in seconds.
const W: f64 = 30.0;
const CAP: f64 = 120.0;
const WATCH: f64 = 20.0;
/// A try comes this long after its wait at most: the wait restarts at a
/// minute report over 20, and a quiet run comes every 4 s.
const SLACK: f64 = 65.0;
/// The host's programme: 4 s periods whose last 0.5 s stays under -60 dBFS.
const PERIOD: u64 = 176_400;
const PAUSE: u64 = 22_050;

fn rig(quiet: bool) -> Rig {
    let mut rig = Rig::new(StreamConfig {
        latency_frames: LOW,
        input_frames: 4 * 44_100,
        ..StreamConfig::default()
    });
    let mut host = Host::new(0.0);
    if quiet {
        host.quiet = Some((PERIOD, PAUSE));
    }
    rig.host = Some(host);
    rig.stream.set_step_down_times(
        Duration::from_secs_f64(W),
        Duration::from_secs_f64(CAP),
        Duration::from_secs_f64(WATCH),
    );
    rig
}

fn ip(k: u8) -> IpAddr {
    IpAddr::V4(Ipv4Addr::new(192, 0, 2, 20 + k))
}

fn run_to(rig: &mut Rig, seconds: f64) {
    let end = rig.instant(seconds);
    rig.run_for(end.saturating_duration_since(rig.now));
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

/// Every receiver in `ids` asks twice a second for 15 s from `from`: the
/// rate steps them all to the rate floor, and the line is last crossed at
/// `from + 15`.
fn step_up(rig: &mut Rig, ids: &[SpeakerId], from: f64) -> f64 {
    let mut second = from;
    while second < from + 15.0 {
        run_to(rig, second);
        for &id in ids {
            lose_one(rig, id);
        }
        second += 0.5;
    }
    run_to(rig, from + 15.5);
    assert_eq!(rig.stream.latency_frames(), CEILING, "stepped up");
    from + 15.0
}

/// Runs until Lf leaves `from` or `end` passes, by 50 ms: when it left.
fn until_lf_leaves(rig: &mut Rig, from: u32, end: f64) -> Option<f64> {
    while rig.t() < end {
        let next = rig.t() + 0.05;
        run_to(rig, next);
        if rig.stream.latency_frames() != from {
            return Some(rig.t());
        }
    }
    None
}

/// The Lf of every step the speakers took since event `from`.
fn steps_since(rig: &Rig, from: usize) -> Vec<u32> {
    rig.events[from..]
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

/// The waits in force at each try, by receiver.
fn tries(rig: &Rig) -> Vec<(IpAddr, f64)> {
    rig.events
        .iter()
        .filter_map(|event| match event {
            StreamEvent::MarginTry { receiver, wait, .. } => Some((*receiver, wait.as_secs_f64())),
            _ => None,
        })
        .collect()
}

fn failed(rig: &Rig) -> Vec<(IpAddr, u32, f64)> {
    rig.events
        .iter()
        .filter_map(|event| match event {
            StreamEvent::MarginTryFailed {
                receiver,
                back_to,
                next_wait,
                ..
            } => Some((*receiver, *back_to, next_wait.as_secs_f64())),
            _ => None,
        })
        .collect()
}

fn held(rig: &Rig) -> Vec<(IpAddr, f64)> {
    rig.events
        .iter()
        .filter_map(|event| match event {
            StreamEvent::MarginTryHeld { receiver, wait } => Some((*receiver, wait.as_secs_f64())),
            _ => None,
        })
        .collect()
}

/// The host frame at input position `f` is in a quiet pause.
fn quiet(f: u64) -> bool {
    f % PERIOD >= PERIOD - PAUSE
}

// covers: P255
// covers: P256
// covers: P257
#[test]
fn a_try_after_the_wait_falls_in_a_quiet_run_that_holds_the_whole_drop() {
    let mut rig = rig(true);
    let id = rig.add(speaker(1, Codec::Pcm));
    let crossed = step_up(&mut rig, &[id], 1.0);
    rig.writer.take_move();
    assert_eq!(
        until_lf_leaves(&mut rig, CEILING, crossed + W - 0.5),
        None,
        "no try before W under the line"
    );
    let at = until_lf_leaves(&mut rig, CEILING, crossed + W + 5.0).expect("a try");
    assert_eq!(
        rig.stream.latency_frames(),
        LOW,
        "to the start level in one step"
    );
    assert_eq!(tries(&rig), [(ip(1), W)]);

    // The 0.3 s skipped, the 0.1 s spare before it and the 5 packets after
    // the step's packet were all cut quiet: 57 packets.
    let moved = rig.writer.take_move().expect("the try is announced");
    assert_eq!(moved.frames, -i64::from(CEILING - LOW));
    let end = moved.position + 352;
    assert!(
        (end - u64::from(CEILING - LOW) - 4_410..end + 5 * 352).all(quiet),
        "the drop at {at} s, position {} lies in measured quiet",
        moved.position
    );
}

// covers: P256
#[test]
fn a_crossing_of_the_line_starts_the_wait_again() {
    let mut rig = rig(true);
    let id = rig.add(speaker(1, Codec::Pcm));
    let crossed = step_up(&mut rig, &[id], 1.0);
    run_to(&mut rig, crossed + W / 2.0);
    // One more request with 30 in the last minute: over the line.
    lose_one(&mut rig, id);
    let again = rig.t();
    assert_eq!(
        until_lf_leaves(&mut rig, CEILING, again + W - 0.5),
        None,
        "the wait counts afresh from {again}"
    );
    // The minute that held the step up reports over 20 and crosses too.
    assert!(until_lf_leaves(&mut rig, CEILING, again + W + SLACK).is_some());
}

// covers: P257
#[test]
fn continuous_music_gets_no_try_while_it_plays() {
    let mut rig = rig(false);
    let id = rig.add(speaker(1, Codec::Pcm));
    let crossed = step_up(&mut rig, &[id], 1.0);
    assert_eq!(until_lf_leaves(&mut rig, CEILING, crossed + 3.0 * W), None);
    assert_eq!(tries(&rig), []);
}

// covers: P257
#[test]
fn a_due_try_falls_at_a_resume_from_a_pause() {
    let mut rig = rig(false);
    let id = rig.add(speaker(1, Codec::Pcm));
    let crossed = step_up(&mut rig, &[id], 1.0);
    run_to(&mut rig, crossed + W + 2.0);
    rig.stream.pause(rig.now);
    rig.drain();
    rig.answer_flushes();
    run_to(&mut rig, crossed + W + 4.0);
    assert_eq!(
        rig.stream.latency_frames(),
        CEILING,
        "no try inside the pause"
    );
    rig.stream.resume(rig.now);
    rig.drain();
    assert_eq!(rig.stream.latency_frames(), LOW);
    assert_eq!(tries(&rig), [(ip(1), W)]);
}

// covers: P257
#[test]
fn a_due_try_falls_at_a_timeline_restart() {
    let mut rig = rig(false);
    let id = rig.add(speaker(1, Codec::Pcm));
    let crossed = step_up(&mut rig, &[id], 1.0);
    run_to(&mut rig, crossed + W + 2.0);
    rig.stream.restart(rig.now);
    rig.drain();
    assert_eq!(rig.stream.latency_frames(), LOW);
    assert_eq!(tries(&rig), [(ip(1), W)]);
}

// covers: P257
#[test]
fn a_due_try_falls_at_a_stream_before_its_first_sound() {
    let mut rig = rig(false);
    let id = rig.add(speaker(1, Codec::Pcm));
    let crossed = step_up(&mut rig, &[id], 1.0);
    run_to(&mut rig, crossed + W + 2.0);
    rig.stream.remove_speaker(id);
    rig.drain();
    rig.stream.set_latency(LOW, rig.now);
    rig.drain();
    run_to(&mut rig, crossed + W + 5.0);
    rig.add(speaker(1, Codec::Pcm));
    rig.stream.set_latency(LOW, rig.now);
    rig.drain();
    assert_eq!(
        rig.stream.latency_frames(),
        LOW,
        "it starts at the start level"
    );
    assert_eq!(tries(&rig), [(ip(1), W)]);
}

// covers: P256
// covers: P258
#[test]
fn a_failed_try_returns_at_once_to_the_level_it_left_and_doubles_the_wait() {
    let mut rig = rig(true);
    let id = rig.add(speaker(1, Codec::Pcm));
    step_up(&mut rig, &[id], 1.0);

    // Try 1 fails on a late request.
    let at = until_lf_leaves(&mut rig, CEILING, 60.0).expect("try 1");
    run_to(&mut rig, at + 2.0);
    let mark = rig.events.len();
    gap(&mut rig, id);
    let back = until_lf_leaves(&mut rig, LOW, at + 3.0).expect("back up at once");
    assert!(back - at < 2.6, "the fast return took {} s", back - at);
    assert_eq!(
        steps_since(&rig, mark),
        [CEILING],
        "straight to the level left"
    );

    // Try 2, a doubled wait later, fails on 6 requests in 3 seconds; 5 do
    // not.
    let at = until_lf_leaves(&mut rig, CEILING, back + 2.0 * W + SLACK).expect("try 2");
    assert!(at - back >= 2.0 * W - 0.5, "try 2 after {} s", at - back);
    for (k, offset) in [1.0, 1.2, 2.0, 2.2, 3.0].into_iter().enumerate() {
        run_to(&mut rig, at + offset);
        lose_one(&mut rig, id);
        assert_eq!(rig.stream.latency_frames(), LOW, "request {k}");
    }
    run_to(&mut rig, at + 3.2);
    lose_one(&mut rig, id);
    let back = until_lf_leaves(&mut rig, LOW, at + 4.0).expect("back up on the sixth");

    // Tries 3 and 4 wait 120 s, the ceiling; both fail.
    let mut back = back;
    for _ in 0..2 {
        let at = until_lf_leaves(&mut rig, CEILING, back + CAP + SLACK).expect("a try");
        assert!(at - back >= CAP - 0.5, "a try after {} s", at - back);
        run_to(&mut rig, at + 2.0);
        gap(&mut rig, id);
        back = until_lf_leaves(&mut rig, LOW, at + 3.0).expect("back up");
    }
    assert_eq!(
        tries(&rig),
        [(ip(1), W), (ip(1), 2.0 * W), (ip(1), CAP), (ip(1), CAP)]
    );
    assert_eq!(
        failed(&rig),
        [
            (ip(1), CEILING, 2.0 * W),
            (ip(1), CEILING, CAP),
            (ip(1), CEILING, CAP),
            (ip(1), CEILING, CAP)
        ]
    );

    // Try 5 holds: the wait is back to W for the next step up.
    let at = until_lf_leaves(&mut rig, CEILING, back + CAP + SLACK).expect("try 5");
    run_to(&mut rig, at + WATCH + 1.0);
    assert_eq!(rig.stream.latency_frames(), LOW);
    assert_eq!(held(&rig), [(ip(1), W)]);
    let from = rig.t() + 1.0;
    let crossed = step_up(&mut rig, &[id], from);
    until_lf_leaves(&mut rig, CEILING, crossed + W + SLACK).expect("try 6");
    assert_eq!(tries(&rig).last(), Some(&(ip(1), W)));
}

// covers: P258
#[test]
fn of_two_receivers_tried_together_only_the_failing_one_goes_back_up() {
    let mut rig = rig(true);
    let one = rig.add(speaker(1, Codec::Pcm));
    let two = rig.add(speaker(2, Codec::Pcm));
    let crossed = step_up(&mut rig, &[one, two], 1.0);
    let at = until_lf_leaves(&mut rig, CEILING, crossed + W + 5.0).expect("a try");
    assert_eq!(rig.stream.latency_frames(), LOW);
    assert_eq!(
        tries(&rig),
        [(ip(1), W), (ip(2), W)],
        "both at one try point"
    );

    run_to(&mut rig, at + 2.0);
    gap(&mut rig, one);
    run_to(&mut rig, at + WATCH + 1.0);
    assert_eq!(
        rig.stream.latency_frames(),
        CEILING,
        "the failed one's floor"
    );
    assert_eq!(failed(&rig), [(ip(1), CEILING, 2.0 * W)]);
    assert_eq!(held(&rig), [(ip(2), W)]);

    // Once the failed one leaves, the held one keeps the start level.
    rig.stream.remove_speaker(one);
    rig.drain();
    run_to(&mut rig, at + WATCH + 70.0);
    assert_eq!(rig.stream.latency_frames(), LOW);
}

// covers: P256
#[test]
fn a_receiver_that_never_falls_under_the_line_never_tries() {
    let mut rig = rig(true);
    let id = rig.add(speaker(1, Codec::Pcm));
    let crossed = step_up(&mut rig, &[id], 1.0);
    let mut second = crossed + 1.0;
    while second < crossed + 3.0 * W {
        run_to(&mut rig, second);
        lose_one(&mut rig, id);
        second += 1.0;
    }
    run_to(&mut rig, crossed + 3.0 * W);
    assert_eq!(rig.stream.latency_frames(), CEILING);
    assert_eq!(tries(&rig), []);
}

// covers: P258
#[test]
fn the_three_log_lines_name_the_receiver_the_levels_and_the_wait() {
    let minutes = |m: u64| Duration::from_secs(60 * m);
    let lines: Vec<String> = [
        StreamEvent::MarginTry {
            receiver: ip(1),
            from: CEILING,
            to: LOW,
            wait: minutes(10),
        },
        StreamEvent::MarginTryFailed {
            receiver: ip(1),
            back_to: CEILING,
            after: Duration::from_secs(14),
            requests: 7,
            late: 1,
            next_wait: minutes(20),
        },
        StreamEvent::MarginTryHeld {
            receiver: ip(1),
            wait: minutes(10),
        },
    ]
    .iter()
    .filter_map(StreamEvent::margin_line)
    .collect();
    assert_eq!(
        lines,
        [
            "margin try: 192.0.2.21 400 ms to 100 ms after 10 min under the line",
            "margin try failed: 192.0.2.21 back to 400 ms after 14 s, 7 requests, 1 late; next wait 20 min",
            "margin try held: 192.0.2.21 wait back to 10 min",
        ]
    );
}
