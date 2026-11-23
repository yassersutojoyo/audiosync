// Apps listening to a microphone while the whole system streams (S485).

import CAudioSyncCore
import CoreAudio
import Darwin
import Foundation

/// The core's rule over which apps a whole-system tap leaves out because
/// they listen to a microphone (`asc_call_listeners_*`). Apps are keyed by
/// the PID of the app macOS holds responsible. Main thread only.
final class CallListenerWatch {
    /// How often the capture asks Core Audio which processes read an input.
    static let pollInterval: TimeInterval = 0.5

    private let rule: OpaquePointer?

    init() { rule = asc_call_listeners_create() }

    deinit { asc_call_listeners_destroy(rule) }

    /// One poll at `now` seconds; true when the apps left out changed.
    @discardableResult
    func update(wholeSystem: Bool, listening: [String], now: TimeInterval) -> Bool {
        let copies = listening.map { strdup($0) }
        defer { copies.forEach { free($0) } }
        let pointers = copies.map { $0.map { UnsafePointer($0) } }
        return pointers.withUnsafeBufferPointer {
            asc_call_listeners_update(rule, wholeSystem, $0.baseAddress, $0.count, now)
        }
    }

    /// The apps left out now.
    var leftOut: [String] {
        (0..<asc_call_listeners_left_out_count(rule)).compactMap { index in
            asc_call_listeners_left_out(rule, index).map { String(cString: $0) }
        }
    }

    /// What the whole-system tap excludes: AudioSync's own process and every
    /// process of each app left out, in a stable order so a poll can tell
    /// whether the tap needs changing.
    static func excluded(own: AudioObjectID, leftOut: [String],
                         processesOf: (pid_t) -> [AudioObjectID]) -> [AudioObjectID] {
        Set([own] + leftOut.compactMap { pid_t($0) }.flatMap(processesOf)).sorted()
    }

    private struct NotMoved: Error {}

    /// Moves the whole-system tap to exclude `excluded`.
    func moveTap<Tap>(to excluded: [AudioObjectID], changeInPlace: () -> Bool, log: (String) -> Void,
                      makeTap: () throws -> Tap, rateFits: (Tap) -> Bool, discard: (Tap) -> Void,
                      closeOld: () -> Void, startReading: (Tap) throws -> Void) throws -> AppRetap.Outcome {
        if changeInPlace() { return .updated }
        log("[AudioSync] capture: the tap can't be changed in place")
        throw NotMoved()
    }
}
