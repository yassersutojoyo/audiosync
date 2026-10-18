//! Clock drift simulation for the delay reader.
//!
//! Capture runs at 48 kHz in 512-frame blocks; the output clock runs 200 ppm
//! fast or 200 ppm slow and reads 512-frame blocks at a 2 s delay, for two
//! hours of audio in each direction. A third of the way in, the output
//! stalls for 2 s. Every frame is self-identifying, and every read checks
//! that nothing repeats (S49) and, once the reader has primed (S45), that
//! the lag stays within T + N of `De - N` (S57). Time is counted in integer
//! ticks so the clocks never drift from rounding.

use std::sync::Arc;

use audiosync_core::reader::{Correction, Reader, tolerance_frames};
use audiosync_core::ring::Ring;

const RATE: u32 = 48_000;
const BLOCK: usize = 512;
const DELAY: i64 = 96_000;
const SECONDS: u64 = 2 * 3600;

#[derive(Debug, Default)]
struct Summary {
    worst: i64,
    reads: u64,
    skips: u64,
    step_backs: u64,
    played: u64,
    silent: u64,
}

/// Runs capture with a block every `writer_period` ticks and the output
/// with a block every `reader_period` ticks.
fn simulate(writer_period: u64, reader_period: u64) -> Summary {
    let ring = Ring::for_rate(RATE).unwrap();
    let mut writer = ring.writer().unwrap();
    let mut reader = Reader::new(Arc::clone(&ring), RATE);
    let t = tolerance_frames(RATE);
    let bound = t + BLOCK as i64;
    let total_blocks = SECONDS * u64::from(RATE) / BLOCK as u64;
    let stall_after = total_blocks / 3;
    let stall_ticks = 2 * u64::from(RATE) / BLOCK as u64 * reader_period;
    let mut stalled = false;

    let mut block = vec![0.0f32; BLOCK * 2];
    let mut out = vec![0.0f32; BLOCK * 2];
    let mut next_frame = 0u64;
    let mut written = 0u64;
    let mut last: i64 = -1;
    let mut primed = false;
    let (mut writer_at, mut reader_at) = (0u64, reader_period / 2);
    let mut summary = Summary::default();

    while written < total_blocks {
        if writer_at <= reader_at {
            for (i, f) in block.chunks_exact_mut(2).enumerate() {
                let tag = (next_frame + i as u64 + 1) as u32;
                f[0] = f32::from_bits(tag);
                f[1] = f32::from_bits(!tag);
            }
            writer.write(&block, BLOCK);
            next_frame += BLOCK as u64;
            written += 1;
            writer_at += writer_period;
            continue;
        }
        let report = reader.read(&mut out, BLOCK, DELAY, t);
        for (offset, f) in out.chunks_exact(2).enumerate() {
            let (a, b) = (f[0].to_bits(), f[1].to_bits());
            if a != 0 && b == !a {
                let k = i64::from(a) - 1;
                assert_eq!(k, report.start + offset as i64);
                assert!(k > last, "S49: frame {k} after {last}");
                last = k;
                summary.played += 1;
            } else {
                assert_eq!((a, b), (0, 0));
                summary.silent += 1;
            }
        }
        // Until more than the delay is written, the reader waits in silence
        // and its lag is W - P with P = 0 (S45); the delay holds from the
        // read that primes on.
        primed |= report.correction == Correction::Prime;
        if primed {
            let error = (report.lag - (DELAY - BLOCK as i64)).abs();
            assert!(
                error <= bound,
                "S57: lag error {error} > {bound} at read {}",
                summary.reads
            );
            summary.worst = summary.worst.max(error);
        } else {
            assert!(report.written <= DELAY && report.lag == report.written);
        }
        match report.correction {
            Correction::SkipForward => summary.skips += 1,
            Correction::StepBack => summary.step_backs += 1,
            _ => {}
        }
        summary.reads += 1;
        reader_at += reader_period;
        if !stalled && written >= stall_after {
            stalled = true;
            reader_at += stall_ticks;
        }
    }
    summary
}

// covers: S57, S52, S49, S50
#[test]
fn output_200_ppm_fast_for_two_hours() {
    // The output consumes a block every 10000 ticks, capture every 10002.
    let s = simulate(10_002, 10_000);
    eprintln!("+200 ppm: {s:?}");
    assert!(s.worst <= tolerance_frames(RATE) + BLOCK as i64, "S57");
    // Every |e| > T is corrected by the read that sees it (S44), so the
    // published error never leaves the dead band.
    assert!(s.worst <= tolerance_frames(RATE));
    // About 2 h × 200 ppm × 48 kHz = 69 120 frames of drift to absorb.
    assert!(s.step_backs >= 25, "drift was corrected by stepping back");
    assert!(s.skips >= 1, "the stall was skipped");
    // Unplayed: the 2 s still in the delay at the end and the 2 s skipped
    // after the stall; stepping back inserts silence but drops nothing.
    assert!(s.played >= (SECONDS - 5) * u64::from(RATE));
}

// covers: S57, S51, S49, S50
#[test]
fn output_200_ppm_slow_for_two_hours() {
    let s = simulate(9_998, 10_000);
    eprintln!("-200 ppm: {s:?}");
    assert!(s.worst <= tolerance_frames(RATE) + BLOCK as i64, "S57");
    // Every |e| > T is corrected by the read that sees it (S44), so the
    // published error never leaves the dead band.
    assert!(s.worst <= tolerance_frames(RATE));
    assert!(s.skips >= 25, "drift was corrected by skipping forward");
    assert_eq!(s.step_backs, 0);
}
