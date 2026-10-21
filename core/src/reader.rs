//! Delay reader (spec section 5: S41-S59, with S158 and S159).
//!
//! Each local output reads the shared ring through its own [`Reader`], at its
//! own delay, with its own drift correction (S9, S58). A read emits exactly N
//! frames; the indices of the frames a reader emits since its latest prime are
//! strictly increasing (S49, S159), so no delay change, drift correction,
//! writer stall, starved render or overrun ever repeats audio. A reader
//! anchors (primes) once more than its delay is written and is silent until
//! then (S45). Drift is held inside a dead band of T frames (S50-S52, S57).
//! During a writer stall a reader plays buffered frames only while it stays
//! inside the dead band, then holds silence (S53); a reader that reaches W
//! is dry and makes no correction until the writer writes again (S44, S52).
//! A delay change is applied by the first read that finds unread frames, so
//! a change that reaches a dry reader waits for the writer; with a block
//! written before each read it is exact (S46).
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
pub const DRIFT_TOLERANCE_MS: u32 = 50;

/// The drift tolerance T at capture rate `rate`: `round(0.050 × rate)` frames
/// (S50); 2400 at 48 kHz. Computed in integers, so ties round up exactly.
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
    /// No re-positioning: the block follows the previous one, or the read
    /// was silent because the reader waits to prime (S45) or is dry (S44
    /// step 2).
    #[default]
    None,
    /// Not anchored (a new reader, or one after a prime reset) and more than
    /// De frames written: anchored at `W - De` and counted as a prime (S45,
    /// S158, S159).
    Prime,
    /// A requested resync, e.g. a delay change, applied by the first read
    /// that finds unread frames: re-anchored at `W - De` (S46).
    Resync,
    /// Behind by more than T: skipped forward to `W - De` (S51).
    SkipForward,
    /// The frames needed were overwritten (`P` below the oldest readable
    /// frame `W - C + M` once that is above 0, or the previous copy was
    /// overtaken by the writer): skipped forward to `W - De` (S55, S40).
    /// Counted as a drift skip.
    Overrun,
    /// Ahead by more than T, whether or not the writer moves, unless the
    /// reader is dry: stepped back to `W - De`; frames already played are
    /// masked (S52, S53).
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
    /// e = (W - P) - De for the P the read started from (0 while not
    /// anchored). Positive when behind, negative when ahead.
    pub error: i64,
    /// Frame index of the block's first frame (P after step 2).
    pub start: i64,
    /// The published `lagFrames`, `W - P` (S56).
    pub lag: i64,
    /// The re-positioning this read made, if any.
    pub correction: Correction,
    /// Leading frames output as silence because they were already played or
    /// are gone (S44 step 3), or the whole block of a read that waits to
    /// prime (S45). Masked silence is not starvation.
    pub masked: usize,
    /// Frames copied from the ring.
    pub copied: usize,
    /// Trailing frames output as silence because the read reached W before
    /// the block was full: a starved render (S53, S158). The whole block of
    /// a dry read (S44 step 2).
    pub starved: usize,
    /// The post-copy check failed and the copied frames were replaced by
    /// silence (S40); the next read skips forward (S55).
    pub rejected: bool,
    /// A drift correction this read would have made but held, because the
    /// read was made with corrections held ([`Reader::read_target_held`]):
    /// [`Correction::SkipForward`] or [`Correction::StepBack`], whose
    /// renderer fades out before an unheld read makes it (S188).
    /// [`Correction::None`] otherwise.
    pub due: Correction,
}

impl ReadReport {
    /// The frames at the block's start that the read filled with silence:
    /// the masked frames, or, when the post-copy check discarded the copy,
    /// those and the discarded ones (S184, S40). A block is this silence,
    /// then [`ReadReport::copied`] frames, then [`ReadReport::starved`]
    /// frames of silence (S44 step 3).
    pub fn leading_silence(&self) -> usize {
        if self.rejected {
            self.frames - self.starved
        } else {
            self.masked
        }
    }
}

/// Where a reader's next read would go on from ([`Reader::next_read`],
/// S206, S207). Plain data.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NextRead {
    /// P: the frame index the read would start at.
    pub position: i64,
    /// `W - P`, with W loaded when asked; 0 when the read would be dry.
    pub readable: i64,
    /// De for a read of the frames asked about (S44 step 1).
    pub delay: i64,
    /// T, the dead band (S50).
    pub tolerance: i64,
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

/// De for a read of `n` frames at delay `delay` (S44 step 1):
/// `min(max(D, N, Lr) + Jr, C - M - N)`, never negative.
fn effective_delay(
    delay: i64,
    n: i64,
    lead: i64,
    late: i64,
    capacity: usize,
    margin: usize,
) -> i64 {
    let ceiling = capacity as i64 - margin as i64 - n;
    delay
        .max(n)
        .max(lead)
        .saturating_add(late)
        .min(ceiling)
        .max(0)
}

/// The control thread's handle to a [`Reader`]: target delay, tolerance,
/// resync and prime-reset requests, the published lag and the counters.
/// Cheap to clone; every method is a lock-free atomic operation (S128).
#[derive(Debug, Clone)]
pub struct ReaderControl {
    shared: Arc<Shared>,
}

impl ReaderControl {
    /// Sets the target delay D in frames and requests a resync, which the
    /// next read that finds unread frames applies (S46); while the reader is
    /// dry, the change waits for the writer. The target is stored before the
    /// flag.
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

    /// Asks the reader to re-anchor at its requested delay, which the next
    /// read that finds unread frames does (S41, S46).
    pub fn request_resync(&self) {
        self.shared.resync.store(true, Ordering::Release);
    }

    /// Asks the next read to return the reader to its initial state: not
    /// anchored and F = 0, with the counters kept, and P kept only to feed
    /// `lagFrames` until the next prime. The next read that finds more than
    /// De frames written then anchors at `W - De` and counts a prime, whether
    /// or not the writer is running (S159).
    pub fn reset_prime(&self) {
        self.shared.prime.store(true, Ordering::Release);
    }

    /// The last published `lagFrames`, `W - P` (S56). Readable from any
    /// thread.
    pub fn lag_frames(&self) -> i64 {
        self.shared.lag.load(Ordering::Relaxed)
    }

    /// Whether this is `reader`'s control.
    pub(crate) fn controls(&self, reader: &Reader) -> bool {
        Arc::ptr_eq(&self.shared, &reader.shared)
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
    /// P: index of the next frame to read. Kept by a prime reset (S159).
    next: i64,
    /// F: one past the highest frame index emitted since the latest prime.
    emitted_end: i64,
    anchored: bool,
    /// The target delay `read_target` applies. Loaded from the control by
    /// every read while not anchored and by the read that applies a resync,
    /// never on its own.
    applied_delay: i64,
    /// Set when a copy failed the post-copy check (S40): the next read skips
    /// forward as for an overrun (S55).
    reanchor: bool,
    /// Lr: the floor of the effective delay that the reader's renderer sets
    /// (S206); 0 for a reader without one.
    lead: i64,
    /// Jr: how late the renderer's callback came, in capture frames (S209);
    /// 0 for a reader without one.
    late: i64,
    /// Jf: how far the floor Jr is measured from moved since the reader
    /// anchored, in capture frames (S209); drift is judged at `De + Jf`.
    floor_shift: i64,
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
    /// The tolerance starts at `round(0.050 × capture_rate)` (S50), the target
    /// delay at 0 (floored to one block, S42). Not anchored until its first
    /// read that finds more than De frames written (S43, S45).
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
            applied_delay: 0,
            reanchor: false,
            lead: 0,
            late: 0,
            floor_shift: 0,
            #[cfg(test)]
            before_check: None,
        }
    }

    /// Sets Lr, the floor of the effective delay (S206): every read uses
    /// `De = min(max(D, N, Lr), C - M - N)` (S44 step 1). A negative value
    /// counts as 0. It moves nothing by itself: the dead band absorbs a
    /// change, and the next prime, resync or correction lands on it. Called
    /// by the renderer that owns the reader, on its thread.
    pub fn set_lead_frames(&mut self, frames: i64) {
        self.lead = frames.max(0);
    }

    /// Lr, as last set.
    pub fn lead_frames(&self) -> i64 {
        self.lead
    }

    /// Sets Jr, the capture frames by which the renderer's callback came
    /// later than its earliest ones (S209): every read uses
    /// `De = min(max(D, N, Lr) + Jr, C - M - N)`. A negative value counts as
    /// 0. Called by the renderer that owns the reader, on its thread.
    pub fn set_late_frames(&mut self, frames: i64) {
        self.late = frames.max(0);
    }

    /// Jr, as last set.
    pub fn late_frames(&self) -> i64 {
        self.late
    }

    /// Sets Jf, how far the floor of the renderer's lateness moved since
    /// the reader's latest anchor, in capture frames (S209). Only the drift
    /// judgement uses it: a read judges its error `e = (W - P) - (De + Jf)`
    /// against the dead band, and anchors at `W - De` as before. Called by
    /// the renderer that owns the reader, on its thread.
    pub fn set_floor_shift_frames(&mut self, frames: i64) {
        self.floor_shift = frames;
    }

    /// The frames the next read can go on with (S206): `W - P`, with W
    /// loaded now, when a read of `frames` frames made now would go on from
    /// where the last one ended, with drift corrections held (`held`, S188)
    /// or not. `None` when it would anchor at `W - De` instead: the reader
    /// is not anchored, a prime reset, a resync or an overrun waits, or, not
    /// held, its error is outside the dead band. 0 when it would be dry.
    /// Changes nothing; called by the renderer that owns the reader, on its
    /// thread, never allocating, locking or blocking.
    pub fn readable_frames(&self, frames: usize, held: bool) -> Option<i64> {
        self.next_read(frames, held).map(|next| next.readable)
    }

    /// Where the next read would go on from (S206, S207): its P, what it
    /// can go on with ([`Reader::readable_frames`]), and its effective
    /// delay and dead band, under the same conditions; `None` when it would
    /// anchor at `W - De` instead. Changes nothing; called by the renderer
    /// that owns the reader, on its thread, never allocating, locking or
    /// blocking.
    pub fn next_read(&self, frames: usize, held: bool) -> Option<NextRead> {
        let waits =
            self.shared.prime.load(Ordering::Relaxed) || self.shared.resync.load(Ordering::Relaxed);
        if !self.anchored || self.reanchor || waits {
            return None;
        }
        let n = frames as i64;
        let (capacity, margin) = (self.ring.capacity(), self.ring.margin());
        let delay = effective_delay(
            self.applied_delay,
            n,
            self.lead,
            self.late,
            capacity,
            margin,
        );
        let tolerance = self.shared.tolerance.load(Ordering::Relaxed).max(0);
        let w = self.ring.frames_written() as i64;
        let position = self.next;
        let next = |readable| {
            Some(NextRead {
                position,
                readable,
                delay,
                tolerance,
            })
        };
        if position >= w {
            return next(0);
        }
        let oldest = self.ring.oldest_readable(w as u64) as i64;
        if oldest > 0 && position < oldest {
            return None;
        }
        if !held {
            let e = (w - position) - delay.saturating_add(self.floor_shift);
            if e > tolerance || e < -tolerance {
                return None;
            }
        }
        next(w - position)
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
    /// interface of S41. A pending prime reset is taken by this read; a
    /// pending resync waits for the first read that primes or finds unread
    /// frames (S44 step 2, S46).
    ///
    /// Never allocates, locks or blocks; O(N).
    pub fn read(
        &mut self,
        dst: &mut [f32],
        frames: usize,
        delay_frames: i64,
        tolerance_frames: i64,
    ) -> ReadReport {
        self.read_at(dst, frames, Some(delay_frames), tolerance_frames, false)
    }

    /// Reads at the delay and tolerance set through [`ReaderControl`].
    ///
    /// The reader keeps the delay it applies. It loads the control's target
    /// on every read while it is not anchored, and afterwards only on the
    /// read that applies a resync request. The control stores a target
    /// before raising its resync flag, so a read between the two keeps the
    /// old delay instead of applying the new one as a drift correction; the
    /// next read applies it as a resync (S46, S158). A dry read leaves the
    /// request for the first read that finds unread frames (S44 step 2). The
    /// flag is taken with acquire ordering before the target is loaded, so a
    /// flagged target is never stale (S133).
    pub fn read_target(&mut self, dst: &mut [f32], frames: usize) -> ReadReport {
        let tolerance = self.shared.tolerance.load(Ordering::Relaxed);
        self.read_at(dst, frames, None, tolerance, false)
    }

    /// [`Reader::read_target`] with drift corrections held (S188): a read
    /// that would skip forward (S51) or step back (S52, S53) leaves P alone,
    /// emits its block contiguously as if the error were inside the dead
    /// band, counts nothing, and reports the correction in
    /// [`ReadReport::due`]. Prime, resync, dry and overrun reads, and the
    /// masking of step 3, are as in an unheld read; the next unheld read
    /// makes a correction that is still due, against its own W.
    pub fn read_target_held(&mut self, dst: &mut [f32], frames: usize) -> ReadReport {
        let tolerance = self.shared.tolerance.load(Ordering::Relaxed);
        self.read_at(dst, frames, None, tolerance, true)
    }

    /// D for this read: the caller's (`given`), or else the control's
    /// target. The target is loaded only when `load_target` is set, which
    /// the caller does after taking the resync flag (S133), and is then kept
    /// as the delay this reader applies.
    fn delay(&mut self, given: Option<i64>, load_target: bool) -> i64 {
        if let Some(delay) = given {
            return delay;
        }
        if load_target {
            self.applied_delay = self.shared.delay.load(Ordering::Relaxed);
        }
        self.applied_delay
    }

    /// One read, S44 steps 1-4, at the caller's delay or at the target, with
    /// drift corrections held when `hold` is set (S188).
    fn read_at(
        &mut self,
        dst: &mut [f32],
        frames: usize,
        given: Option<i64>,
        tolerance: i64,
        hold: bool,
    ) -> ReadReport {
        if take(&self.shared.prime) {
            // S159: the initial state again, except for the counters and P,
            // which only feeds lagFrames until the next prime.
            self.emitted_end = 0;
            self.anchored = false;
            self.reanchor = false;
        }
        let frames = frames.min(dst.len() / 2);
        let n = frames as i64;
        let (capacity, margin) = (self.ring.capacity(), self.ring.margin());
        let (lead, late) = (self.lead, self.late);
        let effective = |delay: i64| effective_delay(delay, n, lead, late, capacity, margin);

        // Step 1.
        let w = self.ring.frames_written() as i64;
        let oldest = self.ring.oldest_readable(w as u64) as i64;
        let t = tolerance.max(0);
        let p = self.next;

        // Step 2: the first case that applies decides. The resync flag is
        // taken before a target is loaded.
        let mut due = Correction::None;
        let (de, correction) = if !self.anchored {
            // Prime, once more than De frames are written (S45, S159).
            let flagged = take(&self.shared.resync);
            let de = effective(self.delay(given, true));
            if w <= de {
                if flagged {
                    // Left as it was: the read that primes clears it.
                    self.shared.resync.store(true, Ordering::Release);
                }
                return self.silent(dst, frames, w, de, false);
            }
            (de, Correction::Prime)
        } else if p >= w {
            // Dry: P never passes W, so the writer has written nothing since
            // the reader reached W. No correction, whatever D is, and a
            // pending resync waits for the first read that finds P < W.
            let de = effective(self.delay(given, false));
            return self.silent(dst, frames, w, de, true);
        } else if take(&self.shared.resync) {
            (effective(self.delay(given, true)), Correction::Resync)
        } else {
            // Drift. Only frames really overwritten make an overrun: a
            // negative P left by a resync early in a session, while
            // `oldest` is still 0, reads silence in step 3 and follows the
            // dead-band rules.
            let de = effective(self.delay(given, false));
            let e = (w - p) - de.saturating_add(self.floor_shift);
            let correction = if self.reanchor || (oldest > 0 && p < oldest) {
                Correction::Overrun
            } else if e > t {
                Correction::SkipForward
            } else if e < -t {
                Correction::StepBack
            } else {
                Correction::None
            };
            if hold && matches!(correction, Correction::SkipForward | Correction::StepBack) {
                // S188: the block follows the previous one; the renderer
                // fades out before an unheld read makes the correction.
                due = correction;
                (de, Correction::None)
            } else {
                (de, correction)
            }
        };
        let e = if correction == Correction::Prime {
            0
        } else {
            (w - p) - de
        };
        if correction != Correction::None {
            self.next = w - de;
            self.anchored = true;
            self.reanchor = false;
        }
        let p = self.next;

        // Step 3. The block splits into masked [p, lo), copied [lo, lim) and
        // starved [lim, end).
        let ring = &*self.ring;
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
            due,
        }
    }

    /// A read that ends in step 2 (S44): N frames of silence, with P, F and
    /// the resync flag left as they are. A reader waiting to prime (S45)
    /// publishes `W - P` and counts nothing; its block counts as masked. A
    /// dry reader publishes 0 and counts a starved render of N frames (S158).
    fn silent(&self, dst: &mut [f32], frames: usize, w: i64, de: i64, dry: bool) -> ReadReport {
        dst[..frames * 2].fill(0.0);
        let (lag, error, masked, starved) = if dry {
            (0, (w - self.next) - de, 0, frames)
        } else {
            (w - self.next, 0, frames, 0)
        };
        self.shared.lag.store(lag, Ordering::Relaxed);
        self.count(Correction::None, 0, starved, false);
        ReadReport {
            frames,
            written: w,
            delay: de,
            error,
            start: self.next,
            lag,
            correction: Correction::None,
            masked,
            copied: 0,
            starved,
            rejected: false,
            due: Correction::None,
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
    const T: i64 = 2400;

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

    // covers: S45, S158
    #[test]
    fn a_fresh_session_stays_silent_until_the_ring_holds_more_than_the_delay() {
        let ring = ring(1 << 18);
        let mut cap = Capture::new(&ring);
        cap.write(1000);
        let mut out = Output::new(&ring);
        let mut blocks = Vec::new();
        for _ in 0..20 {
            blocks.push(out.read(N, 4096));
            cap.write(N);
        }
        // The reads at W = 1000, 1512, ..., 4072 wait in silence; the one at
        // W = 4584 primes at 488, and frames 0 to 487 are never played.
        for b in &blocks[..7] {
            assert_eq!(b.report.correction, Correction::None);
            assert!(b.report.written <= 4096);
            assert_eq!(b.silent(), N);
            assert_eq!(b.report.lag, b.report.written, "W - P while P is 0");
        }
        assert_eq!(blocks[7].report.written, 4584);
        assert_eq!(blocks[7].report.correction, Correction::Prime);
        assert_eq!(blocks[7].first(), Some(488));
        for pair in blocks[7..].windows(2) {
            assert_eq!(pair[1].silent(), 0);
            assert_eq!(pair[1].first(), Some(pair[0].last().unwrap() + 1));
        }
        assert_eq!(
            out.reader.control().counters(),
            ReaderCounters {
                primes: 1,
                ..ReaderCounters::default()
            },
            "the silent reads count nothing"
        );
    }

    // covers: S45, S44, S56, S158
    #[test]
    fn a_new_reader_primes_on_the_first_read_that_finds_more_than_de_written() {
        let ring = ring(1 << 18);
        let mut cap = Capture::new(&ring);
        let mut out = Output::new(&ring);
        // N = 512, De = 1200, one block written before each read.
        for w in [512, 1024] {
            cap.write(N);
            let b = out.read(N, 1200);
            assert_eq!(b.report.written, w);
            assert_eq!(b.report.correction, Correction::None);
            assert_eq!(b.silent(), N);
            assert_eq!(
                (b.report.masked, b.report.copied, b.report.starved),
                (N, 0, 0)
            );
            assert_eq!(b.report.start, 0);
            assert_eq!(b.report.lag, w, "W - P while P is still 0");
            assert_eq!(out.reader.lag_frames(), w);
        }
        assert_eq!(
            out.reader.control().counters(),
            ReaderCounters::default(),
            "a reader waiting to prime counts nothing"
        );
        cap.write(N);
        let b = out.read(N, 1200);
        assert_eq!(b.report.correction, Correction::Prime);
        assert_eq!(b.report.start, 336);
        assert_eq!((b.first(), b.last()), (Some(336), Some(847)));
        assert_eq!(b.silent(), 0);
        assert_eq!(b.report.lag, 688);
        for b in lockstep(&mut cap, &mut out, 1200, 20) {
            assert_eq!(b.report.correction, Correction::None);
            assert_eq!(b.silent(), 0);
            assert_eq!(b.report.lag, 688);
        }
        assert_eq!(
            out.reader.control().counters(),
            ReaderCounters {
                primes: 1,
                ..ReaderCounters::default()
            }
        );
    }

    // covers: S45
    #[test]
    fn a_read_with_exactly_de_frames_written_still_waits() {
        let ring = ring(1 << 18);
        let mut cap = Capture::new(&ring);
        let mut out = Output::new(&ring);
        cap.write(1200);
        let b = out.read(N, 1200);
        assert_eq!(b.report.correction, Correction::None);
        assert_eq!(b.silent(), N);
        assert_eq!(b.report.lag, 1200);
        cap.write(1);
        let b = out.read(N, 1200);
        assert_eq!(b.report.correction, Correction::Prime);
        assert_eq!(
            b.report.start, 1,
            "the read that primes starts at W - De >= 1"
        );
        assert_eq!(b.first(), Some(1));
        assert_eq!(b.silent(), 0);
        assert_eq!(b.report.lag, 688);
    }

    // covers: S45, S46, S158
    #[test]
    fn a_read_that_primes_uses_the_current_target_and_clears_the_resync_flag() {
        let ring = ring(1 << 18);
        let mut cap = Capture::new(&ring);
        let mut out = Output::new(&ring);
        let control = out.reader.control();
        control.set_delay_frames(4096);
        for _ in 0..4 {
            cap.write(N);
            let b = out.read_target(N);
            assert_eq!(b.report.correction, Correction::None);
            assert_eq!((b.report.delay, b.silent()), (4096, N));
        }
        // A newer target, shorter than what is written by now: the next read
        // primes with it.
        control.set_delay_frames(1500);
        cap.write(N);
        let b = out.read_target(N);
        assert_eq!(b.report.correction, Correction::Prime);
        assert_eq!((b.report.delay, b.report.start), (1500, 2560 - 1500));
        // That read took the resync flag: the next one is no resync.
        cap.write(N);
        assert_eq!(out.read_target(N).report.correction, Correction::None);
        let counters = control.counters();
        assert_eq!((counters.primes, counters.resyncs), (1, 0));
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

    // covers: S42, S44, S206
    #[test]
    fn a_reader_is_floored_at_its_lead() {
        let ring = ring(1 << 18);
        let mut cap = Capture::new(&ring);
        cap.write(8 * N);
        let mut out = Output::new(&ring);
        assert_eq!(out.reader.lead_frames(), 0);
        out.reader.set_lead_frames(1075);
        // Below the lead, the reader primes at the lead ...
        let block = out.read(N, 0);
        assert_eq!(block.report.correction, Correction::Prime);
        assert_eq!(block.report.delay, 1075);
        assert_eq!(block.report.start, cap.w() - 1075);
        assert_eq!(block.report.lag, 1075 - N as i64);
        // ... and at or above it, the delay is the delay.
        cap.write(N);
        assert_eq!(out.read(N, 1075).report.delay, 1075);
        cap.write(N);
        assert_eq!(out.read(N, 3000).report.delay, 3000);
        // The cap still wins, and a negative lead counts as none.
        out.reader.set_lead_frames(1 << 30);
        cap.write(N);
        let cap_frames = (1 << 18) - MIN_MARGIN_FRAMES as i64 - N as i64;
        assert_eq!(out.read(N, 0).report.delay, cap_frames);
        out.reader.set_lead_frames(-5);
        assert_eq!(out.reader.lead_frames(), 0);
        cap.write(N);
        assert_eq!(out.read(N, 0).report.delay, N as i64);
    }

    // covers: S206, S207
    #[test]
    fn a_reader_tells_what_its_next_read_can_go_on_with() {
        let ring = ring(1 << 18);
        let mut cap = Capture::new(&ring);
        cap.write(8 * N);
        let mut out = Output::new(&ring);
        let control = out.reader.control();
        let n = N as i64;
        // Not anchored: the next read primes.
        assert_eq!(out.reader.readable_frames(N, false), None);
        control.set_delay_frames(1200);
        assert_eq!(out.read_target(N).report.correction, Correction::Prime);
        assert_eq!(out.reader.readable_frames(N, false), Some(1200 - n));
        // Where it goes on from, at which delay and dead band (S207).
        assert_eq!(
            out.reader.next_read(N, false),
            Some(NextRead {
                position: 9 * n - 1200,
                readable: 1200 - n,
                delay: 1200,
                tolerance: control.tolerance_frames(),
            })
        );
        cap.write(N);
        assert_eq!(out.reader.readable_frames(N, true), Some(1200));
        // A resync waits: the next read anchors, held or not.
        control.request_resync();
        assert_eq!(out.reader.readable_frames(N, true), None);
        assert_eq!(out.read_target(N).report.correction, Correction::Resync);
        assert_eq!(out.reader.readable_frames(N, false), Some(1200 - n));
        // More than T behind: an unheld read skips forward, a held one goes
        // on (S188).
        cap.write(6 * N);
        assert_eq!(out.reader.readable_frames(N, false), None);
        assert_eq!(out.reader.readable_frames(N, true), Some(1200 - n + 6 * n));
        assert_eq!(
            out.read_target(N).report.correction,
            Correction::SkipForward
        );
        // A reader at W would be dry.
        out.read_target(N);
        assert_eq!(out.read_target(N).report.starved, N - 176);
        assert_eq!(out.reader.readable_frames(N, false), Some(0));
        // A prime reset waits: the next read anchors again.
        control.reset_prime();
        cap.write(N);
        assert_eq!(out.reader.readable_frames(N, true), None);
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

    /// The L run of S46: a 24 s ring holding 48 000 frames, then 50 lockstep
    /// rounds at D = 0 (De = N), each read ending at W; then the writer
    /// freezes and five reads at D = 0 run dry. Ends with W = P = F = 73 600.
    fn frozen_at_the_live_edge() -> (Capture, Output) {
        let ring = Ring::for_rate(RATE).unwrap();
        let mut cap = Capture::new(&ring);
        cap.write(48_000);
        let mut out = Output::new(&ring);
        let blocks = lockstep(&mut cap, &mut out, 0, 50);
        assert_eq!(blocks[0].report.correction, Correction::Prime);
        assert_eq!(blocks[0].report.start, 48_000);
        assert!(blocks.iter().all(|b| b.silent() == 0 && b.report.lag == 0));
        assert_eq!((cap.w(), out.last), (73_600, Some(73_599)));
        for _ in 0..5 {
            assert_dry(&out.read(N, 0), 73_600);
        }
        (cap, out)
    }

    /// A dry read (S44 step 2): N frames of silence at `P = W`, no
    /// correction, a starved render of N frames, `lagFrames = 0`.
    fn assert_dry(block: &Block, w: i64) {
        let r = &block.report;
        assert_eq!(r.correction, Correction::None, "no anchor while dry");
        assert_eq!((r.written, r.start, r.lag), (w, w, 0));
        assert_eq!((r.masked, r.copied, r.starved), (0, 0, r.frames));
    }

    // covers: S44, S46, S41, S49, S56, S158
    #[test]
    fn a_change_on_a_dry_reader_waits_for_the_writer_then_lands_exactly() {
        let (mut cap, mut out) = frozen_at_the_live_edge();
        out.reader.control().request_resync();
        for _ in 0..5 {
            let b = out.read(N, 1200);
            assert_dry(&b, 73_600);
            assert_eq!(b.report.delay, 1200);
        }
        assert_eq!(
            out.reader.control().counters(),
            ReaderCounters {
                primes: 1,
                starved_renders: 10,
                starved_frames: 5120,
                ..ReaderCounters::default()
            },
            "the change waits: no resync, no drift correction"
        );
        // The writer resumes. The first read finds P < W and applies the
        // change against its W; all 512 frames lie below F, so it is silent.
        cap.write(N);
        let b = out.read(N, 1200);
        assert_eq!(b.report.correction, Correction::Resync);
        assert_eq!((b.report.written, b.report.start), (74_112, 72_912));
        assert_eq!((b.report.masked, b.silent()), (N, N));
        assert_eq!(b.report.lag, 688);
        // 176 masked frames, then the first frame it never played.
        cap.write(N);
        let b = out.read(N, 1200);
        assert_eq!(b.report.correction, Correction::None);
        assert_eq!(b.report.error, 0);
        assert_eq!(b.report.masked, 176);
        assert_eq!((b.first(), b.last()), (Some(73_600), Some(73_935)));
        assert_eq!(b.report.lag, 688);
        // From then on every block is contiguous and exactly De - N behind.
        let mut last = b.last().unwrap();
        for b in lockstep(&mut cap, &mut out, 1200, 100) {
            assert_eq!(b.report.correction, Correction::None);
            assert_eq!(b.first(), Some(last + 1), "nothing repeated or skipped");
            last = b.last().unwrap();
            assert_eq!(b.report.written - 1 - last, 688);
            assert_eq!(b.report.lag, 688);
        }
        let counters = out.reader.control().counters();
        assert_eq!((counters.primes, counters.resyncs), (1, 1));
        assert_eq!(counters.drift_skips + counters.drift_silences, 0);
        assert_eq!(
            (counters.starved_renders, counters.starved_frames),
            (10, 5120)
        );
    }

    // covers: S46, S47, S52, S158
    #[test]
    fn a_long_change_on_a_dry_reader_lands_exactly_after_the_resume() {
        let (mut cap, mut out) = frozen_at_the_live_edge();
        out.reader.control().request_resync();
        for _ in 0..5 {
            // Far more than T from its new target, and still no step back.
            assert_dry(&out.read(N, 24_000), 73_600);
        }
        cap.write(N);
        let b = out.read(N, 24_000);
        assert_eq!(b.report.correction, Correction::Resync);
        assert_eq!(b.report.start, 74_112 - 24_000);
        let mut blocks = vec![b];
        while blocks.last().unwrap().first().is_none() {
            cap.write(N);
            blocks.push(out.read(N, 24_000));
        }
        assert_eq!(blocks.len(), 46, "audio returns in the 46th block");
        assert_eq!(blocks[45].first(), Some(73_600));
        assert_eq!(blocks.iter().map(Block::silent).sum::<usize>(), 23_488);
        for b in &blocks[1..] {
            assert_eq!(b.report.correction, Correction::None);
        }
        for b in &blocks {
            assert_eq!(b.report.lag, 23_488);
        }
        for b in lockstep(&mut cap, &mut out, 24_000, 50) {
            assert_eq!(b.report.correction, Correction::None);
            assert_eq!(b.silent(), 0);
            assert_eq!(b.report.lag, 23_488);
        }
        let counters = out.reader.control().counters();
        assert_eq!((counters.primes, counters.resyncs), (1, 1));
        assert_eq!(counters.drift_skips + counters.drift_silences, 0);
        assert_eq!(
            (counters.starved_renders, counters.starved_frames),
            (10, 5120)
        );
    }

    // covers: S46, S48
    #[test]
    fn a_change_on_a_dry_reader_skips_ahead_when_more_than_de_arrives() {
        let (mut cap, mut out) = frozen_at_the_live_edge();
        out.reader.control().request_resync();
        assert_dry(&out.read(N, 1200), 73_600);
        // The writer comes back with 4096 frames at once: W - De lies past F.
        cap.write(4096);
        let b = out.read(N, 1200);
        assert_eq!(b.report.correction, Correction::Resync);
        assert_eq!(b.report.start, 77_696 - 1200);
        assert_eq!((b.silent(), b.first()), (0, Some(76_496)));
        assert_eq!(b.report.lag, 688);
        let counters = out.reader.control().counters();
        assert_eq!(counters.resyncs, 1);
        assert_eq!(counters.drift_skips, 0, "a resync, not a drift skip");
    }

    // covers: S46, S41, S133, S158
    #[test]
    fn a_target_set_on_a_dry_reader_waits_and_the_newest_one_is_applied() {
        let (mut cap, mut out) = frozen_at_the_live_edge();
        let control = out.reader.control();
        control.set_delay_frames(1200);
        for _ in 0..3 {
            let b = out.read_target(N);
            assert_dry(&b, 73_600);
            assert_eq!(b.report.delay, N as i64, "still the delay it applies");
        }
        // A newer change supersedes the one still waiting.
        control.set_delay_frames(3000);
        for _ in 0..3 {
            assert_dry(&out.read_target(N), 73_600);
        }
        cap.write(N);
        let b = out.read_target(N);
        assert_eq!(b.report.correction, Correction::Resync);
        assert_eq!((b.report.delay, b.report.start), (3000, 74_112 - 3000));
        assert_eq!(b.report.lag, 3000 - N as i64);
        for b in lockstep_target(&mut cap, &mut out, 20) {
            assert_eq!(b.report.correction, Correction::None);
            assert_eq!(b.report.lag, 3000 - N as i64);
        }
        let counters = control.counters();
        assert_eq!(
            (counters.primes, counters.resyncs),
            (1, 1),
            "one resync, for the newer change"
        );
        assert_eq!(counters.drift_skips + counters.drift_silences, 0);
    }

    // covers: S52, S53, S44, S54, S158
    #[test]
    fn a_dry_reader_makes_no_correction_until_the_writer_writes_again() {
        let ring = ring(1 << 18);
        let mut cap = Capture::new(&ring);
        cap.write(20_000);
        let mut out = Output::new(&ring);
        settle(&mut cap, &mut out, 2048);
        let w = cap.w();
        // The writer stops. Inside the dead band the reader plays what is
        // buffered and reaches W after three blocks ...
        for k in 1..=3 {
            let b = out.read(N, 2048);
            assert_eq!(b.report.correction, Correction::None);
            assert_eq!(b.report.error, -k * N as i64);
            assert_eq!(b.silent(), 0);
        }
        assert_eq!((out.last, out.reader.lag_frames()), (Some(w - 1), 0));
        // ... and then outputs starved blocks: 2048 ahead of its target,
        // more than T, yet no step back while it is dry.
        for _ in 0..20 {
            let b = out.read(N, 2048);
            assert_dry(&b, w);
            assert_eq!(b.report.error, -2048);
        }
        let counters = out.reader.control().counters();
        assert_eq!(counters.drift_silences, 0);
        assert_eq!(
            (counters.starved_renders, counters.starved_frames),
            (20, 20 * N as u64)
        );
        // The writer writes again: the new frames play as they arrive, with
        // the error inside the dead band.
        let blocks = lockstep(&mut cap, &mut out, 2048, 10);
        assert_eq!(blocks[0].first(), Some(w));
        for b in &blocks {
            assert_eq!(b.report.correction, Correction::None);
            assert_eq!((b.silent(), b.report.lag), (0, 0));
            assert!(b.report.error.abs() <= T);
        }
    }

    // covers: S46, S53
    #[test]
    fn a_change_on_the_first_read_after_the_writer_stops_is_applied_at_once() {
        let ring = Ring::for_rate(RATE).unwrap();
        let mut cap = Capture::new(&ring);
        cap.write(48_000);
        let mut out = Output::new(&ring);
        lockstep(&mut cap, &mut out, 4096, 200);
        assert_eq!((cap.w(), out.last), (150_400, Some(146_815)));
        assert_eq!(out.reader.lag_frames(), 3584);
        // The writer stops, and the next read comes with the change.
        out.reader.control().request_resync();
        let b = out.read(N, 5296);
        assert_eq!(b.report.correction, Correction::Resync, "P < W: at once");
        assert_eq!(b.report.start, 145_104);
        assert_eq!(b.report.masked, N);
        assert_eq!(b.report.lag, 4784);
        let lags: Vec<i64> = (0..2).map(|_| out.read(N, 5296).report.lag).collect();
        assert_eq!(lags, [4272, 3760]);
        // The writer resumes; the error the stall left is inside the dead
        // band, so nothing corrects it and the change lands 1024 short.
        cap.write(N);
        let b = out.read(N, 5296);
        assert_eq!(
            (b.report.correction, b.report.error),
            (Correction::None, -1024)
        );
        assert_eq!(b.report.masked, 176);
        assert_eq!(b.first(), Some(146_816));
        assert_eq!(b.report.lag, 3760);
        for b in lockstep(&mut cap, &mut out, 5296, 20) {
            assert_eq!(b.report.correction, Correction::None);
            assert_eq!(b.report.lag, 3760);
        }
    }

    // covers: S46, S52, S53
    #[test]
    fn a_change_applied_during_a_stall_lands_short_by_the_stalled_reads() {
        // After the change, j more reads with the writer stopped leave it
        // j blocks short; the step back at the fifth, the first more than T
        // ahead, starts the count again.
        for (j, short) in [
            (0, 0),
            (1, 512),
            (2, 1024),
            (3, 1536),
            (4, 2048),
            (5, 0),
            (6, 512),
        ] {
            let ring = ring(1 << 18);
            let mut cap = Capture::new(&ring);
            cap.write(20_000);
            let mut out = Output::new(&ring);
            settle(&mut cap, &mut out, 4096);
            out.reader.control().request_resync();
            let b = out.read(N, 5296);
            assert_eq!(b.report.correction, Correction::Resync);
            assert_eq!(b.report.lag, 4784);
            for _ in 0..j {
                out.read(N, 5296);
            }
            for b in lockstep(&mut cap, &mut out, 5296, 20) {
                assert_eq!(b.report.correction, Correction::None, "j = {j}");
                assert_eq!(b.report.lag, 4784 - short, "j = {j}");
            }
        }
    }

    // covers: S46, S52, S53, S54
    #[test]
    fn a_change_applied_during_a_stall_is_lost_once_the_reader_runs_dry() {
        let ring = ring(1 << 18);
        let mut cap = Capture::new(&ring);
        cap.write(20_000);
        let mut out = Output::new(&ring);
        settle(&mut cap, &mut out, 4096);
        let w = cap.w();
        // The writer stops; the next read takes a change to 2000 frames.
        out.reader.control().request_resync();
        let b = out.read(N, 2000);
        assert_eq!(b.report.correction, Correction::Resync);
        assert_eq!((b.first(), b.report.lag), (Some(w - 2000), 1488));
        // Three more reads play what is buffered and reach W ...
        let lags: Vec<i64> = (0..3).map(|_| out.read(N, 2000).report.lag).collect();
        assert_eq!(lags, [976, 464, 0]);
        assert_eq!(out.last, Some(w - 1));
        // ... where the reader is dry: more than T ahead, but no step back.
        for _ in 0..5 {
            assert_dry(&out.read(N, 2000), w);
        }
        // The writer resumes; the error stays inside the dead band, so the
        // change lands De - N short, at lagFrames = 0.
        for b in lockstep(&mut cap, &mut out, 2000, 20) {
            assert_eq!(b.report.correction, Correction::None);
            assert_eq!((b.silent(), b.report.lag), (0, 0));
        }
        assert_eq!(out.reader.control().counters().drift_silences, 0);
    }

    // covers: S50
    #[test]
    fn tolerance_is_50_ms_rounded() {
        assert_eq!(tolerance_frames(48_000), 2400);
        assert_eq!(tolerance_frames(44_100), 2205);
        assert_eq!(tolerance_frames(22_050), 1103, "1102.5 rounds up");
        assert_eq!(tolerance_frames(8_000), 400);
        assert_eq!(tolerance_frames(32_000), 1600);
        assert_eq!(tolerance_frames(96_000), 4800);
        assert_eq!(tolerance_frames(384_000), 19_200);
        let ring = ring(1 << 16);
        let reader = Reader::new(ring, 44_100);
        assert_eq!(reader.control().tolerance_frames(), 2205);
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
        for k in 1..=4 {
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
        assert_eq!(block.report.error, -5 * N as i64);
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
        assert_eq!(silent, 5 * N, "|e| frames of silence, never a repeat");
        assert_eq!(
            next,
            Some(before + 4 * N as i64 + 1),
            "the first frame never played"
        );
        let counters = out.reader.control().counters();
        assert_eq!(
            (counters.drift_silences, counters.drift_silence_frames),
            (1, 5 * N as u64)
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
            if read <= 4 {
                assert_eq!(
                    block.silent(),
                    0,
                    "buffered frames while inside the dead band"
                );
            } else {
                assert_eq!(block.silent(), N, "silence held instead of draining");
            }
            let steps_back = read % 5 == 0;
            assert_eq!(block.report.correction == Correction::StepBack, steps_back);
        }
        assert_eq!(
            played,
            (before + 1..before + 1 + 4 * N as i64).collect::<Vec<_>>()
        );
        let counters = out.reader.control().counters();
        assert_eq!(counters.drift_silences, 20);
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
        for _ in 0..12 {
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

    // covers: S44, S55, S158
    #[test]
    fn a_negative_p_early_in_a_session_is_never_an_overrun() {
        // C - M = 57 344; De at its cap, C - M - N.
        let ring = ring(1 << 16);
        let de = (1 << 16) - MIN_MARGIN_FRAMES as i64 - N as i64;
        let mut cap = Capture::new(&ring);
        cap.write(10_000);
        let mut out = Output::new(&ring);
        assert_eq!(out.read(N, 1024).report.correction, Correction::Prime);
        // A resync to the capped delay this early leaves P negative.
        out.reader.control().request_resync();
        cap.write(N);
        let b = out.read(N, de);
        assert_eq!(b.report.correction, Correction::Resync);
        assert_eq!(b.report.start, 10_512 - de);
        let p = b.report.start + N as i64;
        // 1500 frames later P lies below W - C + M, but nothing was ever
        // overwritten (oldest = 0): the error of 988 frames is inside the
        // dead band, so nothing is corrected and the frames stay masked.
        cap.write(1500);
        assert!(p < cap.w() - (1 << 16) + MIN_MARGIN_FRAMES as i64);
        assert_eq!(ring.oldest_readable(cap.w() as u64), 0);
        let b = out.read(N, de);
        assert_eq!(b.report.correction, Correction::None, "not an overrun");
        assert_eq!((b.report.start, b.report.error), (p, 988));
        assert_eq!(b.report.masked, N);
        assert_eq!(out.reader.control().counters().drift_skips, 0);
    }

    // covers: S40, S55, S184
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
        assert_eq!(
            (block.report.leading_silence(), block.report.copied),
            (N, 0),
            "a discarded copy counts as silence at the block's start (S184)"
        );
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

    // covers: S158, S46, S133
    #[test]
    fn a_delay_stored_before_its_resync_flag_waits_for_the_flag() {
        let ring = ring(1 << 18);
        let mut cap = Capture::new(&ring);
        cap.write(60_000);
        let mut out = Output::new(&ring);
        settle_target(&mut cap, &mut out, 4096, 10);
        // The control thread has stored a new target and not yet raised the
        // resync flag when the output's callback runs.
        out.reader.shared.delay.store(24_000, Ordering::Relaxed);
        let block = lockstep_target(&mut cap, &mut out, 1).pop().unwrap();
        assert_eq!(block.report.correction, Correction::None);
        assert_eq!(block.report.lag, 4096 - N as i64, "still the old delay");
        // The flag follows: the next read applies the new delay as a resync.
        out.reader.control().request_resync();
        let block = lockstep_target(&mut cap, &mut out, 1).pop().unwrap();
        assert_eq!(block.report.correction, Correction::Resync);
        assert_eq!(block.report.lag, 24_000 - N as i64);
        let counters = out.reader.control().counters();
        assert_eq!((counters.primes, counters.resyncs), (1, 1));
        assert_eq!(
            counters.drift_skips + counters.drift_silences,
            0,
            "no drift correction"
        );
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

    // covers: S159, S45, S56, S158
    #[test]
    fn after_a_prime_reset_p_only_feeds_the_lag_until_the_next_prime() {
        let ring = ring(1 << 18);
        let mut cap = Capture::new(&ring);
        cap.write(20_000);
        let mut out = Output::new(&ring);
        settle(&mut cap, &mut out, 4096);
        let p = cap.w() - (4096 - N as i64);
        out.reader.control().reset_prime();
        // Not anchored, with no more than De written: each read waits in
        // silence, keeps P, publishes W - P and counts nothing.
        for _ in 0..3 {
            cap.write(N);
            let b = out.read(N, 60_000);
            assert_eq!(b.report.correction, Correction::None);
            assert_eq!(b.silent(), N);
            assert_eq!(b.report.start, p);
            assert_eq!(b.report.lag, cap.w() - p);
        }
        assert_eq!(
            out.reader.control().counters(),
            ReaderCounters {
                primes: 1,
                ..ReaderCounters::default()
            }
        );
        // The first read with W > De primes like a new reader's first read.
        cap.write(40_000);
        out.last = None;
        let b = out.read(N, 60_000);
        assert_eq!(b.report.correction, Correction::Prime);
        assert_eq!(b.report.start, cap.w() - 60_000);
        assert_eq!(b.silent(), 0);
        assert_eq!(b.report.lag, 60_000 - N as i64);
        assert_eq!(out.reader.control().counters().primes, 2);
    }

    // covers: S159, S46, S53, S158
    #[test]
    fn a_prime_reset_on_a_dry_reader_primes_at_once() {
        let (mut cap, mut out) = frozen_at_the_live_edge();
        // A change waits on the dry reader ...
        out.reader.control().request_resync();
        assert_dry(&out.read(N, 1200), 73_600);
        // ... but a prime reset does not wait for the writer.
        out.reader.control().reset_prime();
        out.last = None;
        let b = out.read(N, 1200);
        assert_eq!(b.report.correction, Correction::Prime);
        assert_eq!(b.report.start, 73_600 - 1200);
        assert_eq!(b.silent(), 0, "F restarted");
        assert_eq!(b.report.lag, 688);
        // Two more reads with the writer stopped reach W; then it is dry.
        assert_eq!(out.read(N, 1200).report.lag, 176);
        let b = out.read(N, 1200);
        assert_eq!(
            (b.report.copied, b.report.starved, b.report.lag),
            (176, 336, 0)
        );
        assert_dry(&out.read(N, 1200), 73_600);
        // The writer resumes, and the reader plays at lagFrames = 0.
        for b in lockstep(&mut cap, &mut out, 1200, 5) {
            assert_eq!(b.report.correction, Correction::None);
            assert_eq!((b.silent(), b.report.lag), (0, 0));
        }
        let counters = out.reader.control().counters();
        assert_eq!(
            (counters.primes, counters.resyncs),
            (2, 0),
            "only the prime"
        );
        assert_eq!(counters.drift_skips + counters.drift_silences, 0);
    }
}
