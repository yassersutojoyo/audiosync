import SwiftUI

extension NoticeMotion {
    /// The columns' move as a notice opens or closes, a closing one's only
    /// once it has begun to fade; none with Reduce Motion.
    func columns(closing: Bool) -> Animation? {
        guard self == .grow else { return nil }
        let move = Animation.timingCurve(0.2, 0.8, 0.2, 1, duration: duration)
        return closing ? move.delay(closeDelay) : move
    }

    /// The height moves with the columns; the fade keeps its own timing, ending
    /// with the opening and starting before the closing.
    var transition: AnyTransition {
        guard self == .grow else { return .opacity.animation(.linear(duration: duration)) }
        return AnyTransition.asymmetric(
            insertion: .opacity.animation(.easeOut(duration: fade).delay(duration - fade)),
            removal: .opacity.animation(.easeOut(duration: fade)))
            .combined(with: .modifier(active: NoticeReveal(progress: 0), identity: NoticeReveal(progress: 1)))
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
        PaneHeight(fraction: progress) { content }
            .clipped()
    }
}
