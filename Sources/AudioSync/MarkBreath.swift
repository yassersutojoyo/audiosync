import Combine
import Foundation

/// The logo in the main window's and the panel's header breathes, slowly,
/// while no network speaker has been found, and only on a surface that is on
/// screen. Its own object, like `SoundTide`, so a window showing or hiding
/// redraws the marks alone.
final class MarkBreath: ObservableObject {
    static let shared = MarkBreath()

    @Published var mainOnScreen = false
    @Published var panelOnScreen = false

    /// Seconds for one breath in and out.
    static let period = 4.5
    /// Seconds the mark takes to come to rest once a speaker shows up.
    static let settle = 0.35

    /// Not while Local Network access is refused: no search runs then, so
    /// the mark would promise one.
    static func breathes(networkSpeakers: Int, localNetworkRefused: Bool, reduceMotion: Bool, onScreen: Bool) -> Bool {
        networkSpeakers == 0 && !localNetworkRefused && !reduceMotion && onScreen
    }

    /// 0 at rest, 1 at the top of a breath; it starts and ends each breath at
    /// rest, so a breath that starts never jumps.
    static func depth(sinceStart seconds: Double) -> Double {
        (1 - cos(2 * .pi * max(0, seconds) / period)) / 2
    }

    static func scale(depth: Double) -> Double { 1 + 0.04 * depth }
    static func opacity(depth: Double) -> Double { 1 - 0.2 * depth }
}

/// When the mark's current breathing began; reset when it stops.
final class BreathStart: ObservableObject {
    var date: Date?

    func since(_ now: Date) -> Date {
        if let date { return date }
        date = now
        return now
    }
}
