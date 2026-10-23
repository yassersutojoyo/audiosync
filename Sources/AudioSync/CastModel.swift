import CAudioSyncCore
import Foundation

// Google Cast speakers, through the core's Cast sender (`asc_cast_*`): what
// discovery lists, the one device the user turned on, its session's attempt
// and status, its volume and its offset. Pure state, so the logic suite
// drives it; `CastSender` talks to the core and `AppDelegate+Cast` wires the
// two together.
//
// One device plays at a time (CAST_SPEC C2); a Cast group is one of them,
// with one volume and one offset under its own ID (C89 to C102). Cast has
// no shared clock, so a device is kept in step only by the offset the user
// sets by ear (C74 to C77): while one is on, local outputs wait that long
// for it. Every event of a session carries the
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
    /// A Cast group, played as one speaker.
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

/// One member of the playing group, as its session reports it (C94, C95).
struct CastMember: Equatable {
    /// The listed device it is, or "" when none matches.
    var id: String
    /// The name the group gives it, or "".
    var name: String
}

/// What a Cast group's row says about its members.
enum CastGroupMembers: Equatable {
    /// Off, or its session hasn't played yet.
    case notReported
    /// It plays and reported no member list (C94).
    case unknown
    case known([String])
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
}

enum CastError: Equatable {
    case connect, launch, media, noFetch, stream, noSession, volumeFixed, volume, artwork
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
    /// The device's own volume step, 0 to 1; 0 while it reports none (C69).
    var step = 0.0
}

/// How the arrow keys move a Cast device's volume.
enum CastVolumeStep: Equatable {
    /// One of the device's own steps, sent as a step (C69).
    case device(up: Bool)
    /// This level, 0 to 1, sent as it is: the device reports no step.
    case level(Double)
}

/// What the sender reported, copied out of the core's event.
enum CastEvent: Equatable {
    /// The device plays the stream in `format`, an `ASC_CAST_FORMAT_` code.
    case started(attempt: UInt64, format: Int32)
    case playing(attempt: UInt64)
    case connectFailed(attempt: UInt64, error: CastError?, retryMs: Int64)
    case recovering(attempt: UInt64)
    case recovered(attempt: UInt64)
    case reloaded(attempt: UInt64)
    case volume(attempt: UInt64, CastVolume)
    case commandFailed(attempt: UInt64, error: CastError?)
    case ended(attempt: UInt64, CastEnd)
    /// The device refused WAV; `format` is loaded instead (C84).
    case formatFallback(attempt: UInt64, format: Int32)
    /// Diagnostics, rebuffering and the stream's own log: for the debug log only.
    case note(attempt: UInt64, String)
    /// The playing group's member list changed; empty while none is known.
    case members(attempt: UInt64, [CastMember])
    /// A line about a Cast group for the debug log.
    case groupLog(String)
    case memoryChanged
    case discoveryStopped
    /// The sender stopped working; `detail` names where it panicked, or "".
    case failed(detail: String)

    var attempt: UInt64? {
        switch self {
        case .started(let a, _), .formatFallback(let a, _), .playing(let a), .connectFailed(let a, _, _),
             .recovering(let a), .recovered(let a), .reloaded(let a), .volume(let a, _), .commandFailed(let a, _),
             .ended(let a, _), .note(let a, _), .members(let a, _):
            return a
        case .groupLog, .memoryChanged, .discoveryStopped, .failed:
            return nil
        }
    }

    /// Copies `raw` right away: its strings last only until the next
    /// `asc_cast_next_event`; `members` reads the playing group's list for a
    /// members event. Nil for a kind this build doesn't know.
    init?(_ raw: AscCastEvent, members: () -> [CastMember] = { [] }) {
        let a = raw.attempt
        let detail = raw.detail.map { String(cString: $0) } ?? ""
        let error = CastError(raw.error)
        switch raw.kind {
        case ASC_CAST_EVENT_STARTED: self = .started(attempt: a, format: raw.format)
        case ASC_CAST_EVENT_PLAYING: self = .playing(attempt: a)
        case ASC_CAST_EVENT_CONNECT_FAILED: self = .connectFailed(attempt: a, error: error, retryMs: raw.retry_ms)
        case ASC_CAST_EVENT_RECOVERING: self = .recovering(attempt: a)
        case ASC_CAST_EVENT_RECOVERED: self = .recovered(attempt: a)
        case ASC_CAST_EVENT_RELOADED: self = .reloaded(attempt: a)
        case ASC_CAST_EVENT_VOLUME:
            self = .volume(attempt: a, CastVolume(level: raw.level, muted: raw.muted, fixed: raw.volume_fixed,
                                                  step: raw.step))
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
        case ASC_CAST_EVENT_FORMAT_FALLBACK: self = .formatFallback(attempt: a, format: raw.format)
        case ASC_CAST_EVENT_DIAGNOSTICS:
            self = .note(attempt: a, String(format: "drift %.0f ms, queued %.0f ms", raw.drift_ms, raw.queued_ms))
        case ASC_CAST_EVENT_STREAM: self = .note(attempt: a, "stream: " + detail)
        case ASC_CAST_EVENT_MEMBERS: self = .members(attempt: a, members())
        case ASC_CAST_EVENT_GROUP_LOG: self = .groupLog(detail)
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
    /// A Cast group's members; nil for a single device.
    var group: CastGroupMembers?
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
    /// The sender stopped working; `wasOn`: the speaker that was on, now off.
    case senderFailed(wasOn: String?)
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
        /// The `ASC_CAST_FORMAT_` code it plays in; 0 until the session reports it.
        var format: Int32 = 0
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
    /// The offset the device memory holds, per device, in ms. 0 ms counts as
    /// none: the sender can't forget an offset, so Undo leaves 0 ms behind.
    private(set) var offsets: [String: Double] = [:]
    /// The playing group's members as its session last reported them; nil
    /// until it does, and once the session is over.
    private(set) var members: [CastMember]?

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
            if let offset { offsets[device.id] = offset > 0 ? offset : nil }
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

    /// The device that is on while its session plays, a group included: the
    /// one Cast row a calibration run measures.
    var playingDevice: String? {
        guard let id = enabled, status(of: id) == .playing else { return nil }
        return id
    }

    /// The `ASC_CAST_FORMAT_` code the session plays in; 0 while none reported one.
    var playingFormat: Int32 { attempt?.format ?? 0 }

    /// The user turns `id` on, a group like any device: refused while
    /// another one is on, a member of the group that is on included (C2, C99).
    mutating func turnOn(_ id: String) -> CastProblem? {
        if let other = enabled, other != id { return .busy }
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
              let device = devices[id], !device.offline else { return .none }
        return .start(id)
    }

    mutating func startSent(_ id: String) { starting = id }

    mutating func stopSent() {
        attempt = nil
        requested = nil
        members = nil
    }

    /// What a group's row says about its members, each named by `name`; nil
    /// for a single device. A list arrives before the stream plays (C94), so
    /// one that is still missing once it plays is unknown.
    func groupMembers(of id: String, name: (CastMember) -> String) -> CastGroupMembers? {
        guard devices[id]?.group == true else { return nil }
        guard attempt?.device == id else { return .notReported }
        let names = (members ?? []).map(name).filter { !$0.isEmpty }
        if !names.isEmpty { return .known(names) }
        return attempt?.status == .playing ? .unknown : .notReported
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
        case .groupLog: return nil
        case .discoveryStopped:
            discovery = .failed
            devices = devices.filter { $0.key == attempt?.device }
            return .changed
        case .failed:
            let wasOn = enabled
            discovery = .failed
            devices = [:]
            attempt = nil
            starting = nil
            enabled = nil
            members = nil
            return .senderFailed(wasOn: wasOn)
        default: break
        }
        guard let current = attempt, event.attempt == current.number else { return nil }
        let id = current.device
        switch event {
        case .started(_, let format):
            attempt?.format = format
            return setStatus(.playing)
        case .formatFallback(_, let format):
            attempt?.format = format
            return nil
        case .playing, .recovered, .reloaded:
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
        case .members(_, let list):
            guard devices[id]?.group == true, members != list else { return nil }
            members = list
            return .changed
        case .ended(_, let end):
            attempt = nil
            requested = nil
            members = nil
            if enabled == id { enabled = nil }
            return .turnedOff(id, problem: end.problem)
        case .commandFailed, .note, .groupLog, .memoryChanged, .discoveryStopped, .failed:
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

    /// The step a device that reports none moves by, as a level.
    static let fallbackVolumeStep = 0.05

    /// An arrow key on the device's slider: one of its own steps once its
    /// status gave one, else the fallback step. Either way the slider, and
    /// what VoiceOver reads back, shows the level the step should reach from
    /// the level shown until the device's status answers. Nil when no session
    /// runs for it or its volume is fixed.
    mutating func stepVolume(_ id: String, up: Bool) -> CastVolumeStep? {
        guard isLive(id), volumes[id]?.fixed != true else { return nil }
        let deviceStep = volumes[id]?.step ?? 0
        let size = deviceStep > 0 ? deviceStep : Self.fallbackVolumeStep
        let level = min(1, max(0, volume(of: id) / 100 + (up ? size : -size)))
        requested = (id, level)
        return deviceStep > 0 ? .device(up: up) : .level(level)
    }

    /// The user's offset for `id`, as the field takes it; nil when unchanged
    /// or not a number.
    mutating func setOffset(_ id: String, ms: Double) -> Double? {
        guard let clamped = Self.clampedOffset(ms), offset(of: id) != clamped else { return nil }
        offsets[id] = clamped > 0 ? clamped : nil
        return clamped
    }

    /// Forgets the offset of `id`, so its next calibration searches the
    /// longest window again; false when none was stored.
    mutating func clearOffset(_ id: String) -> Bool {
        offsets.removeValue(forKey: id) != nil
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

/// With an AirPlay speaker and a Cast device both chosen, the one with the
/// shorter latency waits for the other (C77). AirPlay speakers wait for a
/// longer Cast offset as far as the sender's delay limit reaches, and play
/// ahead of the device by the rest (P99); the Cast feed waits for a longer L.
enum NetworkHoldBack {
    /// The AirPlay sender's largest delay per speaker, in ms.
    static let airPlayLimitMs = Double(ASC_AIRPLAY_MAX_DELAY_MS)

    /// How much later than L the AirPlay speakers should play: the Cast
    /// offset's lead over L, else 0. `airPlay` is L, nil while no AirPlay
    /// speaker is chosen; `cast` the offset, nil while no device is.
    static func airPlayMs(airPlay: Double?, cast: Double?) -> Double {
        guard let airPlay, let cast, airPlay.isFinite, cast.isFinite else { return 0 }
        return max(0, cast - airPlay)
    }

    /// How long the Cast feed waits: L's lead over the Cast offset, else 0.
    static func castMs(airPlay: Double?, cast: Double?) -> Double {
        guard let airPlay, let cast, airPlay.isFinite, cast.isFinite else { return 0 }
        return max(0, airPlay - cast)
    }

    /// The part of the AirPlay hold-back `wantedMs` the sender plays.
    static func airPlayAppliedMs(_ wantedMs: Double) -> Double { min(max(0, wantedMs), airPlayLimitMs) }

    /// How far ahead of the Cast device the AirPlay speakers play.
    static func airPlayAheadMs(_ wantedMs: Double) -> Double { max(0, wantedMs - airPlayLimitMs) }
}

/// The notice that the AirPlay speakers play ahead of the Cast device that
/// is on: posted while the delay limit leaves them 50 ms or more ahead,
/// cleared once they keep up. A dismissed one comes back only when the
/// device or the amount, in tenths of a second, changes.
struct AirPlayAheadNotice: Equatable {
    /// Less than this ahead is within the sync tolerance.
    static let thresholdMs = 50.0

    struct Shown: Equatable {
        var device: String
        var tenths: Int
    }

    /// What to do with the notices: drop the one about `clear`, post `post`.
    struct Step: Equatable {
        var clear: String?
        var post: Shown?
    }

    private(set) var shown: Shown?

    /// `device`: the Cast device that is on, if any. Nil when nothing changes.
    mutating func update(device: String?, aheadMs: Double) -> Step? {
        var now: Shown?
        if let device, aheadMs.isFinite, aheadMs >= Self.thresholdMs {
            now = Shown(device: device, tenths: max(1, Int((aheadMs / 100).rounded())))
        }
        guard now != shown else { return nil }
        let step = Step(clear: shown?.device, post: now)
        shown = now
        return step
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
