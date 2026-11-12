import SwiftUI

/// AudioSync's own push button for the plain buttons in Settings and the main
/// window: the teal tint the notices use, in place of the system bezel. Menus,
/// pickers and system alerts keep the system look.
struct HouseButtonStyle: ButtonStyle {
    var small = false

    func makeBody(configuration: Configuration) -> some View {
        HouseButtonBody(configuration: configuration, small: small)
    }
}

private struct HouseButtonBody: View {
    let configuration: ButtonStyleConfiguration
    let small: Bool
    @Environment(\.isEnabled) private var isEnabled
    @Environment(\.colorScheme) private var colorScheme
    @StateObject private var hover = HoverState()

    var body: some View {
        let dark = colorScheme == .dark
        let text = HouseButton.text(dark: dark)
        let shape = RoundedRectangle(cornerRadius: 7, style: .continuous)
        let fill = HouseButton.fill(hovered: hover.isHovered, pressed: configuration.isPressed, dark: dark)
        configuration.label
            .font(.system(size: small ? 12 : 13, weight: .medium))
            .lineLimit(1)
            .foregroundStyle(isEnabled ? Color(red: text.red, green: text.green, blue: text.blue) : Brand.secondaryText)
            .padding(.horizontal, small ? 10 : 12)
            .frame(height: small ? 22 : 26)
            .background(shape.fill(isEnabled ? Brand.teal.opacity(fill) : Color.primary.opacity(0.05)))
            .overlay(shape.strokeBorder(isEnabled ? Brand.teal.opacity(0.3) : Color.primary.opacity(0.12), lineWidth: 0.5))
            // Disabled text is exempt from the contrast rule; the dimming says it can't be used.
            .opacity(isEnabled ? 1 : 0.6)
            .contentShape(shape)
            .contentShape(.focusEffect, shape)
            .onHover { hover.isHovered = $0 }
            .animation(.easeOut(duration: 0.12), value: fill)
    }
}
