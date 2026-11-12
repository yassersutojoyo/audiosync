import AppKit
import SwiftUI

/// Small window holding the equalizer.
final class EqualizerWindowController {
    private var window: NSWindow?
    let model: EqualizerModel
    private let curveName = CurveNameDraft()

    init(model: EqualizerModel) { self.model = model }

    func show() {
        if window == nil {
            let view = EqualizerView(model: model, curveName: curveName,
                                     curvesFull: { [weak self] in self?.informCurvesFull() },
                                     askRename: { [weak self] index in self?.promptRenameCurve(index) })
            let host = NSHostingController(rootView: view)
            let panel = EqualizerPanel(contentViewController: host)
            panel.title = UIStrings().equalizerTitle
            panel.dropName = { [weak self] in
                guard let draft = self?.curveName, draft.open else { return false }
                draft.open = false
                return true
            }
            panel.styleMask = [.titled, .closable]
            panel.isReleasedWhenClosed = false   // kept and shown again by the next show()
            panel.isExcludedFromWindowsMenu = true   // the Window menu has its own Equalizer item
            panel.center()
            window = panel
        }
        NSApp.activate(ignoringOtherApps: true)
        window?.makeKeyAndOrderFront(nil)
    }

    private func informCurvesFull() {
        let text = UIStrings()
        inform(text.saveCurveTitle, text.curvesFull(Equalizer.maxCurves))
    }

    private func promptRenameCurve(_ index: Int) {
        let text = UIStrings()
        guard model.curves.indices.contains(index) else { return }
        let current = model.curves[index].name
        guard let typed = promptName(title: text.renameCurveTitle, message: text.renameMessage(current),
                                     name: current, confirm: text.rename) else { return }
        if !model.renameCurve(index, to: typed), let name = Equalizer.curveName(typed) {
            inform(text.renameCurveTitle, text.curveNameTaken(name))
        }
    }

    /// The typed name, or nil when the user cancelled.
    private func promptName(title: String, message: String, name: String, confirm: String) -> String? {
        let text = UIStrings()
        let prompt = NSAlert()
        prompt.messageText = title
        prompt.informativeText = message
        let field = NSTextField(frame: NSRect(x: 0, y: 0, width: 240, height: 24))
        field.stringValue = name
        field.placeholderString = text.curveNameField
        field.setAccessibilityLabel(text.curveNameField)
        prompt.accessoryView = field
        prompt.addButton(withTitle: confirm)
        prompt.addButton(withTitle: text.cancel)
        prompt.window.initialFirstResponder = field
        guard prompt.runModal() == .alertFirstButtonReturn else { return nil }
        return field.stringValue
    }

    private func inform(_ title: String, _ message: String) {
        let alert = NSAlert()
        alert.alertStyle = .informational
        alert.messageText = title
        alert.informativeText = message
        alert.runModal()
    }
}

/// The curve name typed by Save Curve…, held by the window's controller so
/// Escape at the window can drop it.
final class CurveNameDraft: ObservableObject {
    @Published var open = false
    @Published var text = ""
    var suggested = ""
}

/// Return or the focus leaving saves, Escape drops the typing.
private struct CurveNameField: View {
    @ObservedObject var draft: CurveNameDraft
    let text: UIStrings
    let finish: (Bool) -> Void
    @FocusState private var focused: Bool

    var body: some View {
        TextField(draft.suggested, text: $draft.text)
            .textFieldStyle(.roundedBorder)
            .accessibilityLabel(text.curveNameField)
            .accessibilityHint(text.zoneNameFieldHint)
            .help(text.saveCurveMessage)
            .focused($focused)
            .onSubmit { finish(true) }
            .onExitCommand { draft.open = false }
            // Save Curve… is a menu item: the field takes the focus once the menu has closed.
            .onAppear { DispatchQueue.main.async { focused = true } }
            .onChange(of: focused) { _, now in
                if !now { finish(false) }
            }
    }
}

/// Escape closes the equalizer, as it does the Guide, except while the curve
/// name field is open: then it drops the name, even before the field has the focus.
private final class EqualizerPanel: NSWindow {
    var dropName: () -> Bool = { false }

    override func cancelOperation(_ sender: Any?) {
        if dropName() { return }
        performClose(sender)
    }
}

private let accent = Brand.teal

/// Set by a click on a band, which then takes keyboard focus as a text field does.
private final class SliderClick: ObservableObject {
    @Published var clicked = false
}

struct EqualizerView: View {
    @ObservedObject var model: EqualizerModel
    @ObservedObject var curveName: CurveNameDraft
    let curvesFull: () -> Void
    let askRename: (Int) -> Void
    private let text = UIStrings()

    var body: some View {
        let s = model.settings
        VStack(alignment: .leading, spacing: 14) {
            HStack {
                Toggle(text.equalizerTitle, isOn: Binding(get: { s.enabled }, set: { on in
                    var n = s; n.enabled = on; model.set(n)
                }))
                .toggleStyle(.switch)
                .font(.headline)
                Spacer()
                if curveName.open {
                    CurveNameField(draft: curveName, text: text, finish: finishCurveName)
                        .frame(width: 220)
                } else {
                    Text(text.equalizerCurve)
                        .accessibilityHidden(true)   // the picker carries the label
                    curvePicker
                        .frame(width: 170)
                }
            }

            HStack(alignment: .bottom, spacing: 6) {
                band(label: text.equalizerPreamp, name: text.equalizerPreampAccessibility, value: s.preamp) { v in
                    var n = s; n.preamp = v; model.set(n)
                }
                Divider().frame(height: 170)
                ForEach(Equalizer.frequencies.indices, id: \.self) { i in
                    let hz = Equalizer.frequencies[i]
                    band(label: text.equalizerBandShort(hz: hz), name: text.equalizerBandName(hz: hz),
                         value: s.gains[i]) { v in
                        var n = s; n.gains[i] = v; model.set(n)
                    }
                }
            }

            HStack {
                Text(text.equalizerCaption(maxGain: Int(Equalizer.maxGain)))
                    .font(.caption).foregroundStyle(Brand.secondaryText)
                Spacer()
                Button(text.equalizerReset) { model.reset() }
            }
        }
        .padding(18)
        .frame(width: 520)
        .tint(accent)
    }

    /// The presets, then the saved curves; the label is the curve the gains
    /// match, or Custom, which is not an entry.
    private var curvePicker: some View {
        let saved = model.shownSavedIndex
        let shown = text.equalizerCurveName(model.shownCurve, saved: model.curves)
        return Menu {
            ForEach(Equalizer.presets, id: \.id) { preset in
                Button(text.equalizerPreset(preset.id)) { model.pick(.preset(preset.id)) }
            }
            if !model.curves.isEmpty {
                Divider()
                ForEach(model.curves.indices, id: \.self) { index in
                    let curve = model.curves[index]
                    Button(curve.name) { model.pick(.saved(index)) }
                }
            }
            Divider()
            Button(text.equalizerSaveCurve) { startCurveName() }
            if let saved, model.curves.indices.contains(saved) {
                let name = model.curves[saved].name
                Button(text.equalizerRenameCurve(name)) { askRename(saved) }
                Button(text.equalizerDeleteCurve(name)) { model.deleteCurve(saved) }
            }
        } label: {
            // The menu takes its title from this content, which read the
            // curve name instead of the fixed label.
            Text(shown)
                .accessibilityElement(children: .ignore)
                .accessibilityLabel(text.equalizerCurve)
                .accessibilityValue(shown)
        }
        .accessibilityLabel(text.equalizerCurve)
        .accessibilityValue(shown)
    }

    /// Save Curve… names the curve in place of the picker. The suggestion is
    /// the saved curve the picker shows, so saving again updates it, or the
    /// next "Curve N".
    private func startCurveName() {
        curveName.suggested = model.shownSavedIndex.map { model.curves[$0].name }
            ?? text.numberedCurve(model.curves.count + 1)
        curveName.text = ""
        curveName.open = true
    }

    private func finishCurveName(returned: Bool) {
        guard curveName.open else { return }
        curveName.open = false
        guard let typed = InPlaceName.applied(typed: curveName.text, suggested: curveName.suggested,
                                              returned: returned) else { return }
        if Equalizer.curveName(typed) != nil, !model.saveCurve(named: typed) { curvesFull() }
    }

    /// `label` is the short text under the slider; `name` is what VoiceOver reads.
    private func band(label: String, name: String, value: Double,
                      set: @escaping (Double) -> Void) -> some View {
        VStack(spacing: 6) {
            Text(text.equalizerGain(value))
                .font(.caption2.monospacedDigit())
                .foregroundStyle(Brand.secondaryText)
                .accessibilityHidden(true)   // the slider reads its own value
            VerticalSlider(value: value, range: Equalizer.maxGain, onChange: set)
                .frame(width: 26, height: 140)
                // Only the slider dims with the EQ off, so the gains and bands stay readable.
                .opacity(model.settings.enabled ? 1 : 0.45)
                .accessibilityLabel(name)
                .accessibilityValue(text.equalizerGainValue(value))
            Text(label).font(.caption2).foregroundStyle(Brand.secondaryText)
                .accessibilityHidden(true)
        }
        .frame(maxWidth: .infinity)
    }
}

/// A centered (−range…+range) vertical slider in 0.5 dB steps; double-click
/// resets to 0. VoiceOver adjusts it like a slider, and once focused (a click,
/// or Tab with keyboard navigation on) the arrow keys move it one step.
struct VerticalSlider: View {
    let value: Double
    let range: Double
    let onChange: (Double) -> Void
    @StateObject private var click = SliderClick()
    @FocusState private var focused: Bool

    static let step = 0.5

    private func nudge(_ steps: Double) {
        let v = ((value + steps * Self.step) / Self.step).rounded() * Self.step
        let clamped = max(-range, min(range, v))
        guard clamped != value else { return }
        onChange(clamped)
    }

    var body: some View {
        track
            // Edit focus only after a click, so Tab reaches the bands only with keyboard navigation on.
            .focusable(interactions: click.clicked ? .edit : .activate)
            .focused($focused)
            .onChange(of: click.clicked) { _, now in if now { focused = true } }
            .onChange(of: focused) { _, now in if !now { click.clicked = false } }
            .onKeyPress(.upArrow) { nudge(1); return .handled }
            .onKeyPress(.downArrow) { nudge(-1); return .handled }
            .onKeyPress(.rightArrow) { nudge(1); return .handled }
            .onKeyPress(.leftArrow) { nudge(-1); return .handled }
            .accessibilityRepresentation {
                Slider(value: Binding(get: { value }, set: { v in
                    onChange((max(-range, min(range, v)) / Self.step).rounded() * Self.step)
                }), in: -range...range, step: Self.step)
            }
            .accessibilityAdjustableAction { direction in
                switch direction {
                case .increment: nudge(1)
                case .decrement: nudge(-1)
                @unknown default: break
                }
            }
    }

    private var track: some View {
        GeometryReader { geo in
            let h = geo.size.height
            let mid = h / 2
            let y = mid - CGFloat(value / range) * (h / 2 - 8)
            ZStack(alignment: .top) {
                // Full `.secondary` keeps the track and knob edge at 3:1, as PowerSwitch does.
                Capsule()
                    .fill(Color.secondary)
                    .frame(width: 4)
                    .frame(maxWidth: .infinity)
                Rectangle()
                    .fill(accent)
                    .frame(width: 4, height: abs(y - mid))
                    .offset(y: min(y, mid))
                    .frame(maxWidth: .infinity)
                Rectangle()
                    .fill(Color.primary)
                    .frame(width: 12, height: 1)
                    .offset(y: mid)
                    .frame(maxWidth: .infinity)
                Circle()
                    .fill(Color.white)
                    .overlay(Circle().strokeBorder(Color.secondary, lineWidth: 1))
                    .shadow(color: .black.opacity(0.3), radius: 1.5, y: 0.5)
                    .frame(width: 16, height: 16)
                    .offset(y: y - 8)
                    .frame(maxWidth: .infinity)
            }
            .contentShape(Rectangle())
            .gesture(DragGesture(minimumDistance: 0).onChanged { g in
                if !click.clicked { click.clicked = true }
                let v = Double((mid - g.location.y) / (h / 2 - 8)) * range
                onChange((max(-range, min(range, v)) * 2).rounded() / 2)   // 0.5 dB steps
            })
            .onTapGesture(count: 2) { onChange(0) }
        }
    }
}
