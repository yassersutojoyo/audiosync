import SwiftUI

/// Adds a Google Cast speaker by address when discovery can't find it (C18).
/// Return adds and Esc cancels. An address that doesn't read, or one where
/// no speaker answers, keeps the sheet open with the reason under the field.
struct CastAddressSheet: View {
    @ObservedObject var model: AppModel
    @State private var typed = ""
    @State private var problem: CastAddProblem?
    /// The address is being looked up and tried; Add waits for it.
    @State private var adding = false
    @FocusState private var fieldFocused: Bool

    var body: some View {
        let t = model.text
        VStack(alignment: .leading, spacing: 12) {
            Text(t.castAddTitle)
                .font(.title3.weight(.semibold))
                .accessibilityAddTraits(.isHeader)
            Text(t.castAddBody)
                .font(.callout)
                .foregroundStyle(Brand.secondaryText)
                .fixedSize(horizontal: false, vertical: true)
            LabeledContent(t.castAddField) {
                TextField(t.castAddField, text: $typed)
                    .labelsHidden()
                    .textFieldStyle(.roundedBorder)
                    .autocorrectionDisabled()
                    .focused($fieldFocused)
                    .disabled(adding)
                    .onChange(of: typed) { problem = nil }
                    .onSubmit(add)
            }
            if let problem {
                Text(t.castAddProblem(problem))
                    .font(.callout)
                    .foregroundStyle(Brand.errorText)
                    .fixedSize(horizontal: false, vertical: true)
            }
            HStack(spacing: 8) {
                if adding { ProgressView().controlSize(.small).accessibilityHidden(true) }
                Spacer(minLength: 0)
                Button(t.cancel) { model.castAddShown = false }
                    .keyboardShortcut(.cancelAction)
                Button(t.castAdd, action: add)
                    .keyboardShortcut(.defaultAction)
                    .disabled(adding || typed.trimmingCharacters(in: .whitespaces).isEmpty)
            }
        }
        .padding(20)
        .frame(width: 420, alignment: .leading)
        .onAppear { fieldFocused = true }
        .onChange(of: problem) {
            if let problem { AccessibilityNotification.Announcement(t.castAddProblem(problem)).post() }
        }
    }

    private func add() {
        guard !adding else { return }
        guard CastManualAddress(typed) != nil else {
            problem = .unreadable
            return
        }
        adding = true
        model.actions.addCastAddress(typed) { found in
            adding = false
            if let found {
                problem = found
                fieldFocused = true
            } else {
                model.castAddShown = false
            }
        }
    }
}
