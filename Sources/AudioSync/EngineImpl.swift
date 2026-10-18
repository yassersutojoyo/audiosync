import Foundation

/// Which implementation runs the audio engine's DSP and timing: the Swift
/// modules (the default) or the Rust core in `core/` (through CAudioSyncCore).
/// Either way Swift keeps capture (the process tap or input device) and the
/// Core Audio output units. The choice is read once per session start, so a
/// change applies the next time streaming starts.
enum EngineImpl: String, CaseIterable {
    case swift, rust

    /// UserDefaults key of the setting, "swift" or "rust" (the hidden debug
    /// option in the AudioSync menu writes it).
    static let defaultsKey = "engineImpl"
    /// Environment override, "swift" or "rust", for tests and harnesses.
    static let environmentKey = "AUDIOSYNC_ENGINE"

    /// Where a choice came from, for the debug log and the debug option.
    enum Origin: String {
        case environment, setting, `default`
    }

    /// The environment override wins over the stored setting. A value that
    /// names neither implementation counts as absent, so an unknown or missing
    /// setting means `swift`.
    static func resolve(environment: String?, stored: String?) -> (impl: EngineImpl, origin: Origin) {
        if let value = environment, let impl = EngineImpl(rawValue: value) { return (impl, .environment) }
        if let value = stored, let impl = EngineImpl(rawValue: value) { return (impl, .setting) }
        return (.swift, .default)
    }

    /// The choice for a session starting now (control thread).
    static func current(defaults: UserDefaults = .standard,
                        environment: [String: String] = ProcessInfo.processInfo.environment)
        -> (impl: EngineImpl, origin: Origin) {
        resolve(environment: environment[environmentKey], stored: defaults.string(forKey: defaultsKey))
    }
}
