// Calls audiosync-core through its C header, the way the macOS app will:
// the version, the volume curve, the presets and one equalizer pass over
// whole 512-frame buffers. Prints one line per check; exits 1 on a failure.
// Touches no audio device.

import AudioSyncCore
import Foundation

func run() -> Int32 {
    var failures: Int32 = 0
    func check(_ ok: Bool, _ what: String) {
        print("\(ok ? "ok  " : "FAIL") \(what)")
        if !ok { failures += 1 }
    }

    let version = String(cString: asc_version())
    check(!version.isEmpty, "core version \(version)")
    check(asc_abi_version() == UInt32(ASC_ABI_VERSION), "interface version \(asc_abi_version())")
    check(asc_volume_gain(50) == 0.125, "volume 50 gives gain 0.125 (about -18 dB)")
    check(abs(asc_volume_position(0.001) - 10) < 1e-9, "gain 0.001 is volume 10 (-60 dB)")
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
        return failures
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
    return failures
}

exit(run() == 0 ? 0 : 1)
