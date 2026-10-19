import Foundation

// One session's audio parts, all on the Rust core (CoreEngine.swift): the
// equalizer, the delay ring and the pipe feed. The capture IOProc (`deliver`)
// and the render callbacks call these final classes directly, so no audio
// callback dispatches through a protocol. Clause numbers (S…) refer to
// docs/ENGINE_SPEC.md.

/// Whether an AirPlay speaker is chosen (S79), which adds the AirPlay delay
/// on top of every local output's hold-back (S176, `LocalDelay`). The core's
/// rule (`CoreSyncRule`) serves the bundled AirPlay engine; AudioSync's own
/// sender has its own (`NativeSyncRule`).
protocol SyncRule {
    /// Some AirPlay speaker the user enabled is in the engine's latest list.
    func airPlayChosen(enabled: Set<String>, listed: [String]) -> Bool
}

/// The audio parts of one session, made when it starts and dropped when it
/// stops, so a new session never hears the previous one's history.
struct EngineParts {
    let equalizer: CoreEqualizer
    let ring: CoreDelayRing
    /// The bundled AirPlay engine's pipe feed; nil when no pipe is fed.
    let fifo: CoreFifoFeed?

    /// 24 s at 192 kHz, the highest capture rate the ring is sized for (S36):
    /// the 20 s maximum delay plus headroom, whatever the session's rate.
    static let ringCapacityFrames = 192_000 * 24

    /// New parts with a pipe feed for `fifoPath` (none when nil) and the
    /// equalizer set to `settings`. Nil when the core can't make one of them.
    static func make(fifoPath: String?, equalizer settings: Equalizer.Settings = Equalizer.Settings()) -> EngineParts? {
        guard let equalizer = CoreEqualizer(), let ring = CoreDelayRing(capacityFrames: ringCapacityFrames)
        else { return nil }
        var fifo: CoreFifoFeed?
        if let fifoPath {
            guard let feed = CoreFifoFeed(path: fifoPath) else { return nil }
            fifo = feed
        }
        equalizer.update(settings)
        return EngineParts(equalizer: equalizer, ring: ring, fifo: fifo)
    }

    /// Control thread, while capture is stopped (each tap build): the
    /// capture rate the equalizer, the pipe feed and AudioSync's own AirPlay
    /// sender (when it runs) convert from.
    func prepare(captureRate: Double) {
        fifo?.setInputRate(captureRate)
        equalizer.setSampleRate(captureRate)
        NativeAirPlayTap.prepare(captureRate: captureRate)
    }

    /// Capture thread: one captured block, equalized once in place, then into
    /// the ring, the pipe feed (S7) and AudioSync's own AirPlay sender when it
    /// runs (`NativeAirPlayTap`). Real-time safe.
    @inline(__always)
    func deliver(_ block: UnsafeMutablePointer<Float>, frames: Int) {
        equalizer.process(block, frames: frames)
        ring.write(interleaved: block, frames: frames)
        fifo?.feed(interleaved: block, frames: frames)
        NativeAirPlayTap.write(block, frames: frames)
    }

    /// A local output reading this session's ring, captured at `captureRate`
    /// Hz; nil when the core can't make its reader or gain stage.
    func makeRenderer(captureRate: Double) -> OutputRenderer? {
        guard let reader = CoreDelayReader(ring: ring, captureRate: captureRate),
              let gain = CoreGainStage() else { return nil }
        return OutputRenderer(reader: reader, gain: gain)
    }
}
