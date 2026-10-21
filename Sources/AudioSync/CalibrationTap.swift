import Darwin
import Synchronization

/// The capture's way to play the calibration signal (S232, S233) through the
/// session's own ring, so it reaches each local output through its renderer
/// at the hold-back it has, like the music would.
///
/// While engaged, the captured audio fades out over `fadeFrames` and every
/// block after that is silence, for the ring and for AudioSync's own AirPlay
/// sender alike; disengaged, it fades back in. A signal asked for with
/// `play` is added into the ring's blocks only, from the start of the next
/// block, and the host time of that write is kept: the moment the signal
/// enters every output's path, before its hold-back. Which output is heard is
/// the host's gains (`AppDelegate.effectiveGain`), not this tap.
///
/// The capture thread calls `isActive`, `fadeMusic` and `addSignal` from
/// `EngineParts.deliver`; everything else runs on the main thread. One
/// calibration at a time.
enum CalibrationTap {
    /// 20 ms at 48 kHz; a fixed count keeps the fade's state one number that
    /// only the capture thread writes.
    static let fadeFrames = 960

    private static let engaged = Atomic<Bool>(false)
    /// The music's level in frames of the fade: 0 silent, `fadeFrames` full.
    /// Written by the capture thread only.
    private static let level = Atomic<Int>(fadeFrames)
    /// The installed signal: mono samples at the capture rate, or nil.
    private static let signal = Atomic<UnsafeMutableRawPointer?>(nil)
    private static let signalFrames = Atomic<Int>(0)
    /// The next signal frame to write; -1 while no signal is asked for.
    private static let position = Atomic<Int>(-1)
    /// Host ticks of the block that carried the signal's first frame; 0 before.
    private static let startTicks = Atomic<UInt64>(0)
    /// Capture-thread calls that may be reading the installed signal.
    private static let users = Atomic<Int>(0)

    // MARK: Capture thread

    /// Engaged, or still fading back in. Two atomic loads.
    static var isActive: Bool {
        engaged.load(ordering: .relaxed) || level.load(ordering: .relaxed) < fadeFrames
    }

    /// The captured audio of `frames` interleaved stereo frames, faded toward
    /// silence while engaged and back toward full otherwise. Real-time safe.
    @inline(__always)
    static func fadeMusic(_ block: UnsafeMutablePointer<Float>, frames: Int) {
        let down = engaged.load(ordering: .acquiring)
        var l = level.load(ordering: .relaxed)
        for i in 0..<frames {
            if down { if l > 0 { l -= 1 } } else if l < fadeFrames { l += 1 }
            let gain = Float(l) / Float(fadeFrames)
            block[2 * i] *= gain
            block[2 * i + 1] *= gain
        }
        level.store(l, ordering: .relaxed)
    }

    /// Adds the signal's next frames into `block`, both channels, when one is
    /// asked for. Real-time safe: atomics, `mach_absolute_time` and sums.
    @inline(__always)
    static func addSignal(_ block: UnsafeMutablePointer<Float>, frames: Int) {
        _ = users.wrappingAdd(1, ordering: .sequentiallyConsistent)
        defer { _ = users.wrappingSubtract(1, ordering: .sequentiallyConsistent) }
        let at = position.load(ordering: .sequentiallyConsistent)
        guard at >= 0, let raw = signal.load(ordering: .sequentiallyConsistent) else { return }
        let total = signalFrames.load(ordering: .sequentiallyConsistent)
        let samples = raw.assumingMemoryBound(to: Float.self)
        let count = max(0, min(frames, total - at))
        if at == 0 { startTicks.store(mach_absolute_time(), ordering: .sequentiallyConsistent) }
        for i in 0..<count {
            let s = samples[at + i]
            block[2 * i] += s
            block[2 * i + 1] += s
        }
        let next = at + count >= total ? -1 : at + count
        // A cancel stored -1 meanwhile: it stands.
        _ = position.compareExchange(expected: at, desired: next, ordering: .sequentiallyConsistent)
    }

    // MARK: Main thread

    /// The captured audio fades out; the ring and the AirPlay sender get
    /// silence from then on.
    static func engage() { engaged.store(true, ordering: .releasing) }

    /// The captured audio fades back in.
    static func disengage() { engaged.store(false, ordering: .releasing) }

    /// Installs `samples` (mono, at the capture rate, kept alive by the caller
    /// until `uninstall` returns) without playing it.
    static func install(_ samples: UnsafeMutablePointer<Float>, frames: Int) {
        position.store(-1, ordering: .sequentiallyConsistent)
        signalFrames.store(frames, ordering: .sequentiallyConsistent)
        signal.store(UnsafeMutableRawPointer(samples), ordering: .sequentiallyConsistent)
    }

    /// Plays the installed signal from the next block on.
    static func play() {
        startTicks.store(0, ordering: .sequentiallyConsistent)
        position.store(0, ordering: .sequentiallyConsistent)
    }

    /// Host ticks of the write that carried the signal's first frame, or nil
    /// while it hasn't been written.
    static var playedAtTicks: UInt64? {
        let ticks = startTicks.load(ordering: .sequentiallyConsistent)
        return ticks == 0 ? nil : ticks
    }

    /// Takes the signal out of the capture path and returns once no capture
    /// call can still be reading it, so its owner may free it. It may wait
    /// for one block.
    static func uninstall() {
        position.store(-1, ordering: .sequentiallyConsistent)
        signal.store(nil, ordering: .sequentiallyConsistent)
        while users.load(ordering: .sequentiallyConsistent) != 0 { usleep(50) }
        position.store(-1, ordering: .sequentiallyConsistent)
        signalFrames.store(0, ordering: .sequentiallyConsistent)
    }
}
