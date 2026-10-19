// Calls audiosync-core through its C header, the way the macOS app will: the
// version, the volume curve, the presets, one equalizer pass, a ring with a
// delay reader, the FIFO feed with its writer stepped through a C pipe sink,
// the AirPlay sync rule, the local delay model and the AirPlay sender's
// handle, audio always a whole
// buffer at a time. Prints one line per check; exits 1 on a failure. Touches
// no audio device, pipe or network; nothing plays.

import AudioSyncCore
import Foundation

var failures: Int32 = 0

func check(_ ok: Bool, _ what: String) {
    print("\(ok ? "ok  " : "FAIL") \(what)")
    if !ok { failures += 1 }
}

func versionVolumeAndEqualizer() {
    let version = String(cString: asc_version())
    check(!version.isEmpty, "core version \(version)")
    check(asc_abi_version() == UInt32(ASC_ABI_VERSION), "interface version \(asc_abi_version())")
    check(asc_volume_gain(50) == 0.125, "volume 50 gives gain 0.125 (about -18 dB)")
    check(abs(asc_volume_position(0.001) - 10) < 1e-9, "gain 0.001 is volume 10 (-60 dB)")
    check(asc_output_gain(50, 50) == 0.125 * 0.125, "an output's gain is its own times the master's")
    check(asc_eq_band_count() == Int(ASC_EQ_BANDS), "\(asc_eq_band_count()) equalizer bands")

    var presetIDs: [String] = []
    for index in 0..<asc_eq_preset_count() {
        presetIDs.append(String(cString: asc_eq_preset_id(index)))
    }
    check(presetIDs.first == "flat" && presetIDs.count == Int(ASC_EQ_PRESETS), "presets: \(presetIDs.joined(separator: ", "))")

    let rate = 48_000.0
    let frames = 512
    guard let eq = asc_eq_create(rate) else {
        check(false, "asc_eq_create")
        return
    }
    defer { asc_eq_destroy(eq) }
    var gains = [Double](repeating: 0, count: asc_eq_band_count())
    gains[5] = 6  // the 1 kHz band
    check(asc_eq_set_settings(eq, true, 0, gains), "settings accepted")

    // One second of a 1 kHz sine at amplitude 0.25 in 512-frame stereo blocks;
    // RMS over the settled second half.
    var block = [Float](repeating: 0, count: frames * 2)
    var energyIn = 0.0
    var energyOut = 0.0
    let blocks = Int(rate) / frames
    for blockIndex in 0..<blocks {
        for i in 0..<frames {
            let t = Double(blockIndex * frames + i) / rate
            let x = Float(0.25 * sin(2 * Double.pi * 1000 * t))
            block[2 * i] = x
            block[2 * i + 1] = x
        }
        let input = block
        block.withUnsafeMutableBufferPointer { buffer in
            asc_eq_process(eq, buffer.baseAddress, frames)
        }
        if blockIndex >= blocks / 2 {
            for i in 0..<frames {
                energyIn += Double(input[2 * i] * input[2 * i])
                energyOut += Double(block[2 * i] * block[2 * i])
            }
        }
    }
    let gainDB = 10 * log10(energyOut / energyIn)
    check(abs(gainDB - 6) < 0.2, String(format: "+6 dB at 1 kHz measures %.2f dB", gainDB))

    block[0] = .nan
    block.withUnsafeMutableBufferPointer { buffer in
        asc_eq_process(eq, buffer.baseAddress, frames)
    }
    check(block.allSatisfy { $0.isFinite } && asc_eq_nan_resets(eq) == 1, "a NaN sample is reset to 0")

    block.withUnsafeMutableBufferPointer { buffer in
        asc_apply_gain(buffer.baseAddress, frames, 0)
    }
    check(block.allSatisfy { $0 == 0 }, "gain 0 gives exact zeros")
}

/// A ring at 48 kHz, its writer and one reader. Frame i carries i + 1 on L
/// and -(i + 1) on R, so silence is never a frame and every frame names its
/// index. The reader waits until the ring holds more than its delay, then
/// plays 512-frame blocks at 4096, then 8192, then 2048 frames of delay.
func ringAndReader() {
    let rate: UInt32 = 48_000
    let capacity = asc_ring_capacity_for_rate(rate)
    check(capacity == 1_152_000, "a 48 kHz ring holds 24 s: \(capacity) frames")
    guard let ring = asc_ring_create(capacity), let writer = asc_ring_writer_create(ring) else {
        check(false, "asc_ring_create, asc_ring_writer_create")
        return
    }
    defer {
        asc_ring_writer_destroy(writer)
        asc_ring_destroy(ring)
    }
    check(asc_ring_writer_create(ring) == nil, "a second writer is refused")
    guard let reader = asc_reader_create(ring, rate) else {
        check(false, "asc_reader_create")
        return
    }
    defer { asc_reader_destroy(reader) }

    let frames = 512
    var next = 0
    var block = [Float](repeating: 0, count: frames * 2)
    var out = [Float](repeating: 0, count: frames * 2)
    var report = AscReadReport()
    var played: [Int] = []
    var torn = false

    // Plays `rounds` blocks at `delay`; returns how many frames were silent
    // and whether every block started exactly `delay` frames behind the writer.
    func play(_ rounds: Int, delay: Int) -> (silent: Int, exact: Bool) {
        var silent = 0
        var exact = true
        for _ in 0..<rounds {
            for i in 0..<frames {
                let tag = Float(next + i + 1)
                block[2 * i] = tag
                block[2 * i + 1] = -tag
            }
            asc_ring_write(writer, block, frames)
            next += frames
            asc_reader_read(reader, &out, frames, &report)
            exact = exact && report.start == Int64(next - delay) && report.frames == frames
            for i in 0..<frames {
                let left = out[2 * i]
                if left == 0 {
                    silent += 1
                    continue
                }
                torn = torn || out[2 * i + 1] != -left
                played.append(Int(left) - 1)
            }
        }
        return (silent, exact)
    }

    asc_reader_set_delay_frames(reader, 4096)
    let waiting = play(8, delay: 4096)
    check(
        waiting.silent == 8 * frames && asc_reader_counters(reader).primes == 0
            && asc_reader_lag_frames(reader) == Int64(next),
        "silent, and not yet anchored, until more than 4096 frames are written (\(waiting.silent) frames)")
    let first = play(92, delay: 4096)
    check(first.exact, "then each block starts exactly 4096 frames behind the writer")
    check(first.silent == 0, "no silence once anchored (\(first.silent) frames)")
    check(asc_ring_frames_written(ring) == UInt64(next), "the ring counts \(next) frames written")
    check(asc_reader_lag_frames(reader) == Int64(4096 - frames), "lag is the delay minus one block: \(asc_reader_lag_frames(reader))")

    asc_reader_set_delay_frames(reader, 8192)
    let longer = play(100, delay: 8192)
    check(longer.exact && longer.silent == 4096, "a 4096-frame longer delay inserts exactly 4096 frames of silence (\(longer.silent))")

    asc_reader_set_delay_frames(reader, 2048)
    let shorter = play(50, delay: 2048)
    check(shorter.exact && shorter.silent == 0, "a shorter delay skips ahead on the next block")
    check(report.correction == ASC_CORRECTION_NONE && report.lag == Int64(2048 - frames), "steady again at 2048 frames")

    let increasing = zip(played, played.dropFirst()).allSatisfy { $0 < $1 }
    check(increasing && !torn, "\(played.count) frames played, none twice, none torn")
    let counters = asc_reader_counters(reader)
    check(
        counters.primes == 1 && counters.resyncs == 2 && counters.drift_skips == 0 && counters.drift_silences == 0
            && counters.starved_renders == 0,
        "counters: 1 prime, 2 resyncs, no drift correction, no starved render")
}

/// What the pipe sink below received.
final class PipeRecorder {
    var stream: [UInt8] = []
    var opens = 0
    var closes = 0
}

func recorder(_ context: UnsafeMutableRawPointer?) -> PipeRecorder {
    Unmanaged<PipeRecorder>.fromOpaque(context!).takeUnretainedValue()
}

/// The FIFO feed: PCM16 conversion, the writer stepped through C callbacks
/// that record what a pipe would receive, and 44.1 kHz input converted to the
/// pipe's 48 kHz.
func fifoFeed() {
    let samples: [Float] = [0.5, -0.5, 1, -1, 2, -2, .nan, .infinity, -.infinity, 0.25, 0.75, -0.25]
    let expected: [Int16] = [16384, -16384, 32767, -32767, 32767, -32767, 0, 32767, -32767, 8192, 24575, -8192]
    var converted = [Int16](repeating: 99, count: samples.count)
    let count = asc_convert_to_pcm16(samples, &converted, samples.count)
    check(count == samples.count && converted == expected, "PCM16: clamped, ties away from zero, NaN to 0")

    guard let feed = asc_fifo_create(UInt32(ASC_FIFO_DEFAULT_QUEUE_SECONDS)) else {
        check(false, "asc_fifo_create")
        return
    }
    defer { asc_fifo_destroy(feed) }
    check(asc_fifo_set_input_rate(feed, UInt32(ASC_PIPE_RATE)), "input at the pipe's rate")
    guard let writer = asc_fifo_writer_create(feed) else {
        check(false, "asc_fifo_writer_create")
        return
    }
    check(asc_fifo_writer_create(feed) == nil, "a second FIFO writer is refused")

    let pipe = PipeRecorder()
    var sink = AscPipeSink(
        context: Unmanaged.passUnretained(pipe).toOpaque(),
        open: { context in
            recorder(context).opens += 1
            return Int32(ASC_PIPE_OK)
        },
        write: { context, bytes, length, written in
            recorder(context).stream.append(contentsOf: UnsafeBufferPointer(start: bytes, count: length))
            written?.pointee = length
            return Int32(ASC_PIPE_OK)
        },
        close: { context in
            recorder(context).closes += 1
        })
    // The first step opens the pipe, which starts at the live edge: only
    // audio fed after it is written.
    check(asc_fifo_writer_step(writer, &sink) == 0 && pipe.opens == 1, "the first step opens the pipe")
    let queued = asc_fifo_feed(feed, samples, samples.count / 2)
    check(queued == samples.count / 2, "\(queued) frames queued at 48 kHz pass through")
    var steps = 0
    while asc_fifo_writer_step(writer, &sink) == 0 && steps < 100 {
        steps += 1
    }
    var pause: UInt32 = 0
    while asc_fifo_writer_close_step(writer, &sink, &pause) {}
    asc_fifo_writer_destroy(writer)
    let wire = expected.flatMap { value -> [UInt8] in
        let bits = UInt16(bitPattern: value)
        return [UInt8(bits & 0xff), UInt8(bits >> 8)]
    }
    check(pipe.stream == wire, "the pipe gets \(pipe.stream.count) bytes of little-endian PCM16, L then R")
    check(pipe.opens == 1 && pipe.closes == 1, "the writer opened and closed the pipe once")
    let afterWrite = asc_fifo_counters(feed)
    check(afterWrite.opens == 1 && afterWrite.bytes_written == UInt64(wire.count), "counters: 1 open, \(afterWrite.bytes_written) bytes written")

    check(asc_fifo_set_input_rate(feed, 44_100), "input at 44.1 kHz")
    let block = [Float](repeating: 0.1, count: 441 * 2)
    for _ in 0..<200 {
        asc_fifo_feed(feed, block, 441)
    }
    let made = Int((asc_fifo_counters(feed).samples_queued - afterWrite.samples_queued) / 2)
    check(abs(made - 96_000) <= 2, "2 s at 44.1 kHz become \(made) frames at 48 kHz")
}

/// Runs `body` with `strings` as a C array of NUL-terminated UTF-8 strings.
func withCStrings<R>(_ strings: [String], _ body: (UnsafePointer<UnsafePointer<CChar>?>?, Int) -> R) -> R {
    let copies = strings.map { strdup($0) }
    defer { copies.forEach { free($0) } }
    let pointers = copies.map { UnsafePointer($0) }
    return pointers.withUnsafeBufferPointer { body($0.baseAddress, $0.count) }
}

/// The AirPlay sync rule over Swift strings, and the delay helpers.
func syncRule() {
    func decide(enabled: [String], listed: [String], wasChosen: Bool) -> AscSyncDecision {
        withCStrings(enabled) { enabledIDs, enabledCount in
            withCStrings(listed) { listedIDs, listedCount in
                var input = AscSyncInput(
                    enabled_ids: enabledIDs, enabled_count: enabledCount, listed_ids: listedIDs,
                    listed_count: listedCount, was_chosen: wasChosen)
                return asc_sync_decide(&input)
            }
        }
    }
    let on = decide(enabled: ["40", "41"], listed: ["12", "40"], wasChosen: false)
    check(on.airplay_chosen && on.flipped, "an enabled AirPlay speaker the engine lists is chosen, a flip")
    let gone = decide(enabled: ["41"], listed: ["12", "40"], wasChosen: true)
    check(!gone.airplay_chosen && gone.flipped, "an enabled speaker the engine doesn't list isn't chosen")
    check(
        asc_sync_effective_delay_ms(1820, true) == 1820 && asc_sync_effective_delay_ms(1820, false) == 0,
        "local delays apply only while an AirPlay speaker is chosen")
    var frames: Int64 = 0
    check(asc_delay_ms_to_frames(1820, 48_000, &frames) && frames == 87_360, "1820 ms is \(frames) frames at 48 kHz")
}

/// The local delay model: the hold-backs of the built-in speakers (the
/// reference), a USB speaker at +150 ms and a Bluetooth speaker at -250 ms,
/// the native sender's L, and the one-time migration of stored delays.
func localDelayModel() {
    let outputs = [
        AscLocalOutput(offset_ms: 0, is_reference: true),
        AscLocalOutput(offset_ms: 150, is_reference: false),
        AscLocalOutput(offset_ms: -250, is_reference: false),
    ]
    var held = [Double](repeating: -1, count: 3)
    check(asc_local_hold_backs_ms(outputs, 3, false, 2000, &held) && held == [250, 400, 0],
          "only local outputs: hold-backs \(held) ms")
    check(asc_local_hold_backs_ms(outputs, 3, true, 2000, &held) && held == [2000, 2150, 1750],
          "an AirPlay speaker chosen adds L: \(held) ms")
    check(asc_local_native_latency_ms(1750, 250) == 2000, "the native sender's L is its latency plus the adjustment")
    var offset: Double = 0
    check(asc_local_offset_sanitize_ms(-2500, &offset) && offset == -2000, "offsets stop at \(offset) ms")
    let stored = [
        AscStoredDelay(is_reference: true, has_stored_delay: true, stored_delay_ms: 2300),
        AscStoredDelay(is_reference: false, has_stored_delay: false, stored_delay_ms: 0),
    ]
    var offsets = [AscMigratedOffset](repeating: AscMigratedOffset(), count: 2)
    var migration = AscDelayMigration()
    check(asc_local_delay_migrate(stored, 2, false, 0, &offsets, &migration)
              && migration.airplay_delay_ms == 2300 && !offsets[0].store
              && offsets[1].store && offsets[1].offset_ms == -300,
          "migration: AirPlay delay \(migration.airplay_delay_ms) ms, an untuned output at \(offsets[1].offset_ms) ms")
}

// AudioSync's own AirPlay sender: created, asked for receivers before any
// discovery (none), its first event and its input, and destroyed twice
// through the same pointer, the second time doing nothing. No discovery
// starts and no speaker is added, so nothing touches the network.
func airPlaySender() {
    var handle: OpaquePointer? = asc_airplay_create()
    check(handle != nil, "the AirPlay sender starts")
    var receiver = AscAirPlayReceiver()
    receiver.availability = -1
    check(!asc_airplay_next_receiver(handle, &receiver) && receiver.availability == -1,
          "no receiver before discovery, the out struct untouched")
    check(!asc_airplay_next_receiver(nil, &receiver) && !asc_airplay_start_discovery(nil),
          "a NULL sender does nothing")
    // Its first event is the timeline, with the base latency L (1.75 s).
    var event = AscAirPlayEvent()
    var taken = false
    for _ in 0..<500 where !taken {
        taken = asc_airplay_next_event(handle, &event)
        if !taken { usleep(10_000) }
    }
    check(taken && event.kind == ASC_AIRPLAY_EVENT_TIMELINE && abs(event.latency_ms - 1750) < 0.01
              && abs(asc_airplay_latency_ms(handle) - 1750) < 0.01 && String(cString: event.id).isEmpty,
          "the first event is the timeline, L = \(event.latency_ms) ms")
    check(!asc_airplay_next_event(handle, &event), "a working sender reports nothing more, no failure")
    check(!asc_airplay_add_speaker(handle, "02005E005301", 50), "only a listed receiver can be added")
    asc_airplay_set_master(handle, 80)
    // The capture's input: handed over once, silent until it has a rate,
    // then 10 ms at 48 kHz become 441 frames at 44.1 kHz.
    var input: OpaquePointer? = asc_airplay_take_input(handle)
    check(input != nil && asc_airplay_take_input(handle) == nil, "the input is handed over once")
    let block = [Float](repeating: 0.1, count: 480 * 2)
    check(asc_airplay_input_write(input, block, 480) == 0, "no audio before a rate")
    check(asc_airplay_input_set_rate(input, 48_000) && asc_airplay_input_write(input, block, 480) == 441,
          "10 ms at 48 kHz become 441 frames at 44.1 kHz")
    asc_airplay_destroy(&handle)
    check(asc_airplay_input_write(input, block, 480) == 441, "the input outlives its sender")
    asc_airplay_input_destroy(&input)
    check(input == nil, "the input's destroy sets the host's pointer to NULL")
    check(handle == nil, "destroy sets the host's pointer to NULL")
    asc_airplay_destroy(&handle)
    asc_airplay_destroy(nil)
    check(Int(ASC_AIRPLAY_FLAG_AIRPLAY2) == 128 && ASC_AIRPLAY_GONE == 3 && ASC_AIRPLAY_EVENT_FAILED == 5,
          "the AirPlay constants cross the header")
}

versionVolumeAndEqualizer()
ringAndReader()
fifoFeed()
syncRule()
localDelayModel()
airPlaySender()
exit(failures == 0 ? 0 : 1)
