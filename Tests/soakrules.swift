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
    /// Memory growth over a run: the last window's footprint minus the first
    /// window's.
    static func memoryGrowth(_ samples: [MemorySample]) -> MemoryGrowth {
        guard let first = samples.first, let last = samples.last else {
            return MemoryGrowth(growth: 0, settledMB: 0, warmUpSeconds: 0)
        }
        return MemoryGrowth(growth: last.mb - first.mb, settledMB: first.mb, warmUpSeconds: 0)
    }

    /// The most a run's memory may grow, from the baseline run's growth:
    /// max(2 × baseline, baseline + 5) MB, a negative baseline counting as 0.
    static func memoryGrowthLimit(baseline: Double) -> Double {
        let b = max(baseline, 0)
        return max(2 * b, b + 5)
    }
}
