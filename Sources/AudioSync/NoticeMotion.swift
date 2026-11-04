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
            .combined(with: .modifier(active: NoticeReveal(open: false), identity: NoticeReveal(open: true)))
    }
}

/// A notice's height opening from its top edge. Clipped, so a closing one
/// never shows past the room it has left.
private struct NoticeReveal: ViewModifier {
    let open: Bool

    func body(content: Content) -> some View {
        content
            .frame(height: open ? nil : 0, alignment: .top)
            .clipped()
    }
}
