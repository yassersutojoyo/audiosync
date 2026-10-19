import Foundation

/// What AudioSync remembers between launches: the source, the speakers that
/// are on, every volume and delay, the groups, auto-connect and the two view
/// choices. The equalizer is kept under `Keys.equalizer` as JSON
/// (`StoredEqualizer`), and the hidden engine option under
/// `EngineImpl.defaultsKey`; both are read and written by their own code.
///
/// Every value is checked as it is read, so a damaged or hand-edited setting
/// never reaches the engine: a number that isn't finite is dropped, volumes
/// are clamped to 0…100 and delays to 0…20 000 ms (spec S76, S77, S124).
/// Clause numbers refer to docs/ENGINE_SPEC.md.
struct AppSettings: Equatable {

    /// The UserDefaults keys. Every key AudioSync stores lives here, and
    /// existing installs depend on each string: a rename loses that setting.
    enum Keys {
        /// `AudioSource.defaultsValue` (S12): "" or missing is the whole system.
        static let source = "audioSourceBundleID"
        /// Engine output IDs of the AirPlay speakers that are on, as an array.
        static let enabledAirPlay = "enabledAirPlayIds"
        /// Device UIDs of the local speakers that are on, as an array.
        static let enabledLocal = "enabledLocalUIDs"
        /// The master slider's position, 0…100.
        static let master = "masterVolume"
        /// Each local speaker's slider position (0…100), by device UID.
        static let localVolumes = "localVolumePosByUID"
        /// Each local speaker's configured delay in ms, by device UID.
        static let localDelays = "localDelayMsByUID"
        /// Each AirPlay speaker's own slider position (0…100), by output ID (S73).
        static let airplayVolumes = "airPlayVolumeByID"
        /// `[SpeakerGroup]` as JSON (S149).
        static let groups = "speakerGroups"
        /// Speaker keys (`SpeakerKey`) marked for auto-connect, as an array.
        static let autoConnect = "autoConnectSpeakers"
        static let showVirtualDevices = "showVirtualOutputDevices"
        /// `AppAppearance.defaultsValue` (S155).
        static let appearance = "appearance"
        /// `Equalizer.Settings` as JSON (S24, `StoredEqualizer`).
        static let equalizer = "equalizerSettings"
        /// One delay in ms for every local speaker, from before delays were
        /// per speaker: where a speaker without its own delay starts (S77).
        static let legacyDelayMs = "delayMs"
        /// Local volumes as linear gain in percent (0…150), from before the
        /// volume curve. Converted to `localVolumes` once, then removed (S75).
        static let legacyLocalGains = "localGainByUID"
    }

    /// Where a speaker without a volume of its own starts (S71, S105).
    static let defaultVolume: Double = 60
    static let defaultMaster: Double = 100
    /// Where a local speaker without a delay of its own starts, unless an
    /// older single delay is stored (S77).
    static let standardDelayMs: Double = 2000
    static let maxDelayMs: Double = 20_000

    var source = AudioSource.system
    var enabledAirPlay: Set<String> = []
    var enabledLocal: Set<String> = []
    var master = AppSettings.defaultMaster
    var localVolumes: [String: Double] = [:]
    var localDelays: [String: Double] = [:]
    var airplayVolumes: [String: Double] = [:]
    var groups: [SpeakerGroup] = []
    var autoConnect: Set<String> = []
    var showVirtualDevices = false
    var appearance = AppAppearance.system
    /// The delay a local speaker plays with until it gets one of its own.
    var defaultDelayMs = AppSettings.standardDelayMs

    /// Reads every setting through `read`, which returns what is stored for a
    /// key (UserDefaults' `object(forKey:)`), or nil.
    static func load(_ read: (String) -> Any?) -> AppSettings {
        var s = AppSettings()
        s.source = AudioSource(defaultsValue: read(Keys.source) as? String)
        s.enabledAirPlay = ids(read(Keys.enabledAirPlay))
        s.enabledLocal = ids(read(Keys.enabledLocal))
        s.autoConnect = ids(read(Keys.autoConnect))
        s.master = read(Keys.master).flatMap(number).map(volume) ?? defaultMaster
        s.localVolumes = levels(read(Keys.localVolumes), volume)
        s.localDelays = levels(read(Keys.localDelays), delay)
        s.defaultDelayMs = read(Keys.legacyDelayMs).flatMap(number).map(delay) ?? standardDelayMs
        s.airplayVolumes = levels(read(Keys.airplayVolumes), volume)
        s.groups = decodeGroups(read(Keys.groups) as? Data)
        s.showVirtualDevices = read(Keys.showVirtualDevices) as? Bool ?? false
        s.appearance = AppAppearance(defaultsValue: read(Keys.appearance) as? String)
        return s
    }

    // MARK: - Reading single values

    /// A stored number, whichever way it was stored; nil for anything else
    /// and for NaN or infinity.
    static func number(_ value: Any) -> Double? {
        let n: Double
        if let d = value as? Double { n = d }
        else if let i = value as? Int { n = Double(i) }
        else if let ns = value as? NSNumber { n = ns.doubleValue }
        else { return nil }
        return n.isFinite ? n : nil
    }

    /// A slider position, clamped to 0…100.
    static func volume(_ value: Double) -> Double { min(100, max(0, value)) }

    /// A delay in ms, clamped to 0…`maxDelayMs`.
    static func delay(_ value: Double) -> Double { min(maxDelayMs, max(0, value)) }

    /// An older linear gain in percent (0…150) as a slider position: the
    /// position whose gain is that percentage, so 100 % and more are 100 (S75).
    static func position(percent: Double) -> Double {
        volume(VolumeCurve.position(gain: percent / 100))
    }

    /// A stored array of IDs as a set; anything that isn't a string is skipped.
    static func ids(_ value: Any?) -> Set<String> {
        guard let list = value as? [Any] else { return [] }
        return Set(list.compactMap { $0 as? String })
    }

    /// A stored dictionary of numbers by ID, each passed through `clean`;
    /// entries that aren't finite numbers are dropped.
    static func levels(_ value: Any?, _ clean: (Double) -> Double) -> [String: Double] {
        guard let dict = value as? [String: Any] else { return [:] }
        var out: [String: Double] = [:]
        for (id, raw) in dict {
            if let n = number(raw) { out[id] = clean(n) }
        }
        return out
    }

    /// The stored groups; none when the data is missing or can't be read.
    static func decodeGroups(_ data: Data?) -> [SpeakerGroup] {
        guard let data, let list = try? JSONDecoder().decode([SpeakerGroup].self, from: data) else { return [] }
        return list
    }
}

/// Where the settings are read and written: UserDefaults in the app, a plain
/// dictionary in the logic tests. Each save writes one key, in the form
/// `AppSettings.load` reads back.
struct SettingsStore {
    typealias Keys = AppSettings.Keys

    let read: (String) -> Any?
    /// Stores a value for a key; nil removes it.
    let write: (Any?, String) -> Void

    static var standard: SettingsStore {
        SettingsStore(read: { UserDefaults.standard.object(forKey: $0) },
                      write: { UserDefaults.standard.set($0, forKey: $1) })
    }

    /// Brings older forms up to date (`migrate`), then reads every setting.
    func load() -> AppSettings {
        migrate()
        return AppSettings.load(read)
    }

    /// Local volumes stored as linear gain become slider positions under
    /// `Keys.localVolumes`, and the old key goes (S75). Positions already
    /// stored win; the old key is removed either way.
    func migrate() {
        guard let gains = read(Keys.legacyLocalGains) else { return }
        if read(Keys.localVolumes) == nil {
            save(levels: AppSettings.levels(gains, AppSettings.position(percent:)), forKey: Keys.localVolumes)
        }
        write(nil, Keys.legacyLocalGains)
    }

    func save(source: AudioSource) { write(source.defaultsValue, Keys.source) }

    /// A set of IDs, sorted so the stored array doesn't churn.
    func save(ids: Set<String>, forKey key: String) { write(ids.sorted(), key) }

    func save(levels: [String: Double], forKey key: String) { write(levels, key) }

    func save(master: Double) { write(master, Keys.master) }

    func save(groups: [SpeakerGroup]) {
        guard let data = try? JSONEncoder().encode(groups) else { return }
        write(data, Keys.groups)
    }

    func save(showVirtualDevices: Bool) { write(showVirtualDevices, Keys.showVirtualDevices) }

    func save(appearance: AppAppearance) { write(appearance.defaultsValue, Keys.appearance) }
}
