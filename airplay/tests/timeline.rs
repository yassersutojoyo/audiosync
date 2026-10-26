//! The timeline's model and its contracts: the rate model under drift
//! (P101), FLUSH numbering through a real session (P34), and the host's
//! local outputs lined up with the speakers (P114).

mod common;

use std::time::Duration;

use audiosync_airplay::audio::FRAMES_PER_PACKET;
use audiosync_airplay::rate::{MAX_RATE_DEVIATION, MAX_UPDATE_SHIFT, RateModel, sync_point};
use audiosync_airplay::rtp::SyncPacket;
use audiosync_airplay::sdp::Codec;
use audiosync_airplay::session::SessionEvent;
use audiosync_airplay::stream::{SpeakerConfig, StreamEvent, migrate_local_delay};
use common::rig::{Host, Rig, Rng, decode, host_frame, speaker};
use common::{FakeReceiver, Link, config};

const PACKET: i64 = FRAMES_PER_PACKET as i64;

// covers: P101
#[test]
fn rate_model_tracks_drift() {
    for ppm in [200.0, -200.0] {
        let true_rate = 44_100.0 * (1.0 + ppm * 1e-6);
        let mut model = RateModel::new(0.0);
        let mut rng = Rng::new(9);
        let mut mark = 1;
        for k in 0..6_000u64 {
            // 10 ms blocks with ±2 ms jitter, 60 s.
            let end = ((k + 1) * 441) as i64;
            let at = end as f64 / true_rate + rng.uniform(-0.002, 0.002);
            // Arrivals alone never move c(n): R̂ changes only at sync
            // points.
            let probe = end + 300;
            let predicted = model.time(probe);
            model.observe(end - 441, end, at);
            assert_eq!(model.time(probe), predicted);
            // Fix c(n) a packet ahead, as the stream does when it cuts.
            while sync_point(mark) <= end + PACKET {
                let start = sync_point(mark);
                let next = sync_point(mark + 1);
                let (at_start, at_next) = (model.time(start), model.time(next));
                model.ensure(start);
                assert!((model.time(start) - at_start).abs() < 1e-12, "continuous");
                // Any frame up to the next sync point moves by at most
                // 0.5 ms.
                for n in [start + 1, (start + next) / 2, next] {
                    let before = at_start
                        + (at_next - at_start) * (n - start) as f64 / (next - start) as f64;
                    assert!((model.time(n) - before).abs() <= MAX_UPDATE_SHIFT + 1e-9);
                }
                let deviation = (model.rate() / 44_100.0 - 1.0).abs();
                assert!(deviation <= MAX_RATE_DEVIATION + 1e-12, "{deviation}");
                mark += 1;
            }
        }
        // R̂ converges on the host's rate.
        let error = (model.rate() - true_rate).abs() / true_rate;
        assert!(error < 20e-6, "{ppm} ppm: R̂ off by {} ppm", error * 1e6);
        // And c(n) on its arrivals.
        let last = 6_000 * 441;
        assert!((model.time(last - 1) - last as f64 / true_rate).abs() < 0.001);
    }

    // Through the stream: a host 200 ppm fast or slow plays without silence
    // or drops, and R̂ follows it.
    for ppm in [200.0, -200.0] {
        let mut rig = Rig::with_host();
        let mut host = Host::new(0.0).seed(11);
        host.rate = 44_100.0 * (1.0 + ppm * 1e-6);
        host.jitter = 0.002;
        rig.host = Some(host);
        let id = rig.add(speaker(1, Codec::Alac));
        rig.run_for(Duration::from_secs(60));
        let counters = rig.stream.counters();
        assert_eq!(counters.silent_frames(), 0, "{ppm} ppm");
        assert_eq!(counters.input_frames_dropped(), 0);
        let rate = 44_100.0 * (1.0 + ppm * 1e-6);
        assert!(((counters.rate() - rate) / rate).abs() < 20e-6);
        let audio = rig.audio(id);
        let packet = audio.last().expect("audio");
        let n = rig.frame_of(id, packet.header.timestamp) as u64;
        assert_eq!(decode(Codec::Alac, &packet.payload)[0], host_frame(n));
    }
}

// covers: P34
#[test]
fn rtsp_flush_numbering() {
    // A session that plays, and the stream that feeds it.
    let mut link = Link::new(config(), FakeReceiver::new());
    link.connect();
    let (first_seq, first_rtptime, destinations) = link
        .events
        .iter()
        .find_map(|event| match event {
            SessionEvent::Recording {
                first_seq,
                first_rtptime,
                destinations,
                ..
            } => Some((*first_seq, *first_rtptime, *destinations)),
            _ => None,
        })
        .expect("recording");
    let mut rig = Rig::with_host();
    let id = rig.add(SpeakerConfig::new(
        Codec::Alac,
        destinations,
        link.session.ids().ssrc,
        first_seq,
        first_rtptime,
    ));
    rig.run_for(Duration::from_secs(2));
    rig.stream
        .set_delay(id, Duration::from_millis(200), rig.now);
    rig.run_for(Duration::from_millis(260));
    let (seq, rtptime) = rig
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
    let flushed_at = rig.t();

    // FLUSH names the first packet after it; the answer's RTP-Info is
    // ignored.
    assert!(link.session.flush(seq, rtptime, link.now));
    link.pump();
    let flush = link.last("FLUSH");
    assert_eq!(
        flush.headers.get("RTP-Info"),
        Some(format!("seq={seq};rtptime={rtptime}").as_str())
    );
    assert_eq!(flush.headers.get("Session"), Some("1"));
    assert!(
        link.events
            .contains(&SessionEvent::Flushed { seq, rtptime })
    );

    // Numbering goes on from s and t: the packet before had s − 1 and
    // t − 352; the next one carries s and t with the marker, after a
    // first-flagged sync.
    let audio = rig.audio(id);
    let index = audio
        .iter()
        .position(|a| a.header.seq == seq)
        .expect("next packet");
    assert_eq!(audio[index - 1].header.seq, seq.wrapping_sub(1));
    assert_eq!(audio[index - 1].header.timestamp, rtptime.wrapping_sub(352));
    assert!(!audio[index - 1].header.marker);
    assert!(audio[index].header.marker);
    assert_eq!(audio[index].header.timestamp, rtptime);
    assert_eq!(audio[index].header.ssrc, link.session.ids().ssrc);
    assert!(audio[index].at >= flushed_at - 0.3);
    let sent = rig.of(id);
    let position = sent
        .iter()
        .position(|s| {
            s.transmit.bytes.len() > 12
                && s.transmit.bytes[2..4] == seq.to_be_bytes()
                && s.transmit.bytes[1] == 0xE0
        })
        .expect("flushed packet");
    let sync = SyncPacket::parse(&sent[position - 1].transmit.bytes).expect("sync before");
    assert!(sync.first);
    assert_eq!(sync.next_rtp, rtptime);
    for packet in &audio[index + 1..] {
        assert!(!packet.header.marker);
    }
}

// covers: P114
#[test]
fn host_contract_local_alignment() {
    // The stored delay setting migrates once to d_local = clamp(configured
    // − L, 0, 20 000 ms).
    let latency = Duration::from_millis(1_750);
    assert_eq!(
        migrate_local_delay(Duration::from_millis(2_000), latency),
        Duration::from_millis(250)
    );
    assert_eq!(
        migrate_local_delay(Duration::from_millis(1_000), latency),
        Duration::ZERO
    );
    assert_eq!(
        migrate_local_delay(Duration::from_millis(20_000), latency),
        Duration::from_millis(18_250)
    );
    assert_eq!(
        migrate_local_delay(Duration::from_millis(30_000), latency),
        Duration::from_millis(20_000)
    );

    let d_local = Duration::from_millis(300);
    let mut rig = Rig::with_host();
    let mut config = speaker(1, Codec::Alac);
    config.delay = d_local;
    let id = rig.add(config);
    rig.run_for(Duration::from_secs(3));

    // A fake local output plays frame n at c(n) + L + d_local, counted on
    // the sender clock from c(n): in step with the speaker whose d_s =
    // d_local, within one frame, for every frame the speaker names.
    let check = |rig: &Rig, since: f64| {
        let local_delay = rig.stream.local_output_delay(d_local).as_secs_f64();
        let syncs: Vec<_> = rig
            .syncs(id)
            .into_iter()
            .filter(|(at, _)| *at >= since)
            .collect();
        assert!(!syncs.is_empty());
        for (_, sync) in syncs {
            let n = rig.frame_of(id, sync.rtp_heard);
            let local = rig.c(n) + local_delay;
            let speaker = rig.clock.seconds_of(sync.time);
            assert!((local - speaker).abs() < 1.0 / 44_100.0, "frame {n}");
        }
        local_delay
    };
    let before = check(&rig, 0.0);
    assert!((before - 2.05).abs() < 1e-9, "L + d_local");

    // Lf changes, held while the speaker sounds and applied at the next
    // restart (P201): the stream reports the new L, the local output takes
    // it.
    rig.stream.set_latency(44_100, rig.now);
    rig.drain();
    assert_eq!(rig.stream.pending_latency_frames(), Some(44_100));
    rig.stream.restart(rig.now);
    rig.drain();
    rig.answer_flushes();
    let changed = rig.t();
    assert!(rig.events.iter().any(|event| matches!(
        event,
        StreamEvent::Timeline { restarted: true, latency, .. }
            if *latency == Duration::from_secs(1)
    )));
    rig.run_for(Duration::from_secs(3));
    let after = check(&rig, changed);
    assert!((after - 1.3).abs() < 1e-9);

    // The timeline restarts: the stream reports the new mapping (the input
    // position of frame 0); the host's frame at position p is frame
    // p − origin, and the local output re-anchors on c(n) of the new model.
    let position = rig.writer.position();
    rig.stream.restart(rig.now);
    rig.drain();
    rig.answer_flushes();
    let restarted = rig.t();
    let origin = rig
        .events
        .iter()
        .rev()
        .find_map(|event| match event {
            StreamEvent::Timeline {
                restarted: true,
                origin,
                ..
            } => Some(*origin),
            _ => None,
        })
        .expect("timeline");
    assert_eq!(origin, position as i64);
    rig.host = Some(Host::new(restarted));
    rig.run_for(Duration::from_secs(3));
    check(&rig, restarted);
    // The frame the local output plays as n is the host's frame at
    // position origin + n, the same one the speaker plays.
    for packet in rig.audio(id).iter().filter(|a| a.at > restarted + 2.5) {
        let n = rig.frame_of(id, packet.header.timestamp) as u64;
        assert_eq!(decode(Codec::Alac, &packet.payload)[0], host_frame(n));
    }
}
