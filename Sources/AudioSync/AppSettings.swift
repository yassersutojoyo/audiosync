import Foundation

/// What AudioSync remembers between launches: the source, the speakers that
/// are on, every volume and delay, the groups, auto-connect and the two view
/// choices. The equalizer is kept under `Keys.equalizer` as JSON
/// (`StoredEqualizer`), read and written by its own code.
///
/// Every value is checked as it is read, so a damaged or hand-edited setting
/// never reaches the engine: a number that isn't finite is dropped, volumes
/// are clamped to 0…100, offsets to −2000…20 000 ms and the AirPlay delays
/// to their ranges (spec S76, S175, S178). Clause numbers refer to
/// docs/ENGINE_SPEC.md.
struct AppSettings: Equatable {

    /// The UserDefaults keys. Every key AudioSync stores lives here, and
    /// existing installs depend on each string: a rename loses that setting.
    enum Keys {
        /// `AudioSource.defaultsValue` (S12): "" or missing is the whole system.
        static let source = "audioSourceBundleID"
        /// App IDs (`NativeAirPlayID`) of the AirPlay speakers that are on, as an array.
        static let enabledAirPlay = "enabledAirPlayIds"
        /// Device UIDs of the local speakers that are on, as an array.
        static let enabledLocal = "enabledLocalUIDs"
        /// The master slider's position, 0…100.
        static let master = "masterVolume"
        /// Each local speaker's slider position (0…100), by device UID.
        static let localVolumes = "localVolumePosByUID"
        /// Each local speaker's absolute delay in ms, by device UID, from
        /// before the local delay model (S77). Read once by the migration
        /// (S179), never written; left in place for older builds.
        static let localDelays = "localDelayMsByUID"
        /// Each AirPlay speaker's own slider position (0…100), by output ID (S73).
        static let airplayVolumes = "airPlayVolumeByID"
        /// `[SpeakerGroup]` as JSON (S149).
        static let groups = "speakerGroups"
        /// Speaker keys (`SpeakerKey`) marked for auto-connect, as an array.
        static let autoConnect = "autoConnectSpeakers"
        /// The names the user gave speakers, by speaker key (`SpeakerNames`).
        static let speakerNames = "speakerNamesByKey"
        static let showVirtualDevices = "showVirtualOutputDevices"
        /// `AppAppearance.defaultsValue` (S155).
        static let appearance = "appearance"
        /// `WhenSourceQuits.defaultsValue` (S227): "stop", or "system" to switch.
        static let whenSourceQuits = "whenSourceQuits"
        /// `Equalizer.Settings` as JSON (S24, `StoredEqualizer`).
        static let equalizer = "equalizerSettings"
        /// One delay in ms for every local speaker, from before delays were
        /// per speaker: where a speaker without its own delay started (S77).
        /// Read once by the migration (S179), never written.
        static let legacyDelayMs = "delayMs"
        /// Local volumes as linear gain in percent (0…150), from before the
        /// volume curve. Converted to `localVolumes` once, then removed (S75).
        static let legacyLocalGains = "localGainByUID"
        /// "swift" or "rust", from builds that offered a choice of audio
        /// engine. Never written now; a value left there has no effect, is
        /// noted once and removed (`RetiredEngineChoice`).
        static let retiredEngineChoice = "engineImpl"
        /// "native" or the old engine's value, from builds that offered a
        /// choice of AirPlay sender. Never written now; a value left there has
        /// no effect, is noted once and removed (`RetiredAirPlayChoice`).
        static let retiredAirPlayChoice = "airplayImpl"
        /// Each local speaker's offset in ms relative to the reference
        /// (−2000…20 000), by device UID (S175). Missing is 0.
        static let localOffsets = "localOffsetMsByUID"
        /// The AirPlay delay of the older builds that shipped the bundled
        /// engine, L itself, in ms. Written once by the migration (S179), for
        /// those builds; never read now.
        static let airPlayDelay = "airPlayDelayMs"
        /// The AirPlay delay, an adjustment on top of the latency the AirPlay
        /// sender reports, in ms (−2000…20 000, 0 when missing; S178).
        static let nativeAirPlayAdjustment = "nativeAirPlayAdjustmentMs"
        /// True once the delays of `localDelays` and `legacyDelayMs` were
        /// migrated to offsets and the AirPlay delays (S179); written last.
        static let localDelaysMigrated = "localDelayModelMigrated"
    }

    /// Where a speaker without a volume of its own starts (S71, S105).
    static let defaultVolume: Double = 60
    static let defaultMaster: Double = 100
    /// Where a local speaker without a delay of its own started before the
    /// local delay model, unless an older single delay was stored (S77).
    static let standardDelayMs: Double = 2000
    static let maxDelayMs: Double = 20_000

    var source = AudioSource.system
    var enabledAirPlay: Set<String> = []
    var enabledLocal: Set<String> = []
    var master = AppSettings.defaultMaster
    var localVolumes: [String: Double] = [:]
    /// The absolute delays from before the local delay model (S77), as the
    /// migration reads them.
    var localDelays: [String: Double] = [:]
    var airplayVolumes: [String: Double] = [:]
    var groups: [SpeakerGroup] = []
    var autoConnect: Set<String> = []
    var speakerNames = SpeakerNames()
    var showVirtualDevices = false
    var appearance = AppAppearance.system
    var whenSourceQuits = WhenSourceQuits.stop
    /// D0 of the migration: what a local speaker without a delay of its own
    /// played with before the local delay model (S77, S179).
    var defaultDelayMs = AppSettings.standardDelayMs
    /// Offsets relative to the reference, by device UID (S175).
    var localOffsets: [String: Double] = [:]
    var airPlayDelay = AirPlayDelay()
    /// The migration to offsets ran (S179).
    var localDelaysMigrated = false

    /// Reads every setting through `read`, which returns what is stored for a
    /// key (UserDefaults' `object(forKey:)`), or nil.
    static func load(_ read: (String) -> Any?) -> AppSettings {
        var s = AppSettings()
        s.source = AudioSource(defaultsValue: read(Keys.source) as? String)
        s.enabledAirPlay = ids(read(Keys.enabledAirPlay))
        s.enabledLocal = ids(read(Keys.enabledLocal))
        s.autoConnect = ids(read(Keys.autoConnect))
        s.speakerNames = SpeakerNames(stored: read(Keys.speakerNames))
        s.master = read(Keys.master).flatMap(number).map(volume) ?? defaultMaster
        s.localVolumes = levels(read(Keys.localVolumes), volume)
        s.localDelays = levels(read(Keys.localDelays), delay)
        s.defaultDelayMs = read(Keys.legacyDelayMs).flatMap(number).map(delay) ?? standardDelayMs
        s.localOffsets = levels(read(Keys.localOffsets)) { LocalDelay.sanitizedOffset($0) ?? 0 }
        s.airPlayDelay.adjustmentMs = read(Keys.nativeAirPlayAdjustment).flatMap(number)
            .flatMap(LocalDelay.sanitizedAdjustment) ?? 0
        s.localDelaysMigrated = read(Keys.localDelaysMigrated) as? Bool ?? false
        s.airplayVolumes = levels(read(Keys.airplayVolumes), volume)
        s.groups = decodeGroups(read(Keys.groups) as? Data)
        s.showVirtualDevices = read(Keys.showVirtualDevices) as? Bool ?? false
        s.appearance = AppAppearance(defaultsValue: read(Keys.appearance) as? String)
        s.whenSourceQuits = WhenSourceQuits(defaultsValue: read(Keys.whenSourceQuits) as? String)
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

    /// Removed once no speaker has a name of the user's.
    func save(speakerNames: SpeakerNames) {
        write(speakerNames.custom.isEmpty ? nil : speakerNames.custom, Keys.speakerNames)
    }

    func save(showVirtualDevices: Bool) { write(showVirtualDevices, Keys.showVirtualDevices) }

    func save(appearance: AppAppearance) { write(appearance.defaultsValue, Keys.appearance) }

    func save(whenSourceQuits: WhenSourceQuits) { write(whenSourceQuits.defaultsValue, Keys.whenSourceQuits) }

    /// The AirPlay delay (S178).
    func save(airPlayDelay: AirPlayDelay) {
        write(airPlayDelay.adjustmentMs, Keys.nativeAirPlayAdjustment)
    }

    /// The one-time migration of the absolute delays to offsets (S179), the
    /// first time a device list is in: `listed` holds its UIDs, `reference`
    /// the reference chosen from it (S173). It reads the old delays, the
    /// older single delay and the speakers that are on, stores the offsets
    /// and the AirPlay delays under their own keys and then the mark, and
    /// returns what it stored. The old keys stay as they are, for older
    /// builds. Nil, changing nothing, once the mark is set; a migration cut
    /// short before the mark runs again from the same old values.
    @discardableResult
    func migrateLocalDelays(listed: [String], reference: String?) -> LocalDelay.Migration? {
        guard read(Keys.localDelaysMigrated) as? Bool != true else { return nil }
        guard let migration = LocalDelay.migrate(
            storedDelays: AppSettings.levels(read(Keys.localDelays), AppSettings.delay),
            legacyDelayMs: read(Keys.legacyDelayMs).flatMap(AppSettings.number),
            enabled: AppSettings.ids(read(Keys.enabledLocal)), listed: listed, reference: reference)
        else { return nil }
        save(levels: migration.offsets, forKey: Keys.localOffsets)
        write(migration.airPlayDelayMs, Keys.airPlayDelay)
        save(airPlayDelay: AirPlayDelay(adjustmentMs: migration.nativeAdjustmentMs))
        write(true, Keys.localDelaysMigrated)
        return migration
    }
}

/// The audio engine choice of older builds: the `engineImpl` setting and the
/// `AUDIOSYNC_ENGINE` environment variable, "swift" or "rust". Every session
/// runs on the Rust core now, so either one is only noted in the debug log:
/// the variable at every launch that has it, the setting once, since that
/// launch removes it (`SettingsStore.retiredChoiceNotes`).
enum RetiredEngineChoice {
    static let defaultsKey = AppSettings.Keys.retiredEngineChoice
    static let environmentKey = "AUDIOSYNC_ENGINE"

    /// The debug log line for a launch that finds either one set (an empty
    /// value counts as unset), or nil when neither is.
    static func launchNote(environment: String?, stored: String?) -> String? {
        var found: [String] = []
        if let value = environment, !value.isEmpty { found.append("\(environmentKey)=\(value)") }
        if let value = stored, !value.isEmpty { found.append("the \(defaultsKey) setting \"\(value)\"") }
        guard !found.isEmpty else { return nil }
        return "[AudioSync] " + found.joined(separator: " and ")
            + (found.count == 1 ? " no longer chooses" : " no longer choose")
            + " the audio engine: every session runs on the Rust core"
    }
}

/// The AirPlay sender choice of older builds: the `airplayImpl` setting and
/// the `AUDIOSYNC_AIRPLAY` environment variable, "native" or the old engine's value.
/// AudioSync's own sender is the only one now, so either one is only noted in
/// the debug log: the variable at every launch that has it, the setting once,
/// since that launch removes it (`SettingsStore.retiredChoiceNotes`).
enum RetiredAirPlayChoice {
    static let defaultsKey = AppSettings.Keys.retiredAirPlayChoice
    static let environmentKey = "AUDIOSYNC_AIRPLAY"

    /// The debug log line for a launch that finds either one set (an empty
    /// value counts as unset), or nil when neither is.
    static func launchNote(environment: String?, stored: String?) -> String? {
        var found: [String] = []
        if let value = environment, !value.isEmpty { found.append("\(environmentKey)=\(value)") }
        if let value = stored, !value.isEmpty { found.append("the \(defaultsKey) setting \"\(value)\"") }
        guard !found.isEmpty else { return nil }
        return "[AudioSync] " + found.joined(separator: " and ")
            + (found.count == 1 ? " no longer chooses" : " no longer choose")
            + " the AirPlay sender: AudioSync's own sender is the only one"
    }
}

extension SettingsStore {
    /// The debug log lines for the retired engine and AirPlay sender choices
    /// at launch. A stored choice is removed once read, whatever its value,
    /// so the next launch notes only an environment variable still set.
    func retiredChoiceNotes(environment: [String: String]) -> [String] {
        let engine = read(RetiredEngineChoice.defaultsKey)
        let airPlay = read(RetiredAirPlayChoice.defaultsKey)
        if engine != nil { write(nil, RetiredEngineChoice.defaultsKey) }
        if airPlay != nil { write(nil, RetiredAirPlayChoice.defaultsKey) }
        return [
            RetiredEngineChoice.launchNote(environment: environment[RetiredEngineChoice.environmentKey],
                                           stored: engine as? String),
            RetiredAirPlayChoice.launchNote(environment: environment[RetiredAirPlayChoice.environmentKey],
                                            stored: airPlay as? String),
        ].compactMap { $0 }
    }
}
