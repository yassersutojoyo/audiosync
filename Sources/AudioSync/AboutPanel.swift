import AppKit

/// The standard About panel, with AudioSync's copyright holder and license
/// (GPL-3.0-or-later, no warranty) shown under the version.
@MainActor
final class AboutPanel: NSObject {

    /// Menu items hold their target weakly, so the panel's target lives here.
    private static var shared: AboutPanel?

    private let text: UIStrings

    private init(text: UIStrings) {
        self.text = text
    }

    /// The About menu item, targeting the panel built from `text`.
    static func menuItem(text: UIStrings) -> NSMenuItem {
        let panel = AboutPanel(text: text)
        shared = panel
        let item = NSMenuItem(title: text.menuAbout, action: #selector(show(_:)), keyEquivalent: "")
        item.target = panel
        return item
    }

    @objc func show(_ sender: Any?) {
        let paragraph = NSMutableParagraphStyle()
        paragraph.alignment = .center
        let credits = NSAttributedString(string: text.aboutLicense, attributes: [
            .font: NSFont.systemFont(ofSize: NSFont.smallSystemFontSize),
            .foregroundColor: NSColor.secondaryLabelColor,
            .paragraphStyle: paragraph,
        ])
        let options = Self.options(credits: credits, copyright: text.aboutCopyright,
                                   info: Bundle.main.infoDictionary ?? [:])
        NSApp.orderFrontStandardAboutPanel(options: options)
        NSApp.activate(ignoringOtherApps: true)
    }

    /// An empty `.version` drops the build number the panel adds in
    /// parentheses; CFBundleVersion itself stays, since Sparkle compares it.
    nonisolated static func options(credits: NSAttributedString, copyright: String,
                                    info: [String: Any]) -> [NSApplication.AboutPanelOptionKey: Any] {
        [
            .applicationVersion: info["CFBundleShortVersionString"] as? String ?? "",
            .version: "",
            .credits: credits,
            NSApplication.AboutPanelOptionKey(rawValue: "Copyright"): copyright,
        ]
    }
}
