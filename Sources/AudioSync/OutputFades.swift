import Foundation

// How a session's stops and switch-offs wait for the local outputs' fades
// (S185, S186). Clause numbers (S…) refer to docs/ENGINE_SPEC.md.
//
// Scaffold: this answers as the Mac code behaved before the fades, so the
// fades' logic checks fail against it. A stop stops capture first and every
// output at once, a switch-off stops its outputs at once, nothing is asked to
// fade out and nothing waits.

/// The host clock in nanoseconds, the time base of the output times a
/// renderer publishes (S185).
enum HostClock {
    /// The host clock now.
    static func now() -> UInt64 { DispatchTime.now().uptimeNanoseconds }
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
    /// The longest a stop or a switch-off waits for its outputs' fades.
    static let capNs: UInt64 = 500_000_000
    static var capSeconds: Double { Double(capNs) / 1_000_000_000 }

    private let clock: () -> UInt64
    private let pause: (UInt64) -> Void

    /// `clock` reads the host clock in nanoseconds; `pause` sleeps for the
    /// nanoseconds given (the bounded wait of `finishAll`).
    init(clock: @escaping () -> UInt64 = HostClock.now,
         pause: @escaping (UInt64) -> Void = { usleep(UInt32(clamping: $0 / 1000)) }) {
        self.clock = clock
        self.pause = pause
    }

    /// Nothing is waiting for its fade.
    var isIdle: Bool { true }

    /// A session's stop is under way.
    var stoppingSession: Bool { false }

    /// Switches `outputs` off.
    func remove(_ outputs: [FadingOutput], then completion: (() -> Void)? = nil) {
        for output in outputs { output.stop() }
        completion?()
    }

    /// Ends a session: capture, then every output, then the pipe writer.
    func stopSession(_ outputs: [FadingOutput], fadeInputs: () -> Void, stopCapture: @escaping () -> Void,
                     stopWriter: @escaping () -> Void, then completion: (() -> Void)? = nil) {
        stopCapture()
        for output in outputs { output.stop() }
        stopWriter()
        completion?()
    }

    /// Runs `completion` once the session stop under way has finished, or now.
    func afterSessionStop(_ completion: @escaping () -> Void) {
        completion()
    }

    /// An output whose device is gone: stopped at once.
    func stopLost(_ output: FadingOutput) {
        output.stop()
    }

    /// Finishes what has faded or waited long enough.
    func poll() {}

    /// Waits until everything here has finished.
    func finishAll() {}
}
