//! The delay ring, its writer and its readers through the C interface (spec
//! sections 4 and 5).
//!
//! A ring handle, its writer and its readers each hold the ring by reference
//! count, so they can be destroyed in any order; the frames are freed with the
//! last of them, on the thread that destroys it (S128, S134). The writer runs
//! on the capture thread. A reader handle has two sides in separate fields:
//! the [`Reader`] that its output thread reads with, and the
//! [`ReaderControl`] that every other call uses, whose methods are lock-free
//! atomic operations (S133). A call borrows only the field it needs, so a read
//! and a control call may run at the same time.

use std::ptr;
use std::sync::Arc;

use super::{host_slice, host_slice_mut, stereo_samples};
use crate::clock;
use crate::reader::{Correction, ReadReport, Reader, ReaderControl, ReaderCounters};
use crate::ring::{self, MIN_MARGIN_FRAMES, Ring, RingWriter};

/// Smallest ring capacity the C interface accepts: twice the margin (S40);
/// the header's `ASC_RING_MIN_FRAMES`.
pub const MIN_RING_FRAMES: usize = 2 * MIN_MARGIN_FRAMES;

/// Largest ring capacity the C interface accepts: 24 s at the highest capture
/// rate (S16, S36); the header's `ASC_RING_MAX_FRAMES`.
pub const MAX_RING_FRAMES: usize = ring::RING_SECONDS as usize * ring::MAX_CAPTURE_RATE as usize;

/// `AscReadReport.correction` for [`Correction::None`].
pub const CORRECTION_NONE: i32 = 0;
/// `AscReadReport.correction` for [`Correction::Prime`].
pub const CORRECTION_PRIME: i32 = 1;
/// `AscReadReport.correction` for [`Correction::Resync`].
pub const CORRECTION_RESYNC: i32 = 2;
/// `AscReadReport.correction` for [`Correction::SkipForward`].
pub const CORRECTION_SKIP_FORWARD: i32 = 3;
/// `AscReadReport.correction` for [`Correction::Overrun`].
pub const CORRECTION_OVERRUN: i32 = 4;
/// `AscReadReport.correction` for [`Correction::StepBack`].
pub const CORRECTION_STEP_BACK: i32 = 5;

/// A delay ring behind an opaque C pointer (S35). Only shared access.
pub struct AscRing {
    ring: Arc<Ring>,
}

/// The ring's single writer behind an opaque C pointer (S35).
pub struct AscRingWriter {
    writer: RingWriter,
}

/// A delay reader behind an opaque C pointer (S41): the output thread's
/// [`Reader`] and the [`ReaderControl`] for every other thread.
pub struct AscReader {
    control: ReaderControl,
    reader: Reader,
}

/// [`ReadReport`] for C.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AscReadReport {
    pub frames: usize,
    pub written: i64,
    pub delay: i64,
    pub error: i64,
    pub start: i64,
    pub lag: i64,
    pub correction: i32,
    pub masked: usize,
    pub copied: usize,
    pub starved: usize,
    pub rejected: bool,
}

/// [`ReaderCounters`] for C (S158).
#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AscReaderCounters {
    pub primes: u64,
    pub resyncs: u64,
    pub drift_skips: u64,
    pub drift_skip_frames: u64,
    pub drift_silences: u64,
    pub drift_silence_frames: u64,
    pub starved_renders: u64,
    pub starved_frames: u64,
    pub rejected_copies: u64,
}

/// The C code of a correction.
fn correction_code(correction: Correction) -> i32 {
    match correction {
        Correction::None => CORRECTION_NONE,
        Correction::Prime => CORRECTION_PRIME,
        Correction::Resync => CORRECTION_RESYNC,
        Correction::SkipForward => CORRECTION_SKIP_FORWARD,
        Correction::Overrun => CORRECTION_OVERRUN,
        Correction::StepBack => CORRECTION_STEP_BACK,
    }
}

impl From<ReadReport> for AscReadReport {
    fn from(report: ReadReport) -> Self {
        AscReadReport {
            frames: report.frames,
            written: report.written,
            delay: report.delay,
            error: report.error,
            start: report.start,
            lag: report.lag,
            correction: correction_code(report.correction),
            masked: report.masked,
            copied: report.copied,
            starved: report.starved,
            rejected: report.rejected,
        }
    }
}

impl From<ReaderCounters> for AscReaderCounters {
    fn from(counters: ReaderCounters) -> Self {
        AscReaderCounters {
            primes: counters.primes,
            resyncs: counters.resyncs,
            drift_skips: counters.drift_skips,
            drift_skip_frames: counters.drift_skip_frames,
            drift_silences: counters.drift_silences,
            drift_silence_frames: counters.drift_silence_frames,
            starved_renders: counters.starved_renders,
            starved_frames: counters.starved_frames,
            rejected_copies: counters.rejected_copies,
        }
    }
}

/// The ring of a ring handle.
///
/// # Safety
///
/// `ring` is null or a live handle from [`asc_ring_create`].
pub(super) unsafe fn ring_of<'a>(ring: *const AscRing) -> Option<&'a Arc<Ring>> {
    // SAFETY: by the caller's contract the pointer is null or a live handle,
    // which is only ever borrowed shared.
    unsafe { ring.as_ref() }.map(|handle| &handle.ring)
}

/// The control side of a reader handle.
///
/// # Safety
///
/// `reader` is null or a live handle from [`asc_reader_create`].
unsafe fn control_of<'a>(reader: *const AscReader) -> Option<&'a ReaderControl> {
    if reader.is_null() {
        return None;
    }
    // SAFETY: the handle is live; this borrows only the `control` field,
    // which the output thread's reads never borrow.
    Some(unsafe { &(*reader).control })
}

/// Writes `value` to `report` unless it is null.
///
/// # Safety
///
/// `report` is null or points to an aligned, writable [`AscReadReport`] that
/// nothing else touches during the call.
unsafe fn put_report(report: *mut AscReadReport, value: AscReadReport) {
    if !report.is_null() {
        // SAFETY: non-null, and by the caller's contract aligned, writable
        // and not touched by anyone else.
        unsafe { report.write(value) };
    }
}

/// Runs `read` on the output side of a reader handle with the destination
/// block, and returns its report; an all-zero report, without reading, when
/// the block is empty or the handle or the block is missing.
///
/// # Safety
///
/// `reader` is null or a live handle whose reads run on one thread at a
/// time; `dst` is null or points to `frames` × 2 aligned, writable floats
/// that nothing else touches during the call.
unsafe fn read_with(
    reader: *mut AscReader,
    dst: *mut f32,
    frames: usize,
    read: impl FnOnce(&mut Reader, &mut [f32]) -> ReadReport,
) -> AscReadReport {
    // A read of 0 frames leaves a pending resync or prime reset to the next
    // read (S46). Taken here, it would anchor a block that doesn't advance
    // with the writer: the next read would start late by whatever was written
    // in between, inside the dead band, so nothing would ever correct it.
    if frames == 0 {
        return AscReadReport::default();
    }
    let Some(samples) = stereo_samples(frames) else {
        return AscReadReport::default();
    };
    if reader.is_null() {
        return AscReadReport::default();
    }
    // SAFETY: by the caller's contract `dst` is null or holds `samples`
    // writable floats that only this call touches.
    let Some(block) = (unsafe { host_slice_mut(dst, samples) }) else {
        return AscReadReport::default();
    };
    // SAFETY: the handle is live and its reads run on one thread at a time;
    // this borrows only the `reader` field, which control calls never borrow.
    let reader = unsafe { &mut (*reader).reader };
    read(reader, block).into()
}

/// Ring capacity for 24 s at `rate`, or 0 for an unsupported rate (S16, S36).
#[unsafe(no_mangle)]
pub extern "C" fn asc_ring_capacity_for_rate(rate: u32) -> usize {
    if ring::is_supported_rate(rate) {
        ring::capacity_for_rate(rate)
    } else {
        0
    }
}

/// Creates an empty ring of `capacity_frames` frames with the smallest margin
/// (S35, S40); null outside [`MIN_RING_FRAMES`]..=[`MAX_RING_FRAMES`].
#[unsafe(no_mangle)]
pub extern "C" fn asc_ring_create(capacity_frames: usize) -> *mut AscRing {
    if !(MIN_RING_FRAMES..=MAX_RING_FRAMES).contains(&capacity_frames) {
        return ptr::null_mut();
    }
    match Ring::new(capacity_frames, MIN_MARGIN_FRAMES) {
        Ok(ring) => Box::into_raw(Box::new(AscRing { ring })),
        Err(_) => ptr::null_mut(),
    }
}

/// Destroys a ring handle; its writer and readers keep the frames alive.
///
/// # Safety
///
/// `ring` is null or a pointer returned by [`asc_ring_create`] that was not
/// destroyed yet, and no other call on it runs now or later.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn asc_ring_destroy(ring: *mut AscRing) {
    if ring.is_null() {
        return;
    }
    // SAFETY: the pointer came from Box::into_raw in asc_ring_create, and by
    // the caller's contract it is destroyed once, with no call in progress.
    drop(unsafe { Box::from_raw(ring) });
}

/// The ring's capacity C in frames.
///
/// # Safety
///
/// `ring` is null or a live ring handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn asc_ring_capacity(ring: *const AscRing) -> usize {
    // SAFETY: by the caller's contract the handle is null or live.
    unsafe { ring_of(ring) }.map_or(0, |ring| ring.capacity())
}

/// Whether the ring can serve a session at `rate` (S16, S36).
///
/// # Safety
///
/// `ring` is null or a live ring handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn asc_ring_supports_rate(ring: *const AscRing, rate: u32) -> bool {
    // SAFETY: by the caller's contract the handle is null or live.
    unsafe { ring_of(ring) }.is_some_and(|ring| ring.supports_rate(rate))
}

/// Any thread: W, the frames written since the last reset, which is also the
/// session's frames captured (S17, S37, S133).
///
/// # Safety
///
/// `ring` is null or a live ring handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn asc_ring_frames_written(ring: *const AscRing) -> u64 {
    // SAFETY: by the caller's contract the handle is null or live.
    unsafe { ring_of(ring) }.map_or(0, |ring| ring.frames_written())
}

/// Any thread: the largest sample magnitude written since the last call,
/// for one level meter polling at its own pace; 0 for a null handle.
///
/// # Safety
///
/// `ring` is null or a live ring handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn asc_ring_take_peak(ring: *const AscRing) -> f32 {
    // SAFETY: by the caller's contract the handle is null or live.
    unsafe { ring_of(ring) }.map_or(0.0, |ring| ring.take_peak())
}

/// Takes the ring's only writer; null while another one exists (S35).
///
/// # Safety
///
/// `ring` is null or a live ring handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn asc_ring_writer_create(ring: *mut AscRing) -> *mut AscRingWriter {
    // SAFETY: by the caller's contract the handle is null or live.
    let Some(ring) = (unsafe { ring_of(ring) }) else {
        return ptr::null_mut();
    };
    match ring.writer() {
        Some(writer) => Box::into_raw(Box::new(AscRingWriter { writer })),
        None => ptr::null_mut(),
    }
}

/// Destroys the writer, so the ring can hand out a new one.
///
/// # Safety
///
/// `writer` is null or a pointer returned by [`asc_ring_writer_create`] that
/// was not destroyed yet, and no other call on it runs now or later.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn asc_ring_writer_destroy(writer: *mut AscRingWriter) {
    if writer.is_null() {
        return;
    }
    // SAFETY: the pointer came from Box::into_raw in asc_ring_writer_create,
    // and by the caller's contract it is destroyed once, with no call in
    // progress.
    drop(unsafe { Box::from_raw(writer) });
}

/// Capture thread: appends `frames` stereo frames (S35, S37, S38, S40) as
/// one block, stamped with the time it is written at on the core's clock
/// (S207). Real-time safe.
///
/// # Safety
///
/// `writer` is null or a live writer handle that no other call uses now;
/// `interleaved` is null or points to `frames` × 2 aligned floats that
/// nothing writes during the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn asc_ring_write(
    writer: *mut AscRingWriter,
    interleaved: *const f32,
    frames: usize,
) {
    let Some(samples) = stereo_samples(frames) else {
        return;
    };
    // SAFETY: by the caller's contract the buffer is null or holds `samples`
    // readable floats that nothing writes during the call.
    let Some(block) = (unsafe { host_slice(interleaved, samples) }) else {
        return;
    };
    // SAFETY: by the caller's contract the handle is null or live and only
    // this call uses it now.
    if let Some(handle) = unsafe { writer.as_mut() } {
        handle.writer.write_at(block, frames, clock::now_ns());
    }
}

/// Control thread, at a session start: sets W to 0, which makes all history
/// unreadable (S39). Only while no capture callback runs and no reader of
/// the ring exists.
///
/// # Safety
///
/// `writer` is null or a live writer handle that no other call uses now.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn asc_ring_writer_reset(writer: *mut AscRingWriter) {
    // SAFETY: by the caller's contract the handle is null or live and only
    // this call uses it now.
    if let Some(handle) = unsafe { writer.as_mut() } {
        handle.writer.reset();
    }
}

/// Creates a reader of `ring` for a session captured at `capture_rate`
/// (S41, S43, S50); null for an unsupported rate (S16) or one the ring is too
/// small for (S36).
///
/// # Safety
///
/// `ring` is null or a live ring handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn asc_reader_create(
    ring: *mut AscRing,
    capture_rate: u32,
) -> *mut AscReader {
    // SAFETY: by the caller's contract the handle is null or live.
    let Some(ring) = (unsafe { ring_of(ring) }) else {
        return ptr::null_mut();
    };
    // A ring too short for the rate would cap the delay without a word.
    if !ring.supports_rate(capture_rate) {
        return ptr::null_mut();
    }
    let reader = Reader::new(Arc::clone(ring), capture_rate);
    let control = reader.control();
    Box::into_raw(Box::new(AscReader { control, reader }))
}

/// Destroys a reader.
///
/// # Safety
///
/// `reader` is null or a pointer returned by [`asc_reader_create`] that was
/// not destroyed yet, and no other call on it runs now or later.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn asc_reader_destroy(reader: *mut AscReader) {
    if reader.is_null() {
        return;
    }
    // SAFETY: the pointer came from Box::into_raw in asc_reader_create, and
    // by the caller's contract it is destroyed once, with no call in progress.
    drop(unsafe { Box::from_raw(reader) });
}

/// Output thread: reads `frames` frames at the target delay
/// ([`Reader::read_target`], S41-S59). A read of 0 frames does nothing and
/// leaves pending requests to the next read. Real-time safe.
///
/// # Safety
///
/// `reader` is null or a live reader handle whose reads run on one thread at
/// a time; `dst` is null or points to `frames` × 2 aligned, writable floats
/// and `report` is null or points to an aligned, writable [`AscReadReport`],
/// neither touched by anything else during the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn asc_reader_read(
    reader: *mut AscReader,
    dst: *mut f32,
    frames: usize,
    report: *mut AscReadReport,
) {
    // SAFETY: the caller's contract is read_with's.
    let done = unsafe { read_with(reader, dst, frames, |r, block| r.read_target(block, frames)) };
    // SAFETY: by the caller's contract `report` is null or writable.
    unsafe { put_report(report, done) };
}

/// Output thread: reads `frames` frames at `delay_frames` with
/// `tolerance_frames` (the interface of S41). Real-time safe.
///
/// # Safety
///
/// As for [`asc_reader_read`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn asc_reader_read_at(
    reader: *mut AscReader,
    dst: *mut f32,
    frames: usize,
    delay_frames: i64,
    tolerance_frames: i64,
    report: *mut AscReadReport,
) {
    // SAFETY: the caller's contract is read_with's.
    let done = unsafe {
        read_with(reader, dst, frames, |r, block| {
            r.read(block, frames, delay_frames, tolerance_frames)
        })
    };
    // SAFETY: by the caller's contract `report` is null or writable.
    unsafe { put_report(report, done) };
}

/// Any thread: sets the target delay and requests a resync (S46).
///
/// # Safety
///
/// `reader` is null or a live reader handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn asc_reader_set_delay_frames(reader: *mut AscReader, frames: i64) {
    // SAFETY: by the caller's contract the handle is null or live.
    if let Some(control) = unsafe { control_of(reader) } {
        control.set_delay_frames(frames);
    }
}

/// Any thread: the target delay in frames.
///
/// # Safety
///
/// `reader` is null or a live reader handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn asc_reader_delay_frames(reader: *const AscReader) -> i64 {
    // SAFETY: by the caller's contract the handle is null or live.
    unsafe { control_of(reader) }.map_or(0, ReaderControl::delay_frames)
}

/// Any thread: sets the drift tolerance T in frames (S50).
///
/// # Safety
///
/// `reader` is null or a live reader handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn asc_reader_set_tolerance_frames(reader: *mut AscReader, frames: i64) {
    // SAFETY: by the caller's contract the handle is null or live.
    if let Some(control) = unsafe { control_of(reader) } {
        control.set_tolerance_frames(frames);
    }
}

/// Any thread: the drift tolerance T in frames.
///
/// # Safety
///
/// `reader` is null or a live reader handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn asc_reader_tolerance_frames(reader: *const AscReader) -> i64 {
    // SAFETY: by the caller's contract the handle is null or live.
    unsafe { control_of(reader) }.map_or(0, ReaderControl::tolerance_frames)
}

/// Any thread: asks the reader to re-anchor at the target delay, which its
/// next read with unread frames does (S41, S46).
///
/// # Safety
///
/// `reader` is null or a live reader handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn asc_reader_request_resync(reader: *mut AscReader) {
    // SAFETY: by the caller's contract the handle is null or live.
    if let Some(control) = unsafe { control_of(reader) } {
        control.request_resync();
    }
}

/// Any thread: asks the next read to start over like a new reader (S159).
///
/// # Safety
///
/// `reader` is null or a live reader handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn asc_reader_reset_prime(reader: *mut AscReader) {
    // SAFETY: by the caller's contract the handle is null or live.
    if let Some(control) = unsafe { control_of(reader) } {
        control.reset_prime();
    }
}

/// Any thread: the `lagFrames` the last read published (S56).
///
/// # Safety
///
/// `reader` is null or a live reader handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn asc_reader_lag_frames(reader: *const AscReader) -> i64 {
    // SAFETY: by the caller's contract the handle is null or live.
    unsafe { control_of(reader) }.map_or(0, ReaderControl::lag_frames)
}

/// Any thread: the reader's counters (S158).
///
/// # Safety
///
/// `reader` is null or a live reader handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn asc_reader_counters(reader: *const AscReader) -> AscReaderCounters {
    // SAFETY: by the caller's contract the handle is null or live.
    unsafe { control_of(reader) }.map_or_else(AscReaderCounters::default, |control| {
        control.counters().into()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reader::tolerance_frames;

    const RATE: u32 = 48_000;
    const BLOCK: usize = 512;

    /// Frame `i` of a test stream: both channels carry `i + 1`, R negated, so
    /// silence (0) is never a frame and every frame names its index.
    fn tagged(first: u64, frames: usize) -> Vec<f32> {
        (first..first + frames as u64)
            .flat_map(|i| {
                let tag = (i + 1) as f32;
                [tag, -tag]
            })
            .collect()
    }

    /// The index a played frame names, or `None` for silence.
    fn index_of(frame: &[f32]) -> Option<u64> {
        assert_eq!(frame[0], -frame[1], "a torn frame");
        (frame[0] != 0.0).then(|| frame[0] as u64 - 1)
    }

    /// A ring handle with a writer, both destroyed on drop.
    struct Session {
        ring: *mut AscRing,
        writer: *mut AscRingWriter,
        next: u64,
    }

    impl Session {
        fn new() -> Session {
            let ring = asc_ring_create(asc_ring_capacity_for_rate(RATE));
            assert!(!ring.is_null());
            // SAFETY: `ring` is live.
            let writer = unsafe { asc_ring_writer_create(ring) };
            assert!(!writer.is_null());
            Session {
                ring,
                writer,
                next: 0,
            }
        }

        fn write(&mut self, frames: usize) {
            let block = tagged(self.next, frames);
            // SAFETY: `writer` is live and `block` holds `frames` frames.
            unsafe { asc_ring_write(self.writer, block.as_ptr(), frames) };
            self.next += frames as u64;
        }

        fn reader(&self, delay: i64) -> *mut AscReader {
            // SAFETY: `ring` is live.
            let reader = unsafe { asc_reader_create(self.ring, RATE) };
            assert!(!reader.is_null());
            // SAFETY: `reader` is live.
            unsafe { asc_reader_set_delay_frames(reader, delay) };
            reader
        }
    }

    impl Drop for Session {
        fn drop(&mut self) {
            // SAFETY: both handles are live and destroyed once.
            unsafe {
                asc_ring_writer_destroy(self.writer);
                asc_ring_destroy(self.ring);
            }
        }
    }

    /// One read of `frames` frames through the C interface.
    fn read(reader: *mut AscReader, frames: usize) -> (Vec<f32>, AscReadReport) {
        let mut dst = vec![f32::NAN; frames * 2];
        let mut report = AscReadReport::default();
        // SAFETY: `reader` is live; `dst` holds `frames` frames.
        unsafe { asc_reader_read(reader, dst.as_mut_ptr(), frames, &mut report) };
        (dst, report)
    }

    // covers: S16, S36, S40
    #[test]
    fn rings_are_created_within_their_limits() {
        assert_eq!(asc_ring_capacity_for_rate(48_000), 1_152_000);
        assert_eq!(asc_ring_capacity_for_rate(192_000), 4_608_000);
        assert_eq!(asc_ring_capacity_for_rate(7_999), 0);
        assert_eq!(asc_ring_capacity_for_rate(384_001), 0);
        assert_eq!(MAX_RING_FRAMES, asc_ring_capacity_for_rate(384_000));
        assert!(asc_ring_create(MIN_RING_FRAMES - 1).is_null());
        assert!(asc_ring_create(MAX_RING_FRAMES + 1).is_null());
        assert!(asc_ring_create(usize::MAX).is_null());
        let small = asc_ring_create(MIN_RING_FRAMES);
        let large = asc_ring_create(asc_ring_capacity_for_rate(192_000));
        // SAFETY: both handles are live, then destroyed once.
        unsafe {
            assert_eq!(asc_ring_capacity(small), MIN_RING_FRAMES);
            assert!(!asc_ring_supports_rate(small, 8_000));
            assert!(asc_ring_supports_rate(large, 192_000));
            assert!(asc_ring_supports_rate(large, 44_100));
            assert!(!asc_ring_supports_rate(large, 384_000));
            assert!(!asc_ring_supports_rate(large, 0));
            assert_eq!(asc_ring_frames_written(large), 0);
            asc_ring_destroy(small);
            asc_ring_destroy(large);
        }
    }

    // covers: S35
    #[test]
    fn a_ring_has_one_writer_at_a_time() {
        let ring = asc_ring_create(MIN_RING_FRAMES);
        // SAFETY: `ring` is live; every writer is destroyed once.
        unsafe {
            let first = asc_ring_writer_create(ring);
            assert!(!first.is_null());
            assert!(asc_ring_writer_create(ring).is_null(), "a second writer");
            asc_ring_writer_destroy(first);
            let again = asc_ring_writer_create(ring);
            assert!(!again.is_null(), "destroying the writer frees the slot");
            asc_ring_writer_destroy(again);
            asc_ring_destroy(ring);
        }
    }

    // covers: S17, S37, S38, S39
    #[test]
    fn writes_count_frames_and_a_reset_clears_the_history() {
        let mut session = Session::new();
        session.write(BLOCK);
        session.write(3);
        // SAFETY: the ring is live.
        assert_eq!(unsafe { asc_ring_frames_written(session.ring) }, 515);
        // A block larger than the ring: W grows by all of it.
        session.write(1_200_000);
        // SAFETY: the ring is live.
        assert_eq!(unsafe { asc_ring_frames_written(session.ring) }, 1_200_515);
        // SAFETY: the writer is live and no reader exists.
        unsafe { asc_ring_writer_reset(session.writer) };
        // SAFETY: the ring is live.
        assert_eq!(unsafe { asc_ring_frames_written(session.ring) }, 0);
        // A reader created after the reset plays nothing from before it: it
        // waits in silence while no more than its delay (one block) is
        // written, then starts that delay behind W.
        session.next = 5_000_000;
        session.write(BLOCK);
        let reader = session.reader(0);
        let (block, report) = read(reader, BLOCK);
        assert_eq!(report.correction, CORRECTION_NONE);
        assert_eq!((report.copied, report.lag), (0, BLOCK as i64));
        assert!(block.iter().all(|&x| x == 0.0));
        session.write(BLOCK);
        let (block, report) = read(reader, BLOCK);
        assert_eq!(report.correction, CORRECTION_PRIME);
        assert_eq!(report.start, BLOCK as i64);
        assert_eq!(index_of(&block[..2]), Some(5_000_000 + BLOCK as u64));
        // SAFETY: `reader` is live and destroyed once.
        unsafe { asc_reader_destroy(reader) };
    }

    // covers: S42, S44, S45, S49, S56, S158
    #[test]
    fn a_reader_plays_every_frame_once_at_its_exact_delay() {
        let mut session = Session::new();
        let delay = 4096i64;
        let reader = session.reader(delay);
        let mut last: Option<u64> = None;
        for round in 0..200 {
            session.write(BLOCK);
            let (block, report) = read(reader, BLOCK);
            let w = session.next as i64;
            assert_eq!(report.frames, BLOCK);
            assert_eq!(report.written, w);
            assert_eq!(report.delay, delay);
            if w <= delay {
                // Silent, and counting nothing, until more than the delay is
                // written; P is still 0.
                assert_eq!(report.correction, CORRECTION_NONE, "round {round}");
                assert_eq!((report.start, report.lag, report.copied), (0, w, 0));
                assert!(block.iter().all(|&x| x == 0.0), "round {round}");
                continue;
            }
            assert_eq!(report.start, w - delay, "round {round}: exact delay");
            assert_eq!(report.lag, delay - BLOCK as i64);
            let expected = if w - (BLOCK as i64) <= delay {
                CORRECTION_PRIME
            } else {
                CORRECTION_NONE
            };
            assert_eq!(report.correction, expected, "round {round}");
            for (i, frame) in block.chunks_exact(2).enumerate() {
                let index = index_of(frame).expect("no silence once primed");
                assert_eq!(
                    index as i64,
                    report.start + i as i64,
                    "frame k plays frame k"
                );
                assert!(last.is_none_or(|last| index > last), "a repeat");
                last = Some(index);
            }
        }
        // SAFETY: `reader` is live, then destroyed once.
        unsafe {
            assert_eq!(asc_reader_lag_frames(reader), delay - BLOCK as i64);
            let counters = asc_reader_counters(reader);
            assert_eq!(
                counters,
                AscReaderCounters {
                    primes: 1,
                    ..AscReaderCounters::default()
                }
            );
            asc_reader_destroy(reader);
        }
    }

    // covers: S46, S47, S48, S49, S158
    #[test]
    fn delay_changes_through_the_c_interface_are_exact_and_never_repeat() {
        let mut session = Session::new();
        let reader = session.reader(4096);
        let mut played = Vec::new();
        let mut run = |session: &mut Session, rounds: usize| {
            let mut silent = 0usize;
            let mut last_report = AscReadReport::default();
            for _ in 0..rounds {
                session.write(BLOCK);
                let (block, report) = read(reader, BLOCK);
                for frame in block.chunks_exact(2) {
                    match index_of(frame) {
                        Some(index) => played.push(index),
                        None => silent += 1,
                    }
                }
                last_report = report;
            }
            (silent, last_report)
        };
        run(&mut session, 50);
        // Increase by 6000 frames: exactly 6000 frames of silence, then on.
        // SAFETY: `reader` is live.
        unsafe { asc_reader_set_delay_frames(reader, 10_096) };
        let (silent, report) = run(&mut session, 50);
        assert_eq!(silent, 6000);
        assert_eq!(report.start, session.next as i64 - 10_096);
        // Decrease by 8000 frames: skipped on the next block.
        // SAFETY: `reader` is live.
        unsafe { asc_reader_set_delay_frames(reader, 2096) };
        let (silent, report) = run(&mut session, 1);
        assert_eq!(silent, 0);
        assert_eq!(report.correction, CORRECTION_RESYNC);
        assert_eq!(report.start, session.next as i64 - 2096);
        assert!(played.windows(2).all(|w| w[1] > w[0]), "no repeat, ever");
        // SAFETY: `reader` is live, then destroyed once.
        unsafe {
            assert_eq!(asc_reader_delay_frames(reader), 2096);
            let counters = asc_reader_counters(reader);
            assert_eq!((counters.primes, counters.resyncs), (1, 2));
            assert_eq!(counters.drift_skips + counters.drift_silences, 0);
            asc_reader_destroy(reader);
        }
    }

    // covers: S46, S56, S159
    #[test]
    fn an_empty_read_leaves_a_resync_or_a_prime_to_the_next_read() {
        let mut session = Session::new();
        let reader = session.reader(4096);
        for _ in 0..20 {
            session.write(BLOCK);
            read(reader, BLOCK);
        }
        // An output cycle that needs no input, as a resampled output's can.
        let read_nothing = || {
            let mut report = AscReadReport {
                frames: 99,
                ..AscReadReport::default()
            };
            // SAFETY: `reader` is live; a buffer of 0 frames may be null.
            unsafe { asc_reader_read(reader, std::ptr::null_mut(), 0, &mut report) };
            assert_eq!(report, AscReadReport::default(), "nothing was read");
            report.frames = 99;
            // SAFETY: as above.
            unsafe { asc_reader_read_at(reader, std::ptr::null_mut(), 0, 8192, 1680, &mut report) };
            assert_eq!(report, AscReadReport::default(), "nothing was read");
        };
        let check = |session: &mut Session, first: i32| {
            for round in 0..40 {
                session.write(BLOCK);
                let (_, report) = read(reader, BLOCK);
                let expected = if round == 0 { first } else { CORRECTION_NONE };
                assert_eq!(report.correction, expected, "round {round}");
                assert_eq!(
                    report.start,
                    session.next as i64 - 8192,
                    "round {round}: exactly the delay behind W"
                );
            }
            // SAFETY: `reader` is live.
            let lag = unsafe { asc_reader_lag_frames(reader) };
            assert_eq!(lag, 8192 - BLOCK as i64);
        };
        // A delay change, then empty reads, then the writer's next block.
        // SAFETY: `reader` is live.
        unsafe { asc_reader_set_delay_frames(reader, 8192) };
        read_nothing();
        check(&mut session, CORRECTION_RESYNC);
        // The same for a prime reset.
        // SAFETY: `reader` is live.
        unsafe { asc_reader_reset_prime(reader) };
        read_nothing();
        check(&mut session, CORRECTION_PRIME);
        // SAFETY: `reader` is live, then destroyed once.
        unsafe {
            let counters = asc_reader_counters(reader);
            assert_eq!((counters.primes, counters.resyncs), (2, 1));
            assert_eq!(counters.drift_skips + counters.drift_silences, 0);
            asc_reader_destroy(reader);
        }
    }

    // covers: S41, S50, S51, S52
    #[test]
    fn read_at_takes_the_delay_and_tolerance_from_the_caller() {
        let mut session = Session::new();
        let reader = session.reader(0);
        // SAFETY: `reader` is live.
        unsafe {
            assert_eq!(asc_reader_tolerance_frames(reader), tolerance_frames(RATE));
            asc_reader_set_tolerance_frames(reader, -5);
            assert_eq!(asc_reader_tolerance_frames(reader), 0);
            asc_reader_set_tolerance_frames(reader, 1680);
        }
        session.write(20_000);
        let mut dst = vec![0.0f32; BLOCK * 2];
        let mut report = AscReadReport::default();
        // SAFETY: `reader` is live; `dst` holds BLOCK frames.
        unsafe { asc_reader_read_at(reader, dst.as_mut_ptr(), BLOCK, 8000, 100, &mut report) };
        assert_eq!(report.correction, CORRECTION_PRIME);
        assert_eq!(report.start, 20_000 - 8000);
        // The caller's tolerance of 100 frames: 200 frames behind is a skip.
        session.write(BLOCK + 200);
        // SAFETY: as above.
        unsafe { asc_reader_read_at(reader, dst.as_mut_ptr(), BLOCK, 8000, 100, &mut report) };
        assert_eq!(report.correction, CORRECTION_SKIP_FORWARD);
        assert_eq!(report.error, 200);
        // 300 frames ahead (the ring didn't move enough) is a step back.
        session.write(BLOCK - 300);
        // SAFETY: as above.
        unsafe { asc_reader_read_at(reader, dst.as_mut_ptr(), BLOCK, 8000, 100, &mut report) };
        assert_eq!(report.correction, CORRECTION_STEP_BACK);
        assert_eq!(report.error, -300);
        assert_eq!(report.masked, 300);
        // SAFETY: `reader` is live, then destroyed once.
        unsafe {
            let counters = asc_reader_counters(reader);
            assert_eq!((counters.drift_skips, counters.drift_skip_frames), (1, 200));
            assert_eq!(
                (counters.drift_silences, counters.drift_silence_frames),
                (1, 300)
            );
            asc_reader_destroy(reader);
        }
    }

    // covers: S53, S158
    #[test]
    fn a_reader_that_reaches_the_newest_frame_reports_starvation() {
        let mut session = Session::new();
        let reader = session.reader(0);
        // More than its delay of one block, so the first read primes.
        session.write(2 * BLOCK);
        let (_, first) = read(reader, BLOCK);
        assert_eq!(first.copied, BLOCK);
        session.write(100);
        let (block, report) = read(reader, BLOCK);
        assert_eq!((report.copied, report.starved), (100, BLOCK - 100));
        assert!(block[200..].iter().all(|&x| x == 0.0));
        // SAFETY: `reader` is live, then destroyed once.
        unsafe {
            let counters = asc_reader_counters(reader);
            assert_eq!(
                (counters.starved_renders, counters.starved_frames),
                (1, 412)
            );
            asc_reader_destroy(reader);
        }
    }

    // covers: S46, S41, S158
    #[test]
    fn a_delay_change_on_a_dry_reader_waits_for_the_writer() {
        let mut session = Session::new();
        let reader = session.reader(0);
        session.write(48_000);
        for _ in 0..10 {
            session.write(BLOCK);
            let (_, report) = read(reader, BLOCK);
            assert_eq!((report.copied, report.lag), (BLOCK, 0));
        }
        // Capture stalls with the reader at W, and the delay changes.
        // SAFETY: `reader` is live.
        unsafe { asc_reader_set_delay_frames(reader, 1200) };
        for _ in 0..5 {
            let (block, report) = read(reader, BLOCK);
            assert_eq!(report.correction, CORRECTION_NONE, "the change waits");
            assert_eq!((report.starved, report.lag), (BLOCK, 0));
            assert!(block.iter().all(|&x| x == 0.0));
        }
        // Capture writes again: the next read applies the change, exactly.
        session.write(BLOCK);
        let (_, report) = read(reader, BLOCK);
        assert_eq!(report.correction, CORRECTION_RESYNC);
        assert_eq!(report.start, session.next as i64 - 1200);
        assert_eq!(report.lag, 1200 - BLOCK as i64);
        for _ in 0..10 {
            session.write(BLOCK);
            let (_, report) = read(reader, BLOCK);
            assert_eq!((report.correction, report.lag), (CORRECTION_NONE, 688));
        }
        // SAFETY: `reader` is live, then destroyed once.
        unsafe {
            let counters = asc_reader_counters(reader);
            assert_eq!((counters.primes, counters.resyncs), (1, 1));
            assert_eq!(counters.drift_skips + counters.drift_silences, 0);
            assert_eq!(
                (counters.starved_renders, counters.starved_frames),
                (5, 5 * BLOCK as u64)
            );
            asc_reader_destroy(reader);
        }
    }

    // covers: S159, S158
    #[test]
    fn reset_prime_starts_over_and_keeps_the_counters() {
        let mut session = Session::new();
        let reader = session.reader(1024);
        session.write(4096);
        let (_, first) = read(reader, BLOCK);
        // SAFETY: `reader` is live.
        unsafe { asc_reader_request_resync(reader) };
        let (_, second) = read(reader, BLOCK);
        assert_eq!(
            (first.correction, second.correction),
            (CORRECTION_PRIME, CORRECTION_RESYNC)
        );
        // SAFETY: `reader` is live.
        unsafe { asc_reader_reset_prime(reader) };
        let (block, third) = read(reader, BLOCK);
        assert_eq!(third.correction, CORRECTION_PRIME);
        // F restarted: the frames played before the prime may play again.
        assert_eq!(index_of(&block[..2]), Some(4096 - 1024));
        // SAFETY: `reader` is live, then destroyed once.
        unsafe {
            let counters = asc_reader_counters(reader);
            assert_eq!((counters.primes, counters.resyncs), (2, 1));
            asc_reader_destroy(reader);
        }
    }

    // covers: S9, S58
    #[test]
    fn readers_of_one_ring_are_independent() {
        let mut session = Session::new();
        let near = session.reader(4096);
        let far = session.reader(12_000);
        for _ in 0..60 {
            session.write(BLOCK);
            let w = session.next as i64;
            let (_, a) = read(near, BLOCK);
            let (_, b) = read(far, BLOCK);
            // Each waits for more than its own delay, then plays exactly
            // that far behind W.
            for (report, delay) in [(a, 4096), (b, 12_000)] {
                if w <= delay {
                    assert_eq!((report.start, report.copied), (0, 0));
                } else {
                    assert_eq!(report.start, w - delay);
                }
            }
        }
        // SAFETY: both readers are live and destroyed once.
        unsafe {
            asc_reader_destroy(near);
            asc_reader_destroy(far);
        }
    }

    // covers: S128, S134
    #[test]
    fn handles_keep_the_ring_alive_in_any_destroy_order() {
        let ring = asc_ring_create(asc_ring_capacity_for_rate(RATE));
        // SAFETY: `ring` is live until it is destroyed below; every handle is
        // destroyed once.
        unsafe {
            let writer = asc_ring_writer_create(ring);
            let reader = asc_reader_create(ring, RATE);
            asc_ring_destroy(ring);
            // Two blocks: more than the reader's delay of one block.
            let block = tagged(0, 2 * BLOCK);
            asc_ring_write(writer, block.as_ptr(), 2 * BLOCK);
            let mut dst = vec![0.0f32; BLOCK * 2];
            asc_reader_read(reader, dst.as_mut_ptr(), BLOCK, std::ptr::null_mut());
            assert_eq!(index_of(&dst[..2]), Some(BLOCK as u64));
            asc_ring_writer_destroy(writer);
            asc_reader_read(reader, dst.as_mut_ptr(), BLOCK, std::ptr::null_mut());
            asc_reader_destroy(reader);
        }
    }

    #[test]
    fn every_correction_has_its_code() {
        let pairs = [
            (Correction::None, CORRECTION_NONE),
            (Correction::Prime, CORRECTION_PRIME),
            (Correction::Resync, CORRECTION_RESYNC),
            (Correction::SkipForward, CORRECTION_SKIP_FORWARD),
            (Correction::Overrun, CORRECTION_OVERRUN),
            (Correction::StepBack, CORRECTION_STEP_BACK),
        ];
        for (correction, code) in pairs {
            assert_eq!(correction_code(correction), code);
        }
        let report = AscReadReport::from(ReadReport {
            frames: 1,
            written: 2,
            delay: 3,
            error: -4,
            start: 5,
            lag: 6,
            correction: Correction::Overrun,
            masked: 7,
            copied: 8,
            starved: 9,
            rejected: true,
            due: Correction::None,
        });
        assert_eq!(
            report,
            AscReadReport {
                frames: 1,
                written: 2,
                delay: 3,
                error: -4,
                start: 5,
                lag: 6,
                correction: CORRECTION_OVERRUN,
                masked: 7,
                copied: 8,
                starved: 9,
                rejected: true,
            }
        );
    }

    #[test]
    fn null_handles_and_bad_buffers_do_nothing() {
        let null_ring = std::ptr::null_mut::<AscRing>();
        let null_writer = std::ptr::null_mut::<AscRingWriter>();
        let null_reader = std::ptr::null_mut::<AscReader>();
        let mut report = AscReadReport {
            frames: 99,
            ..AscReadReport::default()
        };
        let mut dst = [7.0f32; 8];
        // SAFETY: every call gets a null handle, a null buffer or a frame
        // count no buffer can hold, which the interface allows.
        unsafe {
            asc_ring_destroy(null_ring);
            assert_eq!(asc_ring_capacity(null_ring), 0);
            assert!(!asc_ring_supports_rate(null_ring, RATE));
            assert_eq!(asc_ring_frames_written(null_ring), 0);
            assert!(asc_ring_writer_create(null_ring).is_null());
            assert!(asc_reader_create(null_ring, RATE).is_null());
            asc_ring_writer_destroy(null_writer);
            asc_ring_write(null_writer, dst.as_ptr(), 4);
            asc_ring_writer_reset(null_writer);
            asc_reader_destroy(null_reader);
            asc_reader_read(null_reader, dst.as_mut_ptr(), 4, &mut report);
            assert_eq!(report, AscReadReport::default(), "a zero report");
            asc_reader_read_at(null_reader, dst.as_mut_ptr(), 4, 0, 0, std::ptr::null_mut());
            asc_reader_set_delay_frames(null_reader, 5);
            asc_reader_set_tolerance_frames(null_reader, 5);
            asc_reader_request_resync(null_reader);
            asc_reader_reset_prime(null_reader);
            assert_eq!(asc_reader_delay_frames(null_reader), 0);
            assert_eq!(asc_reader_tolerance_frames(null_reader), 0);
            assert_eq!(asc_reader_lag_frames(null_reader), 0);
            assert_eq!(
                asc_reader_counters(null_reader),
                AscReaderCounters::default()
            );
        }
        assert_eq!(dst, [7.0; 8]);

        let mut session = Session::new();
        let reader = session.reader(0);
        session.write(BLOCK);
        // SAFETY: the handles are live; the buffers are null, empty or too
        // large to exist, which the interface allows.
        unsafe {
            asc_ring_write(session.writer, std::ptr::null(), 4);
            asc_ring_write(session.writer, dst.as_ptr(), usize::MAX);
            asc_ring_write(session.writer, std::ptr::null(), 0);
            assert_eq!(asc_ring_frames_written(session.ring), BLOCK as u64);
            asc_reader_read(reader, std::ptr::null_mut(), 4, &mut report);
            asc_reader_read(reader, dst.as_mut_ptr(), usize::MAX, &mut report);
            assert_eq!(report, AscReadReport::default());
            assert_eq!(asc_reader_counters(reader).primes, 0, "nothing was read");
            // Nor is an empty read: the reader anchors on its first real read.
            report.frames = 99;
            asc_reader_read(reader, std::ptr::null_mut(), 0, &mut report);
            assert_eq!(report, AscReadReport::default());
            report.frames = 99;
            asc_reader_read_at(reader, dst.as_mut_ptr(), 0, 0, 0, &mut report);
            assert_eq!(report, AscReadReport::default());
            assert_eq!(asc_reader_counters(reader).primes, 0, "nothing was read");
            asc_reader_read(reader, dst.as_mut_ptr(), 4, &mut report);
            assert_eq!(report.correction, CORRECTION_PRIME);
            asc_reader_destroy(reader);
        }
        assert!(rejects_bad_rates(&session));
    }

    fn rejects_bad_rates(session: &Session) -> bool {
        // SAFETY: the ring is live.
        unsafe {
            asc_reader_create(session.ring, 0).is_null()
                && asc_reader_create(session.ring, 7_999).is_null()
                && asc_reader_create(session.ring, 384_001).is_null()
        }
    }
}
