// Checks the soak harness's memory rule (Tests/soakrules.swift) on recorded
// footprint series, so it needs no audio hardware. Run all tests with:
// Tests/run-tests.sh
import Foundation

var failedChecks = 0
func check(_ ok: Bool, _ name: String, _ detail: @autoclosure () -> String = "") {
    let d = detail()
    print((ok ? "✅ PASS  " : "❌ FAIL  ") + name + (d.isEmpty ? "" : "  — " + d))
    if !ok { failedChecks += 1 }
}
func approx(_ a: Double, _ b: Double, _ tol: Double = 1e-9) -> Bool { abs(a - b) <= tol }

/// The release baseline's two_local memory growth (Tests/baseline/rust.json),
/// still measured from the first window; the next full Tests/bench.sh run on
/// the Mac measures it again with the warm-up.
let baselineGrowth = 2.109
let releaseLimit = SoakRules.memoryGrowthLimit(baseline: baselineGrowth)

/// The harness's verdict on a series, as Tests/soaktest.swift judges it.
func passes(_ samples: [MemorySample], limit: Double = releaseLimit) -> Bool {
    SoakRules.memoryGrowth(samples).growth <= limit
}
func describe(_ samples: [MemorySample], limit: Double = releaseLimit) -> String {
    let g = SoakRules.memoryGrowth(samples)
    return String(format: "growth %.3f MB from %.3f MB after %.0f s, limit %.3f MB",
                  g.growth, g.settledMB, g.warmUpSeconds, limit)
}

/// One sample per 10-second window, the way the soak logs them.
func series(minutes: Int, _ mb: (Double) -> Double) -> [MemorySample] {
    (1...(minutes * 6)).map { i in MemorySample(t: Double(i * 10), mb: mb(Double(i * 10))) }
}

/// A run shaped like the 10-minute soak that tripped the first-to-last rule:
/// the first window at 12 MB, before the session's buffers had their pages,
/// a sharp rise to about 42 MB in the first 1.5 minutes, then flat within a
/// few tenths of a MB to the end (42.1 MB in the last window).
func warmUpThenFlat(minutes: Int) -> [MemorySample] {
    let rise: [Double] = [12.0, 18.4, 24.9, 30.7, 35.2, 38.6, 40.5, 41.4, 41.8]   // t = 10…90 s
    let wobble: [Double] = [0.0, 0.2, -0.1, 0.3, -0.2, 0.1]
    return series(minutes: minutes) { t in
        let i = Int((t / 10).rounded())
        return i <= rise.count ? rise[i - 1] : 42.1 + wobble[i % wobble.count]
    }
}

// MARK: Warm-up, then flat: no leak
let soak10 = warmUpThenFlat(minutes: 10)
check(approx(soak10.last!.mb - soak10.first!.mb, 30.1, 1e-6),
      "the 10-minute series climbs 30.1 MB from its first window, as the run did")
check(passes(soak10), "a 10-minute run that rises to 42 MB in its warm-up and then stays flat passes",
      describe(soak10))
let noGrowthLimit = SoakRules.memoryGrowthLimit(baseline: 0)
check(passes(soak10, limit: noGrowthLimit),
      "the same run passes against a baseline that grew nothing (limit 5 MB)", describe(soak10, limit: noGrowthLimit))
let soak60 = warmUpThenFlat(minutes: 60)
check(passes(soak60), "a 60-minute run of the same shape passes", describe(soak60))
let g10 = SoakRules.memoryGrowth(soak10)
check(approx(g10.warmUpSeconds, 120), "a 10-minute run leaves out its first 2 minutes", describe(soak10))
check(approx(g10.settledMB, 42.1, 1e-6),
      "growth is measured from the median of the warm-up's last three windows (41.9, 42.2, 42.1)",
      describe(soak10))

// A run shorter than two warm-ups: the first half is the warm-up.
let rise2: [Double] = [12.0, 25.0, 36.0, 41.8, 42.1, 42.2, 42.0, 42.1, 42.2, 42.1, 42.0, 42.1]
let soak2 = rise2.enumerated().map { MemorySample(t: Double(($0.offset + 1) * 10), mb: $0.element) }
check(approx(SoakRules.memoryGrowth(soak2).warmUpSeconds, 60), "a 2-minute run leaves out its first minute",
      describe(soak2))
check(passes(soak2), "a 2-minute run that settles in its first minute passes", describe(soak2))

// MARK: A real leak still fails
let leak10 = series(minutes: 10) { t in 40 + 1.5 * t / 60 }   // 1.5 MB a minute from the start
check(!passes(leak10), "a 10-minute run that rises 1.5 MB a minute the whole time fails", describe(leak10))
let leak60 = series(minutes: 60) { t in 40 + 0.2 * t / 60 }   // 12 MB an hour
check(!passes(leak60), "a 60-minute run that rises 12 MB an hour the whole time fails", describe(leak60))
let leakAfter = series(minutes: 10) { t in
    t <= 90 ? soak10[Int(t / 10) - 1].mb : 42.1 + 1.5 * (t - 90) / 60
}
check(!passes(leakAfter), "a run that keeps rising 1.5 MB a minute after its warm-up fails", describe(leakAfter))

// MARK: A spike in the warm-up doesn't hide a later leak
/// The 10-minute warm-up-then-flat run with one window, at `spikeAt` seconds,
/// jumping to 70 MB and straight back, and from the end of the 2-minute
/// warm-up a leak of `leakPerMinute` MB a minute (12 MB by the last window at
/// 1.5 MB a minute, which ends at 54.1 MB, still under the spike).
func spiked(at spikeAt: Double, leakPerMinute: Double) -> [MemorySample] {
    soak10.map { s in
        MemorySample(t: s.t, mb: approx(s.t, spikeAt) ? 70 : s.mb + max(s.t - 120, 0) * leakPerMinute / 60)
    }
}
let spikeEarly = spiked(at: 30, leakPerMinute: 1.5)
check(!passes(spikeEarly), "a 70 MB spike 30 s in doesn't hide a leak of 1.5 MB a minute after the warm-up",
      describe(spikeEarly))
let spikeLast = spiked(at: 120, leakPerMinute: 1.5)
check(!passes(spikeLast), "nor does a spike in the warm-up's last window", describe(spikeLast))
let spikeFlat = spiked(at: 30, leakPerMinute: 0)
check(passes(spikeFlat), "a spike in the warm-up with a flat run after it passes", describe(spikeFlat))

// MARK: Edges and the limit
let none = SoakRules.memoryGrowth([])
check(none.growth == 0 && none.settledMB == 0, "no windows: no growth")
let lone = [MemorySample(t: 10, mb: 40)]
check(SoakRules.memoryGrowth(lone).growth == 0, "a single window: no growth", describe(lone))
let twoInWarmUp = [MemorySample(t: 10, mb: 30), MemorySample(t: 20, mb: 40),
                   MemorySample(t: 30, mb: 41), MemorySample(t: 40, mb: 41)]
let gTwo = SoakRules.memoryGrowth(twoInWarmUp)
check(approx(gTwo.settledMB, 35) && approx(gTwo.growth, 6),
      "only two windows in the warm-up: growth counts from their mean", describe(twoInWarmUp))
check(approx(releaseLimit, 7.109, 1e-9), "limit: max(2 × 2.109, 2.109 + 5) = 7.109 MB",
      String(format: "%.3f", releaseLimit))
check(SoakRules.memoryGrowthLimit(baseline: 10) == 20, "limit: twice a large baseline")
check(SoakRules.memoryGrowthLimit(baseline: -1) == 5, "limit: a baseline that shrank counts as 0, so 5 MB")

// MARK: Lag error leaves out the windows the delay line is still filling
/// A Mac run at a 20 s delay and 10 s windows: -9939 ms at 10 s and -1205 ms
/// at 20 s while the delay line filled, then -11.0 to -11.5 ms to the end.
let longEnds = (1...360).map { Double($0 * 10) }
let longLag: [Double] = longEnds.map { t -> Double in t <= 10 ? -9939 : t <= 20 ? -1205 : (Int(t / 10) % 2 == 0 ? -11.0 : -11.5) }
let long = SoakRules.lagWindows(ends: longEnds, delayMs: 20000, intervalS: 10)
let longMax = long.counted.map { abs(longLag[$0]) }.max() ?? 0
check(long.filling == 2 && long.counted.first == 2 && long.counted.count == 358,
      "a 20 s delay with 10 s windows leaves out the windows ending at 10 s and 20 s",
      "filling \(long.filling), first counted \(long.counted.first ?? -1), counted \(long.counted.count)")
check(approx(longMax, 11.5), "that run's window max |lag error| is 11.5 ms, not 9939 ms", "\(longMax) ms")
check(approx(long.countsFrom, 30), "windows count from the delay plus one window, 30 s", "\(long.countsFrom) s")
let short = SoakRules.lagWindows(ends: [10, 20, 30, 40], delayMs: 2000, intervalS: 10)
check(short.filling == 1 && short.counted == [1, 2, 3], "a 2 s delay with 10 s windows leaves out the first window",
      "filling \(short.filling), counted \(short.counted)")
let late = SoakRules.lagWindows(ends: [10, 12.004, 22], delayMs: 2000, intervalS: 10)
check(late.counted == [1, 2], "a window ending just after the delay plus one window counts", "counted \(late.counted)")
let allFilling = SoakRules.lagWindows(ends: [10, 20], delayMs: 20000, intervalS: 10)
check(allFilling.counted == [0, 1] && allFilling.filling == 0,
      "a run that ends before the delay line filled counts every window rather than none",
      "filling \(allFilling.filling), counted \(allFilling.counted)")
check(SoakRules.lagWindows(ends: [], delayMs: 2000, intervalS: 10).counted.isEmpty, "no windows: none counted")

print(failedChecks == 0 ? "\nALL SOAK RULE TESTS PASSED" : "\n\(failedChecks) FAILURE(S)")
exit(failedChecks > 0 ? 1 : 0)
