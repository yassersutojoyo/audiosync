//! Delay reader (spec section 5: S41-S59, with S158 and S159).
//!
//! Each local output reads the shared ring through its own [`Reader`], at its
//! own delay, with its own drift correction (S9, S58). A read emits exactly N
//! frames; the indices of the frames a reader emits since its latest prime are
//! strictly increasing (S49, S159), so no delay change, drift correction,
//! writer stall, starved render or overrun ever repeats audio. Delay changes
//! are exact (S46); drift is held inside a dead band of T frames (S50-S52,
//! S57). During a writer stall a reader plays buffered frames only while it
//! stays inside the dead band, then holds silence (S53).
//!
//! The control thread talks to a reader through a [`ReaderControl`]: the
//! target delay is stored before the resync flag, the flag is raised with
//! release ordering and taken by the reader with an acquire exchange (S133).
//! Diagnostic counters are relaxed atomics updated only inside a read (S158).

#![forbid(unsafe_code)]

use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};

use crate::ring::Ring;

/// The drift dead band in milliseconds (S50).
pub const DRIFT_TOLERANCE_MS: u32 = 35;

/// The drift tolerance T at capture rate `rate`: `round(0.035 × rate)` frames
/// (S50); 1680 at 48 kHz. Computed in integers, so ties round up exactly.
pub fn tolerance_frames(rate: u32) -> i64 {
    (i64::from(DRIFT_TOLERANCE_MS) * i64::from(rate) + 500) / 1000
}

/// A delay of `ms` milliseconds at `rate` in frames: `floor(ms × rate / 1000)`
/// (S3). Returns `None` for a non-finite delay (it is ignored, S124);
/// negative delays count as 0.
pub fn delay_ms_to_frames(ms: f64, rate: u32) -> Option<i64> {
    if !ms.is_finite() {
        return None;
    }
    // `as` saturates for out-of-range values.
    Some((ms.max(0.0) * f64::from(rate) / 1000.0).floor() as i64)
}

/// Why a read re-positioned the reader (S44 step 2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Correction {
    /// The block follows the previous one.
    #[default]
    None,
    /// Not anchored (the first read, or the first after a prime reset):
    /// anchored at `W - De` and counted as a prime (S45, S158, S159).
    Prime,
    /// A requested resync, e.g. a delay change: re-anchored at `W - De` (S46).
    Resync,
    /// Behind by more than T: skipped forward to `W - De` (S51).
    SkipForward,
    /// The frames needed were overwritten (`P` below `W - C + M`, or the
    /// previous copy was overtaken by the writer): skipped forward to
    /// `W - De` (S55, S40). Counted as a drift skip.
    Overrun,
    /// Ahead by more than T, whether or not the writer moves: stepped back to
    /// `W - De`; frames already played are masked (S52, S53).
    StepBack,
}

/// What one read did. Plain data, returned by value (no allocation).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ReadReport {
    /// N: frames written into `dst`.
    pub frames: usize,
    /// W as loaded at the start of the read.
    pub written: i64,
    /// De: the effective delay used (S42).
    pub delay: i64,
    /// e = (W - P) - De for the P the read started from (0 before the first
    /// anchor). Positive when behind, negative when ahead.
    pub error: i64,
    /// Frame index of the block's first frame (P after step 2).
    pub start: i64,
    /// The published `lagFrames`, `W - P` (S56).
    pub lag: i64,
    /// The re-positioning this read made, if any.
    pub correction: Correction,
    /// Leading frames output as silence because they were already played or
    /// are gone (S44 step 3). Masked silence is not starvation.
    pub masked: usize,
    /// Frames copied from the ring.
    pub copied: usize,
    /// Trailing frames output as silence because the read reached W before
    /// the block was full: a starved render (S53, S158).
    pub starved: usize,
    /// The post-copy check failed and the copied frames were replaced by
    /// silence (S40); the next read skips forward (S55).
    pub rejected: bool,
}

/// A reader's diagnostic counters (S158), plus one for rejected copies.
/// Monotonic; a snapshot need not be consistent across fields.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ReaderCounters {
    /// Anchors because the reader was not anchored: its first read, or the
    /// first after [`ReaderControl::reset_prime`].
    pub primes: u64,
    /// Anchors applying [`ReaderControl::request_resync`].
    pub resyncs: u64,
    /// Skip-forward corrections (S51, S55).
    pub drift_skips: u64,
    /// Frames skipped by those corrections, e per correction.
    pub drift_skip_frames: u64,
    /// Step-back corrections (S52, S53).
    pub drift_silences: u64,
    /// Silence frames those corrections inserted, |e| per correction.
    pub drift_silence_frames: u64,
    /// Reads that reached W before the block was full.
    pub starved_renders: u64,
    /// Silence frames that followed W in those reads.
    pub starved_frames: u64,
    /// Not in S158: copies rejected by the post-copy check (S40).
    pub rejected_copies: u64,
}

/// State shared between a reader and the control thread.
#[derive(Debug, Default)]
struct Shared {
    delay: AtomicI64,
    tolerance: AtomicI64,
    resync: AtomicBool,
    prime: AtomicBool,
    lag: AtomicI64,
    primes: AtomicU64,
    resyncs: AtomicU64,
    drift_skips: AtomicU64,
    drift_skip_frames: AtomicU64,
    drift_silences: AtomicU64,
    drift_silence_frames: AtomicU64,
    starved_renders: AtomicU64,
    starved_frames: AtomicU64,
    rejected_copies: AtomicU64,
}

fn add(counter: &AtomicU64, by: u64) {
    counter.fetch_add(by, Ordering::Relaxed);
}

/// Takes a flag raised with release ordering. A plain load first, so the
/// common case costs no read-modify-write.
fn take(flag: &AtomicBool) -> bool {
    flag.load(Ordering::Relaxed) && flag.swap(false, Ordering::Acquire)
}

/// The control thread's handle to a [`Reader`]: target delay, tolerance,
/// resync and prime-reset requests, the published lag and the counters.
/// Cheap to clone; every method is a lock-free atomic operation (S128).
#[derive(Debug, Clone)]
pub struct ReaderControl {
    shared: Arc<Shared>,
}

impl ReaderControl {
    /// Sets the target delay D in frames and requests a resync, so the next
    /// read applies it exactly (S46). The target is stored before the flag.
    pub fn set_delay_frames(&self, frames: i64) {
        self.shared.delay.store(frames.max(0), Ordering::Relaxed);
        self.request_resync();
    }

    /// The target delay D in frames.
    pub fn delay_frames(&self) -> i64 {
        self.shared.delay.load(Ordering::Relaxed)
    }

    /// Sets the drift tolerance T in frames (S50).
    pub fn set_tolerance_frames(&self, frames: i64) {
        self.shared
            .tolerance
            .store(frames.max(0), Ordering::Relaxed);
    }

    /// The drift tolerance T in frames.
    pub fn tolerance_frames(&self) -> i64 {
        self.shared.tolerance.load(Ordering::Relaxed)
    }

    /// Asks the next read to re-anchor at its requested delay (S41).
    pub fn request_resync(&self) {
        self.shared.resync.store(true, Ordering::Release);
    }

    /// Asks the next read to return the reader to its initial state (not
    /// anchored, F = 0; the counters are kept), so that read anchors at
    /// `W - De` and counts a prime (S159).
    pub fn reset_prime(&self) {
        self.shared.prime.store(true, Ordering::Release);
    }

    /// The last published `lagFrames`, `W - P` (S56). Readable from any
    /// thread.
    pub fn lag_frames(&self) -> i64 {
        self.shared.lag.load(Ordering::Relaxed)
    }

    /// The reader's counters (S158).
    pub fn counters(&self) -> ReaderCounters {
        let s = &*self.shared;
        let get = |c: &AtomicU64| c.load(Ordering::Relaxed);
        ReaderCounters {
            primes: get(&s.primes),
            resyncs: get(&s.resyncs),
            drift_skips: get(&s.drift_skips),
            drift_skip_frames: get(&s.drift_skip_frames),
            drift_silences: get(&s.drift_silences),
            drift_silence_frames: get(&s.drift_silence_frames),
            starved_renders: get(&s.starved_renders),
            starved_frames: get(&s.starved_frames),
            rejected_copies: get(&s.rejected_copies),
        }
    }
}

/// A delay reader bound to one ring (S41), used by one output's IO thread.
///
/// Readers share nothing but the ring (S58).
pub struct Reader {
    ring: Arc<Ring>,
    shared: Arc<Shared>,
    /// P: index of the next frame to read.
    next: i64,
    /// F: one past the highest frame index emitted since the latest prime.
    emitted_end: i64,
    anchored: bool,
    /// Set when a copy failed the post-copy check (S40): the next read skips
    /// forward as for an overrun (S55).
    reanchor: bool,
    /// Test hook run between the copy and the post-copy check.
    #[cfg(test)]
    before_check: Option<Box<dyn FnMut() + Send>>,
}

impl fmt::Debug for Reader {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Reader")
            .field("next", &self.next)
            .field("emitted_end", &self.emitted_end)
            .field("anchored", &self.anchored)
            .finish_non_exhaustive()
    }
}

impl Reader {
    /// Creates a reader of `ring` for a session captured at `capture_rate`.
    /// The tolerance starts at `round(0.035 × capture_rate)` (S50), the target
    /// delay at 0 (floored to one block, S42). Not anchored until its first
    /// read (S43, S45).
    pub fn new(ring: Arc<Ring>, capture_rate: u32) -> Reader {
        let shared = Arc::new(Shared::default());
        shared
            .tolerance
            .store(tolerance_frames(capture_rate), Ordering::Relaxed);
        Reader {
            ring,
            shared,
            next: 0,
            emitted_end: 0,
            anchored: false,
            reanchor: false,
            #[cfg(test)]
            before_check: None,
        }
    }

    /// A control handle for this reader.
    pub fn control(&self) -> ReaderControl {
        ReaderControl {
            shared: Arc::clone(&self.shared),
        }
    }

    /// The ring this reader is bound to.
    pub fn ring(&self) -> &Arc<Ring> {
        &self.ring
    }

    /// The last published `lagFrames` (S56).
    pub fn lag_frames(&self) -> i64 {
        self.shared.lag.load(Ordering::Relaxed)
    }

    /// Reads `frames` frames (at most `dst.len() / 2`) into the start of
    /// `dst` at delay `delay_frames` with tolerance `tolerance_frames`, the
    /// interface of S41. Pending prime-reset and resync requests are taken
    /// by this read.
    ///
    /// Never allocates, locks or blocks; O(N).
    pub fn read(
        &mut self,
        dst: &mut [f32],
        frames: usize,
        delay_frames: i64,
        tolerance_frames: i64,
    ) -> ReadReport {
        let resync = self.take_requests();
        self.read_at(dst, frames, delay_frames, tolerance_frames, resync)
    }

    /// Reads at the delay and tolerance set through [`ReaderControl`]. The
    /// resync flag is taken (acquire) before the delay is loaded, so a delay
    /// change is never applied with a stale target (S133, S46).
    pub fn read_target(&mut self, dst: &mut [f32], frames: usize) -> ReadReport {
        let resync = self.take_requests();
        let delay = self.shared.delay.load(Ordering::Relaxed);
        let tolerance = self.shared.tolerance.load(Ordering::Relaxed);
        self.read_at(dst, frames, delay, tolerance, resync)
    }

    /// Applies a pending prime reset (S159) and takes the resync flag.
    fn take_requests(&mut self) -> bool {
        if take(&self.shared.prime) {
            self.next = 0;
            self.emitted_end = 0;
            self.anchored = false;
            self.reanchor = false;
        }
        take(&self.shared.resync)
    }

    /// One read, S44 steps 1-4.
    fn read_at(
        &mut self,
        dst: &mut [f32],
        frames: usize,
        delay: i64,
        tolerance: i64,
        resync: bool,
    ) -> ReadReport {
        let frames = frames.min(dst.len() / 2);
        let n = frames as i64;
        let ring = &*self.ring;
        let capacity = ring.capacity() as i64;
        let margin = ring.margin() as i64;

        // Step 1.
        let w = ring.frames_written() as i64;
        let de = delay.max(n).min(capacity - margin - n).max(0);
        let oldest = ring.oldest_readable(w as u64) as i64;
        let t = tolerance.max(0);

        // Step 2. Only frames really overwritten (below W - C + M) make an
        // overrun; a negative P early in a session reads silence (step 3)
        // and follows the dead-band rules.
        let e = if self.anchored {
            (w - self.next) - de
        } else {
            0
        };
        let correction = if !self.anchored {
            Correction::Prime
        } else if resync {
            Correction::Resync
        } else if self.reanchor || self.next < w - capacity + margin {
            Correction::Overrun
        } else if e > t {
            Correction::SkipForward
        } else if e < -t {
            Correction::StepBack
        } else {
            Correction::None
        };
        if correction != Correction::None {
            self.next = w - de;
            self.anchored = true;
            self.reanchor = false;
        }
        let p = self.next;

        // Step 3. The block splits into masked [p, lo), copied [lo, lim) and
        // starved [lim, end).
        let end = p + n;
        let lim = end.min(w).max(p);
        let lo = p.max(self.emitted_end).max(oldest).min(lim);
        let masked = (lo - p) as usize;
        let copied = (lim - lo) as usize;
        let starved = (end - lim) as usize;
        let out = &mut dst[..frames * 2];
        let (head, rest) = out.split_at_mut(masked * 2);
        let (body, tail) = rest.split_at_mut(copied * 2);
        head.fill(0.0);
        tail.fill(0.0);
        let mut rejected = false;
        if copied > 0 {
            ring.load_frames(lo as u64, body);
            #[cfg(test)]
            if let Some(hook) = self.before_check.as_mut() {
                hook();
            }
            // S40: keep the copy only if the writer cannot have reached it.
            if !ring.copy_still_valid(lo as u64) {
                body.fill(0.0);
                rejected = true;
                self.reanchor = true;
            }
            self.emitted_end = self.emitted_end.max(lim);
        }

        // Step 4: P = min(P + N, max(P, W)), which is `lim`.
        self.next = lim;
        let lag = w - lim;
        self.shared.lag.store(lag, Ordering::Relaxed);
        self.count(correction, e, starved, rejected);

        ReadReport {
            frames,
            written: w,
            delay: de,
            error: e,
            start: p,
            lag,
            correction,
            masked,
            copied: if rejected { 0 } else { copied },
            starved,
            rejected,
        }
    }

    /// S158: relaxed adds, only here. An anchor is never also a drift
    /// correction.
    fn count(&self, correction: Correction, e: i64, starved: usize, rejected: bool) {
        let s = &*self.shared;
        match correction {
            Correction::None => {}
            Correction::Prime => add(&s.primes, 1),
            Correction::Resync => add(&s.resyncs, 1),
            Correction::SkipForward | Correction::Overrun => {
                add(&s.drift_skips, 1);
                add(&s.drift_skip_frames, e.max(0) as u64);
            }
            Correction::StepBack => {
                add(&s.drift_silences, 1);
                add(&s.drift_silence_frames, e.unsigned_abs());
            }
        }
        if starved > 0 {
            add(&s.starved_renders, 1);
            add(&s.starved_frames, starved as u64);
        }
        if rejected {
            add(&s.rejected_copies, 1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ring::{MIN_MARGIN_FRAMES, RingWriter};

    const RATE: u32 = 48_000;
    const N: usize = 512;
    const T: i64 = 1680;

    fn frame(i: u64) -> [f32; 2] {
        let tag = (i + 1) as u32;
        [f32::from_bits(tag), f32::from_bits(!tag)]
    }

    fn index_of(l: f32, r: f32) -> Option<i64> {
        let (a, b) = (l.to_bits(), r.to_bits());
        (a != 0 && b == !a).then(|| i64::from(a) - 1)
    }

    fn ring(capacity: usize) -> Arc<Ring> {
        Ring::new(capacity, MIN_MARGIN_FRAMES).unwrap()
    }

    /// The capture side: writes self-identifying frames.
    struct Capture {
        writer: RingWriter,
        next: u64,
        buf: Vec<f32>,
    }

    impl Capture {
        fn new(ring: &Arc<Ring>) -> Capture {
            Capture {
                writer: ring.writer().unwrap(),
                next: 0,
                buf: Vec::new(),
            }
        }

        fn write(&mut self, frames: usize) {
            self.buf.clear();
            for i in 0..frames as u64 {
                self.buf.extend_from_slice(&frame(self.next + i));
            }
            self.writer.write(&self.buf, frames);
            self.next += frames as u64;
        }

        fn w(&self) -> i64 {
            self.next as i64
        }
    }

    /// One read's output, decoded.
    struct Block {
        report: ReadReport,
        frames: Vec<Option<i64>>,
    }

    impl Block {
        fn first(&self) -> Option<i64> {
            self.frames.iter().flatten().next().copied()
        }

        fn last(&self) -> Option<i64> {
            self.frames.iter().flatten().last().copied()
        }

        fn silent(&self) -> usize {
            self.frames.iter().filter(|f| f.is_none()).count()
        }
    }

    /// An output: a reader plus the S49 checks applied to every block.
    struct Output {
        reader: Reader,
        buf: Vec<f32>,
        last: Option<i64>,
    }

    impl Output {
        fn new(ring: &Arc<Ring>) -> Output {
            Output {
                reader: Reader::new(Arc::clone(ring), RATE),
                buf: Vec::new(),
                last: None,
            }
        }

        fn read(&mut self, n: usize, delay: i64) -> Block {
            self.read_t(n, delay, T)
        }

        fn read_t(&mut self, n: usize, delay: i64, tolerance: i64) -> Block {
            self.buf.clear();
            self.buf.resize(n * 2, 9.0);
            let report = self.reader.read(&mut self.buf, n, delay, tolerance);
            self.check(report)
        }

        fn read_target(&mut self, n: usize) -> Block {
            self.buf.clear();
            self.buf.resize(n * 2, 9.0);
            let report = self.reader.read_target(&mut self.buf, n);
            self.check(report)
        }

        fn check(&mut self, report: ReadReport) -> Block {
            let frames: Vec<Option<i64>> = self.buf[..report.frames * 2]
                .chunks_exact(2)
                .map(|f| {
                    let index = index_of(f[0], f[1]);
                    assert!(
                        index.is_some() || (f[0] == 0.0 && f[1] == 0.0),
                        "a frame is either valid or silence"
                    );
                    index
                })
                .collect();
            for (offset, index) in frames.iter().enumerate() {
                if let Some(k) = *index {
                    assert_eq!(
                        k,
                        report.start + offset as i64,
                        "frame at its block position"
                    );
                    if let Some(last) = self.last {
                        assert!(k > last, "S49: emitted {k} after {last}");
                    }
                    self.last = Some(k);
                }
            }
            Block { report, frames }
        }
    }

    // covers: S41, S43, S44, S45
    #[test]
    fn new_reader_anchors_at_exact_delay_with_history() {
        let ring = ring(1 << 18);
        let mut cap = Capture::new(&ring);
        cap.write(50 * N);
        let mut out = Output::new(&ring);
        let block = out.read(N, 4096);
        assert_eq!(block.report.correction, Correction::Prime);
        assert_eq!(block.report.start, cap.w() - 4096);
        assert_eq!(block.silent(), 0);
        assert_eq!(block.first(), Some(cap.w() - 4096));
        assert_eq!(block.last(), Some(cap.w() - 4096 + N as i64 - 1));
        assert_eq!(out.reader.lag_frames(), 4096 - N as i64);
    }

    // covers: S41
    #[test]
    fn read_fills_exactly_n_frames() {
        let ring = ring(1 << 18);
        let mut cap = Capture::new(&ring);
        cap.write(10 * N);
        let mut reader = Reader::new(Arc::clone(&ring), RATE);
        let mut dst = vec![7.0f32; 3 * N * 2];
        let report = reader.read(&mut dst, N, 2048, T);
        assert_eq!(report.frames, N);
        assert!(
            dst[N * 2..].iter().all(|&s| s == 7.0),
            "nothing past N frames is touched"
        );
        // A frame count larger than dst is clamped to dst.
        let mut small = vec![0.0f32; 10];
        let report = reader.read(&mut small, N, 2048, T);
        assert_eq!(report.frames, 5);
    }

    // covers: S45
    #[test]
    fn fresh_session_plays_de_minus_w_silence_first() {
        let ring = ring(1 << 18);
        let mut cap = Capture::new(&ring);
        cap.write(1000);
        let mut out = Output::new(&ring);
        let mut silent_before_first = 0usize;
        let mut first = None;
        for _ in 0..20 {
            let block = out.read(N, 4096);
            for f in &block.frames {
                match (first, f) {
                    (None, None) => silent_before_first += 1,
                    (None, Some(k)) => first = Some(*k),
                    _ => {}
                }
            }
            cap.write(N);
        }
        assert_eq!(
            first,
            Some(0),
            "playback starts at the session's first frame"
        );
        assert_eq!(silent_before_first, 4096 - 1000);
    }

    // covers: S42, S56, S44
    #[test]
    fn delay_zero_is_floored_to_one_block_and_plays_near_live() {
        let ring = ring(1 << 18);
        let mut cap = Capture::new(&ring);
        cap.write(8 * N);
        let mut out = Output::new(&ring);
        let mut previous_last = None;
        for _ in 0..50 {
            cap.write(N);
            let block = out.read(N, 0);
            assert_eq!(block.report.delay, N as i64, "De = max(D, N)");
            assert_eq!(block.silent(), 0, "not silent");
            assert_eq!(block.first(), Some(cap.w() - N as i64));
            if let Some(prev) = previous_last {
                assert_eq!(block.first(), Some(prev + 1), "contiguous");
            }
            previous_last = block.last();
            assert_eq!(block.report.lag, 0, "near-live: lag = De - N = 0");
        }
    }

    // covers: S42
    #[test]
    fn delay_is_capped_at_capacity_minus_margin_minus_block() {
        let ring = ring(1 << 16);
        let mut cap = Capture::new(&ring);
        cap.write(1 << 17);
        let mut out = Output::new(&ring);
        let block = out.read(N, 1_000_000_000);
        let cap_frames = (1 << 16) - MIN_MARGIN_FRAMES as i64 - N as i64;
        assert_eq!(block.report.delay, cap_frames);
        assert_eq!(block.report.start, cap.w() - cap_frames);
        assert_eq!(
            block.silent(),
            0,
            "the capped delay still lies inside the readable window"
        );
    }

    // covers: S44, S56, S59
    #[test]
    fn steady_state_is_contiguous_with_any_block_size() {
        let ring = ring(1 << 18);
        let mut cap = Capture::new(&ring);
        cap.write(20_000);
        let mut out = Output::new(&ring);
        let sizes = [512usize, 256, 1024, 480, 333, 4096, 1, 512, 2048, 64];
        let mut previous_last: Option<i64> = None;
        for round in 0..30 {
            for &n in &sizes {
                let block = out.read(n, 8192);
                if round > 0 || n != sizes[0] {
                    assert_eq!(block.report.correction, Correction::None);
                }
                assert_eq!(block.silent(), 0);
                if let Some(prev) = previous_last {
                    assert_eq!(block.first(), Some(prev + 1), "no gap or repeat");
                }
                previous_last = block.last();
                assert_eq!(
                    block.report.lag,
                    8192 - n as i64,
                    "lag = De - N of this call"
                );
                assert_eq!(block.report.start, block.report.written - 8192);
                // The writer then delivers as many frames as were just read.
                cap.write(n);
            }
        }
    }

    // covers: S3
    #[test]
    fn milliseconds_convert_to_floored_frames() {
        assert_eq!(delay_ms_to_frames(2000.0, 48_000), Some(96_000));
        assert_eq!(delay_ms_to_frames(1820.0, 48_000), Some(87_360));
        assert_eq!(delay_ms_to_frames(500.0, 48_000), Some(24_000));
        assert_eq!(delay_ms_to_frames(10.0, 44_100), Some(441));
        assert_eq!(delay_ms_to_frames(0.03, 44_100), Some(1), "floor(1.323)");
        assert_eq!(delay_ms_to_frames(20_000.0, 384_000), Some(7_680_000));
        assert_eq!(delay_ms_to_frames(-5.0, 48_000), Some(0));
        assert_eq!(delay_ms_to_frames(f64::NAN, 48_000), None);
        assert_eq!(delay_ms_to_frames(f64::INFINITY, 48_000), None);
    }

    // covers: S3, S42, S44, S56
    #[test]
    fn two_second_delay_holds_for_the_last_frame_of_each_block() {
        let ring = Ring::for_rate(RATE).unwrap();
        let mut cap = Capture::new(&ring);
        let delay = delay_ms_to_frames(2000.0, RATE).unwrap();
        cap.write(3 * RATE as usize);
        let mut out = Output::new(&ring);
        for _ in 0..200 {
            cap.write(N);
            let block = out.read(N, delay);
            let last = block.last().unwrap();
            assert_eq!(block.report.written - (last + 1), delay - N as i64);
            assert_eq!(block.report.lag, delay - N as i64);
        }
    }

    /// Runs `out` against `cap` in lockstep (one write, one read of N) and
    /// returns the blocks.
    fn lockstep(cap: &mut Capture, out: &mut Output, delay: i64, rounds: usize) -> Vec<Block> {
        (0..rounds)
            .map(|_| {
                cap.write(N);
                out.read(N, delay)
            })
            .collect()
    }

    fn settle(cap: &mut Capture, out: &mut Output, delay: i64) -> i64 {
        let blocks = lockstep(cap, out, delay, 10);
        let last = blocks.last().unwrap();
        assert_eq!(last.silent(), 0);
        last.last().unwrap()
    }

    /// Changes the delay the way the renderer does (S66): store D, request a
    /// resync, read.
    fn change_and_read(cap: &mut Capture, out: &mut Output, from_ms: f64, to_ms: f64) -> Block {
        let from = delay_ms_to_frames(from_ms, RATE).unwrap();
        let to = delay_ms_to_frames(to_ms, RATE).unwrap();
        out.reader.control().set_delay_frames(from);
        let _ = lockstep_target(cap, out, 12);
        out.reader.control().set_delay_frames(to);
        cap.write(N);
        let block = out.read_target(N);
        assert_eq!(block.report.correction, Correction::Resync);
        assert_eq!(block.report.start, cap.w() - to.max(N as i64));
        assert_eq!(
            block.report.lag,
            to.max(N as i64) - N as i64,
            "exact, not within tolerance"
        );
        block
    }

    fn lockstep_target(cap: &mut Capture, out: &mut Output, rounds: usize) -> Vec<Block> {
        (0..rounds)
            .map(|_| {
                cap.write(N);
                out.read_target(N)
            })
            .collect()
    }

    fn session_48k() -> (Capture, Output) {
        let ring = Ring::for_rate(RATE).unwrap();
        let mut cap = Capture::new(&ring);
        cap.write(10 * RATE as usize);
        let out = Output::new(&ring);
        (cap, out)
    }

    // covers: S46, S48
    #[test]
    fn change_to_500_ms_is_exact() {
        let (mut cap, mut out) = session_48k();
        let block = change_and_read(&mut cap, &mut out, 2000.0, 500.0);
        assert_eq!(block.silent(), 0, "a decrease skips forward, no silence");
        for b in lockstep_target(&mut cap, &mut out, 20) {
            assert_eq!(b.report.lag, 24_000 - N as i64);
        }
    }

    // covers: S46, S47
    #[test]
    fn change_to_3000_ms_is_exact() {
        let (mut cap, mut out) = session_48k();
        change_and_read(&mut cap, &mut out, 2000.0, 3000.0);
        for b in lockstep_target(&mut cap, &mut out, 300) {
            assert_eq!(b.report.lag, 144_000 - N as i64);
        }
    }

    // covers: S3, S46, S48
    #[test]
    fn change_to_1820_ms_is_exact() {
        let (mut cap, mut out) = session_48k();
        change_and_read(&mut cap, &mut out, 2000.0, 1820.0);
        for b in lockstep_target(&mut cap, &mut out, 20) {
            assert_eq!(b.report.lag, 87_360 - N as i64);
        }
    }

    // covers: S42, S46, S48
    #[test]
    fn change_to_0_ms_is_exact() {
        let (mut cap, mut out) = session_48k();
        let block = change_and_read(&mut cap, &mut out, 2000.0, 0.0);
        assert_eq!(block.report.delay, N as i64);
        assert_eq!(block.first(), Some(cap.w() - N as i64));
        for b in lockstep_target(&mut cap, &mut out, 20) {
            assert_eq!(b.report.lag, 0);
        }
    }

    /// Sets the target delay through the control handle, then runs `rounds`
    /// lockstep rounds through `read_target`; returns the last frame emitted.
    fn settle_target(cap: &mut Capture, out: &mut Output, delay: i64, rounds: usize) -> i64 {
        out.reader.control().set_delay_frames(delay);
        let blocks = lockstep_target(cap, out, rounds);
        let last = blocks.last().unwrap();
        assert_eq!(last.silent(), 0);
        last.last().unwrap()
    }

    // covers: S47, S49, S46
    #[test]
    fn increase_inserts_exactly_delta_silence_then_continues() {
        let ring = ring(1 << 18);
        let mut cap = Capture::new(&ring);
        cap.write(60_000);
        let mut out = Output::new(&ring);
        let before = settle_target(&mut cap, &mut out, 4096, 10);
        out.reader.control().set_delay_frames(24_000);
        let mut silent = 0usize;
        let mut next_valid = None;
        for round in 0..200 {
            cap.write(N);
            let block = out.read_target(N);
            if round == 0 {
                assert_eq!(block.report.correction, Correction::Resync);
            }
            for f in &block.frames {
                match (next_valid, f) {
                    (None, None) => silent += 1,
                    (None, Some(k)) => next_valid = Some(*k),
                    _ => {}
                }
            }
            assert_eq!(block.report.lag, 24_000 - N as i64);
        }
        assert_eq!(silent, 24_000 - 4096, "exactly delta frames of silence");
        assert_eq!(
            next_valid,
            Some(before + 1),
            "continues after the last frame emitted"
        );
        let block = lockstep_target(&mut cap, &mut out, 1).pop().unwrap();
        assert_eq!(block.report.lag, 24_000 - N as i64, "delay 24000 reached");
        assert_eq!(block.first(), Some(cap.w() - 24_000));
    }

    // covers: S48, S46
    #[test]
    fn decrease_is_applied_on_the_next_block() {
        let ring = ring(1 << 18);
        let mut cap = Capture::new(&ring);
        cap.write(60_000);
        let mut out = Output::new(&ring);
        let before = settle_target(&mut cap, &mut out, 24_000, 10);
        out.reader.control().set_delay_frames(4096);
        cap.write(N);
        let block = out.read_target(N);
        assert_eq!(block.report.correction, Correction::Resync);
        assert_eq!(block.silent(), 0);
        assert_eq!(block.first(), Some(cap.w() - 4096), "next block, new delay");
        assert_eq!(
            block.first().unwrap() - (before + 1),
            24_000 - 4096,
            "skipped delta forward"
        );
        assert_eq!(block.report.lag, 4096 - N as i64);
    }

    // covers: S46, S47, S48, S49
    #[test]
    fn up_then_down_lands_on_the_new_delay_without_replay() {
        let ring = ring(1 << 18);
        let mut cap = Capture::new(&ring);
        cap.write(80_000);
        let mut out = Output::new(&ring);
        settle_target(&mut cap, &mut out, 4096, 10);
        // Up to 30000; partway through its silence, down to 12000.
        out.reader.control().set_delay_frames(30_000);
        lockstep_target(&mut cap, &mut out, 20);
        let last = settle_target(&mut cap, &mut out, 12_000, 100);
        assert_eq!(out.reader.lag_frames(), 12_000 - N as i64);
        assert_eq!(
            last,
            cap.w() - 12_000 + N as i64 - 1,
            "lands on the new delay"
        );
    }

    // covers: S46
    #[test]
    fn a_newer_change_supersedes_an_unfinished_one() {
        let (mut cap, mut out) = session_48k();
        out.reader.control().set_delay_frames(4096);
        lockstep_target(&mut cap, &mut out, 10);
        out.reader.control().set_delay_frames(90_000);
        lockstep_target(&mut cap, &mut out, 3);
        out.reader.control().set_delay_frames(50_000);
        let blocks = lockstep_target(&mut cap, &mut out, 200);
        for b in &blocks {
            assert_eq!(b.report.lag, 50_000 - N as i64);
        }
        assert_eq!(blocks.last().unwrap().first(), Some(cap.w() - 50_000));
    }

    // covers: S46
    #[test]
    fn a_change_waits_for_the_next_read_however_long() {
        let (mut cap, mut out) = session_48k();
        out.reader.control().set_delay_frames(96_000);
        lockstep_target(&mut cap, &mut out, 10);
        // Output frozen: the change arrives, then 5 s pass without a read.
        out.reader.control().set_delay_frames(24_000);
        cap.write(5 * RATE as usize);
        let block = out.read_target(N);
        assert_eq!(block.report.correction, Correction::Resync);
        assert_eq!(block.report.lag, 24_000 - N as i64);
        assert_eq!(block.first(), Some(cap.w() - 24_000));
    }

    // covers: S46, S53, S54
    #[test]
    fn a_change_while_the_writer_is_frozen_is_applied_by_the_next_read() {
        let (mut cap, mut out) = session_48k();
        out.reader.control().set_delay_frames(4096);
        let last_before = lockstep_target(&mut cap, &mut out, 10)
            .last()
            .unwrap()
            .last()
            .unwrap();
        // Capture freezes; the delay changes; the output keeps calling.
        out.reader.control().set_delay_frames(12_000);
        let block = out.read_target(N);
        assert_eq!(block.report.correction, Correction::Resync);
        assert_eq!(block.report.lag, 12_000 - N as i64, "applied exactly");
        for _ in 1..40 {
            let block = out.read_target(N);
            assert_eq!(block.silent(), N, "holds silence while capture is frozen");
            assert_eq!(block.report.starved, 0, "never reaches W");
        }
        // Capture resumes: the reader plays out its masked range, then the
        // first frame it never played; its error stays inside the dead band.
        let blocks = lockstep_target(&mut cap, &mut out, 40);
        let first_after = blocks.iter().find_map(Block::first);
        assert_eq!(first_after, Some(last_before + 1));
        for b in &blocks {
            assert_eq!(b.report.correction, Correction::None);
            assert!(b.report.error.abs() <= T);
            assert_eq!(b.report.lag, 12_000 - N as i64 + b.report.error);
        }
    }

    // covers: S50
    #[test]
    fn tolerance_is_35_ms_rounded() {
        assert_eq!(tolerance_frames(48_000), 1680);
        assert_eq!(tolerance_frames(44_100), 1544, "1543.5 rounds up");
        assert_eq!(tolerance_frames(8_000), 280);
        assert_eq!(tolerance_frames(32_000), 1120);
        assert_eq!(tolerance_frames(96_000), 3360);
        assert_eq!(tolerance_frames(384_000), 13_440);
        let ring = ring(1 << 16);
        let reader = Reader::new(ring, 44_100);
        assert_eq!(reader.control().tolerance_frames(), 1544);
    }

    // covers: S50, S51
    #[test]
    fn nothing_is_corrected_inside_the_dead_band() {
        for (error, corrected) in [(T, false), (T + 1, true), (-T, false), (-T - 1, true)] {
            let ring = ring(1 << 18);
            let mut cap = Capture::new(&ring);
            cap.write(40_000);
            let mut out = Output::new(&ring);
            settle(&mut cap, &mut out, 8192);
            if error > 0 {
                // The writer gets `error` frames ahead of the reader.
                cap.write(N + error as usize);
            } else {
                // The reader takes `-error` extra frames while W stands still
                // (that read is still inside the dead band), and the writer
                // moves on.
                let extra = out.read(error.unsigned_abs() as usize, 8192);
                assert_eq!(extra.report.correction, Correction::None);
                cap.write(N);
            }
            let block = out.read(N, 8192);
            let offset = block.report.written - block.report.start - 8192;
            if corrected {
                assert_ne!(block.report.correction, Correction::None, "error {error}");
                assert_eq!(offset, 0, "corrected to the exact delay");
            } else {
                assert_eq!(block.report.correction, Correction::None, "error {error}");
                assert_eq!(offset, error, "left alone inside the dead band");
            }
        }
    }

    // covers: S51, S49, S56
    #[test]
    fn stall_resumes_after_the_last_frame_at_the_exact_delay() {
        let ring = ring(1 << 18);
        let mut cap = Capture::new(&ring);
        cap.write(60_000);
        let mut out = Output::new(&ring);
        let before = settle(&mut cap, &mut out, 24_000);
        // The output callback stalls for 1 s while capture continues.
        for _ in 0..(RATE as usize / N) {
            cap.write(N);
        }
        let block = out.read(N, 24_000);
        assert_eq!(block.report.correction, Correction::SkipForward);
        assert!(
            block.first().unwrap() > before,
            "resumes after the last frame it had"
        );
        assert_eq!(block.report.lag, 24_000 - N as i64, "delay restored: 23488");
        assert_eq!(block.report.lag, 23_488);
    }

    // covers: S52, S53, S49, S158
    #[test]
    fn running_ahead_steps_back_with_masked_silence() {
        let ring = ring(1 << 18);
        let mut cap = Capture::new(&ring);
        cap.write(60_000);
        let mut out = Output::new(&ring);
        let before = settle(&mut cap, &mut out, 8192);
        // The output takes extra blocks the writer has not matched. Inside
        // the dead band it keeps playing what is buffered ...
        for k in 1..=3 {
            let block = out.read(N, 8192);
            assert_eq!(block.report.correction, Correction::None);
            assert_eq!(block.report.error, -k * N as i64);
            assert_eq!(block.silent(), 0);
        }
        // ... and once more than T ahead it steps back at once, whether or
        // not W moves.
        let w = cap.w();
        let block = out.read(N, 8192);
        assert_eq!(block.report.correction, Correction::StepBack);
        assert_eq!(block.report.error, -4 * N as i64);
        assert_eq!(block.report.start, w - 8192);
        // The frames between the new position and the last emitted are masked.
        let mut silent = block.silent();
        let mut next = block.first();
        while next.is_none() {
            cap.write(N);
            let b = out.read(N, 8192);
            silent += b.silent();
            next = b.first();
        }
        assert_eq!(silent, 4 * N, "|e| frames of silence, never a repeat");
        assert_eq!(
            next,
            Some(before + 3 * N as i64 + 1),
            "the first frame never played"
        );
        let counters = out.reader.control().counters();
        assert_eq!(
            (counters.drift_silences, counters.drift_silence_frames),
            (1, 4 * N as u64)
        );
        assert_eq!(
            counters.starved_renders, 0,
            "masked silence is not starvation"
        );
    }

    // covers: S53, S52, S49, S158
    #[test]
    fn a_writer_stall_above_the_dead_band_holds_silence() {
        let ring = ring(1 << 18);
        let mut cap = Capture::new(&ring);
        cap.write(20_000);
        let mut out = Output::new(&ring);
        let before = settle(&mut cap, &mut out, 4096);
        assert!(4096 > T + N as i64);
        let w = cap.w();
        let mut played: Vec<i64> = Vec::new();
        for read in 1..=103 {
            let block = out.read(N, 4096);
            played.extend(block.frames.iter().flatten());
            assert!(block.report.start < w, "never reaches W");
            assert_eq!(block.report.starved, 0);
            if read <= 3 {
                assert_eq!(
                    block.silent(),
                    0,
                    "buffered frames while inside the dead band"
                );
            } else {
                assert_eq!(block.silent(), N, "silence held instead of draining");
            }
            let steps_back = read % 4 == 0;
            assert_eq!(block.report.correction == Correction::StepBack, steps_back);
        }
        assert_eq!(
            played,
            (before + 1..before + 1 + 3 * N as i64).collect::<Vec<_>>()
        );
        let counters = out.reader.control().counters();
        assert_eq!(counters.drift_silences, 25);
        assert_eq!(counters.drift_silence_frames, 51_200);
        assert_eq!(counters.starved_renders, 0);
    }

    // covers: S53, S49, S158
    #[test]
    fn a_writer_stall_inside_the_dead_band_runs_dry_at_w() {
        let ring = ring(1 << 18);
        let mut cap = Capture::new(&ring);
        cap.write(20_000);
        let mut out = Output::new(&ring);
        settle(&mut cap, &mut out, 1024);
        let w = cap.w();
        for read in 1..=20 {
            let block = out.read(N, 1024);
            assert!(block.report.start <= w, "P never passes W");
            assert_ne!(block.report.correction, Correction::StepBack);
            if read == 1 {
                assert_eq!(block.last(), Some(w - 1), "every frame up to W is played");
                assert_eq!(
                    block.report.starved, 0,
                    "the block was full when it reached W"
                );
            } else {
                assert_eq!(block.report.starved, N, "silent while W does not move");
            }
        }
        assert_eq!(out.reader.lag_frames(), 0);
        let counters = out.reader.control().counters();
        assert_eq!(
            (counters.starved_renders, counters.starved_frames),
            (19, 9728)
        );
        assert_eq!(counters.drift_silences, 0);
    }

    // covers: S54, S49, S56
    #[test]
    fn a_reader_holding_silence_resumes_with_the_first_frame_it_never_played() {
        let ring = ring(1 << 18);
        let mut cap = Capture::new(&ring);
        cap.write(20_000);
        let mut out = Output::new(&ring);
        settle(&mut cap, &mut out, 4096);
        for _ in 0..10 {
            out.read(N, 4096);
        }
        let last_played = out.last.unwrap();
        let blocks = lockstep(&mut cap, &mut out, 4096, 20);
        assert_eq!(blocks.iter().find_map(Block::first), Some(last_played + 1));
        for b in &blocks {
            assert_eq!(b.report.correction, Correction::None);
            assert_eq!(b.report.error, -2 * N as i64, "inside the dead band");
            assert_eq!(b.report.lag, 4096 - N as i64 + b.report.error);
        }
        // A resync makes it exact again.
        out.reader.control().request_resync();
        let b = lockstep(&mut cap, &mut out, 4096, 1).pop().unwrap();
        assert_eq!(b.report.lag, 4096 - N as i64);
    }

    // covers: S54, S49
    #[test]
    fn a_reader_that_ran_dry_plays_new_frames_as_they_arrive() {
        let ring = ring(1 << 18);
        let mut cap = Capture::new(&ring);
        cap.write(20_000);
        let mut out = Output::new(&ring);
        settle(&mut cap, &mut out, 1024);
        for _ in 0..10 {
            out.read(N, 1024);
        }
        let w = cap.w();
        assert_eq!(out.last, Some(w - 1));
        let blocks = lockstep(&mut cap, &mut out, 1024, 10);
        assert_eq!(blocks[0].first(), Some(w), "the first new frame");
        for b in &blocks {
            assert_eq!(b.silent(), 0);
            assert_eq!(b.report.correction, Correction::None);
            assert!(b.report.error.abs() <= T);
        }
    }

    // covers: S54, S51, S56
    #[test]
    fn a_reader_far_behind_after_a_stall_skips_to_the_exact_delay() {
        let ring = ring(1 << 18);
        let mut cap = Capture::new(&ring);
        cap.write(20_000);
        let mut out = Output::new(&ring);
        settle(&mut cap, &mut out, 4096);
        for _ in 0..10 {
            out.read(N, 4096);
        }
        // Capture comes back with two seconds at once before the next read.
        cap.write(2 * RATE as usize);
        let b = out.read(N, 4096);
        assert_eq!(b.report.correction, Correction::SkipForward);
        assert_eq!(b.report.lag, 4096 - N as i64, "exactly De - N");
        assert_eq!(b.first(), Some(cap.w() - 4096));
    }

    // covers: S55, S139, S49, S158
    #[test]
    fn overrun_after_a_very_long_stall_skips_forward() {
        let ring = ring(1 << 16);
        let mut cap = Capture::new(&ring);
        cap.write(30_000);
        let mut out = Output::new(&ring);
        let before = settle(&mut cap, &mut out, 20_000);
        // A stall longer than the ring's history (sleep, wake).
        cap.write(3 << 16);
        let block = out.read(N, 20_000);
        assert_eq!(block.report.correction, Correction::Overrun);
        assert!(block.first().unwrap() > before);
        assert_eq!(block.report.lag, 20_000 - N as i64);
        let counters = out.reader.control().counters();
        assert_eq!(counters.drift_skips, 1);
        assert_eq!(counters.drift_skip_frames, block.report.error as u64);
        // Settled readers sit one block ahead until the writer's next block.
        assert_eq!(block.report.error, (3 << 16) - N as i64);
    }

    // covers: S40, S55
    #[test]
    fn a_copy_overtaken_by_the_writer_becomes_silence_and_skips_forward() {
        let ring = ring(1 << 16);
        let mut cap = Capture::new(&ring);
        cap.write(70_000);
        let mut out = Output::new(&ring);
        let delay = (1 << 16) - MIN_MARGIN_FRAMES as i64 - N as i64;
        settle(&mut cap, &mut out, delay);
        // This read starts one block ahead (no write preceded it), at the
        // oldest readable frame plus two blocks. The reader is "paused"
        // between its copy and its check while the writer moves past that
        // slack: the hook owns the capture side and writes four blocks the
        // first time it runs.
        let w = cap.w();
        let mut lap = Some(cap);
        out.reader.before_check = Some(Box::new(move || {
            if let Some(mut cap) = lap.take() {
                cap.write(4 * N);
            }
        }));
        let block = out.read(N, delay);
        assert_eq!(
            block.report.start - ring.oldest_readable(w as u64) as i64,
            2 * N as i64
        );
        assert!(block.report.rejected);
        assert_eq!(block.silent(), N, "the stale copy is replaced by silence");
        out.reader.before_check = None;
        let w_now = ring.frames_written() as i64;
        let block = out.read(N, delay);
        assert_eq!(block.report.correction, Correction::Overrun);
        assert_eq!(block.report.start, w_now - delay);
        assert!(block.report.error > 0, "a skip forward");
        let counters = out.reader.control().counters();
        assert_eq!(counters.rejected_copies, 1);
        assert_eq!(
            (counters.drift_skips, counters.drift_skip_frames),
            (1, block.report.error as u64)
        );
    }

    // covers: S56
    #[test]
    fn lag_is_readable_from_another_thread() {
        let ring = ring(1 << 18);
        let mut cap = Capture::new(&ring);
        cap.write(20_000);
        let mut out = Output::new(&ring);
        let control = out.reader.control();
        lockstep(&mut cap, &mut out, 4096, 3);
        let seen = std::thread::spawn(move || control.lag_frames())
            .join()
            .unwrap();
        assert_eq!(seen, 4096 - N as i64);
    }

    // covers: S57, S50, S49
    #[test]
    fn a_writer_gap_shorter_than_t_leaves_a_residual_inside_the_dead_band() {
        let ring = Ring::for_rate(RATE).unwrap();
        let mut cap = Capture::new(&ring);
        cap.write(2 * RATE as usize);
        let mut out = Output::new(&ring);
        settle(&mut cap, &mut out, 48_000);
        // Capture skips three callbacks; the output keeps going.
        for _ in 0..3 {
            out.read(N, 48_000);
        }
        let blocks = lockstep(&mut cap, &mut out, 48_000, 50);
        for b in &blocks {
            assert_eq!(b.report.correction, Correction::None);
            assert_eq!(b.report.lag, 45_952, "47488 exact, 1536 short");
            assert!((b.report.lag - (48_000 - N as i64)).abs() <= T);
        }
    }

    // covers: S58, S9, S44
    #[test]
    fn readers_are_independent() {
        let ring = ring(1 << 18);
        let mut cap = Capture::new(&ring);
        cap.write(30_000);
        let mut one = Output::new(&ring);
        let mut two = Output::new(&ring);
        let (mut last1, mut last2) = (None, None);
        for _ in 0..100 {
            cap.write(N);
            let a = one.read(N, 4096);
            let b = two.read(N, 12_000);
            assert_eq!(a.silent() + b.silent(), 0);
            if let (Some(l1), Some(l2)) = (last1, last2) {
                assert_eq!(a.first(), Some(l1 + 1), "reader 1 contiguous");
                assert_eq!(b.first(), Some(l2 + 1), "reader 2 contiguous");
            }
            last1 = a.last();
            last2 = b.last();
            assert_eq!(a.report.lag, 4096 - N as i64);
            assert_eq!(b.report.lag, 12_000 - N as i64);
            assert_ne!(a.first(), b.first(), "different positions");
        }
        // Changing one reader's delay leaves the other alone.
        cap.write(N);
        one.reader.control().set_delay_frames(20_000);
        let a = one.read_target(N);
        let b = two.read(N, 12_000);
        assert_eq!(a.report.lag, 20_000 - N as i64);
        assert_eq!(b.report.correction, Correction::None);
        assert_eq!(b.first(), Some(last2.unwrap() + 1));
    }

    // covers: S41, S46
    #[test]
    fn request_resync_reanchors_to_the_exact_delay() {
        let ring = ring(1 << 18);
        let mut cap = Capture::new(&ring);
        cap.write(30_000);
        let mut out = Output::new(&ring);
        settle(&mut cap, &mut out, 8192);
        // Drift of 1000 frames: inside the dead band, so it stays.
        cap.write(1000);
        let block = lockstep(&mut cap, &mut out, 8192, 1).pop().unwrap();
        assert_eq!(block.report.lag, 8192 - N as i64 + 1000);
        out.reader.control().request_resync();
        let block = lockstep(&mut cap, &mut out, 8192, 1).pop().unwrap();
        assert_eq!(block.report.correction, Correction::Resync);
        assert_eq!(block.report.lag, 8192 - N as i64);
    }

    // covers: S137, S45, S49
    #[test]
    fn a_replugged_output_never_replays_what_it_played_before() {
        let ring = ring(1 << 18);
        let mut cap = Capture::new(&ring);
        cap.write(30_000);
        let mut out = Output::new(&ring);
        let last_before_unplug = settle(&mut cap, &mut out, 8192);
        drop(out);
        // Unplugged for 300 ms, then the next poll re-adds it.
        cap.write(14_400);
        let mut fresh = Output::new(&ring);
        let block = fresh.read(N, 8192);
        assert_eq!(block.report.correction, Correction::Prime);
        assert_eq!(
            block.report.lag,
            8192 - N as i64,
            "in sync on its first callback"
        );
        assert!(block.first().unwrap() > last_before_unplug);
    }

    // covers: S123
    #[test]
    fn non_finite_samples_pass_through_the_reader_unchanged() {
        let ring = ring(1 << 16);
        let mut writer = ring.writer().unwrap();
        let odd = [f32::NAN, f32::INFINITY, f32::NEG_INFINITY, -0.0];
        for _ in 0..1000 {
            writer.write(&odd, 2);
        }
        let mut reader = Reader::new(Arc::clone(&ring), RATE);
        let mut dst = vec![0.0f32; N * 2];
        let report = reader.read(&mut dst, N, 0, T);
        assert_eq!(report.copied, N);
        for pair in dst.chunks_exact(4) {
            for (a, b) in pair.iter().zip(&odd) {
                assert_eq!(a.to_bits(), b.to_bits());
            }
        }
    }

    // covers: S44, S53
    #[test]
    fn a_block_straddling_w_ends_in_silence_and_stops_at_w() {
        let ring = ring(1 << 18);
        let mut cap = Capture::new(&ring);
        cap.write(10_000);
        let mut out = Output::new(&ring);
        settle(&mut cap, &mut out, 1024);
        // Reader runs one block ahead of a stopped writer, then half a block.
        let block = out.read(N, 1024);
        assert_eq!(block.silent(), 0);
        let block = out.read(N + 300, 1024);
        assert_eq!(block.report.copied, 0);
        assert_eq!(block.report.starved, N + 300);
        let w = cap.w();
        assert_eq!(out.reader.lag_frames(), 0, "P = W");
        cap.write(100);
        let block = out.read(N, 1024);
        assert_eq!(block.report.copied, 100);
        assert_eq!(block.report.starved, N - 100);
        assert_eq!(block.first(), Some(w));
    }

    // covers: S59
    #[test]
    fn zero_frame_reads_are_harmless() {
        let ring = ring(1 << 18);
        let mut cap = Capture::new(&ring);
        cap.write(10_000);
        let mut out = Output::new(&ring);
        settle(&mut cap, &mut out, 2048);
        let block = out.read(0, 2048);
        assert_eq!(block.report.frames, 0);
        let block = lockstep(&mut cap, &mut out, 2048, 1).pop().unwrap();
        assert_eq!(block.report.correction, Correction::None);
        assert_eq!(block.silent(), 0);
    }

    // covers: S57, S50, S51, S52
    #[test]
    fn drift_after_a_stall_is_still_held() {
        let ring = Ring::for_rate(RATE).unwrap();
        let mut cap = Capture::new(&ring);
        cap.write(RATE as usize);
        let mut out = Output::new(&ring);
        let delay = 24_000i64;
        settle(&mut cap, &mut out, delay);
        // Stall the output for 2 s, then run it 0.5% fast for 30 s: one extra
        // read every 200 writes.
        cap.write(2 * RATE as usize);
        let mut worst = 0i64;
        for i in 0..(30 * RATE as usize / N) {
            cap.write(N);
            let b = out.read(N, delay);
            worst = worst.max((b.report.lag - (delay - N as i64)).abs());
            if i % 200 == 199 {
                let b = out.read(N, delay);
                worst = worst.max((b.report.lag - (delay - N as i64)).abs());
            }
        }
        assert!(worst <= T + N as i64, "worst error {worst}");
        assert!(out.reader.control().counters().drift_silences > 0);
    }

    // covers: S158
    #[test]
    fn a_steady_run_counts_one_prime_only() {
        let ring = ring(1 << 18);
        let mut cap = Capture::new(&ring);
        cap.write(2000);
        let mut out = Output::new(&ring);
        // A fresh session: the first second of the delay is silence (S45).
        lockstep(&mut cap, &mut out, 4096, 300);
        let counters = out.reader.control().counters();
        assert_eq!(
            counters,
            ReaderCounters {
                primes: 1,
                ..ReaderCounters::default()
            }
        );
    }

    // covers: S158, S46
    #[test]
    fn a_delay_change_counts_one_resync_and_no_drift_correction() {
        let ring = ring(1 << 18);
        let mut cap = Capture::new(&ring);
        cap.write(60_000);
        let mut out = Output::new(&ring);
        settle_target(&mut cap, &mut out, 24_000, 10);
        out.reader.control().set_delay_frames(4096);
        lockstep_target(&mut cap, &mut out, 50);
        let counters = out.reader.control().counters();
        assert_eq!((counters.primes, counters.resyncs), (1, 1));
        assert_eq!(counters.drift_skips + counters.drift_silences, 0);
        assert_eq!(counters.starved_renders, 0);
    }

    // covers: S158, S51
    #[test]
    fn a_lag_4488_frames_too_long_counts_one_skip_of_4488_frames() {
        let ring = ring(1 << 18);
        let mut cap = Capture::new(&ring);
        cap.write(30_000);
        let mut out = Output::new(&ring);
        settle(&mut cap, &mut out, 8192);
        cap.write(N + 4488);
        let block = out.read(N, 8192);
        assert_eq!(block.report.correction, Correction::SkipForward);
        assert_eq!(block.report.error, 4488);
        let counters = out.reader.control().counters();
        assert_eq!(
            (counters.drift_skips, counters.drift_skip_frames),
            (1, 4488)
        );
        assert_eq!(counters.primes, 1);
    }

    // covers: S158, S53
    #[test]
    fn starvation_counts_only_the_silence_after_w() {
        let ring = ring(1 << 18);
        let mut cap = Capture::new(&ring);
        cap.write(20_000);
        let mut out = Output::new(&ring);
        settle(&mut cap, &mut out, 1324);
        // Capture stops; the reader reaches W 300 frames into its second
        // block: 212 starved frames, then whole starved blocks.
        let first = out.read(N, 1324);
        assert_eq!(first.report.starved, 0);
        let second = out.read(N, 1324);
        assert_eq!((second.report.copied, second.report.starved), (300, 212));
        let third = out.read(N, 1324);
        assert_eq!(third.report.starved, N);
        let counters = out.reader.control().counters();
        assert_eq!(
            (counters.starved_renders, counters.starved_frames),
            (2, 212 + N as u64)
        );
        // Masked silence after a delay increase is not starvation: the new
        // position lies 3008 frames before the last frame played.
        cap.write(40 * N);
        out.reader.control().set_delay_frames(24_000);
        let blocks = lockstep_target(&mut cap, &mut out, 10);
        assert!(blocks.iter().all(|b| b.report.starved == 0));
        assert!(blocks.iter().any(|b| b.report.masked > 0));
        assert_eq!(out.reader.control().counters().starved_renders, 2);
    }

    // covers: S158, S159
    #[test]
    fn an_unanchored_read_asked_to_resync_counts_only_the_prime() {
        let ring = ring(1 << 18);
        let mut cap = Capture::new(&ring);
        cap.write(20_000);
        let mut out = Output::new(&ring);
        out.reader.control().set_delay_frames(4096);
        let block = out.read_target(N);
        assert_eq!(block.report.correction, Correction::Prime);
        assert_eq!(out.reader.control().counters().resyncs, 0);
        // The flag was taken by that read.
        cap.write(N);
        assert_eq!(out.read_target(N).report.correction, Correction::None);
        // The same holds after a prime reset.
        out.reader.control().reset_prime();
        out.reader.control().request_resync();
        cap.write(N);
        out.last = None;
        assert_eq!(out.read_target(N).report.correction, Correction::Prime);
        let counters = out.reader.control().counters();
        assert_eq!((counters.primes, counters.resyncs), (2, 0));
    }

    // covers: S159, S49, S45
    #[test]
    fn reset_prime_returns_the_reader_to_its_initial_state() {
        let ring = ring(1 << 18);
        let mut cap = Capture::new(&ring);
        cap.write(30_000);
        let mut out = Output::new(&ring);
        let last = settle(&mut cap, &mut out, 4096);
        // Requested from another thread; the next read takes it.
        let control = out.reader.control();
        std::thread::spawn(move || {
            control.set_delay_frames(12_000);
            control.reset_prime();
        })
        .join()
        .unwrap();
        cap.write(N);
        // F restarts with the prime, so frames played before may be played
        // again: no-repeat holds from the latest prime on.
        out.last = None;
        let block = out.read_target(N);
        assert_eq!(block.report.correction, Correction::Prime);
        assert_eq!(block.report.start, cap.w() - 12_000);
        assert_eq!(block.silent(), 0, "not masked by the old F");
        assert!(block.first().unwrap() <= last);
        assert_eq!(block.report.lag, 12_000 - N as i64);
        let counters = out.reader.control().counters();
        assert_eq!(
            (counters.primes, counters.resyncs),
            (2, 0),
            "counters are kept"
        );
        for b in lockstep_target(&mut cap, &mut out, 20) {
            assert_eq!(b.report.correction, Correction::None);
            assert_eq!(b.silent(), 0);
        }
    }
}
