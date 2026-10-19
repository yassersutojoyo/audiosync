import SwiftUI

// The output device volume hint in the main window and the menu bar panel
// (S198 to S200); what they show comes from DeviceVolume.swift.

/// The hint on its own line under a switched-on local output's volume
/// slider: a decorative glyph, the hint text (one VoiceOver element whose
/// label names the output, with a tooltip that explains it) and the fix as a
/// borderless link after it. It comes and goes with the decision without
/// moving focus or being announced.
struct DeviceVolumeLine: View {
    let shown: DeviceVolumePresentation
    let fix: () -> Void

    var body: some View {
        HStack(spacing: 4) {
            Image(systemName: shown.symbol)
                .imageScale(.small)
                .accessibilityHidden(true)
            Text(shown.text)
                .lineLimit(1)
                .help(shown.help)
                .accessibilityLabel(shown.label)
            Button(action: fix) {
                Text(shown.fixTitle)
                    .font(.caption.weight(.semibold))
                    // Primary text, like the notices' links: the brand teal can
                    // fall below 4.5:1 on a playing row.
                    .foregroundStyle(.primary)
            }
            .buttonStyle(.plain)
            .fixedSize()
            .accessibilityLabel(shown.fixLabel)
        }
        .font(.caption)
        // The secondary text style, at least 4.5:1 in light and dark (Brand.swift).
        .foregroundStyle(Brand.secondaryText)
    }
}

/// The compact panel's glyph for a switched-on local output, in the slot of
/// the availability note: information, not a warning, and no button (the
/// fix is in the main window). The row's VoiceOver hint carries the text.
struct DeviceVolumeBadge: View {
    let glyph: DeviceVolumeGlyph

    var body: some View {
        Image(systemName: glyph.symbol)
            .imageScale(.small)
            .font(.caption)
            .foregroundStyle(Brand.secondaryText)
            .help(glyph.tooltip)
            .accessibilityHidden(true)
    }
}
