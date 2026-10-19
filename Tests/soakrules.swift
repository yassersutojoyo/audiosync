// The soak harness's judging rules that need no hardware, kept apart from
// Tests/soaktest.swift so Tests/run-tests.sh can check them on recorded series
// (Tests/soakrulestest.swift). Tests/build-harness.sh compiles this file into
// the harness. Declarations only: each program's main.swift holds the
// top-level code.

/// One logged window's memory sample: when the window ended, in seconds since
/// the measured run started, and the process's physical footprint in MB.
struct MemorySample {
    let t: Double
    let mb: Double
}

/// How much a run's memory grew, and from where it was measured.
struct MemoryGrowth {
    /// The last window's footprint minus `settledMB`, in MB.
    let growth: Double
    /// The footprint growth is measured from, in MB.
    let settledMB: Double
    /// The seconds at the start of the run that growth leaves out.
    let warmUpSeconds: Double
}

enum SoakRules {
    /// How long the footprint may climb before growth counts. The first window
    /// is logged while the session's buffers are still getting their pages, so
    /// a healthy run climbs once in its first minute or two (by some 30 MB in
    /// a 10-minute soak) and then stays flat.
    static let memoryWarmUpSeconds = 120.0

    /// The warm-up of a run lasting `runSeconds`: the first 2 minutes, or the
    /// first half of a run shorter than 4 minutes, so a short run still has a
    /// measured part.
    static func memoryWarmUp(runSeconds: Double) -> Double {
        min(memoryWarmUpSeconds, max(runSeconds, 0) / 2)
    }

    /// How many of the warm-up's last windows set the level a run settled at.
    static let memorySettleWindows = 3

    /// Memory growth over a run: the last window's footprint minus the level
    /// the run settled at, the median of the warm-up's last three windows (of
    /// those there are, when fewer). A climb that ends within the warm-up
    /// counts as nothing; a leak keeps climbing after it and shows up in full
    /// from the warm-up's end. The median, not the warm-up's highest window,
    /// so one short spike in the warm-up, even in its last window, can't
    /// raise the level and hide a later leak up to the spike's height. The
    /// windows come in time order, and the run lasts until its last one. With
    /// no window in the warm-up (windows longer than it), growth is measured
    /// from the first window.
    static func memoryGrowth(_ samples: [MemorySample]) -> MemoryGrowth {
        guard let first = samples.first, let last = samples.last else {
            return MemoryGrowth(growth: 0, settledMB: 0, warmUpSeconds: 0)
        }
        let warmUp = memoryWarmUp(runSeconds: last.t)
        let lastInWarmUp = samples.filter { $0.t <= warmUp }.suffix(memorySettleWindows).map(\.mb)
        let settled = median(lastInWarmUp) ?? first.mb
        return MemoryGrowth(growth: last.mb - settled, settledMB: settled, warmUpSeconds: warmUp)
    }

    /// The middle value, or the mean of the two middle values of an even
    /// count; nil when there are none.
    static func median(_ values: [Double]) -> Double? {
        guard !values.isEmpty else { return nil }
        let sorted = values.sorted()
        let mid = sorted.count / 2
        return sorted.count % 2 == 1 ? sorted[mid] : (sorted[mid - 1] + sorted[mid]) / 2
    }

    /// The most a run's memory may grow, from the baseline run's growth:
    /// max(2 × baseline, baseline + 5) MB, a negative baseline counting as 0.
    static func memoryGrowthLimit(baseline: Double) -> Double {
        let b = max(baseline, 0)
        return max(2 * b, b + 5)
    }
}
