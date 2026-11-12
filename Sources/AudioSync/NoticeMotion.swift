import SwiftUI

extension NoticeMotion {
    /// The notice's move, opening or closing alike: linear, as `room` and `opacity` shape it, and
    /// the only animation its room and fade follow (a second one made the columns jump back); none
    /// with Reduce Motion.
    var columns: Animation? { self == .grow ? .linear(duration: duration) : nil }

    var transition: AnyTransition {
        guard self == .grow else { return .opacity.animation(.linear(duration: duration)) }
        return .modifier(active: NoticeReveal(progress: 0), identity: NoticeReveal(progress: 1))
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
