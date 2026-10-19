import Foundation

/// The 10-band graphic equalizer's settings and presets. The Rust core runs
/// the filters (`CoreEqualizer`), once on the captured stream, so every
/// output, local and AirPlay, hears the same sound.
enum Equalizer {

    static let frequencies: [Double] = [32, 64, 125, 250, 500, 1000, 2000, 4000, 8000, 16000]
    static let maxGain = 12.0   // dB, either way

    struct Settings: Codable, Equatable {
        var enabled = false
        var preamp = 0.0                                          // dB
        var gains = [Double](repeating: 0, count: Equalizer.frequencies.count)  // dB
    }

    /// A preset's stable identity; its display name lives in `AppText`.
    enum PresetID: String, CaseIterable {
        case flat, bassBoost, bassCut, trebleBoost, vocal, rock, pop, jazz,
             classical, electronic, loudness, smallSpeakers
    }

    struct Preset {
        let id: PresetID
        let gains: [Double]
    }

    static let presets: [Preset] = [
        Preset(id: .flat, gains: [0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
        Preset(id: .bassBoost, gains: [6, 5, 4, 2, 0, 0, 0, 0, 0, 0]),
        Preset(id: .bassCut, gains: [-6, -5, -4, -2, 0, 0, 0, 0, 0, 0]),
        Preset(id: .trebleBoost, gains: [0, 0, 0, 0, 0, 1, 2, 4, 5, 6]),
        Preset(id: .vocal, gains: [-2, -2, -1, 1, 3, 4, 3, 1, 0, -1]),
        Preset(id: .rock, gains: [4, 3, 2, 0, -1, -1, 1, 3, 4, 4]),
        Preset(id: .pop, gains: [-1, 0, 2, 3, 4, 3, 1, 0, -1, -1]),
        Preset(id: .jazz, gains: [3, 2, 1, 2, -1, -1, 0, 1, 2, 3]),
        Preset(id: .classical, gains: [4, 3, 2, 1, -1, -1, 0, 2, 3, 4]),
        Preset(id: .electronic, gains: [5, 4, 1, 0, -2, 1, 0, 1, 4, 5]),
        Preset(id: .loudness, gains: [6, 4, 0, 0, -2, 0, -1, -2, 4, 2]),
        Preset(id: .smallSpeakers, gains: [-4, -2, 2, 3, 2, 1, 1, 2, 2, 1]),
    ]
}

/// Equalizer state for the SwiftUI panel.
final class EqualizerModel: ObservableObject {
    @Published var settings: Equalizer.Settings
    var onChange: (Equalizer.Settings) -> Void = { _ in }

    init(settings: Equalizer.Settings) { self.settings = settings }

    func set(_ new: Equalizer.Settings) {
        settings = new
        onChange(new)
    }

    var matchingPreset: Equalizer.PresetID? {
        Equalizer.presets.first { $0.gains == settings.gains }?.id
    }
}
