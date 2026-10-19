import Foundation

/// Which AirPlay sender the app uses: the bundled engine (OwnTone), the
/// default, or AudioSync's own sender (`airplay/`, reached through the Rust
/// core's C interface, `NativeAirPlay`). The move happens in stages; in this
/// second one, with `native` chosen AudioSync doesn't start the bundled
/// engine: the AirPlay speakers listed are the ones the native sender's
/// discovery finds, and AirPlay audio goes through that sender. PIN pairing
/// and stored credentials come later, so a receiver that asks for a PIN or a
/// password is listed but stays off. The bundled engine starts with the app,
/// so the choice is read once per launch (`forThisRun`), and a change applies
/// from the next launch.
enum AirPlayImpl: String, CaseIterable {
    case owntone, native

    /// The sender this run of AudioSync uses, read once, when the app starts
    /// the bundled engine (or doesn't) at launch.
    static let forThisRun: (impl: AirPlayImpl, origin: Origin) = current()

    /// UserDefaults key of the setting, "owntone" or "native". Only an
    /// explicit choice writes it (the hidden debug option in the AudioSync
    /// menu, or `defaults write`); an absent key means the user never chose.
    static let defaultsKey = "airplayImpl"
    /// Environment override, "owntone" or "native", for tests and harnesses.
    static let environmentKey = "AUDIOSYNC_AIRPLAY"

    /// Where a choice came from, for the debug log and the debug option.
    enum Origin: String {
        case environment, setting, `default`
    }

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

    /// Whether a session feeds the bundled engine's PCM pipe
    /// (`AirPlayEngine.fifoPath`): only the bundled engine reads it.
    /// AudioSync's own sender takes the audio from the capture path
    /// (`NativeAirPlayTap`), so with it no pipe feed runs, none keeps trying
    /// to open the pipe, and a leftover engine process gets no audio.
    var feedsPipe: Bool { self == .owntone }

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

    /// How the speaker list shows it, if at all: one that can play is open;
    /// one that asks for a PIN needs a code (it stays off until pairing comes
    /// to this sender); one that asks for a password has one AudioSync can't
    /// send yet. A receiver that needs another kind of pairing, or that only
    /// AirPlay 2 or nothing here can play, isn't listed.
    var access: AirPlayPairing.Access? {
        switch availability {
        case .available: return .open
        case .needsPin: return .code(password: false)
        case .needsPassword: return .legacyPassword
        case .needsPairing, .airPlay2Only, .unsupported: return nil
        }
    }

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
