import AppKit
import SwiftUI

/// Under a local speaker's delay controls: "Calibrate with Microphone"
/// (S232), what the measurement is doing with Cancel, and then the new
/// delay or why nothing changed.
struct CalibrationControl: View {
    @ObservedObject var model: AppModel
    let id: String
    let name: String

    var body: some View {
        let t = model.text
        let phase = model.calibration.phase(of: id)
        let elsewhere = model.calibration.isRunning && model.calibration.uid != id
        VStack(alignment: .leading, spacing: 4) {
            HStack(spacing: 8) {
                if case .running(let step) = phase {
                    ProgressView()
                        .controlSize(.small)
                        .accessibilityHidden(true)
                    Text(t.calibrationStep(step))
                        .font(.caption)
                        .foregroundStyle(Brand.secondaryText)
                    Button(t.calibrationCancel) { model.actions.cancelCalibration() }
                        .controlSize(.small)
                        .accessibilityLabel(t.calibrationCancelLabel(of: name))
                } else {
                    Button(t.calibrateWithMicrophone) { model.actions.calibrate(id) }
                        .controlSize(.small)
                        .disabled(elsewhere)
                        .help(t.calibrationHint)
                        .accessibilityLabel(t.calibrateLabel(of: name))
                        .accessibilityHint(t.calibrationHint)
                }
            }
            if let message = t.calibrationMessage(phase) {
                Text(message)
                    .font(.caption)
                    .foregroundStyle(isFailure(phase) ? Brand.errorText : Brand.secondaryText)
                    .fixedSize(horizontal: false, vertical: true)
            }
            if case .failed(.microphoneDenied) = phase {
                Button(t.openMicrophoneSettings) { NSWorkspace.shared.open(PrivacyPane.microphone.url) }
                    .controlSize(.small)
            }
        }
        .onChange(of: phase) { _, now in
            if let message = t.calibrationMessage(now) {
                AccessibilityNotification.Announcement(message).post()
            }
        }
    }

    private func isFailure(_ phase: CalibrationPhase) -> Bool {
        if case .failed = phase { return true }
        return false
    }
}
