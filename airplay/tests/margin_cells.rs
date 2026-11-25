//! Cell 2 of P259, the network that stays bad: the jittery television of
//! P200's run, which asks nothing at its stepped level and cuts out below
//! it, with a quiet gap in the source every 2 minutes. Ignored by default;
//! run it by name. `CELL2_SECONDS` sets the run's length (default 3 h) and
//! `CELL2_SCALE` divides the wait, its ceiling and the watch (default 1,
//! the real values).

mod common;

use std::time::Duration;

use audiosync_airplay::latency::session_latency_frames;
use audiosync_airplay::sdp::Codec;
use audiosync_airplay::stream::{StreamConfig, StreamEvent};
use common::jitter::JitteryTv;
use common::rig::{Host, Rig, speaker};

fn env(name: &str, default: f64) -> f64 {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(default)
}

#[test]
#[ignore = "an hours-long rig run, P259 cell 2"]
fn cell_2_the_network_stays_bad() {
    let seconds = env("CELL2_SECONDS", 3.0 * 3600.0);
    let scale = env("CELL2_SCALE", 1.0);
    let (first, ceiling, watch) = (600.0 / scale, 3600.0 / scale, 600.0 / scale);

    let mut rig = Rig::new(StreamConfig {
        latency_frames: 4_410,
        input_frames: 88_200,
        host_answers_latency: true,
    });
    let mut host = Host::new(0.0);
    host.quiet = Some((120 * 44_100, 22_050));
    rig.host = Some(host);
    rig.stream.set_step_down_times(
        Duration::from_secs_f64(first),
        Duration::from_secs_f64(ceiling),
        Duration::from_secs_f64(watch),
    );
    let id = rig.add(speaker(1, Codec::Pcm));
    rig.stream.set_latency(session_latency_frames([0]), rig.now);
    rig.drain();
    let start = rig.stream.latency_frames();
    let mut tv = JitteryTv::new(id, 1123, 0.02);

    let mut seen = 0;
    let mut late = Vec::new();
    let mut lf = start;
    // (time, wait) of each try, (time, back to, next wait) of each failure.
    let mut tries: Vec<(f64, f64)> = Vec::new();
    let mut fails: Vec<(f64, u32, f64)> = Vec::new();
    let mut held = 0;
    let mut downs_outside = Vec::new();
    while rig.t() < seconds {
        let next = (rig.t() + 1.0).floor();
        tv.run_to(&mut rig, next);
        let t = rig.t();
        for event in &rig.events[seen..] {
            match event {
                StreamEvent::MarginTry { wait, .. } => tries.push((t, wait.as_secs_f64())),
                StreamEvent::MarginTryFailed {
                    back_to, next_wait, ..
                } => fails.push((t, *back_to, next_wait.as_secs_f64())),
                StreamEvent::MarginTryHeld { .. } => held += 1,
                _ => {}
            }
        }
        seen = rig.events.len();
        let now_lf = rig.stream.latency_frames();
        if now_lf < lf && tries.last().is_none_or(|&(at, _)| t - at > 2.0) {
            downs_outside.push(t);
        }
        lf = now_lf;
        late.extend(tv.forget(&mut rig));
    }

    println!("cell 2: {seconds} s, scale {scale}, start Lf {start}, end Lf {lf}");
    for (k, &(at, wait)) in tries.iter().enumerate() {
        let fail = fails.iter().find(|&&(back, ..)| back >= at);
        let (back, gaps) = match fail {
            Some(&(back, ..)) => (
                back,
                late.iter().filter(|&&p| p >= at && p <= back + 1.0).count(),
            ),
            None => (f64::NAN, 0),
        };
        println!(
            "try {k}: at {at:.0} s, wait {wait:.0} s, back up after {:.1} s, {} requests, {gaps} packets late",
            back - at,
            tv.asked_between(at, back + 1.0)
        );
    }
    println!(
        "failed {}, held {held}, downs outside a try {downs_outside:?}, packets late in the run {}, first at {:?} s",
        fails.len(),
        late.len(),
        late.first()
    );

    // The pass test: no step down outside a try point, and consecutive
    // tries spaced by the doubling wait, never a fixed interval.
    assert!(downs_outside.is_empty(), "{downs_outside:?}");
    let mut wait = first;
    for pair in tries.windows(2) {
        let fail = fails
            .iter()
            .find(|&&(back, ..)| back >= pair[0].0 && back <= pair[1].0);
        if let Some(&(back, _, next)) = fail {
            wait = (wait * 2.0).min(ceiling);
            assert_eq!(next, wait);
            assert!(pair[1].0 - back >= wait - 1.0, "tries {pair:?}");
        }
    }
}

#[test]
#[ignore = "a rig run, P259 cell 4"]
fn cell_4_two_stepped_receivers() {
    let seconds = env("CELL4_SECONDS", 1_500.0);
    let seeds = (
        env("CELL4_SEED_BAD", 1123.0) as u64,
        env("CELL4_SEED_CLEAN", 77.0) as u64,
    );
    let mut rig = Rig::new(StreamConfig {
        latency_frames: 4_410,
        input_frames: 88_200,
        host_answers_latency: true,
    });
    let mut host = Host::new(0.0);
    host.quiet = Some((120 * 44_100, 22_050));
    rig.host = Some(host);
    let bad = rig.add(speaker(1, Codec::Pcm));
    let clean = rig.add(speaker(2, Codec::Pcm));
    rig.stream
        .set_latency(session_latency_frames([0, 0]), rig.now);
    rig.drain();
    let mut tv_bad = JitteryTv::new(bad, seeds.0, 0.02);
    let mut tv_clean = JitteryTv::new(clean, seeds.1, 0.02);
    // Both are on the bad network until both stepped; then the second's
    // link turns clean and it asks for nothing more.
    let both_bad = 120.0;
    while rig.t() < seconds {
        let step = (seconds - rig.t()).min(0.005);
        rig.run_for(Duration::from_secs_f64(step));
        rig.answer_flushes();
        tv_bad.pump(&mut rig);
        if rig.t() < both_bad {
            tv_clean.pump(&mut rig);
        } else if rig.t() > both_bad + 1.0 {
            tv_bad.forget(&mut rig);
        }
    }
    let lines: Vec<String> = rig
        .events
        .iter()
        .filter_map(StreamEvent::margin_line)
        .collect();
    for line in &lines {
        println!("cell 4: {line}");
    }
    println!("cell 4: end Lf {}", rig.stream.latency_frames());
    let tries = lines
        .iter()
        .filter(|l| l.starts_with("margin try:"))
        .count();
    assert_eq!(tries, 2, "both try together");
    assert!(
        lines
            .iter()
            .any(|l| l.starts_with("margin try failed: 192.0.2.21"))
    );
    assert!(
        lines
            .iter()
            .any(|l| l.starts_with("margin try held: 192.0.2.22"))
    );
    assert!(!lines.iter().any(|l| l.contains("failed: 192.0.2.22")));
    assert_eq!(
        rig.stream.latency_frames(),
        22_050,
        "the failed one's floor"
    );
}
