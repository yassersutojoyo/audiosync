import SwiftUI

extension NoticeMotion {
    /// A notice's own linear move, opening or closing alike, which `room` and `opacity` shape. Nothing
    /// around it animates the notice list, so the columns below follow its room frame by frame instead
    /// of sliding ahead on an animation of their own.
    var transition: AnyTransition {
        let shape: AnyTransition = self == .grow
            ? .modifier(active: NoticeReveal(progress: 0), identity: NoticeReveal(progress: 1)) : .opacity
        return shape.animation(.linear(duration: duration))
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
