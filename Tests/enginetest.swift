// Engine test on real output hardware, completely silent: every output runs at
// gain 0, so no sound is produced. Checks that each output device renders, and
// that per-speaker delay is applied end to end (and changes live), on the
// Rust core's ring, readers and gain stages as a session makes them.
// Run with: Tests/run-tests.sh --hardware
import Foundation
import CoreAudio
import Synchronization

var failedChecks = 0
func check(_ ok: Bool, _ name: String, _ detail: String = "") {
    print((ok ? "✅ PASS  " : "❌ FAIL  ") + name + (detail.isEmpty ? "" : "  — " + detail))
    if !ok { failedChecks += 1 }
}

let rate = 48000.0
guard let parts = EngineParts.make(fifoPath: nil) else {
    print("❌ FAIL  the core's session parts could not be made")
    exit(1)
}
let ring = parts.ring

// Real-time producer: 10 ms chunks paced by the wall clock, like a capture tap.
let producing = Atomic<Bool>(true)
let producer = Thread {
    let chunk = 480
    let buf = UnsafeMutablePointer<Float>.allocate(capacity: chunk * 2)
    buf.initialize(repeating: 0, count: chunk * 2)
    let start = Date()
    var written = 0
    while producing.load(ordering: .relaxed) {
        let due = Int(Date().timeIntervalSince(start) * rate)
        while written + chunk <= due {
            ring.write(interleaved: buf, frames: chunk)
            written += chunk
        }
        usleep(2000)
    }
}
producer.start()
Thread.sleep(forTimeInterval: 3.5)   // build up history beyond the largest delay tested

let devices = AudioDevices().allOutputDevices().filter { !$0.uid.isEmpty && !$0.name.contains("Aggregate") }
check(!devices.isEmpty, "found output devices", devices.map(\.name).joined(separator: ", "))

for dev in devices {
    guard let r = parts.makeRenderer(captureRate: rate) else {
        check(false, "\(dev.name): output can be made")
        continue
    }
    r.setGain(0)   // silent
    do {
        try r.start(deviceID: dev.id, inputSampleRate: rate, delayMs: 2000)
        r.setGain(0)
    } catch let startError {
        check(false, "\(dev.name): output starts", "\(startError)")
        continue
    }
    Thread.sleep(forTimeInterval: 1.0)
    let rendered = r.framesRendered.load(ordering: .relaxed)
    check(rendered > 24000, "\(dev.name): renders audio", "\(rendered) frames in 1 s")

    // Lag ≈ delay − one device buffer, ± one producer chunk.
    for ms in [2000.0, 500.0, 3000.0, 150.0] {
        r.setDelayMs(ms)
        Thread.sleep(forTimeInterval: 0.4)
        let lagMs = Double(r.lagFrames) / rate * 1000
        check(abs(lagMs - ms) < 60, "\(dev.name): delay \(Int(ms)) ms",
              String(format: "measured %.0f ms", lagMs))
    }
    r.stop()
}

producing.store(false, ordering: .relaxed)
print(failedChecks == 0 ? "\nALL ENGINE TESTS PASSED" : "\n\(failedChecks) FAILURE(S)")
exit(failedChecks > 0 ? 1 : 0)
