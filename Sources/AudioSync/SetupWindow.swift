import AppKit
import SwiftUI

/// What the permission setup window shows; `AppDelegate.refreshSetup` fills it.
final class PermissionSetupModel: ObservableObject {
    struct Actions {
        var allow: () -> Void = {}
        var allowMicrophone: () -> Void = {}
        var openSettings: (Permission) -> Void = { _ in }
        var close: () -> Void = {}
    }

    @Published var rows: [Permission: SetupRowState] = [:] {
        didSet { flash(from: oldValue) }
    }
    /// Each player's Automation answer, in `PermissionSetup.players` order.
    @Published var players: [AutomationConsent.Verdict] = []
    @Published var volumeKeys = true
    /// Opened from Check Setup…: a summary in place of the first install's
    /// explanation, and Done in place of Skip for Now.
    @Published var checking = false
    /// Allow is asking for Local Network and waits before the next one.
    @Published var asking = false
    /// Rows that just turned allowed, highlighted for a moment.
    @Published private(set) var flashing: Set<Permission> = []
    var actions = Actions()
    let text: UIStrings

    init(text: UIStrings) { self.text = text }

    var finished: Bool { PermissionSetup.next(rows) == nil }
    var summary: SetupSummary { PermissionSetup.summary(rows, volumeKeys: volumeKeys) }

    private func flash(from old: [Permission: SetupRowState]) {
        let lit = Set(Permission.allCases.filter { old[$0] != nil && old[$0] != .allowed && rows[$0] == .allowed })
        guard !lit.isEmpty else { return }
        flashing.formUnion(lit)
        DispatchQueue.main.asyncAfter(deadline: .now() + 0.15) { [weak self] in self?.flashing.subtract(lit) }
    }
}

/// The permission setup in a window of its own, like the Web Remote: opened
/// by itself once on a first install, and from Check Setup….
final class SetupWindowController: NSObject, NSWindowDelegate {
    let model: PermissionSetupModel
    private var window: NSWindow?
    /// Called when the window closes, by Skip, Done or its close button.
    var onClose: () -> Void = {}
    /// Called when the window comes to the front, maybe back from System Settings.
    var onBecomeKey: () -> Void = {}
    private(set) var isOpen = false

    init(model: PermissionSetupModel) {
        self.model = model
    }

    func show() {
        if window == nil {
            let host = NSHostingController(rootView: PermissionSetupView(model: model))
            let panel = NSWindow(contentViewController: host)
            panel.title = model.text.setupWindowTitle
            panel.styleMask = [.titled, .closable]
            panel.isReleasedWhenClosed = false
            panel.delegate = self
            panel.center()
            window = panel
        }
        isOpen = true
        NSApp.activate(ignoringOtherApps: true)
        window?.makeKeyAndOrderFront(nil)
    }

    func close() { window?.performClose(nil) }

    func windowDidBecomeKey(_ notification: Notification) { onBecomeKey() }

    func windowWillClose(_ notification: Notification) {
        isOpen = false
        onClose()
    }
}

struct PermissionSetupView: View {
    @ObservedObject var model: PermissionSetupModel
    @Environment(\.accessibilityReduceMotion) private var reduceMotion

    var body: some View {
        let t = model.text
        let summary = model.summary
        VStack(alignment: .leading, spacing: 16) {
            VStack(alignment: .leading, spacing: 6) {
                Text(model.checking ? t.setupSummaryHeading(summary) : t.setupHeading)
                    .font(.title2.weight(.semibold))
                    .accessibilityAddTraits(.isHeader)
                Text(model.checking ? t.setupSummaryLine(summary) : t.setupIntro)
                    .foregroundStyle(Brand.secondaryText)
                    .fixedSize(horizontal: false, vertical: true)
            }
            group(t.setupNeeded, PermissionSetup.required)
            group(t.setupLater, PermissionSetup.optional)
            HStack(spacing: 8) {
                Spacer()
                if !model.finished {
                    Button(model.checking ? t.setupDone : t.setupSkip) { model.actions.close() }
                        .keyboardShortcut(.cancelAction)
                }
                Button(primaryTitle) { model.finished ? model.actions.close() : model.actions.allow() }
                    .keyboardShortcut(.defaultAction)
                    .buttonStyle(.borderedProminent)
                    .disabled(model.asking)
            }
        }
        .padding(20)
        .frame(width: 480)
        .animation(reduceMotion ? nil : .easeInOut(duration: 0.25), value: model.volumeKeys)
        .onChange(of: summaryText) { _, line in
            if !line.isEmpty { AccessibilityNotification.Announcement(line).post() }
        }
    }

    /// The heading and line from Check Setup…, announced when they change.
    private var summaryText: String {
        let t = model.text
        let summary = model.summary
        return model.checking ? "\(t.setupSummaryHeading(summary)). \(t.setupSummaryLine(summary))" : ""
    }

    private var primaryTitle: String {
        let t = model.text
        if model.asking { return t.setupAsking }
        return model.finished ? t.setupDone : t.setupAllow
    }

    private func group(_ title: String, _ permissions: [Permission]) -> some View {
        VStack(alignment: .leading, spacing: 8) {
            Text(title)
                .font(.headline)
                .accessibilityAddTraits(.isHeader)
            rowGroup(permissions.filter { PermissionSetup.shows($0, volumeKeys: model.volumeKeys) })
        }
    }

    private func rowGroup(_ permissions: [Permission]) -> some View {
        VStack(spacing: 0) {
            ForEach(Array(permissions.enumerated()), id: \.element) { index, permission in
                if index > 0 { Divider().padding(.leading, 46) }
                row(permission)
            }
        }
        .background(RoundedRectangle(cornerRadius: 8).fill(Color(nsColor: .controlBackgroundColor)))
        .clipShape(RoundedRectangle(cornerRadius: 8))
        .overlay(RoundedRectangle(cornerRadius: 8).strokeBorder(Color(nsColor: .separatorColor)))
    }

    private func row(_ permission: Permission) -> some View {
        let t = model.text
        let state = model.rows[permission]
        let lit = model.flashing.contains(permission) && !reduceMotion
        return HStack(alignment: .top, spacing: 10) {
            Image(systemName: Self.symbol(permission))
                .font(.title3)
                .foregroundStyle(Brand.tealText)
                .frame(width: 24)
                .accessibilityHidden(true)
            VStack(alignment: .leading, spacing: 3) {
                Text(t.permissionTitle(permission))
                    .font(.body.weight(.semibold))
                Text(t.setupDetail(permission))
                    .font(.callout)
                    .foregroundStyle(Brand.secondaryText)
                    .fixedSize(horizontal: false, vertical: true)
                if let state {
                    HStack(alignment: .firstTextBaseline, spacing: 6) {
                        statusIcon(state)
                        Text(permission == .automation
                             ? t.setupAutomation(PermissionSetup.players.map(t.setupPlayer), model.players) : t.setupState(permission, state))
                            .font(.callout.weight(.medium))
                            .foregroundStyle(Self.color(state))
                            .fixedSize(horizontal: false, vertical: true)
                    }
                    .animation(reduceMotion ? nil : .default, value: state)
                }
            }
            .accessibilityElement(children: .combine)
            Spacer(minLength: 8)
            if let state, PermissionSetup.offersSettings(state) {
                Button(t.openSettings) { model.actions.openSettings(permission) }
                    .controlSize(.small)
                    .accessibilityLabel(t.setupOpenSettings(permission))
                    .accessibilityHint(t.openSettingsHint)
                    .help(t.openSettingsHint)
            } else if permission == .microphone, state == .notAsked {
                Button(t.setupAllowOne) { model.actions.allowMicrophone() }
                    .controlSize(.small)
                    .accessibilityLabel(t.setupAllowMicrophone)
            }
        }
        .padding(12)
        .background(Brand.teal.opacity(lit ? 0.14 : 0))
        // On at once, then a fade of about a second.
        .animation(lit ? nil : .easeOut(duration: 0.9), value: lit)
    }

    /// Repeats the state line's words, so VoiceOver skips it.
    @ViewBuilder
    private func statusIcon(_ state: SetupRowState) -> some View {
        if state == .waiting {
            ProgressView()
                .controlSize(.mini)
                .accessibilityHidden(true)
        } else {
            Image(systemName: Self.statusSymbol(state))
                .foregroundStyle(Self.color(state))
                .contentTransition(.symbolEffect(.replace))
                .accessibilityHidden(true)
        }
    }

    private static func statusSymbol(_ state: SetupRowState) -> String {
        switch state {
        case .allowed: return "checkmark.circle.fill"
        case .denied: return "xmark.circle"
        case .askedUnknown: return "info.circle"
        case .notAsked, .waiting: return "circle.dashed"
        }
    }

    private static func symbol(_ permission: Permission) -> String {
        switch permission {
        case .localNetwork: return "network"
        case .audioCapture: return "waveform"
        case .microphone: return "mic"
        case .automation: return "music.note"
        case .accessibility: return "keyboard"
        }
    }

    /// The state line carries its meaning in words; the colour only repeats it.
    private static func color(_ state: SetupRowState) -> Color {
        switch state {
        case .allowed: return Brand.tealText
        case .denied: return Brand.errorText
        case .notAsked, .waiting, .askedUnknown: return Brand.secondaryText
        }
    }
}
