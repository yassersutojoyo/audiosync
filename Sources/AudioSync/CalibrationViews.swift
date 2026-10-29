import AppKit
import SwiftUI

/// Under a local speaker's delay controls: "Calibrate with Microphone"
/// (S232), what the measurement is doing with Cancel, and then the new
/// delay or why nothing changed.
struct CalibrationControl: View {
    @ObservedObject var model: AppModel
    let id: String
    let name: String
    /// The speaker's switch is on.
    let speakerOn: Bool
    /// Headphones or a virtual output: the reason replaces the button.
    var leftOut: CalibrationLeftOut? = nil

    var body: some View {
        if let leftOut, !model.calibration.phase(of: id).isRunning {
            Text(model.text.calibrationLeftOut(leftOut))
                .font(.caption)
                .foregroundStyle(Brand.secondaryText)
                .fixedSize(horizontal: false, vertical: true)
        } else {
            control
        }
    }

    @ViewBuilder private var control: some View {
        let t = model.text
        let phase = model.calibration.phase(of: id)
        let running = phase.isRunning
        let unavailable = CalibrationFlow.unavailable(streaming: model.running, hasReference: model.hasLocalReference,
                                                      speakerOn: speakerOn)
        // Another speaker's run, or a cut-off sweep still playing out, holds the outputs.
        let held = (model.calibration.holdsOutputs || model.calibrateAll.holdsOutputs) && !running
        let hint = t.calibrationButtonHint(running: running, unavailable: unavailable, held: held)
        VStack(alignment: .leading, spacing: 4) {
            HStack(spacing: 8) {
                if case .running(let step) = phase {
                    ProgressView()
                        .controlSize(.small)
                        .accessibilityHidden(true)
                    Text(t.calibrationStep(step))
                        .font(.caption)
                        .foregroundStyle(Brand.secondaryText)
                }
                // One button that turns into Cancel and back, so the keyboard
                // focus stays on it while the run starts and ends.
                Button(running ? t.calibrationCancel : t.calibrateWithMicrophone) {
                    if running { model.actions.cancelCalibration() } else { model.actions.calibrate(id) }
                }
                .controlSize(.small)
                .disabled(!running && (unavailable != nil || held))
                .help(hint)
                .accessibilityLabel(running ? t.calibrationCancelLabel(of: name) : t.calibrateLabel(of: name))
                .accessibilityHint(hint)
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
