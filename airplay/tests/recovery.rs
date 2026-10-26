//! Recovery from the host and the driver falling behind: a capture that
//! pauses and resumes in real time, a driver that stalls for longer than the
//! latency, and an own-encoding speaker flushed back past its join (P85,
//! P101, P112, P121, P143).

mod common;

use std::time::Duration;

use audiosync_airplay::sdp::Codec;
use audiosync_airplay::stream::{SpeakerId, StreamEvent};
use audiosync_airplay::volume::Volume;
use common::rig::{Audio, Host, Rig, decode, host_frame, speaker};

/// The first packet after `after` that carries host frames again.
fn first_with_audio(audio: &[Audio], after: f64) -> usize {
    audio
        .iter()
        .position(|a| a.at > after && decode(Codec::Alac, &a.payload).iter().any(|f| *f != [0, 0]))
        .expect("audio comes back")
}

/// From the first host frame of packet `from` on, every frame is the next
/// host frame: no silence, no gap, no repeat.
fn assert_continuous(audio: &[Audio], from: usize) {
    let mut previous: Option<i16> = None;
    for packet in &audio[from..] {
        for frame in decode(Codec::Alac, &packet.payload) {
            match previous {
                None if frame == [0, 0] => continue,
                None => {}
                Some(value) => {
                    let next = value % 20_000 + 1;
                    assert_eq!(frame, [next, -next], "at {}", packet.at);
                }
            }
            previous = Some(frame[0]);
        }
    }
}

fn arrival_of_block(k: u64, pause: f64) -> f64 {
    // The scripted host's block k is produced at (k + 1) × 10 ms plus the
    // pause, with at most 2 ms of jitter.
    (k + 1) as f64 * 0.010 + pause
}

fn rate_error_ppm(rig: &Rig) -> f64 {
    (rig.stream.rate() / 44_100.0 - 1.0) * 1e6
}

// covers: P85, P101, P112
#[test]
fn a_paused_capture_that_resumes_in_real_time_is_heard_again_at_once() {
    // 30 s of audio, 5 s with nothing (a loopback while the source is
    // silent), then 60 s in real time that never catches up.
    let mut rig = Rig::with_host();
    let mut host = Host::new(0.0).seed(5);
    host.jitter = 0.002;
    host.pauses = vec![(3_000, 5.0)];
    rig.host = Some(host);
    let id = rig.add(speaker(1, Codec::Alac));
    rig.run_for(Duration::from_secs(95));

    let audio = rig.audio(id);
    let resumed = arrival_of_block(3_000, 5.0);
    let back = first_with_audio(&audio, 30.5);
    let packet = &audio[back];
    assert!(
        packet.at - resumed <= packet.margin + 0.008 + 0.002 + 0.003,
        "back {} s after the resume",
        packet.at - resumed
    );
    assert_continuous(&audio, back);
    // The pause is silence the sender inserted; the rate model and M never
    // took the pause for the host's rate or jitter.
    let error = rate_error_ppm(&rig);
    assert!(error.abs() < 50.0, "R̂ off by {error} ppm");
    for &(t, rate) in &rig.rates {
        let ppm = (rate / 44_100.0 - 1.0) * 1e6;
        assert!(ppm.abs() < 100.0, "R̂ off by {ppm} ppm at {t}");
    }
    for &(t, margin) in &rig.margins {
        if t > resumed + 1.0 {
            assert!(margin <= 0.030, "M = {margin} at {t}");
        }
    }
    let silent = rig.stream.counters().silent_frames() as f64 / 44_100.0;
    assert!(silent < 5.0 + 0.040, "{silent} s of silence");
    assert_eq!(rig.stream.counters().input_frames_dropped(), 0);
    let late = rig.stream.speaker_counters(id).expect("speaker");
    assert_eq!(late.late_packets(), 0);
}

// covers: P112
#[test]
fn a_short_pause_comes_back_within_a_block() {
    let mut rig = Rig::with_host();
    let mut host = Host::new(0.0).seed(6);
    host.jitter = 0.002;
    host.pauses = vec![(1_000, 0.3)];
    rig.host = Some(host);
    let id = rig.add(speaker(1, Codec::Alac));
    rig.run_for(Duration::from_secs(50));
    let audio = rig.audio(id);
    let resumed = arrival_of_block(1_000, 0.3);
    let back = first_with_audio(&audio, 10.1);
    let packet = &audio[back];
    assert!(
        packet.at - resumed <= 0.010 + packet.margin + 0.008 + 0.002 + 0.003,
        "back {} s after the resume",
        packet.at - resumed
    );
    assert_continuous(&audio, back);
    assert!(
        rate_error_ppm(&rig).abs() < 50.0,
        "R̂ off by {} ppm",
        rate_error_ppm(&rig)
    );
}

// covers: P84
#[test]
fn repeated_capture_hiccups_raise_the_margin_after_the_first() {
    // A capture thread that hiccups now and then and delivers its backlog
    // at once: the first hiccup leaves a dropout, and M, which takes the
    // late block's residual, covers the ones after it (P84). A residual
    // longer than M can be (a pause) doesn't count.
    for (hiccup, every, seconds) in [(0.035, 300u64, 120u64), (0.030, 1_000, 120)] {
        let mut rig = Rig::with_host();
        let mut host = Host::new(0.0).seed(8);
        host.jitter = 0.002;
        host.stalls = (1..)
            .map(|i| (i * every, hiccup))
            .take_while(|&(block, _)| block < seconds * 100)
            .collect();
        rig.host = Some(host);
        let id = rig.add(speaker(1, Codec::Alac));
        rig.run_for(Duration::from_secs(seconds));
        let counters = rig.stream.counters();
        let silent = counters.silent_frames();
        let one_hiccup = (hiccup * 44_100.0) as u64;
        assert!(
            silent <= 2 * one_hiccup,
            "{hiccup} s every {every} blocks: {silent} silent frames"
        );
        assert_eq!(counters.input_frames_dropped(), silent);
        let highest = rig.margins.iter().map(|&(_, m)| m).fold(0.0, f64::max);
        assert!(
            highest >= 0.010 + hiccup - 0.003,
            "M only reached {highest}"
        );
        assert!(highest <= 0.100);
        // Stalls leave R̂ alone.
        assert!(rate_error_ppm(&rig).abs() < 50.0);
        let audio = rig.audio(id);
        for packet in &audio {
            let n = rig.frame_of(id, packet.header.timestamp) as u64;
            for (offset, frame) in decode(Codec::Alac, &packet.payload).iter().enumerate() {
                if *frame != [0, 0] {
                    assert_eq!(*frame, host_frame(n + offset as u64), "no shift");
                }
            }
        }
    }
}

// covers: P143
#[test]
fn a_driver_stall_longer_than_the_latency_restarts_the_timeline() {
    let mut rig = Rig::with_host();
    let id = rig.add(speaker(1, Codec::Alac));
    rig.run_for(Duration::from_secs(2));

    // A stall shorter than L: the overdue packets go out at once (P84).
    let before = rig.sent.len();
    let soon = rig.now + Duration::from_secs(1);
    rig.jump_to(soon);
    let burst = rig.sent[before..].len();
    assert!(burst > 100, "{burst}");
    assert!(!restarted(&rig));
    rig.run_for(Duration::from_secs(2));

    // A 60 s stall: past their play time, the packets aren't sent; the
    // timeline restarts and every speaker is flushed instead.
    let before = rig.sent.len();
    let later = rig.now + Duration::from_secs(60);
    rig.jump_to(later);
    let burst = rig.sent[before..].len();
    assert!(burst <= 2, "{burst} datagrams in one step");
    assert!(restarted(&rig));
    assert!(rig.events.iter().any(|event| matches!(
        event,
        StreamEvent::Flush { speaker, .. } if *speaker == id
    )));
    rig.answer_flushes();
    let restart_at = rig.t();
    rig.run_for(Duration::from_secs(3));
    let audio = rig.audio(id);
    let back = first_with_audio(&audio, restart_at);
    assert!(audio[back].at - restart_at < 0.1);
    assert_continuous(&audio, back);
    for (_, sync) in rig.syncs(id).iter().filter(|(at, _)| *at >= restart_at) {
        let n = rig.frame_of(id, sync.rtp_heard);
        let stated = rig.clock.seconds_of(sync.time);
        assert!((stated - (rig.c(n) + rig.latency())).abs() < 1e-6);
    }
}

fn restarted(rig: &Rig) -> bool {
    rig.events.iter().any(|event| {
        matches!(
            event,
            StreamEvent::Timeline {
                restarted: true,
                ..
            }
        )
    })
}

// covers: P121
#[test]
fn an_own_encoding_reaches_back_as_far_as_a_delay_can() {
    let mut rig = Rig::with_host();
    rig.run_for(Duration::from_secs(4));
    let mut config = speaker(1, Codec::Alac);
    config.software_volume = Some(Volume::Db(0.0));
    let id: SpeakerId = rig.add(config);
    rig.run_for(Duration::from_millis(500));
    // Raised to 3 s half a second after joining: the flush goes back to
    // packets cut before the speaker joined.
    rig.stream.set_delay(id, Duration::from_secs(3), rig.now);
    rig.run_for(Duration::from_secs(1));
    let (seq, _) = rig
        .events
        .iter()
        .find_map(|event| match event {
            StreamEvent::Flush {
                speaker,
                seq,
                rtptime,
            } if *speaker == id => Some((*seq, *rtptime)),
            _ => None,
        })
        .expect("flush");
    let audio = rig.audio(id);
    let resumed = audio
        .iter()
        .position(|a| a.header.seq == seq)
        .expect("flushed packet");
    for packet in &audio[resumed..] {
        let n = rig.frame_of(id, packet.header.timestamp) as u64;
        assert_eq!(
            decode(Codec::Alac, &packet.payload)[0],
            host_frame(n),
            "its own encoding of packet {}",
            n / 352
        );
    }
    assert_eq!(rig.stream.counters().silent_frames(), 0);
}
