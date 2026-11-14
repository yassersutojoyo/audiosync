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
        let destructive = configuration.role == .destructive
        let text = destructive ? HouseButton.destructiveText(dark: dark) : HouseButton.text(dark: dark)
        let ink = Color(red: text.red, green: text.green, blue: text.blue)
        let tint = destructive ? ink : Brand.teal
        let shape = RoundedRectangle(cornerRadius: 7, style: .continuous)
        let fill = HouseButton.fill(hovered: hover.isHovered, pressed: configuration.isPressed, dark: dark)
            * (destructive ? 0.5 : 1)
        configuration.label
            .font(.system(size: small ? 12 : 13, weight: .medium))
            .lineLimit(1)
            .foregroundStyle(isEnabled ? ink : Brand.secondaryText)
            .padding(.horizontal, small ? 10 : 12)
            .frame(height: small ? 22 : 26)
            .background(shape.fill(isEnabled ? tint.opacity(fill) : Color.primary.opacity(0.05)))
            .overlay(shape.strokeBorder(isEnabled ? tint.opacity(0.3) : Color.primary.opacity(0.12), lineWidth: 0.5))
            // Disabled text is exempt from the contrast rule; the dimming says it can't be used.
            .opacity(isEnabled ? 1 : 0.6)
            .contentShape(shape)
            .contentShape(.focusEffect, shape)
            .onHover { hover.isHovered = $0 }
            .animation(.easeOut(duration: 0.12), value: fill)
    }
}

/// A text field in the windows' box, ringed in the brand focus color while
/// `focused` in place of the system's blue ring.
struct HouseFieldLook: ViewModifier {
    let focused: Bool

    func body(content: Content) -> some View {
        let shape = RoundedRectangle(cornerRadius: 6, style: .continuous)
        content
            .textFieldStyle(.plain)
            .focusEffectDisabled()
            .padding(.horizontal, 6)
            .frame(height: 22)
            .background(shape.fill(Color(nsColor: .textBackgroundColor)))
            .overlay(shape.strokeBorder(focused ? Brand.focus : Color.primary.opacity(0.25),
                                        lineWidth: focused ? 2 : 0.5))
    }
}
