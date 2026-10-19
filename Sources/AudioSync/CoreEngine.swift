import Foundation
import Synchronization
import CAudioSyncCore

// The audio engine's parts on the Rust core (core/include/audiosync_core.h):
// each class owns one core handle. A session holds one set of them
// (EngineParts.swift). The capture IOProc and the render callbacks make only
// the calls the header marks real-time safe, which never allocate, lock, log
// or make a system call.

/// Capture rates the core accepts, as whole Hz.
enum CoreRate {
    /// `rate` rounded to whole Hz when it lies in 8 000…384 000 Hz, else nil.
    static func hz(_ rate: Double) -> UInt32? {
        guard rate.isFinite else { return nil }
        let r = rate.rounded()
        guard r >= 8000, r <= 384_000 else { return nil }
        return UInt32(r)
    }
}

/// The core's delay ring and its single writer (S35–S40).
final class CoreDelayRing {
    /// The `AscRing`; readers are made from it.
    let handle: OpaquePointer
    private let writer: OpaquePointer

    init?(capacityFrames: Int) {
        guard capacityFrames > 0, let ring = asc_ring_create(capacityFrames) else { return nil }
        guard let writer = asc_ring_writer_create(ring) else {
            asc_ring_destroy(ring)
            return nil
        }
        handle = ring
        self.writer = writer
    }

    deinit {
        // The core counts references, so readers still alive keep the frames.
        asc_ring_writer_destroy(writer)
        asc_ring_destroy(handle)
    }

    var capacityFrames: Int { asc_ring_capacity(handle) }
    /// Frames written since the ring was made (any thread).
    var framesWritten: Int64 { Int64(clamping: asc_ring_frames_written(handle)) }

    /// Capture thread: appends `frames` interleaved stereo frames. Real-time safe.
    func write(interleaved samples: UnsafePointer<Float>, frames: Int) {
        asc_ring_write(writer, samples, frames)
    }
}

/// One local output's delayed read of the core's ring, with drift
/// correction (S41–S59).
final class CoreDelayReader {
    /// What the reader did so far (S158), as the core counts it. Reading the
    /// counters never blocks the reader or changes what it plays.
    struct Counters {
        /// Reads that anchored the delay window from scratch.
        var primes: Int64 = 0
        /// Delay changes that a read applied.
        var resyncs: Int64 = 0
        /// Corrections that cut a lag grown too long by skipping ahead, and
        /// the frames skipped.
        var driftSkips: Int64 = 0
        var driftSkipFrames: Int64 = 0
        /// Corrections that stretched a lag grown too short with silence, and
        /// the frames of silence they owed.
        var driftSilences: Int64 = 0
        var driftSilenceFrames: Int64 = 0
        /// Reads that reached the newest written frame and filled the rest of
        /// the block with silence (underruns), and the frames filled.
        var starvedRenders: Int64 = 0
        var starvedFrames: Int64 = 0
    }

    private let handle: OpaquePointer
    /// Keeps the ring's handle valid for `lagFrames` while this reader lives.
    private let sourceRing: CoreDelayRing
    private let rate: UInt32
    /// The next frame to read after the latest read (output thread writes it).
    private let nextFrame = Atomic<Int64>(0)

    /// Nil outside 8 000…384 000 Hz. The drift tolerance starts at 35 ms at
    /// `captureRate` and the delay at 0 until a target is set.
    init?(ring: CoreDelayRing, captureRate: Double) {
        guard let rate = CoreRate.hz(captureRate), let reader = asc_reader_create(ring.handle, rate)
        else { return nil }
        handle = reader
        sourceRing = ring
        self.rate = rate
    }

    deinit { asc_reader_destroy(handle) }

    /// Output thread: a read at the target `setDelay` stored. The core takes a
    /// pending re-anchor before it loads the target, so a change never applies
    /// a stale delay. Real-time safe.
    func readAtTarget(into dst: UnsafeMutablePointer<Float>, frames: Int) {
        var report = AscReadReport()
        asc_reader_read(handle, dst, frames, &report)
        publish(report)
    }

    @inline(__always)
    private func publish(_ report: AscReadReport) {
        // The report's lag is the newest write when the read started minus the
        // next frame to read; an empty read reports nothing.
        if report.frames > 0 { nextFrame.store(report.written - report.lag, ordering: .releasing) }
    }

    /// Any thread: the target delay, converted at the capture rate as the core
    /// converts it (S3); the next read re-anchors there. A non-finite delay is
    /// ignored (false).
    @discardableResult
    func setDelay(ms: Double) -> Bool {
        var frames: Int64 = 0
        let converted = asc_delay_ms_to_frames(ms, rate, &frames)
        if converted { asc_reader_set_delay_frames(handle, frames) }
        return converted
    }

    /// While the output is stopped: the next read starts over like a new
    /// reader's first; the counters are kept (S159).
    func resetPrime() { asc_reader_reset_prime(handle) }

    /// Any thread: the target delay in frames, as `setDelay` converted it.
    var targetDelayFrames: Int64 { asc_reader_delay_frames(handle) }

    /// How far the reader trails the newest write now (S56): the frames
    /// written so far minus the next frame this reader plays. The core's own
    /// `asc_reader_lag_frames` stops at the latest read's start; this one
    /// keeps counting while the writer runs ahead between reads.
    var lagFrames: Int64 { sourceRing.framesWritten - nextFrame.load(ordering: .acquiring) }

    var counters: Counters {
        let c = asc_reader_counters(handle)
        return Counters(
            primes: Int64(clamping: c.primes), resyncs: Int64(clamping: c.resyncs),
            driftSkips: Int64(clamping: c.drift_skips), driftSkipFrames: Int64(clamping: c.drift_skip_frames),
            driftSilences: Int64(clamping: c.drift_silences),
            driftSilenceFrames: Int64(clamping: c.drift_silence_frames),
            starvedRenders: Int64(clamping: c.starved_renders), starvedFrames: Int64(clamping: c.starved_frames))
    }
}

/// The core's 10-band equalizer, applied once to the captured stream (S22–S34).
final class CoreEqualizer {
    private let handle: OpaquePointer

    /// Disabled and flat, designed for `sampleRate` until told otherwise.
    init?(sampleRate: Double = 48000) {
        guard let eq = asc_eq_create(sampleRate) else { return nil }
        handle = eq
    }

    deinit { asc_eq_destroy(handle) }

    /// Control thread: new settings, taking effect at the next block.
    func update(_ new: Equalizer.Settings) {
        var gains = [Double](repeating: 0, count: Int(ASC_EQ_BANDS))
        for band in 0..<min(gains.count, new.gains.count) { gains[band] = new.gains[band] }
        _ = asc_eq_set_settings(handle, new.enabled, new.preamp, &gains)
    }

    /// Control thread, while capture is stopped: the design rate.
    func setSampleRate(_ rate: Double) {
        _ = asc_eq_set_sample_rate(handle, rate)
    }

    /// Capture thread: equalizes interleaved stereo in place. Real-time safe.
    func process(_ buffer: UnsafeMutablePointer<Float>, frames: Int) {
        asc_eq_process(handle, buffer, frames)
    }
}

/// A local output's gain stage (S64, S65): unity until set, a one-buffer
/// ramp after a change, NaN and infinity zeroed.
final class CoreGainStage {
    private let handle: OpaquePointer

    init?() {
        guard let stage = asc_gain_stage_create(1) else { return nil }
        handle = stage
    }

    deinit { asc_gain_stage_destroy(handle) }

    /// Any thread: the linear gain, clamped to 0…1.5.
    func setGain(_ gain: Double) {
        asc_gain_stage_set_gain(handle, Float(max(0, min(1.5, gain))))
    }

    /// Output thread: applies the gain to interleaved stereo in place. Real-time safe.
    func apply(_ buffer: UnsafeMutablePointer<Float>, frames: Int) {
        asc_gain_stage_apply(handle, buffer, frames, 2)
    }

    /// Output thread: lays `frames` stereo frames out on a device buffer of
    /// `channels` interleaved channels (L and R on the first two, silence on
    /// the rest, their mean on a one-channel device, S63), then applies the
    /// gain as `apply` does. Returns the frames written. Real-time safe.
    @discardableResult
    func render(_ stereo: UnsafePointer<Float>, frames: Int,
                into device: UnsafeMutablePointer<Float>, channels: Int) -> Int {
        asc_gain_stage_render(handle, stereo, frames, device, channels)
    }
}

/// The core's volume curve: slider positions (0…100) and linear gain (S70, S71).
struct CoreGainCurve {
    func gain(position: Double) -> Double { asc_volume_gain(position) }
    func position(gain: Double) -> Double { asc_volume_position(gain) }
    /// A local output's gain: its own position times the master's, both through the curve.
    func outputGain(localPosition: Double, masterPosition: Double) -> Double {
        asc_output_gain(localPosition, masterPosition)
    }
}
