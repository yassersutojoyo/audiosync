//! The input ring between the host's real-time audio call and the stream
//! (P6, P111, P112).
//!
//! [`InputWriter`] is the host's side: its calls copy frames into a
//! preallocated single-producer, single-consumer ring and return, without
//! allocating, locking, blocking, logging or touching the file system (P6).
//! Every frame the host hands over gets the next ring position, and ring
//! position = timeline frame plus the stream's offset: the stream cuts
//! packet j from the positions of frames 352·j to 352·j + 351 at its send
//! time. A frame whose position was already cut (its packet went out with
//! silence in its place) is dropped, so a host that falls briefly behind
//! and catches up lands back in place and the timeline never stalls or
//! shifts (P112); when the ring is full the write keeps what fits and drops
//! the rest (P112).
//!
//! A host that is far behind, or that stays behind while it delivers at
//! real-time pace, or whose very first write is behind, or the first write
//! of a capture it marked as new ([`InputWriter::new_capture`]), didn't fall
//! behind: its capture paused and resumed (a loopback delivers nothing
//! while the source is silent), started after the stream did, or replaced
//! the one before it (P143). Its frames are then no longer late copies of
//! the cut positions but new audio, so the write moves up to the first
//! position not yet cut, the skipped positions stay the silence the stream
//! inserted (P96), and the stream places the resumed frames on the
//! timeline as if they had just arrived on schedule.
//!
//! Each write records when its frames arrived. Writes that land in time
//! feed the rate model and the send margin (P84, P101); a late write is
//! flagged, so its lateness can raise the margin while it stays out of the
//! rate fit, and a pause is never taken for the host's rate.
//!
//! Frames are stored one per atomic word (left in the high half), so both
//! sides stay in safe Rust: the writer stores the slots, then publishes its
//! position with release ordering; the reader acquires the position before
//! it reads the slots.

use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, AtomicU32, AtomicU64, Ordering, fence};
use std::time::Instant;

use crate::audio::{Frame, PacketFrames, SAMPLE_RATE, sample_from_f32};

/// The smallest input ring: one second (P111).
pub const MIN_INPUT_FRAMES: usize = SAMPLE_RATE as usize;
/// Arrival records the ring holds between two reads; a record that doesn't
/// fit is dropped, which only thins the rate model's data.
const ARRIVAL_RECORDS: usize = 1024;
/// A write at least this far behind the first position not yet cut is a
/// resumed capture, not a late one: one second.
pub const RESYNC_FRAMES: u64 = SAMPLE_RATE as u64;
/// The arrival record of a resumed write carries this bit in its start.
const RESYNC_BIT: u64 = 1 << 63;
/// The arrival record of a late write carries this bit in its start.
const LATE_BIT: u64 = 1 << 62;

/// One arrival record: the host's frames from ring position `start` up to
/// (not including) `end` arrived at `nanos` from the clock origin. `resync`
/// says the write moved up to the floor (a resumed capture); `late` says
/// some of its frames came after their positions were cut, so they were
/// dropped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Arrival {
    pub start: u64,
    pub end: u64,
    pub nanos: i64,
    pub resync: bool,
    pub late: bool,
}

/// A move of the speakers' timeline (P200): from ring position `position`
/// on, frames sound `frames` frames later than before.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TimelineMove {
    pub position: u64,
    pub frames: i64,
}

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
    /// Arrival records: the positions a write filled (the resync flag in
    /// the start's top bit) and the time it came.
    arrival_start: Box<[AtomicU64]>,
    arrival_end: Box<[AtomicU64]>,
    arrival_at: Box<[AtomicU64]>,
    arrivals_written: AtomicU64,
    arrivals_read: AtomicU64,
    /// The newest [`TimelineMove`] under a sequence lock: odd while the
    /// stream stores it, 0 before the first.
    move_sequence: AtomicU64,
    move_position: AtomicU64,
    move_frames: AtomicI64,
    /// Timeline restarts so far (P143).
    rebases: AtomicU64,
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
        arrival_start: atomics(ARRIVAL_RECORDS),
        arrival_end: atomics(ARRIVAL_RECORDS),
        arrival_at: atomics(ARRIVAL_RECORDS),
        arrivals_written: AtomicU64::new(0),
        arrivals_read: AtomicU64::new(0),
        move_sequence: AtomicU64::new(0),
        move_position: AtomicU64::new(0),
        move_frames: AtomicI64::new(0),
        rebases: AtomicU64::new(0),
    });
    (
        InputWriter {
            shared: shared.clone(),
            late: None,
            fresh: true,
            moves_taken: 0,
            rebases_seen: 0,
            held: vec![0; frames / 2].into_boxed_slice(),
            held_frames: 0,
            carried: 0,
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
    /// When the current run of late writes began and the frames it holds
    /// so far, while the last write was late.
    late: Option<(Instant, u64)>,
    /// No frame of the current capture or of a restarted timeline was
    /// handed over yet, or the last write resumed: the next one is never late.
    fresh: bool,
    /// The sequence of the last [`TimelineMove`] taken.
    moves_taken: u64,
    rebases_seen: u64,
    /// The current run's wholly late writes, in order (P112).
    held: Box<[u32]>,
    held_frames: usize,
    /// The frames of a held partly late write whose positions weren't cut
    /// when it came: the last ones held (P248).
    carried: usize,
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

    /// The host started a new capture (a source switch, P143): its first
    /// write, when the stream already sent silence for those positions,
    /// starts the audio as just arrived instead of being dropped as late.
    /// The timeline goes on. Never allocates, locks or blocks (P6).
    pub fn new_capture(&mut self) {
        self.fresh = true;
        self.late = None;
        // A move announced before this capture was followed by the host's
        // poll, not by this capture's frames.
        self.moves_taken = self.shared.move_sequence.load(Ordering::Acquire);
    }

    /// The speakers' newest timeline move (P200), once: announced when the
    /// step is decided, about L before the speakers make it. Never
    /// allocates, locks or blocks (P6).
    pub fn take_move(&mut self) -> Option<TimelineMove> {
        let shared = &*self.shared;
        let before = shared.move_sequence.load(Ordering::Acquire);
        if before == self.moves_taken || before % 2 == 1 {
            return None;
        }
        let moved = TimelineMove {
            position: shared.move_position.load(Ordering::Relaxed),
            frames: shared.move_frames.load(Ordering::Relaxed),
        };
        fence(Ordering::Acquire);
        if shared.move_sequence.load(Ordering::Relaxed) != before {
            return None;
        }
        self.moves_taken = before;
        Some(moved)
    }

    /// The ring position the next frame handed over will take. With the
    /// stream's timeline origin it names the frame's timeline index (P113).
    pub fn position(&self) -> u64 {
        self.shared.write.load(Ordering::Relaxed)
    }

    fn write_with(&mut self, frames: usize, at: Instant, frame: impl Fn(usize) -> u32) -> usize {
        let shared = &*self.shared;
        // The floor first: a restart counts itself before it moves the floor.
        let floor = shared.floor.load(Ordering::Acquire);
        let rebases = shared.rebases.load(Ordering::Acquire);
        if rebases != self.rebases_seen {
            self.rebases_seen = rebases;
            self.fresh = true;
            self.late = None;
        }
        let fresh = self.fresh;
        let capacity = shared.slots.len() as u64;
        let start = shared.write.load(Ordering::Relaxed);
        let count = frames as u64;
        let behind = floor.saturating_sub(start);
        // Still behind at real-time pace (at least half the time the late
        // run's frames last since it began): not catching up, so resumed.
        let paced = self.late.is_some_and(|(previous, size)| {
            at.saturating_duration_since(previous).as_secs_f64() * f64::from(SAMPLE_RATE)
                >= size as f64 / 2.0
        });
        // A capture's first write that is behind started after the stream
        // did, or after the stream filled the switch to it with silence
        // (P143): its frames were never late, so it starts the audio.
        let tail = self.carried as u64;
        let resync = (behind > 0 || tail > 0) && (behind >= RESYNC_FRAMES || paced || fresh);
        // Otherwise frames whose positions were already cut are late:
        // dropped, their positions kept, so later frames stay where they
        // belong (P112). A held partly late write's frames not cut when it
        // came go first: the host is catching up (P248).
        let carried = if resync { 0 } else { tail };
        let total = carried + count;
        let (late, first) = if resync {
            (0, floor)
        } else {
            let late = behind.min(total);
            (late, start + late)
        };
        // A partly late write after writes in time is held whole until the
        // next one shows whether it resumed a stalled capture (P248).
        let hold_head =
            late > 0 && late < total && self.late.is_none() && frames <= self.held.len();
        // A write held back as late was the resumed capture's first: it
        // starts the audio, ahead of this one.
        let released = self.held_frames as u64;
        let room = (capacity + floor).saturating_sub(first);
        let held = if resync && paced {
            released.min(room)
        } else {
            0
        };
        for index in 0..held {
            let slot = ((first + index) % capacity) as usize;
            shared.slots[slot].store(self.held[index as usize], Ordering::Relaxed);
        }
        let keep = if hold_head {
            0
        } else {
            (total - late).min(room - held)
        };
        for index in 0..keep {
            let slot = ((first + held + index) % capacity) as usize;
            let source = late + index;
            let word = if source < carried {
                self.held[(self.held_frames as u64 - carried + source) as usize]
            } else {
                frame((source - carried) as usize)
            };
            shared.slots[slot].store(word, Ordering::Relaxed);
        }
        // Writes wholly behind are held, in order, until a later one shows
        // whether the host is catching up (they are dropped then) or resumed
        // at real-time pace.
        if late > 0 && late == total {
            if self.held_frames + frames > self.held.len() {
                self.held_frames = 0;
            }
            let hold = frames.min(self.held.len());
            for index in 0..hold {
                self.held[self.held_frames + index] = frame(frames - hold + index);
            }
            self.held_frames += hold;
        } else if hold_head {
            for index in 0..frames {
                self.held[index] = frame(index);
            }
            self.held_frames = frames;
        } else {
            self.held_frames = 0;
        }
        self.carried = if hold_head {
            (count - late) as usize
        } else {
            0
        };
        let end = first + held + keep;
        shared.write.store(end.max(start), Ordering::Release);
        shared.received.fetch_add(
            count + released - self.held_frames as u64,
            Ordering::Release,
        );
        // A resume re-anchors the stream, so the one write after it starts at
        // its own first frame even if the stream already cut past this one.
        if count > 0 {
            self.fresh = resync && !fresh;
        }
        if late > 0 {
            self.late = Some(match self.late {
                Some((since, size)) if late == total => (since, size + count),
                _ => (at, count),
            });
            let reach = if hold_head { start + count } else { end };
            self.record_arrival(start, reach, at, LATE_BIT);
        } else {
            self.late = None;
            if keep > 0 {
                self.record_arrival(first, end, at, if resync { RESYNC_BIT } else { 0 });
            }
        }
        keep as usize
    }

    fn record_arrival(&self, start: u64, end: u64, at: Instant, flag: u64) {
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
        shared.arrival_start[slot].store(start | flag, Ordering::Relaxed);
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
    /// Cuts the 352 frames from ring position `start` on into `out`,
    /// silence where the writer hasn't filled them (positions before the
    /// floor or the ring's start included), and moves the floor past them;
    /// later frames for those positions are dropped (P112). Returns the
    /// number of silent frames.
    pub fn cut(&mut self, start: i64, out: &mut PacketFrames) -> u16 {
        let shared = &*self.shared;
        let capacity = shared.slots.len() as u64;
        let floor = shared.floor.load(Ordering::Relaxed);
        let written = shared.write.load(Ordering::Acquire);
        let mut silent = 0u16;
        for (index, frame) in out.iter_mut().enumerate() {
            let position = start + index as i64;
            let held = u64::try_from(position)
                .ok()
                .filter(|&position| position >= floor && position < written);
            match held {
                Some(position) => {
                    let word = shared.slots[(position % capacity) as usize].load(Ordering::Relaxed);
                    *frame = unpack(word);
                    self.taken += 1;
                }
                None => {
                    *frame = [0, 0];
                    silent += 1;
                }
            }
        }
        let end = start + out.len() as i64;
        if let Ok(end) = u64::try_from(end)
            && end > floor
        {
            shared.floor.store(end, Ordering::Release);
        }
        silent
    }

    /// Announces a timeline move to the host (P200).
    pub(crate) fn announce_move(&self, moved: TimelineMove) {
        let shared = &*self.shared;
        shared.move_sequence.fetch_add(1, Ordering::Relaxed);
        fence(Ordering::Release);
        shared
            .move_position
            .store(moved.position, Ordering::Relaxed);
        shared.move_frames.store(moved.frames, Ordering::Relaxed);
        shared.move_sequence.fetch_add(1, Ordering::Release);
    }

    /// The ring position the host's next frame will take.
    pub(crate) fn written(&self) -> u64 {
        self.shared.write.load(Ordering::Acquire)
    }

    /// The first ring position not yet cut.
    pub fn floor(&self) -> u64 {
        self.shared.floor.load(Ordering::Relaxed)
    }

    /// Starts a new timeline at the writer's position: frames not yet cut
    /// are dropped, and pending arrival records discarded (P143). Returns
    /// the position of the new timeline's first frame.
    pub fn rebase(&mut self) -> u64 {
        let shared = &*self.shared;
        let written = shared.write.load(Ordering::Acquire);
        let floor = shared.floor.load(Ordering::Relaxed);
        // Counted first: a write that sees the new floor sees the restart too.
        shared.rebases.fetch_add(1, Ordering::Release);
        if written > floor {
            shared.floor.store(written, Ordering::Release);
        }
        let records = shared.arrivals_written.load(Ordering::Acquire);
        shared.arrivals_read.store(records, Ordering::Release);
        written.max(floor)
    }

    /// Calls `record` for each arrival since the last call, in order.
    pub fn drain_arrivals(&mut self, mut record: impl FnMut(Arrival)) {
        let shared = &*self.shared;
        let written = shared.arrivals_written.load(Ordering::Acquire);
        let mut read = shared.arrivals_read.load(Ordering::Relaxed);
        while read < written {
            let slot = (read % ARRIVAL_RECORDS as u64) as usize;
            let start = shared.arrival_start[slot].load(Ordering::Relaxed);
            record(Arrival {
                start: start & !(RESYNC_BIT | LATE_BIT),
                end: shared.arrival_end[slot].load(Ordering::Relaxed),
                nanos: shared.arrival_at[slot].load(Ordering::Relaxed) as i64,
                resync: start & RESYNC_BIT != 0,
                late: start & LATE_BIT != 0,
            });
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

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::audio::FRAMES_PER_PACKET;

    // P143: the stream filled the switch with silence, so the new capture's
    // first write lands behind the floor and must start the audio there.
    // covers: P143
    #[test]
    fn a_new_captures_first_write_behind_the_floor_is_kept() {
        let origin = Instant::now();
        let (mut writer, mut reader) = ring(2 * MIN_INPUT_FRAMES, origin);
        let block = [1000i16; 2 * FRAMES_PER_PACKET];
        assert_eq!(writer.write_i16(&block, origin), FRAMES_PER_PACKET);
        let mut out = [[0, 0]; FRAMES_PER_PACKET];
        for packet in 0..10 {
            reader.cut((packet * FRAMES_PER_PACKET) as i64, &mut out);
        }
        reader.drain_arrivals(|_| {});
        let floor = reader.floor();
        assert!(floor > writer.position() && floor - writer.position() < RESYNC_FRAMES);

        writer.new_capture();
        let kept = writer.write_i16(&block, origin + Duration::from_millis(300));

        assert_eq!(kept, FRAMES_PER_PACKET);
        let mut arrivals = Vec::new();
        reader.drain_arrivals(|arrival| arrivals.push(arrival));
        assert_eq!(arrivals.len(), 1);
        assert!(arrivals[0].resync && !arrivals[0].late);
        assert_eq!(arrivals[0].start, floor);
        assert_eq!(writer.position(), floor + FRAMES_PER_PACKET as u64);
    }

    // A capture that stalls for less than RESYNC_FRAMES (a player that
    // closes its stream between tracks) and resumes at real-time pace: its
    // first block after the stall is the next track's first audio, so it
    // must start the audio with the block after it, not be dropped as late.
    // covers: P112
    #[test]
    fn the_first_block_after_a_short_stall_is_kept_when_the_next_one_comes_in_real_time() {
        let origin = Instant::now();
        let (mut writer, mut reader) = ring(2 * MIN_INPUT_FRAMES, origin);
        let block = [1000i16; 2 * FRAMES_PER_PACKET];
        writer.write_i16(&block, origin);
        let mut out = [[0, 0]; FRAMES_PER_PACKET];
        for packet in 0..10 {
            reader.cut((packet * FRAMES_PER_PACKET) as i64, &mut out);
        }
        reader.drain_arrivals(|_| {});
        let floor = reader.floor();

        let first = [2000i16; 2 * FRAMES_PER_PACKET];
        let next = [3000i16; 2 * FRAMES_PER_PACKET];
        let at = origin + Duration::from_millis(100);
        writer.write_i16(&first, at);
        writer.write_i16(&next, at + Duration::from_millis(8));

        assert_eq!(writer.position(), floor + 2 * FRAMES_PER_PACKET as u64);
        reader.cut(floor as i64, &mut out);
        assert_eq!(out[0], [2000, 2000]);
        assert_eq!(reader.dropped(), 0);
    }

    // The host hands one block over in two writes at the same instant (the
    // gap fill's silence then the packet, or a block split for resampling),
    // so the second one is no sign of catching up.
    // covers: P112
    #[test]
    fn a_block_after_a_short_stall_handed_over_in_two_writes_is_kept_whole() {
        let origin = Instant::now();
        let (mut writer, mut reader) = ring(2 * MIN_INPUT_FRAMES, origin);
        let block = [1000i16; 2 * FRAMES_PER_PACKET];
        writer.write_i16(&block, origin);
        let mut out = [[0, 0]; FRAMES_PER_PACKET];
        for packet in 0..10 {
            reader.cut((packet * FRAMES_PER_PACKET) as i64, &mut out);
        }
        reader.drain_arrivals(|_| {});
        let floor = reader.floor();

        let head = [2000i16; 2 * FRAMES_PER_PACKET];
        let rest = [2500i16; 2 * FRAMES_PER_PACKET];
        let next = [3000i16; 2 * FRAMES_PER_PACKET];
        let at = origin + Duration::from_millis(100);
        writer.write_i16(&head, at);
        writer.write_i16(&rest, at);
        writer.write_i16(&next, at + Duration::from_millis(10));

        assert_eq!(writer.position(), floor + 3 * FRAMES_PER_PACKET as u64);
        reader.cut(floor as i64, &mut out);
        assert_eq!(out[0], [2000, 2000]);
        reader.cut((floor + FRAMES_PER_PACKET as u64) as i64, &mut out);
        assert_eq!(out[0], [2500, 2500]);
        assert_eq!(reader.dropped(), 0);
    }

    fn ramp(frames: usize) -> Vec<i16> {
        (1..=frames as i16)
            .flat_map(|value| [value, value])
            .collect()
    }

    // A player that closed its stream between tracks reopens it after a
    // short hole: its first write lands partly behind the floor (549 of 941
    // frames, measured live) with no resume before it, and the next write
    // comes in real time, so that first write is the next track's head.
    // covers: P248
    #[test]
    fn a_partly_late_first_write_after_a_short_stall_keeps_its_head() {
        let origin = Instant::now();
        let (mut writer, mut reader) = ring(2 * MIN_INPUT_FRAMES, origin);
        writer.write_i16(&[1000i16; 2 * 2971], origin);
        let mut out = [[0, 0]; FRAMES_PER_PACKET];
        for packet in 0..10 {
            reader.cut((packet * FRAMES_PER_PACKET) as i64, &mut out);
        }
        assert_eq!(reader.floor() - writer.position(), 549);

        let at = origin + Duration::from_millis(100);
        writer.write_i16(&ramp(941), at);
        let stalled = reader.floor();
        for packet in 0..2 {
            reader.cut(
                (stalled + (packet * FRAMES_PER_PACKET) as u64) as i64,
                &mut out,
            );
        }
        let floor = reader.floor();
        writer.write_i16(&[5000i16; 2 * 941], at + Duration::from_millis(21));

        reader.cut(floor as i64, &mut out);
        assert_eq!(out[0], [1, 1]);
        assert_eq!(reader.dropped(), 0);
    }

    // P248: a partly late write followed at once by the next one is a host
    // catching up: the late head is dropped, the rest stays in place and
    // the timeline doesn't move.
    // covers: P248
    #[test]
    fn a_partly_late_write_followed_at_once_drops_only_its_late_head() {
        let origin = Instant::now();
        let (mut writer, mut reader) = ring(2 * MIN_INPUT_FRAMES, origin);
        writer.write_i16(&[1000i16; 2 * 2971], origin);
        let mut out = [[0, 0]; FRAMES_PER_PACKET];
        for packet in 0..10 {
            reader.cut((packet * FRAMES_PER_PACKET) as i64, &mut out);
        }
        let floor = reader.floor();

        let at = origin + Duration::from_millis(100);
        writer.write_i16(&ramp(941), at);
        writer.write_i16(&[5000i16; 2 * 941], at + Duration::from_millis(2));

        assert_eq!(writer.position(), floor + 392 + 941);
        reader.cut(floor as i64, &mut out);
        assert_eq!(out[0], [550, 550]);
        for packet in 1..4 {
            reader.cut(
                (floor + (packet * FRAMES_PER_PACKET) as u64) as i64,
                &mut out,
            );
        }
        assert_eq!(reader.dropped(), 549);
    }

    fn halves(first: i16, second: i16) -> Vec<i16> {
        let mut samples = vec![first; 2 * FRAMES_PER_PACKET];
        samples.extend([second; 2 * FRAMES_PER_PACKET]);
        samples
    }

    // A resume re-anchors the stream: the write after the resumed one
    // starts at its own first frame even when the stream already cut past
    // the resumed audio, so none of it counts as late.
    // covers: P112
    #[test]
    fn the_write_after_a_resume_is_never_partly_late() {
        let origin = Instant::now();
        let (mut writer, mut reader) = ring(2 * MIN_INPUT_FRAMES, origin);
        let block = [1000i16; 2 * FRAMES_PER_PACKET];
        writer.write_i16(&block, origin);
        let mut out = [[0, 0]; FRAMES_PER_PACKET];
        for packet in 0..10 {
            reader.cut((packet * FRAMES_PER_PACKET) as i64, &mut out);
        }
        let at = origin + Duration::from_millis(100);
        writer.write_i16(&[2000i16; 2 * FRAMES_PER_PACKET], at);
        writer.write_i16(
            &[3000i16; 2 * FRAMES_PER_PACKET],
            at + Duration::from_millis(8),
        );
        let resumed = reader.floor();
        for packet in 0..3 {
            reader.cut(
                (resumed + packet * FRAMES_PER_PACKET as u64) as i64,
                &mut out,
            );
        }
        let floor = reader.floor();
        assert!(floor > writer.position());

        writer.write_i16(&halves(4000, 4500), at + Duration::from_millis(16));

        reader.cut(floor as i64, &mut out);
        assert_eq!(out[0], [4000, 4000]);
        assert_eq!(reader.dropped(), 0);
    }

    // P143: a restart starts the timeline over at the host's next frame,
    // even when the stream sent silence between the restart and that frame.
    // covers: P143
    #[test]
    fn the_first_write_after_a_restart_is_never_partly_late() {
        let origin = Instant::now();
        let (mut writer, mut reader) = ring(2 * MIN_INPUT_FRAMES, origin);
        writer.write_i16(&[1000i16; 20 * FRAMES_PER_PACKET], origin);
        let mut out = [[0, 0]; FRAMES_PER_PACKET];
        for packet in 0..10 {
            reader.cut((packet * FRAMES_PER_PACKET) as i64, &mut out);
        }
        let restarted = reader.rebase();
        let discarded = reader.dropped();
        reader.cut(restarted as i64, &mut out);
        let floor = reader.floor();
        assert!(floor > writer.position());

        writer.write_i16(&halves(4000, 4500), origin + Duration::from_millis(300));

        reader.cut(floor as i64, &mut out);
        assert_eq!(out[0], [4000, 4000]);
        assert_eq!(reader.dropped(), discarded);
    }

    // P112: the re-anchoring covers one write only; a later write that
    // comes behind is late as before, so jitter cannot move the timeline
    // (the write after it comes at once, so P248 doesn't take it as resumed).
    // covers: P112
    #[test]
    fn only_the_first_write_after_a_restart_is_placed_as_resumed() {
        let origin = Instant::now();
        let (mut writer, mut reader) = ring(2 * MIN_INPUT_FRAMES, origin);
        writer.write_i16(&[1000i16; 20 * FRAMES_PER_PACKET], origin);
        let mut out = [[0, 0]; FRAMES_PER_PACKET];
        for packet in 0..10 {
            reader.cut((packet * FRAMES_PER_PACKET) as i64, &mut out);
        }
        let restarted = reader.rebase();
        reader.cut(restarted as i64, &mut out);
        let anchored = reader.floor();
        writer.write_i16(&halves(4000, 4500), origin + Duration::from_millis(300));
        for packet in 0..3 {
            reader.cut(
                (anchored + (packet * FRAMES_PER_PACKET) as u64) as i64,
                &mut out,
            );
        }
        let floor = reader.floor();
        assert!(floor > writer.position());
        let discarded = reader.dropped();

        writer.write_i16(&halves(5000, 5500), origin + Duration::from_millis(316));
        writer.write_i16(&halves(6000, 6500), origin + Duration::from_millis(316));

        reader.cut(floor as i64, &mut out);
        assert_eq!(out[0], [5500, 5500]);
        assert_eq!(reader.dropped() - discarded, FRAMES_PER_PACKET as u64);
    }
}
