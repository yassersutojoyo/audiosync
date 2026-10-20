import AppKit
import SwiftUI

/// Small window holding the equalizer.
final class EqualizerWindowController {
    private var window: NSWindow?
    let model: EqualizerModel

    init(model: EqualizerModel) { self.model = model }

    func show() {
        if window == nil {
            let host = NSHostingController(rootView: EqualizerView(model: model))
            let panel = NSWindow(contentViewController: host)
            panel.title = UIStrings().equalizerTitle
            panel.styleMask = [.titled, .closable]
            panel.isReleasedWhenClosed = false   // kept and shown again by the next show()
            panel.isExcludedFromWindowsMenu = true   // the Window menu has its own Equalizer item
            panel.center()
            window = panel
        }
        NSApp.activate(ignoringOtherApps: true)
        window?.makeKeyAndOrderFront(nil)
    }
}

private let accent = Brand.teal

struct EqualizerView: View {
    @ObservedObject var model: EqualizerModel
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
                Menu(model.matchingPreset.map(text.equalizerPreset) ?? text.equalizerCustom) {
                    ForEach(Equalizer.presets, id: \.id) { preset in
                        Button(text.equalizerPreset(preset.id)) {
                            var n = s
                            n.gains = preset.gains
                            n.enabled = true
                            model.set(n)
                        }
                    }
                }
                .frame(width: 150)
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
            .opacity(s.enabled ? 1 : 0.45)

            HStack {
                Text(text.equalizerCaption(maxGain: Int(Equalizer.maxGain)))
                    .font(.caption).foregroundStyle(Brand.secondaryText)
                Spacer()
                Button(text.equalizerReset) {
                    var n = s
                    n.gains = Equalizer.presets[0].gains
                    n.preamp = 0
                    model.set(n)
                }
            }
        }
        .padding(18)
        .frame(width: 520)
        .tint(accent)
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
                .accessibilityLabel(name)
                .accessibilityValue(text.equalizerGainValue(value))
            Text(label).font(.caption2).foregroundStyle(Brand.secondaryText)
                .accessibilityHidden(true)
        }
        .frame(maxWidth: .infinity)
    }
}

/// A centered (−range…+range) vertical slider in 0.5 dB steps; double-click
/// resets to 0. VoiceOver adjusts it like a slider, and once focused (Tab with
/// keyboard navigation on) the up and down arrow keys move it one step.
struct VerticalSlider: View {
    let value: Double
    let range: Double
    let onChange: (Double) -> Void

    static let step = 0.5

    private func nudge(_ steps: Double) {
        let v = ((value + steps * Self.step) / Self.step).rounded() * Self.step
        let clamped = max(-range, min(range, v))
        guard clamped != value else { return }
        onChange(clamped)
    }

    var body: some View {
        track
            .focusable(interactions: .activate)   // keyboard navigation only, like a native slider
            .onKeyPress(.upArrow) { nudge(1); return .handled }
            .onKeyPress(.downArrow) { nudge(-1); return .handled }
            .accessibilityElement(children: .ignore)
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
                Capsule()
                    .fill(Color.secondary.opacity(0.2))
                    .frame(width: 4)
                    .frame(maxWidth: .infinity)
                Rectangle()
                    .fill(accent)
                    .frame(width: 4, height: abs(y - mid))
                    .offset(y: min(y, mid))
                    .frame(maxWidth: .infinity)
                Rectangle()
                    .fill(Color.secondary.opacity(0.5))
                    .frame(width: 12, height: 1)
                    .offset(y: mid)
                    .frame(maxWidth: .infinity)
                Circle()
                    .fill(Color.white)
                    .shadow(color: .black.opacity(0.3), radius: 1.5, y: 0.5)
                    .frame(width: 16, height: 16)
                    .offset(y: y - 8)
                    .frame(maxWidth: .infinity)
            }
            .contentShape(Rectangle())
            .gesture(DragGesture(minimumDistance: 0).onChanged { g in
                let v = Double((mid - g.location.y) / (h / 2 - 8)) * range
                onChange((max(-range, min(range, v)) * 2).rounded() / 2)   // 0.5 dB steps
            })
            .onTapGesture(count: 2) { onChange(0) }
        }
    }
}
