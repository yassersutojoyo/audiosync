//! What the rates of a session are (S4), what belongs to one session (S5), and
//! the teardown order that ownership enforces around the ring (S134).

use std::sync::Arc;

use audiosync_core::reader::{Correction, Reader, delay_ms_to_frames, tolerance_frames};
use audiosync_core::resample::Resampler;
use audiosync_core::ring::Ring;

/// A frame that names its session and its index (below 2^24).
fn frame(session: u32, index: u64) -> [f32; 2] {
    let tag = (session << 24) | (u32::try_from(index).unwrap() + 1);
    [f32::from_bits(tag), f32::from_bits(!tag)]
}

/// The session and index of a frame made by [`frame`]; `None` for silence.
fn decode(left: f32, right: f32) -> Option<(u32, u64)> {
    let (a, b) = (left.to_bits(), right.to_bits());
    (a != 0 && b == !a).then(|| (a >> 24, u64::from(a & 0x00ff_ffff) - 1))
}

fn block(session: u32, first: u64, frames: usize) -> Vec<f32> {
    (first..first + frames as u64)
        .flat_map(|index| frame(session, index))
        .collect()
}

// covers: S4, S62, S45
#[test]
fn delays_ring_indices_and_lag_count_capture_rate_frames() {
    // Captured at 44.1 kHz, played on a 48 kHz device.
    let rc = 44_100;
    let ring = Ring::for_rate(rc).unwrap();
    let mut capture = ring.writer().unwrap();
    let mut reader = Reader::new(Arc::clone(&ring), rc);
    let mut device = Resampler::new(rc, 48_000).unwrap();
    let delay = delay_ms_to_frames(1000.0, rc).unwrap();
    assert_eq!(delay, 44_100, "a delay is a count of Rc frames");
    let t = tolerance_frames(rc);
    assert_eq!(t, 2205, "and so is the dead band");
    // Every 10 ms: 441 frames captured at Rc, 480 frames played at 48 kHz.
    let captured = vec![0.25f32; 2 * 441];
    let mut wanted = vec![0.0f32; 2 * 4096];
    let mut played = vec![0.0f32; 2 * 480];
    let mut device_frames = 0;
    for round in 0..600 {
        capture.write(&captured, 441);
        let need = device.input_frames_for(480);
        let report = reader.read(&mut wanted, need, delay, t);
        assert_eq!(
            device.process(&wanted[..need * 2], &mut played),
            (need, 480)
        );
        device_frames += 480;
        assert_eq!(report.delay, delay);
        match round {
            // A second of Rc frames is 100 rounds: the reader waits in
            // silence until more than that is written, then primes (S45).
            0..100 => {
                assert_eq!(report.correction, Correction::None, "round {round}");
                assert_eq!((report.copied, report.lag), (0, report.written));
            }
            100 => assert_eq!(report.correction, Correction::Prime),
            _ => {
                // The reader keeps its delay in Rc frames while the device
                // consumes them at its own rate: no correction is ever
                // needed.
                assert_eq!(report.correction, Correction::None, "round {round}");
                let error = report.lag - (delay - need as i64);
                assert!(error.abs() <= t, "round {round}: lag {}", report.lag);
            }
        }
    }
    assert_eq!(ring.frames_written(), 600 * 441, "W counts Rc frames");
    assert_eq!(device_frames, 600 * 480, "the device counts its own frames");
}

// covers: S5, S17, S39, S45
#[test]
fn every_session_has_its_own_history_reader_positions_and_frame_count() {
    let rate = 48_000;
    let (n, delay) = (480usize, 4800i64);
    let t = tolerance_frames(rate);
    let ring = Ring::for_rate(rate).unwrap();
    let mut buf = vec![0.0f32; 2 * n];
    // Session 1: one second captured and played.
    let mut capture = ring.writer().unwrap();
    capture.reset();
    let mut output = Reader::new(Arc::clone(&ring), rate);
    for i in 0..100 {
        capture.write(&block(1, i * n as u64, n), n);
        output.read(&mut buf, n, delay, t);
    }
    assert_eq!(ring.frames_written(), 48_000);
    assert!(
        buf.chunks_exact(2)
            .all(|f| decode(f[0], f[1]).is_some_and(|(session, _)| session == 1))
    );
    // The session stops: capture stops and its outputs are removed.
    drop(output);
    drop(capture);
    // The next session starts with a reset. Its frame count starts at 0, and
    // nothing of session 1 is readable although its samples are still in
    // memory.
    let mut capture = ring.writer().unwrap();
    capture.reset();
    assert_eq!(ring.frames_written(), 0, "framesCaptured restarts at 0");
    let mut output = Reader::new(Arc::clone(&ring), rate);
    let report = output.read(&mut buf, n, delay, t);
    assert_eq!(
        (report.correction, report.start, report.lag),
        (Correction::None, 0, 0),
        "reader positions start fresh, with nothing written yet"
    );
    assert!(buf.iter().all(|sample| *sample == 0.0));
    let mut heard = Vec::new();
    let mut primed_at = None;
    for i in 0..30 {
        capture.write(&block(2, i * n as u64, n), n);
        let report = output.read(&mut buf, n, delay, t);
        if report.correction == Correction::Prime {
            primed_at = Some(report.written);
        }
        heard.extend(buf.chunks_exact(2).filter_map(|f| decode(f[0], f[1])));
    }
    // The reader primes once more than its delay is written (S45), at
    // W = 5280, so it starts with frame 480 of this session.
    assert_eq!(primed_at, Some(11 * n as i64));
    assert!(!heard.is_empty());
    for (k, (session, index)) in heard.iter().enumerate() {
        assert_eq!(*session, 2, "only this session's audio");
        assert_eq!(*index, 480 + k as u64, "counted from the session's start");
    }
}

// covers: S134, S39
#[test]
fn the_ring_is_reset_only_after_capture_stops_and_freed_after_its_last_reader() {
    let ring = Ring::for_rate(48_000).unwrap();
    let mut capture = ring.writer().unwrap();
    capture.write(&[0.5; 2 * 480], 480);
    // A running capture holds the ring's only writer, and a reset needs that
    // writer: nothing can reset the ring under a running capture.
    assert!(ring.writer().is_none());
    drop(capture);
    let mut next_session = ring.writer().expect("free once capture stopped");
    next_session.reset();
    assert_eq!(ring.frames_written(), 0);
    drop(next_session);
    // An output's reader shares the ring, so the engine letting go of it
    // frees nothing a running output callback still reads.
    let mut output = Reader::new(Arc::clone(&ring), 48_000);
    let weak = Arc::downgrade(&ring);
    drop(ring);
    let mut buf = vec![0.0f32; 2 * 480];
    output.read(&mut buf, 480, 4800, 1680);
    assert!(weak.upgrade().is_some());
    // The output's IO stopped and its reader went: now the ring is freed.
    drop(output);
    assert!(weak.upgrade().is_none());
}
