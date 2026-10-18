//! The input ring between the host's real-time audio call and the stream
//! (P6, P111, P112).
//!
//! [`InputWriter`] is the host's side: its calls copy frames into a
//! preallocated single-producer, single-consumer ring and return, without
//! allocating, locking, blocking, logging or touching the file system (P6).
//! Every frame the host hands over gets the next ring position, and ring
//! position = timeline frame (offset by the timeline's origin): the stream
//! cuts packet j from positions 352·j to 352·j + 351 at its send time. A
//! frame whose position was already cut (its packet went out with silence
//! in its place) is dropped, so the timeline never stalls or shifts (P112);
//! when the ring is full the write keeps what fits and drops the rest
//! (P112). Each write also records when its frames arrived, for the rate
//! model (P101).
//!
//! Frames are stored one per atomic word (left in the high half), so both
//! sides stay in safe Rust: the writer stores the slots, then publishes its
//! position with release ordering; the reader acquires the position before
//! it reads the slots.

use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::time::Instant;

use crate::audio::{Frame, PacketFrames, SAMPLE_RATE, sample_from_f32};

/// The smallest input ring: one second (P111).
pub const MIN_INPUT_FRAMES: usize = SAMPLE_RATE as usize;
/// Arrival records the ring holds between two reads; a record that doesn't
/// fit is dropped, which only thins the rate model's data.
const ARRIVAL_RECORDS: usize = 1024;

struct Shared {
    /// Arrival times are nanoseconds from this instant (the sender clock's
    /// origin).
    origin: Instant,
    slots: Box<[AtomicU32]>,
    /// The next ring position the writer fills.
    write: AtomicU64,
    /// Positions below this were cut by the reader.
    floor: AtomicU64,
    /// Frames handed over by the host, kept or not.
    received: AtomicU64,
    /// Arrival records: the position after a write and the time it came.
    arrival_end: Box<[AtomicU64]>,
    arrival_at: Box<[AtomicU64]>,
    arrivals_written: AtomicU64,
    arrivals_read: AtomicU64,
}

/// Creates a ring of `frames` frames (at least [`MIN_INPUT_FRAMES`]), with
/// arrival times counted from `origin`. All memory is allocated here (P111).
pub(crate) fn ring(frames: usize, origin: Instant) -> (InputWriter, InputReader) {
    let frames = frames.max(MIN_INPUT_FRAMES);
    let atomics = |count: usize| (0..count).map(|_| AtomicU64::new(0)).collect();
    let shared = Arc::new(Shared {
        origin,
        slots: (0..frames).map(|_| AtomicU32::new(0)).collect(),
        write: AtomicU64::new(0),
        floor: AtomicU64::new(0),
        received: AtomicU64::new(0),
        arrival_end: atomics(ARRIVAL_RECORDS),
        arrival_at: atomics(ARRIVAL_RECORDS),
        arrivals_written: AtomicU64::new(0),
        arrivals_read: AtomicU64::new(0),
    });
    (
        InputWriter {
            shared: shared.clone(),
        },
        InputReader {
            shared,
            taken: 0,
            dropped: 0,
        },
    )
}

fn pack(left: i16, right: i16) -> u32 {
    (u32::from(left as u16) << 16) | u32::from(right as u16)
}

fn unpack(word: u32) -> Frame {
    [(word >> 16) as u16 as i16, word as u16 as i16]
}

/// The host's side of the input (P6). It is `Send`, so it can move to the
/// audio thread, and not `Clone`: there is one producer.
pub struct InputWriter {
    shared: Arc<Shared>,
}

impl InputWriter {
    /// Hands over interleaved stereo float frames at 44 100 Hz, converted as
    /// P73 says, which arrived at `at` on the sender clock. Returns the
    /// frames kept; the others were late or didn't fit (P112). An odd last
    /// sample is ignored. Never allocates, locks or blocks (P6).
    pub fn write_f32(&mut self, samples: &[f32], at: Instant) -> usize {
        self.write_with(samples.len() / 2, at, |index| {
            pack(
                sample_from_f32(samples[2 * index]),
                sample_from_f32(samples[2 * index + 1]),
            )
        })
    }

    /// Hands over interleaved stereo 16-bit frames, like
    /// [`InputWriter::write_f32`].
    pub fn write_i16(&mut self, samples: &[i16], at: Instant) -> usize {
        self.write_with(samples.len() / 2, at, |index| {
            pack(samples[2 * index], samples[2 * index + 1])
        })
    }

    /// The ring position the next frame handed over will take. With the
    /// stream's timeline origin it names the frame's timeline index (P113).
    pub fn position(&self) -> u64 {
        self.shared.write.load(Ordering::Relaxed)
    }

    fn write_with(&mut self, frames: usize, at: Instant, frame: impl Fn(usize) -> u32) -> usize {
        let shared = &*self.shared;
        let capacity = shared.slots.len() as u64;
        let floor = shared.floor.load(Ordering::Acquire);
        let start = shared.write.load(Ordering::Relaxed);
        let count = frames as u64;
        // Frames whose positions were already cut are late: dropped, their
        // positions kept, so later frames stay where they belong.
        let late = floor.saturating_sub(start).min(count);
        let mut position = start + late;
        let room = (capacity + floor).saturating_sub(position);
        let keep = (count - late).min(room);
        for index in 0..keep {
            let slot = ((position + index) % capacity) as usize;
            shared.slots[slot].store(frame((late + index) as usize), Ordering::Relaxed);
        }
        position += keep;
        shared.write.store(position, Ordering::Release);
        shared.received.fetch_add(count, Ordering::Release);
        if position > start {
            self.record_arrival(position, at);
        }
        keep as usize
    }

    fn record_arrival(&self, end: u64, at: Instant) {
        let shared = &*self.shared;
        let written = shared.arrivals_written.load(Ordering::Relaxed);
        let read = shared.arrivals_read.load(Ordering::Acquire);
        if written - read >= ARRIVAL_RECORDS as u64 {
            return;
        }
        let nanos = match at.checked_duration_since(shared.origin) {
            Some(after) => after.as_nanos() as i64,
            None => -(shared.origin.duration_since(at).as_nanos() as i64),
        };
        let slot = (written % ARRIVAL_RECORDS as u64) as usize;
        shared.arrival_end[slot].store(end, Ordering::Relaxed);
        shared.arrival_at[slot].store(nanos as u64, Ordering::Relaxed);
        shared
            .arrivals_written
            .store(written + 1, Ordering::Release);
    }
}

impl fmt::Debug for InputWriter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("InputWriter")
            .field("position", &self.position())
            .finish()
    }
}

impl fmt::Debug for InputReader {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("InputReader")
            .field("capacity", &self.capacity())
            .field("floor", &self.shared.floor.load(Ordering::Relaxed))
            .field("taken", &self.taken)
            .finish()
    }
}

/// The stream's side of the input.
pub(crate) struct InputReader {
    shared: Arc<Shared>,
    /// Frames the reader took from the ring into packets.
    taken: u64,
    /// The largest dropped-frame count reported so far.
    dropped: u64,
}

impl InputReader {
    /// Cuts the 352 frames from `start` on into `out`, silence where the
    /// writer hasn't filled them yet, and moves the floor past them; later
    /// frames for those positions are dropped (P112). Returns the number of
    /// silent frames.
    pub fn cut(&mut self, start: u64, out: &mut PacketFrames) -> u16 {
        let shared = &*self.shared;
        let capacity = shared.slots.len() as u64;
        let floor = shared.floor.load(Ordering::Relaxed);
        let written = shared.write.load(Ordering::Acquire);
        let mut silent = 0u16;
        for (index, frame) in out.iter_mut().enumerate() {
            let position = start + index as u64;
            if position >= floor && position < written {
                *frame =
                    unpack(shared.slots[(position % capacity) as usize].load(Ordering::Relaxed));
                self.taken += 1;
            } else {
                *frame = [0, 0];
                silent += 1;
            }
        }
        let end = start + out.len() as u64;
        if end > floor {
            shared.floor.store(end, Ordering::Release);
        }
        silent
    }

    /// Starts a new timeline at the writer's position: frames not yet cut
    /// are dropped, and pending arrival records discarded (P143). Returns
    /// the position of the new timeline's first frame.
    pub fn rebase(&mut self) -> u64 {
        let shared = &*self.shared;
        let written = shared.write.load(Ordering::Acquire);
        let floor = shared.floor.load(Ordering::Relaxed);
        if written > floor {
            shared.floor.store(written, Ordering::Release);
        }
        let records = shared.arrivals_written.load(Ordering::Acquire);
        shared.arrivals_read.store(records, Ordering::Release);
        written.max(floor)
    }

    /// Calls `record(end position, nanoseconds from the clock origin)` for
    /// each arrival since the last call, in order.
    pub fn drain_arrivals(&mut self, mut record: impl FnMut(u64, i64)) {
        let shared = &*self.shared;
        let written = shared.arrivals_written.load(Ordering::Acquire);
        let mut read = shared.arrivals_read.load(Ordering::Relaxed);
        while read < written {
            let slot = (read % ARRIVAL_RECORDS as u64) as usize;
            let end = shared.arrival_end[slot].load(Ordering::Relaxed);
            let at = shared.arrival_at[slot].load(Ordering::Relaxed) as i64;
            record(end, at);
            read += 1;
        }
        shared.arrivals_read.store(read, Ordering::Release);
    }

    /// Input frames dropped so far: handed over, but neither cut into a
    /// packet nor still waiting in the ring (P112, P144). That covers late
    /// frames, frames that didn't fit, and frames a restart discarded.
    pub fn dropped(&mut self) -> u64 {
        let shared = &*self.shared;
        // The received count is read before the position: the writer adds
        // to it after publishing the position, so the difference never
        // counts frames still being written.
        let received = shared.received.load(Ordering::Acquire);
        let written = shared.write.load(Ordering::Acquire);
        let floor = shared.floor.load(Ordering::Relaxed);
        let pending = written.saturating_sub(floor);
        let dropped = received.saturating_sub(self.taken + pending);
        self.dropped = self.dropped.max(dropped);
        self.dropped
    }

    /// The ring's size in frames.
    pub fn capacity(&self) -> usize {
        self.shared.slots.len()
    }
}
