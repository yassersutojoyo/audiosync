// Logic tests for AudioSync that need no audio hardware and no permissions.
// Run all tests with: Tests/run-tests.sh
import Foundation
import AppKit
import CAudioSyncCore
import CoreAudio
import CoreServices
import ImageIO
import Network
import dnssd

var failedChecks = 0
func check(_ ok: Bool, _ name: String, _ detail: @autoclosure () -> String = "") {
    let d = detail()
    print((ok ? "✅ PASS  " : "❌ FAIL  ") + name + (d.isEmpty ? "" : "  — " + d))
    if !ok { failedChecks += 1 }
}
func approx(_ a: Double, _ b: Double, _ tol: Double) -> Bool { abs(a - b) <= tol }

// MARK: Volume curve
check(VolumeCurve.gain(position: 0) == 0, "volume: 0 is silent")
check(VolumeCurve.gain(position: 100) == 1, "volume: 100 is unity")
let halfDB = 20 * log10(VolumeCurve.gain(position: 50))
check(approx(halfDB, -18.06, 0.1), "volume: 50 ≈ −18 dB", String(format: "%.2f dB", halfDB))
let tenDB = 20 * log10(VolumeCurve.gain(position: 10))
check(approx(tenDB, -60, 0.1), "volume: 10 ≈ −60 dB", String(format: "%.2f dB", tenDB))
check(approx(VolumeCurve.position(gain: VolumeCurve.gain(position: 37)), 37, 1e-9), "volume: position↔gain round-trip")
check(approx(VolumeCurve.position(gain: 0.0349), 32.7, 0.1), "volume: legacy 3.49% migrates to ≈33")
var mono = true
for p in stride(from: 0.0, to: 100, by: 1) where VolumeCurve.gain(position: p + 1) <= VolumeCurve.gain(position: p) { mono = false }
check(mono, "volume: strictly increasing")

// MARK: Audio source persistence
for src in [AudioSource.system, .app(bundleID: "com.spotify.client"), .input(uid: "BuiltInMicrophoneDevice")] {
    check(AudioSource(defaultsValue: src.defaultsValue) == src, "source: round-trip \(src)")
}
check(AudioSource(defaultsValue: nil) == .system, "source: missing value → system")
check(AudioSource(defaultsValue: "com.apple.Music").bundleID == "com.apple.Music", "source: legacy bundle-ID value → app")

check(AudioSource.input(uid: "x").bundleID == nil, "source: input has no bundle ID")
check(AudioSource(defaultsValue: "") == .system && AudioSource.system.defaultsValue == "",
      "source: the whole system is stored as an empty string")
check(AudioSource(defaultsValue: "input:BuiltInMicrophoneDevice") == .input(uid: "BuiltInMicrophoneDevice")
        && AudioSource.input(uid: "BuiltInMicrophoneDevice").defaultsValue == "input:BuiltInMicrophoneDevice",
      "source: an input device is stored as its UID after an input: tag")
do {
    let usbUID = "AppleUSBAudioEngine:Vendor:Mic:1234:1"
    check(AudioSource(defaultsValue: AudioSource.input(uid: usbUID).defaultsValue) == .input(uid: usbUID),
          "source: an input UID with colons in it comes back whole")
}
check(AudioSource.app(bundleID: "com.spotify.client").defaultsValue == "com.spotify.client"
        && AudioSource(defaultsValue: "com.spotify.client") == .app(bundleID: "com.spotify.client"),
      "source: an app is stored as its plain bundle ID, as earlier versions stored it")
check(AudioSource.system.bundleID == nil, "source: the whole system has no bundle ID")

// MARK: Source apps — display names and the picker's app list
check(SourceApps.displayName(running: "Music", installed: "Apple Music", bundleID: "com.apple.Music") == "Music",
      "source apps: a running app's name comes first")
check(SourceApps.displayName(running: nil, installed: "Music", bundleID: "com.apple.Music") == "Music",
      "source apps: an app that isn't running shows its installed name")
check(SourceApps.displayName(running: nil, installed: nil, bundleID: "com.example.gone") == "com.example.gone",
      "source apps: an app that is neither running nor installed shows its bundle ID")
check(SourceApps.displayName(running: "  ", installed: nil, bundleID: "com.example.app") == "com.example.app"
        && SourceApps.displayName(running: "", installed: " Music ", bundleID: "com.apple.Music") == "Music",
      "source apps: a blank name doesn't count and names are trimmed")
check(SourceApps.appName(fromFinderName: "Safari.app") == "Safari"
        && SourceApps.appName(fromFinderName: "Safari") == "Safari"
        && SourceApps.appName(fromFinderName: "Tool.APP") == "Tool"
        && SourceApps.appName(fromFinderName: ".app") == ".app",
      "source apps: Finder's .app extension is dropped from an installed app's name")
do {
    func app(_ pid: pid_t, _ id: String?, _ name: String?, regular: Bool = true) -> SourceApps.RunningApp {
        SourceApps.RunningApp(pid: pid, bundleID: id, name: name, isRegular: regular, icon: nil)
    }
    let ownPID: pid_t = 50
    let apps = [app(10, "com.spotify.client", "Spotify"), app(11, "com.apple.Music", "Music"),
                app(12, "com.apple.WebKit.GPU", "Safari Graphics", regular: false),
                app(13, "com.example.audiosync", "AudioSync"), app(ownPID, nil, "AudioSync (unbundled)"),
                app(14, nil, "No Bundle"), app(15, "", "Empty ID"),
                app(16, "com.apple.Music", "Music (second copy)"), app(17, "com.example.zed", "zed"),
                app(18, "com.example.blank", "  ")]
    let offered = SourceApps.offered(apps, ownPID: ownPID, ownBundleID: "com.example.audiosync")
    check(offered.map(\.bundleID) == ["com.example.blank", "com.apple.Music", "com.spotify.client", "com.example.zed"],
          "source apps: regular apps with a bundle ID, AudioSync left out, once each, sorted by name",
          "\(offered.map(\.bundleID))")
    check(offered.map(\.name) == ["com.example.blank", "Music", "Spotify", "zed"],
          "source apps: the first instance's name, a blank name replaced by the bundle ID",
          "\(offered.map(\.name))")
    let twins = SourceApps.offered([app(20, "com.b", "Same"), app(21, "com.a", "same")], ownPID: ownPID, ownBundleID: nil)
    check(twins.map(\.bundleID) == ["com.a", "com.b"], "source apps: names that sort alike are ordered by bundle ID")
}

// MARK: Audio processes — which Core Audio process objects make up an app
do {
    typealias Listed = AudioProcesses.Listed
    let appPID: pid_t = 100, ownPID: pid_t = 900
    let helpers: [pid_t: pid_t] = [200: 100, 300: 400, 900: 100]   // helper → responsible process
    let responsible: (pid_t) -> pid_t? = { helpers[$0] }
    let list = [Listed(object: 31, pid: 300), Listed(object: 11, pid: 100), Listed(object: 21, pid: 200),
                Listed(object: 91, pid: 900), Listed(object: 41, pid: 400)]
    check(AudioProcesses.matching(list, appPID: appPID, ownPID: ownPID, responsiblePID: responsible) == [11, 21],
          "processes: the app's own process and its helpers, in Core Audio's order")
    check(AudioProcesses.matching(list, appPID: 400, ownPID: ownPID, responsiblePID: responsible) == [31, 41],
          "processes: another app's helper belongs to that app only")
    check(AudioProcesses.matching(list, appPID: appPID, ownPID: ownPID, responsiblePID: { _ in nil }) == [11],
          "processes: without the responsibility lookup only the app's own process counts")
    check(!AudioProcesses.matching(list, appPID: appPID, ownPID: ownPID, responsiblePID: responsible).contains(91),
          "processes: AudioSync's own process never counts, even when the source app is responsible for it")
    let odd = [Listed(object: AudioObjectID(kAudioObjectUnknown), pid: 100), Listed(object: 12, pid: 0),
               Listed(object: 13, pid: -1), Listed(object: 11, pid: 100), Listed(object: 11, pid: 100)]
    check(AudioProcesses.matching(odd, appPID: appPID, ownPID: ownPID, responsiblePID: { _ in 100 }) == [11],
          "processes: unknown objects and unreadable PIDs never count; an object counts once")
    check(AudioProcesses.matching(list, appPID: 0, ownPID: ownPID, responsiblePID: { _ in 0 }).isEmpty
            && AudioProcesses.objects(forAppPID: -1).isEmpty && AudioProcesses.processObject(for: 0) == nil,
          "processes: no PID, no process objects")
    check(AudioProcesses.matching([], appPID: appPID, ownPID: ownPID, responsiblePID: responsible).isEmpty,
          "processes: an app with no audio client yet has no process objects")
}

// MARK: Appearance — stored choice
for a in AppAppearance.allCases {
    check(AppAppearance(defaultsValue: a.defaultsValue) == a, "appearance: round-trip \(a)")
}
check(AppAppearance(defaultsValue: nil) == .system, "appearance: missing value → match system")
check(AppAppearance(defaultsValue: "sepia") == .system, "appearance: unknown value → match system")
check(AppAppearance.system.appearanceName == nil, "appearance: system leaves NSApp.appearance unset")
check(AppAppearance.light.appearanceName == .aqua && AppAppearance.dark.appearanceName == .darkAqua,
      "appearance: light → aqua, dark → darkAqua")

// MARK: Speaker keys
check(SpeakerKey.make(id: "123", airplay: true) == "ap:123", "speaker key: AirPlay prefix")
check(SpeakerKey.parse("lc:58-18:output").map { $0.id == "58-18:output" && !$0.airplay } == true, "speaker key: local parse keeps colons")
check(SpeakerKey.parse("zz:1") == nil, "speaker key: unknown prefix rejected")
let g = SpeakerGroup(id: "g", name: "Room", members: ["ap:1", "lc:2"])
let decoded = try! JSONDecoder().decode(SpeakerGroup.self, from: try! JSONEncoder().encode(g))
check(decoded == g, "groups: JSON round-trip")

// MARK: Group names — a blank name never becomes a group's name
do {
    let numbered = { (n: Int) in "Group \(n)" }
    check(GroupName.cleaned("  Living room \n") == "Living room", "group name: trimmed")
    check(GroupName.cleaned("   ") == nil && GroupName.cleaned("") == nil && GroupName.cleaned("\t\n") == nil,
          "group name: blank or whitespace-only is no name")
    check(GroupName.forNewGroup(typed: " Office ", existing: [], numbered: numbered) == "Office",
          "group name: a typed name is kept")
    check(GroupName.forNewGroup(typed: "", existing: [], numbered: numbered) == "Group 1",
          "group name: blank first group is Group 1")
    check(GroupName.forNewGroup(typed: "  ", existing: ["Office"], numbered: numbered) == "Group 2",
          "group name: blank second group is Group 2")
    check(GroupName.forNewGroup(typed: "", existing: ["Office", "Group 3"], numbered: numbered) == "Group 4",
          "group name: the default skips names in use")
    check(GroupName.afterRename(typed: "  Kitchen ", current: "Office") == "Kitchen", "group rename: trimmed")
    check(GroupName.afterRename(typed: "   ", current: "Office") == "Office", "group rename: blank keeps the name")
}

// MARK: Speaker IDs — the bundled engine's output IDs move to the native sender's receiver IDs (S204)
do {
    typealias Migration = SpeakerIDMigration
    // Device ID AA:BB:CC:DD:EE:01: the bundled engine's output 187723572702721
    // (the device ID as a number), the native sender's receiver AABBCCDDEE01.
    let tvOld = "187723572702721", tvNew = "native:AABBCCDDEE01"
    // Device ID 02:00:5E:00:53:02: output 2200600335106, receiver 02005E005302.
    let denOld = "2200600335106", denNew = "native:02005E005302"
    // Device ID F0:B3:EC:12:34:56, which discovery doesn't list this run.
    let gone = "264655550428246"
    let saved = Migration.Saved(
        enabledAirPlay: [tvOld, denOld, gone, "native:0A0B0C0D0E0F"],
        airplayVolumes: [tvOld: 35, denOld: 80, gone: 20, denNew: 55],
        groups: [SpeakerGroup(id: "g1", name: "Living Room", members: ["lc:BuiltInSpeakerDevice", "ap:\(tvOld)", "ap:\(gone)"]),
                 SpeakerGroup(id: "g2", name: "Den", members: ["ap:\(denNew)", "ap:\(denOld)", "lc:usb:out"])],
        autoConnect: ["ap:\(tvOld)", "lc:BuiltInSpeakerDevice", "ap:\(gone)"])
    let receivers = ["AABBCCDDEE01", "02005E005302", "raop:Kitchen", "0A0B0C0D0E0F"]

    check(Migration.renames(saved, receivers: receivers) == [tvOld: tvNew, denOld: denNew],
          "speaker IDs: a saved bundled-engine ID moves to the receiver whose device ID it is",
          "\(Migration.renames(saved, receivers: receivers))")
    let migrated = Migration.migrate(saved, receivers: receivers)
    check(migrated.enabledAirPlay == [tvNew, denNew, gone, "native:0A0B0C0D0E0F"],
          "speaker IDs: a speaker that was on stays on under its new ID; one not discovered keeps its old ID",
          "\(migrated.enabledAirPlay.sorted())")
    check(migrated.airplayVolumes == [tvNew: 35, denNew: 55, gone: 20],
          "speaker IDs: the volume moves along, and a volume already saved under the new ID wins",
          "\(migrated.airplayVolumes)")
    check(migrated.groups == [
            SpeakerGroup(id: "g1", name: "Living Room", members: ["lc:BuiltInSpeakerDevice", "ap:\(tvNew)", "ap:\(gone)"]),
            SpeakerGroup(id: "g2", name: "Den", members: ["ap:\(denNew)", "lc:usb:out"])],
          "speaker IDs: a group keeps its name, its order, its local speakers and the ones not discovered, "
            + "and never lists a speaker twice", "\(migrated.groups)")
    check(migrated.autoConnect == ["ap:\(tvNew)", "lc:BuiltInSpeakerDevice", "ap:\(gone)"],
          "speaker IDs: auto-connect follows the new ID", "\(migrated.autoConnect.sorted())")
    check(Migration.renames(migrated, receivers: receivers).isEmpty && Migration.migrate(migrated, receivers: receivers) == migrated,
          "speaker IDs: once moved, a second run changes nothing")
    check(Migration.migrate(saved, receivers: []) == saved
            && Migration.migrate(saved, receivers: ["raop:Kitchen", "airplay:Den TV", "AABBCCDDEE0", "AABBCCDDEE012",
                                                    "GABBCCDDEE01", "native:AABBCCDDEE01"]) == saved,
          "speaker IDs: nothing moves before discovery lists the receiver, and a receiver without a device ID matches nothing")
    // Only an ID as the bundled engine writes it (decimal digits, no sign,
    // space or leading zero, 1 to 2^48 - 1) and only as an AirPlay ID or key.
    let odd = Migration.Saved(
        enabledAirPlay: ["", "0", "+\(tvOld)", " \(tvOld)", "\(tvOld) ", "0\(tvOld)", "-1", "0xAABBCCDDEE01", "AABBCCDDEE01",
                         "281474976710656", "99999999999999999999", "1e5", "native:\(tvOld)"],
        airplayVolumes: ["0": 10, "0\(tvOld)": 40],
        groups: [SpeakerGroup(id: "g", name: "Odd", members: ["zz:\(tvOld)", "lc:\(tvOld)", tvOld, "ap:0\(tvOld)"])],
        autoConnect: ["lc:\(tvOld)", "zz:\(tvOld)", "ap:native:\(tvOld)"])
    check(Migration.migrate(odd, receivers: receivers + ["000000000000", "FFFFFFFFFFFF"]) == odd,
          "speaker IDs: any other ID or key is left as it is")
    let edge = Migration.Saved(enabledAirPlay: ["281474976710655", "1"], airplayVolumes: [:], groups: [], autoConnect: [])
    check(Migration.migrate(edge, receivers: ["FFFFFFFFFFFF", "000000000001"]).enabledAirPlay
            == ["native:FFFFFFFFFFFF", "native:000000000001"],
          "speaker IDs: the largest and smallest device IDs move too")
}

// MARK: Output devices — virtual ones hidden unless asked for or chosen
do {
    func dev(_ uid: String, _ transport: UInt32) -> AudioDevices.Device {
        AudioDevices.Device(id: 0, name: uid, uid: uid, transport: transport)
    }
    let all = [dev("builtin", kAudioDeviceTransportTypeBuiltIn), dev("loopback", kAudioDeviceTransportTypeVirtual),
               dev("usb", kAudioDeviceTransportTypeUSB), dev("multi", kAudioDeviceTransportTypeAggregate),
               dev("auto", kAudioDeviceTransportTypeAutoAggregate), dev("meeting", kAudioDeviceTransportTypeVirtual)]
    let uids = { (d: [AudioDevices.Device]) in d.map(\.uid) }
    check(uids(AudioDevices.listed(all, showVirtual: false, keep: [])) == ["builtin", "usb"],
          "devices: virtual and aggregate hidden by default")
    check(uids(AudioDevices.listed(all, showVirtual: false, keep: ["meeting", "usb"])) == ["builtin", "usb", "meeting"],
          "devices: a chosen virtual device stays listed, in order")
    check(uids(AudioDevices.listed(all, showVirtual: true, keep: [])) == uids(all),
          "devices: Show Virtual Devices lists everything")
    check(!AudioDevices.isVirtual(transport: kAudioDeviceTransportTypeBluetooth)
          && !AudioDevices.isVirtual(transport: kAudioDeviceTransportTypeHDMI)
          && !AudioDevices.isVirtual(transport: 0), "devices: hardware and unknown transports are never hidden")
    let hardware: [UInt32] = [kAudioDeviceTransportTypeBuiltIn, kAudioDeviceTransportTypeUSB,
                              kAudioDeviceTransportTypeBluetooth, kAudioDeviceTransportTypeBluetoothLE,
                              kAudioDeviceTransportTypeHDMI, kAudioDeviceTransportTypeDisplayPort,
                              kAudioDeviceTransportTypeThunderbolt, kAudioDeviceTransportTypePCI,
                              kAudioDeviceTransportTypeFireWire, kAudioDeviceTransportTypeAVB]
    check(hardware.allSatisfy { !AudioDevices.isVirtual(transport: $0) },
          "devices: every hardware transport is listed by default")
    let withAirPlay = [dev("builtin", kAudioDeviceTransportTypeBuiltIn), dev("tv", kAudioDeviceTransportTypeAirPlay),
                       dev("usb", kAudioDeviceTransportTypeUSB)]
    check(uids(AudioDevices.listed(withAirPlay, showVirtual: false, keep: ["tv"])) == ["builtin", "tv", "usb"],
          "devices: macOS's own AirPlay output stays listed, in order, once chosen")
    check(uids(AudioDevices.listed(all, showVirtual: false, keep: ["gone"])) == ["builtin", "usb"],
          "devices: a chosen device that isn't present adds nothing")
    check(AudioDevices.isPrivateAggregate(composition: [kAudioAggregateDeviceIsPrivateKey: NSNumber(value: 1)])
          && !AudioDevices.isPrivateAggregate(composition: [kAudioAggregateDeviceIsPrivateKey: NSNumber(value: 0)])
          && !AudioDevices.isPrivateAggregate(composition: [kAudioAggregateDeviceNameKey: "Multi-Output Device"]),
          "devices: only a private aggregate (the capture's own) is left out of the lists")

    let usb = kAudioDeviceTransportTypeUSB
    let aggregate = kAudioDeviceTransportTypeAggregate
    let listedUSB = AudioDevices.entry(id: 7, alive: 1, channels: 2, transport: usb, composition: nil,
                                       uid: "usb-uid", name: "USB DAC")
    check(listedUSB == AudioDevices.Device(id: 7, name: "USB DAC", uid: "usb-uid", transport: usb)
          && AudioDevices.entry(id: 7, alive: 0, channels: 2, transport: usb, composition: nil,
                                uid: "usb-uid", name: "USB DAC") == nil,
          "devices: a device that reports it is going away is left out")
    check(AudioDevices.entry(id: 7, alive: nil, channels: 2, transport: usb, composition: nil,
                             uid: "usb-uid", name: "USB DAC") == listedUSB,
          "devices: a device that doesn't say whether it is alive stays listed")
    check(AudioDevices.entry(id: 7, alive: 1, channels: 0, transport: usb, composition: nil,
                             uid: "usb-uid", name: "USB DAC") == nil,
          "devices: a device with no channels in the listed direction is left out")
    let ownTap: [String: Any] = [kAudioAggregateDeviceIsPrivateKey: NSNumber(value: 1)]
    let userMade: [String: Any] = [kAudioAggregateDeviceNameKey: "Multi-Output Device"]
    check(AudioDevices.entry(id: 8, alive: 1, channels: 2, transport: aggregate, composition: ownTap,
                             uid: "tap", name: "Tap") == nil
          && AudioDevices.entry(id: 8, alive: 1, channels: 2, transport: aggregate, composition: userMade,
                                uid: "multi", name: "Multi")?.uid == "multi"
          && AudioDevices.entry(id: 8, alive: 1, channels: 2, transport: aggregate, composition: nil,
                                uid: "multi", name: "Multi")?.uid == "multi",
          "devices: a private aggregate is left out, an aggregate the user made stays")
    let unnamed = AudioDevices.entry(id: 9, alive: 1, channels: 2, transport: usb, composition: nil,
                                     uid: "usb-uid", name: nil)
    check(unnamed?.name == "" && unnamed?.uid == "usb-uid",
          "devices: a device whose name can't be read keeps an empty name, never its UID")
    let unread = AudioDevices.entry(id: 9, alive: 1, channels: 2, transport: nil, composition: nil,
                                    uid: nil, name: nil)
    check(unread == AudioDevices.Device(id: 9, name: "", uid: "", transport: 0),
          "devices: an unreadable UID or transport stays empty or unknown")
}

// MARK: Debug log — opt-in, one UTC line per message, capped, never through a link
do {
    check(DebugTrace.isEnabled(environment: ["AUDIOSYNC_DEBUG": "1"]), "debug log: on with AUDIOSYNC_DEBUG=1")
    check(!DebugTrace.isEnabled(environment: [:]) && !DebugTrace.isEnabled(environment: ["AUDIOSYNC_DEBUG": "0"])
          && !DebugTrace.isEnabled(environment: ["AUDIOSYNC_DEBUG": ""])
          && !DebugTrace.isEnabled(environment: ["AUDIOSYNC_DEBUG": "true"]),
          "debug log: off unless the variable is exactly 1")
    if !DebugTrace.enabled {
        var built = false
        DebugTrace.log({ () -> String in built = true; return "never" }())
        check(!built, "debug log: a message is never built while the log is off")
    }
    let t = Date(timeIntervalSince1970: 1_792_345_933.25)   // 2026-10-18 17:52:13.250 UTC
    check(DebugLogFile.line("[AudioSync] hello", at: t) == "2026-10-18T17:52:13.250Z [AudioSync] hello\n",
          "debug log: each line starts with the UTC time to the millisecond")
    check(DebugLogFile.timestamp(Date(timeIntervalSince1970: 59.9999)) == "1970-01-01T00:00:59.999Z",
          "debug log: times round down, never into the next second")

    let fm = FileManager.default
    let dir = fm.temporaryDirectory.appendingPathComponent("audiosync-logtest-\(getpid())")
    try? fm.removeItem(at: dir)
    try? fm.createDirectory(at: dir, withIntermediateDirectories: true)
    func contents(_ path: String) -> String { (try? String(contentsOfFile: path, encoding: .utf8)) ?? "<unreadable>" }

    let path = dir.appendingPathComponent("debug.log").path
    try? "previous launch\n".write(toFile: path, atomically: false, encoding: .utf8)
    let file = DebugLogFile(path: path, limit: 100, clock: { t })
    file.empty()
    file.append("one")
    file.append("two")
    let firstTwo = DebugLogFile.line("one", at: t) + DebugLogFile.line("two", at: t)
    check(contents(path) == firstTwo, "debug log: a launch starts empty, then lines append in order")
    file.append(String(repeating: "x", count: 30))   // 58 + 56 bytes would pass the 100-byte limit
    file.append("later")
    let capped = contents(path)
    check(capped.hasPrefix(firstTwo) && capped.hasSuffix("nothing more is written until the next launch\n")
          && !capped.contains("xxx") && !capped.contains("later"),
          "debug log: past the limit one line says it stopped, and nothing more is written")
    file.empty()
    file.append("again")
    check(contents(path) == DebugLogFile.line("again", at: t), "debug log: emptying starts the limit over")

    let kept = dir.appendingPathComponent("kept.log").path
    try? "earlier\n".write(toFile: kept, atomically: false, encoding: .utf8)
    DebugLogFile(path: kept, clock: { t }).append("next")
    check(contents(kept) == "earlier\n" + DebugLogFile.line("next", at: t),
          "debug log: without a reset, lines are appended to what is there")

    let fresh = dir.appendingPathComponent("fresh.log").path
    DebugLogFile(path: fresh, clock: { t }).append("first")
    let mode = try? fm.attributesOfItem(atPath: fresh)[.posixPermissions] as? Int
    check(mode == 0o600, "debug log: a new log is readable by its owner only", "mode \(mode.map { String($0, radix: 8) } ?? "?")")

    let target = dir.appendingPathComponent("target.txt").path
    let link = dir.appendingPathComponent("link.log").path
    try? "keep\n".write(toFile: target, atomically: false, encoding: .utf8)
    try? fm.createSymbolicLink(atPath: link, withDestinationPath: target)
    let linked = DebugLogFile(path: link, clock: { t })
    linked.empty()
    linked.append("redirected")
    check(contents(target) == "keep\n", "debug log: a symbolic link at the log's path is never followed")

    let original = dir.appendingPathComponent("original.txt").path
    let hard = dir.appendingPathComponent("hard.log").path
    try? "keep\n".write(toFile: original, atomically: false, encoding: .utf8)
    try? fm.linkItem(atPath: original, toPath: hard)
    let hardLinked = DebugLogFile(path: hard, clock: { t })
    hardLinked.empty()
    hardLinked.append("redirected")
    check(contents(original) == "keep\n", "debug log: a hard link at the log's path is never emptied or written")

    let fifo = dir.appendingPathComponent("fifo.log").path
    _ = mkfifo(fifo, 0o600)
    let piped = DebugLogFile(path: fifo, clock: { t })
    let pipeDone = DispatchSemaphore(value: 0)
    DispatchQueue.global().async {
        piped.empty()   // without a reader, a blocking open would never return
        piped.append("x")
        pipeDone.signal()
    }
    check(pipeDone.wait(timeout: .now() + 5) == .success, "debug log: a FIFO at the log's path never blocks it")

    let older = dir.appendingPathComponent("older.log").path
    try? "old\n".write(toFile: older, atomically: false, encoding: .utf8)
    try? fm.setAttributes([.posixPermissions: 0o644], ofItemAtPath: older)
    DebugLogFile(path: older, clock: { t }).empty()
    let olderMode = try? fm.attributesOfItem(atPath: older)[.posixPermissions] as? Int
    check(olderMode == 0o600 && contents(older).isEmpty,
          "debug log: an older log readable by everyone is emptied and made owner-only",
          "mode \(olderMode.map { String($0, radix: 8) } ?? "?")")

    let queued = dir.appendingPathComponent("queued.log").path
    let gate = DispatchQueue(label: "logtest.debug-log")
    var ticks = 0.0
    let sink = DebugLog(file: DebugLogFile(path: queued), queue: gate,
                        clock: { ticks += 1; return t.addingTimeInterval(ticks) })
    gate.suspend()
    sink.reset(firstLine: "first")
    sink.log("second")
    check(!fm.fileExists(atPath: queued), "debug log: the thread that logs never touches the file")
    gate.resume()
    sink.flush()
    check(contents(queued) == DebugLogFile.line("first", at: t.addingTimeInterval(1))
          + DebugLogFile.line("second", at: t.addingTimeInterval(2)),
          "debug log: queued lines land in order, stamped when they were logged")

    let busy = dir.appendingPathComponent("busy.log").path
    let shared = DebugLogFile(path: busy)
    shared.empty()
    DispatchQueue.concurrentPerform(iterations: 8) { worker in
        for n in 0..<200 { shared.append("[AudioSync] worker \(worker) line \(n)") }
    }
    let lines = contents(busy).split(separator: "\n", omittingEmptySubsequences: false)
    let whole = lines.dropLast().allSatisfy { $0.count > 25 && $0.dropFirst(24).hasPrefix(" [AudioSync] worker ") }
    check(lines.count == 8 * 200 + 1 && lines.last == "" && whole,
          "debug log: lines from many threads never interleave", "\(lines.count - 1) lines")
    try? fm.removeItem(at: dir)
}

// MARK: Speaker layout — This Mac, then connected speakers by name with a tag
do {
    check(SpeakerTag(transport: kAudioDeviceTransportTypeBuiltIn) == .builtIn
          && SpeakerTag(transport: kAudioDeviceTransportTypeBluetooth) == .bluetooth
          && SpeakerTag(transport: kAudioDeviceTransportTypeBluetoothLE) == .bluetooth
          && SpeakerTag(transport: kAudioDeviceTransportTypeUSB) == .usb
          && SpeakerTag(transport: kAudioDeviceTransportTypeHDMI) == .hdmi
          && SpeakerTag(transport: kAudioDeviceTransportTypeDisplayPort) == .displayPort
          && SpeakerTag(transport: kAudioDeviceTransportTypeThunderbolt) == .thunderbolt
          && SpeakerTag(transport: kAudioDeviceTransportTypeAirPlay) == .systemAirPlay
          && SpeakerTag(transport: kAudioDeviceTransportTypeFireWire) == .firewire
          && SpeakerTag(transport: kAudioDeviceTransportTypeVirtual) == .virtual
          && SpeakerTag(transport: kAudioDeviceTransportTypeAggregate) == .aggregate
          && SpeakerTag(transport: kAudioDeviceTransportTypeAutoAggregate) == .aggregate
          && SpeakerTag(transport: kAudioDeviceTransportTypePCI) == .pci
          && SpeakerTag(transport: kAudioDeviceTransportTypeAVB) == .avb,
          "layout: transport types map to their tag")
    check(SpeakerTag(transport: 0) == .other, "layout: an unknown transport is Other")
    check(SpeakerTag.builtIn.isThisMac && !SpeakerTag.usb.isThisMac && !SpeakerTag.airplay.isThisMac
          && !SpeakerTag.virtual.isThisMac, "layout: only built-in outputs are This Mac")

    let kinds: [SpeakerTag] = [.airplay, .systemAirPlay, .builtIn, .bluetooth, .usb, .hdmi, .displayPort,
                               .thunderbolt, .firewire, .pci, .avb, .virtual, .aggregate, .other]
    let missing = kinds.filter { NSImage(systemSymbolName: $0.typeSymbol, accessibilityDescription: nil) == nil }
    check(missing.isEmpty, "layout: every type icon is an existing SF Symbol", "\(missing)")
    check(!kinds.contains { $0.typeSymbol.hasPrefix("bolt") },
          "layout: no type icon is a bolt (that marks auto-connect)")
    check(kinds.allSatisfy { $0.typeTint(.off) == .muted && $0.typeTint(.willPlay) == .muted },
          "layout: type icons are grey unless the speaker plays")
    check(SpeakerTag.airplay.typeTint(.playing) == .active && SpeakerTag.bluetooth.typeTint(.playing) == .active
          && SpeakerTag.systemAirPlay.typeTint(.playing) == .active && SpeakerTag.usb.typeTint(.playing) == .teal
          && SpeakerTag.virtual.typeTint(.playing) == .teal,
          "layout: a playing speaker's type icon takes its own color")

    typealias S = (name: String, tag: SpeakerTag)
    let local: [S] = [("MacBook Pro Speakers", .builtIn), ("studio Headphones", .bluetooth),
                      ("Desk Speakers", .usb), ("External Headphones", .builtIn), ("kitchen", .usb)]
    let airplay: [S] = [("Living Room", .airplay), ("Kitchen", .airplay), ("Bedroom", .airplay),
                        ("Speaker 10", .airplay), ("Speaker 9", .airplay)]
    let split = SpeakerLayout.split(local: local, airplay: airplay, tag: { $0.tag }, name: { $0.name })
    check(split.thisMac.map(\.name) == ["MacBook Pro Speakers", "External Headphones"],
          "layout: This Mac keeps Core Audio's order")
    check(split.connected.map(\.name) == ["Bedroom", "Desk Speakers", "kitchen", "Kitchen", "Living Room",
                                          "Speaker 9", "Speaker 10", "studio Headphones"],
          "layout: connected speakers sort by name, ignoring case, numbers in order, ties stable",
          split.connected.map(\.name).joined(separator: ", "))
    check(split.connected.first { $0.name == "Kitchen" }?.tag == .airplay
          && split.connected.first { $0.name == "kitchen" }?.tag == .usb,
          "layout: AirPlay and local speakers share one list")
    let none = SpeakerLayout.split(local: [S](), airplay: [S](), tag: { $0.tag }, name: { $0.name })
    check(none.thisMac.isEmpty && none.connected.isEmpty, "layout: no speakers, empty lists")
}

// MARK: Equalizer presets (the core runs the filters; core/tests/eq.rs)
check(Equalizer.presets.allSatisfy { $0.gains.count == Equalizer.frequencies.count }, "eq: every preset has 10 bands")
check(Equalizer.presets.map(\.id) == Equalizer.PresetID.allCases, "eq: one preset per id, in menu order")

// MARK: Notices — several at once, one per subject, newest first
do {
    let t0 = Date(timeIntervalSince1970: 1_000_000)
    var list = NoticeList()
    check(list.isEmpty && list.newest == nil, "notices: start empty")
    list.post("a", subject: "ap:1", at: t0)
    list.post("source", subject: Notice.source, at: t0 + 1)
    check(list.items.map(\.subject) == [Notice.source, "ap:1"], "notices: newest first, both kept")
    list.post("a again", subject: "ap:1", at: t0 + 2)
    check(list.count == 2 && list.newest?.text == "a again" && list.newest?.date == t0 + 2,
          "notices: same subject replaces the old one and moves to the top")
    for i in 2...6 { list.post("x\(i)", subject: "lc:\(i)", at: t0 + Double(10 + i)) }
    check(list.count == NoticeList.capacity, "notices: capped", "\(list.count)")
    check(list.items.map(\.subject) == ["lc:6", "lc:5", "lc:4", "lc:3"], "notices: the oldest drop off")
    check(list.clear(["lc:5", "ap:1"]) && list.items.map(\.subject) == ["lc:6", "lc:4", "lc:3"],
          "notices: clear removes only the given subjects")
    check(!list.clear(["ap:1"]) && list.count == 3, "notices: clearing an absent subject changes nothing")
    list.clearAll()
    check(list.isEmpty, "notices: clear all")
    let music = Notice.automation("com.apple.Music"), spotify = Notice.automation("com.spotify.client")
    list.post("music", subject: music, at: t0, settings: .automation)
    list.post("spotify", subject: spotify, at: t0 + 1, settings: .automation)
    check(list.count == 2 && list.items.allSatisfy { $0.settings == .automation },
          "notices: one Automation notice per player, each with its settings pane")
    list.post("plain", subject: Notice.source, at: t0 + 2)
    check(list.newest?.settings == nil, "notices: no settings button unless given")
    check(list.clear([music]) && list.items.map(\.subject) == [Notice.source, spotify],
          "notices: allowing one player clears only its notice")
    check(PrivacyPane.localNetwork.url.absoluteString
            == "x-apple.systempreferences:com.apple.preference.security?Privacy_LocalNetwork"
          && PrivacyPane.automation.url.absoluteString.hasSuffix("?Privacy_Automation"),
          "notices: privacy panes open System Settings at the right setting")
}

// MARK: Local Network hint — denied access at once; a guess only when unknown
do {
    let t0 = Date(timeIntervalSince1970: 2_000_000)
    let wait = LocalNetworkHint.emptyBeforeHint
    func hint(_ access: LocalNetworkAccess, up: Bool = true, empty since: Date?, at now: Date) -> LocalNetworkHint {
        LocalNetworkHint.decide(access: access, senderUp: up, emptySince: since, now: now)
    }
    // denied / allowed / unknown × (speakers found, empty briefly, empty for a while)
    for (since, now) in [(nil, t0), (t0, t0 + wait - 1), (t0, t0 + wait)] as [(Date?, Date)] {
        check(hint(.denied, empty: since, at: now) == .denied,
              "local network: denied access shows at once, whatever the list")
        check(hint(.allowed, empty: since, at: now) == .none,
              "local network: allowed access never hints, even on a network without speakers")
    }
    check(hint(.unknown, empty: nil, at: t0) == .none,
          "local network: unknown access, speakers found or engine silent: no hint")
    check(hint(.unknown, empty: t0, at: t0 + wait - 1) == .none,
          "local network: unknown access, no hint during normal discovery")
    check(hint(.unknown, empty: t0, at: t0 + wait) == .possible,
          "local network: unknown access, a soft hint once nothing was found for a while")
    for access in [LocalNetworkAccess.denied, .allowed, .unknown] {
        check(hint(access, up: false, empty: t0, at: t0 + wait) == .none,
              "local network: no hint while the engine isn't up")
    }
    // When the host has to look again for the hint, with no event to say so (S101).
    func due(_ access: LocalNetworkAccess, up: Bool = true, empty since: Date?, at now: Date) -> Date? {
        LocalNetworkHint.dueAt(access: access, senderUp: up, emptySince: since, now: now)
    }
    let hintDue = due(.unknown, empty: t0, at: t0 + 1)
    check(hintDue == t0 + wait && hint(.unknown, empty: t0, at: hintDue ?? t0) == .possible
            && hint(.unknown, empty: t0, at: (hintDue ?? t0) - 0.001) == .none,
          "local network: the host looks again exactly when an empty list brings up the soft hint (S101)",
          "\(String(describing: hintDue))")
    check(due(.unknown, empty: t0, at: t0 + wait) == nil,
          "local network: once the soft hint shows, nothing more falls due for it (S101)")
    check(due(.denied, empty: t0, at: t0) == nil && due(.allowed, empty: t0, at: t0) == nil
            && due(.unknown, up: false, empty: t0, at: t0) == nil && due(.unknown, empty: nil, at: t0) == nil,
          "local network: no hint falls due when access is known, the engine isn't up or speakers were found (S101)")
    // While the answer is "denied", the probe asks again `deniedRecheck` after its browser started.
    let recheck = LocalNetworkProbe.recheckDate(running: true, access: .denied, startedAt: t0)
    check(recheck == t0 + LocalNetworkProbe.deniedRecheck,
          "local network probe: a denied answer is asked again 10 s after the browser started (S101)",
          "\(String(describing: recheck))")
    check(LocalNetworkProbe.recheckDate(running: true, access: .unknown, startedAt: t0) == nil
            && LocalNetworkProbe.recheckDate(running: true, access: .allowed, startedAt: t0) == nil
            && LocalNetworkProbe.recheckDate(running: false, access: .denied, startedAt: t0) == nil,
          "local network probe: only a running probe's denied answer is asked again (S101)")
    // What the in-process Bonjour browser's states say about access.
    let policyDenied = NWError.dns(DNSServiceErrorType(kDNSServiceErr_PolicyDenied))
    check(LocalNetworkProbe.access(for: .waiting(policyDenied)) == .denied,
          "local network probe: a browser waiting on PolicyDenied means denied")
    check(LocalNetworkProbe.access(for: .failed(policyDenied)) == .denied,
          "local network probe: a browser failing on PolicyDenied means denied")
    check(LocalNetworkProbe.access(for: .ready) == .allowed,
          "local network probe: a browser that is ready means allowed")
    check(LocalNetworkProbe.access(for: .waiting(.dns(DNSServiceErrorType(kDNSServiceErr_NoSuchRecord)))) == .unknown
            && LocalNetworkProbe.access(for: .waiting(.posix(.ENETDOWN))) == .unknown,
          "local network probe: other waits say nothing about access")
    check(LocalNetworkProbe.access(for: .setup) == nil && LocalNetworkProbe.access(for: .cancelled) == nil,
          "local network probe: setup and cancel keep the last answer")
}

// MARK: Speaker rows — a speaker never looks like it plays while streaming is off
check(SpeakerState(enabled: true, streaming: true) == .playing, "speaker: on while streaming plays")
check(SpeakerState(enabled: true, streaming: false) == .willPlay, "speaker: on while off waits")
check(SpeakerState(enabled: false, streaming: true) == .off, "speaker: off while streaming is off")
check(SpeakerState(enabled: false, streaming: false) == .off, "speaker: off while off is off")
check(!SpeakerState.playing.clickTurnsOn, "speaker: click on a playing speaker turns it off")
check(SpeakerState.willPlay.clickTurnsOn, "speaker: click on a waiting speaker turns it on (and starts streaming)")
check(SpeakerState.off.clickTurnsOn, "speaker: click on an off speaker turns it on")
check(SpeakerAvailability(.open) == .open, "availability: open")
check(SpeakerAvailability(.code(password: false)) == .needsCode, "availability: PIN asks for a code")
check(SpeakerAvailability(.code(password: true)) == .needsCode, "availability: password asks for a code")

// MARK: Device volume — a turned-down or muted output shows a hint and a one-click fix (S192 to S202)
do {
    /// Core Audio as the device-volume checks see it, so no real device is
    /// read or written: per device a volume scalar and a mute on each element
    /// it has, whether each can be set, and the preferred stereo channels. It
    /// records the listeners and every write, and notifies a listened control
    /// when a check changes it, as Core Audio does on its own queue.
    final class FakeAccess: DeviceVolumeAccess {
        struct Key: Hashable {
            let device: AudioDeviceID
            let control: DeviceVolumeControl
        }
        struct Write: Equatable {
            let device: AudioDeviceID
            let control: DeviceVolumeControl
            let value: Double
        }
        /// What a write does: the device takes it and notifies, ignores it
        /// (no error, nothing changes) or refuses it with an error.
        enum Writes { case apply, ignore, refuse }
        var values: [Key: (value: Double, settable: Bool)] = [:]
        var stereo: [AudioDeviceID: [AudioObjectPropertyElement]] = [:]
        var listeners: [Key: () -> Void] = [:]
        var writes: [Write] = []
        var writeMode = Writes.apply
        /// The most listeners held at once.
        var mostListeners = 0
        /// Core Audio refuses every new listener.
        var refusesListeners = false

        /// A control the device has from the start, on its main element.
        func set(_ device: AudioDeviceID, _ part: DeviceVolumeControl.Part, _ value: Double, settable: Bool = true) {
            set(device, part, channel: kAudioObjectPropertyElementMain, value, settable: settable)
        }

        /// A control the device has from the start, on `element`.
        func set(_ device: AudioDeviceID, _ part: DeviceVolumeControl.Part, channel element: AudioObjectPropertyElement,
                 _ value: Double, settable: Bool = true) {
            values[Key(device: device, control: DeviceVolumeControl(part: part, element: element))] = (value, settable)
        }

        /// A change made outside AudioSync, which Core Audio reports.
        func change(_ device: AudioDeviceID, _ part: DeviceVolumeControl.Part,
                    _ element: AudioObjectPropertyElement = kAudioObjectPropertyElementMain, to value: Double) {
            let key = Key(device: device, control: DeviceVolumeControl(part: part, element: element))
            values[key]?.value = value
            listeners[key]?()
        }

        func isSettable(_ device: AudioDeviceID, _ control: DeviceVolumeControl) -> Bool {
            values[Key(device: device, control: control)]?.settable ?? false
        }

        func read(_ device: AudioDeviceID, _ control: DeviceVolumeControl) -> Double? {
            values[Key(device: device, control: control)]?.value
        }

        func preferredStereoChannels(_ device: AudioDeviceID) -> [AudioObjectPropertyElement]? { stereo[device] }

        func write(_ device: AudioDeviceID, _ control: DeviceVolumeControl, _ value: Double) -> OSStatus {
            writes.append(Write(device: device, control: control, value: value))
            if writeMode == .refuse { return OSStatus(kAudioHardwareIllegalOperationError) }
            if writeMode == .apply { change(device, control.part, control.element, to: value) }
            return OSStatus(kAudioHardwareNoError)
        }

        func listen(_ device: AudioDeviceID, _ control: DeviceVolumeControl,
                    _ changed: @escaping () -> Void) -> Bool {
            guard !refusesListeners else { return false }
            listeners[Key(device: device, control: control)] = changed
            mostListeners = max(mostListeners, listeners.count)
            return true
        }

        func stopListening(_ device: AudioDeviceID, _ control: DeviceVolumeControl) {
            listeners[Key(device: device, control: control)] = nil
        }

        func listened(_ device: AudioDeviceID) -> Bool { listeners.keys.contains { $0.device == device } }
    }

    /// The host's follower on the fake, with the main queue's turns in the
    /// check's hands and the debug log's lines collected.
    final class Host {
        let access = FakeAccess()
        var queued: [() -> Void] = []
        var lines: [String] = []
        var updates = 0
        private(set) lazy var volumes: DeviceVolumes = {
            let volumes = DeviceVolumes(access: access,
                                        schedule: { [unowned self] work in self.queued.append(work) },
                                        log: { [unowned self] message in self.lines.append(message()) })
            volumes.onChange = { [unowned self] in self.updates += 1 }
            return volumes
        }()

        /// The main queue's next turn: what the listeners handed over runs.
        func runMain() {
            let work = queued
            queued = []
            work.forEach { $0() }
        }
    }

    let main = kAudioObjectPropertyElementMain
    let text = UIStrings(locale: Locale(identifier: "en_US"))
    let speakers = "MacBook Pro Speakers"
    let builtIn: AudioDeviceID = 41
    let uid = "BuiltInSpeakerDevice"

    // The quiet MacBook speakers: AudioSync at 100, the device at 13 %.
    do {
        let host = Host()
        host.access.set(builtIn, .volume, 0.13)
        host.access.set(builtIn, .mute, 0)
        host.volumes.follow([uid: builtIn])
        let hint = host.volumes.hints[uid]
        let shown = hint.map { DeviceVolumePresentation($0, name: speakers, text: text) }
        check(hint == .low(percent: 13) && shown?.text == "macOS volume is 13%" && shown?.fixTitle == "Set to 75%",
              "device volume: a switched-on output at 13% shows the low hint and the fix",
              "\(String(describing: hint)): \(shown?.text ?? "nothing") / \(shown?.fixTitle ?? "no fix")")
    }

    do {
        let host = Host()
        host.access.set(builtIn, .volume, 0.6)
        host.access.set(builtIn, .mute, 1)
        host.access.set(52, .volume, 0.13)
        host.access.set(52, .mute, 1)
        host.volumes.follow([uid: builtIn, "usb": 52])
        let muted = host.volumes.hints[uid].map { DeviceVolumePresentation($0, name: speakers, text: text) }
        let both = host.volumes.hints["usb"].map { DeviceVolumePresentation($0, name: "USB DAC", text: text) }
        check(host.volumes.hints[uid] == .muted && muted?.text == "Muted in macOS" && muted?.fixTitle == "Unmute"
                && host.volumes.hints["usb"] == .mutedAndLow(percent: 13)
                && both?.text == "Muted in macOS, volume 13%" && both?.fixTitle == "Unmute at 75%",
              "device volume: a muted output shows the unmute fix",
              "\(String(describing: host.volumes.hints))")
    }

    do {
        let host = Host()
        host.access.set(builtIn, .volume, 0.13)
        host.access.set(builtIn, .mute, 1)
        host.access.set(52, .volume, 0.9)
        host.access.set(52, .mute, 1)
        host.access.set(53, .volume, 0.5)
        host.access.set(53, .mute, 0)
        host.volumes.follow([uid: builtIn, "usb": 52, "fine": 53])
        let beforeClick = host.access.writes
        host.volumes.fix(uid)
        let fixed = host.access.writes
        host.runMain()
        host.access.writes = []
        host.volumes.fix("usb")
        let unmutedOnly = host.access.writes
        host.access.writes = []
        host.volumes.fix("fine")
        let volume = DeviceVolumeControl(part: .volume, element: main)
        let mute = DeviceVolumeControl(part: .mute, element: main)
        check(beforeClick.isEmpty
                && fixed == [FakeAccess.Write(device: builtIn, control: volume, value: 0.75),
                             FakeAccess.Write(device: builtIn, control: mute, value: 0)]
                && host.volumes.hints[uid] == nil
                && unmutedOnly == [FakeAccess.Write(device: 52, control: mute, value: 0)]
                && host.access.read(52, volume) == 0.9
                && host.access.writes.isEmpty,
              "device volume: the fix writes 0.75 and unmute once each, and only on a click",
              "before: \(beforeClick.count), click: \(fixed.map(\.value)), unmute only: \(unmutedOnly.map(\.value))")
    }

    do {
        // Launch, a start, a stop and a source change each reconcile the
        // outputs again; switch-on adds one; quit lets go of all.
        let host = Host()
        host.access.set(builtIn, .volume, 0.05)
        host.access.set(builtIn, .mute, 1)
        host.access.set(52, .volume, 0.1)
        host.access.set(52, .mute, 1)
        host.volumes.follow([uid: builtIn])                // launch
        host.volumes.follow([uid: builtIn])                // start
        host.volumes.follow([uid: builtIn])                // stop
        host.volumes.follow([uid: builtIn])                // source change
        host.volumes.follow([uid: builtIn, "usb": 52])     // switch-on
        host.runMain()
        let followed = host.access.listened(builtIn) && host.access.listened(52)
        host.volumes.stopFollowing()                       // quit
        check(followed && host.access.writes.isEmpty && host.access.listeners.isEmpty,
              "device volume: nothing is written at launch, start, stop, switch-on, source change or quit",
              "followed: \(followed), writes: \(host.access.writes.count), listeners left: \(host.access.listeners.count)")
    }

    do {
        // No volume on the main element: the preferred stereo channels (1 and
        // 2 when the device doesn't say); channel 3 isn't one of them.
        let host = Host()
        host.access.set(60, .volume, channel: 1, 0.10)
        host.access.set(60, .volume, channel: 2, 0.20)
        host.access.set(60, .volume, channel: 3, 0.90)
        host.access.set(61, .volume, channel: 3, 0.04)
        host.access.set(61, .volume, channel: 4, 0.06)
        host.access.set(61, .mute, channel: 3, 1)
        host.access.set(61, .mute, channel: 4, 1)
        host.access.stereo[61] = [3, 4]
        host.access.set(62, .volume, 0.8)
        host.access.set(62, .mute, channel: 1, 1)
        host.access.set(62, .mute, channel: 2, 0)
        host.volumes.follow(["pair": 60, "rear": 61, "half": 62])
        host.volumes.fix("pair")
        let pairWrites = host.access.writes
        let channel = { (element: AudioObjectPropertyElement) in DeviceVolumeControl(part: .volume, element: element) }
        check(host.volumes.hints["pair"] == .low(percent: 15)
                && host.volumes.hints["rear"] == .mutedAndLow(percent: 5)
                && host.volumes.hints["half"] == nil
                && pairWrites == [FakeAccess.Write(device: 60, control: channel(1), value: 0.75),
                                  FakeAccess.Write(device: 60, control: channel(2), value: 0.75)]
                && host.access.read(60, channel(3)) == 0.90,
              "device volume: a per-channel device reads the mean of its stereo channels and the fix sets each",
              "\(String(describing: host.volumes.hints)), writes: \(pairWrites.map { ($0.control.element, $0.value) })")
    }

    do {
        // Many HDMI outputs, aggregates and multi-output devices have no
        // control; a volume that can't be set can't be fixed, so it counts
        // as absent, while a settable mute next to it still counts.
        let host = Host()
        host.access.set(70, .volume, 0.05, settable: false)
        host.access.set(70, .mute, 1, settable: false)
        host.access.set(72, .volume, 0.05, settable: false)
        host.access.set(72, .mute, 1)
        host.volumes.follow(["hdmi": 70, "aggregate": 71, "mutable": 72])
        host.volumes.fix("hdmi")
        host.volumes.fix("aggregate")
        check(host.volumes.hints["hdmi"] == nil && host.volumes.hints["aggregate"] == nil
                && host.volumes.hints["mutable"] == .muted && host.access.writes.isEmpty,
              "device volume: a device without a settable volume or mute shows nothing",
              "\(String(describing: host.volumes.hints)), writes: \(host.access.writes.count)")
    }

    do {
        let listed = [AudioDevices.Device(id: builtIn, name: speakers, uid: uid, transport: kAudioDeviceTransportTypeBuiltIn),
                      AudioDevices.Device(id: 52, name: "USB DAC", uid: "usb", transport: kAudioDeviceTransportTypeUSB)]
        // On: the built-in speakers, an AirPlay speaker and a USB device
        // that is unplugged now; off: the listed USB DAC.
        let wanted = DeviceVolumes.wanted(enabled: [uid, "airplay-kitchen", "unplugged"], listed: listed)
        let host = Host()
        host.access.set(builtIn, .volume, 0.13)
        host.access.set(52, .volume, 0.05)
        host.volumes.follow(wanted)
        let hints: [String: DeviceVolumeHint] = ["airplay-kitchen": .low(percent: 5), "usb": .muted, uid: .low(percent: 13)]
        check(wanted == [uid: builtIn] && host.access.listened(builtIn) && !host.access.listened(52)
                && host.volumes.hints["usb"] == nil
                && DeviceVolumes.hint(for: "airplay-kitchen", airPlay: true, enabled: true, in: hints) == nil
                && DeviceVolumes.hint(for: "usb", airPlay: false, enabled: false, in: hints) == nil
                && DeviceVolumes.hint(for: uid, airPlay: false, enabled: true, in: hints) == .low(percent: 13),
              "device volume: AirPlay rows and switched-off rows show nothing and hold no listener",
              "followed: \(wanted)")
    }

    do {
        let host = Host()
        host.access.set(builtIn, .volume, 0.8)
        host.access.set(builtIn, .mute, 0)
        host.volumes.follow([uid: builtIn])
        let before = host.volumes.hints[uid]
        host.access.change(builtIn, .volume, to: 0.13)
        host.runMain()
        let after = host.volumes.hints[uid]
        let updates = host.updates
        // Several notifications before the main queue's turn: one update.
        host.access.change(builtIn, .volume, to: 0.10)
        host.access.change(builtIn, .volume, to: 0.12)
        let turns = host.queued.count
        host.runMain()
        let collapsed = host.volumes.hints[uid]
        // A notification on the mute alone: both parts are read again.
        host.access.values[FakeAccess.Key(device: builtIn, control: DeviceVolumeControl(part: .volume, element: main))]?.value = 0.9
        host.access.change(builtIn, .mute, to: 1)
        host.runMain()
        let reread = host.volumes.hints[uid]
        // Switched off, then on again on the device's new object.
        host.volumes.follow([:])
        let released = host.access.listeners.isEmpty && host.volumes.hints.isEmpty
        host.access.set(42, .volume, 0.2)
        host.volumes.follow([uid: 42])
        check(before == nil && after == .low(percent: 13) && updates == 1 && turns == 1
                && collapsed == .low(percent: 12) && reread == .muted && released
                && host.volumes.hints[uid] == .low(percent: 20) && host.access.listened(42) && !host.access.listened(builtIn),
              "device volume: a change notification updates the row, and switching off removes the listener",
              "\(String(describing: [before, after, collapsed, reread])), updates \(updates), turns \(turns), released \(released)")
    }

    do {
        let low = DeviceVolumePresentation(.low(percent: 13), name: speakers, text: text)
        let muted = DeviceVolumePresentation(.muted, name: speakers, text: text)
        let both = DeviceVolumePresentation(.mutedAndLow(percent: 13), name: speakers, text: text)
        check(low.label == "MacBook Pro Speakers: macOS volume is 13%"
                && muted.label == "MacBook Pro Speakers: muted in macOS"
                && both.label == "MacBook Pro Speakers: muted in macOS, volume 13%"
                && low.fixLabel == "Set to 75%, MacBook Pro Speakers"
                && muted.fixLabel == "Unmute, MacBook Pro Speakers"
                && both.fixLabel == "Unmute at 75%, MacBook Pro Speakers"
                && [low, muted, both].allSatisfy { !$0.fixTitle.isEmpty && $0.fixLabel.hasPrefix($0.fixTitle) }
                && [low, muted, both].allSatisfy {
                    $0.help == "macOS has its own volume for each output, and it limits AudioSync's slider."
                },
              "device volume: the hint and the fix name the output for VoiceOver, the fix's label starting with its visible text",
              "\(low.label) / \(low.fixLabel); \(muted.label) / \(muted.fixLabel); \(both.label) / \(both.fixLabel)")
    }

    do {
        let low = DeviceVolumeGlyph(.low(percent: 13), text: text)
        let muted = DeviceVolumeGlyph(.muted, text: text)
        let both = DeviceVolumeGlyph(.mutedAndLow(percent: 13), text: text)
        // The panel's row has no button: what it offers to VoiceOver and as
        // a tooltip is the hint alone, never the fix.
        let fixes = ["Set to 75%", "Unmute"]
        check(low.symbol == "speaker.wave.1.fill" && muted.symbol == "speaker.slash.fill" && both.symbol == muted.symbol
                && low.tooltip == "macOS volume is 13%" && muted.tooltip == "Muted in macOS"
                && both.tooltip == "Muted in macOS, volume 13%"
                && [low, muted, both].allSatisfy { glyph in
                    glyph.accessibilityNote == glyph.tooltip && !fixes.contains { glyph.tooltip.contains($0) }
                },
              "device volume: the compact panel's row shows the glyph with the hint as tooltip and VoiceOver hint, without a button",
              "\(low.symbol) \(low.tooltip); \(muted.symbol) \(muted.tooltip); \(both.symbol) \(both.tooltip)")
    }

    do {
        let host = Host()
        host.access.set(builtIn, .volume, 0.13)
        host.access.set(builtIn, .mute, 0)
        host.volumes.follow([uid: builtIn])
        host.access.writeMode = .ignore
        host.volumes.fix(uid)
        host.runMain()
        let ignored = host.volumes.hints[uid]
        let ignoredLine = host.lines.last ?? ""
        host.access.writeMode = .refuse
        host.volumes.fix(uid)
        host.runMain()
        let refused = host.volumes.hints[uid]
        let refusedLine = host.lines.last ?? ""
        check(ignored == .low(percent: 13) && refused == .low(percent: 13) && host.access.writes.count == 2
                && ignoredLine.contains("volume set to 75%: done") && ignoredLine.contains("13%")
                && refusedLine.contains("volume set to 75%: failed"),
              "device volume: a write the device ignores leaves the hint and the fix as they were and logs it",
              "\(ignoredLine) | \(refusedLine)")
    }

    do {
        let host = Host()
        host.access.set(builtIn, .volume, 0.8)
        host.access.set(builtIn, .mute, 0)
        host.volumes.follow([uid: builtIn])
        let first = host.lines
        for level in [0.6, 0.45, 0.3, 0.26] {
            host.access.change(builtIn, .volume, to: level)
            host.runMain()
        }
        let above = host.lines.count
        host.access.change(builtIn, .volume, to: 0.2)
        host.runMain()
        host.access.change(builtIn, .volume, to: 0.15)
        host.runMain()
        let crossed = host.lines.dropFirst(above)
        check(first.count == 1 && first[0].contains(uid) && first[0].contains("80%") && first[0].contains("settable")
                && above == 1 && crossed.count == 1 && crossed.first?.contains("fine → low") == true,
              "device volume: dragging the system volume across 25% logs one line",
              "\(host.lines)")
    }

    do {
        let host = Host()
        host.access.set(builtIn, .volume, 0.13)
        host.volumes.follow([uid: builtIn])
        // The device answers nothing for a moment (its control reads as
        // absent when the notification is applied), then it is back.
        let volume = FakeAccess.Key(device: builtIn, control: DeviceVolumeControl(part: .volume, element: main))
        let saved = host.access.values.removeValue(forKey: volume)
        host.access.listeners[volume]?()
        host.runMain()
        let kept = host.access.listened(builtIn)
        host.access.values[volume] = saved
        host.access.change(builtIn, .volume, to: 0.10)
        host.runMain()
        let back = host.volumes.hints[uid]
        // A mute appears, and Core Audio refuses the listeners for the new controls.
        host.access.refusesListeners = true
        host.access.set(builtIn, .mute, 1)
        host.access.change(builtIn, .volume, to: 0.12)
        host.runMain()
        let refused = host.lines.contains { $0.contains(uid) && $0.contains("2 of its controls not followed") }
        check(kept && back == .low(percent: 10) && host.volumes.hints[uid] == .mutedAndLow(percent: 12) && refused,
              "device volume: a device that answers nothing for a moment keeps its listeners, and a refused listener is logged",
              "kept \(kept), back \(String(describing: back)), now \(String(describing: host.volumes.hints[uid])), "
                + "lines \(host.lines)")
    }
}

// MARK: This Mac's own AirPlay Receiver — never listed (S100)
check(NativeReceiver.isThisMac("My Mac", computerName: "My Mac"), "own receiver: the Mac's own receiver is recognized")
check(NativeReceiver.isThisMac("my mac", computerName: "My Mac"), "own receiver: the match ignores case")
check(!NativeReceiver.isThisMac("My Mac mini", computerName: "My Mac"), "own receiver: only an exact name is the Mac")
check(!NativeReceiver.isThisMac("My Mac", computerName: nil), "own receiver: an unknown computer name hides nothing")
check(!NativeReceiver.isThisMac("", computerName: ""), "own receiver: an empty computer name hides nothing")

// MARK: Capture check — "nothing yet" is not an error until the source is known to play
check(CaptureCheck.verdict(framesCaptured: 1, knownPlayingFor: 60) == .flowing, "capture: frames → flowing")
check(CaptureCheck.verdict(framesCaptured: 0, knownPlayingFor: nil) == .waiting, "capture: no frames, unknown source → waiting")
check(CaptureCheck.verdict(framesCaptured: 0, knownPlayingFor: 2) == .waiting, "capture: playing only briefly → still waiting")
check(CaptureCheck.verdict(framesCaptured: 0, knownPlayingFor: CaptureCheck.playingGrace) == .notWorking, "capture: playing for the grace period with no frames → not working")
check(CaptureCheck.verdict(framesCaptured: 0, knownPlayingFor: nil) != .notWorking, "capture: a paused or unknown source never blames the permission")

// MARK: Capture stage — stereo from any input layout, a rate for every session, an app tap that follows the app
do {
    // One IOProc input as Core Audio hands it over: buffers of interleaved
    // float channels (nil samples: a buffer without data).
    typealias Buffer = (channels: UInt32, samples: [Float]?)
    func withInput(_ buffers: [Buffer], _ body: (UnsafePointer<AudioBufferList>) -> Void) {
        let list = AudioBufferList.allocate(maximumBuffers: max(buffers.count, 1))
        list.count = buffers.count
        var storage: [UnsafeMutablePointer<Float>] = []
        for (i, buffer) in buffers.enumerated() {
            var data: UnsafeMutableRawPointer?
            var bytes: UInt32 = 0
            if let samples = buffer.samples {
                let p = UnsafeMutablePointer<Float>.allocate(capacity: max(samples.count, 1))
                p.initialize(from: samples, count: samples.count)
                storage.append(p)
                data = UnsafeMutableRawPointer(p)
                bytes = UInt32(samples.count * MemoryLayout<Float>.size)
            }
            list[i] = AudioBuffer(mNumberChannels: buffer.channels, mDataByteSize: bytes, mData: data)
        }
        body(list.unsafePointer)
        storage.forEach { $0.deallocate() }
        free(list.unsafeMutablePointer)
    }
    // What the capture IOProc hands to the engine parts: the blocks, joined.
    func stereo(_ buffers: [Buffer], blockFrames: Int = CaptureChannels.blockFrames) -> (samples: [Float], blocks: [Int]) {
        var samples: [Float] = []
        var blocks: [Int] = []
        withInput(buffers) { list in
            let scratch = UnsafeMutablePointer<Float>.allocate(capacity: blockFrames * 2)
            defer { scratch.deallocate() }
            CaptureChannels(list).forEachBlock(in: scratch, blockFrames: blockFrames) { block, count in
                samples += Array(UnsafeBufferPointer(start: block, count: count * 2))
                blocks.append(count)
            }
        }
        return (samples, blocks)
    }

    check(stereo([(2, [0.1, -0.1, 0.2, -0.2, 0.3, -0.3])]).samples == [0.1, -0.1, 0.2, -0.2, 0.3, -0.3],
          "capture channels: the tap's interleaved stereo passes through unchanged")
    check(stereo([(1, [0.1, 0.2, 0.3])]).samples == [0.1, 0.1, 0.2, 0.2, 0.3, 0.3],
          "capture channels: a one-channel input plays on both sides")
    check(stereo([(4, [1, 2, 3, 4, 5, 6, 7, 8])]).samples == [1, 2, 5, 6],
          "capture channels: an input with four channels gives its first two as L and R")
    check(stereo([(1, [1, 2, 3]), (1, [-1, -2, -3])]).samples == [1, -1, 2, -2, 3, -3],
          "capture channels: one buffer per channel gives L from the first and R from the second")
    check(stereo([(1, [1, 2]), (2, [10, 20, 30, 40])]).samples == [1, 10, 2, 30],
          "capture channels: channels are counted across buffers, in order")
    check(stereo([(0, nil), (2, [1, 2])]).samples == [1, 2],
          "capture channels: a buffer without channels is skipped")
    check(stereo([(1, [1, 2, 3]), (1, [9])]).samples == [1, 9],
          "capture channels: only frames both channels have are delivered")
    let nothing = [stereo([]), stereo([(2, nil)]), stereo([(1, nil), (1, [1, 2])]), stereo([(2, [])])]
    check(nothing.allSatisfy { $0.samples.isEmpty && $0.blocks.isEmpty },
          "capture channels: no buffer, no data or no frames delivers nothing")
    let odd = stereo([(2, [Float.nan, .infinity, -.infinity, 0.5])]).samples
    check(odd.count == 4 && odd[0].isNaN && odd[1] == .infinity && odd[2] == -.infinity && odd[3] == 0.5,
          "capture channels: NaN and infinity pass through untouched (the stages after capture handle them)")
    let long = (0..<10_000).flatMap { [Float($0), -Float($0)] }
    let blocked = stereo([(2, long)])
    check(blocked.blocks == [4096, 4096, 1808] && blocked.samples == long,
          "capture channels: a long callback is delivered in order, in blocks of at most 4096 frames",
          "\(blocked.blocks)")
    check(CaptureChannels.blockFrames <= 8192, "capture channels: a block never exceeds the ring's write margin")

    check(CaptureRate.session(reported: 0) == 48000 && CaptureRate.session(reported: .nan) == 48000,
          "capture rate: a rate that reads 0 counts as 48 kHz")
    check([8000.0, 44100, 48000, 96000, 384_000].allSatisfy { CaptureRate.session(reported: $0) == $0 },
          "capture rate: 8 kHz to 384 kHz are taken as they are")
    check(CaptureRate.running(device: 44100, tap: 48000) == 44100,
          "capture rate: the rate of the device the IOProc runs on wins over the tap's")
    check(CaptureRate.running(device: 0, tap: 96000) == 96000,
          "capture rate: the tap's rate when the device's can't be read")
    check(CaptureRate.running(device: 0, tap: 0) == 0
            && !CaptureRate.needsRestart(current: 48000, reported: CaptureRate.running(device: 0, tap: 0)),
          "capture rate: nothing readable keeps the session playing")

    func format(flags: AudioFormatFlags, bits: UInt32 = 32, channels: UInt32 = 2) -> AudioStreamBasicDescription {
        AudioStreamBasicDescription(mSampleRate: 48000, mFormatID: kAudioFormatLinearPCM, mFormatFlags: flags,
                                    mBytesPerPacket: bits / 8 * channels, mFramesPerPacket: 1,
                                    mBytesPerFrame: bits / 8 * channels, mChannelsPerFrame: channels,
                                    mBitsPerChannel: bits, mReserved: 0)
    }
    let float32 = kAudioFormatFlagIsFloat | kAudioFormatFlagIsPacked
    check(CaptureInputFormat.check([format(flags: float32)]) == .usable,
          "input format: native 32-bit float is read as it is")
    check(CaptureInputFormat.check([format(flags: kAudioFormatFlagIsSignedInteger | kAudioFormatFlagIsPacked, bits: 16)]) == .unsupported
            && CaptureInputFormat.check([format(flags: float32 | kAudioFormatFlagIsBigEndian)]) == .unsupported
            && CaptureInputFormat.check([format(flags: float32, bits: 64)]) == .unsupported,
          "input format: integer, big-endian or 64-bit samples aren't read")
    check(CaptureInputFormat.check([]) == .noChannels
            && CaptureInputFormat.check([format(flags: float32, channels: 0)]) == .noChannels,
          "input format: a device without input channels has nothing to capture")
    check(CaptureInputFormat.check([format(flags: float32), format(flags: kAudioFormatFlagIsSignedInteger, bits: 16, channels: 0)]) == .usable,
          "input format: a stream without channels doesn't count")

    check(AppTapFollow.decide(tapped: [5, 7], found: [7, 5], appRunning: true) == .keep,
          "app tap: the same processes in another order keep the tap")
    check(AppTapFollow.decide(tapped: [5], found: [5, 9], appRunning: true) == .retap([5, 9]),
          "app tap: a new audio process (a browser's audio helper) is tapped too")
    check(AppTapFollow.decide(tapped: [5, 9], found: [5], appRunning: true) == .retap([5]),
          "app tap: a process that ends leaves the tap")
    check(AppTapFollow.decide(tapped: [5], found: [], appRunning: true) == .wait,
          "app tap: no audio process while the app runs: no tap, waiting for one")
    check(AppTapFollow.decide(tapped: [], found: [], appRunning: true) == .keep,
          "app tap: still nothing to tap keeps waiting")
    check(AppTapFollow.decide(tapped: [], found: [9, 4, 9], appRunning: true) == .retap([4, 9]),
          "app tap: the first audio process while waiting gets a tap")
    check(AppTapFollow.decide(tapped: [5], found: [5], appRunning: false) == .lost
            && AppTapFollow.decide(tapped: [], found: [], appRunning: false) == .lost,
          "app tap: the app gone means the source is lost")
    check(AppTapFollow.candidates([9, 3, 9, 42], own: 42) == [3, 9] && AppTapFollow.candidates([3], own: nil) == [3],
          "app tap: AudioSync's own process is never tapped, even when the app launched it")

    // Moving the tap to new processes: the steps it takes, in order. "old"
    // is the running capture, which mutes the app until it is closed.
    struct Failed: Error {}
    func retap(inPlace: Bool, make: Bool = true, rateFits: Bool = true, start: Bool = true)
        -> (steps: [String], outcome: AppRetap.Outcome?) {
        var steps: [String] = []
        let outcome = try? AppRetap.run(
            updateInPlace: { steps.append("update in place"); return inPlace },
            makeTap: { () throws -> String in
                steps.append("make new")
                if !make { throw Failed() }
                return "new"
            },
            rateFits: { _ in steps.append("check rate"); return rateFits },
            discard: { steps.append("discard \($0)") },
            closeOld: { steps.append("close old") },
            startReading: { tap in
                steps.append("start \(tap)")
                if !start { throw Failed() }
            })
        return (steps, outcome)
    }
    let inPlace = retap(inPlace: true)
    check(inPlace.outcome == .updated && inPlace.steps == ["update in place"],
          "app retap: the running tap is changed in place first, and nothing stops")
    let rebuilt = retap(inPlace: false)
    check(rebuilt.outcome == .rebuilt
            && rebuilt.steps == ["update in place", "make new", "check rate", "close old", "start new"],
          "app retap: a rebuilt tap exists before the old one closes, so the app stays muted",
          rebuilt.steps.joined(separator: ", "))
    let otherRate = retap(inPlace: false, rateFits: false)
    check(otherRate.outcome == .rateChanged && !otherRate.steps.contains("close old")
            && otherRate.steps.last == "discard new",
          "app retap: a new tap at another rate is discarded and the old one keeps running until the rebuild")
    let notMade = retap(inPlace: false, make: false)
    check(notMade.outcome == nil && notMade.steps == ["update in place", "make new"],
          "app retap: a tap that can't be made leaves the old capture to the caller, which ends the session")
    let notStarted = retap(inPlace: false, start: false)
    check(notStarted.outcome == nil && notStarted.steps.suffix(3) == ["close old", "start new", "discard new"],
          "app retap: a new tap that can't start reading is discarded")

    // Closing a capture. A source switch (S141) or a rate restart (S20) closes
    // the running capture and starts the next one in one turn of the main
    // queue; `nextTurn` holds what was left for a later turn.
    func closeCapture(objects: String?, thenInSameTurn: String? = nil) -> (sameTurn: [String], all: [String]) {
        var steps: [String] = []
        var nextTurn: [() -> Void] = []
        CaptureClose.run(closeIO: { steps.append("stop old IOProc") }, objects: objects,
                         destroy: { steps.append("destroy \($0)") }, later: { nextTurn.append($0) })
        if let thenInSameTurn { steps.append(thenInSameTurn) }
        let sameTurn = steps
        nextTurn.forEach { $0() }
        return (sameTurn, steps)
    }
    let closeSwitch = closeCapture(objects: "old tap", thenInSameTurn: "make new tap")
    check(closeSwitch.all == ["stop old IOProc", "make new tap", "destroy old tap"],
          "capture close: a source switch makes the new tap before the old tap is destroyed",
          closeSwitch.all.joined(separator: ", "))
    let closeStop = closeCapture(objects: "old tap")
    check(closeStop.sameTurn == ["stop old IOProc"] && closeStop.all == ["stop old IOProc", "destroy old tap"],
          "capture close: the IOProc stops at once, the tap and its aggregate device go on the next turn",
          closeStop.all.joined(separator: ", "))
    let closeInput = closeCapture(objects: nil, thenInSameTurn: "make new tap")
    check(closeInput.sameTurn == ["stop old IOProc", "make new tap"] && closeInput.all == closeInput.sameTurn,
          "capture close: an input device's capture has no tap, so nothing is left for later")
}

// MARK: Retired engine choice — noted once, no effect
check(RetiredEngineChoice.defaultsKey == "engineImpl" && RetiredEngineChoice.environmentKey == "AUDIOSYNC_ENGINE",
      "retired engine choice: the setting and the variable older builds read")
check(RetiredEngineChoice.launchNote(environment: nil, stored: nil) == nil
      && RetiredEngineChoice.launchNote(environment: "", stored: "") == nil,
      "retired engine choice: nothing set (or only empty values) means no note")
check(RetiredEngineChoice.launchNote(environment: nil, stored: "swift")
      == "[AudioSync] the engineImpl setting \"swift\" no longer chooses the audio engine: every session runs on the Rust core",
      "retired engine choice: a stored choice is noted, and has no effect")
check(RetiredEngineChoice.launchNote(environment: "swift", stored: nil)
      == "[AudioSync] AUDIOSYNC_ENGINE=swift no longer chooses the audio engine: every session runs on the Rust core",
      "retired engine choice: the environment variable is noted, and has no effect")
check(RetiredEngineChoice.launchNote(environment: "rust", stored: "swift")
      == "[AudioSync] AUDIOSYNC_ENGINE=rust and the engineImpl setting \"swift\" no longer choose the audio engine: "
         + "every session runs on the Rust core",
      "retired engine choice: both are noted in one line")

// MARK: Retired AirPlay sender choice — noted once, no effect
check(RetiredAirPlayChoice.defaultsKey == "airplayImpl" && RetiredAirPlayChoice.environmentKey == "AUDIOSYNC_AIRPLAY",
      "retired airplay choice: the setting and the variable older builds read")
check(RetiredAirPlayChoice.launchNote(environment: nil, stored: nil) == nil
      && RetiredAirPlayChoice.launchNote(environment: "", stored: "") == nil,
      "retired airplay choice: nothing set (or only empty values) means no note")
check(RetiredAirPlayChoice.launchNote(environment: nil, stored: "owntone")
      == "[AudioSync] the airplayImpl setting \"owntone\" no longer chooses the AirPlay sender: "
         + "AudioSync's own sender is the only one",
      "retired airplay choice: a stored choice of the old engine is noted, and has no effect")
check(RetiredAirPlayChoice.launchNote(environment: "native", stored: "owntone")
      == "[AudioSync] AUDIOSYNC_AIRPLAY=native and the airplayImpl setting \"owntone\" no longer choose "
         + "the AirPlay sender: AudioSync's own sender is the only one",
      "retired airplay choice: both are noted in one line")

// MARK: Retired choices at launch — noted once, then removed from the settings
do {
    final class Stored { var values: [String: Any] = [:] }
    let stored = Stored()
    stored.values = ["engineImpl": "swift", "airplayImpl": "owntone", "masterVolume": 80.0]
    let store = SettingsStore(read: { stored.values[$0] }, write: { value, key in stored.values[key] = value })

    let first = store.retiredChoiceNotes(environment: [:])
    check(first == [RetiredEngineChoice.launchNote(environment: nil, stored: "swift")!,
                    RetiredAirPlayChoice.launchNote(environment: nil, stored: "owntone")!],
          "retired choices: the first launch notes both stored choices", "notes: \(first)")
    check(stored.values["engineImpl"] == nil && stored.values["airplayImpl"] == nil
            && stored.values["masterVolume"] as? Double == 80,
          "retired choices: the first launch removes both stored keys and keeps the other settings",
          "left: \(stored.values.keys.sorted())")
    let second = store.retiredChoiceNotes(environment: [:])
    check(second.isEmpty, "retired choices: a second launch over the same settings logs nothing", "notes: \(second)")

    stored.values = ["engineImpl": "", "airplayImpl": 3]
    let odd = store.retiredChoiceNotes(environment: [:])
    check(odd.isEmpty && stored.values.isEmpty,
          "retired choices: an empty or non-text stored value is removed without a note",
          "notes: \(odd), left: \(stored.values.keys.sorted())")

    let environment = ["AUDIOSYNC_ENGINE": "swift", "AUDIOSYNC_AIRPLAY": "native"]
    let fromEnvironment = (1...2).map { _ in store.retiredChoiceNotes(environment: environment) }
    check(fromEnvironment.allSatisfy {
            $0 == [RetiredEngineChoice.launchNote(environment: "swift", stored: nil)!,
                   RetiredAirPlayChoice.launchNote(environment: "native", stored: nil)!] },
          "retired choices: an environment variable still set is noted at every launch (it can't be removed)",
          "notes: \(fromEnvironment)")
}

// MARK: Native AirPlay sender — its IDs, rows, speaker plan, events and sync rule
do {
    check(NativeAirPlayID.pcmDebugIDs(nil).isEmpty && NativeAirPlayID.pcmDebugIDs(" , ").isEmpty,
          "native airplay: no AUDIOSYNC_AIRPLAY_PCM names no receiver")
    check(NativeAirPlayID.pcmDebugIDs("a1:b2:c3:d4:e5:f6, raop:0011223344AA@Den ,,0011223344aa")
            == ["A1B2C3D4E5F6", "raop:0011223344AA@Den", "0011223344AA"],
          "native airplay: AUDIOSYNC_AIRPLAY_PCM IDs are trimmed and device IDs take the 12-hex form")
    let tv = NativeReceiver(id: "02005E005302", name: "Den TV", model: "AppleTV3,2", availability: .needsPin,
                            reason: nil, hints: ["raop", "apple-tv", "pairing", "pin-pairing"])
    check(NativeReceiverChange.appeared(tv).logLine
          == "[AudioSync] native AirPlay: found Den TV (02005E005302), AppleTV3,2, needs pairing with a PIN, hints: raop apple-tv pairing pin-pairing",
          "native airplay: a found receiver's log line")
    let old = NativeReceiver(id: "raop:Old", name: "Old", model: "", availability: .unsupported,
                             reason: "encryption", hints: ["raop"])
    check(NativeReceiverChange.changed(old).logLine
          == "[AudioSync] native AirPlay: changed Old (raop:Old), not supported (encryption), hints: raop",
          "native airplay: a changed receiver's log line, with its reason")
    check(NativeReceiverChange.gone(id: "02005E005302", name: "Den TV").logLine
          == "[AudioSync] native AirPlay: gone Den TV (02005E005302)", "native airplay: a gone receiver it listed")
    check(NativeReceiverChange.gone(id: "02005E005309", name: nil).logLine
          == "[AudioSync] native AirPlay: gone 02005E005309", "native airplay: a gone receiver it never listed")

    let stream = NativeStreamCounters(rateHz: 44_101.26, packetsEncoded: 1_200, silentFrames: 352, inputFramesDropped: 0)
    let desk = NativeSpeakerCounters(packetsSent: 1_190, latePackets: 2, resendRequests: 3, packetsResent: 5,
                                     timingReplies: 4, foreignDatagrams: 1)
    let pair = NativeSpeakerCounters(packetsSent: 2_380, sessions: 2)
    let status = NativeAirPlayStatus.logLine(latencyMs: 1750.9, stream: stream, speakers: [
        .init(id: "02005E005321", name: "Desk", state: .playing, counters: desk),
        .init(id: "pair:1", name: nil, state: .connecting, counters: pair),
        .init(id: "02005E005322", name: "Gone", state: nil, counters: nil),
    ])
    check(status == """
        [AudioSync] native AirPlay: L = 1750 ms, input rate 44101.3 Hz, 1200 packets encoded, 352 silent frames, 0 input frames dropped
          AirPlay speaker Desk (02005E005321): playing; 1190 packets sent, 2 late, 3 resend requests, 5 resent, 4 timing replies, 1 datagrams from other addresses dropped
          AirPlay speaker pair:1: connecting, 2 members summed; 2380 packets sent, 0 late, 0 resend requests, 0 resent, 0 timing replies, 0 datagrams from other addresses dropped
          AirPlay speaker Gone (02005E005322): added
        """, "native airplay: the status line has the Windows log's fields, a target's summed members, and a speaker the sender dropped",
          status)
    check(NativeAirPlayStatus.logLine(latencyMs: .nan, stream: nil, speakers: [])
          == "[AudioSync] native AirPlay: L = 0 ms", "native airplay: a status line without the stream's counters")
    check(NativeAirPlayStatus.interval == 5, "native airplay: the status line comes every 5 s, as on Windows")
}
do {
    check(NativeAirPlayID.app("02005E005301") == "native:02005E005301"
            && NativeAirPlayID.receiver("native:02005E005301") == "02005E005301"
            && NativeAirPlayID.receiver("native:raop:Odd: name") == "raop:Odd: name",
          "native airplay: app IDs round-trip, instance IDs included")
    check(NativeAirPlayID.receiver("123456789012") == nil && NativeAirPlayID.receiver("02005E005301") == nil,
          "native airplay: an ID older versions saved is never taken for a native receiver")
    check(SpeakerKey.parse(SpeakerKey.make(id: NativeAirPlayID.app("raop:A:B"), airplay: true))
            .map { $0.id == "native:raop:A:B" && $0.airplay } == true,
          "native airplay: a native ID survives a speaker key (groups, auto-connect, notices)")
    let saved: Set<String> = ["123456789012", "native:02005E005301"]
    check(NativeAirPlayID.playable(saved) == ["native:02005E005301"],
          "native airplay: a speaker saved as on under an old ID that isn't moved yet neither starts nor holds up streaming")

    func receiver(_ id: String, _ name: String, _ availability: NativeReceiver.Availability) -> NativeReceiver {
        NativeReceiver(id: id, name: name, model: "", availability: availability, reason: nil, hints: [])
    }
    check(receiver("x", "x", .available).access == .open
            && receiver("x", "x", .needsPin).access == .code(password: false)
            && receiver("x", "x", .needsPassword).access == .code(password: true)
            && [NativeReceiver.Availability.needsPairing, .airPlay2Only, .unsupported]
                .allSatisfy { receiver("x", "x", $0).access == nil },
          "native airplay: rows show what can play, or needs a code (PIN or password); nothing else")

    var model = NativeAirPlayModel()
    for r in [receiver("A1", "kitchen", .available), receiver("B2", "Den TV", .needsPin),
              receiver("C3", "Office", .needsPassword), receiver("D4", "HomePod", .airPlay2Only),
              receiver("E5", "Old", .unsupported), receiver("F6", "Attic", .needsPairing)] {
        model.apply(.appeared(r))
    }
    check(model.rows(enabled: []) == ["native:B2", "native:A1", "native:C3"], "native airplay: rows by name", "\(model.rows(enabled: []))")
    check(model.access("A1") == .open && model.access("B2") == .code(password: false)
            && model.access("C3") == .code(password: true) && model.access("D4") == nil,
          "native airplay: a needs-PIN or password receiver shows as asking for a code")
    check(model.soundingIDs == ["native:A1"], "native airplay: only a receiver able to play holds local outputs back")

    let all: Set<String> = ["native:A1", "native:B2", "native:C3", "native:D4", "123456789012"]
    check(model.plan(streaming: false, enabled: all).isEmpty, "native airplay: nothing joins while not streaming")
    check(model.plan(streaming: true, enabled: all)
            == NativeAirPlayPlan(add: ["A1"], remove: [],
                                 turnOff: [.init(id: "B2", reason: .needsCode), .init(id: "C3", reason: .needsPassword)]),
          "native airplay: while streaming, the able join, the PIN and password ones not turned on by a click go off "
            + "(auto-connect and restored switches never pair or prompt), the rest wait")
    model.added("A1")
    check(model.plan(streaming: true, enabled: ["native:A1"]).isEmpty, "native airplay: an added speaker stays")
    check(model.plan(streaming: false, enabled: ["native:A1"]).remove == ["A1"]
            && model.plan(streaming: true, enabled: []).remove == ["A1"],
          "EC13 native airplay: a speaker leaves when switched off or when streaming stops")

    check(model.apply(.state(id: "A1", .connecting, retryMs: nil)) == nil
            && model.apply(.state(id: "A1", .playing, retryMs: nil)) == .playingChanged("native:A1", playing: true),
          "native airplay: playing is reported once")
    model.apply(.gone(id: "A1", name: nil))
    check(model.rows(enabled: []).contains("native:A1") && model.access("A1") == .open && model.soundingIDs == ["native:A1"],
          "EC12 native airplay: a speaker that plays on after its announcement lapsed stays listed and chosen")
    check(model.apply(.error(id: "A1", .connectionLost, retryMs: 1000, detail: "Lost")) == nil
            && model.apply(.state(id: "A1", .retrying, retryMs: 1000)) == .playingChanged("native:A1", playing: false),
          "native airplay: a lost speaker the sender retries stays on")
    check(!model.rows(enabled: []).contains("native:A1") && model.soundingIDs.isEmpty,
          "EC12 native airplay: gone and not playing, it leaves the list and holds nothing back")
    check(model.apply(.error(id: "A1", .notResponding, retryMs: nil, detail: "Unreachable")) == .turnOff("native:A1", .unreachable)
            && model.apply(.error(id: "A1", .pairingRequired, retryMs: nil, detail: "")) == .turnOff("native:A1", .needsCode)
            && model.apply(.error(id: "A1", .wrongPassword, retryMs: nil, detail: "")) == .turnOff("native:A1", .needsPassword)
            && model.apply(.error(id: "A1", .unsupported, retryMs: 5, detail: "")) == .turnOff("native:A1", .cantPlay)
            && model.apply(.error(id: "A1", .incompatible, retryMs: 5, detail: "")) == .turnOff("native:A1", .cantPlay)
            && model.apply(.error(id: "A1", .timingUnavailable, retryMs: nil, detail: "")) == .turnOff("native:A1", .timingUnavailable),
          "native airplay: an error the sender won't retry turns the speaker off, with why")
    model.removed("A1")
    check(model.apply(.state(id: "A1", .playing, retryMs: nil)) == nil
            && model.apply(.error(id: "A1", .notResponding, retryMs: nil, detail: "")) == nil,
          "native airplay: events of a removed speaker are passed over")

    // P169: a second AirPlay 2 speaker waits for the one slot, switched on,
    // and the sender starts it once the other one stops.
    model.apply(.appeared(receiver("G7", "Loft", .available)))
    model.added("G7")
    let refused = model.apply(.error(id: "G7", .oneAirPlay2, retryMs: nil, detail: "one AirPlay 2 speaker at a time"))
    check(refused == .waiting("native:G7", true) && model.apply(.state(id: "G7", .failed, retryMs: nil)) == nil
            && model.isWaiting("G7") && model.plan(streaming: true, enabled: ["native:G7"]).isEmpty,
          "P169 native airplay: a speaker refused the AirPlay 2 slot stays on and in the sender", "\(String(describing: refused))")
    check(model.apply(.error(id: "G7", .oneAirPlay2, retryMs: nil, detail: "")) == nil && model.isWaiting("G7"),
          "P169 native airplay: refused again while it waits, it posts no second notice")
    check(model.apply(.state(id: "G7", .connecting, retryMs: nil)) == .waiting("native:G7", false) && !model.isWaiting("G7")
            && model.apply(.state(id: "G7", .playing, retryMs: nil)) == .playingChanged("native:G7", playing: true),
          "P169 native airplay: once the slot frees, the sender starts it and the row stops saying it waits")
    _ = model.apply(.error(id: "G7", .oneAirPlay2, retryMs: nil, detail: ""))
    check(model.apply(.error(id: "G7", .timingUnavailable, retryMs: nil, detail: "")) == .turnOff("native:G7", .timingUnavailable)
            && !model.isWaiting("G7"),
          "P169 native airplay: a waiting speaker that then can't get PTP timing goes off and no longer waits")
    _ = model.apply(.error(id: "G7", .oneAirPlay2, retryMs: nil, detail: ""))
    model.removed("G7")
    check(!model.isWaiting("G7"), "P169 native airplay: switched off or streaming stopped, it no longer waits")
    check(UIStrings().nativeAirPlayOneAirPlay2(name: "Loft")
            == "AudioSync plays one AirPlay 2 speaker at a time, so Loft waits. It starts once the other one stops.",
          "P169 native airplay: the waiting notice reads as on Windows")
    let lets = "Lets AudioSync play to AirPlay 2 speakers, stereo pairs and home theater groups once you quit and reopen it."
    let stops = "AudioSync stops playing to AirPlay 2 speakers, stereo pairs and home theater groups once you quit and reopen it."
    check(UIStrings().airPlay2Note(on: false, onAtLaunch: true) == stops
            && UIStrings().airPlay2Note(on: true, onAtLaunch: true) == lets
            && UIStrings().airPlay2Note(on: true, onAtLaunch: false) == lets
            && UIStrings().airPlay2Note(on: false, onAtLaunch: false) == lets,
          "AirPlay 2 switch: its note says AirPlay 2 stops only once turned off from a launch that runs it, as on Windows")
    check(UIStrings().nativeAirPlayWaitingTitle(name: "Loft") == "Loft waiting"
            && UIStrings().rowNote(cast: nil, waitingForAirPlay2: true, target: nil) == "Waiting for the other AirPlay 2 speaker"
            && UIStrings().rowNote(cast: nil, waitingForAirPlay2: false, target: nil) == nil,
          "P169 native airplay: the notice's title and the row's note, as on Windows")
    check(UIStrings().nativeAirPlayTimingUnavailable(name: "Loft")
            == "Loft needs PTP timing, which isn't available, so it's off. Another app may be using ports 319 and 320.",
          "native airplay: the PTP text reads as on Windows")
    var titled = NoticeList()
    titled.post("body", subject: "s", title: "Loft waiting")
    check(titled.newest?.title == "Loft waiting" && titled.newest?.text == "body", "notices: a notice can carry a title")

    // A receiver the sender reaches over AirPlay 2 (the core sets this only
    // with the AirPlay 2 switch on) names AirPlay 2 as its kind, the way a
    // Cast row names Google Cast; every other AirPlay row stays "AirPlay".
    var paths = NativeAirPlayModel()
    paths.apply(.appeared(NativeReceiver(id: "H8", name: "Loft", model: "", availability: .available, reason: nil,
                                         hints: ["airplay", "airplay2", "over-airplay2"], overAirPlay2: true)))
    paths.apply(.appeared(NativeReceiver(id: "I9", name: "Den", model: "", availability: .available, reason: nil,
                                         hints: ["raop", "airplay", "airplay2"])))
    check(paths.reachedOverAirPlay2("H8") && !paths.reachedOverAirPlay2("I9") && !paths.reachedOverAirPlay2("J0"),
          "native airplay: only a receiver the sender reaches over AirPlay 2 is marked so, not one that merely lists AirPlay 2")
    let kinds = UIStrings()
    check(kinds.speakerTag(.airplay, airPlay2: true) == "AirPlay 2" && kinds.speakerTag(.airplay) == "AirPlay"
            && kinds.speakerTag(.cast, airPlay2: true) == "Google Cast"
            && kinds.speakerLabel(name: "Loft", tag: kinds.speakerTag(.airplay, airPlay2: true)) == "Loft, AirPlay 2",
          "native airplay: the kind's tooltip and VoiceOver label say AirPlay 2 for such a row, AirPlay for the rest")
    paths.apply(.changed(NativeReceiver(id: "H8", name: "Loft", model: "", availability: .available, reason: nil,
                                        hints: ["airplay", "airplay2", "over-airplay2", "paired"], overAirPlay2: true,
                                        paired: true)))
    check(paths.isPaired("H8") && !paths.isPaired("I9") && !paths.isPaired("J0"),
          "native airplay: only a receiver AudioSync keeps a pairing for offers Forget Pairing")
    check(kinds.nativePairPINMessage(name: "Loft").hasPrefix("Loft shows a 4-digit PIN on its screen. Enter it to pair; "
                                                              + "after that, it connects without a PIN.")
            && !kinds.nativePairWrongPINMessage(name: "Loft").contains("TV"),
          "native airplay: the PIN prompt reads as on Windows, for an Apple TV and an AirPlay 2 receiver alike")
    paths.apply(.appeared(NativeReceiver(id: "K1", name: "Attic", model: "", availability: .needsPin, reason: nil,
                                         hints: ["airplay", "airplay2", "pairing", "over-airplay2"], overAirPlay2: true)))
    check(paths.turnedOnByUser("K1") == .startPairing("K1") && paths.apply(.pinNeeded(id: "K1")) == .prompt,
          "native airplay: a click on an AirPlay 2 receiver asking for a PIN pairs and prompts, as for an Apple TV")
    check(model.apply(.timeline(latencyMs: 1750, restarted: false)) == .latencyChanged
            && model.apply(.timeline(latencyMs: 1750, restarted: true)) == nil && model.latencyMs == 1750,
          "native airplay: local outputs line up again only when L changes")

    do {
        var starts = NativeSenderStarts()
        let t0 = Date(timeIntervalSince1970: 1_000_000)
        check(starts.mayStart(at: t0), "native airplay: the first reconcile starts the sender")
        check(starts.restartAt == nil, "native airplay: before any start fails, no restart falls due (S101)")
        check(starts.finished(started: false, at: t0), "EC7 native airplay: a failed start is told once")
        check(!starts.mayStart(at: t0.addingTimeInterval(3))
                && starts.mayStart(at: t0.addingTimeInterval(NativeSenderStarts.retrySpacing)),
              "EC7 native airplay: after a failed start, a later reconcile starts it again, not a sooner one")
        let restart = starts.restartAt ?? .distantFuture
        check(restart == t0.addingTimeInterval(NativeSenderStarts.retrySpacing)
                && starts.mayStart(at: restart) && !starts.mayStart(at: restart.addingTimeInterval(-0.001)),
              "EC7 native airplay: the restart falls due exactly when a reconcile may start the sender again (S101)",
              "\(String(describing: starts.restartAt))")
        check(!starts.finished(started: false, at: t0.addingTimeInterval(10)),
              "native airplay: another failure in a row posts no second notice")
        check(!starts.finished(started: true, at: t0.addingTimeInterval(20)) && starts.failedAt == nil
                && starts.mayStart(at: t0.addingTimeInterval(20)) && starts.restartAt == nil,
              "native airplay: a start that works ends the failures")
        check(starts.finished(started: false, at: t0.addingTimeInterval(30)),
              "native airplay: a new failure after a working start is told again")

        var stops = NativeSenderStarts()
        check(!stops.stopped, "native airplay: a running sender shows no problem")
        stops.senderFailed()
        check(!stops.mayStart(at: t0.addingTimeInterval(3600)),
              "EC7 native airplay: a sender that stopped after an internal error doesn't restart by itself")
        var failedThenStopped = NativeSenderStarts()
        _ = failedThenStopped.finished(started: false, at: t0)
        failedThenStopped.senderFailed()
        check(failedThenStopped.restartAt == nil,
              "EC7 native airplay: a sender that stopped schedules no restart; only Retry starts one (S101)")
        check(stops.stopped, "EC7 native airplay: it shows as failed, with Retry")
        stops.retry()
        check(stops.mayStart(at: t0) && !stops.stopped, "EC7 native airplay: Retry starts a new sender at once")

        var up = NativeSenderStarts()
        check(!up.isUp, "native airplay: before a start works, the sender isn't up for the Local Network hint")
        _ = up.finished(started: true, at: t0)
        check(up.isUp, "native airplay: a start that works brings the sender up for the Local Network hint")
        up.senderFailed()
        check(!up.isUp, "native airplay: a sender that stopped is down for the Local Network hint")
        up.retry()
        _ = up.finished(started: false, at: t0)
        check(!up.isUp, "native airplay: a start that failed leaves the sender down for the Local Network hint")
    }
    do {
        let mac = NativeReceiver(id: "02005E005340", name: "studio mac", model: "Mac14,3", availability: .available,
                                 reason: nil, hints: ["raop", "airplay"])
        let tv = NativeReceiver(id: "02005E005341", name: "Den TV", model: "AppleTV3,2", availability: .available,
                                reason: nil, hints: ["raop", "apple-tv"])
        var model = NativeAirPlayModel()
        model.computerName = "Studio Mac"
        model.apply(.appeared(mac))
        model.apply(.appeared(tv))
        check(model.rows(enabled: []) == [NativeAirPlayID.app(tv.id)] && model.soundingIDs == [NativeAirPlayID.app(tv.id)],
              "native airplay: this Mac's own AirPlay Receiver is never listed (S100)", "\(model.rows(enabled: []))")
        model.computerName = "Office Mac"
        model.apply(.changed(mac))
        check(model.rows(enabled: []).count == 2, "native airplay: a receiver named otherwise than this Mac is listed")
        var unnamed = NativeAirPlayModel()
        unnamed.apply(.appeared(mac))
        check(unnamed.rows(enabled: []) == [NativeAirPlayID.app(mac.id)], "native airplay: with no computer name known, nothing is hidden")
    }
    do {
        var failing = NativeAirPlayModel()
        failing.added("A1")
        check(failing.apply(.failed(detail: "panic at src/ffi/airplay.rs:42")) == .senderFailed,
              "EC7 native airplay: the sender's failure event makes the app drop it and offer Retry")
        check(NativeSpeakerEvent.failed(detail: "").logLine(name: { _ in nil })
                == "[AudioSync] native AirPlay: the sender stopped after an internal error",
              "native airplay: the failure's log line")
        check(NativeSpeakerEvent.failed(detail: "panic at airplay/src/sender/driver.rs:42").logLine(name: { _ in nil })
                == "[AudioSync] native AirPlay: the sender stopped after an internal error (panic at airplay/src/sender/driver.rs:42)",
              "native airplay: the failure's log line says where the panic happened (S205)")
    }

    // PIN pairing and passwords: only the user's click pairs or prompts.
    do {
        func listed(_ entries: [(String, NativeReceiver.Availability)]) -> NativeAirPlayModel {
            var m = NativeAirPlayModel()
            for (id, availability) in entries { m.apply(.appeared(receiver(id, id, availability))) }
            return m
        }
        check(NativeAirPlayModel.isPin("0123") && !NativeAirPlayModel.isPin("")
                && !NativeAirPlayModel.isPin("12a4") && !NativeAirPlayModel.isPin("12 34")
                && !NativeAirPlayModel.isPin("\u{FF11}\u{FF12}"),
              "native pairing: a PIN is ASCII digits only")

        var m = listed([("A1", .available), ("B2", .needsPin), ("C3", .needsPassword)])
        check(m.turnedOnByUser("A1") == nil, "native pairing: a click on a receiver able to play just joins")
        check(m.turnedOnByUser("B2") == .startPairing("B2"), "native pairing: a click on an Apple TV asking for a PIN pairs")
        check(m.turnedOnByUser("C3") == .prompt, "native pairing: a click on a receiver with a password asks for it")
        check(m.plan(streaming: true, enabled: ["native:B2", "native:C3"]).isEmpty,
              "native pairing: while pairing or prompting, the speaker waits instead of going off")

        // The PIN: shown on the TV, typed, checked.
        check(m.apply(.pinNeeded(id: "B2")) == .prompt, "native pairing: the TV shows a PIN, a prompt is queued")
        let password = m.nextPrompt()
        check(password == NativePrompt(id: "C3", password: true, wrong: false) && m.nextPrompt() == nil,
              "native pairing: prompts come oldest first, one at a time")
        check(m.answered(password!, code: "secret") == .sendPassword(pair: false)
                && m.plan(streaming: true, enabled: ["native:C3"]).add == ["C3"],
              "native pairing: with its password, a receiver joins")
        let pin = m.nextPrompt()
        check(pin == NativePrompt(id: "B2", password: false, wrong: false), "native pairing: then the PIN prompt opens")
        check(m.answered(pin!, code: "12a4") == .askAgain && m.nextPrompt() == NativePrompt(id: "B2", password: false, wrong: true),
              "native pairing: something that isn't a PIN asks again")
        check(m.answered(NativePrompt(id: "B2", password: false, wrong: true), code: "1234") == .submitPin
                && m.stages["B2"] == .checkingPin,
              "native pairing: a PIN goes to the pairing")
        check(m.apply(.pairingFailed(id: "B2", wrongPin: true, .refused, detail: "")) == .startPairing("B2")
                && m.apply(.pinNeeded(id: "B2")) == .prompt
                && m.nextPrompt() == NativePrompt(id: "B2", password: false, wrong: true),
              "native pairing: a wrong PIN starts a new pairing (a new PIN on the TV) and asks again")
        check(m.answered(NativePrompt(id: "B2", password: false, wrong: true), code: "5678") == .submitPin
                && m.apply(.paired(id: "B2", storeNote: "")) == .paired("native:B2"),
              "native pairing: once paired, the speaker joins")
        check(m.plan(streaming: true, enabled: ["native:B2"]).isEmpty,
              "native pairing: just paired, it waits for the sender's new listing instead of going off")
        m.apply(.changed(receiver("B2", "B2", .available)))
        check(m.plan(streaming: true, enabled: ["native:B2"]).add == ["B2"], "native pairing: listed as able to play, it joins")
        m.added("B2")
        check(m.apply(.error(id: "B2", .pairingRequired, retryMs: nil, detail: "")) == .turnOff("native:B2", .stillNeedsCode),
              "native pairing: refused again right after pairing, it goes off instead of pairing in a loop")
        _ = m.turnedOff("B2")
        m.removed("B2")

        // A password the receiver turns down is asked for again within the attempt.
        m.added("C3")
        check(m.apply(.error(id: "C3", .wrongPassword, retryMs: nil, detail: "")) == .prompt
                && m.nextPrompt() == NativePrompt(id: "C3", password: true, wrong: true),
              "native pairing: a wrong password asks again while the user's attempt runs")
        check(m.answered(NativePrompt(id: "C3", password: true, wrong: true), code: "") == .turnOff(.cancelled)
                && !m.turnedOff("C3"),
              "native pairing: an empty password counts as Cancel; no pairing runs to cancel")
        check(m.plan(streaming: true, enabled: ["native:C3"]).turnOff.isEmpty
                && m.plan(streaming: true, enabled: []).remove == ["C3"],
              "native pairing: switched off, the speaker leaves the sender")
    }
    do {
        // Auto-connect and restored switches never pair or prompt.
        var m = NativeAirPlayModel()
        m.apply(.appeared(receiver("B2", "B2", .available)))
        m.added("B2")
        check(m.apply(.error(id: "B2", .pairingRequired, retryMs: nil, detail: "")) == .turnOff("native:B2", .needsCode),
              "native pairing: a stored pairing refused without the user's click goes off with a notice")
        check(m.apply(.pinNeeded(id: "B2")) == .cancelPairing("B2"),
              "native pairing: a PIN request nobody waits for cancels its pairing")

        // A refused stored pairing pairs again, once per attempt.
        m.removed("B2")
        check(m.turnedOnByUser("B2") == nil, "native pairing: a paired Apple TV just joins")
        m.added("B2")
        m.apply(.changed(receiver("B2", "B2", .needsPin)))   // the sender forgot the pairing it turned down
        check(m.apply(.error(id: "B2", .pairingRequired, retryMs: nil, detail: "")) == .startPairing("B2"),
              "native pairing: within the user's attempt, a refused stored pairing pairs again")
        check(m.apply(.paired(id: "B2", storeNote: "")) == .paired("native:B2"), "native pairing: paired again")
        m.apply(.changed(receiver("B2", "B2", .available)))
        check(m.apply(.state(id: "B2", .playing, retryMs: nil)) == .playingChanged("native:B2", playing: true)
                && m.attempts.isEmpty,
              "native pairing: once it plays, the user's attempt is over")
        check(m.apply(.error(id: "B2", .pairingRequired, retryMs: nil, detail: "")) == .turnOff("native:B2", .needsCode),
              "native pairing: refused later on, it goes off with a notice instead of prompting uninvited")

        // Cancel, and a pairing whose speaker went off.
        var c = NativeAirPlayModel()
        c.apply(.appeared(receiver("B2", "B2", .needsPin)))
        _ = c.turnedOnByUser("B2")
        _ = c.apply(.pinNeeded(id: "B2"))
        let prompt = c.nextPrompt()!
        check(c.answered(prompt, code: nil) == .turnOff(.cancelled) && c.stages["B2"] == .askingPin(wrong: false),
              "native pairing: Cancel turns the speaker off and keeps the stage for the turn-off")
        check(c.turnedOff("B2") && !c.turnedOff("B2"),
              "native pairing: the turn-off after Cancel says the pairing runs, so the app cancels it (once)")
        check(c.apply(.paired(id: "B2", storeNote: "")) == nil && c.apply(.pinNeeded(id: "B2")) == .cancelPairing("B2"),
              "native pairing: a pairing that ends after the speaker went off is passed over or cancelled")
        check(c.answered(prompt, code: "1234") == .ignore, "native pairing: an answer for a speaker turned off meanwhile is ignored")

        _ = c.turnedOnByUser("B2")
        check(c.pairingNotStarted("B2") == .turnOff("native:B2", .unreachable),
              "native pairing: a pairing that can't start turns the speaker off")
        _ = c.apply(.pinNeeded(id: "B2"))
        let again = c.nextPrompt()!
        _ = c.answered(again, code: "1234")
        check(c.pinNotTaken("B2") == .turnOff("native:B2", .pairingFailed),
              "native pairing: a PIN the pairing didn't take turns the speaker off")
        check(c.turnedOff("B2"), "native pairing: turning it off cancels the pairing that waits for the PIN")
        _ = c.turnedOnByUser("B2")
        check(c.apply(.pairingFailed(id: "B2", wrongPin: false, .notResponding, detail: "")) == .turnOff("native:B2", .unreachable)
                && !c.turnedOff("B2"),
              "native pairing: a pairing that failed for another reason turns the speaker off, with nothing left to cancel")
        _ = c.turnedOnByUser("B2")
        check(c.apply(.pairingFailed(id: "B2", wrongPin: false, .busy, detail: "")) == .turnOff("native:B2", .pairingFailed),
              "native pairing: a pairing turned down turns the speaker off")

        // An Apple TV that also has a password.
        _ = c.turnedOnByUser("B2")
        let asked = c.apply(.passwordNeeded(id: "B2", wrong: false))
        let passwordPrompt = c.nextPrompt()
        check(asked == .prompt && passwordPrompt == NativePrompt(id: "B2", password: true, wrong: false)
                && c.answered(passwordPrompt!, code: "pw") == .sendPassword(pair: true)
                && c.stages["B2"] == .pairing(afterWrongPin: false),
              "native pairing: a pairing that needs the password asks for it, then pairs again with it")

        // A prompt nobody waits for any more is taken back: the app closes
        // it instead of leaving a PIN typed late to go nowhere unseen.
        var w = NativeAirPlayModel()
        w.apply(.appeared(receiver("B2", "B2", .needsPin)))
        w.apply(.appeared(receiver("C3", "C3", .needsPassword)))
        _ = w.turnedOnByUser("B2")
        _ = w.apply(.pinNeeded(id: "B2"))
        let shown = w.nextPrompt()!
        _ = w.apply(.timeline(latencyMs: 1800, restarted: false))
        check(w.withdrawPrompt() == nil, "native pairing: a prompt its pairing waits for stays open")
        check(w.apply(.pairingFailed(id: "B2", wrongPin: false, .notResponding, detail: "")) == .turnOff("native:B2", .unreachable)
                && w.withdrawPrompt() == shown && w.withdrawPrompt() == nil,
              "native pairing: a pairing that fails under its open PIN prompt takes the prompt back, once")
        check(w.answered(shown, code: nil) == .ignore && w.answered(shown, code: "1234") == .ignore,
              "native pairing: a prompt taken back is no answer, and a PIN typed into it goes nowhere")

        _ = w.turnedOnByUser("B2")
        _ = w.apply(.pinNeeded(id: "B2"))
        let first = w.nextPrompt()!
        _ = w.turnedOff("B2")
        check(w.withdrawPrompt() == first, "native pairing: a speaker switched off under its open prompt takes the prompt back")
        _ = w.turnedOnByUser("B2")
        _ = w.apply(.pinNeeded(id: "B2"))
        check(w.answered(first, code: nil) == .ignore && w.stages["B2"] == .askingPin(wrong: false),
              "native pairing: the closed prompt's answer never counts for the attempt that asks again")
        let second = w.nextPrompt()
        check(second == NativePrompt(id: "B2", password: false, wrong: false),
              "native pairing: the new attempt's prompt opens next")
        w.apply(.changed(receiver("B2", "B2", .available)))
        w.added("B2")
        check(w.apply(.state(id: "B2", .playing, retryMs: nil)) == .playingChanged("native:B2", playing: true)
                && w.withdrawPrompt() == second,
              "native pairing: a speaker that plays under its open PIN prompt (stored credentials) takes the prompt back")
        _ = w.answered(second!, code: nil)

        _ = w.turnedOnByUser("C3")
        let password = w.nextPrompt()
        check(password == NativePrompt(id: "C3", password: true, wrong: false) && w.withdrawPrompt() == nil,
              "native pairing: a password prompt stays open while its speaker waits for it")
        _ = w.turnedOff("C3")
        check(w.withdrawPrompt() == password, "native pairing: switched off, its password prompt is taken back")

        check(NativeSpeakerEvent.pairingFailed(id: "B2", wrongPin: true, .refused, detail: "turned down").logLine(name: { _ in "Den TV" })
                == "[AudioSync] native AirPlay: Den TV (B2): pairing failed: wrong PIN (turned down)"
                && NativeSpeakerEvent.paired(id: "B2", storeNote: "").logLine(name: { _ in nil })
                == "[AudioSync] native AirPlay: B2: paired, credentials stored",
              "native pairing: log lines name the receiver and never a PIN or password")
    }

    let names: (String) -> String? = { $0 == "A1" ? "Kitchen" : nil }
    check(NativeSpeakerEvent.state(id: "A1", .retrying, retryMs: 2500).logLine(name: names)
            == "[AudioSync] native AirPlay: Kitchen (A1): retrying, trying again in 2.5 s",
          "native airplay: a retrying state's log line")
    check(NativeSpeakerEvent.error(id: "Z9", .notResponding, retryMs: nil, detail: "Unreachable").logLine(name: names)
            == "[AudioSync] native AirPlay: Z9 error: not responding (Unreachable); it needs the user",
          "native airplay: an error's log line")
    check(NativeSpeakerEvent.latency(id: "A1", ms: 1750, receiverFrames: 2205).logLine(name: names)
            == "[AudioSync] native AirPlay: Kitchen (A1) latency: L + d_s = 1750 ms, the receiver reports 2205 frames"
            && NativeSpeakerEvent.timeline(latencyMs: 1750, restarted: true).logLine(name: names)
            == "[AudioSync] native AirPlay: timeline: L = 1750 ms, restarted",
          "native airplay: latency and timeline log lines")
}

check(AudioDevices.isVirtual(transport: kAudioDeviceTransportTypeAirPlay), "devices: macOS's own AirPlay output is hidden by default")

// MARK: Native AirPlay targets — stereo pairs and home theater groups as one row (P176 to P181)
do {
    let pair = "pair:5A1B2C3D", left = "02005E005350", right = "02005E005351"
    func member(_ id: String, _ name: String, of target: String,
                _ availability: NativeReceiver.Availability = .available) -> NativeReceiver {
        NativeReceiver(id: id, name: name, model: "AudioAccessory5,1", availability: availability, reason: nil,
                       hints: ["raop", "airplay", "airplay2"], target: target)
    }
    func target(_ id: String, _ name: String, incomplete: Bool = false) -> NativeReceiver {
        NativeReceiver(id: id, name: name, model: "", availability: .available, reason: nil,
                       hints: ["airplay2", "target"], isTarget: true, incomplete: incomplete)
    }
    func listedPair() -> NativeAirPlayModel {
        var model = NativeAirPlayModel()
        model.apply(.appeared(member(left, "Left", of: pair)))
        model.apply(.appeared(member(right, "Right", of: pair)))
        model.apply(.appeared(target(pair, "Living Room")))
        return model
    }
    let t = UIStrings()
    let app = NativeAirPlayID.app

    check(NativeTarget.kind(pair) == .stereoPair && NativeTarget.kind("theater:9F00") == .homeTheater
            && NativeTarget.kind(left) == nil && NativeTarget.kind("raop:pair:x") == nil,
          "P176 native airplay: a target's ID says its kind; a receiver's never does")
    check(NativeReceiverChange.appeared(member(left, "Left", of: pair)).logLine.hasSuffix("member of " + pair),
          "P176 native airplay: a member's log line names its target")

    var early = NativeAirPlayModel()
    early.apply(.appeared(member(left, "Left", of: pair)))
    check(early.rows(enabled: []).isEmpty,
          "P176 native airplay: a member that names its target is held before the target is listed, so auto-connect never takes it alone")

    var model = listedPair()
    model.apply(.appeared(NativeReceiver(id: "K3", name: "Kitchen", model: "", availability: .available, reason: nil,
                                         hints: [])))
    check(model.rows(enabled: []) == ["native:K3", app(pair)],
          "P176 native airplay: a stereo pair is one row, its members show only as part of it", "\(model.rows(enabled: []))")
    let row = model.targetRow(pair)
    check(row == AirPlayTargetRow(kind: .stereoPair, members: ["Left", "Right"], missing: [], incomplete: false)
            && model.targetRow("K3") == nil,
          "P176 native airplay: the pair's row knows its kind and members; a speaker's row has none")
    check(t.rowNote(cast: nil, waitingForAirPlay2: false, target: row) == "Stereo pair: Left and Right"
            && t.airPlayTargetNote(AirPlayTargetRow(kind: .homeTheater, members: ["Left", "Right", "Sub"], missing: [],
                                                    incomplete: false)) == "Home theater group: Left, Right and Sub"
            && t.airPlayTargetNote(AirPlayTargetRow(kind: .stereoPair, members: [], missing: [], incomplete: false))
                == "Stereo pair",
          "P176 native airplay: the row names the kind and its members, as a Cast group's row does")
    let note = t.rowNote(cast: nil, waitingForAirPlay2: false, target: row) ?? ""
    check(t.rowHint(SpeakerState(enabled: false, streaming: false), note: note).hasPrefix(note),
          "P176 native airplay: VoiceOver reads the pair's note in the row's hint")
    check(t.rowNote(cast: nil, waitingForAirPlay2: true, target: row) == t.waitingForAirPlay2,
          "P169 native airplay: a target waiting for the AirPlay 2 slot says so first")

    let on: Set<String> = [app(pair), app(left)]
    check(model.plan(streaming: true, enabled: on)
            == NativeAirPlayPlan(add: [pair], remove: [], turnOff: [], release: [left]),
          "P176 native airplay: the pair is added as one speaker; a member that was on by itself is switched off for it")
    model.added(left)
    check(model.plan(streaming: true, enabled: on).remove == [left],
          "P176 native airplay: a member playing by itself leaves the sender when its pair is switched on")
    model.removed(left)
    check(model.holdingTarget(right, on: false) == pair && model.holdingTarget("K3", on: false) == nil,
          "P176 native airplay: a member of a listed pair can't be turned on by itself")

    var single = NativeAirPlayModel()
    single.apply(.appeared(NativeReceiver(id: left, name: "Left", model: "", availability: .available, reason: nil,
                                          hints: [])))
    single.added(left)
    single.apply(.changed(member(left, "Left", of: pair)))
    single.apply(.appeared(member(right, "Right", of: pair)))
    single.apply(.appeared(target(pair, "Living Room")))
    let leftOn: Set<String> = [app(left)]
    check(single.rows(enabled: leftOn).contains(app(left)) && single.plan(streaming: true, enabled: leftOn).isEmpty
            && single.holdingTarget(left, on: true) == nil,
          "P176 native airplay: a member left on by itself before its pair was listed keeps playing and keeps its row")
    check(!single.rows(enabled: leftOn).contains(app(right)) && single.rows(enabled: leftOn).contains(app(pair)),
          "P176 native airplay: the member that is off shows as part of the pair")

    var playing = listedPair()
    playing.added(pair)
    _ = playing.apply(.state(id: pair, .playing, retryMs: nil))
    playing.apply(.gone(id: pair, name: "Living Room"))
    playing.apply(.changed(member(left, "Left", of: "")))
    check(playing.rows(enabled: [app(pair)]).contains(app(pair)) && playing.holdingTarget(left, on: false) == pair
            && playing.holdingTarget(left, on: true) == pair && !playing.rows(enabled: [app(left)]).contains(app(left)),
          "P176 native airplay: a pair gone while it plays keeps its row until switched off, and its members can't join meanwhile")
    _ = playing.apply(.state(id: pair, .retrying, retryMs: 1000))
    check(playing.rows(enabled: [app(pair)]).contains(app(pair)),
          "P176 native airplay: a gone pair still in the sender keeps its row while it reconnects")
    playing.removed(pair)
    check(playing.holdingTarget(left, on: false) == nil && playing.rows(enabled: []).contains(app(left))
            && !playing.rows(enabled: []).contains(app(pair)),
          "P176 native airplay: switched off, the gone pair's row goes and its members are speakers again")

    var missing = listedPair()
    missing.apply(.gone(id: right, name: "Right"))
    missing.apply(.changed(target(pair, "Living Room", incomplete: true)))
    check(missing.targetRow(pair)
            == AirPlayTargetRow(kind: .stereoPair, members: ["Left", "Right"], missing: ["Right"], incomplete: true)
            && t.airPlayTargetNote(missing.targetRow(pair)!) == "Stereo pair, missing Right",
          "P181 native airplay: a member out of view is named missing")
    var partner = listedPair()
    partner.apply(.changed(target(pair, "Living Room", incomplete: true)))
    check(partner.targetRow(pair)?.incomplete == true
            && t.airPlayTargetNote(partner.targetRow(pair)!) == "Stereo pair, a speaker is missing"
            && t.airPlayTargetNote(AirPlayTargetRow(kind: .homeTheater, members: ["Left", "Right"],
                                                    missing: ["Left", "Right"], incomplete: true))
                == "Home theater group, missing Left and Right",
          "P181 native airplay: an incomplete target with no member out of view says a speaker is missing")

    var reached = NativeAirPlayModel()
    var over = member(left, "Left", of: pair)
    over.overAirPlay2 = true
    reached.apply(.appeared(over))
    reached.apply(.appeared(target(pair, "Living Room")))
    check(reached.reachedOverAirPlay2(pair) && !listedPair().reachedOverAirPlay2(pair),
          "P177 native airplay: a pair reached over AirPlay 2 through a member is tagged AirPlay 2")

    var theater = NativeAirPlayModel()
    theater.added("theater:9F00")
    check(NativeSpeakerEvent.Problem.playsThroughSpeakers.turnOff == .playsThroughSpeakers
            && theater.apply(.error(id: "theater:9F00", .playsThroughSpeakers, retryMs: nil, detail: ""))
                == .turnOff("native:theater:9F00", .playsThroughSpeakers),
          "P177 native airplay: a group whose Apple TV plays, or an Apple TV whose group plays, goes off")
    let texts = [t.nativeAirPlayPlaysThroughSpeakers(name: "Den TV"), t.nativeAirPlayAppleTVPlays(name: "Den"),
                 t.nativeAirPlayPlaysInGroup(name: "Right", group: "Living Room")]
    check(texts == ["Den TV plays through its home theater speakers, and AudioSync already plays to them, so it's off. "
                        + "Turn the group off to play to the Apple TV itself.",
                    "AudioSync already plays to Den's Apple TV, so the group is off. Turn the Apple TV off to play to the group.",
                    "Right plays as part of Living Room, so it stays off by itself."]
            && !texts.joined().contains("\u{2014}"),
          "P177 native airplay: the Apple TV, its group and a held member each say why they are off")

    var pw = NativeAirPlayModel()
    pw.apply(.appeared(member(left, "Left", of: pair)))
    pw.apply(.appeared(member(right, "Right", of: pair, .needsPassword)))
    pw.apply(.appeared(target(pair, "Living Room")))
    let ask = NativePrompt(id: right, password: true, wrong: false)
    check(pw.turnedOnByUser(pair) == .prompt && pw.nextPrompt() == ask
            && pw.plan(streaming: true, enabled: [app(pair)]).add.isEmpty,
          "P176 native airplay: switched on, a pair asks for its member's password by the member's name and waits for it")
    check(pw.answered(ask, code: "secret") == .sendPassword(pair: false) && pw.promptTargets.isEmpty
            && pw.plan(streaming: true, enabled: [app(pair)]).add == [pair],
          "P176 native airplay: once the member's password is in, the pair starts")
    pw.added(pair)
    let again = NativePrompt(id: right, password: true, wrong: true)
    check(pw.apply(.error(id: pair, .wrongPassword, retryMs: nil, detail: "")) == nil
            && pw.apply(.error(id: right, .wrongPassword, retryMs: nil, detail: "")) == .prompt
            && pw.nextPrompt() == again && pw.promptTargets[right] == pair,
          "P176 native airplay: a member that turns the password down is asked again by name, not the pair")
    check(pw.answered(again, code: nil) == .turnOff(.cancelled) && !pw.turnedOff(pair) && pw.promptTargets.isEmpty,
          "P176 native airplay: cancelling the member's password turns the pair off and drops the prompt")
    var auto = listedPair()
    auto.added(pair)
    check(auto.apply(.error(id: pair, .passwordRequired, retryMs: nil, detail: "")) == nil
            && auto.apply(.error(id: right, .passwordRequired, retryMs: nil, detail: ""))
                == .turnOff(app(pair), .memberNeedsPassword(right)),
          "P176 native airplay: a pair auto-connect turned on goes off when a member asks for a password, naming the member")

    var oneSide = listedPair()
    _ = oneSide.turnedOnByUser(pair)
    oneSide.added(pair)
    _ = oneSide.apply(.state(id: pair, .playing, retryMs: nil))   // Left plays: the attempt is over
    check(oneSide.apply(.error(id: right, .wrongPassword, retryMs: nil, detail: ""))
            == .turnOff(app(pair), .memberNeedsPassword(right)) && !oneSide.passwords.contains(right),
          "P176 native airplay: Left plays, then Right refuses its password: the pair goes off, naming Right, and asks again when turned on")
    oneSide.removed(pair)
    var asksAgain = oneSide
    asksAgain.apply(.changed(member(right, "Right", of: pair, .needsPassword)))
    check(asksAgain.turnedOnByUser(pair) == .prompt
            && asksAgain.nextPrompt() == NativePrompt(id: right, password: true, wrong: false),
          "P176 native airplay: turned on again, the pair asks for the member's password")
    check(t.nativeAirPlayMemberNeedsPassword(member: "Right", target: "Living Room")
            == "Right in Living Room asks for a password, so Living Room is off. Turn Living Room on again to enter it.",
          "P176 native airplay: the notice names the member and its pair")

    var plain = NativeAirPlayModel()
    plain.apply(.appeared(NativeReceiver(id: left, name: "Left", model: "", availability: .available, reason: nil,
                                         hints: [])))
    plain.apply(.appeared(NativeReceiver(id: right, name: "Right", model: "", availability: .available, reason: nil,
                                         hints: [])))
    check(plain.rows(enabled: []) == [app(left), app(right)] && plain.holdingTarget(left, on: false) == nil
            && plain.plan(streaming: true, enabled: [app(left), app(right)]).release.isEmpty
            && plain.targetRow(left) == nil && plain.members(of: pair).isEmpty,
          "P170 native airplay: with the AirPlay 2 switch off no target or member link is listed, and nothing changes")
}

// MARK: Pairing store — plaintext by design, so its folder stays out of backups
do {
    let fm = FileManager.default
    let support = fm.temporaryDirectory.appendingPathComponent("audiosync-pairingstest-\(getpid())", isDirectory: true)
    try? fm.removeItem(at: support)
    let file = NativeAirPlayPairings.file(in: support)
    let folder = NativeAirPlayPairings.folder(in: support)
    let appFolder = NativeAirPlayPairings.appFolder(in: support)
    check(file.lastPathComponent == "airplay-pairings.dat"
            && file.deletingLastPathComponent().standardizedFileURL == folder.standardizedFileURL,
          "pairing store: the file lives in the folder that is marked")
    // Its own folder inside AudioSync's: that one also holds the signing
    // keychain's password on a Mac that signs builds, and must stay in
    // backups. Never "AirPlay", the bundled engine's old folder.
    check(folder.lastPathComponent == "Pairings"
            && folder.deletingLastPathComponent().standardizedFileURL == appFolder.standardizedFileURL
            && appFolder.lastPathComponent == "AudioSync"
            && NativeAirPlayPairings.olderFile(in: support).standardizedFileURL
            == appFolder.appendingPathComponent("airplay-pairings.dat").standardizedFileURL,
          "pairing store: a Pairings folder of its own in AudioSync's folder, where older builds kept the file")
    check(NativeAirPlayPairings.folderValues.isExcludedFromBackup == true,
          "pairing store: the folder is marked excluded from backups")
    let first = (try? NativeAirPlayPairings.prepareFolder(in: support)) != nil
    let again = (try? NativeAirPlayPairings.prepareFolder(in: support)) != nil
    let excluded = (try? folder.resourceValues(forKeys: [.isExcludedFromBackupKey]))?.isExcludedFromBackup
    let appExcluded = (try? appFolder.resourceValues(forKeys: [.isExcludedFromBackupKey]))?.isExcludedFromBackup
    let mode = (try? fm.attributesOfItem(atPath: folder.path))?[.posixPermissions] as? Int
    check(first && again && excluded == true && mode == 0o700,
          "pairing store: the folder is created for its owner only and excluded from backups, also when it exists",
          "created \(first), again \(again), excluded \(String(describing: excluded)), mode \(String(describing: mode))")
    check(appExcluded != true, "pairing store: AudioSync's own folder stays in backups",
          "excluded \(String(describing: appExcluded))")

    // Moving the file older builds kept in AudioSync's folder, once.
    let older = NativeAirPlayPairings.olderFile(in: support)
    let contents = { (url: URL) in (try? Data(contentsOf: url)).map { String(decoding: $0, as: UTF8.self) } }
    let noFile = NativeAirPlayPairings.moveOlderFile(in: support)
    check(noFile == .nothing && !fm.fileExists(atPath: older.path) && !fm.fileExists(atPath: file.path)
            && NativeAirPlayPairings.moveLogLine(noFile) == nil,
          "pairing store: with no older file nothing moves and nothing is created", "\(noFile)")
    fm.createFile(atPath: older.path, contents: Data("older".utf8), attributes: [.posixPermissions: 0o600])
    try? fm.setAttributes([.posixPermissions: 0o600], ofItemAtPath: older.path)
    let moved = NativeAirPlayPairings.moveOlderFile(in: support)
    let movedMode = (try? fm.attributesOfItem(atPath: file.path))?[.posixPermissions] as? Int
    check(moved == .moved && !fm.fileExists(atPath: older.path) && contents(file) == "older" && movedMode == 0o600
            && NativeAirPlayPairings.moveLogLine(moved) != nil,
          "pairing store: an older file moves into the folder, unchanged and still for its owner only",
          "\(moved), older left \(fm.fileExists(atPath: older.path)), \(String(describing: contents(file))), mode \(String(describing: movedMode))")
    let movedAgain = NativeAirPlayPairings.moveOlderFile(in: support)
    check(movedAgain == .nothing && contents(file) == "older",
          "pairing store: once moved, the next launch moves nothing", "\(movedAgain)")
    fm.createFile(atPath: older.path, contents: Data("stale".utf8), attributes: [.posixPermissions: 0o600])
    let kept = NativeAirPlayPairings.moveOlderFile(in: support)
    check(kept == .keptNewer && contents(file) == "older" && contents(older) == "stale"
            && NativeAirPlayPairings.moveLogLine(kept) != nil,
          "pairing store: with both files the one in the folder is kept and the older one is left alone",
          "\(kept), folder \(String(describing: contents(file))), older \(String(describing: contents(older)))")
    try? fm.removeItem(at: support)
}

// MARK: Edge cases (docs/EDGE_CASES.md) — each name starts with its case ID
do {
    let t0 = Date(timeIntervalSince1970: 9_000)
    typealias P = LocalOutputSync.Plan

    // EC1 / EC2: a local speaker's device goes away and comes back.
    check(LocalOutputSync.plan(enabled: ["usb"], active: ["usb"], available: ["usb"]) == P(attach: [], detach: []),
          "EC1 unplug: a playing speaker that is still present is left alone")
    check(LocalOutputSync.plan(enabled: ["usb"], active: [], available: []) == P(attach: [], detach: []),
          "EC1 unplug: a lost speaker stays on and waits while its device is missing")
    check(!StreamingPolicy.isIdle(streaming: true, enabledAirPlay: [], enabledLocal: ["usb"]),
          "EC1 unplug: streaming keeps running while the only speaker is unplugged")
    check(LocalOutputSync.plan(enabled: ["usb"], active: [], available: ["usb", "builtin"]) == P(attach: ["usb"], detach: []),
          "EC1 replug: the speaker is attached again once its device is listed")
    check(LocalOutputSync.plan(enabled: [], active: ["usb"], available: ["usb"]) == P(attach: [], detach: ["usb"]),
          "EC1 switch off: a speaker turned off is detached")
    check(LocalOutputSync.plan(enabled: ["usb"], active: ["usb"], available: ["usb"],
                               bound: ["usb": 52], listed: ["usb": 99]) == P(attach: ["usb"], detach: [], restart: ["usb"]),
          "EC1 new ID: a playing speaker whose device came back under a new ID is restarted on it")
    check(LocalOutputSync.plan(enabled: ["usb"], active: ["usb"], available: ["usb"],
                               bound: ["usb": 52], listed: ["usb": 52]) == P(attach: [], detach: []),
          "EC1 new ID: a speaker still on the listed device ID is left alone")
    check(LocalOutputSync.plan(enabled: ["usb"], active: ["usb"], available: [],
                               bound: ["usb": 52], listed: [:]) == P(attach: [], detach: []),
          "EC1 new ID: a playing speaker whose device isn't listed is left alone until it is")
    check(LocalOutputSync.plan(enabled: [], active: ["usb"], available: ["usb"],
                               bound: ["usb": 52], listed: ["usb": 99]) == P(attach: [], detach: ["usb"]),
          "EC1 new ID: a speaker switched off is detached, not restarted")
    check(LocalOutputSync.plan(enabled: ["bt", "usb"], active: ["usb"], available: ["usb"]) == P(attach: [], detach: []),
          "EC2 Bluetooth: a disconnected headset waits; the other speaker keeps playing")
    check(LocalOutputSync.plan(enabled: ["bt", "usb"], active: ["usb"], available: ["bt", "usb"]) == P(attach: ["bt"], detach: []),
          "EC2 Bluetooth: a reconnected headset is attached again")

    // EC4 / EC9: the capture rate changes under a running session.
    check(CaptureRate.needsRestart(current: 48000, reported: 44100), "EC4 rate: a new capture rate rebuilds the session")
    check(CaptureRate.needsRestart(current: 44100, reported: 96000), "EC4 rate: a higher capture rate rebuilds the session")
    check(!CaptureRate.needsRestart(current: 48000, reported: 48000), "EC4 rate: the same rate keeps playing")
    check(!CaptureRate.needsRestart(current: 48000, reported: 0), "EC4 rate: an unreadable rate keeps playing")

    // EC5: switching the source.
    let music = AudioSource.app(bundleID: "com.apple.Music")
    check(SourceSwitch.decide(from: .system, to: music, streaming: true) == .restart,
          "EC5 source: switching while streaming rebuilds the capture")
    check(SourceSwitch.decide(from: music, to: .input(uid: "mic"), streaming: true) == .restart,
          "EC5 source: switching to an input device while streaming rebuilds the capture")
    check(SourceSwitch.decide(from: .system, to: music, streaming: false) == .remember,
          "EC5 source: switching while stopped only remembers the choice")
    check(SourceSwitch.decide(from: music, to: music, streaming: true) == .none,
          "EC5 source: choosing the same source changes nothing")

    // EC8: every speaker off, then one on again.
    check(StreamingPolicy.isIdle(streaming: true, enabledAirPlay: [], enabledLocal: []),
          "EC8 all off: streaming stops when the last speaker is switched off")
    check(!StreamingPolicy.isIdle(streaming: true, enabledAirPlay: ["a"], enabledLocal: []),
          "EC8 all off: one AirPlay speaker still on keeps streaming")
    check(!StreamingPolicy.isIdle(streaming: false, enabledAirPlay: [], enabledLocal: []),
          "EC8 all off: nothing to stop while stopped")
    check(StreamingPolicy.startsStreaming(turnedOn: true, streaming: false),
          "EC8 all off: turning a speaker on again starts streaming")
    check(!StreamingPolicy.startsStreaming(turnedOn: true, streaming: true)
          && !StreamingPolicy.startsStreaming(turnedOn: false, streaming: false),
          "EC8 all off: only a switch-on while stopped starts streaming")
    check(!StreamingPolicy.hasSpeaker(enabledAirPlay: [], enabledLocal: [])
          && StreamingPolicy.hasSpeaker(enabledAirPlay: [], enabledLocal: ["usb"]),
          "EC8 all off: Start with no speaker on asks for one first")

    // EC10: many speakers.
    typealias S = (name: String, tag: SpeakerTag)
    let many: [S] = (1...8).map { ("Speaker \($0)", SpeakerTag.usb) } + [("MacBook Pro Speakers", .builtIn)]
    let manyAirPlay: [S] = (1...8).reversed().map { ("Room \($0)", SpeakerTag.airplay) }
    let big = SpeakerLayout.split(local: many, airplay: manyAirPlay, tag: { $0.tag }, name: { $0.name })
    check(big.thisMac.count == 1 && big.connected.count == 16
          && big.connected.prefix(8).map(\.name) == (1...8).map { "Room \($0)" },
          "EC10 many speakers: sixteen speakers all listed, in name order")
    let locals = Set((1...8).map { "lc\($0)" })
    check(LocalOutputSync.plan(enabled: locals, active: [], available: locals).attach.count == 8,
          "EC10 many speakers: eight local speakers are attached together")

    // EC11: very long speaker names.
    let long = String(repeating: "Living Room Speaker With A Very Long Name ", count: 6) + "End"
    let longs: [S] = [(long + " B", .airplay), (long + " A", .airplay), ("Den", .usb)]
    let longSplit = SpeakerLayout.split(local: [S](), airplay: longs, tag: { $0.tag }, name: { $0.name })
    check(longSplit.connected.map(\.name) == ["Den", long + " A", long + " B"],
          "EC11 long names: names that differ only at the end still sort and stay whole")

    // EC12: the network to the AirPlay speakers drops.
    check(LocalNetworkHint.decide(access: .allowed, senderUp: true, emptySince: t0,
                                  now: t0.addingTimeInterval(600)) == .none,
          "EC12 network loss: an empty list with Local Network allowed shows no permission hint")
}

// MARK: Track info — artwork scaled to a small JPEG
do {
    func artwork(_ width: Int, _ height: Int) -> CGImage {
        let context = CGContext(data: nil, width: width, height: height, bitsPerComponent: 8, bytesPerRow: 0,
                                space: CGColorSpace(name: CGColorSpace.sRGB)!,
                                bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue)!
        for y in stride(from: 0, to: height, by: 4) {
            let f = CGFloat(y) / CGFloat(height)
            context.setFillColor(red: f, green: 0.4, blue: 1 - f, alpha: 1)
            context.fill(CGRect(x: 0, y: y, width: width, height: 4))
        }
        context.setFillColor(red: 1, green: 0.9, blue: 0.2, alpha: 1)
        context.fillEllipse(in: CGRect(x: width / 4, y: height / 4, width: width / 2, height: height / 2))
        return context.makeImage()!
    }
    func size(_ jpeg: Data?) -> (Int, Int)? {
        guard let jpeg, let source = CGImageSourceCreateWithData(jpeg as CFData, nil),
              let image = CGImageSourceCreateImageAtIndex(source, 0, nil) else { return nil }
        return (image.width, image.height)
    }
    let big = artwork(1200, 800)
    let jpeg = TrackMetadata.jpeg(big)
    let (w, h) = size(jpeg) ?? (0, 0)
    check(jpeg.map { $0.starts(with: [0xFF, 0xD8]) && $0.count <= TrackMetadata.artworkLimit } == true,
          "artwork: a JPEG within the artwork limit", "\(jpeg?.count ?? -1) bytes, \(w)×\(h)")
    check(w > 0 && w <= TrackMetadata.artworkSide && h <= TrackMetadata.artworkSide && abs(Double(w) / Double(h) - 1.5) < 0.05,
          "artwork: at most 600 px a side, aspect kept", "\(w)×\(h)")
    let tight = TrackMetadata.jpeg(big, maxBytes: 2500)
    check(tight.map { $0.count <= 2500 } == true && (size(tight)?.0 ?? .max) < w,
          "artwork: a tighter limit gives a smaller image", "\(tight?.count ?? -1) bytes")
    check(TrackMetadata.jpeg(big, maxBytes: 100) == nil, "artwork: nothing when not even the smallest image fits")
    check(size(TrackMetadata.jpeg(artwork(100, 100))).map { $0 == (100, 100) } == true,
          "artwork: a small image isn't scaled up")
    check(TrackMetadata.jpeg(NSImage(cgImage: big, size: NSSize(width: 1200, height: 800))) != nil,
          "artwork: an NSImage (a player's artwork, AudioSync's icon) is encoded too")
}

// MARK: Track info — a player with no track is not an error
check(NowPlayingMonitor.meansNoTrack(-1728) && NowPlayingMonitor.meansNoTrack(-1712),
      "AppleScript: no current track (-1728) and a timeout (-1712) read as no track, not logged")
check(!NowPlayingMonitor.meansNoTrack(-1743) && !NowPlayingMonitor.meansNoTrack(nil)
      && !NowPlayingMonitor.meansNoTrack(-2753),
      "AppleScript: Automation refused (-1743) and other errors are still logged")

// MARK: Track info — what AirPlay receivers with a screen show
do {
    typealias Track = NowPlayingMonitor.Track
    let cover = NSImage(size: NSSize(width: 1, height: 1))
    let spotify = NowPlayingMonitor.spotify, music = NowPlayingMonitor.music
    func track(_ app: String, playing: Bool = true, artwork: NSImage? = cover) -> Track {
        Track(bundleID: app, appName: "Player", title: "Song", artist: "Band", album: "Record",
              isPlaying: playing, artwork: artwork)
    }
    // With no track that counts, receivers get AudioSync's labels: its name as
    // the title, the source's name as the source picker shows it as the
    // artist (a stand-in when there is no usable name) and a short album
    // label. None is blank: a Samsung TV shows no text at all when the artist
    // is blank.
    func noTrack(_ artist: String) -> TrackMetadata.Fields {
        TrackMetadata.Fields(title: "AudioSync", artist: artist, album: "From your Mac")
    }
    let untitled = noTrack("Live audio")
    let system = "All System Audio"   // the source picker's label for the whole system
    func decide(_ t: Track?, _ source: AudioSource, _ name: String? = nil) -> ReceiverInfo {
        ReceiverInfo.decide(track: t, source: source, sourceName: name, untitled: untitled)
    }
    let song = TrackMetadata.Fields(title: "Song", artist: "Band", album: "Record")
    check(decide(track(spotify), .system, system) == ReceiverInfo(fields: song, artwork: .track),
          "receiver info: a playing track, with its artwork")
    check(decide(track(spotify, artwork: nil), .system, system).artwork == .wait,
          "receiver info: Spotify's artwork is on its way: what is shown stays until it arrives")
    check(decide(track(music, artwork: nil), .system, system).artwork == .neutral,
          "receiver info: a Music track without artwork gets AudioSync's icon")
    let bare = Track(bundleID: music, appName: "Music", title: "Song", artist: "", album: "",
                     isPlaying: true, artwork: cover)
    check(decide(bare, .system, system).fields == .init(title: "Song", artist: "", album: ""),
          "receiver info: a track's own blank fields go out as its player gave them")
    check(decide(nil, .system, system) == ReceiverInfo(fields: noTrack(system), artwork: .neutral),
          "receiver info: whole system with nothing playing: the source picker's label as the artist")
    check(decide(track(spotify, playing: false), .system, system)
            == ReceiverInfo(fields: noTrack(system), artwork: .neutral),
          "receiver info: whole system as source: a paused player's track isn't what plays")
    check(decide(track(spotify, playing: false), .app(bundleID: spotify), "Spotify").fields == song,
          "receiver info: a paused track still shows while its player is the source")
    check(decide(track(spotify, playing: false), .app(bundleID: music), "Music").fields == noTrack("Music"),
          "receiver info: another player's paused track doesn't show")
    check(decide(nil, .app(bundleID: "com.google.Chrome"), "Google Chrome")
            == ReceiverInfo(fields: noTrack("Google Chrome"), artwork: .neutral),
          "receiver info: an app without track info: the neutral title and the app's name")
    check(decide(nil, .input(uid: "BuiltInMicrophoneDevice"), "MacBook Pro Microphone")
            == ReceiverInfo(fields: noTrack("MacBook Pro Microphone"), artwork: .neutral),
          "receiver info: an input device: the neutral title and the device's name")
    check(decide(nil, .app(bundleID: "com.example.gone"), "com.example.gone").fields == untitled,
          "receiver info: an app whose name isn't known shows no bundle ID as the artist")
    check(decide(nil, .input(uid: "unplugged")).fields == untitled
            && decide(nil, .app(bundleID: "com.example.app"), " ").fields == untitled,
          "receiver info: a source without a usable name gets the stand-in as the artist")
    // With no track that counts, no field goes out blank, whatever the source
    // and whatever name the caller has for it.
    let named: [(AudioSource, String?)] = [
        (.system, system), (.system, nil),
        (.input(uid: "BuiltInMicrophoneDevice"), "MacBook Pro Microphone"), (.input(uid: "unplugged"), nil),
        (.app(bundleID: "com.google.Chrome"), "Google Chrome"), (.app(bundleID: "com.example.gone"), "com.example.gone"),
        (.app(bundleID: "com.example.app"), " "), (.app(bundleID: "com.example.app"), nil)]
    let blanks = named.filter { entry in
        let f = decide(nil, entry.0, entry.1).fields
        return [f.title, f.artist, f.album].contains { $0.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty }
    }
    check(blanks.isEmpty, "receiver info: no track that counts: no field goes out blank, whatever the source",
          blanks.map { "\($0.0) \($0.1 ?? "(no name)")" }.joined(separator: ", "))

    // Spotify's artwork gets `artworkWait` to arrive; then AudioSync's icon
    // replaces the last track's cover.
    let waiting = decide(track(spotify, artwork: nil), .system)
    check(waiting.waited(ReceiverInfo.artworkWait - 0.1).artwork == .wait,
          "receiver info: the last cover stays while Spotify's artwork may still come")
    check(waiting.waited(ReceiverInfo.artworkWait) == ReceiverInfo(fields: song, artwork: .neutral),
          "receiver info: artwork that never comes gives way to AudioSync's icon")
    check(decide(track(spotify), .system).waited(60).artwork == .track
            && decide(nil, .system).waited(60).artwork == .neutral,
          "receiver info: waiting changes nothing but a wait for artwork")

    // Right after Now Playing starts polling, its track is from before: wait
    // for its first reading, at most `readingWait`, then the track is unknown.
    check(ReceiverInfo.awaitsReading(hasReading: false, pollingFor: 0),
          "receiver info: no reading yet since polling started: wait for it")
    check(!ReceiverInfo.awaitsReading(hasReading: false, pollingFor: ReceiverInfo.readingWait),
          "receiver info: a reading that doesn't come in time isn't waited for")
    check(!ReceiverInfo.awaitsReading(hasReading: true, pollingFor: 0)
            && !ReceiverInfo.awaitsReading(hasReading: false, pollingFor: .infinity),
          "receiver info: a current reading, or none coming (not polling), is not waited for")
}

// MARK: Track info on AudioSync's own sender — a new track never goes out with the last one's cover
do {
    let song = TrackMetadata.Fields(title: "Song", artist: "Band", album: "Record")
    let cover = Data([0xFF, 0xD8, 0xFF, 0x01]), nextCover = Data([0xFF, 0xD8, 0xFF, 0x02])
    var playing = NativeNowPlaying()
    playing.setTrack(song)
    playing.setArtwork(cover)
    check(playing.fields == song && playing.artwork == cover, "native track info: the track goes out with its artwork")
    playing.setTrack(song)
    check(playing.fields == song && playing.artwork == cover,
          "native track info: the same track sent again (a resend) keeps its artwork")
    let next = TrackMetadata.Fields(title: "Next", artist: "Band", album: "Record")
    playing.setTrack(next)
    check(playing.fields == next && playing.artwork == nil,
          "native track info: a new track drops the last one's artwork: it goes out without the wrong cover")
    playing.setArtwork(nextCover)
    check(playing.fields == next && playing.artwork == nextCover, "native track info: the new track's artwork goes out once it comes")
    playing.setTrack(TrackMetadata.Fields(title: "Next", artist: "Other band", album: "Record"))
    let otherArtist = playing.artwork
    playing.setArtwork(nextCover)
    playing.setTrack(TrackMetadata.Fields(title: "Next", artist: "Other band", album: "Single"))
    check(otherArtist == nil && playing.artwork == nil,
          "native track info: another artist or album is a new track too")
}

// MARK: Track info the app gives its own sender — what goes out and what a repeat skips (SentTrackInfo)
do {
    let cover = NSImage(size: NSSize(width: 1, height: 1)), nextCover = NSImage(size: NSSize(width: 1, height: 1))
    let icon = NSImage(size: NSSize(width: 1, height: 1))
    let song = TrackMetadata.Fields(title: "Song", artist: "Band", album: "Record")
    let next = TrackMetadata.Fields(title: "Next", artist: "Band", album: "Record")
    let live = TrackMetadata.Fields(title: "AudioSync", artist: "All System Audio", album: "From your Mac")
    let start = Date(timeIntervalSinceReferenceDate: 0)
    func shows(_ fields: TrackMetadata.Fields, _ artwork: ReceiverInfo.Artwork) -> ReceiverInfo {
        ReceiverInfo(fields: fields, artwork: artwork)
    }
    func nothing(_ u: SentTrackInfo.Update) -> Bool { u.fields == nil && u.picture == nil && u.waitUntil == nil }

    // A track with artwork, then the same again, then the next one with the
    // same cached image (one album): the new track dropped it, so it goes again.
    var sent = SentTrackInfo()
    var u = sent.update(shows(song, .track), artwork: cover, icon: icon, now: start)
    check(u.fields == song && u.picture === cover && u.waitUntil == nil,
          "sent track info: a new track goes out with its artwork")
    u = sent.update(shows(song, .track), artwork: cover, icon: icon, now: start + 1)
    check(nothing(u), "sent track info: the same track again (a model update, a recheck) gives the sender nothing")
    u = sent.update(shows(next, .track), artwork: cover, icon: icon, now: start + 2)
    check(u.fields == next && u.picture === cover,
          "sent track info: the next track with the same cached image sends it again (the new track dropped it)")
    u = sent.update(shows(next, .track), artwork: nextCover, icon: icon, now: start + 3)
    check(u.fields == nil && u.picture === nextCover,
          "sent track info: another image for the same track goes out")
    sent = SentTrackInfo()
    u = sent.update(shows(next, .track), artwork: nextCover, icon: icon, now: start + 4)
    check(u.fields == next && u.picture === nextCover,
          "sent track info: a new sender (a resend) gets the track and its artwork again")

    // AudioSync's icon in place of artwork: once per track, again for the next.
    var icons = SentTrackInfo()
    u = icons.update(shows(live, .neutral), artwork: nil, icon: icon, now: start)
    check(u.fields == live && u.picture === icon && u.waitUntil == nil,
          "sent track info: no track: AudioSync's labels go out with its icon")
    u = icons.update(shows(live, .neutral), artwork: nil, icon: icon, now: start + 1)
    check(nothing(u), "sent track info: the icon isn't sent again for the same fields")
    u = icons.update(shows(song, .neutral), artwork: nil, icon: icon, now: start + 2)
    check(u.fields == song && u.picture === icon,
          "sent track info: a track without artwork after the icon sends the icon again (the new track dropped it)")
    u = icons.update(shows(song, .track), artwork: cover, icon: icon, now: start + 3)
    check(u.fields == nil && u.picture === cover,
          "sent track info: artwork that comes for a track shown with the icon goes out")
    u = icons.update(shows(next, .neutral), artwork: nil, icon: icon, now: start + 4)
    check(u.fields == next && u.picture === icon && u.waitUntil == nil,
          "sent track info: a track without artwork after one with: the icon goes out at once")

    // With no icon to give, none counts as given: it goes out once there is one.
    var noIcon = SentTrackInfo()
    u = noIcon.update(shows(live, .neutral), artwork: nil, icon: nil, now: start)
    check(u.fields == live && u.picture == nil, "sent track info: no icon to give: the fields go out alone")
    u = noIcon.update(shows(live, .neutral), artwork: nil, icon: icon, now: start + 1)
    check(u.fields == nil && u.picture === icon, "sent track info: the icon goes out once there is one")

    // Spotify's artwork comes a moment after the track: the track goes out
    // alone, and the app looks again when the wait ends.
    let waitEnds = start + 10 + ReceiverInfo.artworkWait
    var spotify = SentTrackInfo()
    _ = spotify.update(shows(song, .track), artwork: cover, icon: icon, now: start)
    u = spotify.update(shows(next, .wait), artwork: nil, icon: icon, now: start + 10)
    check(u.fields == next && u.picture == nil && u.waitUntil == waitEnds,
          "sent track info: artwork on its way: the track goes out alone, with when the wait ends")
    u = spotify.update(shows(next, .wait), artwork: nil, icon: icon, now: start + 12)
    check(u.fields == nil && u.picture == nil && u.waitUntil == waitEnds,
          "sent track info: a look during the wait sends nothing and keeps the wait's end")
    u = spotify.update(shows(next, .track), artwork: nextCover, icon: icon, now: start + 13)
    check(u.fields == nil && u.picture === nextCover && u.waitUntil == nil,
          "sent track info: artwork that arrives during the wait goes out")
    var late = SentTrackInfo()
    _ = late.update(shows(song, .track), artwork: cover, icon: icon, now: start)
    _ = late.update(shows(next, .wait), artwork: nil, icon: icon, now: start + 10)
    u = late.update(shows(next, .wait), artwork: nil, icon: icon, now: waitEnds)
    check(u.fields == nil && u.picture === icon && u.waitUntil == nil,
          "sent track info: artwork that never comes: the icon goes out once the wait is over")
    u = late.update(shows(next, .wait), artwork: nil, icon: icon, now: waitEnds + 1)
    check(nothing(u), "sent track info: after the wait, the icon isn't sent again")
}

// MARK: When track info goes to the app's own sender (TrackInfoFeed, the AppDelegate wiring)
do {
    let cover = NSImage(size: NSSize(width: 1, height: 1)), icon = NSImage(size: NSSize(width: 1, height: 1))
    let song = TrackMetadata.Fields(title: "Song", artist: "Band", album: "Record")
    let next = TrackMetadata.Fields(title: "Next", artist: "Band", album: "Record")
    let start = Date(timeIntervalSinceReferenceDate: 0)
    var looked = 0
    func playing(_ fields: TrackMetadata.Fields, _ artwork: ReceiverInfo.Artwork = .track)
        -> () -> (info: ReceiverInfo, artwork: NSImage?) {
        { looked += 1; return (ReceiverInfo(fields: fields, artwork: artwork), artwork == .track ? cover : nil) }
    }
    func look(_ feed: inout TrackInfoFeed, _ fields: TrackMetadata.Fields, at now: Date, sending: Bool = true,
              pollingSince: Date? = nil, hasReading: Bool = true,
              artwork: ReceiverInfo.Artwork = .track) -> TrackInfoFeed.Step {
        feed.send(sending: sending, now: now, pollingSince: pollingSince, hasReading: hasReading, icon: icon,
                  shows: playing(fields, artwork))
    }
    func nothing(_ s: TrackInfoFeed.Step) -> Bool { s.fields == nil && s.picture == nil && s.recheckAt == nil }

    // No receiver shows track info: nothing goes out, nothing is read.
    var feed = TrackInfoFeed()
    check(!feed.modelUpdated(sending: false) && feed.sent == nil,
          "track info feed: a model update without an AirPlay speaker sends nothing")
    check(nothing(look(&feed, song, at: start, sending: false)) && looked == 0 && feed.sent == nil,
          "track info feed: a track change without an AirPlay speaker sends nothing and reads nothing")
    check(!feed.senderStarted(sending: false) && feed.sent == nil,
          "track info feed: a sender that starts without an AirPlay speaker gets nothing")

    // Sending starts with a model update; later updates leave it to track changes.
    check(feed.modelUpdated(sending: true), "track info feed: the model update that starts sending sends everything")
    var step = look(&feed, song, at: start)
    check(step.fields == song && step.picture === cover, "track info feed: the first look sends the track and its artwork")
    check(!feed.modelUpdated(sending: true), "track info feed: another model update while sending sends nothing")
    check(nothing(look(&feed, song, at: start + 1)), "track info feed: the same track again gives the sender nothing")
    step = look(&feed, next, at: start + 2)
    check(step.fields == next, "track info feed: a track change sends the new track")

    // A new sender (a pipe that starts) gets everything again, unchanged or not.
    check(feed.senderStarted(sending: true), "track info feed: a new sender asks for a send")
    step = look(&feed, next, at: start + 3)
    check(step.fields == next && step.picture === cover,
          "track info feed: a new sender gets the unchanged track and its artwork again")

    // Sending stops: what was given is forgotten, so the next start sends it all.
    check(!feed.modelUpdated(sending: false) && feed.sent == nil,
          "track info feed: sending stops: what was given is forgotten")
    check(feed.modelUpdated(sending: true), "track info feed: sending again sends everything again")
    check(look(&feed, next, at: start + 4).fields == next, "track info feed: the track goes out again after a stop")

    // Right after polling starts, the track is from before: hold it back, look
    // again once when the reading wait ends, and only then read what plays.
    var fresh = TrackInfoFeed()
    looked = 0
    let readingEnds = start + ReceiverInfo.readingWait
    step = look(&fresh, song, at: start + 0.1, pollingSince: start, hasReading: false)
    check(step.fields == nil && step.recheckAt == readingEnds && looked == 0,
          "track info feed: before the first reading, nothing goes out and a look is scheduled at the wait's end")
    step = look(&fresh, song, at: start + 0.2, pollingSince: start, hasReading: false)
    check(nothing(step) && looked == 0, "track info feed: the wait's end is scheduled once per date")
    fresh.recheckFired(at: start + 99)
    check(fresh.sent?.recheckAt == readingEnds, "track info feed: a stale look leaves the scheduled one alone")
    fresh.recheckFired(at: readingEnds)
    check(fresh.sent?.recheckAt == nil, "track info feed: the scheduled look clears its date as it runs")
    step = look(&fresh, song, at: readingEnds, pollingSince: start, hasReading: false)
    check(step.fields == song && looked == 1, "track info feed: at the wait's end the track goes out without a reading")
    step = look(&fresh, next, at: readingEnds + 1.1, pollingSince: readingEnds + 1, hasReading: true)
    check(step.fields == next, "track info feed: a reading ends the wait at once")

    // Artwork on its way: the track goes out alone, and one look is scheduled.
    var spotify = TrackInfoFeed()
    let artworkEnds = start + 10 + ReceiverInfo.artworkWait
    step = look(&spotify, song, at: start + 10, artwork: .wait)
    check(step.fields == song && step.picture == nil && step.recheckAt == artworkEnds,
          "track info feed: artwork on its way: the track goes alone and a look is scheduled at the wait's end")
    check(nothing(look(&spotify, song, at: start + 11, artwork: .wait)),
          "track info feed: a look during the artwork wait schedules nothing new")
    spotify.recheckFired(at: artworkEnds)
    step = look(&spotify, song, at: artworkEnds, artwork: .wait)
    check(step.fields == nil && step.picture === icon && step.recheckAt == nil,
          "track info feed: the scheduled look at the wait's end sends AudioSync's icon")
}

// MARK: Windows and the view model (WindowRules.swift)
do {
    let screen = CGRect(x: 0, y: 0, width: 1440, height: 875)
    let second = CGRect(x: 1440, y: 0, width: 1920, height: 1055)
    check(WindowPlacement.isReachable(CGRect(x: 200, y: 100, width: 860, height: 600), screens: [screen]),
          "windows: a saved frame on the screen is used")
    check(!WindowPlacement.isReachable(CGRect(x: 1600, y: 100, width: 860, height: 600), screens: [screen]),
          "windows: a frame saved on a display that is gone is centered instead")
    check(WindowPlacement.isReachable(CGRect(x: 1600, y: 100, width: 860, height: 600), screens: [screen, second]),
          "windows: a frame on the second display is used while it is attached")
    check(!WindowPlacement.isReachable(CGRect(x: 1400, y: 100, width: 860, height: 600), screens: [screen]),
          "windows: a sliver of 40 points can't be grabbed, so the window is centered")
    check(WindowPlacement.isReachable(CGRect(x: 1320, y: 100, width: 860, height: 600), screens: [screen]),
          "windows: 120 points on screen are enough to grab the title bar")
    check(!WindowPlacement.isReachable(CGRect(x: 200, y: 100, width: 860, height: 600), screens: []),
          "windows: no screen at all is never reachable")

    // A PIN or password prompt taken back closes once it is the frontmost
    // modal window, even when another alert sits over it at first.
    var withdrawal = PromptWithdrawal()
    check(withdrawal.withdraw(open: true, frontmost: true) == .close && !withdrawal.pending,
          "prompt withdrawal: a frontmost prompt closes at once")
    check(withdrawal.withdraw(open: true, frontmost: false) == .wait && withdrawal.pending,
          "prompt withdrawal: a prompt under another alert waits to close")
    check(withdrawal.withdraw(open: true, frontmost: false) == .none && withdrawal.pending,
          "prompt withdrawal: taken back again while it waits, no second retry starts")
    check(withdrawal.retry(open: true, frontmost: false) == .wait && withdrawal.pending,
          "prompt withdrawal: it keeps waiting while the other alert is open")
    check(withdrawal.retry(open: true, frontmost: true) == .close && !withdrawal.pending,
          "prompt withdrawal: once the other alert ends, the prompt closes")
    check(withdrawal.retry(open: true, frontmost: true) == .none,
          "prompt withdrawal: nothing pending, a stray retry closes nothing")
    _ = withdrawal.withdraw(open: true, frontmost: false)
    check(withdrawal.retry(open: false, frontmost: false) == .none && !withdrawal.pending,
          "prompt withdrawal: a prompt that went meanwhile stops the retries")
    _ = withdrawal.withdraw(open: true, frontmost: false)
    withdrawal.ended()
    check(!withdrawal.pending && withdrawal.retry(open: true, frontmost: true) == .none,
          "prompt withdrawal: answered before it could close, the next prompt is never closed in its place")
    check(withdrawal.withdraw(open: false, frontmost: false) == .none && !withdrawal.pending,
          "prompt withdrawal: no prompt open, nothing to close")

    let apps = [(bundleID: "com.apple.Music", name: "Music"), (bundleID: "com.spotify.client", name: "Spotify")]
    let inputs = [(uid: "mic-1", name: "MacBook Pro Microphone"), (uid: "", name: "No UID")]
    func list(_ chosen: AudioSource) -> [SourceList.Entry] {
        SourceList.entries(systemName: "All System Audio", apps: apps, inputs: inputs, chosen: chosen,
                           chosenAppName: { $0 == "com.apple.Safari" ? "Safari" : $0 })
    }
    check(list(.system).map(\.id) == ["", "com.apple.Music", "com.spotify.client", "input:mic-1"],
          "sources: the whole system, then apps, then inputs with a UID", "\(list(.system).map(\.id))")
    check(list(.system).first == SourceList.Entry(id: "", name: "All System Audio", isInput: false)
            && list(.system).last?.isInput == true,
          "sources: the whole system's entry has the empty id; inputs are marked")
    check(list(.app(bundleID: "com.apple.Safari")).map(\.name)
            == ["All System Audio", "Music", "Safari", "Spotify", "MacBook Pro Microphone"],
          "sources: a chosen app that isn't running stays listed, sorted among the apps")
    check(list(.input(uid: "gone")).count == 4,
          "sources: a chosen input device that is unplugged is not listed")
    check(list(.app(bundleID: "com.spotify.client")).count == 4,
          "sources: a chosen app that is running is listed once")

    let present: Set<String> = ["ap:a", "lc:b"]
    func summary(_ members: [String], on: Set<String>) -> (available: Int, allOn: Bool) {
        GroupSummary.summarize(members: members, isAvailable: present.contains, isEnabled: on.contains)
    }
    check(summary(["ap:a", "lc:b", "lc:gone"], on: ["ap:a", "lc:b"]) == (2, true),
          "groups: on when every present member is on; absent members don't count")
    check(summary(["ap:a", "lc:b"], on: ["ap:a"]) == (2, false),
          "groups: off while a present member is off")
    check(summary(["lc:gone"], on: ["lc:gone"]) == (0, false),
          "groups: a group with no member present is never on")

    var tuned = Equalizer.Settings()
    tuned.enabled = true; tuned.preamp = -3; tuned.gains[0] = 6
    check(StoredEqualizer.decode(StoredEqualizer.encode(tuned)) == tuned,
          "equalizer storage: saved settings come back unchanged")
    check(StoredEqualizer.decode(nil) == Equalizer.Settings()
            && StoredEqualizer.decode(Data("not json".utf8)) == Equalizer.Settings(),
          "equalizer storage: nothing saved, or unreadable data, gives the defaults")
    var fewBands = tuned
    fewBands.gains = [1, 2, 3]
    check(StoredEqualizer.decode(StoredEqualizer.encode(fewBands)) == Equalizer.Settings(),
          "equalizer storage: settings with another band count give the defaults")
}

// MARK: Settings kept between launches (AppSettings.swift)
do {
    typealias Keys = AppSettings.Keys
    final class Stored { var values: [String: Any] = [:] }
    let stored = Stored()
    let store = SettingsStore(read: { stored.values[$0] }, write: { value, key in stored.values[key] = value })

    let fresh = store.load()
    check(fresh == AppSettings() && fresh.source == .system && fresh.master == 100 && fresh.defaultDelayMs == 2000
            && fresh.enabledAirPlay.isEmpty && fresh.enabledLocal.isEmpty && fresh.groups.isEmpty
            && !fresh.showVirtualDevices && fresh.appearance == .system && fresh.whenSourceQuits == .stop,
          "settings: nothing stored gives the defaults (whole system, master 100, delays 2000 ms)")
    check(!fresh.airPlay2, "settings: AirPlay 2 is off when nothing is stored (P2)")

    var wanted = AppSettings()
    wanted.source = .input(uid: "usb-mic")
    wanted.enabledAirPlay = ["airplay-1", "airplay-2"]
    wanted.enabledLocal = ["BuiltInSpeakerDevice"]
    wanted.master = 72.5
    wanted.localVolumes = ["BuiltInSpeakerDevice": 60, "usb:out": 33.3]
    wanted.localDelays = ["BuiltInSpeakerDevice": 1820, "usb:out": 0]
    wanted.airplayVolumes = ["airplay-1": 45]
    wanted.groups = [SpeakerGroup(id: "g1", name: "Living Room", members: ["ap:airplay-1", "lc:usb:out"])]
    wanted.autoConnect = ["ap:airplay-2"]
    wanted.showVirtualDevices = true
    wanted.appearance = .dark
    wanted.whenSourceQuits = .system
    wanted.airPlay2 = true
    store.save(source: wanted.source)
    store.save(ids: wanted.enabledAirPlay, forKey: Keys.enabledAirPlay)
    store.save(ids: wanted.enabledLocal, forKey: Keys.enabledLocal)
    store.save(master: wanted.master)
    store.save(levels: wanted.localVolumes, forKey: Keys.localVolumes)
    store.save(levels: wanted.localDelays, forKey: Keys.localDelays)
    store.save(levels: wanted.airplayVolumes, forKey: Keys.airplayVolumes)
    store.save(groups: wanted.groups)
    store.save(ids: wanted.autoConnect, forKey: Keys.autoConnect)
    store.save(showVirtualDevices: wanted.showVirtualDevices)
    store.save(appearance: wanted.appearance)
    store.save(whenSourceQuits: wanted.whenSourceQuits)
    store.save(airPlay2: wanted.airPlay2)
    check(store.load() == wanted, "settings: every saved setting comes back unchanged")
    check(stored.values[Keys.airPlay2] as? Bool == true, "settings: the AirPlay 2 switch is stored as a Bool")
    store.save(airPlay2: false)
    check(!store.load().airPlay2, "settings: AirPlay 2 switched off again reads back off")
    stored.values[Keys.airPlay2] = "yes"
    check(!store.load().airPlay2, "settings: an AirPlay 2 value that isn't a Bool reads as off")
    store.save(airPlay2: true)

    check(!AppSettings.airPlay2On(setting: false, environment: [:])
            && !AppSettings.airPlay2On(setting: false, environment: ["AUDIOSYNC_AIRPLAY2": "0"]),
          "settings: AirPlay 2 stays off with the switch off and no AUDIOSYNC_AIRPLAY2=1 (P170)")
    check(AppSettings.airPlay2On(setting: true, environment: [:])
            && AppSettings.airPlay2On(setting: false, environment: ["AUDIOSYNC_AIRPLAY2": "1"]),
          "settings: the switch or AUDIOSYNC_AIRPLAY2=1 turns AirPlay 2 on")
    check(stored.values[Keys.enabledAirPlay] as? [String] == ["airplay-1", "airplay-2"],
          "settings: a set of speakers is stored as a sorted list")
    store.save(source: .system)
    check(stored.values[Keys.source] as? String == "", "settings: the whole system is stored as an empty source (S12)")

    stored.values = [Keys.master: Double.nan,
                     Keys.localVolumes: ["a": 150.0, "b": -5.0, "c": Double.nan, "d": "loud", "e": 42] as [String: Any],
                     Keys.localDelays: ["a": 25_000.0, "b": -1.0, "c": Double.infinity, "d": 1820.0],
                     Keys.airplayVolumes: ["x": 101.0],
                     Keys.groups: Data("not json".utf8),
                     Keys.enabledLocal: ["lc", 7] as [Any],
                     Keys.appearance: "sepia"]
    let damaged = store.load()
    check(damaged.master == 100, "settings: a master that isn't a finite number starts at 100")
    check(damaged.localVolumes == ["a": 100, "b": 0, "e": 42],
          "settings: volumes are clamped to 0…100, anything else is dropped (S76)")
    check(damaged.localDelays == ["a": 20_000, "b": 0, "d": 1820],
          "settings: delays are clamped to 0…20 000 ms, anything else is dropped (S77)")
    check(damaged.airplayVolumes == ["x": 100] && damaged.groups.isEmpty && damaged.enabledLocal == ["lc"]
            && damaged.appearance == .system,
          "settings: unreadable groups, IDs or appearance fall back to the defaults")
    stored.values = [Keys.master: 130]
    check(store.load().master == 100, "settings: a master above 100 is clamped")

    stored.values = [Keys.legacyLocalGains: ["a": 3.49, "b": 150.0, "c": Double.nan] as [String: Any],
                     Keys.legacyDelayMs: 1500.0]
    let migrated = store.load()
    check(approx(migrated.localVolumes["a"] ?? 0, 32.68, 0.01) && migrated.localVolumes["b"] == 100
            && migrated.localVolumes.count == 2,
          "settings: older linear gains become slider positions (3.49 % → 32.68, 150 % → 100, S75)")
    let saved = AppSettings.levels(stored.values[Keys.localVolumes], { $0 })
    check(saved == migrated.localVolumes && stored.values[Keys.legacyLocalGains] == nil,
          "settings: the converted volumes are saved under the current key and the old key is removed")
    check(store.load() == migrated, "settings: a second launch reads the converted volumes as they are")
    check(migrated.defaultDelayMs == 1500, "settings: an older single delay is where local speakers start (S77)")
    stored.values = [Keys.legacyDelayMs: 90_000.0]
    check(store.load().defaultDelayMs == 20_000, "settings: an older single delay is clamped to 20 000 ms")
    stored.values = [Keys.localVolumes: ["a": 70.0], Keys.legacyLocalGains: ["a": 3.49]]
    check(store.load().localVolumes == ["a": 70] && stored.values[Keys.legacyLocalGains] == nil,
          "settings: stored positions win over older gains, which are removed")

    // Existing installs read these exact strings; a rename loses the setting.
    let pinned: [(String, String)] = [
        (Keys.source, "audioSourceBundleID"), (Keys.enabledAirPlay, "enabledAirPlayIds"),
        (Keys.enabledLocal, "enabledLocalUIDs"), (Keys.master, "masterVolume"),
        (Keys.localVolumes, "localVolumePosByUID"), (Keys.localDelays, "localDelayMsByUID"),
        (Keys.airplayVolumes, "airPlayVolumeByID"), (Keys.groups, "speakerGroups"),
        (Keys.autoConnect, "autoConnectSpeakers"), (Keys.showVirtualDevices, "showVirtualOutputDevices"),
        (Keys.appearance, "appearance"), (Keys.whenSourceQuits, "whenSourceQuits"), (Keys.equalizer, "equalizerSettings"),
        (Keys.legacyDelayMs, "delayMs"), (Keys.legacyLocalGains, "localGainByUID"),
        (Keys.retiredEngineChoice, "engineImpl"), (Keys.retiredAirPlayChoice, "airplayImpl"),
        (Keys.localOffsets, "localOffsetMsByUID"),
        (Keys.airPlayDelay, "airPlayDelayMs"), (Keys.nativeAirPlayAdjustment, "nativeAirPlayAdjustmentMs"),
        (Keys.localDelaysMigrated, "localDelayModelMigrated"), (Keys.speakerNames, "speakerNamesByKey"),
        (Keys.airPlay2, "airPlay2Enabled"),
    ]
    let renamed = pinned.filter { $0.0 != $0.1 }.map(\.1)
    check(renamed.isEmpty, "settings: every UserDefaults key keeps the name existing installs use",
          "changed: \(renamed)")
}

// MARK: Local delay model — the built-in speakers are the reference, offsets always apply (LocalDelay.swift, S172-S181)
do {
    typealias Keys = AppSettings.Keys
    // The examples of S176: the built-in speakers R, a USB speaker at +150 ms,
    // a Bluetooth speaker at −250 ms and, with AirPlay chosen, L = 2000 ms.
    let r = "BuiltInSpeakerDevice", usb = "usb:out", bt = "bt:out", tv = "hdmi:tv"
    let offsets: [String: Double] = [usb: 150, bt: -250]
    // Default arguments can't use the constants above, hence the literal and the nil.
    func holds(_ uids: [String], airPlay: Bool = false, l: Double = 2000,
               reference: String? = "BuiltInSpeakerDevice", offsets given: [String: Double]? = nil) -> [String: Double] {
        LocalDelay.holdBacks(takingPart: uids, reference: reference, offsets: given ?? offsets,
                             airPlayChosen: airPlay, latencyMs: l)
    }

    check(holds([r, usb]) == [r: 0, usb: 150],
          "local delay: with only local outputs, a speaker's delay is applied", "\(holds([r, usb]))")
    check(holds([r, bt]) == [r: 250, bt: 0] && holds([r, usb, bt]) == [r: 250, usb: 400, bt: 0],
          "local delay: a Bluetooth speaker at −250 ms makes the built-in speakers wait 250 ms",
          "\(holds([r, bt])), \(holds([r, usb, bt]))")

    // S173: which listed output is the reference.
    let builtIn = kAudioDeviceTransportTypeBuiltIn
    let speakerTerminal = UInt32(kAudioStreamTerminalTypeSpeaker)
    let headphoneTerminal = UInt32(kAudioStreamTerminalTypeHeadphones)
    func device(_ uid: String, _ transport: UInt32, terminal: UInt32? = nil) -> AudioDevices.Device {
        AudioDevices.Device(id: 1, name: uid, uid: uid, transport: transport, outputTerminal: terminal)
    }
    let speakers = device(r, builtIn, terminal: speakerTerminal)
    let headphones = device("BuiltInHeadphoneOutputDevice", builtIn, terminal: headphoneTerminal)
    let usbDevice = device(usb, kAudioDeviceTransportTypeUSB)
    let btDevice = device(bt, kAudioDeviceTransportTypeBluetooth)
    check(LocalDelay.reference(in: [usbDevice, speakers, btDevice]) == r,
          "local delay: the built-in speakers are the reference")
    check(LocalDelay.reference(in: [headphones, speakers, usbDevice]) == r
            && LocalDelay.shownOffset(headphones.uid, reference: r, offsets: [:]) == 0,
          "local delay: the built-in headphone output doesn't take the reference from the built-in speakers")
    let jack = device("AppleHDAEngineOutput:1B,0,1,1:0", builtIn, terminal: headphoneTerminal)
    let unread = device("AppleHDAEngineOutput:1B,0,1,2:0", builtIn)
    check(LocalDelay.reference(in: [usbDevice, jack]) == jack.uid
            && LocalDelay.reference(in: [unread, usbDevice]) == unread.uid,
          "local delay: a lone built-in output that reports no speaker terminal is the reference")
    check(LocalDelay.reference(in: [usbDevice, btDevice]) == nil && LocalDelay.reference(in: []) == nil
            && holds([usb, bt], reference: nil) == [usb: 400, bt: 0],
          "local delay: no built-in output, no reference")
    // Switched off, R isn't among those taking part; the choice ignores the switches.
    check(LocalDelay.reference(in: [speakers, usbDevice, btDevice]) == r
            && holds([usb, bt]) == [usb: 400, bt: 0]
            && LocalDelay.shownOffset(r, reference: r, offsets: [r: 300]) == nil,
          "local delay: a switched-off reference is still the reference", "\(holds([usb, bt]))")

    check(holds([r, usb, bt], airPlay: true) == [r: 2000, usb: 2150, bt: 1750]
            && holds([bt], airPlay: true) == [bt: 1750],
          "local delay: AirPlay chosen adds the AirPlay delay on top", "\(holds([r, usb, bt], airPlay: true))")
    let nativeL = AirPlayDelay(adjustmentMs: 250).latencyMs(senderMs: 1750)
    check(nativeL == 2000 && holds([r, usb, bt], airPlay: true, l: nativeL) == [r: 2000, usb: 2150, bt: 1750],
          "local delay: the native sender adds the latency it reports plus the AirPlay adjustment", "L \(nativeL)")

    // S177: the running renderers whose hold-back changed get it, the others no call.
    let at2000 = holds([r, usb, bt], airPlay: true, l: 2000)
    let at2500 = holds([r, usb, bt], airPlay: true, l: 2500)
    check(LocalDelay.retimes(applied: at2000, holdBacks: at2500) == [r: 2500, usb: 2650, bt: 2250]
            && LocalDelay.retimes(applied: at2000, holdBacks: at2000).isEmpty,
          "local delay: a new AirPlay delay re-times the local speakers",
          "\(LocalDelay.retimes(applied: at2000, holdBacks: at2500))")
    let withoutBT = holds([r, usb]), withBT = holds([r, usb, bt])
    check(LocalDelay.retimes(applied: withoutBT, holdBacks: withBT) == [r: 250, usb: 400] && withBT[bt] == 0
            && LocalDelay.retimes(applied: withBT, holdBacks: withoutBT) == [r: 0, usb: 150],
          "local delay: a late speaker switched on or off re-times the running others",
          "\(LocalDelay.retimes(applied: withoutBT, holdBacks: withBT))")
    var edited = offsets
    edited[bt] = -300
    check(LocalDelay.retimes(applied: withBT, holdBacks: holds([r, usb, bt], offsets: edited)) == [r: 300, usb: 450],
          "local delay: editing the lowest offset re-times the others",
          "\(LocalDelay.retimes(applied: withBT, holdBacks: holds([r, usb, bt], offsets: edited)))")

    // S178, S181: the AirPlay delay field.
    var native = AirPlayDelay(adjustmentMs: 250)
    let shown = native.latencyMs(senderMs: 1750)
    let followed = native.latencyMs(senderMs: 1800)
    let entered = native.enter(2100, senderMs: 1800)
    let ignored = !native.enter(.nan, senderMs: 1800) && native.adjustmentMs == 300
    check(shown == 2000 && followed == 2050 && entered && ignored && native.adjustmentMs == 300
            && native.latencyMs(senderMs: 1800) == 2100,
          "AirPlay delay: the native sender's field shows L + A, follows a new L and an edit stores the difference",
          "shown \(shown), followed \(followed), A \(native.adjustmentMs)")

    // S179: the stored absolute delays become offsets, once.
    func migrate(_ stored: [String: Double], legacy: Double? = nil, enabled: Set<String> = [],
                 listed: [String], reference: String? = "BuiltInSpeakerDevice") -> LocalDelay.Migration? {
        LocalDelay.migrate(storedDelays: stored, legacyDelayMs: legacy, enabled: enabled, listed: listed,
                           reference: reference)
    }
    /// The hold-backs after migration `m` with every output in `uids` taking part.
    func after(_ m: LocalDelay.Migration?, _ uids: [String], airPlay: Bool) -> [String: Double]? {
        m.map { holds(uids, airPlay: airPlay, l: $0.airPlayDelayMs, offsets: $0.offsets) }
    }
    let tuned = migrate([r: 2000, usb: 2100, bt: 1800], listed: [r, usb, bt])
    check(tuned?.airPlayDelayMs == 2000 && tuned?.offsets == [usb: 100, bt: -200]
            && after(tuned, [r, usb, bt], airPlay: true) == [r: 2000, usb: 2100, bt: 1800]
            && after(tuned, [r, usb, bt], airPlay: false) == [r: 200, usb: 300, bt: 0],
          "migration: a setup tuned for AirPlay sounds the same with AirPlay", "\(String(describing: tuned))")
    let late = migrate([r: 2500, bt: 2250], listed: [r, bt])
    check(late?.airPlayDelayMs == 2500 && late?.offsets == [bt: -250]
            && after(late, [r, bt], airPlay: true) == [r: 2500, bt: 2250],
          "migration: the reference's old delay becomes the AirPlay delay", "\(String(describing: late))")
    let noR = migrate([tv: 2400], listed: [r, tv])
    let noRLegacy = migrate([tv: 2400], legacy: 2200, listed: [r, tv])
    check(noR?.airPlayDelayMs == 2000 && noR?.offsets == [tv: 400]
            && after(noR, [r, tv], airPlay: true) == [r: 2000, tv: 2400]
            && after(noR, [r, tv], airPlay: false) == [r: 0, tv: 400]
            && noRLegacy?.airPlayDelayMs == 2200 && noRLegacy?.offsets == [tv: 200],
          "migration: R without a stored delay uses D0", "\(String(describing: noR)), \(String(describing: noRLegacy))")
    let untuned = migrate([r: 2300], enabled: [bt], listed: [r, usb])
    check(untuned?.airPlayDelayMs == 2300 && untuned?.offsets == [usb: -300, bt: -300]
            && after(untuned, [r, usb], airPlay: true) == [r: 2300, usb: 2000],
          "migration: an untuned output that played at D0 sounds the same with AirPlay",
          "\(String(describing: untuned))")
    let together = migrate([:], legacy: 0, listed: [r, usb])
    let below = migrate([r: 300], legacy: 500, listed: [r, usb])
    check(together?.airPlayDelayMs == 2000 && together?.offsets == [usb: 0]
            && after(together, [r, usb], airPlay: true) == [r: 2000, usb: 2000]
            && after(together, [r, usb], airPlay: false) == [r: 0, usb: 0]
            && below?.airPlayDelayMs == 2000 && below?.offsets == [usb: 200],
          "migration: with a legacy delay below 1000 ms, untuned outputs keep playing together",
          "\(String(describing: together)), \(String(describing: below))")
    check(tuned?.nativeAdjustmentMs == 0 && late?.nativeAdjustmentMs == 0 && noRLegacy?.nativeAdjustmentMs == 0,
          "migration: the native sender's adjustment starts at 0")
    let small = migrate([r: 250, bt: 0, usb: 400], listed: [r, bt, usb])
    check(small?.airPlayDelayMs == 2000 && small?.offsets == [bt: -250, usb: 150]
            && after(small, [r, bt, usb], airPlay: false) == [r: 250, bt: 0, usb: 400]
            && after(small, [r, bt, usb], airPlay: true) == [r: 2000, bt: 1750, usb: 2150],
          "migration: small values keep their alignment and take the default AirPlay delay",
          "\(String(describing: small))")
    let clamped = migrate([r: 2500, usb: 0], listed: [r, usb])
    check(clamped?.airPlayDelayMs == 2500 && clamped?.offsets == [usb: -2000]
            && after(clamped, [r, usb], airPlay: true) == [r: 2500, usb: 500],
          "migration: an offset below −2000 ms is clamped", "\(String(describing: clamped))")

    final class Stored {
        var values: [String: Any] = [:]
        var writes = 0
    }
    let stored = Stored()
    let store = SettingsStore(read: { stored.values[$0] },
                              write: { value, key in stored.values[key] = value; stored.writes += 1 })
    stored.values = [Keys.localDelays: [r: 2000.0, usb: 2100.0], Keys.legacyDelayMs: 1800.0,
                     Keys.enabledLocal: [bt]]
    let first = store.migrateLocalDelays(listed: [r, usb], reference: r)
    let writes = stored.writes
    let second = store.migrateLocalDelays(listed: [r, usb, tv], reference: r)
    let loaded = store.load()
    check(first?.offsets == [usb: 100, bt: -200] && first?.airPlayDelayMs == 2000
            && second == nil && stored.writes == writes
            && stored.values[Keys.localDelays] as? [String: Double] == [r: 2000, usb: 2100]
            && stored.values[Keys.legacyDelayMs] as? Double == 1800
            && stored.values[Keys.localDelaysMigrated] as? Bool == true
            && stored.values[Keys.airPlayDelay] as? Double == 2000
            && loaded.localOffsets == [usb: 100, bt: -200] && loaded.airPlayDelay == AirPlayDelay()
            && loaded.localDelaysMigrated,
          "migration: runs once and leaves the old values in place",
          "first \(String(describing: first)), second \(String(describing: second))")
    stored.values = [Keys.localOffsets: [usb: 25_000.0, bt: -9000.0, "x": Double.nan, "y": "loud"] as [String: Any],
                     Keys.airPlayDelay: -5.0, Keys.nativeAirPlayAdjustment: 30_000.0]
    let damaged = store.load()
    stored.values = [Keys.airPlayDelay: 3000.0, Keys.nativeAirPlayAdjustment: "soon"]
    let unreadable = store.load()
    check(damaged.localOffsets == [usb: 20_000, bt: -2000]
            && damaged.airPlayDelay.adjustmentMs == 20_000 && !damaged.localDelaysMigrated
            && unreadable.airPlayDelay == AirPlayDelay(),
          "settings: offsets are kept in −2000…20 000 ms and the AirPlay delay in its range (S175, S178)",
          "\(damaged.localOffsets), \(damaged.airPlayDelay), \(unreadable.airPlayDelay)")

    // A start that fails in the audio core says so, not that capture failed.
    let t = UIStrings()
    let coreMessage = t.streamingStartMessage(for: StreamEngine.StartError.coreUnavailable)
    check(coreMessage == t.engineSetupFailed && coreMessage.contains("open it again")
            && coreMessage != t.streamingStartMessage(for: SourceCapture.CaptureError.tapNotCreated(-1)),
          "start error: the audio core failing to set up asks to reopen the app", coreMessage)

    // S181: what the rows show and the fields take.
    check(t.offsetValue(ms: 150) == "+150 ms" && t.offsetValue(ms: -250) == "−250 ms" && t.offsetValue(ms: 0) == "0 ms"
            && t.offsetValue(ms: -0.4) == "0 ms",
          "offset text: +150 ms, −250 ms and 0 ms",
          "\(t.offsetValue(ms: 150)), \(t.offsetValue(ms: -250)), \(t.offsetValue(ms: 0))")
    let read: [(String, Double?)] = [
        ("+150", 150), ("−250", -250), ("-250", -250), ("150", 150), (" 40 ", 40), ("0", 0),
        ("-5000", -2000), ("99999999999999999999", 20_000), ("", nil), ("+", nil), ("−", nil), ("1.5", nil),
        ("abc", nil), ("nan", nil), ("inf", nil), ("--5", nil), ("+-5", nil), ("5-", nil), ("1 000", nil),
    ]
    let misread = read.filter { LocalDelay.parseOffset($0.0) != $0.1 }.map(\.0)
    check(misread.isEmpty, "offset text: a leading + or − (a hyphen too) is read and other text is ignored",
          "misread: \(misread)")
    check(LocalDelay.shownOffset(r, reference: r, offsets: [r: 120]) == nil
            && LocalDelay.shownOffset(usb, reference: r, offsets: offsets) == 150
            && LocalDelay.shownOffset(tv, reference: r, offsets: offsets) == 0
            && LocalDelay.shownOffset(r, reference: nil, offsets: [r: 120]) == 120,
          "speaker row: the reference has no delay field")
}

// MARK: Speaker names — a name the user gives a speaker, by speaker key (SpeakerNames)
do {
    final class Stored { var values: [String: Any] = [:] }
    let stored = Stored()
    let store = SettingsStore(read: { stored.values[$0] }, write: { value, key in stored.values[key] = value })
    let usb = SpeakerKey.make(id: "usb-uid", airplay: false)
    let homePod = SpeakerKey.make(id: "hp-1", airplay: true)

    var names = store.load().speakerNames
    check(names.shown(key: usb, system: "USB Audio") == "USB Audio" && names.renamed(key: usb) == nil,
          "speaker names: a speaker shows its system name until renamed")

    let changed = names.rename(key: usb, to: "  Desk\nspeaker  ", system: "USB Audio")
    names.rename(key: homePod, to: "Kitchen", system: "HomePod")
    check(changed && names.shown(key: usb, system: "USB Audio") == "Desk speaker"
              && names.shown(key: homePod, system: "HomePod") == "Kitchen",
          "speaker names: a rename shows, trimmed and on one line", "usb: \(names.shown(key: usb, system: "USB Audio"))")

    store.save(speakerNames: names)
    let stored1 = stored.values[AppSettings.Keys.speakerNames] as? [String: String]
    let reloaded = store.load().speakerNames
    check(AppSettings.Keys.speakerNames == "speakerNamesByKey" && stored1 == [usb: "Desk speaker", homePod: "Kitchen"]
              && reloaded == names,
          "speaker names: names are kept by speaker key under speakerNamesByKey and read back after a relaunch",
          "stored: \(String(describing: stored1))")

    let cleared = names.rename(key: usb, to: "   ", system: "USB Audio")
    let same = names.rename(key: homePod, to: "HomePod", system: "HomePod")
    check(cleared && same && names.shown(key: usb, system: "USB Audio") == "USB Audio"
              && names.shown(key: homePod, system: "HomePod 2") == "HomePod 2" && names.custom.isEmpty,
          "speaker names: an empty name, or the system name typed again, brings back the system name")
    store.save(speakerNames: names)
    check(stored.values[AppSettings.Keys.speakerNames] == nil && store.load().speakerNames.custom.isEmpty,
          "speaker names: with no name left the key is removed")

    let unchanged = names.rename(key: usb, to: "", system: "USB Audio")
    let long = names.rename(key: usb, to: String(repeating: "x", count: 80), system: "USB Audio")
    check(!unchanged && long && names.renamed(key: usb)?.count == SpeakerNames.maxLength,
          "speaker names: clearing an unrenamed speaker changes nothing; a long name is cut to 60 characters")

    stored.values[AppSettings.Keys.speakerNames] = ["a": 5, "b": "  ", "c": "Den"] as [String: Any]
    check(store.load().speakerNames.custom == ["c": "Den"],
          "speaker names: a stored entry that isn't a name is skipped")

    let deskTarget = SpeakerRename.Target(id: "usb-uid", key: usb, system: "USB Audio")
    let kitchenTarget = SpeakerRename.Target(id: "hp-1", key: homePod, system: "HomePod")
    var rename = SpeakerRename()
    let opened = rename.start(deskTarget, name: "USB Audio")
    rename.text = "Desk"
    let again = rename.start(deskTarget, name: "USB Audio")
    check(opened == nil && again == nil && rename.text == "Desk" && rename.target == deskTarget,
          "speaker rename: starting a rename shows the name; starting it again keeps the typing")

    let handed = rename.start(kitchenTarget, name: "HomePod")
    check(handed == SpeakerRename.Apply(key: usb, system: "USB Audio", typed: "Desk")
              && rename.target == kitchenTarget && rename.text == "HomePod",
          "speaker rename: renaming another speaker applies the open rename first",
          "handed: \(String(describing: handed))")

    rename.text = "Kitchen"
    let otherCommit = rename.commit(id: "usb-uid")
    let commit = rename.commit(id: "hp-1")
    check(otherCommit == nil && commit == SpeakerRename.Apply(key: homePod, system: "HomePod", typed: "Kitchen")
              && rename.target == nil,
          "speaker rename: Return applies the speaker's own rename once, never another's")

    _ = rename.start(deskTarget, name: "USB Audio")
    rename.drop()
    check(rename.target == nil && rename.commit(id: "usb-uid") == nil,
          "speaker rename: Escape drops the typing, and the focus leaving after it applies nothing")

    _ = rename.start(deskTarget, name: "USB Audio")
    rename.text = "Desk"
    let stillListed = rename.listed(["usb-uid", "hp-1"])
    let gone = rename.listed(["hp-1"])
    let back = rename.listed(["usb-uid", "hp-1"])
    check(stillListed == nil && gone == SpeakerRename.Apply(key: usb, system: "USB Audio", typed: "Desk")
              && back == nil && rename.target == nil,
          "speaker rename: a speaker gone mid-rename applies its typing, and its rename doesn't open again when it returns")
}

// MARK: Delay field — typed text applies on Return or focus loss, once (DelayFieldDraft)
do {
    let t = UIStrings()
    let offset = LocalDelay.sanitizedOffset
    let airPlay = LocalDelay.sanitizedAirPlayDelay
    let signed = t.offsetNumber
    let plain: (Double) -> String = { String(Int($0.rounded())) }

    // The field as the view drives it: keystrokes, then Return and the focus loss after it.
    var saved = 0.0
    var applied: [Double] = []
    var draft = DelayFieldDraft()
    draft.show(saved, format: signed)
    check(draft.text == "0" && !draft.isEditing, "delay field: it starts out showing the saved value")
    for text in ["2", "22", "220"] {
        draft.type(text)
        draft.show(saved, format: signed)   // the view re-syncs while the user types
    }
    check(applied.isEmpty && draft.text == "220" && draft.isEditing,
          "delay field: typing 2, 22, 220 applies nothing and keeps the typed text", "text: \(draft.text)")
    for _ in 0..<2 {   // Return, then the focus loss that follows
        if let v = draft.commit(saved: saved, sanitize: offset, format: signed) { applied.append(v); saved = v }
    }
    check(applied == [220] && draft.text == "+220" && !draft.isEditing,
          "delay field: Return applies the typed value once, and focus loss after it applies nothing more",
          "applied: \(applied), text: \(draft.text)")

    draft.type("abc")
    let bad = draft.commit(saved: saved, sanitize: offset, format: signed)
    draft.type("")
    let empty = draft.commit(saved: saved, sanitize: offset, format: signed)
    check(bad == nil && empty == nil && draft.text == "+220" && !draft.isEditing,
          "delay field: text it can't read applies nothing and shows the saved value again", "text: \(draft.text)")

    draft.type("+0220")
    check(draft.commit(saved: saved, sanitize: offset, format: signed) == nil && draft.text == "+220",
          "delay field: the saved value typed again applies nothing")

    draft.type("-5000")
    check(draft.commit(saved: saved, sanitize: offset, format: signed) == -2000 && draft.text == "−2000",
          "delay field: an offset out of range applies clamped and shows the clamped value", "text: \(draft.text)")

    var air = DelayFieldDraft()
    air.show(1800, format: plain)
    air.type("99999")
    check(air.commit(saved: 1800, sanitize: airPlay, format: plain) == 20_000 && air.text == "20000",
          "delay field: the AirPlay delay clamps to 20 000 ms on commit", "text: \(air.text)")

    air.type("12")
    let stepped = air.step(10, saved: 1800, sanitize: airPlay, format: plain)
    check(stepped == 22 && air.text == "22" && !air.isEditing
              && air.commit(saved: 22, sanitize: airPlay, format: plain) == nil,
          "delay field: a step applies on top of the typed value once, so a later focus loss applies nothing more",
          "stepped: \(String(describing: stepped)), text: \(air.text)")
    air.type("")
    let unread = air.step(-100, saved: 1810, sanitize: airPlay, format: plain)
    let resting = air.step(100, saved: 1710, sanitize: airPlay, format: plain)
    let floor = air.step(-100, saved: 0, sanitize: airPlay, format: plain)
    check(unread == 1710 && resting == 1810 && floor == nil && air.text == "0",
          "delay field: a step without readable typing applies on top of the saved value; at the range's end it applies nothing",
          "unread: \(String(describing: unread)), resting: \(String(describing: resting)), text: \(air.text)")
    air.show(1810, format: plain)

    air.show(1900, format: plain)
    check(air.text == "1900", "delay field: a new saved value shows when nothing is being typed")

    // The draft cue: typed text that isn't the saved value, until it applies or reverts.
    var cue = DelayFieldDraft()
    cue.show(120, format: signed)
    check(!cue.isPending(saved: 120, format: signed), "delay field cue: the saved value shows no draft")
    cue.type("+120")
    check(!cue.isPending(saved: 120, format: signed), "delay field cue: the saved value as typed shows no draft")
    cue.type("22")
    let typing = cue.isPending(saved: 120, format: signed)
    _ = cue.commit(saved: 120, sanitize: offset, format: signed)
    check(typing && !cue.isPending(saved: 22, format: signed),
          "delay field cue: typing shows a draft and Return clears it")
    cue.type("abc")
    let unreadable = cue.isPending(saved: 22, format: signed)
    _ = cue.commit(saved: 22, sanitize: offset, format: signed)
    check(unreadable && !cue.isPending(saved: 22, format: signed),
          "delay field cue: unreadable text shows a draft until Return restores the saved value")
    cue.type("5")
    cue.discard(saved: 22, format: signed)   // Escape
    check(!cue.isPending(saved: 22, format: signed), "delay field cue: Escape clears the draft")
    cue.type("22")
    let step = cue.step(10, saved: 22, sanitize: offset, format: signed)
    check(step == 32 && !cue.isPending(saved: 32, format: signed), "delay field cue: a step clears the draft")
    cue.type("40")
    check(cue.isPending(saved: 32, format: signed) && cue.isPending(saved: 50, format: signed),
          "delay field cue: a saved value changing under the typing keeps the draft")

    // Quit with typing not yet applied: the quit steps drop every draft
    // (AppModel.dropDelayDrafts), so 400 typed over a tuned -270 isn't saved.
    var store: [String: Double] = ["usb": -270, "hdmi": 40, "dock": 10]
    var fields = ["usb": DelayFieldDraft(), "hdmi": DelayFieldDraft(), "dock": DelayFieldDraft()]
    func commit(_ id: String) {
        if let v = fields[id]?.commit(saved: store[id] ?? 0, sanitize: offset, format: signed) { store[id] = v }
    }
    fields["usb"]?.type("4"); fields["usb"]?.type("40"); fields["usb"]?.type("400")
    fields["hdmi"]?.type("abc")
    fields["dock"]?.type("77"); commit("dock")   // Return
    check(store == ["usb": -270, "hdmi": 40, "dock": 77], "delay field quit: typing alone applies nothing",
          "store: \(store)")
    for id in fields.keys { fields[id]?.drop() }   // quit
    check(fields.values.allSatisfy { !$0.isEditing }, "delay field quit: quitting drops every draft")
    for id in fields.keys { commit(id) }   // a focus loss as the app ends
    check(store == ["usb": -270, "hdmi": 40, "dock": 77],
          "delay field quit: a typed delay pending at quit is neither applied nor saved", "store: \(store)")

    // How a panel's typing ends (DelayPanels): Return and Command-S apply it
    // and leave the panel open; a panel applies its typing as it closes
    // (SyncControls.onDisappear), unless Escape dropped it first; a click on
    // a row's delay or box applies the other panels' typing before its own
    // panel opens or closes.
    var delays: [String: Double] = ["usb": -270, "dock": 10]
    var typed = ["usb": DelayFieldDraft(), "dock": DelayFieldDraft()]
    var panels = DelayPanels()
    var retimes = 0
    var order: [String] = []
    func commitTyped(_ id: String) {
        if let v = typed[id]?.commit(saved: delays[id] ?? 0, sanitize: offset, format: signed) {
            delays[id] = v
            retimes += 1
            order.append(id)
        }
    }
    func change(_ action: (inout DelayPanels) -> Set<String>) {
        let before = panels.open
        let first = action(&panels)
        for id in first.sorted() { commitTyped(id) }
        for id in before.subtracting(panels.open).sorted() { commitTyped(id) }
    }
    func clickOutside() {
        change { open in
            open.closeAll()
            return []
        }
    }
    func openBoth() {
        for id in ["usb", "dock"] where !panels.isOpen(id) {
            change { $0.toggle(id) }
            typed[id]?.show(delays[id] ?? 0, format: signed)
        }
    }

    openBoth()
    typed["usb"]?.type("400"); typed["dock"]?.type("77")
    commitTyped("usb")   // Return
    check(delays == ["usb": 400, "dock": 10] && retimes == 1 && panels.open == ["usb", "dock"]
              && typed["dock"]?.isEditing == true,
          "delay panels: Return applies its row's typing once and leaves every panel open", "delays: \(delays)")
    typed["usb"]?.type("410")
    for id in panels.open.sorted() { commitTyped(id) }   // Command-S
    check(delays == ["usb": 410, "dock": 77] && retimes == 3 && panels.open == ["usb", "dock"],
          "delay panels: Command-S applies every open panel's typing and leaves them open", "delays: \(delays)")
    // The focus leaves the field: Tab, a click in another field, or a click on
    // a control that doesn't take the focus (a slider, a button), which ends
    // the typing first.
    func leave(_ id: String) {
        if let v = typed[id]?.focusChanged(to: false, saved: delays[id] ?? 0, sanitize: offset, format: signed) {
            delays[id] = v
            retimes += 1
        }
    }
    typed["dock"]?.type("-40")
    let entering = typed["dock"]?.focusChanged(to: true, saved: 77, sanitize: offset, format: signed)
    leave("dock")
    leave("dock")
    check(entering == nil && delays["dock"] == -40 && retimes == 4 && panels.open == ["usb", "dock"],
          "delay panels: leaving the field for another control applies its typing once and leaves every panel open",
          "delays: \(delays)")
    typed["dock"]?.type("-30")
    change { $0.toggle("dock") }   // its delay value, or a click on its row's box
    leave("dock")
    check(delays["dock"] == -30 && retimes == 5 && panels.open == ["usb"],
          "delay panels: a click on a row's delay or box applies its typing and closes its panel",
          "delays: \(delays)")
    change { $0.toggle("dock") }
    check(panels.open == ["usb", "dock"] && retimes == 5,
          "delay panels: a click on a closed row's box opens its panel and applies nothing")

    typed["usb"]?.type("380"); typed["dock"]?.type("55")
    order = []
    change { $0.toggle("dock") }   // typed in both, then a click on the dock's row
    check(order == ["usb", "dock"] && delays == ["usb": 380, "dock": 55] && panels.open == ["usb"],
          "delay panels: a click on another row applies the open panel's typing first and leaves that panel open",
          "order: \(order), delays: \(delays)")

    // The note under Connected Speakers belongs to the last row's box.
    let noteTargets = [DelayPanels.noteTarget(under: [(id: "usb", hasDelay: true), (id: "dock", hasDelay: true)]),
                       DelayPanels.noteTarget(under: [(id: "dock", hasDelay: true), (id: "kitchen", hasDelay: false)]),
                       DelayPanels.noteTarget(under: [])]
    check(noteTargets == ["dock", nil, nil],
          "delay panels: a click on the note under a list toggles the last row's panel; under a row without a delay, or alone, it is outside",
          "targets: \(noteTargets)")
    openBoth()
    typed["usb"]?.type("390"); typed["dock"]?.type("66")
    order = []
    if let target = noteTargets[0] { change { $0.toggle(target) } }   // the note, under the dock's row
    check(order == ["usb", "dock"] && delays == ["usb": 390, "dock": 66] && panels.open == ["usb"],
          "delay panels: a click on the note applies the other panel's typing first, then closes the last row's panel",
          "order: \(order), delays: \(delays)")

    openBoth()
    typed["usb"]?.type("-250"); typed["dock"]?.type("abc")
    clickOutside()   // blank space, the header, the note alone
    check(delays == ["usb": -250, "dock": 66] && panels.open.isEmpty && typed["dock"]?.text == signed(66),
          "delay panels: a click outside every speaker row applies each readable typing and closes every panel; unreadable text shows the saved delay",
          "delays: \(delays), dock: \(typed["dock"]?.text ?? "")")

    openBoth()
    typed["usb"]?.type("400"); typed["dock"]?.type("77")
    let before = retimes
    for id in typed.keys { typed[id]?.drop() }   // Escape
    clickOutside()
    leave("usb"); leave("dock")   // the field loses the focus as its panel goes
    check(delays == ["usb": -250, "dock": 66] && retimes == before && panels.open.isEmpty,
          "delay panels: Escape drops every typing and closes every panel; the focus leaving after it applies nothing",
          "delays: \(delays)")

    // A click while typing, as the main window's click monitor sees it, then
    // the clicked control's own action.
    func click(typingIn row: String, onField: Bool = false, onStepOf stepRow: String? = nil, delta: Double = 0) {
        if DelayPanels.clickEndsTyping(typingIn: row, onField: onField, onStepOf: stepRow) { leave(row) }
        if let stepRow, let v = typed[stepRow]?.step(delta, saved: delays[stepRow] ?? 0, sanitize: offset, format: signed) {
            delays[stepRow] = v
            retimes += 1
        }
    }
    openBoth()
    var start = retimes
    typed["dock"]?.type("100")
    click(typingIn: "dock", onStepOf: "dock", delta: 10)
    check(delays["dock"] == 110 && retimes == start + 1 && typed["dock"]?.isEditing == false,
          "delay panels: typing 100 and then a click on its own +10 applies 110, once",
          "delays: \(delays), re-timings: \(retimes - start)")
    start = retimes
    typed["dock"]?.type("200")
    click(typingIn: "dock", onStepOf: "usb", delta: 10)
    check(delays == ["usb": -240, "dock": 200] && retimes == start + 2,
          "delay panels: a click on another panel's step applies the typing first, then that step",
          "delays: \(delays)")
    start = retimes
    typed["dock"]?.type("300")
    click(typingIn: "dock", onField: true)
    leave("dock")   // the other field takes the focus
    check(delays["dock"] == 300 && retimes == start + 1,
          "delay panels: a click into another field applies the typing once, as the focus moves", "delays: \(delays)")
}

// MARK: Delay value — a row's delay value follows every applied delay while its panel stays open (DelayBadge)
do {
    let t = UIStrings()
    let offset = LocalDelay.sanitizedOffset
    let signed = t.offsetNumber
    let air = "airplay-delay"
    // The model values as pushModel leaves them after each apply.
    var offsets: [String: Double] = ["usb": 400, "dock": 10]
    var airPlayMs = 1800.0
    func shown(_ id: String) -> String? {
        DelayBadge.text(of: id, airPlayID: air, airPlayMs: airPlayMs,
                        rows: [(id: "builtin", offsetMs: nil), (id: "usb", offsetMs: offsets["usb"]),
                               (id: "dock", offsetMs: offsets["dock"])],
                        text: t)
    }
    var fields = ["usb": DelayFieldDraft(), "dock": DelayFieldDraft()]
    func commit(_ ids: Set<String>) {
        for id in ids.sorted() {
            if let v = fields[id]?.commit(saved: offsets[id] ?? 0, sanitize: offset, format: signed) { offsets[id] = v }
        }
    }
    var panels = DelayPanels()
    panels.toggle("usb")
    panels.toggle("dock")
    check(shown("usb") == "+400 ms", "delay value: a row shows its saved offset", "shown: \(shown("usb") ?? "nil")")

    var seen: [String] = []
    fields["usb"]?.type("450")
    commit(["usb"])   // Return
    seen.append(shown("usb") ?? "nil")
    fields["usb"]?.type("-120")
    commit(panels.open)   // Command-S
    seen.append(shown("usb") ?? "nil")
    fields["usb"]?.type("75")
    commit(panels.toggle("dock"))   // a click on the other row
    seen.append(shown("usb") ?? "nil")
    check(seen == ["+450 ms", "−120 ms", "+75 ms"] && panels.isOpen("usb"),
          "delay value: Return, Command-S and a click on another row each show the applied offset while its panel stays open",
          "seen: \(seen)")

    fields["usb"]?.type("30")
    let closing = panels.open
    panels.closeAll()   // a click on the window's background
    commit(closing)
    check(shown("usb") == "+30 ms", "delay value: a click outside the rows shows the applied offset",
          "shown: \(shown("usb") ?? "nil")")

    airPlayMs = 2100
    check(shown(air) == "2100 ms" && shown("builtin") == nil && shown("gone") == nil,
          "delay value: the AirPlay delay row shows L; the reference and an unlisted row show none")
}

// MARK: Delay field rings — one ring at a time; red replaces the focus ring while invalid (DelayFieldRings)
do {
    let states = [false, true].flatMap { invalid in [false, true].map { (invalid, $0) } }
    for (invalid, focused) in states {
        let rings = DelayFieldRings(invalid: invalid, focused: focused)
        let drawn = (rings.focusRing ? 1 : 0) + rings.redOutlines + (DelayFieldRings.appKitRing && focused ? 1 : 0)
        check(drawn <= 1, "delay rings: at most one ring (invalid \(invalid), focused \(focused))", "\(rings)")
        check(!DelayFieldRings.appKitRing, "delay rings: AppKit's own ring stays off, so it can't linger after a toggle mid-edit",
              "invalid \(invalid), focused \(focused)")
    }
    let focusedInvalid = DelayFieldRings(invalid: true, focused: true)
    check(!focusedInvalid.focusRing && focusedInvalid.redOutlines == 1 && focusedInvalid.redOutsideBezel,
          "delay rings: invalid and focused → one red outline where the focus ring goes", "\(focusedInvalid)")
    let focusedValid = DelayFieldRings(invalid: false, focused: true)
    check(focusedValid.focusRing && focusedValid.redOutlines == 0, "delay rings: valid and focused → the focus ring",
          "\(focusedValid)")
    let unfocusedInvalid = DelayFieldRings(invalid: true, focused: false)
    check(!unfocusedInvalid.focusRing && unfocusedInvalid.redOutlines == 1 && !unfocusedInvalid.redOutsideBezel,
          "delay rings: invalid, focus elsewhere → a red outline on the bezel", "\(unfocusedInvalid)")
    let idle = DelayFieldRings(invalid: false, focused: false)
    check(!idle.focusRing && idle.redOutlines == 0, "delay rings: valid, focus elsewhere → no ring", "\(idle)")
}

// MARK: Delay field input — digits after one leading sign; out of range is marked (LocalDelay.fieldText)
do {
    let t = UIStrings()
    let kept: [String: String] = ["12": "12", "+150": "+150", "−250": "−250", "-30": "-30", "1a2": "12",
                                  "12+": "12", "+-5": "+5", " 40": "40", "abc": "", "++": "+", "": ""]
    let wrong = kept.filter { LocalDelay.fieldText($0.key) != $0.value }.keys.sorted()
    check(wrong.isEmpty, "delay field input: only digits after one leading + or − can be typed", "\(wrong)")

    let proposed = ["12", "+", "−250", "", "12a", "1+", "+-", " 4", "x"]
    let blocked = proposed.filter { !LocalDelay.fieldAccepts($0) }
    check(blocked == ["12a", "1+", "+-", " 4", "x"],
          "delay field input: a keystroke the field can't take is blocked before it shows", "blocked: \(blocked)")

    var field = DelayFieldDraft()
    field.show(0, format: t.offsetNumber)
    field.type("15x")
    check(field.text == "15" && field.isEditing, "delay field input: a letter typed after digits doesn't show",
          "text: \(field.text)")

    let offset = LocalDelay.sanitizedOffset
    var marked: [String] = []
    for text in ["-2500", "+20001", "−2000", "+20000", "−", "+", "", "150"] {
        field.type(text)
        if field.isOutOfRange(sanitize: offset) { marked.append(text) }
    }
    check(marked == ["-2500", "+20001"],
          "delay field input: only a number out of the offset range gets the red outline", "marked: \(marked)")

    var air = DelayFieldDraft()
    air.show(1800, format: { String(Int($0.rounded())) })
    let atRest = air.isOutOfRange(sanitize: LocalDelay.sanitizedAirPlayDelay)
    air.type("-5")
    let negative = air.isOutOfRange(sanitize: LocalDelay.sanitizedAirPlayDelay)
    _ = air.commit(saved: 1800, sanitize: LocalDelay.sanitizedAirPlayDelay, format: { String(Int($0.rounded())) })
    check(!atRest && negative && !air.isOutOfRange(sanitize: LocalDelay.sanitizedAirPlayDelay) && air.text == "0",
          "delay field input: a negative AirPlay delay is marked until Return applies 0", "text: \(air.text)")

    check(t.delayOutOfRange(lowest: "−2000", highest: "+20000", applies: "−2000")
              == "The range is −2000 to +20000 ms. Return applies −2000.",
          "delay field input: the out-of-range hint names the range and the value Return applies")
}

// MARK: Bluetooth IO buffer switch: AUDIOSYNC_BT_IO_FRAMES, Bluetooth outputs only (BluetoothIOBuffer)
do {
    typealias B = BluetoothIOBuffer
    let bt = kAudioDeviceTransportTypeBluetooth, le = kAudioDeviceTransportTypeBluetoothLE
    let wide: (minimum: Double, maximum: Double)? = (15, 8192)
    check(B.requested(nil) == nil && B.requested("") == nil && B.requested("abc") == nil
              && B.requested("3000.5") == nil && B.requested("-2048") == nil,
          "BT IO buffer: unset, empty or not a whole number asks for nothing")
    check(B.requested("255") == nil && B.requested("4097") == nil && B.requested("0") == nil,
          "BT IO buffer: a value outside 256 to 4096 asks for nothing")
    check(B.requested("256") == 256 && B.requested("1024") == 1024 && B.requested(" 4096 ") == 4096
              && B.requested("3072") == 3072,
          "BT IO buffer: a whole number from 256 to 4096 is the request")
    check(B.frames(requested: nil, transport: bt, range: wide) == nil,
          "BT IO buffer: no request leaves a Bluetooth device untouched")
    check(B.frames(requested: 4096, transport: kAudioDeviceTransportTypeBuiltIn, range: wide) == nil
              && B.frames(requested: 4096, transport: kAudioDeviceTransportTypeUSB, range: wide) == nil
              && B.frames(requested: 4096, transport: kAudioDeviceTransportTypeAirPlay, range: wide) == nil
              && B.frames(requested: 4096, transport: 0, range: wide) == nil,
          "BT IO buffer: a device that isn't Bluetooth is never touched")
    check(B.frames(requested: 4096, transport: bt, range: wide) == 4096
              && B.frames(requested: 2048, transport: le, range: wide) == 2048,
          "BT IO buffer: Bluetooth Classic and LE devices get the request inside their range")
    check(B.frames(requested: 4096, transport: bt, range: (15, 2048)) == 2048
              && B.frames(requested: 2048, transport: bt, range: (3000.2, 8192)) == 3001,
          "BT IO buffer: the request is clamped to the device's range")
    check(B.frames(requested: 3072, transport: bt, range: nil) == 3072
              && B.frames(requested: 3072, transport: bt, range: (8192, 15)) == 3072
              && B.frames(requested: 3072, transport: bt, range: (Double.nan, 8192)) == 3072
              && B.frames(requested: 3072, transport: bt, range: (1e12, 1e13)) == 3072,
          "BT IO buffer: an unknown or unusable range leaves the request as it is")
}

// MARK: Observer line — per output values and the Wi-Fi link in the debug log (OutputObservation, WiFiNote, S229)
do {
    let measured = OutputObservation(leastSlackMs: -0.666, floorDropMs: 10.04, shortageFrames: 912, overloads: 3,
                                     timeSteps: 1, bluetoothOutputs: 2)
    check(measured.text == "least slack -0.7 ms, floor drop 10.0 ms, shortage 912 frames, IO overloads 3, "
              + "output-time steps 1, Bluetooth outputs 2",
          "observer: the line names every value with its unit", measured.text)
    let early = OutputObservation(leastSlackMs: nil, floorDropMs: 0, shortageFrames: 0, overloads: 0,
                                  timeSteps: 0, bluetoothOutputs: 0)
    check(early.text.hasPrefix("least slack n/a, floor drop 0.0 ms,"),
          "observer: before the core's warm-up the least slack reads n/a", early.text)
    let rejected = OutputObservation(leastSlackMs: 1, floorDropMs: 0, shortageFrames: 0, overloads: 0, timeSteps: 0,
                                     bluetoothOutputs: 1, rejectedLatencyNs: 30_000_000_000, latencyNs: 150_000_000)
    check(rejected.text.hasSuffix("Bluetooth outputs 1, reported latency 30000.0 ms outside 50 to 1000 ms, used 150.0 ms")
              && !measured.text.contains("reported latency"),
          "observer: a latency report the window rejected shows with the value used instead", rejected.text)
    check(WiFiNote.text(poweredOn: true, band: 2, channel: 36, rssi: -52) == "Wi-Fi 5 GHz, channel 36, RSSI -52 dBm"
              && WiFiNote.text(poweredOn: true, band: 1, channel: 6, rssi: -70) == "Wi-Fi 2.4 GHz, channel 6, RSSI -70 dBm"
              && WiFiNote.text(poweredOn: true, band: 3, channel: 37, rssi: nil) == "Wi-Fi 6 GHz, channel 37, RSSI n/a",
          "observer: the Wi-Fi line gives band, channel and RSSI")
    check(WiFiNote.text(poweredOn: true, band: 2, channel: 36, rssi: 0) == "Wi-Fi 5 GHz, channel 36, RSSI n/a",
          "observer: an RSSI of 0 is one macOS didn't give")
    check(WiFiNote.text(poweredOn: true, band: 0, channel: 36, rssi: -50) == "Wi-Fi n/a"
              && WiFiNote.text(poweredOn: true, band: 2, channel: 0, rssi: -50) == "Wi-Fi n/a"
              && WiFiNote.text(poweredOn: true, band: nil, channel: nil, rssi: nil) == "Wi-Fi n/a",
          "observer: a band or channel macOS withholds reads n/a")
    check(WiFiNote.text(poweredOn: false, band: 2, channel: 36, rssi: -50) == "Wi-Fi off",
          "observer: Wi-Fi switched off says so")
}

// MARK: Fades — stops and switch-offs wait for the outputs' fade-outs (OutputFades.swift, S185, S186)

/// A host clock that the checks move on by hand and that `finishAll`'s pauses
/// move on too, and the events the fakes below note, in order.
final class FadeBench {
    var now: UInt64 = 1_000_000_000
    var events: [String] = []
    /// When each event first happened.
    var times: [String: UInt64] = [:]
    func note(_ event: String) {
        events.append(event)
        if times[event] == nil { times[event] = now }
    }
    /// Where `event` first happened in `events`; Int.max if it never did.
    func index(_ event: String) -> Int { events.firstIndex(of: event) ?? Int.max }
    func retirement() -> OutputRetirement {
        OutputRetirement(clock: { self.now }, pause: { self.now += $0 })
    }
}

/// A local output whose device plays its fade-out `fadesAfterMs` after it was
/// asked for (nil: never, like an output whose IO stalled).
final class FakeFadingOutput: FadingOutput {
    let name: String
    let bench: FadeBench
    let fadesAfterNs: UInt64?
    var askedAt: UInt64?
    var stops = 0
    init(_ name: String, _ bench: FadeBench, fadesAfterMs: UInt64?) {
        self.name = name
        self.bench = bench
        fadesAfterNs = fadesAfterMs.map { $0 * 1_000_000 }
    }
    func requestFadeOut() {
        if askedAt == nil { askedAt = bench.now }
        bench.note("fade \(name)")
    }
    func hasFaded(nowNs: UInt64) -> Bool {
        guard let askedAt, let fadesAfterNs else { return false }
        return nowNs >= askedAt + fadesAfterNs
    }
    func halt() {
        stops += 1
        bench.note("stop \(name)")
    }
}

do {
    let ms: UInt64 = 1_000_000
    /// A session stop on `bench` whose capture notes when it stops.
    func stopSession(_ r: OutputRetirement, _ bench: FadeBench, _ outputs: [FadingOutput]) {
        r.stopSession(outputs, fadeInputs: { bench.note("fade AirPlay inputs") },
                      stopCapture: { bench.note("capture stops") }, then: { bench.note("stopped") })
    }

    // S186: every output and the AirPlay inputs are asked at once, capture
    // runs while they fade, then capture and the outputs stop.
    do {
        let bench = FadeBench(), r = bench.retirement()
        let a = FakeFadingOutput("A", bench, fadesAfterMs: 40), b = FakeFadingOutput("B", bench, fadesAfterMs: 25)
        stopSession(r, bench, [a, b])
        let asked = bench.events
        bench.now += 39 * ms
        r.poll()
        let held = bench.events == asked
        bench.now += 1 * ms
        r.poll()
        check(Set(asked) == ["fade A", "fade B", "fade AirPlay inputs"] && asked.count == 3 && held
                && bench.events == asked + ["capture stops", "stop A", "stop B", "stopped"],
              "fade: a stop asks every output and the AirPlay input to fade out before capture stops",
              "\(bench.events)")
    }

    // S186, S189: with no local output on (only AirPlay speakers), capture
    // still runs until the AirPlay inputs have had their fade-out's frames;
    // while capture gives nothing, it waits the fallback, not the cap.
    do {
        let bench = FadeBench(), r = bench.retirement()
        let asked = bench.now
        r.stopSession([], fadeInputs: { bench.note("fade AirPlay inputs") },
                      inputsFaded: { bench.now >= asked + 30 * ms }, inputsFallbackNs: 105 * ms,
                      stopCapture: { bench.note("capture stops") })
        let heldAtOnce = bench.events == ["fade AirPlay inputs"]
        bench.now += 29 * ms
        r.poll()
        let heldTo29 = !bench.events.contains("capture stops")
        bench.now += 1 * ms
        r.poll()
        let stoppedAt30 = bench.events.contains("capture stops") && r.isIdle

        let silentBench = FadeBench(), silent = silentBench.retirement()
        let start = silentBench.now
        silent.stopSession([], fadeInputs: {}, inputsFaded: { false }, inputsFallbackNs: 105 * ms,
                           stopCapture: { silentBench.note("capture stops") })
        silent.finishAll()
        let waited = silentBench.now - start
        check(heldAtOnce && heldTo29 && stoppedAt30 && waited >= 105 * ms && waited <= 107 * ms,
              "fade: with only AirPlay speakers on, a stop keeps capturing until the AirPlay inputs have faded",
              "held \(heldAtOnce)/\(heldTo29), stopped at 30 ms \(stoppedAt30), silent capture waited \(waited / ms) ms")
    }

    // S186: the slowest output sets the wait, and 500 ms is the most it waits.
    do {
        let bench = FadeBench(), r = bench.retirement()
        stopSession(r, bench, [FakeFadingOutput("quick", bench, fadesAfterMs: 50),
                               FakeFadingOutput("slow", bench, fadesAfterMs: 300)])
        bench.now += 299 * ms
        r.poll()
        let waitedForSlow = !bench.events.contains("capture stops")
        bench.now += 1 * ms
        r.poll()
        let stoppedOnceSlowFaded = bench.events.contains("capture stops") && r.isIdle

        let stuckBench = FadeBench(), stuckRetirement = stuckBench.retirement()
        stopSession(stuckRetirement, stuckBench, [FakeFadingOutput("stuck", stuckBench, fadesAfterMs: nil)])
        stuckBench.now += 499 * ms
        stuckRetirement.poll()
        let heldTo499 = !stuckBench.events.contains("stop stuck")
        stuckBench.now += 1 * ms
        stuckRetirement.poll()
        let cappedAt500 = stuckBench.events.contains("stop stuck") && stuckRetirement.isIdle

        let waitBench = FadeBench(), waitRetirement = waitBench.retirement()
        let start = waitBench.now
        stopSession(waitRetirement, waitBench, [FakeFadingOutput("stuck", waitBench, fadesAfterMs: nil)])
        waitRetirement.finishAll()
        let waited = waitBench.now - start
        check(waitedForSlow && stoppedOnceSlowFaded && heldTo499 && cappedAt500 && waitRetirement.isIdle
                && waited >= 500 * ms && waited <= 502 * ms,
              "fade: a stop waits for the slowest output, never more than 500 ms",
              "slow \(waitedForSlow)/\(stoppedOnceSlowFaded), capped \(heldTo499)/\(cappedAt500), "
                + "finishAll waited \(waited / ms) ms")
    }

    // S185: a renderer publishes the output time of its fade's last frame;
    // the output has faded once the host clock passes it plus the latency.
    // The core's renderer at 48 kHz with 512-frame callbacks: the fade-out
    // runs over the 512 frames of the first callback after the request and
    // the first 448 of the next, so its last frame is at offset 447 there.
    do {
        let rate = 48_000.0, frames = 512, period: UInt64 = 10_666_667, latency: UInt64 = 5 * ms
        if let ring = CoreDelayRing(capacityFrames: 48_000 * 4),
           let renderer = CoreRenderer(ring: ring, captureRate: rate, deviceRate: rate, channels: 2, gain: 1) {
            let level = [Float](repeating: 0.5, count: 480 * 2)
            for _ in 0..<100 { level.withUnsafeBufferPointer { ring.write(interleaved: $0.baseAddress!, frames: 480) } }
            renderer.setDelay(ms: 100)
            var out = [Float](repeating: 0, count: frames * 2)
            let start: UInt64 = 5_000_000_000
            var callbacks: [[Float]] = []
            // Capture keeps pace with the output, so the reader makes no drift correction.
            let block = [Float](repeating: 0.5, count: frames * 2)
            func callback(_ k: Int) {
                block.withUnsafeBufferPointer { ring.write(interleaved: $0.baseAddress!, frames: frames) }
                out.withUnsafeMutableBufferPointer {
                    renderer.render(into: $0.baseAddress!, frames: frames, outputTimeNs: start + UInt64(k) * period)
                }
                callbacks.append(out)
            }
            for k in 0..<4 { callback(k) }
            let notAskedYet = !renderer.hasFaded(latencyNs: latency, nowNs: start + 10 * period)
            renderer.requestFadeOut()
            for k in 4..<7 { callback(k) }
            let end = start + 5 * period + 447 * 1_000_000_000 / 48_000
            let zerosAfter = callbacks[5][(448 * 2)...].allSatisfy { $0 == 0 } && callbacks[6].allSatisfy { $0 == 0 }
            check(notAskedYet && renderer.fadeEndNs == end && zerosAfter
                    && !renderer.hasFaded(latencyNs: latency, nowNs: end + latency - 1)
                    && renderer.hasFaded(latencyNs: latency, nowNs: end + latency),
                  "fade: an output is faded once the host clock passes its last faded frame's output time "
                    + "plus its latency",
                  "published \(renderer.fadeEndNs.map { String($0) } ?? "nothing"), expected \(end); "
                    + "zeros after it: \(zerosAfter)")
        } else {
            check(false, "fade: an output is faded once the host clock passes its last faded frame's output time "
                    + "plus its latency", "the core's renderer could not be made")
        }
    }

    // S230: an output the host marks as Bluetooth adds a capture block of
    // 512 frames to its start lead, which its start line logs in ms.
    do {
        let name = "bluetooth lead: a Bluetooth output's start lead counts a capture block more"
        if let ring = CoreDelayRing(capacityFrames: 48_000 * 4),
           let wired = CoreRenderer(ring: ring, captureRate: 48_000, deviceRate: 48_000, channels: 2, gain: 1),
           let buds = CoreRenderer(ring: ring, captureRate: 48_000, deviceRate: 44_100, channels: 2, gain: 1) {
            buds.setBluetooth(true)
            check(wired.extraLeadMs == 0 && abs(buds.extraLeadMs - 512.0 / 48) < 1e-9, name,
                  "wired \(wired.extraLeadMs) ms, Bluetooth \(buds.extraLeadMs) ms")
        } else {
            check(false, name, "the core's renderer could not be made")
        }
    }

    // S231: a Bluetooth output's latency report outside 50 to 1000 ms gives
    // way to its latest plausible one, else 150 ms; any other stays as it is.
    do {
        let name = "bluetooth latency: a report outside the window gives way, any other output's stays"
        if let ring = CoreDelayRing(capacityFrames: 48_000 * 4),
           let wired = CoreRenderer(ring: ring, captureRate: 48_000, deviceRate: 48_000, channels: 2, gain: 1),
           let buds = CoreRenderer(ring: ring, captureRate: 48_000, deviceRate: 44_100, channels: 2, gain: 1) {
            buds.setBluetooth(true)
            let unknown = buds.setReportedLatency(0, lastPlausibleNs: 0)
            let unknownRejected = buds.rejectedLatencyNs
            let absurd = buds.setReportedLatency(30_000_000_000, lastPlausibleNs: 180_000_000)
            let plausible = buds.setReportedLatency(149_297_000, lastPlausibleNs: 180_000_000)
            let wiredUsed = wired.setReportedLatency(0, lastPlausibleNs: 0)
            check(unknown == 150_000_000 && unknownRejected == 0 && absurd == 180_000_000
                      && plausible == 149_297_000 && buds.rejectedLatencyNs == nil
                      && wiredUsed == 0 && wired.rejectedLatencyNs == nil, name,
                  "\(unknown), \(String(describing: unknownRejected)), \(absurd), \(plausible), \(wiredUsed)")
        } else {
            check(false, name, "the core's renderer could not be made")
        }
    }

    // S186: a source change is a stop and a start, and the start waits for
    // the stop (StreamEngine.start calls finishAll first).
    do {
        let bench = FadeBench(), r = bench.retirement()
        stopSession(r, bench, [FakeFadingOutput("A", bench, fadesAfterMs: 45)])
        r.finishAll()
        bench.note("new capture starts")
        let order = ["fade A", "capture stops", "stop A", "new capture starts"].map(bench.index)
        let faded = bench.times["fade A"].map { (bench.times["capture stops"] ?? 0) >= $0 + 45 * ms } ?? false
        check(order == order.sorted() && !order.contains(Int.max) && faded,
              "fade: a source change fades out on the old source before the new one starts", "\(bench.events)")
    }

    // S185, S186: a switched-off speaker plays its fade before its IO stops.
    do {
        let bench = FadeBench(), r = bench.retirement()
        r.remove([FakeFadingOutput("A", bench, fadesAfterMs: 30)], then: { bench.note("removed") })
        let fading = bench.events == ["fade A"] && !r.isIdle
        bench.now += 29 * ms
        r.poll()
        let stillRunning = !bench.events.contains("stop A")
        bench.now += 1 * ms
        r.poll()
        check(fading && stillRunning && bench.events == ["fade A", "stop A", "removed"] && r.isIdle,
              "fade: a speaker switched off is stopped only after it faded", "\(bench.events)")
    }

    // S186: several speakers switched off together fade at once and cost one wait.
    do {
        let bench = FadeBench(), r = bench.retirement()
        let start = bench.now
        r.remove([FakeFadingOutput("A", bench, fadesAfterMs: 20), FakeFadingOutput("B", bench, fadesAfterMs: 60),
                  FakeFadingOutput("C", bench, fadesAfterMs: 40)], then: { bench.note("removed") })
        let askedAll = bench.events == ["fade A", "fade B", "fade C"]
        r.finishAll()
        let waited = bench.now - start
        check(askedAll && waited >= 60 * ms && waited <= 62 * ms
                && bench.events == ["fade A", "fade B", "fade C", "stop A", "stop B", "stop C", "removed"],
              "fade: switching several speakers off asks all of them to fade, then waits once",
              "waited \(waited / ms) ms: \(bench.events)")
    }

    // S185: an output whose device is gone can't play a fade: it stops at
    // once, also while the others fade out at a stop.
    do {
        let bench = FadeBench(), r = bench.retirement()
        let playing = FakeFadingOutput("lost", bench, fadesAfterMs: nil)
        r.stopLost(playing)
        let atOnce = bench.events == ["stop lost"]
        let b = FakeFadingOutput("B", bench, fadesAfterMs: 40), gone = FakeFadingOutput("gone", bench, fadesAfterMs: nil)
        stopSession(r, bench, [b, gone])
        bench.now += 10 * ms
        r.stopLost(gone)
        let goneAtOnce = bench.events.contains("stop gone") && !bench.events.contains("capture stops")
        bench.now += 30 * ms
        r.poll()
        check(atOnce && goneAtOnce && bench.events.contains("stop B") && bench.events.contains("capture stops")
                && gone.stops == 1 && r.isIdle,
              "fade: a lost device is stopped at once, without a fade", "\(bench.events)")
    }

    // S110, S186: quitting waits for the fades, well inside the 5 s a quit may take.
    check(OutputRetirement.capSeconds == 0.5 && OutputRetirement.capSeconds <= 5,
          "fade: the fade cap is 500 ms and fits in the 5 s quit budget",
          "\(OutputRetirement.capSeconds) s")
}

// MARK: Local speakers that won't start (S69: LocalOutputSync.apply and report), and the master (S72)
do {
    /// An engine double whose attach throws for the UIDs in `refusing`.
    final class RefusingEngine: LocalOutputEngine {
        let refusing: Set<String>
        var activeOutputUIDs: [String]
        var tried: [String] = []
        var removed: [String] = []
        var deviceIDs: [String: AudioObjectID] = [:]
        var delays: [String: Double] = [:]
        var gains: [String: Double] = [:]
        /// The UIDs given a new gain after their attach, in call order.
        var regained: [String] = []
        init(active: [String], refusing: Set<String>) {
            activeOutputUIDs = active
            self.refusing = refusing
        }
        func attachOutput(uid: String, device: AudioObjectID, delayMs: Double, gain: Double) throws {
            tried.append(uid)
            if refusing.contains(uid) { throw NSError(domain: "logictest", code: -1) }
            activeOutputUIDs.append(uid)
            deviceIDs[uid] = device
            delays[uid] = delayMs
            gains[uid] = gain
        }
        func detachOutput(uid: String) {
            removed.append(uid)
            activeOutputUIDs.removeAll { $0 == uid }
        }
        func setGain(uid: String, g: Double) {
            regained.append(uid)
            gains[uid] = g
        }
    }
    typealias F = LocalOutputSync.Failure
    let usb = AudioDevices.Device(id: 52, name: "USB DAC", uid: "usb", transport: kAudioDeviceTransportTypeUSB)
    let usbAgain = AudioDevices.Device(id: 99, name: "USB DAC", uid: "usb", transport: kAudioDeviceTransportTypeUSB)
    let bt = AudioDevices.Device(id: 61, name: "Headphones", uid: "bt", transport: kAudioDeviceTransportTypeBluetooth)

    // The user switches on a Bluetooth headset that won't start, next to a USB
    // DAC that does, a speaker whose device is missing, and one switched off.
    let engine = RefusingEngine(active: ["old"], refusing: ["bt"])
    let enabled: Set<String> = ["usb", "bt", "tv"]
    let applied = LocalOutputSync.apply(enabled: enabled, listing: [usb, bt, usbAgain], to: engine,
                                        holdBacks: ["usb": 40], gain: { $0 == "usb" ? 0.5 : 0.25 })
    check(engine.tried == ["bt", "usb"] && applied.attached == ["usb": 40]
            && applied.failures == [F(uid: "bt", deviceName: "Headphones")],
          "S69 start failure: the speaker that throws is reported, the other one still starts",
          "tried \(engine.tried), attached \(applied.attached), failures \(applied.failures)")
    check(engine.deviceIDs["usb"] == 52 && engine.delays["usb"] == 40 && engine.gains["usb"] == 0.5,
          "S69 start failure: an attach uses the first listed device, its hold-back and its gain")
    check(applied.detached == ["old"] && engine.removed == ["old"] && Set(engine.activeOutputUIDs) == ["usb"],
          "S69 start failure: a speaker switched off is still detached in the same sync")

    let byUser = LocalOutputSync.report(applied.failures, enabled: enabled, origin: .user)
    check(byUser?.enabled == ["usb", "tv"],
          "S69 start failure: the failed speaker is switched off; the others stay on, a missing one too",
          "\(String(describing: byUser?.enabled))")
    check(byUser?.notices == [F(uid: "bt", deviceName: "Headphones")] && byUser?.alertNames == ["Headphones"],
          "S69 start failure: a change the user made gets an alert and a notice for the failed speaker")
    check(!StreamingPolicy.isIdle(streaming: true, enabledAirPlay: [], enabledLocal: byUser?.enabled ?? []),
          "S69 start failure: streaming goes on while another speaker is still on")

    // The failed speaker is not tried again at the next sync.
    let triedBefore = engine.tried.count
    let next = LocalOutputSync.apply(enabled: byUser?.enabled ?? [], listing: [usb, bt], to: engine,
                                     holdBacks: [:], gain: { _ in 1 })
    check(engine.tried.count == triedBefore && next == LocalOutputSync.Applied()
            && LocalOutputSync.report(next.failures, enabled: byUser?.enabled ?? [], origin: .automatic) == nil,
          "S69 start failure: the next sync doesn't try the failed speaker again and reports nothing")

    // A reconcile (a device-list notice, say) attaches the only speaker on, and it won't start.
    let lone = RefusingEngine(active: [], refusing: ["usb", "bt"])
    let lonely = LocalOutputSync.apply(enabled: ["usb"], listing: [usb], to: lone, holdBacks: [:], gain: { _ in 1 })
    let byPoll = LocalOutputSync.report(lonely.failures, enabled: ["usb"], origin: .automatic)
    check(byPoll?.enabled == [] && byPoll?.alertNames == nil && byPoll?.notices == [F(uid: "usb", deviceName: "USB DAC")],
          "S69 start failure: a failure the app caused gets a notice but no alert")
    check(StreamingPolicy.isIdle(streaming: true, enabledAirPlay: [], enabledLocal: byPoll?.enabled ?? ["usb"]),
          "S69 start failure: streaming stops when the failed speaker was the only one on")
    check(!StreamingPolicy.isIdle(streaming: true, enabledAirPlay: ["kitchen"], enabledLocal: byPoll?.enabled ?? []),
          "S69 start failure: an AirPlay speaker still on keeps streaming")

    // Two speakers fail together: one alert names both, each gets its own notice.
    let both = LocalOutputSync.apply(enabled: ["usb", "bt"], listing: [usb, bt],
                                     to: RefusingEngine(active: [], refusing: ["usb", "bt"]), holdBacks: [:], gain: { _ in 1 })
    let twice = LocalOutputSync.report(both.failures, enabled: ["usb", "bt"], origin: .user)
    check(twice?.enabled == [] && twice?.alertNames == ["Headphones", "USB DAC"]
            && twice?.notices.map(\.uid) == ["bt", "usb"],
          "S69 start failure: two failures share one alert and get a notice each",
          "\(String(describing: twice))")

    // S72: a master change gives each running local speaker its gain again;
    // one switched on but not running (here "tv") gets no call.
    let playing = RefusingEngine(active: ["usb", "bt"], refusing: [])
    LocalOutputSync.reapplyGains(on: playing, gain: { ["usb": 0.5, "bt": 0.25, "tv": 0.75][$0] ?? 0 })
    check(playing.regained == ["usb", "bt"] && playing.gains == ["usb": 0.5, "bt": 0.25],
          "S72 master: every running local speaker gets its gain again, and only those",
          "regained \(playing.regained), gains \(playing.gains)")

    // S186: speakers switched off in one sync leave in one call, so they fade out at once.
    final class BatchingEngine: LocalOutputEngine {
        var activeOutputUIDs: [String] = ["a", "b", "c"]
        var calls: [[String]] = []
        func attachOutput(uid: String, device: AudioObjectID, delayMs: Double, gain: Double) throws {}
        func detachOutput(uid: String) { calls.append([uid]) }
        func detachOutputs(uids: [String]) { calls.append(uids) }
        func setGain(uid: String, g: Double) {}
    }
    let batching = BatchingEngine()
    let leftTogether = LocalOutputSync.apply(enabled: ["b"], listing: [], to: batching, holdBacks: [:], gain: { _ in 1 })
    check(batching.calls == [["a", "c"]] && leftTogether.detached == ["a", "c"],
          "S186 switch-off: the speakers a sync switches off are detached together",
          "calls \(batching.calls), detached \(leftTogether.detached)")

    // EC1: the USB DAC came back under a new device ID and no "is alive"
    // notice stopped its renderer; the sync restarts it on the new ID.
    let stale = RefusingEngine(active: ["usb"], refusing: [])
    let healed = LocalOutputSync.apply(enabled: ["usb"], listing: [usbAgain], to: stale, bound: ["usb": 52],
                                       holdBacks: ["usb": 30], gain: { _ in 0.5 })
    check(stale.removed == ["usb"] && stale.tried == ["usb"] && stale.deviceIDs["usb"] == 99
            && healed.restarted == ["usb"] && healed.attached == ["usb": 30] && healed.detached.isEmpty
            && healed.devices == ["usb": 99] && stale.activeOutputUIDs == ["usb"],
          "EC1 new ID: a sync stops the renderer bound to the old ID and starts it on the new one",
          "removed \(stale.removed), tried \(stale.tried), applied \(healed)")
    let steady = LocalOutputSync.apply(enabled: ["usb"], listing: [usbAgain], to: stale, bound: healed.devices,
                                       holdBacks: ["usb": 30], gain: { _ in 0.5 })
    check(steady == LocalOutputSync.Applied() && stale.tried == ["usb"],
          "EC1 new ID: the next sync leaves the restarted speaker alone",
          "\(steady)")

    // EC1 + S69: the restart on the new ID won't start. The speaker is
    // reported and switched off, and nothing is left bound to either ID.
    let refused = RefusingEngine(active: ["usb"], refusing: ["usb"])
    let failedRestart = LocalOutputSync.apply(enabled: ["usb"], listing: [usbAgain], to: refused, bound: ["usb": 52],
                                              holdBacks: [:], gain: { _ in 1 })
    let afterRestart = LocalOutputSync.report(failedRestart.failures, enabled: ["usb"], origin: .automatic)
    check(refused.removed == ["usb"] && refused.activeOutputUIDs.isEmpty && failedRestart.restarted == ["usb"]
            && failedRestart.failures == [F(uid: "usb", deviceName: "USB DAC")]
            && failedRestart.attached.isEmpty && failedRestart.devices.isEmpty && failedRestart.detached.isEmpty
            && afterRestart?.enabled == [] && afterRestart?.notices.map(\.uid) == ["usb"],
          "EC1 new ID: a restart that won't start is reported and switched off, with no device left bound",
          "removed \(refused.removed), applied \(failedRestart), report \(String(describing: afterRestart))")
}

// MARK: Launch, second launch and session events (AppLifecycle.swift)
do {
    let open = AEEventID(kAEOpenApplication)
    let atLogin = OSType(keyAELaunchedAsLogInItem)
    check(AppLifecycle.isLoginItemLaunch(eventID: open, launchedAs: atLogin),
          "launch: macOS opening AudioSync at login is a login-item launch")
    check(!AppLifecycle.isLoginItemLaunch(eventID: open, launchedAs: nil)
            && !AppLifecycle.isLoginItemLaunch(eventID: nil, launchedAs: nil)
            && !AppLifecycle.isLoginItemLaunch(eventID: AEEventID(kAEReopenApplication), launchedAs: atLogin),
          "launch: any other launch is not a login-item launch")
    check(AppLifecycle.opensMainWindow(loginItemLaunch: false) && !AppLifecycle.opensMainWindow(loginItemLaunch: true),
          "launch: the main window opens at launch, but not at login")
    check(AppLifecycle.safetyNetInterval == 30,
          "reconcile: with no event, the host reconciles by itself only every 30 s, the safety net (S101)")
    do {
        let t0 = Date(timeIntervalSince1970: 3_000_000)
        func next(_ due: [Date?], last: Date? = nil, asleep: Bool = false) -> Date? {
            AppLifecycle.nextDueReconcile(due: due, last: last, asleep: asleep)
        }
        check(next([]) == nil && next([nil, nil, nil]) == nil,
              "reconcile: with nothing due, no reconcile is scheduled beyond the safety net (S101)")
        check(next([t0 + 20, nil, t0 + 10]) == t0 + 10,
              "reconcile: the earliest due time is the one scheduled (S101)")
        check(next([t0 + 10], asleep: true) == nil,
              "reconcile: nothing is scheduled while the Mac sleeps; its wake reconciles (S101)")
        check(next([t0 - 5], last: t0) == t0 + AppLifecycle.dueSpacing
                && next([t0 + 10], last: t0) == t0 + 10,
              "reconcile: a due time left in the past runs at most once a second, a later one on time (S101)")
        check(next([t0 - 5]) == t0 - 5,
              "reconcile: a due time already past runs at once when none ran before (S101)")
    }
    check(AppLifecycle.answersSecondLaunch(exiting: false), "second launch: the running copy shows its window")
    check(!AppLifecycle.answersSecondLaunch(exiting: true),
          "second launch: a copy that is quitting doesn't answer, so the new one waits for it")
    check(Set(AppLifecycle.exitSignals) == [SIGTERM, SIGINT] && AppLifecycle.exitSignals.count == 2,
          "quit: SIGTERM and Ctrl-C's SIGINT both take the clean exit path (S111)",
          "\(AppLifecycle.exitSignals)")
    check(AppLifecycle.exitStatus(signal: SIGTERM) == 143 && AppLifecycle.exitStatus(signal: SIGINT) == 130,
          "quit: after a signal's clean exit the status is 128 plus its number, so a calling script sees the interrupt (S111)",
          "\(AppLifecycle.exitStatus(signal: SIGTERM)), \(AppLifecycle.exitStatus(signal: SIGINT))")
    check(AppLifecycle.restartsAfterFormatChange(userStopped: false)
            && !AppLifecycle.restartsAfterFormatChange(userStopped: true),
          "EC4 rate: a format change rebuilds the session the engine stopped, unless the user switched streaming off")
    let music = AudioSource.app(bundleID: "com.apple.Music")
    typealias Report = AppLifecycle.SourceQuitReport
    // A default argument can't use `music`, hence the nil.
    func quit(_ report: Report, _ source: AudioSource? = nil, _ setting: WhenSourceQuits = .stop) -> String {
        guard let answer = AppLifecycle.sourceQuit(report, source: source ?? music, setting: setting) else { return "keep" }
        return "\(answer.action) \(answer.bundleID)"
    }
    let musicQuit = Report.terminated(bundleID: "com.apple.Music", streaming: true)
    check(quit(musicQuit) == "stop com.apple.Music",
          "EC6 app quit: the source app quitting while streaming stops the session", quit(musicQuit))
    check(quit(.terminated(bundleID: "com.apple.Music", streaming: false)) == "keep"
            && quit(.terminated(bundleID: "com.apple.Music", streaming: false), music, .system) == "keep",
          "EC6 app quit: the source app quitting while stopped changes nothing, under either setting (S227 a)")
    check(quit(musicQuit, .system, .system) == "keep" && quit(musicQuit, .input(uid: "mic"), .system) == "keep"
            && quit(.terminated(bundleID: nil, streaming: true), music, .system) == "keep"
            && quit(.terminated(bundleID: "com.spotify.client", streaming: true), music, .system) == "keep",
          "EC6 app quit: another source, another app or an app without a bundle ID keeps streaming (S227 b)")
    for setting in [WhenSourceQuits.stop, .system] {
        check(quit(musicQuit, .system, setting) == "keep",
              "EC6 app quit: the whole-system source never ends on a quit, under either setting (S164, S227 b)", "\(setting)")
        check(quit(.terminated(bundleID: nil, streaming: true), music, setting) == "keep",
              "EC6 app quit: an app without a bundle ID changes nothing, under either setting (S164)", "\(setting)")
    }

    // S227: the setting, through the core's decision.
    check(quit(musicQuit, music, .system) == "switchToSystem com.apple.Music",
          "source quit: with the switch on, the source app quitting while streaming switches to the whole system (S227)",
          quit(musicQuit, music, .system))
    let lost = Report.captureLost(.appNotRunning("com.apple.Music"))
    check(quit(lost) == "stop com.apple.Music" && quit(lost, music, .system) == "switchToSystem com.apple.Music",
          "source quit: the capture finding its app gone counts as that app quitting (S227 f)",
          "\(quit(lost)), \(quit(lost, music, .system))")
    check(quit(.captureLost(.inputDisconnected), music, .system) == "keep"
            && quit(.captureLost(.aggregateDeviceFailed(1)), music, .system) == "keep",
          "source quit: a capture lost for another cause is no app quit (S21 still applies)")
    let rebuild = Report.rebuildFailed(SourceCapture.CaptureError.appNotRunning("com.apple.Music"))
    check(quit(rebuild) == "stop com.apple.Music" && quit(rebuild, music, .system) == "switchToSystem com.apple.Music",
          "source quit: an app gone when the format-change rebuild starts stops it, or rebuilds with the whole system (S227 e)",
          "\(quit(rebuild)), \(quit(rebuild, music, .system))")
    check(quit(.rebuildFailed(SourceCapture.CaptureError.formatUnusable(1)), music, .system) == "keep",
          "source quit: a rebuild that fails for another cause is no app quit")
    // After a switch the source is the whole system; after a stop nothing streams.
    check(quit(musicQuit, .system, .system) == "keep"
            && quit(.terminated(bundleID: "com.apple.Music", streaming: false)) == "keep",
          "source quit: the same quit reported again (the capture, then macOS) acts once (S227 f)")
    check(WhenSourceQuits(defaultsValue: nil) == .stop && WhenSourceQuits(defaultsValue: "stop") == .stop
            && WhenSourceQuits(defaultsValue: "System") == .stop && WhenSourceQuits(defaultsValue: "switch") == .stop
            && WhenSourceQuits(defaultsValue: "system") == .system,
          "source quit: the setting reads stop unless it says system (S227)")
    let t = UIStrings()
    let switched = t.sourceSwitchedToSystem(name: "Music", started: true)
    let notStarted = t.sourceSwitchedToSystem(name: "Music", started: false)
    check(switched.contains("Music") && switched.contains(t.sourceSystem)
            && notStarted.contains("Music") && notStarted.contains(t.sourceSystem) && notStarted != switched,
          "source quit: the notice names the app and All System Audio, also when the whole system couldn't start (S227)",
          "\(switched) / \(notStarted)")
}

// MARK: The way out on an exit signal (SignalQuit, AppLifecycle.swift)
/// What a `SignalQuit` did, in order. Its quit steps end only when the test
/// says so (`finishQuit`); its exit records the status instead of exiting.
final class SignalQuitProbe: @unchecked Sendable {
    private let lock = NSLock()
    private var entries: [String] = []
    private var pendingDone: (@Sendable () -> Void)?
    let exited = DispatchSemaphore(value: 0)

    var log: [String] { lock.lock(); defer { lock.unlock() }; return entries }
    func note(_ entry: String) { lock.lock(); entries.append(entry); lock.unlock() }

    /// The quit steps end now: noted, then reported to the `SignalQuit`.
    func finishQuit() {
        lock.lock(); let done = pendingDone; pendingDone = nil; lock.unlock()
        note("quit done")
        done?()
    }

    func make(queue: DispatchQueue, limit: TimeInterval) -> SignalQuit {
        SignalQuit(queue: queue, limit: limit,
                   quitSteps: { done in
                       self.note("quit")
                       self.lock.lock(); self.pendingDone = done; self.lock.unlock()
                   },
                   exitProcess: { status in
                       self.note("exit \(status)")
                       self.exited.signal()
                   })
    }
}
do {
    check(SignalQuit.step(signal: SIGTERM, quitBegun: false) == .quitThenExit(status: 143)
            && SignalQuit.step(signal: SIGINT, quitBegun: false) == .quitThenExit(status: 130),
          "quit: the first exit signal runs the quit steps (the fade-out), then exits with its status (S111)")
    check(SignalQuit.step(signal: SIGINT, quitBegun: true) == .exitNow(status: 130)
            && SignalQuit.step(signal: SIGTERM, quitBegun: true) == .exitNow(status: 143),
          "quit: an exit signal while the quit steps run exits at once with its own status (S111)")
    // The Cast teardown's deadline counts from the start of the quit steps,
    // beside the fades and the AirPlay teardown, so it alone must fit.
    let fadeCap = Double(ASC_STOP_FADE_CAP_MS) / 1000
    check(CastSender.shutdownWait < SignalQuit.limit && fadeCap < CastSender.shutdownWait,
          "quit: the Cast teardown's wait fits under a signal's limit and outlasts the fades (S111, S186)",
          "wait \(CastSender.shutdownWait) s, fades \(fadeCap) s, limit \(SignalQuit.limit) s")

    // The order: the exit waits for the quit steps, then carries the signal's status.
    let ordered = SignalQuitProbe()
    let orderedQueue = DispatchQueue(label: "logtest.signal-quit.order")
    let orderedQuit = ordered.make(queue: orderedQueue, limit: 30)
    orderedQueue.sync { orderedQuit.handle(signal: SIGTERM) }
    Thread.sleep(forTimeInterval: 0.05)
    let beforeDone = ordered.log
    ordered.finishQuit()
    let exitedAfterDone = ordered.exited.wait(timeout: .now() + 2) == .success
    check(beforeDone == ["quit"] && exitedAfterDone && ordered.log == ["quit", "quit done", "exit 143"],
          "quit: SIGTERM runs the quit steps first and exits with 143 only once they are done (S111)",
          "before they were done \(beforeDone), then \(ordered.log)")

    // The bound: quit steps that never end don't keep the app alive.
    let stuck = SignalQuitProbe()
    let stuckQueue = DispatchQueue(label: "logtest.signal-quit.bound")
    let stuckQuit = stuck.make(queue: stuckQueue, limit: 0.2)
    let started = Date()
    stuckQueue.sync { stuckQuit.handle(signal: SIGINT) }
    let exitedInTime = stuck.exited.wait(timeout: .now() + 3) == .success
    let waited = Date().timeIntervalSince(started)
    stuck.finishQuit()   // the steps end after all, too late
    Thread.sleep(forTimeInterval: 0.05)
    check(exitedInTime && waited >= 0.19 && stuck.log == ["quit", "exit 130", "quit done"],
          "quit: Ctrl-C exits with 130 once the bound has passed when the quit steps don't end, and only once (S111)",
          "after \(String(format: "%.2f", waited)) s: \(stuck.log)")

    // A second signal while the quit steps run: out at once, and only once.
    let twice = SignalQuitProbe()
    let twiceQueue = DispatchQueue(label: "logtest.signal-quit.twice")
    let twiceQuit = twice.make(queue: twiceQueue, limit: 0.2)
    twiceQueue.sync { twiceQuit.handle(signal: SIGTERM) }
    twiceQueue.sync { twiceQuit.handle(signal: SIGINT) }
    let atOnce = twice.log
    Thread.sleep(forTimeInterval: 0.35)   // past the bound
    twice.finishQuit()
    Thread.sleep(forTimeInterval: 0.05)
    check(atOnce == ["quit", "exit 130"] && twice.log == ["quit", "exit 130", "quit done"],
          "quit: a second Ctrl-C while the quit steps run exits at once with 130, and nothing exits again later (S111)",
          "at once \(atOnce), later \(twice.log)")
}

// MARK: Microphone calibration — what the estimate does to an offset (CalibrationFlow.swift, S232-S234)
do {
    func answer(_ status: Int32, recording: Int32 = ASC_CALIBRATION_RECORDING_NONE, offset: Double = 0,
                clamped: Bool = false) -> AscCalibration {
        var r = AscCalibration()
        r.status = UInt32(status)
        r.recording = UInt32(recording)
        r.offset_ms = offset
        r.clamped = clamped
        return r
    }
    let t = UIStrings()
    var offsets = ["usb": 150.0]
    let commit: (Double) -> Void = { offsets["usb"] = $0 }

    let refusals: [(AscCalibration?, CalibrationFailure)] = [
        (answer(ASC_CALIBRATION_TOO_SHORT, recording: ASC_CALIBRATION_RECORDING_REFERENCE, offset: 150), .tooShort(.reference)),
        (answer(ASC_CALIBRATION_TOO_LONG, recording: ASC_CALIBRATION_RECORDING_CHOSEN, offset: 150), .tooLong(.chosen)),
        (answer(ASC_CALIBRATION_CLIPPED, recording: ASC_CALIBRATION_RECORDING_CHOSEN, offset: 150), .clipped(.chosen)),
        (answer(ASC_CALIBRATION_TOO_QUIET, recording: ASC_CALIBRATION_RECORDING_REFERENCE, offset: 150), .tooQuiet(.reference)),
        (answer(ASC_CALIBRATION_AMBIGUOUS_ECHO, recording: ASC_CALIBRATION_RECORDING_CHOSEN, offset: 150), .ambiguousEcho(.chosen)),
        (answer(ASC_CALIBRATION_IMPLAUSIBLE, offset: 150), .implausible),
        (nil, .tooShort(nil)),
    ]
    for (result, failure) in refusals {
        let phase = CalibrationFlow.finish(result, commit: commit)
        let message = t.calibrationMessage(phase) ?? ""
        check(phase == .failed(failure) && offsets == ["usb": 150] && message.contains("wasn't changed"),
              "calibration: a refusal (\(failure)) changes no offset and says why (S232)", "\(phase), \(offsets), \(message)")
    }

    // Refusals the host makes before the estimate say why and that nothing changed.
    let hostRefusals: [CalibrationFailure] = [.microphoneDenied, .noMicrophone, .sourceSilent, .interrupted,
                                              .notPlaying, .noReference, .unsupportedRate]
    for failure in hostRefusals {
        let message = t.calibrationMessage(.failed(failure)) ?? ""
        check(message.hasSuffix(" The delay wasn't changed.") && message.count > " The delay wasn't changed.".count + 10,
              "calibration: a refusal (\(failure)) says why and that the delay wasn't changed", message)
    }

    // Both runs record with one input: a switch to another device, even at
    // the same rate, would change the input latency between them (S232).
    let builtIn = CalibrationMicrophoneID(device: 51, rate: 48_000)
    check(CalibrationFlow.microphoneCheck(first: nil, now: builtIn, largestHoldBackMs: 2000) == nil
            && CalibrationFlow.microphoneCheck(first: builtIn, now: builtIn, largestHoldBackMs: 2000) == nil
            && CalibrationFlow.microphoneCheck(first: builtIn, now: CalibrationMicrophoneID(device: 77, rate: 48_000),
                                               largestHoldBackMs: 2000) == .interrupted
            && CalibrationFlow.microphoneCheck(first: builtIn, now: CalibrationMicrophoneID(device: 51, rate: 44_100),
                                               largestHoldBackMs: 2000) == .interrupted
            && CalibrationFlow.microphoneCheck(first: nil, now: CalibrationMicrophoneID(device: 51, rate: 192_000),
                                               largestHoldBackMs: 20_000) == .tooLong(nil),
          "calibration: the chosen speaker's run must record with the reference's microphone")

    let accepted = CalibrationFlow.finish(answer(ASC_CALIBRATION_ACCEPTED, offset: -37), commit: commit)
    let acceptedText = t.calibrationMessage(accepted) ?? ""
    check(accepted == .done(offsetMs: -37, clamped: false) && offsets == ["usb": -37]
            && acceptedText == "Delay set to −37 ms.",
          "calibration: an accepted estimate commits its offset through the offset path (S232)",
          "\(accepted), \(offsets), \(acceptedText)")

    let clamped = CalibrationFlow.finish(answer(ASC_CALIBRATION_ACCEPTED, offset: -2000, clamped: true), commit: commit)
    let clampedText = t.calibrationMessage(clamped) ?? ""
    check(clamped == .done(offsetMs: -2000, clamped: true) && offsets == ["usb": -2000]
            && clampedText.contains("end of the range") && clampedText.contains("out of sync")
            && clampedText != t.calibrationMessage(.done(offsetMs: -2000, clamped: false)),
          "calibration: a clamped estimate is filled in and says it was clamped (S232)", clampedText)

    check(CalibrationFlow.finish(answer(ASC_CALIBRATION_ACCEPTED, offset: .nan), commit: commit) == .failed(.implausible)
            && offsets == ["usb": -2000],
          "calibration: an accepted answer without a finite offset changes nothing")

    // When one may start: both the speaker and the reference play (S232).
    check(CalibrationFlow.blocker(streaming: true, reference: nil, chosen: "usb", playing: ["usb"]) == .noReference
            && CalibrationFlow.blocker(streaming: false, reference: "mac", chosen: "usb", playing: []) == .notPlaying
            && CalibrationFlow.blocker(streaming: true, reference: "mac", chosen: "usb", playing: ["usb"]) == .notPlaying
            && CalibrationFlow.blocker(streaming: true, reference: "mac", chosen: "mac", playing: ["mac"]) == .notPlaying
            && CalibrationFlow.blocker(streaming: true, reference: "mac", chosen: "usb", playing: ["mac", "usb"]) == nil,
          "calibration: starts only while streaming with the chosen speaker and the reference playing")

    // Frame 0 is the moment the signal entered the outputs' paths, and each
    // recording covers the largest hold-back, latency, the signal and a margin.
    check(CalibrationFlow.trimStart(signalNs: 1_250_000_000, firstFrameNs: 1_000_000_000, rate: 48_000) == 12_000
            && CalibrationFlow.trimStart(signalNs: 999_000_000, firstFrameNs: 1_000_000_000, rate: 48_000) == nil,
          "calibration: each recording starts where the signal entered, and one that began later is refused")
    check(CalibrationFlow.recordingFrames(largestHoldBackMs: 2000, rate: 48_000) == 180_000
            && CalibrationFlow.recordingFrames(largestHoldBackMs: .nan, rate: 48_000) == 84_000,
          "calibration: a recording lasts the largest hold-back plus latency, the 0.75 s signal and 0.5 s (S232)",
          "\(CalibrationFlow.recordingFrames(largestHoldBackMs: 2000, rate: 48_000))")

    // The whole path through the core on synthetic recordings: the chosen
    // output heard 100 ms after the reference is held back 100 ms less.
    if let signal = CalibrationFlow.signal(rate: 48_000) {
        func room(arrivalFrame: Int) -> [Float] {
            var out = [Float](repeating: 0, count: 96_000)
            var seed: UInt32 = 7
            for i in out.indices {
                seed = seed &* 1_664_525 &+ 1_013_904_223
                out[i] = (Float(seed >> 8) / Float(1 << 24) - 0.5) * 0.002
            }
            for (i, s) in signal.enumerated() { out[arrivalFrame + i] += s * 0.5 }
            return out
        }
        var stored = ["usb": 150.0]
        let result = CalibrationFlow.estimate(signal: signal, reference: room(arrivalFrame: 4_800),
                                              chosen: room(arrivalFrame: 9_600), rate: 48_000, oldOffsetMs: 150)
        let phase = CalibrationFlow.finish(result) { stored["usb"] = $0 }
        check(phase == .done(offsetMs: 50, clamped: false) && stored == ["usb": 50],
              "calibration: a speaker heard 100 ms after the reference ends up 100 ms lower (S232)",
              "\(phase), \(stored)")
        let silent = [Float](repeating: 0, count: 96_000)
        let refused = CalibrationFlow.finish(CalibrationFlow.estimate(signal: signal, reference: silent,
                                                                      chosen: room(arrivalFrame: 9_600),
                                                                      rate: 48_000, oldOffsetMs: 150)) { stored["usb"] = $0 }
        check(refused == .failed(.tooQuiet(.reference)) && stored == ["usb": 50],
              "calibration: a silent reference recording is refused and changes nothing (S234)", "\(refused)")
    } else {
        check(false, "calibration: the core makes the test signal at 48 kHz (S233)")
    }

    // A run cut off while its sweep is on the way keeps the one output heard
    // and the music silent until the sweep has played out through the
    // hold-backs; one cut off before it played gives the outputs back at once.
    let settle = CalibrationFlow.settleSeconds(signalSent: true, sameSession: true, largestHoldBackMs: 2000)
    check(settle == CalibrationFlow.quietSeconds(largestHoldBackMs: 2000) && settle >= 2
            && CalibrationFlow.settleSeconds(signalSent: false, sameSession: true, largestHoldBackMs: 2000) == 0
            && CalibrationFlow.settleSeconds(signalSent: true, sameSession: false, largestHoldBackMs: 2000) == 0,
          "calibration: an abort during playing restores the outputs only after the sweep played out (S185)",
          "\(settle) s")

    // A recording past the estimate's limit at the microphone's rate is refused before anything plays.
    check(CalibrationFlow.fits(largestHoldBackMs: 20_000, rate: 48_000)
            && !CalibrationFlow.fits(largestHoldBackMs: 20_000, rate: 192_000)
            && CalibrationFlow.fits(largestHoldBackMs: 2000, rate: 192_000),
          "calibration: a long hold-back at a high microphone rate is refused up front (S232, S234)")

    check(t.calibrationMessage(.cancelled) == "Calibration cancelled. The delay wasn't changed."
            && t.calibrationMessage(.running(.preparing)) == nil && t.calibrationMessage(.idle) == nil,
          "calibration: a cancel says nothing changed")

    // A shown line goes once it no longer describes the speaker.
    let shownIn = CalibrationContext(streaming: true, reference: "mac", speakerOn: true)
    let shown = CalibrationStatus(uid: "usb", phase: .done(offsetMs: 40, clamped: false), context: shownIn)
    var streamingOff = shownIn
    streamingOff.streaming = false
    var otherReference = shownIn
    otherReference.reference = nil
    var speakerOff = shownIn
    speakerOff.speakerOn = false
    let measuring = CalibrationStatus(uid: "usb", phase: .running(.measuring(.reference)), context: shownIn,
                                      holdsOutputs: true)
    check(shown.following(shownIn) == shown
            && [streamingOff, otherReference, speakerOff].allSatisfy { shown.following($0).phase(of: "usb") == .idle }
            && shown.offsetChanged("usb").phase(of: "usb") == .idle && shown.offsetChanged("bt") == shown
            && measuring.following(streamingOff) == measuring && measuring.offsetChanged("usb") == measuring,
          "calibration: a result goes when its offset, streaming, the reference or the speaker's switch changes")

    check(CalibrationFlow.unavailable(streaming: false, hasReference: true, speakerOn: true) == .notPlaying
            && CalibrationFlow.unavailable(streaming: true, hasReference: true, speakerOn: false) == .notPlaying
            && CalibrationFlow.unavailable(streaming: true, hasReference: false, speakerOn: true) == .noReference
            && CalibrationFlow.unavailable(streaming: true, hasReference: true, speakerOn: true) == nil,
          "calibration: the button is off while streaming or the speaker is off, or without a reference")

    let status = CalibrationStatus(uid: "usb", phase: .running(.measuring(.chosen)))
    check(status.phase(of: "usb") == .running(.measuring(.chosen)) && status.phase(of: "bt") == .idle && status.isRunning,
          "calibration: only the speaker being calibrated shows its progress")
}

// MARK: Calibrate All Speakers: the plan, the stops and the sheet (CalibrateAllFlow.swift, S235-S240)
do {
    let t = UIStrings()

    var inputs = CalibrateAllFlow.Inputs(
        streaming: true, reference: "mac",
        localOutputs: [(id: "usb", name: "Desk"), (id: "mac", name: "MacBook Speakers")],
        localOffsets: ["usb": 150], holdBacks: ["mac": 1750, "usb": 1900],
        cast: (id: "cast:tv", name: "TV", offsetMs: nil, waitMs: 0, format: ASC_CAST_FORMAT_WAV),
        airPlay: [(id: "native:a", name: "Kitchen"), (id: "native:b", name: "Bedroom")],
        airPlayDelayMs: 1750, airPlayHoldBackMs: 0)
    let planned = (try? CalibrateAllFlow.plan(inputs).get()) ?? []
    check(planned.map(\.id) == ["mac", "usb", "cast:tv", "native:a", "native:b"]
            && planned[0].offsetMs == nil && planned[1].offsetMs == 150 && planned[1].latencyMs == 2400
            && planned[2].offsetMs == nil && planned[2].latencyMs == 3000
            && planned[3].offsetMs == 1750 && planned[3].latencyMs == 2250 && planned[4].latencyMs == 2250,
          "calibrate all: the reference first, then each speaker with its stored delay and expected arrival, "
            + "one for every AirPlay speaker, so a sweep has left them all when its window or play-out ends (S235)",
          "\(planned)")
    check(CalibrateAllFlow.quietSeconds(planned) == 3 && CalibrateAllFlow.settleSeconds(planned[3]) == 3,
          "calibrate all: the music on its way plays out before the first sweep, a Cast speaker's unknown latency included",
          "\(CalibrateAllFlow.quietSeconds(planned))")

    var held = inputs
    held.cast = (id: "cast:tv", name: "TV", offsetMs: nil, waitMs: 1750, format: ASC_CAST_FORMAT_FLAC)
    let heldPlan = (try? CalibrateAllFlow.plan(held).get()) ?? []
    held.cast = (id: "cast:tv", name: "TV", offsetMs: 1000, waitMs: 750, format: ASC_CAST_FORMAT_FLAC)
    let measuredPlan = (try? CalibrateAllFlow.plan(held).get()) ?? []
    check(heldPlan.count > 2 && heldPlan[2].latencyMs == 7750 && CalibrateAllFlow.quietSeconds(heldPlan) == 7.75
            && measuredPlan.count > 2 && measuredPlan[2].latencyMs == 2250,
          "calibrate all: a Cast speaker without an offset waits its feed's hold-back plus the FLAC guess, "
            + "one with an offset its offset, hold-back and allowance (S241)", "\(heldPlan) \(measuredPlan)")

    var alone = inputs
    alone.localOutputs = [(id: "mac", name: "MacBook Speakers")]
    alone.cast = nil
    alone.airPlay = []
    var stopped = inputs
    stopped.streaming = false
    var noReference = inputs
    noReference.reference = nil
    var referenceOff = inputs
    referenceOff.localOutputs = [(id: "usb", name: "Desk")]
    check(CalibrateAllFlow.plan(alone) == .failure(.noOtherSpeaker)
            && CalibrateAllFlow.plan(stopped) == .failure(.notPlaying)
            && CalibrateAllFlow.plan(noReference) == .failure(.noReference)
            && CalibrateAllFlow.plan(referenceOff) == .failure(.notPlaying),
          "calibrate all: starts only while streaming with the built-in speakers and another speaker playing")

    // Any mid-run change stops the run (S240).
    let taking = ["mac", "usb", "native:a"]
    let started = CalibrateAllContext(source: "system", session: 3, streaming: true,
                                      switchedOn: ["mac", "usb", "native:a"], playing: ["mac", "usb", "native:a"],
                                      delays: ["usb": 150, "hold-back usb": 1900])
    var source = started
    source.source = "com.example.player"
    var speakerOff = started
    speakerOff.playing.remove("native:a")
    var streamingOff = started
    streamingOff.streaming = false
    var added = started
    added.switchedOn.insert("native:b")
    var restarted = started
    restarted.session = 4
    var moved = started
    moved.delays["hold-back usb"] = 1800
    let ends = [source, speakerOff, streamingOff, added, restarted, moved]
        .map { CalibrateAllFlow.end(start: started, now: $0, taking: taking) }
    check(CalibrateAllFlow.end(start: started, now: started, taking: taking) == nil
            && ends == [.sourceChanged, .speakerOff, .speakerOff, .speakersChanged, .speakersChanged, .delaysChanged],
          "calibrate all: a switched source, a speaker turned off, a changed list or a moved delay stops the run (S240)",
          "\(ends)")
    var connecting = started
    connecting.switchedOn.insert("native:b")
    var connected = connecting
    connected.playing.insert("native:b")
    check(CalibrateAllFlow.end(start: connecting, now: connecting, taking: taking) == nil
            && CalibrateAllFlow.end(start: connecting, now: connected, taking: taking) == .speakersChanged,
          "calibrate all: a speaker still connecting at the start that begins playing stops the run, unmuted (S240)")
    check(t.calibrateAllLine(.ended(.rateChanged))
                == "The audio's sample rate changed during the measurement. Nothing was changed."
            && t.calibrateAllLine(.cannotMeasureAgain(name: "Desk"))
                == "Turn on streaming, the built-in speakers and Desk to measure it again.",
          "calibrate all: a changed sample rate and a Measure Again that can't start say so in their own words")
    check(CalibrateAllEnd.delaysChanged.core == UInt32(ASC_CALIBRATION_END_CANCELLED)
            && CalibrateAllEnd.speakerOff.core == UInt32(ASC_CALIBRATION_END_SPEAKER_OFF)
            && t.calibrateAllLine(.ended(.sourceChanged))
                == "The source was switched during the measurement. Nothing was changed.",
          "calibrate all: a stopped run tells the core why and says that nothing changed")

    // A whole run through the core on synthetic recordings at 48 kHz.
    if let signal = CalibrationFlow.signal(rate: 48_000) {
        func room(_ arrival: Int?, frames: Int, seed: UInt32) -> [Float] {
            var out = [Float](repeating: 0, count: frames)
            var s = seed
            for i in out.indices {
                s = s &* 1_664_525 &+ 1_013_904_223
                out[i] = (Float(s >> 8) / Float(1 << 24) - 0.5) * 0.002
            }
            if let arrival {
                for (i, v) in signal.enumerated() where arrival + i < frames { out[arrival + i] += v * 0.5 }
            }
            return out
        }
        func speaker(_ id: String, _ kind: CalibrateAllKind, _ offset: Double?) -> CalibrateAllSpeaker {
            CalibrateAllSpeaker(id: id, name: id, kind: kind, offsetMs: offset, latencyMs: 100)
        }
        let speakers = [speaker("mac", .local, nil), speaker("usb", .local, 150), speaker("hdmi", .local, 0),
                        speaker("bt", .local, -200), speaker("native:a", .airPlay, 1750),
                        speaker("native:b", .airPlay, 1750)]
        // Frames after the reference's 4800: usb 100 ms later both times, hdmi's
        // sweeps 30 ms apart, bt never heard, the AirPlay speakers 200 and 230 ms later.
        func arrival(row: Int, sweep: UInt32) -> Int? {
            switch row {
            case 0: 4_800
            case 1: 9_600
            case 2: sweep == 1 ? 9_600 : 11_040
            case 3: nil
            case 4: 14_400
            default: 15_840
            }
        }
        func play(_ run: CalibrateAllRunHandle, arrivals: (Int, UInt32) -> Int?) -> [String] {
            var steps: [String] = []
            while let step = run.step() {
                steps.append("\(step.row).\(step.sweep)")
                run.record(room(arrivals(step.row, step.sweep), frames: step.window_frames, seed: UInt32(step.row) + 7))
            }
            return steps
        }
        if let run = CalibrateAllRunHandle(rate: 48_000, speakers: speakers) {
            let firstStep = run.step()
            let steps = play(run, arrivals: arrival)
            let sheet = run.finish().map { CalibrateAllSheetHandle($0, speakers: speakers, rate: 48_000, microphone: nil) }
            let shown = sheet?.read(ended: nil)
            check(firstStep?.window_frames == 88_800
                    && steps == ["0.0", "1.1", "1.2", "2.1", "2.2", "3.1", "4.1", "4.2", "5.1", "5.2"],
                  "calibrate all: the reference's sweep, then each speaker's two with a window from its arrival; "
                    + "a refused first sweep skips the second (S235, S236)", "\(steps)")
            let rows = shown?.rows.map(\.result) ?? []
            var usbOK = false, hdmiUncertain = false
            if rows.count == 6, case .certain(let ms, let u, false) = rows[1] { usbOK = ms == 50 && u >= 3 && u < 4 }
            if rows.count == 6, case .uncertain(_, let u) = rows[2] { hdmiUncertain = u > 10 }
            check(rows.first == .reference && usbOK && hdmiUncertain && rows.count == 6
                    && rows[3] == .refused(.tooQuiet(.chosen)),
                  "calibrate all: certain, uncertain and refused rows from the core's sheet (S236)", "\(rows)")
            if let shown, rows.count == 6 {
                let usb = t.calibrateAllResult(shown.rows[1])
                let hdmi = t.calibrateAllResult(shown.rows[2])
                let bt = t.calibrateAllResult(shown.rows[3])
                check(usb == "+50 ms ± 3 ms" && hdmi.contains("differ too much") && hdmi.contains("delay stays")
                        && bt.contains("barely heard this speaker") && bt.hasSuffix("The delay stays.")
                        && !(usb + hdmi + bt).contains("—"),
                      "calibrate all: each row says its new delay with its uncertainty, or why it stays",
                      "\(usb) | \(hdmi) | \(bt)")
            }
            let airPlay = shown?.airPlay
            let airPlayText = airPlay.map(t.calibrateAllAirPlay) ?? ""
            check(airPlay?.state == .spread && airPlay?.newMs == 1965 && airPlay?.certainRows == 2
                    && airPlayText.contains("AirPlay delay: was 1750 ms. New: 1965 ms")
                    && airPlayText.contains("differ by 30 ms") && airPlayText.contains("one delay for all AirPlay speakers"),
                  "calibrate all: the AirPlay delay is the median, and a spread over 10 ms is said in plain words (S237)",
                  airPlayText)

            // Measure Again replaces only its row; one cut off leaves the row as it was (S239).
            if let sheet, let again = sheet.measureAgain(1) {
                let againSteps = play(again, arrivals: { row, _ in row == 0 ? 4_800 : 7_200 })
                let replaced = again.finish().map(sheet.replace) ?? false
                var moved = false
                if case .certain(let ms, _, _) = sheet.read(ended: nil).rows[1].result { moved = ms == 100 }
                check(againSteps == ["0.0", "1.1", "1.2"] && replaced && moved
                        && sheet.read(ended: nil).rows[2].result == rows[2],
                      "calibrate all: Measure Again runs the reference and that row, and replaces that row alone",
                      "\(againSteps), \(sheet.read(ended: nil).rows[1].result)")
            }
            if let sheet, let cut = sheet.measureAgain(2) {
                let before = sheet.read(ended: nil)
                cut.cancel(.speakerOff)
                let replaced = cut.finish().map(sheet.replace) ?? true
                check(!replaced && sheet.read(ended: nil) == before,
                      "calibrate all: a Measure Again stopped by a change is dropped (S239, S240)")
            }

            // Apply: one change per certain local row and one for the AirPlay delay (S238).
            if let sheet {
                let answer = sheet.apply()
                let writes = CalibrateAllFlow.writes(answer.changes, speakers: speakers)
                check(answer.code == UInt32(ASC_CALIBRATION_APPLY_OK) && writes.count == 2
                        && writes.contains(.local(id: "usb", ms: 100, old: 150))
                        && writes.contains(.airPlay(ms: 1965, old: 1750)),
                      "calibrate all: Apply lists the certain rows and the AirPlay delay with the values they replace",
                      "\(writes)")
                check(sheet.apply().code == UInt32(ASC_CALIBRATION_APPLY_ALREADY_APPLIED) && sheet.measureAgain(1) == nil,
                      "calibrate all: an applied sheet is spent: no second Apply, no Measure Again (S238)")
            }
        } else {
            check(false, "calibrate all: the core takes a run of local and AirPlay speakers at 48 kHz (S235)")
        }

        // A run stopped after the reference keeps its rows for display and applies nothing (S240).
        if let run = CalibrateAllRunHandle(rate: 48_000, speakers: Array(speakers.prefix(2))) {
            if let step = run.step() { run.record(room(4_800, frames: step.window_frames, seed: 7)) }
            run.cancel(.sourceChanged)
            if let pointer = run.finish() {
                let sheet = CalibrateAllSheetHandle(pointer, speakers: Array(speakers.prefix(2)), rate: 48_000,
                                                    microphone: nil)
                let shown = sheet.read(ended: .sourceChanged)
                check(asc_calibration_sheet_ended(pointer) == UInt32(ASC_CALIBRATION_END_SOURCE_CHANGED)
                        && !shown.canApply && shown.rows[1].result == .notMeasured
                        && sheet.apply().code == UInt32(ASC_CALIBRATION_APPLY_ENDED),
                      "calibrate all: a run stopped by a switched source applies nothing")
            }
        }
    } else {
        check(false, "calibrate all: the core makes the test signal at 48 kHz (S233)")
    }

    // The stored values must still be the ones measured from.
    let measuredFrom = [CalibrateAllSpeaker(id: "mac", name: "Mac", kind: .local, offsetMs: nil, latencyMs: 0),
                        CalibrateAllSpeaker(id: "usb", name: "Desk", kind: .local, offsetMs: 150, latencyMs: 0)]
    check(CalibrateAllFlow.unchanged(measuredFrom, current: { $0.id == "usb" ? 150 : 9 })
            && !CalibrateAllFlow.unchanged(measuredFrom, current: { $0.id == "usb" ? 120 : nil }),
          "calibrate all: Apply writes nothing once a delay moved after the measurement")

    // Undo puts back what Apply replaced, and forgets a delay that wasn't stored (S238).
    let previous = AirPlayDelay(adjustmentMs: 250)
    let undo = CalibrateAllUndo(writes: [.local(id: "usb", ms: 100, old: 150), .local(id: "hdmi", ms: 40, old: nil),
                                         .cast(id: "cast:tv", ms: 900, old: nil), .cast(id: "cast:hall", ms: 300, old: 250),
                                         .airPlay(ms: 1965, old: 1750)],
                                previousAirPlay: previous)
    let restores = CalibrateAllFlow.restores(undo)
    check(restores == [.airPlay(previous), .cast(id: "cast:hall", ms: 250), .cast(id: "cast:tv", ms: nil),
                       .local(id: "hdmi", ms: nil), .local(id: "usb", ms: 150)],
          "calibrate all: Undo restores each replaced delay and the AirPlay delay, and forgets one that wasn't stored",
          "\(restores)")
    var castMemory = CastModel()
    _ = castMemory.setOffset("tv", ms: 900)
    for case .cast(let id, .none) in restores where id == "cast:tv" { _ = castMemory.clearOffset("tv") }
    var again = inputs
    again.cast = (id: "cast:tv", name: "TV", offsetMs: castMemory.offsets["tv"], waitMs: 0, format: ASC_CAST_FORMAT_WAV)
    let replanned = (try? CalibrateAllFlow.plan(again).get()) ?? []
    check(castMemory.offsets["tv"] == nil && !castMemory.clearOffset("tv")
            && replanned.count > 2 && replanned[2].offsetMs == nil && replanned[2].latencyMs == 3000,
          "calibrate all: after Undo a Cast speaker without a stored offset is searched again, not measured from 0 ms",
          "\(replanned)")
    castMemory.apply(.changed(CastDevice(id: "tv", name: "TV"), offsetMs: 0))
    again.cast = (id: "cast:tv", name: "TV", offsetMs: castMemory.offsets["tv"], waitMs: 0, format: ASC_CAST_FORMAT_WAV)
    let reannounced = (try? CalibrateAllFlow.plan(again).get()) ?? []
    check(castMemory.offsets["tv"] == nil && reannounced.count > 2 && reannounced[2].offsetMs == nil,
          "calibrate all: the 0 ms the sender keeps after Undo doesn't come back when the device re-announces",
          "\(castMemory.offsets)")
    var castAfterRestart = CastModel()
    castAfterRestart.apply(.appeared(CastDevice(id: "tv", name: "TV"), offsetMs: 0))
    again.cast = (id: "cast:tv", name: "TV", offsetMs: castAfterRestart.offsets["tv"], waitMs: 0, format: ASC_CAST_FORMAT_WAV)
    let afterRestart = (try? CalibrateAllFlow.plan(again).get()) ?? []
    check(castAfterRestart.offsets["tv"] == nil && castAfterRestart.offset(of: "tv") == 0
            && afterRestart.count > 2 && afterRestart[2].offsetMs == nil,
          "calibrate all: after Undo and a restart, the 0 ms the sender reports is no stored offset",
          "\(afterRestart)")
    _ = castMemory.setOffset("tv", ms: 300)
    castMemory.apply(.changed(CastDevice(id: "tv", name: "TV"), offsetMs: 300))
    check(castMemory.offsets["tv"] == 300 && castMemory.setOffset("tv", ms: 0) == 0
            && castMemory.offsets["tv"] == nil && castMemory.offset(of: "tv") == 0
            && castMemory.setOffset("tv", ms: 0) == nil,
          "calibrate all: an offset the user sets after Undo is kept again, and 0 ms reads 0 ms but counts as none",
          "\(castMemory.offsets)")

    // Two or more AirPlay rows always show their spread; the plain sentence only over 10 ms (S237).
    let close = t.calibrateAllAirPlay(.init(state: .certain, oldMs: 1750, newMs: 1960, spreadMs: 6, certainRows: 3))
    let apart = t.calibrateAllAirPlay(.init(state: .spread, oldMs: 1750, newMs: 1965, spreadMs: 30, certainRows: 2))
    let single = t.calibrateAllAirPlay(.init(state: .certain, oldMs: 1750, newMs: 1940, spreadMs: 0, certainRows: 1))
    check(close == "AirPlay delay: was 1750 ms. New: 1960 ms, the middle value of 3 AirPlay speakers, which are 6 ms apart."
            && apart.contains("2 AirPlay speakers, which are 30 ms apart.") && apart.contains("differ by 30 ms")
            && single == "AirPlay delay: was 1750 ms. New: 1940 ms.",
          "calibrate all: the AirPlay median shows its spread, and only a spread over 10 ms gets the plain sentence",
          close + " | " + apart + " | " + single)

    check(t.calibrateAllProgress(.listening(name: "Desk", sweep: 2)) == "Listening to Desk, sweep 2 of 2…"
            && t.calibrateAllProgress(.listening(name: "MacBook Speakers", sweep: 0)) == "Listening to MacBook Speakers…"
            && t.calibrateAllProgress(.searching(name: "TV")).contains("up to a minute"),
          "calibrate all: the progress line names the speaker and the sweep, and says when a Cast speaker is searched")
}

// MARK: Google Cast speakers: one session, matched by attempt (CastModel)
do {
    let kitchen = "0123456789abcdef0123456789abcdef"
    let den = "fedcba9876543210fedcba9876543210"
    var m = CastModel()
    m.apply(.appeared(CastDevice(id: kitchen, name: "Kitchen"), offsetMs: 250))
    m.apply(.appeared(CastDevice(id: den, name: "Den"), offsetMs: nil))
    m.apply(.appeared(CastDevice(id: "group1", name: "Everywhere", group: true), offsetMs: nil))
    check(m.rows == [den, "group1", kitchen] && m.offset(of: kitchen) == 250 && m.offset(of: den) == 0,
          "cast: rows by name, offsets from the device memory")

    check(m.turnOn("group1") == nil && m.enabled == "group1" && m.turnOff("group1") && m.enabled == nil,
          "cast: a group turns on and off like any speaker")
    check(m.turnOn(kitchen) == nil && m.turnOn(den) == .busy && m.enabled == kitchen,
          "cast: a second speaker stays off while one is on")
    check(m.plan(streaming: false) == .none && m.plan(streaming: true) == .start(kitchen),
          "cast: a session starts only while streaming")
    m.startSent(kitchen)
    check(m.plan(streaming: true) == .none && m.status(of: kitchen) == .connecting,
          "cast: nothing new while a start waits for its attempt number")
    check(m.started(kitchen, attempt: 3, refusal: ASC_CAST_REFUSED_NONE) == .changed
            && m.attempt?.number == 3 && m.status(of: kitchen) == .connecting,
          "cast: the attempt number comes back, connecting")

    check(m.apply(.started(attempt: 2, format: ASC_CAST_FORMAT_WAV)) == nil && m.status(of: kitchen) == .connecting,
          "cast: an event of an older attempt is dropped")
    check(m.apply(.ended(attempt: 2, .failed(.connect))) == nil && m.enabled == kitchen,
          "cast: a late end of an older attempt never turns the speaker off")
    check(m.playingFormat == 0 && m.apply(.started(attempt: 3, format: ASC_CAST_FORMAT_WAV)) == .playing(kitchen)
            && m.status(of: kitchen) == .playing && m.playingFormat == ASC_CAST_FORMAT_WAV,
          "cast: the current attempt plays, in the format it reports")
    var fellBack = m
    check(fellBack.apply(.formatFallback(attempt: 3, format: ASC_CAST_FORMAT_FLAC)) == nil
            && fellBack.playingFormat == ASC_CAST_FORMAT_FLAC,
          "cast: a format fallback changes the format Calibrate All waits for (S241)")
    check(m.apply(.volume(attempt: 3, CastVolume(level: 0.4, muted: true, fixed: false))) == .changed
            && approx(m.volume(of: kitchen), 40, 1e-9) && m.volumes[kitchen]?.muted == true,
          "cast: the device's volume and mute follow its status")
    check(m.setVolume(kitchen, position: 70) == 0.7 && approx(m.volume(of: kitchen), 70, 1e-9)
            && m.setVolume(den, position: 50) == nil && m.setVolume(kitchen, position: .nan) == nil,
          "cast: the slider sets the device's level only while its session runs")
    check(m.apply(.recovering(attempt: 3)) == .changed && m.status(of: kitchen) == .reconnecting
            && m.apply(.recovered(attempt: 3)) == .playing(kitchen),
          "cast: a lost channel reconnects and plays again")
    check(m.anchorMs == 250 && CastModel.networkWaitMs(airPlay: 2000, cast: m.anchorMs) == 2000
            && CastModel.networkWaitMs(airPlay: nil, cast: 250) == 250
            && CastModel.networkWaitMs(airPlay: 100, cast: 250) == 250
            && CastModel.networkWaitMs(airPlay: nil, cast: nil) == nil,
          "cast: local outputs wait the longer of the AirPlay delay and the Cast offset")

    var takenOver = m
    check(takenOver.apply(.ended(attempt: 3, .takenOver)) == .turnedOff(kitchen, problem: .takenOver)
            && takenOver.enabled == nil && takenOver.attempt == nil && takenOver.anchorMs == nil,
          "cast: another app casting turns the speaker off with a notice")
    var stopped = m
    check(stopped.turnOff(kitchen) && stopped.plan(streaming: true) == .stop, "cast: turning it off stops its session")
    stopped.stopSent()
    check(stopped.apply(.ended(attempt: 3, .stopped)) == nil && stopped.plan(streaming: true) == .none,
          "cast: the end of a stopped session is stale and says nothing")
    let streamingOff = m
    check(streamingOff.plan(streaming: false) == .stop && streamingOff.enabled == kitchen,
          "cast: streaming off stops the session and keeps the speaker chosen")

    var refused = CastModel()
    refused.apply(.appeared(CastDevice(id: den, name: "Den"), offsetMs: nil))
    _ = refused.turnOn(den)
    refused.startSent(den)
    check(refused.started(den, attempt: 0, refusal: ASC_CAST_REFUSED_BUSY) == .turnedOff(den, problem: .busy)
            && refused.enabled == nil && refused.status(of: den) == .idle,
          "cast: attempt 0 means refused, and the speaker goes off")

    check(CastEnd.failed(.connect).problem == .notResponding && CastEnd.failed(.launch).problem == .refused
            && CastEnd.failed(.noFetch).problem == .blocked && CastEnd.failed(.stream).problem == .connectionLost
            && CastEnd.failed(nil).problem == .notResponding
            && CastEnd.stopped.problem == nil && CastEnd.deselected.problem == nil,
          "cast: each way a session ends maps to its notice, or none")

    var failed = m
    check(failed.apply(.failed(detail: "")) == .senderFailed(wasOn: kitchen) && failed.enabled == nil && failed.rows.isEmpty
            && failed.listState(now: Date()) == .failed,
          "cast: a sender that stopped working empties the list and says so")

    check(CastModel.clampedOffset(-5) == 0 && CastModel.clampedOffset(20_001) == 20_000
            && CastModel.clampedOffset(12.6) == 13 && CastModel.clampedOffset(.infinity) == nil,
          "cast: the offset field takes 0 to 20000 whole ms")
    var offsets = m
    check(offsets.setOffset(kitchen, ms: 250) == nil && offsets.setOffset(kitchen, ms: 30_000) == 20_000
            && offsets.offset(of: kitchen) == 20_000, "cast: an offset is stored clamped, and only when it changes")

    var seeded = CastModel()
    seeded.apply(.appeared(CastDevice(id: "hall", name: "Hall"), offsetMs: 500))
    check(seeded.offsets["hall"] == 500, "cast: a device's first announcement seeds the offset its memory holds")
    _ = seeded.setOffset("hall", ms: 300)
    seeded.apply(.changed(CastDevice(id: "hall", name: "Hall"), offsetMs: 500))
    check(seeded.offsets["hall"] == 300,
          "cast: a memory offset read before the user's change and delivered after it doesn't overwrite it",
          "\(seeded.offsets)")

    let start = Date()
    var listed = CastModel()
    check(listed.listState(now: start) == .quiet, "cast: no note before the search starts")
    listed.discovery = .browsing(since: start)
    check(listed.listState(now: start + 5) == .searching && listed.listState(now: start + 20) == .noneFound,
          "cast: searching, then none found after 20 s")
    listed.apply(.appeared(CastDevice(id: den, name: "Den"), offsetMs: nil))
    check(listed.listState(now: start + 30) == .quiet, "cast: no note once a speaker is listed")
    listed.apply(.gone(id: den))
    check(listed.rows.isEmpty && listed.listState(now: start + 30) == .noneFound, "cast: a speaker that goes leaves the list")

    var raw = AscCastEvent()
    raw.kind = ASC_CAST_EVENT_ENDED
    raw.attempt = 7
    raw.reason = ASC_CAST_END_FAILED
    raw.error = ASC_CAST_ERROR_NO_FETCH
    check(CastEvent(raw) == .ended(attempt: 7, .failed(.noFetch)), "cast: an ended event copies its reason and error")
    raw = AscCastEvent()
    raw.kind = ASC_CAST_EVENT_VOLUME
    raw.attempt = 7
    raw.level = 0.25
    raw.volume_fixed = true
    check(CastEvent(raw) == .volume(attempt: 7, CastVolume(level: 0.25, muted: false, fixed: true))
            && CastEvent(raw)?.attempt == 7, "cast: a volume event copies level, mute and fixed")
    raw.kind = ASC_CAST_EVENT_FORMAT_FALLBACK
    raw.format = ASC_CAST_FORMAT_FLAC
    check(CastEvent(raw) == .formatFallback(attempt: 7, format: ASC_CAST_FORMAT_FLAC),
          "cast: a format fallback event copies the format")
    raw.kind = 999
    check(CastEvent(raw) == nil, "cast: an unknown event kind is skipped")
    let change: CastDeviceChange? = kitchen.withCString { id in
        "Kitchen".withCString { name in
            "Nest Audio".withCString { model in
                CastDeviceChange(AscCastDevice(event: ASC_CAST_APPEARED, id: id, name: name, model: model,
                                               flags: UInt32(ASC_CAST_FLAG_GROUP)),
                                 offsetMs: { $0 == kitchen ? 40 : nil })
            }
        }
    }
    check(change == .appeared(CastDevice(id: kitchen, name: "Kitchen", model: "Nest Audio", group: true), offsetMs: 40),
          "cast: a discovery report copies its strings, flags and stored offset")

    let t = UIStrings()
    var notices = NoticeList()
    notices.post(t.castProblem(.busy, name: "Den"), subject: Notice.cast(den))
    notices.post(t.castProblem(.notResponding, name: "Kitchen"), subject: Notice.cast(kitchen))
    notices.post(t.castProblem(.connectionLost, name: "Kitchen"), subject: Notice.cast(kitchen))
    check(notices.count == 2 && notices.newest?.text.contains("Kitchen") == true
            && notices.items.last?.text.contains("Den") == true,
          "cast: one notice per speaker, the newest replacing the older one")
    var senderGone = notices
    senderGone.post("Source quit", subject: Notice.source)
    let castSubjects = Notice.castSubjects(in: senderGone)
    let cleared = senderGone.clear(castSubjects)
    check(Set(castSubjects) == [Notice.cast(den), Notice.cast(kitchen)]
            && cleared && senderGone.items.map(\.subject) == [Notice.source],
          "cast: a sender that stopped clears every speaker's notice, and only those")
    check(t.castSenderStopped(speaker: "Kitchen").contains("Kitchen"), "cast: the sender's notice names the speaker that went off")
    var idleFailure = CastModel()
    check(idleFailure.apply(.failed(detail: "")) == .senderFailed(wasOn: nil), "cast: no speaker named when none was on")
    check(t.castList(.noneFound)?.contains("Local Network") == true, "cast: none found points to Local Network access")
    let problems: [CastProblem] = [.busy, .notResponding, .refused, .blocked, .takenOver, .connectionLost]
    check(problems.allSatisfy { let text = t.castProblem($0, name: "Den"); return text.contains("Den") && !text.contains("\u{2014}") },
          "cast: every notice names the speaker, with no em dash")
    let bothNone = t.networkSpeakersNote(airPlayNone: true, cast: .noneFound) ?? ""
    check(bothNone.hasPrefix("No AirPlay or Google Cast speakers found yet.") && bothNone.contains("Local Network")
            && t.airPlayNoneFound().contains("Local Network")
            && t.networkSpeakersNote(airPlayNone: true, cast: .quiet) == t.airPlayNoneFound()
            && t.networkSpeakersNote(airPlayNone: false, cast: .searching) == t.castList(.searching)
            && t.networkSpeakersNote(airPlayNone: false, cast: .quiet) == nil
            && t.networkSpeakersNote(airPlayNone: true, cast: .failed)?.hasPrefix(t.airPlayNoneFound()) == true,
          "cast: Connected Speakers says where AirPlay and the Cast search stand")
    let hintedNotes: [String?] = [LocalNetworkHint.denied, .possible].flatMap { hint in
        [t.airPlayNoneFound(hint: hint), t.castList(.noneFound, hint: hint),
         t.networkSpeakersNote(airPlayNone: true, cast: .noneFound, hint: hint),
         t.networkSpeakersNote(airPlayNone: true, cast: .quiet, hint: hint),
         t.networkSpeakersNote(airPlayNone: false, cast: .noneFound, hint: hint)]
    }
    check(hintedNotes.allSatisfy { $0?.contains("found yet.") == true && $0?.contains("Local Network") == false }
            && t.airPlayNoneFound(hint: .denied) == "No AirPlay speakers found yet."
            && t.castList(.noneFound, hint: .possible) == "No Google Cast speakers found yet."
            && t.networkSpeakersNote(airPlayNone: true, cast: .noneFound, hint: .denied)
                == "No AirPlay or Google Cast speakers found yet.",
          "network notes leave the Local Network advice to the hint while it shows")
    check(LocalNetworkHint.denied.shown(airPlayProblem: true) == .none
            && LocalNetworkHint.possible.shown(airPlayProblem: true) == .none
            && LocalNetworkHint.denied.shown(airPlayProblem: false) == .denied
            && t.networkSpeakersNote(airPlayNone: false, cast: .noneFound,
                                     hint: LocalNetworkHint.denied.shown(airPlayProblem: true))?.contains("Local Network") == true
            && t.localNetworkHint.contains("same network"),
          "an AirPlay problem hides the Local Network hint, so the notes keep the advice")
    check(t.castNote(CastRow(status: .idle, muted: false, volumeFixed: false, group: nil)) == nil
            && t.castNote(CastRow(status: .connecting, muted: false, volumeFixed: false, group: nil)) == "Connecting…"
            && t.castNote(CastRow(status: .playing, muted: true, volumeFixed: false, group: nil)) == "Muted on the speaker"
            && t.castNote(CastRow(status: .playing, muted: false, volumeFixed: false, group: nil)) == nil,
          "cast: the note under a speaker's name")
}

// MARK: Google Cast groups: one speaker row, its members from the session (C89 to C102)
do {
    let kitchen = "0123456789abcdef0123456789abcdef"
    let den = "fedcba9876543210fedcba9876543210"
    let home = "group-home"
    var m = CastModel()
    m.apply(.appeared(CastDevice(id: kitchen, name: "Kitchen"), offsetMs: 120))
    m.apply(.appeared(CastDevice(id: den, name: "Den"), offsetMs: nil))
    m.apply(.appeared(CastDevice(id: home, name: "Home", group: true), offsetMs: 400))
    let name: (CastMember) -> String = { $0.id == kitchen ? "Kitchen speaker" : $0.name }
    check(m.rows == [den, home, kitchen] && m.groupMembers(of: home, name: name) == .notReported
            && m.groupMembers(of: kitchen, name: name) == nil && m.offset(of: home) == 400,
          "cast group: listed as one row with its own stored offset, members not reported while off")

    check(m.turnOn(home) == nil && m.plan(streaming: true) == .start(home), "cast group: turns on and starts like a device")
    check(m.turnOn(kitchen) == .busy && m.enabled == home,
          "cast group: a member stays its own row and waits while the group plays")
    m.startSent(home)
    _ = m.started(home, attempt: 5, refusal: ASC_CAST_REFUSED_NONE)
    check(m.groupMembers(of: home, name: name) == .notReported, "cast group: no member note while it connects")

    var silent = m
    check(silent.apply(.started(attempt: 5, format: ASC_CAST_FORMAT_WAV)) == .playing(home) && silent.groupMembers(of: home, name: name) == .unknown,
          "cast group: playing without a member list says the members are unknown")
    _ = silent.apply(.members(attempt: 5, []))
    check(silent.groupMembers(of: home, name: name) == .unknown, "cast group: an empty member list stays unknown")

    let list = [CastMember(id: kitchen, name: "Kitchen"), CastMember(id: "", name: "Garage")]
    check(m.apply(.members(attempt: 4, list)) == nil && m.members == nil, "cast group: members of an older attempt are dropped")
    check(m.apply(.members(attempt: 5, list)) == .changed && m.apply(.members(attempt: 5, list)) == nil,
          "cast group: a member list changes the row once")
    _ = m.apply(.started(attempt: 5, format: ASC_CAST_FORMAT_WAV))
    check(m.groupMembers(of: home, name: name) == .known(["Kitchen speaker", "Garage"]),
          "cast group: members by their row's name, an unlisted one by the group's name for it")
    check(m.apply(.groupLog("Cast group message PLAYBACK_SESSION_UPDATED")) == nil,
          "cast group: a group log line changes nothing shown")

    check(m.setVolume(home, position: 30) == 0.3 && approx(m.volume(of: home), 30, 1e-9)
            && approx(m.volume(of: kitchen), 100, 1e-9),
          "cast group: one volume for the group, none sent to a member")
    check(m.setOffset(home, ms: 650) == 650 && m.offset(of: home) == 650 && m.offset(of: kitchen) == 120
            && m.anchorMs == 650,
          "cast group: the offset is stored under the group's id, never a member's, and local outputs wait for it")

    var ended = m
    check(ended.apply(.ended(attempt: 5, .takenOver)) == .turnedOff(home, problem: .takenOver)
            && ended.members == nil && ended.groupMembers(of: home, name: name) == .notReported,
          "cast group: another sender casting to a member ends it as taken over and forgets the members")
    var stopped = m
    _ = stopped.turnOff(home)
    stopped.stopSent()
    check(stopped.members == nil && stopped.turnOn(kitchen) == nil, "cast group: once it is off, a member can play")

    let t = UIStrings()
    check(t.castNote(CastRow(status: .idle, muted: false, volumeFixed: false, group: .notReported)) == "Google Cast group"
            && t.castNote(CastRow(status: .playing, muted: false, volumeFixed: false, group: .unknown))
                == "Google Cast group, members unknown"
            && t.castNote(CastRow(status: .playing, muted: false, volumeFixed: false,
                                  group: .known(["Kitchen", "Den", "Office"])))
                == "Google Cast group: Kitchen, Den and Office"
            && t.listed(["Kitchen"]) == "Kitchen" && t.listed(["Kitchen", "Den"]) == "Kitchen and Den"
            && t.castNote(CastRow(status: .playing, muted: true, volumeFixed: false, group: .known(["Kitchen"])))
                == "Muted on the group"
            && t.castNote(CastRow(status: .connecting, muted: false, volumeFixed: false, group: .notReported)) == "Connecting…",
          "cast group: the note under its name")
    check(t.rowHint(.off, note: t.castNote(CastRow(status: .playing, muted: false, volumeFixed: false,
                                                    group: .known(["Kitchen"]))))
            .contains("Kitchen"),
          "cast group: VoiceOver hears the members in the row's hint")

    var raw = AscCastEvent()
    raw.kind = ASC_CAST_EVENT_MEMBERS
    raw.attempt = 5
    check(CastEvent(raw, members: { list }) == .members(attempt: 5, list) && CastEvent(raw)?.attempt == 5,
          "cast group: a members event copies the list the sender reads")
    raw = AscCastEvent()
    raw.kind = ASC_CAST_EVENT_GROUP_LOG
    let logged: CastEvent? = "group Home announced".withCString { line in
        raw.detail = line
        return CastEvent(raw)
    }
    check(logged == .groupLog("group Home announced") && logged?.attempt == nil,
          "cast group: a group log event carries its line for the debug log")
    check(CastRefusal.problem(ASC_CAST_REFUSED_GROUP) == .notResponding
            && CastEnd.failed(CastError(ASC_CAST_ERROR_GROUP)).problem == .notResponding,
          "cast group: the retired group refusal and error map to nothing group-specific")

    // Calibrate All reads the Cast row as AppDelegate+CalibrateAll does.
    check(m.playingDevice == home && ended.playingDevice == nil,
          "cast group: the playing group is the Cast speaker a calibration run measures")
    let device = m.playingDevice ?? ""
    let calibration = CalibrateAllFlow.Inputs(
        streaming: true, reference: "mac", localOutputs: [(id: "mac", name: "MacBook Speakers")],
        localOffsets: [:], holdBacks: [:],
        cast: (id: CastID.app(device), name: "Home", offsetMs: m.offsets[device], waitMs: 0, format: ASC_CAST_FORMAT_WAV),
        airPlay: [], airPlayDelayMs: 0, airPlayHoldBackMs: 0)
    let planned = (try? CalibrateAllFlow.plan(calibration).get()) ?? []
    let castRows = planned.filter { $0.kind == .cast }
    check(castRows.map(\.id) == [CastID.app(home)] && castRows.first?.offsetMs == 650,
          "cast group: Calibrate All lists the playing group as one Cast row with its own offset", "\(planned)")
    var measured = m
    check(CastID.device(castRows.first?.id ?? "").flatMap { measured.setOffset($0, ms: 720) } == 720
            && measured.offset(of: home) == 720 && measured.offset(of: kitchen) == 120 && measured.offset(of: den) == 0,
          "cast group: the measured offset is stored under the group's id, the members' stay as they were")
}

// MARK: Google Cast track info and volume steps (C53, C54, C69)
do {
    let tv = "00112233445566778899aabbccddeeff"
    var m = CastModel()
    m.apply(.appeared(CastDevice(id: tv, name: "Living room TV"), offsetMs: nil))
    check(m.stepVolume(tv, up: true) == nil, "cast step: an arrow key does nothing while no session runs")
    _ = m.turnOn(tv)
    m.startSent(tv)
    _ = m.started(tv, attempt: 9, refusal: ASC_CAST_REFUSED_NONE)
    _ = m.apply(.started(attempt: 9, format: ASC_CAST_FORMAT_WAV))
    _ = m.apply(.volume(attempt: 9, CastVolume(level: 0.4, muted: false, fixed: false)))
    check(m.stepVolume(tv, up: true) == .level(0.4 + CastModel.fallbackVolumeStep)
            && approx(m.volume(of: tv), 45, 1e-9),
          "cast step: a device that reports no step moves by the fallback step, shown at once")
    _ = m.apply(.volume(attempt: 9, CastVolume(level: 0.45, muted: false, fixed: false)))
    var downLevel: Double?
    if case .level(let level)? = m.stepVolume(tv, up: false) { downLevel = level }
    check(downLevel.map { approx($0, 0.4, 1e-9) } == true, "cast step: the fallback step goes down too")
    _ = m.apply(.volume(attempt: 9, CastVolume(level: 0.4, muted: false, fixed: false, step: 0.02)))
    check(m.stepVolume(tv, up: true) == .device(up: true) && approx(m.volume(of: tv), 42, 1e-9),
          "cast step: once the device reports its step, the key sends a step and the slider shows the level it should reach")
    check(m.stepVolume(tv, up: true) == .device(up: true) && approx(m.volume(of: tv), 44, 1e-9),
          "cast step: a second step before the device answers goes on from the level shown")
    _ = m.apply(.volume(attempt: 9, CastVolume(level: 0.43, muted: false, fixed: false, step: 0.02)))
    check(approx(m.volume(of: tv), 43, 1e-9), "cast step: the device's volume event replaces the level the step should reach")
    check(m.stepVolume(tv, up: false) == .device(up: false) && approx(m.volume(of: tv), 41, 1e-9),
          "cast step: down by the device's step")
    _ = m.apply(.volume(attempt: 9, CastVolume(level: 1, muted: false, fixed: true, step: 0.02)))
    check(m.stepVolume(tv, up: true) == nil, "cast step: a fixed volume takes no step")
    var top = CastModel()
    top.apply(.appeared(CastDevice(id: tv, name: "Living room TV"), offsetMs: nil))
    _ = top.turnOn(tv)
    top.startSent(tv)
    _ = top.started(tv, attempt: 1, refusal: ASC_CAST_REFUSED_NONE)
    _ = top.apply(.volume(attempt: 1, CastVolume(level: 0.98, muted: false, fixed: false)))
    check(top.stepVolume(tv, up: true) == .level(1), "cast step: the fallback step stops at full volume")

    var raw = AscCastEvent()
    raw.kind = ASC_CAST_EVENT_VOLUME
    raw.attempt = 9
    raw.level = 0.5
    raw.step = 0.05
    check(CastEvent(raw) == .volume(attempt: 9, CastVolume(level: 0.5, muted: false, fixed: false, step: 0.05)),
          "cast step: a volume event copies the device's step")

    // The Cast sender has a track info feed of its own: a device switched on
    // while AirPlay receivers already show the track still gets all of it.
    let song = TrackMetadata.Fields(title: "Song", artist: "Band", album: "Record")
    let next = TrackMetadata.Fields(title: "Next", artist: "Band", album: "Record")
    let cover = NSImage(size: NSSize(width: 1, height: 1)), icon = NSImage(size: NSSize(width: 1, height: 1))
    let now = Date(timeIntervalSinceReferenceDate: 0)
    func shows(_ fields: TrackMetadata.Fields) -> () -> (info: ReceiverInfo, artwork: NSImage?) {
        { (ReceiverInfo(fields: fields, artwork: .track), cover) }
    }
    var airPlay = TrackInfoFeed()
    _ = airPlay.modelUpdated(sending: true)
    _ = airPlay.send(sending: true, now: now, pollingSince: nil, hasReading: true, icon: icon, shows: shows(song))
    let sender = CastSender(memoryURL: nil, onChanges: { _ in }, onEvents: { _ in })
    check(!sender.trackInfo.modelUpdated(sending: false) && sender.trackInfo.sent == nil,
          "cast track info: nothing goes out while no Cast device is on")
    check(sender.trackInfo.modelUpdated(sending: true), "cast track info: switching a device on sends everything")
    let first = sender.trackInfo.send(sending: true, now: now, pollingSince: nil, hasReading: true, icon: icon,
                                      shows: shows(song))
    check(first.fields == song && first.picture === cover,
          "cast track info: the device gets the track and cover AirPlay already had")
    check(sender.trackInfo.send(sending: true, now: now + 1, pollingSince: nil, hasReading: true, icon: icon,
                                shows: shows(song)).fields == nil,
          "cast track info: the same track again gives the sender nothing")
    check(sender.trackInfo.send(sending: true, now: now + 2, pollingSince: nil, hasReading: true, icon: icon,
                                shows: shows(next)).fields == next,
          "cast track info: a track change goes to the sender, kept for the next load")
    sender.discard()

    // With no track known, Cast gets none of AudioSync's labels or its icon:
    // blank fields, which the sender leaves out (C53), and no cover.
    let blank = ReceiverInfo.decideForCast(track: nil, source: .system)
    check(blank == ReceiverInfo(fields: .init(title: "", artist: "", album: ""), artwork: .neutral),
          "cast track info: with no track known, the fields are blank, not AudioSync's labels")
    var castFeed = TrackInfoFeed()
    _ = castFeed.modelUpdated(sending: true)
    let untitled = castFeed.send(sending: true, now: now, pollingSince: nil, hasReading: true, icon: nil,
                                 shows: { (blank, nil) })
    check(untitled.fields == blank.fields && untitled.picture == nil,
          "cast track info: with no track known, the device gets blank fields and no cover")

    // A large cover fits C53: at most 1280 by 720, within the byte cap.
    let side = 2000
    let context = CGContext(data: nil, width: side, height: side, bitsPerComponent: 8, bytesPerRow: 0,
                            space: CGColorSpace(name: CGColorSpace.sRGB)!,
                            bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue)!
    for i in 0..<40 {
        context.setFillColor(CGColor(red: CGFloat(i % 7) / 7, green: CGFloat(i % 5) / 5, blue: CGFloat(i % 3) / 3,
                                     alpha: 1))
        context.fill(CGRect(x: i * 50, y: 0, width: 50, height: side - i * 37))
    }
    let jpeg = TrackMetadata.jpeg(context.makeImage()!, maxSide: CastSender.artworkSide,
                                  maxBytes: CastSender.artworkBytes)
    let decoded = jpeg.flatMap { CGImageSourceCreateWithData($0 as CFData, nil) }
        .flatMap { CGImageSourceCreateImageAtIndex($0, 0, nil) }
    check(decoded.map { $0.width <= 1280 && $0.height <= 720 } == true
            && (jpeg?.count ?? .max) <= CastSender.artworkBytes,
          "cast track info: a large cover is scaled to fit 1280 by 720 and the byte cap")
}

// MARK: AirPlay, Google Cast and local outputs at one latency: the hold-backs, the AirPlay cap and the notice (C77)
do {
    let t = UIStrings()
    // When each is heard after the capture: AirPlay at L plus its hold-back,
    // Cast at its offset plus the feed's, a local output at its hold-back
    // minus its offset (its own lateness, negated).
    func heard(l: Double, cast: Double) -> (airPlay: Double, cast: Double, locals: [Double]) {
        let wait = CastModel.networkWaitMs(airPlay: l, cast: cast) ?? 0
        let offsets = ["builtin": 0.0, "usb": 40.0, "bluetooth": 180.0]
        let holds = LocalDelay.holdBacks(takingPart: ["builtin", "usb", "bluetooth"], reference: "builtin",
                                         offsets: ["usb": 40, "bluetooth": 180], airPlayChosen: true, latencyMs: wait)
        return (l + NetworkHoldBack.airPlayAppliedMs(NetworkHoldBack.airPlayMs(airPlay: l, cast: cast)),
                cast + NetworkHoldBack.castMs(airPlay: l, cast: cast),
                offsets.keys.sorted().map { (holds[$0] ?? .nan) - offsets[$0]! })
    }
    // Cast later than L by 1.25 s, 3 s and 13 s, earlier by 0.75 s and by
    // all of L, and equal.
    let cases: [(l: Double, cast: Double)] = [(1750, 3000), (1750, 4750), (1750, 14_750), (1750, 1000), (2000, 0),
                                              (1750, 1750)]
    let spreads = cases.map { c -> Double in
        let h = heard(l: c.l, cast: c.cast)
        let all = [h.airPlay, h.cast] + h.locals
        return (all.max() ?? 0) - (all.min() ?? 0)
    }
    check(spreads.allSatisfy { $0 < 0.5 }, "cast+airplay: AirPlay, Cast and local outputs are heard together either way",
          "spreads \(spreads)")
    check(NetworkHoldBack.airPlayMs(airPlay: nil, cast: 3000) == 0 && NetworkHoldBack.castMs(airPlay: nil, cast: 0) == 0
            && NetworkHoldBack.airPlayMs(airPlay: 1750, cast: nil) == 0 && NetworkHoldBack.castMs(airPlay: 1750, cast: nil) == 0
            && NetworkHoldBack.airPlayMs(airPlay: 1750, cast: .nan) == 0 && NetworkHoldBack.castMs(airPlay: .nan, cast: 0) == 0,
          "cast+airplay: nothing waits unless both are chosen")
    check(NetworkHoldBack.airPlayLimitMs == 20_000
            && NetworkHoldBack.airPlayAppliedMs(13_000) == 13_000 && NetworkHoldBack.airPlayAheadMs(13_000) == 0
            && NetworkHoldBack.airPlayAppliedMs(23_000) == 20_000 && NetworkHoldBack.airPlayAheadMs(23_000) == 3000,
          "cast+airplay: the sender holds AirPlay back up to 20 000 ms and past that plays ahead by the rest")

    let kitchen = "kitchen-1"
    var ahead = AirPlayAheadNotice()
    check(ahead.update(device: kitchen, aheadMs: 0) == nil && ahead.update(device: kitchen, aheadMs: 49) == nil,
          "cast+airplay: no notice while AirPlay keeps up within the tolerance")
    check(ahead.update(device: kitchen, aheadMs: 1500)
            == AirPlayAheadNotice.Step(clear: nil, post: AirPlayAheadNotice.Shown(device: kitchen, tenths: 15)),
          "cast+airplay: the notice is posted with how far ahead AirPlay plays")
    check(ahead.update(device: kitchen, aheadMs: 1520) == nil, "cast+airplay: posted once while the amount stays the same")
    var notices = NoticeList()
    notices.post(t.airPlayAheadOfCast(tenths: 15, speaker: "Kitchen"), subject: Notice.airPlayAhead(kitchen))
    notices.clear([Notice.airPlayAhead(kitchen)])
    check(ahead.update(device: kitchen, aheadMs: 1480) == nil && notices.isEmpty,
          "cast+airplay: a dismissed notice stays away while the amount is the same")
    check(ahead.update(device: kitchen, aheadMs: 1600)
            == AirPlayAheadNotice.Step(clear: kitchen, post: AirPlayAheadNotice.Shown(device: kitchen, tenths: 16)),
          "cast+airplay: a dismissed notice comes back when the amount changes")
    check(ahead.update(device: kitchen, aheadMs: 20) == AirPlayAheadNotice.Step(clear: kitchen, post: nil)
            && ahead.shown == nil,
          "cast+airplay: the notice clears once AirPlay keeps up")
    check(ahead.update(device: kitchen, aheadMs: 60)?.post?.tenths == 1
            && ahead.update(device: nil, aheadMs: 60) == AirPlayAheadNotice.Step(clear: kitchen, post: nil),
          "cast+airplay: a small lead reads as 0.1 s, and the notice clears when the Cast speaker goes off")
    let text = t.airPlayAheadOfCast(tenths: 15, speaker: "Kitchen")
    check(text == "AirPlay speakers can wait at most 20 s for a Google Cast speaker, so they play about 1.5 s ahead of "
            + "Kitchen. Lower its offset to keep them together." && !text.contains("\u{2014}")
            && t.airPlayAheadOfCast(tenths: 20, speaker: "Den").contains("about 2.0 s ahead of Den"),
          "cast+airplay: the notice says how far ahead, with no em dash")
    var castOnes = NoticeList()
    castOnes.post(t.castProblem(.busy, name: "Kitchen"), subject: Notice.cast(kitchen))
    castOnes.post(text, subject: Notice.airPlayAhead(kitchen))
    check(castOnes.count == 2 && Notice.castSubjects(in: castOnes) == [Notice.cast(kitchen)],
          "cast+airplay: the notice never replaces a problem with the Cast speaker")
}

// MARK: Google Cast device memory: saved atomically, read back by a new sender
do {
    let dir = FileManager.default.temporaryDirectory.appendingPathComponent("audiosync-cast-\(UUID().uuidString)")
    defer { try? FileManager.default.removeItem(at: dir) }
    let url = CastMemoryFile.file(in: dir)
    check(url.lastPathComponent == "cast-devices.json" && url.deletingLastPathComponent().lastPathComponent == "AudioSync"
            && CastMemoryFile.load(from: url) == nil, "cast memory: in AudioSync's folder, nothing before a save")
    let id = "0123456789abcdef0123456789abcdef"
    var first = asc_cast_create(nil)
    check(first != nil && asc_cast_offset_ms(first, id) == -1 && asc_cast_set_offset(first, id, 1234.4)
            && !asc_cast_set_offset(first, id, -1), "cast memory: an offset is stored, a negative one refused")
    let json = asc_cast_memory_json(first).map { String(cString: $0) }
    asc_cast_destroy(&first)
    var saved = false
    if let json { saved = (try? CastMemoryFile.save(json, to: url)) != nil }
    let loaded = CastMemoryFile.load(from: url)
    check(saved && loaded == json && first == nil, "cast memory: the JSON is written and read back whole")
    var second = asc_cast_create(loaded)
    check(asc_cast_offset_ms(second, id) == 1234, "cast memory: a new sender starts with the stored offset")
    asc_cast_destroy(&second)
}

// MARK: Guide illustrations: each finds its place in the text, in order (GuideLayout)
do {
    let t = UIStrings()
    let pieces = GuideLayout.pieces(of: t.helpText, anchor: t.guideArtAnchor)
    check(pieces.compactMap(\.art) == GuideArt.allCases && pieces.last?.art == nil
            && pieces.map(\.text).joined() == t.helpText,
          "guide: every illustration has its anchor line, in order, and the text is kept whole",
          "\(pieces.map(\.art))")
    let starts = pieces.dropFirst().map { piece in piece.text.drop { $0 == " " } }
    check(zip(starts, GuideArt.allCases).allSatisfy { $0.hasPrefix(t.guideArtAnchor($1)) },
          "guide: each illustration sits just before its anchor line")

    let made = GuideLayout.pieces(of: "a\n  - B x\nc", anchor: { $0 == .delay ? "- B" : "missing" })
    check(made == [.init(text: "a\n", art: .delay), .init(text: "  - B x\nc", art: nil)],
          "guide: a missing anchor leaves its illustration out; leading spaces are ignored", "\(made)")
    check(GuideArt.allCases.allSatisfy { !$0.lightSVG.contains("<text") && !$0.darkSVG.contains("<text")
                                         && $0.lightSVG != $0.darkSVG },
          "guide: the illustrations carry no text and differ between light and dark")
}

// MARK: Web remote: the snapshot the page gets, and what a phone's change does
do {
    let t = UIStrings()
    typealias Facts = RemoteRow.Facts
    let reference = RemoteRow(Facts(id: "builtin", name: "MacBook Speakers", kind: "Built-in", on: true, volume: 120),
                              text: t)
    let usb = RemoteRow(Facts(id: "usb", name: "Desk", kind: "USB", on: false, volume: 40, offsetMs: 150), text: t)
    let homePod = RemoteRow(Facts(id: "hp", name: "Kitchen", kind: "AirPlay 2", on: true, volume: 30, isAirPlay: true,
                                  airPlayDelayMs: 1750), text: t)
    let castIdle = RemoteRow(Facts(id: "cast", name: "Nest", kind: "Google Cast", on: false, volume: 50,
                                   cast: (live: false, volumeFixed: false), offsetMs: 300), text: t)
    let castGroup = RemoteRow(Facts(id: "group", name: "Home group", kind: "Google Cast", on: false, volume: 50,
                                    cast: (live: false, volumeFixed: false), offsetMs: 0,
                                    note: t.castNote(CastRow(status: .idle, muted: false, volumeFixed: false,
                                                             group: .notReported))),
                              text: t)
    let locked = RemoteRow(Facts(id: "tv", name: "TV", kind: "AirPlay", on: false, volume: 50, isAirPlay: true,
                                 needsCode: true), text: t)
    let pairedTV = RemoteRow(Facts(id: "tv2", name: "TV 2", kind: "AirPlay", on: false, volume: 50, isAirPlay: true,
                                   needsCode: true, paired: true), text: t)
    let rows = [reference, usb, homePod, castIdle, castGroup, locked, pairedTV]

    let snapshot = RemoteSnapshot(computerName: "Studio Mac", master: 80, nowPlaying: nil, rows: rows)
    let object = (try? JSONSerialization.jsonObject(with: Data(snapshot.json.utf8))) as? [String: Any]
    let speakers = object?["speakers"] as? [[String: Any]] ?? []
    let byID = Dictionary(speakers.map { ($0["id"] as? String ?? "", $0) }, uniquingKeysWith: { a, _ in a })
    func field(_ id: String, _ key: String) -> Any? { byID[id]?[key] }
    func range(_ id: String) -> [Int] {
        [field(id, "delay_min_ms"), field(id, "delay_ms"), field(id, "delay_max_ms")].compactMap { $0 as? Int }
    }
    let master = (object?["master"] as? [String: Any])?["volume"] as? Double
    let ids: [String] = speakers.compactMap { $0["id"] as? String }
    let named = object?["computer_name"] as? String == "Studio Mac"
    let noTrack = object?["now_playing"] is NSNull
    check(named && noTrack && master == 80 && ids == rows.map(\.id),
          "web remote: the snapshot has the name, the master volume, no track and the rows in order")
    let clamped = field("builtin", "volume") as? Double == 100
    check(clamped && range("builtin") == [0, 0, 0],
          "web remote: the reference has no delay to change and its volume stays in 0 to 100", "\(range("builtin"))")
    let usbRange = range("usb") == [-2000, 150, 20000]
    let airPlayRange = range("hp") == [0, 1750, 20000]
    let castRange = range("cast") == [0, 300, 20000]
    check(usbRange && airPlayRange && castRange, "web remote: each delay carries the range its field in the window takes",
          "\(range("usb")) \(range("hp")) \(range("cast"))")
    let groupOpen = field("group", "available") as? Bool == true
    let groupNote = field("group", "notice") as? String == "Google Cast group"
    let codeOff = field("tv", "available") as? Bool == false
    let codeNote = field("tv", "notice") as? String == t.remoteTurnOnInApp
    let pairedOpen = field("tv2", "available") as? Bool == true
    let pairedQuiet = field("tv2", "notice") is NSNull
    check(groupOpen && groupNote && codeOff && codeNote && pairedOpen && pairedQuiet,
          "web remote: a Cast group plays from the phone; a speaker that would ask for a code on the Mac can't")
    check(snapshot.json == RemoteSnapshot(computerName: "Studio Mac", master: 80, nowPlaying: nil, rows: rows).json,
          "web remote: the same state gives the same text, so the core sends nothing new")
    let playing = RemoteSnapshot(computerName: "", master: 50,
                                 nowPlaying: .init(title: "Song", artist: "Band", source: "Music"), rows: []).json
    check(playing.contains("\"now_playing\":{\"artist\":\"Band\",\"source\":\"Music\",\"title\":\"Song\"}"),
          "web remote: Now Playing goes out with title, artist and source app", playing)

    // The core's own parser takes the snapshot.
    let noCommand: AscRemoteCommand = { _, _ in }
    var error: Int32 = 0
    var remote = asc_remote_start("{\"port\": 0, \"listen\": [\"127.0.0.1\"]}", noCommand, nil, &error)
    check(remote != nil && asc_remote_publish(remote, snapshot.json) && asc_remote_publish(remote, playing),
          "web remote: the core accepts the snapshots", "start error \(error)")
    asc_remote_stop(&remote)

    let fullCommand = RemoteCommand(json: "{\"type\":\"speaker\",\"id\":\"usb\",\"on\":true,\"volume\":55.5,\"delay_ms\":-40}")
    let numberAsSwitch = RemoteCommand(json: "{\"type\":\"speaker\",\"id\":\"usb\",\"on\":1}")
    let masterCommand = RemoteCommand(json: "{\"type\":\"master\",\"volume\":20}")
    let dropped: [RemoteCommand?] = ["{\"type\":\"master\"}", "{\"type\":\"eq\"}", "not json"].map { RemoteCommand(json: $0) }
    let readFull = fullCommand == .speaker(id: "usb", on: true, volume: 55.5, delayMs: -40)
    let readNumber = numberAsSwitch == .speaker(id: "usb", on: nil, volume: nil, delayMs: nil)
    let readMaster = masterCommand == .master(volume: 20)
    check(readFull && readNumber && readMaster && dropped.allSatisfy { $0 == nil },
          "web remote: commands read as the core sends them; anything else is dropped")

    func plan(_ json: String) -> [RemoteAction] { RemoteCommand(json: json).map { RemoteAction.plan($0, rows: rows) } ?? [] }
    check(plan("{\"type\":\"speaker\",\"id\":\"usb\",\"on\":true,\"volume\":70}")
            == [.setEnabled(id: "usb", on: true), .setVolume(id: "usb", position: 70)],
          "web remote: turning a speaker on with a volume flips the switch, then moves the slider")
    check(plan("{\"type\":\"speaker\",\"id\":\"usb\",\"volume\":70}").isEmpty,
          "web remote: the volume of a speaker that is off changes nothing, as its slider is disabled in the window")
    check(plan("{\"type\":\"speaker\",\"id\":\"cast\",\"on\":true,\"volume\":70}") == [.setEnabled(id: "cast", on: true)],
          "web remote: a Cast speaker's volume waits for its session")
    let toLocal = plan("{\"type\":\"speaker\",\"id\":\"usb\",\"delay_ms\":-5000}") == [.setLocalOffset(id: "usb", ms: -2000)]
    let toCast = plan("{\"type\":\"speaker\",\"id\":\"cast\",\"delay_ms\":450}") == [.setCastOffset(id: "cast", ms: 450)]
    let toAirPlay = plan("{\"type\":\"speaker\",\"id\":\"hp\",\"delay_ms\":1800}") == [.setAirPlayDelay(ms: 1800)]
    let noDelay = plan("{\"type\":\"speaker\",\"id\":\"builtin\",\"delay_ms\":100}").isEmpty
    let unchanged = plan("{\"type\":\"speaker\",\"id\":\"usb\",\"delay_ms\":150}").isEmpty
    check(toLocal && toCast && toAirPlay && noDelay && unchanged,
          "web remote: each delay goes to its own control, clamped like the window's field; the reference has none")
    let leftAlone = ["tv", "gone"].allSatisfy { plan("{\"type\":\"speaker\",\"id\":\"\($0)\",\"on\":true}").isEmpty }
    let pairedOn = plan("{\"type\":\"speaker\",\"id\":\"tv2\",\"on\":true}") == [.setEnabled(id: "tv2", on: true)]
    let groupOn = plan("{\"type\":\"speaker\",\"id\":\"group\",\"on\":true}") == [.setEnabled(id: "group", on: true)]
    check(leftAlone && pairedOn && groupOn,
          "web remote: a speaker that can't be driven or isn't listed is left alone; a paired one and a Cast group turn on")
    let offFirst = plan("{\"type\":\"speaker\",\"id\":\"hp\",\"on\":false,\"volume\":10}") == [.setEnabled(id: "hp", on: false)]
    let masterClamped = plan("{\"type\":\"master\",\"volume\":150}") == [.setMaster(position: 100)]
    check(offFirst && masterClamped, "web remote: a speaker turned off takes no volume; the master volume is clamped")

    check(RemotePort.parse(" 8080 ") == 8080 && RemotePort.parse("80") == nil && RemotePort.parse("70000") == nil
            && RemotePort.parse("abc") == nil && RemotePort.stored(1e30) == nil && RemotePort.stored(7655.5) == nil,
          "web remote: a port from 1024 to 65535 is taken")
    let fresh = AppSettings.load { _ in nil }
    let stored: [String: Any] = ["webRemoteEnabled": true, "webRemotePort": 8080]
    let kept = AppSettings.load { stored[$0] }
    let damaged: [String: Any] = ["webRemotePort": 1e30]
    let dropsDamaged = AppSettings.load { damaged[$0] }.webRemotePort == RemotePort.default
    check(!fresh.webRemote && fresh.webRemotePort == RemotePort.default && kept.webRemote && kept.webRemotePort == 8080
            && dropsDamaged,
          "web remote: off on port 7655 by default; the stored setting and port are read back, a damaged port is dropped")
    check(RemoteAddresses(localHostName: "kitchen", ipv4: "192.0.2.20").urls(port: 7655)
            == ["http://kitchen.local:7655", "http://192.0.2.20:7655"]
            && RemoteAddresses(localHostName: nil, ipv4: "198.51.100.2").urls(port: 8000) == ["http://198.51.100.2:8000"],
          "web remote: the .local address comes first, the IP address second")
    check(t.webRemoteCodeLabel("042917") == "Pairing code 0 4 2 9 1 7",
          "web remote: VoiceOver reads the code digit by digit")

    let hashA = String(repeating: "a1", count: 32), hashB = String(repeating: "0f", count: 32)
    let devicesJSON = { (body: String) in Data(body.utf8) }
    let twoDevices = RemoteDevicesFile.count(devicesJSON("{\"version\":1,\"devices\":[\"\(hashA)\",\"\(hashB)\"]}"))
    let tenDevices = RemoteDevicesFile.count(devicesJSON("{\"version\":1,\"devices\":[\(Array(repeating: "\"\(hashA)\"", count: 10).joined(separator: ","))]}"))
    let unreadable = [
        RemoteDevicesFile.count(nil),
        RemoteDevicesFile.count(devicesJSON("not json")),
        RemoteDevicesFile.count(devicesJSON("{\"version\":2,\"devices\":[\"\(hashA)\"]}")),
        RemoteDevicesFile.count(devicesJSON("{\"version\":true,\"devices\":[\"\(hashA)\"]}")),
        RemoteDevicesFile.count(devicesJSON("{\"version\":1,\"devices\":[\"\(hashA)\",\"short\"]}")),
    ]
    check(twoDevices == 2 && tenDevices == RemoteDevicesFile.maxDevices && unreadable.allSatisfy { $0 == 0 },
          "web remote: the paired count comes from the devices file, capped at 8; a file the core can't read counts as none")

    let devicesFolder = FileManager.default.temporaryDirectory.appendingPathComponent("audiosync-remote-\(UUID().uuidString)")
    try? FileManager.default.createDirectory(at: devicesFolder, withIntermediateDirectories: true)
    let devicesURL = devicesFolder.appendingPathComponent("RemoteDevices.json")
    try? devicesJSON("{\"version\":1,\"devices\":[\"\(hashA)\"]}").write(to: devicesURL)
    let countedBefore = RemoteDevicesFile.count(at: devicesURL)
    let forgot = RemoteDevicesFile.forget(at: devicesURL)
    let countedAfter = RemoteDevicesFile.count(at: devicesURL)
    let forgotMissing = RemoteDevicesFile.forget(at: devicesURL)
    try? FileManager.default.removeItem(at: devicesFolder)
    check(countedBefore == 1 && forgot && countedAfter == 0 && forgotMissing,
          "web remote: Forget All clears the devices file without a running remote; a missing file is already forgotten")
}

print(failedChecks == 0 ? "\nALL LOGIC TESTS PASSED" : "\n\(failedChecks) FAILURE(S)")
exit(failedChecks > 0 ? 1 : 0)
