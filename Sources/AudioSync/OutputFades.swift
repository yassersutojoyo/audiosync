import Foundation
import CAudioSyncCore

// How a session's stops and switch-offs wait for the local outputs' fades
// (S185, S186). Clause numbers (S…) refer to docs/ENGINE_SPEC.md.
//
// An output being stopped fades out first, all of them at once: each keeps
// its device's IO running and renders exact zeros after its 20 ms fade-out,
// and is stopped only once its device has played that fade (the host clock
// has passed the fade's last frame's output time plus the device's and
// stream's latency) or 500 ms after the request, whichever comes first. A
// session's stop waits the same way with its capture still running, so an
// output at a short hold-back fades out on audio that is still being
// captured; then capture stops, every output stops, then the pipe writer.
// Nothing here waits on the main thread unless asked to (`finishAll`): the
// session polls `poll` on a short timer.

/// The host clock in nanoseconds: `mach_absolute_time` through the timebase,
/// the time base of the output times a renderer publishes (S185).
struct HostClock {
    private let numer: UInt64
    private let denom: UInt64

    /// Reads the timebase; call it off the audio threads and keep the value.
    init() {
        var info = mach_timebase_info_data_t()
        mach_timebase_info(&info)
        numer = UInt64(max(info.numer, 1))
        denom = UInt64(max(info.denom, 1))
    }

    /// Host ticks (`mach_absolute_time`, an `AudioTimeStamp`'s `mHostTime`)
    /// in nanoseconds. Plain arithmetic, safe on an audio thread.
    @inline(__always)
    func nanoseconds(ticks: UInt64) -> UInt64 {
        (ticks / denom) &* numer &+ (ticks % denom) &* numer / denom
    }

    /// The host clock now. Safe on an audio thread.
    @inline(__always)
    func nowNs() -> UInt64 { nanoseconds(ticks: mach_absolute_time()) }

    /// The timebase read once, for the control thread; an audio thread
    /// gets its own copy before it starts.
    static let shared = HostClock()

    /// The host clock now, for the control thread.
    static func now() -> UInt64 { shared.nowNs() }
}

/// A local output as a stop sees it (S185).
protocol FadingOutput: AnyObject {
    /// Asks the output to fade out from its next callback; it renders exact
    /// zeros after the fade until it stops.
    func requestFadeOut()
    /// Whether its device has played the fade-out by host time `nowNs`, or
    /// has nothing left to play (its device is gone or its IO stopped).
    func hasFaded(nowNs: UInt64) -> Bool
    /// Stops its IO at once.
    func stop()
}

/// Local outputs on their way out, and the session stops that wait for them
/// (S185, S186). Main thread only.
final class OutputRetirement {
    /// The longest a stop or a switch-off waits for its outputs' fades, from
    /// the request: the core's `ASC_STOP_FADE_CAP_MS` (S186).
    static let capNs = UInt64(ASC_STOP_FADE_CAP_MS) * 1_000_000
    static var capSeconds: Double { Double(capNs) / 1_000_000_000 }

    /// Outputs asked to fade out together, and what follows once they have.
    private final class Batch {
        var outputs: [FadingOutput]
        let askedNs: UInt64
        /// A session's stop: runs once the outputs faded, before they stop.
        let stopCapture: (() -> Void)?
        /// Runs after the outputs stopped (a session's pipe writer, the caller's completion).
        var after: [() -> Void]
        /// A session's stop: whether capture has given the AirPlay inputs
        /// their fade-out's frames, and how long to wait for that when it
        /// gives nothing.
        let inputsFaded: () -> Bool
        let inputsFallbackNs: UInt64

        init(outputs: [FadingOutput], askedNs: UInt64, stopCapture: (() -> Void)?, after: [() -> Void],
             inputsFaded: @escaping () -> Bool = { true }, inputsFallbackNs: UInt64 = 0) {
            self.outputs = outputs
            self.askedNs = askedNs
            self.stopCapture = stopCapture
            self.after = after
            self.inputsFaded = inputsFaded
            self.inputsFallbackNs = inputsFallbackNs
        }

        var isSession: Bool { stopCapture != nil }
    }

    private let clock: () -> UInt64
    private let pause: (UInt64) -> Void
    /// In the order they were asked for.
    private var batches: [Batch] = []

    /// `clock` reads the host clock in nanoseconds; `pause` sleeps for the
    /// nanoseconds given (the bounded wait of `finishAll`).
    init(clock: @escaping () -> UInt64 = HostClock.now,
         pause: @escaping (UInt64) -> Void = { _ = usleep(UInt32(clamping: $0 / 1000)) }) {
        self.clock = clock
        self.pause = pause
    }

    /// Nothing is waiting for its fade.
    var isIdle: Bool { batches.isEmpty }

    /// A session's stop is under way.
    var stoppingSession: Bool { batches.contains { $0.isSession } }

    /// Switches `outputs` off (S185, S186): each is asked to fade out now,
    /// all at once, and they are stopped together once every one has faded
    /// or the cap passed; then `completion` runs.
    func remove(_ outputs: [FadingOutput], then completion: (() -> Void)? = nil) {
        for output in outputs { output.requestFadeOut() }
        batches.append(Batch(outputs: outputs, askedNs: clock(), stopCapture: nil,
                             after: completion.map { [$0] } ?? []))
        poll()
    }

    /// Ends a session (S186): `fadeInputs` asks the AirPlay inputs to fade
    /// out and every one of `outputs` is asked to fade out, all at once.
    /// Once they have faded (or the cap passed), and the switch-offs asked
    /// for before have finished, `stopCapture` runs, then every output
    /// stops, then `stopWriter`, then `completion`. Capture also keeps
    /// running until the AirPlay inputs have had their fade-out's frames
    /// (S189), also with no local output on: until `inputsFaded` says so,
    /// or for `inputsFallbackNs` while capture gives nothing, never past the
    /// cap.
    func stopSession(_ outputs: [FadingOutput], fadeInputs: () -> Void,
                     inputsFaded: @escaping () -> Bool = { true }, inputsFallbackNs: UInt64 = 0,
                     stopCapture: @escaping () -> Void, stopWriter: @escaping () -> Void,
                     then completion: (() -> Void)? = nil) {
        fadeInputs()
        for output in outputs { output.requestFadeOut() }
        var after: [() -> Void] = [stopWriter]
        if let completion { after.append(completion) }
        batches.append(Batch(outputs: outputs, askedNs: clock(), stopCapture: stopCapture, after: after,
                             inputsFaded: inputsFaded, inputsFallbackNs: inputsFallbackNs))
        poll()
    }

    /// Runs `completion` once the session stop under way has finished, or
    /// now when none is under way.
    func afterSessionStop(_ completion: @escaping () -> Void) {
        guard let batch = batches.last(where: { $0.isSession }) else {
            completion()
            return
        }
        batch.after.append(completion)
    }

    /// An output whose device is gone, or whose IO stopped by itself (S68):
    /// nothing can play its fade, so it is stopped at once, also when it is
    /// waiting here for one (S185).
    func stopLost(_ output: FadingOutput) {
        for batch in batches { batch.outputs.removeAll { $0 === output } }
        output.stop()
        poll()
    }

    /// Finishes, in the order they were asked for, the switch-offs and stops
    /// whose outputs have all faded or whose cap passed. A session's stop
    /// also waits for the switch-offs asked for before it, whose outputs
    /// would starve once capture stops. Call it often while not idle.
    func poll() {
        while let index = finishable() {
            let batch = batches.remove(at: index)
            batch.stopCapture?()
            for output in batch.outputs { output.stop() }
            for step in batch.after { step() }
        }
    }

    /// Waits on this thread until everything here has finished: each batch
    /// at most the cap after it was asked for (S186). A start calls it, so a
    /// new session starts only once the last one's outputs have faded and its
    /// capture has stopped; so does quitting.
    func finishAll() {
        poll()
        while !batches.isEmpty {
            pause(1_000_000)
            poll()
        }
    }

    /// The first batch that can finish now, if any.
    private func finishable() -> Int? {
        let now = clock()
        for (index, batch) in batches.enumerated() {
            let waited = now &- batch.askedNs
            let capped = waited >= Self.capNs
            // The inputs' check goes first: it notes capture's progress.
            let inputs = batch.inputsFaded() || waited >= batch.inputsFallbackNs
            let faded = inputs && batch.outputs.allSatisfy { $0.hasFaded(nowNs: now) }
            guard capped || faded else { continue }
            // A session's stop waits for every batch asked for before it.
            if batch.isSession && index > 0 { continue }
            return index
        }
        return nil
    }
}
