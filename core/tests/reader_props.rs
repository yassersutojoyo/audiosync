//! Property tests for the ring and the delay reader.
//!
//! Frames are self-identifying: frame `i` stores `i + 1` in the left
//! sample's bits and its complement in the right sample's bits, so every
//! emitted frame names its index and a torn or stale frame cannot pass for a
//! valid one. Bits are only moved, never computed on as floats.

use std::sync::Arc;

use audiosync_core::reader::{Correction, ReadReport, Reader};
use audiosync_core::ring::{MIN_MARGIN_FRAMES, Ring, RingWriter};
use proptest::prelude::*;
use proptest::test_runner::TestCaseError;

const CAPACITY: usize = 3 * MIN_MARGIN_FRAMES;
const MARGIN: i64 = MIN_MARGIN_FRAMES as i64;
const READERS: usize = 2;
const MAX_READ: usize = 2048;

fn frame(i: u64) -> [f32; 2] {
    let tag = (i + 1) as u32;
    [f32::from_bits(tag), f32::from_bits(!tag)]
}

fn index_of(l: f32, r: f32) -> Option<i64> {
    let (a, b) = (l.to_bits(), r.to_bits());
    (a != 0 && b == !a).then(|| i64::from(a) - 1)
}

#[derive(Debug, Clone)]
enum Op {
    /// The writer delivers this many frames in one call.
    Write(usize),
    /// One reader reads one block.
    Read { reader: usize, frames: usize },
    /// The control thread sets a new delay (with its resync request).
    SetDelay { reader: usize, delay: i64 },
    /// The writer pauses while a reader keeps reading (underrun, gap).
    Pause {
        reader: usize,
        reads: usize,
        frames: usize,
    },
    /// The control thread changes the tolerance.
    SetTolerance { reader: usize, tolerance: i64 },
}

fn op() -> impl Strategy<Value = Op> {
    prop_oneof![
        6 => (1usize..=4096).prop_map(Op::Write),
        1 => (4096usize..=40_000).prop_map(Op::Write),
        8 => (0..READERS, 1usize..=MAX_READ).prop_map(|(reader, frames)| Op::Read { reader, frames }),
        2 => (0..READERS, 0i64..=30_000).prop_map(|(reader, delay)| Op::SetDelay { reader, delay }),
        1 => (0..READERS, 1usize..=30, 1usize..=MAX_READ)
            .prop_map(|(reader, reads, frames)| Op::Pause { reader, reads, frames }),
        1 => (0..READERS, 0i64..=4000).prop_map(|(reader, tolerance)| Op::SetTolerance { reader, tolerance }),
    ]
}

struct Capture {
    writer: RingWriter,
    next: u64,
    buf: Vec<f32>,
}

impl Capture {
    fn write(&mut self, frames: usize) {
        self.buf.clear();
        for i in 0..frames as u64 {
            self.buf.extend_from_slice(&frame(self.next + i));
        }
        self.writer.write(&self.buf, frames);
        self.next += frames as u64;
    }
}

struct Output {
    reader: Reader,
    buf: Vec<f32>,
    /// Highest index ever emitted.
    last: Option<i64>,
    /// Whether a read has primed the reader (S45).
    primed: bool,
    /// The previous read's report.
    previous: Option<ReadReport>,
    /// A delay change was requested and no read has applied it yet.
    change_pending: bool,
}

impl Output {
    /// Reads one block and checks it: every frame is silence or exactly the
    /// frame its position names; indices only ever increase (S49); nothing
    /// unwritten or outside the readable window is emitted (S40); every
    /// re-positioning lands on the exact delay (S42, S46); a reader waits in
    /// silence until more than its delay is written (S45); a dry reader, at
    /// a W the writer has not moved since, makes no correction and keeps a
    /// requested change for the first read that finds unread frames (S44).
    fn read(&mut self, frames: usize) -> Result<ReadReport, TestCaseError> {
        let report = self.reader.read_target(&mut self.buf, frames);
        prop_assert_eq!(report.frames, frames);
        let oldest = (report.written - CAPACITY as i64 + MARGIN).max(0);
        for (offset, f) in self.buf[..frames * 2].chunks_exact(2).enumerate() {
            match index_of(f[0], f[1]) {
                Some(k) => {
                    prop_assert_eq!(k, report.start + offset as i64);
                    if let Some(last) = self.last {
                        prop_assert!(k > last, "S49: {} after {}", k, last);
                    }
                    prop_assert!(k < report.written && k >= oldest);
                    self.last = Some(k);
                }
                None => prop_assert!(f[0].to_bits() == 0 && f[1].to_bits() == 0),
            }
        }
        prop_assert!(report.lag >= 0);
        if report.correction != Correction::None {
            prop_assert_eq!(report.start, report.written - report.delay);
            prop_assert_eq!(report.lag, report.delay - frames as i64);
        }
        if !self.primed {
            if report.correction == Correction::Prime {
                prop_assert!(report.written > report.delay, "S45: primed too early");
                self.primed = true;
            } else {
                prop_assert_eq!(report.correction, Correction::None);
                prop_assert!(report.written <= report.delay, "S45: not primed");
                prop_assert_eq!(report.copied, 0);
                prop_assert_eq!(report.lag, report.written, "W - P with P = 0");
            }
        } else if self
            .previous
            .is_some_and(|p| p.lag == 0 && p.written == report.written)
        {
            prop_assert_eq!(report.correction, Correction::None, "dry: no anchor");
            prop_assert_eq!(
                (report.masked, report.copied, report.starved, report.lag),
                (0, 0, frames, 0)
            );
        } else if self.change_pending {
            prop_assert_eq!(report.correction, Correction::Resync);
        }
        if matches!(report.correction, Correction::Prime | Correction::Resync) {
            self.change_pending = false;
        }
        self.previous = Some(report);
        Ok(report)
    }

    /// Sets a new target delay with its resync request.
    fn set_delay(&mut self, delay: i64) {
        self.reader.control().set_delay_frames(delay);
        self.change_pending = true;
    }
}

fn session() -> (Arc<Ring>, Capture, Vec<Output>) {
    let ring = Ring::new(CAPACITY, MIN_MARGIN_FRAMES).unwrap();
    let capture = Capture {
        writer: ring.writer().unwrap(),
        next: 0,
        buf: Vec::new(),
    };
    let outputs = (0..READERS)
        .map(|_| Output {
            reader: Reader::new(Arc::clone(&ring), 48_000),
            buf: vec![0.0; MAX_READ * 2],
            last: None,
            primed: false,
            previous: None,
            change_pending: false,
        })
        .collect();
    (ring, capture, outputs)
}

fn apply(op: &Op, capture: &mut Capture, outputs: &mut [Output]) -> Result<(), TestCaseError> {
    match *op {
        Op::Write(frames) => capture.write(frames),
        Op::Read { reader, frames } => {
            outputs[reader].read(frames)?;
        }
        Op::SetDelay { reader, delay } => outputs[reader].set_delay(delay),
        Op::Pause {
            reader,
            reads,
            frames,
        } => {
            for _ in 0..reads {
                outputs[reader].read(frames)?;
            }
        }
        Op::SetTolerance { reader, tolerance } => outputs[reader]
            .reader
            .control()
            .set_tolerance_frames(tolerance),
    }
    Ok(())
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    // covers: S49, S46, S42, S44, S45, S47, S48, S53, S54, S58, S59, S40
    #[test]
    fn delay_changes_never_repeat_and_settle_exactly(
        ops in prop::collection::vec(op(), 1..60),
        finals in prop::collection::vec(0i64..=30_000, READERS),
        block in 64usize..=1024,
    ) {
        let (_ring, mut capture, mut outputs) = session();
        for op in &ops {
            apply(op, &mut capture, &mut outputs)?;
        }
        // Settle: each reader gets its final delay, then capture and output
        // run in lockstep long enough for any inserted silence to end.
        for (out, &delay) in outputs.iter_mut().zip(&finals) {
            out.set_delay(delay);
        }
        let rounds = CAPACITY / block + 8;
        let mut last_reports = vec![ReadReport::default(); READERS];
        for _ in 0..rounds {
            capture.write(block);
            for (out, report) in outputs.iter_mut().zip(&mut last_reports) {
                *report = out.read(block)?;
            }
        }
        for ((out, report), &delay) in outputs.iter().zip(&last_reports).zip(&finals) {
            let de = delay.max(block as i64).min(CAPACITY as i64 - MARGIN - block as i64);
            prop_assert_eq!(report.delay, de);
            prop_assert_eq!(report.lag, de - block as i64, "S46: exact, not within tolerance");
            prop_assert_eq!(out.reader.lag_frames(), de - block as i64);
            if report.written >= de {
                prop_assert_eq!(report.copied, block, "playing, not silent");
                prop_assert_eq!(out.last, Some(report.written - de + block as i64 - 1));
            }
        }
    }

    // covers: S49, S51, S52, S53, S55, S44, S45
    #[test]
    fn a_fixed_delay_never_repeats_under_any_schedule(
        schedule in prop::collection::vec((1usize..=3000, 0usize..=3, 1usize..=1024), 1..60),
        delay in 0i64..=20_000,
        tolerance in 0i64..=3000,
    ) {
        let (_ring, mut capture, mut outputs) = session();
        let out = &mut outputs[0];
        out.set_delay(delay);
        out.reader.control().set_tolerance_frames(tolerance);
        for &(write, reads, frames) in &schedule {
            capture.write(write);
            for _ in 0..reads {
                out.read(frames)?;
            }
        }
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    // covers: S58, S9
    #[test]
    fn readers_do_not_disturb_each_other(
        ops in prop::collection::vec(op(), 1..40),
        delay in 512i64..=12_000,
    ) {
        // Reader 0 runs a plain lockstep at a fixed delay while reader 1 is
        // put through arbitrary reads, delay changes and pauses.
        let (_ring, mut capture, mut outputs) = session();
        capture.write(CAPACITY);
        let (plain, others) = outputs.split_at_mut(1);
        let plain = &mut plain[0];
        plain.set_delay(delay);
        let first = plain.read(512)?;
        let mut expected_next = first.start + 512;
        for op in &ops {
            let op = match *op {
                Op::Write(_) => continue,
                Op::Read { frames, .. } => Op::Read { reader: 0, frames },
                Op::SetDelay { delay, .. } => Op::SetDelay { reader: 0, delay },
                Op::Pause { reads, frames, .. } => Op::Pause { reader: 0, reads, frames },
                Op::SetTolerance { tolerance, .. } => Op::SetTolerance { reader: 0, tolerance },
            };
            apply(&op, &mut capture, others)?;
            capture.write(512);
            let report = plain.read(512)?;
            prop_assert_eq!(report.correction, Correction::None);
            prop_assert_eq!(report.start, expected_next);
            prop_assert_eq!(report.lag, delay - 512);
            expected_next += 512;
        }
    }
}
