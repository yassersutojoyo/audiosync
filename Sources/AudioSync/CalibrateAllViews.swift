import AppKit
import SwiftUI

/// The left column's "Calibrate All Speakers" row (S235): opens the sheet
/// and starts the run, or says why it can't.
struct CalibrateAllButton: View {
    @ObservedObject var model: AppModel

    var body: some View {
        let t = model.text
        let blocker = model.calibration.holdsOutputs ? CalibrateAllBlocker.busy : model.calibrateAll.blocker
        VStack(alignment: .leading, spacing: 6) {
            Button { model.actions.calibrateAll() } label: {
                HStack(spacing: 10) {
                    Image(systemName: "mic")
                        .font(.system(size: 15))
                        .foregroundStyle(blocker == nil ? Brand.teal : Color.secondary)
                        .frame(width: 20)
                        .accessibilityHidden(true)
                    Text(t.calibrateAll).font(.body)
                    Spacer()
                    Image(systemName: "chevron.right")
                        .font(.caption.weight(.semibold))
                        .foregroundStyle(.tertiary)
                        .accessibilityHidden(true)
                }
                .contentShape(Rectangle())
            }
            .buttonStyle(.plain)
            // With only headphones or virtual outputs on, the sheet still opens to include one anyway.
            .disabled(blocker != nil && blocker != .onlyLeftOut)
            .help(blocker.map(t.calibrateAllUnavailable) ?? t.calibrateAllHint)
            .accessibilityLabel(t.calibrateAll)
            .accessibilityHint(blocker.map(t.calibrateAllUnavailable) ?? t.calibrateAllHint)
            // Outside the button, so a disabled button doesn't dim the reason.
            if let blocker {
                Text(t.calibrateAllUnavailable(blocker))
                    .font(.caption)
                    .foregroundStyle(Brand.secondaryText)
                    .fixedSize(horizontal: false, vertical: true)
                    .accessibilityHidden(true)
            }
        }
    }
}

/// The Calibrate All Speakers sheet: the run's progress with Cancel, then
/// every speaker's current and new delay, the shared AirPlay delay, Apply,
/// Undo and Measure Again (S238, S239).
struct CalibrateAllSheetView: View {
    @ObservedObject var model: AppModel

    var body: some View {
        let t = model.text
        let status = model.calibrateAll
        VStack(alignment: .leading, spacing: 14) {
            Text(t.calibrateAll)
                .font(.title3.weight(.semibold))
                .accessibilityAddTraits(.isHeader)
            if status.sheet == nil {
                Text(t.calibrateAllIntro)
                    .foregroundStyle(Brand.secondaryText)
                    .fixedSize(horizontal: false, vertical: true)
            }
            if let progress = status.progress {
                HStack(spacing: 8) {
                    ProgressView()
                        .controlSize(.small)
                        .accessibilityHidden(true)
                    Text(t.calibrateAllProgress(progress))
                        .fixedSize(horizontal: false, vertical: true)
                }
            }
            if let sheet = status.sheet {
                results(sheet, status: status, t: t)
            }
            leftOutRows(status, t: t)
            if status.sheet == nil, !status.isRunning, status.line == nil, let blocker = status.blocker {
                Text(t.calibrateAllSheetBlocker(blocker))
                    .foregroundStyle(Brand.secondaryText)
                    .fixedSize(horizontal: false, vertical: true)
            }
            if let line = status.line {
                Text(t.calibrateAllLine(line))
                    .foregroundStyle(isProblem(line) ? Brand.errorText : Brand.secondaryText)
                    .fixedSize(horizontal: false, vertical: true)
            } else if let finished = finishedLine(status, t: t) {
                Text(finished)
                    .foregroundStyle(Brand.secondaryText)
                    .fixedSize(horizontal: false, vertical: true)
            }
            buttons(status, t: t)
        }
        .padding(20)
        .frame(width: 600, alignment: .leading)
        .interactiveDismissDisabled()
        .onChange(of: status.progress) { _, now in
            if let now { AccessibilityNotification.Announcement(t.calibrateAllProgress(now)).post() }
        }
        .onChange(of: status.line) { _, now in
            if let now { AccessibilityNotification.Announcement(t.calibrateAllLine(now)).post() }
        }
        .onChange(of: status.isRunning) { _, running in
            if !running, let finished = finishedLine(status, t: t) {
                AccessibilityNotification.Announcement(finished).post()
            }
        }
    }

    private func results(_ sheet: CalibrateAllSheet, status: CalibrateAllStatus, t: UIStrings) -> some View {
        VStack(alignment: .leading, spacing: 10) {
            Grid(alignment: .leadingFirstTextBaseline, horizontalSpacing: 14, verticalSpacing: 8) {
                GridRow {
                    Text(t.calibrateAllSpeakerColumn)
                    Text(t.calibrateAllCurrentColumn)
                    Text(t.calibrateAllNewColumn)
                    Color.clear.gridCellUnsizedAxes([.horizontal, .vertical])
                }
                .font(.caption.weight(.semibold))
                .foregroundStyle(Brand.secondaryText)
                .accessibilityHidden(true)
                Divider()
                ForEach(sheet.rows) { row in
                    GridRow {
                        VStack(alignment: .leading, spacing: 1) {
                            Text(row.speaker.name).lineLimit(2)
                            Text(t.calibrateAllKind(row.speaker.kind, reference: row.result == .reference))
                                .font(.caption)
                                .foregroundStyle(Brand.secondaryText)
                        }
                        .frame(width: 150, alignment: .leading)
                        .accessibilityElement(children: .combine)
                        Text(current(row, t: t))
                            .monospacedDigit()
                            .accessibilityLabel(t.calibrateAllCurrentColumn + ", " + current(row, t: t))
                            .accessibilityHidden(row.result == .reference)
                        Text(t.calibrateAllResult(row))
                            .monospacedDigit()
                            .foregroundStyle(color(row))
                            .fixedSize(horizontal: false, vertical: true)
                            .frame(maxWidth: .infinity, alignment: .leading)
                            .accessibilityLabel(t.calibrateAllNewColumn + ", " + t.calibrateAllResult(row))
                        if sheet.canMeasureAgain(row) {
                            Button(t.measureAgain) { model.actions.measureAgain(row.index) }
                                .controlSize(.small)
                                .disabled(status.isRunning || status.holdsOutputs)
                                .accessibilityLabel(t.measureAgainLabel(of: row.speaker.name))
                        } else {
                            Color.clear.gridCellUnsizedAxes([.horizontal, .vertical])
                        }
                    }
                }
            }
            if let airPlay = sheet.airPlay {
                Text(t.calibrateAllAirPlay(airPlay))
                    .foregroundStyle(airPlay.state == .certain ? Color.primary : Brand.secondaryText)
                    .fixedSize(horizontal: false, vertical: true)
            }
        }
    }

    /// The outputs that play but sit out, with why. Before a run, and after
    /// one that stopped early, each can be included anyway for the next run.
    @ViewBuilder
    private func leftOutRows(_ status: CalibrateAllStatus, t: UIStrings) -> some View {
        let choosing = canStartRun(status)
        let rows = choosing ? status.leftOut : status.leftOut.filter { !$0.included }
        if !rows.isEmpty {
            VStack(alignment: .leading, spacing: 8) {
                ForEach(rows) { row in
                    HStack(alignment: .firstTextBaseline, spacing: 10) {
                        VStack(alignment: .leading, spacing: 1) {
                            Text(row.name)
                                .lineLimit(2)
                                .foregroundStyle(row.included ? Color.primary : Brand.secondaryText)
                            Text(row.included ? t.calibrateAllLeftOutIncluded : t.calibrationLeftOut(row.reason))
                                .font(.caption)
                                .foregroundStyle(Brand.secondaryText)
                                .fixedSize(horizontal: false, vertical: true)
                        }
                        .accessibilityElement(children: .combine)
                        Spacer()
                        if choosing {
                            if row.included {
                                Button(t.leaveOut) { model.actions.includeInCalibrateAll(row.id, false) }
                                    .controlSize(.small)
                                    .accessibilityLabel(t.leaveOutLabel(of: row.name))
                            } else {
                                Button(t.includeAnyway) { model.actions.includeInCalibrateAll(row.id, true) }
                                    .controlSize(.small)
                                    .help(t.includeAnywayHelp(row.reason))
                                    .accessibilityLabel(t.includeAnywayLabel(of: row.name))
                                    .accessibilityHint(t.includeAnywayHelp(row.reason))
                            }
                        }
                    }
                }
            }
        }
    }

    /// No run holds the outputs and no finished sheet waits for Apply.
    private func canStartRun(_ status: CalibrateAllStatus) -> Bool {
        !status.isRunning && !status.holdsOutputs && (status.sheet == nil || status.sheet?.ended != nil)
    }

    private func buttons(_ status: CalibrateAllStatus, t: UIStrings) -> some View {
        HStack(spacing: 10) {
            if needsMicrophoneSettings(status.line) {
                Button(t.openMicrophoneSettings) { NSWorkspace.shared.open(PrivacyPane.microphone.url) }
            }
            Spacer()
            if status.isRunning {
                Button(t.calibrationCancel) { model.actions.cancelCalibrateAll() }
                    .keyboardShortcut(.cancelAction)
            } else {
                if status.line == .applied {
                    Button(t.calibrateAllUndo) { model.actions.undoCalibrateAll() }
                        .accessibilityLabel(t.calibrateAllUndoLabel)
                        .help(t.calibrateAllUndoHelp)
                        .accessibilityHint(t.calibrateAllUndoHelp)
                }
                let applied = status.sheet?.applied == true
                Button(applied ? t.calibrateAllDone : t.calibrateAllClose) { model.actions.closeCalibrateAll() }
                    .keyboardShortcut(.cancelAction)
                if status.sheet == nil || status.sheet?.ended != nil {
                    Button(t.calibrateAll) { model.actions.calibrateAll() }
                        .keyboardShortcut(.defaultAction)
                        .disabled(status.blocker != nil || status.holdsOutputs)
                } else if let sheet = status.sheet, !sheet.applied, sheet.ended == nil {
                    Button(t.calibrateAllApply) { model.actions.applyCalibrateAll() }
                        .keyboardShortcut(.defaultAction)
                        .disabled(!sheet.canApply)
                        .help(t.calibrateAllApplyHelp)
                        .accessibilityHint(t.calibrateAllApplyHelp)
                }
            }
        }
    }

    /// Under a finished run's rows while no other line shows: how many
    /// speakers were measured clearly, or that there is nothing to apply.
    private func finishedLine(_ status: CalibrateAllStatus, t: UIStrings) -> String? {
        guard status.line == nil, !status.isRunning, let sheet = status.sheet, sheet.ended == nil, !sheet.applied
        else { return nil }
        guard sheet.hasCertainRow else { return t.calibrateAllNothingCertain }
        let measured = sheet.rows.filter { $0.result != .reference }
        return t.calibrateAllFinished(clear: measured.filter(\.isCertain).count, total: measured.count)
    }

    private func current(_ row: CalibrateAllSheet.Row, t: UIStrings) -> String {
        if row.result == .reference { return "" }
        return row.oldMs.map { t.calibrateAllValue($0, kind: row.speaker.kind) } ?? t.calibrateAllNone
    }

    private func color(_ row: CalibrateAllSheet.Row) -> Color {
        switch row.result {
        case .certain: Color.primary
        case .refused: Brand.errorText
        default: Brand.secondaryText
        }
    }

    private func isProblem(_ line: CalibrateAllLine) -> Bool {
        switch line {
        case .ended(.failed), .againEnded(_, .failed), .changedSinceMeasured: true
        default: false
        }
    }

    private func needsMicrophoneSettings(_ line: CalibrateAllLine?) -> Bool {
        switch line {
        case .ended(.failed(.microphoneDenied)), .againEnded(_, .failed(.microphoneDenied)): true
        default: false
        }
    }
}
