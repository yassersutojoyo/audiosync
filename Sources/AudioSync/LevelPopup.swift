import AppKit
import SwiftUI

/// The master's level after a volume key (S429), in place of macOS's volume
/// display, which the key no longer reaches: bottom center of the screen
/// under the pointer, never key or main, and clicks pass through it.
final class LevelPopup {
    private static let size = NSSize(width: 240, height: 64)
    /// Above the Dock or the screen's bottom edge.
    private static let gap: CGFloat = 12
    /// How far the card rises as it comes in, unless Reduce Motion is on.
    private static let rise: CGFloat = 6
    /// About 2.1 s in all: in for 6 %, held until 80 %, then out.
    private static let comeIn = 0.126
    private static let hold = 1.68
    private static let goOut = 0.42

    private let model: LevelPopupModel
    private lazy var panel: NSPanel = self.makePanel()
    /// Counts the shows, so a fade-out that a newer key interrupted doesn't hide the card.
    private var shows = 0

    init(text: UIStrings) { model = LevelPopupModel(text: text) }

    func show(_ level: Double) {
        model.level = level
        // The card is never focused, so VoiceOver hears the level only through an announcement.
        NSAccessibility.post(element: NSApplication.shared, notification: .announcementRequested,
                             userInfo: [.announcement: model.text.volumePopupLevel(level),
                                        .priority: NSAccessibilityPriorityLevel.high.rawValue])
        let mouse = NSEvent.mouseLocation
        let screen = NSScreen.screens.first(where: { NSMouseInRect(mouse, $0.frame, false) }) ?? NSScreen.main
        guard let area = screen?.visibleFrame else { return }
        let rest = NSRect(x: area.midX - Self.size.width / 2, y: area.minY + Self.gap,
                          width: Self.size.width, height: Self.size.height)
        shows += 1
        let show = shows
        // A key while it shows keeps it up without coming in again.
        if !panel.isVisible {
            let motion = !NSWorkspace.shared.accessibilityDisplayShouldReduceMotion
            panel.alphaValue = 0
            panel.setFrame(motion ? rest.offsetBy(dx: 0, dy: -Self.rise) : rest, display: false)
            panel.orderFrontRegardless()
        }
        NSAnimationContext.runAnimationGroup { context in
            context.duration = Self.comeIn
            panel.animator().alphaValue = 1
            panel.animator().setFrame(rest, display: true)
        }
        DispatchQueue.main.asyncAfter(deadline: .now() + Self.hold) { [weak self] in
            guard let self, self.shows == show else { return }
            NSAnimationContext.runAnimationGroup({ context in
                context.duration = Self.goOut
                self.panel.animator().alphaValue = 0
            }, completionHandler: { [weak self] in
                MainActor.assumeIsolated {
                    guard let self, self.shows == show else { return }
                    self.panel.orderOut(nil)
                }
            })
        }
    }

    private func makePanel() -> NSPanel {
        let panel = NSPanel(contentRect: NSRect(origin: .zero, size: Self.size),
                            styleMask: [.borderless, .nonactivatingPanel], backing: .buffered, defer: true)
        panel.level = .statusBar
        panel.isOpaque = false
        panel.backgroundColor = .clear
        panel.hasShadow = true
        panel.ignoresMouseEvents = true
        panel.hidesOnDeactivate = false
        panel.collectionBehavior = [.canJoinAllSpaces, .fullScreenAuxiliary, .stationary, .ignoresCycle]
        panel.contentView = NSHostingView(rootView: LevelPopupView(model: model))
        return panel
    }
}

final class LevelPopupModel: ObservableObject {
    let text: UIStrings
    @Published var level: Double = 0

    init(text: UIStrings) { self.text = text }
}

private struct LevelPopupView: View {
    @ObservedObject var model: LevelPopupModel

    // The fill is the small-text teal (`Brand.tealText`), as the main window's meters.
    private static let background = dynamic(light: 0xF6F6F6, dark: 0x2B2B2B)
    private static let secondary = dynamic(light: 0x525C5A, dark: 0xB8C2C0)
    private static let track = dynamic(light: 0xD9DEDD, dark: 0x3D3D3D)

    var body: some View {
        VStack(alignment: .leading, spacing: 8) {
            Text(model.text.volumePopupName)
                .font(.system(size: 11))
                .foregroundStyle(Self.secondary)
            HStack(spacing: 10) {
                Image(systemName: MasterMute.symbol(level: model.level))
                    .frame(width: 20, alignment: .leading)
                GeometryReader { bar in
                    ZStack(alignment: .leading) {
                        Capsule().fill(Self.track)
                        Capsule().fill(Brand.tealText)
                            .frame(width: bar.size.width * min(max(model.level, 0), 100) / 100)
                    }
                }
                .frame(height: 6)
                Text(model.text.volumePopupLevel(model.level))
                    .font(.system(size: 14).monospacedDigit())
                    .frame(minWidth: 40, alignment: .trailing)
            }
        }
        .foregroundStyle(Color.primary)
        .padding(.vertical, 12)
        .padding(.horizontal, 14)
        .frame(width: 240, height: 64)
        .background(RoundedRectangle(cornerRadius: 14).fill(Self.background))
        .overlay(RoundedRectangle(cornerRadius: 14).strokeBorder(Color(nsColor: .separatorColor), lineWidth: 0.5))
    }

    private static func dynamic(light: UInt32, dark: UInt32) -> Color {
        Color(nsColor: NSColor(name: nil) { appearance in
            let rgb = appearance.bestMatch(from: [.aqua, .darkAqua]) == .darkAqua ? dark : light
            return NSColor(srgbRed: CGFloat(rgb >> 16 & 0xFF) / 255, green: CGFloat(rgb >> 8 & 0xFF) / 255,
                           blue: CGFloat(rgb & 0xFF) / 255, alpha: 1)
        })
    }
}
