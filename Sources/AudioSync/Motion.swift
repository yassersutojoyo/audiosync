import SwiftUI
import CAudioSyncCore

/// How speakers and their values move in the main window and the menu bar
/// panel. With Reduce Motion each one changes at once, or only cross-fades.
enum Motion {
    /// A speaker arriving in or leaving the list fades, and the rows around it
    /// slide to make room, so the eye sees where it went.
    static func rows(reduceMotion: Bool) -> Animation? {
        reduceMotion ? nil : .easeOut(duration: 0.22)
    }

    /// A badge and row tint between off, waiting and playing: a cross-fade
    /// that confirms the click without a jump.
    static func state(reduceMotion: Bool) -> Animation {
        .easeInOut(duration: reduceMotion ? 0.1 : 0.2)
    }

    /// A speaker that starts playing: the filled badge grows out of the
    /// waiting ring, so the change reads as arriving; leaving only fades.
    static func arrival(reduceMotion: Bool) -> AnyTransition {
        reduceMotion ? .opacity
            : .asymmetric(insertion: .scale(scale: 0.6).combined(with: .opacity), removal: .opacity)
    }

    /// The input meter's fall glides over one poll to the next reading, so its
    /// ten readings a second don't show as steps; a rise shows at once.
    static func meter(reduceMotion: Bool) -> Animation? {
        reduceMotion ? nil : .linear(duration: Double(ASC_METER_POLL_MS) / 1000)
    }

    /// A delay value's digits roll, short enough to keep up with a held step.
    static func value(reduceMotion: Bool) -> Animation? {
        reduceMotion ? nil : .snappy(duration: 0.15)
    }

    /// A delay panel or a zone opening or closing in the main window.
    static func panel(reduceMotion: Bool) -> Animation? {
        reduceMotion ? nil : .easeInOut(duration: 0.15)
    }

    /// A note or row showing or going in the menu bar panel (`PanelNotes`):
    /// it fades while the rows below slide, on the panel's own resize curve,
    /// so they keep pace with its edge.
    static func section(reduceMotion: Bool) -> Animation? {
        reduceMotion ? nil : .easeOut(duration: PanelResize.duration)
    }
}
