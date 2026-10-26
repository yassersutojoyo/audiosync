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

    /// Every row but Automation, which macOS asks for per player later.
    @Published var rows: [Permission: SetupRowState] = [:]
    /// Allow is asking for Local Network and waits before the next one.
    @Published var asking = false
    var actions = Actions()
    let text: UIStrings

    init(text: UIStrings) { self.text = text }

    var finished: Bool { PermissionSetup.next(rows) == nil }
}

/// The permission setup in a window of its own, like the Web Remote: opened
/// by itself once on a first install, and from Set Up Permissions….
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
            panel.title = model.text.setupTitle
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

    var body: some View {
        let t = model.text
        VStack(alignment: .leading, spacing: 16) {
            VStack(alignment: .leading, spacing: 6) {
                Text(t.setupHeading)
                    .font(.title2.weight(.semibold))
                    .accessibilityAddTraits(.isHeader)
                Text(t.setupIntro)
                    .foregroundStyle(Brand.secondaryText)
                    .fixedSize(horizontal: false, vertical: true)
            }
            rowGroup(PermissionSetup.required)
            VStack(alignment: .leading, spacing: 8) {
                Text(t.setupLater)
                    .font(.headline)
                    .accessibilityAddTraits(.isHeader)
                rowGroup(PermissionSetup.optional)
            }
            HStack(spacing: 8) {
                Spacer()
                if !model.finished {
                    Button(t.setupSkip) { model.actions.close() }
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
    }

    private var primaryTitle: String {
        let t = model.text
        if model.asking { return t.setupAsking }
        return model.finished ? t.setupDone : t.setupAllow
    }

    private func rowGroup(_ permissions: [Permission]) -> some View {
        VStack(spacing: 0) {
            ForEach(Array(permissions.enumerated()), id: \.element) { index, permission in
                if index > 0 { Divider().padding(.leading, 46) }
                row(permission)
            }
        }
        .background(RoundedRectangle(cornerRadius: 8).fill(Color(nsColor: .controlBackgroundColor)))
        .overlay(RoundedRectangle(cornerRadius: 8).strokeBorder(Color(nsColor: .separatorColor)))
    }

    private func row(_ permission: Permission) -> some View {
        let t = model.text
        let state = model.rows[permission]
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
                    Text(t.setupState(state))
                        .font(.callout.weight(.medium))
                        .foregroundStyle(Self.color(state))
                        .fixedSize(horizontal: false, vertical: true)
                }
            }
            .accessibilityElement(children: .combine)
            Spacer(minLength: 8)
            if let state, PermissionSetup.offersSettings(state) {
                Button(t.openSettings) { model.actions.openSettings(permission) }
                    .controlSize(.small)
                    .accessibilityHint(t.openSettingsHint)
                    .help(t.openSettingsHint)
            } else if permission == .microphone, state == .notAsked {
                Button(t.setupAllowOne) { model.actions.allowMicrophone() }
                    .controlSize(.small)
                    .accessibilityHint(t.permissionTitle(permission))
            }
        }
        .padding(12)
    }

    private static func symbol(_ permission: Permission) -> String {
        switch permission {
        case .localNetwork: return "network"
        case .audioCapture: return "waveform"
        case .microphone: return "mic"
        case .automation: return "music.note"
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
