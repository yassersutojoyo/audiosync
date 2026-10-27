//! A debug-log probe of how much audio an output's stream holds that its
//! device clock hasn't played yet, read [`PROBE_SECONDS`] after the output
//! starts. Render timing doesn't use it: it only shows whether a fresh
//! Bluetooth link's first stream sits deeper than one started later.
//!
//! The render thread samples into atomics through a [`ProbeSampler`]; the
//! controller takes the samples once all are in and logs one line. Without
//! `AUDIOSYNC_DEBUG=1` no probe exists and the render thread does nothing
//! for it.

#[cfg(any(windows, test))]
use std::sync::Arc;
#[cfg(any(windows, test))]
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

/// When the probe samples, in seconds after the output starts.
pub const PROBE_SECONDS: [u32; 4] = [1, 2, 5, 10];

/// One output's samples, in ms, at [`PROBE_SECONDS`]; `None` where the
/// stream's clock gave no reading.
pub type ClockQueue = [Option<u32>; PROBE_SECONDS.len()];

/// A sample not taken yet.
#[cfg(any(windows, test))]
const UNSET: u32 = u32::MAX;

/// A sample taken without a clock reading.
#[cfg(any(windows, test))]
const NO_CLOCK: u32 = u32::MAX - 1;

/// The samples one output's render thread shares with the controller.
#[cfg(any(windows, test))]
pub struct ClockProbe {
    samples: [AtomicU32; PROBE_SECONDS.len()],
    taken: AtomicBool,
}

#[cfg(any(windows, test))]
impl ClockProbe {
    /// A probe with no samples yet.
    pub fn new() -> ClockProbe {
        ClockProbe {
            samples: [const { AtomicU32::new(UNSET) }; PROBE_SECONDS.len()],
            taken: AtomicBool::new(false),
        }
    }

    /// A probe when the environment turns the debug log on, none otherwise.
    #[cfg(windows)]
    pub fn from_env() -> Option<Arc<ClockProbe>> {
        let setting = std::env::var_os(crate::debug_log::ENV_VAR);
        (setting.as_deref() == Some(std::ffi::OsStr::new("1"))).then(|| Arc::new(ClockProbe::new()))
    }

    /// The samples once every one is in, the first time only.
    pub fn take(&self) -> Option<ClockQueue> {
        let _ = &self.samples;
        let _ = &self.taken;
        None
    }
}

/// The render thread's side of a [`ClockProbe`]. Real-time: it never
/// allocates, locks or logs.
#[cfg(any(windows, test))]
pub struct ProbeSampler {
    probe: Arc<ClockProbe>,
    rate: u64,
    /// The index in [`PROBE_SECONDS`] of the next sample.
    next: usize,
}

#[cfg(any(windows, test))]
impl ProbeSampler {
    /// Samples into `probe` for a stream at `rate` Hz.
    pub fn new(probe: Arc<ClockProbe>, rate: u32) -> ProbeSampler {
        ProbeSampler {
            probe,
            rate: u64::from(rate),
            next: 0,
        }
    }

    /// Whether `elapsed` frames since the start reach the next sample time.
    pub fn is_due(&self, elapsed: u64) -> bool {
        let _ = (elapsed, self.rate, self.next);
        false
    }

    /// Records `queued` frames (`None` without a clock reading) for every
    /// sample time `elapsed` frames since the start reach.
    pub fn record(&mut self, elapsed: u64, queued: Option<u64>) {
        let _ = (elapsed, queued, &self.probe);
    }
}

/// The debug-log line for the output named `name`.
pub fn describe_clock_queue(name: &str, samples: &ClockQueue) -> String {
    let _ = (name, samples);
    String::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    const RATE: u32 = 48_000;

    fn sampler() -> (ProbeSampler, Arc<ClockProbe>) {
        let probe = Arc::new(ClockProbe::new());
        (ProbeSampler::new(Arc::clone(&probe), RATE), probe)
    }

    #[test]
    fn each_sample_is_taken_at_the_first_write_that_reaches_its_time() {
        let (mut sampler, probe) = sampler();
        assert!(!sampler.is_due(47_999));
        assert!(sampler.is_due(48_000));
        sampler.record(48_000, Some(4_800));
        assert!(!sampler.is_due(95_999));
        assert_eq!(probe.take(), None, "not every sample is in yet");
        sampler.record(96_000, Some(9_600));
        sampler.record(240_000, Some(2_400));
        assert!(!sampler.is_due(479_999));
        sampler.record(480_000, None);
        assert!(!sampler.is_due(u64::MAX), "nothing is due after the last");
        assert_eq!(probe.take(), Some([Some(100), Some(200), Some(50), None]));
        assert_eq!(probe.take(), None, "the samples are taken once");
    }

    #[test]
    fn one_write_past_several_sample_times_fills_each_of_them() {
        let (mut sampler, probe) = sampler();
        sampler.record(250_000, Some(4_800));
        assert!(!sampler.is_due(250_000));
        assert!(sampler.is_due(480_000));
        sampler.record(480_000, Some(4_824));
        assert_eq!(
            probe.take(),
            Some([Some(100), Some(100), Some(100), Some(100)])
        );
    }

    #[test]
    fn a_write_before_the_next_sample_time_records_nothing() {
        let (mut sampler, probe) = sampler();
        sampler.record(10_000, Some(4_800));
        sampler.record(48_000, Some(9_600));
        sampler.record(96_000, Some(9_600));
        sampler.record(240_000, Some(9_600));
        sampler.record(480_000, Some(9_600));
        assert_eq!(
            probe.take(),
            Some([Some(200), Some(200), Some(200), Some(200)])
        );
    }

    #[test]
    fn the_line_names_the_output_and_each_sample() {
        assert_eq!(
            describe_clock_queue(
                "Bluetooth speaker",
                &[Some(212), Some(190), Some(185), None]
            ),
            "output \"Bluetooth speaker\" clock queue: 1s 212 ms, 2s 190 ms, 5s 185 ms, 10s no clock"
        );
    }
}
