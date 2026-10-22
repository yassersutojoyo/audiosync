import Foundation

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

    /// 24 s at 192 kHz, the highest capture rate the ring is sized for (S36):
    /// the 20 s maximum delay plus headroom, whatever the session's rate.
    static let ringCapacityFrames = 192_000 * 24

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
        NativeAirPlayTap.prepare(captureRate: captureRate)
        CastTap.prepare(captureRate: captureRate)
    }

    /// Capture thread: one captured block, equalized once in place, then into
    /// the ring and AudioSync's own AirPlay and Cast senders when they
    /// run (`NativeAirPlayTap`, `CastTap`), the same samples for all (S7). During a
    /// microphone calibration (`CalibrationTap`) the music fades to silence
    /// on every path and the test signal goes into the one path it targets.
    /// Real-time safe.
    @inline(__always)
    func deliver(_ block: UnsafeMutablePointer<Float>, frames: Int) {
        equalizer.process(block, frames: frames)
        if CalibrationTap.isActive {
            CalibrationTap.fadeMusic(block, frames: frames)
            let target = CalibrationTap.target
            if target != .local { ring.write(interleaved: block, frames: frames) }
            if target != .airPlay { NativeAirPlayTap.write(block, frames: frames) }
            if target != .cast { CastTap.write(block, frames: frames) }
            CalibrationTap.addSignal(block, frames: frames)
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
