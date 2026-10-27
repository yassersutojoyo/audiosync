import Darwin
import Foundation
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
/// enters every output's path, before its hold-back. Which local output is
/// heard is the host's gains (`AppDelegate.effectiveGain`), not this tap.
/// Calibrate All Speakers (S235) sends the signal to AudioSync's own AirPlay
/// or Cast sender instead (`setTarget`); the other paths get silence then.
///
/// The capture thread calls `isActive`, `fadeMusic` and `addSignal` from
/// `EngineParts.deliver`, and the fill thread (`CalibrationFill`) from
/// `EngineParts.fill`, never both at once (`BlockGate`); everything else runs
/// on the main thread. One calibration at a time.
enum CalibrationTap {
    /// 20 ms at 48 kHz; a fixed count keeps the fade's state one number that
    /// only the capture thread writes.
    static let fadeFrames = 960

    /// Where the signal goes: the ring of the local outputs, or one of the
    /// network senders.
    enum Path: UInt8 {
        case local, airPlay, cast
    }

    private static let path = Atomic<UInt8>(Path.local.rawValue)

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
    /// That block came from `CalibrationFill`, not the capture.
    private static let startFilled = Atomic<Bool>(false)
    /// The signal's level in frames of its fade-out; written by the capture
    /// thread while it plays, by `play` before it does.
    private static let signalLevel = Atomic<Int>(fadeFrames)
    /// Set by `fadeOutSignal`.
    private static let stopping = Atomic<Bool>(false)
    /// Capture-thread calls that may be reading the installed signal.
    private static let users = Atomic<Int>(0)

    // MARK: Capture thread

    /// The path the signal goes to. One atomic load.
    static var target: Path { Path(rawValue: path.load(ordering: .acquiring)) ?? .local }

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
    /// asked for; `ticks` is the host time kept when the block carries the
    /// signal's first frame, `filled` whether the fill wrote the block.
    /// Real-time safe: atomics and sums.
    @inline(__always)
    static func addSignal(_ block: UnsafeMutablePointer<Float>, frames: Int, at ticks: UInt64, filled: Bool = false) {
        _ = users.wrappingAdd(1, ordering: .sequentiallyConsistent)
        defer { _ = users.wrappingSubtract(1, ordering: .sequentiallyConsistent) }
        let at = position.load(ordering: .sequentiallyConsistent)
        guard at >= 0, let raw = signal.load(ordering: .sequentiallyConsistent) else { return }
        let total = signalFrames.load(ordering: .sequentiallyConsistent)
        let samples = raw.assumingMemoryBound(to: Float.self)
        let count = max(0, min(frames, total - at))
        if at == 0 {
            startFilled.store(filled, ordering: .sequentiallyConsistent)
            startTicks.store(max(ticks, 1), ordering: .sequentiallyConsistent)
        }
        let fading = stopping.load(ordering: .sequentiallyConsistent)
        var l = signalLevel.load(ordering: .relaxed)
        for i in 0..<count {
            if fading { if l > 0 { l -= 1 } }
            let s = samples[at + i] * (Float(l) / Float(fadeFrames))
            block[2 * i] += s
            block[2 * i + 1] += s
        }
        signalLevel.store(l, ordering: .relaxed)
        let next = at + count >= total || l == 0 ? -1 : at + count
        _ = position.compareExchange(expected: at, desired: next, ordering: .sequentiallyConsistent)
    }

    // MARK: Main thread

    /// The captured audio fades out; the ring and the AirPlay sender get
    /// silence from then on.
    static func engage() { engaged.store(true, ordering: .releasing) }

    /// The captured audio fades back in.
    static func disengage() { engaged.store(false, ordering: .releasing) }

    /// Sends the signal played next to `target`; set while none plays.
    static func setTarget(_ target: Path) { path.store(target.rawValue, ordering: .releasing) }

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
        stopping.store(false, ordering: .sequentiallyConsistent)
        signalLevel.store(fadeFrames, ordering: .sequentiallyConsistent)
        position.store(0, ordering: .sequentiallyConsistent)
    }

    /// A signal still being written fades out over `fadeFrames` and ends,
    /// instead of stopping mid-waveform (S185).
    static func fadeOutSignal() { stopping.store(true, ordering: .sequentiallyConsistent) }

    /// The signal was asked for and its last frame isn't written yet.
    static var isPlaying: Bool { position.load(ordering: .sequentiallyConsistent) >= 0 }

    /// Host ticks of the write that carried the signal's first frame, or nil
    /// while it hasn't been written.
    static var playedAtTicks: UInt64? {
        let ticks = startTicks.load(ordering: .sequentiallyConsistent)
        return ticks == 0 ? nil : ticks
    }

    /// The signal's first frame rode a block of the calibration's silence
    /// fill rather than a captured one; read once `playedAtTicks` is set.
    static var playedOnFill: Bool { startFilled.load(ordering: .sequentiallyConsistent) }

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

/// How much silence the calibration's fill writes (`CalibrationFill`), from
/// the host clock and the time of the capture's latest block. While the
/// capture delivers, nothing; once it has given nothing for `idle(captureFrames:)`, the
/// ring is kept in real time from that moment on, the gap included.
struct CalibrationFillPace {
    /// 40 ms: more than a tap's block (1024 frames at 44.1 kHz last 23 ms)
    /// and inside the readers' 50 ms dead band (S50).
    static let idleNs: UInt64 = 40_000_000

    /// How long the capture must give nothing to count as quiet: `idleNs`,
    /// or two of its callback periods when its buffers are longer, as an
    /// input device's can be when another app raised them (4096 frames).
    func idle(captureFrames: Int) -> UInt64 {
        let periods = (2 * Double(max(0, captureFrames)) * 1e9 / rate).rounded(.up)
        return max(Self.idleNs, UInt64(min(periods, 1e12)))
    }
    /// The most one step writes; a fill held up longer drops the rest.
    static let mostSeconds = 1.0

    let rate: Double
    /// When the first frame of this stretch of fill is due; nil while the capture delivers.
    private var anchorNs: UInt64?
    /// Frames written from `anchorNs` on, so many small steps never add up rounding.
    private var written: Int64 = 0

    init(rate: Double) { self.rate = rate.isFinite && rate > 0 ? rate : 48_000 }

    /// The frames to write at `nowNs` and when the first of them is due.
    /// `lastCaptureNs` is 0 before the capture's first block; `captureFrames`
    /// is the frame count of its latest callback, 0 before the first.
    mutating func step(nowNs: UInt64, lastCaptureNs: UInt64, captureFrames: Int = 0) -> (frames: Int, firstNs: UInt64) {
        let quiet = idle(captureFrames: captureFrames)
        if lastCaptureNs != 0, lastCaptureNs >= nowNs || nowNs - lastCaptureNs < quiet {
            anchorNs = nil
            return (0, 0)
        }
        guard nowNs >= quiet else { return (0, 0) }
        let anchor: UInt64
        if let anchorNs {
            anchor = anchorNs
        } else {
            anchor = nowNs - quiet
            anchorNs = anchor
            written = 0
        }
        let due = Int64((Double(nowNs - anchor) * rate / 1e9).rounded(.down))
        let most = Int64(rate * Self.mostSeconds)
        if due - written > most { written = due - most }
        let frames = max(0, due - written)
        let firstNs = anchor + UInt64((Double(written) * 1e9 / rate).rounded())
        written += frames
        return (Int(frames), firstNs)
    }
}

/// Writes silence through a session's parts while a calibration runs and the
/// capture delivers nothing, so the test signal has blocks to ride on: a
/// process tap delivers no block while its source plays nothing. Its thread
/// wakes every 2 ms and never allocates, locks or logs. Main thread: `init`,
/// `start`, `stop`.
final class CalibrationFill {
    static let chunkFrames = 512

    private let parts: EngineParts
    private let clock = HostClock.shared
    private let block: UnsafeMutablePointer<Float>
    /// The fill thread's own.
    private var pace: CalibrationFillPace
    private let running = Atomic<Bool>(true)
    private let finished = Atomic<Bool>(false)
    private var started = false

    init(parts: EngineParts, captureRate: Double) {
        self.parts = parts
        pace = CalibrationFillPace(rate: captureRate)
        block = .allocate(capacity: 2 * Self.chunkFrames)
        block.initialize(repeating: 0, count: 2 * Self.chunkFrames)
    }

    deinit { block.deallocate() }

    func start() {
        guard !started else { return }
        started = true
        let thread = Thread { self.run() }
        thread.qualityOfService = .userInteractive
        thread.start()
    }

    /// Returns once the thread has written its last block, so nothing it
    /// writes can meet a later session's capture. Waits about 2 ms at most.
    func stop() {
        running.store(false, ordering: .releasing)
        guard started else { return }
        while !finished.load(ordering: .acquiring) { usleep(200) }
    }

    // MARK: Fill thread

    private func run() {
        while running.load(ordering: .acquiring) {
            let last = parts.gate.lastCaptureTicks.load(ordering: .acquiring)
            let step = pace.step(nowNs: clock.nowNs(), lastCaptureNs: clock.nanoseconds(ticks: last),
                                 captureFrames: parts.gate.lastCallbackFrames.load(ordering: .relaxed))
            var done = 0
            while done < step.frames {
                let count = min(step.frames - done, Self.chunkFrames)
                block.update(repeating: 0, count: 2 * count)
                let dueNs = step.firstNs + UInt64((Double(done) * 1e9 / pace.rate).rounded())
                parts.fill(block, frames: count, ticks: clock.ticks(nanoseconds: dueNs))
                done += count
            }
            usleep(2000)
        }
        finished.store(true, ordering: .releasing)
    }
}
