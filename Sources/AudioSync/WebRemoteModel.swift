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
                  isCast: f.cast != nil)
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

    /// `pickerID` is `AudioSource.defaultsValue`, as `SourceList.entries` gives it.
    init(pickerID: String, name: String) {
        id = Self.pageID(pickerID)
        self.name = name
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

    /// The picker refuses none of its entries.
    var json: [String: Any] {
        ["id": id, "name": name, "kind": kind.rawValue, "available": true, "notice": NSNull()]
    }
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
    /// In the picker's order.
    var sources: [RemoteSource] = []
    /// The page ID of the source in use; nil when the picker doesn't list it.
    var sourceID: String? = nil

    /// One line, keys sorted, so an unchanged snapshot gives the same text.
    var json: String {
        let playing: Any = nowPlaying.map { ["title": $0.title, "artist": $0.artist, "source": $0.source] } ?? NSNull()
        let root: [String: Any] = [
            "computer_name": computerName,
            "master": ["volume": Self.volume(master)],
            "now_playing": playing,
            "speakers": rows.map(\.json),
            "sources": sources.map(\.json),
            "source_id": sourceID.map { $0 as Any } ?? NSNull(),
        ]
        guard let data = try? JSONSerialization.data(withJSONObject: root, options: [.sortedKeys]) else { return "" }
        return String(decoding: data, as: UTF8.self)
    }

    /// The core refuses a volume outside 0…100 or one that isn't finite.
    static func volume(_ v: Double) -> Double { v.isFinite ? min(100, max(0, v)) : 0 }
}

/// A change a phone asked for, as the core hands it over.
enum RemoteCommand: Equatable {
    case speaker(id: String, on: Bool?, volume: Double?, delayMs: Double?)
    case master(volume: Double)
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
        switch type {
        case "speaker":
            guard let id = object["id"] as? String else { return nil }
            let on = (object["on"] as? NSNumber).flatMap { CFGetTypeID($0) == CFBooleanGetTypeID() ? $0.boolValue : nil }
            self = .speaker(id: id, on: on, volume: number("volume"), delayMs: number("delay_ms"))
        case "master":
            guard let volume = number("volume") else { return nil }
            self = .master(volume: volume)
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
    /// A pick in the source picker, by its ID there.
    case selectSource(pickerID: String)

    /// The switch first, then the slider (enabled or not by that switch),
    /// then the delay; a value already in place changes nothing. A source
    /// pick needs a listed source other than the current one.
    static func plan(_ command: RemoteCommand, rows: [RemoteRow],
                     sources: [RemoteSource] = [], sourceID: String? = nil) -> [RemoteAction] {
        switch command {
        case .master(let volume):
            return [.setMaster(position: RemoteSnapshot.volume(volume))]
        case .source(let id):
            guard id != sourceID, sources.contains(where: { $0.id == id }),
                  let pickerID = RemoteSource.pickerID(id) else { return [] }
            return [.selectSource(pickerID: pickerID)]
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
