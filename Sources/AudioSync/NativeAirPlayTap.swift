import CAudioSyncCore
import Darwin
import Synchronization

/// The capture's way into AudioSync's own AirPlay sender: the sender's input
/// handle (`asc_airplay_input_*`), installed while the native sender runs.
/// The capture thread hands it every equalized block (`write`, called from
/// `EngineParts.deliver`) and each capture build gives it the capture rate
/// (`prepare`, from `EngineParts.prepare`); the input resamples to 44.1 kHz
/// itself. With nothing installed (the bundled engine is the AirPlay sender)
/// a write is two atomic counts and a load.
///
/// A capture rewrite keeps this contract: `prepare(captureRate:)` at each
/// capture (re)build, before its first block, and `write(_:frames:)` with
/// each block after the equalizer, on the capture thread, alongside the ring
/// and the pipe feed.
///
/// Lifetime: the owner installs an input it took from its sender and, to
/// free it, uninstalls it first; `uninstall` returns once no write or rate
/// call can still be using it, so the input is never freed under the
/// capture thread.
enum NativeAirPlayTap {
    /// The installed input (an `AscAirPlayInput`), or nil.
    private static let slot = Atomic<UnsafeMutableRawPointer?>(nil)
    /// Calls that may be using the input they loaded from `slot`.
    private static let users = Atomic<Int>(0)
    /// The capture rate the last `prepare` gave, as `Double` bits; 0 before one.
    private static let rateBits = Atomic<UInt64>(0)

    /// Capture thread: one block of `frames` interleaved stereo frames, after
    /// the equalizer. Real-time safe: atomics, then the input's write, which
    /// never allocates, locks, blocks or logs.
    @inline(__always)
    static func write(_ block: UnsafePointer<Float>, frames: Int) {
        _ = users.wrappingAdd(1, ordering: .sequentiallyConsistent)
        if let raw = slot.load(ordering: .sequentiallyConsistent) {
            _ = asc_airplay_input_write(OpaquePointer(raw), block, frames)
        }
        _ = users.wrappingSubtract(1, ordering: .sequentiallyConsistent)
    }

    /// Control thread, at each capture build before its first block: the
    /// capture rate. The input starts its resampler afresh at the next write.
    static func prepare(captureRate: Double) {
        rateBits.store(captureRate.bitPattern, ordering: .sequentiallyConsistent)
        _ = users.wrappingAdd(1, ordering: .sequentiallyConsistent)
        if let raw = slot.load(ordering: .sequentiallyConsistent) {
            _ = asc_airplay_input_set_rate(OpaquePointer(raw), captureRate)
        }
        _ = users.wrappingSubtract(1, ordering: .sequentiallyConsistent)
    }

    /// Installs `input`, at the capture rate already known if a capture was
    /// built before. Published first, so a `prepare` running meanwhile either
    /// finds it or its rate is read here.
    static func install(_ input: OpaquePointer) {
        slot.store(UnsafeMutableRawPointer(input), ordering: .sequentiallyConsistent)
        let rate = Double(bitPattern: rateBits.load(ordering: .sequentiallyConsistent))
        if rate > 0 { _ = asc_airplay_input_set_rate(input, rate) }
    }

    /// Takes the input out of the capture path and returns it once no call
    /// can still be using it, for its owner to destroy; nil when none was
    /// installed. Off the capture thread; it may wait for one block.
    static func uninstall() -> OpaquePointer? {
        guard let raw = slot.exchange(nil, ordering: .sequentiallyConsistent) else { return nil }
        while users.load(ordering: .sequentiallyConsistent) != 0 { usleep(50) }
        return OpaquePointer(raw)
    }
}
