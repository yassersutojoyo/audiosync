import CAudioSyncCore
import Foundation

/// The standby hint of a Denon or Marantz receiver asleep off the network
/// (P262 to P264): one core state per receiver ID, whose two marks the app
/// stores with its settings. The rules are in the core.
struct AirPlayStandby {
    private var states: [String: AscStandby] = [:]

    /// Starts from the stored marks.
    init(recognized: Set<String> = [], hintShown: Set<String> = []) {
        for id in recognized.union(hintShown) {
            states[id] = asc_standby_new(recognized.contains(id), hintShown.contains(id))
        }
    }

    /// The receivers recognized as a Denon or Marantz receiver, to store.
    var recognized: Set<String> { Set(states.filter { $0.value.recognized_mark }.keys) }
    /// The receivers whose hint showed since they last played, to store.
    var hintShown: Set<String> { Set(states.filter { $0.value.hint_shown_mark }.keys) }

    /// The receivers the rule already holds a state for.
    var known: Set<String> { Set(states.keys) }

    /// Whether the hint shows on the row of receiver `id`.
    func hint(_ id: String) -> Bool { states[id]?.hint ?? false }

    /// Takes a speaker event's `standby` outcome at `now`, in seconds on a
    /// monotonic clock.
    mutating func outcome(_ id: String, _ code: Int32, now: Double) {
        guard code != ASC_STANDBY_OUTCOME_NONE else { return }
        var state = states[id] ?? asc_standby_new(false, false)
        asc_standby_outcome(&state, code, now)
        states[id] = state
    }

    /// Brings every receiver up to date at `now`: `records` holds each
    /// listed receiver's `denon` code, `wanted` whether the app wants it
    /// (on while streaming runs). Returns those whose hint shows now.
    mutating func update(records: [String: Int32], wanted: (String) -> Bool,
                         now: Double) -> [(id: String, shown: AscStandbyShown)] {
        var shown: [(id: String, shown: AscStandbyShown)] = []
        for id in Set(states.keys).union(records.keys).sorted() {
            var state = states[id] ?? asc_standby_new(false, false)
            var why = AscStandbyShown()
            if asc_standby_update(&state, records[id] ?? ASC_DENON_NOT_IN_VIEW, wanted(id), now, &why) {
                shown.append((id: id, shown: why))
            }
            states[id] = state
        }
        return shown
    }
}
