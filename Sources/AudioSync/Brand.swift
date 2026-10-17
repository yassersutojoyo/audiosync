import AppKit
import SwiftUI

/// AudioSync's brand colors and typeface (see brand/README.md).
enum Brand {
    /// The brand teal, #1C9F91. Also the app's accent color.
    static let teal = Color(red: 28 / 255, green: 159 / 255, blue: 145 / 255)

    /// Teal on light appearances, the brand mint (#63DAC2) on dark ones, as in the logo artwork.
    static let logo = Color(nsColor: NSColor(name: "AudioSyncLogo") { appearance in
        appearance.bestMatch(from: [.aqua, .darkAqua]) == .darkAqua
            ? NSColor(srgbRed: 99 / 255, green: 218 / 255, blue: 194 / 255, alpha: 1)
            : NSColor(srgbRed: 28 / 255, green: 159 / 255, blue: 145 / 255, alpha: 1)
    })

    /// Teal for small text (WCAG AA needs 4.5:1), same hue as the brand teal.
    /// #1C9F91 itself is only 3.27:1 on white and 3.92:1 on the dark window
    /// background (#323232), so each appearance gets its own shade:
    /// - light: #147167, 5.85:1 on white, 4.95:1 on the light window background (#ECECEC)
    /// - dark: #20B6A6, 5.07:1 on #323232, 6.59:1 on #1E1E1E
    static let tealText = Color(nsColor: NSColor(name: "AudioSyncTealText") { appearance in
        appearance.bestMatch(from: [.aqua, .darkAqua]) == .darkAqua
            ? NSColor(srgbRed: 32 / 255, green: 182 / 255, blue: 166 / 255, alpha: 1)
            : NSColor(srgbRed: 20 / 255, green: 113 / 255, blue: 103 / 255, alpha: 1)
    })

    /// Small secondary text (captions, notes, values), in place of `.secondary`.
    /// WCAG AA needs 4.5:1 for small text, and `.secondary` misses it in light
    /// appearance. Measured on macOS 26 (window and card #FFFFFF, menu bar panel
    /// stand-in #F6F6F6 light / #2B2B2B dark, playing-row tint #EDF7F6 / #1E2827):
    /// - `.secondary`: light 3.95 window, 3.89 panel, 3.88 playing row (fails);
    ///   dark 5.89 window, 5.37 panel
    /// - this: light 5.59 window, 5.46 panel, 5.45 playing row;
    ///   dark 6.64 window, 6.00 panel, 6.25 playing row
    /// Icons beside text may stay `.secondary`: non-text graphics need only 3:1.
    static let secondaryText = Color.primary.opacity(0.7)

    /// Poppins Medium, bundled in Contents/Resources/Fonts (registered through
    /// ATSApplicationFontsPath in Info.plist). SwiftUI falls back to the system
    /// font when it is missing, e.g. when the binary runs outside the app bundle.
    static let fontName = "Poppins-Medium"
}

/// The "audiosync" wordmark: "audio" in the label color, "sync" in the logo color.
struct Wordmark: View {
    var size: CGFloat

    var body: some View {
        Text(text)
            .font(.custom(Brand.fontName, size: size))
            .tracking(-0.02 * size)
            // Poppins' line box (ascent 1.05 em, descent 0.35 em) leaves about 0.3 em
            // empty above the ascenders and 0.09 em below the "y": trim it so the
            // wordmark lines up with the text around it like a system font does.
            .padding(.top, -0.3 * size)
            .padding(.bottom, -0.08 * size)
            .accessibilityLabel(AppText().appName)
    }

    /// The logo's lettering, drawn as artwork rather than translated text.
    private var text: AttributedString {
        var audio = AttributedString("audio")
        audio.foregroundColor = .primary
        var sync = AttributedString("sync")
        sync.foregroundColor = Brand.logo
        return audio + sync
    }
}
