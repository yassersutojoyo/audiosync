import AppKit
import SwiftUI

/// The left column's "Calibrate All Speakers" row (S235): opens the sheet
/// and starts the run, or says why it can't.
struct CalibrateAllButton: View {
    @ObservedObject var model: AppModel

    var body: some View {
        let t = model.text
        let blocker = model.calibration.holdsOutputs ? CalibrateAllBlocker.busy : model.calibrateAll.blocker
        let off = CalibrateAllFlow.rowBlocker(blocker)
        VStack(alignment: .leading, spacing: 6) {
            Button { model.actions.calibrateAll() } label: {
                HStack(spacing: 10) {
                    Image(systemName: "mic")
                        .font(.system(size: 15))
                        .foregroundStyle(off == nil ? Brand.teal : Color.secondary)
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
            .disabled(off != nil)
            .help(blocker.map(t.calibrateAllUnavailable) ?? t.calibrateAllHint)
            .accessibilityLabel(t.calibrateAll)
            .accessibilityHint(blocker.map(t.calibrateAllUnavailable) ?? t.calibrateAllHint)
            // Outside the button, so a disabled button doesn't dim the reason.
            if let off {
                Text(t.calibrateAllUnavailable(off))
                    .font(.caption)
                    .foregroundStyle(Brand.secondaryText)
                    .fixedSize(horizontal: false, vertical: true)
                    .accessibilityHidden(true)
            }
        }
    }
}

/// The heights the Calibrate All sheet measures to fit its rows between the
/// fixed top and bottom; the rows' height is nil until the first layout.
final class CalibrateAllLayout: ObservableObject {
    @Published var list: CGFloat?
    @Published var top: CGFloat = 0
    @Published var bottom: CGFloat = 0
}

/// The main window's content height: a sheet on the window is cut off past it.
final class SheetRoom: ObservableObject {
    @Published var height: CGFloat?
}

/// The Calibrate All Speakers sheet: the run's progress with Cancel, then
/// every speaker's current and new delay, the shared AirPlay delay, Apply,
/// Undo and Measure Again (S238, S239).
struct CalibrateAllSheetView: View {
    @ObservedObject var model: AppModel
    /// The main window's content height, nil until it was measured.
    let room: CGFloat?
    /// An object rather than @State: the Command Line Tools can't expand @State.
    @StateObject private var layout = CalibrateAllLayout()
    @FocusState private var focus: CalibrateAllFocus?

    /// Until the window's height is known: keeps Apply and Close on a
    /// 13-inch screen with a dozen or more rows.
    private static let fallbackListHeight: CGFloat = 420
    /// Two or three rows stay readable in the smallest main window.
    private static let minListHeight: CGFloat = 120
    private static let margin: CGFloat = 20
    private static let spacing: CGFloat = 14

    private var listCap: CGFloat {
        guard let room else { return Self.fallbackListHeight }
        let fixed = layout.top + layout.bottom + 2 * Self.spacing + 2 * Self.margin
        return max(Self.minListHeight, room - fixed)
    }

    var body: some View {
        let t = model.text
        let status = model.calibrateAll
        VStack(alignment: .leading, spacing: Self.spacing) {
            VStack(alignment: .leading, spacing: Self.spacing) {
                Text(status.zone == nil ? t.calibrateAll : t.calibrateZone)
                    .font(.title3.weight(.semibold))
                    .accessibilityAddTraits(.isHeader)
                if let zone = status.zone {
                    Text(t.calibrateZoneBring(zone.name))
                        .fixedSize(horizontal: false, vertical: true)
                }
                if status.sheet == nil, !status.isRunning {
                    beforeYouStart(status.volume, t: t)
                } else if status.sheet == nil, let progress = status.progress {
                    runHero(progress, t: t)
                }
            }
            .onGeometryChange(for: CGFloat.self) { $0.size.height } action: { layout.top = $0 }
            if status.sheet != nil || !visibleLeftOut(status).isEmpty {
                // The sheet can't be dismissed by hand and is cut off at the
                // window's bottom, so only the rows scroll and the buttons stay.
                ScrollView {
                    VStack(alignment: .leading, spacing: Self.spacing) {
                        if let sheet = status.sheet {
                            results(sheet, status: status, t: t)
                        }
                        leftOutRows(status, t: t)
                    }
                    .frame(maxWidth: .infinity, alignment: .leading)
                    .onGeometryChange(for: CGFloat.self) { $0.size.height } action: { layout.list = $0 }
                }
                .scrollBounceBehavior(.basedOnSize)
                .frame(height: layout.list.map { min($0, listCap) })
            }
            VStack(alignment: .leading, spacing: Self.spacing) {
                if status.sheet != nil, let progress = status.progress {
                    HStack(spacing: 8) {
                        ProgressView()
                            .controlSize(.small)
                            .accessibilityHidden(true)
                        Text(t.calibrateAllProgress(progress))
                            .fixedSize(horizontal: false, vertical: true)
                    }
                }
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
            .onGeometryChange(for: CGFloat.self) { $0.size.height } action: { layout.bottom = $0 }
        }
        .padding(Self.margin)
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

    /// Before a run: where the microphone goes and whether the reference is
    /// loud enough for it, with the main window's fix when it isn't.
    private func beforeYouStart(_ volume: CalibrationVolumeCheck?, t: UIStrings) -> some View {
        VStack(alignment: .leading, spacing: 6) {
            Text(t.calibrateBeforeStart)
                .font(.caption.weight(.semibold))
                .foregroundStyle(Brand.secondaryText)
                .accessibilityAddTraits(.isHeader)
            VStack(spacing: 0) {
                HStack(spacing: 12) {
                    checkIcon("mic", tint: Brand.tealText, solid: false)
                    VStack(alignment: .leading, spacing: 2) {
                        Text(t.calibrateMicrophoneCheck).fontWeight(.semibold)
                        Text(t.calibrateMicrophoneCheckDetail)
                            .foregroundStyle(Brand.secondaryText)
                            .fixedSize(horizontal: false, vertical: true)
                    }
                    .accessibilityElement(children: .combine)
                    Spacer(minLength: 0)
                }
                .padding(.horizontal, 12)
                .padding(.vertical, 10)
                if let volume {
                    Divider()
                    volumeRow(volume, t: t)
                }
            }
            .background(Color(nsColor: .controlBackgroundColor))
            .clipShape(RoundedRectangle(cornerRadius: 10))
            .overlay(RoundedRectangle(cornerRadius: 10).strokeBorder(Color(nsColor: .separatorColor)))
        }
    }

    /// A warning tinted in the warn color with the fix, or a check with the volume;
    /// it follows the device as it changes, so it goes once the fix works.
    private func volumeRow(_ volume: CalibrationVolumeCheck, t: UIStrings) -> some View {
        let warning = volume.warning
        let tint = warning == nil ? Brand.tealText : Brand.warn
        return HStack(spacing: 12) {
            checkIcon(warning?.symbol ?? "checkmark", tint: tint, solid: warning != nil)
            VStack(alignment: .leading, spacing: 2) {
                Text(t.calibrateVolumeTitle(volume)).fontWeight(.semibold)
                Text(t.calibrateVolumeDetail(volume))
                    .foregroundStyle(warning == nil ? Brand.secondaryText : Color.primary)
                    .fixedSize(horizontal: false, vertical: true)
                if let percent = volume.percent {
                    volumeMeter(percent, tint: tint, muted: isMuted(warning), t: t)
                }
            }
            .accessibilityElement(children: .combine)
            Spacer(minLength: 0)
            if let warning {
                Button(t.deviceVolumeFix(warning)) {
                    // The button goes once the fix works, so focus moves on before it does.
                    if focus == .fix { focus = .start }
                    model.actions.fixCalibrationVolume()
                }
                .focused($focus, equals: .fix)
            }
        }
        .padding(.horizontal, 12)
        .padding(.vertical, 10)
        .background(warning == nil ? Color.clear : Brand.warnBackground)
        .overlay(alignment: .leading) {
            if warning != nil { Rectangle().fill(Brand.warn).frame(width: 3) }
        }
    }

    private func isMuted(_ warning: DeviceVolumeHint?) -> Bool {
        switch warning {
        case .muted, .mutedAndLow: true
        default: false
        }
    }

    /// The volume against the low line. The title already says it, so
    /// VoiceOver skips the meter.
    private func volumeMeter(_ percent: Int, tint: Color, muted: Bool, t: UIStrings) -> some View {
        let width: CGFloat = 260
        let low = CGFloat(CalibrationVolumeCheck.lowPercent) / 100
        return VStack(alignment: .leading, spacing: 3) {
            ZStack(alignment: .leading) {
                Capsule().fill(Brand.meterTrack)
                Capsule()
                    .fill(tint.opacity(muted ? 0.35 : 1))
                    .frame(width: width * CGFloat(min(max(percent, 0), 100)) / 100)
                Rectangle()
                    .fill(Color.primary.opacity(0.55))
                    .frame(width: 2, height: 12)
                    .offset(x: width * low - 1)
            }
            .frame(width: width, height: 6)
            Text(t.percent(0))
                .frame(width: width, alignment: .leading)
                .overlay(alignment: .trailing) { Text(t.percent(100)) }
                .overlay(alignment: .leading) {
                    Text(t.percent(CalibrationVolumeCheck.lowPercent))
                        .frame(width: 40)
                        .offset(x: width * low - 20)
                }
                .font(.caption2)
                .foregroundStyle(Brand.secondaryText)
        }
        .padding(.top, 5)
        .accessibilityHidden(true)
    }

    private func checkIcon(_ symbol: String, tint: Color, solid: Bool) -> some View {
        Image(systemName: symbol)
            .font(.system(size: 12, weight: .bold))
            .foregroundStyle(solid ? Brand.warnBackground : tint)
            .frame(width: 26, height: 26)
            .background(Circle().fill(solid ? tint : tint.opacity(0.15)))
            .accessibilityHidden(true)
    }

    /// A whole run before its results: the sweep under way, in the middle.
    private func runHero(_ progress: CalibrateAllProgress, t: UIStrings) -> some View {
        VStack(spacing: 10) {
            ProgressView()
                .accessibilityHidden(true)
            Text(t.calibrateAllProgress(progress))
                .fontWeight(.semibold)
                .multilineTextAlignment(.center)
                .fixedSize(horizontal: false, vertical: true)
            Text(t.calibrateMusicFadesBack)
                .font(.caption)
                .foregroundStyle(Brand.secondaryText)
        }
        .accessibilityElement(children: .combine)
        .padding(.horizontal, 12)
        .padding(.vertical, 26)
        .frame(maxWidth: .infinity)
        .background(Color(nsColor: .controlBackgroundColor), in: RoundedRectangle(cornerRadius: 10))
        .overlay(RoundedRectangle(cornerRadius: 10).strokeBorder(Color(nsColor: .separatorColor)))
    }

    private func results(_ sheet: CalibrateAllSheet, status: CalibrateAllStatus, t: UIStrings) -> some View {
        VStack(alignment: .leading, spacing: 8) {
            ForEach(sheet.rows) { row in
                resultCard(row, sheet: sheet, status: status, t: t)
            }
            if let airPlay = sheet.airPlay {
                VStack(alignment: .leading, spacing: 4) {
                    Text(t.calibrateAllAirPlay(airPlay))
                        .foregroundStyle(airPlay.state == .certain ? Color.primary : Brand.secondaryText)
                        .fixedSize(horizontal: false, vertical: true)
                    if let moves = status.zone.flatMap({ t.calibrateZoneAirPlayMoves($0.airPlayMoves) }) {
                        Text(moves)
                            .foregroundStyle(Brand.secondaryText)
                            .fixedSize(horizontal: false, vertical: true)
                    }
                }
                .padding(.horizontal, 12)
                .padding(.vertical, 10)
                .frame(maxWidth: .infinity, alignment: .leading)
                .background(Brand.teal.opacity(0.1), in: RoundedRectangle(cornerRadius: 10))
            }
        }
    }

    /// One speaker: the new delay large with its uncertainty and the old
    /// one beside it, a chip for how clearly it was measured, and Measure Again.
    private func resultCard(_ row: CalibrateAllSheet.Row, sheet: CalibrateAllSheet, status: CalibrateAllStatus,
                            t: UIStrings) -> some View {
        let reference = row.result == .reference
        let chip = resultChip(row, t: t)
        let label: [String?] = [row.speaker.name, t.calibrateAllKind(row.speaker.kind, reference: reference), chip?.text,
                     t.calibrateAllResult(row), reference ? nil : t.calibrateAllCurrentColumn + ", " + current(row, t: t)]
        return HStack(spacing: 12) {
            VStack(alignment: .leading, spacing: 2) {
                HStack(alignment: .firstTextBaseline, spacing: 6) {
                    Text(row.speaker.name).fontWeight(.semibold).lineLimit(2)
                    if !reference {
                        Text(t.calibrateAllKind(row.speaker.kind, reference: false))
                            .font(.caption)
                            .foregroundStyle(Brand.secondaryText)
                    }
                    if let chip {
                        Text(chip.text)
                            .font(.caption2.weight(.semibold))
                            .foregroundStyle(chip.warn ? Color.primary : Brand.tealText)
                            .padding(.horizontal, 7)
                            .padding(.vertical, 1)
                            .background((chip.warn ? Brand.warn : Brand.teal).opacity(0.15), in: Capsule())
                            .overlay { if chip.warn { Capsule().strokeBorder(Brand.warn) } }
                    }
                }
                if let delay = newDelay(row) {
                    HStack(alignment: .firstTextBaseline, spacing: 6) {
                        Text(t.calibrateAllValue(delay.ms, kind: row.speaker.kind))
                            .font(.system(size: 15, weight: .semibold))
                            .foregroundStyle(color(row))
                        Text(t.calibrateAllWas(uncertaintyMs: delay.uncertaintyMs, was: current(row, t: t)))
                            .foregroundStyle(Brand.secondaryText)
                    }
                    .monospacedDigit()
                }
                if let note = t.calibrateAllCardNote(row) {
                    Text(newDelay(row) == nil && !reference ? t.calibrateAllNote(note, was: current(row, t: t)) : note)
                        .font(.caption)
                        .foregroundStyle(row.isCertain ? Brand.secondaryText : color(row))
                        .fixedSize(horizontal: false, vertical: true)
                }
            }
            .accessibilityElement(children: .ignore)
            .accessibilityLabel(label.compactMap { $0 }.joined(separator: ", "))
            Spacer(minLength: 0)
            if sheet.canMeasureAgain(row) {
                Button(t.measureAgain) { model.actions.measureAgain(row.index) }
                    .controlSize(.small)
                    .disabled(status.isRunning || status.holdsOutputs)
                    .accessibilityLabel(t.measureAgainLabel(of: row.speaker.name))
            }
        }
        .padding(.horizontal, 12)
        .padding(.vertical, 10)
        .frame(maxWidth: .infinity, alignment: .leading)
        .background(Color(nsColor: .controlBackgroundColor), in: RoundedRectangle(cornerRadius: 10))
        .overlay(RoundedRectangle(cornerRadius: 10).strokeBorder(Color(nsColor: .separatorColor)))
    }

    private func resultChip(_ row: CalibrateAllSheet.Row, t: UIStrings) -> (text: String, warn: Bool)? {
        guard !row.off else { return nil }
        switch row.result {
        case .reference: return (t.calibrateAllKind(row.speaker.kind, reference: true), false)
        case .certain: return (t.calibrateAllMeasuredClearly, false)
        case .uncertain: return (t.calibrateAllNotClear, true)
        default: return nil
        }
    }

    private func newDelay(_ row: CalibrateAllSheet.Row) -> (ms: Double, uncertaintyMs: Double)? {
        guard !row.off else { return nil }
        switch row.result {
        case .certain(let ms, let u, _), .uncertain(let ms, let u): return (ms, u)
        default: return nil
        }
    }

    /// The outputs that play but sit out, with why. Before a run, and after
    /// one that stopped early, each can be included anyway for the next run.
    @ViewBuilder
    private func leftOutRows(_ status: CalibrateAllStatus, t: UIStrings) -> some View {
        let choosing = canStartRun(status)
        let rows = visibleLeftOut(status)
        if !rows.isEmpty {
            VStack(alignment: .leading, spacing: 8) {
                Text(t.calibrateLeftOutTitle)
                    .font(.caption.weight(.semibold))
                    .foregroundStyle(Brand.secondaryText)
                    .accessibilityAddTraits(.isHeader)
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

    private func visibleLeftOut(_ status: CalibrateAllStatus) -> [CalibrateAllLeftOut] {
        canStartRun(status) ? status.leftOut : status.leftOut.filter { !$0.included }
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
                if status.canUndo {
                    Button(t.calibrateAllUndo) { model.actions.undoCalibrateAll() }
                        .accessibilityLabel(t.calibrateAllUndoLabel)
                        .help(t.calibrateAllUndoHelp)
                        .accessibilityHint(t.calibrateAllUndoHelp)
                }
                let applied = status.sheet?.applied == true
                Button(applied ? t.calibrateAllDone : t.calibrateAllClose) { model.actions.closeCalibrateAll() }
                    .keyboardShortcut(.cancelAction)
                if status.sheet == nil || status.sheet?.ended != nil {
                    Button(status.zone == nil ? t.calibrateAll : t.calibrateZone) { model.actions.calibrateAll() }
                        .keyboardShortcut(.defaultAction)
                        .focused($focus, equals: .start)
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

/// The sheet's controls that keyboard focus moves between on its own.
enum CalibrateAllFocus: Hashable {
    case fix, start
}
