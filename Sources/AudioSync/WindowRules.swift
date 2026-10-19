import Foundation
import CoreGraphics

// Small decisions behind the windows and the model the views render, kept
// free of AppKit so the logic tests can check them.

/// Whether a saved window frame can still be reached.
enum WindowPlacement {
    /// How much of a window must lie on one screen's visible area for the
    /// user to grab its title bar and pull it back.
    static let minimumVisible = CGSize(width: 120, height: 60)

    /// True when `frame` overlaps one of `screens` (their visible areas) by at
    /// least `minimumVisible`. A frame saved on a display that is gone, or
    /// pushed under the menu bar, fails and the window is centered instead.
    static func isReachable(_ frame: CGRect, screens: [CGRect]) -> Bool {
        screens.contains { screen in
            let overlap = screen.intersection(frame)
            return !overlap.isNull
                && overlap.width >= minimumVisible.width && overlap.height >= minimumVisible.height
        }
    }
}

/// The entries of the source picker, in the order it lists them.
enum SourceList {
    struct Entry: Equatable {
        /// `AudioSource.defaultsValue`: "" for the whole system, the bundle ID
        /// for an app, "input:<uid>" for an input device.
        let id: String
        let name: String
        let isInput: Bool
    }

    /// The whole system first, then the apps by name, then the input devices
    /// in Core Audio's order. The chosen app stays listed while it isn't
    /// running (its name comes from `chosenAppName`), so the picker still
    /// says what will be captured; a chosen input device that is unplugged
    /// is not listed. Devices without a UID can't be chosen and are skipped.
    static func entries(systemName: String,
                        apps: [(bundleID: String, name: String)],
                        inputs: [(uid: String, name: String)],
                        chosen: AudioSource,
                        chosenAppName: (String) -> String) -> [Entry] {
        var appEntries = apps.map { Entry(id: $0.bundleID, name: $0.name, isInput: false) }
        if case .app(let bundleID) = chosen, !appEntries.contains(where: { $0.id == bundleID }) {
            appEntries.append(Entry(id: bundleID, name: chosenAppName(bundleID), isInput: false))
            appEntries.sort { $0.name.localizedCaseInsensitiveCompare($1.name) == .orderedAscending }
        }
        let inputEntries = inputs.filter { !$0.uid.isEmpty }.map {
            Entry(id: AudioSource.input(uid: $0.uid).defaultsValue, name: $0.name, isInput: true)
        }
        return [Entry(id: AudioSource.system.defaultsValue, name: systemName, isInput: false)]
            + appEntries + inputEntries
    }
}

/// How a speaker group shows in the window and the panel.
enum GroupSummary {
    /// `available`: members present right now. `allOn`: at least one member
    /// is present and every present one is switched on (absent members don't
    /// count, as a click on the group leaves them alone).
    static func summarize(members: [String], isAvailable: (String) -> Bool,
                          isEnabled: (String) -> Bool) -> (available: Int, allOn: Bool) {
        let present = members.filter(isAvailable)
        return (present.count, !present.isEmpty && present.allSatisfy(isEnabled))
    }
}

/// Equalizer settings as the app stores them (JSON in UserDefaults).
enum StoredEqualizer {
    /// The saved settings, or the defaults when nothing usable is saved: no
    /// data, data that doesn't decode, or a band count other than the
    /// equalizer's (from a build with different bands).
    static func decode(_ data: Data?) -> Equalizer.Settings {
        guard let data, let saved = try? JSONDecoder().decode(Equalizer.Settings.self, from: data),
              saved.gains.count == Equalizer.frequencies.count,
              saved.preamp.isFinite, saved.gains.allSatisfy(\.isFinite)
        else { return Equalizer.Settings() }
        return saved
    }

    static func encode(_ settings: Equalizer.Settings) -> Data? {
        try? JSONEncoder().encode(settings)
    }
}
