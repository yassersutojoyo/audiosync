import AppKit

/// The look the user picked in the Appearance menu or the settings.
/// The raw value doubles as the stored setting and as the menu item's
/// `representedObject`, so renaming a case would orphan saved choices.
enum AppAppearance: String, CaseIterable {
    case system
    case light
    case dark

    /// Anything unknown (an older or newer build wrote it, or nothing is
    /// stored yet) falls back to following macOS.
    init(defaultsValue stored: String?) {
        self = stored.flatMap(AppAppearance.init(rawValue:)) ?? .system
    }

    var defaultsValue: String { rawValue }

    /// nil lets macOS choose; a name pins the whole app to that look.
    var appearanceName: NSAppearance.Name? {
        switch self {
        case .system: return nil
        case .light: return .aqua
        case .dark: return .darkAqua
        }
    }

    /// Setting it on the application rather than per window means panels,
    /// menus and alerts created later pick it up too. The status item icon
    /// is a template image, so it keeps matching the menu bar either way.
    /// Callers run on the main thread (launch and the settings didSet);
    /// `assumeIsolated` states that and traps if it ever stops being true.
    func apply() {
        let look = appearanceName.flatMap { NSAppearance(named: $0) }
        MainActor.assumeIsolated { NSApp.appearance = look }
    }
}
