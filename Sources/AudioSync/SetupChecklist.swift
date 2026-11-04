import AppKit
import SwiftUI

/// The setup checklist card above the main window's columns.
struct SetupChecklistCard: View {
    @ObservedObject var model: AppModel
    @Environment(\.accessibilityReduceMotion) private var reduceMotion

    var body: some View {
        let t = model.text
        let inputs = model.setupChecks
        let notWorking = SetupChecklist.captureNotWorking(model.notices)
        let states = SetupCheck.allCases.map { SetupChecklist.state($0, inputs, captureNotWorking: notWorking) }
        let allOK = states.allSatisfy { $0 == .ok }
        VStack(alignment: .leading, spacing: 10) {
            HStack(alignment: .firstTextBaseline) {
                Text(allOK ? t.setupChecklistDone : t.setupChecklistHeading)
                    .font(.headline)
                    .accessibilityAddTraits(.isHeader)
                Spacer(minLength: 8)
                if allOK {
                    Button(t.setupChecklistDismiss) { model.actions.dismissSetupChecklist() }
                        .controlSize(.small)
                        .transition(.opacity)
                }
            }
            ForEach(Array(SetupCheck.allCases.enumerated()), id: \.element) { index, check in
                if index > 0 { Divider() }
                row(check, state: states[index], inputs: inputs, t)
            }
        }
        .padding(BrandTokens.Spacing.cardPadding)
        .frame(maxWidth: .infinity, alignment: .leading)
        .background(RoundedRectangle(cornerRadius: BrandTokens.Radius.card)
            .fill(Color(nsColor: .controlBackgroundColor)))
        .overlay(RoundedRectangle(cornerRadius: BrandTokens.Radius.card)
            .strokeBorder(Color(nsColor: .separatorColor), lineWidth: 0.5))
        .animation(reduceMotion ? nil : .easeOut(duration: 0.2), value: states)
    }

    private func row(_ check: SetupCheck, state: SetupCheckState, inputs: SetupCheckInputs,
                     _ t: UIStrings) -> some View {
        HStack(alignment: .top, spacing: 10) {
            Image(systemName: Self.symbol(state))
                .font(.body.weight(.semibold))
                .foregroundStyle(Self.color(state))
                .frame(width: 20)
                // A changed row's icon swaps with a short scale so the eye finds it; Reduce Motion keeps the fade.
                .id(state)
                .transition(reduceMotion ? AnyTransition.opacity
                                         : AnyTransition.scale(scale: 0.6).combined(with: .opacity))
                .accessibilityHidden(true)
            VStack(alignment: .leading, spacing: 2) {
                Text(t.setupCheckTitle(check, inputs.capturePermission))
                    .font(.body.weight(.medium))
                Text(t.setupCheckState(check, state, inputs))
                    .font(.callout)
                    .foregroundStyle(state == .problem ? Brand.warn : Brand.secondaryText)
                if let line = t.setupCheckLine(check, state, inputs) {
                    Text(line)
                        .font(.callout)
                        .foregroundStyle(Brand.secondaryText)
                        .fixedSize(horizontal: false, vertical: true)
                }
            }
            .accessibilityElement(children: .combine)
            Spacer(minLength: 8)
            if state == .problem, let pane = SetupChecklist.pane(check, inputs) {
                Button(t.openSettings) { NSWorkspace.shared.open(pane.url) }
                    .controlSize(.small)
                    .accessibilityLabel(t.setupCheckSettingsLabel(check, inputs.capturePermission))
                    .accessibilityHint(t.openSettingsHint)
                    .help(t.openSettingsHint)
            }
        }
    }

    private static func symbol(_ state: SetupCheckState) -> String {
        switch state {
        case .checking: return "circle.dotted"
        case .ok: return "checkmark.circle.fill"
        case .problem: return "exclamationmark.triangle.fill"
        }
    }

    /// The state line says it in words; the colour only repeats it.
    private static func color(_ state: SetupCheckState) -> Color {
        switch state {
        case .checking: return .secondary
        case .ok: return Brand.tealText
        case .problem: return Brand.warn
        }
    }
}
