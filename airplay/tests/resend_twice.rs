//! Every resend answered twice (P252): the stream against fake receivers on
//! a fake clock.

mod common;

use std::time::Duration;

use audiosync_airplay::rtp::{RESEND_PREFIX_BYTES, ResendRequest};
use audiosync_airplay::sdp::Codec;
use audiosync_airplay::stream::{
    LocalSocket, MAX_SECOND_COPIES, PacketKind, SECOND_COPY_SPACING, SpeakerId, StreamEvent,
};
use common::rig::{Rig, destinations, speaker};

const D: f64 = 0.020;

fn playing() -> (Rig, SpeakerId) {
    let mut rig = Rig::with_host();
    let id = rig.add(speaker(1, Codec::Alac));
    rig.run_for(Duration::from_secs(3));
    (rig, id)
}

fn last_seq(rig: &Rig, id: SpeakerId) -> u16 {
    rig.audio(id).last().expect("audio").header.seq
}

fn ask(rig: &mut Rig, id: SpeakerId, first: u16, count: u16) {
    let request = ResendRequest {
        seq: 0,
        first,
        count,
    }
    .encode();
    rig.stream.handle_control_packet(id, &request, rig.now);
    rig.drain();
}

/// The resend datagrams to speaker `k` from `start` on: when each left, its
/// sequence number and its bytes.
fn resends(rig: &Rig, id: SpeakerId, k: u8, start: usize) -> Vec<(f64, u16, Vec<u8>)> {
    rig.sent[start..]
        .iter()
        .filter(|s| s.transmit.speaker == id && s.transmit.kind == PacketKind::Resend)
        .map(|s| {
            assert_eq!(s.transmit.from, LocalSocket::Control);
            assert_eq!(s.transmit.to, destinations(k).control);
            let bytes = &s.transmit.bytes;
            (
                s.at,
                u16::from_be_bytes([bytes[2], bytes[3]]),
                bytes.clone(),
            )
        })
        .collect()
}

/// P_s of a sent packet's first frame, for a speaker without delay (P96).
fn play_time(rig: &Rig, id: SpeakerId, seq: u16) -> f64 {
    let audio = rig.audio(id);
    let packet = audio.iter().find(|a| a.header.seq == seq).expect("sent");
    rig.c(rig.frame_of(id, packet.header.timestamp)) + rig.latency()
}

// covers: P252
#[test]
fn every_answered_request_goes_twice_d_apart_to_the_destination_fixed_at_setup() {
    assert_eq!(SECOND_COPY_SPACING, Duration::from_millis(20));
    let mut rig = Rig::with_host();
    let id = rig.add(speaker(1, Codec::Alac));
    let other = rig.add(speaker(2, Codec::Alac));
    rig.run_for(Duration::from_secs(3));
    let last = last_seq(&rig, id);
    let first = last.wrapping_sub(2);
    assert!(play_time(&rig, id, first) > rig.t() + 1.0);
    let asked = rig.t();
    let start = rig.sent.len();
    ask(&mut rig, id, first, 3);
    let other_last = last_seq(&rig, other);
    ask(&mut rig, other, other_last, 1);
    rig.run_for(Duration::from_millis(50));

    let got = resends(&rig, id, 1, start);
    assert_eq!(got.len(), 6, "three packets, each twice");
    // The k first copies at once, as P87 sends them; each second copy D
    // after its own first, same bytes, same order.
    for (index, (at, seq, bytes)) in got[..3].iter().enumerate() {
        assert!((at - asked).abs() < 1e-6);
        assert_eq!(*seq, first.wrapping_add(index as u16));
        let (later, again, copy) = &got[index + 3];
        assert!((later - (asked + D)).abs() < 1e-5, "{}", later - asked);
        assert_eq!(again, seq);
        assert_eq!(copy, bytes);
        assert_eq!(&copy[..2], &[0x80, 0xD6]);
        assert!(copy.len() > RESEND_PREFIX_BYTES);
    }
    assert_eq!(resends(&rig, other, 2, start).len(), 2);

    let counters = rig.stream.speaker_counters(id).expect("counters");
    assert_eq!(counters.resend_requests(), 1);
    assert_eq!(counters.packets_resent(), 3);
    assert_eq!(counters.second_copies_sent(), 3);
    assert_eq!(counters.second_copies_skipped(), 0);
}

// covers: P252
#[test]
fn no_second_copy_once_the_packet_would_be_played() {
    let (mut rig, id) = playing();
    let now = rig.t();
    let audio = rig.audio(id);
    let close = audio
        .iter()
        .map(|a| a.header.seq)
        .find(|&seq| {
            let play = play_time(&rig, id, seq);
            play > now + 0.004 && play < now + D - 0.004
        })
        .expect("a packet played within D");
    let gone = last_seq(&rig, id).wrapping_sub(300);
    assert!(play_time(&rig, id, gone) < now);
    let start = rig.sent.len();
    ask(&mut rig, id, close, 1);
    ask(&mut rig, id, gone, 1);
    rig.run_for(Duration::from_millis(50));

    assert_eq!(resends(&rig, id, 1, start).len(), 2, "first copies only");
    let counters = rig.stream.speaker_counters(id).expect("counters");
    assert_eq!(counters.second_copies_sent(), 0);
    assert_eq!(counters.second_copies_skipped(), 2);
}

// covers: P252
#[test]
fn the_cap_counts_requests_and_a_later_request_replaces_a_waiting_copy() {
    let (mut rig, id) = playing();
    let seq = last_seq(&rig, id).wrapping_sub(10);
    let mut rounds = Vec::new();
    for _ in 0..4 {
        let start = rig.sent.len();
        ask(&mut rig, id, seq, 1);
        rig.run_for(Duration::from_millis(30));
        rounds.push(resends(&rig, id, 1, start).len());
    }
    assert_eq!(rounds, [2, 2, 2, 0], "at most 6 copies in reply");

    let again = last_seq(&rig, id).wrapping_sub(20);
    let start = rig.sent.len();
    let asked = rig.t();
    ask(&mut rig, id, again, 1);
    rig.run_for(Duration::from_millis(10));
    ask(&mut rig, id, again, 1);
    rig.run_for(Duration::from_millis(50));
    let at: Vec<f64> = resends(&rig, id, 1, start)
        .iter()
        .map(|(at, _, _)| at - asked)
        .collect();
    assert_eq!(at.len(), 3, "{at:?}");
    assert!((at[2] - 0.010 - D).abs() < 1e-5, "{at:?}");
}

// covers: P252
#[test]
fn at_most_128_second_copies_wait() {
    assert_eq!(MAX_SECOND_COPIES, 128);
    let (mut rig, id) = playing();
    let last = last_seq(&rig, id);
    let start = rig.sent.len();
    ask(&mut rig, id, last.wrapping_sub(140), 128);
    ask(&mut rig, id, last.wrapping_sub(5), 1);
    rig.run_for(Duration::from_millis(50));

    assert_eq!(resends(&rig, id, 1, start).len(), 129 + 128);
    let counters = rig.stream.speaker_counters(id).expect("counters");
    assert_eq!(counters.second_copies_sent(), 128);
    assert_eq!(counters.second_copies_skipped(), 1);
}

/// Resend datagrams for `seq` from `start` on.
fn copies_of(rig: &Rig, id: SpeakerId, seq: u16, start: usize) -> usize {
    resends(rig, id, 1, start)
        .iter()
        .filter(|(_, s, _)| *s == seq)
        .count()
}

// covers: P252
#[test]
fn a_flush_restart_step_or_session_end_drops_waiting_copies() {
    // The FLUSH of a pause.
    let (mut rig, id) = playing();
    let seq = last_seq(&rig, id).wrapping_sub(2);
    let start = rig.sent.len();
    ask(&mut rig, id, seq, 1);
    rig.run_for(Duration::from_millis(5));
    assert!(rig.stream.pause(rig.now));
    rig.run_for(Duration::from_millis(50));
    assert_eq!(copies_of(&rig, id, seq, start), 1, "pause");

    // The FLUSH of a delay change, 250 ms after it (P100).
    let (mut rig, id) = playing();
    assert!(
        rig.stream
            .set_delay(id, Duration::from_millis(300), rig.now)
    );
    rig.run_for(Duration::from_millis(240));
    let seq = last_seq(&rig, id).wrapping_sub(2);
    let start = rig.sent.len();
    let events = rig.events.len();
    ask(&mut rig, id, seq, 1);
    rig.run_for(Duration::from_millis(50));
    assert!(
        rig.events[events..]
            .iter()
            .any(|e| matches!(e, StreamEvent::Flush { .. }))
    );
    assert_eq!(copies_of(&rig, id, seq, start), 1, "delay flush");

    // A timeline restart (P143).
    let (mut rig, id) = playing();
    let seq = last_seq(&rig, id).wrapping_sub(2);
    let start = rig.sent.len();
    ask(&mut rig, id, seq, 1);
    rig.run_for(Duration::from_millis(5));
    rig.stream.restart(rig.now);
    rig.drain();
    rig.run_for(Duration::from_millis(50));
    assert_eq!(copies_of(&rig, id, seq, start), 1, "restart");

    // A step of Lf (P200): a request each 10 ms until the step applies;
    // the last one's second copy was still waiting then.
    let (mut rig, id) = playing();
    let steps = |rig: &Rig| {
        rig.events
            .iter()
            .filter(|e| {
                matches!(
                    e,
                    StreamEvent::Timeline {
                        restarted: false,
                        ..
                    }
                )
            })
            .count()
    };
    let before = steps(&rig);
    rig.stream.step_up(rig.now);
    rig.drain();
    let start = rig.sent.len();
    let mut asked = Vec::new();
    while steps(&rig) == before {
        assert!(asked.len() < 500, "the step applied");
        let seq = last_seq(&rig, id);
        ask(&mut rig, id, seq, 1);
        asked.push(seq);
        rig.run_for(Duration::from_millis(10));
    }
    rig.run_for(Duration::from_millis(50));
    assert_eq!(copies_of(&rig, id, asked[0], start), 2);
    assert_eq!(copies_of(&rig, id, *asked.last().expect("asked"), start), 1);

    // The end of the session.
    let (mut rig, id) = playing();
    let seq = last_seq(&rig, id).wrapping_sub(2);
    let start = rig.sent.len();
    ask(&mut rig, id, seq, 1);
    assert!(rig.stream.remove_speaker(id));
    rig.run_for(Duration::from_millis(50));
    assert_eq!(copies_of(&rig, id, seq, start), 1, "end");
}
