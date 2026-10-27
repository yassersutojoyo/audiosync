import Foundation
import Network
import dnssd

/// Whether macOS lets AudioSync use the local network, as far as
/// `LocalNetworkProbe` can tell.
enum LocalNetworkAccess: Equatable {
    case unknown, allowed, denied
}

/// The Local Network hint under the AirPlay speakers.
enum LocalNetworkHint: Equatable {
    case none
    /// Access is denied: the sender can't find any speaker.
    case denied
    /// Nothing found for a while and access is unknown: maybe it's denied.
    case possible

    /// How long a running sender may find no AirPlay speaker at all before
    /// the UI suggests checking Local Network access, when `LocalNetworkProbe`
    /// can't tell whether it is allowed. Discovery normally takes a few
    /// seconds; without that permission it never finds any.
    static let emptyBeforeHint: TimeInterval = 20

    /// Which hint to show under the AirPlay speakers.
    /// - Parameters:
    ///   - access: what `LocalNetworkProbe` found out.
    ///   - senderUp: the AirPlay sender and its discovery run.
    ///   - emptySince: since when discovery has listed no speakers (nil: it
    ///     found some, or isn't running).
    /// Denied access is certain, so it shows at once; allowed access rules the
    /// hint out, however long the list stays empty (the network may simply
    /// have no AirPlay speaker). Only when the probe can't tell does a list
    /// that stays empty for `emptyBeforeHint` suggest a check.
    static func decide(access: LocalNetworkAccess, senderUp: Bool, emptySince: Date?, now: Date) -> LocalNetworkHint {
        guard senderUp else { return .none }
        switch access {
        case .denied: return .denied
        case .allowed: return .none
        case .unknown:
            guard let emptySince, now.timeIntervalSince(emptySince) >= emptyBeforeHint else { return .none }
            return .possible
        }
    }

    /// The hint as the views show it: an AirPlay problem takes its place.
    func shown(airPlayProblem: Bool) -> LocalNetworkHint { airPlayProblem ? .none : self }

    /// Whether the speaker notes may name Local Network access in their
    /// network advice (`UIStrings.airPlayNoneFound`), for the hint before an
    /// AirPlay problem hides it. Only `.denied` is certain: `.possible` comes
    /// from an empty list alone, maybe before macOS ever asked.
    var accessRefused: Bool { self == .denied }

    /// When the soft hint would show if nothing changes, while that is still
    /// ahead of `now`: no event says the time has passed, so the host
    /// reconciles then (`AppLifecycle.nextDueReconcile`), and `decide` shows
    /// the hint. Nil when it is shown already or can't come.
    static func dueAt(access: LocalNetworkAccess, senderUp: Bool, emptySince: Date?, now: Date) -> Date? {
        guard senderUp, access == .unknown, let emptySince else { return nil }
        let due = emptySince.addingTimeInterval(emptyBeforeHint)
        return due > now ? due : nil
    }
}

/// AirPlay discovery's first look: while a running sender has listed no
/// speaker for less than `LocalNetworkHint.emptyBeforeHint`, Connected
/// Speakers says it is looking instead of giving network advice, as the
/// Windows app does for its first 20 seconds.
enum AirPlaySearch {
    static func isSearching(access: LocalNetworkAccess, senderUp: Bool, emptySince: Date?, now: Date) -> Bool {
        guard senderUp, access != .denied, let emptySince else { return false }
        return now.timeIntervalSince(emptySince) < LocalNetworkHint.emptyBeforeHint
    }

    /// When the first look ends if nothing changes, while that is ahead of
    /// `now`: no event says so, so the host reconciles then.
    static func endsAt(access: LocalNetworkAccess, senderUp: Bool, emptySince: Date?, now: Date) -> Date? {
        guard isSearching(access: access, senderUp: senderUp, emptySince: emptySince, now: now),
              let emptySince else { return nil }
        return emptySince.addingTimeInterval(LocalNetworkHint.emptyBeforeHint)
    }
}

/// Finds out whether macOS lets AudioSync use the local network.
///
/// There is no API that reports Local Network permission, but Bonjour
/// browsing in AudioSync's own process is subject to it: with access denied,
/// an `NWBrowser` waits with the DNS-SD error `kDNSServiceErr_PolicyDenied`.
/// AudioSync's AirPlay sender runs in the same process, so the same answer
/// applies to its discovery. The probe browses for `_airplay._tcp` (declared
/// in `NSBonjourServices`) and ignores the results.
///
/// Run it only while the AirPlay sender runs: browsing can raise the system's
/// Local Network prompt, which belongs to the moment AirPlay starts. Main
/// thread only; the browser reports on the main queue.
final class LocalNetworkProbe {
    private(set) var access: LocalNetworkAccess = .unknown
    /// Called on the main queue when `access` changes.
    var onChange: (LocalNetworkAccess) -> Void = { _ in }
    private var browser: NWBrowser?
    private var startedAt = Date.distantPast
    /// How often a denied answer is asked again: macOS may answer "denied"
    /// while its prompt is still up, and allowing it there doesn't activate
    /// AudioSync, so nothing else would ask again.
    static let deniedRecheck: TimeInterval = 10

    var isRunning: Bool { browser != nil }

    /// Starts browsing, or starts over (after the user may have changed the
    /// setting). The last answer stands until the new browser gives one.
    func startBrowsing() {
        browser?.cancel()
        let browser = NWBrowser(for: .bonjour(type: "_airplay._tcp", domain: nil), using: NWParameters())
        browser.stateUpdateHandler = { [weak self, weak browser] state in
            guard let self, let browser, browser === self.browser else { return }
            if let access = Self.access(for: state) { self.update(access) }
        }
        self.browser = browser
        startedAt = Date()
        browser.start(queue: .main)
    }

    /// Starts over if the answer is "denied" and was last asked a while ago.
    func recheckIfDenied(now: Date = Date()) {
        guard let due = recheckAt, now >= due else { return }
        startBrowsing()
    }

    /// When `recheckIfDenied` asks again; the host reconciles then
    /// (`AppLifecycle.nextDueReconcile`).
    var recheckAt: Date? { Self.recheckDate(running: isRunning, access: access, startedAt: startedAt) }

    /// While a running browser's answer is "denied": `deniedRecheck` after
    /// it last started. Nil otherwise: nothing to ask again.
    static func recheckDate(running: Bool, access: LocalNetworkAccess, startedAt: Date) -> Date? {
        guard running, access == .denied else { return nil }
        return startedAt.addingTimeInterval(deniedRecheck)
    }

    /// Stops browsing and forgets the answer: it is asked again on the next start.
    func stopBrowsing() {
        browser?.cancel()
        browser = nil
        update(.unknown)
    }

    private func update(_ access: LocalNetworkAccess) {
        guard access != self.access else { return }
        self.access = access
        onChange(access)
    }

    /// What a browser state says about Local Network access; nil: nothing new.
    /// A browser that is ready may browse, which a denied app can't; any other
    /// wait or failure (no network, a Bonjour error) leaves the answer open.
    static func access(for state: NWBrowser.State) -> LocalNetworkAccess? {
        switch state {
        case .ready:
            return .allowed
        case .waiting(let error), .failed(let error):
            if case .dns(let code) = error, code == DNSServiceErrorType(kDNSServiceErr_PolicyDenied) {
                return .denied
            }
            return .unknown
        case .setup, .cancelled:
            return nil
        @unknown default:
            return nil
        }
    }
}
