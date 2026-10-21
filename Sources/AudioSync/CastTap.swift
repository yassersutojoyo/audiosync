import CAudioSyncCore
import Darwin
import Synchronization

/// The capture's way into the Google Cast sender: its input handle
/// (`asc_cast_input_*`), installed while the sender runs, fed exactly like
/// `NativeAirPlayTap`. The capture thread hands it every equalized block
/// (`write`, from `EngineParts.deliver`) and each capture build gives it the
/// capture rate (`prepare`, from `EngineParts.prepare`); the input resamples
/// to 48 kHz itself and drops what it gets while no session runs. With
/// nothing installed a write is two atomic counts and a load.
///
/// Lifetime: the owner installs the input it took from its sender and, to
/// free it, uninstalls it first; `uninstall` returns once no call can still
/// be using it, so the input is never freed under the capture thread.
enum CastTap {
    /// The installed input (an `AscCastInput`), or nil.
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
            _ = asc_cast_input_write(OpaquePointer(raw), block, frames)
        }
        _ = users.wrappingSubtract(1, ordering: .sequentiallyConsistent)
    }

    /// Control thread, at each capture build before its first block: the
    /// capture rate. The input fades in from silence at the next write.
    static func prepare(captureRate: Double) {
        rateBits.store(captureRate.bitPattern, ordering: .sequentiallyConsistent)
        _ = users.wrappingAdd(1, ordering: .sequentiallyConsistent)
        if let raw = slot.load(ordering: .sequentiallyConsistent) {
            _ = asc_cast_input_set_rate(OpaquePointer(raw), captureRate)
        }
        _ = users.wrappingSubtract(1, ordering: .sequentiallyConsistent)
    }

    /// At a session stop: the input fades out over the next 20 ms of blocks
    /// and hands zeros after them, until the next `prepare`.
    static func requestFadeOut() {
        _ = users.wrappingAdd(1, ordering: .sequentiallyConsistent)
        if let raw = slot.load(ordering: .sequentiallyConsistent) {
            asc_cast_input_request_fade_out(OpaquePointer(raw))
        }
        _ = users.wrappingSubtract(1, ordering: .sequentiallyConsistent)
    }

    /// Installs `input`, at the capture rate already known if a capture was
    /// built before.
    static func install(_ input: OpaquePointer) {
        slot.store(UnsafeMutableRawPointer(input), ordering: .sequentiallyConsistent)
        let rate = Double(bitPattern: rateBits.load(ordering: .sequentiallyConsistent))
        if rate > 0 { _ = asc_cast_input_set_rate(input, rate) }
    }

    /// Takes `input` out of the capture path if it is still installed, and
    /// returns once no call can still be using it. Off the capture thread;
    /// it may wait for one block.
    static func uninstall(_ input: OpaquePointer) {
        _ = slot.compareExchange(expected: UnsafeMutableRawPointer(input), desired: nil,
                                 ordering: .sequentiallyConsistent)
        while users.load(ordering: .sequentiallyConsistent) != 0 { usleep(50) }
    }
}
