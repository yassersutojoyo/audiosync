//! A television that reports no latency, behind a jittery 2.4 GHz network
//! (ping 20 to 150 ms, now and then about 300 ms), over four minutes: the
//! step up its resend requests asked for holds while it is in the stream,
//! and the margin steps back down once it left and a minute was clean
//! (P200).

mod common;

use audiosync_airplay::latency::session_latency_frames;
use audiosync_airplay::sdp::Codec;
use audiosync_airplay::stream::StreamConfig;
use common::jitter::JitteryTv;
use common::rig::{Host, Rig, speaker};

const LOW: u32 = 4_410;
const BASE: u32 = 8_820;
const STEPPED: u32 = 13_230;

fn rig() -> Rig {
    let mut rig = Rig::new(StreamConfig {
        latency_frames: LOW,
        input_frames: 88_200,
        host_answers_latency: true,
    });
    let mut host = Host::new(0.0);
    host.quiet = Some((176_400, 22_050));
    rig.host = Some(host);
    rig
}

/// Runs to `end` in whole seconds and adds each Lf change, by second, to
/// `course`.
fn watch(rig: &mut Rig, tv: &mut JitteryTv, end: f64, course: &mut Vec<(f64, u32)>) {
    while rig.t() < end - 1e-9 {
        tv.run_to(rig, (rig.t() + 1.0).floor());
        let frames = rig.stream.latency_frames();
        if course.last().is_none_or(|&(_, last)| last != frames) {
            course.push((rig.t().round(), frames));
        }
    }
}

// covers: P200
#[test]
fn the_margin_a_jittery_television_stepped_up_holds_while_it_plays() {
    let mut rig = rig();
    let id = rig.add(speaker(1, Codec::Pcm));
    rig.stream.set_latency(session_latency_frames([0]), rig.now);
    rig.drain();
    let mut tv = JitteryTv::new(id, 1123, 0.02);
    let mut course = Vec::new();
    watch(&mut rig, &mut tv, 240.0, &mut course);
    let stepped = course
        .iter()
        .position(|&(_, frames)| frames == STEPPED)
        .expect("stepped up");
    assert!(
        course[stepped..]
            .iter()
            .all(|&(_, frames)| frames == STEPPED),
        "Lf by second {course:?}, resend requests after the step {}",
        tv.asked_between(course[stepped].0, 240.0)
    );
    assert_eq!(tv.asked_between(course[stepped].0 + 1.0, 240.0), 0);
}

// covers: P200
#[test]
fn the_margin_steps_down_once_the_jittery_television_left_and_a_minute_was_clean() {
    let mut rig = rig();
    let id = rig.add(speaker(1, Codec::Pcm));
    rig.add(speaker(2, Codec::Pcm));
    rig.stream
        .set_latency(session_latency_frames([0, 0]), rig.now);
    rig.drain();
    let mut tv = JitteryTv::new(id, 1123, 0.02);
    let mut course = Vec::new();
    watch(&mut rig, &mut tv, 150.0, &mut course);
    assert_eq!(rig.stream.latency_frames(), STEPPED, "{course:?}");

    rig.stream.remove_speaker(id);
    rig.drain();
    watch(&mut rig, &mut tv, 160.0, &mut course);
    assert_eq!(rig.stream.latency_frames(), BASE, "{course:?}");
}
