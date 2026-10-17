//! Shared delay ring buffer (spec section 4: S35-S40, with S133 and S135).
//!
//! One ring per engine holds interleaved stereo float32 frames. Exactly one
//! writer (the capture thread) stores frames and publishes `framesWritten`
//! (W); any number of readers copy frames out at their own delay (see
//! `reader.rs`). The writer never waits for, or looks at, a reader: once the
//! ring is full each write overwrites the oldest frames.
//!
//! Samples are stored as relaxed atomic 32-bit words holding the float bit
//! patterns (S135), so a reader racing an overwrite is a detected stale read,
//! never undefined behavior. Ordering comes from the W release/acquire pair,
//! the writer's release fence after each publish (S37) and the reader's
//! acquire fence before its post-copy check (S40).

#![forbid(unsafe_code)]

use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering, fence};

/// Smallest safety margin M, in frames (S40).
pub const MIN_MARGIN_FRAMES: usize = 8192;

/// Seconds of audio a ring holds at its capture rate (S36).
pub const RING_SECONDS: u32 = 24;

/// Lowest capture rate the engine accepts, in Hz (S16).
pub const MIN_CAPTURE_RATE: u32 = 8_000;

/// Highest capture rate the engine accepts, in Hz (S16).
pub const MAX_CAPTURE_RATE: u32 = 384_000;

/// Why a ring could not be created or used at a rate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RingError {
    /// The margin is below [`MIN_MARGIN_FRAMES`].
    MarginTooSmall,
    /// The capacity is below twice the margin.
    CapacityTooSmall,
    /// The rate is outside [`MIN_CAPTURE_RATE`]..=[`MAX_CAPTURE_RATE`].
    UnsupportedRate,
}

impl fmt::Display for RingError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let text = match self {
            RingError::MarginTooSmall => "ring margin is below 8192 frames",
            RingError::CapacityTooSmall => "ring capacity is below twice its margin",
            RingError::UnsupportedRate => "capture rate is outside 8000..=384000 Hz",
        };
        f.write_str(text)
    }
}

impl std::error::Error for RingError {}

/// Capacity, in frames, of a ring that holds [`RING_SECONDS`] at `rate` (S36).
pub fn capacity_for_rate(rate: u32) -> usize {
    RING_SECONDS as usize * rate as usize
}

/// Whether `rate` is a capture rate the engine accepts (S16).
pub fn is_supported_rate(rate: u32) -> bool {
    (MIN_CAPTURE_RATE..=MAX_CAPTURE_RATE).contains(&rate)
}

/// The shared delay ring (S35).
///
/// Created once, before any audio thread starts (S128); shared with readers
/// through `Arc`. The single writer is obtained with [`Ring::writer`].
pub struct Ring {
    /// Two words (L, R) per frame, float bit patterns (S135).
    words: Box<[AtomicU32]>,
    /// C, in frames.
    capacity: u64,
    /// M, in frames.
    margin: u64,
    /// W: frames written since the last reset (S37).
    written: AtomicU64,
    /// Set while a [`RingWriter`] exists.
    writer_taken: AtomicBool,
}

impl fmt::Debug for Ring {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Ring")
            .field("capacity", &self.capacity)
            .field("margin", &self.margin)
            .field("written", &self.frames_written())
            .finish()
    }
}

impl Ring {
    /// Creates a ring of `capacity` frames with a safety margin of `margin`
    /// frames. The margin must be at least [`MIN_MARGIN_FRAMES`] and the
    /// capacity at least twice the margin.
    pub fn new(capacity: usize, margin: usize) -> Result<Arc<Ring>, RingError> {
        if margin < MIN_MARGIN_FRAMES {
            return Err(RingError::MarginTooSmall);
        }
        if capacity < margin.saturating_mul(2) {
            return Err(RingError::CapacityTooSmall);
        }
        let words: Box<[AtomicU32]> = (0..capacity * 2).map(|_| AtomicU32::new(0)).collect();
        Ok(Arc::new(Ring {
            words,
            capacity: capacity as u64,
            margin: margin as u64,
            written: AtomicU64::new(0),
            writer_taken: AtomicBool::new(false),
        }))
    }

    /// Creates a ring that holds [`RING_SECONDS`] of audio at `rate`, with the
    /// minimum margin (S36).
    pub fn for_rate(rate: u32) -> Result<Arc<Ring>, RingError> {
        if !is_supported_rate(rate) {
            return Err(RingError::UnsupportedRate);
        }
        Ring::new(capacity_for_rate(rate), MIN_MARGIN_FRAMES)
    }

    /// Whether this ring can serve a session at capture rate `rate`: the rate
    /// is supported and the ring holds at least [`RING_SECONDS`] of it (S36).
    /// A ring sized for the highest rate it serves rejects higher rates.
    pub fn supports_rate(&self, rate: u32) -> bool {
        is_supported_rate(rate) && self.capacity >= capacity_for_rate(rate) as u64
    }

    /// C, in frames.
    pub fn capacity(&self) -> usize {
        self.capacity as usize
    }

    /// M, in frames.
    pub fn margin(&self) -> usize {
        self.margin as usize
    }

    /// W, the frames written since the last reset, loaded with acquire
    /// ordering (S37). This is also the engine's `framesCaptured` (S133).
    pub fn frames_written(&self) -> u64 {
        self.written.load(Ordering::Acquire)
    }

    /// The readable index range `[max(0, W - C + M), W)` for the current W
    /// (S40).
    pub fn readable(&self) -> (u64, u64) {
        let w = self.frames_written();
        (self.oldest_readable(w), w)
    }

    /// `max(0, w - C + M)`: the oldest readable index when W is `w` (S40).
    pub fn oldest_readable(&self, w: u64) -> u64 {
        (w + self.margin).saturating_sub(self.capacity)
    }

    /// Takes the ring's only writer, or returns `None` while another
    /// [`RingWriter`] exists (S35).
    pub fn writer(self: &Arc<Self>) -> Option<RingWriter> {
        self.writer_taken
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .ok()?;
        Some(RingWriter {
            ring: Arc::clone(self),
        })
    }

    /// Copies `dst.len() / 2` frames, starting at frame index `first`, into
    /// `dst` with relaxed loads. The caller validates the copy afterwards with
    /// [`Ring::copy_still_valid`] (S40).
    pub(crate) fn load_frames(&self, first: u64, dst: &mut [f32]) {
        let frames = (dst.len() / 2).min(self.capacity as usize);
        let start = (first % self.capacity) as usize;
        let head = frames.min(self.capacity as usize - start);
        let (dst_head, dst_tail) = dst[..frames * 2].split_at_mut(head * 2);
        load_words(&self.words[start * 2..(start + head) * 2], dst_head);
        load_words(&self.words[..(frames - head) * 2], dst_tail);
    }

    /// The post-copy check of S40: after an acquire fence, re-loads W as W2
    /// and returns whether `lowest`, the lowest copied index, is still at
    /// least `W2 - C + M`. When it returns false, the copy may hold frames of
    /// a later block and must not be played.
    pub(crate) fn copy_still_valid(&self, lowest: u64) -> bool {
        fence(Ordering::Acquire);
        let w2 = self.written.load(Ordering::Relaxed);
        lowest + self.capacity >= w2 + self.margin
    }
}

/// The ring's single writer (S35), used by the capture thread.
///
/// Dropping it lets [`Ring::writer`] hand out a new one.
pub struct RingWriter {
    ring: Arc<Ring>,
}

impl fmt::Debug for RingWriter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RingWriter")
            .field("ring", &self.ring)
            .finish()
    }
}

impl RingWriter {
    /// The ring this writer writes to.
    pub fn ring(&self) -> &Arc<Ring> {
        &self.ring
    }

    /// Writes `frames` interleaved stereo frames from `interleaved` (at most
    /// `interleaved.len() / 2`) and advances W by that count (S38). Any count
    /// is accepted; when it exceeds C only the last C frames remain.
    ///
    /// The frames are stored in chunks of at most M frames. After each chunk
    /// W is published with release ordering and a release fence follows, so
    /// the writer only ever stores into the slots of frames
    /// `[W - C, W - C + M)` (S37, S40). Never waits, allocates or locks.
    pub fn write(&mut self, interleaved: &[f32], frames: usize) {
        let ring = &*self.ring;
        let frames = frames.min(interleaved.len() / 2);
        let capacity = ring.capacity as usize;
        let margin = ring.margin as usize;
        // Only this writer stores W, so its own last store is the current W.
        let mut w = ring.written.load(Ordering::Relaxed);
        let mut src = &interleaved[..frames * 2];
        while !src.is_empty() {
            let n = (src.len() / 2).min(margin);
            let (chunk, rest) = src.split_at(n * 2);
            let start = (w % ring.capacity) as usize;
            let head = n.min(capacity - start);
            store_words(
                &ring.words[start * 2..(start + head) * 2],
                &chunk[..head * 2],
            );
            store_words(&ring.words[..(n - head) * 2], &chunk[head * 2..]);
            w += n as u64;
            // S37: publish W only after the frames it counts are stored ...
            ring.written.store(w, Ordering::Release);
            // ... and fence before any sample of the next chunk is stored.
            fence(Ordering::Release);
            src = rest;
        }
    }

    /// Sets W to 0, which makes all history unreadable (S39). Call it only
    /// while no capture callback runs and no reader exists.
    pub fn reset(&mut self) {
        self.ring.written.store(0, Ordering::Release);
        fence(Ordering::Release);
    }
}

impl Drop for RingWriter {
    fn drop(&mut self) {
        self.ring.writer_taken.store(false, Ordering::Release);
    }
}

fn store_words(dst: &[AtomicU32], src: &[f32]) {
    for (word, sample) in dst.iter().zip(src) {
        word.store(sample.to_bits(), Ordering::Relaxed);
    }
}

fn load_words(src: &[AtomicU32], dst: &mut [f32]) {
    for (sample, word) in dst.iter_mut().zip(src) {
        *sample = f32::from_bits(word.load(Ordering::Relaxed));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(i: u64) -> [f32; 2] {
        let tag = (i + 1) as u32;
        [f32::from_bits(tag), f32::from_bits(!tag)]
    }

    fn block(first: u64, frames: usize) -> Vec<f32> {
        (first..first + frames as u64).flat_map(frame).collect()
    }

    fn index_of(l: f32, r: f32) -> Option<u64> {
        let (a, b) = (l.to_bits(), r.to_bits());
        (a != 0 && b == !a).then(|| u64::from(a) - 1)
    }

    fn read_raw(ring: &Ring, first: u64, frames: usize) -> Vec<f32> {
        let mut out = vec![0.0; frames * 2];
        ring.load_frames(first, &mut out);
        out
    }

    // covers: S35, S2
    #[test]
    fn one_writer_interleaved_stereo_fixed_capacity() {
        let ring = Ring::new(16_384, 8192).unwrap();
        assert_eq!(ring.capacity(), 16_384);
        assert_eq!(ring.margin(), 8192);
        let writer = ring.writer().expect("first writer");
        assert!(ring.writer().is_none(), "a second writer must be refused");
        drop(writer);
        assert!(
            ring.writer().is_some(),
            "dropping the writer frees the slot"
        );
        let mut writer = ring.writer().unwrap();
        writer.write(&[0.25, -0.5, 0.75, -1.0], 2);
        assert_eq!(read_raw(&ring, 0, 2), vec![0.25, -0.5, 0.75, -1.0]);
    }

    // covers: S35
    #[test]
    fn full_ring_overwrites_oldest_without_waiting() {
        let ring = Ring::new(16_384, 8192).unwrap();
        let mut writer = ring.writer().unwrap();
        // No reader exists and nobody drains the ring; writes never block.
        for k in 0..10u64 {
            writer.write(&block(k * 4096, 4096), 4096);
        }
        let w = ring.frames_written();
        assert_eq!(w, 40_960);
        let newest = read_raw(&ring, w - 1, 1);
        assert_eq!(index_of(newest[0], newest[1]), Some(w - 1));
        let oldest_kept = read_raw(&ring, w - 16_384, 1);
        assert_eq!(index_of(oldest_kept[0], oldest_kept[1]), Some(w - 16_384));
        // Frame w - C - 1 shares its slot with frame w - 1, which overwrote it.
        let overwritten = read_raw(&ring, w - 16_384 - 1, 1);
        assert_eq!(index_of(overwritten[0], overwritten[1]), Some(w - 1));
    }

    // covers: S36, S16
    #[test]
    fn rings_are_sized_for_24_seconds() {
        assert_eq!(capacity_for_rate(48_000), 1_152_000);
        assert_eq!(capacity_for_rate(192_000), 4_608_000);
        let ring = Ring::for_rate(48_000).unwrap();
        assert_eq!(ring.capacity(), 1_152_000);
        assert!(ring.supports_rate(48_000));
        assert!(ring.supports_rate(44_100));
        assert!(
            !ring.supports_rate(96_000),
            "a 48 kHz ring is too short for 96 kHz"
        );
        assert_eq!(
            Ring::for_rate(7_999).unwrap_err(),
            RingError::UnsupportedRate
        );
        assert_eq!(
            Ring::for_rate(384_001).unwrap_err(),
            RingError::UnsupportedRate
        );
        assert!(is_supported_rate(8_000) && is_supported_rate(384_000));
        // A ring sized for 192 kHz serves every rate up to 192 kHz and rejects higher ones.
        let big = Ring::new(capacity_for_rate(192_000), MIN_MARGIN_FRAMES).unwrap();
        for rate in [8_000, 32_000, 44_100, 48_000, 96_000, 192_000] {
            assert!(big.supports_rate(rate), "{rate}");
        }
        assert!(!big.supports_rate(384_000));
        // The cap that leaves room for the 20 s maximum delay (S77) plus margin and a block.
        assert!(capacity_for_rate(48_000) - MIN_MARGIN_FRAMES - 8192 >= 20 * 48_000);
    }

    // covers: S40
    #[test]
    fn margin_and_capacity_are_validated() {
        assert_eq!(
            Ring::new(65_536, 4096).unwrap_err(),
            RingError::MarginTooSmall
        );
        assert_eq!(
            Ring::new(16_000, 8192).unwrap_err(),
            RingError::CapacityTooSmall
        );
        assert!(Ring::new(16_384, 8192).is_ok());
    }

    // covers: S37
    #[test]
    fn frames_written_is_a_monotonic_total() {
        let ring = Ring::new(16_384, 8192).unwrap();
        let mut writer = ring.writer().unwrap();
        let mut expected = 0u64;
        for frames in [1usize, 7, 512, 0, 8192, 3] {
            writer.write(&block(expected, frames), frames);
            expected += frames as u64;
            assert_eq!(ring.frames_written(), expected);
        }
        // A frame count larger than the slice is clamped to the slice.
        writer.write(&block(expected, 2), 10);
        assert_eq!(ring.frames_written(), expected + 2);
    }

    // covers: S38
    #[test]
    fn oversized_write_keeps_the_last_capacity_frames() {
        let ring = Ring::new(16_384, 8192).unwrap();
        let mut writer = ring.writer().unwrap();
        writer.write(&block(0, 100), 100);
        let big = 3 * 16_384 + 1000;
        writer.write(&block(100, big), big);
        let w = ring.frames_written();
        assert_eq!(w, 100 + big as u64, "W grows by the full count");
        let kept = read_raw(&ring, w - 16_384, 16_384);
        for (i, f) in kept.chunks_exact(2).enumerate() {
            assert_eq!(index_of(f[0], f[1]), Some(w - 16_384 + i as u64));
        }
    }

    // covers: S39, S17
    #[test]
    fn reset_clears_history() {
        let ring = Ring::new(16_384, 8192).unwrap();
        let mut writer = ring.writer().unwrap();
        writer.write(&block(0, 5000), 5000);
        assert_eq!(ring.readable(), (0, 5000));
        writer.reset();
        assert_eq!(ring.frames_written(), 0, "framesCaptured restarts at 0");
        assert_eq!(ring.readable(), (0, 0), "nothing is readable after a reset");
        writer.write(&block(1_000_000, 10), 10);
        assert_eq!(ring.readable(), (0, 10));
        let fresh = read_raw(&ring, 0, 10);
        assert_eq!(index_of(fresh[0], fresh[1]), Some(1_000_000));
    }

    // covers: S40
    #[test]
    fn readable_window_is_w_minus_c_plus_m_to_w() {
        let ring = Ring::new(20_000, 8192).unwrap();
        let mut writer = ring.writer().unwrap();
        assert_eq!(ring.readable(), (0, 0));
        writer.write(&block(0, 11_000), 11_000);
        assert_eq!(
            ring.readable(),
            (0, 11_000),
            "max(0, W - C + M) is 0 early on"
        );
        writer.write(&block(11_000, 30_000), 30_000);
        let w = 41_000;
        assert_eq!(ring.readable(), (w - 20_000 + 8192, w));
        assert_eq!(ring.oldest_readable(5), 0);
    }

    // covers: S40, S37
    #[test]
    fn post_copy_check_rejects_a_copy_overtaken_by_the_writer() {
        let ring = Ring::new(16_384, 8192).unwrap();
        let mut writer = ring.writer().unwrap();
        writer.write(&block(0, 16_384), 16_384);
        let (oldest, w) = ring.readable();
        assert_eq!((oldest, w), (8192, 16_384));
        let copy = read_raw(&ring, oldest, 100);
        assert_eq!(index_of(copy[0], copy[1]), Some(oldest));
        assert!(
            ring.copy_still_valid(oldest),
            "nothing moved: the copy is good"
        );
        // The writer advances by one more block while the reader is "paused".
        writer.write(&block(16_384, 512), 512);
        assert!(
            !ring.copy_still_valid(oldest),
            "frames below W2 - C + M may have been overwritten"
        );
        assert!(ring.copy_still_valid(oldest + 512));
    }

    // covers: S135, S123
    #[test]
    fn samples_round_trip_bit_exactly_including_nan_payloads() {
        let ring = Ring::new(16_384, 8192).unwrap();
        let mut writer = ring.writer().unwrap();
        let odd = [
            f32::NAN,
            f32::INFINITY,
            f32::NEG_INFINITY,
            f32::from_bits(0x7fa0_0001), // signaling NaN payload
            f32::from_bits(0xffc0_1234), // negative quiet NaN payload
            -0.0,
            f32::MIN_POSITIVE / 2.0, // subnormal
            1.0e30,
        ];
        writer.write(&odd, 4);
        let back = read_raw(&ring, 0, 4);
        for (a, b) in odd.iter().zip(&back) {
            assert_eq!(a.to_bits(), b.to_bits());
        }
    }

    proptest::proptest! {
        #![proptest_config(proptest::test_runner::Config::with_cases(64))]

        // covers: S35, S38, S40
        #[test]
        fn readable_frames_always_hold_their_own_index(
            sizes in proptest::collection::vec(1usize..=20_000, 1..12),
        ) {
            let ring = Ring::new(16_384, 8192).unwrap();
            let mut writer = ring.writer().unwrap();
            let mut next = 0u64;
            for n in sizes {
                writer.write(&block(next, n), n);
                next += n as u64;
                let (oldest, w) = ring.readable();
                proptest::prop_assert_eq!(w, next);
                proptest::prop_assert_eq!(oldest, (w + 8192).saturating_sub(16_384));
                let copy = read_raw(&ring, oldest, (w - oldest) as usize);
                for (i, f) in copy.chunks_exact(2).enumerate() {
                    proptest::prop_assert_eq!(index_of(f[0], f[1]), Some(oldest + i as u64));
                }
            }
        }
    }

    // covers: S37, S40, S133, S135
    #[test]
    fn concurrent_reads_never_see_torn_or_stale_frames() {
        use std::sync::atomic::AtomicBool;
        let ring = Ring::new(16_384, 8192).unwrap();
        let mut writer = ring.writer().unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let reader_ring = Arc::clone(&ring);
        let reader_stop = Arc::clone(&stop);
        let reader = std::thread::spawn(move || {
            let mut buf = vec![0.0f32; 4096 * 2];
            let (mut good, mut rejected) = (0u64, 0u64);
            while !reader_stop.load(Ordering::Relaxed) {
                let w = reader_ring.frames_written();
                let oldest = reader_ring.oldest_readable(w);
                if w - oldest < 4096 {
                    continue;
                }
                // Read the oldest readable frames: the ones the writer reaches first.
                reader_ring.load_frames(oldest, &mut buf);
                if !reader_ring.copy_still_valid(oldest) {
                    rejected += 1;
                    continue;
                }
                for (i, f) in buf.chunks_exact(2).enumerate() {
                    assert_eq!(
                        index_of(f[0], f[1]),
                        Some(oldest + i as u64),
                        "torn or stale frame"
                    );
                }
                good += 1;
            }
            (good, rejected)
        });
        let mut next = 0u64;
        let sizes = [1usize, 64, 511, 512, 4096, 8192, 777];
        let started = std::time::Instant::now();
        while started.elapsed() < std::time::Duration::from_millis(300) {
            for &n in &sizes {
                writer.write(&block(next, n), n);
                next += n as u64;
            }
        }
        stop.store(true, Ordering::Relaxed);
        let (good, _rejected) = reader.join().unwrap();
        assert!(good > 0, "the reader validated at least one copy");
    }
}
