import Foundation
import CAudioSyncCore

/// The 10-band graphic equalizer's settings, presets and saved curves. The
/// Rust core runs the filters (`CoreEqualizer`), once on the captured stream,
/// so every output, local and AirPlay, hears the same sound.
enum Equalizer {

    static let frequencies: [Double] = [32, 64, 125, 250, 500, 1000, 2000, 4000, 8000, 16000]
    static let maxGain = 12.0   // dB, either way
    static let gainStep = 0.5   // dB

    struct Settings: Codable, Equatable {
        var enabled = false
        var preamp = 0.0                                          // dB
        var gains = [Double](repeating: 0, count: Equalizer.frequencies.count)  // dB

        /// Flat gains and a 0 dB preamp, so the picker reads Flat; the
        /// enabled flag stays (S33).
        mutating func reset() {
            gains = [Double](repeating: 0, count: Equalizer.frequencies.count)
            preamp = 0
        }
    }

    /// A preset's stable identity; its display name lives in `UIStrings`.
    enum PresetID: String, CaseIterable {
        case flat, bassBoost, bassCut, trebleBoost, vocal, rock, pop, jazz,
             classical, electronic, loudness, smallSpeakers
    }

    struct Preset {
        let id: PresetID
        let gains: [Double]
    }

    /// The core's presets in menu order (S33); one the app has no name for
    /// is left out.
    static let presets: [Preset] = (0..<asc_eq_preset_count()).compactMap { index -> Preset? in
        guard let raw = asc_eq_preset_id(index),
              let id = PresetID(rawValue: String(cString: raw)) else { return nil }
        var gains = [Double](repeating: 0, count: Equalizer.frequencies.count)
        guard asc_eq_preset_gains(index, &gains) else { return nil }
        return Preset(id: id, gains: gains)
    }

    /// A gain on the controls' steps: clamped to ±12 dB, 0 for NaN.
    static func quantized(_ db: Double) -> Double {
        guard !db.isNaN else { return 0 }
        return (min(max(db, -maxGain), maxGain) / gainStep).rounded() * gainStep + 0.0
    }

    // MARK: Saved curves (S242)

    static let maxCurves = 50
    static let maxCurveNameCharacters = 40

    /// Ten band gains the user saved under a name; never the preamp.
    struct Curve: Equatable {
        var name: String
        var gains: [Double]
    }

    /// The curve the picker shows.
    enum CurveMatch: Equatable {
        case preset(PresetID)
        case saved(Int)
    }

    /// The first curve whose ten gains all equal `gains`: the presets in menu
    /// order, then the saved curves in their order. The preamp never counts.
    static func matching(_ gains: [Double], saved: [Curve]) -> CurveMatch? {
        if let preset = presets.first(where: { $0.gains == gains }) { return .preset(preset.id) }
        return saved.firstIndex { $0.gains == gains }.map(CurveMatch.saved)
    }

    /// A typed name as a curve keeps it: trimmed and cut to 40 characters;
    /// nil when nothing is left.
    static func curveName(_ typed: String) -> String? {
        let trimmed = typed.trimmingCharacters(in: .whitespacesAndNewlines)
        var scalars = String.UnicodeScalarView()
        scalars.append(contentsOf: trimmed.unicodeScalars.prefix(maxCurveNameCharacters))
        let cut = String(scalars).trimmingCharacters(in: .whitespacesAndNewlines)
        return cut.isEmpty ? nil : cut
    }

    private static func sameName(_ a: String, _ b: String) -> Bool { a.lowercased() == b.lowercased() }

    /// The gains on the controls' steps, ten of them.
    private static func steppedGains(_ gains: [Double]) -> [Double] {
        (0..<frequencies.count).map { $0 < gains.count ? quantized(gains[$0]) : 0 }
    }

    /// Saves `gains` under `typed`: a saved curve of the same name, ignoring
    /// case, takes the new gains in its place; otherwise the curve goes last.
    /// Its index, or nil for an empty name or a full list.
    static func save(_ typed: String, gains: [Double], in curves: inout [Curve]) -> Int? {
        guard let name = curveName(typed) else { return nil }
        let curve = Curve(name: name, gains: steppedGains(gains))
        if let index = curves.firstIndex(where: { sameName($0.name, name) }) {
            curves[index] = curve
            return index
        }
        guard curves.count < maxCurves else { return nil }
        curves.append(curve)
        return curves.count - 1
    }

    /// False for a bad index, an empty name or one another saved curve has.
    static func rename(_ index: Int, to typed: String, in curves: inout [Curve]) -> Bool {
        guard curves.indices.contains(index), let name = curveName(typed),
              !curves.indices.contains(where: { $0 != index && sameName(curves[$0].name, name) })
        else { return false }
        curves[index].name = name
        return true
    }

    /// Saved curves as read back: names as `curveName` keeps them, gains on
    /// the steps, unnamed curves and later duplicates of a name left out, at
    /// most 50.
    static func sanitized(_ saved: [Curve]) -> [Curve] {
        var curves: [Curve] = []
        for curve in saved where curves.count < maxCurves {
            guard let name = curveName(curve.name),
                  !curves.contains(where: { sameName($0.name, name) }) else { continue }
            curves.append(Curve(name: name, gains: steppedGains(curve.gains)))
        }
        return curves
    }
}

/// Saved curves as the app stores them under `Keys.equalizerCurves`: a JSON
/// array of `{"name", "gains"}`, read leniently (an entry without a text name
/// is skipped, a band that is not a number reads as 0 dB).
enum StoredCurves {
    static func decode(_ data: Data?) -> [Equalizer.Curve] {
        guard let data, let entries = try? JSONSerialization.jsonObject(with: data) as? [Any] else { return [] }
        return Equalizer.sanitized(entries.compactMap { entry in
            guard let fields = entry as? [String: Any], let name = fields["name"] as? String else { return nil }
            let gains = (fields["gains"] as? [Any] ?? []).map { ($0 as? NSNumber)?.doubleValue ?? 0 }
            return Equalizer.Curve(name: name, gains: gains)
        })
    }

    static func encode(_ curves: [Equalizer.Curve]) -> Data? {
        let entries: [[String: Any]] = curves.map { ["name": $0.name, "gains": $0.gains] }
        return try? JSONSerialization.data(withJSONObject: entries)
    }
}

/// Equalizer state for the SwiftUI panel.
final class EqualizerModel: ObservableObject {
    @Published var settings: Equalizer.Settings
    @Published private(set) var curves: [Equalizer.Curve]
    var onChange: (Equalizer.Settings) -> Void = { _ in }
    var onCurvesChange: ([Equalizer.Curve]) -> Void = { _ in }

    init(settings: Equalizer.Settings, curves: [Equalizer.Curve] = []) {
        self.settings = settings
        self.curves = Equalizer.sanitized(curves)
    }

    func set(_ new: Equalizer.Settings) {
        settings = new
        onChange(new)
    }

    var shownCurve: Equalizer.CurveMatch? { Equalizer.matching(settings.gains, saved: curves) }

    /// The saved curve the picker shows, which Rename and Delete act on.
    var shownSavedIndex: Int? {
        if case .saved(let index) = shownCurve { return index }
        return nil
    }

    /// Picking a curve sets the gains only and turns the equalizer on.
    func pick(gains: [Double]) {
        var new = settings
        new.gains = gains
        new.enabled = true
        set(new)
    }

    func reset() {
        var new = settings
        new.reset()
        set(new)
    }

    /// False for an empty name or a full list.
    func saveCurve(named typed: String) -> Bool {
        var new = curves
        guard Equalizer.save(typed, gains: settings.gains, in: &new) != nil else { return false }
        setCurves(new)
        return true
    }

    /// False for an empty name or one another saved curve has.
    func renameCurve(_ index: Int, to typed: String) -> Bool {
        var new = curves
        guard Equalizer.rename(index, to: typed, in: &new) else { return false }
        setCurves(new)
        return true
    }

    func deleteCurve(_ index: Int) {
        guard curves.indices.contains(index) else { return }
        var new = curves
        new.remove(at: index)
        setCurves(new)
    }

    private func setCurves(_ new: [Equalizer.Curve]) {
        curves = new
        onCurvesChange(new)
    }
}
