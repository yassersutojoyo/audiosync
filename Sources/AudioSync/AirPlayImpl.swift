import Foundation

/// Which AirPlay sender the app uses: the bundled engine (OwnTone), the
/// default, or AudioSync's own sender (`airplay/`, reached through the Rust
/// core's C interface, `NativeAirPlay`). The move happens in stages; in this
/// first one, a session with `native` chosen also starts the native sender's
/// discovery and writes the receivers it finds to the debug log, while the
/// bundled engine still does all the streaming. The choice is read once per
/// session start, so a change applies the next time streaming starts.
enum AirPlayImpl: String, CaseIterable {
    case owntone, native

    /// UserDefaults key of the setting, "owntone" or "native". Only an
    /// explicit choice writes it (the hidden debug option in the AudioSync
    /// menu, or `defaults write`); an absent key means the user never chose.
    static let defaultsKey = "airplayImpl"
    /// Environment override, "owntone" or "native", for tests and harnesses.
    static let environmentKey = "AUDIOSYNC_AIRPLAY"

    /// Where a choice came from, for the debug log and the debug option: the
    /// same three origins as the engine option's.
    typealias Origin = EngineImpl.Origin

    /// The sender for someone who never chose one: the bundled engine, until
    /// the native sender streams.
    static let defaultImpl: AirPlayImpl = .owntone

    /// The environment override wins over the stored setting. A value that
    /// names neither sender counts as absent, so an unknown or missing setting
    /// means `defaultImpl`.
    static func resolve(environment: String?, stored: String?) -> (impl: AirPlayImpl, origin: Origin) {
        if let value = environment, let impl = AirPlayImpl(rawValue: value) { return (impl, .environment) }
        if let value = stored, let impl = AirPlayImpl(rawValue: value) { return (impl, .setting) }
        return (defaultImpl, .default)
    }

    /// The choice for a session starting now (main thread).
    static func current(defaults: UserDefaults = .standard,
                        environment: [String: String] = ProcessInfo.processInfo.environment)
        -> (impl: AirPlayImpl, origin: Origin) {
        resolve(environment: environment[environmentKey], stored: defaults.string(forKey: defaultsKey))
    }
}

/// A receiver as the native sender's discovery reports it. Nothing secret:
/// no password, PIN, key or credential ever crosses the C interface.
struct NativeReceiver: Equatable {
    /// Whether it can play from the native sender, and if not, what it needs.
    enum Availability: String {
        case available = "can play"
        case needsPin = "needs pairing with a PIN"
        case needsPassword = "asks for a password"
        case needsPairing = "needs pairing"
        case airPlay2Only = "AirPlay 2 only"
        case unsupported = "not supported"
    }

    /// The 12-hex-digit device ID, or "raop:<instance>" / "airplay:<instance>"
    /// for a service without one (the Windows app's IDs too).
    let id: String
    let name: String
    /// "" when it announces none.
    let model: String
    let availability: Availability
    /// Why it can't play, when the sender knows ("encryption", "codec", …).
    let reason: String?
    /// Discovery's hints in a fixed order ("raop", "airplay", "apple-tv", …).
    let hints: [String]

    /// The receiver for the debug log, on one line.
    var logDescription: String {
        var parts = ["\(name) (\(id))"]
        if !model.isEmpty { parts.append(model) }
        parts.append(availability.rawValue + (reason.map { " (\($0))" } ?? ""))
        if !hints.isEmpty { parts.append("hints: " + hints.joined(separator: " ")) }
        return parts.joined(separator: ", ")
    }
}

/// One change the native sender's discovery saw.
enum NativeReceiverChange: Equatable {
    case appeared(NativeReceiver)
    case changed(NativeReceiver)
    /// `name` is the one it was listed under, when it was listed.
    case gone(id: String, name: String?)

    /// The change as a debug log line.
    var logLine: String {
        switch self {
        case .appeared(let receiver):
            return "[AudioSync] native AirPlay: found \(receiver.logDescription)"
        case .changed(let receiver):
            return "[AudioSync] native AirPlay: changed \(receiver.logDescription)"
        case .gone(let id, let name):
            return "[AudioSync] native AirPlay: gone \(name.map { "\($0) (\(id))" } ?? id)"
        }
    }
}
