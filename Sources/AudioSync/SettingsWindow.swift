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
        let view = SettingsView(model: model, updates: updates,
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
                // Its answer re-renders the content, so it waits out the open motion's first frames.
                DispatchQueue.main.asyncAfter(deadline: .now() + WindowOpen.seconds) {
                    MainActor.assumeIsolated { self?.reread() }
                }
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

    /// Plays `WindowOpen` on the content view's layer. The render server moves it as one picture
    /// at display rate; a SwiftUI scale over the tree rounded each AppKit control apart from the text.
    private func animateOpen(_ window: NSWindow) {
        guard let view = window.contentView else { return }
        view.wantsLayer = true
        guard let layer = view.layer else { return }
        let reduce = NSWorkspace.shared.accessibilityDisplayShouldReduceMotion
        var parts = [Self.opening("opacity", from: 0, reduce: reduce)]
        if !reduce {
            let start = WindowOpen.start(frame: layer.frame, position: layer.position,
                                         flipped: view.superview?.isFlipped ?? false)
            parts.append(Self.opening("transform", from: NSValue(caTransform3D: CATransform3DMakeAffineTransform(start)),
                                      reduce: false))
        }
        let group = CAAnimationGroup()
        group.animations = parts
        group.duration = parts.map(\.duration).max() ?? 0
        layer.add(group, forKey: "open")
    }

    /// One part of the open motion, from `from` to the layer's resting value.
    private static func opening(_ keyPath: String, from: Any, reduce: Bool) -> CABasicAnimation {
        let animation: CABasicAnimation
        if reduce {
            animation = CABasicAnimation(keyPath: keyPath)
            animation.duration = WindowOpen.fadeSeconds
        } else {
            let spring = CASpringAnimation(perceptualDuration: WindowOpen.seconds, bounce: 0)
            spring.keyPath = keyPath
            spring.duration = spring.settlingDuration
            animation = spring
        }
        animation.fromValue = from
        return animation
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
    let setUpPermissions: () -> Void
    /// The content's height, for the window to follow.
    let onHeight: (CGFloat) -> Void
    static let width: CGFloat = 460
    @Environment(\.accessibilityReduceMotion) private var reduceMotion
    /// The idle minutes as typed. An object rather than @State: the Command
    /// Line Tools toolchain ships without SwiftUI's macro plugins.
    @StateObject private var minutes = MinutesDraft()
    @FocusState private var minutesFocused: Bool

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
                        .layarKey(t.appearance)
                    Spacer(minLength: 12)
                    Picker(t.appearance, selection: Binding(get: { model.appearance },
                                                            set: { model.actions.setAppearance($0) })) {
                        ForEach(AppAppearance.allCases, id: \.self) { Text(t.appearanceName($0)).tag($0) }
                    }
                    .pickerStyle(.segmented)
                    .labelsHidden()
                    .fixedSize()
                    .layarKey(t.appearance + ", " + t.appearanceName(model.appearance))
                }
                Divider()
                SwitchRow(title: t.launchAtLogin, isOn: Binding(get: { model.launchAtLogin },
                                                                set: { model.actions.setLaunchAtLogin($0) }))
                Divider()
                dockIconRow(t)
                Divider()
                resumeAfterSleepRow(t)
            }
            SettingsBox(title: t.menuSpeakers) {
                volumeKeysRow(t)
                Divider()
                SwitchRow(title: t.showVirtualDevices, isOn: Binding(get: { model.showVirtualDevices },
                                                                     set: { model.actions.setShowVirtualDevices($0) }))
                Divider()
                releaseRow(t)
                Divider()
                Text(t.hiddenSpeakers)
                    .font(.subheadline)
                    .foregroundStyle(Brand.secondaryText)
                    .padding(.top, 8)
                    .padding(.bottom, 2)
                    .accessibilityAddTraits(.isHeader)
                    .layarKey(t.hiddenSpeakers)
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
                            .buttonStyle(HouseButtonStyle())
                            .layarKey(t.menuCheckForUpdates)
                    }
                }
            }
            SettingsBox(title: t.setupTitle) {
                SettingsRow {
                    Button(t.setupMenu, action: setUpPermissions)
                        .buttonStyle(HouseButtonStyle())
                        .layarKey(t.setupMenu)
                }
            }
        }
        .padding(20)
        .frame(width: Self.width, alignment: .leading)
        .onGeometryChange(for: CGFloat.self, of: { $0.size.height }, action: onHeight)
    }

    private func dockIconRow(_ t: UIStrings) -> some View {
        VStack(alignment: .leading, spacing: 4) {
            SwitchRow(title: t.hideDockIcon, isOn: Binding(get: { model.hidesDockIcon },
                                                           set: { model.actions.setHidesDockIcon($0) }))
            Text(t.hideDockIconOn + " " + t.hideDockIconOff)
                .font(.subheadline)
                .foregroundStyle(Brand.secondaryText)
                .fixedSize(horizontal: false, vertical: true)
        }
        .padding(.bottom, 10)
    }

    private func resumeAfterSleepRow(_ t: UIStrings) -> some View {
        VStack(alignment: .leading, spacing: 4) {
            SwitchRow(title: t.resumeAfterSleep, isOn: Binding(get: { model.resumesAfterSleep },
                                                               set: { model.actions.setResumesAfterSleep($0) }))
            Text(t.resumeAfterSleepInfo)
                .font(.subheadline)
                .foregroundStyle(Brand.secondaryText)
                .fixedSize(horizontal: false, vertical: true)
        }
        .padding(.bottom, 10)
    }

    /// Auto-disconnect AirPlay and Cast, and while it is on, after how many minutes of silence (S483).
    private func releaseRow(_ t: UIStrings) -> some View {
        VStack(alignment: .leading, spacing: 4) {
            SwitchRow(title: t.releaseWhenQuiet,
                      isOn: Binding(get: { model.releaseWhenQuiet },
                                    set: { model.actions.setReleaseWhenQuiet($0, model.releaseMinutes) }))
            Text(t.releaseWhenQuietInfo)
                .font(.subheadline)
                .foregroundStyle(Brand.secondaryText)
                .fixedSize(horizontal: false, vertical: true)
            if model.releaseWhenQuiet { releaseMinutesField(t).padding(.top, 6) }
        }
        .padding(.bottom, 10)
    }

    /// Enter or leaving the field applies it; only a whole number from 1 to 240 does, and Escape puts it back.
    private func releaseMinutesField(_ t: UIStrings) -> some View {
        VStack(alignment: .leading, spacing: 4) {
            HStack(spacing: 8) {
                Text(t.releaseAfter).accessibilityHidden(true)
                TextField(t.releaseMinutesLabel, text: $minutes.text)
                    .focused($minutesFocused)
                    .multilineTextAlignment(.trailing)
                    .modifier(HouseFieldLook(focused: minutesFocused))
                    .frame(width: 56)
                    .accessibilityLabel(t.releaseMinutesLabel)
                    .layarKey(t.releaseMinutesLabel)
                    .onSubmit { applyMinutes() }
                    .onExitCommand {
                        minutes.text = String(model.releaseMinutes)
                        minutes.invalid = false
                    }
                Text(t.releaseMinutesUnit).accessibilityHidden(true)
            }
            if minutes.invalid {
                Text(t.releaseMinutesRange)
                    .font(.caption)
                    .foregroundStyle(Brand.errorText)
            }
        }
        .onAppear {
            minutes.text = String(model.releaseMinutes)
            minutes.invalid = false
        }
        .onChange(of: minutesFocused) { _, focused in if !focused { applyMinutes() } }
    }

    private func applyMinutes() {
        // Leaving a field just put back keeps its range message.
        guard minutes.text != String(model.releaseMinutes) else { return }
        let typed = AppSettings.releaseMinutes(typed: minutes.text)
        minutes.invalid = typed == nil
        if let typed { model.actions.setReleaseWhenQuiet(true, typed) } else { minutes.text = String(model.releaseMinutes) }
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
                        .buttonStyle(HouseButtonStyle(small: true))
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
                    .layarKey(t.noHiddenSpeakers)
            }
            .transition(.opacity)
        } else {
            ForEach(Array(model.hidden.menu.enumerated()), id: \.element.key) { index, entry in
                if index > 0 { Divider() }
                SettingsRow {
                    Text(entry.title)
                    Spacer(minLength: 12)
                    Button(t.unhide) { model.unhide(entry) }
                        .buttonStyle(HouseButtonStyle(small: true))
                        .accessibilityLabel(t.unhideLabel(entry.title))
                }
                .transition(.opacity)
            }
        }
    }
}

/// The Auto-disconnect minutes while they are typed, and whether the last try was out of range.
final class MinutesDraft: ObservableObject {
    @Published var text = ""
    @Published var invalid = false
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
                .layarKey(title)
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
            .layarKey(title)
        }
    }
}
