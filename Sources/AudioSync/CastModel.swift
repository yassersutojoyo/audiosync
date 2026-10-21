import CAudioSyncCore
import Foundation

// Google Cast speakers, through the core's Cast sender (`asc_cast_*`): what
// discovery lists, the one device the user turned on, its session's attempt
// and status, its volume and its offset. Pure state, so the logic suite
// drives it; `CastSender` talks to the core and `AppDelegate+Cast` wires the
// two together.
//
// One device plays at a time (CAST_SPEC C2) and a Cast group is listed but
// never driven (C3). Cast has no shared clock, so a device is kept in step
// only by the offset the user sets by ear (C74 to C77): while one is on,
// local outputs wait that long for it. Every event of a session carries the
// attempt number `asc_cast_start` returned; one of an attempt that is no
// longer current is dropped, so a late end never turns off a speaker the
// user turned on again.

/// A Cast device's ID as the app's rows and notices use it.
enum CastID {
    static let prefix = "cast:"

    static func app(_ device: String) -> String { prefix + device }

    static func device(_ appID: String) -> String? {
        appID.hasPrefix(prefix) ? String(appID.dropFirst(prefix.count)) : nil
    }
}

struct CastDevice: Equatable {
    let id: String
    var name: String
    var model: String
    /// A Cast group: listed, never played to.
    var group: Bool
    /// Its records went away while a session with it runs.
    var offline: Bool

    init(id: String, name: String, model: String = "", group: Bool = false, offline: Bool = false) {
        self.id = id
        self.name = name
        self.model = model
        self.group = group
        self.offline = offline
    }
}

enum CastDeviceChange: Equatable {
    /// `offsetMs`: what the device memory holds for it, nil when nothing.
    case appeared(CastDevice, offsetMs: Double?)
    case changed(CastDevice, offsetMs: Double?)
    case gone(id: String)
}

/// Where the session of the device that is on stands.
enum CastStatus: Equatable {
    case idle, connecting, playing, reconnecting
}

/// Why a Cast speaker went off, or stayed off, for its notice.
enum CastProblem: Equatable {
    /// Another Cast speaker is on: one at a time.
    case busy
    case notResponding
    case refused
    /// The device never fetched the stream: most likely a firewall.
    case blocked
    case takenOver
    case connectionLost
    case group
}

enum CastError: Equatable {
    case connect, launch, media, noFetch, stream, noSession, volumeFixed, volume, artwork, group
    case other(Int32)

    init?(_ code: Int32) {
        switch code {
        case 0: return nil
        case ASC_CAST_ERROR_CONNECT: self = .connect
        case ASC_CAST_ERROR_LAUNCH: self = .launch
        case ASC_CAST_ERROR_MEDIA: self = .media
        case ASC_CAST_ERROR_NO_FETCH: self = .noFetch
        case ASC_CAST_ERROR_STREAM: self = .stream
        case ASC_CAST_ERROR_NO_SESSION: self = .noSession
        case ASC_CAST_ERROR_VOLUME_FIXED: self = .volumeFixed
        case ASC_CAST_ERROR_VOLUME: self = .volume
        case ASC_CAST_ERROR_ARTWORK: self = .artwork
        case ASC_CAST_ERROR_GROUP: self = .group
        default: self = .other(code)
        }
    }
}

/// How an attempt ended.
enum CastEnd: Equatable {
    /// AudioSync's own stop.
    case stopped
    /// Another sender stopped or replaced the app or the stream.
    case takenOver
    /// Stopped before it started.
    case deselected
    case failed(CastError?)

    /// What the user is told; nil for a stop the user or AudioSync asked for.
    var problem: CastProblem? {
        switch self {
        case .stopped, .deselected: return nil
        case .takenOver: return .takenOver
        case .failed(.group): return .group
        case .failed(.connect): return .notResponding
        case .failed(.launch), .failed(.media): return .refused
        case .failed(.noFetch): return .blocked
        case .failed(.stream): return .connectionLost
        case .failed: return .notResponding
        }
    }
}

struct CastVolume: Equatable {
    /// 0 to 1.
    var level: Double
    var muted: Bool
    /// The device's volume can't be changed from here.
    var fixed: Bool
}

/// What the sender reported, copied out of the core's event.
enum CastEvent: Equatable {
    case started(attempt: UInt64)
    case playing(attempt: UInt64)
    case connectFailed(attempt: UInt64, error: CastError?, retryMs: Int64)
    case recovering(attempt: UInt64)
    case recovered(attempt: UInt64)
    case reloaded(attempt: UInt64)
    case volume(attempt: UInt64, CastVolume)
    case commandFailed(attempt: UInt64, error: CastError?)
    case ended(attempt: UInt64, CastEnd)
    /// Diagnostics, format fallbacks, rebuffering and the stream's own log:
    /// for the debug log only.
    case note(attempt: UInt64, String)
    case memoryChanged
    case discoveryStopped
    /// The sender stopped working; `detail` names where it panicked, or "".
    case failed(detail: String)

    var attempt: UInt64? {
        switch self {
        case .started(let a), .playing(let a), .connectFailed(let a, _, _), .recovering(let a),
             .recovered(let a), .reloaded(let a), .volume(let a, _), .commandFailed(let a, _),
             .ended(let a, _), .note(let a, _):
            return a
        case .memoryChanged, .discoveryStopped, .failed:
            return nil
        }
    }

    /// Copies `raw` right away: its strings last only until the next
    /// `asc_cast_next_event`. Nil for a kind this build doesn't know.
    init?(_ raw: AscCastEvent) {
        let a = raw.attempt
        let detail = raw.detail.map { String(cString: $0) } ?? ""
        let error = CastError(raw.error)
        switch raw.kind {
        case ASC_CAST_EVENT_STARTED: self = .started(attempt: a)
        case ASC_CAST_EVENT_PLAYING: self = .playing(attempt: a)
        case ASC_CAST_EVENT_CONNECT_FAILED: self = .connectFailed(attempt: a, error: error, retryMs: raw.retry_ms)
        case ASC_CAST_EVENT_RECOVERING: self = .recovering(attempt: a)
        case ASC_CAST_EVENT_RECOVERED: self = .recovered(attempt: a)
        case ASC_CAST_EVENT_RELOADED: self = .reloaded(attempt: a)
        case ASC_CAST_EVENT_VOLUME:
            self = .volume(attempt: a, CastVolume(level: raw.level, muted: raw.muted, fixed: raw.volume_fixed))
        case ASC_CAST_EVENT_COMMAND_FAILED: self = .commandFailed(attempt: a, error: error)
        case ASC_CAST_EVENT_ENDED:
            let end: CastEnd
            switch raw.reason {
            case ASC_CAST_END_STOPPED: end = .stopped
            case ASC_CAST_END_TAKEN_OVER: end = .takenOver
            case ASC_CAST_END_DESELECTED: end = .deselected
            default: end = .failed(error)
            }
            self = .ended(attempt: a, end)
        case ASC_CAST_EVENT_REBUFFERING: self = .note(attempt: a, "the device ran dry")
        case ASC_CAST_EVENT_FORMAT_FALLBACK:
            self = .note(attempt: a, "the device refused WAV; format \(raw.format) instead")
        case ASC_CAST_EVENT_DIAGNOSTICS:
            self = .note(attempt: a, String(format: "drift %.0f ms, queued %.0f ms", raw.drift_ms, raw.queued_ms))
        case ASC_CAST_EVENT_STREAM: self = .note(attempt: a, "stream: " + detail)
        case ASC_CAST_EVENT_MEMORY_CHANGED: self = .memoryChanged
        case ASC_CAST_EVENT_DISCOVERY_STOPPED: self = .discoveryStopped
        case ASC_CAST_EVENT_FAILED: self = .failed(detail: detail)
        default: return nil
        }
    }
}

extension CastDeviceChange {
    /// Copies `raw` right away (its strings last only until the next
    /// `asc_cast_next_device`), with the offset the memory holds for it.
    init?(_ raw: AscCastDevice, offsetMs: (String) -> Double?) {
        let id = raw.id.map { String(cString: $0) } ?? ""
        guard !id.isEmpty else { return nil }
        let device = CastDevice(id: id, name: raw.name.map { String(cString: $0) } ?? "",
                                model: raw.model.map { String(cString: $0) } ?? "",
                                group: raw.flags & UInt32(ASC_CAST_FLAG_GROUP) != 0,
                                offline: raw.flags & UInt32(ASC_CAST_FLAG_OFFLINE) != 0)
        switch raw.event {
        case ASC_CAST_APPEARED: self = .appeared(device, offsetMs: offsetMs(id))
        case ASC_CAST_CHANGED: self = .changed(device, offsetMs: offsetMs(id))
        case ASC_CAST_GONE: self = .gone(id: id)
        default: return nil
        }
    }
}

/// Why `asc_cast_start` refused, as a notice.
enum CastRefusal {
    static func problem(_ code: Int32) -> CastProblem {
        switch code {
        case ASC_CAST_REFUSED_BUSY: return .busy
        case ASC_CAST_REFUSED_GROUP: return .group
        default: return .notResponding
        }
    }
}

/// A Cast speaker's row beyond the name, volume and offset.
struct CastRow: Equatable {
    var status: CastStatus
    /// The device reports itself muted.
    var muted: Bool
    /// The device's volume can't be changed from here.
    var volumeFixed: Bool
    /// A Cast group, which AudioSync can't play to.
    var group: Bool
    /// A session runs or starts: the volume, Mute and Resync reach the device.
    var live: Bool { status != .idle }
}

/// What the speaker list says about Google Cast while it lists none.
enum CastListState: Equatable {
    /// Nothing to say: speakers are listed, or the search hasn't started.
    case quiet
    case searching
    case noneFound
    case failed
}

/// What the app does to the sender next.
enum CastPlan: Equatable {
    case none
    case start(String)
    case stop
}

/// What the app does after an event or a start's answer.
enum CastOutcome: Equatable {
    /// The row changed: refresh the views.
    case changed
    /// The device plays: whatever went wrong is over.
    case playing(String)
    /// The device went off on its own; a notice says why when `problem` is set.
    case turnedOff(String, problem: CastProblem?)
    case memoryChanged
    case senderFailed
}

struct CastModel {
    /// After this long browsing without a device, the list says none was found.
    static let noneFoundAfter: TimeInterval = 20
    /// The offset range the delay field takes, in ms.
    static let maxOffsetMs: Double = 20_000

    enum Discovery: Equatable {
        case off
        case browsing(since: Date)
        case failed
    }

    struct Attempt: Equatable {
        let number: UInt64
        let device: String
        var status: CastStatus
    }

    var discovery = Discovery.off
    private(set) var devices: [String: CastDevice] = [:]
    /// The one device the user turned on.
    private(set) var enabled: String?
    /// The current session; its events are the only ones that count.
    private(set) var attempt: Attempt?
    /// A start sent to the sender whose attempt number hasn't come back yet.
    private(set) var starting: String?
    /// Each device's volume as its latest status reports it.
    private(set) var volumes: [String: CastVolume] = [:]
    /// A level the user set that no status has answered yet.
    private(set) var requested: (id: String, level: Double)?
    /// The offset the device memory holds, per device, in ms.
    private(set) var offsets: [String: Double] = [:]

    /// The devices listed, by name.
    var rows: [String] {
        devices.values.sorted { a, b in
            switch a.name.compare(b.name, options: [.caseInsensitive, .numeric]) {
            case .orderedAscending: return true
            case .orderedDescending: return false
            case .orderedSame: return a.id < b.id
            }
        }.map(\.id)
    }

    /// Whether the change altered what the list shows.
    @discardableResult
    mutating func apply(_ change: CastDeviceChange) -> Bool {
        switch change {
        case .appeared(let device, let offset), .changed(let device, let offset):
            if let offset { offsets[device.id] = offset }
            let changed = devices[device.id] != device
            devices[device.id] = device
            return changed
        case .gone(let id):
            return devices.removeValue(forKey: id) != nil
        }
    }

    func status(of id: String) -> CastStatus {
        attempt?.device == id ? attempt!.status : (starting == id ? .connecting : .idle)
    }

    /// A session runs or starts for `id`: its volume, Mute and Resync reach it.
    func isLive(_ id: String) -> Bool { status(of: id) != .idle }

    /// The slider position, 0 to 100: the level the user set until a status
    /// answers it, else the reported one.
    func volume(of id: String) -> Double {
        let level = requested?.id == id ? requested!.level : (volumes[id]?.level ?? 1)
        return min(100, max(0, level * 100))
    }

    func offset(of id: String) -> Double { offsets[id] ?? 0 }

    /// The user turns `id` on: refused while another device is on, and for a group.
    mutating func turnOn(_ id: String) -> CastProblem? {
        if let other = enabled, other != id { return .busy }
        if devices[id]?.group == true { return .group }
        enabled = id
        return nil
    }

    /// The user turns `id` off. Returns whether it was on.
    @discardableResult
    mutating func turnOff(_ id: String) -> Bool {
        guard enabled == id else { return false }
        enabled = nil
        return true
    }

    /// A session runs while its device is on and streaming is on, whatever
    /// discovery says meanwhile; a new one starts once the device is listed
    /// and online. Nothing new while a start waits for its number.
    func plan(streaming: Bool) -> CastPlan {
        if let attempt, enabled != attempt.device || !streaming { return .stop }
        guard attempt == nil, starting == nil, streaming, let id = enabled,
              let device = devices[id], !device.offline, !device.group else { return .none }
        return .start(id)
    }

    mutating func startSent(_ id: String) { starting = id }

    mutating func stopSent() {
        attempt = nil
        requested = nil
    }

    /// `asc_cast_start`'s answer for `id`: its attempt number, 0 when it
    /// refused with `refusal`. A refused device goes off.
    mutating func started(_ id: String, attempt number: UInt64, refusal: Int32) -> CastOutcome {
        if starting == id { starting = nil }
        guard number != 0 else {
            if enabled == id { enabled = nil }
            return .turnedOff(id, problem: CastRefusal.problem(refusal))
        }
        attempt = Attempt(number: number, device: id, status: .connecting)
        return .changed
    }

    /// Takes in one event. One of an attempt that isn't the current one is
    /// dropped (nil), as is one that changes nothing shown.
    mutating func apply(_ event: CastEvent) -> CastOutcome? {
        switch event {
        case .memoryChanged: return .memoryChanged
        case .discoveryStopped:
            discovery = .failed
            devices = devices.filter { $0.key == attempt?.device }
            return .changed
        case .failed:
            discovery = .failed
            devices = [:]
            attempt = nil
            starting = nil
            enabled = nil
            return .senderFailed
        default: break
        }
        guard let current = attempt, event.attempt == current.number else { return nil }
        let id = current.device
        switch event {
        case .started, .playing, .recovered, .reloaded:
            return setStatus(.playing)
        case .recovering:
            return setStatus(.reconnecting)
        case .connectFailed:
            return current.status == .connecting ? nil : setStatus(.reconnecting)
        case .volume(_, let volume):
            if requested?.id == id { requested = nil }
            guard volumes[id] != volume else { return nil }
            volumes[id] = volume
            return .changed
        case .ended(_, let end):
            attempt = nil
            requested = nil
            if enabled == id { enabled = nil }
            return .turnedOff(id, problem: end.problem)
        case .commandFailed, .note, .memoryChanged, .discoveryStopped, .failed:
            return nil
        }
    }

    private mutating func setStatus(_ status: CastStatus) -> CastOutcome? {
        guard let current = attempt, current.status != status else { return nil }
        attempt?.status = status
        return status == .playing ? .playing(current.device) : .changed
    }

    /// The user's level for the device in a session, 0 to 100; nil when none
    /// runs for it or the position isn't a number. Returns the level, 0 to 1.
    mutating func setVolume(_ id: String, position: Double) -> Double? {
        guard isLive(id), position.isFinite else { return nil }
        let level = min(1, max(0, position / 100))
        requested = (id, level)
        return level
    }

    /// The user's offset for `id`, as the field takes it; nil when unchanged
    /// or not a number.
    mutating func setOffset(_ id: String, ms: Double) -> Double? {
        guard let clamped = Self.clampedOffset(ms), offsets[id] != clamped else { return nil }
        offsets[id] = clamped
        return clamped
    }

    /// An offset in whole ms, clamped to 0 to 20 000; nil when it isn't finite.
    static func clampedOffset(_ ms: Double) -> Double? {
        guard ms.isFinite else { return nil }
        return min(maxOffsetMs, max(0, ms.rounded()))
    }

    /// The device that is on counts for local outputs while it is in a
    /// session or listed online.
    var chosen: Bool {
        guard let id = enabled else { return false }
        return attempt?.device == id || starting == id || devices[id].map { !$0.offline } == true
    }

    /// How long local outputs wait for the device that is on: its offset;
    /// nil while none is chosen.
    var anchorMs: Double? {
        guard chosen, let id = enabled else { return nil }
        return offset(of: id)
    }

    /// What local outputs wait for: L while an AirPlay speaker is chosen,
    /// the Cast offset while a Cast speaker is, the longer when both are.
    static func networkWaitMs(airPlay: Double?, cast: Double?) -> Double? {
        switch (airPlay, cast) {
        case let (a?, c?): return max(a, c)
        case let (a, c): return a ?? c
        }
    }

    func listState(now: Date) -> CastListState {
        switch discovery {
        case .off: return .quiet
        case .failed: return devices.isEmpty ? .failed : .quiet
        case .browsing(let since):
            if !devices.isEmpty { return .quiet }
            return now.timeIntervalSince(since) >= Self.noneFoundAfter ? .noneFound : .searching
        }
    }
}

/// The device memory between runs: the core's JSON (per device, the format
/// that last worked and the offset), in AudioSync's Application Support
/// folder. It holds no secrets, so unlike the pairings it stays in backups.
enum CastMemoryFile {
    static let fileName = "cast-devices.json"

    static func file(in applicationSupport: URL) -> URL {
        applicationSupport.appendingPathComponent("AudioSync", isDirectory: true)
            .appendingPathComponent(fileName)
    }

    /// The stored JSON, or nil when there is none or it isn't text.
    static func load(from url: URL) -> String? {
        guard let data = try? Data(contentsOf: url) else { return nil }
        return String(data: data, encoding: .utf8)
    }

    /// Writes through a temporary file, so a crash mid-write leaves the
    /// previous memory whole.
    static func save(_ json: String, to url: URL) throws {
        try FileManager.default.createDirectory(at: url.deletingLastPathComponent(), withIntermediateDirectories: true)
        try Data(json.utf8).write(to: url, options: .atomic)
    }
}
