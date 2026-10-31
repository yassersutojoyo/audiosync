import CAudioSyncCore
import Foundation

// The web remote's logic that runs without a server: the snapshot the app
// publishes, the commands the page sends back, and what each one changes.

/// Which delay a speaker row carries to the page, with the range its field takes.
enum RemoteDelay: Equatable {
    /// No delay of its own: the reference, a Cast group. The page shows 0 and can't change it.
    case fixed
    /// A local speaker's offset relative to the reference (S175).
    case offset(Double)
    /// The AirPlay delay, shared by every AirPlay speaker (S178).
    case airPlay(Double)
    /// A Google Cast speaker's offset, set by ear.
    case cast(Double)

    var ms: Double {
        switch self {
        case .fixed: return 0
        case .offset(let ms), .airPlay(let ms), .cast(let ms): return ms
        }
    }

    /// Lowest and highest whole ms, from the same clamps the window's field uses.
    var range: (min: Int, max: Int) {
        guard let clamp = clamp else { return (0, 0) }
        return (Int((clamp(-.greatestFiniteMagnitude) ?? 0).rounded()),
                Int((clamp(.greatestFiniteMagnitude) ?? 0).rounded()))
    }

    var clamp: ((Double) -> Double?)? {
        switch self {
        case .fixed: return nil
        case .offset: return LocalDelay.sanitizedOffset
        case .airPlay: return LocalDelay.sanitizedAirPlayDelay
        case .cast: return CastModel.clampedOffset
        }
    }
}

/// One speaker as the page shows it, plus what the app needs to apply a command to it.
struct RemoteRow: Equatable {
    var id: String
    var name: String
    /// The kind label the window shows: "AirPlay 2", "Bluetooth"…
    var kind: String
    var on: Bool
    /// Slider position 0…100.
    var volume: Double
    var delay: RemoteDelay
    var available: Bool
    var notice: String?
    /// A phone's volume applies, at once or, for a speaker that is off or a Cast
    /// speaker without a session, when it plays; a fixed Cast volume takes none (S348).
    var volumeAdjustable: Bool
    /// The `SpeakerKey` value zones list it by.
    var key = ""
    /// Switched on, it waits for an AirPlay 2 speaker to free a slot.
    var waiting = false
    /// Switched on, it would ask for a code on the Mac with nobody there.
    var asksForCode = false
    /// How far the page's arrow keys move the volume slider (C69).
    var volumeStep = 1.0
}

extension RemoteRow {
    /// The facts of a speaker row as the main window has them.
    struct Facts {
        var id, name, kind: String
        var on: Bool
        var volume: Double
        var isAirPlay = false
        /// A Google Cast speaker: whether its volume is fixed.
        var castVolumeFixed: Bool? = nil
        /// A Google Cast speaker's own volume step, 0 to 1; 0 while it reports none.
        var castVolumeStep = 0.0
        /// The row's delay field value; nil where the window has none.
        var offsetMs: Double?
        var airPlayDelayMs: Double = 0
        /// Asks for a PIN or password.
        var needsCode = false
        var paired = false
        /// A password speaker whose password this run holds: turning it on asks nothing.
        var holdsPassword = false
        /// A stereo pair or home theater whose member asks for a password
        /// this run hasn't had: turning it on prompts for that member (P176).
        var memberAsksPassword = false
        /// What the window writes under the name, if anything.
        var note: String?
        var key = ""
        var waiting = false
        var streaming = true
    }

    /// A speaker that would ask for a code when turned on can't be turned on
    /// from the phone: the prompt would open on the Mac with nobody there.
    init(_ f: Facts, text: UIStrings) {
        let turnsOn = SpeakerState(enabled: f.on, streaming: f.streaming).clickTurnsOn
        let codeBlocked = (f.needsCode && !f.paired && !f.holdsPassword || f.memberAsksPassword) && turnsOn
        let castStep = f.castVolumeStep > 0 && f.castVolumeStep <= 1 ? f.castVolumeStep : CastModel.fallbackVolumeStep
        let delay: RemoteDelay
        if f.isAirPlay { delay = .airPlay(f.airPlayDelayMs) }
        else if f.castVolumeFixed != nil { delay = f.offsetMs.map(RemoteDelay.cast) ?? .fixed }
        else { delay = f.offsetMs.map(RemoteDelay.offset) ?? .fixed }
        // Like the window's button: a speaker that will play still turns on.
        self.init(id: f.id, name: f.name, kind: f.kind, on: !turnsOn, volume: f.volume, delay: delay,
                  available: !codeBlocked,
                  notice: codeBlocked ? text.remoteTurnOnInApp : f.note,
                  volumeAdjustable: f.castVolumeFixed != true,
                  key: f.key, waiting: f.waiting, asksForCode: codeBlocked,
                  volumeStep: f.castVolumeFixed == nil ? 1 : max(1, (100 * castStep).rounded()))
    }

    var json: [String: Any] {
        let range = delay.range
        return [
            "id": id, "name": name, "kind": kind, "on": on,
            "volume": RemoteSnapshot.volume(volume), "volume_step": volumeStep,
            "delay_ms": min(range.max, max(range.min, Int(delay.ms.rounded()))),
            "delay_min_ms": range.min, "delay_max_ms": range.max,
            "available": available,
            "notice": notice.map { $0 as Any } ?? NSNull(),
        ]
    }
}

/// A zone as the page shows it (S256): its members the page lists, in zone
/// order, and how many of them are on; hidden or absent members don't count.
struct RemoteZoneRow: Equatable {
    var id, name: String
    var volume: Double
    var members: [String]
    var on = 0
    var waiting = 0
    /// Every member of `zone`, listed or not, for "None of 3 found".
    var all = 0

    init(id: String, name: String, volume: Double, members: [String], on: Int = 0, waiting: Int = 0) {
        self.id = id
        self.name = name
        self.volume = volume
        self.members = members
        self.on = on
        self.waiting = waiting
    }

    init(_ zone: SpeakerZone, listed: [RemoteRow]) {
        let present = zone.members.compactMap { key in listed.first { $0.key == key } }
        self.init(id: zone.id, name: zone.name, volume: zone.volume, members: present.map(\.id),
                  on: present.filter(\.on).count, waiting: present.filter { $0.on && $0.waiting }.count)
        all = zone.members.count
    }

    var json: [String: Any] {
        ["id": id, "name": name, "volume": RemoteSnapshot.volume(volume), "members": members,
         "counts": ["on": on, "total": members.count, "waiting": waiting, "all": all]]
    }
}

/// A preset as the page lists it; `current` as S253 picks it.
struct RemotePresetRow: Equatable {
    var id, name: String
    var current: Bool
    /// The stored members' `SpeakerKey` values; the page never sees them.
    var members: [String] = []

    var json: [String: Any] { ["id": id, "name": name, "current": current] }
}

/// One entry of the source picker as the page lists it.
struct RemoteSource: Equatable {
    enum Kind: String {
        case system, app, input
    }

    /// "system", "app:<bundle ID>", "input:<uid>" or a mix's own "apps:…" or
    /// "except:…": the core takes no empty ID, which is what the picker gives
    /// the whole system.
    var id: String
    var name: String
    var kind: Kind
    var available: Bool
    var notice: String?

    /// `pickerID` is `AudioSource.defaultsValue`, as `SourceList.entries` gives it;
    /// `notConnected` is the note of an unplugged chosen input, which the page greys out.
    init(pickerID: String, name: String, notConnected: String? = nil) {
        id = Self.pageID(pickerID)
        self.name = name
        available = notConnected == nil
        notice = notConnected
        kind = switch AudioSource(defaultsValue: pickerID) {
        case .system: .system
        case .app, .mix: .app
        case .input: .input
        }
    }

    static func pageID(_ pickerID: String) -> String {
        switch AudioSource(defaultsValue: pickerID) {
        case .system: "system"
        case .app(let bundleID): "app:" + bundleID
        case .mix, .input: pickerID
        }
    }

    /// The picker's ID for a page ID; nil for one that names no source.
    static func pickerID(_ pageID: String) -> String? {
        if pageID == "system" { return AudioSource.system.defaultsValue }
        if pageID.hasPrefix("app:"), pageID.count > 4 { return String(pageID.dropFirst(4)) }
        for tag in ["input:", "apps:", "except:"] where pageID.hasPrefix(tag) && pageID.count > tag.count {
            return pageID
        }
        return nil
    }

    /// Only an unplugged chosen input is unavailable, as the picker greys it.
    var json: [String: Any] {
        ["id": id, "name": name, "kind": kind.rawValue, "available": available,
         "notice": notice.map { $0 as Any } ?? NSNull()]
    }
}

/// Everything the page shows, as `asc_remote_publish` takes it.
struct RemoteSnapshot: Equatable {
    struct Playing: Equatable {
        var title, artist, source: String
        /// False while the app is paused.
        var playing = true
    }

    var computerName: String
    var master: Double
    var nowPlaying: Playing?
    var rows: [RemoteRow]
    var zones: [RemoteZoneRow] = []
    var presets: [RemotePresetRow] = []
    /// A microphone run holds the outputs (S258): the page takes no change.
    var calibrating = false
    /// In the picker's order.
    var sources: [RemoteSource] = []
    /// The page ID of the source in use; nil when the picker doesn't list it.
    var sourceID: String? = nil

    /// One line, keys sorted, so an unchanged snapshot gives the same text.
    var json: String {
        let playing: Any = nowPlaying.map { ["title": $0.title, "artist": $0.artist, "source": $0.source, "playing": $0.playing] } ?? NSNull()
        let root: [String: Any] = [
            "computer_name": computerName,
            "master": ["volume": Self.volume(master)],
            "now_playing": playing,
            "speakers": rows.map(\.json),
            "zones": zones.map(\.json),
            "presets": presets.map(\.json),
            "calibrating": calibrating,
            "sources": sources.map(\.json),
            "source_id": sourceID.map { $0 as Any } ?? NSNull(),
        ]
        guard let data = try? JSONSerialization.data(withJSONObject: root, options: [.sortedKeys]) else { return "" }
        return String(decoding: data, as: UTF8.self)
    }

    /// The core refuses a volume outside 0…100 or one that isn't finite.
    static func volume(_ v: Double) -> Double { v.isFinite ? min(100, max(0, v)) : 0 }

    /// Any microphone run (S258): one speaker's, Calibrate All or Measure
    /// Again, also while a cut-off sweep plays out.
    static func calibrating(_ one: CalibrationStatus, _ all: CalibrateAllStatus) -> Bool {
        one.isRunning || one.holdsOutputs || all.isRunning || all.holdsOutputs
    }
}

/// A change a phone asked for, as the core hands it over.
enum RemoteCommand: Equatable {
    case speaker(id: String, on: Bool?, volume: Double?, delayMs: Double?)
    case master(volume: Double)
    case zone(id: String, toggle: Bool, volume: Double?)
    case preset(id: String)
    /// `id` is the page's ID (`RemoteSource.id`).
    case source(id: String)

    /// Nil for anything that doesn't read as one.
    init?(json: String) {
        guard let object = (try? JSONSerialization.jsonObject(with: Data(json.utf8))) as? [String: Any],
              let type = object["type"] as? String else { return nil }
        let number = { (key: String) -> Double? in
            guard let n = object[key] as? NSNumber, CFGetTypeID(n) != CFBooleanGetTypeID() else { return nil }
            return n.doubleValue.isFinite ? n.doubleValue : nil
        }
        let flag = { (key: String) -> Bool? in
            (object[key] as? NSNumber).flatMap { CFGetTypeID($0) == CFBooleanGetTypeID() ? $0.boolValue : nil }
        }
        switch type {
        case "speaker":
            guard let id = object["id"] as? String else { return nil }
            self = .speaker(id: id, on: flag("on"), volume: number("volume"), delayMs: number("delay_ms"))
        case "master":
            guard let volume = number("volume") else { return nil }
            self = .master(volume: volume)
        case "zone":
            guard let id = object["id"] as? String else { return nil }
            let toggle = flag("toggle") ?? false
            let volume = number("volume")
            guard toggle || volume != nil else { return nil }
            self = .zone(id: id, toggle: toggle, volume: volume)
        case "preset":
            guard let id = object["id"] as? String, flag("recall") == true else { return nil }
            self = .preset(id: id)
        case "source":
            guard let id = object["id"] as? String else { return nil }
            self = .source(id: id)
        default:
            return nil
        }
    }
}

/// What applying a command does, one window control at a time.
enum RemoteAction: Equatable {
    case setEnabled(id: String, on: Bool)
    case setVolume(id: String, position: Double)
    case setLocalOffset(id: String, ms: Double)
    case setCastOffset(id: String, ms: Double)
    case setAirPlayDelay(ms: Double)
    case setMaster(position: Double)
    case setZoneVolume(id: String, position: Double)
    /// `keepOff`: the members' keys that would ask for a code; they stay
    /// off with a notice, and the rest plays (S257).
    case toggleZone(id: String, keepOff: [String])
    case recallPreset(id: String, keepOff: [String])
    /// A pick in the source picker, by its ID there.
    case selectSource(pickerID: String)

    /// The switch first, then the volume,
    /// then the delay; a switch or delay already in place changes nothing, and
    /// a volume moves the row even when it equals the shown number (S348). A source
    /// pick needs a listed source; one of the source in use clears a failed
    /// switch's notice, as in the picker (S163). Nothing while calibrating:
    /// the page's snapshot can lag behind a run (S258).
    static func plan(_ command: RemoteCommand, rows: [RemoteRow], zones: [RemoteZoneRow] = [],
                     presets: [RemotePresetRow] = [], calibrating: Bool = false,
                     sources: [RemoteSource] = []) -> [RemoteAction] {
        guard !calibrating else { return [] }
        switch command {
        case .zone(let id, let toggle, let volume):
            guard let zone = zones.first(where: { $0.id == id }) else { return [] }
            var actions: [RemoteAction] = []
            if let volume { actions.append(.setZoneVolume(id: id, position: RemoteSnapshot.volume(volume))) }
            let members = zone.members.compactMap { member in rows.first { $0.id == member } }
            if toggle, members.contains(where: \.available) {
                actions.append(.toggleZone(id: id, keepOff: members.filter(\.asksForCode).map(\.key)))
            }
            return actions
        case .preset(let id):
            guard let preset = presets.first(where: { $0.id == id }) else { return [] }
            let keepOff = rows.filter { $0.asksForCode && preset.members.contains($0.key) }.map(\.key)
            return [.recallPreset(id: id, keepOff: keepOff)]
        case .master(let volume):
            return [.setMaster(position: RemoteSnapshot.volume(volume))]
        case .source(let id):
            guard sources.contains(where: { $0.id == id }),
                  let pickerID = RemoteSource.pickerID(id) else { return [] }
            return [.selectSource(pickerID: pickerID)]
        case .speaker(let id, let on, let volume, let delayMs):
            guard let row = rows.first(where: { $0.id == id }), row.available else { return [] }
            var actions: [RemoteAction] = []
            if let on, on != row.on {
                actions.append(.setEnabled(id: id, on: on))
            }
            if let volume, row.volumeAdjustable {
                actions.append(.setVolume(id: id, position: RemoteSnapshot.volume(volume)))
            }
            if let delayMs, let clamp = row.delay.clamp, let ms = clamp(delayMs.rounded()), ms != row.delay.ms {
                switch row.delay {
                case .offset: actions.append(.setLocalOffset(id: id, ms: ms))
                case .cast: actions.append(.setCastOffset(id: id, ms: ms))
                case .airPlay: actions.append(.setAirPlayDelay(ms: ms))
                case .fixed: break
                }
            }
            return actions
        }
    }
}

/// The page sends a step every 100 ms while a slider moves and never says when
/// it stops, so a local or AirPlay volume is logged once no later step for it
/// came (S348).
struct RemoteVolumeLog {
    private var steps: [String: Int] = [:]

    /// A plan of volume steps alone gets no line of its own: its speakers'
    /// lines come after their last step.
    static func logsPlan(_ actions: [RemoteAction]) -> Bool {
        actions.isEmpty || actions.contains { action in
            if case .setVolume = action { return false }
            return true
        }
    }

    /// The step's number, for `isLast` once the pause has passed.
    mutating func step(_ id: String) -> Int {
        let n = (steps[id] ?? 0) + 1
        steps[id] = n
        return n
    }

    func isLast(_ id: String, _ n: Int) -> Bool { steps[id] == n }
}

/// The port the user types in the setting.
enum RemotePort {
    static let `default` = Int(ASC_REMOTE_DEFAULT_PORT)
    /// Below 1024 needs root on many systems; 0 would pick a random port the phone can't keep.
    static let range = 1024...65535

    static func parse(_ typed: String) -> Int? {
        guard let n = Int(typed.trimmingCharacters(in: .whitespaces)), range.contains(n) else { return nil }
        return n
    }

    static func stored(_ n: Double) -> Int? {
        n == n.rounded() && (Double(range.lowerBound)...Double(range.upperBound)).contains(n) ? Int(n) : nil
    }
}

/// What `asc_remote_start` gets. Only the debug log asks the core for the
/// pairing codes, since whoever reads that log can pair while a code works.
enum RemoteStartConfig {
    static func make(port: Int, localName: String?, devicesPath: String?, debugLog: Bool) -> [String: Any] {
        var config: [String: Any] = ["port": port]
        if let localName { config["local_name"] = localName }
        if let devicesPath { config["devices_path"] = devicesPath }
        if debugLog { config["log_pairing_codes"] = true }
        return config
    }
}

/// `AUDIOSYNC_WEB_REMOTE=1` with the debug log on starts the web remote for
/// one run without touching its setting, so a browser check needs no click.
enum RemoteDebugSwitch {
    static let switchKey = "AUDIOSYNC_WEB_REMOTE"
    static let portKey = "AUDIOSYNC_WEB_REMOTE_PORT"

    /// The port for that run: `AUDIOSYNC_WEB_REMOTE_PORT` when the setting
    /// would take it, the saved port otherwise; nil when the switch is off.
    static func port(environment: [String: String], debugLog: Bool, savedPort: Int) -> Int? {
        guard debugLog, environment[switchKey] == "1" else { return nil }
        return environment[portKey].flatMap(RemotePort.parse) ?? savedPort
    }
}

/// Choosing Web Remote (S368): a new code would kill one a phone is typing and lift a
/// lockout, so only a hidden window opening makes one; a showing window just comes forward.
enum RemoteWindowShow {
    /// `visible` is nil before the window was ever made.
    static func opens(visible: Bool?) -> Bool { visible != true }
}

/// The paired devices' file the core keeps (`{"version": 1, "devices": [SHA-256 hex…], "seen": [Unix seconds…]}`),
/// read and cleared here while the remote is off, so a lost phone can be
/// unpaired before the remote is turned on again.
enum RemoteDevicesFile {
    static let maxDevices = 8
    /// The core forgets a device after this long without a request.
    static let idleLimit: TimeInterval = 90 * 24 * 60 * 60

    /// The devices the core would load at `now`: the newest 8 whose hash reads,
    /// less those idle for 90 days. A file the core can't read counts as none. A
    /// time not written as a whole number counts its device as seen now, and so
    /// does a "seen" list that is missing or of another length for every device.
    static func count(_ data: Data?, now: Date = Date()) -> Int {
        guard let data, let object = (try? JSONSerialization.jsonObject(with: data)) as? [String: Any],
              let version = integer(object["version"]), version.int64Value == 1,
              let devices = object["devices"] as? [Any] else { return 0 }
        let seen = (object["seen"] as? [Any]).flatMap { $0.count == devices.count ? $0 : nil }
        let nowSeconds = now.timeIntervalSince1970
        let times = devices.indices.compactMap { index -> Double? in
            guard (devices[index] as? String).map(isHash) ?? false else { return nil }
            return seen.flatMap { unixSeconds($0[index]) } ?? nowSeconds
        }
        let cutoff = nowSeconds - idleLimit
        return times.suffix(maxDevices).filter { $0 > cutoff }.count
    }

    static func count(at url: URL) -> Int { count(try? Data(contentsOf: url)) }

    private static func isBool(_ number: NSNumber) -> Bool { CFGetTypeID(number) == CFBooleanGetTypeID() }

    /// A number written as a JSON integer, as the core's serde_json `as_u64` wants:
    /// 1.0 and 1e9 parse as floats there and don't read.
    private static func integer(_ value: Any?) -> NSNumber? {
        guard let number = value as? NSNumber, !isBool(number),
              "cCsSiIlLqQ".utf8.contains(UInt8(bitPattern: number.objCType.pointee)) else { return nil }
        return number
    }

    /// Past Int64 the core reads a time ahead of now; counting it as seen now gives the same count.
    private static func unixSeconds(_ value: Any) -> Double? {
        guard let number = integer(value), number.int64Value >= 0 else { return nil }
        return Double(number.int64Value)
    }

    /// The core empties its list before the save that failed, so the count reads 0 then;
    /// Forget All stays available so the user can try again, as the failure text says.
    static func canForget(count: Int, failed: Bool) -> Bool { count > 0 || failed }

    /// A file that isn't there is already forgotten.
    static func forget(at url: URL) -> Bool {
        do {
            try FileManager.default.removeItem(at: url)
            return true
        } catch {
            return !FileManager.default.fileExists(atPath: url.path)
        }
    }

    private static func isHash(_ text: String) -> Bool {
        text.utf8.count == 64 && text.utf8.allSatisfy { (48...57).contains($0) || (97...102).contains($0) || (65...70).contains($0) }
    }
}

/// The addresses the setting shows: the Bonjour name first, since it
/// survives a new IP address from the router, then the IP address.
struct RemoteAddresses: Equatable {
    var localHostName: String?
    var ipv4: String?

    func urls(port: Int) -> [String] {
        [localHostName.map { "\($0).local" }, ipv4].compactMap { $0 }.map { "http://\($0):\(port)" }
    }
}
