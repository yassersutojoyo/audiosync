import AppKit
// The screenshot renderer compiles the app sources without SwiftPM, where
// Sparkle isn't available; build.sh checks that the app links it.
#if canImport(Sparkle)
import Sparkle
#endif

/// Sparkle's updater, started only when `UpdateGate` allows it.
@MainActor
enum AppUpdater {

    #if canImport(Sparkle)
    private static var controller: SPUStandardUpdaterController?
    #endif

    static var isEnabled: Bool {
        UpdateGate.isEnabled(publicEDKey: Bundle.main.object(forInfoDictionaryKey: UpdateGate.publicKeyInfoKey))
    }

    static func start() {
        #if canImport(Sparkle)
        guard isEnabled, controller == nil else { return }
        controller = SPUStandardUpdaterController(startingUpdater: true, updaterDelegate: nil,
                                                  userDriverDelegate: nil)
        #endif
    }

    /// "Check for Updates…", or nil when updates are off, so the menu leaves it out.
    static func menuItem(text: UIStrings) -> NSMenuItem? {
        #if canImport(Sparkle)
        guard let controller else { return nil }
        let item = NSMenuItem(title: text.menuCheckForUpdates,
                              action: #selector(SPUStandardUpdaterController.checkForUpdates(_:)),
                              keyEquivalent: "")
        item.target = controller
        return item
        #else
        return nil
        #endif
    }
}
