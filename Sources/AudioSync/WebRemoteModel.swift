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
    /// The window's slider takes a change now.
    var volumeAdjustable: Bool
    /// Turning a Cast speaker on doesn't enable its slider until its session runs.
    var isCast = false
    /// The `SpeakerKey` value zones list it by.
    var key = ""
    /// Switched on, it waits for an AirPlay 2 speaker to free a slot.
    var waiting = false
}

extension RemoteRow {
    /// The facts of a speaker row as the main window has them.
    struct Facts {
        var id, name, kind: String
        var on: Bool
        var volume: Double
        var isAirPlay = false
        /// A Google Cast speaker: its session state.
        var cast: (live: Bool, volumeFixed: Bool)? = nil
        /// The row's delay field value; nil where the window has none.
        var offsetMs: Double?
        var airPlayDelayMs: Double = 0
        /// Asks for a PIN or password.
        var needsCode = false
        var paired = false
        /// What the window writes under the name, if anything.
        var note: String?
        var key = ""
        var waiting = false
    }

    /// A speaker that would ask for a code when turned on can't be turned on
    /// from the phone: the prompt would open on the Mac with nobody there.
    init(_ f: Facts, text: UIStrings) {
        let codeBlocked = f.needsCode && !f.paired && !f.on
        let delay: RemoteDelay
        if f.isAirPlay { delay = .airPlay(f.airPlayDelayMs) }
        else if f.cast != nil { delay = f.offsetMs.map(RemoteDelay.cast) ?? .fixed }
        else { delay = f.offsetMs.map(RemoteDelay.offset) ?? .fixed }
        let castVolume = f.cast.map { $0.live && !$0.volumeFixed } ?? true
        self.init(id: f.id, name: f.name, kind: f.kind, on: f.on, volume: f.volume, delay: delay,
                  available: !codeBlocked,
                  notice: f.note ?? (codeBlocked ? text.remoteTurnOnInApp : nil),
                  volumeAdjustable: f.on && castVolume,
                  isCast: f.cast != nil, key: f.key, waiting: f.waiting)
    }

    var json: [String: Any] {
        let range = delay.range
        return [
            "id": id, "name": name, "kind": kind, "on": on,
            "volume": RemoteSnapshot.volume(volume),
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
    }

    var json: [String: Any] {
        ["id": id, "name": name, "volume": RemoteSnapshot.volume(volume), "members": members,
         "counts": ["on": on, "total": members.count, "waiting": waiting]]
    }
}

/// A preset as the page lists it; `current` as S253 picks it.
struct RemotePresetRow: Equatable {
    var id, name: String
    var current: Bool

    var json: [String: Any] { ["id": id, "name": name, "current": current] }
}

/// Everything the page shows, as `asc_remote_publish` takes it.
struct RemoteSnapshot: Equatable {
    struct Playing: Equatable {
        var title, artist, source: String
    }

    var computerName: String
    var master: Double
    var nowPlaying: Playing?
    var rows: [RemoteRow]
    var zones: [RemoteZoneRow] = []
    var presets: [RemotePresetRow] = []
    /// A microphone run holds the outputs (S258): the page takes no change.
    var calibrating = false

    /// One line, keys sorted, so an unchanged snapshot gives the same text.
    var json: String {
        let playing: Any = nowPlaying.map { ["title": $0.title, "artist": $0.artist, "source": $0.source] } ?? NSNull()
        let root: [String: Any] = [
            "computer_name": computerName,
            "master": ["volume": Self.volume(master)],
            "now_playing": playing,
            "speakers": rows.map(\.json),
            "zones": zones.map(\.json),
            "presets": presets.map(\.json),
            "calibrating": calibrating,
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
    case toggleZone(id: String)
    case recallPreset(id: String)

    /// The switch first, then the slider (enabled or not by that switch),
    /// then the delay; a value already in place changes nothing. Nothing
    /// while calibrating: the page's snapshot can lag behind a run (S258).
    static func plan(_ command: RemoteCommand, rows: [RemoteRow], zones: [RemoteZoneRow] = [],
                     presets: [RemotePresetRow] = [], calibrating: Bool = false) -> [RemoteAction] {
        guard !calibrating else { return [] }
        switch command {
        case .zone(let id, let toggle, let volume):
            guard let zone = zones.first(where: { $0.id == id }) else { return [] }
            var actions: [RemoteAction] = []
            if let volume { actions.append(.setZoneVolume(id: id, position: RemoteSnapshot.volume(volume))) }
            if toggle, !zone.members.isEmpty { actions.append(.toggleZone(id: id)) }
            return actions
        case .preset(let id):
            return presets.contains { $0.id == id } ? [.recallPreset(id: id)] : []
        case .master(let volume):
            return [.setMaster(position: RemoteSnapshot.volume(volume))]
        case .speaker(let id, let on, let volume, let delayMs):
            guard let row = rows.first(where: { $0.id == id }), row.available else { return [] }
            var actions: [RemoteAction] = []
            var nowOn = row.on
            if let on, on != row.on {
                actions.append(.setEnabled(id: id, on: on))
                nowOn = on
            }
            let adjustable = nowOn == row.on ? row.volumeAdjustable : (nowOn && !row.isCast)
            if let volume, adjustable, RemoteSnapshot.volume(volume) != row.volume {
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

/// The paired devices' file the core keeps (`{"version": 1, "devices": [SHA-256 hex…]}`),
/// read and cleared here while the remote is off, so a lost phone can be
/// unpaired before the remote is turned on again.
enum RemoteDevicesFile {
    static let maxDevices = 8

    /// The core starts empty from a file it can't read, so such a file counts as none.
    static func count(_ data: Data?) -> Int {
        guard let data, let object = (try? JSONSerialization.jsonObject(with: data)) as? [String: Any],
              let version = object["version"] as? NSNumber, CFGetTypeID(version) != CFBooleanGetTypeID(),
              version.doubleValue == 1, let devices = object["devices"] as? [Any],
              devices.allSatisfy({ ($0 as? String).map(isHash) ?? false }) else { return 0 }
        return min(devices.count, maxDevices)
    }

    static func count(at url: URL) -> Int { count(try? Data(contentsOf: url)) }

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
