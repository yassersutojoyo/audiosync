import AppKit
import SwiftUI

/// Settings… (Command-comma): every choice that stays set, in one window of
/// boxes, so the menus keep only commands.
@MainActor
final class SettingsWindowController {
    private var window: NSWindow?
    private var host: NSHostingController<SettingsView>?
    private let model: AppModel
    private let updates = UpdateChecks()
    private let motion = OpenMotion()
    private let setUpPermissions: () -> Void
    private var observers: [NSObjectProtocol] = []

    init(model: AppModel, setUpPermissions: @escaping () -> Void) {
        self.model = model
        self.setUpPermissions = setUpPermissions
    }

    func show() {
        updates.reread()
        let opening = window?.isVisible != true
        let panel = window ?? makeWindow()
        window = panel
        if opening {
            fit(contentHeight())
            animateOpen(panel)
        }
        NSApp.activate(ignoringOtherApps: true)
        panel.makeKeyAndOrderFront(nil)
    }

    private func makeWindow() -> NSWindow {
        let view = SettingsView(model: model, updates: updates, motion: motion,
                                setUpPermissions: setUpPermissions) { [weak self] height in self?.fit(height) }
        let host = NSHostingController(rootView: view)
        self.host = host
        // The window follows the content's height through `fit`, top edge fixed.
        host.sizingOptions = []
        let panel = SettingsPanel(contentViewController: host)
        panel.title = model.text.settingsTitle
        panel.styleMask = [.titled, .closable]
        panel.isReleasedWhenClosed = false
        panel.animationBehavior = .none   // the open motion is `animateOpen`'s
        // The view's fitting size is 0 here: with no sizing options, before layout it has no constraints.
        panel.setContentSize(NSSize(width: SettingsView.width, height: host.sizeThatFits(in: PanelTopLayout.measuring).height))
        panel.center()
        // Launch at Login and Sparkle's setting can change outside AudioSync.
        let center = NotificationCenter.default
        observers = [
            center.addObserver(forName: NSWindow.didBecomeKeyNotification, object: panel, queue: .main) { [weak self] _ in
                MainActor.assumeIsolated { self?.reread() }
            },
            center.addObserver(forName: NSApplication.didBecomeActiveNotification, object: nil, queue: .main) { [weak self] _ in
                MainActor.assumeIsolated {
                    guard let self, let window = self.window, window.isVisible, !window.isKeyWindow else { return }
                    self.reread()
                }
            },
        ]
        return panel
    }

    /// The content may have changed while the window was closed, when no geometry change reached `fit`.
    private func contentHeight() -> CGFloat {
        host?.sizeThatFits(in: PanelTopLayout.measuring).height ?? 0
    }

    private func reread() {
        model.actions.refreshExternalState {}
        updates.reread()
    }

    /// Fades the window in while its content drops 8 pt and grows from 0.97;
    /// with Reduce Motion, the fade alone.
    private func animateOpen(_ window: NSWindow) {
        let reduce = NSWorkspace.shared.accessibilityDisplayShouldReduceMotion
        window.alphaValue = 0
        motion.atRest = reduce
        if !reduce {
            DispatchQueue.main.async { [motion] in
                MainActor.assumeIsolated {
                    withAnimation(.spring(duration: 0.56, bounce: 0)) { motion.atRest = true }
                }
            }
        }
        NSAnimationContext.runAnimationGroup { context in
            context.duration = reduce ? 0.15 : 0.56
            context.timingFunction = reduce ? CAMediaTimingFunction(name: .linear)
                : CAMediaTimingFunction(controlPoints: 0.2, 0.9, 0.3, 1)
            window.animator().alphaValue = 1
        }
    }

    /// Resizes the window to `height` of content, keeping its top edge where it is.
    private func fit(_ height: CGFloat) {
        guard let window else { return }
        let content = window.contentRect(forFrameRect: window.frame)
        guard abs(content.height - height) > 0.5 else { return }
        var frame = window.frameRect(forContentRect: NSRect(x: 0, y: 0, width: content.width, height: height))
        frame.origin = NSPoint(x: window.frame.minX, y: window.frame.maxY - frame.height)
        guard window.isVisible, !NSWorkspace.shared.accessibilityDisplayShouldReduceMotion else {
            window.setFrame(frame, display: true)
            return
        }
        NSAnimationContext.runAnimationGroup { context in
            context.duration = 0.62
            context.timingFunction = CAMediaTimingFunction(controlPoints: 0.22, 0.61, 0.36, 1)
            window.animator().setFrame(frame, display: true)
        }
    }
}

/// Where the content stands in the open motion. An object rather than
/// @State: the Command Line Tools toolchain ships without SwiftUI's macro plugins.
@MainActor
final class OpenMotion: ObservableObject {
    @Published var atRest = true
}

/// Escape closes Settings, as it does the equalizer and the Web Remote.
private final class SettingsPanel: NSWindow {
    override func cancelOperation(_ sender: Any?) {
        performClose(sender)
    }

    override func performKeyEquivalent(with event: NSEvent) -> Bool {
        closeOnEscape(event) || super.performKeyEquivalent(with: event)
    }
}

/// Sparkle's automatic check as the Updates box shows it. An object rather
/// than @State: the Command Line Tools toolchain ships without SwiftUI's macro plugins.
@MainActor
final class UpdateChecks: ObservableObject {
    @Published private(set) var automatic = false

    func reread() { automatic = AppUpdater.checksAutomatically }

    func set(_ on: Bool) {
        AppUpdater.checksAutomatically = on
        reread()
    }
}

struct SettingsView: View {
    @ObservedObject var model: AppModel
    @ObservedObject var updates: UpdateChecks
    @ObservedObject var motion: OpenMotion
    let setUpPermissions: () -> Void
    /// The content's height, for the window to follow.
    let onHeight: (CGFloat) -> Void
    static let width: CGFloat = 460
    @Environment(\.accessibilityReduceMotion) private var reduceMotion

    var body: some View {
        // Unlike a flexible frame, it reports the content's own height when measured unbounded.
        PanelTopLayout { content }
            .tint(Brand.teal)
    }

    @ViewBuilder private var content: some View {
        let t = model.text
        VStack(alignment: .leading, spacing: 16) {
            SettingsBox(title: t.settingsGeneral) {
                SettingsRow {
                    Text(t.appearance)
                    Spacer(minLength: 12)
                    Picker(t.appearance, selection: Binding(get: { model.appearance },
                                                            set: { model.actions.setAppearance($0) })) {
                        ForEach(AppAppearance.allCases, id: \.self) { Text(t.appearanceName($0)).tag($0) }
                    }
                    .pickerStyle(.segmented)
                    .labelsHidden()
                    .fixedSize()
                }
                Divider()
                SwitchRow(title: t.launchAtLogin, isOn: Binding(get: { model.launchAtLogin },
                                                                set: { model.actions.setLaunchAtLogin($0) }))
            }
            SettingsBox(title: t.menuSpeakers) {
                volumeKeysRow(t)
                Divider()
                SwitchRow(title: t.showVirtualDevices, isOn: Binding(get: { model.showVirtualDevices },
                                                                     set: { model.actions.setShowVirtualDevices($0) }))
                Divider()
                Text(t.hiddenSpeakers)
                    .font(.subheadline)
                    .foregroundStyle(Brand.secondaryText)
                    .padding(.top, 8)
                    .padding(.bottom, 2)
                    .accessibilityAddTraits(.isHeader)
                hiddenRows(t)
            }
            .animation(reduceMotion ? .linear(duration: 0.15) : .spring(duration: 0.62, bounce: 0),
                       value: model.hidden.menu)
            if AppUpdater.isRunning {
                SettingsBox(title: t.settingsUpdates) {
                    SwitchRow(title: t.menuCheckUpdatesAutomatically,
                              isOn: Binding(get: { updates.automatic }, set: { updates.set($0) }))
                    Divider()
                    SettingsRow {
                        Button(t.menuCheckForUpdates) { AppUpdater.checkForUpdates() }
                    }
                }
            }
            SettingsBox(title: t.setupTitle) {
                SettingsRow {
                    Button(t.setupMenu, action: setUpPermissions)
                }
            }
        }
        .padding(20)
        .frame(width: Self.width, alignment: .leading)
        .onGeometryChange(for: CGFloat.self, of: { $0.size.height }, action: onHeight)
        .scaleEffect(motion.atRest ? 1 : 0.97, anchor: .top)
        .offset(y: motion.atRest ? 0 : 8)
    }

    /// The switch, what it does, and while macOS keeps the keys, why and where to change it.
    private func volumeKeysRow(_ t: UIStrings) -> some View {
        VStack(alignment: .leading, spacing: 4) {
            SwitchRow(title: t.volumeKeys, isOn: Binding(get: { model.volumeKeys },
                                                         set: { model.actions.setVolumeKeys($0) }))
            Text(t.volumeKeysOn + " " + t.volumeKeysOff)
                .font(.subheadline)
                .foregroundStyle(Brand.secondaryText)
                .fixedSize(horizontal: false, vertical: true)
            if model.volumeKeysNeedAccess {
                HStack(spacing: 12) {
                    Text(t.volumeKeysNeedAccess)
                        .font(.subheadline)
                        .fixedSize(horizontal: false, vertical: true)
                        .frame(maxWidth: .infinity, alignment: .leading)
                    Button(t.openSettings) { model.actions.openAccessibilitySettings() }
                        .controlSize(.small)
                        .accessibilityLabel(t.setupOpenSettings(.accessibility))
                        .accessibilityHint(t.openSettingsHint)
                        .help(t.openSettingsHint)
                }
                .padding(.top, 4)
            }
        }
        .padding(.bottom, 10)
    }

    @ViewBuilder
    private func hiddenRows(_ t: UIStrings) -> some View {
        if model.hidden.menu.isEmpty {
            SettingsRow {
                Text(t.noHiddenSpeakers).foregroundStyle(Brand.secondaryText)
            }
            .transition(.opacity)
        } else {
            ForEach(Array(model.hidden.menu.enumerated()), id: \.element.key) { index, entry in
                if index > 0 { Divider() }
                SettingsRow {
                    Text(entry.title)
                    Spacer(minLength: 12)
                    Button(t.unhide) { model.unhide(entry) }
                        .accessibilityLabel(t.unhideLabel(entry.title))
                }
                .transition(.opacity)
            }
        }
    }
}

/// One titled box of the Settings window, drawn like the main window's cards.
private struct SettingsBox<Content: View>: View {
    let title: String
    @ViewBuilder let content: Content

    var body: some View {
        VStack(alignment: .leading, spacing: 6) {
            Text(title)
                .font(.callout.weight(.semibold))
                .foregroundStyle(Brand.secondaryText)
                .padding(.leading, 2)
                .accessibilityAddTraits(.isHeader)
            VStack(alignment: .leading, spacing: 0) { content }
                .padding(.horizontal, 12)
                .frame(maxWidth: .infinity, alignment: .leading)
                .background(RoundedRectangle(cornerRadius: BrandTokens.Radius.card)
                    .fill(Color(nsColor: .controlBackgroundColor)))
                .overlay(RoundedRectangle(cornerRadius: BrandTokens.Radius.card)
                    .strokeBorder(Color(nsColor: .separatorColor), lineWidth: 0.5))
        }
    }
}

private struct SettingsRow<Content: View>: View {
    @ViewBuilder let content: Content

    var body: some View {
        HStack(spacing: 0) { content }
            .frame(maxWidth: .infinity, minHeight: 40, alignment: .leading)
            .padding(.vertical, 4)
    }
}

/// A switch at the row's trailing edge, its title filling the rest.
private struct SwitchRow: View {
    let title: String
    let isOn: Binding<Bool>

    var body: some View {
        SettingsRow {
            Toggle(isOn: isOn) {
                Text(title)
                    .fixedSize(horizontal: false, vertical: true)
                    .frame(maxWidth: .infinity, alignment: .leading)
            }
            .toggleStyle(.switch)
        }
    }
}
