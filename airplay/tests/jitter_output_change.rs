//! A television that reports no latency, behind a jittery 2.4 GHz network
//! (ping 20 to 150 ms, now and then about 300 ms), when the host changes
//! its output or source mid-song after the jitter stepped the margin up:
//! the margin stays, the television asks for nothing, and it plays every
//! packet in time from the first one after the change (P143, P200).

mod common;

use audiosync_airplay::latency::session_latency_frames;
use audiosync_airplay::sdp::Codec;
use audiosync_airplay::stream::{StreamConfig, StreamEvent};
use common::jitter::JitteryTv;
use common::rig::{Host, Rig, decode, speaker};

const LOW: u32 = 4_410;
/// A no-report receiver's 0.2 s after its one step up (P200).
const STEPPED: u32 = 13_230;
/// The change comes 20 s into the clean minute that follows the step, and
/// the watch ends before that minute does.
const CHANGE: f64 = 25.0;
const WATCH_END: f64 = 60.0;
/// How long the host delivers nothing while its capture is rebuilt.
const HOLE: f64 = 1.0;

/// A playing television whose jitter stepped the margin up.
fn stepped_tv() -> (Rig, JitteryTv) {
    let mut rig = Rig::new(StreamConfig {
        latency_frames: LOW,
        input_frames: 88_200,
        host_answers_latency: true,
    });
    let mut host = Host::new(0.0);
    host.quiet = Some((176_400, 22_050));
    rig.host = Some(host);
    let id = rig.add(speaker(1, Codec::Pcm));
    rig.stream.set_latency(session_latency_frames([0]), rig.now);
    rig.drain();
    let mut tv = JitteryTv::new(id, 1123, 0.02);
    tv.run_to(&mut rig, CHANGE);
    assert!(
        tv.asked_between(0.0, 15.0) > 0,
        "the jitter cut in at 0.2 s"
    );
    assert_eq!(rig.stream.latency_frames(), STEPPED, "stepped up");
    assert_eq!(tv.asked_between(15.0, CHANGE), 0, "clean at 0.3 s");
    (rig, tv)
}

/// From the change on: the margin never drops, the television asks for
/// nothing, and from the first packet that carries the new audio on, every
/// packet comes in order, whole and before its play time.
fn assert_clean_after(rig: &mut Rig, tv: &mut JitteryTv, events_before: usize, resumed: f64) {
    tv.run_to(rig, WATCH_END);
    let after: Vec<u128> = rig.events[events_before..]
        .iter()
        .filter_map(|event| match event {
            StreamEvent::Timeline { latency, .. } => Some(latency.as_millis()),
            _ => None,
        })
        .collect();
    assert!(!after.is_empty(), "the change moved the timeline");
    assert!(after.iter().all(|&ms| ms >= 300), "{after:?}");
    assert_eq!(rig.stream.latency_frames(), STEPPED);
    assert_eq!(tv.asked_between(CHANGE, WATCH_END), 0, "resend requests");

    let whole = |payload: &[u8]| decode(Codec::Pcm, payload).iter().all(|f| *f != [0, 0]);
    let first = tv
        .packets
        .iter()
        .position(|p| p.sent >= resumed && whole(&p.payload))
        .expect("new audio");
    assert!(
        tv.packets[first].sent < resumed + 0.1,
        "new audio {} s after the capture came back",
        tv.packets[first].sent - resumed
    );
    let played = &tv.packets[first..];
    for pair in played.windows(2) {
        assert_eq!(pair[1].seq, pair[0].seq.wrapping_add(1), "lost");
    }
    for packet in played {
        assert!(
            !packet.late(),
            "seq {} late by {}",
            packet.seq,
            packet.arrival - packet.play
        );
        assert!(whole(&packet.payload), "seq {} has silence", packet.seq);
    }
}

// covers: P143, P200
// The hosts' source or output change: the capture is rebuilt, delivers
// nothing for a second, and marks its first write as a new capture, as the
// Mac's tap rebuild and the Windows and Linux feed lease do.
#[test]
fn a_source_change_on_a_jittery_network_keeps_the_margin_and_plays_without_gaps() {
    let (mut rig, mut tv) = stepped_tv();
    let events_before = rig.events.len();
    let host = rig.host.as_mut().expect("host");
    let block = host.written + 1;
    host.pauses.push((block, HOLE));
    tv.run_to(&mut rig, CHANGE + HOLE - 0.02);
    rig.writer.new_capture();
    assert_clean_after(&mut rig, &mut tv, events_before, CHANGE + HOLE);
}

// covers: P143, P200
// A stall at the change that restarts the timeline: the speaker is flushed
// and comes back on a new timeline.
#[test]
fn a_timeline_restart_on_a_jittery_network_keeps_the_margin_and_plays_without_gaps() {
    let (mut rig, mut tv) = stepped_tv();
    let events_before = rig.events.len();
    rig.stream.restart(rig.now);
    rig.drain();
    assert_clean_after(&mut rig, &mut tv, events_before, CHANGE);
}
