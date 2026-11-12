import SwiftUI

extension NoticeMotion {
    /// A notice's linear move, opening or closing alike, which `room` and `opacity` shape. The notice
    /// stack animates on it too: without that a closing notice left the layout at once and faded on top.
    var animation: Animation { .linear(duration: duration) }

    var transition: AnyTransition {
        let shape: AnyTransition = self == .grow
            ? .modifier(active: NoticeReveal(progress: 0), identity: NoticeReveal(progress: 1)) : .opacity
        return shape.animation(animation)
    }
}

/// A notice's height opening from its top edge, laid out at each animated `progress` so the AppKit
/// scroll views below follow it (an animated frame alone jumped them); clipped to the room it has.
private struct NoticeReveal: ViewModifier, Animatable {
    var progress: Double

    var animatableData: Double {
        get { progress }
        set { progress = newValue }
    }

    func body(content: Content) -> some View {
        PaneHeight(fraction: NoticeMotion.room(progress)) { content.opacity(NoticeMotion.opacity(progress)) }
            .clipped()
    }
}
