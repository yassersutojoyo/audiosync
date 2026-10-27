// Engine test on real output hardware, completely silent: every output runs at
// gain 0, so no sound is produced. Checks that each output device renders,
// that per-speaker delay is applied end to end (and changes live), and that
// an output fades in on its first audio and plays its fade-out before its IO
// stops (S184-S186), on the Rust core's ring and renderers as a session makes
// them. Run with: Tests/run-tests.sh --hardware
import Foundation
import CoreAudio
import Synchronization

var failedChecks = 0
func check(_ ok: Bool, _ name: String, _ detail: String = "") {
    print((ok ? "✅ PASS  " : "❌ FAIL  ") + name + (detail.isEmpty ? "" : "  — " + detail))
    if !ok { failedChecks += 1 }
}

let rate = 48000.0
guard let parts = EngineParts.make() else {
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

// covers: S61
for dev in devices {
    guard let r = parts.makeRenderer(captureRate: rate) else {
        check(false, "\(dev.name): output can be made")
        continue
    }
    r.setGain(linear: 0)   // silent
    do {
        try r.start(on: dev.id, captureRate: rate, delayMs: 2000)
        r.setGain(linear: 0)
    } catch let startError {
        check(false, "\(dev.name): output starts", "\(startError)")
        continue
    }
    // S184: the ring holds more than the delay, so the first callback copies
    // audio, and the output fades in from closed to open. A device whose
    // first callback holds the whole fade ends it already open.
    var states: [CoreRenderer.FadeState] = []
    var firstCallbackFrames: Int64 = 0
    let watchUntil = Date().addingTimeInterval(0.3)
    while Date() < watchUntil {
        let state = r.fadeState
        if states.last != state { states.append(state) }
        if firstCallbackFrames == 0 { firstCallbackFrames = r.framesRendered.load(ordering: .relaxed) }
        usleep(250)
    }
    let wholeFadeInOneCallback = firstCallbackFrames >= Int64(r.fadeFrames)
    check(states.last == .open && (states.contains(.fadingIn) || wholeFadeInOneCallback),
          "\(dev.name): first audio fades in",
          "\(states) over \(r.fadeFrames) frames, first callback \(firstCallbackFrames) frames")

    Thread.sleep(forTimeInterval: 0.7)
    // covers: S67
    let rendered = r.framesRendered.load(ordering: .relaxed)
    check(rendered > 24000, "\(dev.name): renders audio", "\(rendered) frames in 1 s")

    // covers: S66
    // Lag ≈ delay − one device buffer, ± one producer chunk; each change
    // fades out and back in (S187) and lands exactly (S46).
    for ms in [2000.0, 500.0, 3000.0, 150.0] {
        r.setDelay(ms: ms)
        Thread.sleep(forTimeInterval: 0.4)
        let lagMs = Double(r.lagFrames) / rate * 1000
        check(abs(lagMs - ms) < 60, "\(dev.name): delay \(Int(ms)) ms",
              String(format: "measured %.0f ms", lagMs))
    }

    // S185: asked to fade out, the output keeps its IO running and renders
    // zeros after the fade until the host clock passes the fade's last
    // frame's output time plus the device's latency; only then may it stop.
    // An output whose latency is beyond the 500 ms cap is stopped at the cap.
    let latencyMs = Double(r.outputLatencyNs) / 1_000_000
    let beyondCap = r.outputLatencyNs >= OutputRetirement.capNs
    let renderedBefore = r.framesRendered.load(ordering: .relaxed)
    let asked = HostClock.now()
    r.requestFadeOut()
    var fadeStates: [CoreRenderer.FadeState] = []
    var fadedAfterNs: UInt64?
    while HostClock.now() - asked < OutputRetirement.capNs {
        let state = r.fadeState
        if fadeStates.last != state { fadeStates.append(state) }
        let now = HostClock.now()
        if r.hasFaded(nowNs: now) {
            fadedAfterNs = now - asked
            break
        }
        usleep(250)
    }
    let renderedWhileFading = r.framesRendered.load(ordering: .relaxed) - renderedBefore
    r.halt()
    let playedFirst = fadedAfterNs != nil && r.stopNote == "stopped once its fade-out had played"
    var fadeDetail = "\(fadeStates); "
    if let fadedAfterNs {
        fadeDetail += String(format: "played after %.1f ms", Double(fadedAfterNs) / 1e6)
    } else {
        fadeDetail += "not played within 500 ms"
    }
    fadeDetail += String(format: ", latency %.1f ms, ", latencyMs)
    fadeDetail += "\(renderedWhileFading) frames rendered meanwhile; \(r.stopNote)"
    check(fadeStates.last == .closed && renderedWhileFading >= Int64(r.fadeFrames) && (playedFirst || beyondCap),
          "\(dev.name): stop fades out before the output stops", fadeDetail)

    // S186: a switch-off the way the session makes it returns once the fade
    // has played, never later than 500 ms after the request.
    do {
        try r.start(on: dev.id, captureRate: rate, delayMs: 150)
        r.setGain(linear: 0)
        Thread.sleep(forTimeInterval: 0.4)
        let retirement = OutputRetirement()
        let began = Date()
        retirement.remove([r])
        retirement.finishAll()
        let took = Date().timeIntervalSince(began)
        let played = r.stopNote == "stopped once its fade-out had played"
        check(retirement.isIdle && took <= 0.52 && (played || took >= 0.5),
              "\(dev.name): stop returns once the fade has played, within 500 ms",
              String(format: "%.1f ms, latency %.1f ms; ", took * 1000, latencyMs) + r.stopNote)
    } catch {
        check(false, "\(dev.name): stop returns once the fade has played, within 500 ms", "restart: \(error)")
    }
    r.halt()
}

producing.store(false, ordering: .relaxed)
print(failedChecks == 0 ? "\nALL ENGINE TESTS PASSED" : "\n\(failedChecks) FAILURE(S)")
exit(failedChecks > 0 ? 1 : 0)
