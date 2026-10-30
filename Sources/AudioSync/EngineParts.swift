import Foundation
import Synchronization

// One session's audio parts, all on the Rust core (CoreEngine.swift): the
// equalizer and the delay ring. The capture IOProc (`deliver`) and the render
// callbacks call these final classes directly, so no audio callback
// dispatches through a protocol. Clause numbers (S…) refer to
// docs/ENGINE_SPEC.md.

/// Whether an AirPlay speaker is chosen (S79), which adds the AirPlay delay
/// on top of every local output's hold-back (S176, `LocalDelay`): AudioSync's
/// own sender decides it (`NativeSyncRule`).
protocol SyncRule {
    /// Some AirPlay speaker the user enabled is in the sender's latest list.
    func airPlayChosen(enabled: Set<String>, listed: [String]) -> Bool
}

/// The audio parts of one session, made when it starts and dropped when it
/// stops, so a new session never hears the previous one's history.
struct EngineParts {
    let equalizer: CoreEqualizer
    let ring: CoreDelayRing
    let gate = BlockGate()

    /// The highest capture rate (Hz) the ring is sized for (S36).
    static let highestCaptureRate = 192_000

    /// 24 s at `highestCaptureRate`: the 20 s maximum delay plus headroom,
    /// whatever the session's rate.
    static let ringCapacityFrames = highestCaptureRate * 24

    /// New parts with the equalizer set to `settings`. Nil when the core
    /// can't make one of them.
    static func make(equalizer settings: Equalizer.Settings = Equalizer.Settings()) -> EngineParts? {
        guard let equalizer = CoreEqualizer(), let ring = CoreDelayRing(capacityFrames: ringCapacityFrames)
        else { return nil }
        equalizer.update(settings)
        return EngineParts(equalizer: equalizer, ring: ring)
    }

    /// Control thread, while capture is stopped (each tap build): the
    /// capture rate the equalizer and AudioSync's own AirPlay and Cast
    /// senders (when they run) convert from.
    func prepare(captureRate: Double) {
        equalizer.setSampleRate(captureRate)
        gate.prepare(captureRate: captureRate)
        NativeAirPlayTap.prepare(captureRate: captureRate)
        CastTap.prepare(captureRate: captureRate)
    }

    /// Capture thread: one captured block, equalized once in place, then into
    /// the ring and AudioSync's own AirPlay and Cast senders when they
    /// run (`NativeAirPlayTap`, `CastTap`), the same samples for all (S7). During a
    /// microphone calibration (`CalibrationTap`) the music fades to silence
    /// on every path and the test signal goes into the one path it targets.
    /// A block that meets the fill (`QuietFill`) writing is dropped: the
    /// fill writes only after the capture gave nothing for a while, so only
    /// the first block after a pause can meet it. The block starts `offset`
    /// frames into a callback of `callbackFrames` frames that ended at host
    /// time `now`. Real-time safe.
    @inline(__always)
    func deliver(_ block: UnsafeMutablePointer<Float>, frames: Int, offset: Int = 0, callbackFrames: Int? = nil,
                 now: UInt64 = mach_absolute_time()) {
        gate.lastCaptureTicks.store(now, ordering: .releasing)
        guard gate.enter() else { return }
        // The block's first frame, as the fill stamps its own, so a run whose two
        // recordings start on different writers lines up.
        let start = gate.blockStartTicks(callbackNow: now, callbackFrames: callbackFrames ?? offset + frames,
                                         offset: offset)
        // Frames the fill already wrote silence for would put the ring ahead of time (S335).
        let skip = gate.filledFrames(from: start, frames: frames)
        if skip < frames {
            write(block + 2 * skip, frames: frames - skip, ticks: start &+ gate.ticks(frames: skip), filled: false)
        }
        gate.leave()
    }

    /// The session's fill thread (`QuietFill`): a block of silence
    /// written like a captured one, its first frame due at host time `ticks`.
    /// False when the capture holds the gate or has delivered since its block
    /// at `last`. Real-time safe.
    func fill(_ block: UnsafeMutablePointer<Float>, frames: Int, ticks: UInt64, last: UInt64) -> Bool {
        guard gate.enter() else { return false }
        guard gate.lastCaptureTicks.load(ordering: .acquiring) == last else {
            gate.leave()
            return false
        }
        write(block, frames: frames, ticks: ticks, filled: true)
        gate.filledTo.store(ticks &+ gate.ticks(frames: frames), ordering: .relaxed)
        gate.leave()
        return true
    }

    @inline(__always)
    private func write(_ block: UnsafeMutablePointer<Float>, frames: Int, ticks: UInt64, filled: Bool) {
        equalizer.process(block, frames: frames)
        if CalibrationTap.isActive {
            CalibrationTap.fadeMusic(block, frames: frames)
            let target = CalibrationTap.target
            if target != .local { ring.write(interleaved: block, frames: frames) }
            if target != .airPlay { NativeAirPlayTap.write(block, frames: frames) }
            if target != .cast { CastTap.write(block, frames: frames) }
            CalibrationTap.addSignal(block, frames: frames, at: ticks, filled: filled)
            switch target {
            case .local: ring.write(interleaved: block, frames: frames)
            case .airPlay: NativeAirPlayTap.write(block, frames: frames)
            case .cast: CastTap.write(block, frames: frames)
            }
            return
        }
        ring.write(interleaved: block, frames: frames)
        NativeAirPlayTap.write(block, frames: frames)
        CastTap.write(block, frames: frames)
    }

    /// A local output reading this session's ring, captured at `captureRate`
    /// Hz; nil for a rate the core doesn't take (S16). Its renderer is made by
    /// each start, for the device it starts on.
    func makeRenderer(captureRate: Double) -> OutputRenderer? {
        guard CoreRate.hz(captureRate) != nil else { return nil }
        return OutputRenderer(ring: ring)
    }
}

/// Keeps a session's two writers apart: the capture IOProc and the fill
/// that writes silence while the capture delivers nothing. The ring, the
/// equalizer and the senders' inputs take one writer at a time. Never
/// waits: a writer that finds it taken skips.
final class BlockGate {
    /// Host ticks of the capture's latest block; 0 before the first.
    let lastCaptureTicks = Atomic<UInt64>(0)
    /// Frames in the capture's latest callback; 0 before the first.
    let lastCallbackFrames = Atomic<Int>(0)
    /// Host ticks where the fill's latest block ends, until a captured block
    /// reaches past it; 0 otherwise. Used inside the gate only.
    let filledTo = Atomic<UInt64>(0)
    /// Host ticks per frame at the capture rate, as a `Double`'s bits; 0 until `prepare`.
    private let ticksPerFrame = Atomic<UInt64>(0)
    private let busy = Atomic<Bool>(false)

    /// Control thread, while capture is stopped.
    func prepare(captureRate: Double) {
        let perFrame = captureRate.isFinite && captureRate > 0
            ? Double(HostClock.shared.ticks(nanoseconds: 1_000_000_000)) / captureRate : 0
        ticksPerFrame.store(perFrame.bitPattern, ordering: .releasing)
    }

    /// How many host ticks `frames` frames last at the capture rate.
    @inline(__always)
    func ticks(frames: Int) -> UInt64 {
        let perFrame = Double(bitPattern: ticksPerFrame.load(ordering: .acquiring))
        return UInt64(max(0, Double(frames) * perFrame))
    }

    /// Host ticks of the first frame of the block `offset` frames into a
    /// callback of `callbackFrames` frames whose last frame ends at `callbackNow`.
    @inline(__always)
    func blockStartTicks(callbackNow: UInt64, callbackFrames: Int, offset: Int) -> UInt64 {
        callbackNow &- ticks(frames: callbackFrames - offset)
    }

    /// Inside the gate: how many of the first frames of a captured block of
    /// `frames` frames starting at host time `start` the fill already wrote.
    @inline(__always)
    func filledFrames(from start: UInt64, frames: Int) -> Int {
        let end = filledTo.load(ordering: .relaxed)
        guard end != 0 else { return 0 }
        let perFrame = Double(bitPattern: ticksPerFrame.load(ordering: .acquiring))
        let covered = end > start && perFrame > 0 ? min(Double(frames), (Double(end - start) / perFrame).rounded()) : 0
        if covered < Double(frames) { filledTo.store(0, ordering: .relaxed) }
        return Int(covered)
    }

    @inline(__always)
    func enter() -> Bool {
        busy.compareExchange(expected: false, desired: true, ordering: .acquiring).exchanged
    }

    @inline(__always)
    func leave() { busy.store(false, ordering: .releasing) }
}
