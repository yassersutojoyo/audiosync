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

// covers: S290, S291 (a level set on the speaker is kept for its next session)
do {
    // A TV remote's level D reaches the Mac as the core's own volume D / k, which row_move gives for a whole D.
    for (level, db) in [(40.0, -18.0), (80.0, -6.0)] {
        let own = AirPlayDeviceLevel.move(row: level, zone: 100, master: 63)?.own ?? .nan
        let rejoin = AirPlayDeviceLevel.sent(own: own, zone: 100, master: 63)
        let row = AirPlayDeviceLevel.row(own: own, zone: 100, master: 63, held: nil)
        check(approx(rejoin, level, 1e-9) && approx(-30 + 0.3 * rejoin, db, 1e-9) && row == level,
              "airplay level: added again or after Calibrate All Speakers, it gets its remote's \(Int(level)) at master 63",
              "sent \(rejoin), row \(row), own \(own)")
    }
}

// covers: S290 (k at 0: a level set on the speaker is held for the row)
do {
    let atZero = AirPlayDeviceLevel.held(level: 40, zone: 100, master: 0)
    let zoneZero = AirPlayDeviceLevel.held(level: 40, zone: 0, master: 63)
    let row = AirPlayDeviceLevel.row(own: 50, zone: 100, master: 0, held: atZero)
    check(atZero == 40 && zoneZero == 40 && row == 40,
          "airplay level: at master 0 a TV set to 40 holds the row at 40",
          "\(String(describing: atZero)) \(String(describing: zoneZero)) row \(row)")
    let moved = AirPlayDeviceLevel.row(own: 50, zone: 100, master: 50, held: nil)
    check(AirPlayDeviceLevel.held(level: 40, zone: 100, master: 63) == nil && moved == 25
            && AirPlayDeviceLevel.sent(own: 50, zone: 100, master: 0) == 0,
          "airplay level: with k above 0 nothing is held, and a held level is never sent", "row \(moved)")

    // covers: S276 (a DACP DeviceVolume of −18 dB at master 0: the core reports own 50, level 40)
    let dacpHeld = AirPlayDeviceLevel.held(level: 40, zone: 100, master: 0)
    let dacpRow = AirPlayDeviceLevel.row(own: 50, zone: 100, master: 0, held: dacpHeld)
    check(dacpHeld == 40 && dacpRow == 40 && AirPlayDeviceLevel.sent(own: 50, zone: 100, master: 0) == 0,
          "airplay level: a DeviceVolume at master 0 keeps own 50, the row shows the TV's 40 and 0 is what a send gives",
          "\(String(describing: dacpHeld)) row \(dacpRow)")
}

// covers: S290 (core/tests/vectors/s290_volume.json, the vectors the core and Windows run, through the calls the Mac makes)
do {
    let file = URL(fileURLWithPath: "core/tests/vectors/s290_volume.json")
    let root = (try? Data(contentsOf: file)).flatMap { try? JSONSerialization.jsonObject(with: $0) as? [String: Any] }
    let vectors = root?["cases"] as? [[String: Any]] ?? []
    check(!vectors.isEmpty, "airplay level vectors: the shared file is read", file.path)
    let known: Set<String> = ["own", "stored_own", "row_move", "step", "tv_level", "device_volume", "master",
                              "sent", "level", "own_after", "row"]
    for vector in vectors {
        let name = vector["name"] as? String ?? "?"
        let zone = vector["zone"] as? Double ?? .nan
        var master = vector["master"] as? Double ?? .nan
        var own = 0.0
        var held: Double?
        let steps = vector["steps"] as? [[String: Any]] ?? []
        check(zone.isFinite && master.isFinite && !steps.isEmpty,
              "airplay level vectors: \(name) has a zone, a master and steps", "\(zone) \(master) \(steps.count)")
        for (index, step) in steps.enumerated() {
            let label = "airplay level vectors: \(name), step \(index + 1)"
            let unknown = Set(step.keys).subtracting(known)
            check(unknown.isEmpty, "\(label): every key is one the Mac runs", "\(unknown.sorted())")
            let ops = Set(step.keys).intersection(["own", "stored_own", "row_move", "step", "tv_level", "device_volume",
                                                   "master"])
            check(ops.count == 1, "\(label): one op, as the core runner applies only one", "\(ops.sorted())")
            var sent = true
            if let value = step["own"] as? Double {
                // A preset's own volume (setNativeAirPlayOwnVolume).
                own = value
                held = nil
            }
            if let value = step["stored_own"] as? Double {
                // Loaded from the settings (AppSettings.airPlayOwn) or a preset (asc_preset_levels); both must agree.
                var preset = [value]
                preset.withUnsafeMutableBufferPointer { asc_preset_levels($0.baseAddress, $0.count, nil, 0, nil, nil) }
                own = AppSettings.airPlayOwn(value)
                check(preset[0] == own, "\(label): a preset keeps the own volume the settings load", "\(preset[0]) \(own)")
                held = nil
            }
            if let row = step["row_move"] as? Double {
                // setNativeAirPlayVolume.
                if let moved = AirPlayDeviceLevel.move(row: row, zone: zone, master: master) {
                    own = moved.own
                    held = nil
                    check(approx(moved.level, AirPlayDeviceLevel.sent(own: own, zone: zone, master: master), 1e-6),
                          "\(label): the level a row move sends is the level the own volume gives", "\(moved)")
                } else {
                    sent = false
                }
            }
            if let by = step["step"] as? Double {
                // The core turns a remote step into a row move from the row shown (policy/remote.rs); the Mac stores its own volume.
                let shown = AirPlayDeviceLevel.row(own: own, zone: zone, master: master, held: held)
                let target = min(100, max(0, shown + by))
                if target != shown, let moved = AirPlayDeviceLevel.move(row: target, zone: zone, master: master) {
                    own = moved.own
                    held = nil
                } else {
                    sent = false
                }
            }
            for key in ["tv_level", "device_volume"] {
                guard let level = step[key] as? Double else { continue }
                // A change on the speaker reaches the Mac as the core's own volume D / k, which row_move gives for a whole D.
                check(level == level.rounded(), "\(label): the level set on the speaker is whole", "\(level)")
                if let moved = AirPlayDeviceLevel.move(row: level, zone: zone, master: master) { own = moved.own }
                held = AirPlayDeviceLevel.held(level: level, zone: zone, master: master)
            }
            if let value = step["master"] as? Double {
                if value != master { held = nil }
                master = value
            }
            if let expected = step["sent"] as? Bool {
                check(sent == expected, "\(label): a level is sent or not", "\(sent)")
            }
            if let level = step["level"] as? Double {
                let got = AirPlayDeviceLevel.level(own: own, zone: zone, master: master, held: held)
                check(approx(got, level, 1e-6), "\(label): the level the speaker has", "\(got), own \(own)")
            }
            if let expected = step["own_after"] as? Double {
                check(approx(own, expected, 1e-6), "\(label): the own volume stored", "\(own)")
            }
            if let row = step["row"] as? Double {
                let got = AirPlayDeviceLevel.row(own: own, zone: zone, master: master, held: held)
                check(got == row, "\(label): the row", "\(got)")
            }
        }
    }
    check(AppSettings.airPlayOwn(126.98) == 126.98 && AppSettings.airPlayOwn(-1) == 0,
          "airplay level: a stored own volume above 100 loads as it is")

    // A DeviceVolume 100 at master 63 stores s = 158.7; after a relaunch the row and the TV still say 100.
    final class Stored { var values: [String: Any] = [:] }
    let stored = Stored()
    let store = SettingsStore(read: { stored.values[$0] }, write: { value, key in stored.values[key] = value })
    let own = AirPlayDeviceLevel.move(row: 100, zone: 100, master: 63)?.own ?? .nan
    store.save(levels: ["A1": own, "B2": -4], forKey: AppSettings.Keys.airplayVolumes)
    let loaded = store.load().airplayVolumes
    let row = AirPlayDeviceLevel.row(own: loaded["A1"] ?? .nan, zone: 100, master: 63, held: nil)
    check(own > 158 && loaded["A1"] == own && loaded["B2"] == 0 && row == 100,
          "airplay level: an own volume above 100 survives a relaunch, so the row stays at the TV's 100",
          "\(loaded), row \(row)")
}

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

// MARK: Several apps as one source
// covers: S10, S242 (the Mac's mix and all-except sources, AudioSync never in them)
do {
    let music = "com.apple.Music", safari = "com.apple.Safari", own = "com.example.audiosync"
    let both = AudioSource.mixed(.only, bundleIDs: [music, safari])
    let except = AudioSource.mixed(.allExcept, bundleIDs: [music])
    check(both == .mix(.only, bundleIDs: [music, safari]) && both.defaultsValue == "apps:com.apple.Music,com.apple.Safari"
            && AudioSource(defaultsValue: both.defaultsValue) == both,
          "mix: several apps are stored under an apps: tag and read back", both.defaultsValue)
    check(except.defaultsValue == "except:com.apple.Music" && AudioSource(defaultsValue: except.defaultsValue) == except,
          "mix: all except is stored under an except: tag and read back", except.defaultsValue)
    check(AudioSource(defaultsValue: music) == .app(bundleID: music) && AudioSource.app(bundleID: music).defaultsValue == music,
          "mix: an old single-app value still loads as that app, and saves unchanged")
    check(AudioSource.mixed(.only, bundleIDs: [music, own, music, " ", "a,b", "x:y"], ownBundleID: own) == .app(bundleID: music),
          "mix: one app left after dropping AudioSync, repeats, blanks and unstorable IDs is a plain app source")
    check(AudioSource.mixed(.only, bundleIDs: []) == .system && AudioSource.mixed(.allExcept, bundleIDs: [own], ownBundleID: own) == .system
            && AudioSource(defaultsValue: "apps:") == .system && AudioSource(defaultsValue: "except:,") == .system,
          "mix: no app left is the whole system, so AudioSync never names itself in a stored mix")
    check(both.bundleID == nil && both.capturedApps == [music, safari] && except.capturedApps.isEmpty,
          "mix: a mix has no single bundle ID; only an only-mix captures its apps")

    check(SourceMix.toggle(safari, in: .app(bundleID: music), mode: .only) == both
            && SourceMix.toggle(safari, in: both, mode: .only) == .app(bundleID: music)
            && SourceMix.toggle(music, in: .app(bundleID: music), mode: .only) == .system,
          "mix: checking an app adds it to the mix, unchecking takes it out")
    check(SourceMix.toggle(music, in: .system, mode: .allExcept) == except
            && SourceMix.toggle(music, in: .app(bundleID: safari), mode: .allExcept) == except
            && SourceMix.toggle(own, in: .system, mode: .allExcept, ownBundleID: own) == .system,
          "mix: in all-except mode a check starts a new exclusion list; AudioSync can't be checked")
    check(SourceMix.setMode(.allExcept, from: .only, of: both) == .mix(.allExcept, bundleIDs: [music, safari])
            && SourceMix.setMode(.only, from: .allExcept, of: except) == .app(bundleID: music),
          "mix: switching the mode keeps the checked apps")
    check(SourceMix.mode(of: .system, remembered: .allExcept) == .allExcept && SourceMix.mode(of: .app(bundleID: music), remembered: .allExcept) == .only
            && SourceMix.mode(of: except, remembered: .only) == .allExcept && SourceMix.checked(.app(bundleID: music), mode: .allExcept).isEmpty,
          "mix: the menu shows the mix's own mode, else the one last shown")

    func judged(_ source: AudioSource, _ quit: String?, running: Set<String>) -> AudioSource {
        SourceQuit.judged(source, quitBundleID: quit, isRunning: { running.contains($0) })
    }
    check(judged(both, music, running: [safari]) == .system && judged(both, music, running: []) == .app(bundleID: music)
            && judged(both, "com.other", running: []) == .system && judged(except, music, running: []) == .system,
          "mix: a mix ends only with its last app; all except outlives every app")
    check(SourceQuit.decide(setting: .stop, source: both, quitBundleID: music, streaming: .running, isRunning: { $0 == safari }) == .keep
            && SourceQuit.decide(setting: .stop, source: both, quitBundleID: music, streaming: .running, isRunning: { _ in false }) == .stop
            && SourceQuit.decide(setting: .system, source: both, quitBundleID: safari, streaming: .running, isRunning: { _ in false })
                == .switchToSystem,
          "mix: an app of a mix quitting keeps streaming while another runs, and acts as set once none does")

    check(AppTapFollow.decideExcluding(tapped: [3], found: [], own: 3) == .keep
            && AppTapFollow.decideExcluding(tapped: [3], found: [9, 5], own: 3) == .retap([3, 5, 9])
            && AppTapFollow.decideExcluding(tapped: [3, 5, 9], found: [9], own: 3) == .retap([3, 9]),
          "mix: an all-except tap follows the excluded apps' processes and always leaves AudioSync out")
    check(AppTapFollow.quitApp(of: [music, safari], ranBefore: [safari]) == safari
            && AppTapFollow.quitApp(of: [music, safari], ranBefore: [music, safari]) == music
            && AppTapFollow.quitApp(of: [music, safari], ranBefore: ["com.other"]) == music
            && AppTapFollow.quitApp(of: [], ranBefore: [safari]).isEmpty,
          "mix: a mix's lost capture names an app that still ran at the last look, not simply the mix's first app")

    let entries = SourceList.entries(systemName: "All System Audio", apps: [(bundleID: music, name: "Music")], inputs: [],
                                     chosen: both, chosenAppName: { _ in "Safari" }, mixName: { $1.joined(separator: " + ") })
    check(entries.map(\.id) == ["", both.defaultsValue, music] && entries[1].name == "Music + Safari",
          "mix: the picker lists the chosen mix under the whole system, named by its apps", "\(entries)")
    let offered = SourceList.mixApps([(bundleID: music, name: "Music")], checked: [safari], chosenAppName: { _ in "Safari" })
    check(offered.map(\.id) == [music, safari], "mix: a checked app that isn't running stays in the menu to be unchecked")
}

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

    // Hundreds of background processes are listed; reading each one's name
    // and icon held the main thread about 25 ms on every panel open.
    final class Reads { var pids: [pid_t] = [] }
    let reads = Reads()
    let listed: [(pid: pid_t, regular: Bool)] = [(1, false), (2, true), (3, false), (4, true)]
    let read = SourceApps.regularApps(listed, isRegular: { $0.regular }) { entry in
        reads.pids.append(entry.pid)
        return app(entry.pid, "com.example.\(entry.pid)", "App \(entry.pid)")
    }
    check(read.map(\.pid) == [2, 4] && reads.pids == [2, 4],
          "source apps: only regular apps have their name and icon read, in the listed order", "\(reads.pids)")
}

// MARK: Audio processes — which Core Audio process objects make up an app
// covers: S10, S11 (an app source's process objects, AudioSync's own never among them)
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
// covers: S155
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
check(SpeakerKey.parseAny("CC:tv") == nil && SpeakerKey.parseAny("cast:tv") == nil && SpeakerKey.parseAny("tv") == nil
      && SpeakerKey.make(id: "manual:203.0.113.20:8009", kind: .cast) == "cc:manual:203.0.113.20:8009"
      && SpeakerKey.parseAny("cc:manual:203.0.113.20:8009").map { $0.id == "manual:203.0.113.20:8009" && $0.kind == .cast } == true
      && SpeakerKey.parse("cc:tv") == nil,
      "speaker key: Cast prefix, colons kept")
check(CastID.key(appID: CastID.app("nest-1")) == "cc:nest-1" && CastID.app(key: "cc:nest-1") == "cast:nest-1"
      && CastID.device(key: "cc:nest-1") == "nest-1" && CastID.device(key: "lc:cast:nest-1") == nil
      && SpeakerKey.of(id: "cast:nest-1", tag: .cast) == "cc:nest-1"
      && SpeakerKey.of(id: "native:A1", tag: .airplay) == "ap:native:A1" && SpeakerKey.of(id: "usb-1", tag: .usb) == "lc:usb-1",
      "speaker key: Cast app ID maps to its key and back")
check(["ap:native:pair:5A1B-77", "ap:pair:5A1B-77", "ap:native:theater:9F00", "ap:theater:9F00"].allSatisfy { key in
          SpeakerKey.parseAny(key).map { SpeakerKey.make(id: $0.id, kind: $0.kind) == key && $0.kind == .airplay } == true
      } && SpeakerKey.parse("ap:native:pair:5A1B-77").map { $0.id == "native:pair:5A1B-77" && $0.airplay } == true,
      "speaker key: pair and theater targets round-trip")
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
// covers: S204
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
    // Zones: one zone per speaker, so a zone holding the new key keeps it and the old key goes everywhere.
    let zoned = Migration.Saved(
        enabledAirPlay: [], airplayVolumes: [:], groups: [], autoConnect: [],
        zones: [SpeakerZone(id: "z1", name: "Kitchen", members: ["ap:\(tvOld)", "lc:usb:out", "ap:\(gone)"], volume: 40),
                SpeakerZone(id: "z2", name: "Den", members: ["ap:\(denOld)"], volume: 100),
                SpeakerZone(id: "z3", name: "Office", members: ["ap:\(denNew)"], volume: 100)])
    check(Migration.renames(zoned, receivers: receivers) == [tvOld: tvNew, denOld: denNew],
          "speaker IDs: a bundled-engine ID kept only in a zone moves too", "\(Migration.renames(zoned, receivers: receivers))")
    let movedZones = Migration.migrate(zoned, receivers: receivers).zones
    check(movedZones == [
            SpeakerZone(id: "z1", name: "Kitchen", members: ["ap:\(tvNew)", "lc:usb:out", "ap:\(gone)"], volume: 40),
            SpeakerZone(id: "z2", name: "Den", members: [], volume: 100),
            SpeakerZone(id: "z3", name: "Office", members: ["ap:\(denNew)"], volume: 100)],
          "speaker IDs: zone members are renamed in place, and a zone already holding the new key keeps it",
          "\(movedZones)")
    var namedZoned = zoned
    namedZoned.zones[0].memberNames = ["ap:\(tvOld)": "TV", "lc:usb:out": "USB"]
    namedZoned.zones[1].memberNames = ["ap:\(denOld)": "Den speaker"]
    let renamedNames = Migration.migrate(namedZoned, receivers: receivers).zones.map(\.memberNames)
    check(renamedNames == [["ap:\(tvNew)": "TV", "lc:usb:out": "USB"], nil, nil],
          "speaker IDs: a zone member's kept name moves with its key, and goes when a zone already holds the new key",
          "\(renamedNames)")
    let flat = [Double](repeating: 0, count: 10)
    let preset = SpeakerPreset(id: "p1", name: "Evening",
                               members: [.init(key: "ap:\(tvOld)", volume: 35), .init(key: "lc:usb:out", volume: 50),
                                         .init(key: "ap:\(denOld)", volume: 80), .init(key: "ap:\(denNew)", volume: 55)],
                               zones: [], master: 100, gains: flat)
    let presetted = Migration.Saved(enabledAirPlay: [], airplayVolumes: [:], groups: [], autoConnect: [], presets: [preset])
    check(Migration.renames(presetted, receivers: receivers) == [tvOld: tvNew, denOld: denNew],
          "speaker IDs: a bundled-engine ID kept only in a preset moves too")
    check(Migration.migrate(presetted, receivers: receivers).presets.first?.members
            == [.init(key: "ap:\(tvNew)", volume: 35), .init(key: "lc:usb:out", volume: 50), .init(key: "ap:\(denNew)", volume: 55)],
          "speaker IDs: speaker ID migration renames preset members, and a preset already holding the new key keeps it",
          "\(Migration.migrate(presetted, receivers: receivers).presets)")
}

// MARK: Zones — the groups become zones once (Zones.swift, AppSettings.swift, S245, S248)
do {
    typealias Keys = AppSettings.Keys
    final class Stored { var values: [String: Any] = [:]; var order: [String] = [] }
    let stored = Stored()
    let store = SettingsStore(read: { stored.values[$0] },
                              write: { value, key in stored.values[key] = value; stored.order.append(key) })
    let groups = [
        SpeakerGroup(id: "g1", name: "Living Room", members: ["lc:BuiltInSpeakerDevice", "ap:native:AABBCCDDEE01"]),
        SpeakerGroup(id: "g2", name: "Group 2", members: ["ap:native:AABBCCDDEE01", "cc:Chromecast-1"]),
        SpeakerGroup(id: "g3", name: "Den", members: ["lc:BuiltInSpeakerDevice"]),
    ]
    let expected = [
        SpeakerZone(id: "g1", name: "Living Room", members: ["lc:BuiltInSpeakerDevice", "ap:native:AABBCCDDEE01"], volume: 100),
        SpeakerZone(id: "g2", name: "Group 2", members: ["cc:Chromecast-1"], volume: 100),
        SpeakerZone(id: "g3", name: "Den", members: [], volume: 100),
    ]
    check(ZoneMigration.zones(from: Array(groups.prefix(1))) == Array(expected.prefix(1))
            && ZoneMigration.zones(from: []).isEmpty,
          "zones: disjoint groups become zones unchanged, at volume 100")
    check(ZoneMigration.zones(from: groups) == expected,
          "zones: overlapping groups keep each speaker in its first zone, and a group left empty is an empty zone",
          "\(ZoneMigration.zones(from: groups))")

    store.save(groups: groups)
    stored.order = []
    let first = store.load()
    check(first.zones == expected && first.groups == groups,
          "zones: the first launch moves the groups to zones and leaves the saved groups as they are", "\(first.zones)")
    // The presets' own migration follows S248 in the same load, so "last" means last of the zone writes.
    let zoneWrites = stored.order.prefix { ![Keys.presets, Keys.presetsMigrated].contains($0) }
    check(zoneWrites.last == Keys.zonesMigrated && zoneWrites.contains(Keys.zones)
            && stored.values[Keys.zonesMigrated] as? Bool == true,
          "zones: the migrated mark is written last", "\(stored.order)")
    stored.values[Keys.zonesMigrated] = nil
    stored.values[Keys.zones] = nil
    check(store.load().zones == expected, "zones: a migration cut short before its mark runs again to the same zones")
    store.save(groups: [SpeakerGroup(id: "g9", name: "Later", members: ["lc:usb:out"])])
    stored.order = []
    check(store.load().zones == expected && stored.order.isEmpty,
          "zones: migrating twice changes nothing: once marked, a launch writes nothing")
    check(first.zonesNoticeDue, "zones: groups moved to zones leave the notice due for the next window")
    store.zonesNoticeShown()
    check(!store.load().zonesNoticeDue, "zones: the groups-became-zones notice shows once")

    let edited = [SpeakerZone(id: "z1", name: "Kitchen", members: ["lc:usb:out", "cc:Group-7"], volume: 35.5),
                  SpeakerZone(id: "z2", name: "Empty", members: [], volume: 0)]
    store.save(zones: edited)
    check(store.load().zones == edited, "zones: zones round-trip through the user defaults, an empty one included")
    let messy = [SpeakerZone(id: "z1", name: "A", members: ["lc:a", "lc:a", "ap:b"], volume: 140),
                 SpeakerZone(id: "z2", name: "B", members: ["ap:b", "cc:c"], volume: -3),
                 SpeakerZone(id: "z1", name: "A again", members: ["lc:d"], volume: 50)]
    store.save(zones: messy)
    check(store.load().zones == [SpeakerZone(id: "z1", name: "A", members: ["lc:a", "ap:b"], volume: 100),
                                 SpeakerZone(id: "z2", name: "B", members: ["cc:c"], volume: 0)],
          "zones: stored zones keep a speaker in its first zone, one zone per ID, volumes within 0…100")
    stored.values[Keys.zones] = Data("not json".utf8)
    check(store.load().zones.isEmpty && stored.values[Keys.zonesMigrated] as? Bool == true,
          "zones: unreadable zones read as none and never migrate again")

    let unnamed = [SpeakerZone(id: "z1", name: "Kitchen", members: ["ap:native:X", "lc:blank"], volume: 80),
                   SpeakerZone(id: "z2", name: "Den", members: [], volume: 100)]
    let named = SpeakerZone.remembering(["ap:native:X": "Kitchen HomePod", "lc:blank": " ", "lc:other": "Desk"],
                                        in: unnamed)
    store.save(zones: named)
    let reloaded = store.load().zones
    let moved = ZoneEdit.moving("ap:native:X", to: "z2", in: reloaded)
    let removed = ZoneEdit.removing("ap:native:X", from: moved)
    let older = try? JSONDecoder().decode([SpeakerZone].self,
                                          from: Data(#"[{"id":"z","name":"Z","members":["lc:x"],"volume":50}]"#.utf8))
    let rewritten = (try? JSONEncoder().encode(older ?? [])).map { String(decoding: $0, as: UTF8.self) } ?? ""
    check(SpeakerZone.storedName(of: "ap:native:X", in: reloaded) == "Kitchen HomePod"
            && SpeakerZone.storedName(of: "lc:blank", in: reloaded) == nil
            && SpeakerZone.storedName(of: "lc:other", in: reloaded) == nil
            && SpeakerZone.remembering(["ap:native:X": "Kitchen HomePod"], in: reloaded) == reloaded
            && SpeakerZone.storedName(of: "ap:native:X", in: moved) == "Kitchen HomePod"
            && SpeakerZone.storedName(of: "ap:native:X", in: removed) == nil
            && removed.allSatisfy { $0.memberNames == nil }
            && older == [SpeakerZone(id: "z", name: "Z", members: ["lc:x"], volume: 50)]
            && !rewritten.contains("memberNames"),
          "zones: an absent member shows its last listed name after a reload, and zones stored before it load unchanged",
          "\(reloaded)")
    var louder = reloaded
    louder[0].volume = 30
    check(SpeakerZone.systemName(of: "ap:native:X", listed: "", in: reloaded) == "Kitchen HomePod"
            && SpeakerZone.systemName(of: "ap:native:X", listed: "Kitchen", in: reloaded) == "Kitchen"
            && SpeakerZone.systemName(of: "lc:blank", listed: "", in: reloaded) == ""
            && SpeakerZone.differInNamesOnly(reloaded, unnamed)
            && !SpeakerZone.differInNamesOnly(louder, unnamed)
            && !SpeakerZone.differInNamesOnly(ZoneEdit.removing("lc:blank", from: unnamed), unnamed),
          "zones: an absent member reads its kept name, a listed one its name now, and a name alone re-applies no volume")
}

// MARK: Presets — overlapping groups become presets once (Presets.swift, AppSettings.swift, S251, S254)
do {
    typealias Keys = AppSettings.Keys
    final class Stored { var values: [String: Any] = [:]; var order: [String] = [] }
    let stored = Stored()
    let store = SettingsStore(read: { stored.values[$0] },
                              write: { value, key in stored.values[key] = value; stored.order.append(key) })
    let flat = [Double](repeating: 0, count: 10)
    store.save(groups: [
        SpeakerGroup(id: "g1", name: "Living Room", members: ["lc:BuiltInSpeakerDevice", "ap:native:AABBCCDDEE01"]),
        SpeakerGroup(id: "g2", name: "Group 2", members: ["ap:native:AABBCCDDEE01", "cc:Chromecast-1"]),
        SpeakerGroup(id: "g3", name: "Den", members: ["lc:BuiltInSpeakerDevice"]),
    ])
    stored.values[Keys.localVolumes] = ["BuiltInSpeakerDevice": 30.0]
    stored.values[Keys.airplayVolumes] = ["native:AABBCCDDEE01": 60.0]
    stored.values[Keys.master] = 80.0
    let expected = [
        SpeakerPreset(id: "g2", name: "Group 2",
                      members: [.init(key: "ap:native:AABBCCDDEE01", volume: 60), .init(key: "cc:Chromecast-1", volume: nil)],
                      zones: [.init(id: "g1", volume: 100), .init(id: "g2", volume: 100)], master: 80, gains: flat),
        SpeakerPreset(id: "g3", name: "Den", members: [.init(key: "lc:BuiltInSpeakerDevice", volume: 30)],
                      zones: [.init(id: "g1", volume: 100)], master: 80, gains: flat),
    ]
    stored.order = []
    let first = store.load()
    check(first.presets == expected && first.groups.count == 3,
          "presets: an overlapping group becomes a preset with its full members", "\(first.presets)")
    check(stored.order.last == Keys.presetsMigrated && stored.values[Keys.presetsMigrated] as? Bool == true,
          "presets: the migrated mark is written last", "\(stored.order)")
    stored.values[Keys.presetsMigrated] = nil
    stored.values[Keys.presets] = nil
    check(store.load().presets == expected, "presets: a migration cut short before its mark runs again to the same presets")
    stored.values[Keys.master] = 20.0
    stored.order = []
    check(store.load().presets == expected && stored.order.isEmpty,
          "presets: migrating presets twice changes nothing: once marked, a launch writes nothing")

    let edited = [SpeakerPreset(id: "p1", name: "Evening",
                                members: [.init(key: "lc:usb:out", volume: 35.5), .init(key: "cc:Group-7", volume: nil)],
                                zones: [.init(id: "z1", volume: 40)], master: 70, gains: [3, -2.5, 0, 0, 0, 0, 0, 0, 1, 12])]
    store.save(presets: edited)
    check(store.load().presets == edited, "presets: presets round-trip through the user defaults, a fixed Cast volume included")
    let messy = [SpeakerPreset(id: "p1", name: "A",
                               members: [.init(key: "lc:a", volume: 140), .init(key: "lc:a", volume: 10), .init(key: "cc:x", volume: nil),
                                         .init(key: "cc:y", volume: 20), .init(key: "zz:b", volume: 5),
                                         .init(key: "ap:c", volume: 140)],
                               zones: [.init(id: "z1", volume: -3), .init(id: "z1", volume: 50)], master: 130,
                               gains: [0.3, 20, -20, 0, 0, 0, 0, 0, 0, 0]),
                 SpeakerPreset(id: "p1", name: "A again", members: [], zones: [], master: 50, gains: flat),
                 SpeakerPreset(id: "p2", name: "Short", members: [], zones: [], master: 50, gains: [0, 0])]
    store.save(presets: messy)
    check(store.load().presets == [SpeakerPreset(id: "p1", name: "A",
                                                 members: [.init(key: "lc:a", volume: 100), .init(key: "cc:x", volume: nil),
                                                           .init(key: "ap:c", volume: 140)],
                                                 zones: [.init(id: "z1", volume: 0)], master: 100,
                                                 gains: [0.5, 12, -12, 0, 0, 0, 0, 0, 0, 0])],
          "presets: stored presets keep one Cast key, one preset per ID, levels within range (an AirPlay own volume may pass 100)",
          "\(store.load().presets)")
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

    // covers: S171
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
// covers: S170 (all but the first line's versions and local time)
do {
    check(DebugTrace.isEnabled(environment: ["AUDIOSYNC_DEBUG": "1"]), "debug log: on with AUDIOSYNC_DEBUG=1")
    check(!DebugTrace.isEnabled(environment: [:]) && !DebugTrace.isEnabled(environment: ["AUDIOSYNC_DEBUG": "0"])
          && !DebugTrace.isEnabled(environment: ["AUDIOSYNC_DEBUG": ""])
          && !DebugTrace.isEnabled(environment: ["AUDIOSYNC_DEBUG": "true"]),
          "debug log: off unless the variable is exactly 1")
    check(LaunchAutoConnect.skips(environment: ["AUDIOSYNC_DEBUG": "1", "AUDIOSYNC_NO_AUTO_CONNECT": "1"]),
          "auto-connect: skipped with AUDIOSYNC_NO_AUTO_CONNECT=1 in a debug launch")
    check(!LaunchAutoConnect.skips(environment: ["AUDIOSYNC_NO_AUTO_CONNECT": "1"])
          && !LaunchAutoConnect.skips(environment: ["AUDIOSYNC_DEBUG": "0", "AUDIOSYNC_NO_AUTO_CONNECT": "1"])
          && !LaunchAutoConnect.skips(environment: ["AUDIOSYNC_DEBUG": "1"])
          && !LaunchAutoConnect.skips(environment: ["AUDIOSYNC_DEBUG": "1", "AUDIOSYNC_NO_AUTO_CONNECT": "true"]),
          "auto-connect: ignored without the debug log, and unless it is exactly 1")
    typealias Keys = AppSettings.Keys
    final class Saved { var values: [String: Any] = [:] }
    let saved = Saved()
    let disk = SettingsStore(read: { saved.values[$0] }, write: { value, key in saved.values[key] = value })
    disk.save(ids: ["native:AABBCCDDEE01"], forKey: Keys.enabledAirPlay)
    disk.save(ids: ["BuiltInSpeakerDevice"], forKey: Keys.enabledLocal)
    disk.save(ids: ["ap:native:AABBCCDDEE01"], forKey: Keys.autoConnect)
    _ = disk.load()
    let before = NSDictionary(dictionary: saved.values)
    let run = disk.memoryOnly()
    let held = run.load().atLaunch(holdingSpeakersOff: true)
    check(held.enabledAirPlay.isEmpty && held.enabledLocal.isEmpty && held.autoConnect == ["ap:native:AABBCCDDEE01"],
          "auto-connect skipped: the speakers saved as on start off, the marks stay", "\(held.enabledAirPlay) \(held.enabledLocal)")
    run.save(ids: ["Headphones"], forKey: Keys.enabledLocal)
    run.save(ids: [], forKey: Keys.autoConnect)
    run.write(nil, Keys.enabledAirPlay)
    check(AppSettings.ids(run.read(Keys.enabledLocal)) == ["Headphones"] && run.read(Keys.enabledAirPlay) == nil,
          "auto-connect skipped: the run reads its own changes")
    check(NSDictionary(dictionary: saved.values) == before,
          "auto-connect skipped: nothing the run changes is saved")
    let next = disk.load().atLaunch(holdingSpeakersOff: false)
    check(next.enabledAirPlay == ["native:AABBCCDDEE01"] && next.enabledLocal == ["BuiltInSpeakerDevice"]
          && next.autoConnect == ["ap:native:AABBCCDDEE01"],
          "auto-connect skipped: the next launch starts from the speakers saved before")
    if !DebugTrace.enabled {
        var built = false
        DebugTrace.log({ () -> String in built = true; return "never" }())
        check(!built, "debug log: a message is never built while the log is off")
    }
    let t = Date(timeIntervalSince1970: 1_792_345_933.25)   // 2026-10-18 17:52:13.250 UTC
    check(DebugLogFile.line("[AudioSync] hello", at: t) == "2026-10-18T17:52:13.250Z [AudioSync] hello\n",
          "debug log: each line starts with the UTC time to the millisecond")
    let forged = "Den\r\n2026-01-01T00:00:00.000Z forged\t\u{1B}[2J"
    check(DebugLogFile.line(forged, at: t)
          == #"2026-10-18T17:52:13.250Z Den\r\#n    | 2026-01-01T00:00:00.000Z forged\t\x1b[2J\#n"#,
          "debug log: a line after the first is marked as a continuation, other control characters show escaped",
          DebugLogFile.line(forged, at: t))
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

    // The badge icon follows the calibration rule: over Bluetooth only a headphone name means headphones.
    check(SpeakerTag.bluetooth.symbol(name: "soundcore Select 4 Go") == "hifispeaker.fill"
          && SpeakerTag.bluetooth.symbol(name: "JBL Flip 6") == "hifispeaker.fill"
          && SpeakerTag.bluetooth.symbol(name: "AirPods Pro") == "headphones"
          && SpeakerTag.bluetooth.symbol(name: "WH-1000XM5 Headphones") == "headphones"
          && SpeakerTag.bluetooth.symbol(name: "Galaxy Buds2") == "headphones",
          "layout: a Bluetooth speaker gets the speaker badge, Bluetooth headphones keep the headphones badge")
    check(SpeakerTag.builtIn.symbol(name: "AirPods Pro") == "laptopcomputer"
          && SpeakerTag.usb.symbol(name: "Desk Headphones") == "cable.connector"
          && SpeakerTag.hdmi.symbol(name: "LG TV") == "display"
          && SpeakerTag.airplay.symbol(name: "Kitchen") == "homepod.fill"
          && SpeakerTag.cast.symbol(name: "Nest Audio") == "hifispeaker.fill"
          && SpeakerTag.virtual.symbol(name: "Headset") == "hifispeaker.fill",
          "layout: outside Bluetooth the badge depends on the transport only")
    let badges = kinds.flatMap { [$0.symbol(name: "Speaker"), $0.symbol(name: "AirPods")] }
        .filter { NSImage(systemSymbolName: $0, accessibilityDescription: nil) == nil }
    check(badges.isEmpty, "layout: every badge icon is an existing SF Symbol", "\(badges)")
    check(SpeakerTag.isHeadphoneName("Living Room AirPods Pro") && SpeakerTag.isHeadphoneName("EARBUDS")
          && !SpeakerTag.isHeadphoneName("soundcore Select 4 Go") && !SpeakerTag.isHeadphoneName(""),
          "layout: a headphone word anywhere in the name, in any case, marks headphones")

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
check(Equalizer.presets.count == asc_eq_preset_count(), "eq: the app names every preset the core has")
do {
    var reset = Equalizer.Settings(enabled: true, preamp: -4, gains: Equalizer.presets[1].gains)
    reset.reset()
    check(reset.enabled && reset.preamp == 0 && Equalizer.matching(reset.gains, saved: []) == .preset(.flat),
          "eq: Reset reads Flat, zeroes the preamp and keeps the switch")

    var gains = Equalizer.Settings().gains
    gains[3] = 2.5
    var curves: [Equalizer.Curve] = []
    check(Equalizer.save("  Desk  ", gains: gains, in: &curves) == 0 && curves[0].name == "Desk",
          "eq curves: a saved curve keeps its trimmed name")
    check(Equalizer.matching(gains, saved: curves) == .saved(0), "eq curves: the gains match the saved curve")
    let model = EqualizerModel(settings: Equalizer.Settings(enabled: false, preamp: -3, gains: gains), curves: curves)
    model.pick(.preset(Equalizer.presets[1].id))
    model.pick(.saved(0))
    check(model.shownCurve == .saved(0) && model.settings.preamp == -3 && model.settings.enabled,
          "eq curves: picking a curve keeps the preamp and turns the equalizer on")
    check(Equalizer.save("Flat-ish", gains: Equalizer.presets[0].gains, in: &curves) == 1
            && Equalizer.matching(Equalizer.presets[0].gains, saved: curves) == .saved(1),
          "eq curves: a saved curve with a preset's gains resolves to the saved curve")
    let flatish = EqualizerModel(settings: Equalizer.Settings(enabled: true, preamp: 0, gains: Equalizer.presets[0].gains),
                                 curves: curves)
    check(flatish.shownSavedIndex == 1,
          "eq curves: a saved curve with a preset's gains keeps Rename and Delete")
    flatish.pick(.preset(.flat))
    check(flatish.shownCurve == .preset(.flat) && flatish.shownSavedIndex == nil,
          "eq curves: a picked preset shows as the preset beside a saved curve with its gains")
    flatish.pick(.saved(1))
    check(flatish.shownSavedIndex == 1, "eq curves: picking that saved curve shows it again")
    var moved = flatish.settings
    moved.gains[0] = 3
    flatish.set(moved)
    check(flatish.shownSavedIndex == nil && flatish.shownCurve == nil,
          "eq curves: moving a band off the picked saved curve shows Custom")
    flatish.pick(.saved(1))
    flatish.reset()
    check(flatish.shownCurve == .preset(.flat), "eq curves: Reset shows Flat beside a saved curve with its gains")
    flatish.deleteCurve(0)
    flatish.pick(.saved(0))
    flatish.deleteCurve(0)
    check(flatish.shownCurve == .preset(.flat) && flatish.chosen == nil,
          "eq curves: deleting the shown saved curve falls back to the preset")
    check(Equalizer.save("DESK", gains: Equalizer.presets[1].gains, in: &curves) == 0 && curves.count == 2,
          "eq curves: saving under a taken name, ignoring case, replaces that curve")
    check(Equalizer.save("   ", gains: gains, in: &curves) == nil, "eq curves: an empty name saves nothing")
    check(!Equalizer.rename(0, to: "flat-ISH", in: &curves) && Equalizer.rename(0, to: "Office", in: &curves)
            && curves[0].name == "Office",
          "eq curves: a rename to another curve's name is refused")
    check(Equalizer.curveName(String(repeating: "x", count: 60))?.count == Equalizer.maxCurveNameCharacters,
          "eq curves: a name is cut to 40 characters")
    var full = (0..<Equalizer.maxCurves).map { Equalizer.Curve(name: "C\($0)", gains: gains) }
    check(Equalizer.save("One more", gains: gains, in: &full) == nil && Equalizer.save("c7", gains: gains, in: &full) == 7,
          "eq curves: a full list refuses a new name but replaces a taken one")

    let stored = Data(#"[{"name":" Desk ","gains":[1.3,"loud",30,-30]},{"name":"desk","gains":[]},{"gains":[1]},7,{"name":"Late","gains":[0]}]"#.utf8)
    let read = StoredCurves.decode(stored)
    check(read.map(\.name) == ["Desk", "Late"] && read[0].gains == [1.5, 0, 12, -12, 0, 0, 0, 0, 0, 0],
          "eq curves: stored curves are read leniently and cleaned")
    check(StoredCurves.decode(StoredCurves.encode(read)) == read && StoredCurves.decode(nil).isEmpty,
          "eq curves: saved curves come back unchanged")
    let text = UIStrings()
    check(text.equalizerCurveName(nil, saved: read) == text.equalizerCustom
            && text.equalizerCurveName(.saved(1), saved: read) == "Late",
          "eq curves: the picker names the shown curve, or Custom when none matches")
}

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
    list.post("Your groups are zones now.", subject: Notice.zones, at: t0, info: true)
    list.post("x", subject: "lc:1", at: t0 + 1)
    check(list.items.map(\.info) == [false, true],
          "notices: one that reports a change keeps its info flag, a problem has none")
    list.clearAll()
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
// covers: S101, S154 (the hint, when the host looks again, and the browser states)
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
    // The first look: "Looking for speakers…" until the same 20 seconds pass, the host looking again then.
    func searching(_ access: LocalNetworkAccess, up: Bool = true, empty since: Date?, at now: Date) -> Bool {
        AirPlaySearch.isSearching(access: access, senderUp: up, emptySince: since, now: now)
    }
    check(searching(.unknown, empty: t0, at: t0) && searching(.allowed, empty: t0, at: t0 + wait - 1)
            && !searching(.allowed, empty: t0, at: t0 + wait) && !searching(.unknown, empty: t0, at: t0 + wait)
            && !searching(.denied, empty: t0, at: t0) && !searching(.unknown, up: false, empty: t0, at: t0)
            && !searching(.unknown, empty: nil, at: t0),
          "AirPlay search: looking while a running sender has listed nothing for under 20 s, unless access is denied")
    check(AirPlaySearch.endsAt(access: .allowed, senderUp: true, emptySince: t0, now: t0 + 1) == t0 + wait
            && AirPlaySearch.endsAt(access: .allowed, senderUp: true, emptySince: t0, now: t0 + wait) == nil
            && AirPlaySearch.endsAt(access: .denied, senderUp: true, emptySince: t0, now: t0) == nil,
          "AirPlay search: the host looks again when the first look ends, even with access allowed")
    let lookingText = UIStrings()
    check(lookingText.networkSpeakersNote(airPlayNone: true, cast: .quiet, airPlaySearching: true) == "Looking for speakers…"
            && lookingText.networkSpeakersNote(airPlayNone: true, cast: .searching, airPlaySearching: true)
                == "Looking for speakers…"
            && lookingText.networkSpeakersNote(airPlayNone: true, cast: .noneFound, airPlaySearching: true)?
                .hasPrefix("Looking for AirPlay speakers… No Google Cast speakers found yet.") == true
            && lookingText.networkSpeakersNote(airPlayNone: false, cast: .quiet, airPlaySearching: true) == nil
            && lookingText.networkSpeakersNote(airPlayNone: true, cast: .quiet, airPlaySearching: false)
                == lookingText.airPlayNoneFound(),
          "AirPlay search: Connected Speakers says it is looking, not the network advice, during the first look")
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
// covers: S106 (the access kinds that stand)
check(SpeakerAvailability(.open) == .open, "availability: open")
check(SpeakerAvailability(.code(password: false)) == .needsCode, "availability: PIN asks for a code")
check(SpeakerAvailability(.code(password: true)) == .needsCode, "availability: password asks for a code")
check(UIStrings().availability(.codeEveryTime) == "Asks for a code every time"
        && UIStrings().availabilityHelp(.codeEveryTime) != nil,
      "availability: a speaker that asks for a code every time says so under its name")
check(SpeakerAvailability(.code(password: false), everyTime: true, on: true) == .codeEveryTime
        && SpeakerAvailability(.open, everyTime: true, on: false) == .codeEveryTime
        && SpeakerAvailability(.open, everyTime: true, on: true) == .open
        && SpeakerAvailability(.code(password: false), everyTime: false, on: false) == .needsCode
        && SpeakerAvailability(.open, everyTime: false, on: false) == .open,
      "availability: asks for a code every time while it asks or is off; paired for this run and on, it is open")
do {
    let pinOff = UIStrings().airPlayTurnedOff(name: "Den", reason: .needsPin)
    let passwordOff = UIStrings().airPlayTurnedOff(name: "Den", reason: .needsPassword)
    check(pinOff.contains("PIN") && !pinOff.lowercased().contains("password"),
          "airplay off: a speaker that needs a PIN says PIN, not password")
    check(passwordOff.contains("password") && !passwordOff.contains("PIN"),
          "airplay off: a speaker that asks for a password says password, not PIN")
    let text = UIStrings()
    check(text.nativeAirPlayAsksForCode(name: "Den", reason: .needsCode, unasked: false) == pinOff,
          "native off: a speaker that needs a PIN gets the PIN notice")
    check(text.nativeAirPlayAsksForCode(name: "Den", reason: .needsPassword, unasked: false) == passwordOff
            && text.nativeAirPlayAsksForCode(name: "Den", reason: .needsPassword, unasked: true) == passwordOff,
          "native off: a speaker that asks for a password gets the password notice")
    check(text.nativeAirPlayAsksForCode(name: "Den", reason: .needsCode, unasked: true)
            == text.nativeAirPlayNotAccepted(name: "Den"),
          "native off: a speaker that turned the session down unasked says it didn't accept")
    check(text.nativeAirPlayAsksForCode(name: "Den", reason: .unreachable, unasked: false) == nil,
          "native off: other reasons keep their own notice")
}

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

    // covers: S192, S193, S195, S198, S199, S200, S202 (through a fake Core Audio volume and mute)
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

    // covers: S200 (the Mac's labels)
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
check(CaptureCheck.verdict(sessionHeard: true, runHeard: false, knownPlayingFor: 60) == .flowing, "capture: sound this session → flowing")
check(CaptureCheck.verdict(sessionHeard: false, runHeard: false, knownPlayingFor: nil) == .waiting, "capture: no sound, unknown source → waiting")
check(CaptureCheck.verdict(sessionHeard: false, runHeard: false, knownPlayingFor: 2) == .waiting, "capture: playing only briefly → still waiting")
check(CaptureCheck.verdict(sessionHeard: false, runHeard: false, knownPlayingFor: CaptureCheck.playingGrace) == .notWorking, "capture: playing for the grace period with no sound → not working")
check(CaptureCheck.verdict(sessionHeard: false, runHeard: false, knownPlayingFor: nil) != .notWorking, "capture: a paused or unknown source never blames the permission")
check(CaptureCheck.verdict(sessionHeard: false, runHeard: true, knownPlayingFor: 60) == .waiting, "capture: sound captured earlier in this run → waiting, not the permission")
check(CaptureCheck.provesPermission(.system) && CaptureCheck.provesPermission(.app(bundleID: "com.apple.Music")),
      "capture: sound from a system or app tap proves the permission")
check(!CaptureCheck.provesPermission(.input(uid: "BuiltInMicrophoneDevice")),
      "capture: a microphone or line-in heard never proves the tap permission")
do {
    let old = NowPlayingMonitor.Position(elapsed: 10, length: 200, readAt: 50, playing: true)
    let moved = NowPlayingMonitor.Position(elapsed: 12, length: 200, readAt: 52, playing: true)
    let stuck = NowPlayingMonitor.Position(elapsed: 10, length: 200, readAt: 52, playing: true)
    let paused = NowPlayingMonitor.Position(elapsed: 12, length: 200, readAt: 52, playing: false)
    let otherTrack = NowPlayingMonitor.Position(elapsed: 12, length: 90, readAt: 52, playing: true)
    check(NowPlayingMonitor.Position.advances(from: old, to: moved), "now playing: a position moving on with the clock advances")
    check(!NowPlayingMonitor.Position.advances(from: old, to: stuck), "now playing: a position standing still does not advance")
    check(!NowPlayingMonitor.Position.advances(from: old, to: paused), "now playing: a paused reading does not advance")
    check(!NowPlayingMonitor.Position.advances(from: old, to: otherTrack), "now playing: another length is no advance")
    check(!NowPlayingMonitor.Position.advances(from: old, to: old), "now playing: the same reading twice does not advance")
}
do {
    var block: [Float] = Array(repeating: 0, count: 8)
    check(!block.withUnsafeBufferPointer { CaptureChannels.hasSound($0.baseAddress!, frames: 4) }, "capture: zeros are silent")
    block[7] = 0.001
    check(block.withUnsafeBufferPointer { CaptureChannels.hasSound($0.baseAddress!, frames: 4) }, "capture: one sample above zero is sound")
    check(!block.withUnsafeBufferPointer { CaptureChannels.hasSound($0.baseAddress!, frames: 3) }, "capture: only the given frames count")
}
do {
    // Seen on a Mac: right after launch, All System Audio silent, and Music
    // reporting a paused local file as playing, its position standing still.
    var watch = CaptureWatch()
    var blamed = false
    for step in 0..<15 {
        let uptime = 100 + 2 * Double(step)
        let stuck = NowPlayingMonitor.Position(elapsed: 42, length: 200, readAt: uptime - 0.3, playing: true)
        if watch.verdict(at: uptime, sessionHeard: false, runHeard: false,
                         reportsPlaying: true, position: stuck) == .notWorking { blamed = true }
    }
    check(!blamed, "capture: a track reported playing whose position stands still never blames the permission")
}
do {
    var watch = CaptureWatch()
    var blamed = false
    for step in 0..<15 {
        let uptime = 100 + 2 * Double(step)
        let moving = NowPlayingMonitor.Position(elapsed: 10 + 2 * Double(step), length: 200,
                                                readAt: uptime - 0.3, playing: true)
        if watch.verdict(at: uptime, sessionHeard: false, runHeard: true,
                         reportsPlaying: true, position: moving) == .notWorking { blamed = true }
    }
    check(!blamed, "capture: once this app run heard audio, later silence never blames the permission")
}
do {
    var watch = CaptureWatch()
    var blamed = false
    for step in 0..<15 {
        let uptime = 100 + 2 * Double(step)
        if watch.verdict(at: uptime, sessionHeard: false, runHeard: false,
                         reportsPlaying: true, position: nil) == .notWorking { blamed = true }
    }
    check(!blamed, "capture: a playing report with no position to watch never blames the permission")
}
do {
    // A denied tap delivers zeros: frames arrive (512 a callback), none with sound.
    var watch = CaptureWatch()
    var verdicts: [CaptureCheck] = []
    var block = [Float](repeating: 0, count: 1024)
    for step in 0..<15 {
        let uptime = 100 + 2 * Double(step)
        let moving = NowPlayingMonitor.Position(elapsed: 10 + 2 * Double(step), length: 200,
                                                readAt: uptime - 0.3, playing: true)
        let heard = block.withUnsafeBufferPointer { CaptureChannels.hasSound($0.baseAddress!, frames: 512) }
        verdicts.append(watch.verdict(at: uptime, sessionHeard: heard, runHeard: false,
                                      reportsPlaying: true, position: moving))
    }
    check(verdicts.first == .waiting && verdicts.last == .notWorking,
          "capture: a track that keeps advancing while a tap delivers only zeros blames the permission after the grace period")
    block[300] = 0.25
    let heard = block.withUnsafeBufferPointer { CaptureChannels.hasSound($0.baseAddress!, frames: 512) }
    let moving = NowPlayingMonitor.Position(elapsed: 42, length: 200, readAt: 131.7, playing: true)
    check(watch.verdict(at: 132, sessionHeard: heard, runHeard: heard, reportsPlaying: true, position: moving) == .flowing,
          "capture: real sound arriving clears the verdict")
}
do {
    // Silent zeros while nothing plays, or the player is paused: a calm wait, never the permission.
    var watch = CaptureWatch()
    var verdicts: [CaptureCheck] = []
    for step in 0..<15 {
        let uptime = 100 + 2 * Double(step)
        let paused = NowPlayingMonitor.Position(elapsed: 42, length: 200, readAt: uptime - 0.3, playing: false)
        verdicts.append(watch.verdict(at: uptime, sessionHeard: false, runHeard: false,
                                      reportsPlaying: step.isMultiple(of: 2), position: step < 7 ? nil : paused))
    }
    check(verdicts.allSatisfy { $0 == .waiting }, "capture: silent zeros with nothing live → waiting")
}

// MARK: Calibration fill: a quiet tap still carries the test signal (CalibrationTap.swift)
do {
    // Calibrate All on All System Audio with nothing playing failed every time: the
    // signal rides on captured blocks, and a tap whose source is silent delivers none.
    var pace = CalibrationFillPace(rate: 48_000)
    let s: UInt64 = 1_000_000_000
    let first = pace.step(nowNs: 10 * s, lastCaptureNs: 0)
    check(first.frames == 1920 && first.firstNs == 10 * s - 40_000_000,
          "calibration fill: a capture that never delivered gets silence, from 40 ms back, for the signal to ride on",
          "\(first)")
    let next = pace.step(nowNs: 10 * s + 2_000_000, lastCaptureNs: 0)
    check(next.frames == 96 && next.firstNs == 10 * s,
          "calibration fill: each step goes on where the last one ended, in real time", "\(next)")
    let flowing = pace.step(nowNs: 11 * s, lastCaptureNs: 11 * s - 5_000_000)
    let racing = pace.step(nowNs: 11 * s, lastCaptureNs: 11 * s + 1000)
    check(flowing.frames == 0 && racing.frames == 0,
          "calibration fill: nothing while the capture delivers", "\(flowing) \(racing)")
    let quietAgain = pace.step(nowNs: 12 * s, lastCaptureNs: 11 * s)
    check(quietAgain.frames == 1920 && quietAgain.firstNs == 12 * s - 40_000_000,
          "calibration fill: a capture gone quiet again starts a new stretch", "\(quietAgain)")
    let late = pace.step(nowNs: 15 * s, lastCaptureNs: 11 * s)
    check(late.frames == 48_000 && late.firstNs == 14 * s,
          "calibration fill: a fill held up for seconds writes the last second only", "\(late)")

    // An input device another app set to 4096-frame buffers calls back every
    // 85 ms: a 40 ms limit would fill spans the next block writes too.
    var input = CalibrationFillPace(rate: 48_000)
    let between = input.step(nowNs: 20 * s, lastCaptureNs: 20 * s - 100_000_000, captureFrames: 4096)
    let stalled = input.step(nowNs: 20 * s + 80_000_000, lastCaptureNs: 20 * s - 100_000_000, captureFrames: 4096)
    let idle = UInt64((2 * 4096 * 1e9 / 48_000).rounded(.up))
    check(between.frames == 0 && stalled.frames > 0 && stalled.firstNs == 20 * s + 80_000_000 - idle,
          "calibration fill: a capture with long buffers counts as quiet only after two of its periods",
          "\(between) \(stalled)")
}

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
    func stereo(_ buffers: [Buffer], blockFrames: Int = CaptureChannels.blockFrames,
                layout: CaptureChannels.Layout = .stereoFloat) -> (samples: [Float], blocks: [Int]) {
        var samples: [Float] = []
        var blocks: [Int] = []
        withInput(buffers) { list in
            let scratch = UnsafeMutablePointer<Float>.allocate(capacity: blockFrames * 2)
            defer { scratch.deallocate() }
            CaptureChannels(list, layout: layout).forEachBlock(in: scratch, blockFrames: blockFrames) { block, count in
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
    // 8 kHz keeps the host ticks per frame whole on both Apple silicon and Intel timebases.
    let stampGate = BlockGate()
    stampGate.prepare(captureRate: 8000)
    let callbackNow: UInt64 = 1_000_000_000_000
    let starts = blocked.blocks.indices.map { index in
        stampGate.blockStartTicks(callbackNow: callbackNow, callbackFrames: 10_000,
                                  offset: blocked.blocks[..<index].reduce(0, +))
    }
    let step = stampGate.ticks(frames: 4096)
    check(step > 0 && starts.count == 3 && starts[1] &- starts[0] == step && starts[2] &- starts[1] == step
            && starts[2] &+ stampGate.ticks(frames: 1808) == callbackNow
            && starts[0] == callbackNow &- stampGate.ticks(frames: 10_000),
          "capture channels: each block of a long callback is stamped at its own place in the callback",
          "\(starts) step \(step)")

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
    let signed = kAudioFormatFlagIsSignedInteger | kAudioFormatFlagIsPacked
    check(CaptureInputFormat.check([format(flags: float32)]) == .usable(.stereoFloat),
          "input format: native 32-bit float is read as it is")
    check(CaptureInputFormat.check([format(flags: float32 | kAudioFormatFlagIsBigEndian)]) == .unsupported
            && CaptureInputFormat.check([format(flags: float32, bits: 64)]) == .unsupported
            && CaptureInputFormat.check([format(flags: kAudioFormatFlagIsPacked, bits: 16)]) == .unsupported
            && CaptureInputFormat.check([format(flags: signed | (8 << kLinearPCMFormatFlagsSampleFractionShift))]) == .unsupported,
          "input format: big-endian, 64-bit, unsigned and fixed-point samples aren't read")
    check(CaptureInputFormat.check([]) == .noChannels
            && CaptureInputFormat.check([format(flags: float32, channels: 0)]) == .noChannels,
          "input format: a device without input channels has nothing to capture")
    check(CaptureInputFormat.check([format(flags: float32), format(flags: signed, bits: 16, channels: 0)]) == .usable(.stereoFloat),
          "input format: a stream without channels doesn't count")

    // Integer streams: which sample type each format is.
    var packed24 = format(flags: signed, bits: 24)
    packed24.mBytesPerFrame = 6
    var high24 = format(flags: signed | kAudioFormatFlagIsAlignedHigh, bits: 24)
    high24.mBytesPerFrame = 8
    high24.mFormatFlags &= ~kAudioFormatFlagIsPacked
    var low24 = high24
    low24.mFormatFlags &= ~kAudioFormatFlagIsAlignedHigh
    var nonInterleaved16 = format(flags: signed | kAudioFormatFlagIsNonInterleaved, bits: 16)
    nonInterleaved16.mBytesPerFrame = 2
    check(CaptureChannels.Sample.of(format(flags: signed, bits: 16)) == .int16
            && CaptureChannels.Sample.of(packed24) == .int24
            && CaptureChannels.Sample.of(high24) == .int24High
            && CaptureChannels.Sample.of(low24) == .int24Low
            && CaptureChannels.Sample.of(format(flags: signed, bits: 32)) == .int32
            && CaptureChannels.Sample.of(nonInterleaved16) == .int16,
          "input format: 16-bit, 24-bit (packed and in 32) and 32-bit signed integers are read")
    check(CaptureInputFormat.check([format(flags: signed, bits: 16)])
            == .usable(CaptureChannels.Layout(left: 0, right: 1, leftSample: .int16, rightSample: .int16)),
          "input format: an integer device is captured without changing its format")

    // Integer to float: full scale, zero and the sign of the smallest step.
    func converted<T>(_ value: T, _ sample: CaptureChannels.Sample) -> Float {
        withUnsafeBytes(of: value) { sample.value(at: $0.baseAddress!) }
    }
    func converted(bytes: [UInt8], _ sample: CaptureChannels.Sample) -> Float {
        bytes.withUnsafeBytes { sample.value(at: $0.baseAddress!) }
    }
    check(converted(Int16.min, .int16) == -1 && converted(Int16.max, .int16) == 32_767 / 32_768
            && converted(Int16(0), .int16) == 0 && converted(Int16(-1), .int16) == -1 / 32_768,
          "integer input: 16-bit min, max, zero and sign")
    check(converted(bytes: [0x00, 0x00, 0x80], .int24) == -1
            && converted(bytes: [0xFF, 0xFF, 0x7F], .int24) == 8_388_607 / 8_388_608
            && converted(bytes: [0, 0, 0], .int24) == 0
            && converted(bytes: [0xFF, 0xFF, 0xFF], .int24) == -1 / 8_388_608
            && converted(bytes: [0x01, 0x00, 0x00], .int24) == 1 / 8_388_608,
          "integer input: packed 24-bit min, max, zero and sign")
    check(converted(Int32(bitPattern: 0x8000_0000), .int24High) == -1
            && converted(Int32(0x7FFF_FF00), .int24High) == 8_388_607 / 8_388_608
            && converted(Int32(0x0000_00FF), .int24High) == 0
            && converted(Int32(bitPattern: 0xFFFF_FF00), .int24High) == -1 / 8_388_608,
          "integer input: 24-bit high in 32 min, max, zero and sign; the low byte is ignored")
    check(converted(Int32(0x0080_0000), .int24Low) == -1
            && converted(Int32(0x007F_FFFF), .int24Low) == 8_388_607 / 8_388_608
            && converted(Int32(0), .int24Low) == 0
            && converted(Int32(0x00FF_FFFF), .int24Low) == -1 / 8_388_608
            && converted(Int32(bitPattern: 0xAB80_0000), .int24Low) == -1,
          "integer input: 24-bit low in 32 min, max, zero and sign; the high byte is ignored")
    check(converted(Int32.min, .int32) == -1 && converted(Int32.max, .int32) <= 1
            && converted(Int32.max, .int32) > 0.999_999 && converted(Int32(0), .int32) == 0
            && converted(Int32(-1), .int32) == -1 / 2_147_483_648,
          "integer input: 32-bit min, max, zero and sign")

    // Channel pairs on devices with more than two channels.
    check(InputChannelPair.offered(channels: 1).isEmpty && InputChannelPair.offered(channels: 2).isEmpty,
          "channel pairs: a mono or stereo device offers no choice")
    let four = InputChannelPair.offered(channels: 4)
    check(four.map { [$0.index, $0.left, $0.right] } == [[0, 0, 1], [1, 2, 3]],
          "channel pairs: four channels offer 1-2 and 3-4", "\(four)")
    let five = InputChannelPair.offered(channels: 5)
    check(five.map { [$0.left, $0.right] } == [[0, 1], [2, 3], [4, 4]] && five[2].isMono && !five[1].isMono,
          "channel pairs: an odd last channel is a pair of its own", "\(five)")
    func resolves(_ saved: Int, on channels: Int, to pair: InputChannelPair, fellBack: Bool) -> Bool {
        let found = InputChannelPair.resolve(saved: saved, channels: channels)
        return found.pair == pair && found.fellBack == fellBack
    }
    check(resolves(0, on: 8, to: .first, fellBack: false) && resolves(0, on: 1, to: .first, fellBack: false),
          "channel pairs: nothing saved reads channels 1-2, on a mono device too")
    check(resolves(1, on: 4, to: four[1], fellBack: false) && resolves(2, on: 5, to: five[2], fellBack: false),
          "channel pairs: a saved pair the device has is read")
    check(resolves(2, on: 4, to: .first, fellBack: true) && resolves(1, on: 2, to: .first, fellBack: true)
            && resolves(-1, on: 8, to: .first, fellBack: true),
          "channel pairs: a saved pair the device no longer has falls back to 1-2 and says so")
    check(InputChannelPair.change(from: 0, to: 1, streaming: true) == .restart
            && InputChannelPair.change(from: 0, to: 1, streaming: false) == .remember
            && InputChannelPair.change(from: 1, to: 1, streaming: true) == .none
            && InputChannelPair.change(from: 1, to: -1, streaming: true) == .none,
          "channel pairs: choosing another pair restarts a running session, the same one does nothing")
    check(UIStrings().inputChannelPair(four[1]) == "Channels 3-4" && UIStrings().inputChannelPair(five[2]) == "Channel 5",
          "channel pairs: shown as the device counts its channels")

    // The pair's channels reach L and R, wherever they sit.
    func pairLayout(_ pair: InputChannelPair) -> CaptureChannels.Layout {
        CaptureChannels.Layout(left: pair.left, right: pair.right, leftSample: .float32, rightSample: .float32)
    }
    check(stereo([(4, [1, 2, 3, 4, 5, 6, 7, 8])], layout: pairLayout(four[1])).samples == [3, 4, 7, 8],
          "channel pairs: pair 3-4 of an interleaved input plays its channels 3 and 4")
    check(stereo([(1, [1, 2]), (1, [3, 4]), (1, [5, 6]), (1, [7, 8])], layout: pairLayout(four[1])).samples == [5, 7, 6, 8],
          "channel pairs: pair 3-4 of one buffer per channel plays the third and fourth buffers")
    check(stereo([(2, [1, 2, 3, 4]), (3, [10, 20, 30, 40, 50, 60])], layout: pairLayout(five[2])).samples == [30, 30, 60, 60],
          "channel pairs: a lone fifth channel plays on both sides")
    check(stereo([(1, [1, 2, 3])], layout: pairLayout(.first)).samples == [1, 1, 2, 2, 3, 3],
          "channel pairs: a mono device on 1-2 still plays its channel on both sides")
    let twoStreams = [format(flags: float32), format(flags: signed, bits: 16)]
    check(CaptureInputFormat.check(twoStreams, pair: four[1])
            == .usable(CaptureChannels.Layout(left: 2, right: 3, leftSample: .int16, rightSample: .int16)),
          "channel pairs: the pair's stream decides its sample type")
    check(CaptureInputFormat.check([format(flags: float32), format(flags: kAudioFormatFlagIsPacked, bits: 16)]) == .usable(.stereoFloat)
            && CaptureInputFormat.check([format(flags: float32), format(flags: kAudioFormatFlagIsPacked, bits: 16)],
                                        pair: four[1]) == .unsupported,
          "channel pairs: only the streams holding the pair have to be readable")
    let int16Pair = CaptureChannels.Layout(left: 2, right: 3, leftSample: .int16, rightSample: .int16)
    let int16Bytes: [Int16] = [0, 0, Int16.min, 16_384, 0, 0, Int16.max, -1]
    let int16Stereo: [Float] = int16Bytes.withUnsafeBytes { raw in
        var list = AudioBufferList(mNumberBuffers: 1, mBuffers: AudioBuffer(
            mNumberChannels: 4, mDataByteSize: UInt32(raw.count), mData: UnsafeMutableRawPointer(mutating: raw.baseAddress)))
        var out = [Float](repeating: 9, count: 4)
        withUnsafePointer(to: &list) { CaptureChannels($0, layout: int16Pair).copy(from: 0, frames: 2, into: &out) }
        return out
    }
    check(int16Stereo == [-1, 0.5, Float(32_767) / 32_768, Float(-1) / 32_768],
          "channel pairs: a 16-bit input's pair 3-4 is converted to float", "\(int16Stereo)")

    let started = [format(flags: float32), format(flags: signed, bits: 16)]
    check(!CaptureInputFormat.needsRestart(started: started, now: started)
            && !CaptureInputFormat.needsRestart(started: started, now: nil),
          "input format: the same formats, or none readable, keep the capture running")
    check(CaptureInputFormat.needsRestart(started: started, now: [format(flags: float32), format(flags: signed, bits: 24)])
            && CaptureInputFormat.needsRestart(started: started,
                                               now: [format(flags: float32), format(flags: signed, bits: 16, channels: 4)])
            && CaptureInputFormat.needsRestart(started: started, now: [format(flags: float32)]),
          "input format: a new bit depth, channel count or stream list restarts the capture")
    var otherRateFormats = started
    otherRateFormats[0].mSampleRate = 44_100
    check(!CaptureInputFormat.needsRestart(started: started, now: otherRateFormats),
          "input format: a rate change is left to the rate listener")

    let storedPairs: [String: Any] = ["a": 1, "b": 2.0, "c": 0, "d": -1, "e": 1.5, "f": "x", "g": Double.nan,
                                      "h": 511, "i": 512]
    check(AppSettings.channelPairs(storedPairs) == ["a": 1, "b": 2, "h": 511] && AppSettings.channelPairs("x").isEmpty
            && InputChannelPair.maxStored == 511,
          "channel pairs: only whole stored pairs from 1 to 511 are read back, as on Windows")
    final class PairStore { var values: [String: Any] = [:] }
    let pairStore = PairStore()
    let pairSettings = SettingsStore(read: { pairStore.values[$0] }, write: { value, key in pairStore.values[key] = value })
    check(pairSettings.load().inputPairs.isEmpty, "channel pairs: a new install reads every device on 1-2")
    pairSettings.save(inputPairs: ["mic-uid": 1])
    let pairsBack = pairSettings.load().inputPairs
    pairSettings.save(inputPairs: [:])
    check(pairsBack == ["mic-uid": 1] && pairStore.values[AppSettings.Keys.inputChannelPairs] == nil,
          "channel pairs: a pair is kept per device UID, and the key goes once all are on 1-2")

    // covers: S10, S11 (the tap follows the app's audio processes)
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

    // covers: S13 (a retap keeps the app muted throughout)
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

    // covers: S216
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
// covers: S213
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
// covers: S213
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
// covers: S213
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
// covers: S222
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
                                     notHeld: 6, timingReplies: 4, foreignDatagrams: 1)
    let pair = NativeSpeakerCounters(packetsSent: 2_380, sessions: 2)
    let status = NativeAirPlayStatus.logLine(latencyMs: 1750.9, stream: stream, speakers: [
        .init(id: "02005E005321", name: "Desk", state: .playing, counters: desk),
        .init(id: "pair:1", name: nil, state: .connecting, counters: pair),
        .init(id: "02005E005322", name: "Gone", state: nil, counters: nil),
    ])
    check(status == """
        [AudioSync] native AirPlay: L = 1750 ms, input rate 44101.3 Hz, 1200 packets encoded, 352 silent frames, 0 input frames dropped
          AirPlay speaker Desk (02005E005321): playing; 1190 packets sent, 2 late, 3 resend requests, 5 resent, 6 not held, 4 timing replies, 1 datagrams from other addresses dropped
          AirPlay speaker pair:1: connecting, 2 members summed; 2380 packets sent, 0 late, 0 resend requests, 0 resent, 0 not held, 0 timing replies, 0 datagrams from other addresses dropped
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
    check(model.joinable(model.rows(enabled: []) + ["native:Z9"], after: ["native:C3"], stopped: false) == ["native:A1"]
            && model.joinable(model.rows(enabled: []), after: ["native:A1"], stopped: false).isEmpty,
          "native airplay: auto-connect may switch on only a new row whose receiver can play",
          "\(model.joinable(model.rows(enabled: []), after: [], stopped: false))")
    check(model.joinable(model.rows(enabled: []), after: [], stopped: true).isEmpty,
          "native airplay: rows rebuilt by a stop are not auto-connected, so the stop doesn't restart streaming")

    // Lounge was paired by hand this run, so it can play until its session ends.
    var lounge = receiver("G7", "Lounge", .available)
    lounge.codeEveryTime = true
    var den = receiver("H8", "Den", .needsPin)
    den.codeEveryTime = true
    var asking = NativeAirPlayModel()
    for r in [lounge, den, receiver("A1", "kitchen", .available), receiver("B2", "Den TV", .needsPin)] {
        asking.apply(.appeared(r))
    }
    let askingRows = asking.rows(enabled: [])
    check(asking.joinable(askingRows, after: [], stopped: false) == ["native:A1"],
          "native airplay: auto-connect never switches on a receiver that asks for a code every time",
          "\(asking.joinable(askingRows, after: [], stopped: false))")
    check(asking.askingEveryTime(askingRows) == ["native:H8", "native:G7"]
            && asking.asksEveryTime("H8") && !asking.asksEveryTime("B2") && !asking.asksEveryTime("A1"),
          "native airplay: a receiver that asks for a code every time goes off whenever it is listed asking and doesn't play, at launch or later",
          "\(asking.askingEveryTime(askingRows))")

    // Lounge was switched on by hand, Den was on from the last run; a sender
    // Retry starts a new model that lists both as new.
    var handOn = EveryTimeGuard()
    handOn.turnedOnByHand(["native:G7"])
    var retried = NativeAirPlayModel()
    for r in [lounge, den] { retried.apply(.appeared(r)) }
    let relisted = retried.askingEveryTime(retried.rows(enabled: []))
    let goOff = handOn.turnsOff(relisted, enabled: ["native:G7", "native:H8"])
    check(goOff == ["native:H8"],
          "native airplay: a speaker that asks every time and was switched on by hand stays on when it appears again, also after a Retry",
          "\(goOff)")
    let afterOff = handOn.turnsOff(["native:G7"], enabled: [])
    let onAgain = handOn.turnsOff(["native:G7"], enabled: ["native:G7"])
    check(afterOff.isEmpty && onAgain == ["native:G7"],
          "native airplay: switched off, it is no longer on by hand", "\(onAgain)")

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
            && model.apply(.error(id: "A1", .airPlay2Only, retryMs: nil, detail: "")) == .turnOff("native:A1", .needsAirPlay2)
            && model.apply(.error(id: "A1", .timingUnavailable, retryMs: nil, detail: "")) == .turnOff("native:A1", .timingUnavailable),
          "native airplay: an error the sender won't retry turns the speaker off, with why")
    model.removed("A1")
    check(model.apply(.state(id: "A1", .playing, retryMs: nil)) == nil
            && model.apply(.error(id: "A1", .notResponding, retryMs: nil, detail: "")) == nil,
          "native airplay: events of a removed speaker are passed over")

    // S222: a receiver powered off mid-stream loses its row about 40 s
    // later, and the sender must stop retrying it then.
    let kitchenOn: Set<String> = ["native:K1"]
    var withdrawn = NativeAirPlayModel()
    withdrawn.apply(.appeared(receiver("K1", "Kitchen", .available)))
    withdrawn.added("K1")
    _ = withdrawn.apply(.state(id: "K1", .playing, retryMs: nil))
    _ = withdrawn.apply(.error(id: "K1", .notResponding, retryMs: 1000, detail: "Unreachable"))
    _ = withdrawn.apply(.state(id: "K1", .retrying, retryMs: 1000))
    let beforeGone = withdrawn.plan(streaming: true, enabled: kitchenOn)
    withdrawn.apply(.gone(id: "K1", name: "Kitchen"))
    let afterGone = withdrawn.plan(streaming: true, enabled: kitchenOn)
    check(beforeGone.isEmpty && afterGone.remove == ["K1"] && afterGone.add.isEmpty && afterGone.turnOff.isEmpty,
          "native airplay: a retrying speaker whose receiver is gone leaves the sender and stays on",
          "before \(beforeGone), after \(afterGone)")
    let switchedOff = withdrawn.plan(streaming: true, enabled: [])
    check(afterGone.gone == ["K1"] && switchedOff.remove == ["K1"] && switchedOff.gone.isEmpty,
          "native airplay: a retrying speaker whose receiver is gone is named by no notice",
          "after \(afterGone), switched off \(switchedOff)")
    withdrawn.removed("K1")
    check(withdrawn.apply(.state(id: "K1", .retrying, retryMs: 1000)) == nil
            && withdrawn.plan(streaming: true, enabled: kitchenOn).isEmpty,
          "native airplay: a speaker gone from the sender waits, on, while its receiver is unlisted")
    withdrawn.apply(.appeared(receiver("K1", "Kitchen", .available)))
    check(withdrawn.plan(streaming: true, enabled: kitchenOn).add == ["K1"],
          "native airplay: a speaker whose receiver was gone joins again when announced able to play")

    var lapsed = NativeAirPlayModel()
    lapsed.apply(.appeared(receiver("K1", "Kitchen", .available)))
    lapsed.added("K1")
    _ = lapsed.apply(.state(id: "K1", .playing, retryMs: nil))
    lapsed.apply(.gone(id: "K1", name: "Kitchen"))
    let playingOn = lapsed.plan(streaming: true, enabled: kitchenOn)
    _ = lapsed.apply(.error(id: "K1", .connectionLost, retryMs: 1000, detail: "Lost"))
    _ = lapsed.apply(.state(id: "K1", .retrying, retryMs: 1000))
    let lost = lapsed.plan(streaming: true, enabled: kitchenOn)
    check(playingOn.isEmpty && lost.remove == ["K1"] && lost.gone == ["K1"] && lost.turnOff.isEmpty,
          "native airplay: a speaker playing on after its announcement lapsed leaves the sender once it is lost",
          "playing \(playingOn), lost \(lost)")

    // P169, S285: a 17th AirPlay 2 speaker waits for a place, switched on,
    // and the sender starts it once one of the others stops.
    model.apply(.appeared(receiver("G7", "Loft", .available)))
    model.added("G7")
    let refused = model.apply(.error(id: "G7", .tooManyAirPlay2, retryMs: nil, detail: "too many AirPlay 2 speakers"))
    check(refused == .waiting("native:G7", .tooManyAirPlay2) && model.apply(.state(id: "G7", .failed, retryMs: nil)) == nil
            && model.isWaiting("G7") && model.plan(streaming: true, enabled: ["native:G7"]).isEmpty,
          "S285 native airplay: a speaker refused an AirPlay 2 place stays on and in the sender", "\(String(describing: refused))")
    check(model.apply(.error(id: "G7", .tooManyAirPlay2, retryMs: nil, detail: "")) == nil && model.isWaiting("G7"),
          "S285 native airplay: refused again while it waits, it posts no second notice")
    check(model.apply(.error(id: "G7", .ptpOtherNetwork, retryMs: nil, detail: "")) == .waiting("native:G7", .ptpOtherNetwork)
            && model.waitReason("G7") == .ptpOtherNetwork,
          "S285 native airplay: a speaker that needs PTP on another network waits and says why")
    check(model.apply(.state(id: "G7", .connecting, retryMs: nil)) == .waiting("native:G7", nil) && !model.isWaiting("G7")
            && model.apply(.state(id: "G7", .playing, retryMs: nil)) == .playingChanged("native:G7", playing: true),
          "S285 native airplay: once the sender lets it, it starts and the row stops saying it waits")
    _ = model.apply(.error(id: "G7", .tooManyAirPlay2, retryMs: nil, detail: ""))
    check(model.apply(.error(id: "G7", .timingUnavailable, retryMs: nil, detail: "")) == .turnOff("native:G7", .timingUnavailable)
            && !model.isWaiting("G7"),
          "P169 native airplay: a waiting speaker that then can't get PTP timing goes off and no longer waits")
    _ = model.apply(.error(id: "G7", .tooManyAirPlay2, retryMs: nil, detail: ""))
    model.removed("G7")
    check(!model.isWaiting("G7"), "P169 native airplay: switched off or streaming stopped, it no longer waits")
    check(UIStrings().nativeAirPlayTooManyAirPlay2(name: "Loft")
            == "AudioSync plays up to \(ASC_AIRPLAY2_SESSIONS) AirPlay 2 speakers at once, so Loft waits. It starts once one of them stops."
            && UIStrings().nativeAirPlayPtpOtherNetwork(name: "Loft")
            == "Loft waits while AudioSync plays to AirPlay 2 speakers on another network. Turn those speakers off and Loft starts.",
          "S285 native airplay: both waiting notices read as on Windows")
    let lets = "Lets AudioSync play to AirPlay 2 speakers, stereo pairs and home theater groups once you quit and reopen it."
    let stops = "AudioSync stops playing to AirPlay 2 speakers, stereo pairs and home theater groups once you quit and reopen it."
    check(UIStrings(airPlay2Auto: false).airPlay2Note(on: false, onAtLaunch: true) == stops
            && UIStrings(airPlay2Auto: false).airPlay2Note(on: true, onAtLaunch: true) == lets
            && UIStrings(airPlay2Auto: false).airPlay2Note(on: true, onAtLaunch: false) == lets
            && UIStrings(airPlay2Auto: false).airPlay2Note(on: false, onAtLaunch: false) == lets,
          "AirPlay 2 switch: its note says AirPlay 2 stops only once turned off from a launch that runs it, as on Windows")
    let preferring = UIStrings(airPlay2Auto: true)
    check(preferring.airPlay2Toggle == "Prefer AirPlay 2"
            && UIStrings(airPlay2Auto: false).airPlay2Toggle == "AirPlay 2 (Experimental)"
            && preferring.airPlay2Note(on: true, onAtLaunch: false)
                == "Speakers that play both AirPlay 1 and AirPlay 2 use AirPlay 2 once you quit and reopen AudioSync."
            && preferring.airPlay2Note(on: false, onAtLaunch: true)
                == "Speakers that play both AirPlay 1 and AirPlay 2 go back to AirPlay 1 once you quit and reopen AudioSync."
            && preferring.helpText.contains("unless 'Prefer AirPlay 2'")
            && !preferring.helpText.contains("(Experimental)"),
          "S313 AirPlay 2 switch: under the AirPlay 2 flag it reads Prefer AirPlay 2, in its note and the Guide too")
    // The menu classes are main-actor bound; top-level code runs on the main thread.
    MainActor.assumeIsolated {
        let noteMenu = NSMenu()
        MenuNoteItem.airPlay2Items(text: UIStrings(airPlay2Auto: false), on: true, onAtLaunch: true,
                                   action: #selector(NSApplication.terminate(_:))).forEach(noteMenu.addItem)
        let noteRow = noteMenu.items.last as? MenuNoteItem
        check(noteMenu.items.count == 2 && noteMenu.items[0].title == "AirPlay 2 (Experimental)"
                && noteMenu.items[0].state == .on && noteMenu.items[0].toolTip == lets
                && noteRow?.note == lets && noteRow?.view != nil && !(noteRow?.isEnabled ?? true),
              "AirPlay 2 switch: its note is a row of its own under it, not a subtitle the menus don't show")
        let oneLine = NSFont.menuFont(ofSize: NSFont.smallSystemFontSize).boundingRectForFont.height
        check((noteRow?.view?.frame.height ?? 0) > oneLine * 1.5,
              "AirPlay 2 switch: its note wraps onto more than one line instead of widening the menu",
              "\(noteRow?.view?.frame.height ?? 0) vs one line \(oneLine)")
        MenuNoteItem.refresh(noteMenu, below: #selector(NSApplication.terminate(_:)), note: stops)
        check(noteRow?.note == stops && noteMenu.items[0].toolTip == stops,
              "AirPlay 2 switch: turned off from a launch that runs it, the note under it says AirPlay 2 stops")
    }
    // The gear menu is held only by the click that opens it, and AppKit can send
    // the chosen item's action after the pop-up call has returned.
    MainActor.assumeIsolated {
        var chosen = 0
        weak var firstTarget: AnyObject?
        var firstAction: Selector?
        autoreleasepool {
            let menu = NSMenu()
            let item = MenuAction.item("Web Remote…") { chosen += 1 }
            menu.addItem(item)
            menu.keepUntilNextPopUp()
            firstTarget = item.target
            firstAction = item.action
        }
        check(firstAction == #selector(MenuAction.choose(_:)), "Gear menu: each item sends its own action to its target",
              firstAction.map(NSStringFromSelector) ?? "none")
        // Its own pool, so the weak load and the call leave nothing autoreleased that holds the target.
        autoreleasepool {
            if let target = firstTarget as? NSObject, let action = firstAction, target.responds(to: action) {
                _ = target.perform(action, with: nil)
            }
        }
        check(chosen == 1,
              "Gear menu: an item chosen after the pop-up call returns still runs its action once nothing else holds the menu")
        autoreleasepool { NSMenu().keepUntilNextPopUp() }
        check(firstTarget == nil, "Gear menu: the next menu that opens releases the one before it")
    }
    check(UIStrings().nativeAirPlayWaitingTitle(name: "Loft") == "Loft waiting"
            && UIStrings().rowNote(cast: nil, waitingForAirPlay2: true, unannounced: false, target: nil) == "Waiting: too many AirPlay 2 speakers"
            && UIStrings().rowNote(cast: nil, waitingForAirPlay2: false, waitingForPtp: true, unannounced: false, target: nil)
            == "Waiting: AirPlay 2 speakers on another network"
            && UIStrings().rowNote(cast: nil, waitingForAirPlay2: false, unannounced: false, target: nil) == nil,
          "S285 native airplay: the notice's title and the row's note for each reason, as on Windows")
    var impostor = NativeAirPlayModel()
    impostor.added("A9")
    check(impostor.apply(.error(id: "A9", .receiverUnverified, retryMs: nil, detail: ""))
                == .turnOff("native:A9", .receiverUnverified)
            && UIStrings().nativeAirPlayReceiverUnverified(name: "Den TV")
                == "Den TV didn't answer as the speaker you paired, so it's off. "
                    + "If it's yours, use Forget Pairing and turn it on to pair again with its PIN.",
          "P56 native airplay: a receiver that fails its signature check goes off with its own notice, as on Windows")
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
        check(m.holdsPassword("C3") && !m.isPaired("C3") && !m.holdsPassword("A1"),
              "native pairing: a receiver whose password was typed offers Forget Password in its row menu")
        var forgot = m
        forgot.forgetPassword("C3")
        check(!forgot.holdsPassword("C3") && forgot.plan(streaming: true, enabled: ["native:C3"]).add.isEmpty,
              "native pairing: after Forget Password the receiver no longer joins with the old password")
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
              "native pairing: a wrong PIN starts a new pairing (the TV shows a PIN again) and asks again")
        check(m.answered(NativePrompt(id: "B2", password: false, wrong: true), code: "5678") == .submitPin
                && m.apply(.pinNeeded(id: "B2")) == .prompt
                && m.stages["B2"] == .askingPin(wrong: true)
                && m.nextPrompt() == NativePrompt(id: "B2", password: false, wrong: true),
              "native pairing: an AirPlay 2 receiver that turns the PIN down asks again in the same pairing")
        check(m.answered(NativePrompt(id: "B2", password: false, wrong: true), code: "4821") == .submitPin
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
        // A receiver that asks for nothing and answers 403 to everything: its
        // refusal comes before any pairing step, so no code is named or asked for.
        var m = NativeAirPlayModel()
        m.apply(.appeared(receiver("S1", "Samsung", .available)))
        m.added("S1")
        let off = m.apply(.error(id: "S1", .pairingRequired, retryMs: nil, detail: ""))
        let words = UIStrings().nativeAirPlayNotAccepted(name: "Samsung")
        check(off == .turnOff("native:S1", .needsCode) && m.refusedUnasked("S1") && m.nextPrompt() == nil
                && !words.contains("PIN") && !words.contains("code") && !words.contains("password"),
              "native pairing: a refusal before any pairing step turns the speaker off with neutral text and no code prompt")
        var asking = NativeAirPlayModel()
        asking.apply(.appeared(receiver("T1", "Den TV", .needsPin)))
        var stored = receiver("P1", "Loft", .available)
        stored.credentialsKept = true
        asking.apply(.appeared(stored))
        check(!asking.refusedUnasked("T1") && !asking.refusedUnasked("P1") && !asking.refusedUnasked("gone"),
              "native pairing: a receiver that asks for a PIN, or holds a pairing, keeps its code text")
        var user = NativeAirPlayModel()
        user.apply(.appeared(receiver("S2", "Samsung", .available)))
        check(user.turnedOnByUser("S2") == nil, "native pairing: a click on a receiver that asks for nothing just joins")
        user.added("S2")
        check(user.apply(.error(id: "S2", .pairingRequired, retryMs: nil, detail: "")) == .startPairing("S2")
                && user.stages["S2"] == .pairing(afterWrongPin: false),
              "native pairing: within the user's attempt, the same refusal from a receiver that asks for nothing pairs with a PIN")
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

        m.removed("B2")
        check(m.refusedOff == ["B2"]
                && m.joinable(["native:B2"], after: [], stopped: false).isEmpty
                && m.plan(streaming: true, enabled: ["native:B2"]).add.isEmpty,
              "native pairing: turned off for a refused pairing, neither auto-connect nor the plan adds it again")
        let recalled = m.plan(streaming: true, enabled: ["native:B2"])
        check(recalled.add.isEmpty && recalled.turnOff == [.init(id: "B2", reason: .needsCode)],
              "native pairing: a refused speaker switched on without a new attempt (a recalled preset) goes off again with a notice")

        // A refused stored pairing pairs again, once per attempt; it stays stored
        // until the new pairing replaces it (P154).
        check(m.turnedOnByUser("B2") == nil && m.refusedOff.isEmpty, "native pairing: a paired Apple TV just joins")
        m.added("B2")
        check(m.apply(.error(id: "B2", .pairingRequired, retryMs: nil, detail: "")) == .startPairing("B2")
                && m.stages["B2"] == .pairing(afterWrongPin: false),
              "native pairing: within the user's attempt, a refused stored pairing pairs again without being forgotten first, though still listed as paired")
        var turnedDown = receiver("B2", "B2", .needsPin)   // the core lists the turned-down pairing as needing its PIN
        turnedDown.credentialsKept = true
        m.apply(.changed(turnedDown))
        check(!m.isPaired("B2") && m.keepsCredentials("B2"),
              "native pairing: a stored pairing it turned down still offers Forget Pairing")
        m.removed("B2")   // it leaves the sender while it pairs
        check(m.plan(streaming: true, enabled: ["native:B2"]).isEmpty,
              "native pairing: while it pairs again, the speaker waits instead of going off")
        check(m.apply(.paired(id: "B2", storeNote: "")) == .paired("native:B2"), "native pairing: paired again")
        m.apply(.changed(receiver("B2", "B2", .available)))
        m.added("B2")
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
    check(NativeSpeakerEvent.state(id: "A1", .unannounced, retryMs: nil).logLine(name: names)
            == "[AudioSync] native AirPlay: Kitchen (A1): unannounced, waiting until it is announced again",
          "P215 native airplay: an unannounced state's log line, as on Windows")
    check(NativeSpeakerEvent.error(id: "Z9", .notResponding, retryMs: nil, detail: "Unreachable").logLine(name: names)
            == "[AudioSync] native AirPlay: Z9 error: not responding (Unreachable); it needs the user",
          "native airplay: an error's log line")
    check(NativeSpeakerEvent.latency(id: "A1", ms: 1750, receiverFrames: 2205).logLine(name: names)
            == "[AudioSync] native AirPlay: Kitchen (A1) latency: L + d_s = 1750 ms, the receiver reports 2205 frames; "
                + "reports nothing"
            && NativeSpeakerEvent.timeline(latencyMs: 1750, restarted: true).logLine(name: names)
            == "[AudioSync] native AirPlay: timeline: L = 1750 ms, restarted",
          "native airplay: latency and timeline log lines")

    // covers: S280, S282, S283 (the Mac host; the Windows tests carry the same names)
    do {
        let rule: ([Int64]) -> UInt32 = { reported in
            reported.withUnsafeBufferPointer { asc_airplay_session_latency_frames($0.baseAddress, $0.count) }
        }
        var known = NativeReceiverLatency()
        known.report("D2", us: 1_300_000)
        let leaving = known.newFrames(over: [], rule: rule)
        let joining = known.newFrames(over: ["A1", "D2"], rule: rule)
        check(leaving == nil && joining == 79_380 && known.newFrames(over: ["A1", "D2"], rule: rule) == nil,
              "airplay latency: is set from known reports before speakers are added",
              "\(String(describing: leaving)) \(String(describing: joining))")

        var follows = NativeReceiverLatency()
        follows.report("A1", us: 200_000)
        let small = follows.newFrames(over: ["A1", "D2"], rule: rule)
        follows.report("D2", us: 1_300_000)
        let large = follows.newFrames(over: ["A1", "D2"], rule: rule)
        let left = follows.newFrames(over: ["A1"], rule: rule)
        check(small == nil && large == 79_380 && left == 77_175,
              "airplay latency: follows a report and a speaker leaving",
              "\(String(describing: small)) \(String(describing: large)) \(String(describing: left))")

        var pair = NativeAirPlayModel()
        pair.apply(.appeared(NativeReceiver(id: "L1", name: "Left", model: "", availability: .available, reason: nil,
                                            hints: [], target: "pair:5f0c")))
        pair.apply(.appeared(NativeReceiver(id: "R1", name: "Right", model: "", availability: .available, reason: nil,
                                            hints: [], target: "pair:5f0c")))
        pair.receiverLatency.report("R1", us: 1_600_000)
        let members = pair.sessions(of: "pair:5f0c")
        check(members == ["L1", "R1"] && pair.receiverLatency.newFrames(over: members, rule: rule) == 88_200
                && pair.receiverLatency.estimateMs(of: members) == 1600,
              "airplay latency: covers every member of a target", "\(members)")

        var estimate = NativeReceiverLatency()
        estimate.report("A1", us: 220_000)
        let one = estimate.updateEstimate(rows: [["A1"]], canChange: true)
        let oneOffset = estimate.offsetMs(of: ["A1"])
        estimate.report("B2", us: 50_000)
        let two = estimate.updateEstimate(rows: [["A1"], ["B2"]], canChange: true)
        check(one == .applied && oneOffset == 0 && two == .unchanged && estimate.estimateMs == 220
                && estimate.offsetMs(of: ["B2"]) == 170,
              "airplay latency: a reported latency is the speaker's starting estimate (one at 220 ms: offset 0, "
                + "L = L_s + 220 + A; 220 and 50 ms: the second gets 170 ms more)")

        var nothing = NativeAirPlayModel()
        let outcome = nothing.apply(.latency(id: "A1", ms: 1750, receiverFrames: nil, reportedUs: 0, sources: 0))
        check(outcome == nil && nothing.receiverLatency.estimateMs(of: ["A1"]) == 0
                && nothing.receiverLatency.newFrames(over: ["A1"], rule: rule) == nil
                && NativeReceiverLatency.reportText(us: 0, sources: 0) == "reports nothing"
                && NativeReceiverLatency.reportText(us: 220_400, sources: 5) == "reports 220 ms (Audio-Latency, arrivalToRender)",
              "airplay latency: a receiver reporting nothing starts at zero without a warning")

        var waits = NativeReceiverLatency()
        waits.report("A1", us: 220_000)
        _ = waits.updateEstimate(rows: [["A1"]], canChange: true)
        waits.report("J9", us: 300_000)
        let held = waits.updateEstimate(rows: [["A1"], ["J9"]], canChange: false)
        let heldAgain = waits.updateEstimate(rows: [["A1"], ["J9"]], canChange: false)
        let late = waits.lateMs(of: ["J9"])
        let joinerOffset = waits.offsetMs(of: ["J9"])
        let applied = waits.updateEstimate(rows: [["A1"], ["J9"]], canChange: true)
        check(held == .held && heldAgain == .unchanged && late == 80 && joinerOffset == 0 && applied == .applied
                && waits.estimateMs == 300 && waits.offsetMs(of: ["A1"]) == 80 && waits.heldMs == nil,
              "airplay latency: the estimate waits while an airplay speaker sounds (a joiner at 300 ms with E = 220 "
                + "plays 80 ms late until the stream is empty)")

        var several = NativeReceiverLatency()
        several.report("L", us: 220_000)
        several.report("R", us: 50_000)
        several.report("D", us: 100_000)
        several.holdTargets(["pair:1": ["L", "R"]], canChange: true)
        _ = several.updateEstimate(rows: [["L", "R"], ["D"]], canChange: true)
        check(several.estimateMs == 220 && several.offsetMs(row: "pair:1", of: ["L", "R"]) == 0
                && several.offsetMs(row: "D", of: ["D"]) == 120,
              "S284 airplay latency: several AirPlay 2 receivers share E, and a target uses its largest member")
        several.holdTargets(["pair:1": ["R"]], canChange: false)
        let oneLeft = several.updateEstimate(rows: [["R"]], canChange: false)
        check(oneLeft == .held && several.estimateMs == 220 && several.offsetMs(row: "pair:1", of: ["R"]) == 0,
              "S284 airplay latency: a receiver leaving while others sound changes no offset")
        several.holdTargets(["pair:1": ["R"]], canChange: true)
        _ = several.updateEstimate(rows: [["R"]], canChange: true)
        check(several.estimateMs == 50 && several.offsetMs(row: "pair:1", of: ["R"]) == 0,
              "S284 airplay latency: the next values come once E may change")
    }

    // covers: S284, S285
    do {
        var group = NativeAirPlayModel()
        for (id, name) in [("L1", "Left"), ("R1", "Right")] {
            group.apply(.appeared(NativeReceiver(id: id, name: name, model: "", availability: .available, reason: nil,
                                                 hints: [], target: "pair:5f0c")))
        }
        group.apply(.appeared(receiver("B2", "Den", .available)))
        group.added("pair:5f0c")
        group.added("B2")
        group.receiverLatency.report("L1", us: 220_000)
        group.receiverLatency.report("R1", us: 50_000)
        group.receiverLatency.report("B2", us: 1_400_000)
        let (rowIDs, rows) = group.latencyRows
        let lf = group.receiverLatency.newFrames(over: Array(rows.joined())) { reported in
            reported.withUnsafeBufferPointer { asc_airplay_session_latency_frames($0.baseAddress, $0.count) }
        }
        let estimate = group.receiverLatency.updateEstimate(rows: rows, canChange: true)
        let latency = group.receiverLatency
        let pair = group.sessions(of: "pair:5f0c")
        let oneRowPerTarget = rowIDs == ["B2", "pair:5f0c"] && rows == [["B2"], ["L1", "R1"]]
        let shared = lf == 83_790 && estimate == .applied && latency.estimateMs == 1400
        let perRow = latency.estimateMs(of: pair) == 220 && latency.offsetMs(of: pair) == 1180
            && latency.offsetMs(of: ["B2"]) == 0
        check(oneRowPerTarget && shared && perRow,
              "airplay latency: several AirPlay 2 receivers share Lf and E, and a target uses its largest member "
                + "(members at 220 and 50 ms: the target's e_s is 220 and both members get the same d′_s)",
              "\(rows) \(String(describing: lf)) \(latency.estimateMs)")

        var leaving = NativeReceiverLatency()
        leaving.report("A1", us: 220_000)
        leaving.report("B2", us: 50_000)
        leaving.report("C3", us: 300_000)
        _ = leaving.updateEstimate(rows: [["A1"], ["B2"], ["C3"]], canChange: true)
        let before = [leaving.offsetMs(of: ["A1"]), leaving.offsetMs(of: ["B2"])]
        // C3 leaves while the others sound, so `NativeAirPlay.remove` passes canChange false.
        let left = leaving.updateEstimate(rows: [["A1"], ["B2"]], canChange: false)
        let during = [leaving.offsetMs(of: ["A1"]), leaving.offsetMs(of: ["B2"])]
        let estimateDuring = leaving.estimateMs
        let restarted = leaving.updateEstimate(rows: [["A1"], ["B2"]], canChange: true)
        check(before == [80, 250] && left == .held && during == before && estimateDuring == 300
                && restarted == .applied && leaving.estimateMs == 220 && leaving.offsetMs(of: ["B2"]) == 170,
              "airplay latency: an AirPlay 2 receiver leaving while others sound changes no delay "
                + "(E and every d′_s held until the stream is empty; the sender holds Lf)",
              "\(before) \(during) \(estimateDuring)")

        // The host holds no AirPlay 2 speaker back itself; the sender refuses the 17th (P204).
        var many = NativeAirPlayModel()
        let ids = (1...17).map { "M\($0)" }
        for id in ids { many.apply(.appeared(receiver(id, id, .available))) }
        let added = many.plan(streaming: true, enabled: Set(ids.map(NativeAirPlayID.app))).add
        var started: [NativeAirPlayOutcome?] = []
        for id in ids {
            many.added(id)
            if id != "M17" { started.append(many.apply(.state(id: id, .playing, retryMs: nil))) }
        }
        let refused = many.apply(.error(id: "M17", .tooManyAirPlay2, retryMs: nil, detail: ""))
        let allPlay = added.count == 17 && started.count == 16 && started.allSatisfy { $0 != nil }
            && ids.dropLast().allSatisfy { many.isPlaying($0) && !many.isWaiting($0) }
        let oneWaits = refused == .waiting("native:M17", .tooManyAirPlay2) && many.isWaiting("M17") && many.states["M17"] != nil
            && many.plan(streaming: true, enabled: Set(ids.map(NativeAirPlayID.app))).isEmpty
        check(allPlay && oneWaits,
              "airplay latency: every AirPlay 2 speaker switched on goes to the sender, and only one it refuses "
                + "waits, switched on and on its own row", "\(added.count) \(String(describing: refused))")

        var failing = NativeAirPlayModel()
        for (id, name) in [("A1", "Kitchen"), ("B2", "Den"), ("C3", "Porch"), ("D4", "Loft")] {
            failing.apply(.appeared(receiver(id, name, .available)))
            _ = failing.turnedOnByUser(id)
            failing.added(id)
        }
        _ = failing.apply(.state(id: "A1", .playing, retryMs: nil))
        let password = failing.apply(.error(id: "B2", .passwordRequired, retryMs: nil, detail: ""))
        let pin = failing.apply(.error(id: "C3", .pairingRequired, retryMs: nil, detail: ""))
        let gone = failing.apply(.error(id: "D4", .notResponding, retryMs: nil, detail: ""))
        // D4 went off, so its switch is off when the host plans again.
        let stillOn = Set(["A1", "B2", "C3"].map(NativeAirPlayID.app))
        let replan = failing.plan(streaming: true, enabled: stillOn)
        let prompt = failing.nextPrompt()
        let ownRows = password == .prompt && pin == .startPairing("C3")
            && gone == .turnOff("native:D4", NativeSpeakerEvent.Problem.notResponding.turnOff) && prompt?.id == "B2"
        check(ownRows && failing.isPlaying("A1") && replan.remove == ["D4"] && replan.add.isEmpty
                && replan.turnOff.isEmpty,
              "airplay latency: one AirPlay 2 speaker failing leaves the others playing",
              "\(String(describing: password)) \(String(describing: pin)) \(String(describing: gone)) \(replan)")
    }

    var volumes = NativeAirPlayModel()
    volumes.apply(.appeared(receiver("A1", "Kitchen", .available)))
    check(volumes.apply(.volume(id: "A1", position: 80, level: 50.4)) == nil,
          "native airplay: a volume from a speaker the app didn't add moves no slider")
    volumes.added("A1")
    check(volumes.apply(.volume(id: "A1", position: 80, level: 50.4)) == .volumeChanged("native:A1", 80, 50.4),
          "native airplay: a volume changed on the speaker moves its slider")
    check(volumes.apply(.warning(id: "A1", ptpPortsTaken: true, detail: "PtpUnavailable")) == .ptpPortsTaken
            && volumes.apply(.warning(id: "A1", ptpPortsTaken: false, detail: "JackDisconnected")) == nil,
          "native airplay: of the warnings, only a PTP port clash becomes a notice")
    check(NativeSpeakerEvent.volume(id: "A1", position: 79.6, level: 50.4).logLine(name: names)
            == "[AudioSync] native AirPlay: Kitchen (A1) volume changed on the speaker: device level 50.4, own volume 80"
            && NativeSpeakerEvent.warning(id: "Z9", ptpPortsTaken: false, detail: "JackDisconnected").logLine(name: names)
            == "[AudioSync] native AirPlay: Z9 warning: JackDisconnected",
          "native airplay: volume and warning log lines")

    check(!PtpPortNotice.shown(portsTaken: false, played: false, timingRows: 2)
            && PtpPortNotice.shown(portsTaken: true, played: false, timingRows: 0)
            && !PtpPortNotice.shown(portsTaken: true, played: true, timingRows: 0)
            && PtpPortNotice.shown(portsTaken: true, played: true, timingRows: 1),
          "ptp notice: shows once the ports are reported taken, until a speaker plays while no row needs PTP timing")
    var ptp = PtpPortNotice()
    ptp.turnedOff("native:H1", timing: true)
    check(!ptp.shown, "ptp notice: a speaker off for PTP timing with no port clash reported says nothing about ports")
    ptp.reported()
    ptp.playing("native:N1")
    check(ptp.shown, "ptp notice: an NTP speaker that plays leaves it while a PTP speaker is off for timing")
    ptp.turnedOff("native:H1", timing: false)
    check(!ptp.shown, "ptp notice: it clears once no row is off for PTP timing")
    ptp.turnedOff("native:H2", timing: true)
    check(ptp.shown, "ptp notice: it comes back when another speaker goes off for PTP timing, though reported once")
    ptp.playing("native:H2")
    check(!ptp.shown, "ptp notice: the PTP speaker playing clears it")
    ptp.turnedOff("native:H1", timing: true)
    ptp.listed(["native:N1"])
    check(!ptp.shown && ptp.timingOff.isEmpty, "ptp notice: a row that leaves the list takes its timing reason along")

    check(volumes.apply(.ptpFree(id: "A1")) == .ptpPortsFree
            && NativeSpeakerEvent.ptpFree(id: "A1").logLine(name: names)
            == "[AudioSync] native AirPlay: Kitchen (A1) bound the PTP ports, which are free again",
          "native airplay: a PTP bind after the clash says the ports are free")
    var episode = PtpPortNotice()
    episode.reported()
    check(episode.shown && episode.raises == 1, "ptp notice: the report raises it")
    episode.playing("native:N1")
    check(!episode.shown, "ptp notice: an NTP speaker that plays clears it while no row is off for PTP timing")
    episode.turnedOff("native:H1", timing: true)
    check(episode.shown && episode.raises == 2,
          "ptp notice: a PTP speaker refused while the ports are taken brings it back")
    episode.playing("native:N2")
    check(episode.shown && episode.raises == 2,
          "ptp notice: a non-PTP speaker that starts playing while the ports stay taken leaves it shown")
    episode.turnedOff("native:H1", timing: true)
    check(episode.raises == 2, "ptp notice: the same speaker refused again in one episode raises nothing")
    episode.turnedOff("native:H2", timing: true)
    check(episode.raises == 3, "ptp notice: another speaker refused in the episode raises it again")
    episode.portsFree()
    check(!episode.shown, "ptp notice: a PTP bind ends the episode and clears it")
    episode.turnedOff("native:H1", timing: true)
    check(!episode.shown && episode.raises == 3, "ptp notice: a refusal with no clash reported raises nothing")
    episode.reported()
    episode.turnedOff("native:H1", timing: true)
    check(episode.shown && episode.raises == 5,
          "ptp notice: a new report starts a new episode, in which the same speaker raises it again")
}

// MARK: Speakers that ask for a code every time: off unless switched on by hand; an open PIN prompt is never restarted
do {
    func tv(_ id: String, _ availability: NativeReceiver.Availability, everyTime: Bool) -> NativeReceiver {
        var receiver = NativeReceiver(id: id, name: id, model: "", availability: availability, reason: nil, hints: [])
        receiver.codeEveryTime = everyTime
        return receiver
    }

    // Listed before its every-time flag is known, on from the last run.
    var late = NativeAirPlayModel()
    late.apply(.appeared(tv("T1", .needsPin, everyTime: false)))
    late.apply(.changed(tv("T1", .needsPin, everyTime: true)))
    var restored = EveryTimeGuard()
    let lateOff = restored.turnsOff(late.askingEveryTime(late.rows(enabled: ["native:T1"])),
                                    enabled: ["native:T1"])
    check(lateOff == ["native:T1"],
          "every time: a speaker on from the last run goes off once it asks for a code every time, also when listed before that",
          "\(lateOff)")

    // The TV setting changes while it plays: the session under way keeps playing.
    var playing = NativeAirPlayModel()
    playing.apply(.appeared(tv("T2", .available, everyTime: false)))
    playing.added("T2")
    _ = playing.apply(.state(id: "T2", .playing, retryMs: nil))
    playing.apply(.changed(tv("T2", .available, everyTime: true)))
    check(playing.askingEveryTime(playing.rows(enabled: ["native:T2"])).isEmpty,
          "every time: a speaker that plays when it starts asking for a code every time keeps playing")

    // Switched on by hand: it pairs and its PIN prompt opens.
    var pin = NativeAirPlayModel()
    pin.apply(.appeared(tv("P1", .needsPin, everyTime: true)))
    let started = pin.turnedOnByUser("P1")
    let asked = pin.apply(.pinNeeded(id: "P1"))
    let prompt = pin.nextPrompt()
    check(started == .startPairing("P1") && asked == .prompt && prompt != nil,
          "every time: switched on by hand, it pairs and asks for the PIN")
    // Neither a reconcile nor the 30 s safety net starts a session for it
    // while the prompt is open, even when it is listed as able to play.
    pin.apply(.changed(tv("P1", .available, everyTime: true)))
    let during = pin.plan(streaming: true, enabled: ["native:P1"])
    check(during.add.isEmpty, "every time: no session starts for a speaker whose PIN prompt is open", "\(during)")
    // A session that was in the sender anyway and is refused starts no
    // second pairing: a new /pair-pin-start makes the TV show a new code.
    pin.added("P1")
    let refused = pin.apply(.error(id: "P1", .pairingRequired, retryMs: nil, detail: ""))
    check(refused == nil && pin.withdrawPrompt() == nil,
          "every time: a refusal while the PIN prompt is open starts no second pairing and keeps the prompt",
          "\(String(describing: refused))")
    let retried = pin.apply(.error(id: "P1", .pairingRequired, retryMs: 1000, detail: ""))
    check(retried == nil && pin.withdrawPrompt() == nil,
          "every time: a refusal the sender retries while the PIN prompt is open starts no second pairing and keeps the prompt",
          "\(String(describing: retried))")
}

// MARK: Every time at launch: on from the last run and discovered, it stays off with no pairing for 15 s
do {
    func tv(_ availability: NativeReceiver.Availability, everyTime: Bool) -> NativeReceiver {
        var receiver = NativeReceiver(id: "TV", name: "TV", model: "", availability: availability, reason: nil, hints: [])
        receiver.codeEveryTime = everyTime
        return receiver
    }
    enum Step { case listed(NativeReceiver), sender(NativeSpeakerEvent) }
    // The app's steps in its order (AppDelegate+NativeAirPlay.swift
    // refreshNativeRows, adoptNativeRows, syncNativeSpeakers, nativeOutcome,
    // turnOffNativeSpeaker; AppDelegate+Zones.swift autoConnect), streaming
    // from launch. No pairings are counted: auto-connect switches rows on
    // without beginNativeAttempts, and the sender starts no PIN pairing by
    // itself (the every-time tests in airplay/tests/ap2_driver.rs), so a
    // session is the only way to one.
    // Copied from AppDelegate: when that order changes, change it here too.
    struct App {
        var model = NativeAirPlayModel()
        var everyTime = EveryTimeGuard()
        var enabled: Set<String> = ["native:TV"]   // enabledAirPlayIds from the last run
        let marked: Set<String> = ["native:TV"]   // auto-connect on
        var order: [String] = []
        var sessions: [Int] = []
        var now = 0

        mutating func refreshNativeRows() {
            let joinable = adoptNativeRows()
            syncNativeSpeakers()
            autoConnect(joinable)
        }
        mutating func adoptNativeRows() -> [String] {
            let adoption = model.adopt(enabled: enabled, known: order, stopped: false, everyTime: &everyTime)
            for id in adoption.turnOff { enabled.remove(id) }
            order = adoption.rows
            return adoption.joinable
        }
        mutating func syncNativeSpeakers() {
            let plan = model.plan(streaming: true, enabled: enabled)
            guard !plan.isEmpty else { return }
            for id in plan.release { enabled.remove(NativeAirPlayID.app(id)) }
            for id in plan.remove { model.removed(id) }
            let appeared = plan.changesRows ? adoptNativeRows() : []
            for id in plan.add {
                model.added(id)
                sessions.append(now)
            }
            for off in plan.turnOff { turnOffNativeSpeaker(NativeAirPlayID.app(off.id)) }
            autoConnect(appeared)
        }
        mutating func turnOffNativeSpeaker(_ id: String) {
            guard enabled.contains(id) else { return }
            enabled.remove(id)
            syncNativeSpeakers()
        }
        mutating func autoConnect(_ ids: [String]) {
            let hits = ids.filter { marked.contains($0) }
            guard !hits.isEmpty else { return }
            enabled.formUnion(hits)
            syncNativeSpeakers()   // applyAirPlaySelection while streaming
        }
        mutating func nativeOutcome(_ outcome: NativeAirPlayOutcome?) {
            switch outcome {
            case .turnOff(let id, _)?: turnOffNativeSpeaker(id)
            case .playingChanged?: refreshNativeRows()
            default: break
            }
        }
    }
    // Each launch: what reaches the app at which second, the second the
    // every-time flag is known, and the sessions that may start before it
    // (the receiver was listed as open).
    let launches: [(name: String, steps: [(Int, Step)], flagAt: Int, sessions: Int)] = [
        ("listed asking", [(2, .listed(tv(.needsPin, everyTime: true)))], 2, 0),
        ("open first, flag on a later listing", [(2, .listed(tv(.available, everyTime: false))),
                                                 (10, .listed(tv(.available, everyTime: true)))], 10, 1),
        ("listed able to play and asking", [(2, .listed(tv(.available, everyTime: true)))], 2, 0),
        ("open over AirPlay 1, then asking over AirPlay 2", [(2, .listed(tv(.available, everyTime: false))),
                                                             (3, .sender(.state(id: "TV", .retrying, retryMs: 10_000))),
                                                             (3, .listed(tv(.needsPin, everyTime: true)))], 3, 1),
    ]
    for launch in launches {
        var app = App()
        var listedYet = false
        var inSenderAfterFlag: [Int] = []
        for t in 0...15 {
            app.now = t
            for (at, step) in launch.steps where at == t {
                switch step {
                case .listed(let receiver):   // nativeReceiverChanged
                    app.model.apply(listedYet ? .changed(receiver) : .appeared(receiver))
                    listedYet = true
                    app.refreshNativeRows()
                case .sender(let event):   // nativeSpeakerEvent
                    let outcome = app.model.apply(event)
                    app.nativeOutcome(outcome)
                }
            }
            // reconcileNativeAirPlay, here every second (the app: at launch, on wake, every 30 s).
            app.syncNativeSpeakers()
            if t >= launch.flagAt, app.model.states["TV"] != nil { inSenderAfterFlag.append(t) }
        }
        check(app.sessions.count == launch.sessions && app.sessions.allSatisfy { $0 < launch.flagAt }
                && inSenderAfterFlag.isEmpty && !app.enabled.contains("native:TV")
                && app.model.plan(streaming: true, enabled: app.enabled).add.isEmpty,
              "every time at launch (\(launch.name)): on from the last run, auto-connect on, it goes off once it asks and no session starts for 15 s",
              "sessions \(app.sessions), in the sender \(inSenderAfterFlag), on \(app.enabled)")
    }
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
    check(t.rowNote(cast: nil, waitingForAirPlay2: false, unannounced: false, target: row) == "Stereo pair: Left and Right"
            && t.airPlayTargetNote(AirPlayTargetRow(kind: .homeTheater, members: ["Left", "Right", "Sub"], missing: [],
                                                    incomplete: false)) == "Home theater group: Left, Right and Sub"
            && t.airPlayTargetNote(AirPlayTargetRow(kind: .stereoPair, members: [], missing: [], incomplete: false))
                == "Stereo pair",
          "P176 native airplay: the row names the kind and its members, as a Cast group's row does")
    let note = t.rowNote(cast: nil, waitingForAirPlay2: false, unannounced: false, target: row) ?? ""
    check(t.rowHint(SpeakerState(enabled: false, streaming: false), note: note).hasPrefix(note),
          "P176 native airplay: VoiceOver reads the pair's note in the row's hint")
    check(t.rowNote(cast: nil, waitingForAirPlay2: true, unannounced: false, target: row) == t.waitingForAirPlay2,
          "P169 native airplay: a target waiting for the AirPlay 2 slot says so first")

    let on: Set<String> = [app(pair), app(left)]
    check(model.plan(streaming: true, enabled: on)
            == NativeAirPlayPlan(add: [pair], remove: [], turnOff: [], release: [left]),
          "P176 native airplay: the pair is added as one speaker; a member that was on by itself is switched off for it")
    model.added(left)
    check(model.plan(streaming: true, enabled: on).remove == [left],
          "P176 native airplay: a member playing by itself leaves the sender when its pair is switched on")
    model.removed(left)
    var unlisted = NativeAirPlayModel()
    unlisted.added(pair)
    let pairOn: Set<String> = [app(pair)]
    check(unlisted.rows(enabled: pairOn) == [app(pair)] && unlisted.plan(streaming: true, enabled: pairOn).isEmpty,
          "P176 native airplay: a target in the sender with no listing keeps its row")
    let off = unlisted.plan(streaming: true, enabled: [])
    unlisted.removed(pair)
    check(off.remove == [pair] && off.changesRows && unlisted.rows(enabled: []).isEmpty
            && unlisted.apply(.state(id: pair, .stopped, retryMs: nil)) == nil,
          "P176 native airplay: switched off, an unlisted target's row goes when it is removed, since its Stopped event "
            + "is passed over", "\(unlisted.rows(enabled: []))")
    check(!NativeAirPlayPlan(add: ["K3"], remove: [], turnOff: []).changesRows,
          "native airplay: a plan that only adds speakers leaves the rows to the sender's events")
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
    check(playing.rows(enabled: [app(pair)]).contains(app(pair))
            && playing.plan(streaming: true, enabled: [app(pair)]).remove.isEmpty,
          "P176 native airplay: a gone pair still in the sender keeps its row while it reconnects")
    playing.removed(pair)
    check(playing.holdingTarget(left, on: false) == nil && playing.rows(enabled: []).contains(app(left))
            && !playing.rows(enabled: []).contains(app(pair)),
          "P176 native airplay: switched off, the gone pair's row goes and its members are speakers again")

    check(NativeSpeakerEvent.State(code: ASC_AIRPLAY_STATE_CONNECTING) == .connecting
            && NativeSpeakerEvent.State(code: ASC_AIRPLAY_STATE_PLAYING) == .playing
            && NativeSpeakerEvent.State(code: ASC_AIRPLAY_STATE_RETRYING) == .retrying
            && NativeSpeakerEvent.State(code: ASC_AIRPLAY_STATE_FAILED) == .failed
            && NativeSpeakerEvent.State(code: ASC_AIRPLAY_STATE_STOPPED) == .stopped
            && NativeSpeakerEvent.State(code: ASC_AIRPLAY_STATE_UNANNOUNCED) == .unannounced
            && NativeSpeakerEvent.State(code: 99) == nil,
          "P215 native airplay: every sender state code has its state, Unannounced included; an unknown one has none")
    var lapsed = listedPair()
    lapsed.added(pair)
    _ = lapsed.apply(.state(id: pair, .playing, retryMs: nil))
    check(lapsed.apply(.state(id: pair, .unannounced, retryMs: nil)) == .playingChanged(app(pair), playing: false)
            && lapsed.isUnannounced(pair) && lapsed.rows(enabled: [app(pair)]).contains(app(pair)),
          "P215 native airplay: a playing pair whose members lapse stops, keeps its row and reads as off the network")
    check(lapsed.apply(.state(id: pair, .connecting, retryMs: nil)) == .unannouncedChanged(app(pair))
            && !lapsed.isUnannounced(pair)
            && lapsed.apply(.state(id: pair, .unannounced, retryMs: nil)) == .unannouncedChanged(app(pair))
            && lapsed.apply(.state(id: pair, .unannounced, retryMs: nil)) == nil,
          "P215 native airplay: going off the network or coming back without playing refreshes the row once")
    let away = t.rowNote(cast: nil, waitingForAirPlay2: false, unannounced: true, target: row)
    check(away == "Not on the network right now, plays once it's back" && away == t.notOnNetwork
            && t.rowHint(SpeakerState(enabled: true, streaming: true), note: away).hasPrefix(t.notOnNetwork)
            && t.rowNote(cast: nil, waitingForAirPlay2: true, unannounced: true, target: row) == t.waitingForAirPlay2,
          "P215 native airplay: an unannounced row says it is off the network, also to VoiceOver; the AirPlay 2 wait says so first")

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

    // covers: S325 (a Bluetooth speaker that starts on a link that just came up restarts once)
    do {
        let links = FreshLinkWatch()
        links.noteListing(["builtin", "bt-old"], now: 100)
        check(links.decide(uid: "bt-old", bluetooth: true, now: 100.5, session: 1) == .settled(linkAgeS: nil),
              "EC2 Bluetooth fresh link: a device listed at launch starts as is")
        links.noteListing(["builtin", "bt-old", "bt", "usb"], now: 200)
        check(links.decide(uid: "usb", bluetooth: false, now: 200.5, session: 1) == .notBluetooth,
              "EC2 Bluetooth fresh link: a wired device that just appeared starts as is")
        check(links.decide(uid: "bt", bluetooth: true, now: 200.5, session: 1) == .restart(linkAgeS: 0.5),
              "EC2 Bluetooth fresh link: one that starts 0.5 s after it appeared restarts once")
        let ticket = links.pendingTicket(uid: "bt")
        check(ticket != nil && links.begin(uid: "bt", ticket: ticket ?? -1, session: 1) && links.held == ["bt"],
              "EC2 Bluetooth fresh link: the restart holds it off until its old output has faded")
        check(!links.begin(uid: "bt", ticket: ticket ?? -1, session: 1),
              "EC2 Bluetooth fresh link: a ticket restarts it only once")
        links.release(uid: "bt")
        check(links.held.isEmpty && links.decide(uid: "bt", bluetooth: true, now: 203, session: 1) == .settled(linkAgeS: nil),
              "EC2 Bluetooth fresh link: its start after the restart plays as is")
        links.noteListing(["builtin", "bt-old", "usb"], now: 300)
        links.noteListing(["builtin", "bt-old", "usb", "bt"], now: 400)
        check(links.decide(uid: "bt", bluetooth: true, now: 404, session: 1) == .settled(linkAgeS: 4),
              "EC2 Bluetooth fresh link: one that starts 4 s after it came back has a settled link")
        links.noteListing(["builtin", "bt-old", "usb"], now: 500)
        links.noteListing(["builtin", "bt-old", "usb", "bt"], now: 600)
        check(links.decide(uid: "bt", bluetooth: true, now: 601, session: 1) == .restart(linkAgeS: 1),
              "EC2 Bluetooth fresh link: every reconnect counts as a fresh link again")
        let dropped = links.pendingTicket(uid: "bt")
        links.cancel(uid: "bt")
        check(dropped != nil && !links.begin(uid: "bt", ticket: dropped ?? -1, session: 1) && links.held.isEmpty,
              "EC2 Bluetooth fresh link: switched off before its restart, it is not restarted")
        links.noteListing(["builtin", "bt-old", "usb"], now: 700)
        links.noteListing(["builtin", "bt-old", "usb", "bt"], now: 800)
        check(links.decide(uid: "bt", bluetooth: true, now: 800.5, session: 1) == .restart(linkAgeS: 0.5),
              "EC2 Bluetooth fresh link: a start in one session arms a restart")
        let stale = links.pendingTicket(uid: "bt")
        check(stale != nil && !links.begin(uid: "bt", ticket: stale ?? -1, session: 2) && links.held.isEmpty
                && links.pendingTicket(uid: "bt") == nil,
              "EC2 Bluetooth fresh link: a restart armed in a session that has ended is dropped")
    }

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
    let repick = SourceSwitch.decide(from: music, to: music, streaming: true)
    check(SourceSwitch.noticesCleared(by: repick, streaming: true) == [Notice.source],
          "EC5 source: picking the playing source again clears the failed switch notice")
    check(SourceSwitch.noticesCleared(by: InputChannelPair.change(from: 1, to: 1, streaming: true), streaming: true)
            == [Notice.source],
          "EC5 source: picking the playing channel pair again clears the failed switch notice")
    check(SourceSwitch.noticesCleared(by: SourceSwitch.decide(from: music, to: music, streaming: false), streaming: false)
            .isEmpty,
          "EC5 source: picking the stopped source again keeps the notice of why it stopped")
    check(SourceSwitch.noticesCleared(by: SourceSwitch.decide(from: .system, to: music, streaming: true), streaming: true)
            == [Notice.source, Notice.capture],
          "EC5 source: picking another source clears the old source's notices")
    do {
        let tv = SpeakerKey.make(id: "tv", airplay: true)
        var list = NoticeList()
        list.post("pin", subject: tv)
        list.post("sender", subject: Notice.airplaySender)
        list.post("ptp", subject: Notice.airplayPtp)
        list.post("still on", subject: Notice.source)
        list.clear(SourceSwitch.noticesCleared(by: repick, streaming: true))
        check(list.items.map(\.subject) == [Notice.airplayPtp, Notice.airplaySender, tv],
              "EC5 source: picking the playing source again leaves the AirPlay pairing and sender notices")
    }

    // EC5: a switch while streaming keeps the speakers connected (S141, S14).
    let toMusic = CaptureSwitch.Change.source(from: .system, to: music)
    let toPair = CaptureSwitch.Change.inputPair(uid: "mic", from: 0, to: 1)
    let switchName: (AudioSource) -> String = { $0 == .system ? "All System Audio" : "Music" }
    let switchPair: (String, Int) -> String = { _, index in index == 0 ? "Channels 1-2" : "Channels 3-4" }
    check(CaptureSwitch.outcome(failure: nil, oldRunning: false) == .switched,
          "EC5 source: a switch whose new capture starts plays it")
    check(CaptureSwitch.logLine(.switched, kept: 3, fromRate: 48000, toRate: 48000)
            == "[AudioSync] source switch: 3 AirPlay or Cast speaker(s) stayed connected, the capture runs at 48000 Hz",
          "EC5 source: a switch keeps the speakers connected and says how many",
          CaptureSwitch.logLine(.switched, kept: 3, fromRate: 48000, toRate: 48000))
    check(CaptureSwitch.logLine(.switched, kept: 2, fromRate: 44100, toRate: 48000)
            == "[AudioSync] source switch: 2 AirPlay or Cast speaker(s) stayed connected, the capture went from 44100 to 48000 Hz",
          "EC5 source: a switch from a 44.1 kHz input to a 48 kHz app keeps the speakers at the new rate",
          CaptureSwitch.logLine(.switched, kept: 2, fromRate: 44100, toRate: 48000))
    check(CaptureSwitch.outcome(failure: .startFailed, oldRunning: true) == .keptOld(.startFailed),
          "EC5 source: a new source that can't start gives way to the old one")
    check(CaptureSwitch.outcome(failure: .microphoneDenied, oldRunning: true) == .keptOld(.microphoneDenied),
          "EC5 source: a denied microphone keeps the old source playing")
    check(CaptureSwitch.outcome(failure: .startFailed, oldRunning: false) == .stopped,
          "EC5 source: streaming stops only when the old source can't start again either")
    check(CaptureSwitch.message(.keptOld(.startFailed), change: toMusic, sourceName: switchName, pairName: switchPair)
            == .stillOnOld(new: "Music", old: "All System Audio"),
          "EC5 source: a failed switch names the new source and the one still playing")
    check(CaptureSwitch.message(.keptOld(.startFailed), change: toPair, sourceName: switchName, pairName: switchPair)
            == .stillOnOld(new: "Channels 3-4", old: "Channels 1-2"),
          "EC5 source: a failed channel pair switch names both pairs")
    check(CaptureSwitch.message(.keptOld(.microphoneDenied), change: toPair, sourceName: switchName,
                                pairName: switchPair) == .microphoneDenied,
          "EC5 source: a denied microphone says where to allow it")
    check(CaptureSwitch.message(.switched, change: toMusic, sourceName: switchName, pairName: switchPair) == .none
            && CaptureSwitch.message(.stopped, change: toMusic, sourceName: switchName, pairName: switchPair) == .none,
          "EC5 source: a switch that works, or a stop with its own start notice, adds no switch notice")
    check(CaptureSwitch.logLine(.keptOld(.startFailed), kept: 1, fromRate: 48000, toRate: 48000)
            == "[AudioSync] source switch failed (the new capture didn't start): the old source plays on, "
            + "1 AirPlay or Cast speaker(s) stayed connected",
          "EC5 source: a failed switch logs that the speakers stayed",
          CaptureSwitch.logLine(.keptOld(.startFailed), kept: 1, fromRate: 48000, toRate: 48000))
    check(UIStrings().sourceSwitchFailed(new: "Music", old: "All System Audio")
            == "Couldn't start Music, still on All System Audio.",
          "EC5 source: the failed switch notice names both sources plainly")
    check(CaptureSwitch.start(starts: true, microphone: .notNeeded) == .rebuildCapture
            && CaptureSwitch.start(starts: true, microphone: .allowed) == .rebuildCapture,
          "EC5 source: a switch rebuilds only the capture and keeps the session")
    check(CaptureSwitch.start(starts: false, microphone: .notNeeded) == .stopSession
            && CaptureSwitch.start(starts: false, microphone: .undetermined) == .stopSession,
          "EC5 source: a switch the start rule refuses (no speaker on any more) stops the session")
    check(CaptureSwitch.start(starts: true, microphone: .undetermined) == .askMicrophone,
          "EC5 source: a switch to an input macOS still has to allow keeps the old source until the answer")
    check(CaptureSwitch.start(starts: true, microphone: .denied) == .keepOld,
          "EC5 source: a switch to an input the microphone permission denies keeps the old capture")
    let mic = AudioSource.input(uid: "mic")
    let toMic = CaptureSwitch.Change.source(from: music, to: mic)
    let backToMusic = toMic.oldCapture { $0 == "mic" ? 1 : 0 }
    let fromMic = CaptureSwitch.Change.source(from: mic, to: music).oldCapture { $0 == "mic" ? 1 : 0 }
    let backToPair = toPair.oldCapture { _ in 1 }
    check(backToMusic?.source == music && backToMusic?.pair == 0 && fromMic?.source == mic && fromMic?.pair == 1
            && backToPair?.source == mic && backToPair?.pair == 0,
          "EC5 source: a failed switch starts the old source on its own stored pair, or the old pair",
          String(describing: (backToMusic, fromMic, backToPair)))
    check(CaptureSwitch.answer(granted: true, to: toMic, source: music, pair: 0, running: true) == .switchNow,
          "EC5 source: an allowed microphone switches to the input under the same speakers")
    check(CaptureSwitch.answer(granted: false, to: toMic, source: music, pair: 0, running: true) == .keepOld,
          "EC5 source: a microphone denied at the prompt keeps the old source playing")
    check(CaptureSwitch.answer(granted: true, to: toMic, source: .system, pair: 0, running: true) == .none
            && CaptureSwitch.answer(granted: false, to: toMic, source: .system, pair: 0, running: true) == .none,
          "EC5 source: an answer after the user picked another source changes nothing")
    check(CaptureSwitch.answer(granted: true, to: toMic, source: music, pair: 0, running: false) == .remember
            && CaptureSwitch.answer(granted: false, to: toMic, source: music, pair: 0, running: false) == .none,
          "EC5 source: an answer after streaming stopped only stores an allowed input")
    check(CaptureSwitch.answer(granted: true, to: toPair, source: mic, pair: 0, running: true) == .switchNow
            && CaptureSwitch.answer(granted: true, to: toPair, source: mic, pair: 2, running: true) == .none,
          "EC5 source: a pair switch waits on the prompt only while the old pair is still stored")
    check(toMusic.stop == .sourceChanged && toPair.stop == .inputPairChanged,
          "EC5 source: the log names a source change and a pair change apart")

    // S227 Switch: the source app quitting moves the session to the whole
    // system the way a switch does, under the same speakers.
    let musicQuits = CaptureSwitch.Change.sourceAppQuit(bundleID: "com.apple.Music")
    let quitSwitched = CaptureSwitch.logLine(.switched, change: musicQuits, kept: 2, fromRate: 44100, toRate: 48000)
    check(quitSwitched == "[AudioSync] source app quit (com.apple.Music), the whole system plays: "
            + "2 AirPlay or Cast speaker(s) stayed connected, the capture went from 44100 to 48000 Hz",
          "EC6 app quit: switching to the whole system keeps the AirPlay and Cast speakers connected (S227)",
          quitSwitched)
    check(musicQuits.stop == .sourceSwitchedToSystem && musicQuits.stop.restarts,
          "EC6 app quit: the switch to the whole system rebuilds only the capture and logs why (S227)")
    let quitStopped = CaptureSwitch.logLine(.stopped, change: musicQuits, kept: 0, fromRate: 48000, toRate: 48000)
    let quitStopLine: String = "[AudioSync] source app quit (com.apple.Music) and the whole system didn't start: "
        + "streaming stops"
    let quitNoNotice = CaptureSwitch.message(.stopped, change: musicQuits, sourceName: switchName,
                                             pairName: switchPair) == .none
    check(musicQuits.oldCapture { _ in 0 } == nil && quitStopped == quitStopLine && quitNoNotice,
          "EC6 app quit: a whole system that can't start stops streaming, with no app to go back to (S227)",
          quitStopped)
    check(CaptureSwitch.answer(granted: true, to: musicQuits, source: .system, pair: 0, running: true) == .none,
          "EC6 app quit: the switch to the whole system never waits on the microphone prompt (S227)")

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
          "EC8 all off: an automatic start with no speaker on asks for one first")

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

    // covers: S154, S167 (an empty list with Local Network allowed never hints)
    check(LocalNetworkHint.decide(access: .allowed, senderUp: true, emptySince: t0,
                                  now: t0.addingTimeInterval(600)) == .none,
          "EC12 network loss: an empty list with Local Network allowed shows no permission hint")
}

// MARK: Track info — artwork scaled to a large and a small JPEG
// covers: S217, S272
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
    let big = artwork(1600, 1200)
    let pair = TrackMetadata.artwork(big)
    let (lw, lh) = size(pair?.large) ?? (0, 0)
    let (sw, sh) = size(pair?.small) ?? (0, 0)
    let large = TrackMetadata.largeArtwork, small = TrackMetadata.smallArtwork
    check(large == .init(side: 1024, bytes: 131_072) && small == .init(side: 600, bytes: 40_960),
          "artwork: the limits are 1024 px and 128 KiB, 600 px and 40 KiB")
    check(pair.map { $0.large.starts(with: [0xFF, 0xD8]) && $0.large.count <= large.bytes } == true
            && lw > small.side && lw <= large.side && lh <= large.side && abs(Double(lw) / Double(lh) - 4.0 / 3) < 0.05,
          "artwork: the large image is at most 1024 px a side and 128 KiB",
          "\(pair?.large.count ?? -1) bytes, \(lw)×\(lh)")
    check(pair.map { $0.small.starts(with: [0xFF, 0xD8]) && $0.small.count <= small.bytes } == true
            && sw > 0 && sw <= small.side && sh <= small.side && abs(Double(sw) / Double(sh) - 4.0 / 3) < 0.05,
          "artwork: the small image is at most 600 px a side and 40 KiB",
          "\(pair?.small.count ?? -1) bytes, \(sw)×\(sh)")
    let modest = TrackMetadata.artwork(artwork(300, 200))
    check(modest.map { $0.large == $0.small } == true && size(modest?.small).map { $0 == (300, 200) } == true,
          "artwork: an image within the small limits is one encoding for both")
    let jpeg = TrackMetadata.jpeg(big, maxSide: small.side, maxBytes: small.bytes)
    let w = size(jpeg)?.0 ?? 0
    let tight = TrackMetadata.jpeg(big, maxSide: small.side, maxBytes: 2500)
    check(tight.map { $0.count <= 2500 } == true && (size(tight)?.0 ?? .max) < w,
          "artwork: a tighter limit gives a smaller image", "\(tight?.count ?? -1) bytes")
    check(TrackMetadata.jpeg(big, maxSide: small.side, maxBytes: 100) == nil,
          "artwork: nothing when not even the smallest image fits")
    check(size(TrackMetadata.artwork(artwork(100, 100))?.large).map { $0 == (100, 100) } == true,
          "artwork: a small image isn't scaled up")
    check(TrackMetadata.artwork(NSImage(cgImage: big, size: NSSize(width: 1600, height: 1200))) != nil,
          "artwork: an NSImage (a player's artwork, AudioSync's icon) is encoded too")
}

// MARK: Track info — a player with no track is not an error
check(NowPlayingMonitor.meansNoTrack(-1728) && NowPlayingMonitor.meansNoTrack(-1712),
      "AppleScript: no current track (-1728) and a timeout (-1712) read as no track, not logged")
check(!NowPlayingMonitor.meansNoTrack(-1743) && !NowPlayingMonitor.meansNoTrack(nil)
      && !NowPlayingMonitor.meansNoTrack(-2753),
      "AppleScript: Automation refused (-1743) and other errors are still logged")

// MARK: Track info — a paused player keeps its track on receivers until it stops or quits
do {
    let spotify = NowPlayingMonitor.spotify
    let source = AudioSource.app(bundleID: spotify)
    let untitled = TrackMetadata.Fields(title: "AudioSync", artist: "Spotify", album: "From your Mac")
    let paused = NowPlayingMonitor.Track(bundleID: spotify, appName: "Spotify", title: "Song", artist: "Band",
                                         album: "Record", isPlaying: false,
                                         artwork: NSImage(size: NSSize(width: 1, height: 1)))
    func shows(_ t: NowPlayingMonitor.Track?) -> ReceiverInfo {
        ReceiverInfo.decide(track: t, source: source, sourceName: "Spotify", untitled: untitled)
    }
    var empty = NowPlayingMonitor.EmptyReadings()
    var shown = empty.next(reading: paused, last: nil, lastPlayerRuns: true)
    shown = empty.next(reading: nil, last: shown, lastPlayerRuns: true)
    check(shows(shown).fields.title == "Song" && shows(shown).artwork == .track,
          "now playing: one empty reading of a paused player keeps its track and cover, not the fallback")
    for _ in 0..<300 { shown = empty.next(reading: nil, last: shown, lastPlayerRuns: true) }
    check(shows(shown).fields.title == "Song" && shows(shown).artwork == .track,
          "now playing: a paused player that still runs keeps its track and cover through 300 unreadable readings")
    check(empty.dropped == nil, "now playing: keeping a paused track through unreadable readings logs no drop")
    shown = empty.next(reading: nil, last: shown, lastPlayerRuns: true, lastPlayer: .stopped)
    check(shown == nil && shows(shown).fields == untitled && empty.dropped == .stopped,
          "now playing: an explicit stopped reading drops a paused track at once")
    let playing = NowPlayingMonitor.Track(bundleID: spotify, appName: "Spotify", title: "Tune", artist: "Band",
                                          album: "Record", isPlaying: true,
                                          artwork: NSImage(size: NSSize(width: 1, height: 1)))
    let dropAfter = NowPlayingMonitor.EmptyReadings.dropAfter
    var stalled = NowPlayingMonitor.EmptyReadings()
    shown = stalled.next(reading: playing, last: nil, lastPlayerRuns: true)
    shown = stalled.next(reading: nil, last: shown, lastPlayerRuns: true)
    shown = stalled.next(reading: playing, last: shown, lastPlayerRuns: true)
    for _ in 1..<dropAfter { shown = stalled.next(reading: nil, last: shown, lastPlayerRuns: true) }
    check(shows(shown).fields.title == "Tune",
          "now playing: a real reading in between starts the count of empty readings after a playing track again")
    shown = stalled.next(reading: nil, last: shown, lastPlayerRuns: true)
    check(shown == nil && shows(shown).fields == untitled && shows(shown).artwork == .neutral
            && stalled.dropped == .unreadable(dropAfter),
          "now playing: \(dropAfter) empty readings in a row after a playing track send the fallback")
    var quit = NowPlayingMonitor.EmptyReadings()
    shown = quit.next(reading: nil, last: paused, lastPlayerRuns: false)
    check(shown == nil && shows(shown).fields == untitled && quit.dropped == .quit,
          "now playing: a player that quit sends the fallback at the first empty reading")
    var nameless = NowPlayingMonitor.EmptyReadings()
    shown = nameless.next(reading: paused, last: nil, lastPlayerRuns: true)
    shown = nameless.next(reading: nil, last: shown, lastPlayerRuns: true, lastPlayer: .playingNameless)
    shown = nameless.next(reading: nil, last: shown, lastPlayerRuns: true)
    for _ in 2..<dropAfter {
        shown = nameless.next(reading: nil, last: shown, lastPlayerRuns: true, lastPlayer: .playingNameless)
    }
    check(shows(shown).fields.title == "Song",
          "now playing: a paused track stays through fewer than \(dropAfter) readings that play without a name")
    shown = nameless.next(reading: nil, last: shown, lastPlayerRuns: true, lastPlayer: .playingNameless)
    check(shown == nil && shows(shown).fields == untitled && nameless.dropped == .playingNameless(dropAfter),
          "now playing: \(dropAfter) readings that play something without a name drop a paused track")
    var denied = NowPlayingMonitor.EmptyReadings()
    shown = denied.next(reading: nil, last: paused, lastPlayerRuns: true, lastPlayer: .denied)
    check(shown == nil && shows(shown).fields == untitled && denied.dropped == .denied,
          "now playing: a player that refuses Apple Events drops a paused track at once")
}

// MARK: Track info — with the whole system as source, the player that played last keeps its paused track
do {
    typealias Track = NowPlayingMonitor.Track
    let spotify = NowPlayingMonitor.spotify, music = NowPlayingMonitor.music
    let cover = NSImage(size: NSSize(width: 1, height: 1))
    func track(_ app: String, _ title: String, playing: Bool) -> Track {
        Track(bundleID: app, appName: "Player", title: title, artist: "Band", album: "Record",
              isPlaying: playing, artwork: cover)
    }
    let untitled = TrackMetadata.Fields(title: "AudioSync", artist: "All System Audio", album: "From your Mac")
    var played = NowPlayingMonitor.LastPlayed()
    func shows(_ t: Track?) -> ReceiverInfo {
        ReceiverInfo.decide(track: t, source: .system, sourceName: "All System Audio", untitled: untitled,
                            lastPlayed: played.bundleID)
    }
    played.note(source: .system)
    let spotifyPlays = track(spotify, "Song", playing: true)
    let spotifyPaused = track(spotify, "Song", playing: false)
    played.note(shown: spotifyPlays, last: nil, dropped: false)
    played.note(shown: spotifyPaused, last: spotifyPlays, dropped: false)
    check(shows(spotifyPaused).fields.title == "Song" && shows(spotifyPaused).artwork == .track,
          "receiver info: system source, paused Spotify, no other player: its track and cover stay")
    let musicPaused = track(music, "Old", playing: false)
    check(NowPlayingMonitor.pausedPick([musicPaused, spotifyPaused], lastPlayed: played.bundleID) == spotifyPaused
            && NowPlayingMonitor.pausedPick([musicPaused, spotifyPaused], lastPlayed: nil) == musicPaused,
          "now playing: with no player playing, the paused player that played last is followed")
    check(shows(musicPaused).fields == untitled,
          "receiver info: system source: a paused player that didn't play last doesn't show")
    let musicPlays = track(music, "Tune", playing: true)
    played.note(shown: musicPlays, last: spotifyPaused, dropped: false)
    check(shows(musicPlays).fields.title == "Tune" && shows(spotifyPaused).fields == untitled,
          "receiver info: system source, paused Spotify, then Music plays: Music wins")
    played.note(source: .app(bundleID: "com.google.Chrome"))
    played.note(source: .system)
    check(played.bundleID == nil && shows(track(music, "Tune", playing: false)).fields == untitled,
          "receiver info: the source changes: a paused track goes to the fallback")
    played.note(shown: spotifyPlays, last: nil, dropped: false)
    played.note(shown: nil, last: spotifyPaused, dropped: true)
    check(played.bundleID == nil && shows(spotifyPaused).fields == untitled,
          "receiver info: system source: the player that played last stops or quits: the fallback")
}

// MARK: Explicit pause of the AirPlay speakers (P213, ExplicitPause in NowPlaying.swift)
// covers: S292 (the Mac host)
do {
    let spotify = NowPlayingMonitor.spotify, music = NowPlayingMonitor.music
    let t0 = Date(timeIntervalSinceReferenceDate: 0)
    let later = t0.addingTimeInterval(ExplicitPause.staleReading)
    func pausedAt(_ t: Date) -> ExplicitPause {
        var pause = ExplicitPause()
        _ = pause.toggled(app: spotify, playing: true, at: t)
        return pause
    }

    var pause = ExplicitPause()
    check(pause.toggled(app: spotify, playing: true, at: t0) == .pause && pause.isPaused,
          "explicit pause: a play-pause to a player read as playing pauses the speakers")
    check(pause.toggled(app: spotify, playing: false, at: t0) == .resume && !pause.isPaused,
          "explicit pause: a play-pause while paused resumes")
    for playing in [true, nil] as [Bool?] {
        var toggled = pausedAt(t0)
        check(toggled.toggled(app: spotify, playing: playing, at: t0) == .resume,
              "explicit pause: a play-pause while paused resumes whatever the reading says",
              String(describing: playing))
    }
    var unknown = ExplicitPause()
    check(unknown.toggled(app: spotify, playing: nil, at: t0) == .nothing
            && unknown.toggled(app: nil, playing: nil, at: t0) == .nothing
            && unknown.toggled(app: spotify, playing: false, at: t0) == .nothing && !unknown.isPaused,
          "explicit pause: a play-pause to a player read as paused, or not read, sends no flush")

    var own = ExplicitPause()
    check(own.read(app: spotify, playing: true, at: t0, sending: true) == .nothing
            && own.read(app: spotify, playing: false, at: t0, sending: true) == .pause && own.isPaused,
          "explicit pause: the player going from playing to paused pauses the speakers")
    check(own.read(app: spotify, playing: false, at: later.addingTimeInterval(10), sending: true) == .nothing
            && own.isPaused,
          "explicit pause: the player read as paused again does nothing")
    var ownResume = ExplicitPause()
    _ = ownResume.read(app: spotify, playing: true, at: t0, sending: true)
    _ = ownResume.read(app: spotify, playing: false, at: t0, sending: true)
    check(ownResume.read(app: spotify, playing: true, at: t0.addingTimeInterval(0.5), sending: true) == .resume
            && !ownResume.isPaused,
          "explicit pause: the player going from paused to playing resumes at once")

    var first = ExplicitPause()
    check(first.read(app: spotify, playing: false, at: t0, sending: true) == .nothing,
          "explicit pause: the first reading is no transition")
    _ = first.read(app: spotify, playing: true, at: t0, sending: true)
    check(first.read(app: nil, playing: nil, at: t0, sending: true) == .nothing
            && first.read(app: spotify, playing: nil, at: t0, sending: true) == .nothing
            && first.read(app: spotify, playing: false, at: t0, sending: true) == .nothing && !first.isPaused,
          "explicit pause: an unknown state or a player that quits never pauses")

    var switched = ExplicitPause()
    _ = switched.read(app: spotify, playing: true, at: t0, sending: true)
    check(switched.read(app: music, playing: false, at: t0, sending: true) == .nothing,
          "explicit pause: a switch to another player that was paused is no transition")
    _ = switched.read(app: music, playing: true, at: t0, sending: true)
    check(switched.read(app: music, playing: false, at: t0, sending: true) == .pause,
          "explicit pause: the active player's own pause counts after a switch")

    check(ExplicitPause.playerState("playing") == true && ExplicitPause.playerState("paused") == false,
          "explicit pause: a player reads as playing or paused from its scripted state")
    for state in ["fast forwarding", "rewinding", "stopped", ""] {
        check(ExplicitPause.playerState(state) == nil,
              "explicit pause: seeking or any other scripted state is no known state", state)
    }

    var restarted = ExplicitPause()
    _ = restarted.read(app: spotify, playing: true, at: t0, sending: true)
    _ = restarted.readingsStopped()
    check(restarted.read(app: spotify, playing: false, at: t0, sending: true) == .nothing && !restarted.isPaused,
          "explicit pause: the first reading after the readings start again is no transition")

    var idle = ExplicitPause()
    _ = idle.read(app: spotify, playing: true, at: t0, sending: false)
    check(idle.read(app: spotify, playing: false, at: t0, sending: false) == .nothing && !idle.isPaused,
          "explicit pause: a player pausing while no speaker plays never pauses")

    var ownHeard = ExplicitPause()
    _ = ownHeard.read(app: spotify, playing: true, at: t0, sending: true)
    _ = ownHeard.read(app: spotify, playing: false, at: t0, sending: true)
    check(ownHeard.heard(sound: 1.0, at: later) == .nothing && ownHeard.heard(sound: 1.25, at: later) == .resume,
          "explicit pause: sound still resumes a pause the player took")

    var stale = pausedAt(t0)
    let soon = t0.addingTimeInterval(ExplicitPause.staleReading - 0.01)
    check(stale.read(app: spotify, playing: true, at: soon, sending: true) == .nothing
            && stale.read(app: nil, playing: nil, at: soon, sending: true) == .nothing && stale.isPaused,
          "explicit pause: a reading that may predate the player's own pause keeps the pause")
    check(stale.read(app: spotify, playing: false, at: later, sending: true) == .nothing && stale.isPaused,
          "explicit pause: the player read as paused keeps the pause")
    check(stale.read(app: spotify, playing: true, at: later, sending: true) == .resume && !stale.isPaused,
          "explicit pause: the player read as playing again resumes")
    var other = pausedAt(t0)
    check(other.read(app: music, playing: false, at: later, sending: true) == .resume,
          "explicit pause: another player counting resumes")
    var none = pausedAt(t0)
    check(none.read(app: spotify, playing: nil, at: later, sending: true) == .resume,
          "explicit pause: no reading of the player resumes")

    var restart = pausedAt(t0)
    check(restart.clear() == .resume && restart.clear() == .nothing,
          "explicit pause: a new capture or stopped readings resume a paused stream once")

    func loud(_ samples: [Float]) -> Int {
        samples.withUnsafeBufferPointer { NativeAirPlayTap.loudFrames($0.baseAddress!, frames: samples.count / 2) }
    }
    check(loud([0, 0, 0, 0]) == 0 && loud([3e-5, -3e-5, 2e-5, -1e-5]) == 0,
          "explicit pause: digital silence and dither are no sound")
    check(loud([0.002, 0, 0, -0.002, 0, 0]) == 2 && loud([0.5, 0.5, -0.5, -0.5]) == 2,
          "explicit pause: a frame with a sample above -60 dBFS is sound")

    var heard = pausedAt(t0)
    check(heard.heard(sound: 1.0, at: t0.addingTimeInterval(1)) == .nothing
            && heard.heard(sound: 1.0, at: later) == .nothing
            && heard.heard(sound: 1.15, at: later) == .nothing && heard.isPaused,
          "explicit pause: sound before the stale-reading time, or under 200 ms after it, keeps the pause")
    check(heard.heard(sound: 1.25, at: later) == .resume && !heard.isPaused,
          "explicit pause: 200 ms of sound after the stale-reading time resumes, whatever the reading says")
    var playing = ExplicitPause()
    check(playing.heard(sound: 60, at: later) == .nothing && !playing.isPaused,
          "explicit pause: sound never pauses")
}

// MARK: Track info — what AirPlay receivers with a screen show
// covers: S218
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
          "receiver info: whole system as source: a paused track whose player didn't play last doesn't show")
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
// covers: S219
do {
    let song = TrackMetadata.Fields(title: "Song", artist: "Band", album: "Record")
    let cover = TrackMetadata.Artwork(large: Data([0xFF, 0xD8, 0xFF, 0x01]), small: Data([0xFF, 0xD8, 0xFF, 0x03]))
    let nextCover = TrackMetadata.Artwork(large: Data([0xFF, 0xD8, 0xFF, 0x02]), small: Data([0xFF, 0xD8, 0xFF, 0x02]))
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
// covers: S219
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

// MARK: Track progress for receivers that show it: a jump is news, running on isn't (P126)
do {
    typealias Position = NowPlayingMonitor.Position
    let start = Date(timeIntervalSinceReferenceDate: 0), uptime: TimeInterval = 1000   // readings use systemUptime
    let at30 = Position(elapsed: 30, length: 200, readAt: uptime, playing: true)
    check(at30.elapsed(at: uptime + 10) == 40 && at30.elapsed(at: uptime + 500) == 200,
          "progress: a playing track runs on from its reading, up to its length")
    let paused = Position(elapsed: 30, length: 200, readAt: uptime, playing: false)
    check(paused.elapsed(at: uptime + 10) == 30, "progress: a paused track stays where it was read")
    check(!Position.jumped(from: at30, to: Position(elapsed: 32.5, length: 200, readAt: uptime + 2, playing: true))
            && !Position.jumped(from: nil, to: nil),
          "progress: a reading where the track ran on to is no news")
    check(Position.jumped(from: at30, to: Position(elapsed: 90, length: 200, readAt: uptime + 2, playing: true))
            && Position.jumped(from: at30, to: Position(elapsed: 5, length: 200, readAt: uptime + 2, playing: true)),
          "progress: a seek forward or back is news")
    check(Position.jumped(from: at30, to: Position(elapsed: 32, length: 180, readAt: uptime + 2, playing: true))
            && Position.jumped(from: at30, to: nil) && Position.jumped(from: nil, to: at30),
          "progress: another length, or a position coming or going, is news")

    let song = TrackMetadata.Fields(title: "Song", artist: "Band", album: "Record")
    let next = TrackMetadata.Fields(title: "Next", artist: "Band", album: "Record")
    let spotify = NowPlayingMonitor.spotify
    let track = NowPlayingMonitor.Track(bundleID: spotify, appName: "Spotify", title: "Song", artist: "Band",
                                        album: "Record", isPlaying: false, artwork: nil, position: paused)
    let untitled = TrackMetadata.Fields(title: "AudioSync", artist: "Live audio", album: "From your Mac")
    check(ReceiverInfo.decide(track: track, source: .app(bundleID: spotify), sourceName: "Spotify",
                              untitled: untitled).position == paused
            && ReceiverInfo.decide(track: track, source: .system, sourceName: nil, untitled: untitled).position == nil,
          "progress: only a track that shows takes its position to the receivers")

    var sent = SentTrackInfo()
    func shows(_ fields: TrackMetadata.Fields, _ position: Position?) -> ReceiverInfo {
        ReceiverInfo(fields: fields, artwork: .neutral, position: position)
    }
    var u = sent.update(shows(song, at30), artwork: nil, icon: nil, now: start)
    check(u.sendsPosition && u.position == at30, "progress: a new track's position goes out with it")
    u = sent.update(shows(song, at30), artwork: nil, icon: nil, now: start + 1)
    check(!u.sendsPosition, "progress: the same reading again gives the sender nothing")
    u = sent.update(shows(song, paused), artwork: nil, icon: nil, now: start + 2)
    check(u.sendsPosition && u.position == paused, "progress: a pause or a seek goes out")
    u = sent.update(shows(next, nil), artwork: nil, icon: nil, now: start + 3)
    check(u.fields == next && !u.sendsPosition,
          "progress: a new track without a length sends none: the new track dropped it in the core")
    u = sent.update(shows(song, paused), artwork: nil, icon: nil, now: start + 4)
    check(u.fields == song && u.sendsPosition && u.position == paused,
          "progress: back to a track whose reading went out before: the new track dropped it, so it goes again")
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

// MARK: Track info wiring (TrackInfoHost): a Now Playing change and a sender start
do {
    final class Host: TrackInfoHost {
        var trackInfoFeed = TrackInfoFeed()
        var sendsTrackInfo = true
        var nowPlayingPollingSince: Date?
        var nowPlayingHasReading = true
        let trackInfoIcon: NSImage? = NSImage(size: NSSize(width: 1, height: 1))
        let cover = NSImage(size: NSSize(width: 1, height: 1))
        var track: NowPlayingMonitor.Track?
        var shown: [NowPlayingMonitor.Track?] = []
        var looks = 0, playbacks = 0, castLooks = 0
        var rechecks: [Date] = []
        var sent: [(fields: TrackMetadata.Fields?, artwork: NSImage?)] = []

        func showNowPlaying(_ track: NowPlayingMonitor.Track?) {
            shown.append(track)
            self.track = track
        }
        func receiverShows() -> (info: ReceiverInfo, artwork: NSImage?) {
            looks += 1
            let fields = track.map { TrackMetadata.Fields(title: $0.title, artist: $0.artist, album: $0.album) }
                ?? TrackMetadata.Fields(title: "AudioSync", artist: "", album: "")
            return (ReceiverInfo(fields: fields, artwork: .track), track == nil ? nil : cover)
        }
        func sendPlayback() { playbacks += 1 }
        func sendCastTrackInfo(now: Date) { castLooks += 1 }
        func recheckTrackInfo(at date: Date) { rechecks.append(date) }
        func setSenderTrackInfo(fields: TrackMetadata.Fields?, artwork: NSImage?, sendsPosition: Bool,
                                position: NowPlayingMonitor.Position?) {
            sent.append((fields, artwork))
        }
    }
    func song(_ title: String) -> NowPlayingMonitor.Track {
        NowPlayingMonitor.Track(bundleID: "com.example.player", appName: "Player", title: title, artist: "Band",
                                album: "Record", isPlaying: true, artwork: nil)
    }
    let first = TrackMetadata.Fields(title: "First", artist: "Band", album: "Record")
    let second = TrackMetadata.Fields(title: "Second", artist: "Band", album: "Record")

    let host = Host()
    _ = host.trackInfoFeed.modelUpdated(sending: true)
    host.nowPlayingChanged(song("First"))
    check(host.shown.count == 1 && host.shown.first??.title == "First",
          "track info wiring: a Now Playing change puts the track in the window's model")
    check(host.sent.count == 1 && host.sent[0].fields == first,
          "track info wiring: a Now Playing change sends the current track to the sender, once",
          "\(host.sent.map { $0.fields?.title ?? "nil" })")
    check(host.sent.first?.artwork != nil && host.sent.first?.artwork !== host.cover,
          "track info wiring: the sender gets a copy of the artwork, since it encodes it on its own queue")
    check(host.playbacks == 1 && host.castLooks == 1,
          "track info wiring: each look also tells the remote's player and gives Cast its look")

    host.nowPlayingChanged(song("First"))
    check(host.sent.count == 1 && host.playbacks == 2,
          "track info wiring: the same track again sends nothing new to the sender")
    host.nowPlayingChanged(song("Second"))
    check(host.sent.count == 2 && host.sent[1].fields == second,
          "track info wiring: the next track goes to the sender once, with its own fields")

    host.resendTrackInfo()
    check(host.sent.count == 3 && host.sent[2].fields == second && host.sent[2].artwork != nil,
          "track info wiring: a sender start sends the current track and artwork again, unchanged and once")
    host.nowPlayingChanged(song("Second"))
    check(host.sent.count == 3, "track info wiring: after the resend the same track sends nothing more")

    let idle = Host()
    idle.sendsTrackInfo = false
    _ = idle.trackInfoFeed.modelUpdated(sending: false)
    idle.nowPlayingChanged(song("First"))
    check(idle.sent.isEmpty && idle.looks == 0 && idle.shown.count == 1,
          "track info wiring: with no AirPlay speaker showing track info, a change reaches the window only")
    idle.resendTrackInfo()
    check(idle.sent.isEmpty && idle.looks == 0 && idle.playbacks == 1 && idle.trackInfoFeed.sent == nil,
          "track info wiring: a sender start with no AirPlay speaker showing track info sends nothing")

    let waiting = Host()
    let polling = Date()
    waiting.nowPlayingPollingSince = polling
    waiting.nowPlayingHasReading = false
    _ = waiting.trackInfoFeed.modelUpdated(sending: true)
    waiting.resendTrackInfo()
    check(waiting.sent.isEmpty && waiting.looks == 0
            && waiting.rechecks == [polling.addingTimeInterval(ReceiverInfo.readingWait)],
          "track info wiring: a sender start before Now Playing's first reading schedules one look, sends nothing")
}

// MARK: Track info — Spotify paused, read as empty every 2 s for 11 minutes, never sends the fallback
do {
    let spotify = NowPlayingMonitor.spotify
    let cover = NSImage(size: NSSize(width: 1, height: 1))
    let icon = NSImage(size: NSSize(width: 2, height: 2))
    func track(playing: Bool) -> NowPlayingMonitor.Track {
        NowPlayingMonitor.Track(bundleID: spotify, appName: "Spotify", title: "Song", artist: "Band",
                                album: "Record", isPlaying: playing, artwork: cover)
    }
    let start = Date(timeIntervalSinceReferenceDate: 800_000_000)
    let poll: TimeInterval = 2
    let emptyReadings = Int(11 * 60 / poll)
    let sources: [(AudioSource, String)] = [(.app(bundleID: spotify), "Spotify"), (.system, "All System Audio")]
    for (source, sourceName) in sources {
        let untitled = TrackMetadata.Fields(title: "AudioSync", artist: sourceName, album: "From your Mac")
        var empty = NowPlayingMonitor.EmptyReadings()
        var played = NowPlayingMonitor.LastPlayed()
        played.note(source: source)
        var feed = TrackInfoFeed()
        _ = feed.modelUpdated(sending: true)
        var last: NowPlayingMonitor.Track?
        var fallbackAt: [TimeInterval] = []
        // One Now Playing reading at `now`, as NowPlayingMonitor.refresh and
        // TrackInfoHost.sendTrackInfo take it; a paused Spotify with no track
        // name answers as unreadable.
        func read(_ reading: NowPlayingMonitor.Track?, at now: Date) {
            let shown = empty.next(reading: reading, last: last, lastPlayerRuns: true, lastPlayer: .unreadable)
            played.note(shown: shown, last: last, dropped: empty.dropped != nil)
            last = shown
            let info = ReceiverInfo.decide(track: shown, source: source, sourceName: sourceName,
                                           untitled: untitled, lastPlayed: played.bundleID)
            let step = feed.send(sending: true, now: now, pollingSince: start, hasReading: true, icon: icon) {
                (info, shown?.artwork)
            }
            if info.fields == untitled || info.artwork != .track || step.fields == untitled || step.picture === icon {
                fallbackAt.append(now.timeIntervalSince(start))
            }
        }
        read(track(playing: true), at: start)
        read(track(playing: false), at: start + poll)
        for n in 0..<emptyReadings { read(nil, at: start + poll * Double(n + 2)) }
        check(fallbackAt.isEmpty && last?.title == "Song" && empty.dropped == nil
                && feed.sent?.fields == TrackMetadata.Fields(title: "Song", artist: "Band", album: "Record"),
              "track info (\(sourceName)): Spotify paused and read as empty every 2 s for 11 minutes keeps its track and cover, never the fallback",
              "fallback at \(fallbackAt.prefix(3)) s, shown \(String(describing: last?.title))")
    }
}

// MARK: Windows and the view model (WindowRules.swift)
// covers: S211, S212, S214
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
    let inputs = [SourceList.Input(uid: "mic-1", name: "MacBook Pro Microphone", transport: kAudioDeviceTransportTypeBuiltIn),
                  SourceList.Input(uid: "", name: "No UID", transport: kAudioDeviceTransportTypeUSB)]
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

    // A Bluetooth headset's hands-free input switches it to call mode, so the
    // picker leaves it out; a Bluetooth LE, USB or built-in input stays.
    let headsetInputs = [
        SourceList.Input(uid: "mic-1", name: "MacBook Pro Microphone", transport: kAudioDeviceTransportTypeBuiltIn),
        SourceList.Input(uid: "buds:input", name: "Example Buds", transport: kAudioDeviceTransportTypeBluetooth),
        SourceList.Input(uid: "hearing:input", name: "Example LE Mic", transport: kAudioDeviceTransportTypeBluetoothLE),
        SourceList.Input(uid: "usb-mic", name: "USB Mic", transport: kAudioDeviceTransportTypeUSB),
        // An aggregate that records from a hands-free input opens it too.
        SourceList.Input(uid: "agg-buds", name: "Buds Aggregate", transport: kAudioDeviceTransportTypeAggregate,
                         inputSubTransports: [kAudioDeviceTransportTypeBuiltIn, kAudioDeviceTransportTypeBluetooth]),
        SourceList.Input(uid: "agg-wired", name: "Wired Aggregate", transport: kAudioDeviceTransportTypeAggregate,
                         inputSubTransports: [kAudioDeviceTransportTypeBuiltIn, kAudioDeviceTransportTypeUSB]),
    ]
    func headsetList(_ chosen: AudioSource) -> [SourceList.Entry] {
        SourceList.entries(systemName: "All System Audio", apps: [], inputs: headsetInputs, chosen: chosen,
                           chosenAppName: { $0 })
    }
    check(headsetList(.system).map(\.id) == ["", "input:mic-1", "input:hearing:input", "input:usb-mic", "input:agg-wired"],
          "sources: a Bluetooth hands-free input, or an aggregate with one, is left out; "
            + "Bluetooth LE, built-in, USB and other aggregate inputs stay",
          "\(headsetList(.system).map(\.id))")
    check(headsetList(.input(uid: "buds:input")).map(\.id) == headsetList(.system).map(\.id),
          "sources: a saved source that is a hands-free input isn't listed, like an unplugged one")
    check(AudioDevices.isHandsFreeInput(transport: kAudioDeviceTransportTypeBluetooth)
            && !AudioDevices.isHandsFreeInput(transport: kAudioDeviceTransportTypeBluetoothLE)
            && !AudioDevices.isHandsFreeInput(transport: kAudioDeviceTransportTypeUSB)
            && !AudioDevices.isHandsFreeInput(transport: kAudioDeviceTransportTypeBuiltIn)
            && !AudioDevices.isHandsFreeInput(transport: 0)
            && AudioDevices.isHandsFreeInput(transport: kAudioDeviceTransportTypeAggregate,
                                             inputSubTransports: [kAudioDeviceTransportTypeBluetooth])
            && AudioDevices.isHandsFreeInput(transport: kAudioDeviceTransportTypeAutoAggregate,
                                             inputSubTransports: [kAudioDeviceTransportTypeUSB, kAudioDeviceTransportTypeBluetooth])
            && !AudioDevices.isHandsFreeInput(transport: kAudioDeviceTransportTypeAggregate,
                                              inputSubTransports: [kAudioDeviceTransportTypeBluetoothLE, kAudioDeviceTransportTypeUSB])
            && !AudioDevices.isHandsFreeInput(transport: kAudioDeviceTransportTypeAggregate),
          "sources: a classic Bluetooth input, or an aggregate recording from one, counts as hands-free")
    let inputLog = SourceList.logLines(headsetInputs)
    check(inputLog.count == 6 && inputLog[1].contains("transport blue: left out: a Bluetooth hands-free input")
            && inputLog[4].contains("transport grup (sub-device inputs bltn, blue): left out")
            && inputLog[0].hasSuffix("transport bltn: listed") && inputLog[3].hasSuffix("transport usb : listed"),
          "sources: the debug log names each input's transport and whether it was left out", "\(inputLog)")
    check(AudioDevices.transportCode(0) == "0",
          "sources: a transport that isn't four printable characters logs as a number")
    // A saved hands-free source doesn't start, and says why rather than "not connected".
    let handsFreeStart = UIStrings().streamingStartMessage(for: SourceCapture.CaptureError.inputHandsFree("buds:input"))
    let handsFreeStop = UIStrings().sourceStopped(.inputHandsFree("buds:input"))
    check(handsFreeStart.contains("hands-free microphone") && handsFreeStart.hasSuffix("Pick another source.")
            && !handsFreeStart.contains("not connected") && handsFreeStop == handsFreeStart
            && handsFreeStart != UIStrings().streamingStartMessage(for: SourceCapture.CaptureError.inputNotFound("buds:input"))
            && !handsFreeStart.contains("\u{2014}"),
          "capture: a saved source that is a hands-free input has its own reason", handsFreeStart)

    let zoneText = UIStrings()
    func zoneCount(on: Int, waiting: Int = 0, present: Int, total: Int) -> String {
        ZoneLayout.count(on: on, waiting: waiting, present: present, total: total, text: zoneText)
    }
    check(zoneCount(on: 0, present: 0, total: 0) == "No speakers",
          "zones: a zone whose members are all hidden or gone says it has no speakers")
    check(zoneCount(on: 0, present: 0, total: 3) == "None of 3 found",
          "zones: a zone with no member listed says none of its members was found")
    check(zoneCount(on: 2, present: 3, total: 3) == "2 of 3 on",
          "zones: the header counts the members that are on out of every shown member")
    check(zoneCount(on: 2, waiting: 1, present: 2, total: 3) == "2 of 3 on, 1 waiting",
          "zones: a member waiting for the AirPlay 2 place counts as on and is named as waiting")

    let zoned: Set<String> = ["lc:usb", "ap:native:den"]
    check(ZoneLayout.zoneless(["lc:built-in", "lc:usb", "ap:native:den", "cc:kitchen"], key: { $0 }, zoned: zoned)
            == ["lc:built-in", "cc:kitchen"],
          "zones: the speaker lists keep only the speakers in no zone, in their order")
    check(ZoneLayout.zoneless(["lc:usb"], key: { $0 }, zoned: zoned).isEmpty,
          "zones: a list whose speakers are all in zones is empty")

    let living = SpeakerZone(id: "z1", name: "Living Room", members: ["lc:usb", "ap:native:den"], volume: 80)
    let kitchen = SpeakerZone(id: "z2", name: "Kitchen", members: ["cc:kitchen"], volume: 100)
    let moved = ZoneEdit.moving("lc:usb", to: "z2", in: [living, kitchen])
    check(moved.map(\.members) == [["ap:native:den"], ["cc:kitchen", "lc:usb"]],
          "zones: Move to Zone takes the speaker out of its zone and adds it after the other one's members")
    check(ZoneEdit.moving("lc:usb", to: "gone", in: [living]) == [living],
          "zones: a move to a zone that isn't there changes nothing")
    let added = ZoneEdit.adding(id: "z3", name: "Desk", members: ["lc:usb", "cc:kitchen", "lc:usb"], to: [living, kitchen])
    check(added.map(\.members) == [["ap:native:den"], [], ["lc:usb", "cc:kitchen"]] && added[2].volume == 100,
          "zones: a new zone takes its speakers from their zones, once each, at full volume")
    check(ZoneEdit.removing("cc:kitchen", from: [kitchen]).map(\.members) == [[]],
          "zones: Remove from Zone leaves the emptied zone in place")
    let pair = SpeakerZone(id: "z4", name: "Den", members: ["lc:usb", "ap:native:left", "ap:native:right"], volume: 100)
    let paired = ZoneEdit.usingTarget("ap:native:pair:lr", for: ["ap:native:left", "ap:native:right"], in: "z4",
                                      zones: [pair])
    check(paired[0].members == ["lc:usb", "ap:native:pair:lr"],
          "zones: Use <target> Instead puts the target where its first member was and drops the others")

    func local(_ on: Bool, listed: Bool = true) -> ZoneMemberFacts { ZoneMemberFacts(kind: .local, listed: listed, on: on) }
    func zonePlan(_ members: [ZoneMemberFacts], streaming: Bool) -> (action: ZoneClick.Action, steps: [ZoneClick.Step]) {
        ZoneClick.plan(members, streaming: streaming, castOnElsewhere: false, airPlay2PlayingElsewhere: false)
    }
    let allOn = zonePlan([local(true), local(true)], streaming: true)
    check(allOn.action == .turnOff && allOn.steps == [.off, .off],
          "zones: a click on a zone whose members all play switches them off")
    let partly = zonePlan([local(true), local(false)], streaming: true)
    check(partly.action == .turnOn && partly.steps == [.on, .on],
          "zones: a click on a zone with a member off switches every member on")
    check(zonePlan([local(true), local(true)], streaming: false).action == .start,
          "zones: a click while streaming is off starts it with the zone's members")
    let gone = zonePlan([local(false, listed: false)], streaming: true)
    check(gone.action == .nothing && gone.steps == [.absent],
          "zones: a zone with no member listed does nothing on a click")

    // covers: S260, S261, S262, S263, S264, S265, S266, S269 (presets in the menus and the panel)
    let presetText = UIStrings()
    let presetFlat = [Double](repeating: 0, count: Equalizer.frequencies.count)
    let livingZone = SpeakerZone(id: "z1", name: "Living Room", members: ["lc:usb:out", "lc:tv"], volume: 60)
    let evening = PresetScene(on: [.init(key: "lc:usb:out", volume: 40), .init(key: "lc:tv", volume: 70)],
                              zones: [livingZone], master: 80, gains: presetFlat)
    let silent = PresetScene(on: [], zones: [livingZone], master: 80, gains: presetFlat)
    func presetRows(_ names: [String]) -> [PresetRow] { names.map { PresetRow(id: "id-" + $0, name: $0) } }
    func numbered(_ n: Int) -> String { presetText.numberedPreset(n) }

    let eleven = PresetMenu.entries(presetRows((1...11).map { "P\($0)" }), text: presetText)
    let presetKeys = eleven.compactMap { entry -> String? in
        if case .preset(_, _, let key, _) = entry { return key } else { return nil }
    }
    check(presetKeys == ["1", "2", "3", "4", "5", "6", "7", "8", "9", "", ""]
            && Array(eleven.suffix(2)) == [.separator, .save(presetText.savePreset)],
          "presets: Control-Command-1 to 9 go to the first nine in list order", "\(presetKeys)")

    check(PresetEdit.saving("Evening", scene: silent, id: "p9", to: [], numbered: numbered) == nil,
          "presets: a save with no speaker on saves nothing")

    let firstSaved = PresetEdit.saving("Evening", scene: evening, id: "p1", to: [], numbered: numbered) ?? []
    let saved = PresetEdit.saving("", scene: evening, id: "p2", to: firstSaved, numbered: numbered) ?? []
    check(saved.map(\.id) == ["p1", "p2"] && saved.map(\.name) == ["Evening", presetText.numberedPreset(2)]
            && saved.first == firstSaved.first && saved.last?.zones == [SpeakerPreset.ZoneVolume(id: "z1", volume: 60)],
          "presets: a save appends, so the other shortcuts keep their keys", "\(saved.map(\.name))")

    var louder = evening
    louder.master = 50
    let updated = PresetEdit.updating("p1", scene: louder, in: saved) ?? []
    check(updated.map(\.id) == ["p1", "p2"] && updated.first?.name == "Evening" && updated.first?.master == 50
            && updated.last == saved.last && PresetEdit.updating("p1", scene: silent, in: saved) == nil,
          "presets: update keeps the ID, name and place")

    let presetRowsNow = PresetCore.rows(updated, louder)
    check(presetRowsNow.map(\.current) == [true, false] && !PresetEdit.canUpdate(presetRowsNow[0])
            && PresetEdit.canUpdate(presetRowsNow[1]),
          "presets: update is offered only while the preset isn't current", "\(presetRowsNow)")

    let third = PresetEdit.saving("Party", scene: evening, id: "p3", to: updated, numbered: numbered) ?? []
    let afterDelete = PresetEdit.deleting("p1", from: third)
    let movedKeys = PresetMenu.entries(PresetCore.rows(afterDelete, louder), text: presetText)
        .compactMap { entry -> String? in
            if case .preset(let id, _, let key, _) = entry { return id + "=" + key } else { return nil }
        }
    check(movedKeys == ["p2=1", "p3=2"] && afterDelete.first == updated.last,
          "presets: delete moves the later shortcuts up and changes nothing that plays", "\(movedKeys)")

    let leftOff = [PresetNotice.LeftOff(name: "Kitchen", reason: .notFound),
                   PresetNotice.LeftOff(name: "Den", reason: .hidden),
                   PresetNotice.LeftOff(name: "Left", reason: .inTarget, target: "Living Room TV"),
                   PresetNotice.LeftOff(name: "Hub", reason: .needsAirPlay2),
                   PresetNotice.LeftOff(name: "Nest", reason: .busy)]
    let leftOffText = "“Evening” is on. Stayed off: Kitchen (\(presetText.zoneMemberNotFound)), Den (Hidden), "
        + "Left (\(presetText.zoneMemberInTarget("Living Room TV"))), Hub (\(presetText.needsAirPlay2)), "
        + "Nest (In use by another app)."
    check(PresetNotice.message(preset: "Evening", leftOff: leftOff, nothingChanged: false, text: presetText)
            == leftOffText,
          "presets: the left-off notice names each member with its reason")

    let fullRecall = PresetRecall.plan([.init(kind: .local, found: true), .init(kind: .local, found: true, on: true)],
                                       others: [true], streaming: true)
    check(fullRecall.action == .apply && fullRecall.steps == [.on, .on] && fullRecall.switchOff == [true]
            && !fullRecall.leavesOff
            && PresetNotice.message(preset: "Evening", leftOff: [], nothingChanged: false, text: presetText) == nil,
          "presets: a full recall clears the notice", "\(fullRecall)")

    let noneHere = PresetRecall.plan([.init(kind: .local, found: false), .init(kind: .cast, found: true, busy: true)],
                                     others: [true], streaming: true)
    let nothingText = "Nothing changed: no speaker in “Evening” can play now. Kitchen (\(presetText.zoneMemberNotFound))."
    check(noneHere.action == .nothing && noneHere.steps == [.leftOff(.notFound), .leftOff(.busy)]
            && noneHere.switchOff == [false] && noneHere.setVolume == [false, false]
            && PresetNotice.message(preset: "Evening", leftOff: [leftOff[0]], nothingChanged: true, text: presetText)
                == nothingText,
          "presets: with no member present nothing changes and only the notice posts", "\(noneHere)")

    let cause = PresetNotice.Cause(presetID: "p1", leftOff: [
        PresetNotice.LeftOff(name: "Kitchen", reason: .notFound, key: "lc:kitchen"),
        PresetNotice.LeftOff(name: "Den", reason: .hidden, key: "ap:den"),
    ], nothingChanged: false)
    check(!cause.isGone(current: nil, onNow: ["lc:kitchen"]) && !cause.isGone(current: "p2", onNow: [])
            && cause.isGone(current: "p1", onNow: []) && cause.isGone(current: nil, onNow: ["lc:kitchen", "ap:den"]),
          "presets: the notice goes once its preset is current or every member it names is on")

    let appFailure = LocalOutputSync.report([LocalOutputSync.Failure(uid: "bt", deviceName: "Headphones")],
                                            enabled: ["bt"], origin: PresetRecallSource.app.origin)
    check(PresetRecallSource.phone.origin == .remote && !PresetRecallSource.phone.beginsAttempts
            && PresetRecallSource.app.origin == .remote && PresetRecallSource.app.beginsAttempts
            && appFailure?.alertNames == nil && appFailure?.notices.count == 1,
          "presets: a recall from the app or the phone shows no alert")

    var boosted = louder
    boosted.gains[3] = 3
    check(PresetCore.current(updated, louder) == "p1" && PresetCore.current(updated, boosted) == nil
            && PresetPanel.label(PresetCore.rows(updated, boosted)) == nil,
          "presets: the current mark follows a band change")

    check(PresetMenu.entries([], text: presetText) == [.empty("No Presets"), .separator, .save("Save as Preset…")]
            && !PresetPanel.showsRow([]) && PresetPanel.showsRow(presetRows(["A"])),
          "presets: no presets, the submenu holds No Presets and the panel row is hidden")

    let presetTexts: [(String, String)] = [
        (presetText.presets, "Presets"),
        (presetText.savePreset, "Save as Preset…"),
        (presetText.savePresetTitle, "Save Preset"),
        (presetText.savePresetMessage, "The speakers that are on, their volumes, the zone volumes, the master volume "
            + "and the curve are saved. Delays and the preamp are not."),
        (presetText.savePresetNeedsSpeakers, "Turn on the speakers you want in the preset first, then save it."),
        (presetText.presetFieldLabel, "Preset name"),
        (presetText.presetPlaceholder, "Evening, Party…"),
        (presetText.numberedPreset(3), "Preset 3"),
        (presetText.renamePreset, "Rename…"),
        (presetText.renamePresetTitle, "Rename Preset"),
        (presetText.updatePreset, "Update to Current Settings"),
        (presetText.deletePreset, "Delete"),
        (presetText.noPresets, "No Presets"),
        (presetText.presetsEmpty, "No presets yet. Turn on the speakers you want, set the volumes and the curve, "
            + "then save them as a preset."),
        (presetText.choosePreset, "Choose a Preset"),
        (presetText.presetCurrent, "Current"),
        (presetText.presetRecallHint, "Recalls this preset"),
        (presetText.presetSaved("Evening"), "Evening saved"),
        (presetText.presetUpdated("Evening"), "Evening updated"),
        (presetText.presetDeleted("Evening"), "Evening deleted"),
        (presetText.presetLeftOffTitle, "Some speakers stayed off"),
        (presetText.presetLeftOff("Evening", "Den (Hidden)"), "“Evening” is on. Stayed off: Den (Hidden)."),
        (presetText.presetNothingChanged("Evening", "Den (Hidden)"),
         "Nothing changed: no speaker in “Evening” can play now. Den (Hidden)."),
        (presetText.presetHidden, "Hidden"),
        (presetText.presetBusy, "In use by another app"),
    ]
    let wrongTexts = presetTexts.filter { $0.0 != $0.1 }.map(\.0)
    check(wrongTexts.isEmpty, "presets: the preset texts", "\(wrongTexts)")

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

    func fades(content: CGFloat, top: CGFloat, height: CGFloat = 400) -> ScrollEdgeFades {
        ScrollEdgeFades.of(contentHeight: content, visibleTop: top, visibleHeight: height, fade: 12)
    }
    check(fades(content: 400, top: 0) == .none, "scroll fades: a column that fits fades at neither edge")
    check(fades(content: 300, top: 0) == .none, "scroll fades: a short column fades at neither edge")
    check(fades(content: 1000, top: 0) == ScrollEdgeFades(top: 0, bottom: 1),
          "scroll fades: a long column at its top fades only at the bottom")
    check(fades(content: 1000, top: 300) == ScrollEdgeFades(top: 1, bottom: 1),
          "scroll fades: a long column scrolled halfway fades at both edges")
    check(fades(content: 1000, top: 600) == ScrollEdgeFades(top: 1, bottom: 0),
          "scroll fades: a long column scrolled to its end fades only at the top")
    check(fades(content: 1000, top: 6) == ScrollEdgeFades(top: 0.5, bottom: 1),
          "scroll fades: the top fade grows with the first points scrolled")
    check(fades(content: 1000, top: -20) == ScrollEdgeFades(top: 0, bottom: 1)
            && fades(content: 1000, top: 620) == ScrollEdgeFades(top: 1, bottom: 0),
          "scroll fades: an elastic overscroll past either end adds no fade there")
    check(fades(content: 401, top: 0) == ScrollEdgeFades(top: 0, bottom: 1.0 / 12),
          "scroll fades: one point past the bottom fades a twelfth of the way")
    check(ScrollEdgeFades.of(contentHeight: 1000, visibleTop: 300, visibleHeight: 400, fade: 0) == .none,
          "scroll fades: no fade height gives no fade")

    let tap = StatusItemClick.action
    check(tap(true, nil, 100) == .close, "menu bar icon: a click while the panel is open closes it")
    check(tap(false, nil, 100) == .open, "menu bar icon: a click while the panel is closed opens it")
    // The press on the icon closes the popover before its click arrives; the
    // closing press and the latest press are the same press, read twice.
    check(tap(false, 98.8, 98.805) == .stayClosed,
          "menu bar icon: the click whose press closed the panel leaves it closed, however long it was held")
    check(tap(false, 100, 100.12) == .open,
          "menu bar icon: a quick second click after a press closed the panel opens it")
    check(tap(false, 98, 100) == .open,
          "menu bar icon: a click long after the panel closed under the icon opens it")
    // Clicks 1 to 5 on the icon, 400 ms apart, open and close the panel in
    // turn. The press of a closing click closes the popover first; AppKit may
    // then drop that click's action or still deliver it.
    for actionDropped in [true, false] {
        var shown = false
        var closingPress: TimeInterval?
        var states: [Bool] = []
        for press in [100.0, 100.4, 100.8, 101.2, 101.6] {
            if shown {
                shown = false
                closingPress = press
                if actionDropped { states.append(shown); continue }
            }
            if tap(shown, closingPress, press) == .open { shown = true }
            closingPress = nil
            states.append(shown)
        }
        let how = actionDropped ? "dropped" : "delivered"
        check(states == [true, false, true, false, true],
              "menu bar icon: five clicks open, close, open, close, open (closing click's action \(how))",
              "\(states)")
    }
    // The status item's window in AppKit coordinates, at the top of a 900-point screen.
    let icon = CGRect(x: 1142, y: 876, width: 24, height: 24)
    let pressed = StatusItemClick.pressClosed
    check(pressed(CGPoint(x: 1154, y: 888), icon, 0.05), "menu bar icon: a press on the icon closes the panel")
    check(pressed(CGPoint(x: 1154, y: 900), icon, 0.05),
          "menu bar icon: a press with the pointer pushed against the top of the screen counts")
    check(!pressed(CGPoint(x: 1154, y: 876), icon, 0.05), "menu bar icon: the line below the icon is not on it")
    check(!pressed(CGPoint(x: 700, y: 500), icon, 0.05), "menu bar icon: a press elsewhere is not a press on the icon")
    check(!pressed(CGPoint(x: 1154, y: 888), icon, 3), "menu bar icon: Escape with the pointer resting on the icon is no press")
    let addRow = CastAddRow.state
    check(addRow(true, .searching) == .reserved && addRow(true, .noneFound) == .shown,
          "menu bar panel: the Add by Address row keeps its room while the Cast search runs, so the panel never grows when it ends")
    check(addRow(true, .failed) == .shown && addRow(true, .quiet) == .none && addRow(false, .searching) == .none
            && addRow(false, .noneFound) == .none,
          "menu bar panel: no Add by Address row once speakers are listed or when adding isn't possible")
    let duringClose = StatusItemClick.pressDuringClose
    check(duringClose(30.1, 30.25) && !duringClose(30.25, 30.25) && !duringClose(31, 30.25) && !duringClose(30.1, nil),
          "menu bar icon: only a press while the panel still fades out is ignored, as Outlook does")
    func near(_ a: TimeInterval, _ b: TimeInterval) -> Bool { abs(a - b) < 1e-9 }
    let shrink = PanelQuiet.duration(.close, system: false)
    check(duringClose(30 + shrink / 2, 30 + shrink) && !duringClose(30 + shrink + 0.01, 30 + shrink),
          "menu bar icon: a press is ignored only while the panel shrinks away; right after, it opens it")

    let window = CGRect(x: 1000, y: 543, width: 306, height: 386)
let grown = PanelResize.windowFrame(window, content: CGSize(width: 280, height: 359), to: CGSize(width: 280, height: 428))
check(grown == CGRect(x: 1000, y: 474, width: 306, height: 455),
      "menu bar panel: a taller panel grows its window downward, its top edge under the icon where it was")
let wider = PanelResize.windowFrame(window, content: CGSize(width: 280, height: 359), to: CGSize(width: 300, height: 300))
check(wider.maxY == window.maxY && wider.midX == window.midX && wider.size == CGSize(width: 326, height: 327),
      "menu bar panel: a resize keeps the window's top edge and middle, so the arrow stays on the icon")
let closeAsked = PopoverCloseRequest.answer
    check(closeAsked(false, true, 30, nil) == .closeOurWay && closeAsked(true, true, 30, nil) == .allow,
          "menu bar panel: an open panel the popover wants closed shrinks away its own way")
    // A press on the icon mid-grow starts the close, and the popover asks
    // again about 40 ms later as its window loses key.
    check(closeAsked(false, false, 30.04, 30 + shrink) == .wait,
          "menu bar panel: the popover asking again while the panel fades out waits for the fade, so it doesn't vanish at once")
    check(closeAsked(false, false, 30 + shrink + 0.01, 30 + shrink) == .allow && closeAsked(false, false, 30, nil) == .allow
            && closeAsked(true, false, 30.04, 30 + shrink) == .allow,
          "menu bar panel: once the fade is over, or with the popover's own animation, the popover may close")

    let grow = PanelMotion.open(reduceMotion: false), shrinkBack = PanelMotion.close(reduceMotion: false)
    check(grow.scale >= 0.9 && grow.scale < 1 && grow.settles >= 0.25 && grow.settles <= 0.3,
          "menu bar panel: it grows out of the icon and settles in about a quarter second")
    check(grow.dampingRatio >= 0.85 && grow.dampingRatio <= 1 && grow.overshoot * (1 - grow.scale) * 400 < 0.5,
          "menu bar panel: the growth never bounces by half a point, even on a tall panel")
    check(shrinkBack.scale > grow.scale && shrinkBack.scale < 1 && shrinkBack.duration >= 0.12 && shrinkBack.duration <= 0.15,
          "menu bar panel: the close is a quick, small shrink")
    check(!PanelMotion.open(reduceMotion: true).grows && !PanelMotion.close(reduceMotion: true).grows
            && PanelMotion.open(reduceMotion: true).duration > 0,
          "menu bar panel: with Reduce Motion it only fades")
    let pinned = PanelMotion.pin(CGPoint(x: 140, y: 420), scale: grow.scale)
    check(abs(140 * grow.scale + Double(pinned.x) - 140) < 1e-9 && abs(420 * grow.scale + Double(pinned.y) - 420) < 1e-9,
          "menu bar panel: the point under the icon stays put while the panel grows")
    check(PanelQuiet.duration(.open, system: false) == grow.settles && shrink == shrinkBack.duration,
          "menu bar panel: deferred work waits exactly for the panel's own motion")
    check(PanelMotion.tail * (1 - grow.scale) * 600 < 0.5,
          "menu bar panel: ending the spring at its settle time moves even a 600 pt panel's far edge under half a point")

    var resize = PanelResize()
    let tall = CGSize(width: 306, height: 455), taller = CGSize(width: 306, height: 478)
    check(near(resize.changed(to: CGSize(width: 306, height: 420), at: 50, settled: 40), 50.1)
            && near(resize.changed(to: tall, at: 50.05, settled: 40), 50.1) && resize.take(at: 50.08) == nil,
          "menu bar panel: size changes within a tenth of a second wait for one step")
    check(resize.take(at: 50.1) == tall && resize.take(at: 50.2) == nil,
          "menu bar panel: the step goes to the latest size, once")
    check(near(resize.changed(to: taller, at: 60, settled: 60.28), 60.28),
          "menu bar panel: a size change while the panel grows in waits until it has settled")

    func sizing(_ events: [PanelSizing.Event]) -> PanelSizing {
        events.reduce(PanelSizing.closed) { $0.after($1) }
    }
    check(!sizing([]).followsContent && !sizing([.show]).followsContent,
          "menu bar panel: the size holds while the popover opens")
    check(sizing([.show, .didShow]).followsContent, "menu bar panel: the size follows the content once fully shown")
    check(!sizing([.show, .didShow, .willClose]).followsContent
            && !sizing([.show, .didShow, .willClose, .didClose]).followsContent,
          "menu bar panel: the size holds from the moment the popover starts to close")
    check(!sizing([.show, .willClose, .didShow]).followsContent,
          "menu bar panel: a show notice arriving after the close began keeps the size held")
    check(sizing([.show, .didShow, .willClose, .show]) == .opening
            && !sizing([.show, .didShow, .willClose, .show, .didClose]).followsContent
            && sizing([.show, .didShow, .willClose, .show, .didClose, .didShow]).followsContent,
          "menu bar panel: reopened mid-close, the old close's notice doesn't end the new open")

    var own = PanelQuiet()
    own.began(.open, at: 40)
    check(near(own.wait(at: 40.1), grow.settles - 0.1) && own.wait(at: 40 + grow.settles) == 0 && grow.settles < 0.6,
          "menu bar panel: with the panel's own grow, work after an open waits less than for the system's bounce")
    own.began(.close, at: 41)
    check(near(own.wait(at: 41), shrinkBack.duration) && own.wait(at: 41 + shrinkBack.duration) == 0,
          "menu bar panel: work after the panel's own close waits only for its shrink")

    var quiet = PanelQuiet(system: true)
    check(quiet.wait(at: 5) == 0, "menu bar panel: with no animation, deferred work runs at once")
    quiet.began(.open, at: 10)
    check(near(quiet.wait(at: 10.2), 0.4) && quiet.wait(at: 10.6) == 0,
          "menu bar panel: work after an open waits until its bounce has settled")
    quiet.began(.close, at: 10.1)
    check(near(quiet.wait(at: 10.3), 0.3), "menu bar panel: a close during the open's bounce never shortens the wait")
    quiet.began(.close, at: 11)
    check(near(quiet.wait(at: 11.1), 0.15) && quiet.wait(at: 11.25) == 0,
          "menu bar panel: work after a close waits until it has faded out")
    quiet.began(.open, at: 11.2)
    check(near(quiet.wait(at: 11.3), 0.5), "menu bar panel: a quick reopen pushes waiting work past its own bounce")

    var queue = QuietQueue()
    queue.quiet = PanelQuiet(system: true)
    queue.quiet.began(.open, at: 20)
    check(queue.enqueue("refresh") && !queue.enqueue("refresh") && queue.enqueue("rebuild"),
          "menu bar panel: a second request under a label already waiting is dropped")
    var firstWait: TimeInterval = 0
    if case .wait(let wait) = queue.step("refresh", at: 20.2) { firstWait = wait }
    check(near(firstWait, 0.4), "menu bar panel: waiting work waits out the open's bounce", "\(firstWait)")
    queue.quiet.began(.close, at: 20.5)
    var secondWait: TimeInterval = 0
    if case .wait(let wait) = queue.step("refresh", at: 20.6) { secondWait = wait }
    check(near(secondWait, 0.15), "menu bar panel: a close begun meanwhile makes waiting work wait again", "\(secondWait)")
    check(queue.step("refresh", at: 20.75) == .run && queue.waiting == ["rebuild"],
          "menu bar panel: once quiet, the waiting work runs and leaves the queue")
    check(queue.enqueue("refresh") && !queue.enqueue("rebuild"),
          "menu bar panel: a label can wait again once its work ran; one still waiting stays deduplicated")
}

// MARK: Settings kept between launches (AppSettings.swift)
// covers: S212
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
    check(damaged.airplayVolumes == ["x": 101] && damaged.groups.isEmpty && damaged.enabledLocal == ["lc"]
            && damaged.appearance == .system,
          "settings: unreadable groups, IDs or appearance fall back to the defaults; an AirPlay own volume above 100 stays (S290)")
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
        (Keys.airPlay2, "airPlay2Enabled"), (Keys.zones, "speakerZones"),
        (Keys.zonesMigrated, "speakerZonesMigrated"), (Keys.zonesNoticeDue, "speakerZonesNoticeDue"),
        (Keys.presets, "speakerPresets"), (Keys.presetsMigrated, "speakerPresetsMigrated"),
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

    // covers: S173 (which listed output is the reference)
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

    // covers: S178, S181 (the AirPlay delay field)
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
    // Read before load(), whose zone and preset migrations (S248, S254) write marks of their own.
    let secondWrote = stored.writes != writes
    let loaded = store.load()
    check(first?.offsets == [usb: 100, bt: -200] && first?.airPlayDelayMs == 2000
            && second == nil && !secondWrote
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

    // covers: S215
    // A start that fails in the audio core says so, not that capture failed.
    let t = UIStrings()
    let coreMessage = t.streamingStartMessage(for: StreamEngine.StartError.coreUnavailable)
    check(coreMessage == t.engineSetupFailed && coreMessage.contains("open it again")
            && coreMessage != t.streamingStartMessage(for: SourceCapture.CaptureError.tapNotCreated(-1)),
          "start error: the audio core failing to set up asks to reopen the app", coreMessage)

    // covers: S181 (what the rows show and the fields take)
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
// covers: S228
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

    let before = SpeakerNames(custom: ["lc:cast:nest-1": "Den", "lc:cast:tv": "Old", "cc:tv": "Lounge",
                                       "lc:cast:": "Blank ID", "lc:usb-uid": "Desk"])
    let moved = before.castKeysMoved()
    check(moved.custom == ["cc:nest-1": "Den", "cc:tv": "Lounge", "lc:usb-uid": "Desk"]
              && moved.castKeysMoved() == moved && SpeakerNames().castKeysMoved() == SpeakerNames()
              && moved.shown(key: CastID.key("nest-1"), system: "Nest") == "Den",
          "speaker names: Cast names move from lc:cast: once", "moved: \(moved.custom)")

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
// covers: S220, S221
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

// MARK: Field editor — the click monitor finds the delay field being typed in (FieldEditor)
// Final and without NSTextViewDelegate, as the main window's delay field is.
final class EditedDelayField: NSTextField {
    var rowID = ""
}
MainActor.assumeIsolated {
    let field = EditedDelayField(frame: .zero)
    field.rowID = "dock"
    let editor = NSTextView(frame: .zero)
    editor.isFieldEditor = true
    // AppKit sets the delegate this way when the field starts editing.
    editor.perform(Selector(("setDelegate:")), with: field)
    check(FieldEditor.field(EditedDelayField.self, editedBy: editor)?.rowID == "dock",
          "delay panels: the click monitor finds the delay field the field editor types for, so a click on its own step keeps the typing")
    let otherField = NSTextField(frame: .zero)
    let other = NSTextView(frame: .zero)
    other.perform(Selector(("setDelegate:")), with: otherField)
    check(FieldEditor.field(EditedDelayField.self, editedBy: NSTextView(frame: .zero)) == nil
            && FieldEditor.field(EditedDelayField.self, editedBy: other) == nil,
          "delay panels: an editor typing for no field, or for another kind of field, has no delay field")
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
    let traced = [DelayBadge.traceLine(id: "usb", shows: shown("usb") ?? ""),
                  DelayBadge.traceLine(id: "builtin", shows: shown("builtin") ?? "")]
    check(traced == ["[AudioSync] delay value usb shows +30 ms",
                     "[AudioSync] delay value builtin shows none (reference speaker or no longer listed)"],
          "delay value: the debug log names a row that shows no value instead of leaving it blank", "\(traced)")
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
// covers: S220
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
// covers: S186 (the Mac's stop order)

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
        if let ring = CoreDelayRing(capacityFrames: 48_000 * 24),
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
        if let ring = CoreDelayRing(capacityFrames: 48_000 * 24),
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
        if let ring = CoreDelayRing(capacityFrames: 48_000 * 24),
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
        let refusal: Error
        init(active: [String], refusing: Set<String>, refusal: Error = NSError(domain: "logictest", code: -1)) {
            activeOutputUIDs = active
            self.refusing = refusing
            self.refusal = refusal
        }
        func attachOutput(uid: String, device: AudioObjectID, delayMs: Double, gain: Double) throws {
            tried.append(uid)
            if refusing.contains(uid) { throw refusal }
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

    // S36: a capture rate the ring can't hold names the rate and Audio MIDI Setup, not exclusive use.
    let tooFast = LocalOutputSync.apply(
        enabled: ["usb", "bt"], listing: [usb, bt],
        to: RefusingEngine(active: [], refusing: ["usb"],
                           refusal: OutputRenderer.StartFailure.captureRateTooHigh(352_800)),
        holdBacks: [:], gain: { _ in 1 })
    let strings = UIStrings()
    let generic = strings.outputCouldNotStart(names: "USB DAC")
    let rateText = strings.outputCouldNotStart(names: "USB DAC", refusedCaptureRate: 352_800)
    let rateText384 = strings.outputCouldNotStart(names: "USB DAC", refusedCaptureRate: 384_000)
    check(tooFast.failures == [F(uid: "usb", deviceName: "USB DAC", refusedCaptureRate: 352_800)]
            && applied.failures.allSatisfy { $0.refusedCaptureRate == nil },
          "S36 start failure: a refused capture rate comes back with the failure, any other failure without one",
          "\(tooFast.failures)")
    check(rateText.contains("USB DAC") && rateText.contains("352.8 kHz") && rateText.contains("192 kHz")
            && rateText.contains("Audio MIDI Setup") && !rateText.contains("exclusively")
            && rateText384.contains("384 kHz") && generic.contains("exclusively")
            && generic.hasPrefix("USB DAC couldn't start, so it's off.")
            && strings.outputCouldNotStart(names: "USB DAC, Den", count: 2).contains("so they're off"),
          "S36 start failure: the text names the capture rate, the 192 kHz limit and Audio MIDI Setup", rateText)
    check(OutputRenderer.StartFailure.captureRateTooHigh(384_000).description.contains("384000 Hz"),
          "S36 start failure: the debug log names the refused rate")
    let rateFailures = [F(uid: "usb", deviceName: "USB DAC", refusedCaptureRate: 384_000),
                        F(uid: "bt", deviceName: "Headphones", refusedCaptureRate: 384_000)]
    let mixedFailures = [rateFailures[0], F(uid: "bt", deviceName: "Headphones")]
    check(LocalOutputSync.sharedRefusedRate(rateFailures) == 384_000
            && LocalOutputSync.sharedRefusedRate(mixedFailures) == nil
            && LocalOutputSync.sharedRefusedRate([]) == nil,
          "S36 start failure: one alert gives the rate only when every failure it names was refused for it",
          "\(String(describing: LocalOutputSync.sharedRefusedRate(mixedFailures)))")

    // covers: S72
    // S72: a master change gives each running local speaker its gain again;
    // one switched on but not running (here "tv") gets no call.
    let playing = RefusingEngine(active: ["usb", "bt"], refusing: [])
    LocalOutputSync.reapplyGains(on: playing, gain: { ["usb": 0.5, "bt": 0.25, "tv": 0.75][$0] ?? 0 })
    check(playing.regained == ["usb", "bt"] && playing.gains == ["usb": 0.5, "bt": 0.25],
          "S72 master: every running local speaker gets its gain again, and only those",
          "regained \(playing.regained), gains \(playing.gains)")

    // covers: S186 (speakers switched off in one sync leave in one call, so they fade out at once)
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
// covers: S215
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
    // covers: S101 (the safety net and the next due reconcile)
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
    // S227 g: the capture let the app's tap go once its audio process ended
    // ("waiting for one"), the app's quit came next, and Core Audio refused
    // the whole system's first tap ('what'). Each wait runs at once here.
    typealias Attempt = AppLifecycle.SystemSwitch.Attempt
    func systemSwitch(_ answers: [Attempt], userActsAfter: Int? = nil, firstWait: TimeInterval = 0)
        -> (streaming: Bool?, lasts: [Bool], waits: [TimeInterval], early: Int) {
        var left = answers, lasts: [Bool] = [], waits: [TimeInterval] = [], pending: [() -> Void] = []
        var streaming: Bool?
        AppLifecycle.SystemSwitch.run(
            firstWait: firstWait,
            start: { last in
                lasts.append(last)
                return left.isEmpty ? .failed : left.removeFirst()
            },
            later: { delay, body in
                waits.append(delay)
                pending.append(body)
            },
            stillWanted: { userActsAfter == lasts.count ? .drop(release: false) : .attempt },
            finish: { streaming = $0 })
        let early = lasts.count
        while !pending.isEmpty { pending.removeFirst()() }
        return (streaming, lasts, waits, early)
    }
    var quitSource = music
    var afterQuit: (streaming: Bool?, lasts: [Bool], waits: [TimeInterval], early: Int) = (nil, [], [], 0)
    if quit(musicQuit, music, .system) == "switchToSystem com.apple.Music" {
        quitSource = .system
        afterQuit = systemSwitch([.captureFailed, .started])
    }
    check(quitSource == .system && afterQuit.streaming == true && afterQuit.lasts == [false, false]
            && afterQuit.waits == [AppLifecycle.SystemSwitch.spacing],
          "source quit: a whole system whose first tap fails right after the app's tap went starts on the next try (S227 g)",
          "\(quitSource), \(afterQuit)")
    let refused = systemSwitch([.captureFailed, .captureFailed, .captureFailed, .started])
    let spacing = AppLifecycle.SystemSwitch.spacing
    check(refused.streaming == false && refused.lasts == [false, false, true] && refused.waits == [spacing, spacing]
            && spacing == 0.5,
          "source quit: the whole system is tried 3 times, 0.5 s apart, then the last failure stops streaming (S227 g)",
          "\(refused)")
    let noSpeaker = systemSwitch([.failed, .started])
    check(noSpeaker.streaming == false && noSpeaker.lasts == [false] && noSpeaker.waits.isEmpty,
          "source quit: a start that fails before the capture (no speaker, permission) is not tried again (S227 g)",
          "\(noSpeaker)")
    let userActed = systemSwitch([.captureFailed, .started], userActsAfter: 1)
    check(userActed.streaming == nil && userActed.lasts == [false],
          "source quit: a stop, start or source change during the wait ends the retries, with no notice (S227 g)",
          "\(userActed)")
    // The Mac sitting (2026-10-26, 5 of 5 runs): the app's tap was destroyed
    // 1 ms before the quit, attempt 1 always lost, attempt 2 worked 1.9 s on.
    typealias QuitSwitch = AppLifecycle.SystemSwitch
    let settleWait = QuitSwitch.firstWait(sinceDestroy: 0.001)
    check(abs(settleWait - (QuitSwitch.settle - 0.001)) < 1e-9 && QuitSwitch.firstWait(sinceDestroy: nil) == 0
            && QuitSwitch.firstWait(sinceDestroy: QuitSwitch.settle) == 0 && QuitSwitch.firstWait(sinceDestroy: 30) == 0,
          "source quit: the whole system waits until the app's tap has been gone a second, never when it went long ago (S227 g)",
          "\(settleWait)")
    let settled = systemSwitch([.started], firstWait: settleWait)
    check(settled.streaming == true && settled.lasts == [false] && settled.waits == [settleWait] && settled.early == 0,
          "source quit: the first attempt starts only once the app's tap settled, and then it works (S227 g)",
          "\(settled)")
    let actedFirst = systemSwitch([.started], userActsAfter: 0, firstWait: settleWait)
    check(actedFirst.streaming == nil && actedFirst.lasts.isEmpty,
          "source quit: a stop or source change before the first attempt makes none (S227 g)", "\(actedFirst)")
    func afterWait(system: Bool = true, stopped: Bool = false, running: Bool = false, userStarted: Bool = false)
        -> QuitSwitch.AfterWait {
        QuitSwitch.afterWait(sourceIsSystem: system, userStopped: stopped, running: running, userStarted: userStarted)
    }
    check(afterWait() == .attempt && afterWait(stopped: true) == .drop(release: false)
            && afterWait(system: false) == .drop(release: true),
          "source quit: after a wait the next attempt runs, unless the user stopped or picked another source (S227 g)",
          "\(afterWait()), \(afterWait(stopped: true)), \(afterWait(system: false))")
    check(afterWait(userStarted: true) == .drop(release: false),
          "source quit: a start by the user during the wait that failed ends the attempts, after its alert (S227 g)",
          "\(afterWait(userStarted: true))")
    check(afterWait(running: true) == .startedMeanwhile
            && afterWait(running: true, userStarted: true) == .drop(release: false)
            && afterWait(system: false, running: true) == .drop(release: false),
          "source quit: auto-connect starting the whole system during the wait still says the app quit and the switch (S227 g)",
          "\(afterWait(running: true)), \(afterWait(running: true, userStarted: true))")
    var meanwhile: Bool?
    QuitSwitch.run(start: { _ in .captureFailed }, later: { _, body in body() }, stillWanted: { .startedMeanwhile },
                   finish: { meanwhile = $0 })
    check(meanwhile == true, "source quit: a start during the wait ends the switch as started, with its notice (S227 g)",
          "\(String(describing: meanwhile))")
    // macOS's app list can miss the mix's other apps in the turn one of them quits.
    let mix3 = AudioSource.mixed(.only, bundleIDs: ["com.spotify.client", "com.apple.Safari", "com.apple.TextEdit"])
    func mixQuit(_ report: Report, running: Set<String> = []) -> String {
        guard let answer = AppLifecycle.sourceQuit(report, source: mix3, setting: .system,
                                                   isRunning: { running.contains($0) }) else { return "keep" }
        return "\(answer.action) \(answer.bundleID)"
    }
    check(mixQuit(.terminated(bundleID: "com.apple.TextEdit", streaming: true)) == "keep"
            && mixQuit(.captureLost(.appNotRunning("com.spotify.client"))) == "keep",
          "mix quit: an app of a mix quitting is never judged from the app list of the turn it quit in (S164)",
          "\(mixQuit(.terminated(bundleID: "com.apple.TextEdit", streaming: true))), "
              + mixQuit(.captureLost(.appNotRunning("com.spotify.client"))))
    let textEditGone = Report.mixAppGone(bundleID: "com.apple.TextEdit", streaming: true)
    check(mixQuit(textEditGone, running: ["com.apple.Safari"]) == "keep"
            && mixQuit(textEditGone) == "switchToSystem com.apple.TextEdit"
            && mixQuit(.mixAppGone(bundleID: "com.apple.TextEdit", streaming: false)) == "keep",
          "mix quit: judged again a moment later, the mix goes on while another of its apps runs, and acts as set once none does",
          "\(mixQuit(textEditGone, running: ["com.apple.Safari"])), \(mixQuit(textEditGone))")
    let quitWhileStreaming = Report.terminated(bundleID: "com.apple.TextEdit", streaming: true)
    check(AppLifecycle.waitsForRecheck(quitWhileStreaming, source: mix3)
            && AppLifecycle.waitsForRecheck(.captureLost(.appNotRunning("com.spotify.client")), source: mix3)
            && !AppLifecycle.waitsForRecheck(.terminated(bundleID: "com.apple.TextEdit", streaming: false), source: mix3)
            && !AppLifecycle.waitsForRecheck(.terminated(bundleID: "com.apple.Music", streaming: true), source: mix3)
            && !AppLifecycle.waitsForRecheck(.terminated(bundleID: nil, streaming: true), source: mix3)
            && !AppLifecycle.waitsForRecheck(.captureLost(.inputDisconnected), source: mix3)
            && !AppLifecycle.waitsForRecheck(quitWhileStreaming, source: .app(bundleID: "com.apple.TextEdit"))
            && !AppLifecycle.waitsForRecheck(quitWhileStreaming,
                                             source: .mixed(.allExcept, bundleIDs: ["com.apple.TextEdit", "com.apple.Safari"])),
          "mix quit: only an app of an only-these mix quitting while it streams, or that mix's capture losing its apps, waits")
    func afterLoss(changed: Bool, stopped: Bool, running: Bool, newSession: Bool = false) -> AppLifecycle.MixLossStep {
        AppLifecycle.afterMixLoss(sourceChanged: changed, userStopped: stopped, engineRunning: running,
                                  sessionChanged: newSession)
    }
    check(afterLoss(changed: false, stopped: false, running: false) == .rebuild
            && afterLoss(changed: true, stopped: false, running: false) == .start
            && afterLoss(changed: true, stopped: true, running: false) == .stop
            && afterLoss(changed: false, stopped: true, running: false) == .stop
            && afterLoss(changed: true, stopped: false, running: true) == .none
            && afterLoss(changed: false, stopped: false, running: true) == .none,
          "mix quit: after a mix's capture loss, a source picked in the wait starts, a stop finishes cleanly, else the mix rebuilds")
    check(afterLoss(changed: false, stopped: false, running: false, newSession: true) == .none
            && afterLoss(changed: true, stopped: true, running: false, newSession: true) == .none,
          "mix quit: a loss's wait ends without acting once streaming was started again within it, so a later loss keeps its own wait")
    // The mix's last app quits while it has no audio process, so no audio process goes away.
    let pair = AudioSource.mixed(.only, bundleIDs: ["com.apple.Music", "com.apple.podcasts"])
    check(AppTapFollow.followsRunningApps(pair) && AppTapFollow.followsRunningApps(.app(bundleID: "com.apple.Music"))
            && !AppTapFollow.followsRunningApps(.system)
            && !AppTapFollow.followsRunningApps(.mixed(.allExcept, bundleIDs: ["com.apple.Music", "com.apple.podcasts"]))
            && !AppTapFollow.followsRunningApps(.input(uid: "BuiltInMicrophoneDevice")),
          "mix quit: an app source's capture looks at its apps again whenever an app quits, audio process or not")
    func lastQuit(_ setting: WhenSourceQuits) -> String {
        guard AppTapFollow.followsRunningApps(pair) else { return "unseen" }
        guard AppTapFollow.look(tapped: [], found: [], appRunning: false, secondLook: false) == .lookAgain,
              AppTapFollow.look(tapped: [], found: [], appRunning: false, secondLook: true) == .lost else { return "waits" }
        let named = AppTapFollow.quitApp(of: pair.capturedApps, ranBefore: ["com.apple.podcasts"])
        guard AppLifecycle.waitsForRecheck(.captureLost(.appNotRunning(named)), source: pair),
              afterLoss(changed: false, stopped: false, running: false) == .rebuild else { return "no rebuild" }
        guard let answer = AppLifecycle.sourceQuit(.rebuildFailed(SourceCapture.CaptureError.appNotRunning(named)),
                                                   source: pair, setting: setting, isRunning: { _ in false })
        else { return "keep" }
        return "\(answer.action) \(answer.bundleID)"
    }
    check(lastQuit(.system) == "switchToSystem com.apple.podcasts" && lastQuit(.stop) == "stop com.apple.podcasts",
          "mix quit: the mix's last app quitting with no audio process is judged again and acts as set",
          "\(lastQuit(.system)), \(lastQuit(.stop))")
    check(AppTapFollow.look(tapped: [5], found: [], appRunning: false, secondLook: false) == .lookAgain
            && AppTapFollow.look(tapped: [5], found: [], appRunning: false, secondLook: true) == .lost
            && AppTapFollow.look(tapped: [5], found: [5, 9], appRunning: true, secondLook: false) == .retap([5, 9])
            && AppTapFollow.look(tapped: [5], found: [], appRunning: true, secondLook: false) == .wait,
          "mix quit: a look that finds none of the apps running looks again a moment later before the source counts as lost")
    // Podcasts quits while Music runs on; in that turn macOS's list can miss Music too.
    func memberQuits() -> String {
        let first = AppTapFollow.look(tapped: [5, 8], found: [], appRunning: false, secondLook: false)
        guard first == .lookAgain else { return "capture lost at once (\(first))" }
        let second = AppTapFollow.look(tapped: [5, 8], found: [5], appRunning: true, secondLook: true)
        guard second == .retap([5]) else { return "second look: \(second)" }
        let notice = Report.terminated(bundleID: "com.apple.podcasts", streaming: true)
        guard AppLifecycle.waitsForRecheck(notice, source: pair) else { return "judged in the quit's turn" }
        let judged = AppLifecycle.sourceQuit(.mixAppGone(bundleID: "com.apple.podcasts", streaming: true), source: pair,
                                             setting: .system, isRunning: { $0 == "com.apple.Music" })
        return judged.map { "\($0.action) \($0.bundleID)" } ?? "keep"
    }
    check(memberQuits() == "keep",
          "mix quit: an app of a mix quitting while another runs loses no capture; its own quit notice is judged and the mix goes on",
          memberQuits())
    // A paused app ends its audio process, never its own; macOS's list of running apps can miss it then.
    typealias Listed = AppTapFollow.ListedApp
    let spotify = "com.spotify.client", podcasts = "com.apple.podcasts", musicID = "com.apple.Music"
    func running(_ app: String, listed: [Listed], seen: [pid_t], live: Set<pid_t>) -> [pid_t] {
        AppTapFollow.runningPIDs(of: app, listed: listed, seen: seen, alive: { live.contains($0) })
    }
    func soloLook(_ pids: [pid_t], secondLook: Bool) -> AppTapFollow {
        AppTapFollow.look(tapped: [7], found: [], appRunning: !pids.isEmpty, secondLook: secondLook)
    }
    let pausedUnlisted = running(spotify, listed: [], seen: [501], live: [501])
    let pausedOtherCase = running(spotify, listed: [Listed(bundleID: "com.Spotify.Client", pid: 501, terminated: false)],
                                  seen: [], live: [501])
    check(pausedUnlisted == [501] && pausedOtherCase == [501]
            && soloLook(pausedUnlisted, secondLook: false) == .wait && soloLook(pausedUnlisted, secondLook: true) == .wait,
          "source app quit: a paused app whose process lives still runs, so its single-app source waits and is never lost",
          "\(pausedUnlisted), \(pausedOtherCase)")
    let pausedListed = running(spotify, listed: [Listed(bundleID: spotify, pid: 501, terminated: false)], seen: [], live: [501])
    check(pausedListed == [501]
            && AppTapFollow.look(tapped: [7], found: [], appRunning: !pausedListed.isEmpty, secondLook: true) == .wait
            && AppTapFollow.look(tapped: [], found: [], appRunning: !pausedListed.isEmpty, secondLook: true) == .keep,
          "source app quit: an app without an audio process is not an app that quit")
    // Podcasts quits but macOS sends no terminate notice and may still list it; only a fresh look at its process tells.
    let staleListing = [Listed(bundleID: podcasts, pid: 702, terminated: false)]
    func podcastsQuit(_ setting: WhenSourceQuits) -> String {
        guard running(podcasts, listed: staleListing, seen: [702], live: [702]) == [702] else { return "not running before" }
        let gone = running(podcasts, listed: staleListing, seen: [702], live: [])
        guard soloLook(gone, secondLook: false) == .lookAgain else { return "first look: \(soloLook(gone, secondLook: false))" }
        guard soloLook(gone, secondLook: true) == .lost else { return "second look: \(soloLook(gone, secondLook: true))" }
        guard let answer = AppLifecycle.sourceQuit(.captureLost(.appNotRunning(podcasts)), source: .app(bundleID: podcasts),
                                                   setting: setting, isRunning: { _ in false })
        else { return "keep" }
        return "\(answer.action) \(answer.bundleID)"
    }
    check(podcastsQuit(.system) == "switchToSystem \(podcasts)" && podcastsQuit(.stop) == "stop \(podcasts)",
          "source app quit: an app that quits without a terminate notice is judged from a fresh look at its process",
          "\(podcastsQuit(.system)), \(podcastsQuit(.stop))")
    let quitUnlisted = running(spotify, listed: [], seen: [501], live: [])
    check(quitUnlisted.isEmpty && soloLook(quitUnlisted, secondLook: false) == .lookAgain
            && soloLook(quitUnlisted, secondLook: true) == .lost,
          "source app quit: an app whose process ended is lost after the second look")
    // A mix of Music (playing) and a paused Spotify, then Spotify really quits.
    let musicListed = [Listed(bundleID: musicID, pid: 610, terminated: false)]
    let mixPaused = running(musicID, listed: musicListed, seen: [], live: [610, 501])
        + running(spotify, listed: musicListed, seen: [501], live: [610, 501])
    let spotifyQuit = running(spotify, listed: musicListed, seen: [501], live: [610])
    let mixQuitJudged = AppLifecycle.sourceQuit(.mixAppGone(bundleID: spotify, streaming: true),
                                                source: .mixed(.only, bundleIDs: [musicID, spotify]), setting: .system,
                                                isRunning: { !running($0, listed: musicListed, seen: [], live: [610]).isEmpty })
    check(mixPaused == [610, 501] && spotifyQuit.isEmpty
            && AppTapFollow.look(tapped: [7, 9], found: [9], appRunning: !mixPaused.isEmpty, secondLook: false) == .retap([9])
            && mixQuitJudged == nil,
          "source app quit: a paused mix member still runs, and a member's real quit leaves the mix on its other apps")
    // macOS 27 sent no terminate notice when Podcasts quit from a Music + Podcasts mix; the capture's look saw it.
    let mixLook: [String: [pid_t]] = [musicID: [610], podcasts: [702]]
    let podcastsGone = AppTapFollow.quitSince(before: mixLook, now: [musicID: [610], podcasts: []])
    check(podcastsGone == [AppTapFollow.QuitApp(bundleID: podcasts, pids: [702])]
            && AppTapFollow.quitSince(before: mixLook, now: mixLook).isEmpty
            && AppTapFollow.quitSince(before: [podcasts: []], now: [podcasts: []]).isEmpty
            && AppTapFollow.quitSince(before: [:], now: [podcasts: []]).isEmpty
            && AppTapFollow.quitSince(before: [podcasts: [702, 703]], now: [podcasts: [703]]).isEmpty,
          "source app quit: the capture reports an app whose processes all ended since its last look, never one it saw no process of",
          "\(podcastsGone)")
    var quitReports = AppLifecycle.QuitReports()
    let seenByCapture = quitReports.isNew(podcasts, pids: [702])
    let noticeAfter = quitReports.isNew("com.apple.Podcasts", pids: [702])
    let noticeFirst = quitReports.isNew(musicID, pids: [610])
    let captureAfter = quitReports.isNew(musicID, pids: [610])
    let relaunchedQuit = quitReports.isNew(podcasts, pids: [905])
    let noProcess = quitReports.isNew(musicID, pids: []) && quitReports.isNew(musicID, pids: [])
    let noBundleID = quitReports.isNew(nil, pids: [610])
    check(seenByCapture && !noticeAfter && noticeFirst && !captureAfter && relaunchedQuit && noProcess && noBundleID,
          "source app quit: the capture's report and macOS's notice of the same quit act once, whichever comes first",
          "\(seenByCapture) \(noticeAfter) \(noticeFirst) \(captureAfter) \(relaunchedQuit) \(noProcess) \(noBundleID)")
    for other in 0..<(AppLifecycle.QuitReports.kept + 4) {
        _ = quitReports.isNew("com.example.app\(other)", pids: [pid_t(2000 + other)])
    }
    check(quitReports.count <= AppLifecycle.QuitReports.kept && quitReports.isNew(podcasts, pids: [702]),
          "source app quit: the record of reported quits keeps only the latest ones", "\(quitReports.count)")
    func capturedQuit(_ source: AudioSource, _ setting: WhenSourceQuits, running: Set<String> = []) -> String {
        var reports = AppLifecycle.QuitReports()
        guard let gone = AppTapFollow.quitSince(before: mixLook, now: [musicID: [610], podcasts: []]).first,
              reports.isNew(gone.bundleID, pids: gone.pids) else { return "unreported" }
        let report = AppLifecycle.SourceQuitReport.terminated(bundleID: gone.bundleID, streaming: true)
        if AppLifecycle.waitsForRecheck(report, source: source) {
            let judged = AppLifecycle.sourceQuit(.mixAppGone(bundleID: gone.bundleID, streaming: true), source: source,
                                                 setting: setting, isRunning: { running.contains($0) })
            return judged.map { "later \($0.action) \($0.bundleID)" } ?? "later keep"
        }
        let judged = AppLifecycle.sourceQuit(report, source: source, setting: setting, isRunning: { running.contains($0) })
        return judged.map { "\($0.action) \($0.bundleID)" } ?? "keep"
    }
    check(capturedQuit(.app(bundleID: podcasts), .stop) == "stop \(podcasts)"
            && capturedQuit(.app(bundleID: podcasts), .system) == "switchToSystem \(podcasts)"
            && capturedQuit(pair, .system, running: [musicID]) == "later keep"
            && capturedQuit(pair, .stop) == "later stop \(podcasts)",
          "source app quit: an app quit seen by the capture takes the terminate notice's path, a mix's judged a second later",
          "\(capturedQuit(.app(bundleID: podcasts), .stop)), \(capturedQuit(pair, .system, running: [musicID]))")
    // A paused app that quits later ends no audio process and Podcasts sends no notice, so a waiting capture looks again.
    let podcastsSource = AudioSource.app(bundleID: podcasts)
    func rechecks(_ source: AudioSource?, tapped: [AudioObjectID] = [], interrupted: Bool = false) -> Bool {
        AppTapFollow.rechecksWhileWaiting(source, tapped: tapped, interrupted: interrupted)
    }
    let idleRechecks = [rechecks(podcastsSource, tapped: [7]), rechecks(podcastsSource, interrupted: true), rechecks(nil),
                        rechecks(.system), rechecks(.mixed(.allExcept, bundleIDs: [podcasts])),
                        rechecks(.input(uid: "BuiltInMicrophoneDevice"))]
    check(AppLifecycle.waitRecheck == 5 && rechecks(podcastsSource) && rechecks(pair) && !idleRechecks.contains(true),
          "source app quit: only an app source waiting for an audio process looks for its app's quit every 5 s",
          "\(AppLifecycle.waitRecheck), \(idleRechecks)")
    func recheckTick(live: Set<pid_t>, _ setting: WhenSourceQuits) -> String {
        guard rechecks(podcastsSource) else { return "no recheck" }
        let pids = running(podcasts, listed: staleListing, seen: [702], live: live)
        let first = AppTapFollow.look(tapped: [], found: [], appRunning: !pids.isEmpty, secondLook: false)
        if first == .keep { return rechecks(podcastsSource) ? "waiting" : "stopped waiting" }
        guard first == .lookAgain else { return "tick: \(first)" }
        let second = AppTapFollow.look(tapped: [], found: [], appRunning: !pids.isEmpty, secondLook: true)
        guard second == .lost else { return "second look: \(second)" }
        guard let answer = AppLifecycle.sourceQuit(.captureLost(.appNotRunning(podcasts)), source: podcastsSource,
                                                   setting: setting, isRunning: { _ in false })
        else { return "keep" }
        return "\(answer.action) \(answer.bundleID)"
    }
    check(recheckTick(live: [702], .stop) == "waiting",
          "source app quit: a re-check of a paused app that still runs keeps the capture waiting", recheckTick(live: [702], .stop))
    check(recheckTick(live: [], .stop) == "stop \(podcasts)" && recheckTick(live: [], .system) == "switchToSystem \(podcasts)",
          "source app quit: a re-check that finds a paused app gone judges it as a quit, as a terminate notice would",
          "\(recheckTick(live: [], .stop)), \(recheckTick(live: [], .system))")
    func noticeQuit(_ report: Report) -> String {
        guard let answer = AppLifecycle.sourceQuit(report, source: pair, setting: .system, isRunning: { _ in false })
        else { return "keep" }
        return "\(answer.action) \(answer.bundleID)"
    }
    check(AppLifecycle.waitsForRecheck(.terminated(bundleID: "com.apple.Podcasts", streaming: true), source: pair)
            && noticeQuit(.mixAppGone(bundleID: "com.apple.Podcasts", streaming: true)) == "switchToSystem com.apple.podcasts"
            && AppLifecycle.sourceQuit(.terminated(bundleID: "COM.APPLE.MUSIC", streaming: true),
                                       source: .app(bundleID: "com.apple.Music"), setting: .stop,
                                       isRunning: { _ in false })?.action == .stop,
          "mix quit: macOS's quit notice matches the source's app in any letter case, as macOS compares bundle IDs (S164)",
          noticeQuit(.mixAppGone(bundleID: "com.apple.Podcasts", streaming: true)))
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
// covers: S111
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
    let hostRefusals: [CalibrationFailure] = [.microphoneDenied, .noMicrophone, .handsFreeMicrophone, .sourceSilent,
                                              .interrupted, .notPlaying, .noReference, .unsupportedRate]
    for failure in hostRefusals {
        let message = t.calibrationMessage(.failed(failure)) ?? ""
        check(message.hasSuffix(" The delay wasn't changed.") && message.count > " The delay wasn't changed.".count + 10,
              "calibration: a refusal (\(failure)) says why and that the delay wasn't changed", message)
    }

    // A hands-free default input is refused with its own reason, for one
    // speaker and for Calibrate All (both ask microphoneRefusal first).
    check(CalibrationFlow.microphoneRefusal(defaultInputTransport: kAudioDeviceTransportTypeBluetooth) == .handsFreeMicrophone
            && CalibrationFlow.microphoneRefusal(defaultInputTransport: kAudioDeviceTransportTypeBuiltIn) == nil
            && CalibrationFlow.microphoneRefusal(defaultInputTransport: kAudioDeviceTransportTypeUSB) == nil
            && CalibrationFlow.microphoneRefusal(defaultInputTransport: kAudioDeviceTransportTypeBluetoothLE) == nil
            && CalibrationFlow.microphoneRefusal(defaultInputTransport: nil) == nil
            && CalibrationFlow.microphoneRefusal(defaultInputTransport: kAudioDeviceTransportTypeAggregate,
                                                 inputSubTransports: [kAudioDeviceTransportTypeBluetooth]) == .handsFreeMicrophone
            && CalibrationFlow.microphoneRefusal(defaultInputTransport: kAudioDeviceTransportTypeAggregate,
                                                 inputSubTransports: [kAudioDeviceTransportTypeUSB]) == nil,
          "calibration: only a Bluetooth hands-free default input, or an aggregate with one, is refused")
    let handsFreeText = t.calibrationMessage(.failed(.handsFreeMicrophone)) ?? ""
    let handsFreeAll = t.calibrateAllLine(.ended(.failed(.handsFreeMicrophone)))
    check(handsFreeText.contains("hands-free") && handsFreeText.contains("Choose another input")
            && handsFreeText != t.calibrationMessage(.failed(.noMicrophone))
            && handsFreeAll.contains("hands-free") && handsFreeAll.hasSuffix(" No delay was changed.")
            && !handsFreeText.contains("\u{2014}"),
          "calibration: a hands-free microphone has its own reason, for one speaker and for Calibrate All",
          "\(handsFreeText) | \(handsFreeAll)")

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
    check(CalibrationFlow.blocker(streaming: true, reference: "mac", chosen: "usb", playing: ["mac", "usb"],
                                  referenceLeftOut: .headphones) == .referenceHeadphones
            && CalibrationFlow.blocker(streaming: true, reference: "mac", chosen: "usb", playing: ["mac", "usb"],
                                       referenceLeftOut: nil) == nil,
          "calibration: with the built-in output on the headphone jack one speaker's calibration doesn't start")

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

    // The window's + step asks for 20 010 ms, the web remote for the 20 000 ms already set.
    let stepped = { (raw: Double) in LocalDelay.sanitizedOffset(raw) ?? .nan }
    check(!CalibrationStatus.offsetDiffers(stored: 20_000, clamped: stepped(20_010))
            && !CalibrationStatus.offsetDiffers(stored: 20_000, clamped: stepped(20_000))
            && !CalibrationStatus.offsetDiffers(stored: nil, clamped: 0)
            && CalibrationStatus.offsetDiffers(stored: nil, clamped: 10)
            && CalibrationStatus.offsetDiffers(stored: 19_990, clamped: stepped(20_000)),
          "calibration: a delay step at the maximum leaves the offset, and its line, as they were")

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
        airPlayDelayMs: 1750, airPlayHoldBackMs: 0, airPlaySenderMs: 1750)
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

    // A sitting: a Samsung TV at L_s 1750 ms was missed with the AirPlay delay field at 0.
    var fieldAtZero = inputs
    fieldAtZero.cast = nil
    fieldAtZero.airPlayDelayMs = 0
    var noSender = fieldAtZero
    noSender.airPlaySenderMs = nil
    let atZero = (try? CalibrateAllFlow.plan(fieldAtZero).get())?.last
    let unknown = (try? CalibrateAllFlow.plan(noSender).get())?.last
    let windowMs = { (speaker: CalibrateAllSpeaker?) in
        (speaker?.latencyMs ?? 0) + CalibrateAllFlow.sweepSeconds * 1000 + 1000 }
    check(atZero?.kind == .airPlay && windowMs(atZero) >= 1750 + 750 + 1000 && atZero?.latencyMs == 2250
            && unknown?.latencyMs == 3500,
          "calibrate all: an AirPlay speaker at L=1750 with delay 0 gets a window of at least 1750 + 750 + 1000 ms, "
            + "from the sender's latency, or the path's 3000 ms bound while the sender reports none (S235)",
          "\(String(describing: atZero)) \(String(describing: unknown))")

    // covers: S235, S283
    var reportedWindow = fieldAtZero
    reportedWindow.airPlaySenderMs = 1750 + 220
    reportedWindow.airPlayReportedMs = [atZero?.id ?? "": 220]
    let reportedSpeaker = (try? CalibrateAllFlow.plan(reportedWindow).get())?.last
    var excessMs = 0.0
    let excessMoved = asc_local_native_adjustment_ms(1970 + 30, 1970, &excessMs)
    check(reportedSpeaker?.latencyMs == 1750 + 220 + 220 + 500 && excessMoved && excessMs == 30,
          "airplay latency: calibrate all measures relative to the reported latency (the window adds R_s on top of "
            + "L_s + E; the AirPlay delay moves only by the measured excess)",
          "\(String(describing: reportedSpeaker))")

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

    // Mic calibration is for external speakers only (decision 2026-10-23): headphones and virtual outputs sit out.
    let hdpn: UInt32 = 0x6864_706E, ispk: UInt32 = 0x6973_706B
    let bt = kAudioDeviceTransportTypeBluetooth, usb = kAudioDeviceTransportTypeUSB
    let builtIn = kAudioDeviceTransportTypeBuiltIn
    check(CalibrationFlow.leftOut(transport: kAudioDeviceTransportTypeVirtual, terminal: nil, dataSource: nil,
                                  name: "Microsoft Teams Audio") == .virtual
            && CalibrationFlow.leftOut(transport: builtIn, terminal: UInt32(kAudioStreamTerminalTypeSpeaker),
                                       dataSource: hdpn, name: "Built-in Output") == .headphones
            && CalibrationFlow.leftOut(transport: builtIn, terminal: UInt32(kAudioStreamTerminalTypeSpeaker),
                                       dataSource: ispk, name: "Built-in Output") == nil
            && CalibrationFlow.leftOut(transport: builtIn, terminal: UInt32(kAudioStreamTerminalTypeHeadphones),
                                       dataSource: nil, name: "External Headphones") == .headphones
            && CalibrationFlow.leftOut(transport: usb, terminal: 0x0302, dataSource: nil, name: "USB Audio") == .headphones
            && CalibrationFlow.leftOut(transport: usb, terminal: 0x0402, dataSource: nil, name: "USB Audio") == .headphones
            && CalibrationFlow.leftOut(transport: usb, terminal: 0x0301, dataSource: nil, name: "USB Audio") == nil
            && CalibrationFlow.leftOut(transport: usb, terminal: nil, dataSource: nil, name: "Desk Headphones") == nil
            && CalibrationFlow.leftOut(transport: bt, terminal: UInt32(kAudioStreamTerminalTypeHeadphones),
                                       dataSource: nil, name: "WH-1000XM5 Headphones") == .headphones
            && CalibrationFlow.leftOut(transport: bt, terminal: nil, dataSource: nil,
                                       name: "Living Room AirPods Pro") == .headphones
            && CalibrationFlow.leftOut(transport: kAudioDeviceTransportTypeBluetoothLE, terminal: 0, dataSource: nil,
                                       name: "Galaxy Buds2") == .headphones
            && CalibrationFlow.leftOut(transport: bt, terminal: nil, dataSource: nil, name: "JBL Flip 6") == nil
            && CalibrationFlow.leftOut(transport: kAudioDeviceTransportTypeAggregate, terminal: nil, dataSource: nil,
                                       name: "Multi-Output Device") == nil
            && CalibrationFlow.leftOut(transport: kAudioDeviceTransportTypeAirPlay, terminal: nil, dataSource: nil,
                                       name: "AirPlay") == nil
            && CalibrationFlow.leftOut(transport: 0, terminal: nil, dataSource: nil, name: "Headset") == nil,
          "calibration: a virtual output, the headphone jack, a headphone or headset terminal, and a Bluetooth "
            + "output named as headphones sit out; speakers, Bluetooth speakers, aggregates and AirPlay stay in")

    // A real soundcore Select 4 Go over A2DP reported the headphones terminal ('hdph') and no data source.
    let ble = kAudioDeviceTransportTypeBluetoothLE, hdph = UInt32(kAudioStreamTerminalTypeHeadphones)
    check(CalibrationFlow.leftOut(transport: 0x626C_7565, terminal: 0x6864_7068, dataSource: nil,
                                  name: "soundcore Select 4 Go") == nil
            && CalibrationFlow.leftOut(transport: ble, terminal: hdph, dataSource: nil,
                                       name: "soundcore Select 4 Go") == nil,
          "calibration: a Bluetooth speaker reporting the headphones terminal takes part")
    check(CalibrationFlow.leftOut(transport: bt, terminal: nil, dataSource: hdpn, name: "soundcore Select 4 Go") == nil
            && CalibrationFlow.leftOut(transport: ble, terminal: nil, dataSource: hdpn,
                                       name: "soundcore Select 4 Go") == nil
            && CalibrationFlow.leftOut(transport: bt, terminal: hdph, dataSource: hdpn, name: "AirPods Pro") == .headphones
            && CalibrationFlow.leftOut(transport: ble, terminal: hdph, dataSource: hdpn,
                                       name: "WH-1000XM5 Headphones") == .headphones
            && CalibrationFlow.leftOut(transport: builtIn, terminal: nil, dataSource: hdpn,
                                       name: "MacBook Pro Speakers") == .headphones
            && CalibrationFlow.leftOut(transport: usb, terminal: hdph, dataSource: nil, name: "USB Audio") == .headphones
            && CalibrationFlow.leftOut(transport: usb, terminal: 0x0402, dataSource: hdpn,
                                       name: "USB Audio") == .headphones,
          "calibration: over Bluetooth only the name decides, so a speaker reporting the headphone data source "
            + "takes part; elsewhere the jack and headphone terminals still sit out")

    let watchList = [
        AudioDevices.Device(id: 41, name: "Built-in Output", uid: "builtin", transport: builtIn),
        AudioDevices.Device(id: 42, name: "Desk", uid: "usb", transport: usb),
        AudioDevices.Device(id: 43, name: "AirPods", uid: "pods", transport: bt),
        AudioDevices.Device(id: 44, name: "External Headphones", uid: "jack", transport: builtIn),
    ]
    check(DataSourceWatch.watched(watchList) == [41, 44],
          "calibration: the built-in outputs' data source is watched, so plugging into the jack updates the window")

    var mixed = inputs
    mixed.localOutputs = [(id: "mac", name: "MacBook Speakers"), (id: "teams", name: "Microsoft Teams Audio"),
                          (id: "pods", name: "AirPods"), (id: "usb", name: "Desk")]
    mixed.leftOut = ["teams": .virtual, "pods": .headphones]
    let mixedPlan = (try? CalibrateAllFlow.plan(mixed).get()) ?? []
    check(mixedPlan.map(\.id) == ["mac", "usb", "cast:tv", "native:a", "native:b"]
            && CalibrateAllFlow.leftOut(mixed) == [
                CalibrateAllLeftOut(id: "teams", name: "Microsoft Teams Audio", reason: .virtual, included: false),
                CalibrateAllLeftOut(id: "pods", name: "AirPods", reason: .headphones, included: false)],
          "calibrate all: headphones and virtual outputs are listed with their reason and never swept",
          "\(mixedPlan.map(\.id)) \(CalibrateAllFlow.leftOut(mixed))")
    var anyway = mixed
    anyway.includedAnyway = ["pods"]
    let anywayPlan = (try? CalibrateAllFlow.plan(anyway).get()) ?? []
    check(anywayPlan.map(\.id) == ["mac", "pods", "usb", "cast:tv", "native:a", "native:b"]
            && CalibrateAllFlow.leftOut(anyway).map(\.included) == [false, true],
          "calibrate all: a row included anyway is measured like any other, in the window's order",
          "\(anywayPlan.map(\.id))")
    var onlyLeftOut = alone
    onlyLeftOut.localOutputs = [(id: "mac", name: "MacBook Speakers"), (id: "blackhole", name: "BlackHole 2ch"),
                                (id: "pods", name: "AirPods")]
    onlyLeftOut.leftOut = ["blackhole": .virtual, "pods": .headphones]
    var onlyLeftOutIncluded = onlyLeftOut
    onlyLeftOutIncluded.includedAnyway = ["pods"]
    var jack = inputs
    jack.leftOut = ["mac": .headphones]
    check(CalibrateAllFlow.plan(onlyLeftOut) == .failure(.onlyLeftOut)
            && (try? CalibrateAllFlow.plan(onlyLeftOutIncluded).get())?.map(\.id) == ["mac", "pods"]
            && CalibrateAllFlow.plan(alone) == .failure(.noOtherSpeaker)
            && CalibrateAllFlow.plan(jack) == .failure(.referenceHeadphones)
            && CalibrateAllFlow.leftOut(jack).isEmpty,
          "calibrate all: when every other speaker on sits out the blocker says so, not that only the built-in "
            + "speakers are on; with the headphone jack in use the reference itself is silent")

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
                == "The audio's sample rate changed during the measurement. No delay was changed."
            && t.calibrateAllLine(.cannotMeasureAgain(name: "Desk"))
                == "Turn on streaming, the built-in speakers and Desk to measure it again.",
          "calibrate all: a changed sample rate and a Measure Again that can't start say so in their own words")
    check(CalibrateAllEnd.delaysChanged.core == UInt32(ASC_CALIBRATION_END_CANCELLED)
            && CalibrateAllEnd.speakerOff.core == UInt32(ASC_CALIBRATION_END_SPEAKER_OFF)
            && t.calibrateAllLine(.ended(.sourceChanged))
                == "The source was switched during the measurement. No delay was changed.",
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
            // covers: S255
            let zoneSpeakers = [speaker("mac", .local, nil), speaker("usb", .local, 150), speaker("attic", .local, 40)]
            if let zoneRun = CalibrateAllRunHandle(rate: 48_000, speakers: zoneSpeakers, off: [false, false, true]) {
                let zoneSteps = play(zoneRun, arrivals: { row, _ in row == 0 ? 4_800 : 9_600 })
                let zoneSheet = zoneRun.finish()
                    .map { CalibrateAllSheetHandle($0, speakers: zoneSpeakers, rate: 48_000, microphone: nil) }
                let listed = zoneSheet?.read(ended: nil).rows ?? []
                // Asked while the row is still off: once measured, a plain Measure Again is allowed again.
                let plainRefused = zoneSheet?.measureAgain(2) == nil
                let onNow = CalibrateAllSpeaker(id: "attic", name: "attic", kind: .local, offsetMs: 60, latencyMs: 100)
                let again = zoneSheet?.measureAgainOff(2, now: onNow)
                let againSteps = again.map { play($0, arrivals: { row, _ in row == 0 ? 4_800 : 7_200 }) } ?? []
                let replaced = again?.finish().map { zoneSheet?.replace($0) ?? false } ?? false
                let after = zoneSheet?.read(ended: nil).rows ?? []
                check(zoneSteps == ["0.0", "1.1", "1.2"] && listed.count == 3 && listed[2].off
                        && listed[2].result == .notMeasured && plainRefused
                        && againSteps == ["0.0", "2.1", "2.2"] && replaced
                        && after.count == 3 && !after[2].off && after[2].oldMs == 60 && after[2].measured,
                      "calibrate zone: a member that is off is listed and never swept, and Measure Again measures it "
                        + "from its values once it is on (S255)", "\(zoneSteps), \(againSteps), \(after.map(\.result))")
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

    // Undo lasts only while every value Apply wrote is still stored (S238).
    var appliedUndo = undo
    appliedUndo.afterApply = [.local(id: "usb", ms: 100), .local(id: "hdmi", ms: 40), .cast(id: "cast:tv", ms: 900),
                              .cast(id: "cast:hall", ms: 300), .airPlay(AirPlayDelay(adjustmentMs: 215))]
    var manual = appliedUndo.afterApply
    manual[0] = .local(id: "usb", ms: 120)
    var cleared = appliedUndo.afterApply
    cleared[2] = .cast(id: "cast:tv", ms: nil)
    var nudged = appliedUndo.afterApply
    nudged[4] = .airPlay(AirPlayDelay(adjustmentMs: 240))
    check(CalibrateAllFlow.undoHolds(appliedUndo, now: appliedUndo.afterApply)
            && !CalibrateAllFlow.undoHolds(appliedUndo, now: manual)
            && !CalibrateAllFlow.undoHolds(appliedUndo, now: cleared)
            && !CalibrateAllFlow.undoHolds(appliedUndo, now: nudged),
          "calibrate all: a delay changed by hand after Apply drops Undo")
    var written = AirPlayDelay(), senderMoved = AirPlayDelay(), sameField = AirPlayDelay()
    let entered = written.enter(1965, senderMs: 1750) && senderMoved.enter(2215, senderMs: 2000)
        && sameField.enter(1965, senderMs: 2000)
    var airPlayUndo = undo
    airPlayUndo.afterApply = [.airPlay(written)]
    check(entered && senderMoved.latencyMs(senderMs: 2000) != written.latencyMs(senderMs: 1750)
            && CalibrateAllFlow.undoHolds(airPlayUndo, now: [.airPlay(senderMoved)])
            && !CalibrateAllFlow.undoHolds(airPlayUndo, now: [.airPlay(sameField)]),
          "calibrate all: a new AirPlay sender latency alone keeps Undo; the same field value typed at it drops Undo",
          "\(written) \(senderMoved) \(sameField)")
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

// MARK: Calibrate Zone: the zone's plan, the speakers silenced outside it and the AirPlay line (S255)
do {
    let inputs = CalibrateAllFlow.Inputs(
        streaming: true, reference: "mac",
        localOutputs: [(id: "usb", name: "Desk"), (id: "mac", name: "MacBook Speakers")],
        localOffsets: ["usb": 150], holdBacks: ["mac": 1750, "usb": 1900],
        cast: (id: "cast:tv", name: "TV", offsetMs: 500, waitMs: 0, format: ASC_CAST_FORMAT_WAV),
        airPlay: [(id: "native:a", name: "Kitchen"), (id: "native:b", name: "Bedroom")],
        airPlayDelayMs: 1750, airPlayHoldBackMs: 0, airPlaySenderMs: 1750)
    let attic = CalibrateAllSpeaker(id: "attic", name: "Attic", kind: .local, offsetMs: 40, latencyMs: 0)
    let scope = CalibrateZoneScope(id: "z1", name: "Upstairs",
                                   members: ["ap:native:b", "lc:usb", "lc:attic", "lc:hidden", "lc:usb"],
                                   present: ["ap:native:b", "lc:usb", "lc:attic"], off: [attic])
    let plan = try? CalibrateAllFlow.zonePlan(inputs, zone: scope).get()
    // covers: S255
    check(plan?.speakers.map(\.id) == ["mac", "native:b", "usb", "attic"] && plan?.off == [false, false, false, true]
            && plan?.taking == ["mac", "native:b", "usb"] && plan?.silenced.map(\.id) == ["cast:tv", "native:a"],
          "calibrate zone: the reference, then the zone's present members in zone order, one that is off listed, "
            + "and every other speaker that plays silenced (S255)", "\(String(describing: plan))")

    var onlyOff = scope
    onlyOff.members = ["lc:attic"]
    var hiddenOnly = scope
    hiddenOnly.members = ["lc:hidden"]
    var noReference = inputs
    noReference.reference = nil
    // covers: S255
    check(CalibrateAllFlow.zonePlan(inputs, zone: onlyOff) == .failure(.noOtherSpeaker)
            && CalibrateAllFlow.zonePlan(inputs, zone: hiddenOnly) == .failure(.noOtherSpeaker)
            && CalibrateAllFlow.zonePlan(noReference, zone: scope) == .failure(.noReference),
          "calibrate zone: a zone with only the reference measurable is refused as nothing to measure (S255)")

    // covers: S255
    let silenced = plan?.silenced ?? []
    let usb = plan?.speakers[2]
    let far = CalibrateAllSpeaker(id: "hdmi", name: "TV speakers", kind: .local, offsetMs: 0, latencyMs: 5000)
    check(usb.map { CalibrateAllFlow.settleSeconds($0, silenced: silenced) } == usb.map { CalibrateAllFlow.settleSeconds($0) }
            && usb.map { CalibrateAllFlow.settleSeconds($0, silenced: silenced + [far]) } == 5.75
            && CalibrateAllFlow.quietSeconds((plan?.speakers ?? []) + silenced) >= CalibrateAllFlow.playOutSeconds(silenced[0]),
          "calibrate zone: the run waits until the speakers silenced on a sweep's path have played it out (S255)")

    let zones = [SpeakerZone(id: "z1", name: "Upstairs", members: ["ap:native:b", "lc:usb"], volume: 100),
                 SpeakerZone(id: "z2", name: "Kitchen", members: ["ap:native:a"], volume: 100),
                 SpeakerZone(id: "z3", name: "Desk", members: ["lc:desk"], volume: 100),
                 SpeakerZone(id: "z4", name: "Garden", members: ["ap:native:g"], volume: 100)]
    let moves = CalibrateAllFlow.airPlayMoves(zones: zones, measured: "z1",
                                              listed: ["ap:native:g", "ap:native:a", "ap:native:b"])
    let porch = CalibrateAllFlow.airPlayMoves(zones: zones, measured: "z2", listed: ["ap:native:a", "ap:native:p"])
    // covers: S255, S237
    check(moves == CalibrateZoneAirPlayMoves(zones: ["Kitchen", "Garden"], noZone: false)
            && porch == CalibrateZoneAirPlayMoves(zones: [], noZone: true),
          "calibrate zone: the AirPlay line names, in zone order, each other zone with a listed AirPlay speaker, "
            + "and speakers in no zone (S255)", "\(moves) | \(porch)")

    let zoneSheet = CalibrateZoneSheet(id: "z1", name: "Upstairs", airPlayMoves: moves)
    let opened = CalibrateAllStatus(shown: true, zone: zoneSheet)
    let closed = CalibrateAllStatus(shown: false, zone: zoneSheet)
    let headphones = CalibrateAllLeftOut(id: "pods", name: "AirPods", reason: .headphones, included: false)
    let member = CalibrateAllLeftOut(id: "usb", name: "Desk", reason: .headphones, included: false)
    // covers: S255
    check(CalibrateAllFlow.startZone(opened) == "z1" && CalibrateAllFlow.startZone(closed) == nil
            && CalibrateAllFlow.startZone(CalibrateAllStatus(shown: true)) == nil
            && !CalibrateAllFlow.previewsLeftOut([headphones], zone: scope)
            && CalibrateAllFlow.previewsLeftOut([headphones, member], zone: scope)
            && CalibrateAllFlow.previewsLeftOut([headphones], zone: nil),
          "calibrate zone: the open sheet's button runs its zone again, never a whole run, and lists the outputs "
            + "that sit out first only when one is in the zone (S255)")

    var started = CalibrateAllContext(source: "system", session: 3, streaming: true, switchedOn: ["mac", "usb"],
                                      playing: ["mac", "usb"], delays: [:])
    started.zoneMembers = ["lc:usb", "lc:attic"]
    var lost = started
    lost.zoneMembers = ["lc:usb"]
    var deleted = started
    deleted.zoneMembers = nil
    var grown = started
    grown.zoneMembers = ["lc:usb", "lc:attic", "lc:desk"]
    var whole = started
    whole.zoneMembers = nil
    let ends = [started, lost, deleted, grown].map { CalibrateAllFlow.end(start: started, now: $0, taking: ["mac", "usb"]) }
    // covers: S240, S255
    check(ends == [nil, .speakersChanged, .speakersChanged, nil]
            && CalibrateAllFlow.end(start: whole, now: whole, taking: ["mac", "usb"]) == nil,
          "calibrate zone: the measured zone losing a member or being deleted stops the run as a changed list (S240)",
          "\(ends)")

    var measuring = opened
    measuring.progress = .listening(name: "Desk", sweep: 1)
    var playingOut = opened
    playingOut.holdsOutputs = true
    let offers = [CalibrateAllFlow.offersZoneRun(opened, singleHoldsOutputs: false, present: 2),
                  CalibrateAllFlow.offersZoneRun(opened, singleHoldsOutputs: false, present: 0),
                  CalibrateAllFlow.offersZoneRun(measuring, singleHoldsOutputs: false, present: 2),
                  CalibrateAllFlow.offersZoneRun(playingOut, singleHoldsOutputs: false, present: 2),
                  CalibrateAllFlow.offersZoneRun(opened, singleHoldsOutputs: true, present: 2)]
    // covers: S255
    check(offers == [true, false, false, false, false],
          "calibrate zone: the menu item is off while no member is here or a calibration holds the outputs (S255)",
          "\(offers)")

    let zoneText = UIStrings()
    let lines = [CalibrateZoneAirPlayMoves(zones: [], noZone: false), CalibrateZoneAirPlayMoves(zones: [], noZone: true),
                 CalibrateZoneAirPlayMoves(zones: ["Kitchen"], noZone: false), moves,
                 CalibrateZoneAirPlayMoves(zones: ["Kitchen", "Garden"], noZone: true)]
        .map(zoneText.calibrateZoneAirPlayMoves)
    let share = "AirPlay speakers share one delay, so a new AirPlay delay applies to "
    // covers: S255
    check(lines == [nil, share + "the AirPlay speakers in no zone too.",
                    share + "the AirPlay speakers in “Kitchen” too.",
                    share + "the AirPlay speakers in “Kitchen” and “Garden” too.",
                    share + "the AirPlay speakers in “Kitchen” and “Garden”, and those in no zone too."]
            && zoneText.calibrateZoneItem == "Calibrate Zone…"
            && zoneText.calibrateZoneBring("Upstairs").hasPrefix("Bring the Mac into “Upstairs”"),
          "calibrate zone: the sheet says where to bring the Mac and which other zones the AirPlay delay moves (S255)",
          "\(lines)")

    let nothing = CalibrateAllFlow.startFailure(.noOtherSpeaker, zone: "Upstairs")
    // covers: S255
    check(nothing == .nothingToMeasure(zone: "Upstairs")
            && CalibrateAllFlow.startFailure(.noOtherSpeaker, zone: nil) == .notPlaying
            && CalibrateAllFlow.startFailure(.notPlaying, zone: "Upstairs") == .notPlaying
            && CalibrateAllFlow.startFailure(.onlyLeftOut, zone: "Upstairs") == nil
            && nothing.map(zoneText.calibrationFailure)
                == "Nothing to measure in “Upstairs”. Turn on a speaker in this zone first.",
          "calibrate zone: a zone with only the reference measurable says there is nothing to measure (S255)",
          "\(String(describing: nothing))")

    let afterDelete = CalibrateAllFlow.deletingZone("z1", from: opened)
    // covers: S255
    check(afterDelete.zone == nil && afterDelete.shown && CalibrateAllFlow.startZone(afterDelete) == nil
            && CalibrateAllFlow.deletingZone("z2", from: opened) == opened,
          "calibrate zone: deleting the measured zone turns its sheet into Calibrate All's, whose button runs "
            + "every speaker (S255)")

    let tv = CalibrateAllSpeaker(id: "tv", name: "TV", kind: .local, offsetMs: nil, latencyMs: 0)
    let offRow = CalibrateAllSheet.Row(index: 1, speaker: tv, oldMs: nil, result: .notMeasured, off: true)
    let skipped = CalibrateAllSheet.Row(index: 1, speaker: tv, oldMs: nil, result: .notMeasured)
    // covers: S255
    check(zoneText.calibrateAllResult(offRow) == "Off, so it wasn't measured. Turn it on and choose Measure Again."
            && zoneText.calibrateAllResult(skipped) == "Not measured.",
          "calibrate zone: a member that was off says Off on its row, not Not measured (S255)",
          zoneText.calibrateAllResult(offRow))
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
                                               flags: UInt32(ASC_CAST_FLAG_GROUP), former: nil),
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
    check(t.castList(.noneFound, localNetworkRefused: true)?.contains("Local Network") == true
            && t.castList(.noneFound)?.contains("Local Network") == false,
          "cast: none found points to Local Network access only once it was refused")
    let problems: [CastProblem] = [.busy, .notResponding, .refused, .blocked, .takenOver, .connectionLost]
    check(problems.allSatisfy { let text = t.castProblem($0, name: "Den"); return text.contains("Den") && !text.contains("\u{2014}") },
          "cast: every notice names the speaker, with no em dash")
    let bothNone = t.networkSpeakersNote(airPlayNone: true, cast: .noneFound) ?? ""
    check(bothNone.hasPrefix("No AirPlay or Google Cast speakers found yet.") && bothNone.contains("same network")
            && !bothNone.contains("Local Network") && !t.airPlayNoneFound().contains("Local Network")
            && t.airPlayNoneFound(localNetworkRefused: true).contains("Local Network")
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
                                     hint: LocalNetworkHint.denied.shown(airPlayProblem: true),
                                     localNetworkRefused: true)?.contains("Local Network") == true
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

// MARK: Google Cast and the master volume (C104)
do {
    let tv = "00112233445566778899aabbccddeeff"
    func reported(_ level: Double, fixed: Bool = false) -> CastVolume {
        CastVolume(level: level, muted: false, fixed: fixed)
    }
    func near(_ level: Double?, _ expected: Double) -> Bool { level.map { approx($0, expected, 1e-9) } == true }
    func session(master: Double, attempt: UInt64) -> CastModel {
        var m = CastModel()
        m.apply(.appeared(CastDevice(id: tv, name: "Living room TV"), offsetMs: nil))
        check(m.setMaster(master) == nil, "cast master: nothing is sent while no session runs")
        _ = m.turnOn(tv)
        m.startSent(tv)
        _ = m.started(tv, attempt: attempt, refusal: ASC_CAST_REFUSED_NONE)
        return m
    }

    var m = session(master: 50, attempt: 1)
    check(near(m.setVolume(tv, position: 80), 0.4) && approx(m.volume(of: tv), 80, 1e-9),
          "cast master: master 50 with the speaker at 80 sends level 0.4 and the slider shows 80")
    check(m.setMaster(0) == 0 && approx(m.volume(of: tv), 80, 1e-9),
          "cast master: master 0 sends level 0, the speaker keeps 80")
    _ = m.apply(.volume(attempt: 1, reported(0)))
    check(approx(m.volume(of: tv), 80, 1e-9), "cast master: the device answering level 0 leaves the speaker at 80")
    check(near(m.setMaster(50), 0.4), "cast master: a master change resends master × speaker")
    _ = m.apply(.volume(attempt: 1, reported(0.4)))
    check(m.apply(.volume(attempt: 1, reported(0.2))) == .changed && approx(m.volume(of: tv), 40, 1e-9),
          "cast master: level 0.2 set elsewhere at master 50 moves the speaker to 40, nothing sent back")
    _ = m.setMaster(0)
    _ = m.apply(.volume(attempt: 1, reported(0)))
    check(m.apply(.volume(attempt: 1, reported(0.3))) != .setVolume(0) && approx(m.volume(of: tv), 40, 1e-9),
          "cast master: at master 0 a level set elsewhere leaves the speaker's level and isn't fought")
    check(m.stepVolume(tv, up: true) == .level(0), "cast master: a step at master 0 sends level 0")

    var off = session(master: 0, attempt: 2)
    check(off.apply(.volume(attempt: 2, reported(0.7))) == .setVolume(0) && approx(off.volume(of: tv), 70, 1e-9),
          "cast master: a speaker turned on at master 0 is sent level 0 at its first status")
    _ = off.apply(.volume(attempt: 2, reported(0)))
    check(off.apply(.volume(attempt: 2, reported(0.5))) != .setVolume(0),
          "cast master: only the first status of a session is answered")

    var loud = session(master: 50, attempt: 3)
    check(loud.apply(.volume(attempt: 3, reported(0.8))) == .setVolume(0.5) && approx(loud.volume(of: tv), 100, 1e-9),
          "cast master: a device louder than the master allows comes down once at the start")
    _ = loud.apply(.volume(attempt: 3, reported(0.5, fixed: true)))
    check(loud.setMaster(20) == nil, "cast master: a fixed volume gets nothing (C68)")

    var still = session(master: 100, attempt: 8)
    _ = still.apply(.volume(attempt: 8, reported(0.5)))
    _ = still.setVolume(tv, position: 0)
    _ = still.apply(.volume(attempt: 8, reported(0)))
    for position in [60.0, 50, 40] { _ = still.setMaster(position) }
    _ = still.apply(.volume(attempt: 8, reported(0.3)))
    _ = still.apply(.volume(attempt: 8, reported(0)))
    check(still.volume(of: tv) == 0 && still.setMaster(100) == 0,
          "cast master: a level the device already has is not waited for, so its buttons' 0 is read back and a muted speaker stays silent")

    var quiet = session(master: 100, attempt: 5)
    _ = quiet.apply(.volume(attempt: 5, reported(0.5)))
    _ = quiet.setVolume(tv, position: 80)
    _ = quiet.apply(.volume(attempt: 5, reported(0.8)))
    quiet.stopSent()
    quiet.startSent(tv)
    _ = quiet.started(tv, attempt: 6, refusal: ASC_CAST_REFUSED_NONE)
    check(quiet.apply(.volume(attempt: 6, reported(0.1))) == .setVolume(0.8) && approx(quiet.volume(of: tv), 80, 1e-9),
          "cast master: a session starts at master × own level when the device is quieter, not read back (C105)")
    quiet.stopSent()
    quiet.startSent(tv)
    _ = quiet.started(tv, attempt: 7, refusal: ASC_CAST_REFUSED_NONE)
    check(quiet.apply(.volume(attempt: 7, reported(0.1, fixed: true))) != .setVolume(0.8),
          "cast master: a fixed volume is not set at the start (C68)")

    var group = CastModel()
    group.apply(.appeared(CastDevice(id: "group1", name: "Everywhere", group: true), offsetMs: nil))
    _ = group.setMaster(50)
    _ = group.turnOn("group1")
    group.startSent("group1")
    _ = group.started("group1", attempt: 4, refusal: ASC_CAST_REFUSED_NONE)
    check(near(group.setVolume("group1", position: 80), 0.4) && near(group.setMaster(100), 0.8),
          "cast master: a group follows the master like a device")

    check(approx(asc_cast_device_level(0.5, 0.8), 0.4, 1e-12) && asc_cast_device_level(0, 0.8) == 0
            && approx(asc_cast_own_level(0.2, 0.5, 0.9), 0.4, 1e-12) && asc_cast_own_level(0.2, 0, 0.7) == 0.7
            && asc_cast_start_own_level(1, true, 0.8, 0.1) == 0.8 && approx(asc_cast_start_own_level(0.5, false, 0, 0.2), 0.4, 1e-12)
            && asc_cast_level_answers(0.4, 0.404) && !asc_cast_level_answers(0.4, 0.41),
          "cast master: the core's arithmetic")
}

// MARK: Zone volumes (S71, S249, S250, C104, C105)
do {
    let zones = [SpeakerZone(id: "z1", name: "Kitchen", members: ["lc:usb:out", "ap:AA", "cc:tv1"], volume: 50),
                 SpeakerZone(id: "z2", name: "Den", members: ["ap:BB", "ap:AA"], volume: 25)]
    check(SpeakerZone.volume(of: "lc:usb:out", in: zones) == 50 && SpeakerZone.volume(of: "ap:BB", in: zones) == 25
            && SpeakerZone.volume(of: "lc:other", in: zones) == 100,
          "zones: each speaker takes its zone's volume, 100 in no zone")
    check(SpeakerZone.volumes(zones, kind: .airplay) == ["AA": 50, "BB": 25]
            && SpeakerZone.volumes(zones, kind: .local) == ["usb:out": 50]
            && SpeakerZone.volumes(zones, kind: .cast) == ["tv1": 50],
          "zones: each kind's members get their zone's volume by ID")

    var identical = true
    for own in stride(from: 0.0, through: 100, by: 7.5) {
        for master in stride(from: 0.0, through: 100, by: 12.5) {
            identical = identical
                && asc_zone_output_gain(own, 100, master).bitPattern == asc_output_gain(own, master).bitPattern
                && asc_cast_zone_level(master / 100, 1, own / 100).bitPattern
                    == asc_cast_device_level(master / 100, own / 100).bitPattern
        }
    }
    check(identical && asc_zone_output_gain(50, 50, 100) == 0.125 * 0.125 && asc_zone_output_gain(80, 0, 100) == 0,
          "zones: a local speaker's gain goes through its zone, and no zone is the gain of before")

    let tv = "tv1"
    func reported(_ level: Double) -> CastVolume { CastVolume(level: level, muted: false, fixed: false) }
    func near(_ level: Double?, _ expected: Double) -> Bool { level.map { approx($0, expected, 1e-9) } == true }
    func session(zone: Double, attempt: UInt64) -> CastModel {
        var m = CastModel()
        m.apply(.appeared(CastDevice(id: tv, name: "Living room TV"), offsetMs: nil))
        check(m.setZones([tv: zone]) == nil, "zones: nothing is sent to a Cast speaker while no session runs")
        _ = m.turnOn(tv)
        m.startSent(tv)
        _ = m.started(tv, attempt: attempt, refusal: ASC_CAST_REFUSED_NONE)
        return m
    }

    var m = session(zone: 50, attempt: 1)
    check(near(m.setVolume(tv, position: 80), 0.4) && approx(m.volume(of: tv), 80, 1e-9),
          "zones: the Cast level is master × zone × own")
    check(near(m.setZones([tv: 25]), 0.2) && m.setZones([tv: 25]) == nil,
          "zones: a zone volume change gives the Cast speaker in a session its new level, once")
    _ = m.apply(.volume(attempt: 1, reported(0.2)))
    check(m.apply(.volume(attempt: 1, reported(0.1))) == .changed && approx(m.volume(of: tv), 40, 1e-9),
          "zones: a Cast report in a zone moves the own level by master times zone")
    _ = m.setZones([tv: 0])
    _ = m.apply(.volume(attempt: 1, reported(0)))
    _ = m.apply(.volume(attempt: 1, reported(0.3)))
    check(approx(m.volume(of: tv), 40, 1e-9), "zones: with the zone at 0 a Cast report leaves the own level")

    var fresh = session(zone: 50, attempt: 2)
    check(fresh.apply(.volume(attempt: 2, reported(0.2))) != .setVolume(0.2) && approx(fresh.volume(of: tv), 40, 1e-9),
          "zones: a Cast session's first status divides by master times zone (C105)")
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

// MARK: Guide text: hand-wrapped lines join into paragraphs the window wraps (GuideLayout)
do {
    let t = UIStrings()
    let paragraphs = GuideLayout.paragraphs(of: t.helpText)
    let texts = paragraphs.map(\.text)
    let titles = t.helpText.split(separator: "\n").prefix(2).map(String.init)
    let topics = ["First Steps", "More Controls", "Calibration", "Speaker Notes", "AirPlay", "Google Cast",
                  "Notices", "Web Remote", "Tips", "Volume", "Notes"]
    let unfinished = texts.filter { text in !text.isEmpty && !(text.last.map { ".:!?".contains($0) } ?? false) }
    check(unfinished == titles + topics,
          "guide: only the title and topic headings end without punctuation, so no paragraph breaks inside a sentence",
          unfinished.joined(separator: " | "))
    check(!texts.contains { $0.contains("\n") || $0.contains("  ") }
            && texts.joined(separator: " ").split(separator: " ") == t.helpText.split { $0 == " " || $0 == "\n" },
          "guide: no paragraph keeps a hard break or a wrap's indent, and every word stays in order")
    let steps = paragraphs.filter { $0.text.first?.isNumber == true }
    check(steps.map(\.hang) == ["1. ", "2. ", "3. ", "4. ", "5. "]
            && paragraphs.filter { $0.text.hasPrefix("- ") }.allSatisfy { $0.hang == "- " }
            && texts.filter(\.isEmpty).count == t.helpText.components(separatedBy: "\n").filter(\.isEmpty).count,
          "guide: steps and list items indent past their marker, and the blank lines between topics stay",
          "\(steps.map(\.hang))")

    let roles = GuideLayout.roles(of: paragraphs)
    let headings = zip(paragraphs, roles).filter { $0.1 == .heading }.map { $0.0.text }
    check(roles.first == .title && roles.filter { $0 == .title }.count == 1 && headings == topics
            && roles.count > 1 && roles[1] == .body,
          "guide: the title and the topic headings are set apart, and the byline stays body text",
          headings.joined(separator: " | "))
    check(GuideLayout.roles(of: [.init(text: "Notices", hang: ""), .init(text: "Text.", hang: "")], opensText: false)
            == [.heading, .body],
          "guide: a piece after an illustration that opens with a topic sets it as a heading")

    let made = GuideLayout.paragraphs(of: "Topic\nOne two\nthree.\n\n1. Step one\n   wraps\n- Item\n  goes on\nAfter")
    check(made == [.init(text: "Topic", hang: ""), .init(text: "One two three.", hang: ""), .init(text: "", hang: ""),
                   .init(text: "1. Step one wraps", hang: "1. "), .init(text: "- Item goes on", hang: "- "),
                   .init(text: "After", hang: "")],
          "guide: a heading stands alone, wrapped lines join, and a line back at the margin leaves the list", "\(made)")
}

// MARK: Guide topics: Calibration has its own topic, with the actions it names
do {
    let t = UIStrings()
    let lines = t.helpText.split(separator: "\n", omittingEmptySubsequences: false).map(String.init)
    let start = lines.firstIndex(of: "Calibration")
    let end = lines.firstIndex(of: "Speaker Notes")
    let topic = start.flatMap { s in end.map { e in s < e ? lines[s..<e].joined(separator: "\n") : "" } } ?? ""
    let named = [t.calibrateWithMicrophone, t.calibrateAll, t.includeAnyway, t.measureAgain,
                 t.calibrateAllApply, t.calibrateAllUndo]
    check(!topic.isEmpty && named.allSatisfy(topic.contains),
          "guide: a Calibration topic before Speaker Notes names Calibrate All Speakers, Include Anyway and the rest",
          named.filter { !topic.contains($0) }.joined(separator: ", "))
    check(topic.contains("Headphones") && topic.contains("virtual outputs")
            && topic.contains("Bluetooth speakers take part"),
          "guide: the Calibration topic says headphones and virtual outputs are left out and Bluetooth speakers stay in")
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

    let entries = SourceList.entries(systemName: "All System Audio", apps: [(bundleID: "com.apple.Music", name: "Music")],
                                     inputs: [SourceList.Input(uid: "BuiltInMic", name: "MacBook Microphone",
                                                               transport: kAudioDeviceTransportTypeBuiltIn)],
                                     chosen: .app(bundleID: "com.apple.Music"), chosenAppName: { $0 })
    let remoteSources = entries.map { RemoteSource(pickerID: $0.id, name: $0.name) }
    check(remoteSources.map(\.id) == ["system", "app:com.apple.Music", "input:BuiltInMic"]
            && remoteSources.map(\.kind) == [.system, .app, .input] && remoteSources.map(\.name) == entries.map(\.name),
          "web remote: the page lists the picker's entries in its order, each with an ID the core takes")
    check(zip(remoteSources, entries).allSatisfy { RemoteSource.pickerID($0.id) == $1.id }
            && ["", "app:", "input:", "Music"].allSatisfy { RemoteSource.pickerID($0) == nil },
          "web remote: a page ID maps back to the picker's ID; one that names no source maps to nothing")
    let sourced = RemoteSnapshot(computerName: "Studio Mac", master: 80, nowPlaying: nil, rows: rows,
                                 sources: remoteSources, sourceID: "app:com.apple.Music")
    let sourcedObject = (try? JSONSerialization.jsonObject(with: Data(sourced.json.utf8))) as? [String: Any]
    let sourceKinds = (sourcedObject?["sources"] as? [[String: Any]])?.compactMap { $0["kind"] as? String }
    check(sourcedObject?["source_id"] as? String == "app:com.apple.Music" && sourceKinds == ["system", "app", "input"]
            && snapshot.json.contains("\"source_id\":null"),
          "web remote: the snapshot carries the sources and the one in use", sourced.json)
    func pick(_ id: String) -> [RemoteAction] {
        RemoteCommand(json: "{\"type\":\"source\",\"id\":\"\(id)\"}").map {
            RemoteAction.plan($0, rows: rows, sources: remoteSources, sourceID: "app:com.apple.Music")
        } ?? []
    }
    check(pick("input:BuiltInMic") == [.selectSource(pickerID: "input:BuiltInMic")]
            && pick("system") == [.selectSource(pickerID: "")]
            && pick("app:com.apple.Music").isEmpty && pick("app:gone").isEmpty
            && RemoteCommand(json: "{\"type\":\"source\",\"id\":3}") == nil,
          "web remote: a source pick is the picker's own; the one in use or one it doesn't list changes nothing")
    let except = AudioSource.mixed(.allExcept, bundleIDs: ["com.spotify.client", "com.google.Chrome"])
    let mixEntries = SourceList.entries(systemName: "All System Audio",
                                        apps: [(bundleID: "com.spotify.client", name: "Spotify")], inputs: [],
                                        chosen: except, chosenAppName: { _ in "Chrome" },
                                        mixName: { "All apps except " + $1.joined(separator: " and ") })
    let mixSources = mixEntries.map { RemoteSource(pickerID: $0.id, name: $0.name) }
    let mixSource = mixSources.first { $0.id == except.defaultsValue }
    let onlyID = AudioSource.mixed(.only, bundleIDs: ["com.spotify.client", "com.google.Chrome"]).defaultsValue
    check(mixSource?.kind == .app && mixSource?.name == "All apps except Spotify and Chrome"
            && zip(mixSources, mixEntries).allSatisfy { RemoteSource.pickerID($0.id) == $1.id }
            && RemoteSource.pickerID(RemoteSource.pageID(onlyID)) == onlyID
            && ["apps:", "except:"].allSatisfy { RemoteSource.pickerID($0) == nil },
          "web remote: a several-apps source in use is listed under apps with the picker's name and maps back",
          mixSources.map(\.id).joined(separator: " "))
    let mixPick = RemoteCommand(json: "{\"type\":\"source\",\"id\":\"\(except.defaultsValue)\"}")
    func planMix(from sourceID: String) -> [RemoteAction]? {
        mixPick.map { RemoteAction.plan($0, rows: rows, sources: mixSources, sourceID: sourceID) }
    }
    check(planMix(from: except.defaultsValue) == [RemoteAction]()
            && planMix(from: "system") == [RemoteAction.selectSource(pickerID: except.defaultsValue)],
          "web remote: a pick of the several-apps source in use changes nothing; from another source it selects it")

    // The core's own parser takes the snapshot.
    let noCommand: AscRemoteCommand = { _, _ in }
    var error: Int32 = 0
    var remote = asc_remote_start("{\"port\": 0, \"listen\": [\"127.0.0.1\"]}", noCommand, nil, &error)
    check(remote != nil && asc_remote_publish(remote, snapshot.json) && asc_remote_publish(remote, playing)
            && asc_remote_publish(remote, sourced.json),
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

    // covers: S256, S253
    let zoneRows = [
        RemoteRow(Facts(id: "usb", name: "Desk", kind: "USB", on: true, volume: 40, key: "lc:usb"), text: t),
        RemoteRow(Facts(id: "hp", name: "Kitchen", kind: "AirPlay 2", on: true, volume: 30, isAirPlay: true,
                        key: "ap:hp", waiting: true), text: t),
        RemoteRow(Facts(id: "bt", name: "Den", kind: "Bluetooth", on: false, volume: 50, key: "lc:bt"), text: t),
    ]
    let storedZones = [
        SpeakerZone(id: "z1", name: "Downstairs", members: ["ap:hp", "lc:hidden", "lc:usb", "lc:bt"], volume: 70),
        SpeakerZone(id: "z2", name: "Studio", members: ["lc:hidden"], volume: 100),
    ]
    let zoneList = storedZones.map { RemoteZoneRow($0, listed: zoneRows) }
    let presetList = [RemotePresetRow(id: "p1", name: "Evening", current: true),
                      RemotePresetRow(id: "p2", name: "Party", current: false)]
    let zoned = RemoteSnapshot(computerName: "Mac", master: 100, nowPlaying: nil, rows: zoneRows,
                               zones: zoneList, presets: presetList)
    let zonedObject = (try? JSONSerialization.jsonObject(with: Data(zoned.json.utf8))) as? [String: Any]
    let zoneObjects = zonedObject?["zones"] as? [[String: Any]] ?? []
    let presetObjects = zonedObject?["presets"] as? [[String: Any]] ?? []
    check(zoneObjects.first?["members"] as? [String] == ["hp", "usb", "bt"]
            && zoneObjects.first?["counts"] as? [String: Int] == ["on": 2, "total": 3, "waiting": 1]
            && zoneObjects.first?["volume"] as? Double == 70
            && zoneObjects.last?["counts"] as? [String: Int] == ["on": 0, "total": 0, "waiting": 0],
          "web remote: a zone lists its listed members in zone order with on, total and waiting; a hidden one doesn't count",
          "\(zoneObjects)")
    check(presetObjects.map { $0["id"] as? String ?? "" } == ["p1", "p2"]
            && presetObjects.map { $0["current"] as? Bool ?? false } == [true, false]
            && zonedObject?["calibrating"] as? Bool == false,
          "web remote: the presets go out in order with the current one marked, and no run means not calibrating")
    var zonedRemote = asc_remote_start("{\"port\": 0, \"listen\": [\"127.0.0.1\"]}", noCommand, nil, &error)
    check(zonedRemote != nil && asc_remote_publish(zonedRemote, zoned.json),
          "web remote: the core accepts a snapshot with zones and presets", "start error \(error)")
    asc_remote_stop(&zonedRemote)

    // covers: S257
    func zonePlan(_ json: String, calibrating: Bool = false) -> [RemoteAction] {
        RemoteCommand(json: json).map {
            RemoteAction.plan($0, rows: zoneRows, zones: zoneList, presets: presetList, calibrating: calibrating)
        } ?? []
    }
    let zoneCommand = "{\"type\":\"zone\",\"id\":\"z1\",\"toggle\":true,\"volume\":30}"
    let presetCommand = "{\"type\":\"preset\",\"id\":\"p2\",\"recall\":true}"
    check(RemoteCommand(json: zoneCommand) == .zone(id: "z1", toggle: true, volume: 30)
            && RemoteCommand(json: presetCommand) == .preset(id: "p2")
            && RemoteCommand(json: "{\"type\":\"preset\",\"id\":\"p2\",\"recall\":false}") == nil
            && RemoteCommand(json: "{\"type\":\"zone\",\"id\":\"z1\",\"toggle\":1}") == nil,
          "web remote: zone and preset commands read as the core sends them; anything else is dropped")
    check(zonePlan(zoneCommand) == [.setZoneVolume(id: "z1", position: 30), .toggleZone(id: "z1", keepOff: [])]
            && zonePlan(presetCommand) == [.recallPreset(id: "p2", keepOff: [])],
          "web remote: a zone command sets the zone's volume, then flips it; a preset command recalls it")
    let noMember = zonePlan("{\"type\":\"zone\",\"id\":\"z2\",\"toggle\":true}").isEmpty
    let unknownZone = zonePlan("{\"type\":\"zone\",\"id\":\"z9\",\"volume\":10}").isEmpty
    let unknownPreset = zonePlan("{\"type\":\"preset\",\"id\":\"p9\",\"recall\":true}").isEmpty
    check(noMember && unknownZone && unknownPreset,
          "web remote: a zone with no listed member doesn't flip; an unknown zone or preset changes nothing")
    let codeRows = zoneRows + [RemoteRow(Facts(id: "tv", name: "TV", kind: "AirPlay", on: false, volume: 50,
                                               isAirPlay: true, needsCode: true, key: "ap:tv"), text: t)]
    let codeZones = [SpeakerZone(id: "z3", name: "Den", members: ["ap:tv", "lc:bt"], volume: 100),
                     SpeakerZone(id: "z4", name: "TV", members: ["ap:tv"], volume: 100)].map {
        RemoteZoneRow($0, listed: codeRows)
    }
    let codePresets = [RemotePresetRow(id: "p3", name: "Movie", current: false, members: ["ap:tv", "lc:bt"])]
    func codePlan(_ command: RemoteCommand) -> [RemoteAction] {
        RemoteAction.plan(command, rows: codeRows, zones: codeZones, presets: codePresets)
    }
    check(codePlan(.zone(id: "z3", toggle: true, volume: nil)) == [.toggleZone(id: "z3", keepOff: ["ap:tv"])]
            && codePlan(.preset(id: "p3")) == [.recallPreset(id: "p3", keepOff: ["ap:tv"])]
            && codePlan(.zone(id: "z4", toggle: true, volume: nil)).isEmpty,
          "web remote: a zone or preset from the phone keeps a member that asks for a code off; with no other member it doesn't flip")

    // covers: S258
    let duringRun = [zoneCommand, presetCommand, "{\"type\":\"master\",\"volume\":20}",
                     "{\"type\":\"speaker\",\"id\":\"usb\",\"on\":false}"]
    check(duringRun.allSatisfy { zonePlan($0, calibrating: true).isEmpty },
          "web remote: the Mac refuses every command that arrives during a microphone run")
    var settling = CalibrateAllStatus()
    settling.holdsOutputs = true
    let measuringOne = CalibrationStatus(uid: "usb", phase: .running(.measuring(.reference)))
    check(!RemoteSnapshot.calibrating(CalibrationStatus(), CalibrateAllStatus())
            && RemoteSnapshot.calibrating(measuringOne, CalibrateAllStatus())
            && RemoteSnapshot.calibrating(CalibrationStatus(), settling),
          "web remote: one speaker's calibration and Calibrate All, also while a cut-off sweep plays out, count as calibrating")

    // covers: S120
    let fromPhone = LocalOutputSync.report([.init(uid: "usb", deviceName: "USB DAC")], enabled: ["usb"], origin: .remote)
    check(fromPhone?.alertNames == nil && fromPhone?.notices.map(\.uid) == ["usb"] && fromPhone?.enabled == [],
          "remote: a start from the phone shows no alert, only a notice for the failed speaker")

    check(RemotePort.parse(" 8080 ") == 8080 && RemotePort.parse("80") == nil && RemotePort.parse("70000") == nil
            && RemotePort.parse("abc") == nil && RemotePort.stored(1e30) == nil && RemotePort.stored(7655.5) == nil,
          "web remote: a port from 1024 to 65535 is taken")
    check(RemoteDevicesFile.canForget(count: 0, failed: true) && RemoteDevicesFile.canForget(count: 2, failed: false)
            && !RemoteDevicesFile.canForget(count: 0, failed: false),
          "web remote: Forget All stays available after a failed save, when no device is left in memory")
    let debugStart = RemoteStartConfig.make(port: 7655, localName: nil, devicesPath: nil, debugLog: true)
    let normalStart = RemoteStartConfig.make(port: 8080, localName: "den", devicesPath: "/devices.json", debugLog: false)
    check(debugStart["log_pairing_codes"] as? Bool == true && debugStart["port"] as? Int == 7655
            && debugStart["local_name"] == nil && normalStart["log_pairing_codes"] == nil
            && normalStart["local_name"] as? String == "den" && normalStart["devices_path"] as? String == "/devices.json",
          "web remote: only the debug log asks the core for the pairing codes")
    // covers: S288 (the Mac host)
    let switchOn = ["AUDIOSYNC_WEB_REMOTE": "1"]
    check(RemoteDebugSwitch.port(environment: switchOn, debugLog: true, savedPort: 8080) == 8080
            && RemoteDebugSwitch.port(environment: switchOn.merging(["AUDIOSYNC_WEB_REMOTE_PORT": "9000"]) { $1 },
                                      debugLog: true, savedPort: 8080) == 9000
            && RemoteDebugSwitch.port(environment: switchOn.merging(["AUDIOSYNC_WEB_REMOTE_PORT": "80"]) { $1 },
                                      debugLog: true, savedPort: 8080) == 8080
            && RemoteDebugSwitch.port(environment: switchOn, debugLog: false, savedPort: 8080) == nil
            && RemoteDebugSwitch.port(environment: ["AUDIOSYNC_WEB_REMOTE": "true"], debugLog: true, savedPort: 8080) == nil,
          "web remote: the debug switch starts it for one run on the saved or given port, only with the debug log")
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

    let day: TimeInterval = 24 * 60 * 60
    let seenNow = Date(timeIntervalSince1970: 1_800_000_000)
    let seenFile = { (seen: String) in
        RemoteDevicesFile.count(devicesJSON("{\"version\":1,\"devices\":[\"\(hashA)\",\"\(hashB)\"],\"seen\":\(seen)}"),
                                now: seenNow)
    }
    let recent = Int(seenNow.timeIntervalSince1970 - 89 * day), idle = Int(seenNow.timeIntervalSince1970 - 90 * day)
    let ahead = Int(seenNow.timeIntervalSince1970 + 400 * day)
    let nineHashes = Array(repeating: "\"\(hashA)\"", count: 9).joined(separator: ",")
    let eightIdle = Array(repeating: String(idle), count: 8).joined(separator: ",")
    let newestEight = RemoteDevicesFile.count(
        devicesJSON("{\"version\":1,\"devices\":[\(nineHashes)],\"seen\":[\(recent),\(eightIdle)]}"), now: seenNow)
    let mismatched: [Int] = [seenFile("[\(recent)]"), seenFile("[\(recent),-1]"), seenFile("[\(recent),1.5]"),
                             seenFile("[\(recent),true]"), seenFile("{}")]
    check(seenFile("[\(recent),\(idle)]") == 1 && seenFile("[\(ahead),\(recent)]") == 2 && newestEight == 0
            && mismatched.allSatisfy { $0 == 0 },
          "web remote: a device idle for 90 days isn't counted, only the newest 8 are; a seen list that doesn't match the devices counts as none")

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

// MARK: Google Cast speakers added by address (C18): what the sheet reads, and the stored list
do {
    let read = { (typed: String) in CastManualAddress(typed).map { "\($0.host)|\($0.port)" } }
    check(read("192.0.2.5") == "192.0.2.5|8009" && read(" Speaker.Local. ") == "speaker.local|8009"
            && read("living-room") == "living-room|8009",
          "cast address: an IPv4 address or a host name alone gets port 8009; a host name is lowercased and trimmed")
    check(read("192.0.2.5:8010") == "192.0.2.5|8010" && read("speaker.local:9000") == "speaker.local|9000"
            && CastManualAddress("speaker.local:8009")?.text == "speaker.local"
            && CastManualAddress("speaker.local:8010")?.text == "speaker.local:8010",
          "cast address: host:port keeps the port; the text leaves out 8009 only")
    let refused = ["", "   ", "speaker.local:", "speaker.local:0", "speaker.local:65536", "speaker.local:80a",
                   "speaker.local:+80", "192.0.2.256", "192.0.2", "192.0.2.05", "1234", "-speaker.local",
                   "speaker_one.local", "spe aker", "a..b", "::1", "[2001:db8::1]:8009", "192.0.2.5:8009:1"]
    let accepted = refused.filter { CastManualAddress($0) != nil }
    check(accepted.isEmpty, "cast address: empty text, a bad port, a bad IPv4 address, a bad host name and IPv6 are refused",
          "accepted: \(accepted)")

    final class Stored { var values: [String: Any] = [:] }
    let stored = Stored()
    let store = SettingsStore(read: { stored.values[$0] }, write: { value, key in stored.values[key] = value })
    let key = AppSettings.Keys.castManualAddresses
    var list = store.castManualList
    let kitchen = CastManualAddress("speaker.local")!, den = CastManualAddress("192.0.2.7:8010")!
    let firstAdd = list.add(kitchen), secondAdd = list.add(den)
    let repeated = list.add(CastManualAddress("SPEAKER.local:8009")!)
    store.save(list)
    let savedText = stored.values[key] as? [String]
    let reread = store.castManualList
    check(key == "castManualAddresses" && firstAdd && secondAdd && !repeated
            && savedText == ["speaker.local", "192.0.2.7:8010"] && reread.addresses == [kitchen, den],
          "cast address list: adds keep their order, the same address twice is stored once, and it reads back")
    stored.values[key] = ["speaker.local", "not valid!", "speaker.local:8009"]
    check(store.castManualList.addresses == [kitchen], "cast address list: a damaged or repeated stored entry is dropped")
    var shrinking = reread
    let removed = shrinking.remove(kitchen), removedAgain = shrinking.remove(kitchen)
    store.save(shrinking)
    let afterOne = store.castManualList.addresses
    shrinking.remove(den)
    store.save(shrinking)
    check(removed && !removedAgain && afterOne == [den] && stored.values[key] == nil,
          "cast address list: Remove drops one address, and the setting goes once none is left")

    let id = "manual:192.0.2.7:8010"
    var m = CastModel()
    m.addedManually(id, address: den)
    m.apply(.appeared(CastDevice(id: id, name: "", manual: true), offsetMs: nil))
    let replaced = m.addedManually(id, address: CastManualAddress("192.0.2.7:8010")!)
    let removedAddress = m.removeManual(id)
    let comesBack = m.apply(.changed(CastDevice(id: id, name: "", manual: true), offsetMs: nil))
    m.addedManually(id, address: den)
    let readded = m.apply(.changed(CastDevice(id: id, name: "", manual: true), offsetMs: nil))
    check(replaced == nil && removedAddress == den && !comesBack && readded && m.devices[id] != nil
            && m.removeManual("0123456789abcdef0123456789abcdef") == nil,
          "cast model: a removed manual device stays off the list until it is added again; only a manual one is removed")
    let t = UIStrings()
    check(t.castNote(CastRow(status: .idle, muted: false, volumeFixed: false, group: nil, manual: true)) == "Added by address"
            && t.castNote(CastRow(status: .idle, muted: false, volumeFixed: false, group: nil)) == nil
            && t.helpText.contains("click\nAdd by Address… under"),
          "cast row: a device added by address says so under its name, and the Guide names the action")

    let manualID = "manual:192.0.2.9:8009", realID = "00112233445566778899aabbccddeeff"
    let identified: (String) -> CastDeviceChange? = { former in
        former.withCString { former in
            realID.withCString { id in
                "Lounge".withCString { name in
                    "".withCString { model in
                        CastDeviceChange(AscCastDevice(event: ASC_CAST_APPEARED, id: id, name: name, model: model,
                                                       flags: 0, former: former), offsetMs: { _ in nil })
                    }
                }
            }
        }
    }
    let lounge = CastManualAddress("192.0.2.9")!
    var found = CastModel()
    found.addedManually(manualID, address: lounge)
    found.apply(.appeared(CastDevice(id: manualID, name: "", manual: true), offsetMs: nil))
    found.apply(.gone(id: manualID))
    let browsed = identified(manualID)
    if let browsed { found.apply(browsed) }
    let moved = found.manualAddresses == [realID: lounge]
    let forgotten = found.removeManual(realID)
    check(browsed == .appeared(CastDevice(id: realID, name: "Lounge"), offsetMs: nil, former: manualID)
            && identified("") == .appeared(CastDevice(id: realID, name: "Lounge"), offsetMs: nil, former: nil)
            && browsed?.id == realID
            && moved && forgotten == lounge && found.devices[realID] != nil && found.manualAddresses.isEmpty,
          "cast model: a manual device a browse finds keeps its address under the new id, and Remove there forgets "
            + "only the address")

    var moving = CastModel()
    moving.addedManually(manualID, address: lounge)
    moving.apply(.appeared(CastDevice(id: manualID, name: "", manual: true), offsetMs: nil))
    _ = moving.turnOn(manualID)
    moving.startSent(manualID)
    _ = moving.started(manualID, attempt: 7, refusal: 0)
    _ = moving.apply(.playing(attempt: 7))
    _ = moving.apply(.volume(attempt: 7, CastVolume(level: 0.4, muted: false, fixed: false)))
    moving.apply(.gone(id: manualID))
    if let browsed { moving.apply(browsed) }
    let playsMoved = moving.status(of: realID) == .playing && moving.status(of: manualID) == .idle
        && moving.enabled == realID && moving.playingDevice == realID && moving.volumes[realID]?.level == 0.4
        && moving.manualAddresses == [realID: lounge]
    let turnOnAgain = moving.turnOn(realID)
    let turnedOff = moving.turnOff(realID)
    check(playsMoved && turnOnAgain == nil && turnedOff && moving.plan(streaming: true) == .stop,
          "cast model: a playing speaker added by address that a browse finds stays on and playing under its id, "
            + "and Off still stops it")

    var offsetMoves = CastModel()
    offsetMoves.apply(.appeared(CastDevice(id: manualID, name: "", manual: true), offsetMs: nil))
    _ = offsetMoves.setOffset(manualID, ms: 240)
    offsetMoves.apply(.gone(id: manualID))
    if let browsed { offsetMoves.apply(browsed) }
    var offsetKept = CastModel()
    offsetKept.apply(.appeared(CastDevice(id: manualID, name: "", manual: true), offsetMs: nil))
    _ = offsetKept.setOffset(manualID, ms: 240)
    offsetKept.apply(.gone(id: manualID))
    offsetKept.apply(.appeared(CastDevice(id: realID, name: "Lounge"), offsetMs: 100, former: manualID))
    var ownSet = CastModel()
    _ = ownSet.setOffset(manualID, ms: 240)
    _ = ownSet.setOffset(realID, ms: 100)
    ownSet.apply(.appeared(CastDevice(id: realID, name: "Lounge"), offsetMs: 100, former: manualID))
    var ownCleared = CastModel()
    _ = ownCleared.setOffset(manualID, ms: 240)
    _ = ownCleared.setOffset(realID, ms: 50)
    let cleared = ownCleared.clearOffset(realID)
    ownCleared.apply(.appeared(CastDevice(id: realID, name: "Lounge"), offsetMs: 240, former: manualID))
    check(offsetMoves.offset(of: realID) == 240 && offsetMoves.offsets[manualID] == nil
            && offsetKept.offset(of: realID) == 100 && ownSet.offset(of: realID) == 100
            && cleared && ownCleared.offset(of: realID) == 240,
          "cast model: a speaker added by address that a browse finds keeps its offset under its id, as the sender "
            + "does: over none or a cleared one, never over one the id already had")

    var stopped = CastModel()
    stopped.apply(.appeared(CastDevice(id: manualID, name: "", manual: true), offsetMs: nil))
    stopped.apply(.appeared(CastDevice(id: realID, name: "Lounge"), offsetMs: nil))
    _ = stopped.apply(.discoveryStopped)
    check(stopped.devices[manualID] != nil && stopped.devices[realID] == nil,
          "cast model: when discovery stops, a speaker added by address stays listed, as the sender keeps it")

    check(t.castAddProblem(.notFound)
            == "AudioSync couldn't find a speaker at that address. Check that it's on and try again."
            && t.castAddProblem(.unreadable) == t.castAddInvalid
            && CastSender.ipv4Address(of: "speaker.invalid") == nil
            && !CastSender.answers(ip: "127.0.0.1", port: 9, timeout: 1),
          "cast address: a name that doesn't resolve or an address where nothing answers is not found, with its own text")
}

// MARK: Menu bar panel colors: contrast with the material counted as clear
do {
    typealias RGBA = (r: Double, g: Double, b: Double, a: Double)
    func resolved(_ color: NSColor, dark: Bool) -> RGBA {
        var out: RGBA = (0, 0, 0, 0)
        NSAppearance(named: dark ? .darkAqua : .aqua)!.performAsCurrentDrawingAppearance {
            let c = color.usingColorSpace(.sRGB)!
            out = (Double(c.redComponent), Double(c.greenComponent), Double(c.blueComponent), Double(c.alphaComponent))
        }
        return out
    }
    func over(_ top: RGBA, _ bottom: RGBA) -> RGBA {
        (top.r * top.a + bottom.r * (1 - top.a), top.g * top.a + bottom.g * (1 - top.a),
         top.b * top.a + bottom.b * (1 - top.a), 1)
    }
    func luminance(_ c: RGBA) -> Double {
        func channel(_ v: Double) -> Double { v <= 0.03928 ? v / 12.92 : pow((v + 0.055) / 1.055, 2.4) }
        return 0.2126 * channel(c.r) + 0.7152 * channel(c.g) + 0.0722 * channel(c.b)
    }
    func ratio(_ a: RGBA, _ b: RGBA) -> Double {
        let (x, y) = (luminance(a), luminance(b))
        return (max(x, y) + 0.05) / (min(x, y) + 0.05)
    }
    func rgb(_ c: BrandRGB) -> RGBA { (c.red, c.green, c.blue, 1) }

    // Worst case: the material adds nothing, so only the backing (and a row's
    // 7% hover fill) stands between the content and a black or white window.
    var worst: [String: Double] = [:]
    for dark in [false, true] {
        let ink: RGBA = dark ? (1, 1, 1, 1) : (0, 0, 0, 1)
        for backdrop in [(0.0, 0.0, 0.0, 1.0), (1.0, 1.0, 1.0, 1.0)] as [RGBA] {
            let panel = over(resolved(.audioSyncPanelBacking, dark: dark), backdrop)
            let row = over((ink.r, ink.g, ink.b, 0.07), panel)
            let pairs: [(String, RGBA, RGBA)] = [
                // Brand.secondaryText is `.primary` at 70%, and the label color itself is the ink at 85%.
                ("secondary text", (ink.r, ink.g, ink.b, BrandTokens.Opacity.secondaryText * 0.85), row),
                ("wordmark sync", resolved(.audioSyncWordmarkSync, dark: dark), panel),
                ("active blue", resolved(.audioSyncActiveBlue, dark: dark), row),
                ("teal icon", rgb(dark ? BrandTokens.Dark.tealText : BrandTokens.Light.tealText), row),
            ]
            for (name, color, background) in pairs {
                let r = ratio(over(color, background), background)
                worst[name] = min(worst[name] ?? .infinity, r)
            }
        }
    }
    let summary = worst.keys.sorted().map { String(format: "%@ %.2f", $0, worst[$0]!) }.joined(separator: ", ")
    check(worst["secondary text"]! >= 4.5, "panel colors: secondary text keeps 4.5:1 over any window", summary)
    // The wordmark's "sync" and the BrandMark fill are logotypes, exempt from WCAG 1.4.3, so their floor is 3:1.
    check(worst["wordmark sync"]! >= 3 && worst["active blue"]! >= 3 && worst["teal icon"]! >= 3,
          "panel colors: the wordmark's sync and the type icons keep 3:1 over any window", summary)
    check(resolved(.audioSyncWordmarkSync, dark: false).g < resolved(.audioSyncWordmarkSync, dark: true).g,
          "panel colors: sync is the darker teal on light and the mint on dark")
}

// MARK: Hidden speakers — out of every list by key, settings kept, back off when shown
do {
    let t = UIStrings()
    check(HiddenSpeakers.key(id: "BuiltInSpeakerDevice", tag: .builtIn) == "lc:BuiltInSpeakerDevice"
          && HiddenSpeakers.key(id: "native:AA11BB22CC33", tag: .airplay) == "ap:native:AA11BB22CC33"
          && HiddenSpeakers.key(id: "cast:nest-1", tag: .cast) == "cast:nest-1"
          && HiddenSpeakers.key(id: "teams-uid", tag: .virtual) == "lc:teams-uid",
          "hidden speakers: the key is the speaker key, and a Cast speaker's app ID")

    typealias Row = (id: String, name: String, tag: SpeakerTag, on: Bool)
    let rows: [Row] = [("BuiltIn", "MacBook Speakers", .builtIn, true), ("teams", "Microsoft Teams Audio", .virtual, true),
                       ("native:AA11BB22CC33", "Kitchen", .airplay, false), ("cast:nest-1", "Nest", .cast, false)]
    let rowKey = { (r: Row) in HiddenSpeakers.key(id: r.id, tag: r.tag) }
    var hidden = HiddenSpeakers()
    check(hidden.hide(key: "lc:teams", name: "Microsoft Teams Audio", kind: "Virtual")
          && !hidden.hide(key: "lc:teams", name: "Renamed", kind: "Virtual")
          && hidden.entries.map(\.name) == ["Microsoft Teams Audio"],
          "hidden speakers: hiding twice keeps one entry with its first name")
    let split = hidden.partition(rows, key: rowKey)
    check(split.shown.map(\.id) == ["BuiltIn", "native:AA11BB22CC33", "cast:nest-1"] && split.hidden.map(\.id) == ["teams"],
          "hidden speakers: a hidden speaker leaves the rows, the others keep their order")

    // The Web Remote plans its commands against the rows the window lists.
    let remoteRows = split.shown.map {
        RemoteRow(RemoteRow.Facts(id: $0.id, name: $0.name, kind: t.speakerTag($0.tag), on: $0.on, volume: 50), text: t)
    }
    let snapshot = RemoteSnapshot(computerName: "Mac", master: 100, nowPlaying: nil, rows: remoteRows)
    let command = RemoteCommand(json: "{\"type\":\"speaker\",\"id\":\"teams\",\"on\":true}")
    check(!snapshot.json.contains("Microsoft Teams Audio")
          && command.map { RemoteAction.plan($0, rows: remoteRows) } == [],
          "hidden speakers: the remote snapshot loses a hidden speaker, and a command naming it changes nothing")

    // Rediscovery: the same key comes back in a new list and stays hidden.
    hidden.hide(key: "cast:nest-1", name: "Nest", kind: "Google Cast")
    hidden.hide(key: "ap:native:AA11BB22CC33", name: "Kitchen", kind: "AirPlay")
    let again: [Row] = [("cast:nest-1", "Nest Audio", .cast, false), ("native:AA11BB22CC33", "Kitchen", .airplay, false)]
    check(hidden.partition(again, key: rowKey).shown.isEmpty,
          "hidden speakers: a Cast or AirPlay speaker that re-announces, even under a new name, stays hidden")

    let shownMembers = ["lc:BuiltIn", "lc:teams"].filter { !hidden.contains($0) }
    check(ZoneLayout.count(on: shownMembers.count, waiting: 0, present: shownMembers.count, total: shownMembers.count,
                           text: t) == "1 of 1 on",
          "hidden speakers: a zone's count leaves a hidden member out of its total")
    check(hidden.autoConnecting(["lc:teams", "lc:usb", "lc:other"], marked: ["lc:teams", "lc:usb"]) == ["lc:usb"],
          "hidden speakers: auto-connect skips a hidden speaker marked for it")

    let menu = hidden.menu(present: ["lc:teams": (name: "Teams", kind: "Virtual")], text: t)
    check(menu.map(\.title) == ["Kitchen (AirPlay, not found)", "Nest (Google Cast, not found)", "Teams (Virtual)"]
          && HiddenSpeakers().menu(present: [:], text: t).isEmpty,
          "hidden speakers: the menu is sorted by shown name, uses the current name when present, says not found otherwise",
          menu.map(\.title).joined(separator: " | "))

    // Shown again: back in the rows; nothing else was touched, so its saved values stay.
    var store: [String: Any] = [AppSettings.Keys.localOffsets: ["teams": 120.0]]
    let settings = SettingsStore(read: { store[$0] }, write: { value, key in store[key] = value })
    settings.save(hiddenSpeakers: hidden)
    let loaded = settings.load()
    check(loaded.hiddenSpeakers == hidden && loaded.localOffsets["teams"] == 120,
          "hidden speakers: the hidden set survives a settings round trip, with the offset kept")
    check(hidden.show("lc:teams")?.name == "Microsoft Teams Audio" && hidden.show("lc:teams") == nil
          && hidden.partition(rows, key: rowKey).shown.contains { $0.id == "teams" },
          "hidden speakers: showing a speaker brings its row back")
    hidden.show("cast:nest-1")
    hidden.show("ap:native:AA11BB22CC33")
    settings.save(hiddenSpeakers: hidden)
    check(store[AppSettings.Keys.hiddenSpeakers] == nil, "hidden speakers: the setting goes once nothing is hidden")

    let lenient = HiddenSpeakers(stored: [["key": "lc:a", "name": "A", "kind": "USB"], ["name": "no key"], "junk",
                                          ["key": "lc:b"], ["key": "lc:a", "name": "again"]] as [Any])
    check(lenient.entries == [.init(key: "lc:a", name: "A", kind: "USB"), .init(key: "lc:b", name: "", kind: "")]
          && HiddenSpeakers(stored: "nonsense").isEmpty,
          "hidden speakers: a malformed stored entry is skipped")

    var moved = HiddenSpeakers()
    moved.hide(key: "ap:123456789", name: "Den", kind: "AirPlay")
    moved.hide(key: "lc:usb", name: "Desk", kind: "USB")
    moved.migrate(airPlay: ["123456789": "native:00075BCD15"])
    check(moved.contains("ap:native:00075BCD15") && !moved.contains("ap:123456789") && moved.contains("lc:usb"),
          "hidden speakers: a hidden AirPlay speaker moves to its native ID with its other settings (S204)")
}

// MARK: The lead the debug log names, and the input level meter (LocalDelay.swift, InputLevel.swift)
do {
    let waiting = LocalDelay.leadDescription(networkMs: 2000)
    let alone = LocalDelay.leadDescription(networkMs: nil)
    check(waiting == "lead: local speakers wait 2000 ms for the network speakers", "lead: \(waiting)")
    check(alone.contains("start lead") && !alone.contains("wait"), "lead: \(alone)")

    let meter = InputLevel()
    check(!meter.update(peak: nil) && !meter.shown && meter.level == 0, "input level: hidden without an input")
    check(meter.update(peak: 1) && meter.shown && meter.level == 1, "input level: full scale fills it")
    meter.update(peak: 0)
    check(abs(meter.level - 0.85) < 1e-5, "input level: it falls back a step, \(meter.level)")
    check(!meter.update(peak: nil) && !meter.shown && meter.level == 0, "input level: hidden again, emptied")
    check(meter.spokenPercent == 0 && asc_meter_spoken_percent(0.42) == 40 && asc_meter_spoken_percent(0.66) == 70
            && asc_meter_spoken_percent(1.4) == 100 && asc_meter_spoken_percent(.nan) == 0,
          "input level: VoiceOver hears it in steps of 10 percent")
    check(UIStrings(locale: Locale(identifier: "en_US")).inputLevelValue(40) == "40%",
          "input level: VoiceOver reads a percent")

    func luminance(_ c: BrandRGB) -> Double {
        func linear(_ v: Double) -> Double { v <= 0.04045 ? v / 12.92 : pow((v + 0.055) / 1.055, 2.4) }
        return 0.2126 * linear(c.red) + 0.7152 * linear(c.green) + 0.0722 * linear(c.blue)
    }
    func contrast(_ a: BrandRGB, _ b: BrandRGB) -> Double {
        let (x, y) = (luminance(a), luminance(b))
        return (max(x, y) + 0.05) / (min(x, y) + 0.05)
    }
    for (name, fill, track) in [("light", BrandTokens.Light.tealText, BrandTokens.Light.separator),
                                ("dark", BrandTokens.Dark.tealText, BrandTokens.Dark.separator)] {
        let ratio = contrast(fill, track)
        check(ratio >= 3, "input level: the fill on its track reaches 3:1 in \(name) appearance (\(ratio))")
    }
}

// MARK: The live input note while an AirPlay speaker is on (InputLevel.swift)
do {
    let mic = AudioSource.input(uid: "BuiltInMicrophoneDevice")
    check(LiveInputAirPlayHint.tenths(source: mic, airPlayOn: true, airPlayDelayMs: 1750) == 18,
          "live input note: shows the AirPlay delay, rounded to a tenth")
    check(LiveInputAirPlayHint.tenths(source: mic, airPlayOn: false, airPlayDelayMs: 1750) == nil,
          "live input note: hidden with no AirPlay speaker on")
    check(LiveInputAirPlayHint.tenths(source: .system, airPlayOn: true, airPlayDelayMs: 1750) == nil
            && LiveInputAirPlayHint.tenths(source: .app(bundleID: "com.apple.Music"), airPlayOn: true,
                                           airPlayDelayMs: 1750) == nil,
          "live input note: hidden for any source but an input device")
    let note = UIStrings(locale: Locale(identifier: "en_US")).liveInputAirPlayHint(tenths: 18)
    check(note.hasPrefix("AirPlay adds a 1.8 s delay") && !note.contains("\u{2014}"), "live input note: \(note)")
}

// MARK: Microphone permission request (MicrophoneRequestGate.swift)
do {
    var gate = MicrophoneRequestGate()
    let first = gate.begin(origin: .user)
    let second = gate.begin(origin: .automatic)
    check(first && !second, "microphone: a start behind an open prompt sends no second request")
    check(gate.answer() == .automatic && gate.answer() == nil,
          "microphone: the one answer runs exactly one start, with the latest origin")
    check(gate.begin(origin: .user) && gate.answer() == .user,
          "microphone: after the answer, a new start may ask again")
    var unasked = MicrophoneRequestGate()
    check(unasked.answer() == nil, "microphone: an answer with no request out starts nothing")
    var switches = MicrophoneRequestGate()
    let toInput = CaptureSwitch.Change.source(from: .system, to: .input(uid: "a"))
    let toOther = CaptureSwitch.Change.source(from: .system, to: .input(uid: "b"))
    _ = switches.begin(origin: .user, switching: toInput)
    _ = switches.begin(origin: .user, switching: toOther)
    let pending = switches.switching
    _ = switches.answer()
    check(pending == toOther && switches.switching == nil,
          "microphone: the answer runs the latest switch once", "\(String(describing: pending))")
    var picked = MicrophoneRequestGate()
    _ = picked.begin(origin: .user, switching: toInput)
    picked.supersedeSwitch()
    let droppedSwitch = picked.switching
    check(droppedSwitch == nil && picked.answer() == nil && picked.begin(origin: .user),
          "microphone: a later pick drops the switch waiting on the prompt, and its answer runs nothing")
    var plainStart = MicrophoneRequestGate()
    _ = plainStart.begin(origin: .user)
    plainStart.supersedeSwitch()
    check(plainStart.answer() == .user, "microphone: a later pick leaves a plain start's answer alone")
    // covers: S141 (a pick that goes back to the old source answers the switch waiting on the prompt)
    var repick = MicrophoneRequestGate()
    _ = repick.begin(origin: .user, switching: toInput)
    repick.repicked(source: .system, pair: 0)
    let repickedSwitch = repick.switching
    check(repickedSwitch == nil && repick.answer() == nil,
          "microphone: picking the source that plays again drops the switch waiting on the prompt (S141)",
          "\(String(describing: repickedSwitch))")
    var repickPair = MicrophoneRequestGate()
    _ = repickPair.begin(origin: .user, switching: .inputPair(uid: "a", from: 0, to: 1))
    repickPair.repicked(source: .input(uid: "a"), pair: 0)
    check(repickPair.switching == nil && repickPair.answer() == nil,
          "microphone: picking the channel pair that plays again drops the pair switch waiting on the prompt (S141)")
    var otherPick = MicrophoneRequestGate()
    _ = otherPick.begin(origin: .user, switching: toInput)
    otherPick.repicked(source: .input(uid: "b"), pair: 0)
    var plainRepick = MicrophoneRequestGate()
    _ = plainRepick.begin(origin: .user)
    plainRepick.repicked(source: .system, pair: 0)
    check(otherPick.switching == toInput && plainRepick.answer() == .user,
          "microphone: a pick of a capture the switch doesn't leave, or with no switch waiting, changes nothing")
}

// MARK: Late output callbacks in the debug log (LateCallbackLog, LogRateLimiter in OutputRenderer.swift)
do {
    let ms: UInt64 = 1_000_000
    var limiter = LogRateLimiter()
    var logged = 0
    var closedCounts: [Int] = []
    for event in 0..<25 {
        let decision = limiter.admit(1, atNs: 5_000 * ms + UInt64(event) * 40 * ms)
        logged += decision.admitted
        if let closed = decision.closed { closedCounts.append(closed) }
    }
    check(logged == 10 && closedCounts.isEmpty, "late callbacks: 25 in one second log 10 lines",
          "\(logged) logged, closed \(closedCounts)")
    let after = limiter.admit(0, atNs: 6_000 * ms)
    check(after.closed == 15 && after.admitted == 0, "late callbacks: then one line counts the 15 suppressed",
          "\(after)")
    check(limiter.admit(0, atNs: 7_000 * ms).closed == nil, "late callbacks: the suppressed count comes once")

    var twoSeconds = LogRateLimiter()
    var firstLogged = 0
    for event in 0..<12 { firstLogged += twoSeconds.admit(1, atNs: UInt64(event) * 50 * ms).admitted }
    let next = twoSeconds.admit(1, atNs: 1_500 * ms)
    var secondLogged = next.admitted
    for event in 1..<12 { secondLogged += twoSeconds.admit(1, atNs: 1_500 * ms + UInt64(event) * 50 * ms).admitted }
    check(firstLogged == 10 && next.closed == 2 && secondLogged == 10,
          "late callbacks: a new second opens a new window of 10",
          "first \(firstLogged), closed \(String(describing: next.closed)), second \(secondLogged)")

    var dropped = LogRateLimiter()
    _ = dropped.admit(1, atNs: 0)
    dropped.drop(3, atNs: 10 * ms)
    check(dropped.admit(0, atNs: 1_000 * ms).closed == 3,
          "late callbacks: callbacks whose details were overwritten count as suppressed")

    let slot = LateCallbackLog.pack(startNs: 123_456 * ms + 789, lateNs: 12_345_678)
    let unpacked = LateCallbackLog.unpack(slot)
    check(unpacked.startMs == 123_456 && unpacked.lateUs == 12_345, "late callbacks: a slot keeps start ms and lateness µs")
    check(LateCallbackLog.unpack(LateCallbackLog.pack(startNs: 0, lateNs: 60 * 1_000 * ms)).lateUs
            == LateCallbackLog.latenessMaskUs, "late callbacks: a huge lateness saturates instead of spilling")
    let wall = Date(timeIntervalSince1970: 1_800_000_000)
    let line = LateCallbackLog.eventLine(slot, name: "Speaker", nowNs: 123_706 * ms, wallNow: wall)
    check(line == "[AudioSync] late output callback at 2027-01-15T07:59:59.750Z on \"Speaker\": 12.3 ms late",
          "late callbacks: the line names the buffer's wall-clock start, the output and the lateness", line)
    // The buffer's host time lies ahead of now by the output lead, so a poll can come before it.
    let ahead = LateCallbackLog.eventLine(slot, name: "Speaker", nowNs: 123_426 * ms, wallNow: wall)
    check(ahead.contains(" at 2027-01-15T08:00:00.0"), "late callbacks: a buffer starting 30 ms after now is stamped ahead", ahead)
    let far = LateCallbackLog.eventLine(slot, name: "Speaker", nowNs: 0, wallNow: wall)
    check(far.contains(" at 2027-01-15T08:02:03.45"), "late callbacks: a start far ahead of now does not trap", far)
    var stopping = LogRateLimiter()
    for event in 0..<12 { _ = stopping.admit(1, atNs: UInt64(event) * ms) }
    check(stopping.flush() == 2 && stopping.flush() == nil,
          "late callbacks: the last output's stop flushes the pending suppressed count once")
    check(LateCallbackLog.suppressedLine(15) == "[AudioSync] 15 more late callbacks suppressed",
          "late callbacks: suppressed-count line")
}

// MARK: Updates run only in release builds that carry the EdDSA public key (UpdateGate.swift)
do {
    let key = "a-test-key-not-a-real-one"
    check(UpdateGate.isEnabled(publicEDKey: key, releaseBuild: true),
          "updates: a key in a release build turns the updater on")
    check(!UpdateGate.isEnabled(publicEDKey: key, releaseBuild: nil),
          "updates: a key without the release flag (a plain ./build.sh or CI) keeps the updater off")
    check(!UpdateGate.isEnabled(publicEDKey: key, releaseBuild: false), "updates: a false release flag keeps the updater off")
    check(!UpdateGate.isEnabled(publicEDKey: key, releaseBuild: "true"),
          "updates: a release flag that isn't a boolean keeps the updater off")
    check(!UpdateGate.isEnabled(publicEDKey: nil, releaseBuild: true), "updates: the release flag without a key keeps the updater off")
    check(!UpdateGate.isEnabled(publicEDKey: "", releaseBuild: true), "updates: the release flag with an empty key keeps the updater off")
    check(!UpdateGate.isEnabled(publicEDKey: " \n\t", releaseBuild: true),
          "updates: the release flag with a whitespace-only key keeps the updater off")
    check(!UpdateGate.isEnabled(publicEDKey: 42, releaseBuild: true),
          "updates: a key that isn't a string keeps the updater off")
    check(!UpdateGate.isEnabled(publicEDKey: nil, releaseBuild: nil), "updates: neither key nor flag keeps the updater off")
    let plist = (try? String(contentsOfFile: "Info.plist", encoding: .utf8)) ?? ""
    check(plist.contains("<key>SUFeedURL</key>\n\t<string>https://github.com/yassersutojoyo/audiosync/releases/latest/download/appcast.xml</string>"),
          "updates: Info.plist points SUFeedURL at the latest release's appcast.xml")
    check(!plist.isEmpty && !plist.contains(UpdateGate.releaseBuildInfoKey),
          "updates: the source Info.plist never carries the release flag (only ./build.sh --release adds it to the bundle)")
}

// MARK: Every session stop names its reason in the debug log (SessionStop in AppLifecycle.swift)
do {
    let all = SessionStop.allCases
    let lines = all.map { $0.logLine() }
    check(Set(lines).count == all.count, "session stop: every reason has its own log line", lines.joined(separator: " | "))
    check(lines.allSatisfy { $0.hasPrefix("[AudioSync] session stops: ") || $0.hasPrefix("[AudioSync] session restarts: ") },
          "session stop: each line says whether the session stops or restarts", lines.joined(separator: " | "))
    check(all.filter(\.restarts) == [.sourceSwitchedToSystem, .sourceChanged, .inputPairChanged, .formatChanged, .rebuild],
          "session stop: only a source switch, a channel pair change, a format change and a rebuild restart")
    check(SessionStop.powerSwitch.logLine(detail: "key code 49 in NSWindow")
            == "[AudioSync] session stops: the power switch turned streaming off (key code 49 in NSWindow)",
          "session stop: the power switch line names the input event, so a key press tells from a click")
    check(SessionStop.sourceAppQuit.logLine(detail: "com.spotify.client")
            == "[AudioSync] session stops: the source app quit (com.spotify.client)",
          "session stop: a quit source app is named")
    // Capture lost: the engine stops the session before it reports the loss, so nothing runs when the host stops.
    check(SessionStop.logs(running: false, interrupted: true) && SessionStop.logs(running: true, interrupted: false),
          "session stop: a stop after a capture interruption is logged although the engine already stopped")
    check(!SessionStop.logs(running: false, interrupted: false),
          "session stop: a stop with no session running and no interruption writes no line")
}

// MARK: First-run permission explanations (PermissionFlow.swift)
do {
    let fresh = PermissionAsks(explained: [], usedBefore: false)
    check(fresh.step(.microphone, state: .notAsked, userAction: true) == .explain,
          "permissions: a click that needs an unasked permission explains it first")
    check(fresh.step(.microphone, state: .notAsked, userAction: false) == .offer,
          "permissions: an automatic use never raises a modal, it offers the explanation in the window")
    check(fresh.step(.microphone, state: .allowed, userAction: false) == .go
            && fresh.step(.audioCapture, state: .allowed, userAction: true) == .go,
          "permissions: an allowed permission goes on without any explanation")
    check(fresh.step(.microphone, state: .denied, userAction: true) == .denied
            && fresh.step(.automation, state: .denied, userAction: false) == .denied,
          "permissions: a denied permission is never explained again, it shows the denied state")
    check(fresh.step(.audioCapture, state: .unknown, userAction: true) == .explain
            && fresh.step(.localNetwork, state: .unknown, userAction: false) == .offer,
          "permissions: a permission macOS can't report is explained on a fresh install")

    let upgraded = PermissionAsks(explained: [], usedBefore: true)
    check(upgraded.step(.audioCapture, state: .unknown, userAction: true) == .go
            && upgraded.step(.localNetwork, state: .unknown, userAction: false) == .go,
          "permissions: after an update, a permission macOS can't report is not explained again")
    check(upgraded.step(.microphone, state: .notAsked, userAction: true) == .explain,
          "permissions: after an update, a permission macOS reports as unasked is still explained")
    // An update from 0.6.0 held Spotify's reading with no card shown, so Now Playing vanished.
    check(upgraded.mayRaiseAutomationPrompt,
          "permissions: after an update, Now Playing reads the players an earlier version read, with no explanation first")
    check(!PermissionAsks(explained: [], usedBefore: false).mayRaiseAutomationPrompt
            && PermissionAsks(explained: ["automation"], usedBefore: false).mayRaiseAutomationPrompt,
          "permissions: on a fresh install Now Playing waits for Continue on the Automation explanation")

    var asks = PermissionAsks(explained: [], usedBefore: false)
    check(asks.offer == nil, "permissions: nothing is offered before a feature needs it")
    asks.hold(.automation)
    asks.hold(.localNetwork)
    check(asks.offer == .localNetwork, "permissions: one explanation at a time, in a fixed order",
          "\(String(describing: asks.offer))")
    asks.accept(.localNetwork)
    check(asks.offer == .automation && asks.step(.localNetwork, state: .unknown, userAction: false) == .go,
          "permissions: Continue remembers the explanation and shows the next one")
    asks.decline(.automation)
    check(asks.offer == nil, "permissions: Not Now hides the explanation for this run")
    asks.hold(.automation)
    check(asks.offer == nil && asks.step(.automation, state: .notAsked, userAction: false) == .offer,
          "permissions: after Not Now an automatic use still waits but offers nothing")
    check(asks.step(.automation, state: .notAsked, userAction: true) == .explain,
          "permissions: after Not Now a click explains it again")
    check(asks.stored == ["localNetwork"], "permissions: only Continue is stored", "\(asks.stored)")
    let relaunched = PermissionAsks(explained: asks.stored, usedBefore: false)
    check(relaunched.step(.localNetwork, state: .unknown, userAction: false) == .go
            && relaunched.step(.automation, state: .notAsked, userAction: false) == .offer,
          "permissions: a relaunch keeps Continue and forgets Not Now")
    check(PermissionAsks(explained: ["retired", "microphone"], usedBefore: false).stored == ["microphone"],
          "permissions: an unknown stored name is dropped")
    // A first run saves speakers itself; the relaunch must not take that for an earlier version's use.
    let suite = "permissions-logictest-\(ProcessInfo.processInfo.processIdentifier)"
    if let defaults = UserDefaults(suiteName: suite) {
        defaults.removePersistentDomain(forName: suite)
        let firstRun = PermissionAsks.load(defaults)
        defaults.set(["local-speaker"], forKey: AppSettings.Keys.enabledLocal)
        let relaunch = PermissionAsks.load(defaults)
        check(!firstRun.usedBefore && !relaunch.usedBefore
                && relaunch.step(.localNetwork, state: .unknown, userAction: false) == .offer,
              "permissions: settings a fresh install saved don't skip the explanation on the next launch")
        defaults.removePersistentDomain(forName: suite)
        defaults.set(70, forKey: AppSettings.Keys.master)
        let updated = PermissionAsks.load(defaults)
        defaults.removeObject(forKey: AppSettings.Keys.master)
        check(updated.usedBefore && PermissionAsks.load(defaults).usedBefore,
              "permissions: an update from a version without explanations is recognized once and remembered")
        defaults.removePersistentDomain(forName: suite)
    } else {
        check(false, "permissions: a test defaults suite opens")
    }

    check(Permission.needed(for: .system) == .audioCapture
            && Permission.needed(for: .app(bundleID: "x")) == .audioCapture
            && Permission.needed(for: .input(uid: "a")) == .microphone,
          "permissions: taps need audio capture, input devices the microphone")
    check(PermissionAsks.usedBefore { $0 == AppSettings.Keys.enabledLocal }
            && !PermissionAsks.usedBefore { _ in false },
          "permissions: saved speakers mean AudioSync ran before")
    check(Permission.allCases.allSatisfy {
            $0.pane.url.absoluteString.hasPrefix("x-apple.systempreferences:com.apple.preference.security?Privacy_")
          },
          "permissions: every permission links to its Privacy & Security pane")
    check(Permission.audioCapture.pane == .screenCapture && Permission.microphone.pane == .microphone
            && Permission.localNetwork.pane == .localNetwork && Permission.automation.pane == .automation,
          "permissions: each permission opens its own pane")
    let words = UIStrings()
    let texts = Permission.allCases.flatMap { [words.permissionTitle($0), words.permissionReason($0)] }
    check(texts.allSatisfy { !$0.isEmpty && !$0.contains("\u{2014}") } && Set(texts).count == texts.count,
          "permissions: every explanation has its own title and reason, without em dashes")
}

// MARK: Streaming with no speaker on, This Mac on at first install
do {
    check(StreamingPolicy.startsWithoutSpeaker(origin: .user) && StreamingPolicy.startsWithoutSpeaker(origin: .remote)
            && !StreamingPolicy.startsWithoutSpeaker(origin: .automatic),
          "no speaker on: the power switch starts streaming anyway, with no alert; an automatic start still waits")
    check(StreamingPolicy.starts(hasSpeaker: false, continuing: true, hadSpeaker: false, origin: .automatic),
          "no speaker on: a rebuild or switch of a session started with every speaker off carries on")
    check(!StreamingPolicy.starts(hasSpeaker: false, continuing: true, hadSpeaker: true, origin: .automatic),
          "no speaker on: an automatic rebuild or switch after the last speaker went off doesn't start")
    check(StreamingPolicy.starts(hasSpeaker: true, continuing: true, hadSpeaker: true, origin: .automatic)
            && !StreamingPolicy.starts(hasSpeaker: false, continuing: false, hadSpeaker: true, origin: .automatic)
            && StreamingPolicy.starts(hasSpeaker: false, continuing: false, hadSpeaker: true, origin: .user),
          "no speaker on: a speaker that is on always starts; a new automatic start waits; the user's starts")
    check(!StreamingPolicy.isIdle(streaming: true, hadSpeaker: false, enabledAirPlay: [], enabledLocal: []),
          "no speaker on: a session started with every speaker off keeps running")
    check(StreamingPolicy.isIdle(streaming: true, hadSpeaker: true, enabledAirPlay: [], enabledLocal: []),
          "no speaker on: once a speaker played, switching the last one off still stops streaming")
    // Started with every speaker off; an AirPlay speaker switched on asks for
    // a code every time and goes off before it plays.
    var hadSpeaker = StreamingPolicy.countsForSession(switchedOn: true, playing: false)
    check(!StreamingPolicy.isIdle(streaming: true, hadSpeaker: hadSpeaker, enabledAirPlay: [], enabledLocal: [])
            && StreamingPolicy.showsNoSpeakerOn(streaming: true, enabledAirPlay: [], enabledLocal: []),
          "no speaker on: one switched on that fails before it plays keeps the session waiting, and says no speaker is on")
    hadSpeaker = hadSpeaker || StreamingPolicy.countsForSession(switchedOn: true, playing: true)
    check(StreamingPolicy.isIdle(streaming: true, hadSpeaker: hadSpeaker, enabledAirPlay: [], enabledLocal: []),
          "no speaker on: one that played and went off stops streaming")
    check(StreamingPolicy.showsNoSpeakerOn(streaming: true, enabledAirPlay: [], enabledLocal: [])
            && !StreamingPolicy.showsNoSpeakerOn(streaming: true, enabledAirPlay: ["a"], enabledLocal: [])
            && !StreamingPolicy.showsNoSpeakerOn(streaming: true, enabledAirPlay: [], enabledLocal: ["usb"])
            && !StreamingPolicy.showsNoSpeakerOn(streaming: false, enabledAirPlay: [], enabledLocal: []),
          "no speaker on: the speaker list says so only while streaming with every speaker off")
    let line = UIStrings().noSpeakerOn
    check(line.hasPrefix("No speaker is on") && !line.contains("\u{2014}") && !line.contains("\n"),
          "no speaker on: one plain line", line)
    // The engine posts the "choose a speaker" notice, and through notify its
    // alert, only for a start that doesn't go ahead.
    check(StreamingPolicy.starts(hasSpeaker: false, continuing: false, hadSpeaker: false, origin: .user)
            && StreamingPolicy.starts(hasSpeaker: false, continuing: false, hadSpeaker: true, origin: .user),
          "no speaker on: the user's start with every speaker off streams and asks nothing, no modal alert")
    check(StreamingPolicy.starts(hasSpeaker: false, continuing: false, hadSpeaker: false, origin: .remote),
          "no speaker on: a phone's start with every speaker off streams and asks nothing")
    check(!StreamingPolicy.starts(hasSpeaker: false, continuing: false, hadSpeaker: false, origin: .automatic)
            && !StartOrigin.automatic.raisesAlert,
          "no speaker on: an automatic start waits behind a notice, never a modal alert")
    check(StartOrigin.user.raisesAlert && !StartOrigin.automatic.raisesAlert && !StartOrigin.remote.raisesAlert,
          "no speaker on: only a click at the Mac may raise a modal alert")
    check(!StreamingPolicy.isIdle(streaming: true, hadSpeaker: false, enabledAirPlay: [], enabledLocal: [])
            && StreamingPolicy.showsNoSpeakerOn(streaming: true, enabledAirPlay: [], enabledLocal: []),
          "no speaker on: after that start the switch stays on and the list says no speaker is on")

    final class Saved { var values: [String: Any] = [:] }
    let saved = Saved()
    let disk = SettingsStore(read: { saved.values[$0] }, write: { value, key in saved.values[key] = value })
    func device(_ uid: String, _ transport: UInt32) -> AudioDevices.Device {
        AudioDevices.Device(id: 0, name: uid, uid: uid, transport: transport)
    }
    let usb = device("usb", kAudioDeviceTransportTypeUSB)
    let mac = device("builtin", kAudioDeviceTransportTypeBuiltIn)
    check(disk.speakerOnAtInstall(listed: [], firstInstall: true) == nil
            && saved.values[AppSettings.Keys.speakerOnAtInstall] == nil,
          "first install: an empty device list waits for the next one")
    check(disk.speakerOnAtInstall(listed: [usb, mac], firstInstall: true) == "builtin",
          "first install: This Mac starts on")
    check(disk.speakerOnAtInstall(listed: [usb, mac], firstInstall: true) == nil,
          "first install: This Mac is switched on once, so a later off stays off")
    let updated = SettingsStore(read: { _ in nil }, write: { _, _ in })
    check(updated.speakerOnAtInstall(listed: [mac], firstInstall: false) == nil,
          "first install: an update never switches a speaker on")
    let other = Saved()
    let noMac = SettingsStore(read: { other.values[$0] }, write: { value, key in other.values[key] = value })
    check(noMac.speakerOnAtInstall(listed: [usb], firstInstall: true) == nil
            && noMac.speakerOnAtInstall(listed: [usb, mac], firstInstall: true) == nil,
          "first install: without a built-in output nothing is switched on, and the chance passes")
}

// MARK: Local Network advice only after it was asked and refused
do {
    let t = UIStrings()
    let notes: [String?] = [t.airPlayNoneFound(), t.castList(.noneFound),
                            t.networkSpeakersNote(airPlayNone: true, cast: .noneFound),
                            t.networkSpeakersNote(airPlayNone: true, cast: .quiet)]
    check(notes.allSatisfy { $0?.contains("Local Network") == false && $0?.contains("same network") == true },
          "local network: before it was asked, the notes give only the same-network advice")
    check(t.airPlayNoneFound(localNetworkRefused: true).contains("Local Network")
            && t.airPlayNoneFound(hint: .denied, localNetworkRefused: true) == "No AirPlay speakers found yet.",
          "local network: once refused, the advice names it, unless the hint on screen already does")
}

// MARK: Permission setup window (PermissionFlow.swift, PermissionSetup)
do {
    let fresh = PermissionAsks(explained: [], usedBefore: false)
    check(PermissionSetup.showsAtLaunch(fresh, seen: false),
          "setup: a first install opens the permission setup by itself")
    check(!PermissionSetup.showsAtLaunch(fresh, seen: true),
          "setup: it opens by itself only once, skipped or done")
    check(!PermissionSetup.showsAtLaunch(PermissionAsks(explained: [], usedBefore: true), seen: false),
          "setup: an update from a version without explanations doesn't open it")
    check(!PermissionSetup.showsAtLaunch(PermissionAsks(explained: ["localNetwork", "audioCapture"], usedBefore: false),
                                         seen: false),
          "setup: a user who already went through both explanations doesn't get it")
    check(PermissionSetup.required == [.localNetwork, .audioCapture]
            && PermissionSetup.optional == [.microphone, .automation],
          "setup: Local Network and audio capture are asked together; microphone and Automation wait for their feature")

    check(PermissionSetup.localNetwork(.unknown, asked: false) == .notAsked
            && PermissionSetup.localNetwork(.unknown, asked: true) == .waiting
            && PermissionSetup.localNetwork(.allowed, asked: false) == .allowed
            && PermissionSetup.localNetwork(.denied, asked: true) == .denied,
          "setup: the Local Network row follows the probe's answer")
    check(PermissionSetup.audioCapture(heard: false, asked: false) == .notAsked
            && PermissionSetup.audioCapture(heard: false, asked: true) == .askedUnknown
            && PermissionSetup.audioCapture(heard: true, asked: false) == .allowed,
          "setup: audio capture shows allowed only once a tap heard sound; asked otherwise, since macOS doesn't say")
    check(PermissionSetup.microphone(.notAsked, asking: false) == .notAsked
            && PermissionSetup.microphone(.notAsked, asking: true) == .waiting
            && PermissionSetup.microphone(.allowed, asking: false) == .allowed
            && PermissionSetup.microphone(.denied, asking: false) == .denied,
          "setup: the microphone row follows what macOS reports")

    var rows: [Permission: SetupRowState] = [.localNetwork: .notAsked, .audioCapture: .notAsked]
    check(PermissionSetup.next(rows) == .localNetwork, "setup: Allow asks for Local Network first")
    rows[.localNetwork] = .waiting
    check(PermissionSetup.next(rows) == .audioCapture, "setup: then audio capture")
    rows[.audioCapture] = .askedUnknown
    check(PermissionSetup.next(rows) == nil, "setup: nothing left to ask once both were asked")
    check(PermissionSetup.next([.localNetwork: .allowed, .audioCapture: .notAsked]) == .audioCapture,
          "setup: a permission already allowed is skipped")

    check(PermissionSetup.offersSettings(.denied) && PermissionSetup.offersSettings(.askedUnknown)
            && !PermissionSetup.offersSettings(.allowed) && !PermissionSetup.offersSettings(.notAsked)
            && !PermissionSetup.offersSettings(.waiting),
          "setup: a refused row, or one macOS can't report after asking, offers its Settings link")

    var asks = PermissionAsks(explained: [], usedBefore: false)
    PermissionSetup.required.forEach { asks.accept($0) }
    check(asks.step(.localNetwork, state: .unknown, userAction: false) == .go
            && asks.step(.audioCapture, state: .unknown, userAction: true) == .go
            && asks.step(.microphone, state: .notAsked, userAction: true) == .explain,
          "setup: after Allow, nothing it asked for is explained again; the microphone still is")

    let t = UIStrings()
    let states: [SetupRowState] = [.notAsked, .waiting, .allowed, .denied, .askedUnknown]
    var words: [String] = states.map(t.setupState)
    words += Permission.allCases.map(t.setupDetail)
    words += Permission.allCases.map(t.setupOpenSettings)
    words += [t.setupTitle, t.setupHeading, t.setupIntro, t.setupLater, t.setupAllow, t.setupAsking, t.setupDone,
              t.setupSkip, t.setupAllowOne, t.setupMenu, t.setupAllowMicrophone]
    check(words.allSatisfy { !$0.isEmpty && !$0.contains("\u{2014}") }
            && Set(states.map(t.setupState)).count == states.count,
          "setup: every line is plain text without em dashes, and each state reads differently")
    check(t.setupMenu.hasSuffix("…"), "setup: the menu item that opens the window ends in an ellipsis")
    check(Set(Permission.allCases.map(t.setupOpenSettings)).count == Permission.allCases.count,
          "setup: each Open Settings button names its permission for VoiceOver")
    check(t.setupDetail(.localNetwork).contains("Privacy & Security → Local Network"),
          "setup: the Local Network row gives the full System Settings path")
}

// MARK: Zone header routing table in the main window (ZoneHeaderHit)
do {
    let expected: [(ZoneHeaderHit.Area, ZoneHeaderHit.Action, String)] = [
        (.name, .fold, "a click on the zone name folds or unfolds the members"),
        (.count, .fold, "a click on the speaker count folds or unfolds the members"),
        (.chevron, .fold, "a click on the chevron folds or unfolds the members"),
        (.blank, .fold, "a click on the blank space before the slider and the three dots folds or unfolds the members"),
        (.zoneIcon, .toggleZone, "a click on the round zone icon switches the zone on or off"),
    ]
    for (area, action, what) in expected {
        check(ZoneHeaderHit.action(for: area) == action, "zone header routing table: \(what)",
              "\(ZoneHeaderHit.action(for: area))")
    }
    check(ZoneHeaderHit.Area.allCases.filter { ZoneHeaderHit.action(for: $0) == .toggleZone } == [.zoneIcon],
          "zone header routing table: only the round zone icon switches the zone, so folding leaves it on")
}

print(failedChecks == 0 ? "\nALL LOGIC TESTS PASSED" : "\n\(failedChecks) FAILURE(S)")
exit(failedChecks > 0 ? 1 : 0)
