import Foundation

// A local output's own macOS volume, remembered per device UID: the level it
// last had while it played through AudioSync is set again before its first
// audio, and the level it was found at is put back after its last. Only
// levels the user chose are ever written; the host does the Core Audio part.

struct LocalVolumeMemory: Equatable {
    /// The found level goes back only after the output's fade-out, which
    /// ends within 500 ms of the switch-off (S186).
    static let putBackAfterS = 0.6

    /// The level each output last had while it played through AudioSync, 0 to 1.
    private(set) var remembered: [String: Double]
    /// The level each output had when it started playing, to put back when it
    /// stops; stored, so one a crash left behind goes back at the next launch.
    private(set) var found: [String: Double]
    /// The outputs playing through AudioSync now.
    private(set) var playing: Set<String> = []

    init(remembered: [String: Double] = [:], found: [String: Double] = [:]) {
        self.remembered = remembered
        self.found = found
    }

    /// Output `uid` starts playing; `current` is its device's level now, nil
    /// when it has no readable, settable volume. Returns the level to set
    /// before its first audio, nil to leave it as found.
    mutating func switchedOn(_ uid: String, current: Double?) -> Double? {
        guard !playing.contains(uid) else { return nil }
        playing.insert(uid)
        guard let current else { return nil }
        // One a crash left set keeps the level it had before AudioSync.
        if found[uid] == nil { found[uid] = current }
        guard let level = remembered[uid] else {
            remembered[uid] = current
            return nil
        }
        return abs(level - current) < 0.001 ? nil : level
    }

    /// The device refused the level `switchedOn` returned: it keeps its own,
    /// so there is nothing to put back.
    mutating func refused(_ uid: String) {
        found[uid] = nil
    }

    /// The device reports `level`, after a change in macOS or on the slider:
    /// while the output plays, that is the level to remember.
    mutating func reported(_ uid: String, level: Double) {
        guard playing.contains(uid), found[uid] != nil else { return }
        remembered[uid] = level
    }

    /// The user set `level` on the output's slider: remembered whether or not it plays.
    mutating func chosen(_ uid: String, level: Double) {
        remembered[uid] = level
    }

    /// Output `uid` stopped playing (switched off, streaming stopped or
    /// AudioSync quits): returns the level to put back, nil to leave it.
    mutating func switchedOff(_ uid: String) -> Double? {
        guard playing.remove(uid) != nil else { return nil }
        return found.removeValue(forKey: uid)
    }

    /// At launch, before anything plays: the found levels a crash left on the
    /// outputs listed now, to put back. An unlisted one waits for its turn.
    mutating func recover(listed: Set<String>) -> [String: Double] {
        var back: [String: Double] = [:]
        for (uid, level) in found where listed.contains(uid) && !playing.contains(uid) {
            back[uid] = level
            found[uid] = nil
        }
        return back
    }
}
