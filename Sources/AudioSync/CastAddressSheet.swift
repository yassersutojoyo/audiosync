import SwiftUI

/// Adds a Google Cast speaker by address when discovery can't find it (C18).
/// Return adds and Esc cancels. An address that doesn't read, or one where
/// no speaker answers, keeps the sheet open with the reason under the field.
final class CastAddressDraft: ObservableObject {
    @Published var typed = ""
    @Published var problem: CastAddProblem?
    /// The address is being looked up and tried; Add waits for it.
    @Published var adding = false
}

struct CastAddressSheet: View {
    @ObservedObject var model: AppModel
    /// An object rather than @State: the Command Line Tools can't expand @State.
    @StateObject private var draft = CastAddressDraft()
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
                TextField(t.castAddField, text: $draft.typed)
                    .labelsHidden()
                    .textFieldStyle(.roundedBorder)
                    .autocorrectionDisabled()
                    .focused($fieldFocused)
                    .disabled(draft.adding)
                    .onChange(of: draft.typed) { draft.problem = nil }
                    .onSubmit(add)
            }
            if let problem = draft.problem {
                Text(t.castAddProblem(problem))
                    .font(.callout)
                    .foregroundStyle(Brand.errorText)
                    .fixedSize(horizontal: false, vertical: true)
            }
            HStack(spacing: 8) {
                if draft.adding { ProgressView().controlSize(.small).accessibilityLabel(t.castAddChecking) }
                Spacer(minLength: 0)
                Button(t.cancel) { model.castAddShown = false }
                    .keyboardShortcut(.cancelAction)
                Button(draft.adding ? t.castAddChecking : t.castAdd, action: add)
                    .keyboardShortcut(.defaultAction)
                    .disabled(draft.adding || draft.typed.trimmingCharacters(in: .whitespaces).isEmpty)
            }
        }
        .padding(20)
        .frame(width: 420, alignment: .leading)
        .onAppear { fieldFocused = true }
        .onDisappear { model.castAddGeneration += 1 }
    }

    /// Shows the reason and has VoiceOver read it, the same one again too.
    private func report(_ found: CastAddProblem) {
        draft.problem = found
        AccessibilityNotification.Announcement(model.text.castAddProblem(found)).post()
    }

    private func add() {
        guard !draft.adding else { return }
        draft.problem = nil
        guard CastManualAddress(draft.typed) != nil else {
            report(.unreadable)
            return
        }
        draft.adding = true
        let generation = model.castAddGeneration
        model.actions.addCastAddress(draft.typed) { found in
            guard model.castAddGeneration == generation else { return }
            draft.adding = false
            if let found {
                report(found)
                fieldFocused = true
            } else {
                model.castAddShown = false
            }
        }
    }
}
