import AppKit

/// Colors that stay legible on the menu bar panel's translucent material,
/// whatever window is behind it. The logic suite measures them with the
/// material counted as clear, over a black and a white backdrop.
extension NSColor {
    /// Laid under the panel's content: the surface (light) or the window
    /// background (dark) at 88%. On the popover material alone, a light panel
    /// over a black window turns mid-grey and the brand teal drops near 2:1.
    static let audioSyncPanelBacking = appearanceColor(
        "AudioSyncPanelBacking", light: BrandTokens.Light.surface, dark: BrandTokens.Dark.background, alpha: 0.88)

    /// A playing AirPlay or Bluetooth speaker's type icon: a system-like blue,
    /// deeper on light and lighter on dark than `systemBlue`, which falls
    /// below 3:1 on the panel over a black (light) or white (dark) window.
    static let audioSyncActiveBlue = appearanceColor(
        "AudioSyncActiveBlue",
        light: BrandRGB(red: 0 / 255, green: 98 / 255, blue: 204 / 255),
        dark: BrandRGB(red: 94 / 255, green: 174 / 255, blue: 255 / 255))

    /// "sync" in the wordmark: the darker text teal on light, since the brand
    /// teal keeps under 3:1 on the panel over a black window; mint on dark.
    static let audioSyncWordmarkSync = appearanceColor(
        "AudioSyncWordmarkSync", light: BrandTokens.Light.tealText, dark: BrandTokens.Dark.logo)

    /// A color with one value in light appearances and another in dark ones.
    static func appearanceColor(_ name: NSColor.Name, light: BrandRGB, dark: BrandRGB,
                                alpha: Double = 1) -> NSColor {
        NSColor(name: name) { appearance in
            let rgb = appearance.bestMatch(from: [.aqua, .darkAqua]) == .darkAqua ? dark : light
            return NSColor(srgbRed: rgb.red, green: rgb.green, blue: rgb.blue, alpha: alpha)
        }
    }
}
