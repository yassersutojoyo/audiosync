//! Where each captured buffer falls in time, for the capture's
//! [`GapFill`](crate::audio::capture::GapFill), in its 100 ns units on the
//! system's monotonic clock (`CLOCK_MONOTONIC`, which PipeWire's graph
//! clock also counts in).
//!
//! PipeWire tells when the graph cycle that filled the newest buffer ran,
//! not where each buffer starts; buffers left from an earlier cycle can come
//! in the same callback, and then look one cycle late. So a buffer that
//! starts up to [`SNAP_HNS`] after the end of the one before it follows on
//! from it; one further away starts a gap (a suspended output, an xrun).
//! The newest buffer may still lag its cycle by a buffer or two (a lost
//! one, or one the main loop hasn't seen yet); the idle timer then waits
//! that much longer, see [`PacketClock::settle`]. Closer than [`GAP_JUMP_HNS`] the cycle's time counts, so the device
//! clock's drift never adds up.

/// 100 ns units per second.
const HNS_PER_SECOND: u64 = 10_000_000;

/// Up to this far, 5 ms, from where it should be, a buffer is where the
/// cycle's time puts it: [`GapFill`](crate::audio::capture::GapFill) treats
/// that as jitter too.
const GAP_JUMP_HNS: u64 = 50_000;

/// A buffer that seems to start up to 50 ms after the one before it ends is
/// taken to follow on from it: more than two cycles at the 1024-frame
/// quantum AudioSync asks for, so a buffer one cycle late is not a gap.
const SNAP_HNS: u64 = 500_000;

/// How long `frames` frames at `rate` last, in 100 ns units.
pub fn length_hns(frames: usize, rate: u32) -> u64 {
    (frames as u128 * u128::from(HNS_PER_SECOND) / u128::from(rate.max(1))) as u64
}

/// Places each buffer of a capture at `rate`.
pub struct PacketClock {
    rate: u32,
    /// Where the last buffer placed ends; `None` before the first.
    end: Option<u64>,
    /// How long the last buffer lasts.
    last_length: u64,
    /// How far the last callback's newest buffer ended before its cycle.
    lag: u64,
}

impl PacketClock {
    pub fn new(rate: u32) -> PacketClock {
        PacketClock {
            rate,
            end: None,
            last_length: 0,
            lag: 0,
        }
    }

    /// Where a buffer of `frames` frames starts, when the graph cycle that
    /// delivered it ran at `cycle_hns` (the end of its last frame).
    pub fn place(&mut self, cycle_hns: u64, frames: usize) -> u64 {
        let length = length_hns(frames, self.rate);
        let estimate = cycle_hns.saturating_sub(length);
        let start = match self.end {
            Some(end) if estimate > end.saturating_add(GAP_JUMP_HNS) => {
                if estimate - end <= SNAP_HNS {
                    end
                } else {
                    estimate
                }
            }
            _ => estimate,
        };
        self.end = Some(start.saturating_add(length));
        self.last_length = length;
        start
    }

    /// After the last buffer of a callback whose cycle ran at `cycle_hns`.
    /// The positions only ever move back to the cycle, never ahead of it: a
    /// cycle past the newest buffer is a lost buffer or one still on its
    /// way, and only the buffer's own time could tell them apart.
    pub fn settle(&mut self, cycle_hns: u64) {
        let Some(end) = self.end else { return };
        if end > cycle_hns {
            self.end = Some(cycle_hns);
        }
        self.lag = cycle_hns.saturating_sub(end).min(SNAP_HNS);
    }

    /// The `now` to hand [`GapFill::idle`](crate::audio::capture::GapFill::idle)
    /// at `now_hns`: two buffers' length earlier, and as far again as the
    /// positions run behind the cycle. GapFill counts a gap 25 ms after the
    /// last frame, but PipeWire's buffers can each last that long or more,
    /// so the next one would always find silence already written.
    pub fn idle_now(&self, now_hns: u64) -> u64 {
        now_hns.saturating_sub(2 * self.last_length + self.lag)
    }
}

/// The system's monotonic clock now, in 100 ns units.
#[cfg(target_os = "linux")]
pub fn monotonic_hns() -> u64 {
    let mut now = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: writes one timespec, which lives on this stack frame.
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut now) };
    let seconds = u64::try_from(now.tv_sec).unwrap_or(0);
    let nanos = u64::try_from(now.tv_nsec).unwrap_or(0);
    seconds.saturating_mul(HNS_PER_SECOND) + nanos / 100
}

#[cfg(test)]
mod tests {
    use super::*;

    const RATE: u32 = 48_000;
    /// 1024 frames at 48 kHz, in 100 ns units.
    const QUANTUM: u64 = 213_333;

    #[test]
    fn a_steady_stream_follows_the_cycles() {
        let mut clock = PacketClock::new(RATE);
        let mut cycle = 10 * HNS_PER_SECOND;
        let first = clock.place(cycle, 1024);
        assert_eq!(first, cycle - QUANTUM);
        for _ in 0..100 {
            cycle += QUANTUM;
            let start = clock.place(cycle, 1024);
            assert_eq!(start, cycle - QUANTUM);
        }
    }

    #[test]
    fn two_buffers_in_one_callback_follow_on() {
        let mut clock = PacketClock::new(RATE);
        let cycle = 10 * HNS_PER_SECOND;
        let first = clock.place(cycle, 1024);
        // The next callback comes two cycles later with both buffers: the
        // older one is placed one cycle late, then snapped back.
        let late = cycle + 2 * QUANTUM;
        let second = clock.place(late, 1024);
        assert_eq!(second, first + QUANTUM);
        let third = clock.place(late, 1024);
        assert_eq!(third, second + QUANTUM);
    }

    #[test]
    fn a_cycle_ahead_of_its_buffer_keeps_the_buffers_in_a_row() {
        // The main loop runs each callback after the next cycle already
        // started, so every callback's newest buffer is one cycle old.
        let mut clock = PacketClock::new(RATE);
        let mut cycle = 10 * HNS_PER_SECOND;
        let mut end = clock.place(cycle, 1024) + QUANTUM;
        clock.settle(cycle);
        cycle += 2 * QUANTUM;
        for _ in 0..100 {
            let start = clock.place(cycle, 1024);
            clock.settle(cycle);
            assert_eq!(start, end);
            end = start + QUANTUM;
            cycle += QUANTUM;
        }
    }

    #[test]
    fn after_lost_cycles_the_idle_timer_waits_for_the_next_buffer() {
        let mut clock = PacketClock::new(RATE);
        let mut cycle = 10 * HNS_PER_SECOND;
        clock.place(cycle, 1024);
        clock.settle(cycle);
        // Two cycles' buffers never come: the positions run two buffers
        // behind the cycles from then on.
        cycle += 3 * QUANTUM;
        clock.place(cycle, 1024);
        clock.settle(cycle);
        for _ in 0..100 {
            let next = cycle + QUANTUM;
            assert!(clock.idle_now(next - 1) <= clock.end.unwrap());
            let start = clock.place(next, 1024);
            clock.settle(next);
            assert_eq!(start + QUANTUM, clock.end.unwrap());
            cycle = next;
        }
    }

    #[test]
    fn a_long_pause_is_a_gap() {
        let mut clock = PacketClock::new(RATE);
        let cycle = 10 * HNS_PER_SECOND;
        clock.place(cycle, 1024);
        let resumed = cycle + 3 * HNS_PER_SECOND;
        assert_eq!(clock.place(resumed, 1024), resumed - QUANTUM);
    }

    #[test]
    fn drift_is_followed_not_summed() {
        let mut clock = PacketClock::new(RATE);
        let mut cycle = 10 * HNS_PER_SECOND;
        clock.place(cycle, 1024);
        // The device runs 0.1 % fast against the monotonic clock: each cycle
        // comes a little early, and each buffer is placed by its cycle.
        for _ in 0..1000 {
            cycle += QUANTUM - 213;
            assert_eq!(clock.place(cycle, 1024), cycle - QUANTUM);
        }
    }

    #[test]
    fn idle_waits_two_buffers_longer() {
        let mut clock = PacketClock::new(RATE);
        assert_eq!(clock.idle_now(HNS_PER_SECOND), HNS_PER_SECOND);
        clock.place(HNS_PER_SECOND, 1024);
        assert_eq!(
            clock.idle_now(2 * HNS_PER_SECOND),
            2 * HNS_PER_SECOND - 2 * QUANTUM
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn the_monotonic_clock_moves_on() {
        let before = monotonic_hns();
        std::thread::sleep(std::time::Duration::from_millis(2));
        assert!(monotonic_hns() >= before + 20_000);
    }
}
