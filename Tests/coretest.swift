// The app's wrappers of the Rust core (no audio hardware). The core's own
// tests (core/) cover the DSP, the delay reader and the rules on their own;
// this suite checks that the Swift side (CoreEngine.swift, EngineParts.swift)
// hands them the right values and reads back what they report. Built with
// the core, which Tests/run-tests.sh builds first.
// Run with: Tests/run-tests.sh
import AppKit

var failedChecks = 0
func check(_ ok: Bool, _ name: String, _ detail: String = "") {
    print((ok ? "✅ PASS  " : "❌ FAIL  ") + name + (detail.isEmpty ? "" : "  — " + detail))
    if !ok { failedChecks += 1 }
}

// MARK: The app's wrappers of the core (CoreEngine.swift, EngineParts.swift)

func approx(_ a: Double, _ b: Double, _ tol: Double) -> Bool { abs(a - b) <= tol }

/// One captured block through a session's parts: equalizer and ring (S7).
func sessionChecks() {
    guard let parts = EngineParts.make() else {
        check(false, "session: the core's parts can be made")
        return
    }
    parts.prepare(captureRate: 48000)
    var block = [Float](repeating: 0.1, count: 512 * 2)
    block.withUnsafeMutableBufferPointer { parts.deliver($0.baseAddress!, frames: 512) }
    check(parts.ring.framesWritten == 512, "session: a captured block reaches the ring (S7)")
    check(parts.makeRenderer(captureRate: 48000) != nil, "session: a renderer can be made on the session's ring")
}

/// A local output's reader the way OutputRenderer drives it: the delay set in
/// ms, every block read at that target (S3, S46, S49, S56, S158, S159).
func readerChecks() {
    guard let ring = CoreDelayRing(capacityFrames: 48000 * 24),
          let reader = CoreDelayReader(ring: ring, captureRate: 48000) else {
        check(false, "delay: the core's ring and reader can be made")
        return
    }
    var src = [Float](repeating: 0, count: 1024), dst = [Float](repeating: 0, count: 1024)
    var produced: Int64 = 0
    var lastPlayed: Float = 0, replayed = false
    /// Writes one block of numbered frames and reads one; the lag of the
    /// block's last frame read, or nil while the read was silent.
    func step() -> Int64? {
        for i in 0..<512 { src[i * 2] = Float(produced + Int64(i) + 1); src[i * 2 + 1] = 0 }
        ring.write(interleaved: src, frames: 512)
        produced += 512
        reader.readAtTarget(into: &dst, frames: 512)
        for i in 0..<512 where dst[i * 2] != 0 {
            if dst[i * 2] <= lastPlayed { replayed = true }
            lastPlayed = dst[i * 2]
        }
        let last = dst[511 * 2]
        return last == 0 ? nil : produced - Int64(last)
    }
    reader.setDelay(ms: 2000)   // before the first read, as OutputRenderer.start does
    for _ in 0..<600 { _ = step() }
    check(step() == 96000 - 512, "delay: 2.0 s holds (last frame of the block)")
    var lagOff: [String] = []
    for ms in [500.0, 3000.0, 1820.0, 0.0, 150.0] {
        reader.setDelay(ms: ms)
        var lag: Int64?
        for _ in 0..<400 { lag = step() }   // an increase finishes its silence first
        let expected = max(Int64(Int(ms / 1000 * 48000)), 512) - 512
        check(lag == expected, "delay: change to \(Int(ms)) ms is exact", "lag \(lag ?? -1)")
        if reader.lagFrames != expected { lagOff.append("\(Int(ms)) ms: \(reader.lagFrames)") }
    }
    check(!replayed, "delay: live changes never replay audio (S49)")
    check(lagOff.isEmpty, "delay: lagFrames reads the lag after each change (S56)", lagOff.joined(separator: ", "))
    let counted = reader.counters
    check(counted.primes == 1 && counted.resyncs == 5 && counted.starvedRenders == 0,
          "delay: one prime, then one resync per delay change (S158)", "\(counted)")
    reader.resetPrime()   // as a stopped output does before it starts again
    check(reader.counters.resyncs == counted.resyncs, "delay: the counters survive resetPrime (S159)")
}

/// The equalizer gets its settings and design rate (S22–S33).
func equalizerChecks() {
    /// The gain at `hz`, in dB, of a 0.25 sine after a second of filtering.
    func gainDB(_ hz: Double, _ settings: Equalizer.Settings, rate: Double = 48000) -> Double {
        guard let eq = CoreEqualizer() else { return .nan }
        eq.setSampleRate(rate)
        eq.update(settings)
        let n = Int(rate)
        var signal = [Float](repeating: 0, count: n * 2)
        for i in 0..<n {
            let v = Float(0.25 * sin(2 * Double.pi * hz * Double(i) / rate))
            signal[i * 2] = v
            signal[i * 2 + 1] = v
        }
        signal.withUnsafeMutableBufferPointer { b in
            var o = 0
            while o < n {
                eq.process(b.baseAddress! + o * 2, frames: min(512, n - o))
                o += 512
            }
        }
        // RMS over the settled second half.
        var acc = 0.0
        for i in (n / 2)..<n { acc += Double(signal[i * 2]) * Double(signal[i * 2]) }
        return 20 * log10(sqrt(acc / Double(n - n / 2)) / (0.25 / 2.0.squareRoot()))
    }
    var s = Equalizer.Settings()
    s.enabled = true
    check(approx(gainDB(1000, s), 0, 0.05), "eq: flat is transparent")
    s.gains[5] = 6
    check(approx(gainDB(1000, s), 6, 0.2), "eq: +6 dB band at 1 kHz",
          String(format: "%.2f", gainDB(1000, s)))
    check(approx(gainDB(8000, s), 0, 0.5), "eq: 1 kHz band leaves 8 kHz alone")
    check(approx(gainDB(1000, s, rate: 44100), 6, 0.2), "eq: same response at 44.1 kHz")
    s.gains = Equalizer.presets.first { $0.id == .bassBoost }!.gains
    check(gainDB(40, s) > 5, "eq: Bass Boost lifts 40 Hz", String(format: "%.2f", gainDB(40, s)))
    s.preamp = -6
    s.gains = Equalizer.presets[0].gains
    check(approx(gainDB(1000, s), -6, 0.1), "eq: preamp −6 dB")
    s.enabled = false
    check(approx(gainDB(1000, s), 0, 0.01), "eq: disabled is bypassed")
    // A NaN sample must not leave the channel pinned at full scale.
    guard let eq = CoreEqualizer() else { return }
    var nan = Equalizer.Settings()
    nan.enabled = true
    nan.gains[5] = 6
    eq.update(nan)
    let n = 4800
    var b = [Float](repeating: 0, count: n * 2)
    b[0] = .nan
    b[1] = .nan
    b.withUnsafeMutableBufferPointer { eq.process($0.baseAddress!, frames: n) }
    b = [Float](repeating: 0, count: n * 2)
    b.withUnsafeMutableBufferPointer { eq.process($0.baseAddress!, frames: n) }
    let peak = b.map { abs($0) }.max() ?? 0
    check(peak == 0, "eq: recovers from a NaN sample (silence stays silent)", "peak \(peak)")
}

/// The volume curve and a local output's gain stage (S64, S65, S70, S71, S75).
func volumeChecks() {
    let curve = CoreGainCurve()
    let table: [(Double, Double)] = [(0, 0), (1, 1e-6), (10, 0.001), (25, 0.015625), (50, 0.125), (60, 0.216),
                                     (75, 0.421875), (90, 0.729), (100, 1)]
    let off = table.filter { !approx(curve.gain(position: $0.0), $0.1, 1e-12) }
    check(off.isEmpty, "core volume: the curve matches the reference table (S70)", "\(off)")
    check(approx(curve.position(gain: curve.gain(position: 37)), 37, 1e-9), "core volume: position↔gain round-trip")
    check(approx(curve.position(gain: 0.0349), 32.7, 0.1), "core volume: a legacy 3.49% migrates to ≈33 (S75)")
    check(approx(curve.outputGain(localPosition: 60, masterPosition: 50), 0.216 * 0.125, 1e-12),
          "core volume: a local output's gain is its own times the master's (S71)")
    guard let stage = CoreGainStage(), let fresh = CoreGainStage() else {
        check(false, "core gain: the gain stage can be made")
        return
    }
    var b = [Float](repeating: 0.5, count: 512 * 2)
    stage.setGain(0)   // set before the first buffer, as every renderer does
    b.withUnsafeMutableBufferPointer { stage.apply($0.baseAddress!, frames: 512) }
    check(b.allSatisfy { $0 == 0 }, "core gain: 0 is exact silence from the first buffer (S64)")
    stage.setGain(0.5)
    for _ in 0..<2 {
        b = [Float](repeating: 0.5, count: 512 * 2)
        b.withUnsafeMutableBufferPointer { stage.apply($0.baseAddress!, frames: 512) }
    }
    check(b.allSatisfy { $0 == 0.25 }, "core gain: 0.5 halves the signal once the change has ramped", "\(b[0])")
    var odd: [Float] = [.nan, .infinity, -.infinity, 0.5]
    odd.withUnsafeMutableBufferPointer { fresh.apply($0.baseAddress!, frames: 2) }
    check(odd == [0, 0, 0, 0.5], "core gain: NaN and ±inf never reach a device (S65)", "\(odd)")
}

sessionChecks()
readerChecks()
equalizerChecks()
volumeChecks()

print(failedChecks == 0 ? "\nALL CORE WRAPPER TESTS PASSED" : "\n\(failedChecks) FAILURE(S)")
exit(failedChecks > 0 ? 1 : 0)
