import Foundation
import Synchronization
import CAudioSyncCore

// The audio engine's parts on the Rust core (core/include/audiosync_core.h):
// each class owns one core handle. A session holds one set of them
// (EngineParts.swift). The capture IOProc and the render callbacks make only
// the calls the header marks real-time safe, which never allocate, lock, log
// or make a system call. The FIFO writer thread steps the core's writer, which
// reaches the pipe only through the POSIX callbacks at the end of this file.

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

// MARK: - FIFO feed

/// The pipe as the core's FIFO writer drives it, through the callbacks below:
/// the path, and the descriptor they open (writer thread only).
private struct CorePipe {
    var descriptor: Int32 = -1
    let path: UnsafeMutablePointer<CChar>
}

/// Opens the pipe write-only and non-blocking (S89): with no reader yet the
/// open fails with ENXIO instead of waiting for one.
private func corePipeOpen(_ context: UnsafeMutableRawPointer?) -> Int32 {
    guard let pipe = context?.assumingMemoryBound(to: CorePipe.self) else { return Int32(ASC_PIPE_FAILED) }
    let fd = Darwin.open(pipe.pointee.path, O_WRONLY | O_NONBLOCK)
    guard fd >= 0 else {
        switch errno {
        case ENXIO: return Int32(ASC_PIPE_NO_READER)
        case EINTR: return Int32(ASC_PIPE_INTERRUPTED)
        default: return Int32(ASC_PIPE_FAILED)
        }
    }
    // A vanished reader must surface as EPIPE, not kill the app (S92).
    _ = fcntl(fd, F_SETNOSIGPIPE, 1)
    pipe.pointee.descriptor = fd
    return Int32(ASC_PIPE_OK)
}

/// One non-blocking write of at most 512 bytes, ending on a frame boundary.
private func corePipeWrite(_ context: UnsafeMutableRawPointer?, _ bytes: UnsafePointer<UInt8>?,
                           _ length: Int, _ written: UnsafeMutablePointer<Int>?) -> Int32 {
    guard let pipe = context?.assumingMemoryBound(to: CorePipe.self), let bytes else {
        return Int32(ASC_PIPE_FAILED)
    }
    let n = Darwin.write(pipe.pointee.descriptor, bytes, length)
    guard n >= 0 else {
        switch errno {
        case EAGAIN: return Int32(ASC_PIPE_WOULD_BLOCK)
        case EINTR: return Int32(ASC_PIPE_INTERRUPTED)
        case EPIPE: return Int32(ASC_PIPE_BROKEN)
        default: return Int32(ASC_PIPE_FAILED)
        }
    }
    written?.pointee = n
    return Int32(ASC_PIPE_OK)
}

private func corePipeClose(_ context: UnsafeMutableRawPointer?) {
    guard let pipe = context?.assumingMemoryBound(to: CorePipe.self), pipe.pointee.descriptor >= 0 else { return }
    _ = Darwin.close(pipe.pointee.descriptor)
    pipe.pointee.descriptor = -1
}

/// One start…stop span of the writer thread.
private final class CoreFifoWriterRun {
    let stopping = Atomic<Bool>(false)
    /// Signalled by `stopWriter`, so a writer pausing for up to 50 ms stops at once.
    let wake = DispatchSemaphore(value: 0)
    /// Signalled by the writer thread once the pipe is closed and the writer gone.
    let finished = DispatchSemaphore(value: 0)
}

/// The core's FIFO feed into a named pipe (S82–S95, S160, S169), which only the
/// soak harness uses now (`EngineParts.fifo`). The core converts captured audio to 48 kHz PCM16 and queues it on
/// the capture thread (`feed`); a thread of this class steps the core's
/// writer, which keeps the backlog cap, writes frame-aligned pieces and
/// reopens after a broken pipe through the POSIX callbacks above (S88–S93).
final class CoreFifoFeed {
    /// What the writer did so far (S160), as the core counts it.
    struct Counters {
        /// Times the pipe was opened: the first open and every reopen.
        var opens: Int64 = 0
        /// Writes that failed while running because the reader went away
        /// (EPIPE); a reopen follows each. A stop isn't counted.
        var writeErrors: Int64 = 0
        /// Times the writer dropped queued audio to get back to the live edge
        /// (falling more than 250 ms behind a reader, or opening the pipe
        /// again), and the 16-bit samples dropped.
        var backlogSkips: Int64 = 0
        var backlogSkipSamples: Int64 = 0
        /// Writes that found the pipe full and waited for the reader.
        var pipeFullWaits: Int64 = 0
        /// Writes the pipe took only part of.
        var shortWrites: Int64 = 0
        /// Bytes that went into the pipe.
        var bytesWritten: Int64 = 0
    }

    private let handle: OpaquePointer
    private let pipePath: String
    // Control thread only.
    private var run: CoreFifoWriterRun?
    private var pipe: UnsafeMutablePointer<CorePipe>?

    init?(path: String, queueSeconds: UInt32 = UInt32(ASC_FIFO_DEFAULT_QUEUE_SECONDS)) {
        guard let feed = asc_fifo_create(queueSeconds) else { return nil }
        handle = feed
        pipePath = path
    }

    deinit {
        stopWriter()
        asc_fifo_destroy(handle)
    }

    /// At each session start, before the first `feed`: the capture rate.
    func setInputRate(_ rate: Double) {
        // A rate the core can't convert from is read as the pipe's own.
        _ = asc_fifo_set_input_rate(handle, CoreRate.hz(rate) ?? UInt32(ASC_PIPE_RATE))
    }

    /// Capture thread: converts and queues; never touches the pipe. Real-time safe.
    func feed(interleaved input: UnsafePointer<Float>, frames: Int) {
        _ = asc_fifo_feed(handle, input, frames)
    }

    /// 16-bit samples queued so far (frames × 2).
    var samplesQueued: Int64 { Int64(clamping: asc_fifo_counters(handle).samples_queued) }

    var counters: Counters {
        let c = asc_fifo_counters(handle)
        return Counters(
            opens: Int64(clamping: c.opens), writeErrors: Int64(clamping: c.write_errors),
            backlogSkips: Int64(clamping: c.backlog_skips),
            backlogSkipSamples: Int64(clamping: c.backlog_skip_samples),
            pipeFullWaits: Int64(clamping: c.pipe_full_waits), shortWrites: Int64(clamping: c.short_writes),
            bytesWritten: Int64(clamping: c.bytes_written))
    }

    /// Starts the writer thread with an empty queue; a no-op while started.
    func startWriter() {
        guard run == nil, let writer = asc_fifo_writer_create(handle) else { return }
        guard let cPath = strdup(pipePath) else {
            asc_fifo_writer_destroy(writer)
            return
        }
        let pipe = UnsafeMutablePointer<CorePipe>.allocate(capacity: 1)
        pipe.initialize(to: CorePipe(path: cPath))
        let run = CoreFifoWriterRun()
        // Let the kernel semaphore behind `wake` be made here, not on the writer thread.
        _ = run.wake.wait(timeout: .now())
        self.run = run
        self.pipe = pipe
        let thread = Thread {
            var sink = AscPipeSink(context: UnsafeMutableRawPointer(pipe),
                                   open: corePipeOpen, write: corePipeWrite, close: corePipeClose)
            while !run.stopping.load(ordering: .acquiring) {
                let pause = asc_fifo_writer_step(writer, &sink)
                if pause > 0 { _ = run.wake.wait(timeout: .now() + .microseconds(Int(pause))) }
            }
            // Completes a frame the reader got only part of, then closes (S93).
            var pauseUs: UInt32 = 0
            while asc_fifo_writer_close_step(writer, &sink, &pauseUs) {
                if pauseUs > 0 { usleep(pauseUs) }
            }
            asc_fifo_writer_destroy(writer)
            run.finished.signal()
        }
        thread.name = "io.github.yassersutojoyo.audiosync.fifo"
        thread.stackSize = 1 << 20
        thread.start()
    }

    /// Stops the writer thread and returns once it closed the pipe and exited,
    /// so a start right after never meets a second writer (S88).
    func stopWriter() {
        guard let run, let pipe else { return }
        run.stopping.store(true, ordering: .releasing)
        run.wake.signal()
        run.finished.wait()
        free(pipe.pointee.path)
        pipe.deinitialize(count: 1)
        pipe.deallocate()
        self.run = nil
        self.pipe = nil
    }
}
