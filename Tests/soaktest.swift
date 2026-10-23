// Soak and benchmark harness for the engine's local-output path, on the Rust
// core's parts as a session makes them.
// Completely silent: every local output runs at gain 0 from before its first
// render callback, and only virtual or built-in outputs are picked by default
// (never AirPlay, never an input device). No system audio setting is changed.
//
// A wall-clock-paced producer stands in for the capture tap. It writes a known
// signal (a frame counter encoded in both channels) into the delay ring buffer
// that the OutputRenderers read. Every `--interval` seconds one record goes to
// windows.jsonl and windows.csv; at the end result.json and summary.txt hold
// the totals and, with `--baseline`, PASS/FAIL against thresholds derived from
// that baseline (see docs/BENCHMARKS.md).
//
// Built and run by Tests/soak.sh and Tests/bench.sh (not by Tests/run-tests.sh,
// which only compiles it, and checks the rules in Tests/soakrules.swift).
//   --out DIR           output directory (required)
//   --seconds S         measured duration (default 3600)
//   --outputs N         local outputs to open, 0…2 (default 2)
//   --delay-ms D        delay of every local output during the run (default 2000)
//   --interval S        seconds per logged window (default 10)
//   --delay-accuracy    first measure lag at 150/500/2000/3000 ms on each output
//   --baseline FILE     baseline JSON written by Tests/bench.sh
//   --device NAME       use this output (repeatable) instead of the default pick
//   --label NAME        name recorded in the result (default "soak")
//   --list              list the outputs the default pick would use, then exit
// Exit status: 0 PASS, 1 FAIL, 2 setup error (including a baseline that lacks a
// metric this run is judged on), 3 no baseline given (numbers only).
import Foundation
import CoreAudio
import Darwin
import Synchronization

// MARK: - Options

struct Options {
    var outDir = ""
    var seconds = 3600.0
    var outputs = 2
    var delayMs = 2000.0
    var interval = 10.0
    var delayAccuracy = false
    var baseline: String?
    var devices: [String] = []
    var label = "soak"
    var list = false
}

/// The engine a result and a baseline name ("rust", the Rust core: the only
/// one since the Swift engine was removed; older baselines may say "swift").
let engineName = "rust"

func fail(_ message: String) -> Never {
    FileHandle.standardError.write(Data(("soaktest: " + message + "\n").utf8))
    exit(2)
}

var opt = Options()
do {
    var args = Array(CommandLine.arguments.dropFirst())
    func value(_ flag: String) -> String {
        guard !args.isEmpty else { fail("\(flag) needs a value") }
        return args.removeFirst()
    }
    func number(_ flag: String) -> Double {
        let v = value(flag)
        guard let d = Double(v) else { fail("\(flag): not a number: \(v)") }
        return d
    }
    while !args.isEmpty {
        let a = args.removeFirst()
        switch a {
        case "--out": opt.outDir = value(a)
        case "--seconds": opt.seconds = number(a)
        case "--outputs": opt.outputs = Int(number(a))
        case "--delay-ms": opt.delayMs = number(a)
        case "--interval": opt.interval = number(a)
        case "--delay-accuracy": opt.delayAccuracy = true
        case "--baseline": opt.baseline = value(a)
        case "--device": opt.devices.append(value(a))
        case "--label": opt.label = value(a)
        case "--list": opt.list = true
        default: fail("unknown option \(a)")
        }
    }
}
if !opt.list && opt.outDir.isEmpty { fail("--out DIR is required") }
guard (0...2).contains(opt.outputs) else { fail("--outputs must be 0, 1 or 2") }
guard opt.seconds > 0, opt.interval >= 1 else { fail("--seconds must be > 0 and --interval >= 1") }

// MARK: - Clock, signal, process stats

let rate = 48000.0
func nowNs() -> UInt64 { clock_gettime_nsec_np(CLOCK_UPTIME_RAW) }
func seconds(since t: UInt64) -> Double { Double(nowNs() &- t) / 1e9 }

/// The test signal: absolute frame index n, 14 bits per channel (L = low bits,
/// R = the next 14), spaced 4 16-bit steps apart. Repeats every 2^28 frames
/// (~93 min).
@inline(__always) func rampSample(_ bits: Int64) -> Float {
    Float(Int(bits & 0x3FFF) * 4 - 32766) / 32767
}

func cpuSeconds() -> Double {
    var u = rusage()
    getrusage(RUSAGE_SELF, &u)
    return Double(u.ru_utime.tv_sec) + Double(u.ru_utime.tv_usec) / 1e6
        + Double(u.ru_stime.tv_sec) + Double(u.ru_stime.tv_usec) / 1e6
}

/// Physical footprint (dirty + compressed memory), the figure Activity Monitor
/// shows as Memory. Steadier than RSS, which drops when the OS compresses the
/// ring buffer's untouched pages, so it is what the leak check uses.
func footprintMB() -> Double {
    var info = task_vm_info_data_t()
    var count = mach_msg_type_number_t(MemoryLayout<task_vm_info_data_t>.size / MemoryLayout<natural_t>.size)
    let kr = withUnsafeMutablePointer(to: &info) {
        $0.withMemoryRebound(to: integer_t.self, capacity: Int(count)) {
            task_info(mach_task_self_, task_flavor_t(TASK_VM_INFO), $0, &count)
        }
    }
    return kr == KERN_SUCCESS ? Double(info.phys_footprint) / 1_048_576 : 0
}

func r3(_ x: Double) -> Double { (x * 1000).rounded() / 1000 }
func ms(_ frames: Double) -> Double { frames / rate * 1000 }

// MARK: - Devices

let allDevices = AudioDevices().allOutputDevices().filter { !$0.uid.isEmpty }
func transportName(_ t: UInt32) -> String {
    switch t {
    case kAudioDeviceTransportTypeVirtual: return "virtual"
    case kAudioDeviceTransportTypeBuiltIn: return "built-in"
    case kAudioDeviceTransportTypeAirPlay: return "airplay"
    case kAudioDeviceTransportTypeBluetooth, kAudioDeviceTransportTypeBluetoothLE: return "bluetooth"
    case kAudioDeviceTransportTypeUSB: return "usb"
    case kAudioDeviceTransportTypeAggregate, kAudioDeviceTransportTypeAutoAggregate: return "aggregate"
    default: return "other"
    }
}

var chosen: [AudioDevices.Device] = []
if opt.devices.isEmpty {
    // Virtual outputs first (they make no sound at all), then the built-in ones.
    let virtual = allDevices.filter { $0.transport == kAudioDeviceTransportTypeVirtual }
    let builtIn = allDevices.filter { $0.transport == kAudioDeviceTransportTypeBuiltIn }
    chosen = Array((virtual + builtIn).prefix(opt.outputs))
} else {
    for name in opt.devices {
        guard let d = allDevices.first(where: { $0.name == name }) else { fail("no output named \"\(name)\"") }
        // AirPlay outputs reach real speakers; aggregate and multi-output devices
        // could hide one among their sub-devices. Neither is ever used.
        switch d.transport {
        case kAudioDeviceTransportTypeAirPlay:
            fail("\(name) is an AirPlay output; not used")
        case kAudioDeviceTransportTypeAggregate, kAudioDeviceTransportTypeAutoAggregate:
            fail("\(name) is an aggregate or multi-output device; not used")
        default: break
        }
        chosen.append(d)
    }
    chosen = Array(chosen.prefix(opt.outputs))
}
if opt.list {
    for d in chosen { print("\(d.name)\t\(transportName(d.transport))") }
    exit(0)
}
guard chosen.count == opt.outputs else {
    fail("needs \(opt.outputs) output(s), found \(chosen.count) virtual/built-in: "
         + chosen.map(\.name).joined(separator: ", "))
}

let outURL = URL(fileURLWithPath: opt.outDir)
try? FileManager.default.createDirectory(at: outURL, withIntermediateDirectories: true)

// MARK: - Baseline (read before the run, so a bad one fails in seconds)

func sysctlString(_ name: String) -> String {
    var size = 0
    guard sysctlbyname(name, nil, &size, nil, 0) == 0, size > 0 else { return "" }
    var buf = [CChar](repeating: 0, count: size)
    guard sysctlbyname(name, &buf, &size, nil, 0) == 0 else { return "" }
    return String(cString: buf)
}
let osv = ProcessInfo.processInfo.operatingSystemVersion
let macOSVersion = osv.patchVersion == 0 ? "\(osv.majorVersion).\(osv.minorVersion)"
    : "\(osv.majorVersion).\(osv.minorVersion).\(osv.patchVersion)"
let modelIdentifier = sysctlString("hw.model")

var baselineConfigs: [String: Any] = [:]
var baselineNote = "no baseline"
var warnings: [String] = []
func baseValue(_ config: String, _ key: String) -> Double? {
    let h = (baselineConfigs[config] as? [String: Any])?["harness"] as? [String: Any]
    let v = (h?["metrics"] as? [String: Any])?[key]
    return (v as? Double) ?? (v as? NSNumber)?.doubleValue
}
/// The baseline metrics this run is judged on, as (config, key).
var judgedOn: [(String, String)] = [("two_local", "footprint_mb_growth"), ("two_local", "cpu_pct_mean")]
if opt.outputs > 0 {
    judgedOn += [("two_local", "local_dropouts_per_h"), ("two_local", "local_starved_renders_per_h"),
                 ("two_local", "local_drift_corrections_per_h"), ("two_local", "local_lag_err_ms_max_abs")]
}
if opt.outputs == 2 { judgedOn.append(("two_local", "inter_output_drift_ms_max_abs")) }
if let path = opt.baseline {
    guard let data = FileManager.default.contents(atPath: path),
          let root = try? JSONSerialization.jsonObject(with: data) as? [String: Any] else {
        fail("could not read the baseline \(path)")
    }
    baselineConfigs = root["configs"] as? [String: Any] ?? [:]
    for (config, key) in judgedOn where baseValue(config, key) == nil {
        fail("the baseline \(path) has no \(config) metric \(key); record it again with Tests/bench.sh")
    }
    baselineNote = "baseline \(path) (\(root["engine"] ?? "?") engine, "
        + "\(root["minutes_per_config"] ?? "?") min per configuration, commit \(root["commit"] ?? "?"))"
    if let e = root["engine"] as? String, e != engineName {
        warnings.append("baseline engine \(e) differs from this run's (\(engineName))")
    }
    let machine = root["machine"] as? [String: Any] ?? [:]
    if let m = machine["model_identifier"] as? String, m != modelIdentifier {
        warnings.append("baseline machine \(m) differs from this one (\(modelIdentifier))")
    }
    if let v = machine["macos"] as? String, v != macOSVersion {
        warnings.append("baseline macOS \(v) differs from this one (\(macOSVersion))")
    }
    let ref = (((baselineConfigs["two_local"] as? [String: Any])?["harness"] as? [String: Any])?["config"]
        as? [String: Any])?["outputs"] as? [[String: Any]] ?? []
    let refNames = ref.compactMap { $0["name"] as? String }.prefix(opt.outputs)
    if opt.outputs > 0 && Array(refNames) != chosen.map(\.name) {
        warnings.append("baseline outputs (\(refNames.joined(separator: ", "))) differ from this run's ("
            + chosen.map(\.name).joined(separator: ", ") + ")")
    }
    for w in warnings { FileHandle.standardError.write(Data(("soaktest: warning: " + w + "\n").utf8)) }
}

// MARK: - Producer (stands in for the capture tap)

// The engine's parts as a session makes them: the ring sized as StreamEngine's
// (no equalizer: capture isn't measured).
guard let parts = EngineParts.make() else {
    fail("the core's session parts could not be made")
}
let ring = parts.ring
let producedFrames = Atomic<Int64>(0)
let producerLateMax = Atomic<Int64>(0)      // frames; the sampler resets it each window
let producing = Atomic<Bool>(true)

let producer = Thread {
    let chunk = 512
    let buf = UnsafeMutablePointer<Float>.allocate(capacity: chunk * 2)
    let start = nowNs()
    var written: Int64 = 0
    while producing.load(ordering: .relaxed) {
        let due = Int64(seconds(since: start) * rate)
        let late = due - written
        if late > producerLateMax.load(ordering: .relaxed) { producerLateMax.store(late, ordering: .relaxed) }
        while written + Int64(chunk) <= due {
            for i in 0..<chunk {
                let n = written + Int64(i)
                buf[i * 2] = rampSample(n)
                buf[i * 2 + 1] = rampSample(n >> 14)
            }
            ring.write(interleaved: buf, frames: chunk)
            written += Int64(chunk)
            producedFrames.store(written, ordering: .releasing)
        }
        usleep(2000)
    }
}
producer.qualityOfService = .userInteractive

producer.start()
Thread.sleep(forTimeInterval: 3.5)   // history beyond the largest delay tested

// MARK: - Outputs (silent)

/// Counts kAudioDeviceProcessorOverload notifications for one device: the HAL
/// missed an IO cycle deadline, which is an audible dropout on that output.
final class OverloadCounter {
    let count = Atomic<Int64>(0)
    private let device: AudioObjectID
    private var block: AudioObjectPropertyListenerBlock?
    private static let queue = DispatchQueue(label: "soaktest.overload")
    private var address = AudioObjectPropertyAddress(mSelector: kAudioDeviceProcessorOverload,
                                                     mScope: kAudioObjectPropertyScopeGlobal,
                                                     mElement: kAudioObjectPropertyElementMain)
    init(device: AudioObjectID) {
        self.device = device
        let b: AudioObjectPropertyListenerBlock = { [unowned self] _, _ in self.count.add(1, ordering: .relaxed) }
        if AudioObjectAddPropertyListenerBlock(device, &address, Self.queue, b) == noErr { block = b }
    }
    var installed: Bool { block != nil }
    func remove() {
        if let b = block { AudioObjectRemovePropertyListenerBlock(device, &address, Self.queue, b) }
        block = nil
    }
}

var overloads: [OverloadCounter] = []
var renderers: [OutputRenderer] = []
for dev in chosen {
    let oc = OverloadCounter(device: dev.id)
    guard oc.installed else { fail("\(dev.name): could not watch for processor overloads") }
    overloads.append(oc)
    guard let r = parts.makeRenderer(captureRate: rate) else {
        fail("\(dev.name): the core could not make a renderer")
    }
    r.setGain(linear: 0)   // silent before the first render callback can run
    do {
        try r.start(on: dev.id, captureRate: rate, delayMs: opt.delayAccuracy ? 2000 : opt.delayMs)
    } catch {
        fail("\(dev.name): output did not start: \(error)")
    }
    renderers.append(r)
}

func sampleLag(_ r: OutputRenderer, count: Int, every: Double) -> (mean: Double, min: Double, max: Double) {
    var s = 0.0, lo = Double.infinity, hi = -Double.infinity
    for _ in 0..<count {
        let v = Double(r.lagFrames)
        s += v; lo = min(lo, v); hi = max(hi, v)
        Thread.sleep(forTimeInterval: every)
    }
    return (s / Double(count), lo, hi)
}

var delayAccuracy: [[String: Any]] = []
if opt.delayAccuracy {
    Thread.sleep(forTimeInterval: 1.0)
    for (i, r) in renderers.enumerated() {
        for target in [150.0, 500.0, 2000.0, 3000.0] {
            r.setDelay(ms: target)
            Thread.sleep(forTimeInterval: 0.5)
            let s = sampleLag(r, count: 20, every: 0.05)
            delayAccuracy.append([
                "output": chosen[i].name, "target_ms": target,
                "measured_ms": r3(ms(s.mean)), "error_ms": r3(ms(s.mean) - target),
                "min_ms": r3(ms(s.min)), "max_ms": r3(ms(s.max)),
            ])
            print(String(format: "delay %@ %4.0f ms: measured %.1f ms (error %+.1f ms)",
                         chosen[i].name, target, ms(s.mean), ms(s.mean) - target))
        }
    }
}
for r in renderers { r.setDelay(ms: opt.delayMs) }
Thread.sleep(forTimeInterval: 1.5)   // settle on the run delay before measuring

// MARK: - Measurement windows

let targetFrames = opt.delayMs / 1000 * rate
let windowsURL = outURL.appendingPathComponent("windows.jsonl")
let csvURL = outURL.appendingPathComponent("windows.csv")
FileManager.default.createFile(atPath: windowsURL.path, contents: nil)
FileManager.default.createFile(atPath: csvURL.path, contents: nil)
let jsonl = try! FileHandle(forWritingTo: windowsURL)
let csv = try! FileHandle(forWritingTo: csvURL)
var csvHeader = ["t_s"]
for i in 0..<renderers.count {
    csvHeader += ["out\(i)_lag_err_ms", "out\(i)_dropouts", "out\(i)_starved_renders", "out\(i)_corrections",
                  "out\(i)_resyncs"]
}
if renderers.count == 2 { csvHeader.append("inter_output_drift_ms") }
csvHeader += ["cpu_pct", "footprint_mb", "producer_late_ms"]
csv.write(Data((csvHeader.joined(separator: ",") + "\n").utf8))

func json(_ obj: Any) -> Data {
    (try? JSONSerialization.data(withJSONObject: obj, options: [.sortedKeys, .fragmentsAllowed])) ?? Data()
}

struct Window {
    var t = 0.0
    var lagErrMean: [Double] = []      // ms, per output
    var lagErrMin: [Double] = []
    var lagErrMax: [Double] = []
    var counters: [CoreDelayReader.Counters] = []   // deltas
    var rendered: [Int64] = []
    var dropouts: [Int64] = []         // processor overloads, per output
    var drift: Double?                 // ms, mean of output 0 lag − output 1 lag
    var driftMin: Double?
    var driftMax: Double?
    var cpu = 0.0
    var footprint = 0.0
    var producerLate = 0.0
}

func minus(_ a: CoreDelayReader.Counters, _ b: CoreDelayReader.Counters) -> CoreDelayReader.Counters {
    .init(primes: a.primes - b.primes, resyncs: a.resyncs - b.resyncs,
          driftSkips: a.driftSkips - b.driftSkips, driftSkipFrames: a.driftSkipFrames - b.driftSkipFrames,
          driftSilences: a.driftSilences - b.driftSilences,
          driftSilenceFrames: a.driftSilenceFrames - b.driftSilenceFrames,
          starvedRenders: a.starvedRenders - b.starvedRenders, starvedFrames: a.starvedFrames - b.starvedFrames)
}
func dict(_ c: CoreDelayReader.Counters) -> [String: Any] {
    ["primes": c.primes, "resyncs": c.resyncs, "drift_skips": c.driftSkips,
     "drift_skip_ms": r3(ms(Double(c.driftSkipFrames))), "drift_silences": c.driftSilences,
     "drift_silence_ms": r3(ms(Double(c.driftSilenceFrames))),
     "starved_renders": c.starvedRenders, "starved_ms": r3(ms(Double(c.starvedFrames)))]
}
var prevCounters = renderers.map(\.counters)
var prevRendered = renderers.map { $0.framesRendered.load(ordering: .relaxed) }
var prevOverloads = overloads.map { $0.count.load(ordering: .relaxed) }
var prevCPU = cpuSeconds()
let footprintStart = footprintMB()
let runStart = nowNs()
var prevWall = runStart
_ = producerLateMax.exchange(0, ordering: .relaxed)
var windows: [Window] = []

print("soak: \(opt.label), \(engineName) engine, \(Int(opt.seconds)) s, outputs: \(chosen.map(\.name).joined(separator: ", "))"
      + ", delay \(Int(opt.delayMs)) ms, all outputs at gain 0")

let samplePeriod = 0.1
while seconds(since: runStart) < opt.seconds - 1e-3 {
    let windowEnd = min(opt.seconds, Double(windows.count + 1) * opt.interval)
    var lagSum = Array(repeating: 0.0, count: renderers.count)
    var lagMin = Array(repeating: Double.infinity, count: renderers.count)
    var lagMax = Array(repeating: -Double.infinity, count: renderers.count)
    var dSum = 0.0, dMin = Double.infinity, dMax = -Double.infinity
    var n = 0
    repeat {   // at least one sample per window, even if the sampler ran late
        let lags = renderers.map { Double($0.lagFrames) }
        for i in 0..<lags.count {
            let e = lags[i] - targetFrames
            lagSum[i] += e; lagMin[i] = min(lagMin[i], e); lagMax[i] = max(lagMax[i], e)
        }
        if lags.count == 2 {
            let d = lags[0] - lags[1]
            dSum += d; dMin = min(dMin, d); dMax = max(dMax, d)
        }
        n += 1
        Thread.sleep(forTimeInterval: samplePeriod)
    } while seconds(since: runStart) < windowEnd
    let now = nowNs()
    var w = Window()
    w.t = r3(Double(now &- runStart) / 1e9)
    let wall = Double(now &- prevWall) / 1e9
    prevWall = now
    for i in 0..<renderers.count {
        w.lagErrMean.append(r3(ms(lagSum[i] / Double(max(n, 1)))))
        w.lagErrMin.append(r3(ms(lagMin[i])))
        w.lagErrMax.append(r3(ms(lagMax[i])))
        let c = renderers[i].counters
        w.counters.append(minus(c, prevCounters[i]))
        prevCounters[i] = c
        let fr = renderers[i].framesRendered.load(ordering: .relaxed)
        w.rendered.append(fr - prevRendered[i])
        prevRendered[i] = fr
        let ov = overloads[i].count.load(ordering: .relaxed)
        w.dropouts.append(ov - prevOverloads[i])
        prevOverloads[i] = ov
    }
    if renderers.count == 2 {
        w.drift = r3(ms(dSum / Double(max(n, 1))))
        w.driftMin = r3(ms(dMin))
        w.driftMax = r3(ms(dMax))
    }
    let cpu = cpuSeconds()
    w.cpu = r3((cpu - prevCPU) / max(wall, 1e-3) * 100)
    prevCPU = cpu
    w.footprint = r3(footprintMB())
    w.producerLate = r3(ms(Double(producerLateMax.exchange(0, ordering: .relaxed))))
    windows.append(w)

    // Log the window.
    var rec: [String: Any] = ["t_s": w.t, "cpu_pct": w.cpu, "footprint_mb": w.footprint, "producer_late_ms": w.producerLate]
    var outs: [[String: Any]] = []
    var row = [String(format: "%.1f", w.t)]
    for i in 0..<renderers.count {
        var o = dict(w.counters[i])
        o["name"] = chosen[i].name
        o["lag_err_ms_mean"] = w.lagErrMean[i]
        o["lag_err_ms_min"] = w.lagErrMin[i]
        o["lag_err_ms_max"] = w.lagErrMax[i]
        o["rendered_frames"] = w.rendered[i]
        o["dropouts"] = w.dropouts[i]
        outs.append(o)
        let c = w.counters[i]
        row += [String(w.lagErrMean[i]), String(w.dropouts[i]), String(c.starvedRenders),
                String(c.driftSkips + c.driftSilences),
                String(c.resyncs)]
    }
    rec["outputs"] = outs
    if let d = w.drift {
        rec["inter_output_drift_ms"] = ["mean": d, "min": w.driftMin ?? 0, "max": w.driftMax ?? 0]
        row.append(String(d))
    }
    row += [String(w.cpu), String(w.footprint), String(w.producerLate)]
    jsonl.write(json(rec) + Data("\n".utf8))
    csv.write(Data((row.joined(separator: ",") + "\n").utf8))

    var line = String(format: "[%6.0f s]", w.t)
    for i in 0..<renderers.count {
        let c = w.counters[i]
        line += String(format: " out%d lag%+.1fms d%lld s%lld c%lld", i, w.lagErrMean[i], w.dropouts[i],
                       c.starvedRenders, c.driftSkips + c.driftSilences)
    }
    if let d = w.drift { line += String(format: " drift%+.1fms", d) }
    line += String(format: " cpu%.1f%% mem%.0fMB", w.cpu, w.footprint)
    print(line)
}

// MARK: - Shut down

for r in renderers { r.halt() }
for o in overloads { o.remove() }
producing.store(false, ordering: .relaxed)
Thread.sleep(forTimeInterval: 0.3)
try? jsonl.close()
try? csv.close()

// MARK: - Summary

let duration = Double(windows.last.map { $0.t } ?? 0)
let hours = duration / 3600
func perHour(_ x: Int64) -> Double { hours > 0 ? r3(Double(x) / hours) : 0 }
func total<T>(_ f: (Window) -> T) -> [T] { windows.map(f) }

var metrics: [String: Any] = ["duration_s": r3(duration)]
var outputDetails: [[String: Any]] = []
var worstDropouts: Int64 = 0, worstStarved: Int64 = 0, worstCorrectionCount: Int64 = 0, worstLagAbs = 0.0
var sanity: [(String, Bool, String)] = []
let lagWindows = SoakRules.lagWindows(ends: windows.map(\.t), delayMs: opt.delayMs, intervalS: opt.interval)
let lagCounted = lagWindows.counted.map { windows[$0] }
for i in 0..<renderers.count {
    var c = CoreDelayReader.Counters()
    for w in windows {
        let d = w.counters[i]
        c.primes += d.primes; c.resyncs += d.resyncs
        c.driftSkips += d.driftSkips; c.driftSkipFrames += d.driftSkipFrames
        c.driftSilences += d.driftSilences; c.driftSilenceFrames += d.driftSilenceFrames
        c.starvedRenders += d.starvedRenders; c.starvedFrames += d.starvedFrames
    }
    let means = lagCounted.map { $0.lagErrMean[i] }
    let lagAbs = means.map(abs).max() ?? 0
    let rendered = total { $0.rendered[i] }.reduce(0, +)
    // Frames at the rate each output renders at, its device's (S62).
    let renderRate = renderers[i].renderRate > 0 ? renderers[i].renderRate : rate
    let renderedRatio = duration > 0 ? Double(rendered) / (duration * renderRate) : 0
    let dropouts = total { $0.dropouts[i] }.reduce(0, +)
    worstDropouts = max(worstDropouts, dropouts)
    worstStarved = max(worstStarved, c.starvedRenders)
    worstCorrectionCount = max(worstCorrectionCount, c.driftSkips + c.driftSilences)
    worstLagAbs = max(worstLagAbs, lagAbs)
    var o = dict(c)
    o["name"] = chosen[i].name
    o["transport"] = transportName(chosen[i].transport)
    o["dropouts"] = dropouts
    o["dropouts_per_h"] = perHour(dropouts)
    o["starved_renders_per_h"] = perHour(c.starvedRenders)
    o["drift_corrections_per_h"] = perHour(c.driftSkips + c.driftSilences)
    o["lag_err_ms_mean"] = r3(means.reduce(0, +) / Double(max(means.count, 1)))
    o["lag_err_ms_window_max_abs"] = r3(lagAbs)
    o["lag_err_ms_min"] = lagCounted.map { $0.lagErrMin[i] }.min() ?? 0
    o["lag_err_ms_max"] = lagCounted.map { $0.lagErrMax[i] }.max() ?? 0
    o["rendered_ratio"] = r3(renderedRatio)
    outputDetails.append(o)
    sanity.append(("\(chosen[i].name) renders in real time", renderedRatio > 0.95,
                   String(format: "%.3f of the expected frames", renderedRatio)))
}
if !renderers.isEmpty {
    metrics["local_dropouts"] = worstDropouts
    metrics["local_dropouts_per_h"] = perHour(worstDropouts)
    metrics["local_starved_renders"] = worstStarved
    metrics["local_starved_renders_per_h"] = perHour(worstStarved)
    metrics["local_drift_corrections"] = worstCorrectionCount
    metrics["local_drift_corrections_per_h"] = perHour(worstCorrectionCount)
    metrics["local_lag_err_ms_max_abs"] = r3(worstLagAbs)
}
if renderers.count == 2 {
    let d = windows.compactMap(\.drift)
    metrics["inter_output_drift_ms_mean"] = r3(d.reduce(0, +) / Double(max(d.count, 1)))
    metrics["inter_output_drift_ms_max_abs"] = d.map(abs).max() ?? 0
    metrics["inter_output_drift_ms_change"] = r3((d.last ?? 0) - (d.first ?? 0))
    metrics["inter_output_drift_ms_sample_min"] = windows.compactMap(\.driftMin).min() ?? 0
    metrics["inter_output_drift_ms_sample_max"] = windows.compactMap(\.driftMax).max() ?? 0
}
let cpus = total(\.cpu)
metrics["cpu_pct_mean"] = r3(cpus.reduce(0, +) / Double(max(cpus.count, 1)))
metrics["cpu_pct_max"] = cpus.max() ?? 0
let memList = total(\.footprint)
metrics["footprint_mb_start"] = r3(footprintStart)
metrics["footprint_mb_max"] = memList.max() ?? 0
// Growth counts from the level the warm-up ended at (the median of its last
// three windows), not from the first window, which comes before the session's
// buffers have their pages.
let memory = SoakRules.memoryGrowth(windows.map { MemorySample(t: $0.t, mb: $0.footprint) })
metrics["footprint_warmup_s"] = r3(memory.warmUpSeconds)
metrics["footprint_mb_settled"] = r3(memory.settledMB)
metrics["footprint_mb_growth"] = r3(memory.growth)
let lateMax = total(\.producerLate).max() ?? 0
metrics["producer_late_ms_max"] = lateMax
sanity.append(("harness producer kept real time", lateMax < 200, String(format: "worst lateness %.1f ms", lateMax)))
sanity.append(("run reached its duration", duration >= opt.seconds * 0.99, String(format: "%.0f s", duration)))

// MARK: - Thresholds against the baseline

struct Check { let name: String; let value: Double; let limit: Double; let pass: Bool; let rule: String }
var checks: [Check] = []
if opt.baseline != nil {
    func base(_ config: String, _ key: String) -> Double? { baseValue(config, key) }
    func mine(_ key: String) -> Double? {
        guard let v = (metrics[key] as? Double) ?? (metrics[key] as? NSNumber)?.doubleValue else {
            fail("internal: this run produced no metric \(key)")
        }
        return v
    }

    /// Event rates: allowed count = ceil(max(2 × base, base + 5) per hour × hours run).
    func rateCheck(_ name: String, count countKey: String, config: String, baseKey: String) {
        guard let count = mine(countKey), let b = base(config, baseKey) else { return }
        let limitRate = max(2 * b, b + 5)
        let allowed = (limitRate * hours).rounded(.up)
        checks.append(Check(name: name, value: count, limit: allowed, pass: count <= allowed,
                            rule: String(format: "count <= ceil(max(2*%.2f, %.2f+5)/h * %.3f h)", b, b, hours)))
    }
    /// Magnitudes: value <= base + margin.
    func marginCheck(_ name: String, key: String, config: String, margin: Double, absolute: Bool = false) {
        guard var v = mine(key), var b = base(config, key) else { return }
        if absolute { v = abs(v); b = abs(b) }
        checks.append(Check(name: name, value: v, limit: r3(b + margin), pass: v <= b + margin,
                            rule: String(format: "<= %.2f + %.0f", b, margin)))
    }
    if !renderers.isEmpty {
        rateCheck("local dropouts", count: "local_dropouts", config: "two_local", baseKey: "local_dropouts_per_h")
        rateCheck("local starved renders", count: "local_starved_renders", config: "two_local",
                  baseKey: "local_starved_renders_per_h")
        rateCheck("local drift corrections", count: "local_drift_corrections", config: "two_local",
                  baseKey: "local_drift_corrections_per_h")
        marginCheck("local lag error |ms|", key: "local_lag_err_ms_max_abs", config: "two_local", margin: 50)
    }
    if renderers.count == 2 {
        marginCheck("inter-output drift |ms|", key: "inter_output_drift_ms_max_abs", config: "two_local", margin: 50)
    }
    if let v = mine("footprint_mb_growth"), let b = base("two_local", "footprint_mb_growth") {
        let limit = SoakRules.memoryGrowthLimit(baseline: b)
        checks.append(Check(name: "memory growth MB", value: v, limit: r3(limit), pass: v <= limit,
                            rule: String(format: "last window minus the median of the last 3 windows of the first %.0f s, "
                                            + "<= max(2*base, base+5)", memory.warmUpSeconds)))
    }
    if let v = mine("cpu_pct_mean"), let b = base("two_local", "cpu_pct_mean") {
        let limit = max(2 * b, b + 5)
        checks.append(Check(name: "CPU % mean", value: v, limit: r3(limit), pass: v <= limit,
                            rule: "<= max(2*base, base+5)"))
    }
}

let sanityOK = sanity.allSatisfy { $0.1 }
let checksOK = checks.allSatisfy(\.pass)
let verdict: String
if !sanityOK || !checksOK { verdict = "FAIL" } else if opt.baseline == nil { verdict = "NO BASELINE" } else { verdict = "PASS" }

let result: [String: Any] = [
    "label": opt.label,
    "engine": engineName,
    "config": ["outputs": chosen.map { ["name": $0.name, "transport": transportName($0.transport)] },
               "delay_ms": opt.delayMs, "seconds": opt.seconds, "interval_s": opt.interval],
    "metrics": metrics,
    "outputs": outputDetails,
    "delay_accuracy": delayAccuracy,
    "sanity": sanity.map { ["name": $0.0, "pass": $0.1, "detail": $0.2] },
    "checks": checks.map { ["name": $0.name, "value": $0.value, "limit": $0.limit, "pass": $0.pass, "rule": $0.rule] },
    "baseline": baselineNote,
    "warnings": warnings,
    "machine": ["model_identifier": modelIdentifier, "macos": macOSVersion],
    "verdict": verdict,
]
try? json(result).write(to: outURL.appendingPathComponent("result.json"))

var text = "AudioSync soak harness: \(opt.label), \(engineName) engine\n"
text += "Outputs (gain 0): \(chosen.map { "\($0.name) [\(transportName($0.transport))]" }.joined(separator: ", "))\n"
text += String(format: "Duration %.0f s, delay %.0f ms, %d windows of %.0f s\n\n", duration, opt.delayMs, windows.count, opt.interval)
for row in delayAccuracy {
    text += String(format: "delay %@ %4.0f ms: error %+.1f ms\n", row["output"] as? String ?? "",
                   row["target_ms"] as? Double ?? 0, row["error_ms"] as? Double ?? 0)
}
if !delayAccuracy.isEmpty { text += "\n" }
for o in outputDetails {
    text += "\(o["name"] ?? ""): dropouts \(o["dropouts"] ?? 0) (\(o["dropouts_per_h"] ?? 0)/h), "
        + "starved renders \(o["starved_renders"] ?? 0), "
        + "drift corrections \((o["drift_skips"] as? Int64 ?? 0) + (o["drift_silences"] as? Int64 ?? 0)) "
        + "(\(o["drift_corrections_per_h"] ?? 0)/h), resyncs \(o["resyncs"] ?? 0), "
        + "lag error mean \(o["lag_err_ms_mean"] ?? 0) ms (window max |\(o["lag_err_ms_window_max_abs"] ?? 0)| ms)\n"
}
if renderers.count == 2 {
    text += "Inter-output drift: mean \(metrics["inter_output_drift_ms_mean"] ?? 0) ms, "
        + "max |\(metrics["inter_output_drift_ms_max_abs"] ?? 0)| ms, change \(metrics["inter_output_drift_ms_change"] ?? 0) ms\n"
}
text += "Process: CPU mean \(metrics["cpu_pct_mean"] ?? 0)%, memory max \(metrics["footprint_mb_max"] ?? 0) MB, "
    + "growth \(metrics["footprint_mb_growth"] ?? 0) MB from \(metrics["footprint_mb_settled"] ?? 0) MB "
    + "after a \(Int(memory.warmUpSeconds)) s warm-up\n\n"
for s in sanity { text += (s.1 ? "ok    " : "FAIL  ") + s.0 + " (" + s.2 + ")\n" }
for c in checks {
    text += (c.pass ? "PASS  " : "FAIL  ") + c.name + String(format: ": %.3f, limit %.3f", c.value, c.limit)
        + " (" + c.rule + ")\n"
}
for w in warnings { text += "WARNING: \(w)\n" }
text += "\n\(baselineNote)\nVERDICT: \(verdict)\n"
try? text.write(to: outURL.appendingPathComponent("summary.txt"), atomically: true, encoding: .utf8)
print("\n" + text)
exit(verdict == "FAIL" ? 1 : verdict == "NO BASELINE" ? 3 : 0)
