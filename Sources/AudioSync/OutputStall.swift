import Foundation

/// Tells a local output whose render callback stopped while its device stays
/// alive (S334): per output, the frames its callback handed the device,
/// silence included, against the uptime clock, which stands still while the
/// Mac sleeps. Main queue only.
struct OutputStallWatch {
    /// How long an output that took frames once may take none.
    static let stallAfterS = 2.0
    /// How often the host reads the counts.
    static let checkEveryS = 0.5

    /// One output's count at a check; `attach` tells a re-attached output,
    /// whose new renderer counts from 0 (`AttachTokens`).
    struct Reading: Equatable {
        var attach: UInt64
        var frames: Int64
    }

    struct Stall: Equatable {
        var uid: String
        /// Uptime seconds since its count last moved.
        var quietS: Double
    }

    private struct Seen {
        var attach: UInt64
        var frames: Int64
        var movedAtS: Double
        var tookFrames: Bool
    }

    private var seen: [String: Seen] = [:]

    /// The outputs in `readings` that stalled by uptime `nowS`, sorted by UID.
    /// A stalled output is forgotten, so it stalls once per attach.
    mutating func check(_ readings: [String: Reading], nowS: Double) -> [Stall] {
        seen = seen.filter { readings[$0.key] != nil }
        var stalls: [Stall] = []
        for (uid, reading) in readings {
            guard let was = seen[uid], was.attach == reading.attach else {
                seen[uid] = Seen(attach: reading.attach, frames: reading.frames, movedAtS: nowS,
                                 tookFrames: reading.frames > 0)
                continue
            }
            if reading.frames != was.frames {
                seen[uid] = Seen(attach: reading.attach, frames: reading.frames, movedAtS: nowS, tookFrames: true)
            } else if was.tookFrames, nowS - was.movedAtS >= Self.stallAfterS {
                stalls.append(Stall(uid: uid, quietS: nowS - was.movedAtS))
                seen[uid] = nil
            }
        }
        return stalls.sorted { $0.uid < $1.uid }
    }

    mutating func reset() {
        seen = [:]
    }
}

/// What the host read on the main queue about a stalled output's device.
enum OutputStallSign: Equatable {
    case hogMode(pid: Int32)
    case deviceNotRunning
    case none

    /// `hogOwner` is the process holding the device in hog mode, nil when
    /// none does or it can't be read; `running` is nil when unreadable.
    static func read(hogOwner: Int32?, ownPID: Int32, running: Bool?) -> OutputStallSign {
        if let owner = hogOwner, owner > 0, owner != ownPID { return .hogMode(pid: owner) }
        if running == false { return .deviceNotRunning }
        return .none
    }

    /// The debug line for a stall of output `number` (its status-line number, never its device's name).
    static func line(number: Int, quietS: Double, sign: OutputStallSign) -> String {
        let what: String
        switch sign {
        case .hogMode(let pid): what = "another process (PID \(pid)) holds its device in hog mode"
        case .deviceNotRunning: what = "its device is no longer running"
        case .none: what = "no sign"
        }
        return "output \(number) stalled: no frame for " + String(format: "%.1f", quietS) + " s, \(what)"
    }
}

/// Why the engine dropped a local output whose speaker stays on.
enum OutputLoss: Equatable {
    /// The device stopped being alive (S68).
    case deviceGone
    /// Its render callback took no frame for `quietS` seconds (S334).
    case stalled(quietS: Double, sign: OutputStallSign)
    /// Its device's rate or channel count changed under it (S330).
    case formatChanged(old: OutputDeviceFormat, new: OutputDeviceFormat)

    var reason: String {
        switch self {
        case .deviceGone: return "device lost"
        case .stalled: return "stalled"
        case .formatChanged: return "format changed"
        }
    }
}

/// What follows a stall (S334, S326): a first stall reopens the output at
/// once. A stall before the reopened stream has played 5 s, or a start the
/// device refuses after a stall, is a stall in a row: the output stays on
/// and is tried again every 10 s until a stream plays 5 s. Main queue only.
struct OutputStallRetry {
    /// How long a reopened stream plays before its stall is over.
    static let trialS = 5.0
    static let retryEveryS = 10.0

    enum Step: Equatable {
        case reopenNow
        /// A stall in a row: the next try at uptime `atS`.
        case retry(atS: Double)
    }

    private struct Entry {
        var startedAtS: Double?
        var retryAtS: Double?
    }

    private var entries: [String: Entry] = [:]

    /// The outputs whose stall isn't over yet.
    var tracked: Set<String> { Set(entries.keys) }

    /// The outputs a sync starts again after a stall; `formatReopens` are
    /// starting again because their device's format changed (S330).
    func afterStall(formatReopens: Set<String>) -> Set<String> {
        tracked
    }

    mutating func stalled(_ uid: String, nowS: Double) -> Step {
        if let started = entries[uid]?.startedAtS, nowS - started < Self.trialS {
            let at = nowS + Self.retryEveryS
            entries[uid] = Entry(startedAtS: nil, retryAtS: at)
            return .retry(atS: at)
        }
        entries[uid] = Entry(startedAtS: nil, retryAtS: nil)
        return .reopenNow
    }

    /// Output `uid` started again; nothing for one that didn't stall.
    mutating func started(_ uid: String, nowS: Double) {
        guard entries[uid] != nil else { return }
        entries[uid] = Entry(startedAtS: nowS, retryAtS: nil)
    }

    /// The device refused the start of `uid`: after a stall that is a stall
    /// in a row, tried again at the uptime returned. nil for an output that
    /// didn't stall, which is switched off as before (S69).
    mutating func refused(_ uid: String, nowS: Double) -> Double? {
        guard entries[uid] != nil else { return nil }
        let at = nowS + Self.retryEveryS
        entries[uid] = Entry(startedAtS: nil, retryAtS: at)
        return at
    }

    /// The outputs waiting for their next try, which no sync starts yet.
    func held(nowS: Double) -> Set<String> {
        Set(entries.filter { ($0.value.retryAtS ?? -.infinity) > nowS }.keys)
    }

    /// Forgets the outputs whose stream has played 5 s and those switched off.
    mutating func settle(enabled: Set<String>, nowS: Double) {
        entries = entries.filter { uid, entry in
            guard enabled.contains(uid) else { return false }
            guard let started = entry.startedAtS else { return true }
            return nowS - started < Self.trialS
        }
    }

    mutating func reset() {
        entries = [:]
    }
}
